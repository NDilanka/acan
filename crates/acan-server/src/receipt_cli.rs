//! Shadow receipts: `acan receipt --repo <clone> --base <ref> --head <ref>`.
//!
//! Runs the contrast between two refs of a clone that has a protected app contract and returns
//! the receipt, with no orders and no Acan workers. The contrast runs the way the gate runs it:
//! the same container policy, the protected inputs of the base, and the same receipt builder.
//! The base is the protected head, so `--base` names a local branch. With no confirmed scope,
//! every observed difference is unclaimed, so the receipt never passes and says why.
//!
//! Every refusal stops the command with no receipt: a ref that cannot be used, a protected
//! contract that cannot be read, a Docker that cannot run the pinned image, a head that does not
//! merge onto the base, or a contrast whose runs did not complete. Nothing runs unisolated.
//!
//! No ref of the clone moves and its working tree is not touched. Git objects of the prepared
//! merges are written to its object store, and every run writes into a temporary directory that
//! is removed when the command ends.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use acan_git::{Preparation, PrepareOutcome, ProtectedPaths, Repo};
use acan_intent::fold::fold;
use acan_intent::{
    CanonicalError, CommitSha, GateState, HumanRef, ImageName, OrderRef, Receipt, RepairBudget,
    RepairRounds, RunOutcome, Scope, ScopeBody, ScopeConfirmation, ScopeConfirmationV1, ScopeRev,
    ScopeV1, UnixMillis,
};
use acan_witness::RunnerConfig;
use clap::ValueEnum;
use serde_json::{Value, json};
use tokio::process::Command as ProcessCommand;
use tokio::sync::Semaphore;

use crate::DEFAULT_WITNESS_CONCURRENCY;
use crate::deps::{self, InstallRecord, Installer, Role};
use crate::jobs::{APP_PORT, RUNNER_PROGRAM};
use crate::observe::{self, Judged, ObserveError, Order, Plan};
use crate::receipt::render_markdown;

/// The version of the JSON envelope.
const SHADOW_VERSION: &str = "acan-shadow/1";
/// The order id a shadow receipt carries. It lives only in the record. No order has this name.
const SHADOW_ORDER: &str = "shadow";
/// The member id that stands in for the confirmer. It lives only in the internal record, and no
/// person is named. Prose never says anyone confirmed or ruled.
const SHADOW_CONFIRMER: &str = "shadow";
/// What every shadow receipt says about its scope and rulings, in the header and in the JSON.
const SCOPE_SENTENCE: &str = "Shadow receipt. No scope was confirmed, so no difference is in scope and nothing was ruled by a person.";
/// The text a redacted value is replaced with.
const REDACTED: &str = "<redacted>";
/// The shortest `.env` value treated as a secret. Shorter values are too common to replace.
const SECRET_MIN_LEN: usize = 8;
/// Provider key prefixes. A word with one of these and enough characters after it is a key,
/// whatever key name holds it.
const KEY_PREFIXES: [&str; 4] = ["sk-", "ghp_", "github_pat_", "xoxb-"];
/// The characters after a key prefix that make a word a key.
const KEY_BODY_MIN_LEN: usize = 16;
/// The shortest word after `key`, `token` or `secret` that is redacted as a token.
const TOKEN_MIN_LEN: usize = 20;
/// The words that put a following long, digit-bearing word in the secret class.
const SECRET_WORDS: [&str; 3] = ["key", "token", "secret"];
/// Key names, in lowercase, whose string values are redacted.
const SENSITIVE_KEYS: [&str; 9] = [
    "authorization",
    "cookie",
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "api-key",
    "apikey",
];
/// How long each Docker preflight command may run.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a shadow receipt was refused. Every variant stops the command with no receipt.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ShadowError {
    /// A ref cannot be used: it is empty, looks like an option, does not name a commit, or (for
    /// `--base`) is not a local branch.
    #[error("cannot use {flag} {name:?}: {detail}")]
    Ref {
        /// The flag the ref was given to.
        flag: &'static str,
        /// The ref as given.
        name: String,
        /// Why it cannot be used.
        detail: String,
    },
    /// The clone is missing or is not a git repository.
    #[error("cannot open the clone {}: {detail}", repo.display())]
    Repo {
        /// The path given as `--repo`.
        repo: PathBuf,
        /// What git reported.
        detail: String,
    },
    /// `.acan/app.json` or the probe corpus is missing or does not decode.
    #[error("the protected app contract cannot be read: {0}")]
    Contract(String),
    /// Docker cannot run the pinned image. There is no unisolated fallback.
    #[error(
        "docker cannot run the pinned container image, and there is no unisolated fallback: {0}"
    )]
    Docker(String),
    /// The head cannot be merged onto the protected head, so there is nothing to test.
    #[error("head {head} conflicts with the protected head: {}", files.join(", "))]
    Conflict {
        /// The head commit.
        head: String,
        /// The conflicting paths.
        files: Vec<String>,
    },
    /// The contrast ended before it was complete, so the evidence is not whole.
    #[error("the contrast is incomplete: {0}")]
    Incomplete(String),
    /// The contrast could not be made, or the receipt could not be built.
    #[error("the contrast cannot be made: {0}")]
    Observe(String),
    /// The receipt could not be written.
    #[error("cannot write the receipt: {0}")]
    Output(String),
}

/// The arguments of `acan receipt`.
#[derive(Debug, Clone, clap::Args)]
pub struct ShadowArgs {
    /// The clone to run. Its `.acan/app.json` is the protected app contract.
    #[arg(long)]
    pub repo: PathBuf,
    /// The base ref: a local branch, whose head is the protected head. Create one with
    /// `git branch <name> <sha>` when the base is a commit.
    #[arg(long)]
    pub base: String,
    /// The head ref, observed against the base.
    #[arg(long)]
    pub head: String,
    /// Where a node app's installed dependencies are cached. Defaults to `$XDG_CACHE_HOME/acan/deps`,
    /// or `~/.cache/acan/deps`.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
}

/// The format `acan receipt` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// The JSON envelope with the receipt inside it.
    Json,
    /// The receipt as Markdown, under a header that names the run.
    Markdown,
}

/// A finished shadow receipt and the facts that name the run it came from.
#[derive(Debug, Clone)]
pub struct ShadowReceipt {
    /// The arguments the run was given.
    pub args: ShadowArgs,
    /// The base commit, the protected head the contrast started from.
    pub base_commit: String,
    /// The head commit as the ref named it.
    pub head_commit: String,
    /// The merged commit every run tested.
    pub merged_commit: String,
    /// The protected paths the head changed. The gate discards these changes, so the head's own
    /// content was not tested there.
    pub discarded: Vec<String>,
    /// The receipt hash.
    pub receipt_hash: String,
    /// The receipt the core built.
    pub receipt: Receipt,
    /// The installs the runs mounted, one per side of the contrast. Empty for an app with no
    /// install, and then the envelope leaves the field out.
    pub installs: Vec<InstallRecord>,
}

impl ShadowReceipt {
    /// The command line that reproduces this run.
    #[must_use]
    pub fn command(&self) -> String {
        format!(
            "acan receipt --repo {} --base {} --head {}",
            shell_word(&self.args.repo.display().to_string()),
            shell_word(&self.args.base),
            shell_word(&self.args.head),
        )
    }

    /// Whether the receipt blocks a land. A blocked receipt is still a receipt.
    #[must_use]
    pub fn blocked(&self) -> bool {
        matches!(self.receipt.gate_state, GateState::Blocked)
    }
}

/// Runs the contrast between `args.base` and `args.head` and builds the receipt.
///
/// # Errors
///
/// Returns [`ShadowError`] when a ref cannot be used, the protected contract cannot be read,
/// Docker cannot run the pinned image, the head conflicts with the base, or the contrast cannot
/// be made or is incomplete.
pub async fn run(args: &ShadowArgs) -> Result<ShadowReceipt, ShadowError> {
    run_with(args, Path::new(RUNNER_PROGRAM)).await
}

async fn run_with(args: &ShadowArgs, docker: &Path) -> Result<ShadowReceipt, ShadowError> {
    check_ref("--base", &args.base)?;
    check_ref("--head", &args.head)?;
    let repo = Repo::open_with_protected(&args.repo, args.base.as_str())
        .await
        .map_err(|err| ShadowError::Repo {
            repo: args.repo.clone(),
            detail: err.to_string(),
        })?;
    let base_head = repo.protected_head().await.map_err(|_| ShadowError::Ref {
        flag: "--base",
        name: args.base.clone(),
        detail: "--base must be a local branch; create one with `git branch <name> <sha>`"
            .to_owned(),
    })?;
    let base_commit = commit_sha(&base_head.to_string())?;
    let head_commit = resolve_commit(repo.root(), &args.head)
        .await
        .ok_or_else(|| ShadowError::Ref {
            flag: "--head",
            name: args.head.clone(),
            detail: "it does not name a commit of the clone".to_owned(),
        })?;

    // The protected paths are the same for every app; the run policy depends on the app's runtime,
    // which the protected contract names, so it is chosen once the contract is read.
    let protected = observe::protected_paths(&observe::runner_policy());
    let base = prepare(&repo, &base_commit, &protected, "the base").await?;

    // The contract is read from the base's protected paths, never from the head, so a head
    // cannot change the contract it is judged by.
    let scratch_root = tempfile::TempDir::new()
        .map_err(|err| ShadowError::Observe(format!("cannot create a scratch directory: {err}")))?;
    let snapshot = scratch_root.path().join("snapshot");
    observe::snapshot(&repo, &base, &snapshot)
        .await
        .map_err(|err| ShadowError::Observe(err.to_string()))?;
    let (app, corpus) =
        observe::read_protected(&snapshot).map_err(|err| ShadowError::Contract(err.to_string()))?;
    let policy = observe::policy_for(&app).map_err(|err| ShadowError::Contract(err.to_string()))?;
    policy
        .check_environment()
        .map_err(|err| ShadowError::Observe(format!("the container policy is invalid: {err}")))?;

    let candidate = prepare(&repo, &head_commit, &protected, "the head").await?;
    preflight(docker, &policy.image.name).await?;

    let scope = shadow_scope(&base_commit)?;
    let confirmation = shadow_confirmation(&scope)?;
    let ledger = fold(&[])
        .map_err(|err| ShadowError::Observe(format!("the ruling log cannot be folded: {err}")))?;
    let order = Order {
        scope: &scope,
        confirmation: &confirmation,
        ledger: &ledger,
        episodes: &[],
    };

    let scratch = scratch_root.path().join("scratch");
    let observations = scratch_root.path().join("observations");
    let slots = Semaphore::new(DEFAULT_WITNESS_CONCURRENCY);
    let runner = RunnerConfig::new(docker.to_path_buf(), APP_PORT);
    let cache = deps::cache_dir(args.cache_dir.as_deref()).map_err(ShadowError::Observe)?;
    let installer = Installer {
        repo: &repo,
        config: &runner,
        slots: &slots,
        cache: &cache,
        scratch: &scratch_root.path().join("installs"),
    };
    let install = |err: deps::DepsError| {
        ShadowError::Observe(format!("the dependencies cannot be installed: {err}"))
    };
    let base_installed = installer
        .install(&base, Role::Base, &app)
        .await
        .map_err(install)?;
    let candidate_installed = installer
        .install(&candidate, Role::Candidate, &app)
        .await
        .map_err(install)?;
    let installs: Vec<InstallRecord> = [base_installed.as_ref(), candidate_installed.as_ref()]
        .into_iter()
        .flatten()
        .map(|installed| installed.record.clone())
        .collect();
    let plan = Plan {
        repo: &repo,
        base: &base,
        candidate: &candidate,
        snapshot: &snapshot,
        scratch: &scratch,
        observations: &observations,
        app: &app,
        corpus: &corpus,
        policy: &policy,
        base_dependencies: base_installed
            .as_ref()
            .map(|installed| installed.modules.as_path()),
        candidate_dependencies: candidate_installed
            .as_ref()
            .map(|installed| installed.modules.as_path()),
        runner: &runner,
        slots: &slots,
        probes_at_once: DEFAULT_WITNESS_CONCURRENCY,
    };
    let judged = Box::pin(observe::judge(&plan, &order))
        .await
        .map_err(observe_refusal)?;
    if let Some(reason) = incomplete_reason(&judged, corpus.probes.len()) {
        return Err(ShadowError::Incomplete(reason));
    }

    let receipt_hash = judged
        .receipt
        .receipt_hash()
        .map_err(|err| ShadowError::Observe(format!("the receipt cannot be hashed: {err}")))?
        .to_string();
    Ok(ShadowReceipt {
        args: args.clone(),
        base_commit: base_commit.to_string(),
        head_commit: head_commit.to_string(),
        merged_commit: candidate.merged_commit().to_string(),
        discarded: candidate
            .discarded()
            .iter()
            .map(ToString::to_string)
            .collect(),
        receipt_hash,
        receipt: judged.receipt,
        installs,
    })
}

/// Refuses a ref that is empty, looks like an option, or has whitespace or control characters.
fn check_ref(flag: &'static str, name: &str) -> Result<(), ShadowError> {
    let problem = if name.is_empty() {
        "the ref is empty"
    } else if name.starts_with('-') {
        "a ref cannot start with '-'"
    } else if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        "a ref cannot contain whitespace or control characters"
    } else {
        return Ok(());
    };
    Err(ShadowError::Ref {
        flag,
        name: name.to_owned(),
        detail: problem.to_owned(),
    })
}

/// Resolves `spec` to the commit it names in the clone, or `None` when it names none.
async fn resolve_commit(root: &Path, spec: &str) -> Option<CommitSha> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{spec}^{{commit}}"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    CommitSha::from_str(text.trim()).ok()
}

fn commit_sha(text: &str) -> Result<CommitSha, ShadowError> {
    CommitSha::from_str(text)
        .map_err(|err| ShadowError::Observe(format!("{text} is not a commit: {err}")))
}

/// Prepares `commit` on the protected head. A conflict is a refusal: there is no merged commit
/// to test.
async fn prepare(
    repo: &Repo,
    commit: &CommitSha,
    protected: &ProtectedPaths,
    label: &str,
) -> Result<Preparation, ShadowError> {
    match repo
        .prepare(commit, protected)
        .await
        .map_err(|err| ShadowError::Observe(format!("{label} cannot be prepared: {err}")))?
    {
        PrepareOutcome::Prepared(prepared) => Ok(prepared),
        PrepareOutcome::Conflict { files, .. } => Err(ShadowError::Conflict {
            head: commit.to_string(),
            files: files.iter().map(ToString::to_string).collect(),
        }),
        _ => Err(ShadowError::Observe(format!(
            "{label} has no prepared outcome"
        ))),
    }
}

/// Checks that Docker answers and holds the pinned image. The runner never pulls, so a missing
/// image is a refusal here rather than a failed run later.
async fn preflight(docker: &Path, image: &ImageName) -> Result<(), ShadowError> {
    if let Err(detail) =
        docker_command(docker, &["version", "--format", "{{.Server.Version}}"]).await
    {
        return Err(ShadowError::Docker(format!(
            "`docker version` failed: {detail}"
        )));
    }
    if let Err(detail) = docker_command(docker, &["image", "inspect", image.as_str()]).await {
        return Err(ShadowError::Docker(format!(
            "image {} is not in the local store ({detail}); the runner never pulls",
            image.as_str()
        )));
    }
    Ok(())
}

/// Runs one docker command with a timeout. Its error is the first line of its stderr.
async fn docker_command(docker: &Path, args: &[&str]) -> Result<(), String> {
    let mut command = ProcessCommand::new(docker);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(PREFLIGHT_TIMEOUT, command.output())
        .await
        .map_err(|_| "it timed out".to_owned())?
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(stderr.lines().next().map_or_else(
        || "it failed without output".to_owned(),
        |line| line.trim().to_owned(),
    ))
}

/// The scope of a shadow run: no items, no must-keep rules, no open choices. Every difference is
/// unclaimed.
fn shadow_scope(base: &CommitSha) -> Result<Scope, ShadowError> {
    Ok(Scope {
        version: ScopeV1,
        order: order_ref()?,
        rev: ScopeRev(1),
        base_commit: base.clone(),
        body: ScopeBody {
            may: Vec::new(),
            must_keep: Vec::new(),
            open: Vec::new(),
        },
    })
}

/// The confirmation a shadow scope carries. It binds the scope so the receipt builder accepts
/// it, and names no person. The budget is zero because no order runs.
fn shadow_confirmation(scope: &Scope) -> Result<ScopeConfirmation, ShadowError> {
    Ok(ScopeConfirmation {
        version: ScopeConfirmationV1,
        order: scope.order.clone(),
        scope_rev: ScopeRev(scope.rev.0),
        scope_hash: scope.scope_hash().map_err(hashing)?,
        semantic_scope_hash: Some(scope.semantic_scope_hash().map_err(hashing)?),
        base_commit: scope.base_commit.clone(),
        ruling_versions: Vec::new(),
        selector_hashes: Vec::new(),
        example_hashes: Vec::new(),
        answers: Vec::new(),
        broad_grants: Vec::new(),
        budget: RepairBudget {
            repair_rounds: RepairRounds::try_from(0).map_err(|err| {
                ShadowError::Observe(format!("the repair budget is invalid: {err}"))
            })?,
            wall_time_ms: 0,
            model_cost_micros: 0,
            max_episodes: 0,
        },
        by: HumanRef::from_str(SHADOW_CONFIRMER).map_err(|err| {
            ShadowError::Observe(format!("the shadow confirmer is invalid: {err}"))
        })?,
        at: UnixMillis(0),
    })
}

fn order_ref() -> Result<OrderRef, ShadowError> {
    OrderRef::from_str(SHADOW_ORDER)
        .map_err(|err| ShadowError::Observe(format!("the shadow order is invalid: {err}")))
}

fn hashing(err: CanonicalError) -> ShadowError {
    ShadowError::Observe(format!("the scope cannot be hashed: {err}"))
}

/// Maps an observe failure to the refusal it is. A protected input that does not read is a
/// contract refusal, and a run that never set up is an incomplete contrast.
fn observe_refusal(err: ObserveError) -> ShadowError {
    match err {
        ObserveError::Protected { .. } => ShadowError::Contract(err.to_string()),
        ObserveError::SetupFailed { .. } => ShadowError::Incomplete(err.to_string()),
        other => ShadowError::Observe(other.to_string()),
    }
}

/// Why the contrast is incomplete, or `None` when every probe has three completed runs.
fn incomplete_reason(judged: &Judged, probes: usize) -> Option<String> {
    if judged.retained.len() != probes {
        return Some(format!(
            "{} of {probes} probes produced a contrast",
            judged.retained.len()
        ));
    }
    let mut runs = Vec::with_capacity(probes * 3);
    for retained in &judged.retained {
        let probe = retained.probe.to_string();
        let example = &retained.example;
        runs.push((format!("{probe} base A"), &example.base_a.run.outcome));
        runs.push((format!("{probe} base B"), &example.base_b.run.outcome));
        runs.push((format!("{probe} candidate"), &example.candidate.run.outcome));
    }
    first_incomplete(runs)
}

/// The first run that did not complete, named with its outcome. Only a completed run is evidence.
fn first_incomplete<'a>(
    runs: impl IntoIterator<Item = (String, &'a RunOutcome)>,
) -> Option<String> {
    runs.into_iter().find_map(|(run, outcome)| {
        let ended = match outcome {
            RunOutcome::Completed {} => return None,
            RunOutcome::SetupFailed { .. } => "setup failed",
            RunOutcome::TimedOut { .. } => "timed out",
            RunOutcome::Error { .. } => "errored",
            RunOutcome::Cancelled {} => "was cancelled",
            _ => "did not complete",
        };
        Some(format!("the {run} run {ended}"))
    })
}

/// Writes `shadow-receipt.json` and `shadow-receipt.md` into `dir`, creating it when missing.
///
/// # Errors
///
/// Returns [`ShadowError::Output`] when the directory or a file cannot be written.
pub fn write_out(report: &ShadowReceipt, dir: &Path) -> Result<(), ShadowError> {
    fs::create_dir_all(dir)
        .map_err(|err| ShadowError::Output(format!("cannot create {}: {err}", dir.display())))?;
    let json = json_text(report)?;
    let markdown = markdown_text(report)?;
    for (name, text) in [
        ("shadow-receipt.json", json),
        ("shadow-receipt.md", markdown),
    ] {
        let path = dir.join(name);
        fs::write(&path, text).map_err(|err| {
            ShadowError::Output(format!("cannot write {}: {err}", path.display()))
        })?;
    }
    Ok(())
}

/// The JSON envelope: the run's commits, the merged commit, the receipt hash, the command that
/// reproduces the run, and the receipt. Redacted before it is returned.
///
/// # Errors
///
/// Returns [`ShadowError::Output`] when the receipt cannot be serialized.
pub fn json_text(report: &ShadowReceipt) -> Result<String, ShadowError> {
    let receipt = serde_json::to_value(&report.receipt)
        .map_err(|err| ShadowError::Output(format!("the receipt cannot be serialized: {err}")))?;
    let mut envelope = json!({
        "version": SHADOW_VERSION,
        "command": report.command(),
        "base": { "ref": report.args.base, "commit": report.base_commit },
        "head": { "ref": report.args.head, "commit": report.head_commit },
        "merged_commit": report.merged_commit,
        "discarded_protected_paths": report.discarded,
        "scope": { "confirmed": false, "note": SCOPE_SENTENCE },
        "receipt_hash": report.receipt_hash,
        "receipt": receipt,
    });
    if !report.installs.is_empty() {
        envelope["installs"] = serde_json::to_value(&report.installs).map_err(|err| {
            ShadowError::Output(format!("the installs cannot be serialized: {err}"))
        })?;
    }
    redact_value(&mut envelope, &env_secrets(&report.args.repo));
    serde_json::to_string_pretty(&envelope)
        .map_err(|err| ShadowError::Output(format!("the envelope cannot be serialized: {err}")))
}

/// The receipt as Markdown, under a header that names the run. Redacted before it is returned.
///
/// # Errors
///
/// Returns [`ShadowError::Output`] when the receipt cannot be rendered.
pub fn markdown_text(report: &ShadowReceipt) -> Result<String, ShadowError> {
    let body = render_markdown(&report.receipt)
        .map_err(|err| ShadowError::Output(format!("the receipt cannot be rendered: {err}")))?;
    let text = format!("{}\n{body}", markdown_header(report));
    Ok(redact_markdown(&text, &env_secrets(&report.args.repo)))
}

fn markdown_header(report: &ShadowReceipt) -> String {
    let discarded = if report.discarded.is_empty() {
        "none".to_owned()
    } else {
        report
            .discarded
            .iter()
            .map(|path| code(path))
            .collect::<Vec<_>>()
            .join(", ")
    };
    [
        "# Acan shadow receipt".to_owned(),
        String::new(),
        SCOPE_SENTENCE.to_owned(),
        String::new(),
        format!("- Command: {}", code(&report.command())),
        format!(
            "- Base: {} at {}",
            code(&report.args.base),
            code(&report.base_commit)
        ),
        format!(
            "- Head: {} at {}",
            code(&report.args.head),
            code(&report.head_commit)
        ),
        format!("- Merged commit tested: {}", code(&report.merged_commit)),
        format!("- Receipt hash: {}", code(&report.receipt_hash)),
        format!("- Protected paths the head changed and the gate discarded: {discarded}"),
        String::new(),
    ]
    .join("\n")
}

/// A value in a Markdown code span. Backticks would end the span, so they are replaced.
fn code(text: &str) -> String {
    format!("`{}`", text.replace('`', "'"))
}

/// A word for a shell command line: bare when it is plain, double-quoted otherwise.
fn shell_word(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | ':' | '-' | '@'));
    if plain {
        text.to_owned()
    } else {
        format!("\"{}\"", text.replace('"', "\\\""))
    }
}

/// The values of the clone's own `.env` file that are long enough to be secrets. They are read
/// only to redact output. They never enter a container or a log.
fn env_secrets(repo: &Path) -> Vec<String> {
    let Ok(entries) = dotenvy::from_path_iter(repo.join(".env")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|(_, value)| value)
        .filter(|value| value.len() >= SECRET_MIN_LEN)
        .collect()
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEYS.iter().any(|needle| lower.contains(needle))
}

/// Replaces every string under a sensitive key, and every known secret inside any string. Numbers
/// and booleans are kept, so counts and denominators stay as the receipt has them.
fn redact_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if matches!(child, Value::String(_)) && is_sensitive_key(key) {
                    *child = Value::String(REDACTED.to_owned());
                } else {
                    redact_value(child, secrets);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item, secrets);
            }
        }
        Value::String(text) => *text = redact_text(text, secrets),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Replaces each known secret, each bearer token and each key-shaped word in `text`.
fn redact_text(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        out = out.replace(secret.as_str(), REDACTED);
    }
    redact_keys(&redact_bearer(&out))
}

/// Whether a word with the word before it is a secret: a provider key by its prefix, or a long
/// word with digits and letters right after `key`, `token` or `secret`.
fn is_key_shape(word: &str, previous: &str) -> bool {
    let prefixed = KEY_PREFIXES.iter().any(|prefix| {
        word.strip_prefix(prefix)
            .is_some_and(|body| body.len() >= KEY_BODY_MIN_LEN)
    });
    let names_secret = previous
        .split(['_', '-'])
        .any(|part| SECRET_WORDS.contains(&part.to_ascii_lowercase().as_str()));
    let token = word.len() >= TOKEN_MIN_LEN
        && word.bytes().any(|byte| byte.is_ascii_digit())
        && word.bytes().any(|byte| byte.is_ascii_alphabetic());
    prefixed || (names_secret && token)
}

/// Replaces every word that is a secret by [`is_key_shape`]. Each word is judged with the word
/// before it, so a value after `api_key:` is caught whatever the key is called.
fn redact_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous = "";
    let mut rest = text;
    while let Some(start) = rest.find(is_word_char) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len());
        let word = &rest[..end];
        if is_key_shape(word, previous) {
            out.push_str(REDACTED);
        } else {
            out.push_str(word);
        }
        previous = word;
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// The characters a word is made of: letters, digits, and the marks keys and encoded tokens use.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '/')
}

/// Replaces the token after every `Bearer ` in `text`, matched without regard to case.
fn redact_bearer(text: &str) -> String {
    const PREFIX: &str = "bearer ";
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(found) = lower[cursor..].find(PREFIX) {
        let start = cursor + found + PREFIX.len();
        out.push_str(&text[cursor..start]);
        let end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |offset| start + offset);
        if end > start {
            out.push_str(REDACTED);
        }
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Redacts Markdown line by line. A line that names a sensitive key loses everything after its
/// first `:` or `=`, because the rest of such a line is a value.
fn redact_markdown(text: &str, secrets: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        out.push_str(&redact_line(line, secrets));
        out.push('\n');
    }
    out
}

fn redact_line(line: &str, secrets: &[String]) -> String {
    let text = redact_text(line, secrets);
    let lower = text.to_ascii_lowercase();
    for key in SENSITIVE_KEYS {
        let Some(at) = lower.find(key) else {
            continue;
        };
        let after = at + key.len();
        if let Some(offset) = text[after..].find([':', '=']) {
            return format!("{}{REDACTED}", &text[..after + offset + 1]);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    const RECEIPT: &str = include_str!("../../acan-intent/fixtures/receipt.json");
    const APP_CONTRACT: &str = include_str!("../../acan-intent/fixtures/app-contract.json");
    const PROBE_CORPUS: &str = include_str!("../../acan-intent/fixtures/probe-corpus.json");

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A clone on `main` with one commit holding `files`.
    fn clone_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temporary clone");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "Tester"]);
        git(dir.path(), &["config", "user.email", "tester@localhost"]);
        for (path, text) in files {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().expect("a file has a parent")).expect("directory");
            fs::write(&full, text).expect("file");
        }
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "base"]);
        dir
    }

    fn contract_files() -> Vec<(&'static str, &'static str)> {
        vec![
            (".acan/app.json", APP_CONTRACT),
            (".acan/probes/core.json", PROBE_CORPUS),
        ]
    }

    fn args(repo: &Path, base: &str, head: &str) -> ShadowArgs {
        ShadowArgs {
            repo: repo.to_path_buf(),
            base: base.to_owned(),
            head: head.to_owned(),
            cache_dir: None,
        }
    }

    fn missing_docker() -> PathBuf {
        PathBuf::from("acan-no-such-docker-program")
    }

    fn fixture_receipt() -> Receipt {
        serde_json::from_str(RECEIPT).expect("the receipt fixture decodes")
    }

    fn report_for(repo: &Path, receipt: Receipt) -> ShadowReceipt {
        ShadowReceipt {
            args: args(repo, "main", "feature"),
            base_commit: "1".repeat(40),
            head_commit: "2".repeat(40),
            merged_commit: "3".repeat(40),
            discarded: vec![".acan/app.json".to_owned()],
            receipt_hash: "abc123".to_owned(),
            receipt,
            installs: Vec::new(),
        }
    }

    #[tokio::test]
    async fn an_empty_directory_is_refused_as_not_a_repository() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let error = run_with(&args(dir.path(), "main", "main"), &missing_docker())
            .await
            .expect_err("a directory that is not a clone is refused");
        assert!(
            matches!(error, ShadowError::Repo { .. }),
            "not a repository: {error}"
        );
    }

    #[tokio::test]
    async fn a_base_that_is_not_a_local_branch_is_refused() {
        let repo = clone_with(&contract_files());
        let error = run_with(
            &args(repo.path(), "no-such-branch", "main"),
            &missing_docker(),
        )
        .await
        .expect_err("a base that is not a branch is refused");
        assert!(
            matches!(&error, ShadowError::Ref { flag: "--base", .. }),
            "base ref: {error}"
        );
    }

    #[tokio::test]
    async fn a_head_that_names_no_commit_is_refused() {
        let repo = clone_with(&contract_files());
        let error = run_with(
            &args(repo.path(), "main", "does-not-exist"),
            &missing_docker(),
        )
        .await
        .expect_err("a head that names no commit is refused");
        assert!(
            matches!(&error, ShadowError::Ref { flag: "--head", .. }),
            "head ref: {error}"
        );
    }

    #[tokio::test]
    async fn a_ref_that_looks_like_an_option_is_refused() {
        let repo = clone_with(&contract_files());
        let error = run_with(
            &args(repo.path(), "main", "--output=/tmp/x"),
            &missing_docker(),
        )
        .await
        .expect_err("an option-like ref is refused");
        assert!(
            matches!(&error, ShadowError::Ref { flag: "--head", .. }),
            "option-like ref: {error}"
        );
    }

    #[tokio::test]
    async fn a_clone_without_a_protected_contract_is_refused_before_docker() {
        let repo = clone_with(&[("README.md", "no contract here\n")]);
        let error = run_with(&args(repo.path(), "main", "main"), &missing_docker())
            .await
            .expect_err("a clone without .acan/app.json is refused");
        assert!(
            matches!(error, ShadowError::Contract(_)),
            "missing contract: {error}"
        );
    }

    #[tokio::test]
    async fn a_contract_that_does_not_decode_is_refused() {
        let repo = clone_with(&[
            (".acan/app.json", "{}"),
            (".acan/probes/core.json", PROBE_CORPUS),
        ]);
        let error = run_with(&args(repo.path(), "main", "main"), &missing_docker())
            .await
            .expect_err("an app contract that does not decode is refused");
        assert!(
            matches!(error, ShadowError::Contract(_)),
            "invalid contract: {error}"
        );
    }

    #[tokio::test]
    async fn a_missing_docker_is_refused_with_no_unisolated_fallback() {
        let repo = clone_with(&contract_files());
        let error = run_with(&args(repo.path(), "main", "main"), &missing_docker())
            .await
            .expect_err("a run without docker is refused");
        assert!(
            matches!(error, ShadowError::Docker(_)),
            "no docker: {error}"
        );
    }

    #[test]
    fn a_run_that_did_not_complete_makes_the_contrast_incomplete() {
        let completed = RunOutcome::Completed {};
        let setup = RunOutcome::SetupFailed {
            step_id: None,
            detail: "the app did not become ready".to_owned(),
        };
        let reason = first_incomplete([
            ("probe-1 base A".to_owned(), &completed),
            ("probe-1 candidate".to_owned(), &setup),
        ])
        .expect("a run that did not set up is incomplete");
        assert_eq!(reason, "the probe-1 candidate run setup failed");
        assert_eq!(
            first_incomplete([("probe-1 base A".to_owned(), &completed)]),
            None,
            "completed runs are complete evidence"
        );
    }

    #[test]
    fn the_command_line_quotes_only_what_needs_quotes() {
        assert_eq!(shell_word("main"), "main");
        assert_eq!(shell_word("unit/df1"), "unit/df1");
        assert_eq!(
            shell_word("D:\\work\\my clone\\acan"),
            "\"D:\\work\\my clone\\acan\""
        );
        let report = report_for(Path::new("clone"), fixture_receipt());
        assert_eq!(
            report.command(),
            "acan receipt --repo clone --base main --head feature"
        );
    }

    #[test]
    fn the_json_names_the_commits_hash_and_command() {
        let receipt = fixture_receipt();
        let report = report_for(Path::new("clone"), receipt);
        let text = json_text(&report).expect("the envelope serializes");
        let value: Value = serde_json::from_str(&text).expect("the envelope is JSON");
        assert_eq!(value["version"], SHADOW_VERSION);
        assert_eq!(value["command"], report.command());
        assert_eq!(value["base"]["commit"], report.base_commit);
        assert_eq!(value["head"]["commit"], report.head_commit);
        assert_eq!(value["merged_commit"], report.merged_commit);
        assert_eq!(value["receipt_hash"], "abc123");
        assert_eq!(value["scope"]["confirmed"], false);
        assert_eq!(value["discarded_protected_paths"][0], ".acan/app.json");
        assert!(
            value["receipt"].is_object(),
            "the receipt is inside the envelope"
        );
    }

    #[test]
    fn the_markdown_names_the_run_before_the_receipt() {
        let report = report_for(Path::new("clone"), fixture_receipt());
        let text = markdown_text(&report).expect("the markdown renders");
        assert!(
            text.starts_with("# Acan shadow receipt\n"),
            "header: {text}"
        );
        assert!(text.contains("- Command: `acan receipt --repo clone --base main --head feature`"));
        assert!(text.contains("- Receipt hash: `abc123`"));
        assert!(text.contains(SCOPE_SENTENCE), "scope sentence: {text}");
        assert!(text.contains("`.acan/app.json`"), "discarded path listed");
    }

    #[test]
    fn a_shadow_run_never_says_a_person_confirmed_or_ruled() {
        let report = report_for(Path::new("clone"), fixture_receipt());
        let markdown = markdown_text(&report).expect("the markdown renders");
        let json = json_text(&report).expect("the envelope serializes");
        // The one sentence that says nothing was ruled is the negation this run is labelled with.
        for text in [
            markdown.replace(SCOPE_SENTENCE, ""),
            json.replace(SCOPE_SENTENCE, ""),
        ] {
            assert!(!text.contains("confirmed by"), "confirmed by: {text}");
            assert!(
                !text.to_ascii_lowercase().contains("ruled"),
                "ruled: {text}"
            );
        }
    }

    #[test]
    fn bearer_tokens_and_known_secrets_are_redacted_from_text() {
        let secrets = vec!["sk-live-0123456789".to_owned()];
        let text = "Authorization: Bearer abc.def.ghi and key sk-live-0123456789 done";
        let redacted = redact_text(text, &secrets);
        assert!(
            !redacted.contains("abc.def.ghi"),
            "bearer token: {redacted}"
        );
        assert!(
            !redacted.contains("sk-live-0123456789"),
            "known secret: {redacted}"
        );
        assert!(redacted.contains("Bearer <redacted>"));
        assert!(redacted.ends_with("done"), "text after the secret is kept");
    }

    #[test]
    fn key_shaped_words_are_redacted_under_any_key_name() {
        let mut value = json!({
            "note": concat!("a pasted ", "ghp_", "fake0123456789abcdef", " was seen"),
            "label": concat!("sk-", "fake-0123456789abcdef"),
            "detail": "api_key 0123456789abcdefghij0 and done",
            "short": "token 0123456789 is short",
            "count": 4,
        });
        redact_value(&mut value, &[]);
        assert_eq!(value["note"], "a pasted <redacted> was seen");
        assert_eq!(value["label"], REDACTED);
        assert_eq!(value["detail"], "api_key <redacted> and done");
        assert_eq!(
            value["short"], "token 0123456789 is short",
            "a short word is kept"
        );
        assert_eq!(value["count"], 4);
    }

    #[test]
    fn sensitive_keys_are_redacted_but_counts_are_kept() {
        let mut value = json!({
            "set_cookie": "sid=abc",
            "headers": { "Authorization": "Bearer xyz" },
            "tokens": 12,
            "items": [{ "note": "fine" }],
        });
        redact_value(&mut value, &[]);
        assert_eq!(value["set_cookie"], REDACTED);
        assert_eq!(value["headers"]["Authorization"], REDACTED);
        assert_eq!(
            value["tokens"], 12,
            "a count under a sensitive word is kept"
        );
        assert_eq!(value["items"][0]["note"], "fine");
    }

    #[test]
    fn markdown_lines_that_name_a_sensitive_key_lose_their_value() {
        let text = "| set-cookie | sid=abc123 |\n| path | /api |";
        let redacted = redact_markdown(text, &[]);
        assert!(!redacted.contains("abc123"), "cookie value: {redacted}");
        assert!(redacted.contains("| path | /api |"), "other lines are kept");
    }

    #[test]
    fn a_secret_too_short_to_be_one_is_not_read_from_env() {
        let dir = tempfile::tempdir().expect("temporary directory");
        fs::write(
            dir.path().join(".env"),
            "SHORT=abc\nLONG_KEY=0123456789abcdef\n",
        )
        .expect("env file");
        assert_eq!(env_secrets(dir.path()), vec!["0123456789abcdef".to_owned()]);
        assert!(env_secrets(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn the_out_directory_receives_both_formats() {
        let report = report_for(Path::new("clone"), fixture_receipt());
        let dir = tempfile::tempdir().expect("temporary directory");
        let out = dir.path().join("nested");
        write_out(&report, &out).expect("both files are written");
        let json = fs::read_to_string(out.join("shadow-receipt.json")).expect("json file");
        let markdown = fs::read_to_string(out.join("shadow-receipt.md")).expect("markdown file");
        assert!(json.contains(SHADOW_VERSION));
        assert!(markdown.starts_with("# Acan shadow receipt"));
    }
}
