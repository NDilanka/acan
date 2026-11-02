//! Acan's single model client. Every model call goes through [`ModelClient`].
//!
//! Providers are configuration. [`LlmConfig`] lists OpenAI-compatible endpoints and routes each
//! [`Role`] to one of them. [`OpenAiCompatible`] talks to the configured providers, and [`Stub`]
//! answers from a script for tests and offline demos.

mod chat;
mod client;
mod config;
mod error;
mod key;
mod openai;
mod plan;
mod providers;
mod role;
mod setup;
mod stub;
mod trace;
mod usage;

pub use chat::{
    ChatRequest, ChatResponse, ContentPart, FunctionCall, ImageUrl, JsonSchemaFormat, Message,
    ToolCall, ToolKind, ToolSpec,
};
pub use client::{Destination, MUSE_SPARK_MODELS, MUSE_SPARK_PROVIDER, ModelClient, Structured};
pub use config::{ConfigError, LlmConfig, ProviderConfig, StructuredOutput, TokenLimitField};
pub use error::{LlmError, UnknownRole};
pub use key::{ApiKey, ApiKeys};
pub use openai::{OpenAiCompatible, RetryPolicy};
pub use plan::{
    KeyOrigin, Plan, ProviderStatus, RoleStatus, environment_keys, resolve, routing_roles,
};
pub use providers::{KeyCheck, PROVIDERS, catalog, check_key, is_provider};
pub use role::Role;
pub use setup::{
    KEYS_FILE, KeyFile, KeyOutcome, KeySource, KeyStatus, SavedSetup, SetupError, save_checked,
};
pub use stub::{Reply, Stub};
pub use trace::{RequestId, RequestIdSource, Traced};
pub use usage::{CallRecord, Usage, UsageLedger, UsageTotals};
