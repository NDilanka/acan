use acan_llm::{
    ApiKey, ApiKeys, ConfigError, LlmConfig, LlmError, ModelClient, OpenAiCompatible, Role,
    TokenLimitField,
};

const CANARY: &str = "sk-canary-must-never-print";

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
fn route_to_unknown_provider_fails_at_load() {
    let text = CONFIG.replace(
        "[routing.fast]\nprovider = \"baseten\"",
        "[routing.fast]\nprovider = \"fireworks\"",
    );

    match LlmConfig::from_toml_str(&text).expect_err("fireworks is not defined") {
        ConfigError::UnknownProvider { provider, .. } => assert_eq!(provider, "fireworks"),
        other => panic!("expected UnknownProvider, got {other}"),
    }
}

#[test]
fn route_to_an_undefined_provider_is_rejected_and_leaves_the_route_alone() {
    let mut config = LlmConfig::from_toml_str(CONFIG).expect("config is valid");

    match config
        .route_to(Role::Foreman, "fireworks")
        .expect_err("fireworks is not defined")
    {
        ConfigError::UnknownProvider { role, provider } => {
            assert_eq!(role, Role::Foreman);
            assert_eq!(provider, "fireworks");
        }
        other => panic!("expected UnknownProvider, got {other}"),
    }
    assert!(
        format!("{config:?}").contains("provider: \"meta\""),
        "foreman route changed after a rejected route_to"
    );
}

/// Adds one line to the meta provider block.
fn with_meta_line(line: &str) -> String {
    CONFIG.replace(
        "api_key_env = \"META_API_KEY\"",
        &format!("api_key_env = \"META_API_KEY\"\n{line}"),
    )
}

#[test]
fn token_limit_field_reads_the_canonical_name_and_defaults_to_max_tokens() {
    let text = with_meta_line("token_limit_field = \"max_completion_tokens\"");

    let config = LlmConfig::from_toml_str(&text).expect("config is valid");
    let fields: Vec<TokenLimitField> = config
        .providers()
        .iter()
        .map(|provider| provider.token_limit_field)
        .collect();

    assert_eq!(
        fields,
        [
            TokenLimitField::MaxCompletionTokens,
            TokenLimitField::MaxTokens
        ],
        "meta reads max_completion_tokens and baseten keeps the default"
    );
}

#[test]
fn unknown_token_limit_value_fails_at_load() {
    let text = with_meta_line("token_limit_field = \"max_output_tokens\"");

    match LlmConfig::from_toml_str(&text).expect_err("max_output_tokens is not a token limit field")
    {
        ConfigError::Parse(_) => {}
        other => panic!("expected Parse, got {other}"),
    }
}

#[test]
fn unknown_provider_field_fails_at_load() {
    let text = with_meta_line("max_output = 5");

    match LlmConfig::from_toml_str(&text).expect_err("max_output is not a provider field") {
        ConfigError::Parse(_) => {}
        other => panic!("expected Parse, got {other}"),
    }
}

#[test]
fn missing_role_route_fails_at_load() {
    let text = CONFIG.replace("[routing.fast]\nprovider = \"baseten\"\n", "");

    assert!(LlmConfig::from_toml_str(&text).is_err());
}

#[test]
fn debug_output_never_contains_the_key() {
    let config = LlmConfig::from_toml_str(CONFIG).expect("config is valid");
    let mut keys = ApiKeys::default();
    keys.insert("meta", ApiKey::new(CANARY));

    assert!(!format!("{:?}", ApiKey::new(CANARY)).contains(CANARY));
    assert!(!format!("{keys:?}").contains(CANARY));
    assert!(!format!("{config:?}").contains(CANARY));

    let client = OpenAiCompatible::new(config, keys).expect("client builds");
    assert!(!format!("{client:?}").contains(CANARY));
}

#[test]
fn director_has_a_config_name_and_round_trips() {
    assert_eq!(Role::Director.as_str(), "director");
    assert_eq!("director".parse::<Role>(), Ok(Role::Director));
}

#[test]
fn director_route_is_optional_and_checked_when_present() {
    let config = LlmConfig::from_toml_str(CONFIG).expect("config without a director row is valid");
    assert!(
        format!("{config:?}").contains("director: None"),
        "the director row is absent, so it falls back to the foreman"
    );

    let text = format!("{CONFIG}\n[routing.director]\nprovider = \"fireworks\"\n");
    match LlmConfig::from_toml_str(&text).expect_err("fireworks is not defined") {
        ConfigError::UnknownProvider { role, provider } => {
            assert_eq!(role, Role::Director);
            assert_eq!(provider, "fireworks");
        }
        other => panic!("expected UnknownProvider, got {other}"),
    }
}

#[tokio::test]
async fn an_unrouted_wonder_role_sends_nothing() {
    let config = LlmConfig::from_toml_str(CONFIG).expect("config is valid without a wonder route");
    let mut keys = ApiKeys::default();
    keys.insert("meta", ApiKey::new(CANARY));
    let client = OpenAiCompatible::new(config, keys).expect("client builds");

    let err = client
        .chat(Role::Wonder, acan_llm::ChatRequest::default())
        .await
        .expect_err("wonder has no route");
    assert!(
        matches!(
            err,
            LlmError::Config(ConfigError::Unrouted { role: Role::Wonder })
        ),
        "got {err:?}"
    );
    assert!(!err.was_sent(), "an unrouted role sends no request");
}

#[test]
fn a_wonder_route_must_name_a_known_provider() {
    let routed =
        format!("{CONFIG}\n[routing.wonder]\nprovider = \"meta\"\nmodel = \"muse-wonder-1.0\"\n");
    LlmConfig::from_toml_str(&routed).expect("a wonder route to meta is valid");

    let unknown = format!("{CONFIG}\n[routing.wonder]\nprovider = \"fireworks\"\n");
    match LlmConfig::from_toml_str(&unknown).expect_err("fireworks is not defined") {
        ConfigError::UnknownProvider { role, provider } => {
            assert_eq!(role, Role::Wonder);
            assert_eq!(provider, "fireworks");
        }
        other => panic!("expected UnknownProvider, got {other}"),
    }
}

/// The client for `text`, with the keys the environment holds.
fn client_from(text: &str) -> OpenAiCompatible {
    let config = LlmConfig::from_toml_str(text).expect("config is valid");
    let keys = ApiKeys::from_env(&config);
    OpenAiCompatible::new(config, keys).expect("the client builds")
}

#[test]
fn an_unrouted_director_is_muse_spark_on_the_foremans_route() {
    let destination = client_from(CONFIG)
        .destination(Role::Director)
        .expect("the director inherits the foreman's route");

    assert_eq!(destination.provider, "meta");
    assert_eq!(destination.model, "muse-spark-1.3");
    assert!(destination.is_muse_spark());
}

#[test]
fn a_director_routed_off_meta_or_to_another_model_is_not_muse_spark() {
    let off_meta = format!("{CONFIG}\n[routing.director]\nprovider = \"baseten\"\n");
    let other_model =
        format!("{CONFIG}\n[routing.director]\nprovider = \"meta\"\nmodel = \"muse-spark-open\"\n");

    let destination = client_from(&off_meta)
        .destination(Role::Director)
        .expect("the director has a route");
    assert!(
        !destination.is_muse_spark(),
        "{destination:?} is on baseten"
    );

    let destination = client_from(&other_model)
        .destination(Role::Director)
        .expect("the director has a route");
    assert!(
        !destination.is_muse_spark(),
        "{destination:?} is not an allowed model"
    );
}

#[test]
fn the_example_config_routes_wonder_to_muse_wonder() {
    let text = include_str!("../llm.example.toml");
    let config = LlmConfig::from_toml_str(text).expect("the example config is valid");
    assert!(
        format!("{config:?}").contains("muse-wonder-1.0"),
        "the example names the wonder model"
    );
}
