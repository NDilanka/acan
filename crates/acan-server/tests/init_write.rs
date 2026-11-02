//! `acan init` against a real git repository: the write path, its refusals, and what it leaves
//! untouched. Each test builds its own clone in a temporary directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use acan_server::init_cli::{DEFAULT_BRANCH, InitArgs, run_with};

const PACKAGE: &str = r#"{
  "name": "shop",
  "scripts": { "start": "node server.js" }
}
"#;

const SERVER: &str = "const port = Number(process.env.PORT);\n\
    app.get('/health', (req, res) => res.send('ok'));\n";

/// A clone with one commit on `main`: a Node app with a lockfile and a start script.
fn clone() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let repo = dir.path().join("shop");
    fs::create_dir(&repo).expect("the clone directory");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Test Author"]);
    git(&repo, &["config", "user.email", "author@example.test"]);
    fs::write(repo.join("package.json"), PACKAGE).expect("package.json");
    fs::write(repo.join("package-lock.json"), "{}\n").expect("the lockfile");
    fs::write(repo.join("server.js"), SERVER).expect("the server");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "app"]);
    (dir, repo)
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn args(repo: &Path) -> InitArgs {
    InitArgs {
        repo: repo.to_path_buf(),
        base: None,
        branch: DEFAULT_BRANCH.to_owned(),
        working_tree: false,
        print: false,
        package: None,
        ready_path: None,
    }
}

fn yes(_: &str) -> bool {
    true
}

#[test]
fn the_contract_lands_on_a_new_branch_and_the_checkout_is_untouched() {
    let (_dir, repo) = clone();
    let main_before = git(&repo, &["rev-parse", "main"]);

    let text = run_with(&args(&repo), &mut yes).expect("init writes the branch");

    assert!(
        text.contains("acan/init"),
        "the message names the branch: {text}"
    );
    assert_eq!(git(&repo, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert_eq!(
        git(&repo, &["rev-parse", "main"]),
        main_before,
        "main did not move"
    );
    assert_eq!(
        git(&repo, &["status", "--porcelain"]),
        "",
        "the checkout is clean"
    );
    let files = git(
        &repo,
        &["ls-tree", "-r", "--name-only", "acan/init", "--", ".acan"],
    );
    assert_eq!(
        files,
        ".acan/.gitignore\n.acan/REVIEW.md\n.acan/app.json\n.acan/probes/core.json"
    );
    let app = git(&repo, &["show", "acan/init:.acan/app.json"]);
    assert!(
        app.contains("\"version\": \"acan-app/2\""),
        "node app: {app}"
    );
    assert!(app.contains("\"install\""), "the install phase is written");
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s", "acan/init"]),
        "Add the acan app contract (acan init)"
    );
}

#[test]
fn print_writes_nothing() {
    let (_dir, repo) = clone();
    let mut args = args(&repo);
    args.print = true;

    let text = run_with(&args, &mut |_| panic!("print never asks")).expect("the plan prints");

    assert!(
        text.contains("## .acan/app.json"),
        "the plan prints: {text}"
    );
    assert_eq!(git(&repo, &["branch", "--list", "acan/init"]), "");
}

#[test]
fn a_refused_confirmation_leaves_no_branch() {
    let (_dir, repo) = clone();

    let err = run_with(&args(&repo), &mut |_| false).expect_err("cancelled");

    assert!(err.to_string().contains("cancelled"), "{err}");
    assert_eq!(git(&repo, &["branch", "--list", "acan/init"]), "");
}

#[test]
fn an_existing_contract_or_branch_is_refused() {
    let (_dir, repo) = clone();
    run_with(&args(&repo), &mut yes).expect("the first write");
    let again = run_with(&args(&repo), &mut yes).expect_err("the branch exists");
    assert!(again.to_string().contains("already exists"), "{again}");

    fs::create_dir_all(repo.join(".acan")).expect(".acan");
    fs::write(repo.join(".acan/app.json"), "{}\n").expect("app.json");
    git(&repo, &["add", ".acan"]);
    git(&repo, &["commit", "-q", "-m", "contract"]);
    let mut fresh = args(&repo);
    fresh.branch = "acan/again".to_owned();
    let err = run_with(&fresh, &mut yes).expect_err("the base has a contract");
    assert!(err.to_string().contains("already has .acan/"), "{err}");
}

#[test]
fn a_detached_head_needs_a_base() {
    let (_dir, repo) = clone();
    let sha = git(&repo, &["rev-parse", "main"]);
    git(&repo, &["checkout", "-q", "--detach", &sha]);

    let err = run_with(&args(&repo), &mut yes).expect_err("detached");

    assert!(err.to_string().contains("detached HEAD"), "{err}");
}

#[test]
fn a_refused_detection_writes_nothing() {
    let (_dir, repo) = clone();
    fs::write(repo.join("pnpm-lock.yaml"), "lockfileVersion: 9\n").expect("the lockfile");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "pnpm"]);

    let err = run_with(&args(&repo), &mut |_| panic!("a refusal never asks")).expect_err("refused");

    assert!(err.to_string().contains("pnpm"), "{err}");
    assert_eq!(git(&repo, &["branch", "--list", "acan/init"]), "");
}

#[test]
fn working_tree_mode_writes_files_without_a_commit() {
    let (_dir, repo) = clone();
    let main_before = git(&repo, &["rev-parse", "main"]);
    let mut args = args(&repo);
    args.working_tree = true;

    let text = run_with(&args, &mut yes).expect("the files are written");

    assert!(text.contains("Nothing is committed"), "{text}");
    assert_eq!(git(&repo, &["rev-parse", "main"]), main_before);
    assert!(repo.join(".acan/app.json").is_file());
    assert!(git(&repo, &["status", "--porcelain"]).contains(".acan/"));
    let again = run_with(&args, &mut yes).expect_err("the files exist now");
    assert!(again.to_string().contains("already exists"), "{again}");
}

#[cfg(unix)]
#[test]
fn the_clones_hooks_and_filters_run_nothing_while_the_branch_is_written() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, repo) = clone();
    // The filter applies to every path. It is set up after the last commit the test makes, so
    // only acan's writes could run it.
    fs::write(repo.join(".gitattributes"), "* filter=mark\n").expect(".gitattributes");
    git(&repo, &["add", ".gitattributes"]);
    git(&repo, &["commit", "-q", "-m", "attributes"]);
    let hook_marker = dir.path().join("hook-ran");
    let filter_marker = dir.path().join("filter-ran");
    let hooks = dir.path().join("hooks");
    fs::create_dir(&hooks).expect("the hooks directory");
    for name in [
        "pre-commit",
        "post-commit",
        "post-checkout",
        "reference-transaction",
    ] {
        let script = hooks.join(name);
        fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", hook_marker.display()),
        )
        .expect("a hook");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("executable");
    }
    git(
        &repo,
        &["config", "core.hooksPath", &hooks.to_string_lossy()],
    );
    git(
        &repo,
        &[
            "config",
            "filter.mark.clean",
            &format!("sh -c 'touch \"{}\"; cat'", filter_marker.display()),
        ],
    );

    run_with(&args(&repo), &mut yes).expect("the branch is written");

    assert!(!hook_marker.exists(), "no hook of the clone ran");
    assert!(!filter_marker.exists(), "no clean filter of the clone ran");
    assert!(git(&repo, &["show", "acan/init:.acan/app.json"]).contains("acan-app/2"));
}

#[cfg(unix)]
#[test]
fn a_link_where_the_contract_goes_is_refused_and_nothing_is_written_through_it() {
    let (dir, repo) = clone();
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).expect("the outside directory");
    std::os::unix::fs::symlink(&outside, repo.join(".acan")).expect("a link named .acan");
    let mut args = args(&repo);
    args.working_tree = true;

    let err = run_with(&args, &mut yes).expect_err("a link is refused");

    assert!(err.to_string().contains("link or junction"), "{err}");
    assert_eq!(
        fs::read_dir(&outside)
            .expect("the outside directory")
            .count(),
        0,
        "nothing was written through the link"
    );
}

#[cfg(unix)]
#[test]
fn a_link_inside_acan_is_refused_before_any_file_is_written() {
    let (dir, repo) = clone();
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).expect("the outside directory");
    fs::create_dir(repo.join(".acan")).expect(".acan");
    std::os::unix::fs::symlink(&outside, repo.join(".acan/probes")).expect("a link named probes");
    let mut args = args(&repo);
    args.working_tree = true;

    let err = run_with(&args, &mut yes).expect_err("a link is refused");

    assert!(err.to_string().contains("link or junction"), "{err}");
    assert_eq!(
        fs::read_dir(&outside)
            .expect("the outside directory")
            .count(),
        0,
        "nothing was written through the link"
    );
    assert!(
        !repo.join(".acan/app.json").exists(),
        "no file was written before the refusal"
    );
}

#[cfg(unix)]
#[test]
fn a_dangling_link_where_a_file_goes_counts_as_existing() {
    let (dir, repo) = clone();
    let target = dir.path().join("created-by-link");
    fs::create_dir(repo.join(".acan")).expect(".acan");
    std::os::unix::fs::symlink(&target, repo.join(".acan/app.json")).expect("a dangling link");
    let mut args = args(&repo);
    args.working_tree = true;

    let err = run_with(&args, &mut yes).expect_err("the link is in the way");

    assert!(err.to_string().contains("already exists"), "{err}");
    assert!(!target.exists(), "the link was not followed");
}
