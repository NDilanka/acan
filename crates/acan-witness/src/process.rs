//! Test-command checks: run only in the pinned container, with a timeout and a bounded tail.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use acan_core::WitnessOutcome;
use acan_intent::ContainerPolicy;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{Limits, docker, driver};

/// How long to wait for piped output to drain after the command exits or is killed. A
/// detached grandchild can hold a pipe open, so the drain is bounded.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Runs `command` inside Docker. Missing Docker or an image is an error, never host execution.
pub(crate) async fn run_command(command: &str, workdir: &Path, limits: &Limits) -> WitnessOutcome {
    run_with(Path::new("docker"), command, workdir, limits).await
}

async fn run_with(docker: &Path, command: &str, workdir: &Path, limits: &Limits) -> WitnessOutcome {
    let policy = ContainerPolicy::default_run();
    let mut sandbox = match Sandbox::new(docker, workdir, &policy, command) {
        Ok(sandbox) => sandbox,
        Err(detail) => return WitnessOutcome::Error { detail },
    };
    let limits = Limits {
        timeout: limits
            .timeout
            .min(Duration::from_millis(policy.timeout_ms.get())),
        ..limits.clone()
    };
    let outcome = run_container(docker, &sandbox.args, &limits).await;
    match driver::remove_bounded(docker, &sandbox.name, driver::REMOVE_DEADLINE).await {
        Ok(()) => {
            sandbox.removed = true;
            outcome
        }
        Err(err) => WitnessOutcome::Error {
            detail: format!("could not remove the test container: {err}"),
        },
    }
}

struct Sandbox {
    docker: PathBuf,
    name: String,
    dir: PathBuf,
    args: Vec<String>,
    removed: bool,
}

impl Sandbox {
    fn new(
        docker: &Path,
        workdir: &Path,
        policy: &ContainerPolicy,
        command: &str,
    ) -> Result<Self, String> {
        let workdir = workdir
            .canonicalize()
            .map_err(|err| format!("cannot resolve the worktree: {err}"))?;
        // Docker follows bind sources and mount targets. Never let a candidate redirect an overlay.
        for path in [".git", ".acan"] {
            if std::fs::symlink_metadata(workdir.join(path))
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                return Err(format!("the test worktree's {path} must not be a symlink"));
            }
        }
        let name = format!("acan-test-{}", uuid::Uuid::new_v4());
        let dir = std::env::temp_dir().join(&name);
        std::fs::create_dir(&dir).map_err(|err| format!("cannot create test sandbox: {err}"))?;
        let mut sandbox = Self {
            docker: docker.to_owned(),
            name,
            dir,
            args: Vec::new(),
            removed: true,
        };
        let sources = docker::MountSources {
            worktree: &workdir,
            snapshot: &sandbox.dir,
            harness: &sandbox.dir,
            empty: &sandbox.dir,
            dependencies: None,
        };
        // A legacy check has no protected-head snapshot. Mask .acan rather than expose state.
        let mounts = docker::mount_values(policy, &sources)?;
        sandbox.args = docker::test_args(&sandbox.name, policy, &mounts, command)?;
        sandbox.removed = false;
        Ok(sandbox)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if !self.removed {
            driver::remove_blocking(&self.docker, &self.name, driver::REMOVE_DEADLINE);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn run_container(docker: &Path, args: &[String], limits: &Limits) -> WitnessOutcome {
    let mut cmd = crate::child_env::command(docker);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Own process group so a timeout can kill the whole tree with one signal.
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return WitnessOutcome::Error {
                detail: format!("could not start Docker for the test: {e}"),
            };
        }
    };
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let mut out_task = tokio::spawn(read_tail(stdout, limits.tail_bytes));
    let mut err_task = tokio::spawn(read_tail(stderr, limits.tail_bytes));

    let waited = tokio::time::timeout(limits.timeout, child.wait()).await;
    if waited.is_err() {
        if let Some(pid) = pid {
            kill_tree(pid);
        }
        // Reap the child so it cannot linger as a zombie.
        let _ = child.wait().await;
    }
    let drain = tokio::time::timeout(DRAIN_GRACE, async {
        ((&mut out_task).await, (&mut err_task).await)
    })
    .await;
    let (out, err) = if let Ok((out, err)) = drain {
        (out.unwrap_or_default(), err.unwrap_or_default())
    } else {
        out_task.abort();
        err_task.abort();
        (Tail::default(), Tail::default())
    };

    match waited {
        Err(_) => WitnessOutcome::Error {
            detail: detail(
                format!(
                    "timed out after {}s; killed the process tree",
                    limits.timeout.as_secs()
                ),
                &out,
                &err,
            ),
        },
        Ok(Err(e)) => WitnessOutcome::Error {
            detail: format!("could not wait on the command: {e}"),
        },
        Ok(Ok(status)) if status.code() == Some(125) => WitnessOutcome::Error {
            detail: detail(
                "Docker could not start the isolated test".to_owned(),
                &out,
                &err,
            ),
        },
        Ok(Ok(status)) if status.success() => WitnessOutcome::Pass,
        Ok(Ok(status)) => WitnessOutcome::Fail {
            detail: detail(exit_line(status), &out, &err),
        },
    }
}

fn exit_line(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "killed by a signal".to_string(),
    }
}

fn detail(head: String, out: &Tail, err: &Tail) -> String {
    use std::fmt::Write as _;

    let mut detail = head;
    if !out.is_empty() {
        write!(
            detail,
            "\nstdout (last {} bytes):\n{}",
            out.len(),
            out.text()
        )
        .expect("writing to a String cannot fail");
    }
    if !err.is_empty() {
        write!(
            detail,
            "\nstderr (last {} bytes):\n{}",
            err.len(),
            err.text()
        )
        .expect("writing to a String cannot fail");
    }
    detail
}

/// The bounded tail of one output stream. Keeps the last `cap` bytes written to it.
#[derive(Debug, Default)]
struct Tail {
    bytes: VecDeque<u8>,
    cap: usize,
}

impl Tail {
    fn new(cap: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            cap,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= self.cap {
            self.bytes.clear();
            self.bytes.extend(&chunk[chunk.len() - self.cap..]);
            return;
        }
        if self.bytes.len() + chunk.len() > self.cap {
            let overflow = self.bytes.len() + chunk.len() - self.cap;
            self.bytes.drain(..overflow);
        }
        self.bytes.extend(chunk);
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn text(&self) -> String {
        let (a, b) = self.bytes.as_slices();
        let mut flat = Vec::with_capacity(a.len() + b.len());
        flat.extend_from_slice(a);
        flat.extend_from_slice(b);
        String::from_utf8_lossy(&flat).into_owned()
    }
}

async fn read_tail<R: AsyncRead + Unpin>(mut reader: R, cap: usize) -> Tail {
    let mut tail = Tail::new(cap);
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => tail.push(&buf[..n]),
        }
    }
    tail
}

/// Kills the whole process tree rooted at `pid`. Best effort: the process may already be
/// gone, and a failure here leaves the timeout outcome an error either way.
#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = crate::child_env::std_command("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Kills the whole process group of `pid`. Best effort.
#[cfg(unix)]
fn kill_tree(pid: u32) {
    // SAFETY: kill only delivers a signal. The group was created by this crate via
    // process_group(0), so the negative pid targets the child's tree. If the child exited
    // in the timeout window its pid could be reused; the window is small and the kill is
    // best effort, so a missed signal is acceptable.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(not(any(windows, unix)))]
compile_error!("acan-witness supports windows and unix only");

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_docker_never_falls_back_to_the_host_shell() {
        let dir = tempfile::tempdir().expect("worktree");
        let outcome = run_with(
            Path::new("acan-witness-no-such-docker"),
            "echo unsafe > host-executed",
            dir.path(),
            &Limits::default(),
        )
        .await;
        let WitnessOutcome::Error { detail } = outcome else {
            panic!("expected Error, got {outcome:?}");
        };
        assert!(!dir.path().join("host-executed").exists(), "{detail}");
    }

    #[test]
    fn legacy_tests_keep_the_probe_isolation_and_mask_server_state() {
        let worktree = tempfile::tempdir().expect("worktree");
        let sandbox = Sandbox::new(
            Path::new("docker"),
            worktree.path(),
            &ContainerPolicy::default_run(),
            "bun test",
        )
        .expect("sandbox");
        let args = &sandbox.args;
        for pair in [
            ["--network", "none"],
            ["--cap-drop", "ALL"],
            ["--security-opt", "no-new-privileges"],
            ["--user", "1000:1000"],
            ["--pull", "never"],
            ["--entrypoint", "sh"],
        ] {
            assert!(args.windows(2).any(|w| w == pair), "{pair:?}: {args:?}");
        }
        assert!(args.iter().any(|arg| arg == "--read-only"));
        assert!(args.iter().any(|arg| arg.contains("@sha256:")));
        assert!(
            args.iter()
                .any(|arg| arg.ends_with("target=/work/.acan,readonly"))
        );
        assert!(args.iter().any(|arg| arg == "HOME=/tmp"));
        assert_eq!(&args[args.len() - 2..], ["-c", "bun test"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn isolated_exit_codes_output_and_timeout_are_reported_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        for (action, expected) in [
            ("exit 0", "pass"),
            ("echo tail-output; exit 3", "fail"),
            ("exit 125", "error"),
            ("exec sleep 30", "timeout"),
        ] {
            let dir = tempfile::tempdir().expect("temp dir");
            let docker = dir.path().join("docker");
            let log = dir.path().join("log");
            std::fs::write(&docker, format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\ncase \"$1\" in\nrun) {action} ;;\nrm) exit 0 ;;\nesac\n", log.display()
            )).expect("fake docker");
            std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755))
                .expect("executable");
            let limits = Limits {
                timeout: Duration::from_millis(200),
                ..Limits::default()
            };
            let outcome =
                run_with(&docker, "never evaluated on the host", dir.path(), &limits).await;
            match (expected, outcome) {
                ("pass", WitnessOutcome::Pass) => {}
                ("fail", WitnessOutcome::Fail { detail }) => assert!(
                    detail.contains("exit code 3") && detail.contains("tail-output"),
                    "{detail}"
                ),
                ("error", WitnessOutcome::Error { .. }) => {}
                ("timeout", WitnessOutcome::Error { detail }) => {
                    assert!(detail.contains("timed out"), "{detail}")
                }
                (_, outcome) => panic!("{expected}: {outcome:?}"),
            }
            let log = std::fs::read_to_string(log).expect("log");
            assert!(log.contains("rm\n--force\nacan-test-"), "{log}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn candidate_symlinks_cannot_redirect_container_overlays() {
        for path in [".git", ".acan"] {
            let dir = tempfile::tempdir().expect("worktree");
            std::os::unix::fs::symlink("/etc", dir.path().join(path)).expect("symlink");
            assert!(
                Sandbox::new(
                    Path::new("docker"),
                    dir.path(),
                    &ContainerPolicy::default_run(),
                    "true"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn tail_keeps_the_last_bytes() {
        let mut tail = Tail::new(4);
        tail.push(b"ab");
        tail.push(b"cdef");
        assert_eq!(tail.text(), "cdef");
        tail.push(b"ghijkl");
        assert_eq!(tail.text(), "ijkl");
    }

    #[test]
    fn an_empty_tail_reports_nothing() {
        let tail = Tail::new(8);
        assert_eq!(
            detail("exit code 1".to_string(), &tail, &tail),
            "exit code 1"
        );
    }
}
