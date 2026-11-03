//! The projects one server runs, and the list the UI reads to show them.
//!
//! The registry holds each project's host and nothing else. Which project a request reaches is
//! decided by its path, so a request can only use the host of the project it names.

use std::sync::{Arc, Mutex};

use acan_llm::ModelClient;
use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::host::Host;
use crate::http::{ApiError, bearer};
use crate::workers::Board;

/// Every project one server runs, and which of them answers the unprefixed routes.
pub(crate) struct Registry<C> {
    default_id: String,
    entries: Vec<Entry<C>>,
    /// The multiplexed streams, which span projects, so shutdown waits for them here.
    streams: Arc<Mutex<JoinSet<()>>>,
}

/// One project as the server runs it.
pub(crate) struct Entry<C> {
    /// The URL segment and the key every API and log line uses for this project.
    pub(crate) id: String,
    /// The name the UI shows.
    pub(crate) name: String,
    /// The project's host: its writer, its snapshot, its tokens and its git state.
    pub(crate) host: Host<C>,
    /// The state of this project's local workers, which `GET /api/p/{id}/workers` reports.
    pub(crate) board: Board,
    /// The protected branch head the list last read, with the seq of the snapshot it was read at.
    /// It is read again only when the snapshot moves, so the list does not run git per request.
    pub(crate) head: tokio::sync::Mutex<Option<(u64, Option<String>)>>,
}

impl<C> Registry<C> {
    /// A registry of `entries`, whose default is `default_id`. The entries must not be empty:
    /// the server shuts down through the first project's signal.
    pub(crate) fn new(
        default_id: String,
        entries: Vec<Entry<C>>,
        streams: Arc<Mutex<JoinSet<()>>>,
    ) -> Self {
        assert!(!entries.is_empty(), "a registry holds at least one project");
        Self {
            default_id,
            entries,
            streams,
        }
    }

    /// The id of the project the unprefixed routes map to.
    pub(crate) fn default_id(&self) -> &str {
        &self.default_id
    }

    /// Every project, in the order the server was given them.
    pub(crate) fn entries(&self) -> &[Entry<C>] {
        &self.entries
    }

    /// The set the multiplexed streams are tracked in.
    pub(crate) fn streams(&self) -> &Arc<Mutex<JoinSet<()>>> {
        &self.streams
    }
}

impl<C: ModelClient> Registry<C> {
    /// The server's shutdown signal, which every project's host shares.
    pub(crate) fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.entries[0].host.shutdown_signal()
    }
}

/// One project as `GET /api/projects` lists it. Repository paths are never part of it.
#[derive(Debug, Serialize)]
pub(crate) struct ProjectView {
    id: String,
    name: String,
    default: bool,
    status: &'static str,
    protected_branch: Option<String>,
    head: HeadView,
    machines: usize,
    workers: WorkersView,
    needs_you: NeedsYouView,
}

/// The project's last committed event, and the protected branch head at that point.
#[derive(Debug, Serialize)]
struct HeadView {
    seq: u64,
    commit: Option<String>,
}

/// The agents in the project's tokens file, and how many hold a live lease.
#[derive(Debug, Serialize)]
struct WorkersView {
    members: usize,
    active: usize,
}

/// What the human is asked to act on. See [`acan_core::NeedsYou`].
#[derive(Debug, Serialize)]
struct NeedsYouView {
    total: usize,
    asking: usize,
    clashing: usize,
    contests: usize,
    open_choices: usize,
}

/// The projects the caller's token is a member of. A member is a principal listed in that
/// project's tokens file, so a token sees only the projects it may use. A token that is a member
/// of none gets 401, the same answer an unknown token gets from any route.
pub(crate) async fn list_projects<C: ModelClient + 'static>(
    State(projects): State<Arc<Registry<C>>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProjectView>>, ApiError> {
    let token = bearer(&headers).ok_or(ApiError::Unauthorized)?;
    let mut views = Vec::new();
    for entry in projects.entries() {
        if entry.host.tokens().principal(token).is_none() {
            continue;
        }
        views.push(view(entry, projects.default_id()).await);
    }
    if views.is_empty() {
        return Err(ApiError::Unauthorized);
    }
    Ok(Json(views))
}

/// One project's entry in the list, read from its last committed snapshot.
async fn view<C: ModelClient + 'static>(entry: &Entry<C>, default_id: &str) -> ProjectView {
    let now = crate::host::now();
    let (seq, workers, needs) = entry.host.read(|world| {
        let seq = world.next_seq().get().saturating_sub(1);
        let active = world
            .orders()
            .flat_map(|order| world.tasks_of(order.id))
            .filter_map(|task| match &task.state {
                acan_core::TaskState::Claimed { claim, .. } if claim.lease.expires_at > now => {
                    Some(claim.lease.holder.clone())
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        (seq, active, world.needs_you())
    });
    ProjectView {
        id: entry.id.clone(),
        name: entry.name.clone(),
        default: entry.id == default_id,
        status: "ready",
        protected_branch: entry
            .host
            .workspace()
            .map(|workspace| workspace.repo().protected_branch().to_owned()),
        head: HeadView {
            seq,
            commit: head_commit(entry, seq).await,
        },
        machines: entry.host.scan().plant.machines.len(),
        workers: WorkersView {
            members: entry.host.tokens().agent_count(),
            active: workers,
        },
        needs_you: NeedsYouView {
            total: needs.total(),
            asking: needs.asking,
            clashing: needs.clashing,
            contests: needs.contests,
            open_choices: needs.open_choices,
        },
    }
}

/// The protected branch head at snapshot `seq`. Read from git only when `seq` differs from the
/// seq it was last read at. `None` when the project has no usable git state.
async fn head_commit<C: ModelClient + 'static>(entry: &Entry<C>, seq: u64) -> Option<String> {
    let mut cached = entry.head.lock().await;
    if let Some((cached_seq, commit)) = cached.as_ref()
        && *cached_seq == seq
    {
        return commit.clone();
    }
    let commit = match entry.host.workspace() {
        Some(workspace) => workspace
            .repo()
            .protected_head()
            .await
            .ok()
            .map(|head| head.to_string()),
        None => None,
    };
    *cached = Some((seq, commit.clone()));
    commit
}
