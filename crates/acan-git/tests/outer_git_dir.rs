//! A git variable in the caller's environment, such as `GIT_DIR`, must not redirect acan-git's
//! reads. An outer git process can set one for its hooks. The variable is set on a child process,
//! so this test binary's own environment stays as it was.

mod common;

use std::path::PathBuf;
use std::process::Command;

use acan_git::Repo;
use common::{commit_files, git, init};

/// Set on the child process that runs the checks.
const CHILD: &str = "ACAN_GIT_OUTER_GIT_DIR_CHILD";
const TESTED_ROOT: &str = "ACAN_GIT_OUTER_GIT_DIR_TESTED_ROOT";
const TESTED_HEAD: &str = "ACAN_GIT_OUTER_GIT_DIR_TESTED_HEAD";

#[tokio::test]
async fn an_outer_git_dir_does_not_redirect_the_reads() {
    if std::env::var_os(CHILD).is_some() {
        checks_in_the_child().await;
        return;
    }
    let tested = init().await;
    let other = init().await;
    let other_head = commit_files(&other.root, &[("other.txt", "other\n")], "other work");
    let tested_head = git(&tested.root, &["rev-parse", "HEAD"]);
    assert_ne!(tested_head, other_head, "the two repositories differ");

    let status = Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", "an_outer_git_dir_does_not_redirect_the_reads"])
        .env(CHILD, "1")
        .env(TESTED_ROOT, &tested.root)
        .env(TESTED_HEAD, &tested_head)
        .env("GIT_DIR", other.root.join(".git"))
        .current_dir(&tested.root)
        .status()
        .expect("the child test runs");
    assert!(status.success(), "a read followed the outer GIT_DIR");
}

/// Runs in the child, where `GIT_DIR` names the other repository.
async fn checks_in_the_child() {
    let root = PathBuf::from(std::env::var_os(TESTED_ROOT).expect("the tested root"));
    let tested_head = std::env::var(TESTED_HEAD).expect("the tested head");
    let repo = Repo::open_with_protected(&root, "main")
        .await
        .expect("the tested repository opens");
    assert_eq!(
        repo.root(),
        std::fs::canonicalize(&root)
            .expect("a canonical root")
            .as_path(),
        "the root is the tested repository"
    );
    let head = repo
        .protected_head()
        .await
        .expect("the protected head")
        .to_string();
    assert_eq!(head, tested_head, "the head is the tested repository's");
}
