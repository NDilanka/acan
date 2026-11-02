//! The install phase of a node app: one container with network egress runs the app's install argv
//! once, in a scratch directory that holds only the manifests and the lockfile.
//!
//! The container runs the argv as its only process, with the policy of [`ContainerPolicy`]'s
//! `node_install`: the pinned Node image, a read-only root filesystem, no capabilities, and a
//! writable `/tmp` and scratch directory. Nothing else of the worktree is mounted, and no
//! protected path, so the install reads nothing a candidate controls except its manifests.
//! `docker rm` removes the container when it ends, on success, failure or timeout.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use acan_intent::ContainerPolicy;
use tokio::process::Command;
use tokio::time::timeout;

use crate::docker;
use crate::driver::{self, CreateError};
use crate::sequence::RunnerConfig;

/// What a finished install reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallReport {
    /// How long the install container ran, from create to exit.
    pub duration: Duration,
}

/// Why an install did not finish.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstallError {
    /// The policy or the argv is refused before docker runs.
    #[error("the install is refused: {0}")]
    Refused(String),
    /// docker could not be started, so no install ran.
    #[error("cannot start {}: {source}", docker.display())]
    Spawn {
        /// The docker program.
        docker: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// docker refused to create the install container, or printed no container id.
    #[error("docker did not create the install container: {0}")]
    Create(String),
    /// The install ran past the policy's time limit and was removed.
    #[error("the install did not finish within {0:?}")]
    Timeout(Duration),
    /// The install exited unsuccessfully. `detail` is the head of its stderr.
    #[error("the install exited with status {code}: {detail}")]
    Failed {
        /// The exit code, or `signal` when a signal ended the install.
        code: String,
        /// The head of the install's stderr.
        detail: String,
    },
}

/// Runs `argv` once in a container under `policy`, with `scratch` mounted read-write at the
/// policy's working directory. The install's output is what it leaves in `scratch`.
///
/// # Errors
///
/// Returns [`InstallError`] when the policy refuses the install, docker cannot create or start the
/// container, the install exits unsuccessfully, or it runs past the policy's time limit.
pub async fn install_dependencies(
    config: &RunnerConfig,
    policy: &ContainerPolicy,
    scratch: &Path,
    argv: &[String],
) -> Result<InstallReport, InstallError> {
    let run_id = uuid::Uuid::new_v4().to_string();
    let name = format!("acan-install-{run_id}");
    let args = docker::install_args(&name, &run_id, policy, scratch, argv)
        .map_err(InstallError::Refused)?;
    let id = match driver::create(config.docker.clone(), args).await {
        Ok(id) => id,
        Err(CreateError::Spawn(source)) => {
            return Err(InstallError::Spawn {
                docker: config.docker.clone(),
                source,
            });
        }
        Err(CreateError::Refused(stderr)) => {
            return Err(InstallError::Create(stderr.trim().to_owned()));
        }
        Err(CreateError::Malformed) => {
            return Err(InstallError::Create(
                "docker printed no container id".to_owned(),
            ));
        }
    };
    let limit = Duration::from_millis(policy.timeout_ms.get());
    let started = Instant::now();
    let outcome = run_created(config, &id, limit).await;
    driver::remove(&config.docker, &id).await;
    outcome.map(|()| InstallReport {
        duration: started.elapsed(),
    })
}

/// Starts the created container `id` and waits for it to exit within `limit`.
async fn run_created(config: &RunnerConfig, id: &str, limit: Duration) -> Result<(), InstallError> {
    let mut child = Command::new(&config.docker)
        .args(["start", "--attach", id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| InstallError::Spawn {
            docker: config.docker.clone(),
            source,
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| InstallError::Create("docker stderr is not piped".to_owned()))?;
    let head = tokio::spawn(driver::keep_head(stderr));
    match timeout(limit, child.wait()).await {
        Err(_elapsed) => {
            let _ = child.start_kill();
            Err(InstallError::Timeout(limit))
        }
        Ok(Err(source)) => Err(InstallError::Spawn {
            docker: config.docker.clone(),
            source,
        }),
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => {
            let code = status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string());
            let detail = head.await.unwrap_or_default();
            Err(InstallError::Failed {
                code,
                detail: detail.trim().to_owned(),
            })
        }
    }
}
