//! The outcomes a sequence ends with when the app or the container runtime cannot finish it. Each
//! test runs the real container runtime; none is skipped when docker is present.

mod support;

use std::future::pending;

use acan_intent::{NonEmpty, RunOutcome, RunRole};
use acan_witness::run_sequence;
use support::{Setup, assert_no_container, runner_config};

/// An app that serves the health check and exits on its first other request.
const CRASHING_APP: &str = r#"Bun.serve({
  port: Number(process.env.PORT),
  hostname: "127.0.0.1",
  fetch(request) {
    if (new URL(request.url).pathname === "/health") return new Response("ok");
    process.exit(3);
  },
});
"#;

#[tokio::test]
async fn an_app_that_exits_on_a_request_is_crashed() {
    let _docker = support::DOCKER.lock().await;
    let mut setup = Setup::base();
    std::fs::write(setup.worktree.join("crashing-app.ts"), CRASHING_APP)
        .expect("the crashing app can be written into the worktree");
    setup.app.boot = NonEmpty::new(
        "bun".to_owned(),
        vec!["run".to_owned(), "crashing-app.ts".to_owned()],
    );
    let config = runner_config();
    let probe = setup.probe("signup-store");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");

    let run = observed.run();
    assert!(
        matches!(run.outcome, RunOutcome::Crashed { .. }),
        "the app exited while a request was in flight: {:?}",
        run.outcome
    );
    assert_no_container(&run.id);
}

#[tokio::test]
async fn an_app_that_never_answers_its_ready_check_is_a_setup_failure() {
    let _docker = support::DOCKER.lock().await;
    let mut setup = Setup::base();
    // The app is not listening one millisecond after it starts, so the ready check cannot pass.
    setup.app.ready.timeout_ms = 1;
    let config = runner_config();
    let probe = setup.probe("signup-store");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");

    let run = observed.run();
    match &run.outcome {
        RunOutcome::SetupFailed { step_id, detail } => {
            assert!(step_id.is_none(), "no step ran: {step_id:?}");
            assert!(detail.contains("not ready"), "detail: {detail}");
        }
        other => panic!("expected a setup failure, got {other:?}"),
    }
    assert_eq!(observed.observations().count(), 0, "no step ran");
    assert_no_container(&run.id);
}

#[tokio::test]
async fn a_pinned_image_the_runtime_does_not_hold_is_isolation_unavailable() {
    let _docker = support::DOCKER.lock().await;
    let mut setup = Setup::base();
    // No image has this digest. The runner never pulls, so the runtime must refuse the create.
    let absent = format!("sha256:{}", "0".repeat(64));
    setup.policy.image.digest = Some(
        serde_json::from_value(serde_json::Value::from(absent))
            .expect("a well-formed digest decodes"),
    );
    let config = runner_config();
    let probe = setup.probe("signup-store");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");

    let run = observed.run();
    match &run.outcome {
        RunOutcome::IsolationUnavailable { detail } => {
            assert!(
                detail.contains("unavailable"),
                "detail names the unavailable runtime: {detail}"
            );
        }
        other => panic!("expected isolation_unavailable, got {other:?}"),
    }
    assert_eq!(observed.observations().count(), 0, "no step ran");
    assert_no_container(&run.id);
}

/// An app that writes more output than the driver keeps, then exits before it answers the ready
/// check. Its last message has no newline.
const CHATTY_APP: &str = r#"process.stdout.write("x".repeat(5000) + "\n");
for (let i = 0; i < 300; i++) console.log(`line ${i}`);
process.stderr.write("the last message");
process.exit(4);
"#;

#[tokio::test]
async fn a_start_that_fails_keeps_the_last_app_output_in_the_diagnostics_directory() {
    let _docker = support::DOCKER.lock().await;
    let mut setup = Setup::base();
    std::fs::write(setup.worktree.join("chatty-app.ts"), CHATTY_APP)
        .expect("the chatty app can be written into the worktree");
    setup.app.boot = NonEmpty::new(
        "bun".to_owned(),
        vec!["run".to_owned(), "chatty-app.ts".to_owned()],
    );
    let config = runner_config();
    let probe = setup.probe("signup-store");
    let diagnostics = tempfile::tempdir().expect("a temp dir");
    let mut input = setup.input(&probe, RunRole::BaseA, &config);
    input.diagnostics_dir = Some(diagnostics.path());

    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");

    let run = observed.run();
    let detail = match &run.outcome {
        RunOutcome::SetupFailed { detail, .. } => detail.clone(),
        other => panic!("expected a setup failure, got {other:?}"),
    };
    assert!(
        !detail.contains("line 299"),
        "the app output stays out of the outcome: {detail}"
    );
    let kept = std::fs::read_to_string(diagnostics.path().join("app-output.txt"))
        .expect("the app output is kept");
    let lines: Vec<&str> = kept.lines().collect();
    assert_eq!(lines.last(), Some(&"the last message"), "{kept}");
    assert!(lines.contains(&"line 299"), "{kept}");
    assert!(
        !lines.contains(&"line 0"),
        "the oldest lines are dropped: {kept}"
    );
    assert_no_container(&run.id);
}

#[tokio::test]
async fn a_start_that_succeeds_keeps_no_app_output() {
    let _docker = support::DOCKER.lock().await;
    let setup = Setup::base();
    let config = runner_config();
    let probe = setup.probe("signup-store");
    let diagnostics = tempfile::tempdir().expect("a temp dir");
    let mut input = setup.input(&probe, RunRole::BaseA, &config);
    input.diagnostics_dir = Some(diagnostics.path());

    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");

    assert!(
        !diagnostics.path().join("app-output.txt").exists(),
        "a start that works keeps no app output"
    );
    assert_no_container(&observed.run().id);
}
