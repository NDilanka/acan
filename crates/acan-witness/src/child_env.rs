//! Host tools get only the environment needed to locate binaries and user/runtime directories.

use std::ffi::OsStr;

const ALLOWED: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "SYSTEMROOT",
    "WINDIR",
    "TEMP",
    "TMP",
    "TMPDIR",
    "APPDATA",
    "LOCALAPPDATA",
    "XDG_RUNTIME_DIR",
];

/// Builds a host command without credentials, daemon overrides, proxies or loader/shell hooks.
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.env_clear();
    for name in ALLOWED {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

/// The async form of [`std_command`].
pub fn command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    std_command(program).into()
}
