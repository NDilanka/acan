use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;
use tokio::time;

use crate::process::Process;
use crate::{AuthStatus, CleanEnv, DroidError, DroidVersion, VersionPolicy, parse_doctor_auth};

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_ARGS: [&str; 5] = [
    "exec",
    "--input-format",
    "stream-jsonrpc",
    "--output-format",
    "stream-jsonrpc",
];
const FORBIDDEN_ARGS: [&str; 4] = [
    "--skip-permissions-unsafe",
    "--input-format",
    "--output-format",
    "-o",
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

    /// Adds one explicit variable using the existing environment's cleanup rules.
    #[must_use]
    pub fn with_env_var(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(name, value);
        self
    }

    /// Removes both an inherited variable and any existing explicit value for it.
    #[must_use]
    pub fn without_env_var(mut self, name: &str) -> Self {
        self.env.remove(name);
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
        let mut command = self.command(&self.resolve()?);
        command.args(["doctor", "--checks", "auth", "--json"]);
        let output = time::timeout(PREFLIGHT_TIMEOUT, Process::output(&mut command))
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
        #[cfg(windows)]
        validate_batch_input(&executable, &self.args)?;
        let mut version_command = self.command(&executable);
        version_command.arg("--version");
        let output = time::timeout(PREFLIGHT_TIMEOUT, Process::output(&mut version_command))
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
        self.env.apply(&mut command);
        if let Some(path) = &self.working_dir {
            command.current_dir(path);
        }
        command
    }

    fn resolve(&self) -> Result<PathBuf, DroidError> {
        if self.executable.as_os_str().is_empty() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "missing droid executable").into(),
            );
        }
        let executable = resolve_program(&self.executable)?;
        #[cfg(windows)]
        validate_batch_input(&executable, &[])?;
        Ok(executable)
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
    resolve_windows_program(program, std::env::var_os("PATH").as_deref())
}

#[cfg(windows)]
fn resolve_windows_program(program: &Path, path: Option<&std::ffi::OsStr>) -> io::Result<PathBuf> {
    fn find_program(path: &Path) -> Option<PathBuf> {
        if path.extension().is_some() {
            return path.is_file().then(|| path.to_path_buf());
        }
        ["exe", "cmd", "bat"]
            .into_iter()
            .map(|ext| path.with_extension(ext))
            .find(|candidate| candidate.is_file())
    }

    let resolved = if explicit_path(program) {
        find_program(&std::path::absolute(program)?)
    } else {
        path.into_iter()
            .flat_map(std::env::split_paths)
            .filter(|dir| !dir.as_os_str().is_empty())
            .find_map(|dir| find_program(&dir.join(program)))
            .map(std::path::absolute)
            .transpose()?
    };
    resolved
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "droid executable was not found"))
}

#[cfg(windows)]
fn validate_batch_input(executable: &Path, args: &[OsString]) -> Result<(), DroidError> {
    use std::ffi::OsStr;

    fn has_metacharacters(value: &OsStr) -> bool {
        value
            .as_encoded_bytes()
            .iter()
            .any(|byte| b"\"%&|<>^!\r\n\0".contains(byte))
    }

    // The standard launcher can normalize trailing dots and spaces before classifying a shim.
    let mut name = executable
        .file_name()
        .unwrap_or_default()
        .as_encoded_bytes();
    while let [rest @ .., b'.' | b' '] = name {
        name = rest;
    }
    if name.len().checked_sub(4).is_some_and(|i| {
        name[i..].eq_ignore_ascii_case(b".cmd") || name[i..].eq_ignore_ascii_case(b".bat")
    }) {
        if has_metacharacters(executable.as_os_str()) {
            return Err(DroidError::UnsafeBatchInput("path"));
        }
        if args.iter().any(|arg| has_metacharacters(arg)) {
            return Err(DroidError::UnsafeBatchInput("argument"));
        }
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn batch_validation_rejects_shell_metacharacters() {
        for ext in ["cmd", "BaT"] {
            let shim = PathBuf::from(format!(r"C:\Program Files (x86)\Droid\droid.{ext}"));
            for value in [
                "%ACAN_MCP_TOKEN%",
                "\" & calc",
                "a|b",
                "a^b",
                "a\nb",
                "a\rb",
                "a\0b",
                "a!b",
                "a>b",
                "a<b",
            ] {
                assert!(matches!(
                    validate_batch_input(&shim, &[value.into()]),
                    Err(DroidError::UnsafeBatchInput("argument"))
                ));
                let unsafe_shim = shim.with_file_name(format!("{value}.{ext}"));
                assert!(matches!(
                    validate_batch_input(&unsafe_shim, &[]),
                    Err(DroidError::UnsafeBatchInput("path"))
                ));
            }
            assert!(validate_batch_input(&shim, &["normal (text)".into()]).is_ok());
            assert!(validate_batch_input(&shim.with_extension("exe"), &["a|b".into()]).is_ok());
            assert!(matches!(
                validate_batch_input(&shim.with_extension(format!("{ext} .")), &["a|b".into()]),
                Err(DroidError::UnsafeBatchInput("argument"))
            ));
        }
    }

    #[test]
    fn windows_resolution_ignores_empty_path_segments() {
        let program = Path::new("Cargo.toml");
        assert!(program.is_file(), "the crate manifest is in the test cwd");
        assert_eq!(
            resolve_windows_program(program, Some(OsStr::new(";")))
                .expect_err("empty PATH components must not search the working directory")
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
