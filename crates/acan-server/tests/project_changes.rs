//! Project changes that a settings change records, and the start applies: a local worker count per
//! project, a folder added as a project, a folder removed, and an added folder that cannot be served.
//!
//! Each test starts servers over temporary repositories on an ephemeral port. The model client is
//! the offline stub. A restart applies the changes the way the binary does: the folders the command
//! line names are changed by what the settings file holds, before the clients are built.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use acan_llm::{Stub, UsageLedger};
use acan_server::projects::ProjectSet;
use acan_server::{
    DEFAULT_WITNESS_CONCURRENCY, Running, ServeConfig, apply_project_changes,
    default_settings_file, start_projects_with_settings,
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

/// The owner's token, as the first start wrote it to the repository's tokens file.
fn owner_token(repo: &Path) -> String {
    let text = fs::read_to_string(repo.join(".acan").join("tokens.json")).expect("the tokens file");
    let entries: Vec<Value> = serde_json::from_str(&text).expect("the tokens parse");
    entries
        .iter()
        .find(|entry| entry["principal"]["kind"] == "member" && entry["principal"]["id"] == "owner")
        .and_then(|entry| entry["token"].as_str())
        .expect("the owner's token is in the tokens file")
        .to_owned()
}

/// One client for every request, so the tests share its connection pool.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Starts a server over `projects`, with its settings in the default project's state directory.
async fn start(projects: &ProjectSet, local_workers: usize) -> Running {
    let clients = projects
        .projects
        .iter()
        .map(|_| (Stub::default(), UsageLedger::default()))
        .collect();
    let config = ServeConfig {
        projects: projects.clone(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
        local_workers,
        map_on_first_view: false,
    };
    let settings = default_settings_file(projects);
    start_projects_with_settings(config, clients, Some(settings))
        .await
        .expect("the server starts")
}

/// Starts a server the way the binary does: the folders the command line names, changed by the
/// project changes in the settings file. Returns the server and each added folder that could not
/// be served, with the reason.
async fn restart(repos: &[PathBuf], local_workers: usize) -> (Running, Vec<String>) {
    let mut projects = ProjectSet::from_repos(repos);
    let settings = default_settings_file(&projects);
    let skipped =
        apply_project_changes(&mut projects, &settings).expect("the project changes apply");
    (start(&projects, local_workers).await, skipped)
}

/// The owner's settings document.
async fn settings_doc(running: &Running, token: &str) -> Value {
    let response = client()
        .get(format!("http://{}/api/settings", running.local_addr))
        .bearer_auth(token)
        .send()
        .await
        .expect("the request is sent");
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.expect("the body is JSON")
}

/// Sends a settings change as the owner.
async fn put_settings(running: &Running, token: &str, body: &Value) -> reqwest::Response {
    client()
        .put(format!("http://{}/api/settings", running.local_addr))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .expect("the request is sent")
}

/// How many local workers project `id` runs, as its workers route reports them.
async fn workers_of(running: &Running, token: &str, id: &str) -> usize {
    let response = client()
        .get(format!("http://{}/api/p/{id}/workers", running.local_addr))
        .bearer_auth(token)
        .send()
        .await
        .expect("the request is sent");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("the body is JSON");
    body["workers"].as_array().expect("a list of workers").len()
}

#[tokio::test]
async fn a_worker_count_in_settings_is_what_its_project_runs_at_the_next_start() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let repos = [alpha.clone(), beta.clone()];
    let (running, skipped) = restart(&repos, 0).await;
    assert!(skipped.is_empty(), "{skipped:?}");
    let token = owner_token(&alpha);

    let response = put_settings(
        &running,
        &token,
        &json!({"projects": {"workers": {"beta": 2}}}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        workers_of(&running, &token, "beta").await,
        0,
        "the count takes effect at the next start"
    );
    running.stop().await.expect("the server stops");

    let (running, _) = restart(&repos, 1).await;
    assert_eq!(workers_of(&running, &token, "beta").await, 2);
    assert_eq!(
        workers_of(&running, &token, "alpha").await,
        1,
        "a project with no count runs the count the command line gives"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_folder_added_in_settings_is_served_at_every_later_start() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let alpha_only = [alpha.clone()];
    let (running, _) = restart(&alpha_only, 0).await;
    let token = owner_token(&alpha);
    let beta_text = beta.display().to_string();

    let response = put_settings(
        &running,
        &token,
        &json!({"projects": {"add": [beta_text.clone()]}}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        settings_doc(&running, &token).await["project_changes"]["add"],
        json!([beta_text.clone()])
    );
    running.stop().await.expect("the server stops");

    // The command line still names only alpha, and beta is served from the settings.
    let (running, skipped) = restart(&alpha_only, 0).await;
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(running.project_ids(), ["alpha", "beta"]);
    assert_eq!(
        settings_doc(&running, &token).await["project_changes"]["add"],
        json!([beta_text.clone()]),
        "the change is kept after the start"
    );
    running.stop().await.expect("the server stops");

    let (running, _) = restart(&alpha_only, 0).await;
    assert_eq!(
        running.project_ids(),
        ["alpha", "beta"],
        "a later start serves it too"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_project_removed_in_settings_stays_removed_until_its_folder_is_added_again() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let beta = repo(root.path(), "beta");
    let repos = [alpha.clone(), beta.clone()];
    let alpha_only = [alpha.clone()];
    let (running, _) = restart(&repos, 0).await;
    let token = owner_token(&alpha);

    let default = put_settings(
        &running,
        &token,
        &json!({"projects": {"remove": ["alpha"]}}),
    )
    .await;
    assert_eq!(
        default.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "the default project cannot be removed"
    );
    let response = put_settings(&running, &token, &json!({"projects": {"remove": ["beta"]}})).await;
    assert_eq!(response.status(), StatusCode::OK);
    running.stop().await.expect("the server stops");

    // The command line still names beta, and the removal keeps it out.
    let (running, _) = restart(&repos, 0).await;
    assert_eq!(running.project_ids(), ["alpha"]);
    let gone = put_settings(&running, &token, &json!({"projects": {"remove": ["beta"]}})).await;
    assert_eq!(
        gone.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a project that is not running cannot be removed again"
    );
    let back = put_settings(
        &running,
        &token,
        &json!({"projects": {"add": [beta.display().to_string()]}}),
    )
    .await;
    assert_eq!(back.status(), StatusCode::OK);
    let doc = settings_doc(&running, &token).await;
    assert_eq!(doc["project_changes"]["remove"], json!([]));
    running.stop().await.expect("the server stops");

    let (running, _) = restart(&alpha_only, 0).await;
    assert_eq!(
        running.project_ids(),
        ["alpha", "beta"],
        "added again, so served"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn an_added_folder_that_cannot_be_served_is_named_once_and_dropped() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = repo(root.path(), "alpha");
    let alpha_only = [alpha.clone()];
    let (running, _) = restart(&alpha_only, 0).await;
    let token = owner_token(&alpha);

    // A folder inside alpha's state directory cannot be served as a project of its own.
    let inner = alpha.join(".acan").join("inner");
    fs::create_dir_all(&inner).expect("the nested folder");
    let response = put_settings(
        &running,
        &token,
        &json!({"projects": {"add": [inner.display().to_string()]}}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    running.stop().await.expect("the server stops");

    let (running, skipped) = restart(&alpha_only, 0).await;
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(
        skipped[0].starts_with(&inner.display().to_string()),
        "{skipped:?}"
    );
    assert!(skipped[0].contains("state directory"), "{skipped:?}");
    assert_eq!(running.project_ids(), ["alpha"]);
    assert_eq!(
        settings_doc(&running, &token).await["project_changes"]["add"],
        json!([]),
        "the folder is dropped from the changes"
    );
    running.stop().await.expect("the server stops");

    let (running, skipped) = restart(&alpha_only, 0).await;
    assert!(skipped.is_empty(), "{skipped:?}");
    running.stop().await.expect("the server stops");
}
