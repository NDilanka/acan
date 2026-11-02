//! The model client the server runs: the providers in a routing file, with the offline replies
//! behind them.
//!
//! A foreman or director call never falls back. When it fails, its job fails with the provider
//! error, so no offline split, draft or tour stands in for a live one. Other roles keep a labelled
//! offline answer for a call that fails. Review is retired: its calls are refused before any
//! provider is asked.
//! Live and offline calls are metered in one ledger, so the usage panel shows which calls were
//! offline.

use std::path::Path;

use acan_llm::{
    ApiKeys, ChatRequest, ChatResponse, ConfigError, Destination, LlmConfig, LlmError, ModelClient,
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
    /// foreman, vision or the director is answered by `offline`. A failed foreman, vision or
    /// director call returns its error.
    Live {
        client: OpenAiCompatible,
        offline: Offline,
    },
    /// Answers split and explain from built-in replies. Used when there is no routing file.
    /// Review, scope and probe calls are refused. Offline has no route, so the director has no
    /// Muse Spark destination and the plant view serves the deterministic tour.
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

    /// Builds the live client for `config` with `keys`. A failed call for a role other than the
    /// foreman, vision or the director is answered offline.
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

/// Whether a failed call for `role` may be answered offline. A foreman, vision or director call
/// never is: a tour the offline client made up would pass for a directed one.
fn answers_offline(role: Role) -> bool {
    !matches!(role, Role::Foreman | Role::Vision | Role::Director)
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
    fn destination(&self, role: Role) -> Option<Destination> {
        match self {
            Self::Live { client, .. } => client.destination(role),
            Self::Offline(_) => None,
        }
    }

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

#[cfg(test)]
mod tests {
    use acan_llm::{
        ApiKeys, Destination, LlmConfig, MUSE_SPARK_MODELS, MUSE_SPARK_PROVIDER, ModelClient, Role,
    };

    use super::{Models, answers_offline};
    use crate::offline::Offline;

    /// Two providers, and a foreman route with no director route of its own.
    const CONFIG: &str = r#"
[[providers]]
name = "meta"
base_url = "https://example.invalid/v1"
api_key_env = "META_API_KEY"
default_model = "muse-spark-1.3"

[[providers]]
name = "baseten"
base_url = "https://example.invalid/v1"
api_key_env = "BASETEN_API_KEY"
default_model = "meta-models/muse-spark-open-1026"

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "baseten"

[routing.fast]
provider = "baseten"
"#;

    #[test]
    fn a_director_or_foreman_call_is_never_answered_offline() {
        assert!(!answers_offline(Role::Director));
        assert!(!answers_offline(Role::Foreman));
        assert!(answers_offline(Role::Fast));
        assert!(answers_offline(Role::Worker));
    }

    #[test]
    fn a_live_director_is_muse_spark_on_the_foremans_route() {
        let config = LlmConfig::from_toml_str(CONFIG).expect("the config is valid");
        let keys = ApiKeys::from_env(&config);
        let models = Models::live(config, keys).expect("the client builds");

        let destination = models
            .destination(Role::Director)
            .expect("a live director has a route");

        assert_eq!(
            destination,
            Destination {
                provider: MUSE_SPARK_PROVIDER.to_owned(),
                model: MUSE_SPARK_MODELS[0].to_owned(),
            }
        );
        assert!(destination.is_muse_spark());
    }

    #[test]
    fn an_offline_client_has_no_destination_for_any_role() {
        let models = Models::Offline(Offline::default());

        assert!(models.destination(Role::Director).is_none());
        assert!(models.destination(Role::Foreman).is_none());
    }
}
