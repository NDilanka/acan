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
mod preset;
mod role;
mod setup;
mod stub;
mod trace;
mod usage;

pub use chat::{
    ChatRequest, ChatResponse, FunctionCall, JsonSchemaFormat, Message, ToolCall, ToolKind,
    ToolSpec,
};
pub use client::{ModelClient, Structured};
pub use config::{ConfigError, LlmConfig, ProviderConfig, StructuredOutput, TokenLimitField};
pub use error::{LlmError, UnknownRole};
pub use key::{ApiKey, ApiKeys};
pub use openai::{OpenAiCompatible, RetryPolicy};
pub use preset::{KeyCheck, PRESETS, check_key, preset};
pub use role::Role;
pub use setup::{
    KEYS_FILE, KeyFile, KeyOutcome, KeySource, KeyStatus, SavedKey, SetupError, save_checked,
};
pub use stub::{Reply, Stub};
pub use trace::{RequestId, RequestIdSource, Traced};
pub use usage::{CallRecord, Usage, UsageLedger, UsageTotals};
