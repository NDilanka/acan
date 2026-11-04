//! The projects one server runs, and the list the UI reads to show them.
//!
//! The registry holds each project's host and nothing else. Which project a request reaches is
//! decided by its path, so a request can only use the host of the project it names.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use acan_core::MemberId;
use acan_llm::ModelClient;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::add::LiveContext;
use crate::auth::{Principal, Tokens};
use crate::host::Host;
use crate::http::{ApiError, bearer};
use crate::signin::SignIns;
use crate::workers::Board;

/// Every project one server runs, and which of them answers the unprefixed routes. A project added
/// while the server runs joins the list, so the list is read through [`Registry::entries`]. The list
/// may be empty: a server opened with no codebase runs none until one is added.
pub(crate) struct Registry<C> {
    /// The id the unprefixed routes map to. Empty while no project runs, and then the first project
    /// added becomes the default.
    default_id: RwLock<String>,
    /// The projects, in the order the server was given them and then the order they were added.
    entries: RwLock<Vec<Arc<Entry<C>>>>,
    /// The multiplexed streams, which span projects, so shutdown waits for them here.
    streams: Arc<Mutex<JoinSet<()>>>,
    /// What a project added while the server runs is opened with. `None` refuses such adds.
    live: Option<LiveContext<C>>,
    /// The server's shutdown signal, which every stream that is not a project's follows.
    shutdown: watch::Receiver<bool>,
    /// The user's own folder, when the server was opened with `acan open`. Its owner token signs in
    /// the owner even while no project runs.
    home: Option<HomeTokens>,
    /// The sign-in codes not yet used. The running server keeps a clone, so it can mint a code for
    /// the link it opens.
    signins: Arc<SignIns>,
}

/// The user's folder a server opened with `acan open` holds. Its tokens file holds the owner's
/// token, so the owner's sign-in and the owner's token rotation reach it with no project running.
pub(crate) struct HomeTokens {
    /// The owner's token table. Clones share one table.
    pub(crate) tokens: Tokens,
    /// The folder the tokens file is in. A rotation writes the file there.
    pub(crate) dir: PathBuf,
    /// The member whose token is the owner's.
    pub(crate) owner: MemberId,
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
    /// The routes of a project added while the server runs. Those paths are not in the router the
    /// server built at start, so requests for them reach these routes through the fallback. `None`
    /// for a project the server started with.
    pub(crate) router: Option<Router>,
    /// The unprefixed routes, `/api/...` and `/mcp`, of a project that became the default when it
    /// was added to a server running no project. The router built at start has no unprefixed
    /// routes then, so they are reached through the fallback too. `None` for every other project.
    pub(crate) plain: Option<Router>,
}

impl<C> Registry<C> {
    /// A registry of `entries`, whose default is `default_id`. The entries may be empty, and then
    /// the default is empty too. `live` says how a project added later is opened, and `None` refuses
    /// such adds. `shutdown` is the server's signal, and `home` is the user's folder if there is one.
    pub(crate) fn new(
        default_id: String,
        entries: Vec<Entry<C>>,
        streams: Arc<Mutex<JoinSet<()>>>,
        live: Option<LiveContext<C>>,
        shutdown: watch::Receiver<bool>,
        home: Option<HomeTokens>,
    ) -> Self {
        Self {
            default_id: RwLock::new(default_id),
            entries: RwLock::new(entries.into_iter().map(Arc::new).collect()),
            streams,
            live,
            shutdown,
            home,
            signins: Arc::new(SignIns::default()),
        }
    }

    /// The id of the project the unprefixed routes map to. Empty while no project runs.
    pub(crate) fn default_id(&self) -> String {
        self.default_id
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every project, in the order it joined the registry.
    pub(crate) fn entries(&self) -> Vec<Arc<Entry<C>>> {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Adds a project that was opened while the server runs. The first one added becomes the
    /// default when no project ran before.
    pub(crate) fn add(&self, entry: Entry<C>) {
        {
            let mut default_id = self
                .default_id
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            if default_id.is_empty() {
                default_id.clone_from(&entry.id);
            }
        }
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::new(entry));
    }

    /// The server's shutdown signal. A stream follows it, whether or not a project runs.
    pub(crate) fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.clone()
    }

    /// The user's folder, when the server was opened with one.
    pub(crate) fn home(&self) -> Option<&HomeTokens> {
        self.home.as_ref()
    }

    /// The sign-in codes not yet used.
    pub(crate) fn signins(&self) -> &Arc<SignIns> {
        &self.signins
    }

    /// What a project added while the server runs is opened with, or `None` when such adds are off.
    pub(crate) fn live(&self) -> Option<&LiveContext<C>> {
        self.live.as_ref()
    }

    /// The live-added routes that answer `path`, if any. A path under `/api/p/<id>` reaches the
    /// routes of the live-added project `id`. Any other API path, and `/mcp`, reaches the default
    /// project's unprefixed routes when that project was added to a server running none.
    pub(crate) fn live_router(&self, path: &str) -> Option<Router> {
        if let Some(rest) = path.strip_prefix("/api/p/") {
            let id = rest.split('/').next()?;
            return self
                .entries()
                .into_iter()
                .find(|entry| entry.id == id)
                .and_then(|entry| entry.router.clone());
        }
        if path != "/mcp" && !path.starts_with("/api/") {
            return None;
        }
        let default_id = self.default_id();
        self.entries()
            .into_iter()
            .find(|entry| entry.id == default_id)
            .and_then(|entry| entry.plain.clone())
    }

    /// The set the multiplexed streams are tracked in.
    pub(crate) fn streams(&self) -> &Arc<Mutex<JoinSet<()>>> {
        &self.streams
    }
}

impl<C: ModelClient> Registry<C> {
    /// The principal `token` acts for in any project or in the user's folder, or `None` for an
    /// unknown token. Each project's tokens are asked, then the user's folder.
    pub(crate) fn principal(&self, token: &str) -> Option<Principal> {
        self.entries()
            .iter()
            .find_map(|entry| entry.host.tokens().principal(token))
            .or_else(|| self.home.as_ref()?.tokens.principal(token))
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
/// of none gets 401, the same answer an unknown token gets from any route. The owner of a user's
/// folder gets an empty list while no project runs.
pub(crate) async fn list_projects<C: ModelClient + 'static>(
    State(projects): State<Arc<Registry<C>>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProjectView>>, ApiError> {
    let token = bearer(&headers).ok_or(ApiError::Unauthorized)?;
    let mut views = Vec::new();
    let default_id = projects.default_id();
    for entry in projects.entries() {
        if entry.host.tokens().principal(token).is_none() {
            continue;
        }
        views.push(view(&entry, &default_id).await);
    }
    let known_at_home = projects
        .home()
        .is_some_and(|home| home.tokens.principal(token).is_some());
    if views.is_empty() && !known_at_home {
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
            map_on_first_view: false,
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
