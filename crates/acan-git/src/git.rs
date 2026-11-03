//! Running the git CLI with a timeout and captured stderr.

use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::Error;

/// Upper bound for one git invocation. Local operations finish in well under a
/// second; this only bounds a wedged process, and the child is killed on expiry.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// What a git invocation produced.
pub(crate) struct GitOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// What a git invocation produced, with stdout as raw bytes.
pub(crate) struct RawOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
}

impl RawOutput {
    /// The failure this invocation reports.
    pub(crate) fn into_error(self, args: &[&str]) -> Error {
        Error::Git {
            args: render(args),
            code: self.status.code(),
            stderr: self.stderr.trim().to_owned(),
        }
    }
}

fn render(args: &[&str]) -> String {
    args.join(" ")
}

/// Run git in `dir`, returning whatever it did. A non-zero exit is not an error
/// here; callers that give exit codes meaning inspect `status`.
pub(crate) async fn run(
    dir: &Path,
    args: &[&str],
    envs: &[(String, String)],
) -> Result<GitOutput, Error> {
    let out = run_raw(dir, args, envs, None).await?;
    Ok(GitOutput {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: out.stderr,
    })
}

/// Run git in `dir` with `input` on its stdin, returning stdout unconverted.
pub(crate) async fn run_raw(
    dir: &Path,
    args: &[&str],
    envs: &[(String, String)],
    input: Option<&[u8]>,
) -> Result<RawOutput, Error> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    // Before the caller's variables, so a variable the caller names still applies.
    crate::git_env::prepare(cmd.as_std_mut());
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let spawn_error = |source| Error::Spawn {
        args: render(args),
        source,
    };
    let mut child = cmd.spawn().map_err(spawn_error)?;
    let pipe = child.stdin.take();
    let feed = async move {
        if let (Some(mut pipe), Some(bytes)) = (pipe, input) {
            pipe.write_all(bytes).await?;
        }
        Ok::<(), std::io::Error>(())
    };
    let (fed, out) = tokio::time::timeout(GIT_TIMEOUT, async {
        tokio::join!(feed, child.wait_with_output())
    })
    .await
    .map_err(|_| Error::Timeout(render(args)))?;
    let out = out.map_err(spawn_error)?;
    // A git that fails early closes its stdin, and its exit status says why; a
    // failed write matters only when git reports success.
    if out.status.success() {
        fed?;
    }
    Ok(RawOutput {
        status: out.status,
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Run git, turning a non-zero exit into [`Error::Git`], and return stdout.
pub(crate) async fn ok(
    dir: &Path,
    args: &[&str],
    envs: &[(String, String)],
) -> Result<String, Error> {
    let out = run(dir, args, envs).await?;
    ensure_ok(out, args)
}

/// Run git with `input` on its stdin, turning a non-zero exit into [`Error::Git`], and
/// return stdout unconverted.
pub(crate) async fn ok_raw(
    dir: &Path,
    args: &[&str],
    envs: &[(String, String)],
    input: Option<&[u8]>,
) -> Result<Vec<u8>, Error> {
    let out = run_raw(dir, args, envs, input).await?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(out.into_error(args))
    }
}

/// Run git and return stdout as text, refusing output that is not UTF-8 rather than
/// replacing bytes, because the caller feeds the paths it names back to git.
pub(crate) async fn ok_utf8(
    dir: &Path,
    args: &[&str],
    envs: &[(String, String)],
) -> Result<String, Error> {
    let bytes = ok_raw(dir, args, envs, None).await?;
    String::from_utf8(bytes).map_err(|_| Error::BadOutput {
        args: render(args),
        detail: "output is not UTF-8".to_owned(),
    })
}

/// Turn a finished invocation into stdout, or [`Error::Git`] on non-zero exit.
pub(crate) fn ensure_ok(out: GitOutput, args: &[&str]) -> Result<String, Error> {
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(Error::Git {
            args: render(args),
            code: out.status.code(),
            stderr: out.stderr.trim().to_owned(),
        })
    }
}
