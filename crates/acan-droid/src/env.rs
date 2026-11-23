use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;

use tokio::process::Command;

const AUTO_UPDATE: &str = "FACTORY_DROID_AUTO_UPDATE_ENABLED";
const RESERVED_PREFIXES: [&[u8]; 3] = [b"BUN_", b"FACTORY_", b"DROID_"];

/// Builds a subprocess environment without inherited Droid or Bun overrides.
///
/// Other inherited variables are kept so the executable can find the user's home and tools.
/// The reserved prefixes are case-insensitive and cannot be reintroduced by the caller.
#[derive(Clone, Default)]
pub struct CleanEnv {
    extras: BTreeMap<OsString, OsString>,
}

impl fmt::Debug for CleanEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CleanEnv")
            .field("extra_count", &self.extras.len())
            .finish()
    }
}

impl CleanEnv {
    /// Starts with no caller-supplied variables.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an explicit variable, unless its name has a reserved prefix.
    pub fn insert(&mut self, name: impl Into<OsString>, value: impl Into<OsString>) {
        let name = name.into();
        if !is_reserved(&name) {
            self.extras.insert(name, value.into());
        }
    }

    pub(crate) fn apply(&self, command: &mut Command) {
        self.apply_from(command, env::vars_os());
    }

    fn apply_from(
        &self,
        command: &mut Command,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
    ) {
        command
            .env_clear()
            .envs(inherited.into_iter().filter(|(name, _)| !is_reserved(name)))
            .envs(&self.extras)
            .env(AUTO_UPDATE, "false");
    }
}

fn is_reserved(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    RESERVED_PREFIXES.iter().any(|prefix| {
        bytes
            .get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn child_receives_cleaned_environment() {
        let mut env = CleanEnv::new();
        env.insert("ACAN_TEST_ORDER", "test-order");
        env.insert("factory_droid_auto_update_enabled", "true");
        env.insert("BuN_explicit", "blocked");
        // Use synthetic inherited values instead of changing the test process's environment.
        let inherited = [
            ("BUN_JSC_useJIT", "false"),
            ("fAcToRy_API_KEY", "synthetic"),
            ("DrOiD_SESSION", "synthetic"),
            ("FACTORY_DROID_AUTO_UPDATE_ENABLED", "true"),
            ("ACAN_TEST_KEEP", "keep"),
        ]
        .map(|(name, value)| (name.into(), value.into()));

        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd.exe");
            command.args([
                "/d",
                "/c",
                "set BUN_ & set FACTORY_ & set DROID_ & set ACAN_TEST_",
            ]);
            command
        };
        #[cfg(not(windows))]
        let mut command = Command::new("printenv");

        env.apply_from(&mut command, inherited);
        let output = command.output().await.expect("run the environment helper");
        assert!(output.status.success(), "environment helper failed");
        let mut lines: Vec<_> = std::str::from_utf8(&output.stdout)
            .expect("read environment helper output")
            .lines()
            .collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            [
                "ACAN_TEST_KEEP=keep",
                "ACAN_TEST_ORDER=test-order",
                "FACTORY_DROID_AUTO_UPDATE_ENABLED=false",
            ]
        );
    }
}
