//! Runs a witness check against a task's worktree and reports a [`WitnessOutcome`].
//!
//! The server's `RunWitness` job calls [`run`] once per gated witness. A test command runs
//! through the platform shell (pwsh on Windows when present, else cmd; sh elsewhere) with a
//! timeout that kills the whole process tree. An HTTP check waits for the app port to accept
//! connections, sends one request, and compares the status and body against the expectation.
//! A check that cannot run reports [`WitnessOutcome::Error`], which the gate never counts as
//! a pass. Details are short, human-readable, and never echo the environment.
//!
//! [`run_sequence`] is the Docker runner for Acan contrast runs. It executes one probe
//! sequence against one commit in a fresh `--network none` container and returns the
//! observations admitted by `acan_intent::admit::observed_run`.

use std::path::Path;
use std::time::Duration;

use acan_core::{Check, WitnessOutcome};

pub mod child_env;
mod docker;
mod driver;
mod http;
mod install;
mod process;
mod record;
mod sequence;

pub use install::{InstallError, InstallReport, install_dependencies};
pub use sequence::{KeptBody, RunnerConfig, SequenceInput, run_sequence, run_sequence_keeping};

/// Bounds and endpoints for witness runs.
#[derive(Debug, Clone)]
pub struct Limits {
    /// How long a test command may run before its process tree is killed.
    pub timeout: Duration,
    /// Bytes kept from the tail of each of a command's stdout and stderr, and from the head
    /// of an HTTP body. A body substring is only searched within these bytes.
    pub tail_bytes: usize,
    /// How long an HTTP check waits for the app port to accept connections. The host starts
    /// the app; the check only waits for it.
    pub ready_timeout: Duration,
    /// Timeout of the HTTP request itself, once the port accepts.
    pub request_timeout: Duration,
    /// The base URL HTTP checks target, e.g. `http://127.0.0.1:3000`. Without one an HTTP
    /// check reports an error.
    pub http_base_url: Option<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            tail_bytes: 4 * 1024,
            ready_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(10),
            http_base_url: None,
        }
    }
}

/// Runs one check in `workdir` and reports the outcome.
///
/// `workdir` is the task's worktree: test commands run there. HTTP checks target the app
/// the host started for the task, at [`Limits::http_base_url`].
pub async fn run(check: &Check, workdir: &Path, limits: &Limits) -> WitnessOutcome {
    match check {
        Check::Test { command } => process::run_command(command, workdir, limits).await,
        Check::Http {
            method,
            path,
            expect_status,
            expect_body,
        } => http::run_http(method, path, *expect_status, expect_body.as_deref(), limits).await,
        Check::Screenshot { .. } => WitnessOutcome::Error {
            detail: "unsupported check kind: screenshot".to_string(),
        },
    }
}
