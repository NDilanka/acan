//! A project added while the server runs: its folder is opened, served and scanned with no restart,
//! and its routes answer under its own id. The add is for the owner alone. It checks the folder,
//! refuses a folder the server already serves, and is kept in the settings file so the next start
//! serves it too.
//!
//! Each test starts a server over temporary repositories on an ephemeral port. The model client is
//! the offline stub.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use acan_llm::{Stub, UsageLedger};
use acan_server::projects::{ProjectSet, ProjectSpec};
use acan_server::{
    ClientFactory, DEFAULT_WITNESS_CONCURRENCY, Running, ServeConfig, default_settings_file,
    start_projects_adding, start_projects_serving,
};
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

/// The token of the principal `kind` `id` in the repository's tokens file.
fn token_of(repo: &Path, kind: &str, id: &str) -> String {
    let text = fs::read_to_string(repo.join(".acan").join("tokens.json")).expect("the tokens file");
    let entries: Vec<Value> = serde_json::from_str(&text).expect("the tokens parse");
    entries
        .iter()
        .find(|entry| entry["principal"]["kind"] == kind && entry["principal"]["id"] == id)
        .and_then(|entry| entry["token"].as_str())
        .unwrap_or_else(|| panic!("the {kind} {id} token is in the tokens file"))
        .to_owned()
}

/// The owner's token, as the first start wrote it.
fn owner_token(repo: &Path) -> String {
    token_of(repo, "member", "owner")
}

/// One client for every request, so the tests share its connection pool.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// The config every test starts with: an ephemeral port, the stub model, and no vision.
fn config(projects: &ProjectSet, local_workers: usize) -> ServeConfig {
    ServeConfig {
        projects: projects.clone(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
        local_workers,
        map_on_first_view: false,
    }
}

/// The stub model for each project, as the test client every add opens.
fn stub_opener() -> ClientFactory<Stub> {
    Box::new(|_: &ProjectSpec| Ok::<_, anyhow::Error>((Stub::default(), UsageLedger::default())))
}

/// Starts a server over `projects` that can add projects while it runs.
async fn start(projects: &ProjectSet, local_workers: usize) -> Running {
    let clients = projects
        .projects
        .iter()
        .map(|_| (Stub::default(), UsageLedger::default()))
        .collect();
    let settings = default_settings_file(projects);
    start_projects_adding(
        config(projects, local_workers),
        clients,
        Some(settings),
        stub_opener(),
    )
    .await
    .expect("the server starts")
}

/// Sends `GET path` with `token`, and returns the status and the JSON body.
async fn get(running: &Running, path: &str, token: &str) -> (StatusCode, Value) {
    let response = client()
        .get(format!("http://{}{path}", running.local_addr))
        .bearer_auth(token)
        .send()
        .await
        .expect("the request is sent");
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// Sends a live add for the folder `path` with `token`.
async fn add(running: &Running, token: Option<&String>, path: &str) -> (StatusCode, Value) {
    let request = client()
        .post(format!(
            "http://{}/api/settings/projects",
            running.local_addr
        ))
        .header("content-type", "application/json")
        .body(json!({ "path": path }).to_string());
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    let response = request.send().await.expect("the request is sent");
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// Sends `PUT /api/settings` with `body` and `token`, and returns the status and the JSON body.
async fn put_settings(running: &Running, token: &str, body: Value) -> (StatusCode, Value) {
    let response = client()
        .put(format!("http://{}/api/settings", running.local_addr))
        .header("content-type", "application/json")
        .bearer_auth(token)
        .body(body.to_string())
        .send()
        .await
        .expect("the request is sent");
    let status = response.status();
    (status, response.json().await.unwrap_or(Value::Null))
}

/// Waits until project `id`'s first scan reports `stage`, and fails after twenty seconds.
async fn wait_for_stage(running: &Running, id: &str, token: &str, stage: &str) {
    for _ in 0..200 {
        let (status, plant) = get(running, &format!("/api/p/{id}/plant"), token).await;
        if status == StatusCode::OK && plant["scan"]["stage"] == stage {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("project {id} never reached the scan stage {stage}");
}

/// The owner's settings document.
async fn settings_doc(running: &Running, token: &str) -> Value {
    let (status, doc) = get(running, "/api/settings", token).await;
    assert_eq!(status, StatusCode::OK);
    doc
}

#[tokio::test]
async fn a_folder_added_while_the_server_runs_is_served_and_scanned() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let projects = ProjectSet::from_repos(std::slice::from_ref(&alpha));
    let running = start(&projects, 0).await;
    let token = owner_token(&alpha);

    let (status, body) = add(&running, Some(&token), &beta.display().to_string()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "the add is accepted: {body}");
    assert_eq!(body["id"], "beta");
    assert_eq!(body["status"], "mapping");

    let (status, list) = get(&running, "/api/projects", &token).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = list
        .as_array()
        .expect("a list of projects")
        .iter()
        .filter_map(|project| project["id"].as_str())
        .collect();
    assert_eq!(ids, ["alpha", "beta"], "the list shows the added project");

    // The new project's own routes answer, and its first scan runs to done with no restart.
    wait_for_stage(&running, "beta", &token, "done").await;
    let (status, _) = get(&running, "/api/p/beta/events", &token).await;
    assert_eq!(status, StatusCode::OK, "the added project's events answer");

    // The folder is kept in the settings, so the next start serves it too. It is served now, so
    // it is not pending.
    let doc = settings_doc(&running, &token).await;
    assert!(
        doc["project_changes"]["add"]
            .as_array()
            .expect("the pending folders")
            .is_empty(),
        "a folder served live is not pending: {doc}"
    );
    let saved: Value = serde_json::from_str(
        &fs::read_to_string(default_settings_file(&projects)).expect("the settings file"),
    )
    .expect("the settings parse");
    assert_eq!(
        saved["projects"]["add"].as_array().map(Vec::len),
        Some(1),
        "the folder is kept for the next start: {saved}"
    );

    // Adding the same folder again is refused, since the server already serves it.
    let (status, body) = add(&running, Some(&token), &beta.display().to_string()).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a second add is refused: {body}"
    );

    // A folder added through a settings change is not live, so it waits for the next start and
    // shows as pending.
    let gamma = repo(root.path(), "gamma");
    let (status, body) = put_settings(
        &running,
        &token,
        json!({ "projects": { "add": [gamma.display().to_string()] } }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the settings change is saved: {body}"
    );
    let doc = settings_doc(&running, &token).await;
    let pending = doc["project_changes"]["add"]
        .as_array()
        .expect("the pending folders");
    assert_eq!(
        pending.len(),
        1,
        "the settings add waits for the start: {doc}"
    );
    assert!(
        pending[0]
            .as_str()
            .is_some_and(|path| path.ends_with("gamma")),
        "the pending folder is the settings add: {doc}"
    );

    running
        .stop()
        .await
        .expect("the server stops with the added project");
}

#[tokio::test]
async fn a_live_add_is_for_the_owner_alone() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let projects = ProjectSet::from_repos(std::slice::from_ref(&alpha));
    // One local worker, so its agent token is in the tokens file to test with.
    let running = start(&projects, 1).await;
    let agent = token_of(&alpha, "agent", "local-1");
    let beta_path = beta.display().to_string();

    let (status, _) = add(&running, None, &beta_path).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no token is unauthorized");
    let (status, _) = add(&running, Some(&agent), &beta_path).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an agent may not add a project"
    );

    let doc = settings_doc(&running, &owner_token(&alpha)).await;
    assert!(
        doc["project_changes"]["add"]
            .as_array()
            .expect("the added folders")
            .is_empty(),
        "a refused add changes no setting"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_folder_that_cannot_be_served_is_refused_and_the_settings_stay() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let inside_state = alpha.join(".acan").join("inner");
    fs::create_dir_all(&inside_state).expect("a folder inside the state folder");
    let file = alpha.join("model/store.ts");
    let projects = ProjectSet::from_repos(std::slice::from_ref(&alpha));
    let running = start(&projects, 0).await;
    let token = owner_token(&alpha);

    let missing = root.path().join("missing").display().to_string();
    let as_file = file.display().to_string();
    let inside = inside_state.display().to_string();
    let served = alpha.display().to_string();
    let cases = [
        ("", StatusCode::BAD_REQUEST, "an empty path"),
        (
            "relative/folder",
            StatusCode::UNPROCESSABLE_ENTITY,
            "a relative path",
        ),
        (
            missing.as_str(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a folder that does not exist",
        ),
        (
            as_file.as_str(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a file, not a folder",
        ),
        (
            inside.as_str(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a folder inside another project's state folder",
        ),
        (
            served.as_str(),
            StatusCode::CONFLICT,
            "the folder the server already serves",
        ),
    ];
    for (path, want, why) in cases {
        let (status, body) = add(&running, Some(&token), path).await;
        assert_eq!(status, want, "{why} is refused: {body}");
    }

    let doc = settings_doc(&running, &token).await;
    assert!(
        doc["project_changes"]["add"]
            .as_array()
            .expect("the added folders")
            .is_empty(),
        "no refused add is kept in the settings"
    );
    let (status, list) = get(&running, "/api/projects", &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        list.as_array().expect("a list").len(),
        1,
        "no project was added"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_server_started_without_live_adds_refuses_them() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let projects = ProjectSet::from_repos(std::slice::from_ref(&alpha));
    let clients = vec![(Stub::default(), UsageLedger::default())];
    let settings = default_settings_file(&projects);
    let running = start_projects_serving(config(&projects, 0), clients, Some(settings))
        .await
        .expect("the server starts");
    let token = owner_token(&alpha);

    let (status, body) = add(&running, Some(&token), &beta.display().to_string()).await;
    assert_eq!(status, StatusCode::CONFLICT, "the add is refused: {body}");
    assert_eq!(body["error"], "conflict");
    assert_eq!(body["what"], "live_add_unavailable");
    running.stop().await.expect("the server stops");
}
