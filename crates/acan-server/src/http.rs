//! HTTP routes. Every `/api` route checks its bearer token here. The stream checks the same
//! token, from the header or from its first frame (see [`crate::stream`]).
//!
//! Status codes: a missing or unknown token is 401. A valid token whose principal may not use
//! the route is 403. That is an auth boundary, so an agent token on `POST /api/commands` gets 403
//! and never reaches the origin. A command the origin refuses is 409, with the `Rejection` as the
//! body.

use std::str::FromStr;
use std::sync::Arc;

use acan_core::{
    AgentId, Command, GateRunId, Generation, HumanCmd, MemberId, OrderId, OrderRev, Rejection,
    ScopeConfirmation, ScopeDraftOutcome, Seq, Stamped, TaskId, TaskState,
};
use acan_intent::{
    AppContractV1, EpisodeId, GetReceiptRequest, GetReceiptResponse, GetRulingsRequest,
    GetRulingsResponse, GetScopeRequest, GetScopeResponse, MachineId, McpError, McpErrorCode,
    McpV1, TaskRef,
};
use acan_llm::{ModelClient, Role, RoleStatus, UsageTotals};
use acan_scan::Plant;
use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::Principal;
use crate::host::{Host, SubmitError};
use crate::intent_api::{self, IntentError};
use crate::jobs;
use crate::mcp;
use crate::plant_view;
use crate::public::{self, PublicRoot};
use crate::registry::{self, Registry};
use crate::release::{self, BugReport, UpdateStatus, VersionInfo};
use crate::ruling::{self, RulingBody, RulingDraftView};
use crate::vision::{self, Side, VisionWitness};
use crate::workers::{Board, WorkersView};
use crate::{STATE_DIR, observe, redact, setup, stream};

/// Where one project's routes are mounted: the prefix of its API paths, and the path of its MCP
/// endpoint. The default project is mounted at `/api` and `/mcp`, and every project at
/// `/api/p/{id}` and `/api/p/{id}/mcp`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mount<'a> {
    pub(crate) api: &'a str,
    pub(crate) mcp: &'a str,
}

/// The routes one project's host answers under `mount`. The board is this project's own local
/// workers, which `GET .../workers` reports.
fn project_routes<C: ModelClient + 'static>(
    mount: Mount<'_>,
    host: Host<C>,
    board: Board,
) -> Router {
    let api = mount.api;
    Router::new()
        .route(mount.mcp, post(mcp::post::<C>))
        .route(&format!("{api}/commands"), post(post_command::<C>))
        .route(&format!("{api}/events"), get(get_events::<C>))
        .route(&format!("{api}/plant"), get(get_plant::<C>))
        .route(&format!("{api}/contract"), get(get_contract::<C>))
        .route(&format!("{api}/scan"), post(post_scan::<C>))
        .route(&format!("{api}/usage"), get(get_usage::<C>))
        .route(&format!("{api}/workers"), get(get_workers::<C>))
        .route(&format!("{api}/update"), get(get_update::<C>))
        .route(&format!("{api}/bug-report"), get(get_bug_report::<C>))
        .route(&format!("{api}/setup"), get(setup::get_status::<C>))
        .route(&format!("{api}/setup/keys"), post(setup::post_key::<C>))
        .route(
            &format!("{api}/setup/keys/{{provider}}"),
            delete(setup::delete_key::<C>),
        )
        .route(
            &format!("{api}/setup/roles/{{role}}"),
            put(setup::put_role::<C>),
        )
        .route(&format!("{api}/stream"), get(stream::upgrade::<C>))
        .route(
            &format!("{api}/tasks/{{task}}/worktree"),
            get(get_worktree::<C>),
        )
        .route(&format!("{api}/tasks/{{task}}/scope"), get(get_scope::<C>))
        .route(
            &format!("{api}/tasks/{{task}}/rulings"),
            get(get_rulings::<C>),
        )
        .route(
            &format!("{api}/tasks/{{task}}/receipt"),
            get(get_receipt::<C>),
        )
        .route(
            &format!("{api}/orders/{{order}}/plant-view"),
            get(get_plant_view::<C>),
        )
        .route(
            &format!("{api}/orders/{{order}}/scope-draft"),
            get(get_scope_draft::<C>),
        )
        .route(
            &format!("{api}/orders/{{order}}/scope/confirmation"),
            get(get_scope_confirmation::<C>),
        )
        .route(
            &format!("{api}/orders/{{order}}/episodes/{{episode}}/ruling-draft"),
            get(get_ruling_draft::<C>),
        )
        .route(
            &format!("{api}/orders/{{order}}/episodes/{{episode}}/ruling"),
            post(post_ruling::<C>),
        )
        .route(
            &format!("{api}/tasks/{{task}}/vision"),
            get(get_vision::<C>),
        )
        .route(
            &format!("{api}/tasks/{{task}}/vision/{{file}}"),
            get(get_vision_image::<C>),
        )
        .route(
            &format!("{api}/tasks/{{task}}/vision/{{index}}/baseline.png"),
            get(get_vision_baseline::<C>),
        )
        .layer(Extension(board))
        .with_state(host)
}

/// Builds the router for every project. Each project answers under `/api/p/{id}`, and the default
/// project also answers at the unprefixed paths, which the setup page and older clients use. The
/// server's own health and version are not any project's. The public files are served for every
/// path the API does not claim.
pub(crate) fn router<C: ModelClient + 'static>(
    projects: Arc<Registry<C>>,
    web: Option<PublicRoot>,
) -> Router {
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/setup", get(setup::page))
        .merge(
            Router::new()
                .route("/api/projects", get(registry::list_projects::<C>))
                .with_state(Arc::clone(&projects)),
        );
    for entry in projects.entries() {
        let api = format!("/api/p/{}", entry.id);
        let mcp = format!("{api}/mcp");
        router = router.merge(project_routes(
            Mount {
                api: &api,
                mcp: &mcp,
            },
            entry.host.clone(),
            entry.board.clone(),
        ));
        if entry.id == projects.default_id() {
            router = router.merge(project_routes(
                Mount {
                    api: "/api",
                    mcp: "/mcp",
                },
                entry.host.clone(),
                entry.board.clone(),
            ));
        }
    }
    match web {
        Some(root) => {
            let root = Arc::new(root);
            router.fallback(move |request| public::serve(Arc::clone(&root), request))
        }
        None => router,
    }
}

/// A failed request, with the status it maps to.
#[derive(Debug)]
pub enum ApiError {
    /// The bearer token is missing or unknown.
    Unauthorized,
    /// The token is valid, but its principal may not use this route.
    Forbidden(&'static str),
    /// The route names something the world does not have.
    NotFound(&'static str),
    /// The request is malformed. The name says what it lacks, never what it sent.
    BadRequest(&'static str),
    /// The request is valid, but the server's state does not allow it. The name says why.
    Conflict(&'static str),
    /// The origin refused the command. The body is the [`Rejection`].
    Rejected(Rejection),
    /// The scope-first lane refused the call with a closed code. The body is the [`McpError`].
    Refused(McpError),
    /// Something on the server failed. The detail goes to the log, not the response.
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "unauthorized" })),
            )
                .into_response(),
            Self::Forbidden(why) => (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "forbidden", "why": why })),
            )
                .into_response(),
            Self::NotFound(what) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "not_found", "what": what })),
            )
                .into_response(),
            Self::BadRequest(what) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "bad_request", "what": what })),
            )
                .into_response(),
            Self::Conflict(why) => (
                StatusCode::CONFLICT,
                Json(json!({ "error": "conflict", "what": why })),
            )
                .into_response(),
            Self::Rejected(rejection) => (StatusCode::CONFLICT, Json(rejection)).into_response(),
            Self::Refused(err) => (refusal_status(&err.error), Json(err)).into_response(),
            Self::Internal(err) => {
                tracing::error!(error = ?err, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "internal" })),
                )
                    .into_response()
            }
        }
    }
}

impl From<SubmitError> for ApiError {
    fn from(err: SubmitError) -> Self {
        match err {
            SubmitError::Rejected(rejection) => Self::Rejected(rejection),
            SubmitError::Internal(err) => Self::Internal(err),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err)
    }
}

impl From<IntentError> for ApiError {
    fn from(err: IntentError) -> Self {
        match err {
            IntentError::Refused(err) => Self::Refused(err),
            IntentError::Internal(err) => Self::Internal(err),
        }
    }
}

/// The HTTP status of a scope-first refusal. A scope or evidence that moved is a conflict with
/// what the caller read. A malformed request is 400, and a missing task or receipt is 404.
fn refusal_status(code: &McpErrorCode) -> StatusCode {
    match code {
        McpErrorCode::Unauthorized {} => StatusCode::UNAUTHORIZED,
        McpErrorCode::Forbidden { .. } => StatusCode::FORBIDDEN,
        McpErrorCode::NotFound { .. } => StatusCode::NOT_FOUND,
        McpErrorCode::InvalidRequest { .. } => StatusCode::BAD_REQUEST,
        McpErrorCode::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
        McpErrorCode::ScopeNotConfirmed { .. }
        | McpErrorCode::StaleScope { .. }
        | McpErrorCode::StaleEvidence { .. }
        | McpErrorCode::BudgetExhausted { .. } => StatusCode::CONFLICT,
    }
}

/// The bearer token in an `Authorization: Bearer` header, if there is one.
pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// The principal a token acts for.
pub(crate) fn authenticate<C: ModelClient>(
    host: &Host<C>,
    token: Option<&str>,
) -> Result<Principal, ApiError> {
    token
        .and_then(|token| host.tokens().principal(token))
        .cloned()
        .ok_or(ApiError::Unauthorized)
}

pub(crate) fn member<C: ModelClient>(
    host: &Host<C>,
    headers: &HeaderMap,
) -> Result<MemberId, ApiError> {
    match authenticate(host, bearer(headers))? {
        Principal::Member(member) => Ok(member),
        Principal::Agent(_) => Err(ApiError::Forbidden("only members may do this over HTTP")),
    }
}

async fn health() -> &'static str {
    "ok"
}

/// The build's identity. Public, since it holds nothing private and the app shows it before anyone
/// has signed in.
async fn version() -> Json<VersionInfo> {
    Json(release::version_info())
}

/// Every human command. The scope-first human commands arrive here too, as bodies: ReviseScope,
/// ConfirmScope, ResolveContest and RecordRuling. Only a member reaches the origin, and the origin
/// checks that the member is the order's poster or an owner. An agent token gets 403 here. A scope
/// revision with a fixed base anchor in a must-keep rule gets 400, see [`refuse_fixed_must_keep`].
async fn post_command<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Json(cmd): Json<HumanCmd>,
) -> Result<Json<Vec<Stamped>>, ApiError> {
    let member = member(&host, &headers)?;
    refuse_fixed_must_keep(&cmd)?;
    let events = host.submit(Command::Human { member, cmd }).await?;
    Ok(Json(events))
}

/// Refuses a scope revision whose must-keep obligations name a saved example as their base. A later
/// gate run loads no saved examples, so it would read such an obligation as unknown and block on
/// it. The rule's predicates are checked at any depth, so a fixed anchor under `all` is refused too.
fn refuse_fixed_must_keep(cmd: &HumanCmd) -> Result<(), ApiError> {
    let HumanCmd::ReviseScope { scope, .. } = cmd else {
        return Ok(());
    };
    let fixed = scope
        .body
        .must_keep
        .iter()
        .flat_map(|rule| rule.obligations.iter())
        .any(|obligation| !obligation.predicate.fixed_anchors().is_empty());
    if fixed {
        return Err(ApiError::BadRequest(
            "must-keep predicate must use a fresh base anchor: fixed anchors are not supported yet",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    from: u64,
}

/// The log from `from`, as the caller's principal may read it. A member gets each event as stored;
/// an agent gets the member-only fields redacted, with the same seq, time and actor.
async fn get_events<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let principal = authenticate(&host, bearer(&headers))?;
    let events = host.store().load_from(Seq::new(query.from)).await?;
    let views = events
        .iter()
        .map(|stamped| redact::view(&principal, stamped))
        .collect::<anyhow::Result<Vec<Value>>>()?;
    Ok(Json(views))
}

/// The plant, with the two flags the UI reads from it: whether the protected app contract is in
/// place, and whether the advisory vision check runs. The plant's own fields stay at the top level.
#[derive(Serialize)]
struct PlantResponse {
    #[serde(flatten)]
    plant: Plant,
    contract: ContractView,
    vision: VisionView,
}

async fn get_plant<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
) -> Result<Json<PlantResponse>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    let plant = host.scan().plant.clone();
    // The plant still loads when the protected head cannot be read. The contract then reads absent.
    let contract = match contract_of(&host).await {
        Ok(contract) => contract,
        Err(err) => {
            tracing::warn!(error = ?err, "the protected app contract could not be read for the plant");
            ContractView {
                present: false,
                version: None,
            }
        }
    };
    Ok(Json(PlantResponse {
        plant,
        contract,
        vision: vision_of(&host),
    }))
}

/// Whether the protected app contract is in place, and its version. Any authenticated principal may
/// read it: it is the protected head's file, and the agent's worktree holds the same one.
async fn get_contract<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
) -> Result<Json<ContractView>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    Ok(Json(contract_of(&host).await?))
}

async fn post_scan<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
) -> Result<Json<Vec<Stamped>>, ApiError> {
    member(&host, &headers)?;
    let events = host.rescan().await?;
    Ok(Json(events))
}

async fn get_usage<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
) -> Result<Json<Vec<UsageTotals>>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    Ok(Json(host.ledger().snapshot()))
}

/// The local workers this server runs and what each is doing. Members only, since an agent token
/// reads the origin through MCP, not through this route.
async fn get_workers<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    Extension(board): Extension<Board>,
    headers: HeaderMap,
) -> Result<Json<WorkersView>, ApiError> {
    member(&host, &headers)?;
    Ok(Json(board.view()))
}

/// The update check's status. Any principal may read it.
async fn get_update<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
) -> Result<Json<UpdateStatus>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    Ok(Json(release::update_status()))
}

/// The bug report: a pre-filled issue link, and its full text. Members only, since the person
/// using the app is the one who files it.
async fn get_bug_report<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
) -> Result<Json<BugReport>, ApiError> {
    member(&host, &headers)?;
    Ok(Json(release::bug_report()))
}

/// Whether the protected app contract is in place at the protected head, and its version.
#[derive(Serialize)]
pub(crate) struct ContractView {
    present: bool,
    version: Option<String>,
}

/// The contract at the protected head, read from the same snapshot the gate tests against. It is
/// present when the app contract and the probe corpus both read and decode, since the gate needs
/// both. A server with no git repository has no protected head to read, so that is an error.
pub(crate) async fn contract_of<C: ModelClient>(host: &Host<C>) -> Result<ContractView, ApiError> {
    let workspace = host
        .workspace()
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("the server has no git repository")))?;
    let snapshot = jobs::protected_snapshot(workspace.repo(), &host.repo().join(STATE_DIR)).await?;
    Ok(match observe::read_protected(&snapshot) {
        Ok(_) => ContractView {
            present: true,
            version: Some(AppContractV1::TAG.to_owned()),
        },
        Err(_) => ContractView {
            present: false,
            version: None,
        },
    })
}

/// Whether the advisory vision check runs, and why it does not. The UI reads this rather than
/// parsing the reason a vision record gives.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct VisionView {
    on: bool,
    reason: Option<String>,
}

/// The vision flag of this server: the check runs when the server runs it and a provider serves
/// the vision role.
fn vision_of<C: ModelClient>(host: &Host<C>) -> VisionView {
    let keys = host.client().key_status();
    vision_view(host.vision().is_some(), keys.role(Role::Vision))
}

/// The vision flag from whether the server runs the check and the status of the vision role.
fn vision_view(configured: bool, role: Option<&RoleStatus>) -> VisionView {
    if !configured {
        return VisionView {
            on: false,
            reason: Some("the server runs no vision checks".to_owned()),
        };
    }
    match role {
        Some(status) => VisionView {
            on: status.off_reason.is_none(),
            reason: status.off_reason.map(str::to_owned),
        },
        None => VisionView {
            on: false,
            reason: Some("no model serves vision".to_owned()),
        },
    }
}

/// Where a claimed task's worktree is, the branch it is on, and the protected-head snapshot its
/// tests mount protected paths from.
#[derive(Serialize)]
pub(crate) struct WorktreeView {
    task: String,
    path: String,
    branch: String,
    snapshot: String,
}

/// Where the worktree of a claimed task is. Only the agent holding that task's lease may read
/// it, so the path reaches the one agent that works in it. The worktree is created here if the
/// lease grant has not created it yet.
async fn get_worktree<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path(task): Path<String>,
) -> Result<Json<WorktreeView>, ApiError> {
    let agent = match authenticate(&host, bearer(&headers))? {
        Principal::Agent(agent) => agent,
        Principal::Member(_) => {
            return Err(ApiError::Forbidden(
                "only the agent holding the task's lease may read its worktree",
            ));
        }
    };
    let task: TaskId = task.parse().map_err(|_| ApiError::NotFound("task"))?;
    Ok(Json(worktree_of(&host, &agent, task).await?))
}

/// The worktree of `task` for the agent holding its lease. Shared by the HTTP route and the MCP
/// `get_worktree` tool, so both answer the same way.
pub(crate) async fn worktree_of<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    task: TaskId,
) -> Result<WorktreeView, ApiError> {
    host.read(|world| held_generation(world, task, agent))?;
    let workspace = host
        .workspace()
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("the server has no git repository")))?;
    // The task lock is held across the re-read, so a release between the check above and the
    // create cannot hand this request the next claim's worktree.
    let _task = workspace.lock_task(task).await;
    let generation = host.read(|world| held_generation(world, task, agent))?;
    let worktree = workspace
        .worktree(task, generation)
        .await
        .map_err(|err| ApiError::Internal(err.into()))?;
    let snapshot = jobs::protected_snapshot(workspace.repo(), &host.repo().join(STATE_DIR)).await?;
    Ok(WorktreeView {
        task: task.to_string(),
        path: worktree.path().display().to_string(),
        branch: worktree.branch().to_owned(),
        snapshot: snapshot.display().to_string(),
    })
}

/// The generation of the lease `agent` holds on `task`.
fn held_generation(
    world: &acan_core::World,
    task: TaskId,
    agent: &AgentId,
) -> Result<Generation, ApiError> {
    match world.task(task) {
        Some(found) => match &found.state {
            TaskState::Claimed { claim, .. } if &claim.lease.holder == agent => {
                Ok(claim.lease.generation)
            }
            _ => Err(ApiError::Forbidden(
                "only the agent holding the task's lease may read its worktree",
            )),
        },
        None => Err(ApiError::NotFound("task")),
    }
}

/// The query of `GET /api/tasks/{task}/rulings`.
#[derive(Deserialize)]
struct RulingsQuery {
    /// Comma-separated machine ids. Absent or empty means every machine.
    #[serde(default)]
    machines: String,
}

/// The confirmed scope of a task's order. Any authenticated principal may read it, as with the
/// other reads, because an agent reads the same scope over MCP before it reads back.
async fn get_scope<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path(task): Path<String>,
) -> Result<Json<GetScopeResponse>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    let request = GetScopeRequest {
        version: McpV1,
        task: task_ref(&task)?,
    };
    Ok(Json(intent_api::get_scope(&host, &request)?))
}

/// The rulings in force on the named machines, or on every machine.
async fn get_rulings<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path(task): Path<String>,
    Query(query): Query<RulingsQuery>,
) -> Result<Json<GetRulingsResponse>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    let machine_ids = query
        .machines
        .split(',')
        .filter(|id| !id.is_empty())
        .map(MachineId::from_str)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid_request("machines must be comma-separated machine ids"))?;
    let request = GetRulingsRequest {
        version: McpV1,
        task: task_ref(&task)?,
        machine_ids,
    };
    Ok(Json(intent_api::get_rulings(&host, &request)?))
}

/// The latest scope draft of an order: the draft the foreman returned, or the failure that
/// ended its last attempt, with the order revision it was for. Members only, since a human
/// adopts a draft. The draft confirms nothing; the order's scope is the one a human revises and
/// confirms.
#[derive(Serialize)]
pub(crate) struct ScopeDraftView {
    order: String,
    rev: OrderRev,
    #[serde(flatten)]
    outcome: ScopeDraftOutcome,
}

/// The plant view of the order's latest receipt, with its tour filled in. Members only, as the
/// other order reads are; a 404 names what the order lacks.
async fn get_plant_view<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path(order): Path<String>,
) -> Result<Json<acan_intent::PlantView>, ApiError> {
    member(&host, &headers)?;
    let order: OrderId = order.parse().map_err(|_| ApiError::NotFound("order"))?;
    Ok(Json(plant_view::order_view(&host, order).await?))
}

async fn get_scope_draft<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path(order): Path<String>,
) -> Result<Json<ScopeDraftView>, ApiError> {
    member(&host, &headers)?;
    let order: OrderId = order.parse().map_err(|_| ApiError::NotFound("order"))?;
    let view = host.read(|world| {
        if world.order(order).is_none() {
            return Err(ApiError::NotFound("order"));
        }
        world
            .scope_draft(order)
            .map(|(rev, outcome)| ScopeDraftView {
                order: order.to_string(),
                rev,
                outcome: outcome.clone(),
            })
            .ok_or(ApiError::NotFound("no scope draft for this order yet"))
    })?;
    Ok(Json(view))
}

/// The body a member sends as `ConfirmScope` for an order's current scope revision. It carries
/// the hashes and bindings the origin checks, read from the lane and the ledger, so a script and
/// a UI send the same bytes. It confirms nothing. The member sends it, and the origin accepts it
/// only if the open choices and broad items are answered in it.
async fn get_scope_confirmation<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path(order): Path<String>,
) -> Result<Json<ScopeConfirmation>, ApiError> {
    let member = member(&host, &headers)?;
    let order: OrderId = order.parse().map_err(|_| ApiError::NotFound("order"))?;
    let body = host.read(|world| {
        if world.lane(order).is_none() {
            return Err(ApiError::NotFound("scope for this order"));
        }
        acan_core::confirmation_body(world, order, &member, crate::host::now())
            .map_err(ApiError::Rejected)
    })?;
    Ok(Json(body))
}

/// The ruling draft of an episode: what a member rules from. Members only, since a human rules.
/// See [`ruling::draft`].
async fn get_ruling_draft<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path((order, episode)): Path<(String, String)>,
) -> Result<Json<RulingDraftView>, ApiError> {
    member(&host, &headers)?;
    let order: OrderId = order.parse().map_err(|_| ApiError::NotFound("order"))?;
    let episode: EpisodeId = episode.parse().map_err(|_| ApiError::NotFound("episode"))?;
    Ok(Json(ruling::draft(&host, order, episode)?))
}

/// A member's ruling on one episode. The body is the decision and predicate the member chose and
/// the draft it was made from; the server builds the ledger event. Members only, since a human
/// rules. See [`ruling::record`].
async fn post_ruling<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path((order, episode)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Vec<Stamped>>, ApiError> {
    let member = member(&host, &headers)?;
    let order: OrderId = order.parse().map_err(|_| ApiError::NotFound("order"))?;
    let episode: EpisodeId = episode.parse().map_err(|_| ApiError::NotFound("episode"))?;
    let body: RulingBody =
        serde_json::from_slice(&body).map_err(|_| ApiError::BadRequest("ruling body"))?;
    Ok(Json(
        ruling::record(&host, &member, order, episode, body).await?,
    ))
}

/// The latest receipt of a task's gate runs.
async fn get_receipt<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path(task): Path<String>,
) -> Result<Json<GetReceiptResponse>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    let request = GetReceiptRequest {
        version: McpV1,
        task: task_ref(&task)?,
    };
    Ok(Json(intent_api::get_receipt(&host, &request)?))
}

/// The advisory vision record of the task's latest gate run.
async fn get_vision<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path(task): Path<String>,
) -> Result<Json<VisionWitness>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    let (task, run) = vision_run(&host, &task)?;
    Ok(Json(vision::served(host.vision(), task, run).await?))
}

/// The candidate screenshot of one vision check. The segment is `{index}.png`.
async fn get_vision_image<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path((task, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    authenticate(&host, bearer(&headers))?;
    let index = file
        .strip_suffix(".png")
        .ok_or(ApiError::NotFound("image"))?;
    vision_png(&host, &task, index, Side::Candidate).await
}

/// The baseline screenshot of one vision check. 404 when the check has no baseline.
async fn get_vision_baseline<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path((task, index)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    authenticate(&host, bearer(&headers))?;
    vision_png(&host, &task, &index, Side::Baseline).await
}

/// The task and its latest gate run. A task with no gate run is 404.
fn vision_run<C: ModelClient>(host: &Host<C>, raw: &str) -> Result<(TaskId, GateRunId), ApiError> {
    let task: TaskId = raw.parse().map_err(|_| ApiError::NotFound("task"))?;
    host.read(|world| {
        if world.task(task).is_none() {
            return Err(ApiError::NotFound("task"));
        }
        vision::latest_run(world, task)
            .map(|run| (task, run))
            .ok_or(ApiError::NotFound("gate run"))
    })
}

/// One stored screenshot as `image/png`. The index is parsed as a number, so the file read is
/// always one the vision witness named.
async fn vision_png<C: ModelClient>(
    host: &Host<C>,
    task: &str,
    index: &str,
    side: Side,
) -> Result<Response, ApiError> {
    let (task, run) = vision_run(host, task)?;
    let index: u32 = index.parse().map_err(|_| ApiError::NotFound("image"))?;
    let Some(vision) = host.vision() else {
        return Err(ApiError::NotFound("image"));
    };
    let png = vision
        .image(task, run, index, side)
        .await
        .map_err(|err| ApiError::Internal(err.into()))?
        .ok_or(ApiError::NotFound("image"))?;
    Ok((
        [
            (CONTENT_TYPE, "image/png"),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (CACHE_CONTROL, "no-store"),
        ],
        png,
    )
        .into_response())
}

/// A task id from a path. One that has no contract spelling names no task.
fn task_ref(raw: &str) -> Result<TaskRef, ApiError> {
    TaskRef::from_str(raw).map_err(|_| ApiError::NotFound("task"))
}

/// A request the scope-first lane refuses before it reaches the origin.
fn invalid_request(detail: &str) -> ApiError {
    ApiError::Refused(McpError::new(
        McpErrorCode::InvalidRequest {
            detail: detail.to_owned(),
        },
        detail,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vision_role(off_reason: Option<&'static str>) -> RoleStatus {
        RoleStatus {
            role: Role::Vision,
            provider: off_reason.is_none().then(|| "meta".to_owned()),
            picked: false,
            off_reason,
            meta_only: true,
        }
    }

    #[test]
    fn a_server_that_runs_no_vision_checks_says_so() {
        let view = vision_view(false, Some(&vision_role(None)));
        assert_eq!(
            view,
            VisionView {
                on: false,
                reason: Some("the server runs no vision checks".to_owned()),
            }
        );
    }

    #[test]
    fn vision_is_on_when_a_provider_serves_its_role() {
        let view = vision_view(true, Some(&vision_role(None)));
        assert_eq!(
            view,
            VisionView {
                on: true,
                reason: None,
            }
        );
    }

    #[test]
    fn vision_is_off_with_the_reason_its_role_gives() {
        let view = vision_view(true, Some(&vision_role(Some("it needs a Meta key"))));
        assert_eq!(
            view,
            VisionView {
                on: false,
                reason: Some("it needs a Meta key".to_owned()),
            }
        );
    }

    #[test]
    fn vision_is_off_when_no_model_lists_its_role() {
        let view = vision_view(true, None);
        assert_eq!(
            view,
            VisionView {
                on: false,
                reason: Some("no model serves vision".to_owned()),
            }
        );
    }
}
