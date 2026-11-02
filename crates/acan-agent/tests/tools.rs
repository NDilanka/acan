//! The worker's local tools on a real git worktree. A write is allowed only to a declared file,
//! every path stays inside the worktree, a commit refuses a change outside the declaration, no
//! tool writes or commits a protected path even when it is declared, and the test command runs
//! only in a locked-down container, never on the host.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::{Duration, Instant};

use acan_agent::{Worktree, normalize_path};
use acan_intent::{ContainerPolicy, RepoPath};
use serde_json::{Value, json};

const DECLARED: &str = "model/store.ts";
const APP: &str = ".acan/app.json";
const APP_TEXT: &str = "{}\n";
const PROTECTED_TEST: &str = "tests/protected.test.ts";
const PROTECTED_TEST_TEXT: &str = "test('kept', () => {});\n";
/// The image the contract pins: `oven/bun:1.4.4` at the digest the H01 spike ran.
const PINNED_IMAGE: &str =
    "oven/bun:1.4.4@sha256:1d22548f177f4ff38e0c15baa3c2c46c6c2060ba2d1bc573bb4720b407548f1d";

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temporary repository");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test User"]);
    git(root, &["config", "user.email", "test@example.test"]);
    // Windows checkouts convert to CRLF by default, and the files are compared byte for byte.
    git(root, &["config", "core.autocrlf", "false"]);
    fs::create_dir_all(root.join("model")).expect("machine directory");
    fs::write(root.join(DECLARED), "original\n").expect("fixture");
    fs::write(root.join("model/other.ts"), "other\n").expect("fixture");
    fs::create_dir_all(root.join(".acan")).expect("contract directory");
    fs::write(root.join(APP), APP_TEXT).expect("fixture");
    fs::create_dir_all(root.join("tests")).expect("test directory");
    fs::write(root.join(PROTECTED_TEST), PROTECTED_TEST_TEXT).expect("fixture");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial"]);
    dir
}

/// Runs git in `dir` and returns its stdout. Panics with git's stderr when the command fails.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Process::new("git")
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
    String::from_utf8(output.stdout).expect("git prints UTF-8")
}

fn repo_paths(paths: &[&str]) -> Vec<RepoPath> {
    paths
        .iter()
        .map(|path| path.parse().expect("a repo path"))
        .collect()
}

fn worktree(root: &Path) -> Worktree {
    Worktree::new(root.to_path_buf(), &[DECLARED.to_owned()], None)
}

/// The protected-head snapshot a test command's container mounts protected paths from. It holds
/// the protected paths as `repo()` committed them, so it matches the base commit.
fn head_snapshot() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a protected-head snapshot");
    fs::create_dir_all(dir.path().join(".acan")).expect("snapshot directory");
    fs::write(dir.path().join(APP), APP_TEXT).expect("snapshot fixture");
    fs::create_dir_all(dir.path().join("tests")).expect("snapshot directory");
    fs::write(dir.path().join(PROTECTED_TEST), PROTECTED_TEST_TEXT).expect("snapshot fixture");
    dir
}

#[test]
fn a_write_is_allowed_only_to_a_declared_file() {
    let dir = repo();
    let tools = worktree(dir.path());
    let refused = tools
        .write_file("model/other.ts", "changed\n")
        .expect_err("an undeclared file is refused");
    assert!(
        refused.contains("not declared"),
        "the refusal names the rule: {refused}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("model/other.ts")).expect("readable"),
        "other\n",
        "the refused write left the file alone"
    );
    tools
        .write_file(DECLARED, "changed\n")
        .expect("the declared file is written");
    assert_eq!(
        fs::read_to_string(dir.path().join(DECLARED)).expect("readable"),
        "changed\n"
    );
}

#[tokio::test]
async fn paths_inside_the_worktree_are_read_listed_and_written() {
    let dir = repo();
    let tools = Worktree::new(
        dir.path().to_path_buf(),
        &[DECLARED.to_owned(), "model/new/fresh.ts".to_owned()],
        None,
    );
    assert_eq!(
        tools
            .read_file(DECLARED, 0)
            .await
            .expect("an inside file reads"),
        "original\n"
    );
    let listing = tools
        .list_files("model")
        .expect("an inside directory lists");
    assert!(listing.contains("store.ts"), "{listing}");
    tools
        .write_file("model/new/fresh.ts", "fresh\n")
        .expect("a declared file in a new directory is written");
    assert_eq!(
        fs::read_to_string(dir.path().join("model/new/fresh.ts")).expect("readable"),
        "fresh\n"
    );
}

/// The offset a cut page's marker names for the next page.
fn next_offset(marker: &str) -> u64 {
    let digits: String = marker
        .split("with offset ")
        .nth(1)
        .expect("the marker names the next offset")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().expect("the next offset is a whole number")
}

/// A file longer than one page comes back page by page: each cut page names the offset of the
/// next, the pages join back into the whole file, and no character is split between two pages.
#[tokio::test]
async fn a_long_file_reads_on_page_by_page() {
    let dir = repo();
    // Multi-byte characters sit on and across the page boundary, so a cut must move to a
    // character edge for the pages to join back into the file.
    let text: String = "ab€cdé\u{1F600}".repeat(9_000);
    fs::write(dir.path().join(DECLARED), &text).expect("the long file");
    let tools = worktree(dir.path());

    let mut joined = String::new();
    let mut offset = 0;
    let mut pages = 0;
    loop {
        let page = tools
            .read_file(DECLARED, offset)
            .await
            .expect("a page reads");
        pages += 1;
        if let Some(at) = page.find("\n[truncated: ") {
            assert!(
                at <= 32 * 1024,
                "a page is at most 32 KB of text, got {at} bytes"
            );
            joined.push_str(&page[..at]);
            offset = next_offset(&page[at..]);
        } else {
            joined.push_str(&page);
            break;
        }
    }
    assert!(
        pages >= 3,
        "a {} byte file takes several pages, got {pages}",
        text.len()
    );
    assert_eq!(joined, text, "the pages join back into the whole file");

    let end = u64::try_from(text.len()).expect("the length fits");
    assert_eq!(
        tools
            .read_file(DECLARED, end)
            .await
            .expect("an offset at the end reads nothing"),
        "",
        "the end of the file has no text"
    );
    assert!(
        tools.read_file(DECLARED, end + 1).await.is_err(),
        "an offset past the end is refused"
    );
}

/// An offset inside a multi-byte character starts the page at the next character, so the
/// text never begins with a broken character.
#[tokio::test]
async fn a_page_from_inside_a_character_starts_at_the_next_one() {
    let dir = repo();
    fs::write(dir.path().join(DECLARED), "€uro\n").expect("the fixture");
    let tools = worktree(dir.path());
    assert_eq!(
        tools.read_file(DECLARED, 1).await.expect("the page reads"),
        "uro\n"
    );
}

#[tokio::test]
async fn paths_outside_the_worktree_are_refused() {
    let dir = repo();
    let tools = worktree(dir.path());
    for path in [
        "../escape.txt",
        "/etc/passwd",
        "model/../../escape",
        ".git/config",
    ] {
        assert!(
            tools.read_file(path, 0).await.is_err(),
            "reading {path:?} is refused"
        );
        assert!(
            tools.write_file(path, "x").is_err(),
            "writing {path:?} is refused"
        );
    }
    assert!(
        tools.list_files("..").is_err(),
        "listing outside the worktree is refused"
    );
}

/// Creates a link at `at` to `target`. Windows needs the directory form for a directory, and a
/// Windows account without developer mode or elevation cannot create links at all.
fn link(target: &Path, at: &Path, is_dir: bool) {
    #[cfg(unix)]
    {
        let _ = is_dir;
        std::os::unix::fs::symlink(target, at).expect("the link is created");
    }
    #[cfg(windows)]
    {
        let made = if is_dir {
            std::os::windows::fs::symlink_dir(target, at)
        } else {
            std::os::windows::fs::symlink_file(target, at)
        };
        made.expect("the link is created (Windows needs developer mode or elevation for links)");
    }
}

/// A link inside the worktree that points outside it must not carry reads or writes out.
#[tokio::test]
async fn a_symlink_that_leaves_the_worktree_is_refused() {
    let dir = repo();
    let outside = tempfile::tempdir().expect("a directory outside the worktree");
    let secret = outside.path().join("secret.txt");
    fs::write(&secret, "outside\n").expect("the outside file");
    let link_path = "model/link.ts";
    link(&secret, &dir.path().join(link_path), false);
    let tools = Worktree::new(dir.path().to_path_buf(), &[link_path.to_owned()], None);

    assert!(
        tools.read_file(link_path, 0).await.is_err(),
        "reading through the link is refused"
    );
    assert!(
        tools.write_file(link_path, "changed\n").is_err(),
        "writing through the link is refused"
    );
    assert_eq!(
        fs::read_to_string(&secret).expect("readable"),
        "outside\n",
        "the outside file is untouched"
    );
}

/// A declared path that is a link to an undeclared file in the same worktree must not carry the
/// write to that file, and neither may a declared path under a linked directory.
#[test]
fn a_write_never_goes_through_a_link_even_inside_the_worktree() {
    let dir = repo();
    let root = dir.path();
    link(
        &root.join("model/other.ts"),
        &root.join("model/linked.ts"),
        false,
    );
    link(&root.join("model"), &root.join("alias"), true);
    let tools = Worktree::new(
        root.to_path_buf(),
        &["model/linked.ts".to_owned(), "alias/other.ts".to_owned()],
        None,
    );

    assert!(
        tools.write_file("model/linked.ts", "changed\n").is_err(),
        "a declared link is not written through"
    );
    assert!(
        tools.write_file("alias/other.ts", "changed\n").is_err(),
        "a declared path under a linked directory is not written through"
    );
    assert_eq!(
        fs::read_to_string(root.join("model/other.ts")).expect("readable"),
        "other\n",
        "the undeclared file is untouched"
    );
}

#[test]
fn a_path_has_one_form_to_the_origin_and_the_worktree() {
    assert_eq!(
        normalize_path("./model/store.ts").expect("a plain path"),
        DECLARED
    );
    assert_eq!(
        normalize_path("model//store.ts").expect("a plain path"),
        DECLARED
    );
    for bad in [
        "model/store.ts.",
        "model/store.ts ",
        "model/a:b",
        "./",
        ".GIT/config",
    ] {
        assert!(normalize_path(bad).is_err(), "accepted {bad:?}");
    }
}

/// A backslash is a separator on Windows and a name character elsewhere, so the form it takes
/// depends on the platform the worktree lives on. The origin only ever sees the `/` form.
#[test]
fn a_backslash_path_is_refused_or_read_as_a_separator_by_platform() {
    let backslash = normalize_path("model\\store.ts");
    if cfg!(windows) {
        assert_eq!(backslash.expect("a separator on Windows"), DECLARED);
    } else {
        assert!(backslash.is_err(), "a name the origin does not keep");
    }
}

#[test]
fn a_declared_alias_names_the_declared_file_for_writes() {
    let dir = repo();
    let tools = Worktree::new(
        dir.path().to_path_buf(),
        &["./model/store.ts".to_owned()],
        None,
    );
    tools
        .write_file("model/store.ts", "changed\n")
        .expect("the plain form of a declared alias is written");
    assert_eq!(
        fs::read_to_string(dir.path().join(DECLARED)).expect("readable"),
        "changed\n"
    );
}

#[tokio::test]
async fn a_commit_refuses_a_change_outside_the_declared_files() {
    let dir = repo();
    let root = dir.path();
    let tools = worktree(root);
    tools
        .write_file(DECLARED, "changed\n")
        .expect("the declared file is written");
    fs::write(root.join("model/other.ts"), "sneaked\n").expect("an undeclared change");

    let refused = tools
        .commit("change the store")
        .await
        .expect_err("the commit is refused");
    assert!(
        refused.contains("changed outside the declared files")
            && refused.contains("model/other.ts"),
        "the refusal names the file: {refused}"
    );
    assert_eq!(
        git(root, &["diff", "--cached", "--name-only"]),
        "",
        "the refused change is not staged"
    );
    assert_eq!(
        git(root, &["rev-parse", "HEAD"]).trim(),
        git(root, &["rev-parse", "main"]).trim(),
        "the refused commit did not move HEAD"
    );
}

#[tokio::test]
async fn a_commit_of_only_declared_changes_moves_head() {
    let dir = repo();
    let root = dir.path();
    let tools = worktree(root);
    tools
        .write_file(DECLARED, "changed\n")
        .expect("the declared file is written");

    let head = tools
        .commit("change the store")
        .await
        .expect("the declared change commits");
    assert_eq!(
        git(root, &["rev-parse", "HEAD"]).trim(),
        head,
        "the returned commit is HEAD"
    );
    assert_eq!(
        git(root, &["show", "--name-only", "--format=", "HEAD"]).trim(),
        DECLARED,
        "the commit carries only the declared file"
    );
}

#[tokio::test]
async fn run_tests_needs_a_configured_command() {
    let dir = repo();
    let tools = worktree(dir.path());
    let refused = tools
        .run_tests()
        .await
        .expect_err("no test command is configured");
    assert!(refused.contains("no test command"), "{refused}");
}

/// A stand-in docker CLI: `docker version` exits `version_exit`, and any other call except
/// `kill` records that it ran in `ran`, runs its body (shell on unix, batch on Windows), and
/// exits `run_exit`.
fn fake_docker_with(
    dir: &Path,
    version_exit: u8,
    run_exit: u8,
    unix_body: &str,
    windows_body: &str,
) -> (PathBuf, PathBuf) {
    let ran = dir.join("ran.txt");
    #[cfg(windows)]
    {
        let _ = unix_body;
        let docker = dir.join("docker.cmd");
        fs::write(
            &docker,
            format!(
                "@echo off\r\nif \"%1\"==\"version\" exit /b {version_exit}\r\nif \"%1\"==\"kill\" exit /b 0\r\necho ran> \"{}\"\r\n{windows_body}\r\nexit /b {run_exit}\r\n",
                ran.display()
            ),
        )
        .expect("the fake docker");
        (docker, ran)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = windows_body;
        let docker = dir.join("docker");
        fs::write(
            &docker,
            format!(
                "#!/bin/sh\nif [ \"$1\" = version ]; then exit {version_exit}; fi\nif [ \"$1\" = kill ]; then exit 0; fi\necho ran > '{}'\n{unix_body}\nexit {run_exit}\n",
                ran.display()
            ),
        )
        .expect("the fake docker");
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).expect("executable");
        (docker, ran)
    }
}

/// [`fake_docker_with`] with an empty run body.
fn fake_docker(dir: &Path, version_exit: u8, run_exit: u8) -> (PathBuf, PathBuf) {
    fake_docker_with(dir, version_exit, run_exit, "", "")
}

/// A test command that writes a file in the worktree and one under `.acan/`.
const WRITING_COMMAND: &str = "echo pwned > pwned.txt && echo pwned > .acan/pwned.txt";

fn assert_never_ran_on_the_host(root: &Path) {
    assert!(
        !root.join("pwned.txt").exists() && !root.join(".acan/pwned.txt").exists(),
        "the test command did not run on the host"
    );
}

#[tokio::test]
async fn run_tests_without_docker_is_isolation_unavailable_and_never_runs_on_the_host() {
    let dir = repo();
    let root = dir.path();
    let elsewhere = tempfile::tempdir().expect("a directory without docker");
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some(WRITING_COMMAND.to_owned()))
        .with_snapshot(head.path())
        .with_docker(elsewhere.path().join("no-such-docker"));
    let refused = tools
        .run_tests()
        .await
        .expect_err("no docker means no test run");
    assert!(refused.starts_with("isolation unavailable"), "{refused}");
    assert_never_ran_on_the_host(root);
}

#[tokio::test]
async fn run_tests_with_the_docker_daemon_down_is_isolation_unavailable() {
    let dir = repo();
    let root = dir.path();
    let bin = tempfile::tempdir().expect("a directory for the fake docker");
    let (docker, ran) = fake_docker(bin.path(), 1, 0);
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some(WRITING_COMMAND.to_owned()))
        .with_snapshot(head.path())
        .with_docker(docker);
    let refused = tools
        .run_tests()
        .await
        .expect_err("a daemon that does not answer means no test run");
    assert!(
        refused.starts_with("isolation unavailable") && refused.contains("not reachable"),
        "{refused}"
    );
    assert!(!ran.exists(), "no container was started");
    assert_never_ran_on_the_host(root);
}

#[tokio::test]
async fn run_tests_reports_the_container_exit_as_pass_fail_or_unavailable() {
    let dir = repo();
    let root = dir.path();
    let head = head_snapshot();
    for (run_exit, expected) in [
        (0, Ok("pass")),
        (3, Ok("fail: exit code 3")),
        (125, Err("isolation unavailable")),
    ] {
        let bin = tempfile::tempdir().expect("a directory for the fake docker");
        let (docker, ran) = fake_docker(bin.path(), 0, run_exit);
        let tools = Worktree::new(root.to_path_buf(), &[], Some(WRITING_COMMAND.to_owned()))
            .with_snapshot(head.path())
            .with_docker(docker);
        let outcome = tools.run_tests().await;
        assert!(
            ran.exists(),
            "exit {run_exit}: the container run was asked for"
        );
        match (&outcome, expected) {
            (Ok(text), Ok(start)) | (Err(text), Err(start)) => {
                assert!(text.starts_with(start), "exit {run_exit}: {text}");
            }
            _ => panic!("exit {run_exit}: expected {expected:?}, got {outcome:?}"),
        }
    }
    assert_never_ran_on_the_host(root);
}

#[test]
fn the_test_container_is_locked_down_with_protected_paths_read_only() {
    let dir = repo();
    let root = dir.path();
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_protected(&repo_paths(&[PROTECTED_TEST, "probes"]))
        .with_snapshot(head.path());
    let host = std::path::absolute(root).expect("an absolute worktree");
    let snapshot = std::path::absolute(head.path()).expect("an absolute snapshot");
    let read_only = |source: PathBuf, target: &str| {
        format!(
            "type=bind,source={},target={target},readonly",
            source.display()
        )
    };
    // `.acan` and the protected test come from the snapshot, never the worktree. The missing
    // `probes` is masked: an empty read-only directory from the mask source.
    let mask = std::env::temp_dir().join("acan-test-1-mask");
    let expected: Vec<String> = [
        "run",
        "--rm",
        "--name",
        "acan-test-1",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        "1000:1000",
        "--cpus",
        "2",
        "--memory",
        "1073741824",
        "--memory-swap",
        "1073741824",
        "--pids-limit",
        "256",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=268435456",
        "--env",
        "HOME=/tmp",
        "--env",
        "PORT=3000",
        "--mount",
        &format!("type=bind,source={},target=/work", host.display()),
        "--mount",
        &read_only(host.join(".git"), "/work/.git"),
        "--mount",
        &read_only(snapshot.join(".acan"), "/work/.acan"),
        "--mount",
        &read_only(mask, "/work/probes"),
        "--mount",
        &read_only(
            snapshot.join("tests").join("protected.test.ts"),
            "/work/tests/protected.test.ts",
        ),
        "--workdir",
        "/work",
        "--entrypoint",
        "sh",
        PINNED_IMAGE,
        "-c",
        "bun test",
    ]
    .map(str::to_owned)
    .to_vec();
    assert_eq!(
        tools
            .test_container_args("acan-test-1")
            .expect("the container arguments"),
        expected,
        "the default policy's exact docker arguments"
    );
}

#[test]
fn the_test_container_refuses_to_mount_a_protected_link() {
    let dir = repo();
    let root = dir.path();
    let outside = tempfile::tempdir().expect("a directory outside the worktree");
    link(outside.path(), &root.join("probes"), true);
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_protected(&repo_paths(&["probes"]))
        .with_snapshot(head.path());
    let refused = tools
        .test_container_args("acan-test-1")
        .expect_err("a protected link is not mounted");
    assert!(refused.contains("link"), "{refused}");
}

#[tokio::test]
async fn restore_brings_back_a_tracked_file_and_removes_an_untracked_one() {
    let dir = repo();
    let root = dir.path();
    let new = "model/new.ts";
    let tools = Worktree::new(
        root.to_path_buf(),
        &[DECLARED.to_owned(), new.to_owned()],
        None,
    );
    tools
        .write_file(DECLARED, "changed\n")
        .expect("the tracked file is written");
    tools
        .write_file(new, "new\n")
        .expect("the new file is written");

    tools
        .restore(&[DECLARED.to_owned(), new.to_owned()])
        .await
        .expect("both files are restored");

    assert_eq!(
        fs::read_to_string(root.join(DECLARED)).expect("the file is readable"),
        "original\n",
        "the tracked file is back at its committed text"
    );
    assert!(!root.join(new).exists(), "the untracked file is removed");
}

#[test]
fn declare_replaces_the_files_a_write_may_touch() {
    let dir = repo();
    let mut tools = worktree(dir.path());
    tools.declare(&["model/other.ts".to_owned()]);
    tools
        .write_file(DECLARED, "changed\n")
        .expect_err("the old declaration no longer allows the write");
    tools
        .write_file("model/other.ts", "changed\n")
        .expect("the new declaration allows the write");
}

/// `.acan/` is protected under every spelling the tools accept, even when the task declares
/// it, and every spelling the tools do not accept is refused before it reaches the disk.
#[test]
fn a_protected_path_is_never_written_even_when_declared() {
    let dir = repo();
    let root = dir.path();
    let mut protected_spellings = vec![
        ".acan/app.json",
        "./.acan/app.json",
        ".acan//app.json",
        ".ACAN/app.json",
        ".Acan/App.JSON",
        ".acan/new.json",
        ".acan",
    ];
    if cfg!(windows) {
        protected_spellings.extend([".acan\\app.json", ".\\.acan/app.json"]);
    }
    let refused_spellings = [
        "model/../.acan/app.json",
        ".acan/../.acan/app.json",
        "model\\..\\.acan\\app.json",
        ".acan./app.json",
        ".acan /app.json",
        ".acan:stream",
        ".acan/app.json::$DATA",
    ];
    let declared: Vec<String> = protected_spellings
        .iter()
        .chain(&refused_spellings)
        .map(|path| (*path).to_owned())
        .collect();
    let tools = Worktree::new(root.to_path_buf(), &declared, None);

    for path in protected_spellings {
        let refused = tools
            .write_file(path, "pwned\n")
            .expect_err("a protected path is refused");
        assert!(refused.contains("protected"), "{path:?}: {refused}");
    }
    for path in refused_spellings {
        assert!(
            tools.write_file(path, "pwned\n").is_err(),
            "writing {path:?} is refused"
        );
    }
    assert_eq!(
        fs::read_to_string(root.join(APP)).expect("readable"),
        APP_TEXT,
        "the protected file is untouched"
    );
    assert!(
        !root.join(".acan/new.json").exists(),
        "no protected file was created"
    );
}

#[test]
fn an_extra_protected_path_covers_itself_and_everything_under_it() {
    let dir = repo();
    let root = dir.path();
    let declared = [
        PROTECTED_TEST,
        "tests/protected.test.tsx",
        "probes/core.json",
        "probes.md",
    ]
    .map(str::to_owned);
    let tools = Worktree::new(root.to_path_buf(), &declared, None)
        .with_protected(&repo_paths(&[PROTECTED_TEST, "probes"]));

    for path in [
        PROTECTED_TEST,
        "TESTS/Protected.Test.ts",
        "probes/core.json",
    ] {
        let refused = tools
            .write_file(path, "pwned\n")
            .expect_err("a protected path is refused");
        assert!(refused.contains("protected"), "{path:?}: {refused}");
    }
    for path in ["tests/protected.test.tsx", "probes.md"] {
        tools
            .write_file(path, "fine\n")
            .expect("a sibling with a longer name is not protected");
    }
    assert_eq!(
        fs::read_to_string(root.join(PROTECTED_TEST)).expect("readable"),
        PROTECTED_TEST_TEXT
    );
    assert!(
        !root.join("probes").exists(),
        "no protected file was created"
    );
}

/// A declared path under a link or junction to `.acan/` names a protected file by another name.
#[test]
fn a_link_or_junction_into_a_protected_directory_is_refused() {
    let dir = repo();
    let root = dir.path();
    link(&root.join(".acan"), &root.join("model/settings"), true);
    let declared = vec!["model/settings/app.json".to_owned()];
    #[cfg(windows)]
    let declared = {
        let made = Process::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("model").join("junction"))
            .arg(root.join(".acan"))
            .output()
            .expect("cmd runs");
        assert!(made.status.success(), "the junction is created");
        let mut declared = declared;
        declared.push("model/junction/app.json".to_owned());
        declared
    };
    let tools = Worktree::new(root.to_path_buf(), &declared, None);
    for path in &declared {
        assert!(
            tools.write_file(path, "pwned\n").is_err(),
            "writing {path:?} is refused"
        );
    }
    assert_eq!(
        fs::read_to_string(root.join(APP)).expect("readable"),
        APP_TEXT,
        "the protected file is untouched"
    );
}

/// Windows gives `.acan` a short name such as `ACAN~1`, which reaches the same directory.
#[cfg(windows)]
#[test]
fn a_short_name_for_a_protected_directory_is_refused() {
    use std::os::windows::process::CommandExt as _;

    let dir = repo();
    let root = dir.path();
    let listed = Process::new("cmd")
        .raw_arg("/C for %I in (.acan) do @echo %~snxI")
        .current_dir(root)
        .output()
        .expect("cmd runs");
    let short = String::from_utf8_lossy(&listed.stdout).trim().to_owned();
    if short.is_empty() || short.eq_ignore_ascii_case(".acan") {
        eprintln!("this volume makes no short names; nothing to check");
        return;
    }
    let path = format!("{short}/app.json");
    let tools = Worktree::new(root.to_path_buf(), std::slice::from_ref(&path), None);
    let refused = tools
        .write_file(&path, "pwned\n")
        .expect_err("the short name is refused");
    assert!(refused.contains("protected"), "{path}: {refused}");
    assert_eq!(
        fs::read_to_string(root.join(APP)).expect("readable"),
        APP_TEXT,
        "the protected file is untouched"
    );
}

async fn assert_commit_refused(root: &Path, tools: &Worktree, names: &str) {
    let refused = tools
        .commit("touch the contract")
        .await
        .expect_err("the commit is refused");
    assert!(
        refused.contains(&format!("{names} is protected")),
        "the refusal names the protected path: {refused}"
    );
    assert_eq!(
        git(root, &["diff", "--cached", "--name-only"]),
        "",
        "the refused change is not staged"
    );
    assert_eq!(
        git(root, &["rev-parse", "HEAD"]).trim(),
        git(root, &["rev-parse", "main"]).trim(),
        "the refused commit did not move HEAD"
    );
}

#[tokio::test]
async fn a_commit_refuses_a_changed_protected_file_even_when_declared() {
    let dir = repo();
    let root = dir.path();
    fs::write(root.join(APP), "{\"pwned\": true}\n").expect("a change from outside the tools");
    let tools = Worktree::new(root.to_path_buf(), &[APP.to_owned()], None);
    assert_commit_refused(root, &tools, APP).await;
}

#[tokio::test]
async fn a_commit_refuses_deleting_a_protected_file() {
    let dir = repo();
    let root = dir.path();
    fs::remove_file(root.join(APP)).expect("a deletion from outside the tools");
    let tools = Worktree::new(root.to_path_buf(), &[APP.to_owned()], None);
    assert_commit_refused(root, &tools, APP).await;
}

/// git pairs the deleted protected file with the new declared one as a rename, and a rename
/// lists only its new name unless rename detection is off.
#[tokio::test]
async fn a_commit_refuses_renaming_a_protected_file_to_a_declared_path() {
    let dir = repo();
    let root = dir.path();
    fs::rename(root.join(APP), root.join("model/moved.json")).expect("a rename");
    let tools = Worktree::new(root.to_path_buf(), &["model/moved.json".to_owned()], None);
    assert_commit_refused(root, &tools, APP).await;
}

#[tokio::test]
async fn a_commit_refuses_a_change_to_an_extra_protected_path() {
    let dir = repo();
    let root = dir.path();
    fs::write(root.join(PROTECTED_TEST), "test('gone', () => {});\n").expect("a change");
    let tools = Worktree::new(root.to_path_buf(), &[PROTECTED_TEST.to_owned()], None)
        .with_protected(&repo_paths(&[PROTECTED_TEST]));
    assert_commit_refused(root, &tools, PROTECTED_TEST).await;
}

#[tokio::test]
async fn restore_leaves_a_protected_path_alone() {
    let dir = repo();
    let root = dir.path();
    let local = ".acan/local.json";
    fs::write(root.join(local), "{}\n").expect("an untracked protected file");
    let tools = Worktree::new(root.to_path_buf(), &[local.to_owned()], None);
    tools
        .restore(&[local.to_owned()])
        .await
        .expect("restore succeeds");
    assert!(
        root.join(local).exists(),
        "the protected file is not removed"
    );
}

const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The contract's container policy fixture with `edit` applied. The result is decoded by the
/// contract's own decoder, so an invalid value is refused here too.
fn policy_with(edit: impl FnOnce(&mut Value)) -> ContainerPolicy {
    let mut value: Value = serde_json::from_str(include_str!(
        "../../acan-intent/fixtures/container-policy.json"
    ))
    .expect("the contract policy fixture");
    edit(&mut value);
    serde_json::from_value(value).expect("a container policy")
}

/// A read-only bind mount of `source` at `target`, as the test command's container gets it.
fn ro_mount(source: &Path, target: &str) -> String {
    format!(
        "type=bind,source={},target={target},readonly",
        source.display()
    )
}

/// Whether a mask directory this process made is still in the temporary directory.
fn no_mask_left_behind() -> bool {
    let prefix = format!("acan-test-{}-", std::process::id());
    fs::read_dir(std::env::temp_dir())
        .expect("the temporary directory")
        .filter_map(Result::ok)
        .all(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            !(name.starts_with(&prefix) && name.ends_with("-mask"))
        })
}

#[test]
fn the_default_policy_is_the_contract_fixture_and_pins_the_image() {
    let dir = repo();
    let tools = worktree(dir.path());
    assert_eq!(
        *tools.container_policy(),
        policy_with(|_| {}),
        "every limit, mount and variable is the contract's"
    );
    assert_eq!(
        *tools.container_policy(),
        ContainerPolicy::default_run(),
        "the runtime default is the fixture"
    );
    let image = tools.container_policy().image.clone();
    assert_eq!(
        image.digest.as_ref().map(ToString::to_string).as_deref(),
        PINNED_IMAGE.split('@').nth(1),
        "the default run pins the digest the spike ran"
    );
}

#[test]
fn the_policy_cannot_name_a_published_port() {
    let refused = serde_json::from_value::<ContainerPolicy>({
        let mut value: Value = serde_json::from_str(include_str!(
            "../../acan-intent/fixtures/container-policy.json"
        ))
        .expect("the contract policy fixture");
        value["network"] = json!({ "kind": "loopback_published", "port": 3000 });
        value
    })
    .expect_err("a published port does not decode");
    assert!(
        refused.to_string().contains("loopback_published"),
        "{refused}"
    );
}

#[test]
fn a_non_default_policy_renders_its_own_limits_into_the_arguments() {
    let dir = repo();
    let root = dir.path();
    let policy = policy_with(|v| {
        v["image"]["name"] = json!("oven/bun:1.2");
        v["image"]["digest"] = json!(DIGEST);
        v["user"] = json!("2000:2000");
        v["cpus_milli"] = json!(1500);
        v["memory_bytes"] = json!(536_870_912);
        v["pids"] = json!(128);
        v["timeout_ms"] = json!(60_000);
        v["tmp_bytes"] = json!(67_108_864);
        v["mounts"]["workdir"] = json!("/app");
        v["mounts"]["protected"] = json!([".acan", "src/limits.ts"]);
    });
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_policy(policy)
        .with_snapshot(head.path());
    let host = std::path::absolute(root).expect("an absolute worktree");
    let snapshot = std::path::absolute(head.path()).expect("an absolute snapshot");
    let mask = std::env::temp_dir().join("acan-test-2-mask");
    let expected: Vec<String> = [
        "run",
        "--rm",
        "--name",
        "acan-test-2",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        "2000:2000",
        "--cpus",
        "1.5",
        "--memory",
        "536870912",
        "--memory-swap",
        "536870912",
        "--pids-limit",
        "128",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=67108864",
        "--env",
        "HOME=/tmp",
        "--env",
        "PORT=3000",
        "--mount",
        &format!("type=bind,source={},target=/app", host.display()),
        "--mount",
        &ro_mount(&host.join(".git"), "/app/.git"),
        "--mount",
        &ro_mount(&snapshot.join(".acan"), "/app/.acan"),
        "--mount",
        &ro_mount(&mask, "/app/src/limits.ts"),
        "--workdir",
        "/app",
        "--entrypoint",
        "sh",
        &format!("oven/bun:1.2@{DIGEST}"),
        "-c",
        "bun test",
    ]
    .map(str::to_owned)
    .to_vec();
    assert_eq!(
        tools
            .test_container_args("acan-test-2")
            .expect("the container arguments"),
        expected,
        "a non-default policy's exact docker arguments"
    );
}

#[test]
fn readonly_git_decides_whether_git_is_mounted_read_only() {
    let dir = repo();
    let root = dir.path();
    let host = std::path::absolute(root).expect("an absolute worktree");
    let git_mount = ro_mount(&host.join(".git"), "/work/.git");
    let head = head_snapshot();
    let args_with = |readonly_git: bool| {
        Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
            .with_policy(policy_with(|v| {
                v["mounts"]["readonly_git"] = json!(readonly_git);
            }))
            .with_snapshot(head.path())
            .test_container_args("acan-test-6")
            .expect("the container arguments")
    };
    let guarded = args_with(true);
    assert!(
        guarded.contains(&git_mount),
        "readonly_git true left .git writable: {guarded:?}"
    );
    let open = args_with(false);
    assert!(
        !open.contains(&git_mount),
        "readonly_git false still mounted .git read-only: {open:?}"
    );
    let snapshot = std::path::absolute(head.path()).expect("an absolute snapshot");
    assert!(
        open.contains(&ro_mount(&snapshot.join(".acan"), "/work/.acan")),
        "readonly_git false dropped the protected .acan mount: {open:?}"
    );
}

#[test]
fn a_protected_path_under_git_is_refused_while_the_git_guard_is_on() {
    let dir = repo();
    let head = head_snapshot();
    let args_with = |protected: &str, readonly_git: bool| {
        Worktree::new(dir.path().to_path_buf(), &[], Some("bun test".to_owned()))
            .with_policy(policy_with(|v| {
                v["mounts"]["protected"] = json!([".acan", protected]);
                v["mounts"]["readonly_git"] = json!(readonly_git);
            }))
            .with_snapshot(head.path())
            .test_container_args("acan-test-7")
    };
    for protected in [".git", ".git/hooks", ".GIT/config"] {
        let refused = args_with(protected, true).expect_err("a protected path under .git");
        assert!(refused.contains("lies under .git"), "{refused}");
    }
    // With the guard off, `.git` is not mounted from the worktree at all: the protected path is
    // covered from the snapshot, which lacks it, so an empty mask stands in for it.
    let open = args_with(".git/hooks", false).expect("the guard is off");
    let worktree_git = std::path::absolute(dir.path())
        .expect("an absolute worktree")
        .join(".git");
    let from_worktree = format!("source={},", worktree_git.display());
    assert!(
        !open.iter().any(|arg| arg.contains(&from_worktree)),
        "the worktree's .git was mounted: {open:?}"
    );
}

#[tokio::test]
async fn a_missing_protected_path_is_refused_when_the_policy_says_so() {
    let dir = repo();
    let root = dir.path();
    let bin = tempfile::tempdir().expect("a directory for the fake docker");
    let (docker, ran) = fake_docker(bin.path(), 0, 0);
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_protected(&repo_paths(&["probes"]))
        .with_policy(policy_with(|v| {
            v["mounts"]["missing_protected"] = json!("refused");
        }))
        .with_snapshot(head.path())
        .with_docker(docker);
    let refused = tools
        .test_container_args("acan-test-3")
        .expect_err("a missing protected path is refused");
    assert!(
        refused.contains("probes") && refused.contains("refuses"),
        "{refused}"
    );
    let refused = tools.run_tests().await.expect_err("the run is refused");
    assert!(
        refused.starts_with("isolation unavailable") && refused.contains("probes"),
        "{refused}"
    );
    assert!(!ran.exists(), "no container was started");
    assert_never_ran_on_the_host(root);
}

#[tokio::test]
async fn a_run_without_a_snapshot_never_starts_a_container() {
    let dir = repo();
    let root = dir.path();
    let bin = tempfile::tempdir().expect("a directory for the fake docker");
    let (docker, ran) = fake_docker(bin.path(), 0, 0);
    let tools = Worktree::new(root.to_path_buf(), &[], Some(WRITING_COMMAND.to_owned()))
        .with_docker(docker);
    let refused = tools
        .test_container_args("acan-test-4")
        .expect_err("no snapshot means no container arguments");
    assert!(refused.contains("snapshot"), "{refused}");
    let refused = tools.run_tests().await.expect_err("the run is refused");
    assert!(
        refused.starts_with("isolation unavailable") && refused.contains("snapshot"),
        "{refused}"
    );
    assert!(!ran.exists(), "no container was started");
    assert_never_ran_on_the_host(root);
}

#[tokio::test]
async fn the_policy_timeout_cuts_a_test_run_that_outlives_it() {
    let dir = repo();
    let root = dir.path();
    let bin = tempfile::tempdir().expect("a directory for the fake docker");
    let (docker, _ran) = fake_docker_with(bin.path(), 0, 0, "sleep 5", "ping -n 6 127.0.0.1 > nul");
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_policy(policy_with(|v| v["timeout_ms"] = json!(500)))
        .with_snapshot(head.path())
        .with_docker(docker);
    let started = Instant::now();
    let timed_out = tools
        .run_tests()
        .await
        .expect_err("the run outlives its timeout");
    assert!(timed_out.contains("timed out after 500ms"), "{timed_out}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the run was cut at the policy timeout"
    );
}

#[tokio::test]
async fn a_masked_path_leaves_no_empty_host_directory_and_keeps_what_the_run_wrote() {
    let dir = repo();
    let root = dir.path();
    let bin = tempfile::tempdir().expect("a directory for the fake docker");
    let limits = root.join("src").join("limits.ts");
    let candidate = root.join("src").join("candidate.ts");
    // The fake stands in for docker: it creates the masked path's mount point on the host, and
    // the candidate writes a file beside it.
    let (docker, _ran) = fake_docker_with(
        bin.path(),
        0,
        0,
        &format!(
            "mkdir -p '{}' && echo candidate > '{}'",
            limits.display(),
            candidate.display()
        ),
        &format!(
            "mkdir \"{}\" && echo candidate> \"{}\"",
            limits.display(),
            candidate.display()
        ),
    );
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_protected(&repo_paths(&["src/limits.ts"]))
        .with_snapshot(head.path())
        .with_docker(docker);
    let outcome = tools.run_tests().await.expect("the run completes");
    assert_eq!(outcome, "pass");
    assert!(candidate.exists(), "a file the run wrote is kept");
    assert!(!limits.exists(), "the empty mount point is removed");
    assert!(no_mask_left_behind(), "the empty mask directory is removed");
}

#[test]
fn a_masked_path_under_a_link_is_refused() {
    let dir = repo();
    let root = dir.path();
    let outside = tempfile::tempdir().expect("a directory outside the worktree");
    link(outside.path(), &root.join("linkdir"), true);
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_protected(&repo_paths(&["linkdir/new.txt"]))
        .with_snapshot(head.path());
    let refused = tools
        .test_container_args("acan-test-5")
        .expect_err("a masked path under a link is refused");
    assert!(refused.contains("link"), "{refused}");
}

#[test]
fn a_path_the_policy_protects_is_refused_even_when_declared() {
    let dir = repo();
    let root = dir.path();
    let policy = policy_with(|v| v["mounts"]["protected"] = json!([".acan", DECLARED]));
    let tools = Worktree::new(root.to_path_buf(), &[DECLARED.to_owned()], None).with_policy(policy);
    let refused = tools
        .write_file(DECLARED, "changed\n")
        .expect_err("a policy-protected path is refused");
    assert!(refused.contains("protected"), "{refused}");
    assert_eq!(
        fs::read_to_string(root.join(DECLARED)).expect("the file"),
        "original\n"
    );
}

/// The arguments of `--env` in `args`, in order.
fn env_values(args: &[String]) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == "--env")
        .map(|pair| pair[1].clone())
        .collect()
}

#[test]
fn a_protected_file_edited_in_the_worktree_is_not_what_the_container_sees() {
    let dir = repo();
    let root = dir.path();
    fs::write(root.join(APP), "{\"pwned\": true}\n").expect("an edit in the worktree");
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_snapshot(head.path());
    let args = tools
        .test_container_args("acan-test-7")
        .expect("the container arguments");
    let host = std::path::absolute(root).expect("an absolute worktree");
    let snapshot = std::path::absolute(head.path()).expect("an absolute snapshot");
    assert!(
        args.contains(&ro_mount(&snapshot.join(".acan"), "/work/.acan")),
        "the protected directory is mounted from the snapshot: {args:?}"
    );
    let worktree_copy = host.join(".acan").display().to_string();
    assert!(
        !args.iter().any(|arg| arg.contains(&worktree_copy)),
        "no argument names the worktree's copy of .acan: {args:?}"
    );
    assert_eq!(
        fs::read_to_string(snapshot.join(APP)).expect("the snapshot copy"),
        APP_TEXT,
        "the snapshot holds the base version the container reads"
    );
}

#[test]
fn a_protected_path_the_snapshot_lacks_is_masked_not_read_from_the_worktree() {
    let dir = repo();
    let root = dir.path();
    let added = "tests/added.test.ts";
    fs::write(root.join(added), "test('mine', () => {});\n")
        .expect("a protected test the task added");
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_protected(&repo_paths(&[added]))
        .with_snapshot(head.path());
    let args = tools
        .test_container_args("acan-test-8")
        .expect("the container arguments");
    let host = std::path::absolute(root).expect("an absolute worktree");
    let mask = std::env::temp_dir().join("acan-test-8-mask");
    assert!(
        args.contains(&ro_mount(&mask, "/work/tests/added.test.ts")),
        "the copy the task added is masked: {args:?}"
    );
    let worktree_copy = host
        .join("tests")
        .join("added.test.ts")
        .display()
        .to_string();
    assert!(
        !args.iter().any(|arg| arg.contains(&worktree_copy)),
        "the worktree's copy is never mounted: {args:?}"
    );
}

#[tokio::test]
async fn a_snapshot_file_the_worktree_lacks_leaves_no_empty_file_behind() {
    let dir = repo();
    let root = dir.path();
    let bin = tempfile::tempdir().expect("a directory for the fake docker");
    let head = head_snapshot();
    fs::write(
        head.path().join("tests").join("gone.test.ts"),
        "test('gone', () => {});\n",
    )
    .expect("a snapshot file the worktree lacks");
    let gone = root.join("tests").join("gone.test.ts");
    // The fake stands in for docker, which makes the mount point of a file it mounts.
    let (docker, _ran) = fake_docker_with(
        bin.path(),
        0,
        0,
        &format!("touch '{}'", gone.display()),
        &format!("type nul > \"{}\"", gone.display()),
    );
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_protected(&repo_paths(&["tests/gone.test.ts"]))
        .with_snapshot(head.path())
        .with_docker(docker);
    let outcome = tools.run_tests().await.expect("the run completes");
    assert_eq!(outcome, "pass");
    assert!(
        !gone.exists(),
        "the empty mount point docker made is removed"
    );
}

#[test]
fn the_environment_is_exactly_what_the_policy_names() {
    let dir = repo();
    let root = dir.path();
    let head = head_snapshot();
    let tools = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_snapshot(head.path());
    assert_eq!(
        env_values(
            &tools
                .test_container_args("acan-test-9")
                .expect("the arguments")
        ),
        ["HOME=/tmp", "PORT=3000"],
        "the default run passes exactly the variables the fixture names"
    );
    let only_home = Worktree::new(root.to_path_buf(), &[], Some("bun test".to_owned()))
        .with_policy(policy_with(|v| {
            v["environment"] = json!({ "HOME": "/tmp" })
        }))
        .with_snapshot(head.path());
    let args = only_home
        .test_container_args("acan-test-9")
        .expect("the arguments");
    assert_eq!(env_values(&args), ["HOME=/tmp"]);
    assert!(
        !args.iter().any(|arg| arg.contains("PORT=")),
        "a variable outside the policy's list never reaches the arguments: {args:?}"
    );
}

#[test]
fn a_credential_variable_is_refused_by_the_decoder_and_before_any_container_starts() {
    let mut value: Value = serde_json::from_str(include_str!(
        "../../acan-intent/fixtures/container-policy.json"
    ))
    .expect("the contract policy fixture");
    value["environment"] = json!({ "OPENAI_API_KEY": "sk-test" });
    let decoded =
        serde_json::from_value::<ContainerPolicy>(value).expect_err("decoding refuses it");
    assert!(decoded.to_string().contains("credential"), "{decoded}");

    // A policy built in code skips the decoder, so the runner checks the names again.
    let dir = repo();
    let head = head_snapshot();
    let mut policy = policy_with(|_| {});
    policy
        .environment
        .insert("OPENAI_API_KEY".to_owned(), "sk-test".to_owned());
    let tools = Worktree::new(dir.path().to_path_buf(), &[], Some("bun test".to_owned()))
        .with_policy(policy)
        .with_snapshot(head.path());
    let refused = tools
        .test_container_args("acan-test-10")
        .expect_err("a credential name is refused");
    assert!(refused.contains("credential"), "{refused}");
}
