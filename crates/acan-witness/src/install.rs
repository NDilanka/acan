//! The install phase of a node app: one container with network egress runs the app's install argv
//! once, in a scratch directory that holds only the manifests and the lockfile.
//!
//! The container runs the argv as its only process, with the policy of [`ContainerPolicy`]'s
//! `node_install`: the pinned Node image, a read-only root filesystem, no capabilities, and a
//! writable `/tmp` and scratch directory. Nothing else of the worktree is mounted, and no
//! protected path, so the install reads nothing a candidate controls except its manifests.
//!
//! The whole install, from create to removal, runs under the policy's time limit. The container is
//! removed by its name whichever way the install ends, including when its caller stops waiting, so
//! no install container keeps running after the install returns or is dropped.

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
    /// How long the install took, from the create to the removal of its container.
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
    let limit = Duration::from_millis(policy.timeout_ms.get());
    let started = Instant::now();
    // The name is known before docker is asked to create anything, so the container can be
    // removed by name whether or not its id was ever returned.
    let mut container = InstallContainer {
        docker: config.docker.clone(),
        name,
        removed: false,
    };
    let outcome = match timeout(limit, run(config, args)).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => Err(InstallError::Timeout(limit)),
    };
    container.remove().await;
    outcome.map(|()| InstallReport {
        duration: started.elapsed(),
    })
}

/// Creates the install container, then starts it and waits for it to exit.
async fn run(config: &RunnerConfig, args: Vec<String>) -> Result<(), InstallError> {
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
    // Dropping the child, when the install times out or its caller stops waiting, kills the
    // docker client. The container itself is removed by name.
    let mut child = Command::new(&config.docker)
        .args(["start", "--attach", &id])
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
    let status = child.wait().await.map_err(|source| InstallError::Spawn {
        docker: config.docker.clone(),
        source,
    })?;
    if status.success() {
        return Ok(());
    }
    let code = status
        .code()
        .map_or_else(|| "signal".to_owned(), |code| code.to_string());
    let detail = head.await.unwrap_or_default();
    Err(InstallError::Failed {
        code,
        detail: detail.trim().to_owned(),
    })
}

/// The install container, by its name. It is removed on every path that returns, and synchronously
/// on drop, so an install that stops early leaves no container running.
struct InstallContainer {
    docker: PathBuf,
    name: String,
    removed: bool,
}

impl InstallContainer {
    /// Removes the container, retrying while docker is busy.
    async fn remove(&mut self) {
        driver::remove(&self.docker, &self.name).await;
        self.removed = true;
    }
}

impl Drop for InstallContainer {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        // Reached only when the install stopped before its removal, so the container may be running.
        let _ = std::process::Command::new(&self.docker)
            .args(["rm", "--force", &self.name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::num::NonZeroU64;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A stand-in for docker. `create` prints a container id, `start` runs until it is killed, and
    /// every other command is appended to the returned log.
    fn fake_docker(dir: &Path) -> (PathBuf, PathBuf) {
        let log = dir.join("log");
        let program = dir.join("docker");
        let script = format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             create) echo 0123456789abcdef ;;\n\
             start) exec sleep 30 ;;\n\
             *) echo \"$*\" >> '{}' ;;\n\
             esac\n",
            log.display()
        );
        fs::write(&program, script).expect("the fake docker");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("executable");
        (program, log)
    }

    fn argv() -> Vec<String> {
        ["npm", "ci", "--ignore-scripts"]
            .map(str::to_owned)
            .to_vec()
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
    }

    #[test]
    fn the_install_time_limit_is_ten_minutes() {
        assert_eq!(ContainerPolicy::node_install().timeout_ms.get(), 600_000);
    }

    #[test]
    fn an_install_past_its_limit_is_removed_by_name() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (program, log) = fake_docker(dir.path());
        let config = RunnerConfig::new(program, 3000);
        let mut policy = ContainerPolicy::node_install();
        policy.timeout_ms = NonZeroU64::new(300).expect("a limit");

        let err = runtime()
            .block_on(install_dependencies(&config, &policy, dir.path(), &argv()))
            .expect_err("the install runs past its limit");

        assert!(matches!(err, InstallError::Timeout(_)), "{err:?}");
        let log = fs::read_to_string(&log).expect("the log");
        assert!(
            log.contains("rm --force acan-install-"),
            "the container is removed by name: {log}"
        );
    }

    #[test]
    fn an_install_whose_caller_stops_waiting_is_removed_before_the_caller_returns() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (program, log) = fake_docker(dir.path());
        let config = RunnerConfig::new(program, 3000);
        let policy = ContainerPolicy::node_install();

        let stopped = runtime().block_on(async {
            timeout(
                Duration::from_millis(300),
                install_dependencies(&config, &policy, dir.path(), &argv()),
            )
            .await
        });

        assert!(stopped.is_err(), "the caller stopped waiting");
        let log = fs::read_to_string(&log).expect("the log");
        assert!(
            log.contains("rm --force acan-install-"),
            "the container was removed when the install was dropped: {log}"
        );
    }
}
