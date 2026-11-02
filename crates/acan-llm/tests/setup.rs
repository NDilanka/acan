//! The setup side of model keys: the provider check, the save that follows it, and the keys file.
//! Providers are mocks, so no test sends a key anywhere real.

use acan_llm::{KeyCheck, KeyFile, KeyOutcome, LlmConfig, SetupError, check_key, save_checked};
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
    let saved = file.load().expect("readable").expect("a key is saved");
    assert_eq!(saved.provider, "meta");
    assert_eq!(saved.key.secret_text(), KEY);
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
    assert!(file.load().expect("readable").is_some());
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

    let unknown = save_checked(&file, &config, "baseten", KEY).await;
    assert!(matches!(unknown, Err(SetupError::UnknownProvider)));

    for bad in ["", "two words", "line\nbreak", " spaced "] {
        let outcome = save_checked(&file, &config, "meta", bad).await;
        assert!(matches!(outcome, Err(SetupError::InvalidKey)), "{bad:?}");
    }
    assert!(!file.path().exists());
}

#[test]
fn a_saved_key_round_trips_and_a_second_save_replaces_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file = KeyFile::new(dir.path().join("acan").join("keys.toml"));
    assert!(file.load().expect("no file reads as none").is_none());

    file.save("meta", "first-key-0123456789").expect("saves");
    file.save("fireworks", "second-key-0123456789")
        .expect("saves again");

    let saved = file.load().expect("readable").expect("a key");
    assert_eq!(saved.provider, "fireworks");
    assert_eq!(saved.key.secret_text(), "second-key-0123456789");
    let leftovers: Vec<_> = std::fs::read_dir(file.path().parent().expect("a folder"))
        .expect("the folder reads")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert_eq!(leftovers.len(), 1, "no temporary file is left behind");
}

#[cfg(unix)]
#[test]
fn the_keys_file_and_its_folder_are_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("temp dir");
    let folder = dir.path().join("acan");
    let file = KeyFile::new(folder.join("keys.toml"));
    file.save("meta", "private-key-0123456789").expect("saves");
    // A file that was readable by others is replaced, and the replacement is private too.
    std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644))
        .expect("loosen the file");
    file.save("meta", "private-key-0123456789")
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
    std::fs::write(
        file.path(),
        "provider = \"meta\"\nkey = \"SECRET-VALUE-0123456789\n",
    )
    .expect("writes");

    let err = file.load().expect_err("a broken file is an error");
    assert!(!err.to_string().contains("SECRET-VALUE"), "{err}");

    std::fs::write(
        file.path(),
        "provider = \"meta\"\nkey = \"SECRET-VALUE-0123456789\"\nextra = \"x\"\n",
    )
    .expect("writes");
    let err = file.load().expect_err("an unknown field is an error");
    assert!(!format!("{err:?}").contains("SECRET-VALUE"), "{err:?}");
}
