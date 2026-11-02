//! The environment of the git calls that read a contract.
//!
//! Git takes its repository, index, object store, configuration and transport from its
//! environment, and a partial clone fetches a missing object from its promisor remote while it
//! reads. A contract is read from the clone it is given and from nothing else, and nothing is
//! fetched. So every call removes each `GIT_` variable from its environment, and sets
//! `GIT_NO_LAZY_FETCH`, which makes git fail on a missing object instead of fetching it.

use std::ffi::{OsStr, OsString};
use std::process::Command;

/// Prepares a git call. Every `GIT_` variable this process has is removed from the call's
/// environment, then `GIT_NO_LAZY_FETCH=1` is set. The caller sets its own variables after this.
pub(crate) fn prepare(command: &mut Command) {
    prepare_with(command, &inherited_git_variables());
}

/// Removes each of `names` from the call's environment, then sets `GIT_NO_LAZY_FETCH`.
fn prepare_with(command: &mut Command, names: &[OsString]) {
    for name in names {
        command.env_remove(name);
    }
    command.env("GIT_NO_LAZY_FETCH", "1");
}

/// The names of the `GIT_` variables in this process's environment.
fn inherited_git_variables() -> Vec<OsString> {
    std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| is_git_variable(name))
        .collect()
}

/// Whether `name` is a git variable. The comparison ignores case, as Windows environment names do.
fn is_git_variable(name: &OsStr) -> bool {
    name.to_string_lossy()
        .to_ascii_uppercase()
        .starts_with("GIT_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_git_variable_is_named_whatever_its_case_and_no_other_is() {
        for name in [
            "GIT_DIR",
            "GIT_CONFIG_COUNT",
            "git_index_file",
            "Git_Ssh_Command",
        ] {
            assert!(
                is_git_variable(OsStr::new(name)),
                "{name} is a git variable"
            );
        }
        for name in ["PATH", "HOME", "GITHUB_TOKEN", "GIT", "XGIT_DIR"] {
            assert!(
                !is_git_variable(OsStr::new(name)),
                "{name} is not a git variable"
            );
        }
    }

    #[test]
    fn a_git_call_removes_each_named_variable_and_sets_no_lazy_fetch() {
        let mut command = Command::new("git");
        let names = [OsString::from("GIT_DIR"), OsString::from("git_index_file")];

        prepare_with(&mut command, &names);

        let envs: Vec<(OsString, Option<OsString>)> = command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
            .collect();
        assert!(
            envs.contains(&(OsString::from("GIT_DIR"), None)),
            "{envs:?}"
        );
        assert!(
            envs.contains(&(OsString::from("git_index_file"), None)),
            "{envs:?}"
        );
        assert!(
            envs.contains(&(
                OsString::from("GIT_NO_LAZY_FETCH"),
                Some(OsString::from("1"))
            )),
            "{envs:?}"
        );
    }
}
