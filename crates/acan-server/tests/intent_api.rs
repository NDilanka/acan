//! The scope-first lane over the real server. Agents reach it over MCP, and members reach the
//! reads and the human commands over HTTP.
//!
//! Each test boots a server on a temporary repository with the offline models, so the job loop
//! splits the order into tasks, and every command runs through acan-core's origin. The tokens file
//! is written before the server starts: an owner token for the member `owner`, and an agent token
//! for `agent-1`, which the owner registers.

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use acan_core::{Event, Stamped};
use acan_intent::Scope;
use acan_server::{DEFAULT_WITNESS_CONCURRENCY, Models, Running, ServerConfig, start};
use reqwest::StatusCode;
use serde_json::{Value, json};

const OWNER_TOKEN: &str = "acan_owner_test";
const AGENT_TOKEN: &str = "acan_agent_test";
const AGENT: &str = "agent-1";
/// The commit every scope binds to. The core checks it against the scope, not against the repo.
const BASE: &str = "0123456789abcdef0123456789abcdef01234567";
const SENTENCE: &str = "split the store into a reader and a writer";
const CONFIRMED_AT: u64 = 1_700_000_000_000;
/// The JSON-RPC code for a call to a tool the endpoint does not have.
const INVALID_PARAMS: i64 = -32602;
const WAIT: Duration = Duration::from_secs(10);

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("model")).expect("model dir");
    fs::write(
        dir.path().join("model/store.ts"),
        "export function load() {\n  return 1;\n}\n",
    )
    .expect("write fixture");
    dir
}

/// Writes the tokens file the server reads at startup: the owner, and the agent that acts for it.
fn write_tokens(repo: &Path) {
    let state = repo.join(".acan");
    fs::create_dir_all(&state).expect("the state dir");
    let tokens = json!([
        { "token": OWNER_TOKEN, "principal": { "kind": "member", "id": "owner" } },
        { "token": AGENT_TOKEN, "principal": { "kind": "agent", "id": AGENT } },
    ]);
    fs::write(state.join("tokens.json"), tokens.to_string()).expect("the tokens file");
}

async fn boot(repo: &Path) -> Running {
    write_tokens(repo);
    let models = Models::load(&repo.join(".acan/llm.toml"))
        .expect("a missing routing file selects the offline models");
    let ledger = models.ledger();
    let config = ServerConfig {
        repo: repo.to_path_buf(),
        addr: loopback(),
        team: false,
        web: None,
        protected_branch: None,
        witness_http_base_url: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
    };
    start(config, models, ledger)
        .await
        .expect("the server starts")
}

fn url(running: &Running, path: &str) -> String {
    format!("http://{}{path}", running.local_addr)
}

/// The status and the body. A body that is not JSON, such as an extractor's rejection text, is
/// kept as a string so the assertion that fails shows it.
async fn answer(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, body)
}

async fn get_json(running: &Running, path: &str, token: &str) -> (StatusCode, Value) {
    let response = client()
        .get(url(running, path))
        .bearer_auth(token)
        .send()
        .await
        .expect("the request is sent");
    answer(response).await
}

async fn post_command(running: &Running, token: &str, cmd: &Value) -> (StatusCode, Value) {
    let response = client()
        .post(url(running, "/api/commands"))
        .bearer_auth(token)
        .json(cmd)
        .send()
        .await
        .expect("the command request is sent");
    answer(response).await
}

/// One JSON-RPC call to the MCP endpoint. Every call here is a message the endpoint answers.
async fn rpc(running: &Running, token: &str, method: &str, params: Value) -> Value {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let response = client()
        .post(url(running, "/mcp"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("the MCP call is sent");
    assert!(response.status().is_success(), "the endpoint answers");
    response.json().await.expect("the reply is JSON-RPC")
}

/// Calls one tool. Returns whether the tool failed, and its answer parsed from the result text.
async fn tool(running: &Running, token: &str, name: &str, arguments: Value) -> (bool, Value) {
    let reply = rpc(
        running,
        token,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await;
    let result = &reply["result"];
    let is_error = result["isError"]
        .as_bool()
        .expect("the result says whether the tool failed");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("the result has its text");
    (
        is_error,
        serde_json::from_str(text).expect("the text is JSON"),
    )
}

/// The closed code of a typed refusal.
fn error_code(refusal: &Value) -> &str {
    refusal["error"]["code"]
        .as_str()
        .expect("a refusal carries a closed code")
}

async fn event_count(running: &Running) -> usize {
    let (status, body) = get_json(running, "/api/events?from=0", OWNER_TOKEN).await;
    assert_eq!(status, StatusCode::OK);
    body.as_array().expect("the events are a list").len()
}

/// A scope on `order` at revision `rev`, with one must-keep rule over the signup answer's body.
fn scope_json(order: &str, rev: u32) -> Value {
    json!({
        "version": "acan-scope/1",
        "order": order,
        "rev": rev,
        "base_commit": BASE,
        "body": {
            "may": [],
            "must_keep": [{
                "id": "keep-signup-body",
                "selector": {
                    "surface": "http",
                    "method": "POST",
                    "route_template": "/api/signup",
                    "step_context": { "kind": "step", "step_id": "s1" },
                    "principal": { "kind": "anonymous" },
                    "channel": "body"
                },
                "obligations": [{
                    "obligation_id": "obl-signup-body",
                    "predicate": {
                        "predicate": "equals_base",
                        "step_id": "s1",
                        "path": "body",
                        "anchor": { "kind": "fresh" }
                    },
                    "text": "The signup answer's body matches the base run"
                }],
                "text": "Signup answers with the same body as before the change",
                "machine_ids": ["auth"]
            }],
            "open": []
        }
    })
}

fn scope_of(order: &str, rev: u32) -> Scope {
    serde_json::from_value(scope_json(order, rev)).expect("the scope decodes")
}

/// The scope's hash, as a JSON string.
fn hash_of(scope: &Scope) -> Value {
    json!(scope.scope_hash().expect("the scope hashes"))
}

/// The numeric id of an order. The human commands carry it as a bare number, while the scope and
/// the confirmation spell it `o-N`.
fn order_no(order: &str) -> u64 {
    order
        .strip_prefix("o-")
        .and_then(|n| n.parse().ok())
        .expect("an order id such as o-3")
}

fn revise_cmd(order: &str, rev: u32) -> Value {
    json!({ "cmd": "revise_scope", "order": order_no(order), "scope": scope_json(order, rev) })
}

/// The human confirmation of revision 1: every hash the core binds, and no open choices.
fn confirm_cmd(order: &str) -> Value {
    let scope = scope_of(order, 1);
    let selector_hashes: Vec<Value> = scope
        .body
        .must_keep
        .iter()
        .map(|rule| json!(rule.selector.selector_hash().expect("the selector hashes")))
        .collect();
    json!({
        "cmd": "confirm_scope",
        "order": order_no(order),
        "confirmation": {
            "version": "acan-scope-confirmation/1",
            "order": order,
            "scope_rev": scope.rev,
            "scope_hash": scope.scope_hash().expect("the scope hashes"),
            "semantic_scope_hash": scope.semantic_scope_hash().expect("the scope hashes"),
            "base_commit": scope.base_commit,
            "ruling_versions": [],
            "selector_hashes": selector_hashes,
            "example_hashes": [],
            "answers": [],
            "broad_grants": [],
            "budget": {
                "repair_rounds": 2,
                "wall_time_ms": 600_000,
                "model_cost_micros": 0,
                "max_episodes": 4
            },
            "by": "owner",
            "at": CONFIRMED_AT
        }
    })
}

fn readback(task: &str, rev: u32, hash: &Value, summary: &str) -> Value {
    json!({
        "version": "acan-mcp/1",
        "task": task,
        "scope_rev": rev,
        "scope_hash": hash,
        "summary": summary,
        "predictions": []
    })
}

/// Boots a server, posts the order, registers the agent, and waits until the offline split has
/// made the order's tasks. Returns the server and the order's id, such as `o-1`.
async fn split_order(repo: &Path) -> (Running, String) {
    let running = boot(repo).await;
    let post = json!({
        "cmd": "post_order",
        "machine": "model",
        "sentence": SENTENCE,
        "witnesses": []
    });
    let (status, _) = post_command(&running, OWNER_TOKEN, &post).await;
    assert_eq!(status, StatusCode::OK, "the order is accepted");
    let register = json!({ "cmd": "register_agent", "agent": AGENT });
    let (status, _) = post_command(&running, OWNER_TOKEN, &register).await;
    assert_eq!(status, StatusCode::OK, "the agent is registered");

    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, body) = get_json(&running, "/api/events?from=0", OWNER_TOKEN).await;
        let events: Vec<Stamped> = serde_json::from_value(body).expect("events JSON");
        let split = events.iter().find_map(|stamped| match &stamped.event {
            Event::OrderSplit { order, .. } => Some(order.to_string()),
            _ => None,
        });
        if let Some(order) = split {
            return (running, order);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the offline split never landed: {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The first task of `order`, as the agent lists it over MCP.
async fn first_task(running: &Running, order: &str) -> String {
    let (is_error, rows) = tool(running, AGENT_TOKEN, "list_tasks", json!({})).await;
    assert!(!is_error, "list_tasks answers");
    rows.as_array()
        .expect("the task list")
        .iter()
        .find(|row| row["order"] == order)
        .and_then(|row| row["task"].as_str())
        .expect("the split made a task")
        .to_owned()
}

/// A confirmed scope on an order, with the agent's task. Returns the server, the order, the task
/// and the hash.
async fn confirmed_order(repo: &Path) -> (Running, String, String, Value) {
    let (running, order) = split_order(repo).await;
    let task = first_task(&running, &order).await;
    let (status, body) = post_command(&running, OWNER_TOKEN, &revise_cmd(&order, 1)).await;
    assert_eq!(status, StatusCode::OK, "the poster sets the scope: {body}");
    let (status, body) = post_command(&running, OWNER_TOKEN, &confirm_cmd(&order)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner confirms the scope: {body}"
    );
    let hash = hash_of(&scope_of(&order, 1));
    (running, order, task, hash)
}

#[tokio::test]
async fn a_readback_is_refused_until_a_human_confirms_the_scope() {
    let dir = temp_repo();
    let (running, order) = split_order(dir.path()).await;
    let task = first_task(&running, &order).await;
    let scope = scope_of(&order, 1);
    let (status, body) = post_command(&running, OWNER_TOKEN, &revise_cmd(&order, 1)).await;
    assert_eq!(status, StatusCode::OK, "the poster sets the scope: {body}");

    let (is_error, refusal) = tool(
        &running,
        AGENT_TOKEN,
        "get_scope",
        json!({ "version": "acan-mcp/1", "task": task }),
    )
    .await;
    assert!(is_error, "an unconfirmed scope is not read as confirmed");
    assert_eq!(error_code(&refusal), "scope_not_confirmed");
    assert_eq!(refusal["error"]["order"], json!(order));

    let (is_error, refusal) = tool(
        &running,
        AGENT_TOKEN,
        "post_scoped_readback",
        readback(&task, 1, &hash_of(&scope), "build the reader"),
    )
    .await;
    assert!(
        is_error,
        "a readback against an unconfirmed scope is refused"
    );
    assert_eq!(error_code(&refusal), "scope_not_confirmed");
    assert_eq!(refusal["error"]["order"], json!(order));
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_readback_at_a_wrong_revision_or_hash_is_refused_with_the_current_one() {
    let dir = temp_repo();
    let (running, _, task, hash) = confirmed_order(dir.path()).await;

    let (is_error, scope) = tool(
        &running,
        AGENT_TOKEN,
        "get_scope",
        json!({ "version": "acan-mcp/1", "task": task }),
    )
    .await;
    assert!(!is_error, "the confirmed scope is read");
    assert_eq!(scope["scope_hash"], hash);
    assert_eq!(scope["confirmed_at"], json!(CONFIRMED_AT));
    assert_eq!(scope["scope"]["rev"], json!(1));

    let (is_error, refusal) = tool(
        &running,
        AGENT_TOKEN,
        "post_scoped_readback",
        readback(&task, 2, &hash, "build the reader"),
    )
    .await;
    assert!(is_error, "a wrong revision is refused");
    assert_eq!(error_code(&refusal), "stale_scope");
    assert_eq!(refusal["error"]["current_rev"], json!(1));
    assert_eq!(refusal["error"]["current_hash"], hash);

    let wrong_hash = json!(format!("sha256:{}", "0".repeat(64)));
    let (is_error, refusal) = tool(
        &running,
        AGENT_TOKEN,
        "post_scoped_readback",
        readback(&task, 1, &wrong_hash, "build the reader"),
    )
    .await;
    assert!(is_error, "a wrong hash is refused");
    assert_eq!(error_code(&refusal), "stale_scope");
    assert_eq!(refusal["error"]["current_hash"], hash);

    let mut unversioned = readback(&task, 1, &hash, "build the reader");
    unversioned
        .as_object_mut()
        .expect("a readback is an object")
        .remove("version");
    let (is_error, refusal) =
        tool(&running, AGENT_TOKEN, "post_scoped_readback", unversioned).await;
    assert!(is_error, "a request without its version is refused");
    assert_eq!(error_code(&refusal), "invalid_request");

    let (is_error, ack) = tool(
        &running,
        AGENT_TOKEN,
        "post_scoped_readback",
        readback(&task, 1, &hash, "build the reader"),
    )
    .await;
    assert!(
        !is_error,
        "the confirmed revision and hash are acknowledged"
    );
    assert_eq!(ack["task"], json!(task));
    assert_eq!(ack["scope_rev"], json!(1));
    assert_eq!(ack["scope_hash"], hash);
    assert_eq!(ack["predictions_recorded"], json!(0));
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn a_contest_needs_the_claim_and_a_known_episode() {
    let dir = temp_repo();
    let (running, _, task, hash) = confirmed_order(dir.path()).await;
    let contest = json!({
        "version": "acan-mcp/1",
        "task": task,
        "episode_id": format!("sha256:{}", "1".repeat(64)),
        "kind": "blocked",
        "reproducer": format!("sha256:{}", "2".repeat(64)),
        "rationale": "the scope blocks the reader"
    });

    let (is_error, refusal) = tool(&running, AGENT_TOKEN, "file_contest", contest.clone()).await;
    assert!(is_error, "a contest before the claim is refused");
    assert_eq!(error_code(&refusal), "forbidden");
    assert_eq!(refusal["error"]["action"], json!("file_contest"));

    let (is_error, _) = tool(
        &running,
        AGENT_TOKEN,
        "post_scoped_readback",
        readback(&task, 1, &hash, "build the reader"),
    )
    .await;
    assert!(!is_error, "the readback is acknowledged");
    let (is_error, claimed) =
        tool(&running, AGENT_TOKEN, "claim_task", json!({ "task": task })).await;
    assert!(!is_error, "the acknowledged readback claims the task");
    assert!(claimed["generation"].is_u64());

    let (is_error, refusal) = tool(&running, AGENT_TOKEN, "file_contest", contest).await;
    assert!(is_error, "an episode the order does not have is refused");
    assert_eq!(error_code(&refusal), "not_found");
    assert_eq!(refusal["error"]["what"], json!("episode"));
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn an_agent_token_is_refused_on_every_human_route_and_tool() {
    let dir = temp_repo();
    let (running, order, task, _) = confirmed_order(dir.path()).await;
    let before = event_count(&running).await;

    let revise_again = revise_cmd(&order, 2);
    let confirm_again = confirm_cmd(&order);
    let resolve = json!({ "cmd": "resolve_contest", "contest": "k-1", "status": "upheld" });
    let ruling = json!({
        "cmd": "record_ruling",
        "event": serde_json::from_str::<Value>(include_str!(
            "../../acan-intent/fixtures/ledger-note.json"
        ))
        .expect("the ledger note fixture decodes")
    });
    for body in [&revise_again, &confirm_again, &resolve, &ruling] {
        let (status, refusal) = post_command(&running, AGENT_TOKEN, body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "an agent is refused on the human route for {}",
            body["cmd"]
        );
        assert_eq!(refusal["error"], json!("forbidden"));
    }

    for name in [
        "revise_scope",
        "confirm_scope",
        "resolve_contest",
        "record_ruling",
    ] {
        let reply = rpc(
            &running,
            AGENT_TOKEN,
            "tools/call",
            json!({ "name": name, "arguments": {} }),
        )
        .await;
        assert_eq!(
            reply["error"]["code"],
            json!(INVALID_PARAMS),
            "{name} is not a tool an agent can call"
        );
    }

    assert_eq!(
        event_count(&running).await,
        before,
        "no refused call changed the log"
    );
    let (status, scope) =
        get_json(&running, &format!("/api/tasks/{task}/scope"), OWNER_TOKEN).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        scope["scope"]["rev"],
        json!(1),
        "the agent's revision never landed"
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn members_read_the_scope_rulings_and_receipt_over_http() {
    let dir = temp_repo();
    let (running, _, task, hash) = confirmed_order(dir.path()).await;

    let (status, scope) =
        get_json(&running, &format!("/api/tasks/{task}/scope"), OWNER_TOKEN).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(scope["scope_hash"], hash);
    assert_eq!(scope["confirmed_at"], json!(CONFIRMED_AT));
    assert_eq!(scope["budget"]["max_episodes"], json!(4));

    let (status, rulings) =
        get_json(&running, &format!("/api/tasks/{task}/rulings"), OWNER_TOKEN).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rulings["rulings"], json!([]));

    let (status, filtered) = get_json(
        &running,
        &format!("/api/tasks/{task}/rulings?machines=auth,store"),
        OWNER_TOKEN,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(filtered["rulings"], json!([]));

    let (status, refusal) = get_json(
        &running,
        &format!("/api/tasks/{task}/rulings?machines=not%20an%20id"),
        OWNER_TOKEN,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&refusal), "invalid_request");

    let (status, refusal) =
        get_json(&running, &format!("/api/tasks/{task}/receipt"), OWNER_TOKEN).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&refusal), "not_found");
    assert_eq!(refusal["error"]["what"], json!("receipt"));

    let (status, refusal) = get_json(&running, "/api/tasks/t-99/scope", OWNER_TOKEN).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&refusal), "not_found");

    let (status, _) = get_json(
        &running,
        &format!("/api/tasks/{task}/scope"),
        "acan_unknown",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an unknown token is refused"
    );

    let (is_error, rulings) = tool(
        &running,
        AGENT_TOKEN,
        "get_rulings",
        json!({ "version": "acan-mcp/1", "task": task, "machine_ids": [] }),
    )
    .await;
    assert!(!is_error, "the agent reads the same rulings over MCP");
    assert_eq!(rulings["rulings"], json!([]));

    let (is_error, refusal) = tool(
        &running,
        AGENT_TOKEN,
        "get_receipt",
        json!({ "version": "acan-mcp/1", "task": task }),
    )
    .await;
    assert!(is_error, "a task with no gate run has no receipt");
    assert_eq!(error_code(&refusal), "not_found");
    assert_eq!(refusal["error"]["what"], json!("receipt"));
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn tools_list_offers_the_fourteen_tools_and_no_human_scope_tool() {
    let dir = temp_repo();
    let running = boot(dir.path()).await;
    let reply = rpc(&running, AGENT_TOKEN, "tools/list", json!({})).await;
    let mut names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .expect("the tool list")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a tool has a name"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "claim_task",
            "declare_changes",
            "file_contest",
            "get_receipt",
            "get_rulings",
            "get_scope",
            "get_task",
            "get_worktree",
            "heartbeat_task",
            "list_tasks",
            "post_readback",
            "post_scoped_readback",
            "release_task",
            "report_built",
        ]
    );
    running.stop().await.expect("the server stops");
}

#[tokio::test]
async fn the_plant_view_is_for_members_and_names_what_an_order_lacks() {
    let dir = temp_repo();
    let (running, order, _, _) = confirmed_order(dir.path()).await;
    let path = format!("/api/orders/{order}/plant-view");

    let response = client()
        .get(url(&running, &path))
        .send()
        .await
        .expect("the request is sent");
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no token is refused");
    assert_eq!(body, json!({ "error": "unauthorized" }));

    let (status, _) = get_json(&running, &path, "acan_unknown").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an unknown token is refused"
    );

    let (status, body) = get_json(&running, &path, AGENT_TOKEN).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "an agent is refused");
    assert_eq!(body["error"], json!("forbidden"));

    // The order is confirmed, but no gate run has produced a receipt for its scope.
    let (status, body) = get_json(&running, &path, OWNER_TOKEN).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({ "error": "not_found", "what": "no receipt for this order yet" })
    );

    let (status, body) = get_json(&running, "/api/orders/o-99/plant-view", OWNER_TOKEN).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "error": "not_found", "what": "order" }));

    let (status, body) =
        get_json(&running, "/api/orders/not-an-order/plant-view", OWNER_TOKEN).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "error": "not_found", "what": "order" }));
    running.stop().await.expect("the server stops");
}
