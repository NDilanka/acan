//! The acan server: one or more worlds' origins, each with its event log, and the HTTP and
//! WebSocket surface around them.
//!
//! [`start`] opens one repository's world in `.acan/world.db`, scans the code, and serves the API.
//! [`start_projects`] does the same for several projects behind one listener. Each project has
//! its own host: one writer task owns its origin and its current scan; HTTP handlers and the job
//! loop send it commands and await a reply, and readers use the snapshot it publishes after each
//! persisted batch. [`Running::stop`] is a barrier: it returns only when no write can still happen
//! in any project.

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
pub mod projects;
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
mod registry;
pub mod release;
pub mod replay;
mod ruling;
mod settings;
mod setup;
mod statemap;
mod store;
mod stream;
mod stream_all;
mod touches;
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
use acan_llm::{KeyFile, ModelClient, UsageLedger};
use anyhow::{Context, bail, ensure};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
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

use crate::projects::{ProjectSet, ProjectSpec};
use crate::public::PublicRoot;
use crate::registry::{Entry, Registry};
use crate::settings::Settings;
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

/// What to serve, and where: the projects, and the settings every project shares.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// The projects to serve. The default one also answers the unprefixed routes.
    pub projects: ProjectSet,
    /// The address to listen on. Port 0 picks a free port.
    pub addr: SocketAddr,
    /// Allows a non-loopback address. Without it, binding elsewhere is refused.
    pub team: bool,
    /// A dedicated public directory served for every path the API does not claim. It must not
    /// contain any project's repository or state directory.
    pub web: Option<PathBuf>,
    /// How many witness runs may run at once, across every project. Must be at least 1.
    pub witness_concurrency: usize,
    /// The advisory vision witness, shared by every project. `None` runs no vision checks.
    pub vision: Option<VisionConfig>,
    /// How many local worker loops each project runs in this process, unless the settings give
    /// that project another count. Zero runs none. Each loop has its own agent, `local-1` and so
    /// on, in every project.
    pub local_workers: usize,
}

impl From<ServerConfig> for ServeConfig {
    fn from(config: ServerConfig) -> Self {
        let mut projects = ProjectSet::from_repos(std::slice::from_ref(&config.repo));
        if let Some(project) = projects.projects.first_mut() {
            project.protected_branch = config.protected_branch;
            project.witness_http_base_url = config.witness_http_base_url;
        }
        Self {
            projects,
            addr: config.addr,
            team: config.team,
            web: config.web,
            witness_concurrency: config.witness_concurrency,
            vision: config.vision,
            local_workers: 0,
        }
    }
}

/// A server that is listening. Dropping it does not stop the server.
#[derive(Debug)]
pub struct Running {
    /// The address the server bound, with the real port.
    pub local_addr: SocketAddr,
    /// The human token every project accepts. Present only on the first start, when it was
    /// minted. The same token is in every project's tokens file.
    pub owner_token: Option<Token>,
    shutdown: watch::Sender<bool>,
    server: Option<JoinHandle<io::Result<()>>>,
    runtimes: Vec<Runtime>,
    /// The multiplexed streams, which span projects, so shutdown waits for them first.
    streams: Arc<Mutex<JoinSet<()>>>,
    /// The default project's stream and ruling holds, which the tests park to hold a write.
    #[cfg(test)]
    pub(crate) stream_hold: stream::Hold,
    #[cfg(test)]
    pub(crate) ruling_hold: stream::Hold,
}

/// One project's background work: its job loop, its writer, and the streams open on it.
#[derive(Debug)]
struct Runtime {
    id: String,
    jobs: JoinHandle<()>,
    worker_loops: Vec<JoinHandle<()>>,
    writer: Writer,
    streams: Arc<Mutex<JoinSet<()>>>,
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

    /// The ids of the projects this server serves, in the order they were given.
    #[must_use]
    pub fn project_ids(&self) -> Vec<&str> {
        self.runtimes
            .iter()
            .map(|runtime| runtime.id.as_str())
            .collect()
    }

    /// The writer of the first project, which the tests drive directly.
    #[cfg(test)]
    pub(crate) fn writer(&self) -> &Writer {
        &self.runtimes[0].writer
    }

    /// The writer of the project `id`, which a test stops to make that project degraded.
    #[cfg(test)]
    pub(crate) fn writer_of(&self, id: &str) -> &Writer {
        &self
            .runtimes
            .iter()
            .find(|runtime| runtime.id == id)
            .expect("the project runs")
            .writer
    }

    /// Stops the server and returns when no write can still happen: the listener stops taking
    /// connections, in-flight handlers finish against the writers, streams close, the job loops
    /// are aborted and joined, and each writer drains its queue and stops.
    ///
    /// # Errors
    ///
    /// Returns an error when the listener or a writer task failed. Teardown runs either way.
    ///
    /// # Panics
    ///
    /// Panics when a stream task panicked while holding the stream set lock.
    pub async fn stop(mut self) -> anyhow::Result<()> {
        self.shutdown.send_replace(true);
        // A local worker may be waiting on the origin for up to its wait limit, so its loop is
        // aborted rather than awaited. Its lease then ends on its own after the origin's TTL.
        for runtime in &mut self.runtimes {
            for worker in std::mem::take(&mut runtime.worker_loops) {
                worker.abort();
                let _ = worker.await;
            }
        }
        let served = self.wait().await;
        drain_streams(&self.streams, "every project").await;
        let mut written: anyhow::Result<()> = Ok(());
        for runtime in std::mem::take(&mut self.runtimes) {
            runtime.jobs.abort();
            let _ = runtime.jobs.await;
            drain_streams(&runtime.streams, &runtime.id).await;
            written = written.and(runtime.writer.shutdown().await);
        }
        served.and(written)
    }
}

/// Waits, for at most [`STREAM_DRAIN`], for the streams in `streams` to close. The streams were
/// already told to close, so the wait is only for their last writes to the socket.
async fn drain_streams(streams: &Mutex<JoinSet<()>>, project: &str) {
    let mut set = std::mem::take(&mut *streams.lock().expect("stream set lock poisoned"));
    let drained = tokio::time::timeout(STREAM_DRAIN, async move {
        while set.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!(project, "a stream did not close during shutdown");
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
    let mut config = ServeConfig::from(config);
    config.local_workers = local_workers;
    start_projects(config, vec![(client, ledger)]).await
}

/// Opens every project's world, replays it, scans its code, and starts one listener that serves
/// them all. `clients` holds one model client and usage ledger per project, in the order of
/// `config.projects.projects`. Each project runs `config.local_workers` worker loops.
///
/// Every project opens before anything binds, so a project that fails to open leaves nothing
/// running. The witness pool is one pool for all projects. The settings are kept as in
/// [`start_projects_with_settings`] with no file given.
///
/// # Errors
///
/// Returns the errors of [`start_projects_with_settings`].
pub async fn start_projects<C: ModelClient + 'static>(
    config: ServeConfig,
    clients: Vec<(C, UsageLedger)>,
) -> anyhow::Result<Running> {
    start_projects_with_settings(config, clients, None).await
}

/// As [`start_projects`], and keeps the settings in `settings_file`. `None` keeps them in
/// `settings.json` in the default project's state directory. A missing settings file reads as the
/// defaults. A file that does not parse stops the start, so the server never replaces it with the
/// defaults.
///
/// Each project runs the local worker count its settings give, or `config.local_workers` when they
/// give none. The folders settings added and removed are the caller's to apply, with
/// [`apply_project_changes`], before it builds the clients.
///
/// # Errors
///
/// Returns an error when the bind address is non-loopback without `team`, the projects do not
/// validate, a project's state directory, tokens or database cannot be opened, a stored log does
/// not replay, a scan fails, the web directory is not a dedicated public directory for every
/// repository, the settings file cannot be read or does not parse, an agent token cannot be
/// written, a local worker cannot be registered, or the address cannot be bound.
pub async fn start_projects_with_settings<C: ModelClient + 'static>(
    config: ServeConfig,
    clients: Vec<(C, UsageLedger)>,
    settings_file: Option<PathBuf>,
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
    config.projects.validate()?;
    let specs = config.projects.projects.clone();
    ensure!(
        clients.len() == specs.len(),
        "every project needs a model client: {} projects, {} clients",
        specs.len(),
        clients.len()
    );

    // Every repository is checked against the public root, so no project's files can be served.
    let mut web = None;
    for spec in &specs {
        let root = config
            .web
            .as_deref()
            .map(|dir| PublicRoot::new(dir, &spec.repo))
            .transpose()?;
        if web.is_none() {
            web = root;
        }
    }
    let states: Vec<PathBuf> = specs.iter().map(|spec| spec.repo.join(STATE_DIR)).collect();
    for state in &states {
        prepare_state_dir(state)?;
    }
    // The validated projects are never empty, so the first one's state directory exists.
    let settings_file = settings_file.unwrap_or_else(|| states[0].join(settings::FILE));
    let stored = settings::load(&settings_file)?;
    let mut visions = Vec::with_capacity(specs.len());
    for spec in &specs {
        visions.push(
            config
                .vision
                .as_ref()
                .map(|vision| Vision::open(vision, &spec.repo))
                .transpose()?,
        );
    }
    let founder = MemberId::from_str(OWNER)?;
    let dirs: Vec<&Path> = states.iter().map(PathBuf::as_path).collect();
    let (tokens, owner_token) = Tokens::load_shared(&dirs, &founder)?;

    let mut openings = Vec::with_capacity(specs.len());
    let inputs = specs.into_iter().zip(clients).zip(tokens).zip(visions);
    for (((spec, (client, ledger)), mut tokens), vision) in inputs {
        let state_dir = spec.repo.join(STATE_DIR);
        // Each project's local workers hold their own tokens, kept in that project's state
        // directory, so the same worker keeps its token across starts.
        let workers = settings::worker_count(&stored, &spec.id, config.local_workers);
        let mut worker_tokens = Vec::with_capacity(workers);
        for index in 0..workers {
            let token = tokens.agent_token(&state_dir, &workers::agent(index)?)?;
            worker_tokens.push(token.as_str().to_owned());
        }
        let store = EventStore::open(&state_dir.join(DB_FILE)).await?;
        let log = store.load_from(Seq::new(0)).await?;
        let origin = if log.is_empty() {
            let (origin, opened) =
                Origin::open(world_ref(&spec.repo)?, founder.clone(), host::now());
            store.append(&[opened]).await?;
            origin
        } else {
            Origin::replay(log)?
        };
        let repo = spec.repo.clone();
        let mut scan = tokio::task::spawn_blocking(move || scan(&repo))
            .await
            .context("the scan task did not finish")??;
        // Read from the settings file at each scan. A flag that is off makes no model call and
        // writes no cache.
        let statemap = statemap::SettingsSource::new(settings_file.clone(), spec.id.clone());
        statemap::refine(&client, &mut scan.plant, statemap.current()).await;
        openings.push(Opening {
            spec,
            store,
            origin,
            scan,
            tokens,
            vision,
            client,
            ledger,
            statemap,
            worker_tokens,
        });
    }

    // Bind before the writers and the job loops start, so a failed bind leaves nothing running.
    let listener = TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("binding {}", config.addr))?;
    let local_addr = listener.local_addr()?;

    let (shutdown, shutdown_rx) = watch::channel(false);
    // One witness pool for every project, so the server runs at most `witness_concurrency`
    // witness runs at once.
    let slots = Arc::new(Semaphore::new(config.witness_concurrency));
    let mut entries: Vec<Entry<C>> = Vec::with_capacity(openings.len());
    let mut project_workers: Vec<Vec<String>> = Vec::with_capacity(openings.len());
    for opening in openings {
        let Opening {
            spec,
            store,
            origin,
            scan,
            tokens,
            vision,
            client,
            ledger,
            statemap,
            worker_tokens,
        } = opening;
        let git = open_git(&spec).await;
        let host = Host::new_with_statemap(
            HostParts {
                origin,
                store,
                tokens,
                scan,
                ledger,
                client,
                repo: spec.repo.clone(),
                shutdown: shutdown_rx.clone(),
                git,
                witness_http_base_url: spec.witness_http_base_url.clone(),
                witness_concurrency: config.witness_concurrency,
                probe_concurrency: DEFAULT_PROBE_CONCURRENCY,
                vision,
            },
            Arc::clone(&slots),
            Some(statemap),
        );
        entries.push(Entry {
            id: spec.id,
            name: spec.name,
            host,
            board: workers::Board::new(worker_tokens.len()),
            head: tokio::sync::Mutex::new(None),
        });
        project_workers.push(worker_tokens);
    }
    for entry in &entries {
        if let Err(err) = entry
            .host
            .submit(Command::System(SystemCmd::IndexUpdated))
            .await
        {
            for entry in &entries {
                let _ = entry.host.shutdown().await;
            }
            return Err(err).context("recording the initial code index");
        }
    }
    for (entry, worker_tokens) in entries.iter().zip(&project_workers) {
        for index in 0..worker_tokens.len() {
            let registration = Command::Human {
                member: founder.clone(),
                cmd: HumanCmd::RegisterAgent {
                    agent: workers::agent(index)?,
                },
            };
            if let Err(err) = entry.host.submit(registration).await {
                for entry in &entries {
                    let _ = entry.host.shutdown().await;
                }
                return Err(err).context("registering a local worker");
            }
        }
    }

    // Each project's loops reach that project's routes, so their MCP calls take the project's
    // own agent tokens.
    let mut runtimes = Vec::with_capacity(entries.len());
    for (entry, worker_tokens) in entries.iter().zip(project_workers) {
        let jobs = jobs::spawn(entry.host.clone());
        entry.host.wake_jobs();
        let worker_loops = workers::spawn(
            entry.host.clone(),
            entry.board.clone(),
            worker_tokens,
            entry.host.repo().to_path_buf(),
            format!("{}/api/p/{}", workers::origin_url(local_addr), entry.id),
        );
        runtimes.push(Runtime {
            id: entry.id.clone(),
            jobs,
            worker_loops,
            writer: entry.host.writer(),
            streams: Arc::clone(entry.host.streams()),
        });
    }
    #[cfg(test)]
    let stream_hold = entries[0].host.stream_hold().clone();
    #[cfg(test)]
    let ruling_hold = entries[0].host.ruling_hold().clone();
    let streams = Arc::new(Mutex::new(JoinSet::new()));
    let registry = Arc::new(Registry::new(
        config.projects.default.clone(),
        entries,
        Arc::clone(&streams),
    ));
    let shared_settings = Arc::new(Settings::new(
        settings_file,
        stored,
        Arc::clone(&registry),
        founder.clone(),
    ));
    let app = http::router(registry, web, shared_settings, founder);
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
        runtimes,
        streams,
        #[cfg(test)]
        stream_hold,
        #[cfg(test)]
        ruling_hold,
    })
}

/// The settings file beside `keys`: `settings.json` in the folder the keys file is in. A user's
/// settings sit with that user's model keys, so every project the user runs shares them.
#[must_use]
pub fn settings_file_beside(keys: &KeyFile) -> PathBuf {
    keys.path().with_file_name(settings::FILE)
}

/// The settings file a start keeps when it is given none: `settings.json` in the first project's
/// state directory, the one the start opens first.
#[must_use]
pub fn default_settings_file(projects: &ProjectSet) -> PathBuf {
    projects
        .projects
        .first()
        .map_or_else(
            || PathBuf::from(STATE_DIR),
            |project| project.repo.join(STATE_DIR),
        )
        .join(settings::FILE)
}

/// Applies the project changes that `settings_file` holds to `projects`: the folders settings added
/// and removed. Call this before the clients are built, and start with the same `projects`. The
/// result names each added folder that cannot be served, with the reason. Such a folder is dropped
/// from the changes, so the next start does not name it again.
///
/// # Errors
///
/// Returns an error when the settings file cannot be read, does not parse, or cannot be written,
/// or when the set that results does not validate.
pub fn apply_project_changes(
    projects: &mut ProjectSet,
    settings_file: &Path,
) -> anyhow::Result<Vec<String>> {
    let mut stored = settings::load(settings_file)?;
    let add: Vec<PathBuf> = stored.projects.add.iter().map(PathBuf::from).collect();
    let remove: Vec<PathBuf> = stored.projects.remove.iter().map(PathBuf::from).collect();
    let skipped = projects.apply_changes(&add, &remove)?;
    if skipped.is_empty() {
        return Ok(Vec::new());
    }
    stored
        .projects
        .add
        .retain(|folder| !skipped.iter().any(|(repo, _)| repo == Path::new(folder)));
    settings::file::save(settings_file, &stored)
        .context("dropping the folders that cannot be served from the project changes")?;
    Ok(skipped
        .into_iter()
        .map(|(repo, reason)| format!("{}: {reason}", repo.display()))
        .collect())
}

/// A project whose world is open and scanned, waiting for its host. Nothing is bound yet.
struct Opening<C> {
    spec: ProjectSpec,
    store: EventStore,
    origin: Origin,
    scan: Scan,
    tokens: Tokens,
    vision: Option<Vision>,
    client: C,
    ledger: UsageLedger,
    /// Where the project's scans read their statemap settings.
    statemap: statemap::SettingsSource,
    /// The tokens of this project's local workers, in worker order.
    worker_tokens: Vec<String>,
}

/// Opens a project's git state for worktrees, witnesses and landing. A directory that is not a
/// usable repository still serves its world. The jobs that need git then report that they cannot
/// run, so the gate records the failure instead of hanging.
async fn open_git(project: &ProjectSpec) -> Option<acan_git::Repo> {
    let opened = match &project.protected_branch {
        Some(branch) => acan_git::Repo::open_with_protected(&project.repo, branch).await,
        None => acan_git::Repo::open(&project.repo).await,
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
