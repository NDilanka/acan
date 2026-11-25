//! The V1 gate only moves the protected branch after an audit event is durable.

use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use acan_core::{CommitId, Generation, TaskId};
use acan_git::{Author, Repo, TaskWorktree};
use acan_intent::{PathScope, RepoRules, ScopeEntry};
use acan_server::EventStore;
use acan_server::land_v1::{
    CheckCommand, CheckPolicy, CheckStatus, FrozenCheckSet, LandGateError, LandingAuditRecord,
    LandingEventSink, SecretMasker, V1LandRequest, execute_v1_landing,
};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().into()
}

struct Fixture {
    dir: tempfile::TempDir,
    repo: Repo,
    task: TaskId,
    worktree: TaskWorktree,
    base: CommitId,
    scope: PathScope,
    rules: RepoRules,
    author: Author,
}

async fn fixture(mode: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Tester"]);
    git(root, &["config", "user.email", "tester@example.com"]);
    fs::write(root.join("app.txt"), "base\n").expect("write base");
    git(root, &["add", "app.txt"]);
    git(root, &["commit", "-qm", "base"]);
    let repo = Repo::open(root).await.expect("repo");
    let base = repo.protected_head().await.expect("base head");
    let task = "t-1".parse().expect("task id");
    let worktree = repo
        .worktrees()
        .create(task, Generation::new(1))
        .await
        .expect("task worktree");
    fs::write(worktree.path().join("app.txt"), mode).expect("write task edit");
    let scope = PathScope::new([ScopeEntry::from_input("app.txt").expect("file")]).expect("scope");
    Fixture {
        dir,
        repo,
        task,
        worktree,
        base,
        scope,
        rules: RepoRules::default(),
        author: Author::new("Acan", "acan@example.com"),
    }
}

impl Fixture {
    fn request<'a>(
        &'a self,
        checks: &'a FrozenCheckSet,
        masker: &'a SecretMasker,
        sink: &'a dyn LandingEventSink,
    ) -> V1LandRequest<'a> {
        V1LandRequest {
            repo: &self.repo,
            task: self.task,
            worktree: &self.worktree,
            expected_base: &self.base,
            expected_protected_head: &self.base,
            scope: &self.scope,
            rules: &self.rules,
            author: &self.author,
            message: "approved edit",
            frozen_checks: checks,
            approved_check_hash: &checks.sha256,
            masker,
            event_sink: sink,
        }
    }

    fn sink(&self) -> RecordingSink<'_> {
        RecordingSink {
            fixture: self,
            fail: false,
            records: Mutex::new(Vec::new()),
        }
    }

    async fn assert_protected_head_unchanged(&self) {
        assert_eq!(
            self.repo.protected_head().await.expect("protected head"),
            self.base
        );
        assert_eq!(
            fs::read_to_string(self.dir.path().join("app.txt"))
                .expect("original file")
                .trim(),
            "base"
        );
    }
}

struct RecordingSink<'a> {
    fixture: &'a Fixture,
    fail: bool,
    records: Mutex<Vec<LandingAuditRecord>>,
}

impl LandingEventSink for RecordingSink<'_> {
    fn record_landing<'a>(
        &'a self,
        record: &'a LandingAuditRecord,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.fixture.assert_protected_head_unchanged().await;
            assert_eq!(record.expected_protected_head, self.fixture.base);
            if self.fail {
                anyhow::bail!("event storage unavailable");
            }
            self.records
                .lock()
                .expect("lock records")
                .push(record.clone());
            Ok(())
        })
    }
}

fn command(id: &str) -> CheckCommand {
    CheckCommand {
        id: id.to_owned(),
        program: std::env::current_exe()
            .expect("current test binary")
            .to_str()
            .expect("UTF-8 executable path")
            .to_owned(),
        args: ["--ignored", "--exact", "check_fixture", "--nocapture"]
            .map(str::to_owned)
            .to_vec(),
    }
}

fn freeze(policy: CheckPolicy) -> FrozenCheckSet {
    FrozenCheckSet::freeze(vec![command("test")], policy).expect("freeze checks")
}

const LITERAL: &str = "fake-long-secret-value";
const SK_TOKEN: &str = "sk-fake_token_0123456789";
const BEARER: &str = "Bearer demo.header.signature";

// Run this binary as a check so the command/args and timeouts work on both Unix and Windows
// without relying on shell quoting, available interpreters or process descendants.
#[test]
#[ignore = "executed as a subprocess by the landing checks"]
fn check_fixture() {
    match fs::read_to_string("app.txt")
        .expect("check ran in the task worktree")
        .trim()
    {
        "pass" => println!("the check passed"),
        "wait" => std::thread::sleep(Duration::from_secs(10)),
        "output" => {
            print!("{}", "o".repeat(200 * 1024));
            eprint!("{}", "e".repeat(200 * 1024));
        }
        "secrets" => {
            println!("{LITERAL} {SK_TOKEN} {BEARER}");
            eprintln!("{LITERAL} {SK_TOKEN} {BEARER}");
        }
        "cut secret" => {
            // The cap lands inside the literal on stderr, with no other output on that stream.
            eprint!("{}{LITERAL}{}", "x".repeat(1012), "z".repeat(4096));
        }
        "failure" => panic!("failed with {LITERAL}"),
        other => panic!("unexpected check mode {other:?}"),
    }
}

#[tokio::test]
async fn persisting_the_audit_precedes_advancing_the_protected_branch() {
    let f = fixture("pass").await;
    let checks = freeze(CheckPolicy::default());
    let sink = f.sink();
    let receipt = execute_v1_landing(f.request(&checks, &SecretMasker::default(), &sink))
        .await
        .expect("land");

    assert_eq!(
        receipt.landed_commit.as_ref(),
        Some(&receipt.audit.staged.commit)
    );
    assert_eq!(
        f.repo.protected_head().await.expect("protected head"),
        receipt.audit.staged.commit
    );
    assert_eq!(
        f.worktree.head().await.expect("task head"),
        receipt.audit.staged.commit
    );
    assert_eq!(
        fs::read_to_string(f.dir.path().join("app.txt"))
            .expect("landed")
            .trim(),
        "pass"
    );
    assert_eq!(receipt.audit.check_hash, checks.sha256);
    assert_eq!(receipt.audit.checks[0].status, CheckStatus::Passed);
    assert!(receipt.audit.checks[0].stdout.contains("the check passed"));
    assert_eq!(*sink.records.lock().expect("lock records"), [receipt.audit]);
}

#[tokio::test]
async fn rejects_changed_checks_and_an_unrelated_approved_hash_before_staging() {
    let f = fixture("pass").await;
    let mut checks = freeze(CheckPolicy::default());
    let masker = SecretMasker::default();
    let sink = f.sink();
    let approved = checks.sha256.clone();
    checks.checks[0]
        .args
        .push("changed after approval".to_owned());
    assert!(matches!(
        checks.verify_integrity(),
        Err(LandGateError::CheckFreezeMismatch)
    ));
    let mut request = f.request(&checks, &masker, &sink);
    request.approved_check_hash = &approved;
    assert!(matches!(
        execute_v1_landing(request).await,
        Err(LandGateError::CheckFreezeMismatch)
    ));

    let checks =
        FrozenCheckSet::freeze(checks.checks, CheckPolicy::default()).expect("freeze new set");
    let mut request = f.request(&checks, &masker, &sink);
    request.approved_check_hash = &approved;
    assert!(matches!(
        execute_v1_landing(request).await,
        Err(LandGateError::CheckFreezeMismatch)
    ));
    assert_eq!(f.worktree.head().await.expect("task head"), f.base);
    assert_eq!(
        git(f.worktree.path(), &["diff", "--cached", "--name-only"]),
        ""
    );
    f.assert_protected_head_unchanged().await;
    assert!(sink.records.lock().expect("lock records").is_empty());
}

#[test]
fn freeze_rejects_invalid_checks_and_hashes_the_policy_and_order() {
    let check = command("test");
    let policy = CheckPolicy::default();
    for result in [
        FrozenCheckSet::freeze(vec![check.clone(), check.clone()], policy),
        FrozenCheckSet::freeze(vec![command(" ")], policy),
        FrozenCheckSet::freeze(
            vec![CheckCommand {
                program: String::new(),
                ..check.clone()
            }],
            policy,
        ),
        FrozenCheckSet::freeze(
            vec![check.clone()],
            CheckPolicy {
                timeout: Duration::ZERO,
                ..policy
            },
        ),
        FrozenCheckSet::freeze(
            vec![check.clone()],
            CheckPolicy {
                max_output_bytes: 0,
                ..policy
            },
        ),
    ] {
        assert!(matches!(result, Err(LandGateError::InvalidChecks(_))));
    }
    let frozen = FrozenCheckSet::freeze(vec![check, command("other")], policy).expect("freeze");
    assert_eq!(frozen.sha256.len(), 64);
    frozen.matches_hash(&frozen.sha256).expect("matches");
    let mut changed = frozen.clone();
    changed.policy.timeout += Duration::from_nanos(1);
    assert!(matches!(
        changed.verify_integrity(),
        Err(LandGateError::CheckFreezeMismatch)
    ));
    let mut changed = frozen;
    changed.checks.swap(0, 1);
    assert!(matches!(
        changed.verify_integrity(),
        Err(LandGateError::CheckFreezeMismatch)
    ));
}

#[tokio::test]
async fn a_timed_out_check_is_killed_and_never_lands() {
    let f = fixture("wait").await;
    let checks = freeze(CheckPolicy {
        timeout: Duration::from_millis(100),
        ..CheckPolicy::default()
    });
    let sink = f.sink();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        execute_v1_landing(f.request(&checks, &SecretMasker::default(), &sink)),
    )
    .await
    .expect("the child is not waited on past its timeout");
    let Err(LandGateError::ChecksFailed { receipt }) = result else {
        panic!("expected a failed check, got {result:?}");
    };
    assert_eq!(receipt.audit.checks[0].status, CheckStatus::TimedOut);
    assert_eq!(receipt.landed_commit, None);
    f.assert_protected_head_unchanged().await;
    assert!(sink.records.lock().expect("lock records").is_empty());
}

#[tokio::test]
async fn drains_both_streams_without_retaining_bytes_past_the_cap() {
    let f = fixture("output").await;
    let checks = freeze(CheckPolicy {
        timeout: Duration::from_secs(5),
        max_output_bytes: 1024,
    });
    let sink = f.sink();
    let receipt = execute_v1_landing(f.request(&checks, &SecretMasker::default(), &sink))
        .await
        .expect("chatty check still passes");
    let output = &receipt.audit.checks[0];
    assert!(output.truncated);
    assert_eq!(output.stdout.len(), 1024);
    assert_eq!(output.stderr.len(), 1024);
    assert!(output.stdout.contains("oooo"));
    assert!(output.stderr.contains("eeee"));
}

#[tokio::test]
async fn masks_check_output_before_returning_or_persisting_it() {
    let f = fixture("secrets").await;
    let checks = freeze(CheckPolicy::default());
    let sink = f.sink();
    let masker = SecretMasker::with_literals([LITERAL, "fake-long-secret"]);
    let receipt = execute_v1_landing(f.request(&checks, &masker, &sink))
        .await
        .expect("land");
    let records = sink.records.lock().expect("lock records");
    assert_eq!(records.as_slice(), std::slice::from_ref(&receipt.audit));
    for output in [
        &receipt.audit.checks[0].stdout,
        &records[0].checks[0].stderr,
    ] {
        assert!(output.contains("[REDACTED] [REDACTED] [REDACTED]"));
        for secret in [LITERAL, SK_TOKEN, BEARER] {
            assert!(!output.contains(secret));
        }
    }
}

#[tokio::test]
async fn masks_a_literal_cut_in_half_at_the_capture_boundary() {
    let f = fixture("cut secret").await;
    let checks = freeze(CheckPolicy {
        max_output_bytes: 1024,
        ..CheckPolicy::default()
    });
    let sink = f.sink();
    let masker = SecretMasker::with_literals([LITERAL]);
    let receipt = execute_v1_landing(f.request(&checks, &masker, &sink))
        .await
        .expect("land");
    let output = &receipt.audit.checks[0];
    assert!(output.truncated);
    assert!(output.stderr.ends_with("[REDACTED]"));
    assert!(!output.stderr.contains("fake"));
    assert!(output.stderr.len() <= 1024);
}

#[test]
fn masks_other_high_confidence_tokens_and_private_key_blocks() {
    let masker = SecretMasker::default();
    assert_eq!(
        masker.mask(concat!(
            "ghp_1234567890abcdef acan_owner_0123456789abcdef AKIA0000111122223333 ",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nnot a real key\n-----END OPENSSH PRIVATE KEY-----",
            "\nkeep this"
        )),
        "[REDACTED] [REDACTED] [REDACTED] [REDACTED]\nkeep this",
    );
}

#[tokio::test]
async fn a_failed_check_returns_masked_output_and_does_not_stop_the_other_checks() {
    let f = fixture("failure").await;
    let checks = FrozenCheckSet::freeze(
        vec![command("first"), command("second")],
        CheckPolicy::default(),
    )
    .expect("freeze");
    let sink = f.sink();
    let masker = SecretMasker::with_literals([LITERAL]);
    let result = execute_v1_landing(f.request(&checks, &masker, &sink)).await;
    let Err(LandGateError::ChecksFailed { receipt }) = result else {
        panic!("expected two failed checks, got {result:?}");
    };
    assert_eq!(receipt.audit.checks.len(), 2);
    for check in &receipt.audit.checks {
        assert!(matches!(check.status, CheckStatus::Failed { .. }));
        assert!(check.stderr.contains("[REDACTED]"));
        assert!(!check.stderr.contains(LITERAL));
    }
    f.assert_protected_head_unchanged().await;
    assert!(sink.records.lock().expect("lock records").is_empty());
}

#[tokio::test]
async fn an_event_write_failure_leaves_the_protected_branch_untouched() {
    let f = fixture("pass").await;
    let checks = freeze(CheckPolicy::default());
    let sink = RecordingSink {
        fail: true,
        ..f.sink()
    };
    let result = execute_v1_landing(f.request(&checks, &SecretMasker::default(), &sink)).await;
    assert!(
        matches!(result, Err(LandGateError::EventWriteHalted(error)) if error == "event storage unavailable")
    );
    f.assert_protected_head_unchanged().await;
    assert_ne!(
        f.worktree.head().await.expect("task head"),
        f.base,
        "staging did succeed"
    );
}

#[tokio::test]
async fn the_gate_cannot_commit_to_the_protected_branch_from_a_switched_worktree() {
    let f = fixture("pass").await;
    let checks = freeze(CheckPolicy::default());
    let sink = f.sink();
    git(
        f.worktree.path(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    let result = execute_v1_landing(f.request(&checks, &SecretMasker::default(), &sink)).await;
    assert!(matches!(result, Err(LandGateError::Stage(_))));
    f.assert_protected_head_unchanged().await;
    assert!(sink.records.lock().expect("lock records").is_empty());
}

#[tokio::test]
async fn the_event_store_persists_the_masked_audit_separately_from_origin_events() {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    let f = fixture("secrets").await;
    let dir = tempfile::tempdir().expect("state directory");
    let database = dir.path().join("world.db");
    let store = EventStore::open(&database).await.expect("open store");
    let checks = freeze(CheckPolicy::default());
    let masker = SecretMasker::with_literals([LITERAL]);
    let receipt = execute_v1_landing(f.request(&checks, &masker, &store))
        .await
        .expect("land");
    assert!(
        store
            .load_from(acan_core::Seq::new(0))
            .await
            .expect("origin log")
            .is_empty()
    );
    drop(store);

    let pool = SqlitePoolOptions::new()
        .connect_with(SqliteConnectOptions::new().filename(&database))
        .await
        .expect("reopen store");
    let json: String = sqlx::query_scalar("SELECT json FROM v1_landing_events ORDER BY seq")
        .fetch_one(&pool)
        .await
        .expect("read persisted audit event");
    let found: LandingAuditRecord = serde_json::from_str(&json).expect("decode audit event");
    assert_eq!(found, receipt.audit);
    assert!(!json.contains(LITERAL) && !json.contains(SK_TOKEN) && !json.contains(BEARER));
    pool.close().await;
}
