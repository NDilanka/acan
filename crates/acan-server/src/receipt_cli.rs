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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use acan_git::{Preparation, PrepareOutcome, ProtectedPaths, Repo, git_env};
use acan_intent::fold::fold;
use acan_intent::{
    CanonicalError, CommitSha, GateState, HumanRef, ImageName, OrderRef, Receipt, RepairBudget,
    RepairRounds, RepoPath, RunOutcome, Scope, ScopeBody, ScopeConfirmation, ScopeConfirmationV1,
    ScopeRev, ScopeV1, UnixMillis,
};
use acan_witness::RunnerConfig;
use clap::ValueEnum;
use serde_json::{Value, json};
use tokio::process::Command as ProcessCommand;
use tokio::sync::Semaphore;

use crate::DEFAULT_WITNESS_CONCURRENCY;
use crate::deps::{self, InstallRecord, Installer, Role};
use crate::init;
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
/// How long `git rev-parse` may take to resolve the head before the run refuses it.
const REF_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// The arguments do not name what the mode needs.
    #[error("{0}")]
    Usage(String),
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
    /// The head ref, observed against the base. Required, except with `--check-contract`.
    #[arg(long)]
    pub head: Option<String>,
    /// Where a node app's installed dependencies are cached. Defaults to `$XDG_CACHE_HOME/acan/deps`,
    /// or `~/.cache/acan/deps`.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    /// Checks the base's protected contract without running it, and needs no `--head`. Docker is
    /// reported as information only, and no container starts.
    #[arg(long)]
    pub check_contract: bool,
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
            shell_word(self.args.head.as_deref().unwrap_or_default()),
        )
    }

    /// Whether the receipt blocks a land. A blocked receipt is still a receipt.
    #[must_use]
    pub fn blocked(&self) -> bool {
        matches!(self.receipt.gate_state, GateState::Blocked)
    }
}

/// Runs the contrast between `args.base` and `args.head` and builds the receipt. When `diagnostics`
/// is given, a run whose start fails keeps the app's last output there. The output is not part of
/// the receipt.
///
/// # Errors
///
/// Returns [`ShadowError`] when a ref cannot be used, the protected contract cannot be read,
/// Docker cannot run the pinned image, the head conflicts with the base, or the contrast cannot
/// be made or is incomplete.
pub async fn run(
    args: &ShadowArgs,
    diagnostics: Option<&Path>,
) -> Result<ShadowReceipt, ShadowError> {
    run_with(args, Path::new(RUNNER_PROGRAM), diagnostics).await
}

async fn run_with(
    args: &ShadowArgs,
    docker: &Path,
    diagnostics: Option<&Path>,
) -> Result<ShadowReceipt, ShadowError> {
    let head = args.head.as_deref().ok_or_else(|| {
        ShadowError::Usage(
            "--head is required; it is only omitted with --check-contract".to_owned(),
        )
    })?;
    check_ref("--base", &args.base)?;
    check_ref("--head", head)?;
    let (repo, base_read) = open_base(args).await?;
    let head_commit = match resolve_commit(repo.root(), head).await {
        Ok(Some(commit)) => commit,
        Ok(None) => {
            return Err(ShadowError::Ref {
                flag: "--head",
                name: head.to_owned(),
                detail: "it does not name a commit of the clone".to_owned(),
            });
        }
        Err(detail) => {
            return Err(ShadowError::Ref {
                flag: "--head",
                name: head.to_owned(),
                detail: format!("git could not resolve it: {detail}"),
            });
        }
    };

    // The protected paths are the same for every app; the run policy depends on the app's runtime,
    // which the protected contract names, so it is chosen once the contract is read.
    let protected = observe::protected_paths(&observe::runner_policy());
    let base = prepare(&repo, &base_read, &protected, "the base").await?;
    // Every run names the commit the base was prepared on, so the envelope names what was tested.
    ensure_unmoved(base.protected_head(), &base_read, &args.base)?;
    let base_commit = base.protected_head().clone();

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
    ensure_unmoved(candidate.protected_head(), &base_commit, &args.base)?;
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
        diagnostics,
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

/// Opens the clone and resolves the protected head that `--base` names. The base must be a local
/// branch.
async fn open_base(args: &ShadowArgs) -> Result<(Repo, CommitSha), ShadowError> {
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
    Ok((repo, base_commit))
}

/// What `--check-contract` found in the base's protected contract.
#[derive(Debug, Clone)]
pub struct ContractCheck {
    /// The base commit the contract was read from.
    pub base_commit: String,
    /// The runtime the contract names.
    pub runtime: String,
    /// Whether the contract names an install phase.
    pub install: bool,
    /// How many probes the corpus holds.
    pub probes: usize,
    /// Whether Docker answered. This is information only: the check starts no container.
    pub docker: Result<(), String>,
}

/// Checks the base's protected contract without running the app. It reads the contract from the
/// base's protected paths, as a receipt does, decodes it, runs the contract's own checks and the
/// corpus cross-checks, and checks the container policy the runs would use. Docker is asked
/// whether it answers, and that answer is information only.
///
/// # Errors
///
/// Returns [`ShadowError`] when the base cannot be used, the clone cannot be read, the contract
/// does not decode or fails its checks, or its container policy is invalid.
pub async fn check_contract(args: &ShadowArgs) -> Result<ContractCheck, ShadowError> {
    check_contract_with(args, Path::new(RUNNER_PROGRAM)).await
}

async fn check_contract_with(
    args: &ShadowArgs,
    docker: &Path,
) -> Result<ContractCheck, ShadowError> {
    check_ref("--base", &args.base)?;
    let (repo, base_commit) = open_base(args).await?;
    let protected = observe::runner_policy().mounts.protected;
    let scratch_root = tempfile::TempDir::new()
        .map_err(|err| ShadowError::Observe(format!("cannot create a scratch directory: {err}")))?;
    let snapshot = scratch_root.path().join("snapshot");
    write_protected(repo.root(), &base_commit, &protected, &snapshot).await?;
    let (app, corpus) =
        observe::read_protected(&snapshot).map_err(|err| ShadowError::Contract(err.to_string()))?;
    init::cross_check(&corpus).map_err(|err| ShadowError::Contract(err.to_string()))?;
    let policy = observe::policy_for(&app).map_err(|err| ShadowError::Contract(err.to_string()))?;
    policy
        .check_environment()
        .map_err(|err| ShadowError::Observe(format!("the container policy is invalid: {err}")))?;
    Ok(ContractCheck {
        base_commit: base_commit.to_string(),
        runtime: app.runtime.as_str().to_owned(),
        install: app.install.is_some(),
        probes: corpus.probes.len(),
        docker: docker_command(docker, &["version", "--format", "{{.Server.Version}}"]).await,
    })
}

/// The check as a person reads it: the contract's facts, then Docker as information.
#[must_use]
pub fn check_text(check: &ContractCheck) -> String {
    let install = if check.install {
        "with an install phase"
    } else {
        "with no install phase"
    };
    let docker = match &check.docker {
        Ok(()) => "available".to_owned(),
        Err(detail) => format!("not available: {detail}"),
    };
    format!(
        "contract ok at base {}: runtime {}, {install}, {} probes\n\
         docker: {docker} (information only; the check starts no container)",
        check.base_commit, check.runtime, check.probes
    )
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

/// Resolves `spec` to the commit it names in the clone. `Ok(None)` means git ran and the spec
/// names no commit. An error means git did not answer: it did not start, failed, or timed out.
async fn resolve_commit(root: &Path, spec: &str) -> Result<Option<CommitSha>, String> {
    resolve_commit_with(Path::new("git"), root, spec, REF_TIMEOUT).await
}

async fn resolve_commit_with(
    git: &Path,
    root: &Path,
    spec: &str,
    limit: Duration,
) -> Result<Option<CommitSha>, String> {
    let mut command = ProcessCommand::new(git);
    git_env::prepare(command.as_std_mut());
    command
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{spec}^{{commit}}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(limit, command.output())
        .await
        .map_err(|_| format!("git did not answer within {} s", limit.as_secs()))?
        .map_err(|err| format!("git cannot run: {err}"))?;
    match output.status.code() {
        Some(0) => {
            let text = String::from_utf8_lossy(&output.stdout);
            CommitSha::from_str(text.trim())
                .map(Some)
                .map_err(|err| format!("git printed a value that is not a commit: {err}"))
        }
        // With `--quiet`, git exits 1 and prints nothing when the spec names no object.
        Some(1) => Ok(None),
        _ => Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map_or_else(
                || format!("git exited with {}", output.status),
                str::to_owned,
            )),
    }
}

fn commit_sha(text: &str) -> Result<CommitSha, ShadowError> {
    CommitSha::from_str(text)
        .map_err(|err| ShadowError::Observe(format!("{text} is not a commit: {err}")))
}

/// Refuses when the protected head is not the commit a run was pinned to. A branch that moved
/// between reading it and preparing on it would otherwise be tested under the old name.
fn ensure_unmoved(now: &CommitSha, pinned: &CommitSha, base: &str) -> Result<(), ShadowError> {
    if now == pinned {
        return Ok(());
    }
    Err(ShadowError::Ref {
        flag: "--base",
        name: base.to_owned(),
        detail: "the branch moved while the receipt was prepared; run it again".to_owned(),
    })
}

/// Writes the protected paths of `commit` into `dir`, in the layout `observe::read_protected`
/// reads. The files are listed and read with git's read-only commands, so the clone gets no
/// index, objects or refs. A link or a submodule under a protected path is refused, because a
/// contract is only ever plain files. Every listed name must be a portable repo path, and no two
/// names may name one file. The whole listing is checked before the first file is written.
async fn write_protected(
    root: &Path,
    commit: &CommitSha,
    protected: &BTreeSet<RepoPath>,
    dir: &Path,
) -> Result<(), ShadowError> {
    let commit = commit.to_string();
    let mut files: BTreeMap<RepoPath, String> = BTreeMap::new();
    for path in protected {
        let listing = git_stdout(
            root,
            &[
                "ls-tree",
                "-r",
                "-z",
                "--full-tree",
                &commit,
                "--",
                path.as_str(),
            ],
        )
        .await?;
        for entry in listing
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let (name, oid) = listed_blob(entry)?;
            files.insert(name, oid);
        }
    }
    check_no_collisions(&files)?;
    for (name, oid) in &files {
        let bytes = git_stdout(root, &["cat-file", "blob", oid]).await?;
        let target = dir.join(name.as_str());
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                ShadowError::Observe(format!("cannot create {}: {err}", parent.display()))
            })?;
        }
        write_new(&target, &bytes)?;
    }
    Ok(())
}

/// One entry of `git ls-tree -z`, read as the portable repo path it names and its blob id. The
/// name is decoded strictly, so bytes that are not UTF-8 are refused rather than replaced.
fn listed_blob(entry: &[u8]) -> Result<(RepoPath, String), ShadowError> {
    let malformed = || {
        ShadowError::Observe(format!(
            "git listed {:?} oddly",
            String::from_utf8_lossy(entry)
        ))
    };
    let tab = entry
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(malformed)?;
    let (meta, name) = (&entry[..tab], &entry[tab + 1..]);
    let name = std::str::from_utf8(name).map_err(|_| {
        ShadowError::Contract(format!(
            "{:?} is not UTF-8; a contract is only plain files with portable names",
            String::from_utf8_lossy(name)
        ))
    })?;
    let meta = std::str::from_utf8(meta).map_err(|_| malformed())?;
    let mut fields = meta.split(' ');
    let (Some(mode), Some(kind), Some(oid), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(malformed());
    };
    if kind != "blob" || mode == "120000" {
        return Err(ShadowError::Contract(format!(
            "{name} is a link or a submodule; a contract is only plain files"
        )));
    }
    let path = RepoPath::from_str(name).map_err(|_| {
        ShadowError::Contract(format!(
            "{name:?} is not a portable repo path, so it is refused: it may hold a backslash, a \
             colon, a reserved device name, or a segment that ends in a dot or a space"
        ))
    })?;
    // Windows gives a long name a short alias such as `LONGFI~1.TXT`, and a `~` before a digit can
    // be that alias. A name outside ASCII folds to another name there. Neither is a contract name.
    if !name.is_ascii()
        || name
            .as_bytes()
            .windows(2)
            .any(|pair| pair[0] == b'~' && pair[1].is_ascii_digit())
    {
        return Err(ShadowError::Contract(format!(
            "{name:?} is refused: a contract's names are ASCII, and no `~` is followed by a digit, \
             because Windows would read such a name as the short alias of another"
        )));
    }
    Ok((path, oid.to_owned()))
}

/// Refuses names that cannot all be written as files of one tree. Two names that differ only by
/// case name one file on a case-insensitive file system, and a name that is a file cannot also
/// be a directory on the way to another name, in any case.
fn check_no_collisions(files: &BTreeMap<RepoPath, String>) -> Result<(), ShadowError> {
    let mut folded: BTreeMap<String, &str> = BTreeMap::new();
    for path in files.keys() {
        if let Some(other) = folded.insert(path.as_str().to_ascii_lowercase(), path.as_str()) {
            return Err(ShadowError::Contract(format!(
                "{other:?} and {:?} name one file on a case-insensitive file system, so a \
                 contract cannot hold both",
                path.as_str()
            )));
        }
    }
    for (name, original) in &folded {
        for (end, _) in name.match_indices('/') {
            if let Some(ancestor) = folded.get(&name[..end]) {
                return Err(ShadowError::Contract(format!(
                    "{ancestor:?} is a file and {original:?} lies under it, so a contract cannot \
                     hold both"
                )));
            }
        }
    }
    Ok(())
}

/// Writes `bytes` to `target`, which must not exist yet. A second name that reaches a file already
/// written is refused, so no name can replace another one in the snapshot.
fn write_new(target: &Path, bytes: &[u8]) -> Result<(), ShadowError> {
    use std::io::Write;
    let failed = |err: std::io::Error| {
        ShadowError::Observe(format!("cannot write {}: {err}", target.display()))
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::AlreadyExists => ShadowError::Contract(format!(
                "{} is already written, so two names of the contract name one file",
                target.display()
            )),
            _ => failed(err),
        })?;
    file.write_all(bytes).map_err(failed)
}

/// Runs git in `root` and returns its standard output. A failure carries git's message, and a
/// missing object is a failure, because git is not allowed to fetch it.
async fn git_stdout(root: &Path, args: &[&str]) -> Result<Vec<u8>, ShadowError> {
    let mut command = ProcessCommand::new("git");
    git_env::prepare(command.as_std_mut());
    let output = command
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|err| ShadowError::Observe(format!("cannot run git: {err}")))?;
    if !output.status.success() {
        return Err(ShadowError::Observe(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
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
    // Both files are staged in the directory first, so a write that fails here changes nothing.
    let mut staged = Vec::with_capacity(2);
    for (name, text) in [
        ("shadow-receipt.json", json),
        ("shadow-receipt.md", markdown),
    ] {
        let path = dir.join(name);
        let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|err| {
            ShadowError::Output(format!("cannot write {}: {err}", path.display()))
        })?;
        file.write_all(text.as_bytes()).map_err(|err| {
            ShadowError::Output(format!("cannot write {}: {err}", path.display()))
        })?;
        staged.push((path, file));
    }
    // Then each staged file takes its place. When one cannot, the files already placed go back
    // to what they were, so the pair is either both new or both as they were before.
    let mut placed: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::with_capacity(2);
    for (path, file) in staged {
        let previous = fs::read(&path).ok();
        if let Err(err) = file.persist(&path) {
            restore(&placed);
            return Err(ShadowError::Output(format!(
                "cannot write {}: {}",
                path.display(),
                err.error
            )));
        }
        placed.push((path, previous));
    }
    Ok(())
}

/// Puts back the files a failed `write_out` had already replaced: their earlier bytes, or nothing
/// when they did not exist. The error that is reported is the write that failed, so a failure
/// here is not reported on its own.
fn restore(placed: &[(PathBuf, Option<Vec<u8>>)]) {
    for (path, previous) in placed.iter().rev() {
        let _ = match previous {
            Some(bytes) => fs::write(path, bytes),
            None => fs::remove_file(path),
        };
    }
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
            code(report.args.head.as_deref().unwrap_or_default()),
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

/// Whether a character ends a bearer token. Whitespace ends it, and so does a quote, a bracket or
/// a comma that frames the token in text or code. Any other character belongs to the token, so a
/// token with an unusual character is redacted whole rather than cut at that character.
fn ends_bearer_token(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
        )
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
            .find(ends_bearer_token)
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
/// first `:` or `=`, because the rest of such a line is a value. Each line keeps its own ending,
/// so CRLF stays CRLF and a last line without one stays without one.
fn redact_markdown(text: &str, secrets: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    for segment in text.split_inclusive('\n') {
        let (line, ending) = match segment.strip_suffix("\r\n") {
            Some(line) => (line, "\r\n"),
            None => segment
                .strip_suffix('\n')
                .map_or((segment, ""), |line| (line, "\n")),
        };
        out.push_str(&redact_line(line, secrets));
        out.push_str(ending);
    }
    out
}

fn redact_line(line: &str, secrets: &[String]) -> String {
    let text = redact_text(line, secrets);
    let lower = text.to_ascii_lowercase();
    // The leftmost key in the line wins, so a line that names two keys loses every value, not
    // just the value of the key that comes first in the list.
    let Some(after) = SENSITIVE_KEYS
        .iter()
        .filter_map(|key| lower.find(key).map(|at| at + key.len()))
        .min()
    else {
        return text;
    };
    if let Some(offset) = text[after..].find([':', '=']) {
        return format!("{}{REDACTED}", &text[..after + offset + 1]);
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
            head: Some(head.to_owned()),
            cache_dir: None,
            check_contract: false,
        }
    }

    fn check_args(repo: &Path, base: &str) -> ShadowArgs {
        ShadowArgs {
            head: None,
            check_contract: true,
            ..args(repo, base, "")
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
        let error = run_with(&args(dir.path(), "main", "main"), &missing_docker(), None)
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
            None,
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
            None,
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
            None,
        )
        .await
        .expect_err("an option-like ref is refused");
        assert!(
            matches!(&error, ShadowError::Ref { flag: "--head", .. }),
            "option-like ref: {error}"
        );
    }

    #[tokio::test]
    async fn a_run_without_a_head_is_refused_before_anything_runs() {
        let repo = clone_with(&contract_files());
        let mut without = args(repo.path(), "main", "main");
        without.head = None;
        let error = run_with(&without, &missing_docker(), None)
            .await
            .expect_err("a run needs a head");
        assert!(
            matches!(&error, ShadowError::Usage(text) if text.contains("--check-contract")),
            "missing head: {error}"
        );
    }

    #[tokio::test]
    async fn a_refused_run_creates_no_diagnostics_directory() {
        let repo = clone_with(&contract_files());
        let dir = tempfile::tempdir().expect("temporary directory");
        let diagnostics = dir.path().join("diagnostics");
        let error = run_with(
            &args(repo.path(), "main", "main"),
            &missing_docker(),
            Some(&diagnostics),
        )
        .await
        .expect_err("a run without docker is refused");
        assert!(
            matches!(error, ShadowError::Docker(_)),
            "no docker: {error}"
        );
        assert!(
            !diagnostics.exists(),
            "a refused run keeps no diagnostics directory"
        );
    }

    /// Every path under `dir` with its length, sorted, so a change to a directory shows.
    fn listing(dir: &Path) -> Vec<(PathBuf, u64)> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            for entry in fs::read_dir(&current).expect("a directory") {
                let path = entry.expect("an entry").path();
                let meta = fs::symlink_metadata(&path).expect("metadata");
                if meta.is_dir() {
                    pending.push(path.clone());
                }
                found.push((path, meta.len()));
            }
        }
        found.sort();
        found
    }

    #[cfg(unix)]
    fn set_writable(dir: &Path, writable: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if writable { 0o755 } else { 0o555 };
        fs::set_permissions(dir, fs::Permissions::from_mode(mode)).expect("the mode is set");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_contract_writes_nothing_to_the_clone() {
        let repo = clone_with(&contract_files());
        let git_dir = repo.path().join(".git");
        let before = listing(&git_dir);
        // A check that needs a file in the git directory, such as an index, fails here.
        set_writable(&git_dir, false);
        let probe_refused = fs::write(git_dir.join("probe"), "").is_err();
        let checked =
            check_contract_with(&check_args(repo.path(), "main"), &missing_docker()).await;
        set_writable(&git_dir, true);

        assert!(
            probe_refused,
            "the test needs a git directory its user cannot write"
        );
        checked.expect("the check passes with a git directory it cannot write");
        assert_eq!(
            listing(&git_dir),
            before,
            "the check left the clone's git directory as it found it"
        );
    }

    /// A partial clone of `origin` whose file contents are not local. Its promisor remote runs a
    /// script that only records that it ran, so a fetch leaves a marker. Returns the clone and the
    /// marker's path.
    #[cfg(unix)]
    fn promisor_clone_of(origin: &Path) -> (tempfile::TempDir, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        git(origin, &["config", "uploadpack.allowFilter", "true"]);
        let dir = tempfile::tempdir().expect("a directory for the partial clone");
        let partial = dir.path().join("partial");
        let source = format!("file://{}", origin.display());
        git(
            dir.path(),
            &[
                "clone",
                "-q",
                "--no-checkout",
                "--filter=blob:none",
                &source,
                &partial.to_string_lossy(),
            ],
        );
        let marker = dir.path().join("fetch-ran");
        let transport = dir.path().join("fake-upload-pack");
        fs::write(
            &transport,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .expect("the transport script");
        fs::set_permissions(&transport, fs::Permissions::from_mode(0o755)).expect("executable");
        git(
            &partial,
            &[
                "config",
                "remote.origin.uploadpack",
                &transport.to_string_lossy(),
            ],
        );
        (dir, partial, marker)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_contract_fetches_nothing_from_a_promisor_remote() {
        let origin = clone_with(&contract_files());
        let (_dir, partial, marker) = promisor_clone_of(origin.path());

        let error = check_contract_with(&check_args(&partial, "main"), &missing_docker())
            .await
            .expect_err("the contract's contents are missing and are not fetched");

        assert!(!marker.exists(), "git fetched a missing object: {error}");
    }

    /// The clone's HEAD and the snapshot a protected `.acan` is written to. Returns the result of
    /// the write, the snapshot directory, and the scratch directory that holds it.
    async fn write_protected_at_head(
        repo: &Path,
    ) -> (Result<(), ShadowError>, PathBuf, tempfile::TempDir) {
        let head = git_output(repo, &["rev-parse", "HEAD"]);
        let commit = commit_sha(&head).expect("HEAD is a commit");
        let protected: BTreeSet<RepoPath> = [RepoPath::from_str(".acan").expect("a path")].into();
        let scratch = tempfile::TempDir::new().expect("scratch directory");
        let snapshot = scratch.path().join("snapshot");
        let written = write_protected(repo, &commit, &protected, &snapshot).await;
        (written, snapshot, scratch)
    }

    /// Trimmed standard output of git in `dir`.
    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[tokio::test]
    async fn names_that_escape_or_are_not_portable_are_refused_before_any_file_is_written() {
        // Each name is one that Windows would read as a path outside `.acan`, as a device, or as a
        // name it cannot store. Each sorts after the valid contract files, so a materializer that
        // wrote as it listed would have written the contract before it refused the name.
        let names = [
            ".acan/zz..\\..\\escape.txt",
            ".acan/zzC:escape.txt",
            ".acan/zz/NUL.json",
            ".acan/zzapp.json.",
            ".acan/zzapp.json ",
        ];
        for name in names {
            let mut files = contract_files();
            files.push((name, "not a contract"));
            let repo = commit_files(&files);

            let (written, snapshot, scratch) = write_protected_at_head(repo.path()).await;

            let error = written.expect_err(name);
            assert!(
                matches!(error, ShadowError::Contract(_)),
                "{name}: {error:?}"
            );
            assert!(
                !snapshot.join(".acan").exists(),
                "{name}: nothing under .acan was written"
            );
            assert!(
                !scratch.path().join("escape.txt").exists(),
                "{name}: nothing was written outside the snapshot"
            );
        }
    }

    /// Runs git in `dir` with `input` on standard input. Returns its standard output, trimmed.
    fn git_in(dir: &Path, args: &[&str], input: &[u8]) -> String {
        use std::io::Write;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git runs");
        child
            .stdin
            .take()
            .expect("git's input")
            .write_all(input)
            .expect("the input is written");
        let output = child.wait_with_output().expect("git finishes");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A repository whose `main` is one commit of `files`, built with git's plumbing and not the
    /// file system. A name that Windows would store differently, such as one with a backslash or a
    /// trailing dot, reaches the commit exactly as written, so the test is the same on every
    /// platform. The trees are written as raw bytes with `--literally`: git's index and tree
    /// checks refuse some of these names on Windows before any commit is made.
    fn commit_files(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temporary repository");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "Tester"]);
        git(dir.path(), &["config", "user.email", "tester@localhost"]);
        let mut root = FixtureTree::default();
        for (path, text) in files {
            let blob = git_in(
                dir.path(),
                &["hash-object", "-w", "--stdin"],
                text.as_bytes(),
            );
            root.insert(path, &blob);
        }
        let tree = root.write(dir.path());
        let commit = git_in(dir.path(), &["commit-tree", &tree, "-m", "base"], &[]);
        git(dir.path(), &["update-ref", "refs/heads/main", &commit]);
        dir
    }

    /// The directories and files of one fixture tree, by name.
    #[derive(Default)]
    struct FixtureTree {
        files: BTreeMap<String, String>,
        dirs: BTreeMap<String, FixtureTree>,
    }

    impl FixtureTree {
        /// Adds the blob `blob` at `path`, making each directory the path names on the way.
        fn insert(&mut self, path: &str, blob: &str) {
            match path.split_once('/') {
                Some((dir, rest)) => self
                    .dirs
                    .entry(dir.to_owned())
                    .or_default()
                    .insert(rest, blob),
                None => {
                    self.files.insert(path.to_owned(), blob.to_owned());
                }
            }
        }

        /// Writes this tree and every tree under it, and returns the id of this one. Entries are
        /// sorted as git sorts them, a directory by its name followed by a slash.
        fn write(&self, repo: &Path) -> String {
            let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            for (name, blob) in &self.files {
                let entry = tree_entry(b"100644", name, &hex_bytes(blob));
                entries.push((name.as_bytes().to_vec(), entry));
            }
            for (name, tree) in &self.dirs {
                let id = tree.write(repo);
                let entry = tree_entry(b"40000", name, &hex_bytes(&id));
                entries.push((format!("{name}/").into_bytes(), entry));
            }
            entries.sort();
            let raw: Vec<u8> = entries.into_iter().flat_map(|(_, entry)| entry).collect();
            git_in(
                repo,
                &["hash-object", "-t", "tree", "-w", "--literally", "--stdin"],
                &raw,
            )
        }
    }

    /// One tree entry as git stores it: the mode, the name, a zero byte and the id in binary.
    fn tree_entry(mode: &[u8], name: &str, id: &[u8]) -> Vec<u8> {
        let mut entry = mode.to_vec();
        entry.push(b' ');
        entry.extend_from_slice(name.as_bytes());
        entry.push(0);
        entry.extend_from_slice(id);
        entry
    }

    /// The bytes of a hexadecimal object id.
    fn hex_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("a hexadecimal id"))
            .collect()
    }

    #[tokio::test]
    async fn names_windows_would_read_as_another_name_are_refused_before_any_file_is_written() {
        // `LONGFI~1.TXT` is the short alias Windows gives a long name, and a name outside ASCII
        // folds to another name there, so neither is a contract name.
        let names = [
            ".acan/zzlongfi~1.txt",
            ".acan/zz~1.json",
            ".acan/\u{3c2}zz.json",
            ".acan/\u{3a3}zz.json",
        ];
        for name in names {
            let mut files = contract_files();
            files.push((name, "not a contract"));
            let repo = commit_files(&files);

            let (written, snapshot, _scratch) = write_protected_at_head(repo.path()).await;

            let error = written.expect_err(name);
            assert!(
                matches!(error, ShadowError::Contract(_)),
                "{name}: {error:?}"
            );
            assert!(
                !snapshot.join(".acan").exists(),
                "{name}: nothing under .acan was written"
            );
        }
    }

    #[tokio::test]
    async fn a_tilde_that_no_digit_follows_is_an_ordinary_contract_name() {
        let mut files = contract_files();
        files.push((".acan/zz~a.json", "a name with a tilde"));
        let repo = commit_files(&files);

        let (written, snapshot, _scratch) = write_protected_at_head(repo.path()).await;

        written.expect("the name is written");
        assert!(snapshot.join(".acan/zz~a.json").is_file());
    }

    #[test]
    fn a_name_that_would_overwrite_a_written_file_is_refused() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let target = dir.path().join("app.json");
        write_new(&target, b"first").expect("a new file is written");

        let error = write_new(&target, b"second").expect_err("an existing file is not overwritten");

        assert!(matches!(error, ShadowError::Contract(_)), "{error:?}");
        assert_eq!(fs::read(&target).expect("the file"), b"first");
    }

    #[tokio::test]
    async fn names_that_differ_only_by_case_are_refused_before_any_file_is_written() {
        let mut files = contract_files();
        files.push((".acan/APP.json", "a second app"));
        let repo = commit_files(&files);

        let (written, snapshot, _scratch) = write_protected_at_head(repo.path()).await;

        let error = written.expect_err("two names for one file are refused");
        assert!(error.to_string().contains("one file"), "{error}");
        assert!(!snapshot.join(".acan").exists(), "nothing was written");
    }

    #[tokio::test]
    async fn a_file_that_differs_only_by_case_from_a_directory_is_refused() {
        // `.acan/Probes` is a file, and `.acan/probes/` is a directory on a case-insensitive
        // file system, so the two cannot both be written.
        let mut files = contract_files();
        files.push((".acan/Probes", "a file, not a directory"));
        let repo = commit_files(&files);

        let (written, snapshot, _scratch) = write_protected_at_head(repo.path()).await;

        let error = written.expect_err("a file and a directory of one name are refused");
        assert!(error.to_string().contains("lies under it"), "{error}");
        assert!(!snapshot.join(".acan").exists(), "nothing was written");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_name_that_is_not_utf8_is_refused_before_any_file_is_written() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let repo = clone_with(&contract_files());
        let name = repo.path().join(OsStr::from_bytes(b".acan/zz\xff.json"));
        fs::write(&name, "not UTF-8").expect("a file with a byte that is not UTF-8");
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "odd name"]);

        let (written, snapshot, _scratch) = write_protected_at_head(repo.path()).await;

        let error = written.expect_err("a name that is not UTF-8 is refused");
        assert!(error.to_string().contains("not UTF-8"), "{error}");
        assert!(!snapshot.join(".acan").exists(), "nothing was written");
    }

    #[tokio::test]
    async fn check_contract_passes_a_valid_contract_and_needs_no_head() {
        let repo = clone_with(&contract_files());
        let check = check_contract_with(&check_args(repo.path(), "main"), &missing_docker())
            .await
            .expect("the contract passes");
        assert_eq!(check.runtime, "bun-1.4.4");
        assert!(!check.install, "the bun fixture has no install phase");
        assert!(check.probes > 0, "the corpus has probes");
        assert!(
            check.docker.is_err(),
            "docker is information, not a failure"
        );
        let text = check_text(&check);
        assert!(text.contains("information only"), "{text}");
    }

    #[tokio::test]
    async fn check_contract_refuses_a_contract_that_fails_its_checks() {
        // An install phase is only for acan-app/2, so this version 1 contract cannot pass.
        let app = APP_CONTRACT.replacen(
            '{',
            "{\n  \"install\": [\"npm\", \"ci\", \"--ignore-scripts\"],",
            1,
        );
        let repo = clone_with(&[
            (".acan/app.json", app.as_str()),
            (".acan/probes/core.json", PROBE_CORPUS),
        ]);
        let error = check_contract_with(&check_args(repo.path(), "main"), &missing_docker())
            .await
            .expect_err("a contract that fails its checks is refused");
        assert!(
            matches!(error, ShadowError::Contract(_)),
            "failing check: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn check_contract_asks_docker_only_for_its_version() {
        use std::os::unix::fs::PermissionsExt;
        let repo = clone_with(&contract_files());
        let tools = tempfile::tempdir().expect("a directory for a fake docker");
        let calls = tools.path().join("calls.txt");
        let docker = tools.path().join("docker");
        fs::write(
            &docker,
            format!("#!/bin/sh\necho \"$@\" >> {}\nexit 1\n", calls.display()),
        )
        .expect("the fake docker");
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).expect("executable");
        let check = check_contract_with(&check_args(repo.path(), "main"), &docker)
            .await
            .expect("the contract passes without docker");
        assert!(check.docker.is_err(), "the fake docker failed");
        let text = fs::read_to_string(&calls).expect("docker was asked");
        assert_eq!(
            text.trim(),
            "version --format {{.Server.Version}}",
            "only the version was asked: {text}"
        );
    }

    #[tokio::test]
    async fn a_clone_without_a_protected_contract_is_refused_before_docker() {
        let repo = clone_with(&[("README.md", "no contract here\n")]);
        let error = run_with(&args(repo.path(), "main", "main"), &missing_docker(), None)
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
        let error = run_with(&args(repo.path(), "main", "main"), &missing_docker(), None)
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
        let error = run_with(&args(repo.path(), "main", "main"), &missing_docker(), None)
            .await
            .expect_err("a run without docker is refused");
        assert!(
            matches!(error, ShadowError::Docker(_)),
            "no docker: {error}"
        );
    }

    #[tokio::test]
    async fn a_git_that_cannot_answer_is_not_read_as_a_missing_commit() {
        let repo = clone_with(&contract_files());
        let root = repo.path();
        let unrunnable =
            resolve_commit_with(Path::new("acan-no-such-git"), root, "main", REF_TIMEOUT).await;
        assert!(
            matches!(&unrunnable, Err(detail) if detail.contains("cannot run")),
            "a git that cannot start: {unrunnable:?}"
        );
        let stalled = resolve_commit_with(Path::new("git"), root, "main", Duration::ZERO).await;
        assert!(
            matches!(&stalled, Err(detail) if detail.contains("did not answer")),
            "a git that does not answer in time: {stalled:?}"
        );
        let missing = resolve_commit_with(Path::new("git"), root, "no-such-ref", REF_TIMEOUT).await;
        assert!(
            matches!(missing, Ok(None)),
            "a ref that names no commit is Ok(None)"
        );
        let named = resolve_commit_with(Path::new("git"), root, "main", REF_TIMEOUT).await;
        assert!(
            matches!(named, Ok(Some(_))),
            "a ref that names a commit resolves"
        );
    }

    fn head_of(dir: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[tokio::test]
    async fn a_base_that_moves_after_it_is_read_is_refused() {
        let repo_dir = clone_with(&contract_files());
        let read = commit_sha(&head_of(repo_dir.path())).expect("the head is a commit");
        git(
            repo_dir.path(),
            &["commit", "-q", "--allow-empty", "-m", "moved"],
        );
        let repo = Repo::open_with_protected(repo_dir.path(), "main")
            .await
            .expect("the clone opens");
        let protected = observe::protected_paths(&observe::runner_policy());
        let PrepareOutcome::Prepared(base) = repo
            .prepare(&read, &protected)
            .await
            .expect("the base prepares")
        else {
            panic!("the base merges cleanly onto itself");
        };
        let error = ensure_unmoved(base.protected_head(), &read, "main")
            .expect_err("a base that moved is refused");
        assert!(
            matches!(&error, ShadowError::Ref { flag: "--base", .. }),
            "moved base: {error}"
        );
        ensure_unmoved(&read, &read, "main").expect("an unmoved base is kept");
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
    fn installs_are_named_in_the_envelope_only_when_a_run_mounted_one() {
        let repo = Path::new("clone");
        let without = json_text(&report_for(repo, fixture_receipt())).expect("the envelope");
        let value: Value = serde_json::from_str(&without).expect("the envelope is JSON");
        assert!(value.get("installs").is_none(), "no install, no field");

        let mut report = report_for(repo, fixture_receipt());
        report.installs = vec![InstallRecord {
            role: Role::Candidate,
            commit: "3".repeat(40),
            key: "sha256:0".to_owned(),
            lockfile_sha256: "sha256:1".to_owned(),
            cache: deps::Cache::Hit,
            image_digest: "sha256:2".to_owned(),
            argv: vec!["npm".to_owned(), "ci".to_owned()],
            install_scripts: "ignored",
            network: "egress",
            duration_ms: 10,
        }];
        let text = json_text(&report).expect("the envelope serializes");
        let value: Value = serde_json::from_str(&text).expect("the envelope is JSON");
        assert_eq!(value["installs"][0]["role"], "candidate");
        assert_eq!(value["installs"][0]["cache"], "hit");
        assert_eq!(value["installs"][0]["install_scripts"], "ignored");
        assert_eq!(value["installs"][0]["network"], "egress");
        assert_eq!(value["installs"][0]["argv"][1], "ci");
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
    fn a_quote_after_a_bearer_token_is_kept() {
        let text = "header: \"Bearer ab.cd-ef~1+2/3=\", next: 'Bearer xyz'`ok`";
        assert_eq!(
            redact_bearer(text),
            "header: \"Bearer <redacted>\", next: 'Bearer <redacted>'`ok`"
        );
    }

    #[test]
    fn a_bearer_token_with_an_unusual_character_is_redacted_whole() {
        assert_eq!(
            redact_bearer("auth Bearer abc!def$ghi:jkl tail"),
            "auth Bearer <redacted> tail",
            "the rest of a token is not left in view after an unusual character"
        );
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
    fn a_line_with_two_sensitive_keys_loses_every_value() {
        let text = "password: hunter2, token: abc123";
        let redacted = redact_markdown(text, &[]);
        assert!(
            !redacted.contains("hunter2"),
            "first key's value: {redacted}"
        );
        assert!(
            !redacted.contains("abc123"),
            "second key's value: {redacted}"
        );
    }

    #[test]
    fn markdown_keeps_its_line_endings() {
        let text = "| path | /api |\r\npassword: hunter2\r\n| status | 200 |";
        assert_eq!(
            redact_markdown(text, &[]),
            "| path | /api |\r\npassword:<redacted>\r\n| status | 200 |",
            "CRLF stays CRLF, and a last line without one gets none"
        );
        assert_eq!(redact_markdown("a\nb\n", &[]), "a\nb\n", "LF stays LF");
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

    #[test]
    fn a_pair_that_cannot_be_completed_leaves_no_half_pair() {
        let report = report_for(Path::new("clone"), fixture_receipt());
        let dir = tempfile::tempdir().expect("temporary directory");
        let out = dir.path().join("out");
        // A directory where the Markdown file goes makes its move fail after the JSON moved.
        fs::create_dir_all(out.join("shadow-receipt.md")).expect("a directory in the way");
        let error = write_out(&report, &out).expect_err("the markdown cannot be placed");
        assert!(matches!(error, ShadowError::Output(_)), "output: {error}");
        assert!(
            !out.join("shadow-receipt.json").exists(),
            "the json that moved is taken back"
        );
        assert_eq!(
            fs::read_dir(&out).expect("the directory").count(),
            1,
            "no staged file is left behind"
        );
    }

    #[test]
    fn a_failed_pair_write_restores_the_earlier_json() {
        let report = report_for(Path::new("clone"), fixture_receipt());
        let dir = tempfile::tempdir().expect("temporary directory");
        let out = dir.path();
        fs::write(out.join("shadow-receipt.json"), "earlier json").expect("an earlier json");
        fs::create_dir(out.join("shadow-receipt.md")).expect("a directory in the way");
        write_out(&report, out).expect_err("the markdown cannot be placed");
        assert_eq!(
            fs::read_to_string(out.join("shadow-receipt.json")).expect("the json still exists"),
            "earlier json",
            "the earlier json is put back"
        );
        assert_eq!(
            fs::read_dir(out).expect("the directory").count(),
            2,
            "only the earlier json and the directory remain"
        );
    }
}
