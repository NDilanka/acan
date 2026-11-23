//! What the app tells its user about itself: the version, whether a newer alpha build is out, and
//! a bug report that is safe to post.
//!
//! One process serves one world, and its log is process-wide, so the state the routes read is
//! process-wide too. A server built without [`configure`] or [`spawn_update_check`] reports no
//! update check and no log lines, which is what the tests see.

mod log;
mod redact;
mod report;
mod update;

use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::Serialize;

pub use log::LogTee;
pub use redact::Redaction;
pub use report::BugReport;
pub use update::{UpdateState, UpdateStatus, spawn_update_check};

/// The version this build reports. The release workflow sets `ACAN_VERSION` to the release tag,
/// with or without its leading `v`. A local build reports the crate version.
#[must_use]
pub fn version() -> &'static str {
    let raw = option_env!("ACAN_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    raw.strip_prefix('v').unwrap_or(raw)
}

/// The commit this build came from, set by the release workflow. `unknown` for a local build.
#[must_use]
pub fn commit() -> &'static str {
    option_env!("ACAN_COMMIT").unwrap_or("unknown")
}

/// The build's identity, as `GET /version` returns it. It holds nothing private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionInfo {
    pub name: &'static str,
    pub version: &'static str,
    pub commit: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
}

/// The build's identity now.
#[must_use]
pub fn version_info() -> VersionInfo {
    VersionInfo {
        name: "acan",
        version: version(),
        commit: commit(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    }
}

/// What the process-wide state holds.
struct State {
    redaction: Redaction,
    update: UpdateStatus,
}

static STATE: Mutex<State> = Mutex::new(State {
    redaction: Redaction::EMPTY,
    update: UpdateStatus::off(),
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sets what every bug report removes from its text: the secrets in the environment, and the
/// paths of this machine. Call it once at startup, after the `.env` file is loaded.
pub fn configure(redaction: Redaction) {
    state().redaction = redaction;
}

/// The update check as it stands now. `current` is always this build's version, even before a
/// check has run.
#[must_use]
pub fn update_status() -> UpdateStatus {
    let mut status = state().update.clone();
    status.current = version();
    status
}

/// Replaces the update check's status. Used by the update check itself.
pub(crate) fn set_update_status(status: UpdateStatus) {
    state().update = status;
}

/// A bug report built from the last log lines and the current redaction.
#[must_use]
pub fn bug_report() -> BugReport {
    let redaction = state().redaction.clone();
    report::build(&version_info(), &log::recent(), &redaction)
}
