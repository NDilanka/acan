//! The setup side of model keys: the provider check, the save that follows it, the keys file with
//! one key per provider and the role picks, and the plan that resolves them. Providers are mocks,
//! so no test sends a key anywhere real.

use acan_llm::{
    KeyCheck, KeyFile, KeyOutcome, LlmConfig, Role, SetupError, check_key, resolve, save_checked,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "meta-test-key-0123456789";

/// A one-provider config whose provider is the mock server.
fn config_for(server: &MockServer) -> LlmConfig {
    let base_url = format!("{}/v1", server.uri());
    let toml = format!(
        r#"
[[providers]]
name = "meta"
base_url = "{base_url}"
api_key_env = "META_API_KEY"
default_model = "muse-spark-1.3"

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "meta"

[routing.fast]
provider = "meta"
"#
    );
    LlmConfig::from_toml_str(&toml).expect("the test config is valid")
}

async fn answering(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_provider_that_lists_models_accepts_the_key() {
    let server = answering(200).await;
    let outcome = check_key(&format!("{}/v1", server.uri()), KEY).await;
    assert_eq!(outcome, KeyCheck::Accepted);
}

#[tokio::test]
async fn a_provider_that_refuses_the_key_says_so() {
    for status in [401, 403] {
        let server = answering(status).await;
        let outcome = check_key(&format!("{}/v1", server.uri()), KEY).await;
        assert_eq!(outcome, KeyCheck::Refused, "status {status}");
    }
}

#[tokio::test]
async fn an_answer_that_does_not_decide_is_unchecked() {
    let server = answering(500).await;
    let outcome = check_key(&format!("{}/v1", server.uri()), KEY).await;
    assert_eq!(outcome, KeyCheck::Unchecked);
}

#[tokio::test]
async fn a_provider_nobody_listens_on_is_unreachable() {
    let outcome = check_key("http://127.0.0.1:9/v1", KEY).await;
    assert_eq!(outcome, KeyCheck::Unreachable);
}

#[tokio::test]
async fn an_accepted_key_is_saved_and_reported_as_checked() {
    let server = answering(200).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("acan").join("keys.toml"));

    let outcome = save_checked(&file, &config_for(&server), "meta", KEY)
        .await
        .expect("the save runs");

    assert_eq!(outcome, KeyOutcome::Stored { checked: true });
    let saved = file.load().expect("readable");
    let key = saved.keys.get("meta").expect("a key is saved for meta");
    assert_eq!(key.secret_text(), KEY);
}

#[tokio::test]
async fn an_unchecked_key_is_saved_and_reported_as_unchecked() {
    let server = answering(500).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("keys.toml"));

    let outcome = save_checked(&file, &config_for(&server), "meta", KEY)
        .await
        .expect("the save runs");

    assert_eq!(outcome, KeyOutcome::Stored { checked: false });
    assert!(file.load().expect("readable").keys.contains_key("meta"));
}

#[tokio::test]
async fn a_refused_key_is_not_saved() {
    let server = answering(401).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("keys.toml"));

    let outcome = save_checked(&file, &config_for(&server), "meta", KEY)
        .await
        .expect("the save runs");

    assert_eq!(outcome, KeyOutcome::Rejected);
    assert!(
        !file.path().exists(),
        "nothing is written for a refused key"
    );
}

#[tokio::test]
async fn an_unreachable_provider_saves_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("keys.toml"));
    let config = LlmConfig::from_toml_str(
        r#"
[[providers]]
name = "meta"
base_url = "http://127.0.0.1:9/v1"
api_key_env = "META_API_KEY"
default_model = "muse-spark-1.3"

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "meta"

[routing.fast]
provider = "meta"
"#,
    )
    .expect("the test config is valid");

    let outcome = save_checked(&file, &config, "meta", KEY)
        .await
        .expect("the save runs");

    assert_eq!(outcome, KeyOutcome::Unreachable);
    assert!(!file.path().exists());
}

#[tokio::test]
async fn an_unknown_provider_or_a_bad_key_is_refused_before_any_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("keys.toml"));
    let config = config_for(&server);

    let unknown = save_checked(&file, &config, "openai", KEY).await;
    assert!(matches!(unknown, Err(SetupError::UnknownProvider)));

    for bad in ["", "two words", "line\nbreak", " spaced "] {
        let outcome = save_checked(&file, &config, "meta", bad).await;
        assert!(matches!(outcome, Err(SetupError::InvalidKey)), "{bad:?}");
    }
    assert!(!file.path().exists());
}

#[test]
fn each_provider_keeps_its_own_key_and_a_second_save_replaces_only_its_own() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("acan").join("keys.toml"));
    assert!(file.load().expect("no file reads as empty").keys.is_empty());

    file.set_key("meta", "first-key-0123456789").expect("saves");
    file.set_key("fireworks", "second-key-0123456789")
        .expect("saves another provider");
    file.set_key("meta", "third-key-0123456789")
        .expect("replaces meta");

    let saved = file.load().expect("readable");
    assert_eq!(saved.keys.len(), 2);
    assert_eq!(saved.keys["meta"].secret_text(), "third-key-0123456789");
    assert_eq!(
        saved.keys["fireworks"].secret_text(),
        "second-key-0123456789"
    );
    let leftovers: Vec<_> = std::fs::read_dir(file.path().parent().expect("a folder"))
        .expect("the folder reads")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert_eq!(leftovers.len(), 1, "no temporary file is left behind");
}

#[test]
fn a_role_pick_survives_a_key_save_and_a_removed_key_takes_its_picks_with_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("keys.toml"));
    file.set_key("meta", "meta-key-0123456789").expect("saves");
    file.set_key("baseten", "baseten-key-0123456789")
        .expect("saves");
    file.set_role(Role::Worker, Some("baseten")).expect("picks");
    file.set_role(Role::Fast, Some("meta")).expect("picks");

    file.set_key("fireworks", "fireworks-key-0123456789")
        .expect("saves");
    let saved = file.load().expect("readable");
    assert_eq!(
        saved.roles.get(&Role::Worker).map(String::as_str),
        Some("baseten")
    );
    assert_eq!(
        saved.roles.get(&Role::Fast).map(String::as_str),
        Some("meta")
    );

    assert!(
        file.remove_key("baseten").expect("removes"),
        "a key was removed"
    );
    assert!(
        !file.remove_key("baseten").expect("removes"),
        "and then none"
    );
    let saved = file.load().expect("readable");
    assert!(!saved.keys.contains_key("baseten"));
    assert!(
        !saved.roles.contains_key(&Role::Worker),
        "its pick went with it"
    );
    assert_eq!(
        saved.roles.get(&Role::Fast).map(String::as_str),
        Some("meta")
    );
}

#[test]
fn a_role_pick_names_only_a_provider_the_page_offers_and_clears() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("keys.toml"));

    assert!(matches!(
        file.set_role(Role::Worker, Some("openai")),
        Err(SetupError::UnknownProvider)
    ));
    file.set_role(Role::Worker, Some("fireworks"))
        .expect("picks");
    file.set_role(Role::Worker, None).expect("clears");
    assert!(file.load().expect("readable").roles.is_empty());
}

#[test]
fn a_keys_file_with_an_entry_the_page_does_not_use_skips_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("keys.toml"));
    std::fs::write(
        file.path(),
        "[keys]\nmeta = \"meta-key-0123456789\"\nopenai = \"other-key-0123456789\"\n\n[roles]\nworker = \"baseten\"\nreviewer = \"meta\"\n",
    )
    .expect("writes");

    let saved = file.load().expect("readable");
    assert_eq!(saved.keys.keys().collect::<Vec<_>>(), ["meta"]);
    assert_eq!(saved.roles.len(), 1, "the unknown role is skipped");
}

#[test]
fn a_saved_setup_resolves_each_role_to_the_provider_that_has_a_key() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("keys.toml"));
    file.set_key("meta", "meta-key-0123456789").expect("saves");
    file.set_key("baseten", "baseten-key-0123456789")
        .expect("saves");
    file.set_role(Role::Fast, Some("meta")).expect("picks");

    let plan = resolve(
        &file.load().expect("readable"),
        &acan_llm::ApiKeys::default(),
    )
    .expect("resolves");
    let served: Vec<Option<&str>> = plan
        .roles
        .iter()
        .map(|role| role.provider.as_deref())
        .collect();
    assert_eq!(
        served,
        [
            Some("meta"),
            Some("baseten"),
            Some("meta"),
            Some("meta"),
            Some("meta"),
            Some("meta"),
            Some("meta"),
        ]
    );
    assert!(plan.config.is_some());
}

#[cfg(unix)]
#[test]
fn the_keys_file_and_its_folder_are_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("temp dir");
    let folder = dir.path().join("acan");
    let file = KeyFile::new(folder.join("keys.toml"));
    file.set_key("meta", "private-key-0123456789")
        .expect("saves");
    // A file that was readable by others is replaced, and the replacement is private too.
    std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644))
        .expect("loosen the file");
    file.set_key("meta", "private-key-0123456789")
        .expect("saves again");

    let mode = |path: &std::path::Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(file.path()), 0o600);
    assert_eq!(mode(&folder), 0o700);
}

#[test]
fn a_malformed_keys_file_is_an_error_that_quotes_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("keys.toml"));
    std::fs::write(file.path(), "[keys]\nmeta = \"SECRET-VALUE-0123456789\n").expect("writes");

    let err = file.load().expect_err("a broken file is an error");
    assert!(!err.to_string().contains("SECRET-VALUE"), "{err}");

    std::fs::write(
        file.path(),
        "[keys]\nmeta = \"SECRET-VALUE-0123456789\"\n\n[extra]\nx = \"y\"\n",
    )
    .expect("writes");
    let err = file.load().expect_err("an unknown table is an error");
    assert!(!format!("{err:?}").contains("SECRET-VALUE"), "{err:?}");
}
