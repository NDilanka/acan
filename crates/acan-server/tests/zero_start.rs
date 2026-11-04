//! A server opened on the user's folder with no codebase: it answers cleanly, signs the owner in
//! with a one-time code, takes a codebase added later and serves it at once, and serves the added
//! codebases again after a restart.
//!
//! Each test makes its user folder and repositories in a temporary directory and listens on an
//! ephemeral port. The model client is the offline stub. A code's expiry is checked by the unit
//! tests beside the sign-in codes, since it takes a minute to reach over HTTP.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use acan_core::MemberId;
use acan_llm::{Stub, UsageLedger};
use acan_server::projects::{ProjectSet, ProjectSpec};
use acan_server::{
    ClientFactory, DEFAULT_WITNESS_CONCURRENCY, Running, ServeConfig, Tokens, UserFolder,
    apply_project_changes, start_open, start_projects,
};
use reqwest::StatusCode;
use serde_json::{Value, json};

/// A repository folder with a few small source files, so the scan has something to index.
fn repo(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("model")).expect("the model folder");
    fs::create_dir_all(dir.join("view")).expect("the view folder");
    fs::write(
        dir.join("model/store.ts"),
        "export function load() {\n  return 1;\n}\n",
    )
    .expect("a source file");
    fs::write(
        dir.join("model/cache.ts"),
        "export function keep(value: number) {\n  return value;\n}\n",
    )
    .expect("a source file");
    fs::write(
        dir.join("view/page.ts"),
        "import { load } from '../model/store';\nexport const page = load();\n",
    )
    .expect("a source file");
    dir
}

/// The user's folder for a test, inside its temporary directory.
fn user_folder(root: &Path) -> PathBuf {
    root.join("user")
}

/// The member whose token is the owner's.
fn founder() -> MemberId {
    MemberId::from_str("owner").expect("a member id")
}

/// The owner's token in the user's folder, as the start wrote it.
fn owner_token(folder: &Path) -> String {
    Tokens::owner_token_in(folder, &founder())
        .expect("the tokens file reads")
        .expect("the user's folder holds the owner's token")
        .as_str()
        .to_owned()
}

/// One client for every request, so the tests share its connection pool.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// The config every test starts with: an ephemeral port, the stub model, and no vision.
fn config(projects: &ProjectSet) -> ServeConfig {
    ServeConfig {
        projects: projects.clone(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
        local_workers: 0,
        map_on_first_view: false,
    }
}

/// The stub model for each project, as the test client every add opens.
fn stub_opener() -> ClientFactory<Stub> {
    Box::new(|_: &ProjectSpec| Ok::<_, anyhow::Error>((Stub::default(), UsageLedger::default())))
}

/// Starts a server on the user's folder `folder` over `projects`, which may be empty.
async fn open(projects: &ProjectSet, folder: &Path) -> Running {
    let clients = projects
        .projects
        .iter()
        .map(|_| (Stub::default(), UsageLedger::default()))
        .collect();
    let user = UserFolder {
        dir: folder.to_path_buf(),
        keys: Stub::default(),
    };
    start_open(config(projects), clients, user, stub_opener())
        .await
        .expect("the server starts")
}

/// Sends `GET path`, with `token` when one is given, and returns the status and the JSON body.
async fn get(running: &Running, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let request = client().get(format!("http://{}{path}", running.local_addr));
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    let response = request.send().await.expect("the request is sent");
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// Sends `POST path` with the raw `body`, with `token` when one is given, and returns the status
/// and the JSON body.
async fn post(
    running: &Running,
    path: &str,
    token: Option<&str>,
    body: String,
) -> (StatusCode, Value) {
    let request = client()
        .post(format!("http://{}{path}", running.local_addr))
        .header("content-type", "application/json")
        .body(body);
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    let response = request.send().await.expect("the request is sent");
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// Adds the folder `repo` while the server runs, checks the add is accepted, and returns the new
/// project's id once its plant answers.
async fn add_and_wait(running: &Running, token: &str, repo: &Path) -> String {
    let (status, body) = post(
        running,
        "/api/settings/projects",
        Some(token),
        json!({ "path": repo.display().to_string() }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "the add is accepted: {body}");
    assert!(
        body["name"].is_string(),
        "the add names the project: {body}"
    );
    assert_eq!(body["status"], "mapping", "the new project maps: {body}");
    let id = body["id"]
        .as_str()
        .expect("the new project's id")
        .to_owned();
    for _ in 0..600 {
        let (status, _) = get(running, &format!("/api/p/{id}/plant"), Some(token)).await;
        if status == StatusCode::OK {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("project {id} never served its plant");
}

/// The projects `GET /api/projects` lists for `token`, as `(id, default)` pairs.
async fn listed(running: &Running, token: &str) -> Vec<(String, bool)> {
    let (status, list) = get(running, "/api/projects", Some(token)).await;
    assert_eq!(status, StatusCode::OK, "the list answers: {list}");
    list.as_array()
        .expect("a list of projects")
        .iter()
        .map(|project| {
            (
                project["id"].as_str().expect("an id").to_owned(),
                project["default"].as_bool().expect("a default flag"),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_server_with_no_codebase_answers_cleanly() {
    let root = tempfile::tempdir().expect("temp dir");
    let folder = user_folder(root.path());
    let running = open(&ProjectSet::from_repos(&[]), &folder).await;
    let token = owner_token(&folder);

    let (status, list) = get(&running, "/api/projects", Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "the owner may list: {list}");
    assert_eq!(list, json!([]), "no project runs");
    let (status, _) = get(&running, "/api/projects", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no token is unauthorized");

    for path in ["/api/plant", "/api/setup"] {
        let (status, body) = get(&running, path, Some(&token)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} has no codebase");
        assert_eq!(body["error"], "no_codebase", "{path} says why: {body}");
    }

    let (status, _) = get(&running, "/health", None).await;
    assert_eq!(status, StatusCode::OK, "the server is healthy");

    let (status, doc) = get(&running, "/api/settings", Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "the settings answer: {doc}");
    assert_eq!(doc["projects"], json!([]), "the settings list no project");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_sign_in_code_works_once_and_gives_the_owner_token() {
    let root = tempfile::tempdir().expect("temp dir");
    let folder = user_folder(root.path());
    let running = open(&ProjectSet::from_repos(&[]), &folder).await;
    let token = owner_token(&folder);

    // The code is a credential, so it travels in request bodies only and no message shows it.
    let (status, minted) = post(&running, "/api/signin", Some(&token), String::new()).await;
    assert_eq!(status, StatusCode::OK, "the owner gets a code");
    assert_eq!(minted["expires_in"], 60, "a code lasts a minute");
    let code = minted["code"].as_str().expect("a code").to_owned();
    assert_eq!(code.len(), 32, "a code is 32 characters");

    let exchange = json!({ "code": code }).to_string();
    let (status, body) = post(&running, "/api/signin/exchange", None, exchange.clone()).await;
    assert_eq!(status, StatusCode::OK, "the code is exchanged");
    assert!(
        body["token"].as_str() == Some(token.as_str()),
        "the exchange gives the owner's token"
    );
    let (status, _) = post(&running, "/api/signin/exchange", None, exchange).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a code works once");

    let unknown = json!({ "code": "0".repeat(32) }).to_string();
    let (status, _) = post(&running, "/api/signin/exchange", None, unknown).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an unknown code is refused"
    );

    let (status, _) = post(&running, "/api/signin", Some("not-a-token"), String::new()).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an unknown bearer gets no code"
    );
    let (status, _) = post(&running, "/api/signin", None, String::new()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no bearer gets no code");

    let (status, _) = post(
        &running,
        "/api/signin/exchange",
        None,
        "not json".to_owned(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a body that is not JSON is refused"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_server_without_a_user_folder_has_no_sign_in() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let projects = ProjectSet::from_repos(std::slice::from_ref(&alpha));
    let clients = vec![(Stub::default(), UsageLedger::default())];
    let running = start_projects(config(&projects), clients)
        .await
        .expect("the server starts");
    let token = running
        .owner_token
        .as_ref()
        .expect("the first start mints the owner's token")
        .as_str()
        .to_owned();

    let (status, body) = post(&running, "/api/signin", Some(&token), String::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "there is no sign-in");
    assert_eq!(
        body["what"], "signin",
        "the answer names the sign-in: {body}"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_codebase_added_to_an_empty_server_serves_at_once() {
    let root = tempfile::tempdir().expect("temp dir");
    let folder = user_folder(root.path());
    let alpha = repo(root.path(), "alpha");
    let running = open(&ProjectSet::from_repos(&[]), &folder).await;
    let token = owner_token(&folder);

    let id = add_and_wait(&running, &token, &alpha).await;
    assert_eq!(
        listed(&running, &token).await,
        [(id, true)],
        "the first codebase added is the default"
    );
    let (status, body) = get(&running, "/api/plant", Some(&token)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the unprefixed plant is the new default's: {body}"
    );

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_second_codebase_joins_and_the_first_stays_the_default() {
    let root = tempfile::tempdir().expect("temp dir");
    let folder = user_folder(root.path());
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let running = open(&ProjectSet::from_repos(&[]), &folder).await;
    let token = owner_token(&folder);

    let first = add_and_wait(&running, &token, &alpha).await;
    let second = add_and_wait(&running, &token, &beta).await;
    assert_eq!(
        listed(&running, &token).await,
        [(first, true), (second.clone(), false)],
        "the first codebase stays the default"
    );
    let (status, _) = get(&running, &format!("/api/p/{second}/plant"), Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "the second codebase serves");

    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_codebase_added_while_running_comes_back_after_a_restart() {
    let root = tempfile::tempdir().expect("temp dir");
    let folder = user_folder(root.path());
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let running = open(&ProjectSet::from_repos(&[]), &folder).await;
    let token = owner_token(&folder);
    let first = add_and_wait(&running, &token, &alpha).await;
    let second = add_and_wait(&running, &token, &beta).await;

    let settings_file = folder.join("settings.json");
    let saved: Value = serde_json::from_str(
        &fs::read_to_string(&settings_file).expect("the settings file in the user's folder"),
    )
    .expect("the settings parse");
    let kept: Vec<&str> = saved["projects"]["add"]
        .as_array()
        .expect("the added folders")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        kept,
        [alpha.display().to_string(), beta.display().to_string()],
        "both folders are kept for the next start"
    );
    assert!(
        saved["projects"].get("live").is_none(),
        "no separate list marks the folders opened while running: {saved}"
    );
    let (status, doc) = get(&running, "/api/settings", Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "the settings answer: {doc}");
    assert_eq!(
        doc["project_changes"]["add"],
        json!([]),
        "both folders are served, so none is pending"
    );
    running.stop().await.expect("the server stops");

    let mut projects = ProjectSet::from_repos(&[]);
    let skipped =
        apply_project_changes(&mut projects, &settings_file).expect("the project changes apply");
    assert!(
        skipped.is_empty(),
        "every folder can be served: {skipped:?}"
    );
    let running = open(&projects, &folder).await;
    assert_eq!(
        listed(&running, &token).await,
        [(first, true), (second, false)],
        "the restart serves both codebases, the first as the default"
    );
    running.stop().await.expect("the restarted server stops");
}
