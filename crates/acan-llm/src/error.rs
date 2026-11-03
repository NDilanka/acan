use crate::config::ConfigError;
use crate::role::Role;
use crate::usage::CallRecord;

/// A role name that is not foreman, worker, fast, or vision.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown role \"{0}\", expected foreman, worker, fast, or vision")]
pub struct UnknownRole(pub String);

/// The accounting of a reply the provider sent but Acan could not use: the call's tokens and
/// latency, and the request id the provider sent, when it sent one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTrace {
    pub call: CallRecord,
    pub request_id: Option<String>,
}

/// Why a model call failed. Each HTTP status class and timeout has its own variant.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LlmError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("no key for provider {provider}, set {env_var} in the environment or .env")]
    MissingKey { provider: String, env_var: String },

    /// `body` is the provider's reply with configured keys replaced, cut to 512 bytes.
    #[error("{provider} returned 402 payment required")]
    PaymentRequired { provider: String, body: String },

    #[error("{provider} still rate limited after {attempts} attempts")]
    RateLimited { provider: String, attempts: u32 },

    #[error("{provider} still returned {status} after {attempts} attempts")]
    ServerError {
        provider: String,
        status: u16,
        attempts: u32,
    },

    /// `body` is the provider's reply with configured keys replaced, cut to 512 bytes.
    #[error("{provider} returned HTTP {status}")]
    Status {
        provider: String,
        status: u16,
        body: String,
    },

    #[error("{provider} timed out")]
    Timeout { provider: String },

    #[error("request to {provider} failed")]
    Transport {
        provider: String,
        #[source]
        source: reqwest::Error,
    },

    /// `detail` is the parser message with configured keys replaced.
    #[error("{provider} sent a response Acan cannot read")]
    MalformedResponse { provider: String, detail: String },

    /// `detail` is the parser message. `raw` is the full reply text. Both have configured keys
    /// replaced. `trace` is the call that produced the reply.
    #[error("reply does not match the requested schema")]
    SchemaValidation {
        provider: String,
        detail: String,
        raw: String,
        trace: Box<ReplyTrace>,
    },

    /// The reply stopped at its output cap before it finished, so its text is incomplete.
    /// `max_tokens` is the cap the request set, or `None` when the provider's default applied.
    /// `trace` is the call that produced the reply, with the tokens it used.
    #[error("{provider} stopped the reply at {} before it finished", cap_text(.max_tokens))]
    OutputCapped {
        provider: String,
        max_tokens: Option<u32>,
        trace: Box<ReplyTrace>,
    },

    #[error("no scripted reply for role {role}")]
    NoScriptedReply { role: Role },

    /// A role that needs Meta is off, because no Meta key is set. No request is sent. The role
    /// has no route, so no other provider can answer for it.
    #[error("{role} is off: {reason}")]
    RoleOff { role: Role, reason: &'static str },
}

impl LlmError {
    /// The trace of the reply this error came from, when the provider sent one: a reply cut at
    /// the output cap, or one that does not parse. Transport and status errors have none.
    #[must_use]
    pub fn reply_trace(&self) -> Option<&ReplyTrace> {
        match self {
            Self::OutputCapped { trace, .. } | Self::SchemaValidation { trace, .. } => Some(trace),
            _ => None,
        }
    }
}

fn cap_text(max_tokens: &Option<u32>) -> String {
    match max_tokens {
        Some(cap) => format!("the {cap}-token output cap"),
        None => "the provider's default output cap".to_owned(),
    }
}
