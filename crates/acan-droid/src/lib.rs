//! A client for Droid's headless JSON-RPC stream.

mod env;
mod protocol;
mod session;
mod spawn;
mod version;

pub use env::CleanEnv;
pub use protocol::{
    AskAnswer, AskQuestion, AskUserReply, AskUserRequest, DroidEvent, Headroom, InboundMessage,
    OutboundRequest, OutboundResponse, PermissionDetails, PermissionOption, PermissionReply,
    PermissionRequest, PermissionToolUse, RequestId, RpcError, ScriptCall, ScriptConfirmation,
    SessionSettings, ToolUse,
};
pub use session::{DroidSession, SessionInit};
pub use spawn::SpawnConfig;
pub use version::{AuthCheck, AuthStatus, DroidVersion, VersionPolicy, parse_doctor_auth};

/// A failure to start or communicate with Droid.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DroidError {
    /// The subprocess or transport failed.
    #[error("droid I/O: {0}")]
    Io(#[from] std::io::Error),
    /// A protocol frame or preflight report could not be decoded.
    #[error("invalid droid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// An unsupported or malformed protocol message was received.
    #[error("droid protocol: {0}")]
    Protocol(&'static str),
    /// A CLI argument would bypass permissions or override the protocol format.
    #[error("droid launch argument is not allowed: {0}")]
    ForbiddenArgument(&'static str),
    /// The CLI reported a version outside the configured range.
    #[error("untested droid version {version} (supported: {policy})")]
    UnsupportedVersion {
        /// The CLI's reported version.
        version: DroidVersion,
        /// The configured supported range.
        policy: VersionPolicy,
    },
    /// The CLI did not report a valid semantic version.
    #[error("invalid droid version: {0}")]
    InvalidVersion(#[from] semver::Error),
    /// A preflight command exited without a usable result.
    #[error("droid {command} exited with {status}")]
    PreflightExit {
        /// The command that failed.
        command: &'static str,
        /// Its process exit status.
        status: std::process::ExitStatus,
    },
    /// The transport closed before the requested operation completed.
    #[error("droid exited before {0} completed")]
    Exited(&'static str),
    /// The peer did not complete an operation within its deadline.
    #[error("timed out waiting for droid {0}")]
    Timeout(&'static str),
    /// Droid rejected a JSON-RPC request.
    #[error("droid RPC error {0}: {1}")]
    Rpc(i64, String),
    /// The startup handshake did not confirm the requested settings.
    #[error("droid settings did not confirm the requested {0}")]
    SettingsMismatch(&'static str),
    /// A message was attempted before a successful startup handshake or after abort.
    #[error("droid session is not confirmed")]
    NotConfirmed,
    /// Initialization was already attempted.
    #[error("droid session initialization was already attempted")]
    AlreadyInitialized,
    /// A previous turn has not ended.
    #[error("a droid turn is still active")]
    TurnInProgress,
}
