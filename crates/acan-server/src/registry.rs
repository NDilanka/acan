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
        status: if entry.host.writer_stopped() {
            "degraded"
        } else {
            "ready"
        },
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

#[cfg(test)]
mod tests {
    //! A project whose writer stops reports `degraded` and refuses requests with 503, and the
    //! other projects keep serving. The writer is stopped here, in the crate, because an
    //! integration test cannot stop one project's writer.

    use std::fs;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};

    use acan_llm::{Stub, UsageLedger};
    use reqwest::StatusCode;
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

    use crate::projects::ProjectSet;
    use crate::{DEFAULT_WITNESS_CONCURRENCY, Running, ServeConfig, start_projects};

    fn repo(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("model")).expect("the machine folder");
        fs::write(
            dir.join("model/store.ts"),
            "export function load() {\n  return 1;\n}\n",
        )
        .expect("a source file");
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
        };
        let clients = repos
            .iter()
            .map(|_| (Stub::default(), UsageLedger::default()))
            .collect();
        start_projects(config, clients)
            .await
            .expect("the server starts")
    }

    async fn get(running: &Running, path: &str, token: &str) -> (StatusCode, Value) {
        let response = reqwest::Client::new()
            .get(format!("http://{}{path}", running.local_addr))
            .bearer_auth(token)
            .send()
            .await
            .expect("the request is sent");
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    fn status_of<'a>(list: &'a Value, id: &str) -> &'a str {
        let project = list
            .as_array()
            .expect("a list of projects")
            .iter()
            .find(|project| project["id"] == json!(id))
            .expect("the project is listed");
        project["status"].as_str().expect("a status")
    }

    #[tokio::test]
    async fn a_project_whose_writer_stops_is_degraded_and_the_others_keep_serving() {
        let root = tempfile::tempdir().expect("temp dir");
        let running = start(&[repo(root.path(), "alpha"), repo(root.path(), "beta")]).await;
        let token = running
            .owner_token
            .as_ref()
            .expect("the first start mints the human token")
            .as_str()
            .to_owned();

        running
            .writer_of("beta")
            .shutdown()
            .await
            .expect("beta's writer stops");

        let (status, body) = get(&running, "/api/p/beta/plant", &token).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], json!("project_unavailable"));
        let (status, _) = get(&running, "/api/p/beta/events", &token).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "every route of the stopped project refuses"
        );

        let (status, list) = get(&running, "/api/projects", &token).await;
        assert_eq!(status, StatusCode::OK, "the list still answers");
        assert_eq!(status_of(&list, "beta"), "degraded");
        assert_eq!(status_of(&list, "alpha"), "ready");

        let (status, _) = get(&running, "/api/plant", &token).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the default project, alpha, still answers"
        );

        let posted = reqwest::Client::new()
            .post(format!(
                "http://{}/api/p/alpha/commands",
                running.local_addr
            ))
            .bearer_auth(&token)
            .json(&json!({
                "cmd": "post_order",
                "machine": "model",
                "sentence": "alpha work",
                "witnesses": []
            }))
            .send()
            .await
            .expect("the command is sent");
        assert_eq!(posted.status(), StatusCode::OK, "alpha takes commands");

        let mut request = format!("ws://{}/api/p/alpha/stream", running.local_addr)
            .into_client_request()
            .expect("the stream URL");
        let value = HeaderValue::from_str(&format!("Bearer {token}")).expect("a header value");
        request.headers_mut().insert(AUTHORIZATION, value);
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("alpha's stream opens");
        drop(socket);

        running.stop().await.expect("the server stops");
    }
}
