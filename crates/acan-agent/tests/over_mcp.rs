//! The harness over MCP against a real origin on the scope-first lane. The origin, its writer, its
//! git worktrees, and its gate are real. The foreman and the workers are scripted, so no test
//! needs the network.
//!
//! The owner confirms the order's scope before any worker claims. A worker reads that scope over
//! MCP, posts a readback that carries it, and only then claims. The legacy model review is not
//! scripted anywhere: the lane never derives it.
//!
//! Each worker's first edit waits on a gate the test opens after it has seen the clash. A real
//! model call takes longer than a second declaration, so the gate stands in for that delay. The
//! worker's own write still waits for build clearance, which the origin withholds while the
//! clash is open.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::str::FromStr;
use std::time::Duration;

use acan_agent::{Exit, McpClient, Outcome, Reply, WorkerConfig, run};
use acan_core::{
    AgentId, Check, ClashId, Event, Generation, HumanCmd, MachineKey, Origin, Rejection,
    Settlement, Stage, Stamped, TaskId, TaskState, WitnessSpec, World,
};
use acan_llm::{
    CallRecord, ChatRequest, ChatResponse, FunctionCall, LlmError, Message, ModelClient,
    Reply as StubReply, Role, Stub, ToolCall, ToolKind, Usage, UsageLedger,
};
use acan_server::{DEFAULT_WITNESS_CONCURRENCY, Running, ServerConfig, start};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

const OWNER_TOKEN: &str = "acan_owner_over_mcp";
const ONE_TOKEN: &str = "acan_agent_one_over_mcp";
const TWO_TOKEN: &str = "acan_agent_two_over_mcp";
const ONE: &str = "agent-1";
const TWO: &str = "agent-2";
const MACHINE: &str = "model";
const FILE: &str = "model/store.ts";
const ORIGINAL: &str = "export function load() { return 1; }\n";
const CHANGED: &str = "export function load() { return 2; }\n";
/// A second file on the same machine. A narrowed task moves its change here.
const OTHER: &str = "model/other.ts";
const OTHER_ORIGINAL: &str = "export const other = 1;\n";
const OTHER_CHANGED: &str = "export const other = 2;\n";
/// Passes when the built commit carries the file, so the gate checks the commit, not the disk.
const WITNESS: &str = "git cat-file -e HEAD:model/store.ts";
/// The longest one step of a scenario may take. A step spans git work in the origin and the
/// worker, which runs several times slower on a loaded host, so the bound is generous. A stalled
/// step still fails, and the panic names the step and what the origin showed.
const WAIT: Duration = Duration::from_secs(120);
/// How long a worker waits for a claim, a verdict, or build clearance. A worker that gives up
/// releases its claim, and the origin then dissolves a clash the scenario is still holding open
/// while it waits on worktrees, so this outlasts the scenario's own waits.
const WORKER_WAIT: Duration = Duration::from_secs(600);
const POLL: Duration = Duration::from_millis(20);
/// How long a worker waits for a gate receipt after it reports its build. The harness runs no
/// gate that answers, so the wait ends and the build stays reported.
const RECEIPT_WAIT: Duration = Duration::from_millis(500);
/// How long the test lets the workers run after the gate opens, to show nothing is cleared.
const HOLD: Duration = Duration::from_millis(300);
/// The confirmation's acceptance time. The core binds the confirmation to its scope, not to a clock.
const CONFIRMED_AT: u64 = 1_700_000_000_000;
/// The scenarios run one at a time. Each starts an origin and its own git work, so in parallel
/// they slow each other's git calls, and the deadlines would then measure that contention.
static SCENARIO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The foreman answers by the shape of its request: an explanation carries a clash, and a split
/// carries a machine. It splits the order into `tasks` tasks on one machine, at most two, and
/// explains a clash without a suggestion. It never reviews readbacks: the scope-first lane has no
/// review request, so a request of that shape is refused.
struct Foreman {
    tasks: usize,
}

impl ModelClient for Foreman {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let context: Value = request
            .last_user_text()
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or(LlmError::NoScriptedReply { role })?;
        let reply = if context.get("clash").is_some() {
            explain()
        } else if context.get("machine").is_some() {
            split(self.tasks)
        } else {
            return Err(LlmError::NoScriptedReply { role });
        };
        Ok(answer(role, reply.to_string()))
    }
}

fn answer(role: Role, content: String) -> ChatResponse {
    ChatResponse {
        content: Some(content),
        tool_calls: Vec::new(),
        finish_reason: Some("stop".to_owned()),
        call: CallRecord {
            role,
            provider: "scripted".to_owned(),
            model: "foreman".to_owned(),
            usage: Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
            },
            latency: Duration::ZERO,
        },
    }
}

fn split(tasks: usize) -> Value {
    let task = |title: &str| {
        json!({
            "title": title,
            "brief": format!("{title} in {FILE}"),
            "scope": [MACHINE],
        })
    };
    let all = [task("Change the loader"), task("Change the loader again")];
    json!({ "tasks": all[..tasks] })
}

fn explain() -> Value {
    json!({
        "consequence": "Both tasks change the same file.",
        "breaks": null,
        "suggested": null,
    })
}

/// A worker model. Plans answer at once. An edit waits on the gate first, so the test decides
/// when the worker may try to write.
struct Worker {
    script: Stub,
    gate: watch::Receiver<bool>,
}

impl Worker {
    fn new(gate: watch::Receiver<bool>, script: Stub) -> Self {
        Self { script, gate }
    }
}

impl ModelClient for Worker {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        if !request.tools.is_empty() {
            let mut gate = self.gate.clone();
            // The sender lives until the test ends, so a closed channel only means the test is over.
            let _ = gate.wait_for(|open| *open).await;
        }
        self.script.chat(role, request).await
    }
}

/// The worker's script: plan the declared file, write it, commit it, then stop.
fn worker_script() -> Stub {
    worker_script_planning(&[FILE])
}

/// The worker's script with the files it plans: it writes `FILE`, commits it, then stops.
fn worker_script_planning(files: &[&str]) -> Stub {
    Stub::default()
        .when(
            Role::Worker,
            |request| request.tools.is_empty(),
            StubReply::json(&json!({ "summary": "change the loader", "files": files })),
        )
        .when(
            Role::Worker,
            |request| tool_replies(request) == 0,
            StubReply {
                tool_calls: vec![tool_call(
                    "write_file",
                    json!({ "path": FILE, "content": CHANGED }),
                )],
                ..StubReply::default()
            },
        )
        .when(
            Role::Worker,
            |request| tool_replies(request) == 1,
            StubReply {
                tool_calls: vec![tool_call(
                    "commit",
                    json!({ "message": "change the loader" }),
                )],
                ..StubReply::default()
            },
        )
        .when(
            Role::Worker,
            |_| true,
            StubReply::text("the change is committed"),
        )
}

fn tool_replies(request: &ChatRequest) -> usize {
    request
        .messages
        .iter()
        .filter(|message| matches!(message, Message::Tool { .. }))
        .count()
}

fn tool_call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        kind: ToolKind::Function,
        function: FunctionCall {
            name: name.to_owned(),
            arguments: arguments.to_string(),
        },
    }
}

fn worker_config(base: &str, token: &str, repo: &Path) -> WorkerConfig {
    WorkerConfig {
        server: base.to_owned(),
        token: token.to_owned(),
        repo: repo.to_path_buf(),
        test_command: None,
        max_steps: 5,
        wait: WORKER_WAIT,
        receipt_wait: RECEIPT_WAIT,
        heartbeat: Duration::from_secs(1),
        poll: POLL,
    }
}

fn spawn_worker(
    base: &str,
    token: &'static str,
    repo: &Path,
    gate: watch::Receiver<bool>,
) -> JoinHandle<Outcome> {
    spawn_scripted(base, token, repo, worker_script(), gate)
}

fn spawn_scripted(
    base: &str,
    token: &'static str,
    repo: &Path,
    script: Stub,
    gate: watch::Receiver<bool>,
) -> JoinHandle<Outcome> {
    let config = worker_config(base, token, repo);
    tokio::spawn(async move {
        let model = Worker::new(gate, script);
        run(&config, &model).await
    })
}

/// A worker whose first plan names `FILE`. Once a narrowing has taken the file, the planning
/// call carries that and the worker plans `after` instead. Its edit writes `OTHER` and commits.
fn narrowed_script(after: Value) -> Stub {
    let plans = Stub::default()
        .when(
            Role::Worker,
            |request| {
                request.tools.is_empty()
                    && request
                        .last_user_text()
                        .is_some_and(|text| text.contains("gave up these places"))
            },
            StubReply::json(&after),
        )
        .when(
            Role::Worker,
            |request| request.tools.is_empty(),
            StubReply::json(&json!({ "summary": "change the loader", "files": [FILE] })),
        );
    writes_other(plans)
}

/// A worker whose plans follow the places it gave up. Once a narrowing takes `FILE` it plans
/// `OTHER`. Once a second narrowing takes `OTHER` as well, it plans `FILE` again, which the harness
/// must refuse. Its edit writes `OTHER` and commits.
fn twice_narrowed_script() -> Stub {
    let plans = Stub::default()
        .when(
            Role::Worker,
            |request| request.tools.is_empty() && gave_up(request, OTHER),
            StubReply::json(&json!({ "summary": "change the loader", "files": [FILE] })),
        )
        .when(
            Role::Worker,
            |request| request.tools.is_empty() && gave_up(request, FILE),
            StubReply::json(&json!({ "summary": "change the other file", "files": [OTHER] })),
        )
        .when(
            Role::Worker,
            |request| request.tools.is_empty(),
            StubReply::json(&json!({ "summary": "change the loader", "files": [FILE] })),
        );
    writes_other(plans)
}

/// Whether a planning call lists `file` among the places the task gave up.
fn gave_up(request: &ChatRequest, file: &str) -> bool {
    request.last_user_text().is_some_and(|text| {
        text.contains("gave up these places") && text.contains(&format!("- {file}:"))
    })
}

/// Adds the edit to a worker's plans: it writes `OTHER`, commits it, then stops.
fn writes_other(plans: Stub) -> Stub {
    plans
        .when(
            Role::Worker,
            |request| tool_replies(request) == 0,
            StubReply {
                tool_calls: vec![tool_call(
                    "write_file",
                    json!({ "path": OTHER, "content": OTHER_CHANGED }),
                )],
                ..StubReply::default()
            },
        )
        .when(
            Role::Worker,
            |request| tool_replies(request) == 1,
            StubReply {
                tool_calls: vec![tool_call(
                    "commit",
                    json!({ "message": "change the other file" }),
                )],
                ..StubReply::default()
            },
        )
        .when(
            Role::Worker,
            |_| true,
            StubReply::text("the change is committed"),
        )
}

/// A repository with `main` as the protected branch and the user's checkout on `user-work`.
fn user_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temporary repository");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test User"]);
    git(root, &["config", "user.email", "test@example.test"]);
    // Windows checkouts convert to CRLF by default, and the file is compared byte for byte.
    git(root, &["config", "core.autocrlf", "false"]);
    fs::create_dir_all(root.join(MACHINE)).expect("machine directory");
    fs::write(root.join(FILE), ORIGINAL).expect("fixture");
    fs::write(root.join(OTHER), OTHER_ORIGINAL).expect("fixture");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial"]);
    git(root, &["switch", "-q", "-c", "user-work"]);
    dir
}

/// Runs git in `dir` and returns its stdout. Panics with git's stderr when the command fails.
/// A lock held by the server's own git is contention, so it is retried.
fn git(dir: &Path, args: &[&str]) -> String {
    for attempt in 1..=5 {
        let output = Process::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        if output.status.success() {
            return String::from_utf8(output.stdout).expect("git prints UTF-8");
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("index.lock") || attempt == 5 {
            panic!("git {args:?} failed: {stderr}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unreachable!("the loop returns or panics")
}

fn worktree_dir(root: &Path, task: TaskId) -> PathBuf {
    root.join(".acan").join("worktrees").join(task.to_string())
}

/// Writes the token file before the first start, so the test holds an owner and two agents.
fn write_tokens(root: &Path) {
    let state = root.join(".acan");
    fs::create_dir_all(&state).expect("state directory");
    fs::write(
        state.join("tokens.json"),
        format!(
            r#"[
              {{"token": "{OWNER_TOKEN}", "principal": {{"kind": "member", "id": "owner"}}}},
              {{"token": "{ONE_TOKEN}", "principal": {{"kind": "agent", "id": "{ONE}"}}}},
              {{"token": "{TWO_TOKEN}", "principal": {{"kind": "agent", "id": "{TWO}"}}}}
            ]"#
        ),
    )
    .expect("tokens file");
}

async fn start_origin(root: &Path, tasks: usize) -> Running {
    write_tokens(root);
    let config = ServerConfig {
        repo: root.to_path_buf(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        team: false,
        web: None,
        protected_branch: Some("main".to_owned()),
        witness_http_base_url: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: None,
    };
    start(config, Foreman { tasks }, UsageLedger::default())
        .await
        .expect("the origin starts on the test repository")
}

fn base_url(running: &Running) -> String {
    format!("http://{}", running.local_addr)
}

fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Sends one member command over HTTP as the owner.
async fn owner_command(running: &Running, cmd: &HumanCmd) -> reqwest::Response {
    client()
        .post(format!("{}/api/commands", base_url(running)))
        .bearer_auth(OWNER_TOKEN)
        .json(cmd)
        .send()
        .await
        .expect("the command is sent")
}

async fn post_order(running: &Running) {
    let response = owner_command(
        running,
        &HumanCmd::PostOrder {
            machine: MachineKey::from_str(MACHINE).expect("valid machine key"),
            sentence: "change the loader".to_owned(),
            witnesses: vec![WitnessSpec {
                check: Check::Test {
                    command: WITNESS.to_owned(),
                },
                reason: "the built commit carries the change".to_owned(),
            }],
        },
    )
    .await;
    assert!(
        response.status().is_success(),
        "the order is posted: {}",
        response.status()
    );
}

async fn register(running: &Running, agent: &str) {
    let response = owner_command(
        running,
        &HumanCmd::RegisterAgent {
            agent: AgentId::from_str(agent).expect("valid agent id"),
        },
    )
    .await;
    assert!(
        response.status().is_success(),
        "{agent} is registered: {}",
        response.status()
    );
}

/// The protected head the scope binds to: `main`, where the repository starts.
fn base_commit(root: &Path) -> String {
    git(root, &["rev-parse", "main"]).trim().to_owned()
}

/// The order the split made, as the scope names it, such as `o-1`.
async fn split_order_id(running: &Running) -> String {
    events(running)
        .await
        .iter()
        .find_map(|stamped| match &stamped.event {
            Event::OrderSplit { order, .. } => Some(order.to_string()),
            _ => None,
        })
        .expect("the order was split before its scope is set")
}

/// A scope on `order` at revision 1 that binds `base`. It has no may-change items and one must-keep
/// rule over the signup answer's body, the same rule the server's scope-first tests confirm. The
/// scenarios check the claim, the clash and the redeclaration, not the rule's verdict.
fn scope_json(order: &str, base: &str) -> Value {
    json!({
        "version": "acan-scope/1",
        "order": order,
        "rev": 1,
        "base_commit": base,
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

/// The owner sets the split order's scope to revision 1 and confirms it, so workers may read the
/// scope and claim. Both commands go through the typed owner route, and each must be accepted.
async fn confirm_scope(running: &Running, base: &str) {
    let order = split_order_id(running).await;
    let order_no: u64 = order
        .strip_prefix("o-")
        .and_then(|number| number.parse().ok())
        .expect("an order id such as o-3");
    let scope_value = scope_json(&order, base);
    let scope: acan_intent::Scope =
        serde_json::from_value(scope_value.clone()).expect("the scope decodes");
    let revise: HumanCmd = serde_json::from_value(json!({
        "cmd": "revise_scope",
        "order": order_no,
        "scope": scope_value,
    }))
    .expect("the revision is a human command");
    let response = owner_command(running, &revise).await;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(status.is_success(), "the scope is set: {status} {body}");

    let selector_hashes: Vec<Value> = scope
        .body
        .must_keep
        .iter()
        .map(|rule| json!(rule.selector.selector_hash().expect("the selector hashes")))
        .collect();
    let confirm: HumanCmd = serde_json::from_value(json!({
        "cmd": "confirm_scope",
        "order": order_no,
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
    }))
    .expect("the confirmation is a human command");
    let response = owner_command(running, &confirm).await;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "the scope is confirmed: {status} {body}"
    );
}

/// The stored log, replayed to a world. Read through the owner's events route.
async fn events(running: &Running) -> Vec<Stamped> {
    client()
        .get(format!("{}/api/events", base_url(running)))
        .bearer_auth(OWNER_TOKEN)
        .send()
        .await
        .expect("the events request is sent")
        .error_for_status()
        .expect("the owner may read the log")
        .json()
        .await
        .expect("the log is JSON")
}

/// The origin's view of a stored log: how many events it holds, the last one, and each task's
/// state. A failure prints this, so a stalled step shows what the origin had done.
fn describe(log: &[Stamped], world: &World) -> String {
    let last = log.last().map_or_else(
        || "none".to_owned(),
        |stamped| format!("{:?}", stamped.event),
    );
    let tasks: Vec<String> = world
        .orders()
        .flat_map(|order| world.tasks_of(order.id))
        .map(|task| format!("{}: {:?}", task.id, task.state))
        .collect();
    format!(
        "{} event(s), last {last}; tasks [{}]",
        log.len(),
        tasks.join("; ")
    )
}

/// The origin's current view, read through the owner's events route.
async fn origin_view(running: &Running) -> String {
    let log = events(running).await;
    let origin = Origin::replay(log.iter().cloned()).expect("the stored log replays");
    describe(&log, origin.world())
}

/// Polls the stored log until `done` accepts it. A timeout names the wait that stalled and the
/// origin's view on the last read.
async fn settle<T>(
    running: &Running,
    what: &str,
    done: impl Fn(&[Stamped], &World) -> Option<T>,
) -> T {
    let mut last = String::from("no read of the stored log");
    let outcome = timeout(WAIT, async {
        loop {
            let log = events(running).await;
            let origin = Origin::replay(log.iter().cloned()).expect("the stored log replays");
            if let Some(found) = done(&log, origin.world()) {
                return found;
            }
            last = describe(&log, origin.world());
            sleep(POLL).await;
        }
    })
    .await;
    outcome.unwrap_or_else(|_| {
        panic!("timed out after {WAIT:?} waiting for {what}; the origin showed {last}")
    })
}

/// Waits for a worker to finish within `WAIT`. A worker that does not finish fails the test with
/// the origin's view, which shows the step the worker was waiting in.
async fn finished(running: &Running, worker: JoinHandle<Outcome>, who: &str) -> Outcome {
    match timeout(WAIT, worker).await {
        Ok(joined) => joined.unwrap_or_else(|err| panic!("{who} panicked: {err}")),
        Err(_) => panic!(
            "{who} did not finish within {WAIT:?}; the origin showed {}",
            origin_view(running).await
        ),
    }
}

fn tasks_of_world(world: &World) -> Vec<TaskId> {
    world
        .orders()
        .flat_map(|order| world.tasks_of(order.id))
        .map(|task| task.id)
        .collect()
}

/// The task `agent` holds a claim on, if any.
fn held_by(world: &World, agent: &AgentId) -> Option<TaskId> {
    world
        .orders()
        .flat_map(|order| world.tasks_of(order.id))
        .find_map(|task| match &task.state {
            TaskState::Claimed { claim, .. } if &claim.lease.holder == agent => Some(task.id),
            _ => None,
        })
}

fn is_stage(world: &World, task: TaskId, check: fn(&Stage) -> bool) -> bool {
    matches!(
        world.task(task).map(|found| &found.state),
        Some(TaskState::Claimed { stage, .. }) if check(stage)
    )
}

fn is_declared(stage: &Stage) -> bool {
    matches!(stage, Stage::Declared { .. })
}

fn is_building(stage: &Stage) -> bool {
    matches!(stage, Stage::Building { .. })
}

/// The first clash in the log, with the key's two tasks in `a`, `b` order.
fn clash_parties(log: &[Stamped]) -> Option<(ClashId, TaskId, TaskId)> {
    log.iter().find_map(|stamped| match &stamped.event {
        Event::ClashFound { clash, key, .. } => Some((*clash, key.a(), key.b())),
        _ => None,
    })
}

/// Every clash the log has found, in the order it was found.
fn clashes_found(log: &[Stamped]) -> Vec<ClashId> {
    log.iter()
        .filter_map(|stamped| match &stamped.event {
            Event::ClashFound { clash, .. } => Some(*clash),
            _ => None,
        })
        .collect()
}

/// Whether a task was cleared to build after the first clash in the log.
fn cleared_after_clash(log: &[Stamped]) -> bool {
    log.iter()
        .skip_while(|stamped| !matches!(stamped.event, Event::ClashFound { .. }))
        .any(|stamped| matches!(stamped.event, Event::BuildCleared { .. }))
}

/// Waits until the task's worktree is checked out. Git creates the directory and the `.git`
/// file before it writes the index, so a status read in that window lists every file as deleted.
/// A timeout names the path and the last read of it.
async fn wait_for_checkout(path: &Path) {
    let mut last = String::from("no read of the worktree yet");
    let outcome = timeout(WAIT, async {
        loop {
            let git_file = path.join(".git").exists();
            let status = git_status(path);
            if git_file && status == Some(String::new()) {
                return;
            }
            last = format!(".git present: {git_file}; git status: {status:?}");
            sleep(POLL).await;
        }
    })
    .await;
    outcome.unwrap_or_else(|_| {
        panic!(
            "the worktree {} was not checked out within {WAIT:?}; last seen: {last}",
            path.display()
        )
    });
}

/// The porcelain status of `dir`, or `None` while git cannot report it.
fn git_status(dir: &Path) -> Option<String> {
    let output = Process::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_on_one_file_clash_before_either_writes() {
    let _scenario = SCENARIO.lock().await;
    let repo = user_repo();
    let root = repo.path();
    let running = start_origin(root, 2).await;
    let base = base_url(&running);
    post_order(&running).await;
    register(&running, ONE).await;
    register(&running, TWO).await;
    let tasks = settle(&running, "the split into two tasks", |_, world| {
        let tasks = tasks_of_world(world);
        (tasks.len() == 2).then_some(tasks)
    })
    .await;
    assert_eq!(tasks.len(), 2, "the order splits into two tasks");
    confirm_scope(&running, &base_commit(root)).await;

    let (gate, open) = watch::channel(false);
    let one = spawn_worker(&base, ONE_TOKEN, repo.path(), open.clone());
    let two = spawn_worker(&base, TWO_TOKEN, repo.path(), open);

    let first = settle(&running, "agent-1 to claim a task", |_, world| {
        held_by(world, &AgentId::from_str(ONE).expect("valid agent id"))
    })
    .await;
    let second = settle(&running, "agent-2 to claim a task", |_, world| {
        held_by(world, &AgentId::from_str(TWO).expect("valid agent id"))
    })
    .await;
    assert_ne!(first, second, "each worker claims its own task");

    let (clash, key_a, _) = settle(&running, "both declarations to clash", |log, world| {
        let found = clash_parties(log)?;
        let both_declared = [first, second]
            .iter()
            .all(|task| is_stage(world, *task, is_declared));
        both_declared.then_some(found)
    })
    .await;
    // The menu offers the serialize in the clash key's order, so the key's `a` goes first.
    let (first_task, first_worker, second_task, second_worker) = if key_a == first {
        (first, one, second, two)
    } else {
        (second, two, first, one)
    };
    for task in [first, second] {
        let worktree = worktree_dir(root, task);
        wait_for_checkout(&worktree).await;
        assert_eq!(
            git(&worktree, &["status", "--porcelain"]),
            "",
            "{task} has no change while the clash is open"
        );
    }
    let log = events(&running).await;
    assert!(
        !cleared_after_clash(&log),
        "no task is cleared to build while the clash is open"
    );

    gate.send_replace(true);
    sleep(HOLD).await;
    let log = events(&running).await;
    assert!(
        !cleared_after_clash(&log),
        "opening the gate does not clear a task past the clash"
    );
    for task in [first, second] {
        let worktree = worktree_dir(root, task);
        assert_eq!(
            fs::read_to_string(worktree.join(FILE)).expect("the file is readable"),
            ORIGINAL,
            "{task} has not written {FILE}"
        );
    }

    let response = owner_command(
        &running,
        &HumanCmd::SettleClash {
            clash,
            choice: Settlement::Serialize {
                first: first_task,
                then: second_task,
            },
        },
    )
    .await;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        panic!(
            "the clash settles while both tasks are declared: {status} {body}; the origin showed {}",
            origin_view(&running).await
        );
    }

    settle(&running, "the first task to be cleared", |log, world| {
        let cleared = log.iter().any(
            |stamped| matches!(&stamped.event, Event::BuildCleared { task } if *task == first_task),
        );
        (cleared && is_stage(world, first_task, is_building)).then_some(())
    })
    .await;
    assert!(
        !is_stage(&running_world(&running).await, second_task, is_building),
        "the second task waits for the first to land"
    );

    let outcome = finished(&running, first_worker, "the first worker").await;
    assert!(
        matches!(&outcome.exit, Exit::Built { task, .. } if task == &first_task.to_string()),
        "the first worker builds its task: {}",
        outcome.exit
    );
    // The claim rests on the scope-first readback, which the worker logs before it claims.
    assert!(
        outcome
            .steps
            .iter()
            .any(|step| step.starts_with(&format!("read back {first_task} at scope rev 1"))),
        "the worker reads back the confirmed scope before it claims: {:#?}",
        outcome.steps
    );
    assert_eq!(
        fs::read_to_string(worktree_dir(root, first_task).join(FILE))
            .expect("the file is readable"),
        CHANGED,
        "the first worker wrote its change"
    );
    second_worker.abort();
    running.stop().await.expect("the origin stops");
}

/// Posts the order and starts two workers on it, each with its script. Waits until each worker
/// holds a task and both tasks are declared on `FILE` with the clash open. Returns the clash, the
/// task each worker holds, and both workers.
async fn two_declared_on_one_file(
    running: &Running,
    root: &Path,
    one_script: Stub,
    two_script: Stub,
    open: watch::Receiver<bool>,
) -> (
    ClashId,
    TaskId,
    TaskId,
    JoinHandle<Outcome>,
    JoinHandle<Outcome>,
) {
    let base = base_url(running);
    post_order(running).await;
    register(running, ONE).await;
    register(running, TWO).await;
    let tasks = settle(running, "the split into two tasks", |_, world| {
        let tasks = tasks_of_world(world);
        (tasks.len() == 2).then_some(tasks)
    })
    .await;
    assert_eq!(tasks.len(), 2, "the order splits into two tasks");
    confirm_scope(running, &base_commit(root)).await;

    let one = spawn_scripted(&base, ONE_TOKEN, root, one_script, open.clone());
    let two = spawn_scripted(&base, TWO_TOKEN, root, two_script, open);
    let first = settle(running, "agent-1 to claim a task", |_, world| {
        held_by(world, &AgentId::from_str(ONE).expect("valid agent id"))
    })
    .await;
    let second = settle(running, "agent-2 to claim a task", |_, world| {
        held_by(world, &AgentId::from_str(TWO).expect("valid agent id"))
    })
    .await;
    assert_ne!(first, second, "each worker claims its own task");
    let clash = settle(running, "both declarations to clash", |log, world| {
        let (clash, _, _) = clash_parties(log)?;
        let both_declared = [first, second]
            .iter()
            .all(|task| is_stage(world, *task, is_declared));
        both_declared.then_some(clash)
    })
    .await;
    (clash, first, second, one, two)
}

/// Settles the clash by narrowing `task`, as the owner would from the UI.
async fn narrow(running: &Running, clash: ClashId, task: TaskId) {
    let response = owner_command(
        running,
        &HumanCmd::SettleClash {
            clash,
            choice: Settlement::Narrow { task },
        },
    )
    .await;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "the clash is narrowed: {status} {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_narrowed_task_redeclares_without_the_file_it_gave_up_and_lands() {
    let _scenario = SCENARIO.lock().await;
    let repo = user_repo();
    let root = repo.path();
    let running = start_origin(root, 2).await;
    let (_gate, open) = watch::channel(true);
    let script = narrowed_script(json!({ "summary": "change the other file", "files": [OTHER] }));
    let (clash, _, second, one, two) =
        two_declared_on_one_file(&running, root, worker_script(), script, open).await;

    narrow(&running, clash, second).await;

    let narrowed = finished(&running, two, "the narrowed worker").await;
    assert!(
        matches!(&narrowed.exit, Exit::Built { task, .. } if task == &second.to_string()),
        "the narrowed worker builds its task: {}",
        narrowed.exit
    );
    let prefix = format!("narrowed {second}: gave up {FILE}; declared 1 file(s) without them;");
    assert!(
        narrowed.steps.iter().any(|step| step.starts_with(&prefix)),
        "the redeclaration is logged: {:#?}",
        narrowed.steps
    );
    let worktree = worktree_dir(root, second);
    assert_eq!(
        git(
            &worktree,
            &["show", "--name-only", "--pretty=format:", "HEAD"]
        )
        .trim(),
        OTHER,
        "the narrowed task's commit carries only the file it kept"
    );
    assert_eq!(
        fs::read_to_string(worktree.join(OTHER)).expect("the file is readable"),
        OTHER_CHANGED,
        "the narrowed worker wrote its change to the file it kept"
    );

    let built = finished(&running, one, "the other worker").await;
    assert!(
        matches!(&built.exit, Exit::Built { .. }),
        "the other worker builds: {}",
        built.exit
    );
    running.stop().await.expect("the origin stops");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_narrowed_task_that_cannot_go_on_without_the_file_releases_with_the_reason() {
    let _scenario = SCENARIO.lock().await;
    let repo = user_repo();
    let root = repo.path();
    let running = start_origin(root, 2).await;
    let (_gate, open) = watch::channel(true);
    let script = narrowed_script(json!({
        "summary": "the change needs the loader file",
        "files": [],
    }));
    let (clash, _, second, one, two) =
        two_declared_on_one_file(&running, root, worker_script(), script, open).await;

    narrow(&running, clash, second).await;

    let narrowed = finished(&running, two, "the narrowed worker").await;
    assert!(
        matches!(
            &narrowed.exit,
            Exit::Narrowed { task, reason }
                if task == &second.to_string() && reason.contains(&format!("cannot be done without {FILE}"))
        ),
        "the narrowed worker stops with the reason: {}",
        narrowed.exit
    );
    assert!(
        narrowed
            .steps
            .iter()
            .any(|step| step.starts_with(&format!("released {second}: "))),
        "the claim is released with the reason: {:#?}",
        narrowed.steps
    );
    // The other worker waits on the declare barrier while its sibling is open again, so this test
    // does not wait for its build.
    one.abort();
    running.stop().await.expect("the origin stops");
}

/// The first narrowing moves the task to `OTHER`. The second narrowing takes `OTHER` too, and the
/// planner then plans `FILE` again. The harness refuses that plan, because the task gave up `FILE`
/// earlier, so the claim is released with both places named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_narrowed_task_narrowed_again_is_not_planned_back_onto_a_file_it_gave_up() {
    let _scenario = SCENARIO.lock().await;
    let repo = user_repo();
    let root = repo.path();
    let running = start_origin(root, 2).await;
    let (_gate, open) = watch::channel(true);
    let (clash, _, second, one, two) = two_declared_on_one_file(
        &running,
        root,
        worker_script_planning(&[FILE, OTHER]),
        twice_narrowed_script(),
        open,
    )
    .await;

    narrow(&running, clash, second).await;
    // The first narrowing moves the task onto `OTHER`, which the first worker also declares. That
    // declaration is the second clash in the log.
    let other = settle(
        &running,
        "the narrowed task to clash on the other file",
        |log, _| clashes_found(log).get(1).copied(),
    )
    .await;
    narrow(&running, other, second).await;

    let narrowed = finished(&running, two, "the narrowed worker").await;
    let expected = format!("no plan without {FILE}, {OTHER} was accepted");
    assert!(
        matches!(
            &narrowed.exit,
            Exit::Narrowed { task, reason }
                if task == &second.to_string() && reason.contains(&expected)
        ),
        "the second re-plan is refused for the file the first narrowing took: {}",
        narrowed.exit
    );
    assert!(
        narrowed
            .steps
            .iter()
            .any(|step| step.starts_with(&format!("released {second}: "))),
        "the claim is released with the reason: {:#?}",
        narrowed.steps
    );

    // The other worker waits on the declare barrier while its sibling is released, so this test
    // does not wait for its build.
    one.abort();
    running.stop().await.expect("the origin stops");
}

async fn running_world(running: &Running) -> World {
    let log = events(running).await;
    Origin::replay(log)
        .expect("the stored log replays")
        .world()
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_refuses_a_wrong_or_member_token_as_the_http_routes_do() {
    let _scenario = SCENARIO.lock().await;
    let repo = user_repo();
    let running = start_origin(repo.path(), 2).await;
    let base = base_url(&running);
    let initialize = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });

    let wrong = mcp_post(&running, "acan_not_a_token", &initialize).await;
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);
    let wrong_body: Value = wrong.json().await.expect("the refusal is JSON");
    let http = client()
        .get(format!("{base}/api/tasks/t-0/worktree"))
        .bearer_auth("acan_not_a_token")
        .send()
        .await
        .expect("the worktree request is sent");
    assert_eq!(http.status(), reqwest::StatusCode::UNAUTHORIZED);
    let http_body: Value = http.json().await.expect("the refusal is JSON");
    assert_eq!(
        wrong_body, http_body,
        "MCP refuses a bad token as HTTP does"
    );

    let member = mcp_post(&running, OWNER_TOKEN, &initialize).await;
    assert_eq!(
        member.status(),
        reqwest::StatusCode::FORBIDDEN,
        "a member token may not use the agent endpoint"
    );

    let refused = McpClient::new(&base, "acan_not_a_token".to_owned())
        .connect()
        .await
        .expect_err("the harness is refused with a bad token");
    assert!(
        refused.to_string().contains("refused the agent token"),
        "the harness names the refusal: {refused:#}"
    );
    running.stop().await.expect("the origin stops");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_list_offers_the_fourteen_tools() {
    let _scenario = SCENARIO.lock().await;
    let repo = user_repo();
    let running = start_origin(repo.path(), 2).await;
    let response = mcp_post(
        &running,
        ONE_TOKEN,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    assert!(response.status().is_success());
    let body: Value = response.json().await.expect("the listing is JSON");
    let mut names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("the listing has tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
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
    running.stop().await.expect("the origin stops");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_fence_is_refused_with_the_rejection_text() {
    let _scenario = SCENARIO.lock().await;
    let repo = user_repo();
    let root = repo.path();
    let running = start_origin(root, 2).await;
    let base = base_url(&running);
    post_order(&running).await;
    register(&running, ONE).await;
    let task = settle(&running, "the split", |_, world| {
        tasks_of_world(world).first().copied()
    })
    .await;

    confirm_scope(&running, &base_commit(root)).await;

    let mcp = McpClient::new(&base, ONE_TOKEN.to_owned());
    mcp.connect().await.expect("the harness connects");
    // The agent reads the confirmed scope and posts a readback at its revision and hash. The
    // origin acknowledges the readback, and the claim rests on that acknowledgement.
    let scope = mcp
        .call(
            "get_scope",
            json!({ "version": "acan-mcp/1", "task": task.to_string() }),
        )
        .await
        .expect("the scope call completes");
    let Reply::Done(scope) = scope else {
        panic!("the confirmed scope is read: {scope:?}");
    };
    let posted = mcp
        .call(
            "post_scoped_readback",
            json!({
                "version": "acan-mcp/1",
                "task": task.to_string(),
                "scope_rev": scope["scope"]["rev"],
                "scope_hash": scope["scope_hash"],
                "summary": "change the loader",
                "predictions": [],
            }),
        )
        .await
        .expect("the readback call completes");
    assert!(
        matches!(posted, Reply::Done(_)),
        "the readback is acknowledged: {posted:?}"
    );
    let claimed = mcp
        .call("claim_task", json!({ "task": task.to_string() }))
        .await
        .expect("the claim call completes");
    let Reply::Done(claimed) = claimed else {
        panic!("the claim is accepted");
    };
    let held = u32::try_from(
        claimed["generation"]
            .as_u64()
            .expect("the claim names a generation"),
    )
    .expect("the generation fits in u32");

    let stale = mcp
        .call(
            "declare_changes",
            json!({
                "task": task.to_string(),
                "generation": held + 1,
                "files": [FILE],
            }),
        )
        .await
        .expect("the declare call completes");
    let expected = Rejection::StaleGeneration {
        current: Some(Generation::new(held)),
    }
    .to_string();
    assert_eq!(
        stale,
        Reply::Refused(expected.clone()),
        "a stale fence is refused with the rejection text"
    );

    let raw = mcp_post(
        &running,
        ONE_TOKEN,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "heartbeat_task",
                "arguments": { "task": task.to_string(), "generation": held + 1 },
            },
        }),
    )
    .await;
    let body: Value = raw.json().await.expect("the answer is JSON");
    assert!(
        body.get("error").is_none(),
        "a rejection is not a transport error"
    );
    assert_eq!(body["result"]["isError"], json!(true));
    assert_eq!(body["result"]["content"][0]["text"], json!(expected));

    let renewed = mcp
        .call(
            "heartbeat_task",
            json!({ "task": task.to_string(), "generation": held }),
        )
        .await
        .expect("the renewal call completes");
    assert!(
        matches!(renewed, Reply::Done(_)),
        "the holder's current fence renews"
    );
    running.stop().await.expect("the origin stops");
}

/// A worker whose plan answers at once and whose edit step fails, as a model outage would.
struct FailingEdit {
    script: Stub,
}

impl ModelClient for FailingEdit {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        if request.tools.is_empty() {
            return self.script.chat(role, request).await;
        }
        Err(LlmError::NoScriptedReply { role })
    }
}

/// A worker that announces its first edit step, then waits on the gate as `Worker` does. The
/// announcement tells the test the worker is past clearance and the worktree, inside the edit.
struct AnnouncedEdit {
    script: Stub,
    gate: watch::Receiver<bool>,
    entered: watch::Sender<bool>,
}

impl ModelClient for AnnouncedEdit {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        if !request.tools.is_empty() {
            self.entered.send_replace(true);
            let mut gate = self.gate.clone();
            let _ = gate.wait_for(|open| *open).await;
        }
        self.script.chat(role, request).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_claim_stops_the_work_and_nothing_is_written() {
    let _scenario = SCENARIO.lock().await;
    let repo = user_repo();
    let root = repo.path();
    // One worker alone: a sibling task nobody claims would hold the declare barrier for its
    // whole deadline, so the order splits into one task.
    let running = start_origin(root, 1).await;
    let base = base_url(&running);
    post_order(&running).await;
    register(&running, ONE).await;
    settle(&running, "the split", |_, world| {
        (tasks_of_world(world).len() == 1).then_some(())
    })
    .await;
    confirm_scope(&running, &base_commit(root)).await;

    // The gate stays shut, so the worker waits inside its first edit step, holding its claim.
    let (gate, open) = watch::channel(false);
    let (entered_tx, mut entered) = watch::channel(false);
    let config = worker_config(&base, ONE_TOKEN, repo.path());
    let worker = tokio::spawn(async move {
        let model = AnnouncedEdit {
            script: worker_script(),
            gate: open,
            entered: entered_tx,
        };
        run(&config, &model).await
    });
    timeout(WAIT, entered.wait_for(|seen| *seen))
        .await
        .expect("the worker reaches its edit step")
        .expect("the worker is still running");
    let one = AgentId::from_str(ONE).expect("valid agent id");
    let task = settle(&running, "agent-1 to hold the task", |_, world| {
        held_by(world, &one)
    })
    .await;
    let worktree = worktree_dir(root, task);
    wait_for_checkout(&worktree).await;

    // The claim ends at the origin, as an expired lease would end it. The worker's next renewal
    // is refused, and that refusal is what stops its work.
    let holder = McpClient::new(&base, ONE_TOKEN.to_owned());
    holder.connect().await.expect("the holder connects");
    let Reply::Done(detail) = holder
        .call("get_task", json!({ "task": task.to_string() }))
        .await
        .expect("get_task completes")
    else {
        panic!("get_task is accepted for the holder");
    };
    let generation = detail["generation"]
        .as_u64()
        .expect("the claim names a generation");
    let released = holder
        .call(
            "release_task",
            json!({ "task": task.to_string(), "generation": generation }),
        )
        .await
        .expect("the release completes");
    assert!(
        matches!(released, Reply::Done(_)),
        "the claim is released: {released:?}"
    );

    let outcome = finished(&running, worker, "the worker").await;
    assert!(
        matches!(&outcome.exit, Exit::Lost { task: lost, .. } if lost == &task.to_string()),
        "the worker reports the loss: {}",
        outcome.exit
    );

    // The gate opens only after the work has stopped, so nothing the model does can write now.
    // The origin may already have removed the released task's worktree, so a missing file is
    // fine; a file with the change in it is not.
    gate.send_replace(true);
    sleep(HOLD).await;
    if let Ok(text) = fs::read_to_string(worktree.join(FILE)) {
        assert_eq!(text, ORIGINAL, "the lost worker wrote nothing");
    }
    running.stop().await.expect("the origin stops");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_error_releases_the_claim() {
    let _scenario = SCENARIO.lock().await;
    let repo = user_repo();
    // One worker alone: a sibling task nobody claims would hold the declare barrier for its
    // whole deadline, so the order splits into one task.
    let running = start_origin(repo.path(), 1).await;
    let base = base_url(&running);
    post_order(&running).await;
    register(&running, ONE).await;
    settle(&running, "the split", |_, world| {
        (tasks_of_world(world).len() == 1).then_some(())
    })
    .await;
    confirm_scope(&running, &base_commit(repo.path())).await;

    let config = worker_config(&base, ONE_TOKEN, repo.path());
    let worker = tokio::spawn(async move {
        let model = FailingEdit {
            script: worker_script(),
        };
        run(&config, &model).await
    });
    let outcome = finished(&running, worker, "the failing worker").await;
    assert!(
        matches!(&outcome.exit, Exit::Failed { reason } if reason.contains("the edit step failed")),
        "the worker names the failed step: {}",
        outcome.exit
    );
    assert!(
        outcome
            .steps
            .iter()
            .any(|step| step.starts_with("released ")),
        "the claim was released: {:?}",
        outcome.steps
    );
    let world = running_world(&running).await;
    assert!(
        held_by(&world, &AgentId::from_str(ONE).expect("valid agent id")).is_none(),
        "the origin holds no claim for agent-1"
    );
    running.stop().await.expect("the origin stops");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_envelope_is_checked_before_any_method_runs() {
    let _scenario = SCENARIO.lock().await;
    let repo = user_repo();
    let running = start_origin(repo.path(), 2).await;
    let bad_request = reqwest::StatusCode::BAD_REQUEST;

    let wrong_version = mcp_post(
        &running,
        ONE_TOKEN,
        &json!({ "jsonrpc": "1.0", "id": 7, "method": "ping" }),
    )
    .await;
    assert_eq!(wrong_version.status(), bad_request);
    let body: Value = wrong_version.json().await.expect("the refusal is JSON");
    assert_eq!(body["error"]["code"], json!(-32600));
    assert_eq!(body["id"], json!(7), "the refusal answers the request's id");

    let no_version = mcp_post(&running, ONE_TOKEN, &json!({ "id": 8, "method": "ping" })).await;
    assert_eq!(no_version.status(), bad_request);
    let body: Value = no_version.json().await.expect("the refusal is JSON");
    assert_eq!(body["error"]["code"], json!(-32600));

    let batch = mcp_post(
        &running,
        ONE_TOKEN,
        &json!([{ "jsonrpc": "2.0", "id": 9, "method": "ping" }]),
    )
    .await;
    assert_eq!(batch.status(), bad_request);
    let body: Value = batch.json().await.expect("the refusal is JSON");
    assert_eq!(body["error"]["code"], json!(-32600));
    assert_eq!(
        body["id"],
        Value::Null,
        "a message that is not an object has no id"
    );

    let object_id = mcp_post(
        &running,
        ONE_TOKEN,
        &json!({ "jsonrpc": "2.0", "id": { "x": 1 }, "method": "ping" }),
    )
    .await;
    assert_eq!(object_id.status(), bad_request);
    let body: Value = object_id.json().await.expect("the refusal is JSON");
    assert_eq!(body["error"]["code"], json!(-32600));
    assert_eq!(body["id"], Value::Null, "an object id is not a valid id");

    // A null id is still a request, so it gets an answer that carries the null id.
    let null_id = mcp_post(
        &running,
        ONE_TOKEN,
        &json!({ "jsonrpc": "2.0", "id": null, "method": "ping" }),
    )
    .await;
    assert_eq!(null_id.status(), reqwest::StatusCode::OK);
    let body: Value = null_id.json().await.expect("the answer is JSON");
    assert_eq!(
        body.get("id"),
        Some(&Value::Null),
        "the answer carries null"
    );
    assert_eq!(body["result"], json!({}));

    // A notification has no id and gets no reply body.
    let notice = mcp_post(
        &running,
        ONE_TOKEN,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert_eq!(notice.status(), reqwest::StatusCode::ACCEPTED);
    running.stop().await.expect("the origin stops");
}

async fn mcp_post(running: &Running, token: &str, body: &Value) -> reqwest::Response {
    client()
        .post(format!("{}/mcp", base_url(running)))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .expect("the MCP request is sent")
}
