//! The final gate end to end, against the real acan-demo at its pinned commit and a real Docker.
//!
//! A candidate that changes nothing a probe reads lands on the protected branch. A candidate that
//! seeds a regression in the signup response is blocked, and its receipt names the must-keep rule
//! it breaks. A runtime that cannot isolate a run never installs anything. A ruling that is
//! waiting for a human survives a restart of the host, and land refuses it until its answer is in
//! the log.
//!
//! The live tests drive a host built over a clone of the demo with core commands only: the owner
//! posts the order and confirms the scope, the worker reads back, claims, declares and reports the
//! built commit. The job loop does the rest: it prepares the candidate, observes and judges it,
//! records the receipt, and lands it through the final gate. The clone is the only thing written,
//! and `ACAN_DEMO_DIR` is only read.

#[path = "../src/observations.rs"]
#[allow(
    dead_code,
    reason = "the gate's observation store has callers outside this test"
)]
mod observations;

#[path = "../src/observe.rs"]
#[allow(
    dead_code,
    reason = "the gate's observe pass has callers outside this test"
)]
mod observe;

#[path = "../src/land.rs"]
#[allow(
    dead_code,
    reason = "the gate's final check and install have callers outside this test"
)]
mod land;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::str::FromStr;
use std::time::Duration;

use acan_core::{
    AgentCmd, AgentId, Command, CommitId, Event, Fence, GateRunId, HumanCmd, LaneVerdict,
    MachineKey, MemberId, OrderId, Origin, ScopeBinding, Seq, Stage, SystemCmd, TaskId, TaskState,
    Timestamp, World, WorldId, WorldRef,
};
use acan_git::{InstallOutcome, Preparation, PrepareOutcome, ProtectedPaths, Repo};
use acan_intent::{
    AppContract, CommitSha, ContainerPolicy, CoreDerived, DiscrepancyEpisode, EpisodeId,
    EpisodeKey, FixtureId, FoldedLedger, ItemClass, ItemState, LedgerEvent, Outcome, ProbeCorpus,
    Receipt, Scope, ScopeConfirmation, canonical_episode_selector, fold,
};
use acan_llm::{Reply, Role, Stub, UsageLedger};
use acan_server::{
    DEFAULT_PROBE_CONCURRENCY, DEFAULT_WITNESS_CONCURRENCY, EventStore, Host, HostParts, Tokens,
    scan, spawn_jobs,
};
use acan_witness::RunnerConfig;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;

/// The environment variable that names the acan-demo checkout. The tests only read it.
const DEMO_DIR_VAR: &str = "ACAN_DEMO_DIR";
/// The acan-demo commit the protected branch is pinned to.
const DEMO_COMMIT: &str = "f91962156841332d5753d869554839f7c4cdea79";
/// The protected branch of the pinned clone.
const PROTECTED_BRANCH: &str = "main";
/// The port the app listens on inside its own container.
const APP_PORT: u16 = 3000;
/// The order the scope confirms.
const ORDER: &str = "o-demo-signup";
/// The protected probe corpus, which every candidate is judged by and no candidate may change.
const PROTECTED_CORPUS: &str = ".acan/probes/core.json";
/// The must-keep rule that keeps the signup answer as it is.
const SIGNUP_RULE: &str = "k-signup-body";
/// The signup handler's answer, as the pinned demo has it.
const SIGNUP_RESPONSE: &str = "return { id: user.id, username: user.username };";
/// The seeded regression: the answer upper-cases the username, so the answer differs from base.
const REGRESSED_SIGNUP_RESPONSE: &str =
    "return { id: user.id, username: user.username.toUpperCase() };";
/// The obligation of the signup rule that the seeded regression breaks.
const SIGNUP_OBLIGATION: &str = "obl-signup-body";
/// The ruling lineage the human answer belongs to.
const RULING: &str = "r-2";
/// The probe hash the residue episode names. The episode's identity does not include it, so any
/// valid hash will do for this test.
const EPISODE_PROBE: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";
/// The may-change item of the scope the Docker-free tests confirm.
const LIMIT_ITEM: &str = "i-limit";
/// The obligation that item must meet.
const LIMIT_OBLIGATION: &str = "obl-limit";

/// Serializes the Docker tests, so their containers do not compete for the same CPU budget and
/// their sequences stay inside the 30 s budget.
static DOCKER: Mutex<()> = Mutex::const_new(());

/// A clone of the acan-demo, reset to the pinned commit on the protected branch.
struct Demo {
    _dir: TempDir,
    root: PathBuf,
    repo: Repo,
}

impl Demo {
    async fn pinned() -> Self {
        let source = std::env::var_os(DEMO_DIR_VAR).unwrap_or_else(|| {
            panic!("set {DEMO_DIR_VAR} to a checkout of acan-demo at {DEMO_COMMIT}")
        });
        let dir = tempfile::tempdir().expect("a temporary directory for the clone");
        let root = dir.path().join("demo");
        git(
            dir.path(),
            &[
                "clone",
                "--quiet",
                "--no-hardlinks",
                &source.to_string_lossy(),
                &root.to_string_lossy(),
            ],
        );
        git(
            &root,
            &["checkout", "--quiet", "-B", PROTECTED_BRANCH, DEMO_COMMIT],
        );
        let repo = Repo::open_with_protected(&root, PROTECTED_BRANCH)
            .await
            .expect("the clone opens with its protected branch");
        Self {
            _dir: dir,
            root,
            repo,
        }
    }

    /// The protected head.
    fn head(&self) -> CommitSha {
        CommitSha::from_str(&git(&self.root, &["rev-parse", PROTECTED_BRANCH]))
            .expect("the protected head is a commit")
    }

    /// Commits `edit` on a new branch off the protected head, and returns the commit. The checkout
    /// is left on the protected branch, which install needs.
    fn candidate(&self, branch: &str, edit: impl FnOnce(&Path)) -> CommitSha {
        git(
            &self.root,
            &["checkout", "--quiet", "-b", branch, PROTECTED_BRANCH],
        );
        edit(&self.root);
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "--quiet", "-m", branch]);
        let commit = CommitSha::from_str(&git(&self.root, &["rev-parse", "HEAD"]))
            .expect("the candidate is a commit");
        git(&self.root, &["checkout", "--quiet", PROTECTED_BRANCH]);
        commit
    }

    /// Commits `edit` on the protected branch itself, which moves its head, and returns the new
    /// head. The checkout must be on the protected branch, as [`Demo::candidate`] leaves it.
    fn advance(&self, edit: impl FnOnce(&Path)) -> CommitSha {
        edit(&self.root);
        git(&self.root, &["add", "-A"]);
        git(
            &self.root,
            &["commit", "--quiet", "-m", "advance the protected head"],
        );
        self.head()
    }
}

/// Everything one gate pass reads: the base preparation, the candidate preparation, the protected
/// inputs of the base, the scope and its confirmation, and the ruling log.
struct Gate {
    _dir: TempDir,
    base: Preparation,
    candidate: Preparation,
    snapshot: PathBuf,
    scratch: PathBuf,
    observations: PathBuf,
    app: AppContract,
    corpus: ProbeCorpus,
    policy: ContainerPolicy,
    runner: RunnerConfig,
    scope: Scope,
    confirmation: ScopeConfirmation,
    ledger: CoreDerived<FoldedLedger>,
    episodes: Vec<DiscrepancyEpisode>,
    /// The container permits the contrast takes, as the host's witness pool gives them.
    slots: Semaphore,
    /// How many probes the contrast runs at once.
    probes_at_once: usize,
}

/// The size of the container pool the gate tests use. The host gives its witness runs
/// `DEFAULT_WITNESS_CONCURRENCY` permits, so that is the default. `ACAN_OBSERVE_POOL` overrides it
/// for timing runs.
fn pool_size() -> usize {
    env_count("ACAN_OBSERVE_POOL", DEFAULT_WITNESS_CONCURRENCY)
}

/// How many probes the gate tests run at once. `ACAN_OBSERVE_PROBES` overrides the host default
/// for timing runs.
fn probes_at_once() -> usize {
    env_count("ACAN_OBSERVE_PROBES", DEFAULT_PROBE_CONCURRENCY)
}

/// A count from the environment variable `name`, or `default` when the variable is unset.
fn env_count(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(text) => text
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a count, got {text:?}")),
        Err(_) => default,
    }
}

impl Gate {
    async fn new(demo: &Demo, candidate: &CommitSha, runner: RunnerConfig) -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory for the gate");
        let base = prepare(&demo.repo, &demo.head()).await;
        let candidate = prepare(&demo.repo, candidate).await;
        let snapshot = dir.path().join("snapshot");
        observe::snapshot(&demo.repo, &base, &snapshot)
            .await
            .expect("the protected snapshot is written");
        let (app, corpus) = observe::read_protected(&snapshot).expect("the protected inputs read");
        let scope = scope(&demo.head());
        let confirmation = confirmation(&scope);
        let ledger = fold::fold(&[]).expect("the empty ruling log folds");
        let policy = observe::runner_policy();
        Self {
            base,
            candidate,
            snapshot,
            scratch: dir.path().join("scratch"),
            observations: dir.path().join("observations"),
            app,
            corpus,
            policy,
            runner,
            scope,
            confirmation,
            ledger,
            episodes: Vec::new(),
            slots: Semaphore::new(pool_size()),
            probes_at_once: probes_at_once(),
            _dir: dir,
        }
    }

    fn plan<'a>(&'a self, repo: &'a Repo) -> observe::Plan<'a> {
        self.plan_at(repo, &self.scratch, self.probes_at_once)
    }

    /// The plan of this gate that writes into `scratch` and keeps `probes_at_once` probes in
    /// flight. A pass never reuses a scratch directory another pass wrote, so each needs its own.
    fn plan_at<'a>(
        &'a self,
        repo: &'a Repo,
        scratch: &'a Path,
        probes_at_once: usize,
    ) -> observe::Plan<'a> {
        observe::Plan {
            repo,
            base: &self.base,
            candidate: &self.candidate,
            snapshot: &self.snapshot,
            scratch,
            observations: &self.observations,
            app: &self.app,
            corpus: &self.corpus,
            policy: &self.policy,
            runner: &self.runner,
            slots: &self.slots,
            probes_at_once,
        }
    }

    /// The same gate with the given discrepancy episodes, which the receipt reads.
    fn with_episodes(mut self, episodes: Vec<DiscrepancyEpisode>) -> Self {
        self.episodes = episodes;
        self
    }

    fn order(&self) -> observe::Order<'_> {
        observe::Order {
            scope: &self.scope,
            confirmation: &self.confirmation,
            ledger: &self.ledger,
            episodes: &self.episodes,
        }
    }
}

/// Prepares `commit` on the protected head, and fails the test unless it merges cleanly.
async fn prepare(repo: &Repo, commit: &CommitSha) -> Preparation {
    let outcome = repo
        .prepare(commit, &protected())
        .await
        .expect("the commit prepares");
    let PrepareOutcome::Prepared(preparation) = outcome else {
        panic!("the commit conflicts with the protected head");
    };
    preparation
}

fn protected() -> ProtectedPaths {
    observe::protected_paths(&observe::runner_policy())
}

fn runner() -> RunnerConfig {
    RunnerConfig::new("docker", APP_PORT)
}

/// The scope the demo order confirms: one must-keep rule over the signup answer's body. The rule
/// reaches the signup probes at their first step and holds when the answer matches base.
fn scope(base: &CommitSha) -> Scope {
    scope_of(ORDER, base)
}

/// The scope of `order`, with the signup rule on `base`.
fn scope_of(order: &str, base: &CommitSha) -> Scope {
    serde_json::from_value(json!({
        "version": "acan-scope/1",
        "order": order,
        "rev": 1,
        "base_commit": base,
        "body": {
            "may": [],
            "must_keep": [{
                "id": SIGNUP_RULE,
                "selector": {
                    "surface": "http",
                    "method": "POST",
                    "route_template": "/api/signup",
                    "step_context": { "kind": "step", "step_id": "s1" },
                    "principal": { "kind": "anonymous" },
                    "channel": "body"
                },
                "obligations": [{
                    "obligation_id": "obl-signup-body",
                    "predicate": {
                        "predicate": "equals_base",
                        "step_id": "s1",
                        "path": "body",
                        "anchor": { "kind": "fresh" }
                    },
                    "text": "The signup answer's body matches the base run"
                }],
                "text": "Signup answers with the same body as before the change",
                "machine_ids": ["auth", "store"]
            }],
            "open": []
        }
    }))
    .expect("the demo scope decodes")
}

/// The human confirmation of `scope`, binding its hashes, selectors and examples.
fn confirmation(scope: &Scope) -> ScopeConfirmation {
    let binding = ScopeBinding::of(scope).expect("the scope binds");
    let order = serde_json::to_value(scope).expect("the scope serializes")["order"].clone();
    serde_json::from_value(json!({
        "version": "acan-scope-confirmation/1",
        "order": order,
        "scope_rev": scope.rev,
        "scope_hash": binding.scope_hash,
        "semantic_scope_hash": scope.semantic_scope_hash().expect("the scope hashes"),
        "base_commit": scope.base_commit,
        "ruling_versions": [],
        "selector_hashes": binding.selector_hashes,
        "example_hashes": binding.example_hashes,
        "answers": [],
        "broad_grants": [],
        "budget": {
            "repair_rounds": 2,
            "wall_time_ms": 600_000,
            "model_cost_micros": 0,
            "max_episodes": 4
        },
        "by": "owner",
        "at": 1_700_000_000_000_u64
    }))
    .expect("the demo confirmation decodes")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Process::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn touch_notes(root: &Path) {
    fs::write(root.join("NOTES.md"), "A note the probes never read.\n")
        .expect("the note is written");
}

fn seed_regression(root: &Path) {
    let file = root.join("src").join("auth.ts");
    let source = fs::read_to_string(&file).expect("the demo has src/auth.ts");
    assert_eq!(
        source.matches(SIGNUP_RESPONSE).count(),
        1,
        "the signup response appears once in the pinned demo"
    );
    fs::write(
        &file,
        source.replace(SIGNUP_RESPONSE, REGRESSED_SIGNUP_RESPONSE),
    )
    .expect("the regression is written");
}

#[tokio::test]
async fn a_passing_order_lands_on_the_protected_branch() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let before = demo.head();
    let candidate = demo.candidate("pass", touch_notes);
    let gate = Gate::new(&demo, &candidate, runner()).await;

    let judged = observe::judge(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect("the candidate is judged");
    assert!(
        land::blockers(&judged.receipt).is_empty(),
        "a candidate that changes no probed behavior passes: {:?}",
        land::blockers(&judged.receipt)
    );

    let current = observe::current(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect("the current context computes");
    let clearance = land::final_gate(&judged.receipt, &current, &gate.candidate)
        .expect("the gate clears the passing candidate");
    let landed = land::install(&demo.repo, clearance)
        .await
        .expect("the protected branch moves to the candidate");

    assert_eq!(landed, candidate, "the merged commit is the candidate");
    assert_eq!(
        demo.head(),
        candidate,
        "the protected branch now holds the candidate"
    );
    assert_ne!(before, candidate);
}

#[tokio::test]
async fn a_seeded_regression_is_blocked_and_the_receipt_names_it() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let before = demo.head();
    let candidate = demo.candidate("regress", seed_regression);
    let gate = Gate::new(&demo, &candidate, runner()).await;

    let judged = observe::judge(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect("the regressed candidate is judged");
    let item = judged
        .receipt
        .items
        .iter()
        .find(|item| item.item_id.as_str() == SIGNUP_RULE)
        .unwrap_or_else(|| {
            panic!(
                "the receipt names the signup rule: {:?}",
                judged.receipt.items
            )
        });
    assert_eq!(item.class, ItemClass::ObligationFailed);
    assert!(
        matches!(item.evaluation.outcome, Outcome::Changed { .. }),
        "the answer differs from base: {:?}",
        item.evaluation.outcome
    );

    let current = observe::current(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect("the current context computes");
    let error = land::final_gate(&judged.receipt, &current, &gate.candidate)
        .expect_err("the regressed candidate is blocked");
    let land::LandError::Blocked(blockers) = error else {
        panic!("the regression is refused by the receipt, not by context: {error:?}");
    };
    assert!(
        blockers.iter().any(|blocker| blocker
            .item
            .as_ref()
            .is_some_and(|id| id.as_str() == SIGNUP_RULE)),
        "the blockers name the signup rule: {blockers:?}"
    );
    assert_eq!(demo.head(), before, "nothing was installed");
}

#[tokio::test]
async fn a_runtime_that_cannot_isolate_a_run_never_installs() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let before = demo.head();
    let candidate = demo.candidate("unisolated", touch_notes);
    let runner = RunnerConfig::new("acan-no-such-container-runtime", APP_PORT);
    let gate = Gate::new(&demo, &candidate, runner).await;

    let judged = observe::judge(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect("the unisolated candidate is judged as unknown, not as a pass");
    assert!(
        !land::blockers(&judged.receipt).is_empty(),
        "a run that never started cannot pass: {:?}",
        judged.receipt.items
    );
    let current = observe::current(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect("the current context computes");
    assert!(
        land::final_gate(&judged.receipt, &current, &gate.candidate).is_err(),
        "the gate refuses an unisolated candidate"
    );
    assert_eq!(demo.head(), before, "nothing was installed");
}

/// The contrast of `gate` in `scratch`, with `probes_at_once` probes in flight.
async fn judge_at(
    gate: &Gate,
    demo: &Demo,
    scratch: &Path,
    probes_at_once: usize,
) -> observe::Judged {
    observe::judge(
        &gate.plan_at(&demo.repo, scratch, probes_at_once),
        &gate.order(),
    )
    .await
    .expect("the candidate is judged")
}

#[tokio::test]
async fn a_contrast_with_one_probe_in_flight_and_with_three_has_the_same_receipt() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let candidate = demo.candidate("concurrent", touch_notes);
    let gate = Gate::new(&demo, &candidate, runner()).await;
    let sequential_dir =
        tempfile::tempdir().expect("a temporary directory for the sequential pass");
    let concurrent_dir =
        tempfile::tempdir().expect("a temporary directory for the concurrent pass");

    let sequential = judge_at(&gate, &demo, &sequential_dir.path().join("scratch"), 1).await;
    let concurrent = judge_at(&gate, &demo, &concurrent_dir.path().join("scratch"), 3).await;

    assert_eq!(
        renamed_receipt(&concurrent.receipt),
        renamed_receipt(&sequential.receipt),
        "the receipt does not depend on how many probes run at once"
    );
}

/// The receipt as JSON, with each run id and each hash renamed in order of first appearance. A
/// run id is a fresh uuid, and every hash the receipt holds for a run covers that id, so two
/// passes over the same inputs never agree on either. Renaming them leaves the rest of the
/// receipt as it is: two receipts are equal here exactly when they say the same things about the
/// same runs.
fn renamed_receipt(receipt: &Receipt) -> Value {
    let mut value = serde_json::to_value(receipt).expect("the receipt serializes");
    rename_run_bound(&mut value, &mut BTreeMap::new());
    value
}

fn rename_run_bound(value: &mut Value, names: &mut BTreeMap<String, String>) {
    match value {
        Value::String(text) if is_run_bound(text) => {
            let next = format!("run-bound-{}", names.len());
            *text = names.entry(text.clone()).or_insert(next).clone();
        }
        Value::Array(items) => {
            for item in items {
                rename_run_bound(item, names);
            }
        }
        Value::Object(fields) => {
            for field in fields.values_mut() {
                rename_run_bound(field, names);
            }
        }
        _ => {}
    }
}

/// Whether `text` is a run id (a canonical uuid) or a `sha256:` hash.
fn is_run_bound(text: &str) -> bool {
    let uuid = text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    uuid || text.starts_with("sha256:")
}

#[tokio::test]
async fn a_probe_that_cannot_start_ends_the_contrast_with_its_error() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let candidate = demo.candidate("unknown-fixture", touch_notes);
    let mut gate = Gate::new(&demo, &candidate, runner()).await;
    gate.corpus.probes[0].spec.fixture =
        FixtureId::from_str("fixture-not-in-the-corpus").expect("a valid fixture id");

    let error = observe::judge(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect_err("a contrast whose probe cannot start builds no receipt");
    assert!(
        matches!(error, observe::ObserveError::MissingFixture { .. }),
        "the failing probe's own error is returned: {error:?}"
    );
}

#[tokio::test]
async fn a_candidate_cannot_change_the_protected_inputs() {
    let demo = Demo::pinned().await;
    let candidate = demo.candidate("edit-corpus", |root| {
        let file = root.join(PROTECTED_CORPUS);
        let text = fs::read_to_string(&file).expect("the corpus is read");
        fs::write(&file, format!("{text}\n")).expect("the edit is written");
    });

    let prepared = prepare(&demo.repo, &candidate).await;
    let discarded: Vec<&str> = prepared
        .discarded()
        .iter()
        .map(|path| path.as_str())
        .collect();
    assert!(
        discarded.contains(&PROTECTED_CORPUS),
        "the edit to the corpus is discarded: {discarded:?}"
    );
    let head_corpus = git(
        &demo.root,
        &["show", &format!("{}:{PROTECTED_CORPUS}", demo.head())],
    );
    let merged_corpus = git(
        &demo.root,
        &[
            "show",
            &format!("{}:{PROTECTED_CORPUS}", prepared.merged_commit()),
        ],
    );
    assert_eq!(
        merged_corpus, head_corpus,
        "the merged commit holds the protected head's corpus"
    );
}

#[tokio::test]
async fn a_head_move_forces_a_fresh_preparation_and_run() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let candidate = demo.candidate("moved-head", touch_notes);
    let gate = Gate::new(&demo, &candidate, runner()).await;
    let judged = observe::judge(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect("the candidate is judged");
    assert!(
        land::blockers(&judged.receipt).is_empty(),
        "the candidate passes before the head moves: {:?}",
        land::blockers(&judged.receipt)
    );
    let current = observe::current(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect("the current context computes");
    let stale = land::final_gate(&judged.receipt, &current, &gate.candidate)
        .expect("the gate clears the candidate before the head moves");

    let moved = demo.advance(|root| {
        fs::write(root.join("MOVED.md"), "The protected head moved.\n")
            .expect("the note is written");
    });
    let refused = land::install(&demo.repo, stale)
        .await
        .expect_err("a clearance for the old head does not install over the new one");
    assert!(
        matches!(
            &refused,
            land::LandError::NotInstalled(InstallOutcome::HeadMoved { current: head })
                if *head == moved
        ),
        "the protected branch moved, so nothing installs: {refused:?}"
    );
    assert_eq!(demo.head(), moved, "the moved head is untouched");

    let fresh = Gate::new(&demo, &candidate, runner()).await;
    let fresh_current = observe::current(&fresh.plan(&demo.repo), &fresh.order())
        .await
        .expect("the fresh context computes");
    let not_cleared = land::final_gate(&judged.receipt, &fresh_current, &fresh.candidate)
        .expect_err("the run against the old head does not clear the new head");
    assert!(
        matches!(not_cleared, land::LandError::ContextChanged),
        "the old receipt names the old head: {not_cleared:?}"
    );

    let rerun = observe::judge(&fresh.plan(&demo.repo), &fresh.order())
        .await
        .expect("the candidate is run again on the moved head");
    assert_eq!(
        rerun.context.protected_head, moved,
        "the new run is tested on the moved head"
    );
    assert!(
        land::blockers(&rerun.receipt).is_empty(),
        "the fresh run passes: {:?}",
        land::blockers(&rerun.receipt)
    );
    let cleared = land::final_gate(&rerun.receipt, &fresh_current, &fresh.candidate)
        .expect("the fresh run clears the gate");
    let landed = land::install(&demo.repo, cleared)
        .await
        .expect("the fresh candidate installs");
    assert_eq!(
        landed,
        *fresh.candidate.merged_commit(),
        "the tested merged commit is what lands"
    );
    assert_eq!(demo.head(), landed, "the protected branch holds it");
}

#[tokio::test]
async fn a_missing_protected_path_is_masked_and_the_run_is_judged() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    demo.advance(|root| {
        fs::remove_dir_all(root.join("test")).expect("the protected test directory is removed");
    });
    let candidate = demo.candidate("no-protected-test", touch_notes);
    let gate = Gate::new(&demo, &candidate, runner()).await;
    let judged = observe::judge(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect("the candidate is judged");
    assert!(
        land::blockers(&judged.receipt).is_empty(),
        "a missing protected path is masked by an empty mount, so the run is judged on its behavior: {:?}",
        land::blockers(&judged.receipt)
    );
    let item = judged
        .receipt
        .items
        .iter()
        .find(|item| item.item_id.as_str() == SIGNUP_RULE)
        .expect("the receipt names the signup rule");
    assert!(
        item.sequence.iter().all(|step| step.status.is_some()),
        "every role answered, so no run failed to start: {:?}",
        item.sequence
    );
}

/// The episode the seeded regression opens, as the repair loop leaves it once its budget runs
/// out: in residue, waiting for a human ruling. The receipt keys the signup item by it.
fn residue_episode(scope: &Scope) -> DiscrepancyEpisode {
    let selector = canonical_episode_selector(&scope.body.must_keep[0].selector);
    let key: EpisodeKey = serde_json::from_value(json!({
        "order": ORDER,
        "selector": selector,
        "obligation_id": SIGNUP_OBLIGATION,
    }))
    .expect("the episode key decodes");
    let id = key.episode_id().expect("the episode key hashes");
    serde_json::from_value(json!({
        "version": "acan-episode/1",
        "id": id,
        "order": ORDER,
        "scope_lineage": [{
            "rev": scope.rev,
            "scope_hash": scope.scope_hash().expect("the scope hashes"),
        }],
        "probe_hash": EPISODE_PROBE,
        "step_id": "s1",
        "selector": selector,
        "obligation_id": SIGNUP_OBLIGATION,
        "evidence_revisions": [],
        "candidate_attempts": [],
        "bounce_count": 2,
        "state": "residue",
    }))
    .expect("the residue episode decodes")
}

/// The human's answer to `episode` on `order`: a note that records the ruling without enforcing
/// it. A note is never counted as a check, so the item stays a blocker until a run on the
/// answered head carries it.
fn answer(episode: &EpisodeId, order: OrderId) -> LedgerEvent {
    serde_json::from_value(json!({
        "event": "note_recorded",
        "schema_version": "acan-ledger/1",
        "event_seq": 1,
        "actor": "owner",
        "at": 1_700_000_100_000_u64,
        "ruling_id": RULING,
        "order": order.to_string(),
        "episode_id": episode,
        "text": "The signup answer changes on purpose; a later decision will settle it",
        "reason": {
            "kind": "unexpressible",
            "detail": "the change is a product call, not a predicate",
        },
        "machine_ids": ["auth"],
    }))
    .expect("the answer decodes")
}

/// The gate's order with `ledger` as its folded ruling state, so the context it computes binds
/// that ledger's head.
fn order_on<'a>(gate: &'a Gate, ledger: &'a CoreDerived<FoldedLedger>) -> observe::Order<'a> {
    observe::Order {
        scope: &gate.scope,
        confirmation: &gate.confirmation,
        ledger,
        episodes: &gate.episodes,
    }
}

#[tokio::test]
async fn a_pending_ruling_survives_a_restart_and_is_answered_in_the_log() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let before = demo.head();
    let candidate = demo.candidate("residue", seed_regression);
    let episode = residue_episode(&scope(&before));
    let gate = Gate::new(&demo, &candidate, runner())
        .await
        .with_episodes(vec![episode.clone()]);
    let judged = observe::judge(&gate.plan(&demo.repo), &gate.order())
        .await
        .expect("the regressed candidate is judged");
    let item = judged
        .receipt
        .items
        .iter()
        .find(|item| item.item_id.as_str() == SIGNUP_RULE)
        .expect("the receipt names the signup rule");
    assert_eq!(
        item.state,
        ItemState::Residue,
        "the episode waits for a human ruling"
    );
    assert_eq!(
        item.episode_id.as_ref(),
        Some(&episode.id),
        "the item is keyed by the episode"
    );
    let waiting = item.item_id.clone();

    // The owner posts the order the residue belongs to, so a ruling on it has an order to name.
    // Nothing is ruled yet: the core ledger is empty and the residue is pending.
    let state = tempfile::tempdir().expect("a temporary state root");
    let first = host_over(state.path(), Stub::default()).await;
    owner_says(
        &first,
        HumanCmd::PostOrder {
            machine: MachineKey::from_str(MACHINE).expect("valid machine key"),
            sentence: "keep the signup answer as it is".to_owned(),
            witnesses: Vec::new(),
        },
    )
    .await;
    let order = first
        .read(|world| world.orders().next().map(|order| order.id))
        .expect("the order is posted");
    let answer_event = answer(&episode.id, order);
    assert_eq!(
        first
            .folded_rulings()
            .expect("the empty ledger folds")
            .value()
            .head
            .event_seq,
        0,
        "the first host folds the genesis head"
    );
    assert!(answered_in(&first).is_empty(), "nothing is answered yet");
    assert_eq!(
        land::pending(&judged.receipt, &answered_in(&first)),
        vec![waiting.clone()],
        "the ruling is pending before any answer"
    );
    first.shutdown().await.expect("the first host drains");
    drop(first);

    // A restart rebuilds the host from the same store, and nothing from the first start is kept.
    // The ruling is still pending: the answer state comes from the stored events alone.
    let restarted = host_over(state.path(), Stub::default()).await;
    assert_eq!(
        land::pending(&judged.receipt, &answered_in(&restarted)),
        vec![waiting.clone()],
        "the ruling is still pending after the restart"
    );
    let folded = restarted.folded_rulings().expect("the ledger folds");
    let current = observe::current(&gate.plan(&demo.repo), &order_on(&gate, &folded))
        .await
        .expect("the current context computes");
    let error = land::final_gate(&judged.receipt, &current, &gate.candidate)
        .expect_err("land refuses a pending ruling");
    let land::LandError::Blocked(blockers) = error else {
        panic!("the pending ruling is refused by the receipt: {error:?}");
    };
    assert!(
        blockers
            .iter()
            .any(|blocker| blocker.item.as_ref() == Some(&waiting)),
        "the blockers name the pending item: {blockers:?}"
    );
    assert_eq!(demo.head(), before, "nothing was installed");

    // The owner answers through RecordRuling, the core command. The origin appends it after the
    // ledger head it holds, and the gate folds it from then on.
    owner_says(
        &restarted,
        HumanCmd::RecordRuling {
            event: Box::new(answer_event),
        },
    )
    .await;
    let answered = restarted
        .folded_rulings()
        .expect("the answered ledger folds");
    assert_eq!(
        answered.value().head.event_seq,
        1,
        "the answer is the ledger's first event"
    );
    assert!(
        answered_in(&restarted).contains(&episode.id),
        "the answer names the episode"
    );
    assert!(
        land::pending(&judged.receipt, &answered_in(&restarted)).is_empty(),
        "no ruling is pending after the answer"
    );
    restarted
        .shutdown()
        .await
        .expect("the restarted host drains");
    drop(restarted);

    // A second restart folds the same answer from the stored events, and nothing is pending.
    let rebuilt = host_over(state.path(), Stub::default()).await;
    let folded = rebuilt
        .folded_rulings()
        .expect("the rebuilt host folds the ledger");
    assert_eq!(
        folded.value().head.event_seq,
        1,
        "the folded head is the answer"
    );
    assert_eq!(
        folded.value().notes.len(),
        1,
        "the answer folds as one note"
    );
    assert!(
        land::pending(&judged.receipt, &answered_in(&rebuilt)).is_empty(),
        "no ruling is pending after the second restart"
    );

    // The answer moved the ledger head, so the receipt from before it names another context.
    // Only a run on the answered head can clear the gate.
    let current = observe::current(&gate.plan(&demo.repo), &order_on(&gate, &folded))
        .await
        .expect("the current context computes");
    let stale = land::final_gate(&judged.receipt, &current, &gate.candidate)
        .expect_err("the receipt from before the answer is refused");
    assert!(
        matches!(stale, land::LandError::ContextChanged),
        "the old receipt is for another ledger head: {stale:?}"
    );
    assert_eq!(demo.head(), before, "nothing was installed");
    rebuilt.shutdown().await.expect("the rebuilt host drains");
}

/// The episodes the host's core ledger answers, read the way the gate reads them.
fn answered_in(host: &Host<Stub>) -> BTreeSet<EpisodeId> {
    host.read(|world| land::answered(world.ledger().events()))
}

/// The owner, who registers the worker, posts the order and confirms the scope.
fn owner() -> MemberId {
    MemberId::from_str("owner").expect("valid member id")
}

/// The worker the live tests drive. It only reads back, claims, declares and reports.
fn worker() -> AgentId {
    AgentId::from_str(WORKER).expect("valid agent id")
}

/// The worker's agent id as the origin stores it.
const WORKER: &str = "worker-a";
/// The machine the demo order is posted on.
const MACHINE: &str = "auth";
/// The file the worker declares it will change. The signup answer lives here in the pinned demo.
const SIGNUP_FILE: &str = "src/auth.ts";
/// The one task the foreman splits the demo order into.
const SPLIT: &str =
    r#"{"tasks":[{"title":"signup","brief":"keep the signup answer as it is","scope":["auth"]}]}"#;
/// How long one live step may take before the test gives up on it. A judged candidate runs
/// every probe three times in Docker, so this covers a slow machine.
const LIVE_WAIT: Duration = Duration::from_secs(900);

/// A model that splits the demo order as `SPLIT` asks and answers nothing else.
fn foreman() -> Stub {
    Stub::default().when(Role::Foreman, |_| true, Reply::text(SPLIT))
}

/// A host built over `root`, the way the server builds one: its own store and state directory
/// under `root/.acan`, the protected branch when `root` is a git checkout, and nothing running.
/// Like the server, it writes the state directory's `.gitignore` only when there is none: the
/// demo tracks its own, and overwriting that file would dirty the protected checkout.
async fn host_over(root: &Path, client: Stub) -> Host<Stub> {
    let state = root.join(".acan");
    fs::create_dir_all(&state).expect("the state dir");
    let ignore = state.join(".gitignore");
    if !ignore.exists() {
        fs::write(&ignore, "*\n").expect("the state dir's ignore file");
    }
    let store = EventStore::open(&state.join("world.db"))
        .await
        .expect("the event store opens");
    let log = store
        .load_from(Seq::new(0))
        .await
        .expect("the stored log reads");
    let origin = if log.is_empty() {
        let (origin, opened) = Origin::open(
            WorldRef {
                id: WorldId::from_str("demo-signup").expect("valid world id"),
                repo: root.display().to_string(),
            },
            owner(),
            Timestamp::from_millis(1),
        );
        store
            .append(&[opened])
            .await
            .expect("the open event stores");
        origin
    } else {
        Origin::replay(log).expect("the stored log replays")
    };
    let (tokens, _) = Tokens::load_or_create(&state, &owner()).expect("the tokens load");
    let (_shutdown, shutdown) = tokio::sync::watch::channel(false);
    let git = Repo::open_with_protected(root, PROTECTED_BRANCH).await.ok();
    Host::new(HostParts {
        origin,
        store,
        tokens,
        scan: scan(root).expect("the scan succeeds"),
        ledger: UsageLedger::default(),
        client,
        repo: root.to_path_buf(),
        shutdown,
        git,
        witness_http_base_url: None,
        witness_concurrency: 2,
        probe_concurrency: DEFAULT_PROBE_CONCURRENCY,
    })
}

/// Routes the server's log lines to the test output, so a live step that stalls shows the job
/// that failed and why. Only the first call installs the subscriber.
fn trace_server() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init();
}

/// A live host with its job loop running.
struct Live {
    host: Host<Stub>,
    jobs: JoinHandle<()>,
}

impl Live {
    async fn start(root: &Path) -> Self {
        trace_server();
        let host = host_over(root, foreman()).await;
        let jobs = spawn_jobs(host.clone());
        host.wake_jobs();
        Self { host, jobs }
    }

    /// Stops the job loop, then drains the writer. Nothing can write after this returns.
    async fn stop(self) {
        self.jobs.abort();
        let _ = self.jobs.await;
        self.host.shutdown().await.expect("the writer drains");
    }
}

/// Sends `cmd` as the owner.
async fn owner_says(host: &Host<Stub>, cmd: HumanCmd) {
    host.submit(Command::Human {
        member: owner(),
        cmd,
    })
    .await
    .expect("the owner's command is accepted");
}

/// Sends `cmd` as the worker.
async fn worker_says(host: &Host<Stub>, cmd: AgentCmd) {
    host.submit(Command::Agent {
        agent: worker(),
        cmd,
    })
    .await
    .expect("the worker's command is accepted");
}

/// Polls `check` until it yields a value. The test fails when `LIVE_WAIT` passes first.
async fn until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + LIVE_WAIT;
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The footprint the worker declares: the file the signup answer lives in.
fn footprint() -> acan_core::DeclarationInput {
    serde_json::from_value(json!({
        "files": [SIGNUP_FILE],
        "symbols": [],
        "interfaces": [],
        "inserts": []
    }))
    .expect("the footprint decodes")
}

/// The task's claim generation, once the task is claimed.
fn claimed_generation(world: &World, task: TaskId) -> Option<acan_core::Generation> {
    match &world.task(task)?.state {
        TaskState::Claimed { claim, .. } => Some(claim.lease.generation),
        _ => None,
    }
}

/// Drives the world to a built candidate with core commands only. The owner registers the
/// worker, posts the order, and confirms the scope once the job loop has split it. The worker
/// reads back, claims, declares, and reports `candidate`. Returns the order and its one task.
async fn report_built(live: &Live, demo: &Demo, candidate: &CommitSha) -> (OrderId, TaskId) {
    let (order, task) = post_and_split(&live.host).await;
    let scope = scope_of(&order.to_string(), &demo.head());
    build(&live.host, order, task, &scope, candidate).await;
    (order, task)
}

/// Registers the worker, posts the demo order, and waits for the job loop to split it. Returns
/// the order and its one task.
async fn post_and_split(host: &Host<Stub>) -> (OrderId, TaskId) {
    owner_says(host, HumanCmd::RegisterAgent { agent: worker() }).await;
    owner_says(
        host,
        HumanCmd::PostOrder {
            machine: MachineKey::from_str(MACHINE).expect("valid machine key"),
            sentence: "keep the signup answer as it is".to_owned(),
            witnesses: Vec::new(),
        },
    )
    .await;
    let order = until("the order", || {
        host.read(|world| world.orders().next().map(|order| order.id))
    })
    .await;
    let task = until("the split task", || {
        host.read(|world| world.tasks_of(order).next().map(|task| task.id))
    })
    .await;
    (order, task)
}

/// The owner confirms `scope` for `order`, the worker reads it back, claims, declares, and
/// reports `candidate` built. The gate run it starts waits for its prepared candidate.
async fn build(
    host: &Host<Stub>,
    order: OrderId,
    task: TaskId,
    scope: &Scope,
    candidate: &CommitSha,
) {
    owner_says(
        host,
        HumanCmd::ReviseScope {
            order,
            scope: Box::new(scope.clone()),
        },
    )
    .await;
    owner_says(
        host,
        HumanCmd::ConfirmScope {
            order,
            confirmation: Box::new(confirmation(scope)),
        },
    )
    .await;
    worker_says(
        host,
        AgentCmd::PostScopedReadback {
            task,
            scope_rev: scope.rev,
            scope_hash: scope.scope_hash().expect("the scope hashes"),
            summary: "keep the signup answer, add nothing".to_owned(),
            predictions: Vec::new(),
        },
    )
    .await;
    worker_says(host, AgentCmd::Claim { task }).await;
    let generation = host
        .read(|world| claimed_generation(world, task))
        .expect("the claim is granted");
    let fence = Fence { task, generation };
    worker_says(
        host,
        AgentCmd::Declare {
            fence,
            footprint: footprint(),
        },
    )
    .await;
    worker_says(
        host,
        AgentCmd::ReportBuilt {
            fence,
            head: CommitId::from_str(candidate.as_str()).expect("the candidate is a commit id"),
        },
    )
    .await;
}

/// The run that is prepared and still waits for its receipt, when there is one.
fn prepared_run(world: &World, order: OrderId) -> Option<GateRunId> {
    let lane = world.lane(order)?;
    lane.runs
        .iter()
        .find(|(_, run)| run.prepared.is_some() && run.receipt.is_none())
        .map(|(run, _)| *run)
}

/// Whether `task` has landed on the protected branch.
fn landed(world: &World, task: TaskId) -> bool {
    matches!(
        world.task(task).map(|task| &task.state),
        Some(TaskState::Landed { .. })
    )
}

#[tokio::test]
async fn a_live_order_lands_through_the_job_loop() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let before = demo.head();
    let candidate = demo.candidate("live-pass", touch_notes);
    let live = Live::start(&demo.root).await;

    let (order, task) = report_built(&live, &demo, &candidate).await;
    until("the candidate to land", || {
        live.host.read(|world| landed(world, task).then_some(()))
    })
    .await;

    assert_eq!(
        demo.head(),
        candidate,
        "the tested merged commit is what the protected branch holds"
    );
    assert_ne!(before, candidate, "the protected branch moved");
    let passed = live.host.read(|world| {
        world
            .lane(order)
            .and_then(|lane| lane.latest_receipt())
            .is_some_and(|record| matches!(record.verdict, LaneVerdict::Passed))
    });
    assert!(passed, "the receipt that landed the candidate passed");
    live.stop().await;
}

#[tokio::test]
async fn a_live_seeded_regression_is_blocked_and_its_receipt_names_the_rule() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let before = demo.head();
    let candidate = demo.candidate("live-regress", seed_regression);
    let live = Live::start(&demo.root).await;

    let (order, task) = report_built(&live, &demo, &candidate).await;
    let receipt: Receipt = until("the regression's receipt", || {
        live.host.read(|world| {
            world
                .lane(order)
                .and_then(|lane| lane.latest_receipt())
                .map(|record| record.receipt.clone())
        })
    })
    .await;
    let item = receipt
        .items
        .iter()
        .find(|item| item.item_id.as_str() == SIGNUP_RULE)
        .expect("the receipt names the signup rule");
    assert_eq!(item.class, ItemClass::ObligationFailed);
    assert!(
        live.host.read(|world| world
            .lane(order)
            .and_then(|lane| lane.latest_receipt())
            .is_some_and(|record| matches!(record.verdict, LaneVerdict::Blocked { .. }))),
        "the regressed candidate is blocked"
    );
    assert_eq!(demo.head(), before, "nothing was installed");
    assert!(
        live.host.read(|world| !landed(world, task)),
        "the task did not land"
    );
    live.stop().await;
}

#[tokio::test]
async fn a_restart_between_the_prepared_run_and_its_receipt_neither_repeats_nor_skips_it() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let candidate = demo.candidate("live-restart", touch_notes);
    let live = Live::start(&demo.root).await;

    let (order, task) = report_built(&live, &demo, &candidate).await;
    let run = until("the candidate to be prepared", || {
        live.host.read(|world| prepared_run(world, order))
    })
    .await;
    // The host stops while the run is being observed: its preparation is recorded, its receipt is not.
    live.stop().await;

    let live = Live::start(&demo.root).await;
    until("the candidate to land after the restart", || {
        live.host.read(|world| landed(world, task).then_some(()))
    })
    .await;
    assert_eq!(
        demo.head(),
        candidate,
        "the restarted run lands the candidate"
    );

    let log = live
        .host
        .store()
        .load_from(Seq::new(0))
        .await
        .expect("the log reads");
    let prepared = log
        .iter()
        .filter(|stamped| matches!(&stamped.event, Event::CandidatePrepared { run: found, .. } if *found == run))
        .count();
    let receipts = log
        .iter()
        .filter(|stamped| matches!(&stamped.event, Event::ReceiptRecorded { run: found, .. } if *found == run))
        .count();
    assert_eq!(prepared, 1, "the run was prepared once");
    assert_eq!(receipts, 1, "the run has exactly one receipt");
    live.stop().await;
}

/// A scope with one may-change item: the login route's 429 status. Its receipt shape is the one
/// the core's fixtures use, so a run can pass without a container. `rev` tells revisions apart.
fn limit_scope(order: &str, base: &CommitSha, rev: u32) -> Scope {
    serde_json::from_value(json!({
        "version": "acan-scope/1",
        "order": order,
        "rev": rev,
        "base_commit": base,
        "body": {
            "may": [{
                "id": LIMIT_ITEM,
                "selector": {
                    "surface": "http", "method": "POST", "route_template": "/api/login",
                    "step_context": {"kind": "step", "step_id": "s6"},
                    "principal": {"kind": "anonymous"}, "channel": "status"
                },
                "change": "add",
                "acceptance": [{
                    "obligation_id": LIMIT_OBLIGATION,
                    "predicate": {"predicate": "present", "at": {"ref": "field", "run": "candidate", "step_id": "s6", "path": "status"}},
                    "text": "the 429 status shows"
                }],
                "provenance": "user",
                "example": {"kind": "target", "step_id": "s6", "path": "status", "after": {"state": "present", "value": 429}},
                "machine_ids": [MACHINE]
            }],
            "must_keep": [],
            "open": []
        }
    }))
    .expect("the limit scope decodes")
}

/// A content hash in the form the contract spells, from one byte.
fn sha(byte: u8) -> String {
    format!("sha256:{byte:064x}")
}

/// The may-change item a passing run reports for [`limit_scope`].
fn limit_item() -> Value {
    json!({
        "item_id": LIMIT_ITEM,
        "class": "obligation_fulfilled",
        "state": "open",
        "machine_id": MACHINE,
        "probe_hash": sha(7),
        "episode_id": null,
        "attempts": 0,
        "budget": 2,
        "route_path": {"machines": [MACHINE], "source": "static"},
        "sequence": [],
        "evidence": [],
        "evaluation": {
            "authorization": {"kind": "may_change", "item_id": LIMIT_ITEM},
            "outcome": {"kind": "changed", "differences": [{
                "probe_hash": sha(7), "step_id": "s6", "path": "status",
                "base": {"state": "present", "value": 401},
                "candidate": {"state": "present", "value": 429}
            }]},
            "obligations": [{"obligation_id": LIMIT_OBLIGATION, "result": {"result": "pass"}}]
        }
    })
}

/// The receipt a run passes with for [`limit_scope`], built on the context `prepared` records.
fn passing_receipt(order: &str, scope: &Scope, prepared: &Value) -> Value {
    json!({
        "version": "acan-receipt/1",
        "order": order,
        "scope_rev": scope.rev,
        "scope_hash": scope.scope_hash().expect("the scope hashes"),
        "prepared": prepared,
        "gate_state": "running",
        "items": [limit_item()],
        "counts": {
            "preserved": 0, "obligation_fulfilled": 1, "obligation_failed": 0,
            "authorized_but_unspecified": 0, "out_of_scope": 0, "violates_ruling": 0,
            "unstable_base": 0, "incomparable": 0, "error": 0
        },
        "probes": {"total": 1, "run": 1, "errored": 0},
        "routes": {"total": 1, "covered": 1, "uncovered": []},
        "behavior": {"obligations_total": 1, "passed": 1, "failed": 0, "unknown": 0},
        "masks": [],
        "rulings": [],
        "contests": []
    })
}

/// The prepared context of a candidate: `merged` is the commit the run tests, and the ledger head
/// is the one the origin holds now.
fn prepared_json(base: &CommitSha, scope: &Scope, merged: &CommitSha, ledger_head: Value) -> Value {
    json!({
        "base_commit": base,
        "context": {
            "protected_head": base,
            "merged_commit": merged,
            "overlay_hash": sha(1),
            "effective_artifact_hashes": {},
            "ledger_head": ledger_head,
            "scope_hash": scope.scope_hash().expect("the scope hashes"),
            "probe_set_hash": sha(2),
            "fixture_set_hash": sha(3),
            "runner_hash": sha(4),
            "evaluator_version": "eval-1",
            "runtime": "rt-1",
            "controlled_env_hash": sha(5),
            "sandbox_policy_hash": sha(6)
        }
    })
}

/// The gate run a task is in, while it is gating.
fn gating_run(world: &World, task: TaskId) -> Option<GateRunId> {
    match &world.task(task)?.state {
        TaskState::Claimed {
            stage: Stage::Gating { run, .. },
            ..
        } => Some(*run),
        _ => None,
    }
}

/// A scope-first order whose candidate passed its gate, with no job running and no container.
/// The owner and the worker drive the world through the split and the build, then the prepared
/// context and a passing receipt are recorded directly, as the job loop would record them. The
/// task is queued when this returns. Returns the host, the order, the task and the run.
async fn passed_candidate(
    demo: &Demo,
    candidate: &CommitSha,
) -> (Host<Stub>, OrderId, TaskId, GateRunId) {
    let live = Live::start(&demo.root).await;
    let (order, task) = post_and_split(&live.host).await;
    live.stop().await;

    let host = host_over(&demo.root, foreman()).await;
    let scope = limit_scope(&order.to_string(), &demo.head(), 1);
    build(&host, order, task, &scope, candidate).await;
    let run = host
        .read(|world| gating_run(world, task))
        .expect("the build starts a gate run");
    let ledger_head = host
        .read(|world| serde_json::to_value(world.ledger().head()))
        .expect("the ledger head serializes");
    let prepared = prepared_json(&demo.head(), &scope, candidate, ledger_head);
    host.submit(Command::System(SystemCmd::CandidatePrepared {
        run,
        prepared: Box::new(serde_json::from_value(prepared.clone()).expect("the context decodes")),
    }))
    .await
    .expect("the candidate is prepared");
    let receipt = passing_receipt(&order.to_string(), &scope, &prepared);
    host.submit(Command::System(SystemCmd::ReceiptReady {
        run,
        receipt: Box::new(serde_json::from_value(receipt).expect("the receipt decodes")),
    }))
    .await
    .expect("the passing receipt is recorded");
    assert!(
        host.read(|world| matches!(
            world.task(task).map(|found| &found.state),
            Some(TaskState::Queued { .. })
        )),
        "the passing receipt queues the task"
    );
    (host, order, task, run)
}

#[tokio::test]
async fn a_scope_revised_while_queued_never_installs() {
    let demo = Demo::pinned().await;
    let before = demo.head();
    let candidate = demo.candidate("queued-revise", touch_notes);
    let (host, order, task, _run) = passed_candidate(&demo, &candidate).await;

    // The owner revises the scope after the candidate passed and before it lands. Nothing
    // confirmed the new revision, so the queued candidate must not reach the protected branch.
    owner_says(
        &host,
        HumanCmd::ReviseScope {
            order,
            scope: Box::new(limit_scope(&order.to_string(), &before, 2)),
        },
    )
    .await;
    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();
    until("the queued land to be decided", || {
        host.read(|world| {
            matches!(
                world.task(task).map(|found| &found.state),
                Some(TaskState::Open | TaskState::Landed { .. })
            )
            .then_some(())
        })
    })
    .await;

    assert_eq!(demo.head(), before, "nothing was installed");
    assert!(
        host.read(|world| !landed(world, task)),
        "the task did not land"
    );
    jobs.abort();
    let _ = jobs.await;
    host.shutdown().await.expect("the writer drains");
}

/// A land reads the queued delivery, then checks it again under the landing fence. A revision
/// that lands between the read and the check ends the delivery, so the land refuses it and
/// installs nothing.
#[tokio::test]
async fn a_delivery_read_before_a_revision_does_not_stand() {
    let demo = Demo::pinned().await;
    let before = demo.head();
    let candidate = demo.candidate("stale-revise", touch_notes);
    let (host, order, task, _run) = passed_candidate(&demo, &candidate).await;
    let delivery = host
        .read(|world| land::Delivery::of(world, task))
        .expect("the queued delivery reads");
    assert!(
        host.read(|world| delivery.stands(world)).is_ok(),
        "a delivery read from the queue stands"
    );

    owner_says(
        &host,
        HumanCmd::ReviseScope {
            order,
            scope: Box::new(limit_scope(&order.to_string(), &before, 2)),
        },
    )
    .await;

    assert!(
        host.read(|world| delivery.stands(world)).is_err(),
        "a revised scope ends the delivery read before it"
    );
    assert_eq!(demo.head(), before, "nothing was installed");
    host.shutdown().await.expect("the writer drains");
}

/// The crash window after an install: the branch moved to the tested commit, then the process
/// stopped before the land result was recorded. After a restart the queued delivery finds the
/// branch on the tested commit and records the land as landed, instead of preparing the
/// candidate again on a head it no longer matches and reopening installed work.
#[tokio::test]
async fn a_land_installed_before_its_result_was_recorded_is_recorded_as_landed() {
    let demo = Demo::pinned().await;
    let candidate = demo.candidate("crash-window", touch_notes);
    let (host, _order, task, _run) = passed_candidate(&demo, &candidate).await;
    host.shutdown().await.expect("the writer drains");

    // The crash: the install moved the protected branch to the tested commit and nothing recorded
    // its result.
    git(
        &demo.root,
        &["merge", "--ff-only", "--quiet", candidate.as_str()],
    );
    assert_eq!(demo.head(), candidate, "the install moved the branch");

    let host = host_over(&demo.root, foreman()).await;
    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();
    until("the queued land to be decided", || {
        host.read(|world| {
            matches!(
                world.task(task).map(|found| &found.state),
                Some(TaskState::Open | TaskState::Landed { .. })
            )
            .then_some(())
        })
    })
    .await;

    assert!(
        host.read(|world| landed(world, task)),
        "the installed commit is recorded as the land"
    );
    assert_eq!(
        demo.head(),
        candidate,
        "the branch still holds the tested commit"
    );
    jobs.abort();
    let _ = jobs.await;
    host.shutdown().await.expect("the writer drains");
}

/// Records a passing receipt for `run` on the context it was prepared with, as a judge that passed
/// would. The task is queued when this returns. Returns the merged commit that receipt tests.
async fn pass_on_prepared(host: &Host<Stub>, order: OrderId, run: GateRunId) -> CommitSha {
    let prepared = host
        .read(|world| {
            world
                .lane(order)
                .and_then(|lane| lane.runs.get(&run))
                .and_then(|lane_run| lane_run.prepared.clone())
        })
        .expect("the run is prepared");
    let receipt = passing_receipt(
        &order.to_string(),
        &scope_of(&order.to_string(), &prepared.base_commit),
        &serde_json::to_value(&prepared).expect("the prepared context encodes"),
    );
    host.submit(Command::System(SystemCmd::ReceiptReady {
        run,
        receipt: Box::new(serde_json::from_value(receipt).expect("the receipt decodes")),
    }))
    .await
    .expect("the passing receipt is recorded");
    prepared.context.merged_commit
}

/// The protected head moves after the scope was confirmed and the candidate was prepared on the
/// old head, before its receipt lands. Nothing the run was tested under moved but the head, so the
/// core re-binds the confirmed scope to the new head. The run is then prepared and judged again on
/// it, and the candidate lands on the new head. The merged commit of the old head is never
/// installed.
#[tokio::test]
async fn a_moved_protected_head_rebinds_the_scope_and_the_candidate_is_judged_again() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let old_head = demo.head();
    let candidate = demo.candidate("rebind-head", touch_notes);

    // The job loop prepares the candidate on the old head and is stopped before its judge
    // records a receipt. The receipt below is the one the run passed with on the old head.
    let live = Live::start(&demo.root).await;
    let (order, task) = report_built(&live, &demo, &candidate).await;
    let run = until("the candidate to be prepared", || {
        live.host.read(|world| prepared_run(world, order))
    })
    .await;
    live.stop().await;

    let host = host_over(&demo.root, foreman()).await;
    let old_merged = pass_on_prepared(&host, order, run).await;

    let moved = demo.advance(|root| {
        fs::write(root.join("MOVED.md"), "The protected head moved.\n")
            .expect("the note is written");
    });
    assert_ne!(moved, old_head, "the protected head moved");

    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();
    until("the candidate to land on the moved head", || {
        host.read(|world| landed(world, task).then_some(()))
    })
    .await;

    let landed_head = demo.head();
    assert_ne!(
        landed_head, old_merged,
        "the merged commit of the old head is never installed"
    );
    git(
        &demo.root,
        &[
            "merge-base",
            "--is-ancestor",
            moved.as_str(),
            landed_head.as_str(),
        ],
    );
    let receipted = host
        .read(|world| {
            world
                .lane(order)
                .and_then(|lane| lane.latest_receipt())
                .map(|record| {
                    (
                        record.receipt.prepared.base_commit.clone(),
                        record.receipt.prepared.context.merged_commit.clone(),
                    )
                })
        })
        .expect("the landed candidate has a receipt");
    assert_eq!(
        receipted,
        (moved.clone(), landed_head.clone()),
        "the receipt that landed names the moved head and the merged commit it installed"
    );

    let log = host
        .store()
        .load_from(Seq::new(0))
        .await
        .expect("the log reads");
    let rebound = log
        .iter()
        .filter(|stamped| matches!(&stamped.event, Event::ScopeRebound { order: found, .. } if *found == order))
        .count();
    assert_eq!(rebound, 1, "the confirmed scope was re-bound once");
    let prepared_runs = log
        .iter()
        .filter(|stamped| matches!(&stamped.event, Event::CandidatePrepared { run: found, .. } if *found == run))
        .count();
    assert_eq!(prepared_runs, 2, "the run was prepared on each head, once");

    jobs.abort();
    let _ = jobs.await;
    host.shutdown().await.expect("the writer drains");
}

/// The protected head moves, and so does a protected input the candidate was tested under: a fixture
/// recipe in the probe corpus. A re-bound run would be judged under inputs no human confirmed, so
/// the core refuses the re-bind. The queued land is refused, nothing installs, and the scope stays
/// confirmed on the old head until a human confirms it again.
#[tokio::test]
async fn a_moved_protected_input_refuses_the_rebind_and_nothing_lands() {
    let _docker = DOCKER.lock().await;
    let demo = Demo::pinned().await;
    let old_head = demo.head();
    let candidate = demo.candidate("refuse-rebind", touch_notes);

    let live = Live::start(&demo.root).await;
    let (order, task) = report_built(&live, &demo, &candidate).await;
    let run = until("the candidate to be prepared", || {
        live.host.read(|world| prepared_run(world, order))
    })
    .await;
    live.stop().await;

    let host = host_over(&demo.root, foreman()).await;
    pass_on_prepared(&host, order, run).await;

    let moved = demo.advance(|root| {
        let corpus = root.join(PROTECTED_CORPUS);
        let text = fs::read_to_string(&corpus).expect("the corpus reads");
        let edited = text.replacen("alice-fixture-pw", "alice-fixture-pw-moved", 1);
        assert_ne!(edited, text, "the corpus has a fixture password to move");
        fs::write(&corpus, edited).expect("the corpus is written");
    });
    assert_ne!(moved, old_head, "the protected head moved");

    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();
    until("the queued land to be decided", || {
        host.read(|world| {
            (!matches!(
                world.task(task).map(|found| &found.state),
                Some(TaskState::Queued { .. })
            ))
            .then_some(())
        })
    })
    .await;

    assert_eq!(demo.head(), moved, "nothing was installed");
    assert!(
        host.read(|world| !landed(world, task)),
        "the task did not land"
    );
    assert_eq!(
        host.read(|world| world
            .lane(order)
            .and_then(|lane| lane.confirmation.as_ref())
            .map(|confirmation| confirmation.base_commit.clone())),
        Some(old_head),
        "the scope stays confirmed on the old head"
    );

    let log = host
        .store()
        .load_from(Seq::new(0))
        .await
        .expect("the log reads");
    let rebound = log
        .iter()
        .filter(|stamped| matches!(&stamped.event, Event::ScopeRebound { .. }))
        .count();
    assert_eq!(rebound, 0, "no re-bind was recorded");
    let refused = log
        .iter()
        .filter(|stamped| matches!(&stamped.event, Event::LandFailed { task: found, reason } if *found == task && reason.contains("confirmed again")))
        .count();
    assert_eq!(
        refused, 1,
        "the land is refused: the scope must be confirmed again"
    );

    jobs.abort();
    let _ = jobs.await;
    host.shutdown().await.expect("the writer drains");
}

/// The same for a cancel: a delivery read before the order is cancelled does not stand after it.
#[tokio::test]
async fn a_delivery_read_before_a_cancel_does_not_stand() {
    let demo = Demo::pinned().await;
    let before = demo.head();
    let candidate = demo.candidate("stale-cancel", touch_notes);
    let (host, order, task, _run) = passed_candidate(&demo, &candidate).await;
    let delivery = host
        .read(|world| land::Delivery::of(world, task))
        .expect("the queued delivery reads");

    owner_says(&host, HumanCmd::CancelOrder { order }).await;

    assert!(
        host.read(|world| delivery.stands(world)).is_err(),
        "a cancelled order ends the delivery read before it"
    );
    assert_eq!(demo.head(), before, "nothing was installed");
    host.shutdown().await.expect("the writer drains");
}
