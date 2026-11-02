//! Preparing a candidate for the gate, materializing it for a run, and installing it.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use acan_core::Generation;
use acan_git::{InstallOutcome, Preparation, PrepareOutcome, ProtectedPaths};
use acan_intent::{
    ArtifactHash, CommitSha, ContentHash, GateContext, LedgerHead, OverlayManifest, RepoPath,
};

use common::{TestRepo, commit_files, git, init, task_id};

const PROBE: &str = "{\"probe\": \"login\"}\n";
const TEST: &str = "test('login', () => {});\n";

fn sha(raw: &str) -> CommitSha {
    raw.parse().expect("a commit sha")
}

fn path(raw: &str) -> RepoPath {
    raw.parse().expect("a repo path")
}

fn protected() -> ProtectedPaths {
    ProtectedPaths::new([path("test/login.test.ts")])
}

/// The protected files of [`plant`], as the overlay must hold them.
fn plant_overlay() -> OverlayManifest {
    let files = [
        (".acan/probes/login.json", PROBE),
        ("test/login.test.ts", TEST),
    ];
    OverlayManifest {
        files: files
            .iter()
            .map(|(name, content)| {
                let hash: ArtifactHash = ContentHash::of_bytes(content.as_bytes()).into();
                (path(name), hash)
            })
            .collect(),
    }
}

/// A repository whose protected head holds a probe, a protected test, and app code.
async fn plant() -> TestRepo {
    let t = init().await;
    commit_files(
        &t.root,
        &[
            (".acan/probes/login.json", PROBE),
            ("test/login.test.ts", TEST),
            ("src/app.ts", "app v1\n"),
        ],
        "protected inputs",
    );
    t
}

fn main_head(t: &TestRepo) -> CommitSha {
    sha(&git(&t.root, &["rev-parse", "refs/heads/main"]))
}

fn write(root: &Path, name: &str, content: &str) {
    let full = root.join(name);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(full, content).expect("write file");
}

/// The file's text with LF endings. A checkout with `core.autocrlf` writes CRLF.
fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path)
        .expect("file exists")
        .replace("\r\n", "\n")
}

/// A task worktree branched off the protected head as it is now.
async fn worktree(t: &TestRepo, task: &str) -> PathBuf {
    t.repo
        .worktrees()
        .create(task_id(task), Generation::new(1))
        .await
        .expect("create worktree")
        .path()
        .to_path_buf()
}

/// Stage everything in `dir`, ignored `.acan` files included, and commit it.
fn commit_all(dir: &Path, message: &str) -> CommitSha {
    git(dir, &["add", "-A", "--force"]);
    git(dir, &["commit", "-q", "-m", message]);
    sha(&git(dir, &["rev-parse", "HEAD"]))
}

/// A candidate branched off the protected head now, with `edit` applied and committed.
async fn candidate(t: &TestRepo, task: &str, edit: impl FnOnce(&Path)) -> CommitSha {
    let dir = worktree(t, task).await;
    edit(&dir);
    commit_all(&dir, task)
}

async fn prepare(t: &TestRepo, candidate: &CommitSha) -> Preparation {
    match t
        .repo
        .prepare(candidate, &protected())
        .await
        .expect("prepare")
    {
        PrepareOutcome::Prepared(preparation) => preparation,
        other => panic!("expected Prepared, got {other:?}"),
    }
}

/// Materialize into a fresh directory, which lives as long as the returned guard.
async fn materialize(t: &TestRepo, preparation: &Preparation) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let hash = t
        .repo
        .materialize(preparation, dir.path())
        .await
        .expect("materialize");
    assert_eq!(hash, preparation.overlay_hash());
    dir
}

/// A context the caller or a candidate filled with claims of its own.
fn claimed_context() -> GateContext {
    let hash = |label: &str| ContentHash::of_bytes(label.as_bytes());
    GateContext {
        protected_head: sha(&"a".repeat(40)),
        merged_commit: sha(&"b".repeat(40)),
        overlay_hash: hash("claimed overlay").into(),
        effective_artifact_hashes: BTreeMap::new(),
        ledger_head: LedgerHead {
            event_seq: 0,
            hash: None,
        },
        scope_hash: hash("scope").into(),
        probe_set_hash: hash("probes").into(),
        fixture_set_hash: hash("fixtures").into(),
        runner_hash: hash("runner").into(),
        evaluator_version: "acan-intent/0.1.0".parse().expect("a version"),
        runtime: "bun-1.4.4".parse().expect("a runtime"),
        controlled_env_hash: hash("env").into(),
        sandbox_policy_hash: hash("policy").into(),
    }
}

fn blob(t: &TestRepo, commit: &CommitSha, name: &str) -> String {
    git(&t.root, &["cat-file", "blob", &format!("{commit}:{name}")])
}

fn tree_paths(t: &TestRepo, commit: &CommitSha) -> Vec<String> {
    git(&t.root, &["ls-tree", "-r", "--name-only", commit.as_str()])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The stand-in for a test suite: every name any `calls*.txt` calls is defined in
/// `defs.txt`.
fn suite_passes(dir: &Path) -> bool {
    let defs = std::fs::read_to_string(dir.join("defs.txt")).expect("defs.txt");
    std::fs::read_dir(dir)
        .expect("read dir")
        .map(|entry| entry.expect("entry").path())
        .filter(|file| {
            file.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("calls"))
        })
        .all(|file| {
            let calls = std::fs::read_to_string(file).expect("calls file");
            calls
                .lines()
                .all(|name| defs.lines().any(|def| def == name))
        })
}

#[tokio::test]
async fn prepare_moves_no_branch_and_no_checkout() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;
    commit_files(&t.root, &[("docs.txt", "landed meanwhile\n")], "meanwhile");
    let head = main_head(&t);
    let refs = git(&t.root, &["for-each-ref"]);
    let status = git(&t.root, &["status", "--porcelain"]);

    let preparation = prepare(&t, &c).await;

    assert_eq!(main_head(&t), head, "the protected head did not move");
    assert_eq!(
        git(&t.root, &["for-each-ref"]),
        refs,
        "no ref was created or moved"
    );
    assert_eq!(git(&t.root, &["status", "--porcelain"]), status);
    assert_eq!(read_text(&t.root.join("src/app.ts")), "app v1\n");
    assert_eq!(preparation.protected_head(), &head);
    assert_eq!(preparation.candidate(), &c);
    let merged = preparation.merged_commit();
    assert_eq!(
        git(
            &t.root,
            &["rev-list", "--parents", "-n", "1", merged.as_str()]
        ),
        format!("{merged} {head} {c}"),
        "the prepared commit merges the candidate into the protected head"
    );
    assert_eq!(blob(&t, merged, "src/app.ts"), "app v2");
    assert_eq!(blob(&t, merged, "docs.txt"), "landed meanwhile");
    assert!(preparation.discarded().is_empty());
}

#[tokio::test]
async fn a_candidate_on_top_of_the_head_is_prepared_as_itself() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;

    let preparation = prepare(&t, &c).await;

    assert_eq!(preparation.merged_commit(), &c);
    assert_eq!(preparation.overlay(), &plant_overlay());
}

/// Lets the clock pass a second. Git records commit times to the second, so two commits made
/// within one second can be identical even when their times were meant to differ.
fn wait_for_the_clock() {
    std::thread::sleep(std::time::Duration::from_millis(1100));
}

/// The author and committer names, emails and dates of `commit`, as git reports them.
fn identity(t: &TestRepo, commit: &CommitSha) -> String {
    git(
        &t.root,
        &[
            "log",
            "-1",
            "--date=raw",
            "--format=%an|%ae|%ad|%cn|%ce|%cd",
            commit.as_str(),
        ],
    )
}

/// A merge commit depends only on its inputs. Preparing the same non-fast-forward candidate
/// again, a second later, names the same merged commit. The merge takes its names and dates
/// from the candidate. Before, it took fresh dates from the clock, so each prepare was a new
/// commit.
#[tokio::test]
async fn preparing_the_same_candidate_twice_gives_the_same_merged_commit() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;
    commit_files(&t.root, &[("docs.txt", "landed meanwhile\n")], "meanwhile");

    let first = prepare(&t, &c).await;
    wait_for_the_clock();
    let second = prepare(&t, &c).await;

    assert_eq!(
        second.merged_commit(),
        first.merged_commit(),
        "the same inputs give the same merged commit"
    );
    assert_eq!(second.overlay_hash(), first.overlay_hash());
    assert_eq!(
        identity(&t, first.merged_commit()),
        identity(&t, &c),
        "the merge carries the candidate's names and dates"
    );
}

/// A restart between the preparation and the receipt prepares the candidate again in a new
/// process. The origin recorded the first commit, and the run is refused unless the new one is
/// the same commit id.
#[tokio::test]
async fn a_restart_between_prepare_and_receipt_keeps_the_merged_commit() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;
    commit_files(&t.root, &[("docs.txt", "landed meanwhile\n")], "meanwhile");
    let recorded = prepare(&t, &c).await;
    wait_for_the_clock();

    let restarted = acan_git::Repo::open(&t.root)
        .await
        .expect("the repository reopens");
    let again = match restarted
        .prepare(&c, &protected())
        .await
        .expect("prepare after the restart")
    {
        PrepareOutcome::Prepared(preparation) => preparation,
        other => panic!("expected Prepared, got {other:?}"),
    };

    assert_eq!(
        again.merged_commit(),
        recorded.merged_commit(),
        "the restarted preparation names the recorded commit"
    );
    assert_eq!(again.protected_head(), recorded.protected_head());
}

#[tokio::test]
async fn prepare_refuses_a_candidate_that_is_not_a_commit() {
    let t = plant().await;
    let missing = sha(&"0".repeat(40));
    let err = t
        .repo
        .prepare(&missing, &protected())
        .await
        .expect_err("not a commit");
    assert!(matches!(err, acan_git::Error::Git { .. }), "{err:?}");
}

#[tokio::test]
async fn branches_that_conflict_together_are_caught_at_prepare() {
    let t = plant().await;
    let a = candidate(&t, "t-1", |dir| write(dir, "shared.txt", "from a\n")).await;
    let b = candidate(&t, "t-2", |dir| write(dir, "shared.txt", "from b\n")).await;
    let alone = prepare(&t, &b).await;
    assert_eq!(alone.merged_commit(), &b, "b merges cleanly on its own");

    let first = prepare(&t, &a).await;
    let installed = t
        .repo
        .install(&first, &first.bind(claimed_context()))
        .await
        .expect("install a");
    assert!(
        matches!(installed, InstallOutcome::Installed { .. }),
        "{installed:?}"
    );
    let head = main_head(&t);

    let outcome = t.repo.prepare(&b, &protected()).await.expect("prepare b");

    assert_eq!(
        outcome,
        PrepareOutcome::Conflict {
            protected_head: head.clone(),
            files: vec![path("shared.txt")],
        }
    );
    assert_eq!(main_head(&t), head, "a conflict moves nothing");
}

#[tokio::test]
async fn branches_that_pass_alone_and_fail_together_fail_on_the_prepared_commit() {
    let t = plant().await;
    commit_files(
        &t.root,
        &[("defs.txt", "greet\n"), ("calls.txt", "greet\n")],
        "a definition and its caller",
    );
    let a = candidate(&t, "t-1", |dir| {
        write(dir, "defs.txt", "hello\n");
        write(dir, "calls.txt", "hello\n");
    })
    .await;
    let b = candidate(&t, "t-2", |dir| write(dir, "calls-b.txt", "greet\n")).await;

    let prepared_a = prepare(&t, &a).await;
    let b_alone = prepare(&t, &b).await;
    assert!(suite_passes(materialize(&t, &prepared_a).await.path()));
    assert!(suite_passes(materialize(&t, &b_alone).await.path()));

    let installed = t
        .repo
        .install(&prepared_a, &prepared_a.bind(claimed_context()))
        .await
        .expect("install a");
    assert!(
        matches!(installed, InstallOutcome::Installed { .. }),
        "{installed:?}"
    );

    let stale = t
        .repo
        .install(&b_alone, &b_alone.bind(claimed_context()))
        .await
        .expect("install stale b");
    assert_eq!(
        stale,
        InstallOutcome::HeadMoved {
            current: prepared_a.merged_commit().clone()
        },
        "b was tested alone, not with a"
    );

    let together = prepare(&t, &b).await;
    assert_eq!(together.protected_head(), prepared_a.merged_commit());
    assert!(
        !suite_passes(materialize(&t, &together).await.path()),
        "b merged onto a calls a name a removed"
    );
}

#[tokio::test]
async fn install_moves_the_head_to_the_prepared_commit_only() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;
    commit_files(&t.root, &[("docs.txt", "landed meanwhile\n")], "meanwhile");
    let preparation = prepare(&t, &c).await;
    let tested = preparation.bind(claimed_context());

    let outcome = t
        .repo
        .install(&preparation, &tested)
        .await
        .expect("install");

    let merged = preparation.merged_commit().clone();
    assert_eq!(
        outcome,
        InstallOutcome::Installed {
            commit: merged.clone()
        }
    );
    assert_eq!(main_head(&t), merged);
    // The clean checkout of main follows the install.
    assert_eq!(read_text(&t.root.join("src/app.ts")), "app v2\n");
    assert_eq!(git(&t.root, &["status", "--porcelain"]), "");

    let again = t.repo.install(&preparation, &tested).await.expect("again");
    assert_eq!(again, InstallOutcome::Installed { commit: merged });
}

#[tokio::test]
async fn install_refuses_a_context_that_names_another_preparation() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;
    commit_files(&t.root, &[("docs.txt", "landed meanwhile\n")], "meanwhile");
    let preparation = prepare(&t, &c).await;
    let head = main_head(&t);
    let tested = preparation.bind(claimed_context());

    let mut other_commit = tested.clone();
    other_commit.merged_commit = c.clone();
    let mut other_overlay = tested.clone();
    other_overlay.overlay_hash = ContentHash::of_bytes(b"another overlay").into();
    let mut other_head = tested.clone();
    other_head.protected_head = c.clone();

    for context in [claimed_context(), other_commit, other_overlay, other_head] {
        let outcome = t
            .repo
            .install(&preparation, &context)
            .await
            .expect("install");
        assert_eq!(outcome, InstallOutcome::ContextMismatch);
        assert_eq!(main_head(&t), head, "a mismatched context moves nothing");
    }
}

#[tokio::test]
async fn a_moved_head_invalidates_the_preparation() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;
    let preparation = prepare(&t, &c).await;
    commit_files(&t.root, &[("docs.txt", "landed after prepare\n")], "after");
    let moved = main_head(&t);

    let outcome = t
        .repo
        .install(&preparation, &preparation.bind(claimed_context()))
        .await
        .expect("install");

    assert_eq!(
        outcome,
        InstallOutcome::HeadMoved {
            current: moved.clone()
        }
    );
    assert_eq!(main_head(&t), moved, "nothing was installed");

    let fresh = prepare(&t, &c).await;
    assert_eq!(fresh.protected_head(), &moved);
    let outcome = t
        .repo
        .install(&fresh, &fresh.bind(claimed_context()))
        .await
        .expect("install fresh");
    assert_eq!(
        outcome,
        InstallOutcome::Installed {
            commit: fresh.merged_commit().clone()
        }
    );
}

#[tokio::test]
async fn the_gate_context_takes_the_fields_git_owns_from_git() {
    let t = plant().await;
    let head = main_head(&t);
    let c = candidate(&t, "t-1", |dir| {
        write(dir, "src/app.ts", "app v2\n");
        write(
            dir,
            ".acan/probes/login.json",
            "{\"probe\": \"weakened\"}\n",
        );
    })
    .await;
    let preparation = prepare(&t, &c).await;
    let claimed = claimed_context();

    let bound = preparation.bind(claimed.clone());

    assert_eq!(bound.protected_head, head);
    assert_eq!(&bound.merged_commit, preparation.merged_commit());
    assert_ne!(
        bound.merged_commit, c,
        "the candidate forged a protected file"
    );
    assert_eq!(
        bound.overlay_hash,
        plant_overlay().overlay_hash().expect("hashing"),
        "the overlay hash is the protected head's, not the candidate's"
    );
    let mut rest = bound.clone();
    rest.protected_head = claimed.protected_head.clone();
    rest.merged_commit = claimed.merged_commit.clone();
    rest.overlay_hash = claimed.overlay_hash;
    assert_eq!(rest, claimed, "fields git does not own are left alone");
}

#[tokio::test]
async fn a_deleted_protected_test_is_restored_from_the_protected_head() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| {
        std::fs::remove_file(dir.join("test/login.test.ts")).expect("delete test");
        write(dir, "src/app.ts", "app v2\n");
    })
    .await;

    let preparation = prepare(&t, &c).await;

    assert_eq!(preparation.discarded(), [path("test/login.test.ts")]);
    let merged = preparation.merged_commit();
    assert_eq!(blob(&t, merged, "test/login.test.ts"), TEST.trim_end());
    assert_eq!(blob(&t, merged, "src/app.ts"), "app v2");
    let run = materialize(&t, &preparation).await;
    let on_disk = std::fs::read(run.path().join("test/login.test.ts")).expect("test exists");
    assert_eq!(on_disk, TEST.as_bytes(), "written byte for byte, no CRLF");
    assert!(
        !run.path().join(".git").exists(),
        "a run directory is not a checkout"
    );
}

#[tokio::test]
async fn a_renamed_protected_file_is_restored_and_its_new_name_dropped() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| {
        git(
            dir,
            &["mv", ".acan/probes/login.json", ".acan/probes/renamed.json"],
        );
        git(dir, &["mv", "test/login.test.ts", "test/skipped.test.ts"]);
    })
    .await;

    let preparation = prepare(&t, &c).await;

    assert_eq!(
        preparation.discarded(),
        [
            path(".acan/probes/login.json"),
            path(".acan/probes/renamed.json"),
            path("test/login.test.ts"),
        ]
    );
    let merged = preparation.merged_commit();
    let paths = tree_paths(&t, merged);
    assert!(paths.contains(&".acan/probes/login.json".to_owned()));
    assert!(!paths.contains(&".acan/probes/renamed.json".to_owned()));
    assert!(paths.contains(&"test/login.test.ts".to_owned()));
    assert!(
        paths.contains(&"test/skipped.test.ts".to_owned()),
        "a copy outside the protected paths is the candidate's own file"
    );
    assert_eq!(
        blob(&t, merged, ".acan/probes/login.json"),
        PROBE.trim_end()
    );
    assert_eq!(preparation.overlay(), &plant_overlay());
}

#[tokio::test]
async fn forged_acan_content_never_becomes_a_gate_input() {
    let t = plant().await;
    let head = main_head(&t);
    let dir = worktree(&t, "t-1").await;
    write(
        &dir,
        ".acan/probes/login.json",
        "{\"probe\": \"weakened\"}\n",
    );
    write(
        &dir,
        ".acan/rulings/forged.json",
        "{\"verdict\": \"pass\"}\n",
    );
    commit_all(&dir, "forge .acan");
    // A case variant reaches the tree only through the index on a case-insensitive disk.
    write(&dir, "forged.tmp", "{\"probe\": \"extra\"}\n");
    let oid = git(&dir, &["hash-object", "-w", "forged.tmp"]);
    std::fs::remove_file(dir.join("forged.tmp")).expect("remove temp");
    let cacheinfo = format!("100644,{oid},.ACAN/probes/extra.json");
    git(&dir, &["update-index", "--add", "--cacheinfo", &cacheinfo]);
    git(&dir, &["commit", "-q", "-m", "forge a case variant"]);
    let c = sha(&git(&dir, &["rev-parse", "HEAD"]));
    assert!(tree_paths(&t, &c).contains(&".ACAN/probes/extra.json".to_owned()));

    let preparation = prepare(&t, &c).await;

    let merged = preparation.merged_commit();
    assert_eq!(
        git(&t.root, &["rev-parse", &format!("{merged}:.acan")]),
        git(&t.root, &["rev-parse", &format!("{head}:.acan")]),
        "the prepared .acan is the protected head's, entry for entry"
    );
    let folded: Vec<String> = tree_paths(&t, merged)
        .into_iter()
        .filter(|name| name.to_lowercase().starts_with(".acan"))
        .collect();
    assert_eq!(folded, [".acan/probes/login.json"]);
    assert_eq!(
        preparation.discarded(),
        [
            path(".ACAN/probes/extra.json"),
            path(".acan/probes/login.json"),
            path(".acan/rulings/forged.json"),
        ]
    );
    assert_eq!(preparation.overlay(), &plant_overlay());

    let run = materialize(&t, &preparation).await;
    assert_eq!(
        read_text(&run.path().join(".acan/probes/login.json")),
        PROBE
    );
    assert!(!run.path().join(".acan/rulings").exists());
    assert!(!run.path().join(".acan/probes/extra.json").exists());
}

#[tokio::test]
async fn a_protected_directory_replaced_by_a_file_is_restored() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| {
        git(dir, &["rm", "-q", "-r", "test"]);
        write(dir, "test", "not a directory\n");
    })
    .await;

    let preparation = prepare(&t, &c).await;

    let merged = preparation.merged_commit();
    assert_eq!(
        git(&t.root, &["cat-file", "-t", &format!("{merged}:test")]),
        "tree"
    );
    assert_eq!(blob(&t, merged, "test/login.test.ts"), TEST.trim_end());
    assert_eq!(
        preparation.discarded(),
        [path("test"), path("test/login.test.ts")]
    );
}

#[tokio::test]
async fn restoring_before_a_run_undoes_what_the_last_run_wrote() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;
    let preparation = prepare(&t, &c).await;
    let run = materialize(&t, &preparation).await;
    let root = run.path();
    write(root, ".acan/probes/login.json", "tampered\n");
    write(root, ".acan/new.json", "{}\n");
    std::fs::remove_file(root.join("test/login.test.ts")).expect("delete test");
    write(root, "src/app.ts", "rebuilt by the run\n");

    let hash = t
        .repo
        .restore_protected(&preparation, root)
        .await
        .expect("restore");

    assert_eq!(hash, preparation.overlay_hash());
    assert_eq!(read_text(&root.join(".acan/probes/login.json")), PROBE);
    assert_eq!(read_text(&root.join("test/login.test.ts")), TEST);
    assert!(!root.join(".acan/new.json").exists());
    assert_eq!(
        read_text(&root.join("src/app.ts")),
        "rebuilt by the run\n",
        "files outside the protected paths are left alone"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn restoring_does_not_write_through_a_junction() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;
    let preparation = prepare(&t, &c).await;
    let run = materialize(&t, &preparation).await;
    let outside = tempfile::tempdir().expect("tempdir");
    let link = run.path().join("test");
    std::fs::remove_dir_all(&link).expect("remove test dir");
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path())
        .output()
        .expect("spawn mklink");
    assert!(made.status.success(), "mklink /J failed: {made:?}");

    t.repo
        .restore_protected(&preparation, run.path())
        .await
        .expect("restore");

    let meta = std::fs::symlink_metadata(&link).expect("test exists");
    assert!(meta.is_dir(), "test is a real directory again");
    assert_eq!(read_text(&link.join("login.test.ts")), TEST);
    assert_eq!(
        std::fs::read_dir(outside.path()).expect("outside").count(),
        0,
        "nothing was written through the junction"
    );
}

#[tokio::test]
async fn an_export_reads_the_protected_files_of_its_commit_after_the_head_moves_on() {
    let t = plant().await;
    let old = main_head(&t);
    commit_files(
        &t.root,
        &[(".acan/probes/login.json", "{\"probe\": \"logout\"}\n")],
        "the probe changes, the route count does not",
    );
    let head = main_head(&t);
    assert_ne!(head, old);

    let export = t
        .repo
        .export_protected(&old, &protected())
        .await
        .expect("export");
    let at_head = t
        .repo
        .export_protected(&head, &protected())
        .await
        .expect("export at the head");

    assert_eq!(export.commit(), &old);
    assert_eq!(export.overlay(), &plant_overlay());
    assert_ne!(export.overlay_hash(), at_head.overlay_hash());

    let dir = tempfile::tempdir().expect("tempdir");
    let hash = t
        .repo
        .write_protected(&export, dir.path())
        .await
        .expect("write");
    assert_eq!(hash, export.overlay_hash());
    assert_eq!(
        read_text(&dir.path().join(".acan/probes/login.json")),
        PROBE,
        "the probe of the commit, not of the head"
    );
}

#[tokio::test]
async fn an_export_refuses_a_commit_the_repository_does_not_have() {
    let t = plant().await;

    let missing = sha(&"c".repeat(40));

    assert!(
        t.repo
            .export_protected(&missing, &protected())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_gate_futures_can_run_on_a_spawned_task() {
    fn assert_send<T: Send>(_: &T) {}
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;
    let preparation = prepare(&t, &c).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let tested = preparation.bind(claimed_context());
    let protected = protected();
    assert_send(&t.repo.prepare(&c, &protected));
    assert_send(&t.repo.materialize(&preparation, dir.path()));
    assert_send(&t.repo.restore_protected(&preparation, dir.path()));
    assert_send(&t.repo.install(&preparation, &tested));
}

#[tokio::test]
async fn materialize_refuses_a_directory_that_is_not_empty() {
    let t = plant().await;
    let c = candidate(&t, "t-1", |dir| write(dir, "src/app.ts", "app v2\n")).await;
    let preparation = prepare(&t, &c).await;
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "left.txt", "from an earlier run\n");

    let err = t
        .repo
        .materialize(&preparation, dir.path())
        .await
        .expect_err("not empty");

    assert!(matches!(err, acan_git::Error::TargetNotEmpty(_)), "{err:?}");
}
