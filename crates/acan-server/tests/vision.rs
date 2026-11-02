//! The vision routes over the real server. A gate run needs the witness containers, so these
//! tests cover what a member sees before one: the auth, the 404s, and the refusal to keep
//! screenshots inside the repository. The record and the image layout are covered in the
//! crate's unit tests.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use acan_core::{Event, Stamped};
use acan_server::{
    DEFAULT_WITNESS_CONCURRENCY, Models, Running, ServerConfig, VisionConfig, start,
};
use reqwest::StatusCode;
use serde_json::{Value, json};

const OWNER_TOKEN: &str = "acan_owner_test";
const WAIT: Duration = Duration::from_secs(10);

fn temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("model")).expect("model dir");
    fs::write(
        dir.path().join("model/store.ts"),
        "export function load() {\n  return 1;\n}\n",
    )
    .expect("write fixture");
    let state = dir.path().join(".acan");
    fs::create_dir_all(&state).expect("the state dir");
    let tokens = json!([
        { "token": OWNER_TOKEN, "principal": { "kind": "member", "id": "owner" } },
    ]);
    fs::write(state.join("tokens.json"), tokens.to_string()).expect("the tokens file");
    dir
}

fn config(repo: &Path, data_dir: PathBuf) -> ServerConfig {
    ServerConfig {
        repo: repo.to_path_buf(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        protected_branch: None,
        witness_http_base_url: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: Some(VisionConfig {
            data_dir,
            ..VisionConfig::default()
        }),
    }
}

async fn boot(config: ServerConfig) -> anyhow::Result<Running> {
    let models = Models::load(&config.repo.join(".acan/llm.toml"))
        .expect("a missing routing file selects the offline models");
    let ledger = models.ledger();
    start(config, models, ledger).await
}

async fn get(running: &Running, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = reqwest::Client::new().get(format!("http://{}{path}", running.local_addr));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.expect("the request is sent");
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// Posts an order and waits for the offline split. Returns the first task's id, such as `t-1`.
async fn split_task(running: &Running) -> String {
    let post = json!({
        "cmd": "post_order",
        "machine": "model",
        "sentence": "split the store into a reader and a writer",
        "witnesses": []
    });
    let response = reqwest::Client::new()
        .post(format!("http://{}/api/commands", running.local_addr))
        .bearer_auth(OWNER_TOKEN)
        .json(&post)
        .send()
        .await
        .expect("the command is sent");
    assert_eq!(response.status(), StatusCode::OK, "the order is accepted");

    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, body) = get(running, "/api/events?from=0", Some(OWNER_TOKEN)).await;
        let events: Vec<Stamped> = serde_json::from_value(body).expect("events JSON");
        let task = events.iter().find_map(|stamped| match &stamped.event {
            Event::OrderSplit { tasks, .. } => tasks.first().map(|(id, _)| id.to_string()),
            _ => None,
        });
        if let Some(task) = task {
            return task;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the offline split never landed: {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn assert_not_found(answer: &(StatusCode, Value), what: &str) {
    let (status, body) = answer;
    assert_eq!(*status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], json!("not_found"), "{body}");
    assert_eq!(body["what"], json!(what), "{body}");
}

#[tokio::test]
async fn the_vision_routes_need_a_token_a_task_and_a_gate_run() {
    let repo = temp_repo();
    let data = tempfile::tempdir().expect("a data dir");
    let running = boot(config(repo.path(), data.path().to_path_buf()))
        .await
        .expect("the server starts");
    let task = split_task(&running).await;

    let routes = [
        format!("/api/tasks/{task}/vision"),
        format!("/api/tasks/{task}/vision/0.png"),
        format!("/api/tasks/{task}/vision/0/baseline.png"),
    ];
    for route in &routes {
        let (status, _) = get(&running, route, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{route} without a token");
        let (status, _) = get(&running, route, Some("acan_unknown")).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{route} with an unknown token"
        );
        assert_not_found(&get(&running, route, Some(OWNER_TOKEN)).await, "gate run");
        let unknown = route.replace(&task, "t-99");
        assert_not_found(&get(&running, &unknown, Some(OWNER_TOKEN)).await, "task");
    }
    assert_not_found(
        &get(
            &running,
            &format!("/api/tasks/{task}/vision/witness.json"),
            Some(OWNER_TOKEN),
        )
        .await,
        "image",
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_vision_directory_inside_the_repository_stops_the_server_from_starting() {
    let repo = temp_repo();
    let inside = repo.path().join("screenshots");
    let refused = boot(config(repo.path(), inside.clone())).await;
    let Err(err) = refused else {
        panic!("the server refuses to start");
    };
    assert!(
        format!("{err:#}").contains("inside the repository"),
        "{err:#}"
    );
    assert!(!inside.exists(), "nothing is written into the repository");
}
