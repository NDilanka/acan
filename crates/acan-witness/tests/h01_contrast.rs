//! Three real runs of one probe, compared by `acan_intent`: base A, base B and the candidate, each
//! in its own container with its own worktree, store and port.

mod support;

use std::future::pending;
use std::time::Instant;

use acan_intent::compare::{Contrast, check_contrast, compare, derive_masks};
use acan_intent::{CorpusProbe, ObservedRun, Outcome, RunOutcome, RunRole};
use acan_witness::run_sequence;
use support::{Setup, runner_config};

/// Runs `probe` in base A, base B and the candidate at once. Each run has its own container.
async fn run_three(
    probe: &CorpusProbe,
    base_a: &Setup,
    base_b: &Setup,
    candidate: &Setup,
) -> [ObservedRun; 3] {
    let config = runner_config();
    let input_a = base_a.input(probe, RunRole::BaseA, &config);
    let input_b = base_b.input(probe, RunRole::BaseB, &config);
    let input_c = candidate.input(probe, RunRole::Candidate, &config);
    let (a, b, c) = tokio::join!(
        run_sequence(&input_a, pending::<()>()),
        run_sequence(&input_b, pending::<()>()),
        run_sequence(&input_c, pending::<()>()),
    );
    [
        a.expect("base A's record admits"),
        b.expect("base B's record admits"),
        c.expect("the candidate's record admits"),
    ]
}

#[tokio::test]
async fn a_changed_candidate_response_is_the_one_difference_compare_reports() {
    let _docker = support::DOCKER.lock().await;
    let base_a = Setup::base();
    let base_b = Setup::base();
    let candidate = Setup::changed_candidate();
    let probe = base_a.probe("signup-store");

    let started = Instant::now();
    let runs = run_three(&probe, &base_a, &base_b, &candidate).await;
    eprintln!(
        "h01 timing: three runs (signup-store) in parallel: {} ms",
        started.elapsed().as_millis()
    );
    for run in &runs {
        assert!(
            matches!(run.run().outcome, RunOutcome::Completed {}),
            "every run of the contrast completes: {:?}",
            run.run().outcome
        );
    }

    let [base_a_run, base_b_run, candidate_run] = &runs;
    let contrast = Contrast {
        probe: &probe,
        fixture: base_a.fixture(&probe),
        base_a: base_a_run,
        base_b: base_b_run,
        candidate: candidate_run,
    };
    check_contrast(&contrast).expect("the three runs are comparable");
    let masks = derive_masks(&contrast).expect("the base runs derive their masks");
    let comparison = compare(&contrast, &masks).expect("the three runs compare");

    let differences = match &comparison.value().outcome {
        Outcome::Changed { differences } => differences,
        other => panic!("the changed candidate compares as changed, got {other:?}"),
    };
    assert_eq!(
        differences.len(),
        1,
        "the upper-cased username is the only difference: {differences:?}"
    );
    let difference = &differences[0];
    assert_eq!(
        difference.step_id.as_str(),
        "s1",
        "the difference is in the signup"
    );
    assert_eq!(
        difference.path.as_str(),
        "body.username",
        "the difference is in the username"
    );
}

#[tokio::test]
async fn an_unchanged_candidate_compares_as_unchanged() {
    let _docker = support::DOCKER.lock().await;
    let base_a = Setup::base();
    let base_b = Setup::base();
    let candidate = Setup::unchanged_candidate();
    let probe = base_a.probe("signup-store");

    let runs = run_three(&probe, &base_a, &base_b, &candidate).await;
    let [base_a_run, base_b_run, candidate_run] = &runs;
    let contrast = Contrast {
        probe: &probe,
        fixture: base_a.fixture(&probe),
        base_a: base_a_run,
        base_b: base_b_run,
        candidate: candidate_run,
    };
    check_contrast(&contrast).expect("the three runs are comparable");
    let masks = derive_masks(&contrast).expect("the base runs derive their masks");
    let comparison = compare(&contrast, &masks).expect("the three runs compare");

    assert!(
        matches!(comparison.value().outcome, Outcome::Unchanged {}),
        "the unchanged candidate compares as unchanged, got {:?}",
        comparison.value().outcome
    );
}
