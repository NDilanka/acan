//! Provider presets for a tester with one key. Each preset is a complete routing file that sends
//! every role to one OpenAI-compatible provider, so one key runs the server. The blocks match the
//! ones in `llm.example.toml`. A key is sent only to the `base_url` of its preset.

use std::time::Duration;

use crate::config::LlmConfig;

/// The presets a tester can choose from, in the order the setup page lists them.
pub const PRESETS: [&str; 3] = ["baseten", "fireworks", "meta"];

/// How long a key check may take before it reports the provider unreachable.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

const BASETEN: &str = r#"
[[providers]]
name = "baseten"
base_url = "https://inference.baseten.co/v1"
api_key_env = "BASETEN_API_KEY"
default_model = "meta-models/muse-spark-open-1026"
structured_output = "json_schema"
timeout_secs = 120

[routing.foreman]
provider = "baseten"

[routing.worker]
provider = "baseten"

[routing.fast]
provider = "baseten"
"#;

const FIREWORKS: &str = r#"
[[providers]]
name = "fireworks"
base_url = "https://api.fireworks.ai/inference/v1"
api_key_env = "FIREWORKS_API_KEY"
default_model = "accounts/fireworks/models/muse-spark-open"
structured_output = "json_schema"
timeout_secs = 120

[routing.foreman]
provider = "fireworks"

[routing.worker]
provider = "fireworks"

[routing.fast]
provider = "fireworks"
"#;

const META: &str = r#"
[[providers]]
name = "meta"
base_url = "https://api.meta.ai/v1"
api_key_env = "META_API_KEY"
default_model = "muse-spark-1.3"
structured_output = "json_schema"
token_limit_field = "max_completion_tokens"
timeout_secs = 120

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "meta"

[routing.fast]
provider = "meta"
"#;

/// The config for the preset named `name`, or `None` when there is no such preset.
#[must_use]
pub fn preset(name: &str) -> Option<LlmConfig> {
    let text = match name {
        "baseten" => BASETEN,
        "fireworks" => FIREWORKS,
        "meta" => META,
        _ => return None,
    };
    LlmConfig::from_toml_str(text).ok()
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
    use crate::role::Role;

    #[test]
    fn every_preset_parses_and_routes_each_role_to_its_one_provider() {
        for name in PRESETS {
            let config = preset(name).unwrap_or_else(|| panic!("the {name} preset parses"));
            assert_eq!(config.providers().len(), 1, "{name} names one provider");
            for role in [Role::Foreman, Role::Worker, Role::Fast] {
                let route = config.resolve(role).expect("every role is routed");
                assert_eq!(route.provider.name, name);
                assert!(
                    route.provider.base_url.starts_with("https://"),
                    "{name} sends keys over HTTPS"
                );
            }
        }
    }

    #[test]
    fn an_unknown_preset_name_has_no_config() {
        assert!(preset("openai").is_none());
    }
}
