//! The model providers a tester can key, and the routing that sends each role to one of them.
//!
//! Each provider is one OpenAI-compatible endpoint with its own key. The blocks match the ones in
//! `llm.example.toml`. A key is sent only to the `base_url` of its own provider.

use std::fmt::Write;
use std::time::Duration;

use crate::config::{ConfigError, LlmConfig};
use crate::role::Role;

/// The providers the setup page offers, in the order it lists them.
pub const PROVIDERS: [&str; 3] = ["meta", "baseten", "fireworks"];

/// How long a key check may take before it reports the provider unreachable.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// One `[[providers]]` block per entry of [`PROVIDERS`], in the same order, with no routing.
const BLOCKS: [&str; 3] = [
    r#"
[[providers]]
name = "meta"
base_url = "https://api.meta.ai/v1"
api_key_env = "META_API_KEY"
default_model = "muse-spark-1.3"
structured_output = "json_schema"
token_limit_field = "max_completion_tokens"
timeout_secs = 120
"#,
    r#"
[[providers]]
name = "baseten"
base_url = "https://inference.baseten.co/v1"
api_key_env = "BASETEN_API_KEY"
default_model = "meta-models/muse-spark-open-1026"
structured_output = "json_schema"
timeout_secs = 120
"#,
    r#"
[[providers]]
name = "fireworks"
base_url = "https://api.fireworks.ai/inference/v1"
api_key_env = "FIREWORKS_API_KEY"
default_model = "accounts/fireworks/models/muse-spark-open"
structured_output = "json_schema"
timeout_secs = 120
"#,
];

/// Whether `name` is a provider the setup page offers.
#[must_use]
pub fn is_provider(name: &str) -> bool {
    PROVIDERS.contains(&name)
}

/// The config that routes each role to the provider given for it. Every provider's block is
/// included, so each provider's key variable is known, whether or not a role uses it.
///
/// # Errors
///
/// Returns the errors of [`LlmConfig::from_toml_str`] when a role has no route, or names a
/// provider that is not in [`PROVIDERS`].
pub fn config_for(routes: &[(Role, &str)]) -> Result<LlmConfig, ConfigError> {
    let mut text = String::new();
    for block in BLOCKS {
        text.push_str(block);
    }
    for (role, provider) in routes {
        // Writing to a String cannot fail.
        let _ = write!(text, "\n[routing.{role}]\nprovider = \"{provider}\"\n");
    }
    LlmConfig::from_toml_str(&text)
}

/// A config with every provider in it and every role routed to `meta`. It names each provider's
/// key variable and base URL. It is not used for calls.
///
/// # Errors
///
/// Returns the errors of [`config_for`], which cannot happen for the fixed blocks.
pub fn catalog() -> Result<LlmConfig, ConfigError> {
    let routes: Vec<(Role, &str)> = Role::ALL.iter().map(|role| (*role, "meta")).collect();
    config_for(&routes)
}

/// What a key check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCheck {
    /// The provider accepted the key.
    Accepted,
    /// The provider refused the key.
    Refused,
    /// The provider could not be reached, or did not answer in time.
    Unreachable,
    /// The provider answered, but not in a way that shows whether the key works.
    Unchecked,
}

/// Asks the provider at `base_url` whether it accepts `key`, with one request to its model list.
/// The key goes only to that provider. A check reports its outcome, never the key.
pub async fn check_key(base_url: &str, key: &str) -> KeyCheck {
    let Ok(client) = reqwest::Client::builder().timeout(CHECK_TIMEOUT).build() else {
        return KeyCheck::Unreachable;
    };
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    match client.get(url).bearer_auth(key).send().await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                KeyCheck::Accepted
            } else if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                KeyCheck::Refused
            } else {
                KeyCheck::Unchecked
            }
        }
        Err(_) => KeyCheck::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_block_parses_and_names_its_provider_over_https() {
        for name in PROVIDERS {
            let config = config_for(&[
                (Role::Foreman, name),
                (Role::Worker, name),
                (Role::Fast, name),
            ])
            .expect("the block parses");
            let provider = config
                .providers()
                .iter()
                .find(|provider| provider.name == name)
                .expect("the provider is in the config");
            assert!(
                provider.base_url.starts_with("https://"),
                "{name}: keys go over HTTPS"
            );
        }
    }

    #[test]
    fn the_catalog_holds_every_provider_and_routes_every_role() {
        let config = catalog().expect("the catalog parses");
        let names: Vec<&str> = config.providers().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, PROVIDERS);
        for role in Role::ALL {
            assert_eq!(config.route_of(role), "meta");
        }
    }

    #[test]
    fn only_the_listed_providers_are_providers() {
        assert!(is_provider("meta"));
        assert!(is_provider("fireworks"));
        assert!(!is_provider("openai"));
    }
}
