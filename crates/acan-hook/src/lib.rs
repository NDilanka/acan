//! Fast, fail-closed hook between Droid and the Acan daemon.
//!
//! Droid runs `acan-hook` for `SessionStart` (a heartbeat) and for `PreToolUse` (the gate on
//! writes, shell and MCP calls). Outside an Acan session, where `ACAN_ORDER_ID` is unset or
//! empty, the hook exits 0 at once without reading stdin. Inside one, every failure exits
//! [`BLOCK_EXIT_CODE`], the only non-zero code Droid treats as a block. The daemon makes the
//! decision. This hook gives fast feedback; the scope guarantee also rests on the daemon and
//! the landing gate.

#![forbid(unsafe_code)]

pub mod input;
pub mod protocol;
pub mod transport;

use serde::de::DeserializeOwned;
use std::fmt;
use std::io::{self, Read};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use input::Plan;
use protocol::HookRequest;
use transport::Endpoint;

/// Set by Acan on the Droid process. Its presence marks an Acan session.
pub const ORDER_ENV: &str = "ACAN_ORDER_ID";
/// Daemon endpoint: a Windows named pipe path, a Unix socket path, or `tcp://127.0.0.1:<port>`.
pub const ENDPOINT_ENV: &str = "ACAN_HOOK_ENDPOINT";
/// Optional deadline override in milliseconds, clamped to the range below.
pub const TIMEOUT_ENV: &str = "ACAN_HOOK_TIMEOUT_MS";
/// Deadline used when [`TIMEOUT_ENV`] is unset or not a number. Droid's own hook timeout is
/// far longer, so the hook gives up and blocks first.
pub const DEFAULT_TIMEOUT_MS: u64 = 200;
/// Shortest accepted deadline.
pub const MIN_TIMEOUT_MS: u64 = 50;
/// Longest accepted deadline.
pub const MAX_TIMEOUT_MS: u64 = 2000;
/// Exit code Droid treats as a block. Droid treats every other non-zero exit as non-blocking.
pub const BLOCK_EXIT_CODE: i32 = 2;
/// Largest hook stdin the hook reads, in bytes. Droid's hook payloads are a few kilobytes, so a
/// larger payload is refused rather than buffered.
pub const MAX_STDIN_BYTES: u64 = 256 * 1024;

/// Why the hook blocks a call or cannot decide. Every variant exits with [`BLOCK_EXIT_CODE`].
#[derive(Debug)]
pub enum HookError {
    /// `ACAN_ORDER_ID` is set but `ACAN_HOOK_ENDPOINT` is not.
    MissingEndpoint,
    /// The endpoint is not a canonical named pipe path, an absolute socket path, or a loopback
    /// `tcp://` address.
    BadEndpoint(String),
    /// An environment variable is not usable, for example a blank `ACAN_ORDER_ID`.
    BadEnv(String),
    /// Hook stdin is not the JSON Droid sends for this event.
    BadInput(String),
    /// The daemon could not be reached, or the local transport failed.
    Daemon(io::Error),
    /// The daemon's reply is not a complete protocol-version-1 decision.
    BadReply(String),
    /// The daemon denied the call.
    Blocked(String),
    /// The hook did not finish within its deadline.
    Timeout(Duration),
    /// The hook failed for a reason outside the daemon and the input.
    Internal(String),
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEndpoint => write!(f, "{ENDPOINT_ENV} is not set"),
            Self::BadEndpoint(why) => write!(f, "bad endpoint: {why}"),
            Self::BadEnv(why) => write!(f, "bad environment: {why}"),
            Self::BadInput(why) => write!(f, "bad hook input: {why}"),
            Self::Daemon(err) => write!(f, "daemon unavailable: {err}"),
            Self::BadReply(why) => write!(f, "bad daemon reply: {why}"),
            Self::Blocked(reason) => write!(f, "blocked: {reason}"),
            Self::Timeout(limit) => write!(f, "timed out after {} ms", limit.as_millis()),
            Self::Internal(why) => write!(f, "internal error: {why}"),
        }
    }
}

impl std::error::Error for HookError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Daemon(err) => Some(err),
            _ => None,
        }
    }
}

/// Runs the hook for this process: reads the environment and stdin, then asks the daemon.
///
/// `started` is when the process entered `main`. The deadline counts from it, so setup before
/// the stdin read uses up the same budget as the wait for the daemon.
///
/// `Ok(())` means exit 0. Any `Err` means exit [`BLOCK_EXIT_CODE`] with the error on stderr.
///
/// # Errors
///
/// Returns the reason to block: a missing or unreachable daemon, bad input, a deny decision,
/// or a missed deadline.
pub fn run_from_process(started: Instant) -> Result<(), HookError> {
    let Some(order) = order_from_env()? else {
        return Ok(());
    };
    let endpoint = Endpoint::from_env()?;
    let limit = timeout_from_env();
    run_with_deadline(started, limit, move || {
        let mut raw = String::new();
        // Reading one byte past the cap is enough to tell an oversized payload apart.
        io::stdin()
            .take(MAX_STDIN_BYTES + 1)
            .read_to_string(&mut raw)
            .map_err(|err| HookError::BadInput(format!("cannot read stdin: {err}")))?;
        if raw.len() as u64 > MAX_STDIN_BYTES {
            return Err(HookError::BadInput(format!(
                "hook input exceeds {MAX_STDIN_BYTES} bytes"
            )));
        }
        match input::plan(input::parse(&raw)?, &order)? {
            Plan::Pass => Ok(()),
            Plan::Ask(request) => ask(&endpoint, &request),
        }
    })
}

/// Parses one JSON object into `T`. An array or scalar is refused even where serde would read
/// it as a struct, so hook input and daemon replies are always named-field objects.
///
/// # Errors
///
/// Returns the parse error text when `raw` is not JSON, or is not an object that fits `T`.
fn parse_object<T: DeserializeOwned>(raw: &str) -> Result<T, String> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|err| err.to_string())?;
    if !value.is_object() {
        return Err("expected a JSON object".to_owned());
    }
    serde_json::from_value(value).map_err(|err| err.to_string())
}

/// Exit code for a panic: [`BLOCK_EXIT_CODE`] in an Acan session, 0 outside one.
#[must_use]
pub fn panic_exit_code() -> i32 {
    match order_from_env() {
        Ok(None) => 0,
        Ok(Some(_)) | Err(_) => BLOCK_EXIT_CODE,
    }
}

/// The order id when this is an Acan session. Unset or empty means not an Acan session.
/// A value that is only whitespace is an error, so a blank id can never pass as no session.
/// The id is sent without surrounding whitespace.
fn order_from_env() -> Result<Option<String>, HookError> {
    match std::env::var_os(ORDER_ENV) {
        None => Ok(None),
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => {
            let text = value
                .into_string()
                .map_err(|_| HookError::BadEnv(format!("{ORDER_ENV} is not valid UTF-8")))?;
            let order = text.trim();
            if order.is_empty() {
                return Err(HookError::BadEnv(format!("{ORDER_ENV} is only whitespace")));
            }
            Ok(Some(order.to_owned()))
        }
    }
}

/// Parses a timeout override in milliseconds. Unset or unparsable input gives
/// [`DEFAULT_TIMEOUT_MS`]; other values are clamped to the accepted range.
fn timeout_ms(raw: Option<&str>) -> u64 {
    raw.and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)
}

fn timeout_from_env() -> Duration {
    let raw = std::env::var(TIMEOUT_ENV).ok();
    Duration::from_millis(timeout_ms(raw.as_deref()))
}

/// Sends one request and turns the reply into the hook result.
fn ask(endpoint: &Endpoint, request: &HookRequest) -> Result<(), HookError> {
    let line = protocol::encode_request(request)?;
    let reply = transport::exchange(endpoint, line.as_bytes())?;
    protocol::parse_reply(&reply)?.into_verdict()
}

/// Runs `work` on a worker thread and gives up once `limit` has passed since `started`.
///
/// The clock starts at `started`, so time spent before this call uses up the same budget. If
/// the budget is already gone, `work` never starts. A stalled stdin read, connect, or reply
/// therefore cannot hold the hook past its deadline. The caller exits the process on error,
/// which also ends the worker.
fn run_with_deadline<F>(started: Instant, limit: Duration, work: F) -> Result<(), HookError>
where
    F: FnOnce() -> Result<(), HookError> + Send + 'static,
{
    let remaining = (started + limit).saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(HookError::Timeout(limit));
    }
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("acan-hook-work".to_owned())
        .spawn(move || {
            // The receiver is gone once the deadline has passed, so a late result is dropped.
            let _ = tx.send(work());
        })
        .map_err(|err| HookError::Internal(format!("cannot start worker: {err}")))?;
    match rx.recv_timeout(remaining) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(HookError::Timeout(limit)),
        Err(RecvTimeoutError::Disconnected) => Err(HookError::Internal(
            "worker stopped without an answer".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_defaults_and_clamps() {
        assert_eq!(timeout_ms(None), DEFAULT_TIMEOUT_MS);
        assert_eq!(timeout_ms(Some("not a number")), DEFAULT_TIMEOUT_MS);
        assert_eq!(timeout_ms(Some(" 300 ")), 300);
        assert_eq!(timeout_ms(Some("1")), MIN_TIMEOUT_MS);
        assert_eq!(timeout_ms(Some("60000")), MAX_TIMEOUT_MS);
    }

    #[test]
    fn deadline_passes_through_a_result_in_time() {
        assert!(run_with_deadline(Instant::now(), Duration::from_secs(5), || Ok(())).is_ok());
    }

    #[test]
    fn deadline_gives_up_on_slow_work() {
        let err = run_with_deadline(Instant::now(), Duration::from_millis(20), || {
            thread::sleep(Duration::from_millis(500));
            Ok(())
        })
        .expect_err("slow work must miss the deadline");
        assert!(matches!(err, HookError::Timeout(_)), "got {err}");
    }

    #[test]
    fn setup_time_counts_against_the_deadline() {
        let started = Instant::now();
        thread::sleep(Duration::from_millis(60));
        let err = run_with_deadline(started, Duration::from_millis(50), || Ok(()))
            .expect_err("a deadline that passed during setup must fail");
        assert!(matches!(err, HookError::Timeout(_)), "got {err}");
    }
}
