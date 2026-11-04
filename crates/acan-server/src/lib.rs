//! The acan server: one or more worlds' origins, each with its event log, and the HTTP and
//! WebSocket surface around them.
//!
//! [`start`] opens one repository's world in `.acan/world.db`, scans the code, and serves the API.
//! [`start_projects`] does the same for several projects behind one listener. [`start_open`] serves
//! the user's own folder, and may start with no project at all until one is added. Each project has
//! its own host: one writer task owns its origin and its current scan; HTTP handlers and the job
//! loop send it commands and await a reply, and readers use the snapshot it publishes after each
//! persisted batch. [`Running::stop`] is a barrier: it returns only when no write can still happen
//! in any project.

mod add;
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
pub mod open;
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
mod scan_feed;
mod settings;
mod setup;
mod signin;
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
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use acan_core::{Command, HumanCmd, MemberId, Origin, Seq, WorldId, WorldRef};
use acan_llm::{KeyFile, ModelClient, UsageLedger};
use anyhow::{Context, bail, ensure};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};

pub use add::ClientFactory;
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

use crate::add::LiveContext;
use crate::projects::{ProjectSet, ProjectSpec};
use crate::public::PublicRoot;
use crate::registry::{Entry, HomeTokens, Registry};
use crate::settings::Settings;
use crate::signin::SignIns;
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
    /// Whether each project's codebase maps when its first page connects, rather than as the server
    /// starts. A waiting project's map stays blank and takes no writes until then. Such a server
    /// must be started with [`start_projects_serving`], since waiting for the maps would wait for a
    /// page that may never come.
    pub map_on_first_view: bool,
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
            map_on_first_view: false,
        }
    }
}

/// The user's own folder, which a server opened with [`start_open`] holds. It is not any project's
/// folder, so the server can run with no codebase.
#[derive(Debug, Clone)]
pub struct UserFolder<C> {
    /// The user's folder. It holds `settings.json` and `tokens.json`, with the model keys beside
    /// them. Its tokens file holds the owner's token, which every project then shares.
    pub dir: PathBuf,
    /// The model client that holds the user's keys. The settings read and change the keys through
    /// it while no project runs.
    pub keys: C,
}

/// A project's mapping state as its watch channel holds it: `None` while the codebase maps, then
/// `Some(Ok)` once it is open or `Some(Err)` when mapping failed.
type MappingState = watch::Receiver<Option<Result<(), String>>>;

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
    /// Each project's codebase mapping task. Stop aborts them first.
    mapping: Vec<JoinHandle<()>>,
    /// Each project's mapping state, by project id, for [`Running::wait_mapped`].
    mapped: Vec<(String, MappingState)>,
    /// The runtimes of projects added while the server runs. Stop tears them down after the rest.
    live_runtimes: Arc<Mutex<Vec<Runtime>>>,
    /// The multiplexed streams, which span projects, so shutdown waits for them first.
    streams: Arc<Mutex<JoinSet<()>>>,
    /// The sign-in codes the server's routes redeem. Shared with the registry.
    signins: Arc<SignIns>,
    /// The default project's stream and ruling holds, which the tests park to hold a write.
    #[cfg(test)]
    pub(crate) stream_hold: stream::Hold,
    #[cfg(test)]
    pub(crate) ruling_hold: stream::Hold,
}

/// One project's background work: its job loop, its writer, and the streams open on it.
#[derive(Debug)]
pub(crate) struct Runtime {
    id: String,
    jobs: JoinHandle<()>,
    worker_loops: Vec<JoinHandle<()>>,
    writer: Writer,
    streams: Arc<Mutex<JoinSet<()>>>,
    /// The codebase mapping task of a project added while the server runs. Startup keeps its
    /// mapping tasks in [`Running`] instead.
    mapping: Option<JoinHandle<()>>,
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

    /// Waits until every project's codebase has mapped, so each takes writes. An error names the
    /// project whose codebase did not map.
    ///
    /// # Errors
    ///
    /// Returns an error when a project's codebase failed to map, or its mapping stopped first.
    pub async fn wait_mapped(&self) -> anyhow::Result<()> {
        for (id, state) in &self.mapped {
            let mut state = state.clone();
            let settled = state
                .wait_for(Option::is_some)
                .await
                .map(|settled| (*settled).clone());
            match settled {
                Ok(Some(Err(reason))) => bail!("project {id}: {reason}"),
                Ok(_) => {}
                Err(_) => bail!("project {id}: its codebase did not map"),
            }
        }
        Ok(())
    }

    /// A new sign-in code, which `POST /api/signin/exchange` trades once for the owner's token. It
    /// is valid for one minute and works once. Only a server started with a user folder, through
    /// [`start_open`], answers the exchange; any other server refuses every code.
    #[must_use]
    pub fn sign_in_code(&self) -> String {
        self.signins.mint(Instant::now())
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
        for mapping in std::mem::take(&mut self.mapping) {
            mapping.abort();
            let _ = mapping.await;
        }
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
        let live = std::mem::take(
            &mut *self
                .live_runtimes
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for mut runtime in live {
            if let Some(mapping) = runtime.mapping.take() {
                mapping.abort();
                let _ = mapping.await;
            }
            for worker in std::mem::take(&mut runtime.worker_loops) {
                worker.abort();
                let _ = worker.await;
            }
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
    ensure!(
        !config.map_on_first_view,
        "a server that maps on first view waits for a page, so it must start with start_projects_serving"
    );
    let running = start_projects_serving(config, clients, settings_file).await?;
    if let Err(err) = running.wait_mapped().await {
        let _ = running.stop().await;
        return Err(err);
    }
    Ok(running)
}

/// As [`start_projects_with_settings`], but returns once the server listens, even while the
/// codebases map. A project takes no writes until its codebase maps, and its scan reports on the
/// live feed meanwhile. A caller that needs the maps waits with [`Running::wait_mapped`].
///
/// # Errors
///
/// Returns the errors [`start_projects_with_settings`] returns, except that a codebase that fails
/// to map does not fail the start.
pub async fn start_projects_serving<C: ModelClient + 'static>(
    config: ServeConfig,
    clients: Vec<(C, UsageLedger)>,
    settings_file: Option<PathBuf>,
) -> anyhow::Result<Running> {
    start_listening(config, clients, settings_file, None, None).await
}

/// As [`start_projects_serving`], and a project added while the server runs is opened with
/// `opener`. The settings' add endpoint then serves a new folder at once, with no restart, and
/// starts its first scan. Without an opener, such adds are refused.
///
/// # Errors
///
/// Returns the errors [`start_projects_serving`] returns.
pub async fn start_projects_adding<C: ModelClient + 'static>(
    config: ServeConfig,
    clients: Vec<(C, UsageLedger)>,
    settings_file: Option<PathBuf>,
    opener: ClientFactory<C>,
) -> anyhow::Result<Running> {
    start_listening(config, clients, settings_file, Some(opener), None).await
}

/// As [`start_projects_adding`], for a server that belongs to the user's `folder` rather than to a
/// project. `config.projects` may be empty: the server then answers with no codebase, and the first
/// folder added becomes the default project. The settings are kept in `settings.json` in the
/// folder, and its tokens file holds the owner's token, which every project shares. Sign-in codes
/// work only on a server started this way.
///
/// # Errors
///
/// Returns the errors [`start_projects_serving`] returns, plus an error when the user's folder
/// cannot be created or overlaps the public folder.
pub async fn start_open<C: ModelClient + 'static>(
    config: ServeConfig,
    clients: Vec<(C, UsageLedger)>,
    folder: UserFolder<C>,
    opener: ClientFactory<C>,
) -> anyhow::Result<Running> {
    let settings_file = folder.dir.join(settings::FILE);
    start_listening(
        config,
        clients,
        Some(settings_file),
        Some(opener),
        Some(folder),
    )
    .await
}

async fn start_listening<C: ModelClient + 'static>(
    config: ServeConfig,
    clients: Vec<(C, UsageLedger)>,
    settings_file: Option<PathBuf>,
    opener: Option<ClientFactory<C>>,
    folder: Option<UserFolder<C>>,
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
    ensure!(
        folder.is_some() || !specs.is_empty(),
        "there is nothing to serve: give at least one project, or start with the user's folder"
    );
    if let Some(folder) = &folder {
        prepare_state_dir(&folder.dir)?;
    }

    // Every repository, and the user's folder, is checked against the public root, so no private
    // file can be served.
    let web = match config.web.as_deref() {
        Some(dir) => {
            let root = PublicRoot::open(dir)?;
            for spec in &specs {
                PublicRoot::new(dir, &spec.repo)?;
            }
            if let Some(folder) = &folder {
                root.keep_out(&folder.dir)?;
            }
            Some(root)
        }
        None => None,
    };
    let states: Vec<PathBuf> = specs.iter().map(|spec| spec.repo.join(STATE_DIR)).collect();
    for state in &states {
        prepare_state_dir(state)?;
    }
    let settings_file = match (settings_file, &folder, states.first()) {
        (Some(file), _, _) => file,
        (None, Some(folder), _) => folder.dir.join(settings::FILE),
        (None, None, Some(state)) => state.join(settings::FILE),
        (None, None, None) => bail!("there is no folder to keep the settings in"),
    };
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
    // The user's folder is listed first, so its owner token is the one every project takes.
    let mut dirs: Vec<&Path> = folder.iter().map(|folder| folder.dir.as_path()).collect();
    dirs.extend(states.iter().map(PathBuf::as_path));
    let (mut tokens, owner_token) = Tokens::load_shared(&dirs, &founder)?;
    let (home, own_keys) = match folder {
        Some(UserFolder { dir, keys }) => {
            // One table per directory, in order, so the first is the user's folder's.
            let table = tokens.remove(0);
            let home = HomeTokens {
                tokens: table,
                dir,
                owner: founder.clone(),
            };
            (Some(home), Some(Arc::new(keys)))
        }
        None => (None, None),
    };
    let home_dir = home.as_ref().map(|home| home.dir.clone());

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
        // Only the skeleton is read before the listener binds. The codebase maps after that, with
        // its progress on the live feed.
        let scan = tokio::task::spawn_blocking(move || index::skeleton(&repo))
            .await
            .context("the skeleton scan task did not finish")??;
        // Read from the settings file at each scan. A flag that is off makes no model call and
        // writes no cache.
        let statemap = statemap::SettingsSource::new(settings_file.clone(), spec.id.clone());
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
        let host = Host::new_mapping(
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
        if config.map_on_first_view {
            host.hold_until_viewed();
        }
        entries.push(Entry {
            id: spec.id,
            name: spec.name,
            host,
            board: workers::Board::new(worker_tokens.len()),
            head: tokio::sync::Mutex::new(None),
            router: None,
            plain: None,
        });
        project_workers.push(worker_tokens);
    }
    // Each project maps its codebase in the background. It records the index and registers its
    // local workers, then opens to writes. Its job and worker loops wait for that.
    let mut mapping = Vec::with_capacity(entries.len());
    let mut mapped = Vec::with_capacity(entries.len());
    for (entry, worker_tokens) in entries.iter().zip(&project_workers) {
        mapped.push((entry.id.clone(), entry.host.mapping_state()));
        mapping.push(tokio::spawn(map_project(
            entry.host.clone(),
            founder.clone(),
            worker_tokens.len(),
            config.map_on_first_view,
        )));
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
            mapping: None,
        });
    }
    #[cfg(test)]
    let stream_hold = entries.first().map_or_else(stream::Hold::default, |entry| {
        entry.host.stream_hold().clone()
    });
    #[cfg(test)]
    let ruling_hold = entries.first().map_or_else(stream::Hold::default, |entry| {
        entry.host.ruling_hold().clone()
    });
    let streams = Arc::new(Mutex::new(JoinSet::new()));
    let live_runtimes = Arc::new(Mutex::new(Vec::new()));
    let live = opener.map(|open_client| LiveContext {
        founder: founder.clone(),
        local_addr,
        shutdown: shutdown_rx.clone(),
        slots: Arc::clone(&slots),
        witness_concurrency: config.witness_concurrency,
        local_workers: config.local_workers,
        vision: config.vision.clone(),
        settings_file: settings_file.clone(),
        web: config.web.clone(),
        open_client,
        runtimes: Arc::clone(&live_runtimes),
        home: home_dir,
    });
    let registry = Arc::new(Registry::new(
        config.projects.default.clone(),
        entries,
        Arc::clone(&streams),
        live,
        shutdown_rx.clone(),
        home,
    ));
    let signins = Arc::clone(registry.signins());
    let shared_settings = Arc::new(Settings::new(
        settings_file,
        stored,
        Arc::clone(&registry),
        founder.clone(),
        own_keys,
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
        mapping,
        mapped,
        live_runtimes,
        streams,
        signins,
        #[cfg(test)]
        stream_hold,
        #[cfg(test)]
        ruling_hold,
    })
}

/// Maps one project's codebase in the background: scans it, commits its index, registers its local
/// workers, and opens it to writes. A failure is recorded on the project, which then takes no
/// writes, and the server keeps running so the page can show why. With `on_view`, the scan starts
/// only once a page has connected to the project.
pub(crate) async fn map_project<C: ModelClient + 'static>(
    host: Host<C>,
    founder: MemberId,
    workers: usize,
    on_view: bool,
) {
    if on_view {
        host.wait_viewed().await;
    }
    let mapped = async {
        host.scan_and_commit().await?;
        for index in 0..workers {
            let registration = Command::Human {
                member: founder.clone(),
                cmd: HumanCmd::RegisterAgent {
                    agent: workers::agent(index)?,
                },
            };
            host.submit_unmapped(registration).await?;
        }
        anyhow::Ok(())
    }
    .await;
    match mapped {
        Ok(()) => host.open_writes(),
        Err(err) => {
            let reason = format!("{err:#}");
            tracing::error!(error = %reason, "mapping the codebase failed");
            host.mapping_failed(reason);
        }
    }
    host.wake_jobs();
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
pub(crate) async fn open_git(project: &ProjectSpec) -> Option<acan_git::Repo> {
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
pub(crate) fn prepare_state_dir(dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        fs::write(&ignore, "*\n").with_context(|| format!("writing {}", ignore.display()))?;
    }
    Ok(())
}

pub(crate) fn world_ref(repo: &Path) -> anyhow::Result<WorldRef> {
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
