//! The model client the server runs: the providers in a routing file, with the offline replies
//! behind them.
//!
//! A foreman call never falls back. When it fails, its job fails with the provider error, so no
//! offline split or draft stands in for a live one. Other roles keep a labelled offline answer
//! for a call that fails. Review is retired: its calls are refused before any provider is asked.
//! Live and offline calls are metered in one ledger, so the usage panel shows which calls were
//! offline.

use std::path::Path;

use acan_llm::{
    ApiKeys, ChatRequest, ChatResponse, ConfigError, LlmConfig, LlmError, ModelClient,
    OpenAiCompatible, Role, Structured, Traced, UsageLedger,
};
use anyhow::Context;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::model::{self, Ask};
use crate::offline::Offline;

/// The client behind every model call. Routing is configuration; this enum only picks the source.
#[expect(
    clippy::large_enum_variant,
    reason = "one value per server, built once and never moved in a hot path"
)]
pub enum Models {
    /// Calls the configured OpenAI-compatible providers. A failed call for a role other than the
    /// foreman is answered by `offline`. A failed foreman call returns its error.
    Live {
        client: OpenAiCompatible,
        offline: Offline,
    },
    /// Answers split and explain from built-in replies. Used when there is no routing file.
    /// Review, scope and probe calls are refused.
    Offline(Offline),
}

impl Models {
    /// Loads the routing file at `path`. When the file is absent, falls back to the offline
    /// replies.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or validated.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::warn!(
                config = %path.display(),
                "no model routing file; model jobs get built-in offline replies"
            );
            return Ok(Self::Offline(Offline::default()));
        }
        let config =
            LlmConfig::load(path).with_context(|| format!("loading {}", path.display()))?;
        let keys = ApiKeys::from_env(&config);
        Ok(Self::live(config, keys)?)
    }

    /// Builds the live client for `config` with `keys`. A failed non-foreman call is answered
    /// offline.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Client`] when the HTTP client cannot be built.
    pub fn live(config: LlmConfig, keys: ApiKeys) -> Result<Self, ConfigError> {
        let ledger = UsageLedger::default();
        let client = OpenAiCompatible::new(config, keys)?.with_ledger(ledger.clone());
        Ok(Self::Live {
            client,
            offline: Offline::with_ledger(ledger),
        })
    }

    /// The usage ledger that records every call this client makes, live or offline.
    #[must_use]
    pub fn ledger(&self) -> UsageLedger {
        match self {
            Self::Live { client, .. } => client.ledger().clone(),
            Self::Offline(offline) => offline.ledger().clone(),
        }
    }
}

/// Whether a failed call for `role` may be answered offline. A foreman call never is.
fn answers_offline(role: Role) -> bool {
    role != Role::Foreman
}

/// Whether `request` is a review. The server no longer sends review to any model.
fn is_retired(request: &ChatRequest) -> bool {
    model::ask_of(request) == Some(Ask::Review)
}

/// The error for a refused review. No provider is asked, so the job fails with this error.
///
/// `LlmError` has no refusal variant, so this reuses `NoScriptedReply`. The warning names the
/// actual cause.
fn refuse_review(role: Role) -> LlmError {
    tracing::warn!(
        %role,
        "model review is retired from the server; the review call was not sent"
    );
    LlmError::NoScriptedReply { role }
}

/// Logs a failed live call. A reply that does not match the schema also logs the parser's detail
/// and the raw reply, since that is what shows why the model drifted.
fn warn_failed(role: Role, err: &LlmError, outcome: &'static str) {
    if let LlmError::SchemaValidation { detail, raw, .. } = err {
        tracing::warn!(%role, error = %err, %detail, %raw, outcome, "live model call failed");
    } else {
        tracing::warn!(%role, error = %err, outcome, "live model call failed");
    }
}

impl ModelClient for Models {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        if is_retired(&request) {
            return Err(refuse_review(role));
        }
        match self {
            Self::Live { client, offline } => {
                let err = match client.chat(role, request.clone()).await {
                    Ok(response) => return Ok(response),
                    Err(err) => err,
                };
                if !answers_offline(role) {
                    warn_failed(role, &err, "the job fails with this error");
                    return Err(err);
                }
                warn_failed(role, &err, "answering with the offline reply");
                offline.chat(role, request).await
            }
            Self::Offline(offline) => offline.chat(role, request).await,
        }
    }

    async fn chat_json<T>(
        &self,
        role: Role,
        request: ChatRequest,
    ) -> Result<Structured<T>, LlmError>
    where
        T: DeserializeOwned + JsonSchema,
    {
        if is_retired(&request) {
            return Err(refuse_review(role));
        }
        match self {
            Self::Live { client, offline } => {
                // The live result ends its statement before the fallback awaits, so the future
                // stays Send even when `T` is not.
                let err = match client.chat_json::<T>(role, request.clone()).await {
                    Ok(structured) => return Ok(structured),
                    Err(err) => err,
                };
                if !answers_offline(role) {
                    warn_failed(role, &err, "the job fails with this error");
                    return Err(err);
                }
                warn_failed(role, &err, "answering with the offline reply");
                offline.chat_json::<T>(role, request).await
            }
            Self::Offline(offline) => offline.chat_json::<T>(role, request).await,
        }
    }

    async fn chat_traced(
        &self,
        role: Role,
        request: ChatRequest,
    ) -> Result<Traced<ChatResponse>, LlmError> {
        if is_retired(&request) {
            return Err(refuse_review(role));
        }
        match self {
            Self::Live { client, offline } => {
                let err = match client.chat_traced(role, request.clone()).await {
                    Ok(traced) => return Ok(traced),
                    Err(err) => err,
                };
                if !answers_offline(role) {
                    warn_failed(role, &err, "the job fails with this error");
                    return Err(err);
                }
                warn_failed(role, &err, "answering with the offline reply");
                offline.chat_traced(role, request).await
            }
            Self::Offline(offline) => offline.chat_traced(role, request).await,
        }
    }
}
