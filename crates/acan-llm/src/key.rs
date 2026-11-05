use std::collections::BTreeMap;
use std::fmt;

use crate::config::LlmConfig;

const REDACTED: &str = "<redacted>";

/// An API key. Debug output is redacted, and the value is only read inside this crate.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// The key's text, for the bug report's redaction list. Never print it or send it anywhere.
    #[must_use]
    pub fn secret_text(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Keys by provider name. A provider without a key fails its calls, not the whole client.
#[derive(Debug, Clone, Default)]
pub struct ApiKeys(BTreeMap<String, ApiKey>);

impl ApiKeys {
    /// Reads each provider's `api_key_env` variable. Unset or blank variables are skipped.
    pub fn from_env(config: &LlmConfig) -> Self {
        let catalog = crate::providers::catalog().expect("the provider catalog is valid");
        let keys = config
            .providers()
            .iter()
            .filter_map(|provider| {
                if !catalog.providers().iter().any(|known| {
                    known.name == provider.name && known.api_key_env == provider.api_key_env
                }) {
                    return None;
                }
                let value = std::env::var(&provider.api_key_env)
                    .ok()
                    .filter(|value| !value.trim().is_empty())?;
                Some((provider.name.clone(), ApiKey::new(value)))
            })
            .collect();
        Self(keys)
    }

    pub fn insert(&mut self, provider: impl Into<String>, key: ApiKey) {
        self.0.insert(provider.into(), key);
    }

    /// Whether `provider` has a key here.
    #[must_use]
    pub fn has(&self, provider: &str) -> bool {
        self.0.contains_key(provider)
    }

    /// Each provider's name with its key, for the bug report's redaction list. Never print a key
    /// from here.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ApiKey)> {
        self.0
            .iter()
            .map(|(provider, key)| (provider.as_str(), key))
    }

    /// Adds `key` for `provider` unless a key is already set, so an environment variable wins over
    /// a saved key.
    #[must_use]
    pub fn with_fallback(mut self, provider: &str, key: ApiKey) -> Self {
        self.0.entry(provider.to_owned()).or_insert(key);
        self
    }

    /// Whether no provider has a key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn get(&self, provider: &str) -> Option<&ApiKey> {
        self.0.get(provider)
    }

    /// Replaces every configured key in `text` with a fixed marker. Matches the exact key bytes.
    pub(crate) fn redact(&self, text: &str) -> String {
        self.0
            .values()
            .map(ApiKey::expose)
            .filter(|key| !key.is_empty())
            .fold(text.to_owned(), |text, key| text.replace(key, REDACTED))
    }

    /// Length in bytes of the longest configured key.
    pub(crate) fn longest_len(&self) -> usize {
        self.0
            .values()
            .map(|key| key.expose().len())
            .max()
            .unwrap_or(0)
    }
}
