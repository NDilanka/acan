//! The server over real HTTP and WebSocket connections on an ephemeral port.
//!
//! Each test boots a server on a temporary repository, so every command runs through acan-core's
//! origin and the host. The model client is the offline stub.

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use acan_core::{Actor, Event, HumanCmd, MachineKey, MemberId, Role as MemberRole, Stamped};
use acan_llm::{ChatRequest, Message, ModelClient, Reply, Role, Stub, UsageLedger};
use acan_server::{Models, Running, ServerConfig, scan, start};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long a test waits for the server before it gives up.
const WAIT: Duration = Duration::from_secs(10);

/// Commands posted while a client reads nothing. Enough to fill the socket buffers and then the
/// live buffer, so the server has to close the stream instead of queueing without limit.
const FLOOD: usize = 8_000;

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

fn config(repo: &Path, addr: SocketAddr, team: bool) -> ServerConfig {
    ServerConfig {
        repo: repo.to_path_buf(),
        addr,
        team,
        web: None,
        protected_branch: None,
        witness_http_base_url: None,
        witness_concurrency: acan_server::DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
    }
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

async fn boot(repo: &Path) -> Running {
    start(
        config(repo, loopback(), false),
        Stub::default(),
        UsageLedger::default(),
    )
    .await
    .expect("the server starts")
}

fn owner_token(running: &Running) -> String {
    running
        .owner_token
        .as_ref()
        .expect("the first start mints the owner token")
        .as_str()
        .to_owned()
}

fn url(running: &Running, path: &str) -> String {
    format!("http://{}{path}", running.local_addr)
}

fn add_member(name: &str) -> HumanCmd {
    HumanCmd::AddMember {
        member: MemberId::from_str(name).expect("valid member id"),
        role: MemberRole::Member,
    }
}

/// One client for every request. Building one per call dominates the flood test, which posts
/// thousands of commands.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

async fn post_command(running: &Running, token: &str, cmd: &HumanCmd) -> reqwest::Response {
    client()
        .post(url(running, "/api/commands"))
        .bearer_auth(token)
        .json(cmd)
        .send()
        .await
        .expect("the command request is sent")
}

async fn get_json(running: &Running, path: &str, token: &str) -> (StatusCode, Value) {
    let response = client()
        .get(url(running, path))
        .bearer_auth(token)
        .send()
        .await
        .expect("the request is sent");
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

async fn accepted_seqs(running: &Running, token: &str, cmd: &HumanCmd) -> Vec<u64> {
    let response = post_command(running, token, cmd).await;
    assert_eq!(response.status(), StatusCode::OK, "the command is accepted");
    let events: Vec<Stamped> = response.json().await.expect("events JSON");
    events.iter().map(|event| event.seq.get()).collect()
}

fn seqs(events: &[Stamped]) -> Vec<u64> {
    events.iter().map(|event| event.seq.get()).collect()
}

async fn connect(running: &Running, token: &str, from: u64) -> Socket {
    let mut request = format!("ws://{}/api/stream?from={from}", running.local_addr)
        .into_client_request()
        .expect("stream URL");
    let value = HeaderValue::from_str(&format!("Bearer {token}")).expect("header value");
    request.headers_mut().insert(AUTHORIZATION, value);
    let (socket, _response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the stream connects");
    socket
}

/// Reads exactly `count` events. Ping and pong frames are skipped.
async fn next_events(socket: &mut Socket, count: usize) -> Vec<Stamped> {
    let mut events = Vec::with_capacity(count);
    while events.len() < count {
        let message = timeout(WAIT, socket.next())
            .await
            .expect("the stream sent nothing within the wait")
            .expect("the stream ended early")
            .expect("the stream read failed");
        if let WsMessage::Text(text) = message {
            events.push(serde_json::from_str::<Stamped>(&text).expect("a stream event"));
        }
    }
    events
}

/// Reads events until the server closes the stream. Returns the events and the close frame.
async fn drain(socket: &mut Socket) -> (Vec<Stamped>, u16, String) {
    let mut events = Vec::new();
    loop {
        let message = timeout(WAIT, socket.next())
            .await
            .expect("the stream went quiet without closing");
        match message {
            Some(Ok(WsMessage::Text(text))) => {
                events.push(serde_json::from_str::<Stamped>(&text).expect("a stream event"));
            }
            Some(Ok(WsMessage::Close(Some(frame)))) => {
                return (events, u16::from(frame.code), frame.reason.to_string());
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => panic!("the stream failed before a close frame: {err}"),
            None => panic!(
                "the stream ended without a close frame after {} events",
                events.len()
            ),
        }
    }
}

#[tokio::test]
async fn a_non_loopback_bind_is_refused_without_team_and_writes_nothing() {
    let dir = temp_repo();
    let addr = SocketAddr::from(([0, 0, 0, 0], 0));

    let err = start(
        config(dir.path(), addr, false),
        Stub::default(),
        UsageLedger::default(),
    )
    .await
    .expect_err("a non-loopback bind needs the team flag");

    assert!(
        err.to_string().contains("--team"),
        "the refusal names the flag: {err}"
    );
    assert!(
        !dir.path().join(".acan").exists(),
        "a refused bind creates no state directory"
    );
}

#[tokio::test]
async fn health_is_open_and_every_api_route_refuses_a_bad_token() {
    let dir = temp_repo();
    let running = boot(dir.path()).await;
    let http = reqwest::Client::new();

    let health = http
        .get(url(&running, "/health"))
        .send()
        .await
        .expect("health");
    assert_eq!(health.status(), StatusCode::OK, "health needs no token");
    assert_eq!(health.text().await.expect("health body"), "ok");

    let bad = "acan_not_a_real_token";
    for path in ["/api/plant", "/api/events?from=0", "/api/usage"] {
        let (status, _) = get_json(&running, path, bad).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "GET {path} with a bad token"
        );
    }
    let refused = post_command(&running, bad, &add_member("member-x")).await;
    assert_eq!(
        refused.status(),
        StatusCode::UNAUTHORIZED,
        "POST /api/commands with a bad token"
    );

    let mut request = format!("ws://{}/api/stream?from=0", running.local_addr)
        .into_client_request()
        .expect("stream URL");
    let value = HeaderValue::from_str(&format!("Bearer {bad}")).expect("header value");
    request.headers_mut().insert(AUTHORIZATION, value);
    match tokio_tungstenite::connect_async(request).await {
        Err(WsError::Http(response)) => {
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "the upgrade is refused"
            );
        }
        other => panic!("a bad token must fail the upgrade with 401, got {other:?}"),
    }
}

/// Opens the stream with no `Authorization` header, as a browser does.
async fn connect_bare(running: &Running, query: &str) -> Socket {
    let (socket, _response) =
        tokio_tungstenite::connect_async(format!("ws://{}/api/stream?{query}", running.local_addr))
            .await
            .expect("the upgrade needs no header");
    socket
}

#[tokio::test]
async fn a_browser_stream_authenticates_with_its_first_frame() {
    let dir = temp_repo();
    let running = boot(dir.path()).await;
    let token = owner_token(&running);
    let mut socket = connect_bare(&running, "from=0").await;

    socket
        .send(WsMessage::Text(
            json!({ "token": token }).to_string().into(),
        ))
        .await
        .expect("the token frame is sent");

    let events = next_events(&mut socket, 2).await;
    assert_eq!(seqs(&events), [0, 1], "the backlog follows the token frame");
}

#[tokio::test]
async fn a_stream_with_a_bad_first_frame_is_closed_before_any_event() {
    let dir = temp_repo();
    let running = boot(dir.path()).await;
    let mut socket = connect_bare(&running, "from=0").await;

    socket
        .send(WsMessage::Text(
            json!({ "token": "acan_not_a_real_token" })
                .to_string()
                .into(),
        ))
        .await
        .expect("the token frame is sent");

    let (events, code, _) = drain(&mut socket).await;
    assert!(
        events.is_empty(),
        "nothing is sent to an unauthenticated stream"
    );
    assert_eq!(code, 1008, "closed as a policy violation");
}

#[tokio::test]
async fn a_token_in_the_url_does_not_authenticate_and_a_silent_stream_times_out() {
    let dir = temp_repo();
    let running = boot(dir.path()).await;
    let token = owner_token(&running);
    let mut socket = connect_bare(&running, &format!("from=0&token={token}")).await;

    let started = tokio::time::Instant::now();
    let (events, code, _) = drain(&mut socket).await;

    assert!(events.is_empty(), "a query token sends no backlog");
    assert_eq!(code, 1008, "closed as a policy violation");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "the server waited for a token frame before closing, {:?}",
        started.elapsed()
    );
}
#[tokio::test]
async fn the_owner_token_is_minted_once_and_survives_a_restart() {
    let dir = temp_repo();
    let first = boot(dir.path()).await;
    let token = owner_token(&first);
    first.stop().await.expect("the first server stops");

    let second = start(
        config(dir.path(), loopback(), false),
        Stub::default(),
        UsageLedger::default(),
    )
    .await
    .expect("the server restarts");
    assert!(
        second.owner_token.is_none(),
        "a restart does not print a new owner token"
    );

    let (status, _) = get_json(&second, "/api/usage", &token).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner token still works after a restart"
    );
}

#[tokio::test]
async fn plant_endpoint_returns_the_scanned_plant() {
    let dir = temp_repo();
    let running = boot(dir.path()).await;
    let token = owner_token(&running);

    let (status, body) = get_json(&running, "/api/plant", &token).await;

    assert_eq!(status, StatusCode::OK);
    let expected = serde_json::to_value(scan(dir.path()).expect("scan").plant).expect("plant JSON");
    assert_eq!(
        body, expected,
        "the endpoint returns the same plant as a fresh scan"
    );
}

#[tokio::test]
async fn an_accepted_command_is_stored_and_listed_from_its_seq() {
    let dir = temp_repo();
    let running = boot(dir.path()).await;
    let token = owner_token(&running);

    // Seq 0 is WorldOpened and seq 1 is the IndexUpdated the server records at startup.
    let accepted = accepted_seqs(&running, &token, &add_member("member-a")).await;
    assert_eq!(accepted, [2], "the new member is the next event");

    let (status, body) = get_json(&running, "/api/events?from=0", &token).await;
    assert_eq!(status, StatusCode::OK);
    let events: Vec<Stamped> = serde_json::from_value(body).expect("events JSON");
    assert_eq!(
        seqs(&events),
        [0, 1, 2],
        "the stored log holds the accepted event"
    );

    let (_, tail) = get_json(&running, "/api/events?from=2", &token).await;
    let tail: Vec<Stamped> = serde_json::from_value(tail).expect("events JSON");
    assert_eq!(seqs(&tail), [2], "from=2 returns only the new event");
}

#[tokio::test]
async fn a_rejected_command_is_409_with_the_rejection_body() {
    let dir = temp_repo();
    let running = boot(dir.path()).await;
    let token = owner_token(&running);
    let cmd = HumanCmd::CancelOrder {
        order: acan_core::OrderId::from_str("o-99").expect("valid order id"),
    };

    let response = post_command(&running, &token, &cmd).await;

    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "an origin rejection is a 409"
    );
    let body: Value = response.json().await.expect("rejection JSON");
    assert_eq!(
        body["reason"],
        json!("unknown"),
        "the body is the Rejection, tagged by reason"
    );
}

#[tokio::test]
async fn an_agent_token_cannot_send_http_commands() {
    let dir = temp_repo();
    let state = dir.path().join(".acan");
    fs::create_dir_all(&state).expect("state dir");
    fs::write(
        state.join("tokens.json"),
        r#"[
          {"token": "acan_owner_test", "principal": {"kind": "member", "id": "owner"}},
          {"token": "acan_agent_test", "principal": {"kind": "agent", "id": "agent-1"}}
        ]"#,
    )
    .expect("write tokens");
    let running = boot(dir.path()).await;
    assert!(
        running.owner_token.is_none(),
        "a token file that exists mints nothing"
    );

    let response = post_command(&running, "acan_agent_test", &add_member("member-b")).await;

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "agents do not send over HTTP"
    );
    let body: Value = response.json().await.expect("forbidden JSON");
    assert_eq!(body["error"], json!("forbidden"));
}

#[tokio::test]
async fn a_resumed_stream_delivers_the_missed_events_once_and_in_order() {
    let dir = temp_repo();
    let running = boot(dir.path()).await;
    let token = owner_token(&running);
    for name in ["member-a", "member-b", "member-c"] {
        accepted_seqs(&running, &token, &add_member(name)).await;
    }

    // Seqs 0 to 4 exist. The first connection reads the backlog and leaves after seq 2.
    let mut first = connect(&running, &token, 0).await;
    let read = next_events(&mut first, 3).await;
    assert_eq!(seqs(&read), [0, 1, 2], "the backlog arrives in order");
    drop(first);

    // A command accepted while the client is away is among the events it misses.
    accepted_seqs(&running, &token, &add_member("member-d")).await;

    let mut second = connect(&running, &token, 3).await;
    let backlog = next_events(&mut second, 3).await;
    assert_eq!(
        seqs(&backlog),
        [3, 4, 5],
        "resuming from seq 3 returns exactly the missed events"
    );

    accepted_seqs(&running, &token, &add_member("member-e")).await;
    let live = next_events(&mut second, 1).await;
    assert_eq!(
        seqs(&live),
        [6],
        "an event accepted after the backlog arrives live, once"
    );
}

#[tokio::test]
async fn a_client_that_falls_behind_is_closed_with_a_resume_reason() {
    let dir = temp_repo();
    let running = boot(dir.path()).await;
    let token = owner_token(&running);
    let mut socket = connect(&running, &token, 0).await;

    // The client reads nothing while the origin keeps accepting commands.
    for n in 0..FLOOD {
        let response = post_command(&running, &token, &add_member(&format!("member-{n}"))).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "flood command {n} is accepted"
        );
    }

    let (events, code, reason) = drain(&mut socket).await;

    assert_eq!(
        code, 4001,
        "a lagging client is closed with the resume code"
    );
    let next = events.last().map_or(0, |event| event.seq.get() + 1);
    assert_eq!(
        reason,
        format!("lagged; resume from seq {next}"),
        "the reason names the seq the client must resume from"
    );
    assert_eq!(
        seqs(&events),
        (0..next).collect::<Vec<u64>>(),
        "every event before the close arrived in order, with none dropped"
    );
}

#[tokio::test]
async fn usage_endpoint_reports_the_calls_the_stub_answered() {
    let dir = temp_repo();
    let stub = Stub::default().when(Role::Foreman, |_| true, Reply::text("{}"));
    let ledger = stub.ledger().clone();
    let request = ChatRequest {
        messages: vec![Message::user("ping")],
        ..ChatRequest::default()
    };
    stub.chat(Role::Foreman, request)
        .await
        .expect("the stub answers the scripted call");

    let running = start(config(dir.path(), loopback(), false), stub, ledger)
        .await
        .expect("the server starts");
    let token = owner_token(&running);

    let (status, body) = get_json(&running, "/api/usage", &token).await;

    assert_eq!(status, StatusCode::OK);
    let totals = body.as_array().expect("usage is an array");
    assert_eq!(totals.len(), 1, "one role, provider and model were called");
    assert_eq!(
        totals[0]["role"],
        serde_json::to_value(Role::Foreman).expect("role JSON")
    );
    assert_eq!(totals[0]["provider"], json!("stub"));
    assert_eq!(totals[0]["calls"], json!(1));
}

#[tokio::test]
async fn a_posted_order_runs_its_split_job_through_the_client() {
    let dir = temp_repo();
    // The reply is not JSON on purpose. The Stub records a call when it answers, before the
    // reply is parsed, so the ledger shows the job reached the client either way. The failed
    // job retries a bounded number of times.
    let stub = Stub::default().when(Role::Foreman, |_| true, Reply::text("not json"));
    let ledger = stub.ledger().clone();
    let running = start(config(dir.path(), loopback(), false), stub, ledger)
        .await
        .expect("the server starts");
    let token = owner_token(&running);

    let order = HumanCmd::PostOrder {
        machine: MachineKey::from_str("model").expect("valid machine key"),
        sentence: "split the store into a reader and a writer".to_owned(),
        witnesses: Vec::new(),
    };
    let response = post_command(&running, &token, &order).await;
    assert_eq!(response.status(), StatusCode::OK, "the order is accepted");

    let calls = foreman_calls(&running, &token).await;

    assert!(
        (1..=5).contains(&calls),
        "the split job reached the client, within its retry bound: {calls} calls"
    );
}

#[tokio::test]
async fn without_a_routing_file_a_posted_order_is_split_by_the_offline_models() {
    let dir = temp_repo();
    let models = Models::load(&dir.path().join(".acan/llm.toml"))
        .expect("a missing routing file selects the offline models");
    let ledger = models.ledger();
    let running = start(config(dir.path(), loopback(), false), models, ledger)
        .await
        .expect("the server starts");
    let token = owner_token(&running);
    let order = HumanCmd::PostOrder {
        machine: MachineKey::from_str("model").expect("valid machine key"),
        sentence: "split the store into a reader and a writer".to_owned(),
        witnesses: Vec::new(),
    };
    assert_eq!(
        post_command(&running, &token, &order).await.status(),
        StatusCode::OK
    );

    let deadline = tokio::time::Instant::now() + WAIT;
    let split = loop {
        let (_, body) = get_json(&running, "/api/events?from=0", &token).await;
        let events: Vec<Stamped> = serde_json::from_value(body).expect("events JSON");
        if let Some(split) = events
            .iter()
            .find(|stamped| matches!(stamped.event, Event::OrderSplit { .. }))
        {
            break split.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the offline split never landed: {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        split.actor,
        Actor::System,
        "the split is the job loop's result"
    );
    let (_, usage) = get_json(&running, "/api/usage", &token).await;
    assert_eq!(
        usage[0]["provider"],
        json!("offline"),
        "the call is metered: {usage}"
    );
    assert_eq!(usage[0]["calls"], json!(1), "one call split the order");
    running.stop().await.expect("the server stops");
}
/// Polls the usage endpoint until the foreman has a call, or the wait runs out.
async fn foreman_calls(running: &Running, token: &str) -> u64 {
    let foreman = serde_json::to_value(Role::Foreman).expect("role JSON");
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, usage) = get_json(running, "/api/usage", token).await;
        let calls: u64 = usage
            .as_array()
            .into_iter()
            .flatten()
            .filter(|total| total["role"] == foreman)
            .filter_map(|total| total["calls"].as_u64())
            .sum();
        if calls > 0 || tokio::time::Instant::now() >= deadline {
            return calls;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
