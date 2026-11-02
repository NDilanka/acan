//! The model client the server runs: the providers in a routing file, or the one provider whose key
//! was saved from the setup page, with the offline replies behind them.
//!
//! A repository's routing file decides the providers, and its keys come from the environment or
//! its `.env`. Without one, a key saved from the setup page picks one provider (see
//! [`acan_llm::preset`]). Until a key is in use, calls get the offline replies. A key saved while
//! the server runs takes effect at once, so no restart is needed.
//!
//! A foreman call never falls back. When it fails, its job fails with the provider error, so no
//! offline split or draft stands in for a live one. Other roles keep a labelled offline answer for
//! a call that fails. Review is retired: its calls are refused before any provider is asked. Live
//! and offline calls are metered in one ledger, so the usage panel shows which calls were offline.

use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};

use acan_llm::{
    ApiKey, ApiKeys, ChatRequest, ChatResponse, ConfigError, KeyFile, KeyOutcome, KeySource,
    KeyStatus, LlmConfig, LlmError, ModelClient, OpenAiCompatible, Role, SavedKey, SetupError,
    Structured, UsageLedger, preset, save_checked,
};
use anyhow::Context;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::model::{self, Ask};
use crate::offline::Offline;
use crate::release;

/// The client behind every model call. Routing is configuration; this type only decides where a
/// call goes.
pub struct Models {
    ledger: UsageLedger,
    offline: Offline,
    /// The live client, when one is in use. A key saved from the setup page replaces it.
    live: RwLock<Option<Arc<Live>>>,
    /// Where a key saved from the setup page is kept. `None` when a routing file decides, or when
    /// the user has no config folder.
    keys_file: Option<KeyFile>,
    /// Whether a routing file decides the providers.
    routing: bool,
}

/// A live client and what it was built from.
struct Live {
    client: OpenAiCompatible,
    /// Whether a provider has a key. Without one, live calls fail with a missing key.
    has_key: bool,
    /// The provider a saved key belongs to. `None` when a routing file chose the providers.
    saved_provider: Option<String>,
}

impl Models {
    /// Loads the routing file at `path`. When the file is absent, falls back to the offline
    /// replies.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or validated.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Self::open(path, None)
    }

    /// Loads the routing file at `path`, or, when there is none, the key saved in `keys_file`.
    ///
    /// A routing file decides everything, so `keys_file` is not read when the file exists. A saved
    /// key that cannot be read is logged, and the server answers with the offline replies, so the
    /// setup page can replace it.
    ///
    /// # Errors
    ///
    /// Returns an error when the routing file exists but cannot be read or validated.
    pub fn open(path: &Path, keys_file: Option<KeyFile>) -> anyhow::Result<Self> {
        if path.exists() {
            let config =
                LlmConfig::load(path).with_context(|| format!("loading {}", path.display()))?;
            let keys = ApiKeys::from_env(&config);
            let models = Self::build(None, true);
            models.set_live(config, keys, None)?;
            return Ok(models);
        }

        let models = Self::build(keys_file, false);
        match models.keys_file.as_ref().map(KeyFile::load) {
            Some(Ok(Some(saved))) => models.activate_saved(saved)?,
            Some(Err(err)) => tracing::warn!(
                error = %err,
                "the saved model key cannot be read; answering with built-in replies"
            ),
            Some(Ok(None)) | None => {}
        }
        if models.active().is_none() {
            tracing::warn!("no model key is set; model jobs get built-in offline replies");
        }
        Ok(models)
    }

    /// Builds the live client for `config` with `keys`. A failed non-foreman call is answered
    /// offline.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Client`] when the HTTP client cannot be built.
    pub fn live(config: LlmConfig, keys: ApiKeys) -> Result<Self, ConfigError> {
        let models = Self::build(None, true);
        models.set_live(config, keys, None)?;
        Ok(models)
    }

    /// The usage ledger that records every call this client makes, live or offline.
    #[must_use]
    pub fn ledger(&self) -> UsageLedger {
        self.ledger.clone()
    }

    fn build(keys_file: Option<KeyFile>, routing: bool) -> Self {
        let ledger = UsageLedger::default();
        Self {
            offline: Offline::with_ledger(ledger.clone()),
            ledger,
            live: RwLock::new(None),
            keys_file,
            routing,
        }
    }

    /// Uses the provider a saved key belongs to. A provider name the presets do not know is logged
    /// and ignored, so the server answers offline.
    fn activate_saved(&self, saved: SavedKey) -> anyhow::Result<()> {
        let SavedKey { provider, key } = saved;
        let Some(config) = preset(&provider) else {
            tracing::warn!("the saved model key names a provider the setup page does not offer");
            return Ok(());
        };
        release::add_secret(key.secret_text());
        let keys = ApiKeys::from_env(&config).with_fallback(&provider, key);
        self.set_live(config, keys, Some(provider))?;
        Ok(())
    }

    fn set_live(
        &self,
        config: LlmConfig,
        keys: ApiKeys,
        saved_provider: Option<String>,
    ) -> Result<(), ConfigError> {
        let has_key = !keys.is_empty();
        let client = OpenAiCompatible::new(config, keys)?.with_ledger(self.ledger.clone());
        let live = Live {
            client,
            has_key,
            saved_provider,
        };
        *self.live.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(live));
        Ok(())
    }

    /// The live client in use now, if there is one. The lock is released before this returns, so
    /// no call holds it across an await.
    fn active(&self) -> Option<Arc<Live>> {
        self.live
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
        let Some(live) = self.active() else {
            return self.offline.chat(role, request).await;
        };
        let err = match live.client.chat(role, request.clone()).await {
            Ok(response) => return Ok(response),
            Err(err) => err,
        };
        if !answers_offline(role) {
            warn_failed(role, &err, "the job fails with this error");
            return Err(err);
        }
        warn_failed(role, &err, "answering with the offline reply");
        self.offline.chat(role, request).await
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
        let Some(live) = self.active() else {
            return self.offline.chat_json::<T>(role, request).await;
        };
        // The live result ends its statement before the fallback awaits, so the future stays
        // Send even when `T` is not.
        let err = match live.client.chat_json::<T>(role, request.clone()).await {
            Ok(structured) => return Ok(structured),
            Err(err) => err,
        };
        if !answers_offline(role) {
            warn_failed(role, &err, "the job fails with this error");
            return Err(err);
        }
        warn_failed(role, &err, "answering with the offline reply");
        self.offline.chat_json::<T>(role, request).await
    }

    fn key_status(&self) -> KeyStatus {
        let can_save = self.keys_file.is_some() && !self.routing;
        let live = self.active();
        let configured = live.as_ref().is_some_and(|live| live.has_key);
        let provider = live
            .filter(|_| configured)
            .and_then(|live| live.saved_provider.clone());
        let source = if self.routing {
            Some(KeySource::RoutingFile)
        } else if configured {
            Some(KeySource::SavedKey)
        } else {
            None
        };
        KeyStatus {
            configured,
            provider,
            source,
            can_save,
        }
    }

    async fn save_key(&self, provider: &str, key: &str) -> Result<KeyOutcome, SetupError> {
        let file = match (&self.keys_file, self.routing) {
            (_, true) => return Err(SetupError::RoutingFile),
            (None, false) => return Err(SetupError::Unavailable),
            (Some(file), false) => file,
        };
        let config = preset(provider).ok_or(SetupError::UnknownProvider)?;
        let outcome = save_checked(file, &config, provider, key).await?;
        if matches!(outcome, KeyOutcome::Stored { .. }) {
            release::add_secret(key);
            let keys = ApiKeys::from_env(&config).with_fallback(provider, ApiKey::new(key));
            self.set_live(config, keys, Some(provider.to_owned()))?;
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SAVED: &str = "meta-saved-key-0123456789";

    fn keys_file(dir: &tempfile::TempDir) -> KeyFile {
        KeyFile::new(dir.path().join("acan").join("keys.toml"))
    }

    #[test]
    fn a_saved_key_selects_its_provider_and_the_status_never_holds_the_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = keys_file(&dir);
        file.save("meta", SAVED).expect("saves");

        let models =
            Models::open(&dir.path().join("no-routing.toml"), Some(file)).expect("the models open");

        let status = models.key_status();
        assert!(status.configured);
        assert_eq!(status.provider.as_deref(), Some("meta"));
        assert_eq!(status.source, Some(KeySource::SavedKey));
        assert!(status.can_save);
        let text = serde_json::to_string(&status).expect("the status serializes");
        assert!(!text.contains(SAVED), "the status carries no key");
    }

    #[test]
    fn without_a_key_calls_are_offline_and_a_key_can_be_saved() {
        let dir = tempfile::tempdir().expect("temp dir");
        let models = Models::open(&dir.path().join("no-routing.toml"), Some(keys_file(&dir)))
            .expect("the models open");

        let status = models.key_status();
        assert!(!status.configured);
        assert_eq!(status.source, None);
        assert!(status.can_save);
    }

    #[test]
    fn a_routing_file_decides_and_refuses_saves() {
        let dir = tempfile::tempdir().expect("temp dir");
        let routing = dir.path().join("llm.toml");
        std::fs::write(
            &routing,
            r#"
[[providers]]
name = "meta"
base_url = "http://127.0.0.1:9/v1"
api_key_env = "ACAN_TEST_UNSET_META_KEY"
default_model = "muse-spark-1.3"

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "meta"

[routing.fast]
provider = "meta"
"#,
        )
        .expect("writes the routing file");
        let models = Models::open(&routing, Some(keys_file(&dir))).expect("the models open");

        let status = models.key_status();
        assert!(!status.configured, "the test environment has no key");
        assert!(!status.can_save);
        assert_eq!(status.source, Some(KeySource::RoutingFile));
        assert_eq!(status.provider, None);
    }

    #[tokio::test]
    async fn a_saved_key_takes_effect_without_a_restart() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "index": 0,
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "answered live"}
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("temp dir");
        let models = Models::open(&dir.path().join("no-routing.toml"), Some(keys_file(&dir)))
            .expect("the models open");
        let request = || ChatRequest {
            messages: vec![acan_llm::Message::user("hello")],
            ..ChatRequest::default()
        };
        assert!(
            models.chat(Role::Worker, request()).await.is_err(),
            "with no key, a non-model call has no offline reply"
        );

        let config = LlmConfig::from_toml_str(&format!(
            r#"
[[providers]]
name = "meta"
base_url = "{}/v1"
api_key_env = "META_API_KEY"
default_model = "muse-spark-1.3"

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "meta"

[routing.fast]
provider = "meta"
"#,
            server.uri()
        ))
        .expect("the test config is valid");
        let mut keys = ApiKeys::default();
        keys.insert("meta", ApiKey::new(SAVED));
        models
            .set_live(config, keys, Some("meta".to_owned()))
            .expect("the live client builds");

        let response = models
            .chat(Role::Worker, request())
            .await
            .expect("the live provider answers");
        assert_eq!(response.content.as_deref(), Some("answered live"));
        assert!(models.key_status().configured);
    }
}
