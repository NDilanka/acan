//! The V1 scope scan against real task worktrees and their indexes.

mod common;

use std::fs;
use std::path::Path;

use acan_core::{CommitId, Generation};
use acan_git::{
    Author, ChangeStatus, ChangedFile, Error, LandV1Error, TaskWorktree, stage_and_commit_worktree,
};
use acan_intent::{PathScope, RepoRules, ScopeEntry};

use common::{TestRepo, commit_files, git, init, task_id};

struct Fixture {
    repo: TestRepo,
    task: TaskWorktree,
    base: CommitId,
}

async fn fixture() -> Fixture {
    let repo = init().await;
    let base = commit_files(
        &repo.root,
        &[
            (".gitignore", "/build/\n/.acan/\n"),
            ("src/app.txt", "original app\n"),
            ("Cargo.lock", "lock\n"),
            ("ci/checks.yml", "checks\n"),
        ],
        "test inputs",
    )
    .parse()
    .expect("base commit");
    let task = repo
        .repo
        .worktrees()
        .create(task_id("t-1"), Generation::new(1))
        .await
        .expect("task worktree");
    Fixture { repo, task, base }
}

fn scope(input: &str) -> PathScope {
    PathScope::new([ScopeEntry::from_input(input).expect("scope entry")]).expect("scope")
}

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().expect("file parent")).expect("create parent");
    fs::write(path, text).expect("write file");
}

impl Fixture {
    async fn stage(
        &self,
        scope: &PathScope,
        rules: &RepoRules,
    ) -> Result<acan_git::StagedCommit, LandV1Error> {
        stage_and_commit_worktree(
            self.task.path(),
            &self.base,
            scope,
            rules,
            &Author::new("Acan", "acan@example.com"),
            "approved task edits",
        )
        .await
    }

    async fn assert_clean_index_and_unchanged_heads(&self) {
        assert_eq!(
            git(self.task.path(), &["diff", "--cached", "--name-only"]),
            ""
        );
        assert_eq!(self.task.head().await.expect("task head"), self.base);
        assert_eq!(
            self.repo
                .repo
                .protected_head()
                .await
                .expect("protected head"),
            self.base
        );
    }
}

#[tokio::test]
async fn commits_in_scope_paths_with_acans_author() {
    let f = fixture().await;
    write(f.task.path(), "src/app.txt", "edited\n");
    write(f.task.path(), "src/new café.txt", "new file\n");
    let staged = f
        .stage(&scope("src/"), &RepoRules::default())
        .await
        .expect("commit");
    assert_eq!(staged.base, f.base);
    f.assert_clean_index_and_unchanged_heads().await;
    assert_eq!(
        git(
            f.task.path(),
            &[
                "log",
                "-1",
                "--format=%an <%ae>|%cn <%ce>|%s",
                staged.commit.as_str()
            ]
        ),
        "Acan <acan@example.com>|Acan <acan@example.com>|approved task edits"
    );
    assert_eq!(
        staged.changed,
        vec![
            ChangedFile {
                path: "src/app.txt".to_owned(),
                status: ChangeStatus::Modified
            },
            ChangedFile {
                path: "src/new café.txt".to_owned(),
                status: ChangeStatus::Added
            },
        ]
    );
}

#[tokio::test]
async fn refuses_direct_agent_commits() {
    let f = fixture().await;
    let agent = commit_files(f.task.path(), &[("src/app.txt", "agent commit\n")], "agent");
    let actual_head = agent.parse().expect("agent head");
    let result = f.stage(&scope("src/"), &RepoRules::default()).await;
    assert!(matches!(result,
        Err(LandV1Error::AgentCommittedDirectly { expected_base, actual_head: found })
        if expected_base == f.base && found == actual_head
    ));
    assert_eq!(f.task.head().await.expect("agent head"), actual_head);
}

#[tokio::test]
async fn refuses_edits_outside_the_approved_scope() {
    let f = fixture().await;
    write(f.task.path(), "shared.txt", "outside\n");
    let result = f.stage(&scope("src/"), &RepoRules::default()).await;
    assert!(matches!(result, Err(LandV1Error::OutOfScope { paths }) if paths == ["shared.txt"]));
    f.assert_clean_index_and_unchanged_heads().await;
}

#[tokio::test]
async fn refuses_protected_paths_even_with_a_root_scope() {
    let f = fixture().await;
    let rules = RepoRules {
        check_files: vec!["ci/checks.yml".parse().expect("check path")],
        ..RepoRules::default()
    };
    write(f.task.path(), ".factory/hooks.json", "{}");
    write(f.task.path(), "Cargo.lock", "new lock\n");
    write(f.task.path(), "ci/checks.yml", "new checks\n");
    // A forced ignored path is a staged change, not an ignored artifact.
    write(f.task.path(), ".acan/extra", "internal\n");
    git(f.task.path(), &["add", "-f", ".acan/extra"]);
    let result = f.stage(&scope("."), &rules).await;
    assert!(matches!(result, Err(LandV1Error::ProtectedPath { paths })
        if paths == [".acan/extra", ".factory/hooks.json", "Cargo.lock", "ci/checks.yml"]
    ));
    assert_eq!(
        git(f.task.path(), &["diff", "--cached", "--name-only"]),
        ".acan/extra"
    );
    assert_eq!(f.task.head().await.expect("task head"), f.base);
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_symlinks_inside_or_outside_the_worktree() {
    use std::os::unix::fs::symlink;

    let f = fixture().await;
    for (target, escaping) in [
        (f.repo.root.join("shared.txt"), true),
        (f.task.path().join("src/app.txt"), false),
    ] {
        let link = f.task.path().join("src/link");
        symlink(target, &link).expect("create symlink");
        let result = f.stage(&scope("src/"), &RepoRules::default()).await;
        match result {
            Err(LandV1Error::EscapeRefused { path }) if escaping => assert_eq!(path, "src/link"),
            Err(LandV1Error::SymlinkRefused { path }) if !escaping => assert_eq!(path, "src/link"),
            other => panic!("unexpected link outcome: {other:?}"),
        }
        f.assert_clean_index_and_unchanged_heads().await;
        fs::remove_file(link).expect("remove test symlink");
    }
    let src = f.task.path().join("src");
    fs::remove_file(src.join("app.txt")).expect("remove fixture file");
    fs::remove_dir(&src).expect("remove fixture directory");
    symlink(f.repo.root.join("src"), src).expect("replace directory with symlink");
    let result = f.stage(&scope("src/"), &RepoRules::default()).await;
    assert!(matches!(result, Err(LandV1Error::EscapeRefused { .. })));
    f.assert_clean_index_and_unchanged_heads().await;
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_a_nonportable_untracked_path() {
    let f = fixture().await;
    write(f.task.path(), "src/C:outside.txt", "unportable\n");
    let result = f.stage(&scope("src/"), &RepoRules::default()).await;
    assert!(
        matches!(result, Err(LandV1Error::EscapeRefused { path }) if path == "src/C:outside.txt")
    );
    f.assert_clean_index_and_unchanged_heads().await;
}

#[tokio::test]
async fn refuses_a_staged_gitlink_even_without_a_checkout() {
    let f = fixture().await;
    let entry = format!("160000,{},src/submodule", f.base);
    git(
        f.task.path(),
        &["update-index", "--add", "--cacheinfo", &entry],
    );
    let result = f.stage(&scope("src/"), &RepoRules::default()).await;
    assert!(
        matches!(result, Err(LandV1Error::SubmoduleRefused { path }) if path == "src/submodule")
    );
    assert_eq!(f.task.head().await.expect("task head"), f.base);
}

#[tokio::test]
async fn refuses_renames_in_both_directions_even_when_not_staged() {
    for (from, to, prestage) in [
        ("shared.txt", "src/from-outside.txt", false),
        ("src/app.txt", "to-outside.txt", false),
        ("src/app.txt", "to-outside.txt", true),
    ] {
        let f = fixture().await;
        if prestage {
            git(f.task.path(), &["mv", from, to]);
        } else {
            fs::rename(f.task.path().join(from), f.task.path().join(to)).expect("rename file");
        }
        let error = f
            .stage(&scope("src/"), &RepoRules::default())
            .await
            .expect_err("refuse rename");
        assert!(
            matches!(&error, LandV1Error::RenameOutsideScope { from: a, to: b } if a == from && b == to),
            "{error:?}"
        );
        let index = git(f.task.path(), &["diff", "--cached", "--name-status"]);
        assert_eq!(
            index,
            if prestage {
                "R100\tsrc/app.txt\tto-outside.txt"
            } else {
                ""
            }
        );
        assert_eq!(f.task.head().await.expect("task head"), f.base);
        assert!(
            f.task.path().join(to).exists(),
            "unstaging does not discard local work"
        );
    }
}

#[tokio::test]
async fn refuses_a_copy_from_an_unchanged_out_of_scope_source_after_staging() {
    let f = fixture().await;
    fs::copy(
        f.task.path().join("shared.txt"),
        f.task.path().join("src/copied.txt"),
    )
    .expect("copy file");
    let result = f.stage(&scope("src/"), &RepoRules::default()).await;
    assert!(
        matches!(result, Err(LandV1Error::RenameOutsideScope { from, to })
        if from == "shared.txt" && to == "src/copied.txt")
    );
    f.assert_clean_index_and_unchanged_heads().await;
}

#[tokio::test]
async fn accounts_for_ignored_files_without_committing_them() {
    let f = fixture().await;
    write(f.task.path(), "build/tmp.log", "log\n");
    write(f.task.path(), "build/deeper/trace.log", "trace\n");
    write(f.task.path(), "src/app.txt", "edited\n");
    let staged = f
        .stage(&scope("src/"), &RepoRules::default())
        .await
        .expect("commit");
    assert_eq!(
        staged.ignored.paths,
        ["build/deeper/trace.log", "build/tmp.log"]
    );
    assert_eq!(staged.changed.len(), 1);
    assert_eq!(
        git(
            f.task.path(),
            &[
                "ls-tree",
                "-r",
                "--name-only",
                staged.commit.as_str(),
                "--",
                "build"
            ]
        ),
        ""
    );
}

#[tokio::test]
async fn refuses_protected_ignored_files_even_without_non_ignored_changes() {
    let f = fixture().await;
    write(f.task.path(), ".acan/agent-file", "internal\n");
    write(f.task.path(), "build/check.yml", "ignored check\n");
    let rules = RepoRules {
        check_files: vec!["build/check.yml".parse().expect("check file")],
        ..RepoRules::default()
    };
    let result = f.stage(&scope("src/"), &rules).await;
    assert!(
        matches!(result, Err(LandV1Error::ProtectedIgnoredFiles { paths })
        if paths == [".acan/agent-file", "build/check.yml"])
    );
    f.assert_clean_index_and_unchanged_heads().await;
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_an_escaping_symlink_inside_a_collapsed_ignored_directory() {
    let f = fixture().await;
    write(f.task.path(), "build/deeper/trace.log", "trace\n");
    std::os::unix::fs::symlink(&f.repo.root, f.task.path().join("build/deeper/link"))
        .expect("create symlink");
    let result = f.stage(&scope("src/"), &RepoRules::default()).await;
    assert!(
        matches!(result, Err(LandV1Error::EscapeRefused { path }) if path == "build/deeper/link")
    );
    f.assert_clean_index_and_unchanged_heads().await;
}

#[tokio::test]
async fn refuses_nested_git_files_and_directories_that_git_status_hides() {
    let f = fixture().await;
    write(f.task.path(), "src/.git", "gitdir: invalid");
    fs::create_dir_all(f.task.path().join("build/deeper/.git")).expect("create nested metadata");
    let result = f.stage(&scope("src/"), &RepoRules::default()).await;
    assert!(matches!(result, Err(LandV1Error::ProtectedPath { paths })
        if paths == ["build/deeper/.git", "src/.git"]));
    f.assert_clean_index_and_unchanged_heads().await;
}

#[cfg(unix)]
#[tokio::test]
async fn an_ignored_internal_symlink_is_accounted_for_but_not_staged() {
    let f = fixture().await;
    write(f.task.path(), "src/app.txt", "edited\n");
    fs::create_dir(f.task.path().join("build")).expect("create ignored directory");
    std::os::unix::fs::symlink(f.task.path().join("src"), f.task.path().join("build/link"))
        .expect("create symlink");
    let staged = f
        .stage(&scope("src/"), &RepoRules::default())
        .await
        .expect("stage");
    assert_eq!(staged.ignored.paths, ["build/link"]);
    assert_eq!(staged.changed.len(), 1);
}

#[tokio::test]
async fn neither_staging_nor_install_runs_repo_hooks() {
    for checkout in [true, false] {
        let f = fixture().await;
        if !checkout {
            git(&f.repo.root, &["checkout", "--detach", f.base.as_str()]);
        }
        let hooks = f.repo.root.join(".git/test-hooks");
        for name in [
            "pre-commit",
            "post-index-change",
            "reference-transaction",
            "post-merge",
        ] {
            write(&hooks, name, "#!/bin/sh\n: > hook-ran\nexit 1\n");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(hooks.join(name), fs::Permissions::from_mode(0o755))
                    .expect("make hook executable");
            }
        }
        git(
            f.task.path(),
            &[
                "config",
                "core.hooksPath",
                hooks.to_str().expect("hooks path"),
            ],
        );
        write(f.task.path(), "src/app.txt", "edited\n");
        let scope = scope("src/");
        let rules = RepoRules::default();
        let staged = f
            .stage(&scope, &rules)
            .await
            .expect("stage with hooks disabled");
        let candidate = f
            .repo
            .repo
            .prepare_v1_candidate(
                staged,
                &f.base,
                &scope,
                &rules,
                &Author::new("Acan", "acan@example.com"),
            )
            .await
            .expect("prepare candidate");
        let landed = f
            .repo
            .repo
            .install_v1_candidate(&candidate)
            .await
            .expect("install");
        assert!(
            matches!(landed, acan_git::LandOutcome::Landed { commit } if commit == candidate.staged().commit)
        );
        assert!(!f.task.path().join("hook-ran").exists());
        assert!(!f.repo.root.join("hook-ran").exists());
    }
}

#[tokio::test]
async fn refuses_an_edit_that_merge_renames_outside_scope() {
    let f = fixture().await;
    git(&f.repo.root, &["mv", "src/app.txt", "moved.txt"]);
    git(&f.repo.root, &["commit", "-qm", "move the task file"]);
    let head = f.repo.repo.protected_head().await.expect("protected head");
    write(f.task.path(), "src/app.txt", "edited by task\n");
    let scope = scope("src/");
    let rules = RepoRules::default();
    let staged = f
        .stage(&scope, &rules)
        .await
        .expect("task edit is in scope");
    let result = f
        .repo
        .repo
        .prepare_v1_candidate(
            staged,
            &head,
            &scope,
            &rules,
            &Author::new("Acan", "acan@example.com"),
        )
        .await;
    assert!(matches!(result, Err(LandV1Error::OutOfScope { paths }) if paths == ["moved.txt"]));
    assert_eq!(
        f.repo.repo.protected_head().await.expect("protected head"),
        head
    );
}

#[tokio::test]
async fn validates_paths_from_both_sides_of_the_merged_tree() {
    for (protected, task) in [("README", "readme"), ("LIB/one.txt", "lib/two.txt")] {
        let f = fixture().await;
        let head = commit_files(
            &f.repo.root,
            &[(protected, "from protected\n")],
            "advance protected",
        )
        .parse()
        .expect("protected head");
        write(f.task.path(), task, "from task\n");
        let scope = scope(".");
        let rules = RepoRules::default();
        let staged = f
            .stage(&scope, &rules)
            .await
            .expect("no collision in task tree");
        let result = f
            .repo
            .repo
            .prepare_v1_candidate(
                staged,
                &head,
                &scope,
                &rules,
                &Author::new("Acan", "acan@example.com"),
            )
            .await;
        assert!(matches!(
            result,
            Err(LandV1Error::Git(Error::CaseCollision(_)))
        ));
        assert_eq!(
            f.repo.repo.protected_head().await.expect("protected head"),
            head
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_a_nonportable_path_inherited_unchanged_from_the_protected_head() {
    let f = fixture().await;
    let head = commit_files(
        &f.repo.root,
        &[("aux.txt", "not a portable filename\n")],
        "advance protected",
    )
    .parse()
    .expect("protected head");
    write(f.task.path(), "src/app.txt", "edited by task\n");
    let scope = scope(".");
    let rules = RepoRules::default();
    let staged = f.stage(&scope, &rules).await.expect("portable task tree");
    let result = f
        .repo
        .repo
        .prepare_v1_candidate(
            staged,
            &head,
            &scope,
            &rules,
            &Author::new("Acan", "acan@example.com"),
        )
        .await;
    assert!(matches!(
        result,
        Err(LandV1Error::EscapeRefused { path }) if path == "aux.txt"
    ));
    assert_eq!(
        f.repo.repo.protected_head().await.expect("protected head"),
        head
    );
}

#[tokio::test]
async fn preserves_staged_chmod_under_filemode_false_and_force_added_ignored_files() {
    let f = fixture().await;
    write(f.task.path(), ".gitignore", "src/forced.gen\n");
    write(f.task.path(), "src/forced.gen", "generated in scope\n");
    git(f.task.path(), &["config", "core.filemode", "false"]);
    git(
        f.task.path(),
        &["update-index", "--chmod=+x", "src/app.txt"],
    );
    git(f.task.path(), &["add", "-f", "src/forced.gen"]);

    let scope = scope(".");
    let staged = f
        .stage(&scope, &RepoRules::default())
        .await
        .expect("stage preserves index metadata");
    let tree = git(
        &f.repo.root,
        &["ls-tree", "-r", staged.commit.as_str(), "--", "src/"],
    );
    assert!(
        tree.contains("100755 blob") && tree.contains("src/app.txt"),
        "expected executable mode preserved for src/app.txt, got:\n{tree}"
    );
    assert!(
        tree.contains("src/forced.gen"),
        "expected force-added ignored file preserved, got:\n{tree}"
    );
}
