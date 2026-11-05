//! Test-command checks: run through the platform shell with a timeout and a bounded tail.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use acan_core::WitnessOutcome;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::Limits;

/// How long to wait for piped output to drain after the command exits or is killed. A
/// detached grandchild can hold a pipe open, so the drain is bounded.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// The shell a test command runs through.
#[derive(Debug, Clone, Copy)]
struct Shell {
    program: &'static str,
    args: &'static [&'static str],
}

/// pwsh on Windows when it is on `PATH`, else cmd; sh elsewhere.
#[cfg(windows)]
fn default_shell() -> Shell {
    let pwsh = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("pwsh.exe").is_file()));
    if pwsh {
        Shell {
            program: "pwsh",
            args: &["-NoProfile", "-NonInteractive", "-Command"],
        }
    } else {
        Shell {
            program: "cmd",
            args: &["/C"],
        }
    }
}

/// pwsh on Windows when it is on `PATH`, else cmd; sh elsewhere.
#[cfg(not(windows))]
fn default_shell() -> Shell {
    Shell {
        program: "sh",
        args: &["-c"],
    }
}

/// Runs `command` in `workdir` through the platform shell.
pub(crate) async fn run_command(command: &str, workdir: &Path, limits: &Limits) -> WitnessOutcome {
    run_as(default_shell(), command, workdir, limits).await
}

async fn run_as(shell: Shell, command: &str, workdir: &Path, limits: &Limits) -> WitnessOutcome {
    let mut cmd = crate::child_env::command(shell.program);
    cmd.args(shell.args)
        .arg(command)
        .current_dir(workdir)
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
                detail: format!("could not start {}: {e}", shell.program),
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
    async fn a_shell_that_cannot_start_is_an_error() {
        let shell = Shell {
            program: "acan-witness-no-such-shell",
            args: &[],
        };
        let outcome = run_as(shell, "exit 0", Path::new("."), &Limits::default()).await;
        let WitnessOutcome::Error { detail } = outcome else {
            panic!("expected Error, got {outcome:?}");
        };
        assert!(detail.contains("could not start"), "{detail}");
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
