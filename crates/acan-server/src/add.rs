//! Adds a project to a server that is already running. Startup opens every project before the
//! listener binds (see `start_listening` in lib.rs). A live add opens one project the same way,
//! registers it, and starts its first scan on the same feed, so no restart is needed. Its routes
//! are served by the fallback in `http.rs`, because the router was built at start without them.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use acan_core::{MemberId, Origin, Seq};
use acan_llm::{ModelClient, UsageLedger};
use anyhow::Context as _;
use tokio::sync::{Semaphore, watch};

use crate::auth::Tokens;
use crate::host::{self, Host, HostParts};
use crate::http::{ApiError, FieldError, Mount, project_routes};
use crate::projects::{ProjectSpec, spec_for};
use crate::public::PublicRoot;
use crate::registry::{Entry, Registry};
use crate::settings::data::DataOwner;
use crate::store::EventStore;
use crate::vision::{Vision, VisionConfig};
use crate::{DB_FILE, DEFAULT_PROBE_CONCURRENCY, Runtime, STATE_DIR};
use crate::{index, jobs, map_project, open_git, prepare_state_dir, settings, statemap, workers};

/// Opens the model client and usage ledger of a project added while the server runs. The CLI
/// builds one from the project's routing file, as startup does for each project.
pub type ClientFactory<C> =
    Box<dyn Fn(&ProjectSpec) -> anyhow::Result<(C, UsageLedger)> + Send + Sync>;

/// What a live add needs from the server that started. Built by startup, so a project opened later
/// gets the same witness pool, shutdown signal, owner and settings file as the others.
pub(crate) struct LiveContext<C> {
    /// The member whose token is the owner's.
    pub(crate) founder: MemberId,
    /// The address the server listens on, which a project's workers call back into.
    pub(crate) local_addr: SocketAddr,
    /// Signalled when the server shuts down.
    pub(crate) shutdown: watch::Receiver<bool>,
    /// The witness pool every project shares.
    pub(crate) slots: Arc<Semaphore>,
    /// How many witness runs may run at once, as startup was given.
    pub(crate) witness_concurrency: usize,
    /// How many local workers a project runs unless the settings give it another count.
    pub(crate) local_workers: usize,
    /// The advisory vision witness, shared by every project. `None` runs no vision checks.
    pub(crate) vision: Option<VisionConfig>,
    /// The settings file, which each project's scans read their statemap settings from.
    pub(crate) settings_file: PathBuf,
    /// The public folder, if one is served. A project's folder must sit outside it.
    pub(crate) web: Option<PathBuf>,
    /// Opens the model client of a project added while the server runs.
    pub(crate) open_client: ClientFactory<C>,
    /// The runtimes of projects added while the server runs. Shutdown stops them.
    pub(crate) runtimes: Arc<Mutex<Vec<Runtime>>>,
}

/// Checks that `folder` can be served beside the running projects, and returns its spec. The folder
/// must be an absolute path to an existing folder. It must not be a folder a running project serves,
/// nor sit inside, or contain, another project's state folder, nor sit inside the public folder.
///
/// # Errors
///
/// Returns the status the request should get: `400` for an empty path, `409` when the folder is
/// already served, and `422` with the rule it breaks for any other path.
pub(crate) fn check_new<C: ModelClient + 'static>(
    projects: &Registry<C>,
    folder: &str,
    web: Option<&Path>,
) -> Result<ProjectSpec, ApiError> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Err(ApiError::BadRequest("a folder path"));
    }
    let invalid = |why: &str| {
        ApiError::Invalid(vec![FieldError {
            path: "path".to_owned(),
            why: why.to_owned(),
        }])
    };
    let repo = PathBuf::from(folder);
    if !repo.is_absolute() {
        return Err(invalid("must be an absolute folder path"));
    }
    let canonical = match repo.canonicalize() {
        Ok(canonical) if canonical.is_dir() => canonical,
        _ => return Err(invalid("must be an existing folder")),
    };
    let entries = projects.entries();
    for entry in &entries {
        let theirs = entry.host.repo();
        let theirs = theirs
            .canonicalize()
            .unwrap_or_else(|_| theirs.to_path_buf());
        if theirs == canonical {
            return Err(ApiError::Conflict("already_served"));
        }
        if canonical.starts_with(theirs.join(STATE_DIR))
            || theirs.starts_with(canonical.join(STATE_DIR))
        {
            return Err(invalid("sits inside another project's state folder"));
        }
    }
    if let Some(dir) = web
        && PublicRoot::new(dir, &canonical).is_err()
    {
        return Err(invalid("must not be inside the public folder"));
    }
    let mut taken: BTreeSet<String> = entries.iter().map(|entry| entry.id.clone()).collect();
    Ok(spec_for(&repo, &mut taken))
}

/// Opens `spec` for the running server, registers it, and starts its first scan. The scan reports on
/// the project's live feed, and the project takes writes once the scan is done. Returns the new
/// project's id and name.
///
/// # Errors
///
/// Returns an error when the project's folder, tokens, event log or model client cannot be opened,
/// or when its state folder cannot be written. A project that fails here is not registered.
pub(crate) async fn open<C: ModelClient + 'static>(
    live: &LiveContext<C>,
    registry: &Registry<C>,
    spec: ProjectSpec,
) -> anyhow::Result<(String, String)> {
    let state_dir = spec.repo.join(STATE_DIR);
    prepare_state_dir(&state_dir)?;
    // The owner's token is the one every project holds. The new project's file takes it from the
    // projects already running, so one browser token opens every project.
    let mut dirs: Vec<PathBuf> = registry
        .entries()
        .iter()
        .map(|entry| entry.host.repo().join(STATE_DIR))
        .collect();
    dirs.push(state_dir.clone());
    let refs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
    let (mut all, _) = Tokens::load_shared(&refs, &live.founder)?;
    let mut tokens = all.pop().context("the new project's tokens")?;

    let stored = settings::load(&live.settings_file)?;
    let workers_here = settings::worker_count(&stored, &spec.id, live.local_workers);
    let mut worker_tokens = Vec::with_capacity(workers_here);
    for index in 0..workers_here {
        let token = tokens.agent_token(&state_dir, &workers::agent(index)?)?;
        worker_tokens.push(token.as_str().to_owned());
    }

    let store = EventStore::open(&state_dir.join(DB_FILE)).await?;
    let log = store.load_from(Seq::new(0)).await?;
    let origin = if log.is_empty() {
        let (origin, opened) = Origin::open(
            crate::world_ref(&spec.repo)?,
            live.founder.clone(),
            host::now(),
        );
        store.append(&[opened]).await?;
        origin
    } else {
        Origin::replay(log)?
    };
    let repo = spec.repo.clone();
    let scan = tokio::task::spawn_blocking(move || index::skeleton(&repo))
        .await
        .context("the skeleton scan task did not finish")??;
    let statemap = statemap::SettingsSource::new(live.settings_file.clone(), spec.id.clone());
    let vision = live
        .vision
        .as_ref()
        .map(|config| Vision::open(config, &spec.repo))
        .transpose()?;
    let (client, ledger) = (live.open_client)(&spec)?;
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
            shutdown: live.shutdown.clone(),
            git,
            witness_http_base_url: spec.witness_http_base_url.clone(),
            witness_concurrency: live.witness_concurrency,
            probe_concurrency: DEFAULT_PROBE_CONCURRENCY,
            vision,
        },
        Arc::clone(&live.slots),
        Some(statemap),
    );

    let board = workers::Board::new(worker_tokens.len());
    let api = format!("/api/p/{}", spec.id);
    let mcp = format!("{api}/mcp");
    let owner = DataOwner {
        project: spec.id.clone(),
        founder: live.founder.clone(),
    };
    let router = project_routes(
        Mount {
            api: &api,
            mcp: &mcp,
        },
        host.clone(),
        board.clone(),
        owner,
    );
    let mapping = tokio::spawn(map_project(
        host.clone(),
        live.founder.clone(),
        worker_tokens.len(),
        false,
    ));
    let jobs = jobs::spawn(host.clone());
    host.wake_jobs();
    let worker_loops = workers::spawn(
        host.clone(),
        board.clone(),
        worker_tokens,
        spec.repo.clone(),
        format!("{}/api/p/{}", workers::origin_url(live.local_addr), spec.id),
    );
    live.runtimes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Runtime {
            id: spec.id.clone(),
            jobs,
            worker_loops,
            writer: host.writer(),
            streams: Arc::clone(host.streams()),
            mapping: Some(mapping),
        });
    let (id, name) = (spec.id.clone(), spec.name.clone());
    registry.add(Entry {
        id: spec.id,
        name: spec.name,
        host,
        board,
        head: tokio::sync::Mutex::new(None),
        router: Some(router),
    });
    Ok((id, name))
}
