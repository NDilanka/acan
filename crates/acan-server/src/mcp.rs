//! The MCP endpoint agents use: JSON-RPC 2.0 over Streamable HTTP at `/mcp`.
//!
//! Every tool is a thin transport over the same `Command::Agent` path that the origin takes for
//! any agent command. Bearer auth, principal resolution, the writer and fencing are the ones the
//! HTTP routes use, so a tool decides nothing itself. A refused command comes back as a tool
//! error that carries the [`Rejection`] text, not as a transport error. The endpoint answers each
//! request with one JSON body and never opens a stream, which the transport allows.

use std::str::FromStr;

use acan_core::{
    AgentCmd, AgentId, Claim, Clash, Command, CommitId, DeclarationInput, Event, Fence, FilePath,
    Generation, Locus, Order, Readback, Stage, Stamped, SymbolRef, Task, TaskId, TaskState, World,
};
use acan_intent::{
    ContestRequest, GetReceiptRequest, GetRulingsRequest, GetScopeRequest, McpError, McpErrorCode,
    PostReadbackRequest,
};
use acan_llm::ModelClient;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::Principal;
use crate::host::{Host, SubmitError};
use crate::http::{ApiError, authenticate, bearer, worktree_of};
use crate::intent_api::{self, IntentResult};

/// The revision the endpoint answers with when a client asks for one it does not list.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// The revisions the endpoint speaks. Each one gets the same answers, so a client's revision is
/// echoed back when it is listed here.
const SUPPORTED_VERSIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Every tool the endpoint serves. The scope-first human commands (revise, confirm, resolve and
/// rule) are not here: an agent that names one gets an unknown tool, and the human route refuses
/// agent tokens. Agents confirm nothing.
const TOOLS: [&str; 14] = [
    "list_tasks",
    "get_task",
    "post_readback",
    "claim_task",
    "heartbeat_task",
    "release_task",
    "declare_changes",
    "report_built",
    "get_worktree",
    "get_scope",
    "get_rulings",
    "get_receipt",
    "post_scoped_readback",
    "file_contest",
];

/// One JSON-RPC message, checked against the 2.0 envelope before any method runs.
enum Envelope {
    /// A request. Its answer carries the same id, which is a string, a number, or null.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification has no id, so it gets no reply body.
    Notification,
}

/// A JSON-RPC protocol error. Tool failures are not these; they travel as tool results.
struct Rpc {
    code: i64,
    message: String,
}

impl Rpc {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Handles one JSON-RPC message from an agent.
pub(crate) async fn post<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let agent = match authenticate(&host, bearer(&headers)) {
        Ok(Principal::Agent(agent)) => agent,
        Ok(Principal::Member(_)) => {
            return ApiError::Forbidden("only agents may use the MCP endpoint").into_response();
        }
        Err(err) => return err.into_response(),
    };
    let value: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(err) => {
            let rpc = Rpc::new(PARSE_ERROR, format!("parse error: {err}"));
            return (StatusCode::BAD_REQUEST, Json(error_body(Value::Null, rpc))).into_response();
        }
    };
    let (id, method, params) = match envelope(&value) {
        Ok(Envelope::Request { id, method, params }) => (id, method, params),
        Ok(Envelope::Notification) => return StatusCode::ACCEPTED.into_response(),
        Err((id, rpc)) => {
            return (StatusCode::BAD_REQUEST, Json(error_body(id, rpc))).into_response();
        }
    };
    let outcome = match method.as_str() {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_list() })),
        "tools/call" => call_tool(&host, &agent, &params).await,
        other => Err(Rpc::new(
            METHOD_NOT_FOUND,
            format!("unknown method {other}"),
        )),
    };
    let body = match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(rpc) => error_body(id, rpc),
    };
    (StatusCode::OK, Json(body)).into_response()
}

/// Checks the JSON-RPC 2.0 envelope of one message. A message that breaks it is an invalid
/// request. Its error carries the request's own id when that id is valid, and null otherwise.
fn envelope(value: &Value) -> Result<Envelope, (Value, Rpc)> {
    let invalid = |id: &Value, text: &str| (id.clone(), Rpc::new(INVALID_REQUEST, text));
    let Some(object) = value.as_object() else {
        return Err(invalid(&Value::Null, "a message is a JSON object"));
    };
    // A present id is a request, even when it is null. Only a missing id makes a notification.
    let id = object.get("id");
    let reply_id = match id {
        None => Value::Null,
        Some(id) if id.is_string() || id.is_number() || id.is_null() => id.clone(),
        Some(_) => {
            return Err(invalid(
                &Value::Null,
                "id must be a string, a number, or null",
            ));
        }
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(invalid(&reply_id, "jsonrpc must be \"2.0\""));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Err(invalid(&reply_id, "the message names no method"));
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    if !(params.is_null() || params.is_object() || params.is_array()) {
        return Err(invalid(&reply_id, "params must be an object or an array"));
    }
    Ok(match id {
        None => Envelope::Notification,
        Some(id) => Envelope::Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
        },
    })
}

fn error_body(id: Value, rpc: Rpc) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": rpc.code, "message": rpc.message },
    })
}

fn initialize(params: &Value) -> Value {
    let requested = params["protocolVersion"]
        .as_str()
        .unwrap_or(PROTOCOL_VERSION);
    let version = if SUPPORTED_VERSIONS.contains(&requested) {
        requested
    } else {
        PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "acan", "version": env!("CARGO_PKG_VERSION") },
    })
}

async fn call_tool<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    params: &Value,
) -> Result<Value, Rpc> {
    let name = params["name"]
        .as_str()
        .ok_or_else(|| Rpc::new(INVALID_PARAMS, "tools/call needs a tool name"))?;
    if !TOOLS.contains(&name) {
        return Err(Rpc::new(INVALID_PARAMS, format!("unknown tool {name}")));
    }
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(match dispatch(host, agent, name, arguments).await {
        Ok(value) => tool_result(&value.to_string(), false),
        Err(text) => tool_result(&text, true),
    })
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

async fn dispatch<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    match name {
        "list_tasks" => Ok(list_tasks(host)),
        "get_task" => get_task(host, agent, args),
        "post_readback" => post_readback(host, agent, args).await,
        "claim_task" => claim_task(host, agent, args).await,
        "heartbeat_task" => heartbeat_task(host, agent, args).await,
        "release_task" => release_task(host, agent, args).await,
        "declare_changes" => declare_changes(host, agent, args).await,
        "report_built" => report_built(host, agent, args).await,
        "get_worktree" => get_worktree(host, agent, args).await,
        "get_scope" => {
            let request: GetScopeRequest = intent_args(args)?;
            intent_answer(intent_api::get_scope(host, &request))
        }
        "get_rulings" => {
            let request: GetRulingsRequest = intent_args(args)?;
            intent_answer(intent_api::get_rulings(host, &request))
        }
        "get_receipt" => {
            let request: GetReceiptRequest = intent_args(args)?;
            intent_answer(intent_api::get_receipt(host, &request))
        }
        "post_scoped_readback" => {
            let request: PostReadbackRequest = intent_args(args)?;
            intent_answer(intent_api::post_scoped_readback(host, agent, &request).await)
        }
        "file_contest" => {
            let request: ContestRequest = intent_args(args)?;
            intent_answer(intent_api::file_contest(host, agent, &request).await)
        }
        other => Err(format!("unknown tool {other}")),
    }
}

/// Decodes a scope-first tool's arguments. A request that does not decode gets the same typed
/// envelope as every other scope-first refusal.
fn intent_args<T: DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|err| {
        let detail = err.to_string();
        api_message(ApiError::Refused(McpError::new(
            McpErrorCode::InvalidRequest {
                detail: detail.clone(),
            },
            format!("invalid arguments: {detail}"),
        )))
    })
}

/// The answer of one scope-first call as the tool result. A refusal is its typed envelope, so the
/// agent can branch on the closed code.
fn intent_answer<T: Serialize>(result: IntentResult<T>) -> Result<Value, String> {
    let answer = result.map_err(|err| api_message(ApiError::from(err)))?;
    serde_json::to_value(answer).map_err(|err| api_message(ApiError::Internal(err.into())))
}

#[derive(Deserialize)]
struct TaskArgs {
    task: String,
}

#[derive(Deserialize)]
struct FenceArgs {
    task: String,
    generation: u32,
}

impl FenceArgs {
    fn fence(&self) -> Result<Fence, String> {
        Ok(Fence {
            task: task_arg(&self.task)?,
            generation: Generation::new(self.generation),
        })
    }
}

#[derive(Deserialize)]
struct ReadbackArgs {
    task: String,
    summary: String,
    #[serde(default)]
    touches: Vec<Locus>,
}

#[derive(Deserialize)]
struct DeclareArgs {
    task: String,
    generation: u32,
    #[serde(default)]
    files: Vec<FilePath>,
    #[serde(default)]
    symbols: Vec<SymbolRef>,
    #[serde(default)]
    interfaces: Vec<SymbolRef>,
    #[serde(default)]
    inserts: Vec<FilePath>,
}

#[derive(Deserialize)]
struct ReportArgs {
    task: String,
    generation: u32,
    head: String,
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|err| format!("invalid arguments: {err}"))
}

fn task_arg(raw: &str) -> Result<TaskId, String> {
    TaskId::from_str(raw).map_err(|_| format!("{raw:?} is not a task id such as t-1"))
}

/// Sends one agent command through the writer. A refused command becomes its rejection text.
async fn submit<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    cmd: AgentCmd,
) -> Result<Vec<Stamped>, String> {
    host.submit(Command::Agent {
        agent: agent.clone(),
        cmd,
    })
    .await
    .map_err(|err| match err {
        SubmitError::Rejected(rejection) => rejection.to_string(),
        SubmitError::Internal(err) => {
            tracing::error!(error = ?err, "an agent command failed to persist");
            "internal error".to_owned()
        }
    })
}

fn list_tasks<C: ModelClient>(host: &Host<C>) -> Value {
    host.read(|world| {
        let rows: Vec<Value> = world
            .orders()
            .flat_map(|order| world.tasks_of(order.id))
            .filter(|task| matches!(task.state, TaskState::Open | TaskState::Claimed { .. }))
            .map(task_row)
            .collect();
        Value::Array(rows)
    })
}

fn get_task<C: ModelClient>(host: &Host<C>, agent: &AgentId, args: Value) -> Result<Value, String> {
    let TaskArgs { task } = parse(args)?;
    let task = task_arg(&task)?;
    host.read(|world| {
        let found = world.task(task).ok_or_else(|| format!("no task {task}"))?;
        let order = world.order(found.order);
        let mut row = task_row(found);
        let readbacks: Vec<Value> = world
            .readbacks_of(task)
            .map(|readback| {
                json!({
                    "readback": readback.id.to_string(),
                    "agent": readback.agent.to_string(),
                    "status": readback_status(readback, order),
                })
            })
            .collect();
        let your_readback = world
            .readbacks_of(task)
            .filter(|readback| &readback.agent == agent)
            .last()
            .map(|readback| {
                json!({
                    "readback": readback.id.to_string(),
                    "status": readback_status(readback, order),
                })
            });
        let your_readback_matched = world.readbacks_of(task).any(|readback| {
            &readback.agent == agent && readback_status(readback, order) == "match"
        });
        let clashes: Vec<Value> = world.clashes_for(task).map(clash_json).collect();
        row["you_hold"] = json!(found_holder(found).is_some_and(|holder| holder == agent));
        row["your_readback"] = json!(your_readback);
        row["your_readback_matched"] = json!(your_readback_matched);
        row["readbacks"] = json!(readbacks);
        row["clashes"] = json!(clashes);
        Ok(row)
    })
}

/// The status of a readback against the order's current witnesses.
fn readback_status(readback: &Readback, order: Option<&Order>) -> &'static str {
    match (&readback.verdict, order) {
        (None, _) => "pending",
        (Some(verdict), Some(order)) if verdict.matches(order) => "match",
        _ => "not_matched",
    }
}

fn found_holder(task: &Task) -> Option<&AgentId> {
    match &task.state {
        TaskState::Claimed { claim, .. } => Some(&claim.lease.holder),
        _ => None,
    }
}

fn task_row(task: &Task) -> Value {
    let (state, stage, holder, generation): (&str, Option<&str>, Option<String>, Option<u32>) =
        match &task.state {
            TaskState::Open => ("open", None, None, None),
            TaskState::Claimed { claim, stage } => (
                "claimed",
                Some(stage_name(stage)),
                Some(claim.lease.holder.to_string()),
                Some(claim.lease.generation.get()),
            ),
            TaskState::Queued { .. } => ("queued", None, None, None),
            TaskState::Landed { .. } => ("landed", None, None, None),
            TaskState::Dropped => ("dropped", None, None, None),
        };
    json!({
        "task": task.id.to_string(),
        "order": task.order.to_string(),
        "title": task.spec.title,
        "brief": task.spec.brief,
        "scope": task.spec.scope,
        "state": state,
        "stage": stage,
        "holder": holder,
        "generation": generation,
    })
}

fn stage_name(stage: &Stage) -> &'static str {
    match stage {
        Stage::Declaring => "declaring",
        Stage::Declared { .. } => "declared",
        Stage::Building { .. } => "building",
        Stage::Gating { .. } => "gating",
    }
}

fn clash_json(clash: &Clash) -> Value {
    json!({
        "clash": clash.id.to_string(),
        "kind": clash.kind,
        "a": clash.key.a().to_string(),
        "b": clash.key.b().to_string(),
        "at": clash.key.at(),
        "state": clash.state,
    })
}

fn task_snapshot<C: ModelClient>(host: &Host<C>, task: TaskId) -> Result<Value, String> {
    host.read(|world| {
        world
            .task(task)
            .map(task_row)
            .ok_or_else(|| format!("no task {task}"))
    })
}

async fn post_readback<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    args: Value,
) -> Result<Value, String> {
    let args: ReadbackArgs = parse(args)?;
    let task = task_arg(&args.task)?;
    let events = submit(
        host,
        agent,
        AgentCmd::PostReadback {
            task,
            summary: args.summary,
            touches: args.touches,
        },
    )
    .await?;
    let readback = events.iter().find_map(|stamped| match &stamped.event {
        Event::ReadbackPosted { readback } => Some(readback.id.to_string()),
        _ => None,
    });
    Ok(json!({ "task": task.to_string(), "readback": readback }))
}

async fn claim_task<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    args: Value,
) -> Result<Value, String> {
    let TaskArgs { task } = parse(args)?;
    let task = task_arg(&task)?;
    submit(host, agent, AgentCmd::Claim { task }).await?;
    host.read(|world| {
        let claim = held_claim(world, task, agent)?;
        Ok(json!({
            "task": task.to_string(),
            "generation": claim.lease.generation.get(),
            "readback": claim.readback.to_string(),
            "expires_at": claim.lease.expires_at.as_millis(),
        }))
    })
}

/// The claim `agent` holds on `task`.
fn held_claim<'w>(world: &'w World, task: TaskId, agent: &AgentId) -> Result<&'w Claim, String> {
    match world.task(task).map(|found| &found.state) {
        Some(TaskState::Claimed { claim, .. }) if &claim.lease.holder == agent => Ok(claim),
        Some(_) => Err(format!("task {task} is not claimed by you")),
        None => Err(format!("no task {task}")),
    }
}

async fn heartbeat_task<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    args: Value,
) -> Result<Value, String> {
    let args: FenceArgs = parse(args)?;
    let fence = args.fence()?;
    let events = submit(host, agent, AgentCmd::Renew { fence }).await?;
    let expires_at = events.iter().find_map(|stamped| match &stamped.event {
        Event::LeaseRenewed { expires_at, .. } => Some(expires_at.as_millis()),
        _ => None,
    });
    Ok(json!({
        "task": fence.task.to_string(),
        "generation": fence.generation.get(),
        "expires_at": expires_at,
    }))
}

async fn release_task<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    args: Value,
) -> Result<Value, String> {
    let args: FenceArgs = parse(args)?;
    let fence = args.fence()?;
    submit(host, agent, AgentCmd::Release { fence }).await?;
    Ok(json!({
        "task": fence.task.to_string(),
        "generation": fence.generation.get(),
        "released": true,
    }))
}

async fn declare_changes<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    args: Value,
) -> Result<Value, String> {
    let args: DeclareArgs = parse(args)?;
    let fence = Fence {
        task: task_arg(&args.task)?,
        generation: Generation::new(args.generation),
    };
    let footprint = DeclarationInput {
        files: args.files,
        symbols: args.symbols,
        interfaces: args.interfaces,
        inserts: args.inserts,
    };
    let events = submit(host, agent, AgentCmd::Declare { fence, footprint }).await?;
    let clashes: Vec<Value> = events
        .iter()
        .filter_map(|stamped| match &stamped.event {
            Event::ClashFound {
                clash, key, kind, ..
            } => Some(json!({
                "clash": clash.to_string(),
                "kind": kind,
                "a": key.a().to_string(),
                "b": key.b().to_string(),
                "at": key.at(),
            })),
            _ => None,
        })
        .collect();
    let mut row = task_snapshot(host, fence.task)?;
    row["clashes_found"] = json!(clashes);
    Ok(row)
}

async fn report_built<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    args: Value,
) -> Result<Value, String> {
    let args: ReportArgs = parse(args)?;
    let fence = Fence {
        task: task_arg(&args.task)?,
        generation: Generation::new(args.generation),
    };
    let head = CommitId::from_str(&args.head).map_err(|err| format!("invalid commit: {err}"))?;
    let events = submit(host, agent, AgentCmd::ReportBuilt { fence, head }).await?;
    let run = events.iter().find_map(|stamped| match &stamped.event {
        Event::BuildReported { run, .. } => Some(run.to_string()),
        _ => None,
    });
    let mut row = task_snapshot(host, fence.task)?;
    row["run"] = json!(run);
    Ok(row)
}

async fn get_worktree<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    args: Value,
) -> Result<Value, String> {
    let TaskArgs { task } = parse(args)?;
    let task = task_arg(&task)?;
    let view = worktree_of(host, agent, task).await.map_err(api_message)?;
    Ok(json!(view))
}

/// The tool-error text for a failed route. Internal detail goes to the log, not the agent.
fn api_message(err: ApiError) -> String {
    match err {
        ApiError::Unauthorized => "unauthorized".to_owned(),
        ApiError::Forbidden(why) => why.to_owned(),
        ApiError::NotFound(what) => format!("no such {what}"),
        ApiError::BadRequest(what) => format!("bad request: {what}"),
        ApiError::Conflict(why) => format!("conflict: {why}"),
        ApiError::Invalid(fields) => fields
            .iter()
            .map(|field| format!("{}: {}", field.path, field.why))
            .collect::<Vec<_>>()
            .join("; "),
        ApiError::Rejected(rejection) => rejection.to_string(),
        ApiError::Refused(err) => serde_json::to_string(&err)
            .unwrap_or_else(|_| "the refusal did not serialize".to_owned()),
        ApiError::ProjectUnavailable => "the project is unavailable".to_owned(),
        ApiError::Internal(err) => {
            tracing::error!(error = ?err, "an MCP tool failed");
            "internal error".to_owned()
        }
    }
}

fn tool_list() -> Vec<Value> {
    let version = json!({
        "type": "string",
        "enum": ["acan-mcp/1"],
        "description": "Always acan-mcp/1.",
    });
    let machine_ids = json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "The machines to read rulings for. Empty or absent means every machine.",
    });
    let task = json!({ "type": "string", "description": "The task id, such as t-1." });
    let generation = json!({
        "type": "integer",
        "description": "The generation claim_task returned for this task.",
    });
    let fence = json!({
        "type": "object",
        "properties": { "task": task, "generation": generation },
        "required": ["task", "generation"],
    });
    let files = json!({ "type": "array", "items": { "type": "string" } });
    let symbols = json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "file": { "type": "string" },
                "name": { "type": "string" },
            },
            "required": ["file", "name"],
        },
    });
    vec![
        tool(
            "list_tasks",
            "Open and claimed tasks, with their brief and scope. Call this first.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "get_task",
            "One task: its state, your readback status, and the clashes it is party to.",
            json!({
                "type": "object",
                "properties": { "task": task },
                "required": ["task"],
            }),
        ),
        tool(
            "post_readback",
            "Restate what you will do on an open task. The foreman reviews it against the order's witnesses.",
            json!({
                "type": "object",
                "properties": {
                    "task": task,
                    "summary": { "type": "string" },
                    "touches": {
                        "type": "array",
                        "description": "Places you expect to work, such as {\"at\": \"machine\", \"machine\": \"model\"}.",
                        "items": { "type": "object" },
                    },
                },
                "required": ["task", "summary"],
            }),
        ),
        tool(
            "claim_task",
            "Claim a task whose readback matched. Returns the generation to use in every later call.",
            json!({
                "type": "object",
                "properties": { "task": task },
                "required": ["task"],
            }),
        ),
        tool(
            "heartbeat_task",
            "Renew your lease on a claimed task. Call it well inside the 60 second lease.",
            fence.clone(),
        ),
        tool(
            "release_task",
            "Give a claimed task back to the pool.",
            fence.clone(),
        ),
        tool(
            "declare_changes",
            "Declare the files and symbols you will change, before you write any code. Returns the clashes found.",
            json!({
                "type": "object",
                "properties": {
                    "task": task,
                    "generation": generation,
                    "files": files,
                    "symbols": symbols.clone(),
                    "interfaces": symbols,
                    "inserts": files,
                },
                "required": ["task", "generation"],
            }),
        ),
        tool(
            "report_built",
            "Report the commit you built on a task. Starts its witnesses.",
            json!({
                "type": "object",
                "properties": {
                    "task": task,
                    "generation": generation,
                    "head": { "type": "string", "description": "The commit id to test." },
                },
                "required": ["task", "generation", "head"],
            }),
        ),
        tool(
            "get_worktree",
            "The path, branch and protected-head snapshot of the worktree for a task you hold.",
            json!({
                "type": "object",
                "properties": { "task": task },
                "required": ["task"],
            }),
        ),
        tool(
            "get_scope",
            "The confirmed scope of your task's order, with the hash a readback must echo. Refused until a human confirms the scope.",
            intent_schema(&version, &[("task", task.clone())], &[]),
        ),
        tool(
            "get_rulings",
            "The rulings in force on the named machines, or on every machine when none is named. Read them before you build.",
            intent_schema(
                &version,
                &[("task", task.clone()), ("machine_ids", machine_ids)],
                &[],
            ),
        ),
        tool(
            "get_receipt",
            "The latest receipt of your task, with its hash. Refused until a gate run of the task has one.",
            intent_schema(&version, &[("task", task.clone())], &[]),
        ),
        tool(
            "post_scoped_readback",
            "Read back a scope-first task at the confirmed revision and hash. Refused while the scope is unconfirmed or has moved. The acknowledgement authorizes only the claim the confirmed scope already allows.",
            intent_schema(
                &version,
                &[
                    ("task", task.clone()),
                    (
                        "scope_rev",
                        json!({ "type": "integer", "description": "The scope revision get_scope returned." }),
                    ),
                    (
                        "scope_hash",
                        json!({ "type": "string", "description": "The scope hash get_scope returned." }),
                    ),
                    (
                        "summary",
                        json!({ "type": "string", "description": "Your plan, in your words." }),
                    ),
                    (
                        "predictions",
                        json!({
                            "type": "array",
                            "description": "What your change will do: each item is a selector and a predicted value. Scored against the receipt, never evidence.",
                            "items": { "type": "object" },
                        }),
                    ),
                ],
                &["scope_rev", "scope_hash", "summary", "predictions"],
            ),
        ),
        tool(
            "file_contest",
            "Object to one discrepancy episode of your task's order, with a reproducer. The contest asks the human and never changes the scope. Only the agent holding the task may file.",
            intent_schema(
                &version,
                &[
                    ("task", task.clone()),
                    (
                        "episode_id",
                        json!({ "type": "string", "description": "The episode id, such as sha256:..." }),
                    ),
                    (
                        "kind",
                        json!({
                            "type": "string",
                            "enum": ["blocked", "spec_conflict", "proposed_change"],
                        }),
                    ),
                    (
                        "reproducer",
                        json!({ "type": "string", "description": "The probe hash that shows the problem." }),
                    ),
                    (
                        "rationale",
                        json!({ "type": "string", "description": "Why, in your words." }),
                    ),
                ],
                &["episode_id", "kind", "reproducer", "rationale"],
            ),
        ),
    ]
}

/// The input schema of a scope-first tool: the contract version, then `properties`. `task` is
/// always required; `required` names the rest.
fn intent_schema(version: &Value, properties: &[(&str, Value)], required: &[&str]) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("version".to_owned(), version.clone());
    for (name, schema) in properties {
        map.insert((*name).to_owned(), schema.clone());
    }
    let mut needed = vec!["version", "task"];
    needed.extend_from_slice(required);
    json!({
        "type": "object",
        "properties": map,
        "required": needed,
    })
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}
