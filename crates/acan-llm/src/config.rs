use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::role::Role;

const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// How `chat_json` asks a provider for structured output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutput {
    /// Sends `response_format` with the JSON Schema.
    #[default]
    JsonSchema,
    /// Puts the schema in the prompt and validates the reply locally. For providers without `response_format`.
    Prompt,
}

/// The wire name a provider expects for the reply-length cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenLimitField {
    /// Sends `max_tokens`. Providers that name no field get this one.
    #[default]
    MaxTokens,
    /// Sends `max_completion_tokens`, the name Meta's docs make canonical.
    MaxCompletionTokens,
}

/// One OpenAI-compatible endpoint. The key is not stored here; `api_key_env` names the variable that holds it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api_key_env: String,
    pub default_model: String,
    #[serde(default)]
    pub structured_output: StructuredOutput,
    #[serde(default)]
    pub token_limit_field: TokenLimitField,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// Points one role at a provider, optionally with a model other than the provider default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
}

/// One route per role. Foreman, worker and fast must be routed, so a missing one fails at load
/// time. Vision is optional: without a vision route, vision calls use the foreman's route.
/// Wonder is optional and has no fallback: without its route, no wonder call is sent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    pub foreman: Route,
    pub worker: Route,
    pub fast: Route,
    #[serde(default)]
    pub vision: Option<Route>,
    #[serde(default)]
    pub wonder: Option<Route>,
}

impl Routing {
    fn route(&self, role: Role) -> Option<&Route> {
        match role {
            Role::Foreman => Some(&self.foreman),
            Role::Worker => Some(&self.worker),
            Role::Fast => Some(&self.fast),
            Role::Vision => Some(self.vision.as_ref().unwrap_or(&self.foreman)),
            Role::Wonder => self.wonder.as_ref(),
        }
    }

    fn route_mut(&mut self, role: Role) -> &mut Route {
        match role {
            Role::Foreman => &mut self.foreman,
            Role::Worker => &mut self.worker,
            Role::Fast => &mut self.fast,
            Role::Vision => self.vision.get_or_insert_with(|| self.foreman.clone()),
            Role::Wonder => self.wonder.get_or_insert_with(|| self.foreman.clone()),
        }
    }
}

/// Providers plus routing, as loaded from a TOML file. Holds no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    providers: Vec<ProviderConfig>,
    routing: Routing,
}

pub(crate) struct ResolvedRoute<'a> {
    pub(crate) provider: &'a ProviderConfig,
    pub(crate) model: &'a str,
}

impl LlmConfig {
    /// Parses and validates a config from TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] for invalid TOML or unknown fields. Returns the validation
    /// variants for a duplicate provider, a base URL that is not HTTP or HTTPS, or a route to an
    /// unknown provider.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Reads and validates a config file.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Read`] when the file cannot be read. Otherwise returns the errors of
    /// [`LlmConfig::from_toml_str`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    pub fn providers(&self) -> &[ProviderConfig] {
        &self.providers
    }

    /// Points `role` at `provider`, using that provider's default model.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnknownProvider`] when no provider has that name. The config is
    /// unchanged in that case.
    pub fn route_to(&mut self, role: Role, provider: &str) -> Result<(), ConfigError> {
        if !self.providers.iter().any(|known| known.name == provider) {
            return Err(ConfigError::UnknownProvider {
                role,
                provider: provider.to_owned(),
            });
        }
        *self.routing.route_mut(role) = Route {
            provider: provider.to_owned(),
            model: None,
        };
        Ok(())
    }

    pub(crate) fn resolve(&self, role: Role) -> Result<ResolvedRoute<'_>, ConfigError> {
        let route = self
            .routing
            .route(role)
            .ok_or(ConfigError::Unrouted { role })?;
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.name == route.provider)
            .ok_or_else(|| ConfigError::UnknownProvider {
                role,
                provider: route.provider.clone(),
            })?;
        let model = route
            .model
            .as_deref()
            .unwrap_or(provider.default_model.as_str());
        Ok(ResolvedRoute { provider, model })
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let mut names = BTreeSet::new();
        for provider in &self.providers {
            if !names.insert(provider.name.as_str()) {
                return Err(ConfigError::DuplicateProvider(provider.name.clone()));
            }
            let base_url = provider.base_url.as_str();
            if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
                return Err(ConfigError::InvalidBaseUrl {
                    provider: provider.name.clone(),
                    base_url: provider.base_url.clone(),
                });
            }
        }
        for role in [Role::Foreman, Role::Worker, Role::Fast, Role::Vision] {
            self.resolve(role)?;
        }
        if self.routing.wonder.is_some() {
            self.resolve(Role::Wonder)?;
        }
        Ok(())
    }
}

/// Failures while loading or checking the config.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("cannot read config {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Parse(#[from] toml::de::Error),

    #[error("provider {0} is defined more than once")]
    DuplicateProvider(String),

    #[error("provider {provider} has base_url {base_url}, expected http or https")]
    InvalidBaseUrl { provider: String, base_url: String },

    #[error("{role} routes to unknown provider {provider}")]
    UnknownProvider { role: Role, provider: String },

    #[error("{role} has no route; add a [routing.{role}] block to send its calls")]
    Unrouted { role: Role },

    #[error("cannot build the HTTP client")]
    Client(#[from] reqwest::Error),
}
