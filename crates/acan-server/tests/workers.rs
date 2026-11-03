//! Local workers over HTTP: `start_with_workers` runs the worker loops in this process, and
//! `GET /api/workers` reports each one. The loops talk to this same server over `/mcp`, so these
//! tests run a real origin on an ephemeral port.

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use acan_llm::{
    ChatRequest, ChatResponse, KeyStatus, LlmError, ModelClient, Role, Stub, UsageLedger,
};
use acan_server::{Running, ServerConfig, start, start_with_workers};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::{Instant, sleep, timeout};

/// How long a test waits for the loops to report.
const WAIT: Duration = Duration::from_secs(10);

/// The offline stub with a model key set, so a loop runs instead of waiting for one.
struct Keyed(Stub);

impl ModelClient for Keyed {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.0.chat(role, request).await
    }

    fn key_status(&self) -> KeyStatus {
        KeyStatus {
            configured: true,
            ..KeyStatus::default()
        }
    }
}

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

fn url(running: &Running, path: &str) -> String {
    format!("http://{}{path}", running.local_addr)
}

fn owner_token(running: &Running) -> String {
    running
        .owner_token
        .as_ref()
        .expect("the first start mints the owner token")
        .as_str()
        .to_owned()
}

/// The agent token of `agent` in the repository's tokens file, if it has one.
fn agent_token(repo: &Path, agent: &str) -> Option<String> {
    let text = fs::read_to_string(repo.join(".acan").join("tokens.json")).expect("tokens file");
    let entries: Vec<Value> = serde_json::from_str(&text).expect("tokens parse");
    entries.iter().find_map(|entry| {
        let principal = &entry["principal"];
        (principal["kind"] == "agent" && principal["id"] == agent)
            .then(|| entry["token"].as_str().expect("token text").to_owned())
    })
}

async fn get_workers(running: &Running, token: Option<&str>) -> (StatusCode, Value) {
    let client = reqwest::Client::new();
    let request = client.get(url(running, "/api/workers"));
    let response = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    }
    .send()
    .await
    .expect("the request is sent");
    let status = response.status();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// Polls until every worker reports `state`, or the test gives up.
async fn wait_for_state(running: &Running, state: &str, count: usize) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let (status, body) = get_workers(running, Some(&owner_token(running))).await;
        assert_eq!(status, StatusCode::OK, "the owner reads the workers");
        let workers = body["workers"].as_array().expect("a list of workers");
        if workers.len() == count && workers.iter().all(|worker| worker["state"] == state) {
            return body;
        }
        assert!(
            Instant::now() < deadline,
            "no workers reported {state}: {body}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_server_that_runs_no_workers_reports_an_empty_list() {
    let repo = temp_repo();
    let running = start(config(repo.path()), Stub::default(), UsageLedger::default())
        .await
        .expect("the server starts");

    let (status, body) = get_workers(&running, Some(&owner_token(&running))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({ "workers": [] }));
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_worker_without_a_model_key_says_so_and_only_members_read_the_list() {
    let repo = temp_repo();
    let running = start_with_workers(
        config(repo.path()),
        Stub::default(),
        UsageLedger::default(),
        2,
    )
    .await
    .expect("the server starts");

    let body = wait_for_state(&running, "no_key", 2).await;
    assert_eq!(body["workers"][0]["id"], "local-1");
    assert_eq!(body["workers"][1]["id"], "local-2");
    assert!(body["workers"][0]["last"].is_null(), "no run has ended");

    let (missing, _) = get_workers(&running, None).await;
    assert_eq!(missing, StatusCode::UNAUTHORIZED);
    let agent = agent_token(repo.path(), "local-1").expect("local-1 has a token");
    let (agent_status, _) = get_workers(&running, Some(&agent)).await;
    assert_eq!(
        agent_status,
        StatusCode::FORBIDDEN,
        "an agent token gets 403"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn each_local_worker_is_an_agent_registered_to_the_owner() {
    let repo = temp_repo();
    let running = start_with_workers(
        config(repo.path()),
        Stub::default(),
        UsageLedger::default(),
        1,
    )
    .await
    .expect("the server starts");

    let token = agent_token(repo.path(), "local-1").expect("local-1 has a token");
    assert_ne!(token, owner_token(&running), "the agent has its own token");
    let events: Value = reqwest::Client::new()
        .get(url(&running, "/api/events?from=0"))
        .bearer_auth(owner_token(&running))
        .send()
        .await
        .expect("the events are read")
        .json()
        .await
        .expect("events parse");
    let registered = events
        .as_array()
        .expect("a list of events")
        .iter()
        .any(|stamped| {
            stamped["event"]["type"] == "agent_registered"
                && stamped["event"]["agent"]["id"] == "local-1"
                && stamped["event"]["agent"]["owner"] == "owner"
        });
    assert!(registered, "local-1 is registered to the owner: {events}");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_restart_keeps_each_local_workers_token() {
    let repo = temp_repo();
    let first = start_with_workers(
        config(repo.path()),
        Stub::default(),
        UsageLedger::default(),
        1,
    )
    .await
    .expect("the server starts");
    let token = agent_token(repo.path(), "local-1").expect("local-1 has a token");
    first.stop().await.expect("the server stops");

    let second = start_with_workers(
        config(repo.path()),
        Stub::default(),
        UsageLedger::default(),
        1,
    )
    .await
    .expect("the server restarts");

    assert_eq!(agent_token(repo.path(), "local-1"), Some(token));
    second.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_running_worker_does_not_hold_up_shutdown() {
    let repo = temp_repo();
    let running = start_with_workers(
        config(repo.path()),
        Keyed(Stub::default()),
        UsageLedger::default(),
        1,
    )
    .await
    .expect("the server starts");

    // With a key set the loop starts a run, which waits for work for up to ten minutes.
    let body = wait_for_state(&running, "running", 1).await;
    assert_eq!(body["workers"][0]["id"], "local-1");

    let stopped = timeout(WAIT, running.stop()).await;
    assert!(
        stopped.is_ok(),
        "shutdown waited for the worker's wait limit"
    );
    stopped
        .expect("shutdown finishes in time")
        .expect("the server stops");
}
