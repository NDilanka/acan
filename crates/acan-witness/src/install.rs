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
//! no install container keeps running after the install returns or is dropped. The removal has its
//! own deadline, and when docker does not confirm it, the install reports that, because the
//! container may still exist.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use acan_intent::ContainerPolicy;
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
    /// The install container could not be removed, so it may still exist. This is reported even
    /// when the install itself also failed.
    #[error("the install container {name} could not be removed, so it may still exist: {detail}")]
    Remove {
        /// The container's name.
        name: String,
        /// Why the removal failed.
        detail: String,
    },
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
    install_within(config, policy, scratch, argv, driver::REMOVE_DEADLINE).await
}

/// As [`install_dependencies`], with the deadline the container's removal is given.
async fn install_within(
    config: &RunnerConfig,
    policy: &ContainerPolicy,
    scratch: &Path,
    argv: &[String],
    removal: Duration,
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
        removal,
    };
    let outcome = match timeout(limit, run(config, args)).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => Err(InstallError::Timeout(limit)),
    };
    // A container that may still exist is the failure the caller must hear of, so it is reported
    // before the outcome of the install.
    container.remove().await?;
    outcome?;
    Ok(InstallReport {
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
    let mut child = crate::child_env::command(&config.docker)
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

/// The install container, by its name. It is removed on every path that returns, and without
/// awaiting on drop, so an install that stops early leaves no container running.
struct InstallContainer {
    docker: PathBuf,
    name: String,
    /// Set only when docker confirmed the removal, so a removal that failed is retried on drop.
    removed: bool,
    /// The deadline each removal is given.
    removal: Duration,
}

impl InstallContainer {
    /// Removes the container, retrying while docker is busy, and within the removal's deadline.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::Remove`] when docker does not confirm the removal.
    async fn remove(&mut self) -> Result<(), InstallError> {
        driver::remove_bounded(&self.docker, &self.name, self.removal)
            .await
            .map_err(|err| InstallError::Remove {
                name: self.name.clone(),
                detail: err.to_string(),
            })?;
        self.removed = true;
        Ok(())
    }
}

impl Drop for InstallContainer {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        // Reached when the install stopped before its removal was confirmed, so the container may
        // be running. The retry is bounded too, so a stalled daemon cannot hold the thread.
        driver::remove_blocking(&self.docker, &self.name, self.removal);
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
        fake_docker_with(dir, "true")
    }

    /// As [`fake_docker`], except that each `rm` is logged and then runs `on_rm`, a shell command.
    fn fake_docker_with(dir: &Path, on_rm: &str) -> (PathBuf, PathBuf) {
        let log = dir.join("log");
        let program = dir.join("docker");
        let log_text = log.display();
        let script = format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             create) echo 0123456789abcdef ;;\n\
             start) exec sleep 30 ;;\n\
             rm) echo \"$*\" >> '{log_text}'; {on_rm} ;;\n\
             *) echo \"$*\" >> '{log_text}' ;;\n\
             esac\n"
        );
        fs::write(&program, script).expect("the fake docker");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("executable");
        // A fork in another test can inherit the script's write handle for a moment, and an exec of
        // a file that is open for writing fails with "text file busy". Wait until it can run.
        for _ in 0..200 {
            let probe = std::process::Command::new(&program)
                .arg("create")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if probe.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
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
    fn a_removal_that_stalls_is_given_up_at_its_deadline_and_reported() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (program, _log) = fake_docker_with(dir.path(), "exec sleep 30");
        let config = RunnerConfig::new(program, 3000);
        let mut policy = ContainerPolicy::node_install();
        policy.timeout_ms = NonZeroU64::new(300).expect("a limit");
        let started = Instant::now();

        let err = runtime()
            .block_on(install_within(
                &config,
                &policy,
                dir.path(),
                &argv(),
                Duration::from_millis(300),
            ))
            .expect_err("the removal never finishes");

        assert!(
            matches!(&err, InstallError::Remove { name, .. } if name.starts_with("acan-install-")),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the stalled removal was given up at its deadline"
        );
    }

    #[test]
    fn a_removal_docker_refuses_is_retried_and_reported_with_its_container() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (program, log) =
            fake_docker_with(dir.path(), "echo 'Error: device is busy' >&2; exit 1");
        let config = RunnerConfig::new(program, 3000);
        let mut policy = ContainerPolicy::node_install();
        policy.timeout_ms = NonZeroU64::new(300).expect("a limit");

        let err = runtime()
            .block_on(install_within(
                &config,
                &policy,
                dir.path(),
                &argv(),
                Duration::from_secs(5),
            ))
            .expect_err("docker refuses the removal");

        let text = err.to_string();
        assert!(matches!(&err, InstallError::Remove { .. }), "{text}");
        assert!(
            text.contains("acan-install-") && text.contains("device is busy"),
            "{text}"
        );
        let log = fs::read_to_string(&log).expect("the log");
        assert!(
            log.matches("rm --force acan-install-").count() > 1,
            "the refused removal was retried, and again on drop: {log}"
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
