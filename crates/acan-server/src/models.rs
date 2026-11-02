//! The model client the server runs: the providers and role picks a tester saved from the setup
//! page, or the providers in a repository's routing file, with the offline replies behind them.
//!
//! A repository's routing file decides the providers, and its keys come from the environment or
//! its `.env`. A role that needs Meta is off unless its route runs Muse Spark. Without a routing
//! file, each provider with a key (saved from the setup page, or set in the environment) serves
//! the roles it is picked for, or the default provider for each role. A role with no provider is
//! off, and the setup page says why. A role that needs Meta is off without a Meta key. An off role
//! is refused before any request is sent, to any provider. Until every other role has a provider,
//! calls get the offline replies. A key or pick saved while the server runs takes effect at once.
//!
//! A foreman or vision call never falls back. When it fails, its job fails with the provider error,
//! so no offline split or draft stands in for a live one. Other roles keep a labelled offline
//! answer for a call that fails. Review is retired: its calls are refused before any provider is asked. Live
//! and offline calls are metered in one ledger, so the usage panel shows which calls were offline.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use acan_llm::{
    ApiKeys, ChatRequest, ChatResponse, ConfigError, KeyFile, KeyOrigin, KeyOutcome, KeySource,
    KeyStatus, LlmConfig, LlmError, ModelClient, OpenAiCompatible, Plan, ProviderStatus, Role,
    SavedSetup, SetupError, Structured, Traced, UsageLedger, catalog, environment_keys,
    is_provider, resolve, routing_roles, save_checked,
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
    /// The live client and the status the setup page shows. Replaced whole on each change, so a
    /// call reads one consistent state.
    state: RwLock<Arc<State>>,
    /// Held while a change to the keys file is read and the state rebuilt from it. The last change
    /// to finish then leaves the state that matches the file.
    changes: Mutex<()>,
    /// Where keys and role picks saved from the setup page are kept. `None` when the user has no
    /// config folder.
    keys_file: Option<KeyFile>,
    /// Whether a routing file decides the providers.
    routing: bool,
}

/// The live client, when every role has a provider, and the status the setup page shows.
struct State {
    /// `None` until every role has a provider. Then calls get the offline replies.
    live: Option<OpenAiCompatible>,
    status: KeyStatus,
}

impl State {
    /// The state for a setup from the setup page: the plan's providers, picks and keys. `can_save`
    /// is whether the setup page can write keys here.
    fn from_plan(plan: Plan, can_save: bool, ledger: &UsageLedger) -> Result<Self, ConfigError> {
        for (_, key) in plan.keys.iter() {
            release::add_secret(key.secret_text());
        }
        let status = KeyStatus {
            configured: plan.config.is_some(),
            source: (!plan.keys.is_empty()).then_some(KeySource::Setup),
            can_save,
            providers: plan.providers,
            roles: plan.roles,
        };
        let live = match plan.config {
            Some(config) => {
                Some(OpenAiCompatible::new(config, plan.keys)?.with_ledger(ledger.clone()))
            }
            None => None,
        };
        Ok(Self { live, status })
    }

    /// The state for a routing file: every role uses the provider the file names for it.
    fn from_routing(
        config: LlmConfig,
        keys: ApiKeys,
        ledger: &UsageLedger,
    ) -> Result<Self, ConfigError> {
        for (_, key) in keys.iter() {
            release::add_secret(key.secret_text());
        }
        let providers = config
            .providers()
            .iter()
            .map(|provider| ProviderStatus::new(&provider.name, false, keys.has(&provider.name)))
            .collect();
        let roles = routing_roles(&config);
        let status = KeyStatus {
            configured: !keys.is_empty(),
            source: Some(KeySource::RoutingFile),
            can_save: false,
            providers,
            roles,
        };
        let live = OpenAiCompatible::new(config, keys)?.with_ledger(ledger.clone());
        Ok(Self {
            live: Some(live),
            status,
        })
    }
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

    /// Loads the routing file at `path`, or, when there is none, the keys and picks saved in
    /// `keys_file` and the keys the environment sets.
    ///
    /// A routing file decides everything, so `keys_file` is not read when the file exists. Saved
    /// setup that cannot be read is logged, and the server answers with the offline replies until
    /// the setup page replaces it.
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
            models.replace(State::from_routing(config, keys, &models.ledger)?);
            return Ok(models);
        }

        let models = Self::build(keys_file, false);
        let saved = match models.keys_file.as_ref().map(KeyFile::load) {
            Some(Ok(saved)) => saved,
            Some(Err(err)) => {
                tracing::warn!(
                    error = %err,
                    "the saved model keys cannot be read; answering with built-in replies"
                );
                SavedSetup::default()
            }
            None => SavedSetup::default(),
        };
        models.install(&saved)?;
        if !models.current().status.configured {
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
        models.replace(State::from_routing(config, keys, &models.ledger)?);
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
            state: RwLock::new(Arc::new(State {
                live: None,
                status: KeyStatus::default(),
            })),
            changes: Mutex::new(()),
            keys_file,
            routing,
        }
    }

    /// The state in use now. The lock is released before this returns, so no call holds it across
    /// an await.
    fn current(&self) -> Arc<State> {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn replace(&self, state: State) {
        *self.state.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(state);
    }

    /// Resolves `saved` against the environment and makes the result the state in use.
    fn install(&self, saved: &SavedSetup) -> Result<(), ConfigError> {
        let environment = environment_keys()?;
        let plan = resolve(saved, &environment)?;
        let state = State::from_plan(plan, self.keys_file.is_some(), &self.ledger)?;
        self.replace(state);
        Ok(())
    }

    /// Rebuilds the state from the keys file after a change was written to it.
    fn sync_from(&self, file: &KeyFile) -> Result<(), SetupError> {
        let _changes = self.changes.lock().unwrap_or_else(PoisonError::into_inner);
        let saved = file.load().map_err(SetupError::Write)?;
        self.install(&saved)?;
        Ok(())
    }

    /// The keys file a change may be written to. A routing file decides everything, so none is.
    fn writable(&self) -> Result<&KeyFile, SetupError> {
        match (&self.keys_file, self.routing) {
            (_, true) => Err(SetupError::RoutingFile),
            (None, false) => Err(SetupError::Unavailable),
            (Some(file), false) => Ok(file),
        }
    }
}

/// Whether a failed call for `role` may be answered offline. Foreman calls never are.
fn answers_offline(role: Role) -> bool {
    !matches!(role, Role::Foreman | Role::Vision)
}

/// The refusal for a call to a role that is off for want of a Meta key. Such a role has no route,
/// so the call is not sent to any provider and gets no offline reply. `None` when the call may go
/// on.
fn off_refusal(state: &State, role: Role) -> Option<LlmError> {
    if !role.needs_meta() {
        return None;
    }
    let reason = state.status.role(role)?.off_reason?;
    Some(LlmError::RoleOff { role, reason })
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
        let state = self.current();
        if let Some(refusal) = off_refusal(&state, role) {
            return Err(refusal);
        }
        let Some(live) = state.live.as_ref() else {
            return self.offline.chat(role, request).await;
        };
        let err = match live.chat(role, request.clone()).await {
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
        let state = self.current();
        if let Some(refusal) = off_refusal(&state, role) {
            return Err(refusal);
        }
        let Some(live) = state.live.as_ref() else {
            return self.offline.chat_json::<T>(role, request).await;
        };
        // The live result ends its statement before the fallback awaits, so the future stays
        // Send even when `T` is not.
        let err = match live.chat_json::<T>(role, request.clone()).await {
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

    async fn chat_traced(
        &self,
        role: Role,
        request: ChatRequest,
    ) -> Result<Traced<ChatResponse>, LlmError> {
        if is_retired(&request) {
            return Err(refuse_review(role));
        }
        let state = self.current();
        if let Some(refusal) = off_refusal(&state, role) {
            return Err(refusal);
        }
        let Some(live) = state.live.as_ref() else {
            return self.offline.chat_traced(role, request).await;
        };
        let err = match live.chat_traced(role, request.clone()).await {
            Ok(traced) => return Ok(traced),
            Err(err) => err,
        };
        if !answers_offline(role) {
            warn_failed(role, &err, "the job fails with this error");
            return Err(err);
        }
        warn_failed(role, &err, "answering with the offline reply");
        self.offline.chat_traced(role, request).await
    }

    fn key_status(&self) -> KeyStatus {
        self.current().status.clone()
    }

    async fn save_key(&self, provider: &str, key: &str) -> Result<KeyOutcome, SetupError> {
        let file = self.writable()?;
        if !is_provider(provider) {
            return Err(SetupError::UnknownProvider);
        }
        let config = catalog()?;
        let outcome = save_checked(file, &config, provider, key).await?;
        if matches!(outcome, KeyOutcome::Stored { .. }) {
            self.sync_from(file)?;
        }
        Ok(outcome)
    }

    fn remove_key(&self, provider: &str) -> Result<bool, SetupError> {
        let file = self.writable()?;
        if !is_provider(provider) {
            return Err(SetupError::UnknownProvider);
        }
        // The key in use decides. The environment's key is not the page's to remove, and a saved
        // key under it stays until the environment key is unset.
        let from_environment =
            self.current().status.providers.iter().any(|known| {
                known.name == provider && known.source == Some(KeyOrigin::Environment)
            });
        if from_environment {
            return Err(SetupError::FromEnvironment);
        }
        let removed = file.remove_key(provider)?;
        self.sync_from(file)?;
        Ok(removed)
    }

    fn set_role(&self, role: Role, provider: Option<&str>) -> Result<(), SetupError> {
        let file = self.writable()?;
        if let Some(name) = provider {
            let has_key = self
                .current()
                .status
                .providers
                .iter()
                .any(|known| known.name == name && (known.saved || known.environment));
            if !has_key {
                return Err(if is_provider(name) {
                    SetupError::NoKey
                } else {
                    SetupError::UnknownProvider
                });
            }
        }
        file.set_role(role, provider)?;
        self.sync_from(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SAVED: &str = "meta-saved-key-0123456789";

    fn keys_file(dir: &tempfile::TempDir) -> KeyFile {
        KeyFile::new(dir.path().join("acan").join("keys.toml"))
    }

    fn models_with(dir: &tempfile::TempDir, file: &KeyFile) -> Models {
        Models::open(&dir.path().join("no-routing.toml"), Some(file.clone())).expect("opens")
    }

    #[test]
    fn a_saved_key_serves_the_roles_and_the_status_never_holds_the_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = keys_file(&dir);
        file.set_key("meta", SAVED).expect("saves");

        let models = models_with(&dir, &file);

        let status = models.key_status();
        assert!(status.configured);
        assert_eq!(status.source, Some(KeySource::Setup));
        assert!(status.can_save);
        let meta = status
            .providers
            .iter()
            .find(|provider| provider.name == "meta")
            .expect("meta is listed");
        assert!(meta.saved);
        assert_eq!(status.roles[0].provider.as_deref(), Some("meta"));
        let text = serde_json::to_string(&status).expect("the status serializes");
        assert!(!text.contains(SAVED), "the status carries no key");
    }

    #[test]
    fn without_a_key_every_role_is_off_and_the_status_says_so() {
        let dir = tempfile::tempdir().expect("temp dir");
        let models = models_with(&dir, &keys_file(&dir));

        let status = models.key_status();
        assert!(!status.configured);
        assert_eq!(status.source, None);
        assert!(status.can_save);
        assert!(status.roles.iter().all(|role| role.off_reason.is_some()));
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
        assert_eq!(status.roles[1].provider.as_deref(), Some("meta"));
        assert!(matches!(
            models.set_role(Role::Worker, None),
            Err(SetupError::RoutingFile)
        ));
    }

    #[test]
    fn a_key_saved_while_running_takes_effect_without_a_restart() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = keys_file(&dir);
        let models = models_with(&dir, &file);
        assert!(!models.key_status().configured);

        file.set_key("baseten", "baseten-saved-key-0123456789")
            .expect("saves");
        models.sync_from(&file).expect("the state follows the file");

        let status = models.key_status();
        assert!(status.configured);
        assert_eq!(status.roles[1].provider.as_deref(), Some("baseten"));
    }

    #[test]
    fn a_pick_takes_effect_and_a_pick_needs_a_key_for_its_provider() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = keys_file(&dir);
        file.set_key("meta", SAVED).expect("saves");
        file.set_key("baseten", "baseten-saved-key-0123456789")
            .expect("saves");
        let models = models_with(&dir, &file);

        models
            .set_role(Role::Worker, Some("meta"))
            .expect("meta has a key");
        let status = models.key_status();
        assert_eq!(status.roles[1].provider.as_deref(), Some("meta"));
        assert!(status.roles[1].picked);

        assert!(matches!(
            models.set_role(Role::Worker, Some("fireworks")),
            Err(SetupError::NoKey)
        ));
        assert!(matches!(
            models.set_role(Role::Worker, Some("openai")),
            Err(SetupError::UnknownProvider)
        ));
    }

    #[test]
    fn removing_a_key_drops_it_and_the_picks_that_used_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = keys_file(&dir);
        file.set_key("meta", SAVED).expect("saves");
        file.set_key("baseten", "baseten-saved-key-0123456789")
            .expect("saves");
        let models = models_with(&dir, &file);
        models
            .set_role(Role::Worker, Some("baseten"))
            .expect("picks");

        assert!(models.remove_key("baseten").expect("removes"));

        let status = models.key_status();
        let baseten = status
            .providers
            .iter()
            .find(|provider| provider.name == "baseten")
            .expect("baseten is listed");
        assert!(!baseten.saved);
        assert!(!status.roles[1].picked, "the pick went with the key");
    }

    #[tokio::test]
    async fn a_client_built_from_a_config_answers_through_its_provider() {
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
        keys.insert("meta", acan_llm::ApiKey::new(SAVED));
        let dir = tempfile::tempdir().expect("temp dir");
        let models = Models::open(&dir.path().join("no-routing.toml"), None).expect("opens");
        models.replace(State::from_routing(config, keys, &models.ledger).expect("builds"));
        let request = || ChatRequest {
            messages: vec![acan_llm::Message::user("hello")],
            ..ChatRequest::default()
        };

        let response = models
            .chat(Role::Worker, request())
            .await
            .expect("the live provider answers");
        assert_eq!(response.content.as_deref(), Some("answered live"));
        assert!(models.key_status().configured);
    }

    #[tokio::test]
    async fn with_no_key_a_non_model_call_has_no_offline_reply() {
        let dir = tempfile::tempdir().expect("temp dir");
        let models = models_with(&dir, &keys_file(&dir));
        let request = ChatRequest {
            messages: vec![acan_llm::Message::user("hello")],
            ..ChatRequest::default()
        };
        assert!(
            models.chat(Role::Worker, request).await.is_err(),
            "with no key, a non-model call has no offline reply"
        );
    }

    #[tokio::test]
    async fn a_vision_call_without_a_meta_key_is_refused_before_any_request() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = keys_file(&dir);
        file.set_key("baseten", "baseten-saved-key-0123456789")
            .expect("saves");
        let models = models_with(&dir, &file);
        assert_eq!(
            models.key_status().roles[3].off_reason,
            Some("it needs a Meta key"),
            "vision is the fourth role and is off"
        );
        let request = || ChatRequest {
            messages: vec![acan_llm::Message::user("hello")],
            ..ChatRequest::default()
        };

        let refused = |err: LlmError| {
            matches!(
                err,
                LlmError::RoleOff {
                    role: Role::Vision,
                    reason: "it needs a Meta key",
                }
            ) && err.to_string() == "vision is off: it needs a Meta key"
        };
        assert!(refused(
            models
                .chat(Role::Vision, request())
                .await
                .expect_err("refused")
        ));
        assert!(refused(
            models
                .chat_traced(Role::Vision, request())
                .await
                .expect_err("refused")
        ));
        assert!(refused(
            models
                .chat_json::<Value>(Role::Vision, request())
                .await
                .expect_err("refused")
        ));
        assert!(
            models.ledger().records().is_empty(),
            "no request was sent and no offline reply was given"
        );
    }

    #[tokio::test]
    async fn vision_is_refused_with_no_key_at_all_too() {
        let dir = tempfile::tempdir().expect("temp dir");
        let models = models_with(&dir, &keys_file(&dir));
        let request = ChatRequest {
            messages: vec![acan_llm::Message::user("hello")],
            ..ChatRequest::default()
        };
        assert!(matches!(
            models.chat(Role::Vision, request).await,
            Err(LlmError::RoleOff {
                role: Role::Vision,
                reason: "it needs a Meta key",
            })
        ));
        assert!(models.ledger().records().is_empty());
    }

    #[tokio::test]
    async fn a_routed_vision_that_is_not_muse_spark_is_refused_and_never_sent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "index": 0,
                    "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "foreman answer"}
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })))
            .mount(&server)
            .await;
        // The foreman, worker and fast roles run on baseten. Vision inherits the foreman's route,
        // which is not Muse Spark, so vision must be refused without a request to baseten.
        let config = LlmConfig::from_toml_str(&format!(
            r#"
[[providers]]
name = "baseten"
base_url = "{}/v1"
api_key_env = "BASETEN_API_KEY"
default_model = "meta-models/muse-spark-open-1026"

[routing.foreman]
provider = "baseten"

[routing.worker]
provider = "baseten"

[routing.fast]
provider = "baseten"
"#,
            server.uri()
        ))
        .expect("the test config is valid");
        let mut keys = ApiKeys::default();
        keys.insert(
            "baseten",
            acan_llm::ApiKey::new("baseten-routed-key-0123456789"),
        );
        let models = Models::live(config, keys).expect("the models build");
        let request = || ChatRequest {
            messages: vec![acan_llm::Message::user("hello")],
            ..ChatRequest::default()
        };

        let status = models.key_status();
        assert_eq!(status.roles[3].provider, None);
        assert_eq!(
            status.roles[3].off_reason,
            Some("it needs a Meta Muse Spark route")
        );
        assert!(matches!(
            models.chat(Role::Vision, request()).await,
            Err(LlmError::RoleOff {
                role: Role::Vision,
                reason: "it needs a Meta Muse Spark route",
            })
        ));
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "vision was not sent to baseten"
        );

        let answer = models
            .chat(Role::Foreman, request())
            .await
            .expect("the foreman's route still answers");
        assert_eq!(answer.content.as_deref(), Some("foreman answer"));
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            1
        );
    }

    #[test]
    fn a_key_from_the_environment_cannot_be_removed_from_the_page() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = keys_file(&dir);
        file.set_key("baseten", "baseten-saved-key-0123456789")
            .expect("saves");
        let models = models_with(&dir, &file);
        let mut environment = ApiKeys::default();
        environment.insert(
            "meta",
            acan_llm::ApiKey::new("environment-meta-key-0123456789"),
        );
        let plan = resolve(&SavedSetup::default(), &environment).expect("resolves");
        models.replace(State::from_plan(plan, true, &models.ledger).expect("builds"));
        assert_eq!(
            models.key_status().providers[0].source,
            Some(KeyOrigin::Environment)
        );

        assert!(matches!(
            models.remove_key("meta"),
            Err(SetupError::FromEnvironment)
        ));
        assert!(
            matches!(models.remove_key("baseten"), Ok(true)),
            "a saved key is still removed"
        );
    }
}
