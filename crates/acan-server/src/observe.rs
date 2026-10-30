//! Observe and judge one candidate: every probe runs on the base twice and on the prepared
//! merged commit once, each in a fresh container, and the receipt is a pure function of what
//! those runs recorded.
//!
//! Each run starts a fresh app from the commit it names. Protected inputs (`.acan/` and `test/`)
//! are mounted read-only from a snapshot of the protected head, never from a worktree, so a
//! candidate cannot change the contract it is judged by. Observations are stored by hash. A run
//! that cannot start, times out or is cancelled keeps its outcome: it is never dropped and never
//! counted as a pass. A probe whose three runs cannot be compared stops the pass with an error,
//! and nothing is installed.
//!
//! The probe driver shares the container with the candidate's code. The runner's network is
//! `none`, so nothing outside the container reaches the app, but a candidate that controls its own
//! process could alter what the driver reports. The receipt does not claim the driver is isolated
//! from the candidate.

use std::collections::BTreeMap;
use std::fs;
use std::future::{Future, pending, poll_fn};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use acan_git::{Preparation, ProtectedPaths, Repo};
use acan_intent::admit::AdmitError;
use acan_intent::compare::{self, CompareError, Contrast};
use acan_intent::receipt::{self, ProbeResult, ReceiptError, ReceiptInputs};
use acan_intent::{
    AppContract, ArtifactHash, CanonicalError, CheckResult, Comparison, ContainerPolicy,
    ContentHash, CoreDerived, CorpusProbe, DiscrepancyEpisode, EnvHash, EnvManifest, EpisodeKey,
    EpisodeState, EpisodeV1, EvaluatorVersion, Fixture, FixtureId, FixtureSetHash,
    FixtureSetManifest, FoldedLedger, GateContext, GateState, InvalidValue, Mask, ObservationHash,
    ObservedRun, PreparedCommit, ProbeCorpus, ProbeHash, ProbeSetHash, ProbeSetManifest, Receipt,
    RepoPath, RunOutcome, RunRole, RunnerBuild, RunnerHash, RunnerManifest, RuntimeId,
    SandboxPolicyHash, Scope, ScopeConfirmation, ScopeRevision, UnixMillis,
};
use acan_witness::{RunnerConfig, SequenceInput, run_sequence};
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use crate::land;
use crate::observations::{self, ObservationError};

/// The protected directory, relative to a snapshot's root.
const PROTECTED_DIR: &str = ".acan";
/// The app contract inside [`PROTECTED_DIR`].
const APP_FILE: &str = "app.json";
/// The probe corpus inside [`PROTECTED_DIR`].
const CORPUS_FILE: &str = "probes/core.json";
/// The lockfile the app was installed from, relative to a worktree's root.
const LOCKFILE: &str = "bun.lock";
/// The directory the candidate's artifact hashes are read from. Nothing runs in it.
const ARTIFACTS_DIR: &str = "artifacts";
/// The runner build name `acan_witness` stamps on every run. The receipt refuses runs whose runner
/// hash differs, so a drift here fails the gate instead of passing silently.
const RUNNER_BUILD: &str = "acan-witness/0.1.0";
/// The evaluator that classifies the runs: the receipt builder in acan-intent.
const EVALUATOR: &str = "acan-intent/0.1.0";
/// How many times one run that did not set up is started again after its first attempt. A run
/// that never reached a probe step has no evidence, so a second start measures the same thing.
/// More starts only spend containers, and a run still failing to set up stops the pass.
const SETUP_RETRIES: u32 = 2;
/// Why a contrast could not be observed or judged. Every variant stops the candidate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ObserveError {
    /// A protected input in the snapshot is missing or does not decode.
    #[error("cannot read the protected input {}: {detail}", path.display())]
    Protected {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        detail: String,
    },
    /// A file or directory the runner needs could not be created or read.
    #[error("cannot access {}: {source}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// Git could not write a run's tree or the snapshot.
    #[error(transparent)]
    Git(#[from] acan_git::Error),
    /// A run's record was not admitted. That is a runner bug, never a model's doing.
    #[error(transparent)]
    Admit(#[from] AdmitError),
    /// An observation could not be stored.
    #[error(transparent)]
    Store(#[from] ObservationError),
    /// A stored observation hashed differently from the one the run recorded.
    #[error("observation {recorded} was stored as {stored}")]
    HashMismatch {
        /// The hash the run recorded.
        recorded: ObservationHash,
        /// The hash the stored content has.
        stored: ObservationHash,
    },
    /// The probe names a fixture the corpus does not hold.
    #[error("probe fixture {fixture} is not in the corpus")]
    MissingFixture {
        /// The fixture id.
        fixture: FixtureId,
    },
    /// The three runs of a probe cannot be compared.
    #[error("probe {probe} cannot be compared: {source}")]
    Compare {
        /// The probe.
        probe: ProbeHash,
        /// Why the contrast does not check.
        #[source]
        source: CompareError,
    },
    /// A run of a probe did not set up on every attempt, and the probe produced no evidence.
    /// Nothing is judged or installed from it.
    #[error("probe {probe} did not set up as {role:?} after {attempts} attempts: {detail}")]
    SetupFailed {
        /// The probe's label.
        probe: String,
        /// The role whose run did not set up.
        role: RunRole,
        /// How many times the run was started.
        attempts: u32,
        /// What the last attempt reported.
        detail: String,
    },
    /// The receipt could not be built from these inputs.
    #[error(transparent)]
    Receipt(#[from] ReceiptError),
    /// A value has no canonical JSON form.
    #[error(transparent)]
    Hash(#[from] CanonicalError),
    /// A built-in name is not a valid value. A build bug.
    #[error(transparent)]
    Name(#[from] InvalidValue),
}

/// One admitted run and the hashes of the observations it stored, in step order.
#[derive(Debug, Clone)]
pub(crate) struct RecordedRun {
    /// The admitted run, with its observations.
    pub(crate) observed: ObservedRun,
    /// The stored observations' hashes, in the run's step order. The receipt reads the runs, not
    /// these hashes, so nothing reads them yet.
    #[allow(
        dead_code,
        reason = "kept with the run so a later receipt can name the stored observations it read"
    )]
    pub(crate) observations: Vec<ObservationHash>,
}

/// The three runs of one probe.
#[derive(Debug, Clone)]
pub(crate) struct ProbeRuns {
    /// The probe, with its normalization plan.
    pub(crate) probe: CorpusProbe,
    /// The first base run.
    pub(crate) base_a: RecordedRun,
    /// The second base run.
    pub(crate) base_b: RecordedRun,
    /// The candidate run.
    pub(crate) candidate: RecordedRun,
}

/// What one observe pass runs against. Every directory must be missing or empty: the runs write
/// into them, so a pass never reuses another pass's worktrees.
#[derive(Debug)]
pub(crate) struct Plan<'a> {
    /// The repository the protected head and the candidate live in.
    pub(crate) repo: &'a Repo,
    /// The preparation whose protected head is the base the runs start from. Its merged commit is
    /// the protected head itself.
    pub(crate) base: &'a Preparation,
    /// The preparation whose merged commit is the candidate under test.
    pub(crate) candidate: &'a Preparation,
    /// The protected-head snapshot the containers mount protected paths from. See [`snapshot`].
    pub(crate) snapshot: &'a Path,
    /// The directory the per-run worktrees are written under.
    pub(crate) scratch: &'a Path,
    /// The directory observations are stored under, by hash.
    pub(crate) observations: &'a Path,
    /// The protected app contract, read from the snapshot.
    pub(crate) app: &'a AppContract,
    /// The protected probe corpus, read from the snapshot.
    pub(crate) corpus: &'a ProbeCorpus,
    /// The container policy the runs apply.
    pub(crate) policy: &'a ContainerPolicy,
    /// How the host reaches docker and the app.
    pub(crate) runner: &'a RunnerConfig,
    /// The host's container permits. A container run holds one for as long as it runs. The witness
    /// runs take from the same pool, so the containers of every task together stay within it.
    pub(crate) slots: &'a Semaphore,
    /// How many probes of the contrast are in flight at once. One runs them in corpus order.
    pub(crate) probes_at_once: usize,
}

/// The order a candidate is judged against: its confirmed scope, the confirmation of that scope,
/// and the folded ruling log in force.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Order<'a> {
    /// The confirmed scope.
    pub(crate) scope: &'a Scope,
    /// The human confirmation of that scope.
    pub(crate) confirmation: &'a ScopeConfirmation,
    /// The ruling log, folded.
    pub(crate) ledger: &'a CoreDerived<FoldedLedger>,
    /// The discrepancy episodes the receipt tracks. An episode in `residue` or `contested`
    /// state makes its item wait for a human ruling.
    pub(crate) episodes: &'a [DiscrepancyEpisode],
}

/// One probe's contrast with the masks and comparison the core derives from it.
#[derive(Debug)]
pub(crate) struct Judgement<'a> {
    /// The three runs, borrowed from the observe pass.
    pub(crate) contrast: Contrast<'a>,
    /// The masks derived from the base runs.
    pub(crate) masks: Vec<CoreDerived<Mask>>,
    /// The comparison under those masks.
    pub(crate) comparison: CoreDerived<Comparison>,
}

/// The inputs of a context that do not come from one preparation: the protected contract, the
/// corpus and policy hashes, and the candidate's own artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Facts {
    artifacts: BTreeMap<RepoPath, ArtifactHash>,
    probe_set: ProbeSetHash,
    fixture_set: FixtureSetHash,
    runner: RunnerHash,
    runtime: RuntimeId,
    env: EnvHash,
    policy: SandboxPolicyHash,
    evaluator: EvaluatorVersion,
}

/// A judged candidate: the context it was tested under and the receipt the core built from it.
#[derive(Debug, Clone)]
pub(crate) struct Judged {
    /// The tested context. The receipt is void for any other.
    pub(crate) context: GateContext,
    /// The receipt, whose gate state is `Blocked` when it does not pass.
    pub(crate) receipt: Receipt,
}

/// The container policy every run applies: the contract default, which the witness and the
/// fixture `container-policy.json` also use. It pins the image by digest, gives no network, and
/// mounts `.acan` read-only from the snapshot.
pub(crate) fn runner_policy() -> ContainerPolicy {
    ContainerPolicy::default_run()
}

/// The paths the preparation of every candidate restores from the protected head: the policy's
/// protected mounts, which are exactly the paths the containers mount from the snapshot.
pub(crate) fn protected_paths(policy: &ContainerPolicy) -> ProtectedPaths {
    ProtectedPaths::new(policy.mounts.protected.iter().cloned())
}

/// Writes the protected paths of `base`'s protected head into `dir`, the snapshot the containers
/// mount protected paths from. `dir` is created when missing.
///
/// # Errors
///
/// Returns [`ObserveError`] when the directory cannot be created or git cannot write the paths.
pub(crate) async fn snapshot(
    repo: &Repo,
    base: &Preparation,
    dir: &Path,
) -> Result<(), ObserveError> {
    fs::create_dir_all(dir).map_err(|source| io_error(dir, source))?;
    repo.restore_protected(base, dir).await?;
    Ok(())
}

/// Reads the protected app contract and probe corpus from a snapshot.
///
/// # Errors
///
/// Returns [`ObserveError::Protected`] when either file is missing or does not decode.
pub(crate) fn read_protected(snapshot: &Path) -> Result<(AppContract, ProbeCorpus), ObserveError> {
    let root = snapshot.join(PROTECTED_DIR);
    let app = read_json(&root.join(APP_FILE))?;
    let corpus = read_json(&root.join(CORPUS_FILE))?;
    Ok((app, corpus))
}

/// The content hash of the lockfile in a materialized worktree.
///
/// # Errors
///
/// Returns [`ObserveError::Io`] when the lockfile cannot be read.
pub(crate) fn lockfile_hash(worktree: &Path) -> Result<ArtifactHash, ObserveError> {
    let path = worktree.join(LOCKFILE);
    let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
    Ok(ArtifactHash::from(ContentHash::of_bytes(&bytes)))
}

/// Runs every probe of the corpus: base A, base B, and the candidate, each in its own container
/// and its own worktree, with the three of one probe at once. Up to `probes_at_once` probes are in
/// flight, and each container run takes one of the host's permits in `plan.slots`. A run that does
/// not set up is started again by [`settle`], up to [`SETUP_RETRIES`] times, and each start is
/// logged.
///
/// A probe that fails stops the pass. No probe that has not started yet starts, the probes already
/// running finish, and the first failure in corpus order is returned. No partial result is built.
///
/// # Errors
///
/// Returns [`ObserveError`] when a worktree cannot be written, a record does not admit, or a run
/// still does not set up after its last start. A run that did not complete for any other reason
/// is not an error: its outcome is in the record.
///
/// # Panics
///
/// Panics when `plan.probes_at_once` is zero, because no probe could ever start.
pub(crate) async fn observe(plan: &Plan<'_>) -> Result<Vec<ProbeRuns>, ObserveError> {
    assert!(
        plan.probes_at_once > 0,
        "a contrast needs at least one probe in flight, got {}",
        plan.probes_at_once
    );
    let corpus: &ProbeCorpus = plan.corpus;
    let turns = Semaphore::new(plan.probes_at_once);
    let failed = AtomicBool::new(false);
    let outcomes = join_all(
        corpus
            .probes
            .iter()
            .enumerate()
            .map(|(index, probe)| observe_probe(plan, index, probe, &turns, &failed)),
    )
    .await;
    let mut runs = Vec::with_capacity(corpus.probes.len());
    for outcome in outcomes {
        // A probe is skipped only after another probe failed. That failure is an `Err` in this
        // list, and `?` returns it before the loop can finish, so a finished loop has no skips.
        if let Some(probe_runs) = outcome? {
            runs.push(probe_runs);
        }
    }
    Ok(runs)
}

/// Runs the probe at `index` unless another probe has already failed. The probe holds one of
/// `turns` for its whole run, so no more than `probes_at_once` probes run at once.
///
/// # Errors
///
/// Returns the error of the probe's own runs. A skipped probe is `Ok(None)`.
async fn observe_probe(
    plan: &Plan<'_>,
    index: usize,
    probe: &CorpusProbe,
    turns: &Semaphore,
    failed: &AtomicBool,
) -> Result<Option<ProbeRuns>, ObserveError> {
    let _turn = turns
        .acquire()
        .await
        .expect("the probe turns are never closed");
    if failed.load(Ordering::Acquire) {
        return Ok(None);
    }
    let result = probe_runs(plan, index, probe).await;
    if result.is_err() {
        failed.store(true, Ordering::Release);
    }
    result.map(Some)
}

/// Runs base A, base B and the candidate of the probe at `index`, each in its own worktree.
///
/// # Errors
///
/// Returns [`ObserveError`] when the fixture is missing or one of the runs fails.
async fn probe_runs(
    plan: &Plan<'_>,
    index: usize,
    probe: &CorpusProbe,
) -> Result<ProbeRuns, ObserveError> {
    let fixture = fixture_of(plan.corpus, probe)?;
    let probe_dir = plan.scratch.join(format!("probe-{index}"));
    let (dir_a, dir_b, dir_c) = (
        probe_dir.join("base-a"),
        probe_dir.join("base-b"),
        probe_dir.join("candidate"),
    );
    let (base_a, base_b, candidate) = tokio::join!(
        settled(plan, probe, fixture, plan.base, &dir_a, RunRole::BaseA),
        settled(plan, probe, fixture, plan.base, &dir_b, RunRole::BaseB),
        settled(
            plan,
            probe,
            fixture,
            plan.candidate,
            &dir_c,
            RunRole::Candidate
        ),
    );
    Ok(ProbeRuns {
        probe: probe.clone(),
        base_a: base_a?,
        base_b: base_b?,
        candidate: candidate?,
    })
}

/// Polls every future until each one has finished, and returns the outputs in the order given.
/// This crate does not depend on `futures`, so the probe fan-out polls its futures itself.
async fn join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let mut running: Vec<Option<Pin<Box<F>>>> = futures
        .into_iter()
        .map(|future| Some(Box::pin(future)))
        .collect();
    let mut outputs: Vec<Option<F::Output>> = running.iter().map(|_| None).collect();
    poll_fn(|cx| {
        let mut waiting = false;
        for (slot, output) in running.iter_mut().zip(outputs.iter_mut()) {
            let Some(future) = slot.as_mut() else {
                continue;
            };
            match future.as_mut().poll(cx) {
                Poll::Ready(value) => {
                    *output = Some(value);
                    *slot = None;
                }
                Poll::Pending => waiting = true,
            }
        }
        if waiting {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    outputs
        .into_iter()
        .map(|output| output.expect("every future finished before the join returned"))
        .collect()
}

/// Runs one role of `probe` on `preparation` and records it. Every start stages its own worktree
/// under `dir`, numbered by attempt, so no start reuses another's files.
async fn settled(
    plan: &Plan<'_>,
    probe: &CorpusProbe,
    fixture: &Fixture,
    preparation: &Preparation,
    dir: &Path,
    role: RunRole,
) -> Result<RecordedRun, ObserveError> {
    settle(
        &probe.label,
        role,
        |attempt| async move {
            let staged = stage(plan.repo, preparation, &dir.join(attempt.to_string())).await?;
            let input = sequence(plan, probe, fixture, &staged, role);
            // The permit covers the container run only. A run waiting for it has not started, and
            // its budget starts inside `run_sequence`, so the wait never spends that budget.
            let run = {
                let _slot = plan
                    .slots
                    .acquire()
                    .await
                    .expect("the container permits are never closed");
                run_sequence(&input, pending::<()>()).await?
            };
            record(plan.observations, run)
        },
        |run: &RecordedRun| {
            let evidence = run.observed.observations().any(|(_, observation)| {
                probe
                    .spec
                    .steps
                    .iter()
                    .any(|step| step.id() == &observation.step_id)
            });
            did_not_set_up(&run.observed.run().outcome, evidence)
        },
    )
    .await
}

/// Runs `attempt` until its run sets up, starting it again while it does not, at most
/// [`SETUP_RETRIES`] times after the first start. `setup_failure` returns the reason a run did
/// not set up, or `None` when it did. Every start that does not set up is logged.
///
/// # Errors
///
/// Returns [`ObserveError::SetupFailed`] with the start count and the last reason when every start
/// does not set up, and whatever `attempt` returns otherwise.
async fn settle<T, Attempt, Fut>(
    probe: &str,
    role: RunRole,
    mut attempt: Attempt,
    setup_failure: impl Fn(&T) -> Option<String>,
) -> Result<T, ObserveError>
where
    Attempt: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, ObserveError>>,
{
    let mut number: u32 = 1;
    loop {
        let run = attempt(number).await?;
        let Some(detail) = setup_failure(&run) else {
            return Ok(run);
        };
        if number > SETUP_RETRIES {
            tracing::error!(probe, ?role, attempts = number, %detail, "a probe run never set up");
            return Err(ObserveError::SetupFailed {
                probe: probe.to_owned(),
                role,
                attempts: number,
                detail,
            });
        }
        tracing::warn!(
            probe,
            ?role,
            attempt = number,
            %detail,
            "a probe run did not set up; starting it again"
        );
        number += 1;
    }
}

/// Why a run did not set up, when that is all it did. Only a `SetupFailed` outcome counts, and
/// only when no probe step produced an observation: a run with evidence is never started again.
fn did_not_set_up(outcome: &RunOutcome, evidence: bool) -> Option<String> {
    match outcome {
        RunOutcome::SetupFailed { detail, .. } if !evidence => Some(detail.clone()),
        _ => None,
    }
}

/// Derives the masks and the comparison of every probe's runs. The first probe whose runs cannot
/// be compared stops the pass.
///
/// # Errors
///
/// Returns [`ObserveError::Compare`] for the first contrast that does not check or compare.
pub(crate) fn compare_runs<'a>(
    runs: &'a [ProbeRuns],
    corpus: &'a ProbeCorpus,
) -> Result<Vec<Judgement<'a>>, ObserveError> {
    runs.iter()
        .map(|runs| -> Result<Judgement<'a>, ObserveError> {
            let fixture = fixture_of(corpus, &runs.probe)?;
            let contrast = Contrast {
                probe: &runs.probe,
                fixture,
                base_a: &runs.base_a.observed,
                base_b: &runs.base_b.observed,
                candidate: &runs.candidate.observed,
            };
            let probe = runs.probe.spec.probe_hash()?;
            let compared = |source: CompareError| ObserveError::Compare { probe, source };
            let masks = compare::derive_masks(&contrast).map_err(&compared)?;
            let comparison = compare::compare(&contrast, &masks).map_err(&compared)?;
            Ok(Judgement {
                contrast,
                masks,
                comparison,
            })
        })
        .collect()
}

/// The receipt inputs of each judgement, in the same order.
pub(crate) fn results<'j>(judgements: &'j [Judgement<'_>]) -> Vec<ProbeResult<'j>> {
    judgements
        .iter()
        .map(|judgement| ProbeResult {
            contrast: judgement.contrast,
            masks: &judgement.masks,
            comparison: &judgement.comparison,
        })
        .collect()
}

impl Facts {
    /// The facts of the protected inputs and the candidate's artifacts.
    ///
    /// # Errors
    ///
    /// Returns [`ObserveError`] when a manifest cannot be hashed or a built-in name is invalid.
    fn of(
        plan: &Plan<'_>,
        order: &Order<'_>,
        artifacts: BTreeMap<RepoPath, ArtifactHash>,
    ) -> Result<Self, ObserveError> {
        let retired = &order.ledger.value().retired_probes;
        let probe_set = ProbeSetManifest::of(plan.corpus, retired)?.probe_set_hash()?;
        let fixture_set = FixtureSetManifest::of(plan.corpus)?.fixture_set_hash()?;
        let runner = RunnerManifest {
            build: RunnerBuild::from_str(RUNNER_BUILD)?,
            app: plan.app.clone(),
        }
        .runner_hash()?;
        // The controlled environment is the policy's variables, with the app's port variable set
        // to the port the driver gives the app, as the witness does when it starts a run.
        let mut vars = plan.policy.environment.clone();
        vars.insert(plan.app.port_env.clone(), plan.runner.port.to_string());
        let env = EnvManifest {
            vars,
            clock_start: UnixMillis(0),
            seed: 0,
        }
        .env_hash()?;
        Ok(Self {
            artifacts,
            probe_set,
            fixture_set,
            runner,
            runtime: plan.app.runtime.clone(),
            env,
            policy: plan.policy.sandbox_policy_hash()?,
            evaluator: EvaluatorVersion::from_str(EVALUATOR)?,
        })
    }
}

impl Order<'_> {
    /// The tested context of `preparation` under these facts and this order's ruling log.
    fn context(&self, facts: &Facts, preparation: &Preparation) -> GateContext {
        preparation.bind(GateContext {
            protected_head: preparation.protected_head().clone(),
            merged_commit: preparation.merged_commit().clone(),
            overlay_hash: preparation.overlay_hash(),
            effective_artifact_hashes: facts.artifacts.clone(),
            ledger_head: self.ledger.value().head,
            scope_hash: self.confirmation.scope_hash,
            probe_set_hash: facts.probe_set,
            fixture_set_hash: facts.fixture_set,
            runner_hash: facts.runner,
            evaluator_version: facts.evaluator.clone(),
            runtime: facts.runtime.clone(),
            controlled_env_hash: facts.env,
            sandbox_policy_hash: facts.policy,
        })
    }
}

/// Observes every probe, judges the contrasts and builds the receipt of the candidate.
///
/// # Errors
///
/// Returns [`ObserveError`] when a run cannot be recorded or a contrast does not check. Such a
/// candidate has no receipt and nothing may install it.
pub(crate) async fn judge(plan: &Plan<'_>, order: &Order<'_>) -> Result<Judged, ObserveError> {
    let facts = Facts::of(plan, order, artifacts(plan).await?)?;
    // Three containers per probe make this future large, so it is boxed where it is awaited.
    let runs = Box::pin(observe(plan)).await?;
    let judgements = compare_runs(&runs, plan.corpus)?;
    let results = results(&judgements);
    let context = order.context(&facts, plan.candidate);
    let mut episodes = order.episodes.to_vec();
    let first = build(
        order,
        plan,
        &results,
        &context,
        &episodes,
        GateState::Prepared,
    )?;
    let opened = open_episodes(&first)?;
    let mut receipt = if opened.is_empty() {
        first
    } else {
        // The failing items the order holds no episode for name the ones opened here, so the
        // receipt can be recorded: the origin refuses a failing item that names no episode.
        episodes.extend(opened);
        build(
            order,
            plan,
            &results,
            &context,
            &episodes,
            GateState::Prepared,
        )?
    };
    if !land::blockers(&receipt).is_empty() {
        receipt = build(
            order,
            plan,
            &results,
            &context,
            &episodes,
            GateState::Blocked,
        )?;
    }
    Ok(Judged { context, receipt })
}

/// The open episodes the failing scope items of `receipt` need. The receipt builder attaches an
/// episode only when the order already holds one for the item's selector, so a first failure
/// names none. Each failing item with a selector and no episode gets one, keyed by that selector
/// and its first failing obligation. The key is the episode's identity, so the same failure gets
/// the same episode on every pass. An item with no selector cannot be keyed, so it gets none and
/// stays unnamed.
///
/// # Errors
///
/// Returns [`ObserveError::Hash`] when a key does not serialize.
fn open_episodes(receipt: &Receipt) -> Result<Vec<DiscrepancyEpisode>, ObserveError> {
    let mut opened: Vec<DiscrepancyEpisode> = Vec::new();
    for item in &receipt.items {
        if item.class.counts_toward_pass() || item.episode_id.is_some() {
            continue;
        }
        let (Some(selector), Some(step)) = (item.selector.clone(), item.sequence.first()) else {
            continue;
        };
        let obligation_id = item
            .evaluation
            .obligations
            .iter()
            .find(|check| !matches!(check.result, CheckResult::Pass {}))
            .map(|check| check.obligation_id.clone());
        let id = EpisodeKey {
            order: receipt.order.clone(),
            selector: selector.clone(),
            obligation_id: obligation_id.clone(),
        }
        .episode_id()?;
        if opened.iter().any(|episode| episode.id == id) {
            continue;
        }
        opened.push(DiscrepancyEpisode {
            version: EpisodeV1,
            id,
            order: receipt.order.clone(),
            scope_lineage: vec![ScopeRevision {
                rev: receipt.scope_rev,
                scope_hash: receipt.scope_hash,
            }],
            probe_hash: item.probe_hash,
            step_id: step.step_id.clone(),
            selector,
            obligation_id,
            evidence_revisions: Vec::new(),
            candidate_attempts: Vec::new(),
            bounce_count: 0,
            state: EpisodeState::Open,
        });
    }
    Ok(opened)
}

/// The context the candidate would be tested under right now, from the current ruling log and the
/// protected inputs of `plan`. The final gate compares it with the receipt's context.
///
/// # Errors
///
/// Returns [`ObserveError`] when the candidate's lockfile cannot be read or a hash fails.
pub(crate) async fn current(
    plan: &Plan<'_>,
    order: &Order<'_>,
) -> Result<GateContext, ObserveError> {
    let facts = Facts::of(plan, order, artifacts(plan).await?)?;
    Ok(order.context(&facts, plan.candidate))
}

/// The receipt of `results` under `context`, in `state`.
fn build(
    order: &Order<'_>,
    plan: &Plan<'_>,
    results: &[ProbeResult<'_>],
    context: &GateContext,
    episodes: &[DiscrepancyEpisode],
    state: GateState,
) -> Result<Receipt, ObserveError> {
    let prepared = PreparedCommit {
        base_commit: plan.base.protected_head().clone(),
        context: context.clone(),
    };
    let inputs = ReceiptInputs {
        scope: order.scope,
        confirmation: order.confirmation,
        prepared: &prepared,
        ledger: order.ledger,
        corpus: plan.corpus,
        results,
        anchors: &[],
        episodes,
        contests: &[],
        gate_state: state,
    };
    Ok(receipt::build_receipt(&inputs)?.value().clone())
}

/// The candidate's own artifact hashes: the lockfile of its merged tree.
async fn artifacts(plan: &Plan<'_>) -> Result<BTreeMap<RepoPath, ArtifactHash>, ObserveError> {
    let dir = plan.scratch.join(ARTIFACTS_DIR);
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|source| io_error(&dir, source))?;
    }
    let lock = materialize(plan.repo, plan.candidate, &dir).await?;
    Ok(BTreeMap::from([(LOCKFILE.parse()?, lock)]))
}

/// One run's worktree, the commit it is at, and the hash of its lockfile.
struct Staged {
    worktree: PathBuf,
    commit: acan_intent::CommitSha,
    lockfile: ArtifactHash,
}

/// Writes `preparation`'s merged tree into `dir` and records what a run needs to know about it.
async fn stage(repo: &Repo, preparation: &Preparation, dir: &Path) -> Result<Staged, ObserveError> {
    let lockfile = materialize(repo, preparation, dir).await?;
    Ok(Staged {
        worktree: dir.to_path_buf(),
        commit: preparation.merged_commit().clone(),
        lockfile,
    })
}

/// Writes `preparation`'s merged tree into `dir` and returns the hash of its lockfile.
async fn materialize(
    repo: &Repo,
    preparation: &Preparation,
    dir: &Path,
) -> Result<ArtifactHash, ObserveError> {
    repo.materialize(preparation, dir).await?;
    lockfile_hash(dir)
}

/// Stores each observation of `run` under `dir` and keeps the run with their hashes.
fn record(dir: &Path, run: ObservedRun) -> Result<RecordedRun, ObserveError> {
    let mut observations = Vec::new();
    for (recorded, observation) in run.observations() {
        let stored = observations::put(dir, observation)?;
        if stored != *recorded {
            return Err(ObserveError::HashMismatch {
                recorded: *recorded,
                stored,
            });
        }
        observations.push(stored);
    }
    Ok(RecordedRun {
        observed: run,
        observations,
    })
}

/// One run's input: the shared protected inputs, plus the worktree and commit it runs on.
fn sequence<'a>(
    plan: &Plan<'a>,
    probe: &'a CorpusProbe,
    fixture: &'a Fixture,
    staged: &'a Staged,
    role: RunRole,
) -> SequenceInput<'a> {
    let corpus: &'a ProbeCorpus = plan.corpus;
    SequenceInput {
        probe,
        fixture,
        routes: &corpus.routes,
        app: plan.app,
        policy: plan.policy,
        commit: staged.commit.clone(),
        role,
        lockfile_hash: staged.lockfile,
        worktree: &staged.worktree,
        snapshot: plan.snapshot,
        config: plan.runner,
    }
}

fn fixture_of<'c>(
    corpus: &'c ProbeCorpus,
    probe: &CorpusProbe,
) -> Result<&'c Fixture, ObserveError> {
    corpus
        .fixtures
        .iter()
        .find(|fixture| fixture.id == probe.spec.fixture)
        .ok_or_else(|| ObserveError::MissingFixture {
            fixture: probe.spec.fixture.clone(),
        })
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, ObserveError> {
    let text = fs::read_to_string(path).map_err(|source| ObserveError::Protected {
        path: path.to_path_buf(),
        detail: source.to_string(),
    })?;
    serde_json::from_str(&text).map_err(|source| ObserveError::Protected {
        path: path.to_path_buf(),
        detail: source.to_string(),
    })
}

fn io_error(path: &Path, source: io::Error) -> ObserveError {
    ObserveError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use acan_intent::ContainerNetwork;

    use super::*;

    #[test]
    fn the_runner_policy_is_offline_and_pinned_by_digest() {
        let policy = runner_policy();
        assert!(
            matches!(policy.network, ContainerNetwork::None {}),
            "runs have no network: {:?}",
            policy.network
        );
        assert!(
            policy.image.digest.is_some(),
            "the image is pinned by digest"
        );
    }

    #[tokio::test]
    async fn a_setup_failure_is_started_again_and_the_run_that_sets_up_is_taken() {
        let mut calls = 0;
        let taken = settle(
            "login",
            RunRole::BaseA,
            |attempt| {
                calls += 1;
                async move { Ok::<u32, ObserveError>(attempt) }
            },
            |attempt: &u32| (*attempt < 2).then(|| "the app did not become ready".to_owned()),
        )
        .await
        .expect("the second attempt sets up");
        assert_eq!(taken, 2, "the run taken is the second attempt");
        assert_eq!(calls, 2, "a setup failure is started again once");
    }

    #[tokio::test]
    async fn a_setup_failure_stops_after_two_retries_with_its_reason() {
        let mut calls = 0;
        let error = settle(
            "login",
            RunRole::Candidate,
            |attempt| {
                calls += 1;
                async move { Ok::<u32, ObserveError>(attempt) }
            },
            |_: &u32| Some("the app did not become ready".to_owned()),
        )
        .await
        .expect_err("every attempt fails to set up");
        assert_eq!(calls, 3, "one attempt and two retries");
        assert!(
            matches!(error, ObserveError::SetupFailed { attempts: 3, .. }),
            "the error counts the attempts: {error:?}"
        );
        assert!(
            error.to_string().contains("did not become ready"),
            "the error names the reason: {error}"
        );
    }

    #[tokio::test]
    async fn a_run_that_is_not_a_setup_failure_is_never_started_again() {
        let mut calls = 0;
        let taken = settle(
            "login",
            RunRole::BaseB,
            |attempt| {
                calls += 1;
                async move { Ok::<u32, ObserveError>(attempt) }
            },
            |_: &u32| None,
        )
        .await
        .expect("the first attempt is taken");
        assert_eq!(taken, 1);
        assert_eq!(calls, 1, "a run that set up is not retried");
    }

    #[test]
    fn only_a_setup_failure_with_no_probe_observation_is_started_again() {
        let setup = RunOutcome::SetupFailed {
            step_id: None,
            detail: "the app did not become ready".to_owned(),
        };
        assert_eq!(
            did_not_set_up(&setup, false),
            Some("the app did not become ready".to_owned())
        );
        assert_eq!(
            did_not_set_up(&setup, true),
            None,
            "a run that produced a probe observation is never started again"
        );
        assert_eq!(did_not_set_up(&RunOutcome::Completed {}, false), None);
        assert_eq!(
            did_not_set_up(
                &RunOutcome::Error {
                    detail: "a probe step failed".to_owned()
                },
                false
            ),
            None,
            "a probe error is not a setup failure"
        );
    }

    #[test]
    fn the_preparation_protects_every_path_the_containers_mount() {
        let policy = runner_policy();
        let protected = protected_paths(&policy);
        for path in &policy.mounts.protected {
            assert!(
                protected.iter().any(|kept| kept == path),
                "{path} is restored from the protected head"
            );
        }
        assert!(
            protected.covers(".acan/probes/core.json"),
            "the protected inputs are always protected"
        );
    }
}
