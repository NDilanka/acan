//! The release routes over HTTP: the public version, the update status, and the bug report.
//!
//! The update check is not started here, since a test must not reach GitHub. So the status is
//! `off`, which is what a server started with `--no-update-check` reports.

use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use acan_llm::{Stub, UsageLedger};
use acan_server::{Running, ServerConfig, start};
use reqwest::StatusCode;
use serde_json::{Value, json};

const OWNER_TOKEN: &str = "acan_00000000000000000000000000000001";
const AGENT_TOKEN: &str = "acan_00000000000000000000000000000002";

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

async fn boot(repo: &Path) -> Running {
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
    start(config, Stub::default(), UsageLedger::default())
        .await
        .expect("the server starts")
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

#[tokio::test]
async fn the_version_is_public_and_names_the_build() {
    let repo = temp_repo();
    let running = boot(repo.path()).await;

    let response = get_with(&running, "/version", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["name"], "acan");
    assert_eq!(body["version"], acan_server::release::version());
    assert_eq!(body["commit"], acan_server::release::commit());
    assert!(body["os"].is_string());
    assert!(body["arch"].is_string());

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_update_status_needs_a_token_and_is_off_without_a_check() {
    let repo = temp_repo();
    let running = boot(repo.path()).await;

    let anonymous = get_with(&running, "/api/update", None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let member = get_with(&running, "/api/update", Some(OWNER_TOKEN)).await;
    assert_eq!(member.status(), StatusCode::OK);
    let body: Value = member.json().await.expect("json");
    assert_eq!(body["state"], "off");
    assert_eq!(body["current"], acan_server::release::version());
    assert!(body["latest"].is_null());

    let agent = get_with(&running, "/api/update", Some(AGENT_TOKEN)).await;
    assert_eq!(
        agent.status(),
        StatusCode::OK,
        "any principal may read the status"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_bug_report_is_for_members_and_never_carries_the_owner_token() {
    let repo = temp_repo();
    let running = boot(repo.path()).await;

    let anonymous = get_with(&running, "/api/bug-report", None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let agent = get_with(&running, "/api/bug-report", Some(AGENT_TOKEN)).await;
    assert_eq!(agent.status(), StatusCode::FORBIDDEN);

    let member = get_with(&running, "/api/bug-report", Some(OWNER_TOKEN)).await;
    assert_eq!(member.status(), StatusCode::OK);
    let report: Value = member.json().await.expect("json");
    let link = report["url"].as_str().expect("a link");
    let body = report["body"].as_str().expect("a body");
    assert!(link.starts_with("https://github.com/NDilanka/acan/issues/new?"));
    assert!(body.contains("## Build"));
    assert!(body.contains("## Last log lines"));
    assert!(!body.contains(OWNER_TOKEN));
    assert!(!link.contains(OWNER_TOKEN));
    assert!(!body.contains(repo.path().to_str().expect("a unicode temp path")));

    running.stop().await.expect("the server stops");
}
