use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;
use tokio::time;

use crate::{AuthStatus, CleanEnv, DroidError, DroidVersion, VersionPolicy, parse_doctor_auth};

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_ARGS: [&str; 5] = [
    "exec",
    "--input-format",
    "stream-jsonrpc",
    "--output-format",
    "stream-jsonrpc",
];
const FORBIDDEN_ARGS: [&str; 3] = [
    "--skip-permissions-unsafe",
    "--input-format",
    "--output-format",
];

/// Configures a cleaned subprocess in Droid's headless JSON-RPC mode.
#[derive(Clone, Debug)]
pub struct SpawnConfig {
    executable: PathBuf,
    working_dir: Option<PathBuf>,
    env: CleanEnv,
    version_policy: VersionPolicy,
    args: Vec<OsString>,
}

impl Default for SpawnConfig {
    fn default() -> Self {
        Self::new("droid")
    }
}

impl SpawnConfig {
    /// Uses the named program on PATH or the explicit executable path.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            working_dir: None,
            env: CleanEnv::new(),
            version_policy: VersionPolicy::default(),
            args: Vec::new(),
        }
    }

    /// Sets the child process's working directory.
    #[must_use]
    pub fn with_working_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(path.into());
        self
    }

    /// Sets the caller-supplied environment variables.
    #[must_use]
    pub fn with_env(mut self, env: CleanEnv) -> Self {
        self.env = env;
        self
    }

    /// Overrides the default version range.
    #[must_use]
    pub fn with_version_policy(mut self, policy: VersionPolicy) -> Self {
        self.version_policy = policy;
        self
    }

    /// Replaces extra arguments to `droid exec`; the stream format is always set by the client.
    ///
    /// # Errors
    ///
    /// Returns an error if any argument disables permissions or selects a different format.
    pub fn with_args(
        mut self,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Result<Self, DroidError> {
        self.args = args.into_iter().map(Into::into).collect();
        validate_args(&self.args)?;
        Ok(self)
    }

    /// Runs Droid's auth diagnostic with the same environment cleanup as the session.
    ///
    /// A negative auth report is returned even when the diagnostic also exits nonzero.
    ///
    /// # Errors
    ///
    /// Returns an error if the command could not run or did not supply a valid auth report.
    pub async fn check_auth(&self) -> Result<AuthStatus, DroidError> {
        let output = time::timeout(
            PREFLIGHT_TIMEOUT,
            self.command(&self.resolve()?)
                .args(["doctor", "--checks", "auth", "--json"])
                .output(),
        )
        .await
        .map_err(|_| DroidError::Timeout("doctor"))??;
        let parsed = stdout_text(&output.stdout).and_then(parse_doctor_auth);
        if output.status.success() || parsed.as_ref().is_ok_and(|check| !check.authenticated) {
            parsed
        } else {
            Err(DroidError::PreflightExit {
                command: "doctor",
                status: output.status,
            })
        }
    }

    pub(crate) async fn session_command(&self) -> Result<(Command, DroidVersion), DroidError> {
        let executable = self.resolve()?;
        let output = time::timeout(
            PREFLIGHT_TIMEOUT,
            self.command(&executable).arg("--version").output(),
        )
        .await
        .map_err(|_| DroidError::Timeout("version preflight"))??;
        if !output.status.success() {
            return Err(DroidError::PreflightExit {
                command: "--version",
                status: output.status,
            });
        }
        let version = stdout_text(&output.stdout)?.parse()?;
        self.version_policy.check(&version)?;
        let mut command = self.command(&executable);
        command.args(STREAM_ARGS).args(&self.args);
        Ok((command, version))
    }

    fn command(&self, executable: &Path) -> Command {
        // On Windows std routes .cmd paths through cmd.exe /d /c and quotes the args.
        let mut command = Command::new(executable);
        command.kill_on_drop(true);
        self.env.apply(&mut command);
        if let Some(path) = &self.working_dir {
            command.current_dir(path);
        }
        command
    }

    fn resolve(&self) -> io::Result<PathBuf> {
        if self.executable.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing droid executable",
            ));
        }
        resolve_program(&self.executable)
    }
}

fn validate_args(args: &[OsString]) -> Result<(), DroidError> {
    for arg in args {
        let flag = arg
            .as_encoded_bytes()
            .split(|&byte| byte == b'=')
            .next()
            .unwrap_or_default();
        if let Some(&forbidden) = FORBIDDEN_ARGS
            .iter()
            .find(|name| flag.eq_ignore_ascii_case(name.as_bytes()))
        {
            return Err(DroidError::ForbiddenArgument(forbidden));
        }
    }
    Ok(())
}

fn stdout_text(bytes: &[u8]) -> Result<&str, DroidError> {
    std::str::from_utf8(bytes).map_err(|_| DroidError::Protocol("preflight output is not UTF-8"))
}

fn explicit_path(program: &Path) -> bool {
    program.file_name() != Some(program.as_os_str())
}

#[cfg(not(windows))]
fn resolve_program(program: &Path) -> io::Result<PathBuf> {
    if explicit_path(program) {
        std::path::absolute(program)
    } else {
        Ok(program.to_path_buf())
    }
}

#[cfg(windows)]
fn resolve_program(program: &Path) -> io::Result<PathBuf> {
    fn find_program(path: &Path) -> Option<PathBuf> {
        if path.extension().is_some() {
            return path.is_file().then(|| path.to_path_buf());
        }
        ["exe", "cmd"]
            .into_iter()
            .map(|ext| path.with_extension(ext))
            .find(|candidate| candidate.is_file())
    }

    let resolved = if explicit_path(program) {
        find_program(&std::path::absolute(program)?)
    } else {
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .find_map(|dir| find_program(&dir.join(program)))
            .map(std::path::absolute)
            .transpose()?
    };
    resolved
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "droid executable was not found"))
}
