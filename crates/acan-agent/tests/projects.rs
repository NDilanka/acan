//! The harness against a server that runs several projects. A worker's MCP calls go to its
//! project's routes under `/api/p/<id>`, and its token opens only the project that lists it.
//!
//! The server is real and the model is the offline stub, so no test needs the network.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use acan_agent::McpClient;
use acan_llm::{Stub, UsageLedger};
use acan_server::projects::ProjectSet;
use acan_server::{DEFAULT_WITNESS_CONCURRENCY, Running, ServeConfig, start_projects};

const ALPHA_WORKER: &str = "acan_agent_alpha_0123456789abcdef";
const BETA_WORKER: &str = "acan_agent_beta_0123456789abcdef";

/// A repository folder with one source file, and a tokens file that grants `token` to `agent`.
fn project(root: &Path, name: &str, token: &str, agent: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("model")).expect("the machine folder");
    fs::write(
        dir.join("model/store.ts"),
        "export function load() {\n  return 1;\n}\n",
    )
    .expect("a source file");
    let state = dir.join(".acan");
    fs::create_dir_all(&state).expect("the state directory");
    let entries =
        format!(r#"[{{"token": "{token}", "principal": {{"kind": "agent", "id": "{agent}"}}}}]"#);
    fs::write(state.join("tokens.json"), entries).expect("the tokens file");
    dir
}

async fn start(repos: &[PathBuf]) -> Running {
    let config = ServeConfig {
        projects: ProjectSet::from_repos(repos),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
        local_workers: 0,
        map_on_first_view: false,
    };
    let clients = repos
        .iter()
        .map(|_| (Stub::default(), UsageLedger::default()))
        .collect();
    start_projects(config, clients)
        .await
        .expect("the server starts over the projects")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_reaches_its_own_project_and_no_other() {
    let root = tempfile::tempdir().expect("temp dir");
    let alpha = project(root.path(), "alpha", ALPHA_WORKER, "worker-a");
    let beta = project(root.path(), "beta", BETA_WORKER, "worker-b");
    let running = start(&[alpha, beta]).await;
    let base = format!("http://{}", running.local_addr);

    McpClient::new(&format!("{base}/api/p/alpha"), ALPHA_WORKER.to_owned())
        .connect()
        .await
        .expect("alpha's worker connects to alpha");
    McpClient::new(&format!("{base}/api/p/beta"), BETA_WORKER.to_owned())
        .connect()
        .await
        .expect("beta's worker connects to beta");

    let refused = McpClient::new(&format!("{base}/api/p/beta"), ALPHA_WORKER.to_owned())
        .connect()
        .await
        .expect_err("alpha's worker is refused by beta");
    assert!(
        refused.to_string().contains("refused the agent token"),
        "the worker names the refusal: {refused:#}"
    );
    let refused = McpClient::new(&format!("{base}/api/p/alpha"), BETA_WORKER.to_owned())
        .connect()
        .await
        .expect_err("beta's worker is refused by alpha");
    assert!(
        refused.to_string().contains("refused the agent token"),
        "the worker names the refusal: {refused:#}"
    );

    McpClient::new(&base, ALPHA_WORKER.to_owned())
        .connect()
        .await
        .expect("the unprefixed route is the default project, which is alpha");
    running.stop().await.expect("the server stops");
}
