//! Real probe sequences against the pinned demo app. Each run starts its own container, and
//! every test here runs the real container runtime: none is skipped when docker is present.

mod support;

use std::future::pending;
use std::time::{Duration, Instant};

use acan_intent::{
    CaptureOutcome, Completeness, IncompleteReason, ObservedRun, RunOutcome, RunRole,
};
use acan_witness::{RunnerConfig, run_sequence};
use support::{APP_PORT, Setup, assert_no_container, runner_config};

/// Prints one run's wall time. `cargo test -- --nocapture` shows the lines.
fn timing(name: &str, started: Instant) {
    eprintln!("h01 timing: {name}: {} ms", started.elapsed().as_millis());
}

/// The first observation's HTTP status, by step id.
fn statuses(observed: &ObservedRun) -> Vec<(String, Option<u16>)> {
    observed
        .observations()
        .map(|(_, observation)| {
            (
                observation.step_id.as_str().to_owned(),
                observation.channels.status,
            )
        })
        .collect()
}

/// The status of the observation a step recorded, or a failure naming the step.
fn status_of(observed: &ObservedRun, step: &str) -> u16 {
    statuses(observed)
        .into_iter()
        .find(|(id, _)| id == step)
        .and_then(|(_, status)| status)
        .unwrap_or_else(|| panic!("step {step} recorded no status: {:?}", statuses(observed)))
}

#[tokio::test]
async fn a_stateful_sequence_runs_against_the_demo_and_admits_its_records() {
    let _docker = support::DOCKER.lock().await;
    let setup = Setup::base();
    let config = runner_config();
    let probe = setup.probe("links-create-list-delete");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let started = Instant::now();
    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");
    timing("links-create-list-delete (10 steps, 6 requests)", started);

    let run = observed.run();
    assert!(
        matches!(run.outcome, RunOutcome::Completed {}),
        "the sequence completes: {:?}",
        run.outcome
    );
    assert_eq!(status_of(&observed, "f1"), 201, "signup as alice");
    assert_eq!(status_of(&observed, "f2"), 200, "login as alice");
    assert_eq!(status_of(&observed, "f5"), 201, "signup as bob");
    assert_eq!(status_of(&observed, "f9"), 200, "relogin as alice");
    assert_eq!(
        status_of(&observed, "s1"),
        201,
        "alice creates a link with her bearer token"
    );
    assert_eq!(status_of(&observed, "s3"), 200, "alice lists her links");
    assert_eq!(status_of(&observed, "s5"), 200, "alice deletes her link");
    assert_eq!(status_of(&observed, "s6"), 200, "alice lists again");

    let login = observed
        .observations()
        .find(|(_, observation)| observation.step_id.as_str() == "f2")
        .expect("the login is recorded")
        .1;
    assert!(
        login.channels.cookies.contains_key("session"),
        "the login's session cookie is recorded: {:?}",
        login.channels.cookies.keys().collect::<Vec<_>>()
    );

    let bound: Vec<&str> = run
        .captures
        .iter()
        .map(|capture| capture.binding().as_str())
        .collect();
    assert_eq!(
        bound,
        [
            "alice_session",
            "alice_id",
            "bob_session",
            "bob_id",
            "alice_relogin_session",
            "link_code",
        ],
        "every capture the probe declares is recorded in order"
    );
    assert!(
        run.captures
            .iter()
            .all(|capture| matches!(capture, CaptureOutcome::Captured { .. })),
        "every capture binds a value: {:?}",
        run.captures
    );
    assert_no_container(&run.id);
}

#[tokio::test]
async fn a_step_past_its_deadline_times_out_and_leaves_no_container() {
    let _docker = support::DOCKER.lock().await;
    let setup = Setup::base();
    let config = runner_config();
    // One millisecond cannot cover a round trip through the container, so the step always
    // exceeds its own deadline.
    let probe = setup.probe_with_deadline("signup-store", 1);
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let started = Instant::now();
    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");
    timing("deadline exceeded at s1 (1 ms step deadline)", started);

    let run = observed.run();
    match &run.outcome {
        RunOutcome::TimedOut { step_id } => assert_eq!(step_id.as_str(), "s1", "timed out at s1"),
        other => panic!("expected timed_out at s1, got {other:?}"),
    }
    let (_, observation) = observed
        .observations()
        .next()
        .expect("the timed-out step keeps an observation that says why");
    let reasons = match &observation.completeness {
        Completeness::Incomplete { reasons } => reasons.iter().collect::<Vec<_>>(),
        Completeness::Complete {} => panic!("a timed-out step is incomplete"),
    };
    assert!(
        reasons
            .iter()
            .any(|reason| matches!(reason, IncompleteReason::DeadlineExceeded {})),
        "the reason is the deadline: {reasons:?}"
    );
    assert_no_container(&run.id);
}

#[tokio::test]
async fn cancelling_a_run_removes_its_container() {
    let _docker = support::DOCKER.lock().await;
    let setup = Setup::base();
    let config = runner_config();
    let probe = setup.probe("links-create-list-delete");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    // Cancel while the container is starting or the app is becoming ready.
    let started = Instant::now();
    let observed = run_sequence(&input, tokio::time::sleep(Duration::from_millis(400)))
        .await
        .expect("the runner's record admits");
    timing("cancelled at 400 ms", started);

    let run = observed.run();
    assert!(
        matches!(run.outcome, RunOutcome::Cancelled {}),
        "the run is cancelled: {:?}",
        run.outcome
    );
    assert_no_container(&run.id);
}

#[tokio::test]
async fn cancelling_at_once_starts_nothing_and_leaves_no_container() {
    let _docker = support::DOCKER.lock().await;
    let setup = Setup::base();
    let config = runner_config();
    let probe = setup.probe("links-create-list-delete");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let observed = run_sequence(&input, std::future::ready(()))
        .await
        .expect("the runner's record admits");

    let run = observed.run();
    assert!(
        matches!(run.outcome, RunOutcome::Cancelled {}),
        "the run is cancelled: {:?}",
        run.outcome
    );
    assert_eq!(observed.observations().count(), 0, "no step ran");
    assert_no_container(&run.id);
}

#[tokio::test]
async fn a_missing_docker_program_is_isolation_unavailable_and_nothing_runs_on_the_host() {
    let _docker = support::DOCKER.lock().await;
    let setup = Setup::base();
    let missing = std::env::temp_dir()
        .join("acan-h01-no-such-directory")
        .join("docker.exe");
    assert!(!missing.exists(), "the test docker path must not exist");
    let config = RunnerConfig::new(missing, APP_PORT);
    let probe = setup.probe("signup-store");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");

    match &observed.run().outcome {
        RunOutcome::IsolationUnavailable { detail } => {
            assert!(
                detail.contains("cannot start"),
                "detail names the failed start: {detail}"
            );
        }
        other => panic!("expected isolation_unavailable, got {other:?}"),
    }
    assert_eq!(observed.observations().count(), 0, "no step ran");
}

#[tokio::test]
async fn the_request_budget_ends_the_run_as_an_error_after_the_bound() {
    let _docker = support::DOCKER.lock().await;
    let mut setup = Setup::base();
    setup.app.limits.max_requests = 1;
    let config = runner_config();
    let probe = setup.probe("signup-store");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");

    let run = observed.run();
    match &run.outcome {
        RunOutcome::Error { detail } => {
            assert!(
                detail.contains("1 requests"),
                "the bound is named: {detail}"
            );
        }
        other => panic!("expected an error at the request bound, got {other:?}"),
    }
    assert_eq!(
        observed.observations().count(),
        1,
        "the request inside the bound is recorded; the store read past it is not"
    );
    assert_no_container(&run.id);
}

#[tokio::test]
async fn the_output_bound_marks_a_truncated_body_incomplete() {
    let _docker = support::DOCKER.lock().await;
    let mut setup = Setup::base();
    setup.app.limits.max_output_bytes = 8;
    let config = runner_config();
    let probe = setup.probe("signup-store");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");

    let run = observed.run();
    assert!(
        matches!(run.outcome, RunOutcome::Completed {}),
        "a truncated body is an incomplete record, not a failed run: {:?}",
        run.outcome
    );
    let (_, signup) = observed
        .observations()
        .next()
        .expect("the signup is recorded");
    match &signup.completeness {
        Completeness::Incomplete { reasons } => assert!(
            reasons
                .iter()
                .any(|reason| matches!(reason, IncompleteReason::OutputTruncated {})),
            "the reason is the output bound: {reasons:?}"
        ),
        Completeness::Complete {} => panic!("a body past the output bound is incomplete"),
    }
    assert_no_container(&run.id);
}

#[tokio::test]
async fn a_second_client_address_and_the_controlled_clock_run_in_one_sequence() {
    let _docker = support::DOCKER.lock().await;
    let setup = Setup::base();
    let config = runner_config();
    let probe = setup.probe("link-quota-window");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let started = Instant::now();
    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");
    timing(
        "link-quota-window (17 steps, two clients, two clock advances)",
        started,
    );

    let run = observed.run();
    assert!(
        matches!(run.outcome, RunOutcome::Completed {}),
        "both clock advances succeed and the sequence completes: {:?}",
        run.outcome
    );
    eprintln!("h01 link-quota-window statuses: {:?}", statuses(&observed));
    let (_, from_b) = observed
        .observations()
        .find(|(_, observation)| observation.step_id.as_str() == "s13")
        .expect("the client-b request is recorded");
    assert_eq!(
        from_b
            .request
            .as_ref()
            .map(|request| request.client.as_str()),
        Some("client-b"),
        "the request is sent from the second client"
    );
    assert!(
        (200..300).contains(&status_of(&observed, "s13")),
        "the second client's request is answered: {:?}",
        statuses(&observed)
    );
    assert!(
        (200..300).contains(&status_of(&observed, "s17")),
        "the request after the clock advanced is answered: {:?}",
        statuses(&observed)
    );
    assert_no_container(&run.id);
}

#[tokio::test]
async fn a_probe_request_is_labelled_with_its_route_template() {
    let _docker = support::DOCKER.lock().await;
    let setup = Setup::base();
    let config = runner_config();
    let probe = setup.probe("login-failures");
    let input = setup.input(&probe, RunRole::BaseA, &config);

    let observed = run_sequence(&input, pending::<()>())
        .await
        .expect("the runner's record admits");

    let run = observed.run();
    assert!(
        matches!(run.outcome, RunOutcome::Completed {}),
        "the failed logins complete: {:?}",
        run.outcome
    );
    for step in ["s1", "s2"] {
        let (_, observation) = observed
            .observations()
            .find(|(_, observation)| observation.step_id.as_str() == step)
            .unwrap_or_else(|| panic!("step {step} is recorded"));
        let status = observation.channels.status.unwrap_or(0);
        assert!(
            (400..500).contains(&status),
            "step {step} is refused, got {status}"
        );
        let template = observation
            .request
            .as_ref()
            .map(|request| request.route_template.as_str());
        assert_eq!(
            template,
            Some("/api/login"),
            "step {step} is labelled with its route-table pattern"
        );
    }
    assert_no_container(&run.id);
}
