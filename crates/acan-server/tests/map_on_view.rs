//! A server that maps on first view: each codebase waits for a page's stream, takes no writes
//! while it waits, and maps once a page connects.
//!
//! Each test boots a server on a temporary repository, on an ephemeral port. The model client is
//! the offline stub, so no test calls a model.

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use acan_core::{HumanCmd, MemberId, Role as MemberRole};
use acan_llm::{Stub, UsageLedger};
use acan_server::projects::ProjectSet;
use acan_server::{
    DEFAULT_WITNESS_CONCURRENCY, Running, ServeConfig, start_projects_serving,
    start_projects_with_settings,
};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

/// How long a test waits for the scan to reach a stage before it gives up.
const WAIT: Duration = Duration::from_secs(10);

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

fn config(repo: &Path, map_on_first_view: bool) -> ServeConfig {
    ServeConfig {
        projects: ProjectSet::from_repos(&[repo.to_path_buf()]),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
        local_workers: 0,
        map_on_first_view,
    }
}

fn clients() -> Vec<(Stub, UsageLedger)> {
    vec![(Stub::default(), UsageLedger::default())]
}

fn owner_token(running: &Running) -> String {
    running
        .owner_token
        .as_ref()
        .expect("the first start mints the owner token")
        .as_str()
        .to_owned()
}

async fn get_json(running: &Running, token: &str, path: &str) -> (StatusCode, Value) {
    let response = reqwest::Client::new()
        .get(format!("http://{}{path}", running.local_addr))
        .bearer_auth(token)
        .send()
        .await
        .expect("the request is sent");
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

/// The scan stage the plant route reports.
async fn stage(running: &Running, token: &str) -> String {
    let (status, body) = get_json(running, token, "/api/plant").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the plant loads while the codebase maps"
    );
    body["scan"]["stage"]
        .as_str()
        .expect("the plant names the scan stage")
        .to_owned()
}

async fn wait_for_stage(running: &Running, token: &str, wanted: &str) {
    timeout(WAIT, async {
        while stage(running, token).await != wanted {
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the scan did not reach {wanted}"));
}

/// A write from the owner, which the server refuses until the codebase maps.
async fn add_member(running: &Running, token: &str) -> reqwest::Response {
    let cmd = HumanCmd::AddMember {
        member: MemberId::from_str("newcomer").expect("a member id"),
        role: MemberRole::Member,
    };
    reqwest::Client::new()
        .post(format!("http://{}/api/commands", running.local_addr))
        .bearer_auth(token)
        .json(&cmd)
        .send()
        .await
        .expect("the command request is sent")
}

#[tokio::test]
async fn a_codebase_waits_for_its_first_page_before_it_maps() {
    let dir = temp_repo();
    let running = start_projects_serving(config(dir.path(), true), clients(), None)
        .await
        .expect("the server starts without waiting for the maps");
    let token = owner_token(&running);

    assert_eq!(stage(&running, &token).await, "waiting");
    sleep(Duration::from_millis(300)).await;
    assert_eq!(
        stage(&running, &token).await,
        "waiting",
        "a plant read does not start the scan"
    );
    let refused = add_member(&running, &token).await;
    assert_eq!(
        refused.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a write waits for the codebase to map"
    );

    // The first authenticated stream is the page, and its connect starts the scan.
    let mut request = format!("ws://{}/api/stream?from=0", running.local_addr)
        .into_client_request()
        .expect("the stream URL");
    let value = HeaderValue::from_str(&format!("Bearer {token}")).expect("header value");
    request.headers_mut().insert(AUTHORIZATION, value);
    let (page, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the stream connects");

    wait_for_stage(&running, &token, "done").await;
    let accepted = add_member(&running, &token).await;
    assert_eq!(
        accepted.status(),
        StatusCode::OK,
        "writes open once the scan commits"
    );
    drop(page);
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_blocking_start_refuses_a_server_that_waits_for_a_page() {
    let dir = temp_repo();
    let refused = start_projects_with_settings(config(dir.path(), true), clients(), None).await;
    assert!(
        refused.is_err(),
        "a start that waits for the maps would wait for a page that may never come"
    );
}
