//! The settings routes over HTTP: who may read and change them, how a change takes effect, and what
//! the settings file keeps. No test sends a key to a real provider. A key is set in the keys file
//! directly, and the only key changes sent are ones refused before any request, or cleared.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use acan_core::{Event, Stamped};
use acan_llm::{KeyFile, UsageLedger};
use acan_server::{Models, Running, ServerConfig, start};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const OWNER_TOKEN: &str = "acan_00000000000000000000000000000011";
const AGENT_TOKEN: &str = "acan_00000000000000000000000000000012";
const MEMBER_TOKEN: &str = "acan_00000000000000000000000000000013";
/// A key the keys file holds before the server starts. No response may carry it.
const SAVED_KEY: &str = "meta-saved-key-0123456789";
/// A fake key the repository's `.env` file holds. No export may carry its text.
const FAKE_KEY: &str = "fake-key-for-data-test";
/// A fake token the tokens file holds besides the test tokens. No export may carry its text.
const FAKE_TOKEN: &str = "acan_fake-token-for-data-test";
/// The body that clears the local data. Any other body is refused.
const CLEAR_BODY: &str = r#"{"what":"local","confirm":true}"#;
/// The refusal of an export by anyone but the owner.
const ONLY_OWNER_EXPORTS: &str = "only the owner may export data";
/// The refusal of a clear by anyone but the owner.
const ONLY_OWNER_CLEARS: &str = "only the owner may clear data";
/// The order the split tests post. It names the one machine a temp repository has.
const SPLIT_SENTENCE: &str = "split the store into a reader and a writer";
/// How long a test waits for the server before it gives up.
const WAIT: Duration = Duration::from_secs(10);
/// A WebSocket the tests open to the server.
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("model")).expect("model dir");
    fs::write(
        dir.path().join("model/store.ts"),
        "export function load() {\n  return 1;\n}\n",
    )
    .expect("write fixture");
    dir
}

/// The settings file, in the repository's state directory: where a server with no settings file
/// given keeps it.
fn settings_file(repo: &Path) -> PathBuf {
    repo.join(".acan").join("settings.json")
}

/// Writes the tokens file the server reads at startup: the owner, a member who is not the owner,
/// and an agent acting for the owner.
fn write_tokens(repo: &Path) {
    let state = repo.join(".acan");
    fs::create_dir_all(&state).expect("the state dir");
    let tokens = json!([
        { "token": OWNER_TOKEN, "principal": { "kind": "member", "id": "owner" } },
        { "token": MEMBER_TOKEN, "principal": { "kind": "member", "id": "dana" } },
        { "token": AGENT_TOKEN, "principal": { "kind": "agent", "id": "agent-1" } },
    ]);
    fs::write(state.join("tokens.json"), tokens.to_string()).expect("the tokens file");
}

fn config(repo: &Path) -> ServerConfig {
    ServerConfig {
        repo: repo.to_path_buf(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        protected_branch: None,
        witness_http_base_url: None,
        witness_concurrency: acan_server::DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
    }
}

async fn boot(repo: &Path, models: Models) -> Running {
    write_tokens(repo);
    let ledger: UsageLedger = models.ledger();
    start(config(repo), models, ledger)
        .await
        .expect("the server starts")
}

/// Models with no routing file, and a keys file in `dir` that holds each `(provider, key)` saved.
fn models(dir: &Path, saved: &[(&str, &str)]) -> Models {
    let keys = KeyFile::new(dir.join("keys").join("keys.toml"));
    for &(provider, key) in saved {
        keys.set_key(provider, key)
            .expect("the keys file is written");
    }
    Models::open(&dir.join("no-routing.toml"), Some(keys)).expect("the models open")
}

async fn send(
    running: &Running,
    method: reqwest::Method,
    token: Option<&str>,
    body: Option<&Value>,
) -> reqwest::Response {
    let url = format!("http://{}/api/settings", running.local_addr);
    let mut request = reqwest::Client::new().request(method, url);
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.expect("the request is sent")
}

async fn get(running: &Running, token: Option<&str>) -> reqwest::Response {
    send(running, reqwest::Method::GET, token, None).await
}

async fn put(running: &Running, token: Option<&str>, body: &Value) -> reqwest::Response {
    send(running, reqwest::Method::PUT, token, Some(body)).await
}

async fn json(response: reqwest::Response) -> Value {
    response.json().await.expect("the body is JSON")
}

/// The owner's settings document as it reads now.
async fn owner_doc(running: &Running) -> Value {
    json(get(running, Some(OWNER_TOKEN)).await).await
}

/// The paths of the fields a `422` names.
async fn invalid_paths(response: reqwest::Response) -> Vec<String> {
    let body = json(response).await;
    body["fields"]
        .as_array()
        .expect("the fields are listed")
        .iter()
        .map(|field| field["path"].as_str().expect("a path").to_owned())
        .collect()
}

#[tokio::test]
async fn a_request_with_no_token_or_an_unknown_token_is_unauthorized() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    assert_eq!(get(&running, None).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        get(&running, Some("acan_not_a_token")).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let change = json!({"look": {"theme": "dark"}});
    assert_eq!(
        put(&running, None, &change).await.status(),
        StatusCode::UNAUTHORIZED
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn an_agent_may_neither_read_nor_change_the_settings() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let read = get(&running, Some(AGENT_TOKEN)).await;
    assert_eq!(read.status(), StatusCode::FORBIDDEN);
    let body = json(read).await;
    assert_eq!(body["why"], "only members may read settings");

    let change = json!({"look": {"theme": "dark"}});
    let refused = put(&running, Some(AGENT_TOKEN), &change).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let body = json(refused).await;
    assert_eq!(body["why"], "only the owner may change settings");
    assert!(
        !settings_file(repo.path()).exists(),
        "a refused change writes nothing"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_member_reads_the_settings_but_may_not_change_them() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let read = get(&running, Some(MEMBER_TOKEN)).await;
    assert_eq!(read.status(), StatusCode::OK);
    let doc = json(read).await;
    assert_eq!(doc["you"], "member");
    assert_eq!(doc["editable"], false);
    assert!(doc.get("tokens").is_none(), "only the owner sees tokens");

    let change = json!({"look": {"theme": "dark"}});
    let refused = put(&running, Some(MEMBER_TOKEN), &change).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let body = json(refused).await;
    assert_eq!(body["why"], "only the owner may change settings");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_owner_reads_an_editable_document_of_the_defaults() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let doc = owner_doc(&running).await;
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["you"], "owner");
    assert_eq!(doc["editable"], true);
    assert!(doc["tokens"].is_object(), "the owner sees the token key");
    assert_eq!(doc["look"]["top_down_default"], false);
    assert_eq!(doc["look"]["theme"], "paper");
    assert_eq!(doc["notifications"]["needs_you"], true);
    assert_eq!(doc["budgets"]["repair_rounds"], 2);
    assert_eq!(doc["budgets"]["call_limits"]["foreman"], 30);
    let roles = doc["harness"]["roles"].as_object().expect("the roles");
    assert_eq!(roles.len(), 7);
    assert!(
        roles
            .values()
            .all(|adapter| adapter.as_str() == Some("builtin"))
    );
    assert_eq!(doc["harness"]["custom_command"], json!([]));
    assert_eq!(doc["models"]["configured"], false);
    assert_eq!(doc["models"]["keys"].as_array().expect("keys").len(), 3);
    assert_eq!(doc["models"]["roles"].as_array().expect("roles").len(), 7);
    assert_eq!(doc["projects"][0]["workers"], 0);
    for field in ["name", "version", "commit", "os", "arch"] {
        assert!(doc["about"].get(field).is_some(), "about has {field}");
    }

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_budget_change_applies_and_the_next_read_shows_it() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let change = json!({"budgets": {"repair_rounds": 3, "call_limits": {"worker": 40}}});
    let response = put(&running, Some(OWNER_TOKEN), &change).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(
        body["applied"],
        json!({"budgets.repair_rounds": "stored", "budgets.call_limits.worker": "stored"})
    );
    assert_eq!(body["keys"], json!({}));
    assert_eq!(body["settings"]["budgets"]["repair_rounds"], 3);

    let doc = owner_doc(&running).await;
    assert_eq!(doc["budgets"]["repair_rounds"], 3);
    assert_eq!(doc["budgets"]["call_limits"]["worker"], 40);
    assert_eq!(
        doc["budgets"]["call_limits"]["foreman"], 30,
        "the limits not named keep their values"
    );

    let saved = fs::read_to_string(settings_file(repo.path())).expect("the file is written");
    let saved: Value = serde_json::from_str(&saved).expect("the file is JSON");
    assert_eq!(saved["budgets"]["repair_rounds"], 3);
    assert!(saved.get("tokens").is_none(), "the file holds no token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(settings_file(repo.path()))
            .expect("the file exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the settings file is private");
    }

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn one_bad_field_refuses_the_whole_change_and_writes_nothing() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let kept = json!({"budgets": {"repair_rounds": 3}});
    assert_eq!(
        put(&running, Some(OWNER_TOKEN), &kept).await.status(),
        StatusCode::OK
    );
    let before = fs::read(settings_file(repo.path())).expect("the file is there");

    let change = json!({
        "budgets": {"repair_rounds": 4, "max_episodes": 0},
        "look": {"theme": "dark"},
    });
    let refused = put(&running, Some(OWNER_TOKEN), &change).await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = json(refused).await;
    assert_eq!(body["error"], "invalid");
    let paths: Vec<&str> = body["fields"]
        .as_array()
        .expect("the fields are listed")
        .iter()
        .map(|field| field["path"].as_str().expect("a path"))
        .collect();
    assert_eq!(paths, ["budgets.max_episodes"]);

    assert_eq!(
        fs::read(settings_file(repo.path())).expect("the file is there"),
        before,
        "the settings file is unchanged"
    );
    let doc = owner_doc(&running).await;
    assert_eq!(doc["budgets"]["repair_rounds"], 3);
    assert_eq!(doc["look"]["theme"], "paper");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_read_only_or_unknown_key_is_a_bad_request_and_changes_nothing() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    for change in [
        json!({"tokens": {}}),
        json!({"about": {}}),
        json!({"version": 2}),
        json!({"budgets": {"nope": 1}}),
        json!({"look": {"theme": "dark"}, "tokens": {}}),
    ] {
        let response = put(&running, Some(OWNER_TOKEN), &change).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{change}");
    }

    assert!(!settings_file(repo.path()).exists());
    assert_eq!(owner_doc(&running).await["look"]["theme"], "paper");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_look_and_the_notifications_are_stored_for_the_page_to_apply() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let change = json!({
        "look": {"theme": "blueprint", "label_density": "dense", "top_down_default": true},
        "notifications": {"sound": true},
    });
    let response = put(&running, Some(OWNER_TOKEN), &change).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(
        body["applied"],
        json!({
            "look.theme": "client",
            "look.label_density": "client",
            "look.top_down_default": "client",
            "notifications.sound": "client",
        })
    );

    let doc = owner_doc(&running).await;
    assert_eq!(doc["look"]["theme"], "blueprint");
    assert_eq!(doc["look"]["label_density"], "dense");
    assert_eq!(doc["look"]["top_down_default"], true);
    assert_eq!(doc["notifications"]["sound"], true);

    let neon = put(
        &running,
        Some(OWNER_TOKEN),
        &json!({"look": {"theme": "neon"}}),
    )
    .await;
    assert_eq!(neon.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid_paths(neon).await, ["look.theme"]);

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_custom_adapter_needs_a_command_and_a_command_is_stored() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let bare = json!({"harness": {"roles": {"foreman": "custom"}}});
    let refused = put(&running, Some(OWNER_TOKEN), &bare).await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid_paths(refused).await, ["harness.roles.foreman"]);

    let with_command = json!({"harness": {
        "roles": {"foreman": "custom"},
        "custom_command": ["run-it", "--fast"],
    }});
    let stored = put(&running, Some(OWNER_TOKEN), &with_command).await;
    assert_eq!(stored.status(), StatusCode::OK);
    assert_eq!(
        json(stored).await["applied"],
        json!({"harness.roles.foreman": "stored", "harness.custom_command": "stored"})
    );
    let doc = owner_doc(&running).await;
    assert_eq!(doc["harness"]["roles"]["foreman"], "custom");
    assert_eq!(
        doc["harness"]["custom_command"],
        json!(["run-it", "--fast"])
    );

    let clear = json!({"harness": {"custom_command": []}});
    let refused = put(&running, Some(OWNER_TOKEN), &clear).await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid_paths(refused).await, ["harness.roles.foreman"]);

    let back = json!({"harness": {"roles": {"foreman": "builtin"}, "custom_command": []}});
    assert_eq!(
        put(&running, Some(OWNER_TOKEN), &back).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        owner_doc(&running).await["harness"]["roles"]["foreman"],
        "builtin"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn project_changes_are_recorded_for_the_next_start() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let id = owner_doc(&running).await["projects"][0]["id"]
        .as_str()
        .expect("the project has an id")
        .to_owned();

    let workers = json!({"projects": {"workers": {&id: 2}}});
    let response = put(&running, Some(OWNER_TOKEN), &workers).await;
    assert_eq!(response.status(), StatusCode::OK);
    let worker_path = format!("projects.workers.{id}");
    assert_eq!(
        json(response).await["applied"],
        json!({ worker_path: "restart" })
    );
    assert_eq!(owner_doc(&running).await["projects"][0]["workers"], 2);

    let too_many = json!({"projects": {"workers": {&id: 9}}});
    let refused = put(&running, Some(OWNER_TOKEN), &too_many).await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        invalid_paths(refused).await,
        [format!("projects.workers.{id}")]
    );

    let relative = json!({"projects": {"add": ["relative/folder"]}});
    let refused = put(&running, Some(OWNER_TOKEN), &relative).await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid_paths(refused).await, ["projects.add"]);

    let folder = tempfile::tempdir().expect("temp dir");
    let folder_path = folder.path().display().to_string();
    let add = json!({"projects": {"add": [&folder_path], "remove": ["gone-project"]}});
    let response = put(&running, Some(OWNER_TOKEN), &add).await;
    assert_eq!(response.status(), StatusCode::OK);
    let saved: Value = serde_json::from_str(
        &fs::read_to_string(settings_file(repo.path())).expect("the file is written"),
    )
    .expect("the file is JSON");
    assert_eq!(saved["projects"]["add"], json!([folder_path]));
    assert_eq!(saved["projects"]["remove"], json!(["gone-project"]));

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_saved_key_is_cleared_and_reported_and_never_returned() {
    let repo = temp_repo();
    let running = boot(
        repo.path(),
        models(repo.path(), &[("fireworks", SAVED_KEY)]),
    )
    .await;

    let text = owner_doc(&running).await.to_string();
    assert!(!text.contains(SAVED_KEY), "the read carries no key");
    let fireworks = |doc: &Value| {
        doc["models"]["keys"]
            .as_array()
            .expect("keys")
            .iter()
            .find(|key| key["provider"] == "fireworks")
            .expect("fireworks is listed")
            .clone()
    };
    assert_eq!(fireworks(&owner_doc(&running).await)["set"], true);

    let clear = json!({"models": {"keys": {"fireworks": {"clear": true}}}});
    let response = put(&running, Some(OWNER_TOKEN), &clear).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["keys"], json!({"fireworks": "cleared"}));
    assert_eq!(body["applied"], json!({"models.keys.fireworks": "live"}));
    assert!(
        !body.to_string().contains(SAVED_KEY),
        "the response carries no key"
    );
    assert_eq!(fireworks(&owner_doc(&running).await)["set"], false);

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_role_pick_is_refused_without_a_key_and_applied_with_one() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let pick = json!({"models": {"roles": {"fast": "baseten"}}});
    let refused = put(&running, Some(OWNER_TOKEN), &pick).await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(json(refused).await["what"], "no_key");
    running.stop().await.expect("the server stops");

    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[("baseten", SAVED_KEY)])).await;
    let response = put(&running, Some(OWNER_TOKEN), &pick).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json(response).await["applied"],
        json!({"models.roles.fast": "live"})
    );
    let doc = owner_doc(&running).await;
    let fast = doc["models"]["roles"]
        .as_array()
        .expect("roles")
        .iter()
        .find(|role| role["role"] == "fast")
        .expect("fast is listed")
        .clone();
    assert_eq!(fast["provider"], "baseten");
    assert_eq!(fast["picked"], true);

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_key_is_refused_before_any_request_when_malformed_or_for_an_unknown_provider() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let malformed = json!({"models": {"keys": {"meta": {"key": "SECRET-TYPED two words"}}}});
    let refused = put(&running, Some(OWNER_TOKEN), &malformed).await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let text = json(refused).await.to_string();
    assert!(
        !text.contains("SECRET-TYPED"),
        "a refused key is not echoed"
    );
    assert!(text.contains("models.keys.meta"));

    let unknown = json!({"models": {"keys": {"openai": {"key": "fake-key-typed"}}}});
    let refused = put(&running, Some(OWNER_TOKEN), &unknown).await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid_paths(refused).await, ["models.keys"]);

    assert!(!settings_file(repo.path()).exists(), "nothing is written");
    let doc = owner_doc(&running).await;
    let meta = doc["models"]["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .find(|key| key["provider"] == "meta")
        .expect("meta is listed");
    assert_eq!(meta["set"], false);

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn no_response_carries_a_saved_key() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[("meta", SAVED_KEY)])).await;

    let owner = get(&running, Some(OWNER_TOKEN)).await;
    assert!(!json(owner).await.to_string().contains(SAVED_KEY));
    let member = get(&running, Some(MEMBER_TOKEN)).await;
    assert!(!json(member).await.to_string().contains(SAVED_KEY));

    let budgets = json!({"budgets": {"drift_threshold": 10}});
    let saved = put(&running, Some(OWNER_TOKEN), &budgets).await;
    assert!(!json(saved).await.to_string().contains(SAVED_KEY));

    let refused = json!({"models": {"keys": {"meta": {"key": SAVED_KEY, "clear": true}}}});
    let response = put(&running, Some(OWNER_TOKEN), &refused).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!json(response).await.to_string().contains(SAVED_KEY));

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_settings_survive_a_restart() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let change = json!({"budgets": {"wall_time_ms": 120000}, "look": {"theme": "dark"}});
    assert_eq!(
        put(&running, Some(OWNER_TOKEN), &change).await.status(),
        StatusCode::OK
    );
    running.stop().await.expect("the server stops");

    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let doc = owner_doc(&running).await;
    assert_eq!(doc["budgets"]["wall_time_ms"], 120000);
    assert_eq!(doc["look"]["theme"], "dark");
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_settings_file_that_does_not_parse_stops_the_start_and_is_kept() {
    let repo = temp_repo();
    fs::create_dir_all(repo.path().join(".acan")).expect("the state dir");
    fs::write(settings_file(repo.path()), "{ not json").expect("the file is written");
    write_tokens(repo.path());
    let models = models(repo.path(), &[]);
    let ledger = models.ledger();

    let message = match start(config(repo.path()), models, ledger).await {
        Ok(_) => panic!("a server must not start over a settings file it cannot read"),
        Err(err) => err.to_string(),
    };
    assert!(message.contains("settings.json"), "message: {message}");
    assert_eq!(
        fs::read_to_string(settings_file(repo.path())).expect("the file is still there"),
        "{ not json",
        "the unreadable file is kept as it was"
    );
}

/// A POST to `path` with `token` as the bearer, and no body.
async fn post_to(running: &Running, path: &str, token: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new().post(format!("http://{}{path}", running.local_addr));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.expect("the request is sent")
}

/// Opens the stream at `path`, with `token` in the upgrade's `Authorization` header.
async fn open_stream(running: &Running, path: &str, token: &str) -> Socket {
    let mut request = format!("ws://{}{path}", running.local_addr)
        .into_client_request()
        .expect("the stream URL");
    let value = HeaderValue::from_str(&format!("Bearer {token}")).expect("the header value");
    request.headers_mut().insert(AUTHORIZATION, value);
    let (socket, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the stream connects");
    socket
}

/// Reads frames until the server closes the stream. Returns the close code and reason.
async fn closed_by_server(socket: &mut Socket) -> (u16, String) {
    loop {
        let message = timeout(WAIT, socket.next())
            .await
            .expect("the stream closes in time");
        match message {
            Some(Ok(WsMessage::Close(Some(frame)))) => {
                return (u16::from(frame.code), frame.reason.to_string());
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => panic!("the stream failed before a close frame: {err}"),
            None => panic!("the stream ended without a close frame"),
        }
    }
}

/// The tokens file of the repository, as it stands on disk.
fn tokens_text(repo: &Path) -> String {
    fs::read_to_string(repo.join(".acan").join("tokens.json")).expect("the tokens file")
}

#[tokio::test]
async fn statemap_is_a_role_in_the_models_the_harness_and_the_call_limits() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let doc = owner_doc(&running).await;
    assert_eq!(doc["budgets"]["call_limits"]["statemap"], 30);
    assert_eq!(doc["harness"]["roles"]["statemap"], "builtin");
    let model_roles = doc["models"]["roles"].as_array().expect("roles");
    assert!(
        model_roles.iter().any(|role| role["role"] == "statemap"),
        "the model roles list statemap"
    );

    let change = json!({"budgets": {"call_limits": {"statemap": 40}}});
    let response = put(&running, Some(OWNER_TOKEN), &change).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json(response).await["applied"],
        json!({"budgets.call_limits.statemap": "live"})
    );
    let read = owner_doc(&running).await;
    assert_eq!(read["budgets"]["call_limits"]["statemap"], 40);
    assert_eq!(
        read["applied"]["budgets.call_limits.statemap"], "live",
        "the read says how the statemap limit applies"
    );
    assert_eq!(read["applied"]["budgets.repair_rounds"], "stored");

    let harness = json!({"harness": {"roles": {"statemap": "codex"}}});
    assert_eq!(
        put(&running, Some(OWNER_TOKEN), &harness).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        owner_doc(&running).await["harness"]["roles"]["statemap"],
        "codex"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_statemap_llm_flag_is_set_per_project_and_applies_at_once() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let id = owner_doc(&running).await["projects"][0]["id"]
        .as_str()
        .expect("the project has an id")
        .to_owned();
    assert_eq!(
        owner_doc(&running).await["projects"][0]["statemap_llm"],
        false,
        "the flag is off by default"
    );

    let flag_path = format!("projects.statemap_llm.{id}");
    let on = json!({"projects": {"statemap_llm": {&id: true}}});
    let response = put(&running, Some(OWNER_TOKEN), &on).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json(response).await["applied"],
        json!({ flag_path.clone(): "live" })
    );
    assert_eq!(
        owner_doc(&running).await["projects"][0]["statemap_llm"],
        true
    );
    let saved: Value = serde_json::from_str(
        &fs::read_to_string(settings_file(repo.path())).expect("the file is written"),
    )
    .expect("the file is JSON");
    assert_eq!(saved["projects"]["statemap_llm"][&id], true);

    let off = json!({"projects": {"statemap_llm": {&id: false}}});
    assert_eq!(
        put(&running, Some(OWNER_TOKEN), &off).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        owner_doc(&running).await["projects"][0]["statemap_llm"],
        false
    );

    let unknown = json!({"projects": {"statemap_llm": {"no-such-project": true}}});
    let refused = put(&running, Some(OWNER_TOKEN), &unknown).await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        invalid_paths(refused).await,
        ["projects.statemap_llm.no-such-project"]
    );

    let with_restart = json!({"projects": {"statemap_llm": {&id: true}, "workers": {&id: 1}}});
    let response = put(&running, Some(OWNER_TOKEN), &with_restart).await;
    let worker_path = format!("projects.workers.{id}");
    assert_eq!(
        json(response).await["applied"],
        json!({ flag_path: "live", worker_path: "restart" }),
        "the worker count waits for a restart, and the flag is read on each use"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_owner_token_is_read_by_the_owner_alone() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let url = "/api/settings/tokens/owner";

    let owner = reqwest::Client::new()
        .get(format!("http://{}{url}", running.local_addr))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the request is sent");
    assert_eq!(owner.status(), StatusCode::OK);
    assert_eq!(json(owner).await["token"], OWNER_TOKEN);

    for (token, status) in [
        (Some(MEMBER_TOKEN), StatusCode::FORBIDDEN),
        (Some(AGENT_TOKEN), StatusCode::FORBIDDEN),
        (None, StatusCode::UNAUTHORIZED),
    ] {
        let mut request = reqwest::Client::new().get(format!("http://{}{url}", running.local_addr));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.expect("the request is sent");
        assert_eq!(response.status(), status);
    }
    let member = reqwest::Client::new()
        .get(format!("http://{}{url}", running.local_addr))
        .bearer_auth(MEMBER_TOKEN)
        .send()
        .await
        .expect("the request is sent");
    assert_eq!(
        json(member).await["why"],
        "only the owner may manage tokens"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn rotating_the_owner_token_stops_the_old_one_and_writes_the_new() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let refused = post_to(
        &running,
        "/api/settings/tokens/owner/rotate",
        Some(MEMBER_TOKEN),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    let response = post_to(
        &running,
        "/api/settings/tokens/owner/rotate",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.expect("the body is text");
    assert!(
        !text.contains(OWNER_TOKEN),
        "the response does not echo the old token"
    );
    let body: Value = serde_json::from_str(&text).expect("the body is JSON");
    let new = body["token"].as_str().expect("the new token").to_owned();
    assert!(new.starts_with("acan_") && new != OWNER_TOKEN);

    assert_eq!(
        get(&running, Some(OWNER_TOKEN)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(get(&running, Some(&new)).await.status(), StatusCode::OK);

    let on_disk = tokens_text(repo.path());
    assert!(
        on_disk.contains(&new),
        "the tokens file holds the new token"
    );
    assert!(
        !on_disk.contains(OWNER_TOKEN),
        "the tokens file drops the old token"
    );
    assert!(
        on_disk.contains(AGENT_TOKEN) && on_disk.contains(MEMBER_TOKEN),
        "the other tokens are kept"
    );

    let owner = reqwest::Client::new()
        .get(format!(
            "http://{}/api/settings/tokens/owner",
            running.local_addr
        ))
        .bearer_auth(&new)
        .send()
        .await
        .expect("the request is sent");
    assert_eq!(json(owner).await["token"], new.as_str());

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn rotating_the_agent_tokens_counts_them_and_the_old_ones_stop() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let refused = post_to(
        &running,
        "/api/settings/tokens/agents/rotate",
        Some(MEMBER_TOKEN),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    let response = post_to(
        &running,
        "/api/settings/tokens/agents/rotate",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json(response).await["rotated"], 1);

    assert_eq!(
        get(&running, Some(AGENT_TOKEN)).await.status(),
        StatusCode::UNAUTHORIZED,
        "the old agent token stops working"
    );
    assert!(
        !tokens_text(repo.path()).contains(AGENT_TOKEN),
        "the tokens file drops the old agent token"
    );
    let doc = owner_doc(&running).await;
    assert_eq!(doc["tokens"]["agents"][0]["id"], "agent-1");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn no_settings_read_carries_a_whole_owner_or_agent_token() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let doc = owner_doc(&running).await;
    let text = doc.to_string();
    assert!(
        !text.contains(OWNER_TOKEN),
        "the owner's view masks the owner token"
    );
    assert!(
        !text.contains(AGENT_TOKEN),
        "the owner's view masks the agent token"
    );
    assert_eq!(doc["tokens"]["owner"], "acan_…0011");
    assert_eq!(doc["tokens"]["agents"][0]["masked"], "acan_…0012");

    let member = json(get(&running, Some(MEMBER_TOKEN)).await)
        .await
        .to_string();
    assert!(!member.contains(OWNER_TOKEN) && !member.contains(AGENT_TOKEN));

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_revoked_owner_stream_is_closed_with_4001_and_the_token_revoked_reason() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let mut socket = open_stream(&running, "/api/stream?from=0", OWNER_TOKEN).await;

    let rotated = post_to(
        &running,
        "/api/settings/tokens/owner/rotate",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(rotated.status(), StatusCode::OK);

    let (code, reason) = closed_by_server(&mut socket).await;
    assert_eq!(code, 4001, "a revoked connection is closed with 4001");
    assert_eq!(reason, "token revoked");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_revoked_agent_stream_and_a_revoked_multiplexed_stream_are_closed() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let mut agent = open_stream(&running, "/api/stream?from=0", AGENT_TOKEN).await;
    let mut all = open_stream(&running, "/api/stream/all", OWNER_TOKEN).await;
    all.send(WsMessage::Text(json!({"from": {}}).to_string().into()))
        .await
        .expect("the subscription is sent");

    let rotated = post_to(
        &running,
        "/api/settings/tokens/agents/rotate",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(rotated.status(), StatusCode::OK);
    assert_eq!(
        closed_by_server(&mut agent).await,
        (4001, "token revoked".to_owned())
    );

    let rotated = post_to(
        &running,
        "/api/settings/tokens/owner/rotate",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(rotated.status(), StatusCode::OK);
    assert_eq!(
        closed_by_server(&mut all).await,
        (4001, "token revoked".to_owned())
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_model_test_is_owner_only_and_a_role_must_exist() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let url = format!(
        "http://{}/api/settings/models/roles/fast/test",
        running.local_addr
    );

    let unknown = post_to(
        &running,
        "/api/settings/models/roles/judge/test",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert_eq!(json(unknown).await["what"], "role");

    let member = reqwest::Client::new()
        .post(&url)
        .bearer_auth(MEMBER_TOKEN)
        .send()
        .await
        .expect("the request is sent");
    assert_eq!(member.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(member).await["why"], "only the owner may test a model");

    let agent = reqwest::Client::new()
        .post(&url)
        .bearer_auth(AGENT_TOKEN)
        .send()
        .await
        .expect("the request is sent");
    assert_eq!(agent.status(), StatusCode::FORBIDDEN);

    let none = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .expect("the request is sent");
    assert_eq!(none.status(), StatusCode::UNAUTHORIZED);

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_model_test_with_no_provider_sends_nothing_and_reports_no_key() {
    let repo = temp_repo();
    // No keys are set, so the client has no provider to ask. No request leaves the server.
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    let response = post_to(
        &running,
        "/api/settings/models/roles/statemap/test",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["role"], "statemap");
    assert_eq!(body["ok"], false);
    assert_eq!(body["provider"], Value::Null);
    assert_eq!(body["model"], Value::Null);
    assert_eq!(body["ms"], 0);
    assert_eq!(body["error"], "no_key");

    running.stop().await.expect("the server stops");
}

/// A `GET` of `path` with `token` as the bearer, when there is one.
async fn get_path(running: &Running, path: &str, token: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new().get(format!("http://{}{path}", running.local_addr));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.expect("the request is sent")
}

/// A POST of `body`, sent as JSON, to `path` with `token` as the bearer, when there is one.
async fn post_text(
    running: &Running,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("http://{}{path}", running.local_addr))
        .header("content-type", "application/json")
        .body(body.to_owned());
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.expect("the request is sent")
}

/// Records one run by posting an order to the temp repository's machine. A new world's log holds
/// only its start, which is not a run, so an export of the recorded run needs this first.
async fn record_a_run(running: &Running) {
    let order = json!({
        "cmd": "post_order",
        "machine": "model",
        "sentence": SPLIT_SENTENCE,
        "witnesses": []
    });
    let response = post_text(
        running,
        "/api/commands",
        Some(OWNER_TOKEN),
        &order.to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "the run is recorded");
}

/// The id of the default project, as `GET /api/projects` lists it.
async fn default_project(running: &Running) -> String {
    let list = json(get_path(running, "/api/projects", Some(OWNER_TOKEN)).await).await;
    list.as_array()
        .expect("a list of projects")
        .iter()
        .find(|project| project["default"] == json!(true))
        .and_then(|project| project["id"].as_str())
        .expect("a default project")
        .to_owned()
}

/// Whether `bytes` holds `text`.
fn holds(bytes: &[u8], text: &str) -> bool {
    bytes
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

/// The entries of a ZIP archive the server writes, read from its central directory: each name with
/// its bytes. Every entry is stored, so its bytes sit where its local header says they do.
fn zip_entries(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
    let u16_at = |at: usize| usize::from(u16::from_le_bytes([archive[at], archive[at + 1]]));
    let u32_at = |at: usize| {
        u32::from_le_bytes([
            archive[at],
            archive[at + 1],
            archive[at + 2],
            archive[at + 3],
        ]) as usize
    };
    // The end record is 22 bytes, since no archive here has a comment.
    let end = archive.len() - 22;
    assert_eq!(
        u32_at(end),
        0x0605_4b50,
        "the archive ends with its end record"
    );
    let count = u16_at(end + 10);
    let mut at = u32_at(end + 16);
    let mut entries = Vec::new();
    for _ in 0..count {
        assert_eq!(u32_at(at), 0x0201_4b50, "a central directory header");
        let size = u32_at(at + 24);
        let name_len = u16_at(at + 28);
        let extra_len = u16_at(at + 30);
        let comment_len = u16_at(at + 32);
        let local = u32_at(at + 42);
        let name =
            String::from_utf8(archive[at + 46..at + 46 + name_len].to_vec()).expect("a UTF-8 name");
        let data = local + 30 + u16_at(local + 26) + u16_at(local + 28);
        entries.push((name, archive[data..data + size].to_vec()));
        at += 46 + name_len + extra_len + comment_len;
    }
    entries
}

/// The names of the entries, in archive order.
fn names_of(entries: &[(String, Vec<u8>)]) -> Vec<&str> {
    entries.iter().map(|(name, _)| name.as_str()).collect()
}

/// The manifest entry of an export, parsed.
fn manifest_of(entries: &[(String, Vec<u8>)]) -> Value {
    let (_, bytes) = entries
        .iter()
        .find(|(name, _)| name == "manifest.json")
        .expect("a manifest entry");
    serde_json::from_slice(bytes).expect("the manifest is JSON")
}

/// Waits until the offline split of the posted order lands in the event log.
async fn wait_for_split(running: &Running) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let body = json(get_path(running, "/api/events?from=0", Some(OWNER_TOKEN)).await).await;
        let events: Vec<Stamped> = serde_json::from_value(body).expect("events JSON");
        if events
            .iter()
            .any(|stamped| matches!(stamped.event, Event::OrderSplit { .. }))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the offline split never landed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_data_routes_refuse_a_member_an_agent_and_an_unknown_token() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let project = default_project(&running).await;
    let deps = repo.path().join(".acan").join("deps").join("x");
    fs::create_dir_all(&deps).expect("a dependency folder");

    let exports = [
        "/api/settings/data/export?what=all".to_owned(),
        format!("/api/p/{project}/settings/data/export?what=all"),
    ];
    let clears = [
        "/api/settings/data/clear".to_owned(),
        format!("/api/p/{project}/settings/data/clear"),
    ];
    for path in &exports {
        for token in [Some(MEMBER_TOKEN), Some(AGENT_TOKEN)] {
            let response = get_path(&running, path, token).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
            let body = json(response).await;
            assert_eq!(
                body,
                json!({ "error": "forbidden", "why": ONLY_OWNER_EXPORTS })
            );
        }
        for token in [None, Some("acan_not_a_token")] {
            let response = get_path(&running, path, token).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
    }
    for path in &clears {
        for token in [Some(MEMBER_TOKEN), Some(AGENT_TOKEN)] {
            let response = post_text(&running, path, token, CLEAR_BODY).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
            let body = json(response).await;
            assert_eq!(
                body,
                json!({ "error": "forbidden", "why": ONLY_OWNER_CLEARS })
            );
        }
        for token in [None, Some("acan_not_a_token")] {
            let response = post_text(&running, path, token, CLEAR_BODY).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
    }
    assert!(deps.is_dir(), "a refused clear deletes nothing");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_owner_exports_the_receipts_and_the_recorded_run_as_one_archive() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    record_a_run(&running).await;
    let project = default_project(&running).await;

    let response = get_path(
        &running,
        "/api/settings/data/export?what=all",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/zip");
    let disposition = response.headers()["content-disposition"]
        .to_str()
        .expect("a text header")
        .to_owned();
    assert!(
        disposition.starts_with(&format!("attachment; filename=\"acan-export-{project}-")),
        "{disposition}"
    );
    assert!(disposition.ends_with(".zip\""), "{disposition}");
    let archive = response.bytes().await.expect("the archive");
    let entries = zip_entries(&archive);
    assert_eq!(
        names_of(&entries),
        [
            "manifest.json",
            "replay/plant.json",
            "replay/events.json",
            "replay/manifest.json",
        ],
        "the repository has no task, so no receipt, and the recorded run is three files"
    );
    let manifest = manifest_of(&entries);
    assert_eq!(manifest["format"], json!(1));
    assert_eq!(manifest["what"], "all");
    assert_eq!(manifest["project"], project);
    assert_eq!(manifest["receipts"], json!(0));
    assert_eq!(manifest["replay"], json!(true));
    assert!(manifest["version"].is_string());
    assert!(manifest["commit"].is_string());
    assert!(manifest["exported_at"].is_u64());
    let (_, events) = entries
        .iter()
        .find(|(name, _)| name == "replay/events.json")
        .expect("the events");
    let events: Value = serde_json::from_slice(events).expect("the events are JSON");
    assert!(
        !events.as_array().expect("a list").is_empty(),
        "the boot events are recorded"
    );

    // The replay alone, and the receipts alone, each hold their own files.
    let response = get_path(
        &running,
        "/api/settings/data/export?what=replay",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let replay = zip_entries(&response.bytes().await.expect("the archive"));
    assert_eq!(
        names_of(&replay),
        [
            "manifest.json",
            "replay/plant.json",
            "replay/events.json",
            "replay/manifest.json",
        ]
    );
    let response = get_path(
        &running,
        &format!("/api/p/{project}/settings/data/export?what=receipts"),
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let receipts = zip_entries(&response.bytes().await.expect("the archive"));
    assert_eq!(names_of(&receipts), ["manifest.json"]);
    let manifest = manifest_of(&receipts);
    assert_eq!(manifest["replay"], json!(false));
    assert_eq!(manifest["what"], "receipts");
    assert_eq!(manifest["project"], project);

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn an_export_with_no_recorded_run_is_a_conflict_and_receipts_still_answer() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    // The event log is the recorded run. The server keeps it open, but the export looks for the file.
    fs::remove_file(repo.path().join(".acan").join("world.db")).expect("the event log is removed");

    for what in ["all", "replay"] {
        let response = get_path(
            &running,
            &format!("/api/settings/data/export?what={what}"),
            Some(OWNER_TOKEN),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{what}");
        let body = json(response).await;
        assert_eq!(
            body,
            json!({ "error": "conflict", "what": "no_recorded_run" })
        );
    }

    let response = get_path(
        &running,
        "/api/settings/data/export?what=receipts",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let entries = zip_entries(&response.bytes().await.expect("the archive"));
    assert_eq!(names_of(&entries), ["manifest.json"]);
    assert_eq!(manifest_of(&entries)["replay"], json!(false));

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_fresh_repository_has_no_recorded_run_so_its_replay_is_a_conflict() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    // A new world's log holds only its opening event: the file exists, but no run is recorded.
    assert!(repo.path().join(".acan").join("world.db").is_file());

    for what in ["all", "replay"] {
        let response = get_path(
            &running,
            &format!("/api/settings/data/export?what={what}"),
            Some(OWNER_TOKEN),
        )
        .await;
        let status = response.status();
        let body = response.bytes().await.expect("the body");
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{what}: {}",
            String::from_utf8_lossy(&body)
        );
    }

    let response = get_path(
        &running,
        "/api/settings/data/export?what=receipts",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let entries = zip_entries(&response.bytes().await.expect("the archive"));
    assert_eq!(names_of(&entries), ["manifest.json"]);

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn an_export_names_one_of_three_kinds_and_refuses_any_other() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;

    for query in ["", "?what=", "?what=zip", "?what=Receipts", "?what=all%20"] {
        let response = get_path(
            &running,
            &format!("/api/settings/data/export{query}"),
            Some(OWNER_TOKEN),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query:?}");
        let body = json(response).await;
        assert_eq!(
            body,
            json!({
                "error": "bad_request",
                "what": "what must be receipts, replay or all",
            }),
            "{query:?}"
        );
    }

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn no_export_holds_the_text_of_a_dotenv_value_or_of_a_token() {
    let repo = temp_repo();
    fs::write(
        repo.path().join(".env"),
        format!("DATA_TEST_KEY={FAKE_KEY}\n"),
    )
    .expect(".env");
    // The tokens file holds one token more than the test tokens. boot writes its own file, so this
    // server starts from a file written here.
    let state = repo.path().join(".acan");
    fs::create_dir_all(&state).expect("the state dir");
    let tokens = json!([
        { "token": OWNER_TOKEN, "principal": { "kind": "member", "id": "owner" } },
        { "token": MEMBER_TOKEN, "principal": { "kind": "member", "id": "dana" } },
        { "token": AGENT_TOKEN, "principal": { "kind": "agent", "id": "agent-1" } },
        { "token": FAKE_TOKEN, "principal": { "kind": "agent", "id": "agent-2" } },
    ]);
    fs::write(state.join("tokens.json"), tokens.to_string()).expect("the tokens file");
    let served = models(repo.path(), &[]);
    let ledger: UsageLedger = served.ledger();
    let running = start(config(repo.path()), served, ledger)
        .await
        .expect("the server starts");
    record_a_run(&running).await;

    let response = get_path(
        &running,
        "/api/settings/data/export?what=all",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let archive = response.bytes().await.expect("the archive").to_vec();
    assert!(
        !holds(&archive, FAKE_KEY),
        "the archive holds no .env value"
    );
    assert!(!holds(&archive, FAKE_TOKEN), "the archive holds no token");
    for (name, bytes) in zip_entries(&archive) {
        assert!(!holds(&bytes, FAKE_KEY), "{name} holds no .env value");
        assert!(!holds(&bytes, FAKE_TOKEN), "{name} holds no token");
    }

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn an_export_skips_a_task_that_has_no_receipt() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let order = json!({
        "cmd": "post_order",
        "machine": "model",
        "sentence": SPLIT_SENTENCE,
        "witnesses": []
    });
    let response = post_text(
        &running,
        "/api/commands",
        Some(OWNER_TOKEN),
        &order.to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "the order is accepted");
    let register = json!({ "cmd": "register_agent", "agent": "agent-1" });
    let response = post_text(
        &running,
        "/api/commands",
        Some(OWNER_TOKEN),
        &register.to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "the agent is registered");
    wait_for_split(&running).await;

    // The split made a task, and no gate run has given it a receipt. Its export is skipped.
    let response = get_path(
        &running,
        "/api/settings/data/export?what=receipts",
        Some(OWNER_TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let entries = zip_entries(&response.bytes().await.expect("the archive"));
    assert_eq!(names_of(&entries), ["manifest.json"]);
    assert_eq!(manifest_of(&entries)["receipts"], json!(0));

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_owner_clears_the_dependency_cache_and_nothing_else() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let state = repo.path().join(".acan");
    fs::create_dir_all(state.join("deps").join("x")).expect("a dependency folder");
    fs::write(state.join("deps").join("x").join("file"), "installed").expect("an entry");
    fs::create_dir_all(state.join("examples")).expect("examples");
    fs::write(state.join("examples").join("keep.json"), "{}").expect("an example");
    fs::create_dir_all(state.join("observations")).expect("observations");
    fs::write(state.join("observations").join("keep.json"), "{}").expect("an observation");
    let log_before = fs::read(state.join("world.db")).expect("the event log");
    let tokens_before = tokens_text(repo.path());

    let response = post_text(
        &running,
        "/api/settings/data/clear",
        Some(OWNER_TOKEN),
        CLEAR_BODY,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(
        body,
        json!({
            "cleared": ["deps", "tour_cache"],
            "kept": [
                "receipts",
                "event_log",
                "repo",
                "tokens",
                "examples",
                "observations",
                "vision",
            ],
        })
    );
    assert!(!state.join("deps").exists(), "the dependency cache is gone");
    assert_eq!(
        fs::read(state.join("world.db")).expect("the event log"),
        log_before,
        "the event log is unchanged"
    );
    assert_eq!(
        tokens_text(repo.path()),
        tokens_before,
        "the tokens are unchanged"
    );
    assert!(state.join("examples").join("keep.json").is_file());
    assert!(state.join("observations").join("keep.json").is_file());
    assert!(repo.path().join("model").join("store.ts").is_file());

    // With no cache left, a clear is not an error.
    let response = post_text(
        &running,
        "/api/settings/data/clear",
        Some(OWNER_TOKEN),
        CLEAR_BODY,
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a missing cache is not an error"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_clear_refuses_every_body_but_the_exact_one_and_deletes_nothing() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let deps_entry = repo
        .path()
        .join(".acan")
        .join("deps")
        .join("x")
        .join("file");
    fs::create_dir_all(deps_entry.parent().expect("a folder")).expect("a dependency folder");
    fs::write(&deps_entry, "installed").expect("an entry");

    let refused = [
        r#"{"what":"local","confirm":false}"#,
        r#"{"what":"local"}"#,
        r#"{"what":"all","confirm":true}"#,
        r#"{"what":"local","confirm":true,"extra":1}"#,
        r#"{"what":"local","confirm":"true"}"#,
        r#"{"what":"local","confirm":1}"#,
        r#"{"confirm":true}"#,
        "not json",
        "",
    ];
    for body in refused {
        let response = post_text(
            &running,
            "/api/settings/data/clear",
            Some(OWNER_TOKEN),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body:?}");
        let answer = json(response).await;
        assert_eq!(answer["error"], "bad_request", "{body:?}");
        assert!(deps_entry.is_file(), "{body:?} deletes nothing");
    }
    // A request with no body at all is a bad body too.
    let response = post_to(&running, "/api/settings/data/clear", Some(OWNER_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        deps_entry.is_file(),
        "a request with no body deletes nothing"
    );

    running.stop().await.expect("the server stops");
}

#[cfg(unix)]
#[tokio::test]
async fn a_dependency_cache_that_is_a_link_is_refused_and_its_target_kept() {
    let repo = temp_repo();
    let running = boot(repo.path(), models(repo.path(), &[])).await;
    let elsewhere = tempfile::tempdir().expect("temp dir");
    fs::write(elsewhere.path().join("keep"), "kept").expect("a file outside the state dir");
    std::os::unix::fs::symlink(elsewhere.path(), repo.path().join(".acan").join("deps"))
        .expect("the link is made");

    let response = post_text(
        &running,
        "/api/settings/data/clear",
        Some(OWNER_TOKEN),
        CLEAR_BODY,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json(response).await;
    assert_eq!(
        body,
        json!({ "error": "conflict", "what": "the dependency cache is a link" })
    );
    assert!(
        elsewhere.path().join("keep").is_file(),
        "the link's target is kept"
    );

    running.stop().await.expect("the server stops");
}
