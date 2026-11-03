//! The host side of the in-container driver.
//!
//! One probe sequence gets one container. `docker create` makes it first, so its id is known
//! before anything runs, and `docker start --attach --interactive` runs the driver as the
//! container's only process. The host writes one JSON operation per line to the container's stdin
//! and reads one JSON reply per line from its stdout. Closing stdin ends the driver, which stops
//! the app, and `--rm` removes the container when it exits.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

/// The driver's source. It is compiled into this crate and written to a fresh host directory for
/// each run, so the container never runs code from the worktree.
pub(crate) const DRIVER_SOURCE: &str = include_str!("harness/driver.ts");

/// The Node driver's source, for node apps. It speaks the same protocol as [`DRIVER_SOURCE`].
pub(crate) const NODE_DRIVER_SOURCE: &str = include_str!("harness/driver.mjs");

/// How long a container may take to exit after its stdin closes before it is removed by force.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// How many times a removal is tried before it gives up, and the wait between tries. A removal
/// may find the daemon still busy with the container it has just been asked to stop.
const REMOVE_ATTEMPTS: u32 = 10;
const REMOVE_RETRY_GAP: Duration = Duration::from_millis(50);
/// How long one container removal may take in all before it is given up. A stalled daemon must
/// not hold a run or an install past its limit.
pub(crate) const REMOVE_DEADLINE: Duration = Duration::from_secs(10);
/// How often a removal that cannot await checks whether docker has finished.
const BLOCKING_POLL: Duration = Duration::from_millis(10);

/// Bytes of the CLI's stderr kept for diagnosis.
const STDERR_HEAD_BYTES: usize = 8 * 1024;

/// The ready check the driver polls before it answers `start`.
#[derive(Debug, Serialize)]
pub(crate) struct Ready<'a> {
    pub path: &'a str,
    pub status: u16,
    pub timeout_ms: u64,
}

/// One operation for the driver.
#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Op<'a> {
    /// Starts the app and waits until its ready check passes.
    Start {
        argv: &'a [String],
        cwd: &'a str,
        environment: &'a BTreeMap<String, String>,
        port_env: &'a str,
        port: u16,
        max_output_bytes: u64,
        ready: Ready<'a>,
    },
    /// Sends one request from a loopback source address.
    Request {
        client: &'a str,
        method: &'a str,
        path: &'a str,
        headers: &'a BTreeMap<String, String>,
        body: Option<&'a str>,
    },
    /// Advances the app's controlled clock.
    Clock { path: &'a str, by_ms: u64 },
    /// Reads a path on the app's loopback interface, such as a trusted sink.
    Get { path: &'a str },
}

/// The driver's reply to one operation.
#[derive(Debug, Deserialize)]
pub(crate) struct Reply {
    pub ok: bool,
    #[serde(default)]
    pub crashed: bool,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub headers: BTreeMap<String, Value>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub binary: bool,
    #[serde(default)]
    pub truncated: bool,
}

/// Why an operation got no usable reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallError {
    /// The pipe closed or the write failed: the container or the driver is gone.
    Lost,
    /// The reply line was not the driver's protocol.
    Malformed,
}

/// Why a container could not be created.
#[derive(Debug)]
pub(crate) enum CreateError {
    /// The docker program could not be started.
    Spawn(io::Error),
    /// docker refused the create. The text is its stderr.
    Refused(String),
    /// docker finished without printing a container id.
    Malformed,
}

/// Creates the container described by `args` without starting it, and returns its id.
///
/// The caller runs this in its own task, so the create finishes even when the caller stops
/// waiting: the id it returns is then the one the caller removes.
pub(crate) async fn create(docker: PathBuf, args: Vec<String>) -> Result<String, CreateError> {
    let output = Command::new(&docker)
        .args(&args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(CreateError::Spawn)?;
    if !output.status.success() {
        return Err(CreateError::Refused(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(id)
    } else {
        Err(CreateError::Malformed)
    }
}

/// Why a container could not be removed. The container may still exist.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RemoveError {
    /// docker did not finish the removal within the deadline.
    #[error("docker did not finish the removal within {0:?}")]
    Timeout(Duration),
    /// docker refused the removal on every attempt. The text is its last stderr, or a spawn error.
    #[error("docker refused the removal: {0}")]
    Refused(String),
}

/// Removes the container `target`, which is a container id or name. A container that does not
/// exist is already removed. The removal is retried while the daemon is busy, and all of it is
/// given `deadline`.
///
/// # Errors
///
/// Returns [`RemoveError`] when docker refuses the removal on every attempt, or does not finish
/// within the deadline. The container may then still exist.
pub(crate) async fn remove_bounded(
    docker: &Path,
    target: &str,
    deadline: Duration,
) -> Result<(), RemoveError> {
    let attempts = async {
        let mut last = String::new();
        for _ in 0..REMOVE_ATTEMPTS {
            let output = Command::new(docker)
                .args(["rm", "--force", target])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await;
            match output {
                Ok(output) if output.status.success() => return Ok(()),
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                    if stderr.contains("No such container") {
                        return Ok(());
                    }
                    last = stderr.trim().to_owned();
                }
                Err(source) => last = source.to_string(),
            }
            sleep(REMOVE_RETRY_GAP).await;
        }
        Err(RemoveError::Refused(last))
    };
    timeout(deadline, attempts)
        .await
        .unwrap_or(Err(RemoveError::Timeout(deadline)))
}

/// Removes a probe's container, bounded by [`REMOVE_DEADLINE`]. A removal that fails is not
/// reported here: the run's record does not carry it, and the bound only keeps a stalled daemon
/// from holding the run.
pub(crate) async fn remove(docker: &Path, target: &str) {
    let _ = remove_bounded(docker, target, REMOVE_DEADLINE).await;
}

/// Removes `target` without awaiting, for a drop that cannot await. The removal is given
/// `deadline`, after which the docker client is killed. A failed removal is not retried here.
pub(crate) fn remove_blocking(docker: &Path, target: &str, deadline: Duration) {
    let Ok(mut child) = std::process::Command::new(docker)
        .args(["rm", "--force", target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let until = Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(None) if Instant::now() < until => std::thread::sleep(BLOCKING_POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(Some(_)) | Err(_) => return,
        }
    }
}

/// A started `docker start --attach` for one created container.
///
/// Dropping a container that was not finished removes it by id, so a dropped run cannot leave
/// a container behind.
pub(crate) struct Container {
    id: String,
    docker: PathBuf,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    stderr: JoinHandle<String>,
    finished: bool,
}

impl Container {
    /// Starts the created container `id` with its stdio attached.
    ///
    /// # Errors
    ///
    /// Returns the spawn error when the docker program cannot be started.
    pub(crate) fn attach(docker: &Path, id: &str) -> io::Result<Self> {
        let mut child = Command::new(docker)
            .args(["start", "--attach", "--interactive", id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("docker stdin is not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("docker stdout is not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("docker stderr is not piped"))?;
        Ok(Self {
            id: id.to_owned(),
            docker: docker.to_owned(),
            child,
            stdin: Some(stdin),
            lines: BufReader::new(stdout).lines(),
            stderr: tokio::spawn(keep_head(stderr)),
            finished: false,
        })
    }

    /// Sends one operation and reads its reply line.
    ///
    /// A call dropped before its reply is read leaves the pipe out of step with the driver, so
    /// the container must be finished after that, never used for another call.
    ///
    /// # Errors
    ///
    /// Returns [`CallError::Lost`] when the pipe is closed and [`CallError::Malformed`] when the
    /// reply is not a reply.
    pub(crate) async fn call(&mut self, op: &Op<'_>) -> Result<Reply, CallError> {
        let mut line = serde_json::to_string(op).map_err(|_| CallError::Malformed)?;
        line.push('\n');
        let stdin = self.stdin.as_mut().ok_or(CallError::Lost)?;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|_| CallError::Lost)?;
        stdin.flush().await.map_err(|_| CallError::Lost)?;
        let reply = self
            .lines
            .next_line()
            .await
            .map_err(|_| CallError::Lost)?
            .ok_or(CallError::Lost)?;
        serde_json::from_str(&reply).map_err(|_| CallError::Malformed)
    }

    /// Waits briefly for the CLI to exit and returns what it wrote to stderr.
    pub(crate) async fn diagnose(&mut self) -> String {
        let _ = timeout(EXIT_GRACE, self.child.wait()).await;
        match timeout(EXIT_GRACE, &mut self.stderr).await {
            Ok(Ok(text)) => text,
            _ => String::new(),
        }
    }

    /// Ends the container.
    ///
    /// Without `force`, closing stdin lets the driver stop the app and exit, and `--rm` removes
    /// the container. A container that does not exit within the grace period, or any container
    /// when `force` is set, is killed and removed by id.
    pub(crate) async fn finish(mut self, force: bool) {
        if !force {
            drop(self.stdin.take());
            if timeout(EXIT_GRACE, self.child.wait()).await.is_ok() {
                self.finished = true;
                return;
            }
        }
        let _ = self.child.start_kill();
        remove(&self.docker, &self.id).await;
        let _ = self.child.wait().await;
        self.finished = true;
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Reached only when the run future was dropped before the container was finished.
        // Remove the container without awaiting, under a deadline, so none is left running.
        remove_blocking(&self.docker, &self.id, REMOVE_DEADLINE);
    }
}

/// Reads a child's stderr to the end and keeps only its head.
pub(crate) async fn keep_head(mut stderr: ChildStderr) -> String {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while let Ok(read) = stderr.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        let room = STDERR_HEAD_BYTES.saturating_sub(head.len());
        head.extend_from_slice(&chunk[..read.min(room)]);
    }
    String::from_utf8_lossy(&head).into_owned()
}
