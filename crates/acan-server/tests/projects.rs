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

use acan_llm::{KeyFile, Stub, UsageLedger};
use acan_server::projects::ProjectSet;
use acan_server::{DEFAULT_WITNESS_CONCURRENCY, Models, Running, ServeConfig, start_projects};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Method, StatusCode};
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

/// Boots a server over `repos` whose model clients read and write the one keys file, the way the
/// binary builds them. A project with its own routing file ignores that file.
async fn boot_sharing_keys(repos: &[PathBuf], keys: &KeyFile) -> Running {
    let clients = repos
        .iter()
        .map(|repo| {
            let llm = repo.join(".acan").join("llm.toml");
            let models = Models::open(&llm, Some(keys.clone())).expect("the models open");
            let ledger = models.ledger();
            (models, ledger)
        })
        .collect();
    start_projects(
        config(repos, SocketAddr::from(([127, 0, 0, 1], 0))),
        clients,
    )
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

/// Runs `git` in `dir` and returns its trimmed standard output. Commits carry their own identity,
/// so the test never reads or writes the machine's git configuration.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Acan Test",
            "-c",
            "user.email=test@acan.invalid",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?} succeeds");
    String::from_utf8(output.stdout)
        .expect("git prints UTF-8")
        .trim()
        .to_owned()
}

/// A repository folder that is a git repository with one commit on `main`, so the server opens
/// its git state and reports the protected head.
fn git_repo(root: &Path, name: &str) -> PathBuf {
    let dir = repo(root, name);
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "initial"]);
    dir
}

/// The entry for `id` in a project list.
fn project<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .expect("a list of projects")
        .iter()
        .find(|project| project["id"] == json!(id))
        .unwrap_or_else(|| panic!("the list holds {id}"))
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
async fn the_project_list_reports_head_workers_and_needs_you_without_paths() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = git_repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let agent = "acan_agent_alpha_0123456789abcdef";
    grant_agent(&alpha, agent, "worker-a");
    let head = git(&alpha, &["rev-parse", "main"]);
    let running = boot(&[alpha, beta]).await;
    let owner = owner_token(&running);

    let (status, list) = get(&running, "/api/projects", &owner).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !list
            .to_string()
            .contains(root.path().to_str().expect("UTF-8 path")),
        "the list never names a repository path"
    );

    let alpha_view = project(&list, "alpha");
    assert_eq!(alpha_view["status"], json!("ready"));
    assert_eq!(alpha_view["protected_branch"], json!("main"));
    assert_eq!(
        alpha_view["head"]["commit"],
        json!(head),
        "the head is the protected branch's commit"
    );
    assert!(alpha_view["head"]["seq"].is_u64(), "the head names a seq");
    assert_eq!(
        alpha_view["workers"],
        json!({ "members": 1, "active": 0 }),
        "alpha lists one agent and none holds a lease"
    );
    assert_eq!(
        alpha_view["needs_you"],
        json!({ "total": 0, "asking": 0, "clashing": 0, "contests": 0, "open_choices": 0 }),
        "a fresh project has nothing for the human"
    );

    let beta_view = project(&list, "beta");
    assert!(
        beta_view["head"]["commit"].is_null(),
        "a folder that is not a git repository has no head commit"
    );
    assert!(beta_view["protected_branch"].is_null());
    assert_eq!(beta_view["workers"], json!({ "members": 0, "active": 0 }));
    assert_eq!(beta_view["needs_you"]["total"], json!(0));
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

/// Writes a tokens file with `entries` before the server starts.
fn write_entries(repo: &Path, entries: &Value) {
    let state = repo.join(".acan");
    fs::create_dir_all(&state).expect("the state directory");
    fs::write(state.join("tokens.json"), entries.to_string()).expect("the tokens file");
}

/// The command that posts an order on the `model` machine.
fn post_order_cmd(sentence: &str) -> Value {
    json!({ "cmd": "post_order", "machine": "model", "sentence": sentence, "witnesses": [] })
}

/// Sends `cmd` to one project's commands route.
async fn command_in(
    running: &Running,
    project: &str,
    token: &str,
    cmd: &Value,
) -> (StatusCode, Value) {
    let response = client()
        .post(format!(
            "http://{}/api/p/{project}/commands",
            running.local_addr
        ))
        .bearer_auth(token)
        .json(cmd)
        .send()
        .await
        .expect("the command is sent");
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// The status the server answers a stream upgrade on one project with. A refused upgrade is its
/// HTTP status; an accepted one is 101.
async fn stream_status(running: &Running, project: &str, token: &str) -> StatusCode {
    let mut request = format!("ws://{}/api/p/{project}/stream", running.local_addr)
        .into_client_request()
        .expect("the stream URL");
    let value = HeaderValue::from_str(&format!("Bearer {token}")).expect("a header value");
    request.headers_mut().insert(AUTHORIZATION, value);
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => {
            drop(socket);
            StatusCode::SWITCHING_PROTOCOLS
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status(),
        Err(err) => panic!("the stream failed without an HTTP answer: {err}"),
    }
}

/// What a write to one project could change in another: its seq, its log, its world file, its
/// protected branch head, and whether it has a worktrees folder.
#[derive(Debug, PartialEq)]
struct Footprint {
    seq: u64,
    log: Vec<Value>,
    world: Vec<u8>,
    head: String,
    worktrees: bool,
}

async fn footprint(running: &Running, project: &str, token: &str, repo: &Path) -> Footprint {
    let log = events_of(running, project, token).await;
    Footprint {
        seq: log.last().map_or(0, seq_of),
        log,
        world: fs::read(repo.join(".acan").join("world.db")).expect("the world file"),
        head: git(repo, &["rev-parse", "main"]),
        worktrees: repo.join(".acan").join("worktrees").exists(),
    }
}

#[tokio::test]
async fn an_order_in_a_never_touches_b() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = git_repo(root.path(), "alpha");
    let beta = git_repo(root.path(), "beta");
    let running = boot(&[alpha, beta.clone()]).await;
    let owner = owner_token(&running);
    let before = footprint(&running, "beta", &owner, &beta).await;

    let (status, body) = command_in(&running, "alpha", &owner, &post_order_cmd("alpha work")).await;
    assert_eq!(status, StatusCode::OK, "alpha takes the order: {body}");
    let alpha_log = events_of(&running, "alpha", &owner).await;
    assert!(
        alpha_log
            .iter()
            .any(|event| event.to_string().contains("alpha work")),
        "the order is in alpha's log"
    );

    let after = footprint(&running, "beta", &owner, &beta).await;
    assert_eq!(
        after, before,
        "beta's seq, log, world file, head and worktrees are unchanged"
    );
    running.stop().await.expect("the server stops");
}

/// A scope at revision 1 over `base`, with one open choice and no rules.
fn scope_with_choice(order: &str, base: &str) -> Value {
    json!({
        "version": "acan-scope/1",
        "order": order,
        "rev": 1,
        "base_commit": base,
        "body": {
            "may": [],
            "must_keep": [],
            "open": [{
                "id": "c-retry",
                "machine_id": "model",
                "question": "Should a failed request be retried?",
                "options": [
                    { "key": "never", "label": "Never retry" },
                    { "key": "once", "label": "Retry once" }
                ]
            }]
        }
    })
}

#[tokio::test]
async fn a_real_order_with_an_open_choice_is_counted_for_its_project_only() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = git_repo(root.path(), "alpha");
    let beta = git_repo(root.path(), "beta");
    let base = git(&alpha, &["rev-parse", "main"]);
    let running = boot(&[alpha, beta]).await;
    let owner = owner_token(&running);

    let (status, body) = command_in(&running, "alpha", &owner, &post_order_cmd("alpha work")).await;
    assert_eq!(status, StatusCode::OK, "alpha takes the order: {body}");
    let revise = json!({
        "cmd": "revise_scope",
        "order": 0,
        "scope": scope_with_choice("o-0", &base),
    });
    let (status, body) = command_in(&running, "alpha", &owner, &revise).await;
    assert_eq!(status, StatusCode::OK, "the scope is revised: {body}");

    let (status, list) = get(&running, "/api/projects", &owner).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        project(&list, "alpha")["needs_you"],
        json!({ "total": 1, "asking": 0, "clashing": 0, "contests": 0, "open_choices": 1 }),
        "the order's open choice is what alpha asks of the human"
    );
    assert_eq!(
        project(&list, "beta")["needs_you"]["total"],
        json!(0),
        "beta asks nothing"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn tokens_are_per_project() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let member = "acan_member_alpha_0123456789abcdef";
    let agent = "acan_agent_alpha_0123456789abcdef";
    let beta_agent = "acan_agent_beta_0123456789abcdef";
    write_entries(
        &alpha,
        &json!([
            { "token": member, "principal": { "kind": "member", "id": "lee" } },
            { "token": agent, "principal": { "kind": "agent", "id": "worker-a" } }
        ]),
    );
    grant_agent(&beta, beta_agent, "worker-b");
    let running = boot(&[alpha, beta]).await;

    let owners = [("alpha", member), ("alpha", agent), ("beta", beta_agent)];
    for (home, token) in owners {
        let other = if home == "alpha" { "beta" } else { "alpha" };
        assert_eq!(
            get(&running, &format!("/api/p/{home}/plant"), token)
                .await
                .0,
            StatusCode::OK,
            "{token} opens its own project"
        );
        assert_eq!(
            get(&running, &format!("/api/p/{other}/plant"), token)
                .await
                .0,
            StatusCode::UNAUTHORIZED,
            "{token} gets 401 on the plant of {other}"
        );
        assert_eq!(
            get(&running, &format!("/api/p/{other}/events?from=0"), token)
                .await
                .0,
            StatusCode::UNAUTHORIZED,
            "{token} gets 401 on the events of {other}"
        );
        let (status, _) = command_in(&running, other, token, &post_order_cmd("stray")).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{token} gets 401 on the commands of {other}"
        );
        assert_eq!(
            stream_status(&running, other, token).await,
            StatusCode::UNAUTHORIZED,
            "{token} gets 401 on the stream of {other}"
        );
        let (status, _) = rpc(&running, &format!("/api/p/{other}/mcp"), token).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{token} gets 401 on the MCP endpoint of {other}"
        );
    }

    let (_, list) = get(&running, "/api/projects", member).await;
    assert_eq!(
        list.as_array().expect("a list").len(),
        1,
        "the member lists only alpha"
    );
    running.stop().await.expect("the server stops");
}

/// One request to a project with `token`, and the status and JSON body it answers with.
async fn send(
    running: &Running,
    method: Method,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = client()
        .request(method, format!("http://{}{path}", running.local_addr))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("the request is sent");
    let status = response.status();
    (
        status,
        response.json::<Value>().await.unwrap_or(Value::Null),
    )
}

/// The entry for `role` in a setup status.
fn role<'a>(status: &'a Value, name: &str) -> &'a Value {
    status["roles"]
        .as_array()
        .expect("a list of roles")
        .iter()
        .find(|role| role["role"] == json!(name))
        .unwrap_or_else(|| panic!("the status holds {name}"))
}

/// Routing for a project that decides its own providers.
const ROUTING: &str = r#"
[[providers]]
name = "meta"
base_url = "https://api.meta.ai/v1"
api_key_env = "META_API_KEY"
default_model = "muse-spark-1.3"

[routing.foreman]
provider = "meta"

[routing.worker]
provider = "meta"

[routing.fast]
provider = "meta"
"#;

#[tokio::test]
async fn keys_shared_routing_not_shared() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let routed = repo(root.path(), "gamma");
    fs::create_dir_all(routed.join(".acan")).expect("the state directory");
    fs::write(routed.join(".acan").join("llm.toml"), ROUTING).expect("the routing file");
    let keys = KeyFile::new(root.path().join("keys").join("keys.toml"));
    keys.set_key("meta", "meta-saved-key-0123456789")
        .expect("a key is saved");
    let running = boot_sharing_keys(&[alpha, beta, routed], &keys).await;
    let owner = owner_token(&running);

    let (_, before) = get(&running, "/api/p/beta/setup", &owner).await;
    assert_eq!(before["configured"], json!(true), "beta sees the saved key");

    let (status, body) = send(
        &running,
        Method::PUT,
        "/api/p/alpha/setup/roles/worker",
        &owner,
        Some(json!({ "provider": "meta" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "alpha picks meta for worker: {body}"
    );
    let (_, after_pick) = get(&running, "/api/p/beta/setup", &owner).await;
    assert_eq!(
        role(&after_pick, "worker")["picked"],
        json!(true),
        "beta uses the pick made in alpha"
    );

    let (status, body) = send(
        &running,
        Method::DELETE,
        "/api/p/alpha/setup/keys/meta",
        &owner,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "alpha removes the key: {body}");
    assert_eq!(body["removed"], json!(true));
    let (_, after_removal) = get(&running, "/api/p/beta/setup", &owner).await;
    assert_eq!(
        after_removal["configured"],
        json!(false),
        "beta no longer has the key"
    );
    assert_eq!(
        role(&after_removal, "worker")["picked"],
        json!(false),
        "beta drops the pick that used the key"
    );

    let (_, routed_status) = get(&running, "/api/p/gamma/setup", &owner).await;
    assert_eq!(routed_status["source"], json!("routing_file"));
    assert_eq!(routed_status["can_save"], json!(false));
    let (status, _) = send(
        &running,
        Method::DELETE,
        "/api/p/gamma/setup/keys/meta",
        &owner,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a routing project refuses a removal, so the shared keys do not reach it"
    );
    running.stop().await.expect("the server stops");
}

/// One tools/list call to a project's MCP endpoint, and the status it answers with.
async fn rpc(running: &Running, path: &str, token: &str) -> (StatusCode, Value) {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
    let response = client()
        .post(format!("http://{}{path}", running.local_addr))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("the MCP call is sent");
    let status = response.status();
    (
        status,
        response.json::<Value>().await.unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn unprefixed_equals_default() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = git_repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let agent = "acan_agent_alpha_0123456789abcdef";
    grant_agent(&alpha, agent, "worker-a");
    let running = boot(&[alpha, beta]).await;
    let owner = owner_token(&running);
    let (status, body) = command_in(&running, "alpha", &owner, &post_order_cmd("alpha work")).await;
    assert_eq!(status, StatusCode::OK, "alpha takes the order: {body}");

    for path in [
        "/plant",
        "/events?from=0",
        "/usage",
        "/setup",
        "/orders/o-0/plant-view",
    ] {
        let (unprefixed_status, unprefixed) = get(&running, &format!("/api{path}"), &owner).await;
        let (prefixed_status, prefixed) =
            get(&running, &format!("/api/p/alpha{path}"), &owner).await;
        assert_eq!(
            unprefixed_status, prefixed_status,
            "{path}: the same status"
        );
        assert_eq!(
            unprefixed, prefixed,
            "{path}: the same answer from the default project"
        );
    }

    let (unprefixed_status, unprefixed) = rpc(&running, "/mcp", agent).await;
    let (prefixed_status, prefixed) = rpc(&running, "/api/p/alpha/mcp", agent).await;
    assert_eq!(unprefixed_status, prefixed_status);
    assert_eq!(
        unprefixed, prefixed,
        "MCP answers the same from the default"
    );
    running.stop().await.expect("the server stops");
}
