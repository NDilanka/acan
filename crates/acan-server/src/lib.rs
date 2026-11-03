//! The acan server: one world's origin, its event log, and the HTTP and WebSocket surface around it.
//!
//! [`start`] opens the repository's world in `.acan/world.db`, scans the code, and serves the API.
//! One writer task owns the origin and the current scan; HTTP handlers and the job loop send it
//! commands and await a reply, and readers use the snapshot it publishes after each persisted
//! batch. [`Running::stop`] is a barrier: it returns only when no write can still happen.

mod auth;
pub mod deps;
#[cfg(test)]
mod e2e;
mod examples;
mod git;
mod host;
mod http;
mod index;
pub mod init;
pub mod init_cli;
mod intent_api;
mod jobs;
#[allow(
    dead_code,
    reason = "the ruling answer path and its folds have no caller yet; intent_api owns the route"
)]
mod land;
mod mcp;
pub mod model;
mod models;
#[allow(
    dead_code,
    reason = "reading stored observations back has no caller in the server yet; only the store writes them"
)]
mod observations;
mod observe;
mod offline;
mod plant_view;
mod public;
#[allow(
    dead_code,
    reason = "the receipt document renderer has no caller in the server yet; the receipt route serves the stored receipt"
)]
mod receipt;
#[allow(
    dead_code,
    reason = "the receipt command line has no caller in the server yet; it renders the receipt renderer, which has none"
)]
pub mod receipt_cli;
mod redact;
pub mod release;
pub mod replay;
mod ruling;
mod setup;
mod store;
mod stream;
mod tour;
mod vision;
mod workers;
mod writer;

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acan_core::{Command, HumanCmd, MemberId, Origin, Seq, SystemCmd, WorldId, WorldRef};
use acan_llm::{ModelClient, UsageLedger};
use anyhow::{Context, bail};
use axum::Extension;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

pub use auth::{Principal, Token, Tokens};
pub use host::{Host, HostParts, LIVE_BUFFER, SubmitError};
pub use index::{IndexError, Scan, code_index, scan};
/// Starts the job loop for a host built by hand. [`start`] does this for the server; a caller
/// that builds its own [`Host`] runs the same loop with this.
pub use jobs::spawn as spawn_jobs;
pub use models::Models;
pub use store::EventStore;
pub use vision::{
    CHROME, Comparison, EDGE, MustKeepItem, Verdict, Vision, VisionCheck, VisionConfig,
    VisionWitness, WitnessStatus, default_browser, default_data_dir,
};
pub use workers::{State as WorkerState, Status as WorkerStatus, WorkersView};

use crate::public::PublicRoot;
use crate::writer::Writer;

const STATE_DIR: &str = ".acan";
const DB_FILE: &str = "world.db";
const OWNER: &str = "owner";

/// The file that holds the repository's tokens: the owner's and each agent's.
#[must_use]
pub fn tokens_file(repo: &Path) -> PathBuf {
    repo.join(STATE_DIR).join(auth::TOKENS_FILE)
}

/// How long shutdown waits for stream tasks to close before giving up on them. Streams only
/// read the log and write to their socket, so a wedged one cannot hold a write hostage.
const STREAM_DRAIN: Duration = Duration::from_secs(2);

/// How many witness runs may run at once unless configured otherwise. Each run is a test command,
/// an HTTP check or one contrast container. A contrast probe runs base A, base B and the candidate
/// together, so fewer than three permits makes every contrast slower than one probe at a time.
pub const DEFAULT_WITNESS_CONCURRENCY: usize = 3;

/// How many probes of one contrast run at once. Every container run also takes a permit from the
/// witness pool, so the pool, not this value, bounds how many containers run at once. Three passed
/// the intent gate suite three times in a row.
pub const DEFAULT_PROBE_CONCURRENCY: usize = 3;

/// What to serve, and where.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The repository whose world this server runs.
    pub repo: PathBuf,
    /// The address to listen on. Port 0 picks a free port.
    pub addr: SocketAddr,
    /// Allows a non-loopback address. Without it, binding elsewhere is refused.
    pub team: bool,
    /// A dedicated public directory served for every path the API does not claim, such as the
    /// renderer page. It must not contain the repository or its `.acan` state directory.
    pub web: Option<PathBuf>,
    /// The branch finished work lands on. `None` lands on the branch checked out in the
    /// repository. A clean checkout of that branch is fast-forwarded to the landed commit. A
    /// checkout with uncommitted changes, or with an untracked or ignored file the land would
    /// overwrite, refuses the land and nothing moves.
    pub protected_branch: Option<String>,
    /// The base URL HTTP witness checks target, such as `http://127.0.0.1:3000`. `None` makes
    /// every HTTP check report an error, since the server does not start the app yet.
    pub witness_http_base_url: Option<String>,
    /// How many witness runs may run at once. Must be at least 1.
    pub witness_concurrency: usize,
    /// The advisory vision witness. `None` runs no vision checks, and the vision route then
    /// serves a `failed` record for every gate run.
    pub vision: Option<VisionConfig>,
}

/// A server that is listening. Dropping it does not stop the server.
#[derive(Debug)]
pub struct Running {
    /// The address the server bound, with the real port.
    pub local_addr: SocketAddr,
    /// The owner token. Present only on the first start, when it was minted.
    pub owner_token: Option<Token>,
    shutdown: watch::Sender<bool>,
    server: Option<JoinHandle<io::Result<()>>>,
    jobs: JoinHandle<()>,
    worker_loops: Vec<JoinHandle<()>>,
    writer: Writer,
    streams: Arc<Mutex<JoinSet<()>>>,
    #[cfg(test)]
    pub(crate) stream_hold: stream::Hold,
    #[cfg(test)]
    pub(crate) ruling_hold: stream::Hold,
}

impl Running {
    /// Waits until the listener stops on its own, which happens only when it fails. Does not
    /// stop anything; call [`stop`](Self::stop) afterwards to tear the rest down.
    ///
    /// # Errors
    ///
    /// Returns an error when the listener fails or the server task panics.
    pub async fn wait(&mut self) -> anyhow::Result<()> {
        // Polled by reference and cleared only once finished, so a cancelled wait (Ctrl+C in
        // the CLI) leaves the handle for `stop` to join.
        if let Some(server) = self.server.as_mut() {
            let finished = server.await;
            self.server = None;
            finished.context("the server task did not finish")??;
        }
        Ok(())
    }

    /// Stops the server and returns when no write can still happen: the listener stops taking
    /// connections, in-flight handlers finish against the writer, streams close, the job loop
    /// is aborted and joined, and the writer drains its queue and stops.
    ///
    /// # Errors
    ///
    /// Returns an error when the listener or the writer task failed. Teardown runs either way.
    ///
    /// # Panics
    ///
    /// Panics when a stream task panicked while holding the stream set lock.
    pub async fn stop(mut self) -> anyhow::Result<()> {
        self.shutdown.send_replace(true);
        // A local worker may be waiting on the origin for up to its wait limit, so its loop is
        // aborted rather than awaited. Its lease then ends on its own after the origin's TTL.
        for worker in std::mem::take(&mut self.worker_loops) {
            worker.abort();
            let _ = worker.await;
        }
        let served = self.wait().await;
        self.jobs.abort();
        let _ = (&mut self.jobs).await;
        let mut streams =
            std::mem::take(&mut *self.streams.lock().expect("stream set lock poisoned"));
        let drained = tokio::time::timeout(STREAM_DRAIN, async move {
            while streams.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tracing::warn!("a stream did not close during shutdown");
        }
        let written = self.writer.shutdown().await;
        served.and(written)
    }
}

/// Opens the repository's world, replays it, scans the code, and starts listening. Runs no local
/// workers; see [`start_with_workers`].
///
/// # Errors
///
/// Returns an error when the bind address is non-loopback without `team`, the state directory,
/// tokens or database cannot be opened, the stored log does not replay, the scan fails, the web
/// directory is not a dedicated public directory, the vision data directory cannot be created or
/// is inside the repository, or the address cannot be bound.
pub async fn start<C: ModelClient + 'static>(
    config: ServerConfig,
    client: C,
    ledger: UsageLedger,
) -> anyhow::Result<Running> {
    start_with_workers(config, client, ledger, 0).await
}

/// As [`start`], and runs `local_workers` worker loops in this process. Each loop has its own
/// agent, `local-1` and so on, registered to the owner. Its token is kept in the state directory,
/// so the same agent keeps its token across starts. Zero runs none.
///
/// # Errors
///
/// Returns the errors of [`start`], plus an error when an agent token cannot be written or a
/// local worker cannot be registered.
pub async fn start_with_workers<C: ModelClient + 'static>(
    config: ServerConfig,
    client: C,
    ledger: UsageLedger,
    local_workers: usize,
) -> anyhow::Result<Running> {
    if !config.addr.ip().is_loopback() && !config.team {
        bail!(
            "refusing to bind {} without --team, since that exposes this world to the network",
            config.addr.ip()
        );
    }
    if config.witness_concurrency == 0 {
        bail!("witness_concurrency must allow at least one witness run");
    }

    let web = config
        .web
        .as_deref()
        .map(|web| PublicRoot::new(web, &config.repo))
        .transpose()?;
    let state_dir = config.repo.join(STATE_DIR);
    prepare_state_dir(&state_dir)?;
    let vision = config
        .vision
        .as_ref()
        .map(|vision| Vision::open(vision, &config.repo))
        .transpose()?;
    let founder = MemberId::from_str(OWNER)?;
    let (mut tokens, owner_token) = Tokens::load_or_create(&state_dir, &founder)?;
    let mut worker_tokens = Vec::with_capacity(local_workers);
    for index in 0..local_workers {
        let token = tokens.agent_token(&state_dir, &workers::agent(index)?)?;
        worker_tokens.push(token.as_str().to_owned());
    }
    let store = EventStore::open(&state_dir.join(DB_FILE)).await?;

    let log = store.load_from(Seq::new(0)).await?;
    let origin = if log.is_empty() {
        let (origin, opened) = Origin::open(world_ref(&config.repo)?, founder.clone(), host::now());
        store.append(&[opened]).await?;
        origin
    } else {
        Origin::replay(log)?
    };

    let repo = config.repo.clone();
    let scan = tokio::task::spawn_blocking(move || scan(&repo))
        .await
        .context("the scan task did not finish")??;

    // Bind before the writer and the job loop start, so a failed bind leaves nothing running.
    let listener = TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("binding {}", config.addr))?;
    let local_addr = listener.local_addr()?;

    let (shutdown, shutdown_rx) = watch::channel(false);
    let git = open_git(&config).await;
    let host = Host::new(HostParts {
        origin,
        store,
        tokens,
        scan,
        ledger,
        client,
        repo: config.repo.clone(),
        shutdown: shutdown_rx,
        git,
        witness_http_base_url: config.witness_http_base_url.clone(),
        witness_concurrency: config.witness_concurrency,
        probe_concurrency: DEFAULT_PROBE_CONCURRENCY,
        vision,
    });
    if let Err(err) = host.submit(Command::System(SystemCmd::IndexUpdated)).await {
        let _ = host.shutdown().await;
        return Err(err).context("recording the initial code index");
    }
    for index in 0..local_workers {
        let registration = Command::Human {
            member: founder.clone(),
            cmd: HumanCmd::RegisterAgent {
                agent: workers::agent(index)?,
            },
        };
        if let Err(err) = host.submit(registration).await {
            let _ = host.shutdown().await;
            return Err(err).context("registering a local worker");
        }
    }

    let jobs = jobs::spawn(host.clone());
    host.wake_jobs();
    let board = workers::Board::new(local_workers);
    let worker_loops = workers::spawn(
        host.clone(),
        board.clone(),
        worker_tokens,
        config.repo.clone(),
        workers::origin_url(local_addr),
    );
    let writer = host.writer();
    let streams = Arc::clone(host.streams());
    #[cfg(test)]
    let stream_hold = host.stream_hold().clone();
    #[cfg(test)]
    let ruling_hold = host.ruling_hold().clone();
    let app = http::router(host, web).layer(Extension(board));
    let mut shutdown_seen = shutdown.subscribe();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_seen.wait_for(|off| *off).await;
            })
            .await
    });
    Ok(Running {
        local_addr,
        owner_token,
        shutdown,
        server: Some(server),
        jobs,
        worker_loops,
        writer,
        streams,
        #[cfg(test)]
        stream_hold,
        #[cfg(test)]
        ruling_hold,
    })
}

/// Opens the repository's git state for worktrees, witnesses and landing. A directory that is not
/// a usable repository still serves its world. The jobs that need git then report that they
/// cannot run, so the gate records the failure instead of hanging.
async fn open_git(config: &ServerConfig) -> Option<acan_git::Repo> {
    let opened = match &config.protected_branch {
        Some(branch) => acan_git::Repo::open_with_protected(&config.repo, branch).await,
        None => acan_git::Repo::open(&config.repo).await,
    };
    let repo = match opened {
        Ok(repo) => repo,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "no usable git state; worktrees, witnesses and landing are unavailable"
            );
            return None;
        }
    };
    match repo.protected_head().await {
        Ok(_) => Some(repo),
        Err(err) => {
            tracing::warn!(
                error = %err,
                branch = repo.protected_branch(),
                "the protected branch has no commit; git work is unavailable"
            );
            None
        }
    }
}

/// Creates the state directory. Its own `.gitignore` keeps the tokens and database out of git.
fn prepare_state_dir(dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        fs::write(&ignore, "*\n").with_context(|| format!("writing {}", ignore.display()))?;
    }
    Ok(())
}

fn world_ref(repo: &Path) -> anyhow::Result<WorldRef> {
    let name = repo
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("world");
    let id = WorldId::from_str(name).or_else(|_| WorldId::from_str("world"))?;
    Ok(WorldRef {
        id,
        repo: repo.display().to_string(),
    })
}
