use crate::config::ConfigError;
use crate::role::Role;

/// A role name that is not foreman, worker, fast, or vision.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown role \"{0}\", expected foreman, worker, fast, or vision")]
pub struct UnknownRole(pub String);

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
    /// replaced.
    #[error("reply does not match the requested schema")]
    SchemaValidation {
        provider: String,
        detail: String,
        raw: String,
    },

    #[error("no scripted reply for role {role}")]
    NoScriptedReply { role: Role },
}
