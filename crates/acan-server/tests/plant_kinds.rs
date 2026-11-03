//! Who may set a machine's kind, and what the change does: the owner writes `.acan/kinds.json`,
//! the rescan redraws the machine, and a member or an agent gets 403.
//!
//! Each test boots a server over a temporary repository on an ephemeral port. The model client is
//! the offline stub.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use acan_llm::{Stub, UsageLedger};
use acan_server::projects::ProjectSet;
use acan_server::{DEFAULT_WITNESS_CONCURRENCY, Running, ServeConfig, start_projects};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const MEMBER: &str = "acan_member_alpha_0123456789abcdef";
const AGENT: &str = "acan_agent_alpha_0123456789abcdef";
const STRANGER: &str = "acan_unknown_token_0123456789abcdef";

/// A repository folder with one source file under `model/`, so the scan finds a machine named
/// `model`.
fn repo(root: &Path) -> PathBuf {
    let dir = root.join("alpha");
    fs::create_dir_all(dir.join("model")).expect("the repository folder");
    fs::write(
        dir.join("model/store.ts"),
        "export function load() {\n  return 1;\n}\n",
    )
    .expect("a source file");
    dir
}

/// Writes the tokens file: one member token and one agent token.
fn write_tokens(repo: &Path) {
    let state = repo.join(".acan");
    fs::create_dir_all(&state).expect("the state directory");
    let entries = json!([
        { "token": MEMBER, "principal": { "kind": "member", "id": "lee" } },
        { "token": AGENT, "principal": { "kind": "agent", "id": "worker-a" } }
    ]);
    fs::write(state.join("tokens.json"), entries.to_string()).expect("the tokens file");
}

async fn boot(repo: &Path) -> Running {
    let config = ServeConfig {
        projects: ProjectSet::from_repos(&[repo.to_path_buf()]),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
        local_workers: 0,
    };
    start_projects(config, vec![(Stub::default(), UsageLedger::default())])
        .await
        .expect("the server starts")
}

/// One client for every request, so the tests share its connection pool.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

fn owner_token(running: &Running) -> String {
    running
        .owner_token
        .as_ref()
        .expect("the first start mints the human token")
        .as_str()
        .to_owned()
}

/// One request to alpha's `path` with `token`, and the status and JSON body it answers with.
async fn send(
    running: &Running,
    method: Method,
    path: &str,
    token: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let mut request = client()
        .request(
            method,
            format!("http://{}/api/p/alpha{path}", running.local_addr),
        )
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().await.expect("the request is sent");
    let status = response.status();
    (
        status,
        response.json::<Value>().await.unwrap_or(Value::Null),
    )
}

/// The machine named `name` in alpha's plant.
async fn machine(running: &Running, owner: &str, name: &str) -> Value {
    let (_, plant) = send(running, Method::GET, "/plant", owner, None).await;
    plant["machines"]
        .as_array()
        .expect("a list of machines")
        .iter()
        .find(|machine| machine["name"] == json!(name))
        .cloned()
        .unwrap_or_else(|| panic!("the plant holds {name}"))
}

/// The kinds file as JSON, read from alpha's `.acan` folder.
fn kinds_file(repo: &Path) -> Value {
    let text = fs::read_to_string(repo.join(".acan").join("kinds.json")).expect("the kinds file");
    serde_json::from_str(&text).expect("a JSON kinds file")
}

#[tokio::test]
async fn only_the_owner_sets_a_kind_and_the_rescan_redraws_it() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path());
    write_tokens(&alpha);
    let running = boot(&alpha).await;
    let owner = owner_token(&running);
    let set = json!({ "model": "gantry" });

    let (status, _) = send(&running, Method::PUT, "/plant/kinds", STRANGER, Some(&set)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a stranger gets 401");

    let (status, _) = send(&running, Method::PUT, "/plant/kinds", AGENT, Some(&set)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "an agent gets 403");

    let (status, body) = send(
        &running,
        Method::POST,
        "/commands",
        &owner,
        Some(&json!({ "cmd": "add_member", "member": "lee", "role": "member" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the owner adds lee: {body}");
    let (status, _) = send(&running, Method::PUT, "/plant/kinds", MEMBER, Some(&set)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member gets 403");
    assert!(
        !alpha.join(".acan").join("kinds.json").exists(),
        "a refused change writes nothing"
    );

    let (status, _) = send(
        &running,
        Method::PUT,
        "/plant/kinds",
        &owner,
        Some(&json!({ "model": "widget" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a kind no one has is 400");

    let (status, _) = send(
        &running,
        Method::PUT,
        "/plant/kinds",
        &owner,
        Some(&json!({ "nowhere": "gantry" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a machine not in the plant is 404"
    );

    let (status, body) = send(&running, Method::PUT, "/plant/kinds", &owner, Some(&set)).await;
    assert_eq!(status, StatusCode::OK, "the owner sets the kind: {body}");
    let model = machine(&running, &owner, "model").await;
    assert_eq!(model["kind"], json!("gantry"), "the rescan redraws it");
    assert_eq!(model["kind_set_by_user"], json!(true));
    assert_eq!(model["kind_guessed"], json!(null), "a set kind is no guess");
    assert_eq!(
        kinds_file(&alpha),
        json!({ "kinds": { "model": "gantry" } })
    );

    let (status, body) = send(
        &running,
        Method::PUT,
        "/plant/kinds",
        &owner,
        Some(&json!({ "model": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the owner clears the kind: {body}");
    let model = machine(&running, &owner, "model").await;
    assert_eq!(
        model["kind"],
        json!("mould"),
        "the name rule gives it back its kind"
    );
    assert_ne!(
        model["kind_set_by_user"],
        json!(true),
        "the machine is no longer set by the user"
    );
    assert_eq!(kinds_file(&alpha), json!({ "kinds": {} }));

    running.stop().await.expect("the server stops");
}
