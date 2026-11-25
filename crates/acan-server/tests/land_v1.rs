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
            expected_head: self.base.clone(),
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
    expected_head: CommitId,
    fail: bool,
    records: Mutex<Vec<LandingAuditRecord>>,
}

impl LandingEventSink for RecordingSink<'_> {
    fn record_landing<'a>(
        &'a self,
        record: &'a LandingAuditRecord,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(
                self.fixture
                    .repo
                    .protected_head()
                    .await
                    .expect("protected head"),
                self.expected_head
            );
            assert_eq!(record.expected_protected_head, self.expected_head);
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
const FINE_PAT: &str = "github_pat_0123456789abcdef_test";
const SLACK_TOKEN: &str = "xoxb-0000000000-test";
const DAEMON_SECRET_VAR: &str = "ACAN_LANDING_REGRESSION_SECRET";

// Run this binary as a check so the command/args and timeouts work on both Unix and Windows
// without relying on shell quoting or available interpreters.
#[test]
#[ignore = "executed as a subprocess by the landing checks"]
fn check_fixture() {
    match fs::read_to_string("app.txt")
        .expect("check ran in a materialized candidate")
        .trim()
    {
        "pass" => println!("the check passed"),
        "wait" => std::thread::sleep(Duration::from_secs(10)),
        "merged" => assert_eq!(
            fs::read_to_string("protected.txt").expect("protected-head addition"),
            "from protected\n"
        ),
        "isolation" => {
            assert!(!Path::new(".git").exists());
            assert!(!Path::new("ignored.log").exists());
            assert!(std::env::var_os(DAEMON_SECRET_VAR).is_none());
            // A later check must not run in the snapshot this check dirtied.
            fs::write(".git", "check-created file").expect("create test file");
            fs::write("ignored.log", "check-created artifact").expect("create test file");
        }
        "output" => {
            print!("{}", "o".repeat(200 * 1024));
            eprint!("{}", "e".repeat(200 * 1024));
        }
        "secrets" => {
            println!("{LITERAL} {SK_TOKEN} {BEARER} {FINE_PAT} {SLACK_TOKEN}");
            eprintln!("{LITERAL} {SK_TOKEN} {BEARER} {FINE_PAT} {SLACK_TOKEN}");
        }
        "cut secret" => {
            // The cap lands inside the literal on stderr, with no other output on that stream.
            eprint!("{}{LITERAL}{}", "x".repeat(1012), "z".repeat(4096));
        }
        "failure" => panic!("failed with {LITERAL}"),
        #[cfg(unix)]
        other if other.starts_with("tree ") => {
            use std::io::Write;

            let (mode, address) = other[5..].split_once(' ').expect("mode and address");
            let mut connection = std::net::TcpStream::connect(address).expect("connect leader");
            connection.write_all(b"L").expect("identify leader");
            #[expect(
                clippy::zombie_processes,
                reason = "exercise timeout with a leader that exits before its descendant"
            )]
            let mut descendant =
                std::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args(["--ignored", "--exact", "check_descendant", "--nocapture"])
                    .spawn()
                    .expect("spawn descendant keeping stdout open");
            if mode == "wait" {
                descendant.wait().expect("wait for descendant");
            } else {
                std::process::exit(0);
            }
        }
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
    assert_eq!(f.worktree.head().await.expect("task head"), f.base);
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
async fn checks_and_audits_the_exact_merge_installed_on_a_newer_protected_head() {
    let f = fixture("merged").await;
    fs::write(f.dir.path().join("protected.txt"), "from protected\n").expect("protected file");
    fs::write(
        f.dir.path().join(".gitattributes"),
        "protected.txt export-ignore\n",
    )
    .expect("attributes must not change the materialized file set");
    git(f.dir.path(), &["add", ".gitattributes", "protected.txt"]);
    git(f.dir.path(), &["commit", "-qm", "advance protected"]);
    let head = f.repo.protected_head().await.expect("advanced head");
    let checks = freeze(CheckPolicy::default());
    let sink = RecordingSink {
        expected_head: head.clone(),
        ..f.sink()
    };
    let masker = SecretMasker::default();
    let mut request = f.request(&checks, &masker, &sink);
    request.expected_protected_head = &head;
    let receipt = execute_v1_landing(request).await.expect("land exact merge");
    let candidate = receipt.audit.staged.commit.as_str();
    assert_eq!(receipt.audit.checks[0].status, CheckStatus::Passed);
    assert_eq!(
        receipt.landed_commit.as_ref(),
        Some(&receipt.audit.staged.commit)
    );
    assert_eq!(
        f.repo
            .protected_head()
            .await
            .expect("protected head")
            .as_str(),
        candidate
    );
    assert_eq!(f.worktree.head().await.expect("unmoved task head"), f.base);
    let first = format!("{candidate}^1");
    let second_parent = format!("{candidate}^2^");
    assert_eq!(
        git(f.dir.path(), &["rev-parse", &first, &second_parent]),
        format!("{head}\n{}", f.base)
    );
    assert_eq!(receipt.audit.staged.changed.len(), 1);
    assert_eq!(receipt.audit.staged.changed[0].path, "app.txt");
    assert_eq!(*sink.records.lock().expect("lock records"), [receipt.audit]);
}

#[tokio::test]
async fn merge_conflicts_leave_both_branches_and_the_sink_untouched() {
    let f = fixture("pass").await;
    fs::write(f.dir.path().join("app.txt"), "conflicting protected edit\n")
        .expect("protected edit");
    git(f.dir.path(), &["commit", "-qam", "conflicting edit"]);
    let head = f.repo.protected_head().await.expect("advanced head");
    let checks = freeze(CheckPolicy::default());
    let sink = f.sink();
    let masker = SecretMasker::default();
    let mut request = f.request(&checks, &masker, &sink);
    request.expected_protected_head = &head;
    let error = execute_v1_landing(request)
        .await
        .expect_err("refuse conflict");
    assert!(
        matches!(error, LandGateError::Stage(detail) if detail.contains("conflicts with protected head"))
    );
    assert_eq!(f.repo.protected_head().await.expect("protected head"), head);
    assert_eq!(f.worktree.head().await.expect("task head"), f.base);
    assert!(sink.records.lock().expect("lock records").is_empty());
}

#[test]
fn checks_get_fresh_snapshots_and_do_not_inherit_daemon_secrets() {
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--ignored",
            "--exact",
            "isolated_check_driver",
            "--nocapture",
        ])
        .env(DAEMON_SECRET_VAR, "fake-daemon-secret")
        .status()
        .expect("start daemon fixture");
    assert!(status.success());
}

#[tokio::test]
#[ignore = "run in a subprocess with a synthetic daemon secret"]
async fn isolated_check_driver() {
    use std::io::Write;

    assert_eq!(
        std::env::var(DAEMON_SECRET_VAR).as_deref(),
        Ok("fake-daemon-secret")
    );
    let f = fixture("isolation").await;
    let mut exclude = fs::OpenOptions::new()
        .append(true)
        .open(f.dir.path().join(".git/info/exclude"))
        .expect("open exclude");
    writeln!(exclude, "/ignored.log").expect("configure ignore");
    fs::write(
        f.worktree.path().join("ignored.log"),
        "ignored task artifact",
    )
    .expect("write ignored artifact");
    assert!(f.worktree.path().join(".git").exists());
    let checks = FrozenCheckSet::freeze(
        vec![command("first"), command("second")],
        CheckPolicy::default(),
    )
    .expect("freeze");
    let sink = f.sink();
    let receipt = execute_v1_landing(f.request(&checks, &SecretMasker::default(), &sink))
        .await
        .expect("both clean-snapshot checks pass");
    assert_eq!(receipt.audit.staged.ignored.paths, ["ignored.log"]);
    assert_eq!(receipt.audit.checks.len(), 2);
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

#[cfg(unix)]
#[test]
#[ignore = "executed by the Unix check process as its descendant"]
fn check_descendant() {
    use std::io::Write;

    let app = fs::read_to_string("app.txt").expect("read check mode");
    let (_, address) = app.trim().rsplit_once(' ').expect("test address");
    let mut connection = std::net::TcpStream::connect(address).expect("connect descendant");
    connection.write_all(b"D").expect("identify descendant");
    std::thread::sleep(Duration::from_secs(10));
    drop(connection);
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_kills_the_leader_and_a_descendant_holding_stdout_open() {
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    for mode in ["wait", "exit"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
        let address = listener.local_addr().expect("listener address");
        let f = fixture(&format!("tree {mode} {address}\n")).await;
        let checks = freeze(CheckPolicy {
            timeout: Duration::from_secs(1),
            ..CheckPolicy::default()
        });
        let sink = f.sink();
        let masker = SecretMasker::default();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            execute_v1_landing(f.request(&checks, &masker, &sink)),
        )
        .await
        .expect("timed-out tree terminates promptly");
        let Err(LandGateError::ChecksFailed { receipt }) = result else {
            panic!("expected failed check, got {result:?}");
        };
        assert_eq!(receipt.audit.checks[0].status, CheckStatus::TimedOut);
        // Each process owned a separate connection. EOF proves death even when the
        // descendant is orphaned and its PID is still present as a zombie.
        let ids = tokio::time::timeout(Duration::from_secs(1), async {
            let mut ids = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("check process connected");
                stream
                    .read_to_end(&mut ids)
                    .await
                    .expect("process terminated and closed socket");
            }
            ids.sort_unstable();
            ids
        })
        .await
        .expect("both processes terminated promptly");
        assert_eq!(ids, b"DL");
        f.assert_protected_head_unchanged().await;
        assert!(sink.records.lock().expect("lock records").is_empty());
    }
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
        assert!(output.contains("[REDACTED] [REDACTED] [REDACTED] [REDACTED] [REDACTED]"));
        for secret in [LITERAL, SK_TOKEN, BEARER, FINE_PAT, SLACK_TOKEN] {
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
async fn a_transient_event_write_failure_leaves_both_branches_unchanged_and_can_be_retried() {
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
    assert_eq!(
        f.worktree.head().await.expect("task head"),
        f.base,
        "the same task base remains available for retry"
    );
    assert_eq!(
        git(f.worktree.path(), &["diff", "--cached", "--name-only"]),
        ""
    );
    let sink = f.sink();
    let receipt = execute_v1_landing(f.request(&checks, &SecretMasker::default(), &sink))
        .await
        .expect("retry with the same task base");
    assert_eq!(
        f.repo.protected_head().await.expect("protected head"),
        receipt.audit.staged.commit
    );
    assert_eq!(*sink.records.lock().expect("lock records"), [receipt.audit]);
}

#[tokio::test]
async fn masks_diagnostics_from_staging_check_spawn_and_install() {
    let f = fixture("pass").await;
    let checks = freeze(CheckPolicy::default());
    let masker = SecretMasker::default();
    let sink = f.sink();

    let outside = f.worktree.path().join(SLACK_TOKEN);
    fs::write(&outside, "outside scope").expect("create outside-scope file");
    let staged = execute_v1_landing(f.request(&checks, &masker, &sink))
        .await
        .expect_err("refuse out-of-scope file");
    assert!(matches!(&staged, LandGateError::Stage(detail) if detail.contains("[REDACTED]")));
    assert!(!format!("{staged:?} {staged}").contains(SLACK_TOKEN));
    fs::remove_file(outside).expect("restore task worktree");

    let missing = FrozenCheckSet::freeze(
        vec![CheckCommand {
            program: format!("./{SLACK_TOKEN}"),
            ..command("missing")
        }],
        CheckPolicy::default(),
    )
    .expect("freeze missing executable");
    let failed = execute_v1_landing(f.request(&missing, &masker, &sink)).await;
    let Err(LandGateError::ChecksFailed { receipt }) = failed else {
        panic!("expected spawn failure, got {failed:?}");
    };
    assert_eq!(receipt.audit.checks[0].status, CheckStatus::FailedToRun);
    assert!(
        receipt.audit.checks[0]
            .stderr
            .contains("could not start ./[REDACTED]")
    );
    assert!(!format!("{receipt:?}").contains(SLACK_TOKEN));

    git(f.dir.path(), &["branch", FINE_PAT]);
    let repo = Repo::open_with_protected(f.dir.path(), FINE_PAT)
        .await
        .expect("alternate protected branch");
    let lock = format!(".git/refs/heads/{FINE_PAT}.lock");
    fs::write(f.dir.path().join(lock), "").expect("block ref install");
    let mut request = f.request(&checks, &masker, &sink);
    request.repo = &repo;
    let install = execute_v1_landing(request)
        .await
        .expect_err("locked ref prevents install");
    assert!(matches!(&install, LandGateError::Install(detail) if detail.contains("[REDACTED]")));
    assert!(!format!("{install:?} {install}").contains(FINE_PAT));
    assert_eq!(repo.protected_head().await.expect("alternate head"), f.base);
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
    for secret in [LITERAL, SK_TOKEN, BEARER, FINE_PAT, SLACK_TOKEN] {
        assert!(!json.contains(secret));
    }
    pool.close().await;
}
