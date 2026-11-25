//! The V1 landing gate: verify the approved checks, test an Acan-made task commit, and
//! persist permission to land it before moving the protected branch.

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::future::Future;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use acan_core::{CommitId, TaskId};
use acan_git::{Author, LandOutcome, LandV1Error, Repo, StagedCommit, TaskWorktree};
use acan_intent::{PathScope, RepoRules, canonical_json};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::EventStore;

/// The default budget of one check.
pub const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(120);
/// The default budget of each of a check's stdout and stderr streams.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// One approved executable and its arguments. No shell is added by the gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckCommand {
    pub id: String,
    pub program: String,
    pub args: Vec<String>,
}

/// The time and per-stream byte budgets of every check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckPolicy {
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl Default for CheckPolicy {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_CHECK_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// Checks and their budgets as frozen at order approval. Store the approved hash separately
/// from this value: both are required to authorize a land.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenCheckSet {
    pub checks: Vec<CheckCommand>,
    pub policy: CheckPolicy,
    pub sha256: String,
}

impl FrozenCheckSet {
    /// Freeze the ordered check list and policy into one canonical SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Rejects blank or repeated IDs, blank programs, and zero budgets.
    pub fn freeze(checks: Vec<CheckCommand>, policy: CheckPolicy) -> Result<Self, LandGateError> {
        validate_checks(&checks, policy)?;
        let sha256 = check_hash(&checks, policy)?;
        Ok(Self {
            checks,
            policy,
            sha256,
        })
    }

    /// Recompute the digest instead of trusting the stored one.
    ///
    /// # Errors
    ///
    /// Returns [`LandGateError::CheckFreezeMismatch`] if the frozen set was changed.
    pub fn verify_integrity(&self) -> Result<(), LandGateError> {
        if validate_checks(&self.checks, self.policy).is_err()
            || check_hash(&self.checks, self.policy)? != self.sha256
        {
            return Err(LandGateError::CheckFreezeMismatch);
        }
        Ok(())
    }

    /// Verify integrity and require the digest recorded at approval.
    ///
    /// # Errors
    ///
    /// Returns [`LandGateError::CheckFreezeMismatch`] when either digest does not agree.
    pub fn matches_hash(&self, expected_sha256: &str) -> Result<(), LandGateError> {
        self.verify_integrity()?;
        if self.sha256 != expected_sha256 {
            return Err(LandGateError::CheckFreezeMismatch);
        }
        Ok(())
    }
}

fn validate_checks(checks: &[CheckCommand], policy: CheckPolicy) -> Result<(), LandGateError> {
    if policy.timeout.is_zero() || policy.max_output_bytes == 0 {
        return Err(LandGateError::InvalidChecks(
            "check budgets must be positive",
        ));
    }
    let mut ids = BTreeSet::new();
    for check in checks {
        if check.id.trim().is_empty() || check.program.trim().is_empty() {
            return Err(LandGateError::InvalidChecks(
                "check IDs and programs must not be blank",
            ));
        }
        if !ids.insert(&check.id) {
            return Err(LandGateError::InvalidChecks("check IDs must be unique"));
        }
    }
    Ok(())
}

fn check_hash(checks: &[CheckCommand], policy: CheckPolicy) -> Result<String, LandGateError> {
    #[derive(Serialize)]
    struct Approval<'a> {
        version: u8,
        checks: &'a [CheckCommand],
        timeout_nanos: String,
        max_output_bytes: String,
    }
    // Decimal strings keep the full integer precision even with canonical JSON's number rules.
    let json = canonical_json(&Approval {
        version: 1,
        checks,
        timeout_nanos: policy.timeout.as_nanos().to_string(),
        max_output_bytes: policy.max_output_bytes.to_string(),
    })?;
    Ok(format!("{:x}", Sha256::digest(json)))
}

const REDACTED: &str = "[REDACTED]";

static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"-----BEGIN (?:[^\r\n]+? )?PRIVATE KEY-----(?s:.*?)",
        r"(?:-----END (?:[^\r\n]+? )?PRIVATE KEY-----|\z)",
        r"|sk-[A-Za-z0-9_-]{16,}",
        r"|gh[pousr]_[A-Za-z0-9_]{16,}",
        r"|acan_[a-z_]*[0-9a-f]{16,}",
        r"|(?i:\bBearer)[ \t]+[A-Za-z0-9._~+/-]+=*",
        r"|AKIA[0-9A-Z]{16}",
    ))
    .expect("valid secret patterns")
});

// Recognizable token starts that may have been cut off before their full shape was retained.
static PARTIAL_TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?:sk-[A-Za-z0-9_-]*|gh[pousr]_[A-Za-z0-9_]*|acan_[a-z_0-9]*",
        r"|AKIA[0-9A-Z]*|(?i:\bBearer)[ \t]+|-----BEGIN [A-Z0-9 -]*)\z",
    ))
    .expect("valid partial secret patterns")
});

/// Masks known token shapes and literal secrets. The literals are never logged or serialized.
#[derive(Default)]
pub struct SecretMasker {
    literals: Vec<String>,
}

impl SecretMasker {
    /// Also mask the supplied literal values with at least eight characters.
    #[must_use]
    pub fn with_literals(secrets: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut literals: Vec<_> = secrets
            .into_iter()
            .map(|secret| secret.as_ref().to_owned())
            .filter(|secret| secret.chars().take(8).count() == 8)
            .collect();
        literals.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        literals.dedup();
        Self { literals }
    }

    /// Replace secrets with `[REDACTED]`.
    #[must_use]
    pub fn mask(&self, text: &str) -> String {
        self.mask_text(text, false)
    }

    fn mask_text(&self, text: &str, incomplete: bool) -> String {
        let mut spans: Vec<_> = TOKENS.find_iter(text).map(|m| m.range()).collect();
        for literal in &self.literals {
            spans.extend(
                text.match_indices(literal)
                    .map(|(at, value)| at..at + value.len()),
            );
            if incomplete {
                // Even the partial prefix of a literal at the capture boundary must not be stored.
                let start = text.ceil_char_boundary(text.len().saturating_sub(literal.len()));
                if let Some((at, _)) = text[start..]
                    .char_indices()
                    .find(|(at, _)| literal.starts_with(&text[start + at..]))
                {
                    spans.push(start + at..text.len());
                }
            }
        }
        if incomplete {
            spans.extend(PARTIAL_TOKENS.find_iter(text).map(|m| m.range()));
        }
        // Merge overlaps on the original text: replacing a short literal or a token first could
        // otherwise conceal a longer matching secret and leave its remaining characters visible.
        spans.sort_unstable_by_key(|span| (span.start, Reverse(span.end)));
        let mut merged: Vec<Range<usize>> = Vec::new();
        for span in spans {
            if let Some(last) = merged.last_mut()
                && span.start < last.end
            {
                last.end = last.end.max(span.end);
            } else {
                merged.push(span);
            }
        }
        let mut masked = String::with_capacity(text.len());
        let mut at = 0;
        for span in merged {
            masked.push_str(&text[at..span.start]);
            masked.push_str(REDACTED);
            at = span.end;
        }
        masked.push_str(&text[at..]);
        masked
    }
}

/// The result of one frozen check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRunResult {
    pub id: String,
    pub status: CheckStatus,
    pub stdout: String,
    pub stderr: String,
    /// At least one output stream exceeded its byte budget or was cut off on timeout.
    pub truncated: bool,
}

/// Whether a check completed and passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed { code: Option<i32> },
    TimedOut,
    FailedToRun,
}

struct Capture {
    bytes: Vec<u8>,
    cap: usize,
    truncated: bool,
    finished: bool,
}

impl Capture {
    fn new(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            cap,
            truncated: false,
            finished: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let keep = bytes.len().min(self.cap - self.bytes.len());
        self.bytes.extend_from_slice(&bytes[..keep]);
        self.truncated |= keep < bytes.len();
    }

    async fn drain(&mut self, mut pipe: impl AsyncRead + Unpin) -> io::Result<()> {
        let mut scratch = [0; 8192];
        loop {
            let n = pipe.read(&mut scratch).await?;
            if n == 0 {
                self.finished = true;
                return Ok(());
            }
            self.push(&scratch[..n]);
        }
    }

    fn finish(mut self, masker: &SecretMasker) -> (String, bool) {
        let incomplete = self.truncated || !self.finished;
        if incomplete
            && let Some(last) = self.bytes.utf8_chunks().last()
            && std::str::from_utf8(last.invalid()).is_err_and(|error| error.error_len().is_none())
        {
            self.bytes.truncate(self.bytes.len() - last.invalid().len());
        }
        let mut output = masker.mask_text(&String::from_utf8_lossy(&self.bytes), incomplete);
        let truncated = incomplete || output.len() > self.cap;
        output.truncate(output.floor_char_boundary(self.cap));
        (output, truncated)
    }
}

async fn run_check(
    root: &Path,
    check: &CheckCommand,
    policy: CheckPolicy,
    masker: &SecretMasker,
) -> CheckRunResult {
    let program = Path::new(&check.program);
    // `current_dir` with a relative executable path has platform-dependent semantics.
    // Resolve explicit paths ourselves; bare program names still use the normal lookup.
    let mut command = if program.is_relative() && program.components().count() > 1 {
        Command::new(root.join(program))
    } else {
        Command::new(program)
    };
    acan_git::git_env::prepare(command.as_std_mut());
    command
        .args(&check.args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut stdout = Capture::new(policy.max_output_bytes);
    let mut stderr = Capture::new(policy.max_output_bytes);
    let status = match command.spawn() {
        Ok(mut child) => {
            let out = child.stdout.take().expect("stdout is piped");
            let err = child.stderr.take().expect("stderr is piped");
            // Include the pipe drains in the deadline: a descendant may hold a pipe open
            // even after the command exits. Dropping these futures also closes the pipes.
            let result = tokio::time::timeout(policy.timeout, async {
                tokio::try_join!(child.wait(), stdout.drain(out), stderr.drain(err))
            })
            .await;
            match result {
                Ok(Ok((exit, (), ()))) if exit.success() => CheckStatus::Passed,
                Ok(Ok((exit, (), ()))) => CheckStatus::Failed { code: exit.code() },
                Ok(Err(error)) => {
                    let _ = child.kill().await;
                    // Do not concatenate a diagnostic with incomplete data from the child.
                    stderr = Capture::new(policy.max_output_bytes);
                    stderr.push(error.to_string().as_bytes());
                    stderr.finished = true;
                    CheckStatus::FailedToRun
                }
                Err(_) => {
                    let _ = child.kill().await;
                    CheckStatus::TimedOut
                }
            }
        }
        Err(error) => {
            stdout.finished = true;
            stderr.push(error.to_string().as_bytes());
            stderr.finished = true;
            CheckStatus::FailedToRun
        }
    };
    let (stdout, stdout_cut) = stdout.finish(masker);
    let (stderr, stderr_cut) = stderr.finish(masker);
    CheckRunResult {
        id: check.id.clone(),
        status,
        stdout,
        stderr,
        truncated: stdout_cut || stderr_cut,
    }
}

/// The durable authorization for a proposed land, not a claim that installation succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingAuditRecord {
    pub task: TaskId,
    pub expected_protected_head: CommitId,
    pub staged: StagedCommit,
    pub check_hash: String,
    pub checks: Vec<CheckRunResult>,
}

/// The result of the V1 gate, including the commit installed on success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct V1LandReceipt {
    pub audit: LandingAuditRecord,
    pub landed_commit: Option<CommitId>,
}

/// Stores the authorization before installation is attempted.
pub trait LandingEventSink: Send + Sync {
    fn record_landing<'a>(
        &'a self,
        record: &'a LandingAuditRecord,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

impl LandingEventSink for EventStore {
    fn record_landing<'a>(
        &'a self,
        record: &'a LandingAuditRecord,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(self.append_v1_landing(record))
    }
}

/// All inputs to one landing attempt. `approved_check_hash` comes from the approval, not from
/// the mutable worktree or a fresh freeze of its current config.
pub struct V1LandRequest<'a> {
    pub repo: &'a Repo,
    pub task: TaskId,
    pub worktree: &'a TaskWorktree,
    pub expected_base: &'a CommitId,
    pub expected_protected_head: &'a CommitId,
    pub scope: &'a PathScope,
    pub rules: &'a RepoRules,
    pub author: &'a Author,
    pub message: &'a str,
    pub frozen_checks: &'a FrozenCheckSet,
    pub approved_check_hash: &'a str,
    pub masker: &'a SecretMasker,
    pub event_sink: &'a dyn LandingEventSink,
}

/// Why a V1 landing halted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LandGateError {
    #[error("invalid frozen checks: {0}")]
    InvalidChecks(&'static str),
    #[error("frozen checks do not match the approved hash")]
    CheckFreezeMismatch,
    #[error("the worktree does not belong to the landing task")]
    WorktreeMismatch,
    #[error("one or more checks did not pass")]
    ChecksFailed { receipt: Box<V1LandReceipt> },
    #[error("landing halted because the event could not be written: {0}")]
    EventWriteHalted(String),
    #[error("the protected branch did not advance: {0:?}")]
    InstallRefused(LandOutcome),
    #[error(transparent)]
    Stage(#[from] LandV1Error),
    #[error(transparent)]
    Git(#[from] acan_git::Error),
    #[error(transparent)]
    Canonical(#[from] acan_intent::CanonicalError),
}

/// Stage approved task edits, run the frozen checks, persist authorization, then attempt
/// to install the tested task commit against the expected protected head.
///
/// # Errors
///
/// Refuses invalid approval, unsafe changes, failed checks, event-write failures, and git
/// installation refusals. An event-write failure never attempts to install.
pub async fn execute_v1_landing(req: V1LandRequest<'_>) -> Result<V1LandReceipt, LandGateError> {
    req.frozen_checks.matches_hash(req.approved_check_hash)?;
    if *req.worktree.task() != req.task
        || req.worktree.branch() == req.repo.protected_branch()
        || req.worktree.path()
            != req
                .repo
                .root()
                .join(".acan")
                .join("worktrees")
                .join(req.task.to_string())
    {
        return Err(LandGateError::WorktreeMismatch);
    }
    let staged = req
        .worktree
        .stage_and_commit_v1(
            req.expected_base,
            req.scope,
            req.rules,
            req.author,
            req.message,
        )
        .await?;
    let mut checks = Vec::with_capacity(req.frozen_checks.checks.len());
    for check in &req.frozen_checks.checks {
        checks.push(
            run_check(
                req.worktree.path(),
                check,
                req.frozen_checks.policy,
                req.masker,
            )
            .await,
        );
    }
    let audit = LandingAuditRecord {
        task: req.task,
        expected_protected_head: req.expected_protected_head.clone(),
        staged,
        check_hash: req.frozen_checks.sha256.clone(),
        checks,
    };
    let mut receipt = V1LandReceipt {
        audit,
        landed_commit: None,
    };
    if !receipt
        .audit
        .checks
        .iter()
        .all(|check| check.status == CheckStatus::Passed)
    {
        return Err(LandGateError::ChecksFailed {
            receipt: Box::new(receipt),
        });
    }

    // This is deliberately before the first protected-branch write. A store error must
    // never be handled by moving the branch and trying to log the decision afterwards.
    req.event_sink
        .record_landing(&receipt.audit)
        .await
        .map_err(|error| LandGateError::EventWriteHalted(req.masker.mask(&error.to_string())))?;
    match req
        .repo
        .land(
            req.task,
            &receipt.audit.staged.commit,
            req.expected_protected_head,
        )
        .await?
    {
        LandOutcome::Landed { commit } => receipt.landed_commit = Some(commit),
        outcome => return Err(LandGateError::InstallRefused(outcome)),
    }
    Ok(receipt)
}
