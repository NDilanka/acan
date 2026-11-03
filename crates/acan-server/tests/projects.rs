//! Several projects in one server: each project's routes and tokens, the unprefixed default, and
//! the one human token every project accepts.
//!
//! Each test boots a server over temporary repositories on an ephemeral port. The model client is
//! the offline stub.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use std::time::Duration;

use acan_llm::{Stub, UsageLedger};
use acan_server::projects::ProjectSet;
use acan_server::{DEFAULT_WITNESS_CONCURRENCY, Running, ServeConfig, start_projects};
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

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long a multiplexed stream may stay silent before a test takes it as caught up.
const QUIET: Duration = Duration::from_millis(400);
/// How long a test waits for the server to close a stream before it gives up.
const WAIT: Duration = Duration::from_secs(10);

/// A repository folder with one source file, so the scan has something to index.
fn repo(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("model")).expect("the repository folder");
    fs::write(
        dir.join("model/store.ts"),
        "export function load() {\n  return 1;\n}\n",
    )
    .expect("a source file");
    dir
}

/// Writes an agent entry into a project's tokens file before the server starts, the way an
/// operator grants a worker a token.
fn grant_agent(repo: &Path, token: &str, agent: &str) {
    let state = repo.join(".acan");
    fs::create_dir_all(&state).expect("the state directory");
    let entries = json!([{ "token": token, "principal": { "kind": "agent", "id": agent } }]);
    fs::write(state.join("tokens.json"), entries.to_string()).expect("the tokens file");
}

fn tokens_text(repo: &Path) -> String {
    fs::read_to_string(repo.join(".acan").join("tokens.json")).expect("the tokens file")
}

/// The config of a server over `repos`, listening on `addr`.
fn config(repos: &[PathBuf], addr: SocketAddr) -> ServeConfig {
    ServeConfig {
        projects: ProjectSet::from_repos(repos),
        addr,
        team: false,
        web: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
        local_workers: 0,
    }
}

/// Starts a server over `repos` on `addr`. The error is what the server refused.
async fn try_boot(repos: &[PathBuf], addr: SocketAddr) -> anyhow::Result<Running> {
    let clients = repos
        .iter()
        .map(|_| (Stub::default(), UsageLedger::default()))
        .collect();
    start_projects(config(repos, addr), clients).await
}

async fn boot(repos: &[PathBuf]) -> Running {
    try_boot(repos, SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("the server starts")
}

/// One client for every request, so the tests share its connection pool.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

async fn get(running: &Running, path: &str, token: &str) -> (StatusCode, Value) {
    let response = client()
        .get(format!("http://{}{path}", running.local_addr))
        .bearer_auth(token)
        .send()
        .await
        .expect("the request is sent");
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

fn owner_token(running: &Running) -> String {
    running
        .owner_token
        .as_ref()
        .expect("the first start mints the human token")
        .as_str()
        .to_owned()
}

#[tokio::test]
async fn one_human_token_opens_every_project_and_sits_in_each_tokens_file() {
    let root = tempfile::tempdir().expect("temp dir");
    let repos = [repo(root.path(), "alpha"), repo(root.path(), "beta")];
    let running = boot(&repos).await;
    let owner = owner_token(&running);

    for id in ["alpha", "beta"] {
        let (status, body) = get(&running, &format!("/api/p/{id}/plant"), &owner).await;
        assert_eq!(status, StatusCode::OK, "{id}: the human token opens it");
        assert!(
            body.get("machines").is_some(),
            "{id} answers with its plant"
        );
    }
    for repo in &repos {
        assert!(
            tokens_text(repo).contains(&owner),
            "{} holds the shared human token",
            repo.display()
        );
    }
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn an_agent_token_opens_only_its_own_project() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let agent = "acan_agent_alpha_0123456789abcdef";
    grant_agent(&alpha, agent, "worker-a");
    let running = boot(&[alpha, beta]).await;

    let (status, _) = get(&running, "/api/p/alpha/plant", agent).await;
    assert_eq!(status, StatusCode::OK, "the agent reads its own project");
    let (status, _) = get(&running, "/api/p/beta/plant", agent).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "beta does not know the agent"
    );

    let (status, body) = get(&running, "/api/projects", agent).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .expect("a list of projects")
        .iter()
        .map(|project| project["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(
        ids,
        ["alpha"],
        "the agent sees only the project it belongs to"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_unprefixed_routes_are_the_default_project() {
    let root = tempfile::tempdir().expect("temp dir");
    let repos = [repo(root.path(), "alpha"), repo(root.path(), "beta")];
    let running = boot(&repos).await;
    let owner = owner_token(&running);

    let (_, prefixed) = get(&running, "/api/p/alpha/plant", &owner).await;
    let (_, unprefixed) = get(&running, "/api/plant", &owner).await;
    assert_eq!(prefixed, unprefixed, "the default is the first project");

    let (_, list) = get(&running, "/api/projects", &owner).await;
    let defaults: Vec<&str> = list
        .as_array()
        .expect("a list of projects")
        .iter()
        .filter(|project| project["default"] == json!(true))
        .map(|project| project["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(defaults, ["alpha"]);
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn an_unknown_project_is_not_found_and_a_stranger_gets_no_list() {
    let root = tempfile::tempdir().expect("temp dir");
    let running = boot(&[repo(root.path(), "alpha")]).await;
    let owner = owner_token(&running);

    let (status, _) = get(&running, "/api/p/nope/plant", &owner).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an unknown project is not found"
    );

    let (status, _) = get(&running, "/api/projects", "acan_nobody_0123456789abcdef").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a token in no project lists nothing"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_human_token_survives_a_restart() {
    let root = tempfile::tempdir().expect("temp dir");
    let repos = [repo(root.path(), "alpha"), repo(root.path(), "beta")];
    let first = boot(&repos).await;
    let owner = owner_token(&first);
    first.stop().await.expect("the first server stops");

    let second = boot(&repos).await;
    assert!(
        second.owner_token.is_none(),
        "a restart mints no new human token"
    );
    let (status, _) = get(&second, "/api/p/beta/plant", &owner).await;
    assert_eq!(status, StatusCode::OK, "the same token still opens beta");
    second.stop().await.expect("the second server stops");
}

#[tokio::test]
async fn startup_refuses_overlap_before_anything_binds() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    // A repository inside another project's state directory.
    let nested = repo(&alpha.join(".acan"), "nested");
    let overlapping = [
        (
            "the same repository twice",
            vec![alpha.clone(), alpha.clone()],
        ),
        ("a repository inside another's state", vec![alpha, nested]),
    ];
    for (case, repos) in overlapping {
        // Ask for a free port, then release it. A refused start must leave it free.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = probe.local_addr().expect("the port");
        drop(probe);

        let refused = try_boot(&repos, addr).await;
        assert!(refused.is_err(), "{case}: the server refuses to start");
        std::net::TcpListener::bind(addr)
            .unwrap_or_else(|err| panic!("{case}: the refused start left {addr} bound: {err}"));
    }
}

#[test]
fn the_bug_report_removes_every_projects_repository() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let redaction = ProjectSet::from_repos(&[alpha.clone(), beta.clone()]).redaction();

    let text = format!(
        "reading {}/model/store.ts and {}/model/store.ts",
        alpha.display(),
        beta.display()
    );
    assert_eq!(
        redaction.scrub(&text),
        "reading <repo>/model/store.ts and <repo>/model/store.ts",
        "the second project's paths are removed as well as the first's"
    );
}

/// Opens the multiplexed stream with the token in the header, and sends nothing else.
async fn open_all(running: &Running, token: &str) -> Socket {
    let mut request = format!("ws://{}/api/stream/all", running.local_addr)
        .into_client_request()
        .expect("the stream URL");
    let value = HeaderValue::from_str(&format!("Bearer {token}")).expect("a header value");
    request.headers_mut().insert(AUTHORIZATION, value);
    let (socket, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the stream connects");
    socket
}

/// Opens the multiplexed stream and sends its subscription, the frame that gives each project's
/// cursor.
async fn subscribe_all(running: &Running, token: &str, from: Value) -> Socket {
    let mut socket = open_all(running, token).await;
    socket
        .send(WsMessage::Text(json!({ "from": from }).to_string().into()))
        .await
        .expect("the subscription is sent");
    socket
}

/// Reads frames until the stream is quiet, which for a stream that stays open means it has sent
/// its backlog and nothing since.
async fn read_until_quiet(socket: &mut Socket) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Ok(Some(Ok(message))) = timeout(QUIET, socket.next()).await {
        match message {
            WsMessage::Text(text) => {
                frames.push(serde_json::from_str(&text).expect("a JSON frame"));
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
    frames
}

/// Reads frames until the server closes the stream. Returns the frames and the close code.
async fn read_until_closed(socket: &mut Socket) -> (Vec<Value>, u16) {
    let mut frames = Vec::new();
    loop {
        let message = timeout(WAIT, socket.next())
            .await
            .expect("the stream went quiet without closing");
        match message {
            Some(Ok(WsMessage::Text(text))) => {
                frames.push(serde_json::from_str(&text).expect("a JSON frame"));
            }
            Some(Ok(WsMessage::Close(Some(frame)))) => return (frames, u16::from(frame.code)),
            Some(Ok(_)) => {}
            Some(Err(err)) => panic!("the stream failed before a close frame: {err}"),
            None => panic!("the stream ended without a close frame"),
        }
    }
}

/// One project's events as its own log holds them, for a member of that project.
async fn events_of(running: &Running, project: &str, token: &str) -> Vec<Value> {
    let (status, body) = get(running, &format!("/api/p/{project}/events?from=0"), token).await;
    assert_eq!(status, StatusCode::OK, "{project}: the events are readable");
    body.as_array().expect("a list of events").clone()
}

fn seq_of(frame: &Value) -> u64 {
    frame["seq"].as_u64().expect("a seq")
}

#[tokio::test]
async fn multiplexed_stream_shows_only_member_projects_tagged() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let agent = "acan_agent_alpha_0123456789abcdef";
    grant_agent(&alpha, agent, "worker-a");
    let running = boot(&[alpha, beta]).await;
    let owner = owner_token(&running);

    // The agent is a member of alpha only, so its stream carries alpha's events and no beta's.
    let mut member = subscribe_all(&running, agent, json!({})).await;
    let frames = read_until_quiet(&mut member).await;
    assert!(
        !frames.is_empty(),
        "the agent's stream carries alpha's backlog"
    );
    for frame in &frames {
        assert_eq!(
            frame["project"], "alpha",
            "every frame is tagged, and only alpha's"
        );
        assert!(frame.get("event").is_some(), "the frame carries the event");
    }

    // The owner is a member of both, and each project's frames carry its own tag.
    let mut both = subscribe_all(&running, &owner, json!({})).await;
    let frames = read_until_quiet(&mut both).await;
    for id in ["alpha", "beta"] {
        assert!(
            frames.iter().any(|frame| frame["project"] == id),
            "the owner's stream carries {id}'s events"
        );
    }

    // A token that is a member of nothing is closed with 1008 and gets no event.
    let mut stranger = open_all(&running, "acan_nobody_0123456789abcdef").await;
    let (frames, code) = read_until_closed(&mut stranger).await;
    assert_eq!(code, 1008, "a token in no project is closed with 1008");
    assert!(frames.is_empty(), "and it receives nothing");
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn cursors_are_per_project() {
    let root = tempfile::tempdir().expect("temp dir");
    let running = boot(&[repo(root.path(), "alpha"), repo(root.path(), "beta")]).await;
    let owner = owner_token(&running);
    let alpha_log = events_of(&running, "alpha", &owner).await;
    let beta_log = events_of(&running, "beta", &owner).await;
    assert!(alpha_log.len() >= 2, "alpha has a backlog to skip part of");

    // Alpha resumes after its first event. Beta is not named, so it starts at 0 and gets all of
    // its events.
    let mut socket = subscribe_all(&running, &owner, json!({ "alpha": 1 })).await;
    let frames = read_until_quiet(&mut socket).await;
    let seqs = |project: &str| -> Vec<u64> {
        frames
            .iter()
            .filter(|frame| frame["project"] == project)
            .map(|frame| frame["event"]["seq"].as_u64().expect("a seq"))
            .collect()
    };
    let expected_alpha: Vec<u64> = alpha_log
        .iter()
        .map(seq_of)
        .filter(|seq| *seq >= 1)
        .collect();
    let expected_beta: Vec<u64> = beta_log.iter().map(seq_of).collect();
    assert_eq!(
        seqs("alpha"),
        expected_alpha,
        "alpha resumes from its own cursor"
    );
    assert_eq!(seqs("beta"), expected_beta, "beta starts from 0");
    running.stop().await.expect("the server stops");
}
