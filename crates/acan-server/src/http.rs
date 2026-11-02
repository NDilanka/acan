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
    AgentId, Command, Generation, HumanCmd, MemberId, OrderId, OrderRev, Rejection,
    ScopeConfirmation, ScopeDraftOutcome, Seq, Stamped, TaskId, TaskState,
};
use acan_intent::{
    ContrastExample, ContrastExampleHash, EpisodeId, GetReceiptRequest, GetReceiptResponse,
    GetRulingsRequest, GetRulingsResponse, GetScopeRequest, GetScopeResponse, LedgerHead,
    MachineId, McpError, McpErrorCode, McpV1, NormalizationHash, NormalizationVersion, ProbeHash,
    ScopeHash, Selector, TaskRef,
};
use acan_llm::{ModelClient, UsageTotals};
use acan_scan::Plant;
use axum::extract::{Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::Principal;
use crate::examples;
use crate::host::{Host, SubmitError};
use crate::intent_api::{self, IntentError};
use crate::jobs;
use crate::mcp;
use crate::public::{self, PublicRoot};
use crate::{STATE_DIR, redact, stream};

/// Builds the router. The public files are served for every path the API does not claim.
pub(crate) fn router<C: ModelClient + 'static>(host: Host<C>, web: Option<PublicRoot>) -> Router {
    let router = Router::new()
        .route("/health", get(health))
        .route("/mcp", post(mcp::post::<C>))
        .route("/api/commands", post(post_command::<C>))
        .route("/api/events", get(get_events::<C>))
        .route("/api/plant", get(get_plant::<C>))
        .route("/api/scan", post(post_scan::<C>))
        .route("/api/usage", get(get_usage::<C>))
        .route("/api/stream", get(stream::upgrade::<C>))
        .route("/api/tasks/{task}/worktree", get(get_worktree::<C>))
        .route("/api/tasks/{task}/scope", get(get_scope::<C>))
        .route("/api/tasks/{task}/rulings", get(get_rulings::<C>))
        .route("/api/tasks/{task}/receipt", get(get_receipt::<C>))
        .route("/api/orders/{order}/scope-draft", get(get_scope_draft::<C>))
        .route(
            "/api/orders/{order}/scope/confirmation",
            get(get_scope_confirmation::<C>),
        )
        .route(
            "/api/orders/{order}/episodes/{episode}/ruling-draft",
            get(get_ruling_draft::<C>),
        );
    let router = match web {
        Some(root) => {
            let root = Arc::new(root);
            router.fallback(move |request| public::serve(Arc::clone(&root), request))
        }
        None => router,
    };
    router.with_state(host)
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

fn member<C: ModelClient>(host: &Host<C>, headers: &HeaderMap) -> Result<MemberId, ApiError> {
    match authenticate(host, bearer(headers))? {
        Principal::Member(member) => Ok(member),
        Principal::Agent(_) => Err(ApiError::Forbidden("only members may do this over HTTP")),
    }
}

async fn health() -> &'static str {
    "ok"
}

/// Every human command. The scope-first human commands arrive here too, as bodies: ReviseScope,
/// ConfirmScope, ResolveContest and RecordRuling. Only a member reaches the origin, and the origin
/// checks that the member is the order's poster or an owner. An agent token gets 403 here.
async fn post_command<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Json(cmd): Json<HumanCmd>,
) -> Result<Json<Vec<Stamped>>, ApiError> {
    let member = member(&host, &headers)?;
    let events = host.submit(Command::Human { member, cmd }).await?;
    Ok(Json(events))
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

async fn get_plant<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
) -> Result<Json<Plant>, ApiError> {
    authenticate(&host, bearer(&headers))?;
    let scan = host.scan();
    Ok(Json(scan.plant.clone()))
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

/// What a member needs to rule on one discrepancy episode: the contrast examples kept for it, the
/// example the ruling is on, and the hashes and versions a decision names. Members only, since a
/// human rules. The draft confirms nothing. `RecordRuling` checks the decision a member sends.
#[derive(Serialize)]
struct RulingDraftView {
    order: String,
    episode: EpisodeId,
    scope_hash: ScopeHash,
    ledger_head: LedgerHead,
    selector: Selector,
    probe_hash: ProbeHash,
    /// The example the ruling is on: the episode's latest candidate run, for its probe.
    ruled_on: ContrastExampleHash,
    normalization_version: NormalizationVersion,
    normalization_hash: NormalizationHash,
    /// Every example kept for the episode, by run and then hash.
    examples: Vec<DraftExample>,
}

/// One example kept for an episode, with the normalization it was compared under.
#[derive(Serialize)]
struct DraftExample {
    run: String,
    hash: ContrastExampleHash,
    probe_hash: ProbeHash,
    normalization_version: NormalizationVersion,
    normalization_hash: NormalizationHash,
    example: ContrastExample,
}

/// The ruling draft of an episode. The episode's latest evidence names the candidate run the
/// ruling is on, so the example of that run and the episode's probe is `ruled_on`.
async fn get_ruling_draft<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Path((order, episode)): Path<(String, String)>,
) -> Result<Json<RulingDraftView>, ApiError> {
    member(&host, &headers)?;
    let order: OrderId = order.parse().map_err(|_| ApiError::NotFound("order"))?;
    let episode: EpisodeId = episode.parse().map_err(|_| ApiError::NotFound("episode"))?;
    let (scope_hash, ledger_head, selector, latest) = host.read(|world| {
        let lane = world
            .lane(order)
            .ok_or(ApiError::NotFound("scope for this order"))?;
        let found = lane
            .episodes
            .get(&episode)
            .ok_or(ApiError::NotFound("episode"))?;
        let latest = found
            .evidence
            .last()
            .ok_or(ApiError::NotFound("evidence for this episode"))?;
        Ok::<_, ApiError>((
            lane.scope_hash,
            world.ledger().head(),
            found.selector.clone(),
            (latest.run.to_string(), latest.probe_hash),
        ))
    })?;
    let (run, probe_hash) = latest;
    let dir = host.repo().join(STATE_DIR).join(examples::DIR);
    let kept = examples::read(&dir, &episode).map_err(|err| ApiError::Internal(err.into()))?;
    let examples = kept
        .into_iter()
        .map(|stored| {
            let internal = |err| ApiError::Internal(anyhow::Error::from(err));
            let probe = stored.example.probe.spec.probe_hash().map_err(internal)?;
            let normalization = &stored.example.probe.normalization;
            Ok(DraftExample {
                run: stored.run,
                hash: stored.hash,
                probe_hash: probe,
                normalization_version: normalization.version.clone(),
                normalization_hash: normalization.normalization_hash().map_err(internal)?,
                example: stored.example,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let ruled = ruled_example(&examples, &run, probe_hash)?;
    Ok(Json(RulingDraftView {
        order: order.to_string(),
        episode,
        scope_hash,
        ledger_head,
        selector,
        probe_hash,
        ruled_on: ruled.hash,
        normalization_version: ruled.normalization_version.clone(),
        normalization_hash: ruled.normalization_hash,
        examples,
    }))
}

/// The example a ruling is on: the one kept for the episode's latest run and probe. A run that
/// crashed before its receipt was recorded is run again under the same id, and the store then holds
/// an example from each attempt. The receipt does not say which attempt it rests on, so when more
/// than one example matches, the draft refuses rather than pick one.
fn ruled_example<'a>(
    examples: &'a [DraftExample],
    run: &str,
    probe_hash: ProbeHash,
) -> Result<&'a DraftExample, ApiError> {
    let mut matching = examples
        .iter()
        .filter(|kept| kept.run == run && kept.probe_hash == probe_hash);
    match (matching.next(), matching.next()) {
        (None, _) => Err(ApiError::NotFound(
            "retained example for the episode's latest run",
        )),
        (Some(kept), None) => Ok(kept),
        (Some(_), Some(_)) => {
            let message = format!(
                "two retained examples match run {run}: the receipt does not say which \
                 attempt it rests on, so the ruling draft refuses"
            );
            Err(ApiError::Refused(McpError::new(
                McpErrorCode::StaleEvidence {
                    detail: message.clone(),
                },
                message,
            )))
        }
    }
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

    /// A kept example of the fixture's probe, filed under `run`. `digit` names its hash.
    fn kept(run: &str, digit: &str) -> DraftExample {
        let example: ContrastExample = serde_json::from_str(include_str!(
            "../../acan-intent/fixtures/contrast-example.json"
        ))
        .expect("the fixture decodes");
        DraftExample {
            run: run.to_owned(),
            hash: ContrastExampleHash::from_str(&format!("sha256:{}", digit.repeat(64)))
                .expect("a hash"),
            probe_hash: example.probe.spec.probe_hash().expect("hashing"),
            normalization_version: example.probe.normalization.version.clone(),
            normalization_hash: example
                .probe
                .normalization
                .normalization_hash()
                .expect("hashing"),
            example,
        }
    }

    #[test]
    fn the_one_example_of_the_latest_run_and_probe_is_the_ruled_one() {
        let examples = [kept("run-1", "a"), kept("run-2", "b")];
        let probe = examples[0].probe_hash;
        let ruled = ruled_example(&examples, "run-1", probe).expect("one example matches");
        assert_eq!(ruled.hash, examples[0].hash);
    }

    #[test]
    fn two_examples_of_the_latest_run_refuse_rather_than_pick_one() {
        // A run that crashed before its receipt was recorded runs again under the same id, and
        // the store then keeps an example from each attempt.
        let examples = [kept("run-1", "a"), kept("run-1", "b")];
        let probe = examples[0].probe_hash;
        match ruled_example(&examples, "run-1", probe).err() {
            Some(ApiError::Refused(err)) => assert!(
                matches!(err.error, McpErrorCode::StaleEvidence { .. }),
                "two matches are stale evidence: {:?}",
                err.error
            ),
            other => panic!("two matching examples must refuse, got {other:?}"),
        }
    }

    #[test]
    fn no_example_of_the_latest_run_is_not_found() {
        let examples = [kept("run-1", "a")];
        let probe = examples[0].probe_hash;
        assert!(matches!(
            ruled_example(&examples, "run-2", probe).err(),
            Some(ApiError::NotFound(_))
        ));
    }
}
