//! Several projects in one server: each project's routes and tokens, the unprefixed default, and
//! the one human token every project accepts.
//!
//! Each test boots a server over temporary repositories on an ephemeral port. The model client is
//! the offline stub.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use acan_llm::{Stub, UsageLedger};
use acan_server::projects::ProjectSet;
use acan_server::{DEFAULT_WITNESS_CONCURRENCY, Running, ServeConfig, start_projects};
use reqwest::StatusCode;
use serde_json::{Value, json};

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
