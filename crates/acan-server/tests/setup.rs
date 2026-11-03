//! The setup routes over HTTP: the page, the key status, saving and removing a key, and picking the
//! provider a role uses. No test sends a key to a real provider. A save that the provider would
//! check is refused before any request, or tested in `acan-llm` against a mock.

use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use acan_llm::{KeyFile, UsageLedger};
use acan_server::{Models, Running, ServerConfig, start};
use reqwest::StatusCode;
use serde_json::{Value, json};

const OWNER_TOKEN: &str = "acan_00000000000000000000000000000011";
const AGENT_TOKEN: &str = "acan_00000000000000000000000000000012";
const SAVED_KEY: &str = "meta-saved-key-0123456789";
const TYPED_KEY: &str = "typed-key-that-must-not-echo-0123";

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

/// Writes the tokens file the server reads at startup: an owner, and an agent acting for it.
fn write_tokens(repo: &Path) {
    let state = repo.join(".acan");
    fs::create_dir_all(&state).expect("the state dir");
    let tokens = json!([
        { "token": OWNER_TOKEN, "principal": { "kind": "member", "id": "owner" } },
        { "token": AGENT_TOKEN, "principal": { "kind": "agent", "id": "agent-1" } },
    ]);
    fs::write(state.join("tokens.json"), tokens.to_string()).expect("the tokens file");
}

async fn boot(repo: &Path, models: Models) -> Running {
    write_tokens(repo);
    let config = ServerConfig {
        repo: repo.to_path_buf(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        protected_branch: None,
        witness_http_base_url: None,
        witness_concurrency: acan_server::DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
    };
    let ledger: UsageLedger = models.ledger();
    start(config, models, ledger)
        .await
        .expect("the server starts")
}

/// Models with no routing file, and a keys file in `dir` that holds `saved`, if given.
fn models_without_routing(dir: &Path, saved: Option<(&str, &str)>) -> Models {
    let keys = KeyFile::new(dir.join("keys").join("keys.toml"));
    if let Some((provider, key)) = saved {
        keys.set_key(provider, key)
            .expect("the keys file is written");
    }
    Models::open(&dir.join("no-routing.toml"), Some(keys)).expect("the models open")
}

fn url(running: &Running, path: &str) -> String {
    format!("http://{}{path}", running.local_addr)
}

async fn get_with(running: &Running, path: &str, token: Option<&str>) -> reqwest::Response {
    let request = reqwest::Client::new().get(url(running, path));
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    request.send().await.expect("the request is sent")
}

async fn send_json(
    running: &Running,
    method: reqwest::Method,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new().request(method, url(running, path));
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_owned());
    }
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    request.send().await.expect("the request is sent")
}

async fn pick_role(running: &Running, role: &str, body: &str) -> reqwest::Response {
    let path = format!("/api/setup/roles/{role}");
    send_json(
        running,
        reqwest::Method::PUT,
        &path,
        Some(OWNER_TOKEN),
        Some(body),
    )
    .await
}

async fn post_key(running: &Running, token: Option<&str>, body: &str) -> reqwest::Response {
    send_json(
        running,
        reqwest::Method::POST,
        "/api/setup/keys",
        token,
        Some(body),
    )
    .await
}

#[tokio::test]
async fn the_setup_page_is_served_without_a_token_and_holds_no_token() {
    let repo = temp_repo();
    let running = boot(repo.path(), models_without_routing(repo.path(), None)).await;

    let response = get_with(&running, "/setup", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = response.text().await.expect("the page");
    assert!(page.contains("/api/setup/keys"), "the page posts its key");
    assert!(page.contains("/api/setup/roles/"), "the page picks roles");
    assert!(!page.contains(OWNER_TOKEN) && !page.contains(AGENT_TOKEN));
    assert!(
        !page.contains("acan_0000"),
        "no token is written into the page"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_status_lists_providers_and_roles_but_never_a_key() {
    let repo = temp_repo();
    let running = boot(repo.path(), models_without_routing(repo.path(), None)).await;

    let anonymous = get_with(&running, "/api/setup", None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let agent = get_with(&running, "/api/setup", Some(AGENT_TOKEN)).await;
    assert_eq!(agent.status(), StatusCode::OK);
    let status: Value = agent.json().await.expect("json");
    assert_eq!(status["configured"], false);
    assert_eq!(status["can_save"], true);
    assert!(status["source"].is_null());
    let providers: Vec<&str> = status["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .map(|provider| provider["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(providers, ["meta", "baseten", "fireworks"]);
    assert_eq!(status["roles"].as_array().expect("roles").len(), 7);
    assert!(
        status["roles"][0]["off_reason"].is_string(),
        "an off role says why"
    );

    running.stop().await.expect("the server stops");

    let repo = temp_repo();
    let running = boot(
        repo.path(),
        models_without_routing(repo.path(), Some(("meta", SAVED_KEY))),
    )
    .await;
    let response = get_with(&running, "/api/setup", Some(OWNER_TOKEN)).await;
    let text = response.text().await.expect("the status");
    let status: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(status["configured"], true);
    assert_eq!(status["source"], "setup");
    assert_eq!(status["providers"][0]["saved"], true);
    assert_eq!(status["roles"][0]["provider"], "meta");
    assert!(!text.contains(SAVED_KEY), "the status carries no key");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_status_marks_the_roles_that_need_meta_and_a_pick_of_another_provider_is_refused() {
    let repo = temp_repo();
    let running = boot(
        repo.path(),
        models_without_routing(repo.path(), Some(("baseten", SAVED_KEY))),
    )
    .await;

    let status: Value = get_with(&running, "/api/setup", Some(OWNER_TOKEN))
        .await
        .json()
        .await
        .expect("json");
    let meta_only: Vec<(&str, bool)> = status["roles"]
        .as_array()
        .expect("roles")
        .iter()
        .map(|role| {
            (
                role["role"].as_str().expect("a role name"),
                role["meta_only"].as_bool().expect("a meta_only flag"),
            )
        })
        .collect();
    assert_eq!(
        meta_only,
        [
            ("foreman", false),
            ("worker", false),
            ("fast", false),
            ("vision", true),
            ("director", true),
            ("wonder", true),
        ]
    );

    let refused = pick_role(&running, "vision", r#"{"provider": "baseten"}"#).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body: Value = refused.json().await.expect("json");
    assert_eq!(body["what"], "needs_meta");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn saving_a_key_is_for_members_and_refuses_bad_input_before_any_request() {
    let repo = temp_repo();
    let running = boot(repo.path(), models_without_routing(repo.path(), None)).await;
    let key_body =
        |provider: &str, key: &str| json!({ "provider": provider, "key": key }).to_string();

    let anonymous = post_key(&running, None, &key_body("meta", TYPED_KEY)).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let agent = post_key(&running, Some(AGENT_TOKEN), &key_body("meta", TYPED_KEY)).await;
    assert_eq!(agent.status(), StatusCode::FORBIDDEN);

    let not_json = post_key(
        &running,
        Some(OWNER_TOKEN),
        "{\"provider\": \"meta\", \"key\": ",
    )
    .await;
    assert_eq!(not_json.status(), StatusCode::BAD_REQUEST);
    let body: Value = not_json.json().await.expect("json");
    assert_eq!(body["what"], "a JSON body with provider and key");

    let unknown = post_key(&running, Some(OWNER_TOKEN), &key_body("openai", TYPED_KEY)).await;
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    let body: Value = unknown.json().await.expect("json");
    assert_eq!(body["what"], "provider");

    for bad in ["", "two words in it"] {
        let response = post_key(&running, Some(OWNER_TOKEN), &key_body("meta", bad)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad:?}");
        let text = response.text().await.expect("the body");
        assert!(!text.contains("two words"), "a refused key is not echoed");
    }

    let spaced = post_key(
        &running,
        Some(OWNER_TOKEN),
        &key_body("meta", "  typed key with spaces  "),
    )
    .await;
    assert_eq!(
        spaced.status(),
        StatusCode::BAD_REQUEST,
        "inner spaces are refused"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_removed_key_leaves_the_others_and_its_picks_go_with_it() {
    let repo = temp_repo();
    let keys = KeyFile::new(repo.path().join("keys").join("keys.toml"));
    keys.set_key("meta", SAVED_KEY).expect("saves");
    keys.set_key("baseten", "baseten-saved-key-0123456789")
        .expect("saves");
    let models = Models::open(&repo.path().join("no-routing.toml"), Some(keys)).expect("opens");
    let running = boot(repo.path(), models).await;

    let agent = send_json(
        &running,
        reqwest::Method::DELETE,
        "/api/setup/keys/baseten",
        Some(AGENT_TOKEN),
        None,
    )
    .await;
    assert_eq!(agent.status(), StatusCode::FORBIDDEN);

    let removed = send_json(
        &running,
        reqwest::Method::DELETE,
        "/api/setup/keys/baseten",
        Some(OWNER_TOKEN),
        None,
    )
    .await;
    assert_eq!(removed.status(), StatusCode::OK);
    let body: Value = removed.json().await.expect("json");
    assert_eq!(body["removed"], true);

    let text = get_with(&running, "/api/setup", Some(OWNER_TOKEN))
        .await
        .text()
        .await
        .expect("the status");
    let status: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(status["providers"][1]["saved"], false, "baseten is gone");
    assert_eq!(status["providers"][0]["saved"], true, "meta stays");
    assert_eq!(status["roles"][1]["provider"], "meta", "worker falls back");
    assert!(!text.contains(SAVED_KEY), "the status carries no key");

    let unknown = send_json(
        &running,
        reqwest::Method::DELETE,
        "/api/setup/keys/openai",
        Some(OWNER_TOKEN),
        None,
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_role_is_picked_only_for_a_provider_that_has_a_key() {
    let repo = temp_repo();
    let running = boot(repo.path(), models_without_routing(repo.path(), None)).await;
    let agent = send_json(
        &running,
        reqwest::Method::PUT,
        "/api/setup/roles/worker",
        Some(AGENT_TOKEN),
        Some(r#"{"provider": "meta"}"#),
    )
    .await;
    assert_eq!(agent.status(), StatusCode::FORBIDDEN);

    let no_key = pick_role(&running, "worker", r#"{"provider": "meta"}"#).await;
    assert_eq!(no_key.status(), StatusCode::CONFLICT);
    let body: Value = no_key.json().await.expect("json");
    assert_eq!(body["what"], "no_key");

    let unknown_role = pick_role(&running, "reviewer", r#"{"provider": "meta"}"#).await;
    assert_eq!(unknown_role.status(), StatusCode::NOT_FOUND);

    let unknown_provider = pick_role(&running, "worker", r#"{"provider": "openai"}"#).await;
    assert_eq!(unknown_provider.status(), StatusCode::BAD_REQUEST);
    let body: Value = unknown_provider.json().await.expect("json");
    assert_eq!(body["what"], "provider");

    running.stop().await.expect("the server stops");

    let repo = temp_repo();
    let running = boot(
        repo.path(),
        models_without_routing(repo.path(), Some(("meta", SAVED_KEY))),
    )
    .await;
    let picked = send_json(
        &running,
        reqwest::Method::PUT,
        "/api/setup/roles/worker",
        Some(OWNER_TOKEN),
        Some(r#"{"provider": "meta"}"#),
    )
    .await;
    assert_eq!(picked.status(), StatusCode::OK);
    let status: Value = get_with(&running, "/api/setup", Some(OWNER_TOKEN))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(status["roles"][1]["picked"], true);
    assert_eq!(status["roles"][1]["provider"], "meta");

    let cleared = send_json(
        &running,
        reqwest::Method::PUT,
        "/api/setup/roles/worker",
        Some(OWNER_TOKEN),
        Some(r#"{"provider": null}"#),
    )
    .await;
    assert_eq!(cleared.status(), StatusCode::OK);
    let status: Value = get_with(&running, "/api/setup", Some(OWNER_TOKEN))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(
        status["roles"][1]["picked"], false,
        "the default serves it again"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_routing_file_decides_and_a_saved_key_or_pick_is_refused() {
    let repo = temp_repo();
    let state = repo.path().join(".acan");
    fs::create_dir_all(&state).expect("the state dir");
    let routing = state.join("llm.toml");
    fs::write(
        &routing,
        r#"
[[providers]]
name = "meta"
base_url = "http://127.0.0.1:9/v1"
api_key_env = "ACAN_TEST_UNSET_SETUP_KEY"
default_model = "muse-spark-1.3"

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "meta"

[routing.fast]
provider = "meta"
"#,
    )
    .expect("the routing file");
    let keys = KeyFile::new(repo.path().join("keys.toml"));
    let models = Models::open(&routing, Some(keys)).expect("the models open");
    let running = boot(repo.path(), models).await;

    let status: Value = get_with(&running, "/api/setup", Some(OWNER_TOKEN))
        .await
        .json()
        .await
        .expect("json");
    assert_eq!(status["source"], "routing_file");
    assert_eq!(status["can_save"], false);
    assert_eq!(status["roles"][0]["provider"], "meta");

    let body = json!({ "provider": "meta", "key": TYPED_KEY }).to_string();
    let response = post_key(&running, Some(OWNER_TOKEN), &body).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["what"], "routing_file");

    let pick = send_json(
        &running,
        reqwest::Method::PUT,
        "/api/setup/roles/worker",
        Some(OWNER_TOKEN),
        Some(r#"{"provider": "meta"}"#),
    )
    .await;
    assert_eq!(pick.status(), StatusCode::CONFLICT);
    assert!(!repo.path().join("keys.toml").exists(), "nothing is saved");

    running.stop().await.expect("the server stops");
}
