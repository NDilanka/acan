//! The model jobs against an in-memory world, and the foreman's drafts.
//!
//! Prompts carry what the model judges with. A failed foreman call fails with its provider
//! error, and a failed call for another role is answered offline and labelled as such. A scope
//! or probe draft becomes a proposal only after Acan checks it, and it gains authority only
//! through `HumanSpec::adopt`.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command as Process;
use std::str::FromStr;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use acan_core::{
    AgentCmd, AgentId, CallFailure, ClashId, CodeIndex, Command, Ctx, DeclarationInput, Event,
    HumanCmd, Judgement, Locus, MachineKey, MemberId, OrderId, OrderRev, Origin, ReadbackId,
    ReadbackVerdict, Seq, Stamped, Step, SystemCmd, TaskId, TaskSpec, TaskState, Timestamp,
    WitnessId, WorldId, WorldRef,
};
use acan_git::Repo;
use acan_intent::{
    AppContract, CorpusProbe, Fixture, HumanRef, HumanSpec, ModelRef, ProbeCorpus, ProbeProposal,
    RouteEntry, RunLimits, ScopeBody, UnixMillis,
};
use acan_llm::{
    ApiKey, ApiKeys, ChatRequest, Destination, LlmConfig, LlmError, Message, ModelClient,
    OpenAiCompatible, Reply, Role, Stub, UsageLedger,
};
use acan_server::model::{
    self, DraftBounds, DraftError, DraftReply, ReplyError, SplitReply, SplitTask,
};
use acan_server::{
    DEFAULT_PROBE_CONCURRENCY, DEFAULT_WITNESS_CONCURRENCY, EventStore, Host, HostParts, Models,
    Tokens, scan, spawn_jobs,
};
use serde_json::{Value, json};
use support::Demo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const ORDER_SENTENCE: &str = "make the greeting configurable from the settings file";
const RATE_LIMIT_ORDER: &str =
    "Add per-user rate limiting to link creation and keep the login flow unchanged";

/// The probe corpus fixture. Its route table matches the demo README's route table.
const CORPUS: &str = include_str!("../../acan-intent/fixtures/probe-corpus.json");
/// The scope draft fixture, whose proposal is a scope body the model writes.
const SCOPE_FIXTURE: &str = include_str!("../../acan-intent/fixtures/scope-draft.json");

fn user_json(request: &ChatRequest) -> Value {
    let text = request
        .last_user_text()
        .expect("every model request carries its context as the user message");
    serde_json::from_str(text).expect("the context is JSON")
}

/// Splits `order` into two fixed tasks, the way a live split would.
fn split_in_two(demo: &mut Demo, order: OrderId) -> [TaskId; 2] {
    let rev = order_rev(demo, order);
    let spec = |title: &str| TaskSpec {
        title: title.to_owned(),
        brief: format!("{title}, then run the greet tests"),
        scope: vec![support::machine(support::UI)],
    };
    demo.apply(Command::System(SystemCmd::SplitReady {
        order,
        rev,
        tasks: vec![spec("Read the greeting"), spec("Add the setting")],
    }));
    match demo.tasks_of(order).as_slice() {
        [first, second] => [*first, *second],
        other => panic!("two tasks were expected, got {other:?}"),
    }
}

fn order_rev(demo: &Demo, order: OrderId) -> OrderRev {
    demo.world().order(order).expect("the order exists").rev
}

/// Gives every readback a matching verdict at the current revision, the verdict a legacy claim
/// reads. Test fixture only: no model judges here, since review is retired.
fn approve(demo: &mut Demo, order: OrderId, readbacks: &BTreeSet<ReadbackId>) {
    let (rev, witnesses) = {
        let found = demo.world().order(order).expect("the order exists");
        (found.rev, found.witnesses.clone())
    };
    let verdicts = readbacks
        .iter()
        .map(|readback| {
            let per_witness: BTreeMap<WitnessId, Judgement> = witnesses
                .iter()
                .map(|witness| (*witness, Judgement::Match))
                .collect();
            (*readback, ReadbackVerdict { rev, per_witness })
        })
        .collect();
    demo.apply(Command::System(SystemCmd::ReadbacksReviewed {
        order,
        rev,
        verdicts,
        question: None,
    }));
}

#[test]
fn the_split_prompt_lists_the_plant_machines_and_the_order() {
    let mut demo = Demo::open();
    let order = demo.post_order(ORDER_SENTENCE);
    let machines = model::plant_machines(demo.index());

    let request = model::split_request(demo.world(), order, &machines).expect("the order exists");
    let context = user_json(&request);

    assert_eq!(
        context["machines"],
        json!(["api", "ui"]),
        "the model may name only machines on the plant"
    );
    assert_eq!(context["machine"], "ui", "the order's own machine");
    assert_eq!(context["order"], ORDER_SENTENCE);
    assert_eq!(
        context["witnesses"][0]["reason"], "the greeting keeps its default",
        "the witnesses the split must keep"
    );
}

#[test]
fn a_split_that_names_a_machine_off_the_plant_is_refused() {
    let mut demo = Demo::open();
    let order = demo.post_order(ORDER_SENTENCE);
    let machines = model::plant_machines(demo.index());
    let rev = order_rev(&demo, order);
    let reply = SplitReply {
        tasks: vec![SplitTask {
            title: "Add a database column".to_owned(),
            brief: "Add the column and its migration".to_owned(),
            scope: vec!["db".to_owned()],
        }],
    };

    let err =
        model::split_result(order, rev, &machines, reply).expect_err("db is not on the plant");

    assert!(
        matches!(&err, ReplyError::UnknownMachine(name) if name == "db"),
        "got {err:?}"
    );
}

#[test]
fn the_explain_prompt_carries_both_declared_footprints_and_the_menu() {
    let mut demo = Demo::open();
    let order = demo.post_order(ORDER_SENTENCE);
    let tasks = split_in_two(&mut demo, order);
    let readbacks = support::read_back(&mut demo, tasks);
    approve(&mut demo, order, &readbacks);
    let clash = support::claim_and_declare(&mut demo, tasks);

    let request = model::explain_request(demo.world(), clash).expect("the clash exists");
    let context = user_json(&request);

    let declared = json!([{ "file": support::GREET_FILE, "name": support::GREET }]);
    let parties = context["tasks"].as_array().expect("two parties");
    assert_eq!(parties.len(), 2, "both tasks are in the prompt");
    for party in parties {
        assert_eq!(
            party["declared"]["symbols"], declared,
            "each party's declared symbols are in the prompt"
        );
    }
    assert!(
        context["menu"]
            .as_array()
            .is_some_and(|menu| !menu.is_empty()),
        "the menu of legal choices is in the prompt"
    );
    assert_eq!(
        context["witnesses"][0]["reason"], "the greeting keeps its default",
        "the witnesses a breaking choice would name are in the prompt"
    );
}

/// A loopback address whose connections close at once, so a request to it fails fast. The
/// listener is held for the rest of the test, so no other test's server can take the port.
async fn unreachable_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local port is free");
    let address = listener.local_addr().expect("the listener has an address");
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });
    format!("http://{address}/v1")
}

/// Reads one HTTP request, headers and body, so the reply is never written into an unread body.
/// Returns the body bytes.
async fn drain_request(socket: &mut TcpStream) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = socket
            .read(&mut chunk)
            .await
            .expect("the request can be read");
        if read == 0 {
            return Vec::new();
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let body_start = end + 4;
        if buffer.len() >= body_start + length {
            return buffer[body_start..body_start + length].to_vec();
        }
    }
}

/// Serves every connection with one chat-completions reply whose text is `content` and whose
/// finish reason is `finish_reason`, and reports 120 prompt and 30 completion tokens. When
/// `record` is set, each request body is sent to it as JSON before the reply goes out. Returns
/// the base URL for the provider.
async fn serve(content: &str, finish_reason: &str, record: Option<Sender<Value>>) -> String {
    let reply = json!({
        "choices": [{ "message": { "role": "assistant", "content": content }, "finish_reason": finish_reason }],
        "usage": { "prompt_tokens": 120, "completion_tokens": 30 },
    })
    .to_string();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local port is free");
    let address = listener.local_addr().expect("the listener has an address");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let reply = reply.clone();
            let record = record.clone();
            tokio::spawn(async move {
                let body = drain_request(&mut socket).await;
                if let (Some(record), Ok(json)) = (&record, serde_json::from_slice::<Value>(&body))
                {
                    let _ = record.send(json);
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{address}/v1")
}

async fn serve_reply(content: &str) -> String {
    serve(content, "stop", None).await
}

/// Like [`serve_reply`], and also returns the receiver of each request body the provider gets.
async fn serve_reply_recording(content: &str) -> (String, Receiver<Value>) {
    let (sender, received) = std::sync::mpsc::channel();
    (serve(content, "stop", Some(sender)).await, received)
}

/// Like [`serve_reply`], but the reply stops at the output cap, with finish reason `length`.
async fn serve_reply_capped(content: &str) -> String {
    serve(content, "length", None).await
}

/// A routing file where every role uses one `baseten` provider at `base_url`.
fn config(base_url: &str) -> LlmConfig {
    LlmConfig::from_toml_str(&format!(
        r#"
[[providers]]
name = "baseten"
base_url = "{base_url}"
api_key_env = "BASETEN_API_KEY"
default_model = "meta-models/muse-spark-open-1026"
timeout_secs = 5

[routing.foreman]
provider = "baseten"

[routing.worker]
provider = "baseten"

[routing.fast]
provider = "baseten"
"#
    ))
    .expect("the test config is valid")
}

fn keys(with_key: bool) -> ApiKeys {
    let mut keys = ApiKeys::default();
    if with_key {
        keys.insert("baseten", ApiKey::new("test-key-not-real"));
    }
    keys
}

/// The split prompt for a fresh order, with the order and the machine list it was built from.
fn split_case() -> (Demo, OrderId, Vec<MachineKey>, ChatRequest) {
    let mut demo = Demo::open();
    let order = demo.post_order(ORDER_SENTENCE);
    let machines = model::plant_machines(demo.index());
    let request = model::split_request(demo.world(), order, &machines).expect("the order exists");
    (demo, order, machines, request)
}

/// A request with one system prompt and a user message, for the prompts a test builds by hand.
fn prompt_request(system: &str, user: &str) -> ChatRequest {
    ChatRequest {
        messages: vec![Message::system(system), Message::user(user)],
        ..ChatRequest::default()
    }
}

/// What the drafts are checked against in these tests: a plant and the corpus route table and
/// fixtures. The request budget is set per check.
struct Plant {
    machines: Vec<MachineKey>,
    routes: Vec<RouteEntry>,
    fixtures: Vec<Fixture>,
    probes: Vec<CorpusProbe>,
}

fn corpus() -> ProbeCorpus {
    serde_json::from_str(CORPUS).expect("the probe corpus fixture decodes")
}

fn plant_with(machines: Vec<MachineKey>) -> Plant {
    let corpus = corpus();
    Plant {
        machines,
        routes: corpus.routes,
        fixtures: corpus.fixtures,
        probes: corpus.probes,
    }
}

/// The machines the rate-limit scope fixture names, and the ones the split fixture uses.
fn fixed_machines() -> Vec<MachineKey> {
    ["api", "auth", "errors", "links", "ratelimit", "ui"]
        .into_iter()
        .map(|name| MachineKey::from_str(name).expect("a valid machine name"))
        .collect()
}

impl Plant {
    fn bounds(&self, max_requests: u32) -> DraftBounds<'_> {
        DraftBounds {
            machines: &self.machines,
            routes: &self.routes,
            fixtures: &self.fixtures,
            probes: &self.probes,
            limits: RunLimits {
                max_requests,
                max_output_bytes: 65_536,
                sequence_timeout_ms: 30_000,
            },
        }
    }
}

fn model_ref() -> ModelRef {
    ModelRef::from_str("muse-spark").expect("a valid model id")
}

/// The scope body of the scope draft fixture, as the model writes it.
fn fixture_scope() -> Value {
    let draft: Value = serde_json::from_str(SCOPE_FIXTURE).expect("the scope fixture is JSON");
    draft["draft"]["proposal"].clone()
}

/// A probe that logs in as a fresh client, captures the session cookie, and reads the links with
/// it. Two HTTP requests, one capture, and only inputs.
fn probe_json() -> Value {
    json!({
        "fixture": "f-alice",
        "steps": [
            {
                "step": "http",
                "id": "s1",
                "client": "client-a",
                "request": {
                    "method": "POST",
                    "path": "/api/login",
                    "body": { "email": "alice@example.com", "password": "correct horse" }
                },
                "redirect": "manual",
                "deadline_ms": 5000
            },
            {
                "step": "capture",
                "id": "s2",
                "from": "s1",
                "source": { "part": "cookie", "name": "session" },
                "binding": "session",
                "kind": "session"
            },
            {
                "step": "http",
                "id": "s3",
                "client": "client-a",
                "request": {
                    "method": "GET",
                    "path": "/api/links",
                    "headers": { "cookie": "session=${session}" }
                },
                "redirect": "manual",
                "deadline_ms": 5000
            }
        ],
        "route_template": "/api/login",
        "rationale": "Logs in and reads the links with the session, so the login flow is exercised."
    })
}

/// Checks a scope body given as JSON, the way the live path does after the reply decodes.
fn check_scope_json(scope: Value, plant: &Plant) -> Result<(), DraftError> {
    let body: ScopeBody = serde_json::from_value(scope).expect("the scope body decodes");
    model::scope_draft(body, model_ref(), &plant.bounds(5)).map(drop)
}

/// Checks a probe given as JSON against a budget of `max_requests` HTTP requests.
fn check_probe_json(probe: Value, plant: &Plant, max_requests: u32) -> Result<(), DraftError> {
    let proposal: ProbeProposal = serde_json::from_value(probe).expect("the probe decodes");
    model::probe_draft(proposal, model_ref(), &plant.bounds(max_requests)).map(drop)
}

#[test]
fn the_scope_fixture_is_a_model_proposal_that_acan_accepts() {
    let plant = plant_with(fixed_machines());
    let body: ScopeBody =
        serde_json::from_value(fixture_scope()).expect("the fixture decodes as a scope body");

    let draft = model::scope_draft(body, model_ref(), &plant.bounds(5))
        .expect("the fixture names only routes and machines the plant has");

    assert_eq!(draft.model().as_str(), "muse-spark");
    assert_eq!(
        draft.proposal().may.len(),
        3,
        "the fixture has three may items"
    );
}

#[test]
fn a_draft_gains_authority_only_through_adopt() {
    let plant = plant_with(fixed_machines());
    let body: ScopeBody =
        serde_json::from_value(fixture_scope()).expect("the fixture decodes as a scope body");
    let draft = model::scope_draft(body, model_ref(), &plant.bounds(5)).expect("a valid draft");

    let human = HumanRef::from_str("human-1").expect("a valid human id");
    let spec = HumanSpec::adopt(draft, human, UnixMillis(1_793_259_000_000));

    assert_eq!(
        spec.value().may.len(),
        3,
        "the adopted spec holds the draft's items"
    );
}

#[test]
fn a_scope_that_names_a_route_off_the_table_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["selector"]["route_template"] = json!("/api/nope");

    let err = check_scope_json(scope, &plant).expect_err("/api/nope is not in the route table");

    assert!(
        matches!(&err, DraftError::UnknownRoute(route) if route == "/api/nope"),
        "got {err:?}"
    );
}

#[test]
fn a_scope_that_names_a_machine_off_the_plant_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["machine_ids"] = json!(["db"]);

    let err = check_scope_json(scope, &plant).expect_err("db is not on the plant");

    assert!(
        matches!(&err, DraftError::UnknownMachine(machine) if machine == "db"),
        "got {err:?}"
    );
}

#[test]
fn a_scope_that_names_a_step_the_corpus_does_not_define_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["selector"]["step_context"] =
        json!({ "kind": "step", "step_id": "login-step" });

    let err = check_scope_json(scope, &plant).expect_err("login-step is not a corpus step");

    assert!(
        matches!(&err, DraftError::UndefinedStep(step) if step == "login-step"),
        "got {err:?}"
    );
}

#[test]
fn a_scope_whose_example_names_an_undefined_step_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["example"]["step_id"] = json!("audit-step");

    let err = check_scope_json(scope, &plant).expect_err("audit-step is not a corpus step");

    assert!(
        matches!(&err, DraftError::UndefinedStep(step) if step == "audit-step"),
        "got {err:?}"
    );
}

#[test]
fn a_predicate_that_reads_an_undefined_step_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["acceptance"][0]["predicate"] = json!({
        "predicate": "equals_base",
        "step_id": "login-step",
        "path": "body",
        "anchor": { "kind": "fresh" }
    });

    let err = check_scope_json(scope, &plant).expect_err("login-step is not a corpus step");

    assert!(
        matches!(&err, DraftError::UndefinedStep(step) if step == "login-step"),
        "got {err:?}"
    );
}

/// A must-keep rule on `/api/signup` whose one status predicate reads `step`.
fn keep_rule_reading(step: &str) -> Value {
    let mut scope = fixture_scope();
    scope["must_keep"][0]["selector"]["route_template"] = json!("/api/signup");
    scope["must_keep"][0]["selector"]["step_context"] = json!({ "kind": "any" });
    scope["must_keep"][0]["obligations"] = json!([{
        "obligation_id": "obl-keep",
        "predicate": {
            "predicate": "equals_base",
            "step_id": step,
            "path": "status",
            "anchor": { "kind": "fresh" }
        },
        "text": "Status stays as before."
    }]);
    scope
}

#[test]
fn a_status_predicate_on_a_step_off_the_rule_route_is_refused() {
    // The corpus sends s3 as a request to /api/login and to /api/links, never to /api/signup.
    let plant = plant_with(fixed_machines());
    let err = check_scope_json(keep_rule_reading("s3"), &plant)
        .expect_err("s3 never sends a request to /api/signup");

    assert!(
        matches!(&err, DraftError::StepOffRoute { step, route }
            if step == "s3" && route == "/api/signup"),
        "got {err:?}"
    );
}

#[test]
fn a_status_predicate_on_a_step_that_sends_the_rule_route_is_accepted() {
    // s1 sends POST /api/login in the login probe, and the rule is for POST /api/login.
    let plant = plant_with(fixed_machines());
    let mut scope = keep_rule_reading("s1");
    scope["must_keep"][0]["selector"]["route_template"] = json!("/api/login");

    check_scope_json(scope, &plant).expect("s1 sends a login request in the login probe");
}

#[test]
fn an_example_expecting_a_rejected_status_inside_the_allowed_count_is_refused() {
    // The window allows its first five logins, so the first login must not get the 429 the
    // example expects.
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["acceptance"][0]["predicate"]["policy"]["obligations"] =
        json!([{ "kind": "first_allowed" }]);
    scope["may"][0]["example"]["step_id"] = json!("s1");

    let err = check_scope_json(scope, &plant).expect_err("s1 is within the first five logins");

    assert!(
        matches!(&err, DraftError::ExampleContradictsAcceptance { step, status, allowed }
            if step == "s1" && *status == 429 && *allowed == 5),
        "got {err:?}"
    );
}

#[test]
fn an_example_expecting_a_rejected_status_past_the_allowed_count_is_accepted() {
    // s6 is the sixth login in its probe, so the window may reject it.
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["acceptance"][0]["predicate"]["policy"]["obligations"] =
        json!([{ "kind": "first_allowed" }]);

    check_scope_json(scope, &plant).expect("the sixth login may get 429");
}

#[test]
fn a_side_effect_no_probe_observes_after_its_route_is_refused() {
    // The corpus reads the store only after POST /api/signup, never after GET /api/links.
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["selector"] = json!({
        "surface": "http",
        "method": "GET",
        "route_template": "/api/links",
        "step_context": { "kind": "any" },
        "principal": { "kind": "any" },
        "channel": "side_effect",
        "field_path": "store.audit[*].route"
    });

    let err = check_scope_json(scope, &plant).expect_err("no probe reads the store after GET");

    assert!(
        matches!(
            &err,
            DraftError::UnwitnessedSideEffect { sink, route }
                if sink == "store" && route == "/api/links"
        ),
        "got {err:?}"
    );
}

#[test]
fn a_side_effect_a_probe_observes_after_its_route_is_accepted() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["selector"] = json!({
        "surface": "http",
        "method": "POST",
        "route_template": "/api/signup",
        "step_context": { "kind": "step", "step_id": "s2" },
        "principal": { "kind": "any" },
        "channel": "side_effect",
        "field_path": "store.users[*].email"
    });

    check_scope_json(scope, &plant)
        .expect("the signup probe reads the store after POST /api/signup");
}

#[test]
fn an_empty_scope_is_refused_and_never_stands_for_a_reply() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"] = json!([]);

    let err = check_scope_json(scope, &plant).expect_err("a scope with no may items is empty");

    assert_eq!(err, DraftError::EmptyScope);
}

#[test]
fn a_scope_obligation_over_thirty_words_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["acceptance"][0]["text"] = json!(vec!["word"; 31].join(" "));

    let err = check_scope_json(scope, &plant).expect_err("31 words is over the 30-word cap");

    assert!(
        matches!(
            &err,
            DraftError::TooLong {
                field: "obligation text",
                words: 31,
                cap: 30,
            }
        ),
        "got {err:?}"
    );
}

#[test]
fn a_scope_option_over_eight_words_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["open"][0]["options"][0]["label"] = json!(["word"; 9].join(" "));

    let err = check_scope_json(scope, &plant).expect_err("9 words is over the 8-word cap");

    assert!(
        matches!(
            &err,
            DraftError::TooLong {
                field: "option label",
                words: 9,
                cap: 8,
            }
        ),
        "got {err:?}"
    );
}

#[test]
fn a_scope_at_each_word_cap_is_accepted() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["may"][0]["acceptance"][0]["text"] = json!(vec!["word"; 30].join(" "));
    scope["open"][0]["question"] = json!(vec!["word"; 30].join(" "));
    scope["open"][0]["options"][0]["label"] = json!(["word"; 8].join(" "));

    check_scope_json(scope, &plant)
        .expect("a 30-word obligation, a 30-word question and an 8-word option fit their caps");
}

#[test]
fn a_must_keep_rule_over_thirty_words_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["must_keep"][0]["text"] = json!(vec!["word"; 31].join(" "));

    let err = check_scope_json(scope, &plant).expect_err("31 words is over the 30-word cap");

    assert!(
        matches!(
            &err,
            DraftError::TooLong {
                field: "must-keep rule",
                words: 31,
                cap: 30,
            }
        ),
        "got {err:?}"
    );
}

#[test]
fn a_must_keep_rule_at_thirty_words_is_accepted() {
    let plant = plant_with(fixed_machines());
    let mut scope = fixture_scope();
    scope["must_keep"][0]["text"] = json!(vec!["word"; 30].join(" "));

    check_scope_json(scope, &plant).expect("a 30-word must-keep rule fits its cap");
}

#[test]
fn the_probe_fixture_is_accepted_as_a_proposal() {
    let plant = plant_with(fixed_machines());
    let proposal: ProbeProposal = serde_json::from_value(probe_json()).expect("the probe decodes");

    let draft = model::probe_draft(proposal, model_ref(), &plant.bounds(5))
        .expect("the probe names a fixture, a route and bound captures");

    assert_eq!(draft.proposal().steps.len(), 3);
}

#[test]
fn a_probe_on_a_route_off_the_table_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut probe = probe_json();
    probe["route_template"] = json!("/api/nope");

    let err = check_probe_json(probe, &plant, 5).expect_err("/api/nope is not a route");

    assert!(
        matches!(&err, DraftError::UnknownRoute(route) if route == "/api/nope"),
        "got {err:?}"
    );
}

#[test]
fn a_probe_on_a_fixture_the_corpus_lacks_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut probe = probe_json();
    probe["fixture"] = json!("f-nope");

    let err = check_probe_json(probe, &plant, 5).expect_err("f-nope is not in the corpus");

    assert!(
        matches!(&err, DraftError::UnknownFixture(fixture) if fixture == "f-nope"),
        "got {err:?}"
    );
}

#[test]
fn a_probe_that_uses_a_capture_no_step_binds_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut probe = probe_json();
    probe["steps"][2]["request"]["headers"]["cookie"] = json!("session=${token}");

    let err = check_probe_json(probe, &plant, 5).expect_err("token is never bound");

    assert_eq!(
        err,
        DraftError::UnknownCapture {
            step: "s3".to_owned(),
            name: "token".to_owned(),
        }
    );
}

#[test]
fn a_placeholder_in_a_body_is_checked_like_one_in_a_header() {
    let plant = plant_with(fixed_machines());
    let mut probe = probe_json();
    probe["steps"][2]["request"]["body"] = json!({ "who": "${nobody}" });

    let err = check_probe_json(probe, &plant, 5).expect_err("nobody is never bound");

    assert!(
        matches!(&err, DraftError::UnknownCapture { name, .. } if name == "nobody"),
        "got {err:?}"
    );
}

#[test]
fn a_capture_that_reads_a_step_that_is_not_an_earlier_request_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut probe = probe_json();
    probe["steps"][1]["from"] = json!("s9");

    let err = check_probe_json(probe, &plant, 5).expect_err("s9 is not a step");

    assert_eq!(err, DraftError::UnknownStep("s9".to_owned()));
}

/// The probe's two requests run after the two that f-alice's recipe sends, so the sequence is
/// four requests long. The probe alone would fit a limit of three.
#[test]
fn a_probe_that_sends_more_requests_than_the_run_allows_is_refused() {
    let plant = plant_with(fixed_machines());

    let err = check_probe_json(probe_json(), &plant, 3)
        .expect_err("four requests with the recipe are more than three allowed");

    assert_eq!(
        err,
        DraftError::Oversized {
            requests: 4,
            max: 3,
        }
    );
}

#[test]
fn a_probe_at_the_request_limit_is_accepted() {
    let plant = plant_with(fixed_machines());

    check_probe_json(probe_json(), &plant, 4).expect("four requests fit a limit of four");
}

#[test]
fn a_probe_may_use_a_capture_its_fixture_recipe_binds() {
    let plant = plant_with(fixed_machines());
    let mut probe = probe_json();
    probe["steps"][2]["request"]["headers"]["cookie"] = json!("session=${alice_session}");

    check_probe_json(probe, &plant, 5)
        .expect("the recipe binds alice_session before the probe runs");
}

#[test]
fn a_probe_that_reuses_a_step_id_of_its_fixture_recipe_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut probe = probe_json();
    probe["steps"][2]["id"] = json!("f2");

    let err = check_probe_json(probe, &plant, 5).expect_err("f2 is the recipe's login step");

    assert_eq!(err, DraftError::DuplicateStep("f2".to_owned()));
}

#[test]
fn a_probe_that_reuses_a_step_id_is_refused() {
    let plant = plant_with(fixed_machines());
    let mut probe = probe_json();
    probe["steps"][2]["id"] = json!("s1");

    let err = check_probe_json(probe, &plant, 5).expect_err("s1 is used twice");

    assert_eq!(err, DraftError::DuplicateStep("s1".to_owned()));
}

#[tokio::test]
async fn an_unreachable_provider_fails_the_foreman_call_visibly() {
    let url = unreachable_url().await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let (_demo, _order, _machines, request) = split_case();

    let err = models
        .chat_json::<SplitReply>(Role::Foreman, request)
        .await
        .expect_err("a failed foreman call is an error, not an offline split");

    assert!(matches!(err, LlmError::Transport { .. }), "got {err:?}");
    assert!(
        models.ledger().records().is_empty(),
        "no call reached a provider, so none is metered"
    );
}

#[tokio::test]
async fn a_missing_key_fails_the_foreman_call_visibly() {
    let url = unreachable_url().await;
    let models = Models::live(config(&url), keys(false)).expect("the client builds");
    let (_demo, _order, _machines, request) = split_case();

    let err = models
        .chat_json::<SplitReply>(Role::Foreman, request)
        .await
        .expect_err("a missing key is an error, not an offline split");

    assert!(matches!(err, LlmError::MissingKey { .. }), "got {err:?}");
}

#[tokio::test]
async fn a_reply_that_breaks_the_schema_fails_the_foreman_call_and_is_metered() {
    let url = serve_reply("this is prose, not the split the schema asks for").await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let (_demo, _order, _machines, request) = split_case();

    let err = models
        .chat_json::<SplitReply>(Role::Foreman, request)
        .await
        .expect_err("a reply that breaks the schema fails the call");

    assert!(
        matches!(err, LlmError::SchemaValidation { .. }),
        "got {err:?}"
    );
    let records = models.ledger().records();
    assert_eq!(records.len(), 1, "the live call that answered is metered");
    assert_eq!(records[0].provider, "baseten");
    assert_eq!(records[0].usage.prompt_tokens, 120);
    assert_eq!(records[0].usage.completion_tokens, 30);
}

#[tokio::test]
async fn an_unreachable_provider_never_turns_a_scope_draft_into_an_offline_one() {
    let url = unreachable_url().await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let plant = plant_with(fixed_machines());
    let request = model::scope_request(RATE_LIMIT_ORDER, &plant.bounds(5), None);

    let err = models
        .chat_json::<DraftReply>(Role::Foreman, request)
        .await
        .expect_err("a scope draft fails visibly when the foreman cannot answer");

    assert!(matches!(err, LlmError::Transport { .. }), "got {err:?}");
    assert!(
        models.ledger().records().is_empty(),
        "no offline draft was metered"
    );
}

#[test]
fn the_scope_request_lists_the_steps_and_side_effects_the_corpus_defines() {
    let plant = plant_with(fixed_machines());
    let request = model::scope_request(RATE_LIMIT_ORDER, &plant.bounds(5), None);
    let content = request
        .messages
        .iter()
        .find_map(|message| match message {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .expect("the request has a user message");
    let context: Value = serde_json::from_str(&content).expect("the context is JSON");

    let steps = context["steps"].as_array().expect("the steps are a list");
    assert!(
        steps.contains(&json!("s2")),
        "the signup probe's step: {steps:?}"
    );
    assert!(
        !steps.contains(&json!("login-step")),
        "an undefined step is never offered: {steps:?}"
    );
    let effects = context["side_effects"]
        .as_array()
        .expect("the side effects are a list");
    assert!(
        effects.contains(&json!({
            "step_id": "s2",
            "sink": "store",
            "after": { "method": "POST", "route_template": "/api/signup" }
        })),
        "the signup store read is offered with its request: {effects:?}"
    );
}

#[test]
fn the_scope_request_states_the_side_effect_rule_and_carries_the_last_error() {
    let plant = plant_with(fixed_machines());
    let refusal = "selector is malformed: \"channel SideEffect with field_path None\"";
    let retry = model::scope_request(RATE_LIMIT_ORDER, &plant.bounds(5), Some(refusal));
    let first = model::scope_request(RATE_LIMIT_ORDER, &plant.bounds(5), None);

    assert!(
        model::SCOPE_PROMPT.contains(
            "A side_effect selector needs a field_path that starts with store or outbound_http."
        ),
        "the prompt states the rule"
    );
    let context = |request: &acan_llm::ChatRequest| -> Value {
        let content = request
            .messages
            .iter()
            .find_map(|message| match message {
                acan_llm::Message::User { content } => Some(content.clone()),
                _ => None,
            })
            .expect("the request has a user message");
        serde_json::from_str(&content).expect("the context is JSON")
    };
    let schema = context(&retry)["schema"].to_string();
    assert!(
        schema.contains("starts with its sink, `store` or `outbound_http`"),
        "the schema states the side_effect path rule: {schema}"
    );
    assert_eq!(context(&retry)["previous_error"], refusal);
    assert!(
        context(&first)["previous_error"].is_null(),
        "a first attempt carries no error"
    );
}

#[tokio::test]
async fn an_offline_client_gives_no_scope_draft() {
    let models = Models::load(Path::new("no-such-routing-file.toml")).expect("the offline client");
    let plant = plant_with(fixed_machines());
    let request = model::scope_request(RATE_LIMIT_ORDER, &plant.bounds(5), None);

    let err = models
        .chat_json::<DraftReply>(Role::Foreman, request)
        .await
        .expect_err("an offline client has no scope to draft");

    assert!(
        matches!(err, LlmError::NoScriptedReply { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn an_empty_scope_reply_is_a_provider_error_and_not_an_empty_scope() {
    let url = serve_reply("").await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let plant = plant_with(fixed_machines());
    let request = model::scope_request(RATE_LIMIT_ORDER, &plant.bounds(5), None);

    let err = models
        .chat_json::<DraftReply>(Role::Foreman, request)
        .await
        .expect_err("an empty reply is not a scope");

    assert!(
        matches!(err, LlmError::SchemaValidation { .. }),
        "got {err:?}"
    );
}

/// A scope reply cut at the output cap is refused before it is parsed, and the error names the
/// cap, so a cut draft is never read as a shorter one.
#[tokio::test]
async fn a_scope_draft_cut_at_the_output_cap_fails_the_foreman_call_and_names_the_cap() {
    let url = serve_reply_capped(r#"{"may": [{"id": "limit-get-links""#).await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let plant = plant_with(fixed_machines());
    let request = model::scope_request(RATE_LIMIT_ORDER, &plant.bounds(5), None);

    let err = models
        .chat_json::<DraftReply>(Role::Foreman, request)
        .await
        .expect_err("a reply cut at the cap is not a scope");

    assert!(
        matches!(
            err,
            LlmError::OutputCapped { max_tokens: Some(cap), .. } if cap == model::DRAFT_MAX_TOKENS
        ),
        "got {err:?}"
    );
    assert!(
        err.to_string()
            .contains(&format!("{}-token output cap", model::DRAFT_MAX_TOKENS)),
        "the error names the cap: {err}"
    );
    assert_eq!(
        models.ledger().records().len(),
        1,
        "the call that was cut is metered"
    );
}

/// The draft's output cap reaches the provider. Muse Spark spends hidden reasoning tokens from
/// the cap, and a cap of 64 returned no text in receipt P00F, so the draft asks for a generous one.
#[tokio::test]
async fn a_draft_request_reaches_the_provider_with_a_generous_output_cap() {
    let (url, sent) = serve_reply_recording(&fixture_scope().to_string()).await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let plant = plant_with(fixed_machines());
    let request = model::scope_request(RATE_LIMIT_ORDER, &plant.bounds(5), None);

    models
        .chat_json::<DraftReply>(Role::Foreman, request)
        .await
        .expect("the provider's scope reply is an object");

    let body = sent
        .try_recv()
        .expect("the provider received the draft request");
    assert_eq!(
        body["max_tokens"],
        json!(model::DRAFT_MAX_TOKENS),
        "the cap is sent under the provider's default field"
    );
}

#[tokio::test]
async fn an_explicit_empty_scope_reply_is_refused_by_acan() {
    let content = json!({ "may": [], "must_keep": [], "open": [] }).to_string();
    let url = serve_reply(&content).await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let plant = plant_with(fixed_machines());
    let request = model::scope_request(RATE_LIMIT_ORDER, &plant.bounds(5), None);

    let structured = models
        .chat_json::<DraftReply>(Role::Foreman, request)
        .await
        .expect("the reply is an object");
    let err = model::scope_from_reply(structured.value, model_ref(), &plant.bounds(5))
        .expect_err("a scope with no may items is refused");

    assert_eq!(err, DraftError::EmptyScope);
}

#[tokio::test]
async fn a_scope_reply_that_sets_a_count_is_refused() {
    let mut scope = fixture_scope();
    scope["counts"] = json!(3);
    let url = serve_reply(&scope.to_string()).await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let plant = plant_with(fixed_machines());
    let request = model::scope_request(RATE_LIMIT_ORDER, &plant.bounds(5), None);

    let structured = models
        .chat_json::<DraftReply>(Role::Foreman, request)
        .await
        .expect("the reply is an object");
    let err = model::scope_from_reply(structured.value, model_ref(), &plant.bounds(5))
        .expect_err("a scope that sets a count is refused");

    assert!(
        matches!(&err, DraftError::Malformed(detail) if detail.contains("`counts`")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_probe_that_sets_an_expected_value_mask_class_or_count_is_refused() {
    let plant = plant_with(fixed_machines());
    for key in ["expected", "mask", "class", "count"] {
        let mut probe = probe_json();
        probe[key] = json!(1);
        let url = serve_reply(&probe.to_string()).await;
        let models = Models::live(config(&url), keys(true)).expect("the client builds");
        let request = model::probe_request(RATE_LIMIT_ORDER, &plant.bounds(5));

        let structured = models
            .chat_json::<DraftReply>(Role::Foreman, request)
            .await
            .expect("the reply is an object");
        let err = model::probe_from_reply(structured.value, model_ref(), &plant.bounds(5))
            .expect_err("a probe that sets an expectation field is refused");

        let field = format!("`{key}`");
        assert!(
            matches!(&err, DraftError::Malformed(detail) if detail.contains(&field)),
            "{key}: got {err:?}"
        );
    }
}

#[tokio::test]
async fn a_probe_step_that_sets_an_expected_value_is_refused() {
    let mut probe = probe_json();
    probe["steps"][0]["expected"] = json!(200);
    let url = serve_reply(&probe.to_string()).await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let plant = plant_with(fixed_machines());
    let request = model::probe_request(RATE_LIMIT_ORDER, &plant.bounds(5));

    let structured = models
        .chat_json::<DraftReply>(Role::Foreman, request)
        .await
        .expect("the reply is an object");
    let err = model::probe_from_reply(structured.value, model_ref(), &plant.bounds(5))
        .expect_err("a step that sets an expected value is refused");

    assert!(
        matches!(&err, DraftError::Malformed(detail) if detail.contains("`expected`")),
        "got {err:?}"
    );
}

#[test]
fn a_scope_reply_in_the_schema_shape_becomes_a_proposal_through_acan() {
    let plant = plant_with(fixed_machines());

    let draft = model::scope_from_reply(DraftReply(fixture_scope()), model_ref(), &plant.bounds(5))
        .expect("the fixture reply is a valid scope");

    assert_eq!(draft.proposal().may.len(), 3);
}

#[tokio::test]
async fn an_explain_the_live_call_cannot_answer_is_labelled_offline() {
    let url = unreachable_url().await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let request = prompt_request(model::EXPLAIN_PROMPT, "{}");

    let reply = models
        .chat_json::<model::ExplainReply>(Role::Fast, request)
        .await
        .expect("the offline explanation answers a non-foreman call");

    assert!(
        reply.value.breaks.is_none(),
        "the offline explanation names no witness"
    );
    let records = models.ledger().records();
    assert_eq!(records.len(), 1, "the answer is metered once");
    assert_eq!(
        records[0].provider, "offline",
        "the answer is marked offline"
    );
    assert_eq!(records[0].role, Role::Fast);
}

#[tokio::test]
async fn a_review_is_refused_by_the_live_and_the_offline_client() {
    let review = prompt_request(model::REVIEW_PROMPT, "{}");
    let content = json!({ "readbacks": [], "question": null }).to_string();
    let url = serve_reply(&content).await;
    let live = Models::live(config(&url), keys(true)).expect("the client builds");

    let err = live
        .chat_json::<Value>(Role::Foreman, review.clone())
        .await
        .expect_err("the live server sends no review");

    assert!(
        matches!(err, LlmError::NoScriptedReply { .. }),
        "got {err:?}"
    );
    assert!(live.ledger().records().is_empty(), "no provider was asked");

    let offline = Models::load(Path::new("no-such-routing-file.toml")).expect("the offline client");
    let err = offline
        .chat_json::<Value>(Role::Foreman, review)
        .await
        .expect_err("the offline client sends no review either");

    assert!(
        matches!(err, LlmError::NoScriptedReply { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_live_reply_that_matches_the_schema_is_used_as_live() {
    let content = json!({
        "tasks": [{ "title": "Read the greeting", "brief": "Read it from settings", "scope": ["ui"] }]
    })
    .to_string();
    let url = serve_reply(&content).await;
    let models = Models::live(config(&url), keys(true)).expect("the client builds");
    let (_demo, _order, _machines, request) = split_case();

    let split = models
        .chat_json::<SplitReply>(Role::Foreman, request)
        .await
        .expect("the live reply parses");

    assert_eq!(
        split.value.tasks[0].title, "Read the greeting",
        "the live reply is used"
    );
    let records = models.ledger().records();
    assert_eq!(records.len(), 1, "one live call, no fallback");
    assert_eq!(records[0].provider, "baseten");
    assert_eq!(records[0].model, "meta-models/muse-spark-open-1026");
}

/// Reads and decodes one JSON file, panicking with its path when either step fails.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("decoding {}: {err}", path.display()))
}

/// The live proof. One real Muse Spark call drafts the scope of the rate-limit order, and Acan
/// checks the draft against the demo repo's own machines, probe corpus and app contract. It spends
/// a real call, so it runs only when `ACAN_LIVE_PROOF` and `ACAN_DEMO` are both set. The test
/// reads the demo repo and never writes to it. The keys come from the `.env` copied into the
/// worktree, and nothing is printed.
#[tokio::test]
async fn muse_spark_drafts_the_scope_of_the_rate_limit_order() {
    let (Some(_), Some(demo)) = (
        std::env::var_os("ACAN_LIVE_PROOF"),
        std::env::var_os("ACAN_DEMO"),
    ) else {
        eprintln!("skipped: set ACAN_LIVE_PROOF and ACAN_DEMO to spend one live call");
        return;
    };
    let llm_dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../acan-llm"));
    dotenvy::from_path(llm_dir.join(".env")).expect("the worktree holds a copy of .env");
    let config =
        LlmConfig::load(llm_dir.join("llm.toml")).expect("the worktree holds a copy of llm.toml");
    let keys = ApiKeys::from_env(&config);
    let client = OpenAiCompatible::new(config, keys).expect("the client builds");

    let demo = Path::new(&demo);
    let scan = acan_server::scan(demo).expect("the demo repo scans");
    let machines = model::plant_machines(&scan.index);
    let corpus: ProbeCorpus = read_json(&demo.join(".acan").join("probes").join("core.json"));
    let app: AppContract = read_json(&demo.join(".acan").join("app.json"));
    let bounds = DraftBounds {
        machines: &machines,
        routes: &corpus.routes,
        fixtures: &corpus.fixtures,
        probes: &corpus.probes,
        limits: app.limits,
    };
    let request = model::scope_request(RATE_LIMIT_ORDER, &bounds, None);

    let traced = client
        .chat_json_traced::<DraftReply>(Role::Foreman, request)
        .await
        .expect("the live foreman call succeeds");
    println!(
        "LIVE_PROOF provider={} model={} reply_model={:?} request_id={:?} usage={:?}",
        traced.value.call.provider,
        traced.value.call.model,
        traced.reply_model,
        traced.request_id,
        traced.value.call.usage,
    );
    let model_name = traced
        .reply_model
        .clone()
        .unwrap_or_else(|| traced.value.call.model.clone());
    let echoed = ModelRef::from_str(&model_name).expect("the echoed model id is valid");

    let draft = model::scope_from_reply(traced.value.value, echoed, &bounds)
        .expect("Acan accepts the live draft");
    println!(
        "LIVE_PROOF_DRAFT {}",
        serde_json::to_string(draft.proposal()).expect("the draft serializes")
    );
    assert!(
        !draft.proposal().may.is_empty(),
        "the draft has may-change items"
    );
}

/// The app contract a scope draft reads from the protected branch.
const APP_CONTRACT: &str = include_str!("../../acan-intent/fixtures/app-contract.json");

fn owner() -> MemberId {
    MemberId::from_str("owner").expect("a member id")
}

/// Runs git in `dir`, failing the test on a non-zero exit.
fn git(dir: &Path, args: &[&str]) {
    let out = Process::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A host over a git checkout whose protected branch holds the app contract and the probe corpus,
/// and whose foreman is `client`. The checkout has no code, so its plant has no machines.
async fn scope_host(root: &Path, client: Stub) -> Host<Stub> {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Tester"]);
    git(root, &["config", "user.email", "tester@example.com"]);
    fs::create_dir_all(root.join(".acan/probes")).expect("the protected directory");
    fs::write(root.join(".acan/app.json"), APP_CONTRACT).expect("the app contract");
    fs::write(root.join(".acan/probes/core.json"), CORPUS).expect("the probe corpus");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "protected inputs"]);

    let state = root.join(".acan");
    let store = EventStore::open(&state.join("world.db"))
        .await
        .expect("the event store opens");
    let (origin, opened) = Origin::open(
        WorldRef {
            id: WorldId::from_str("scope-draft").expect("a world id"),
            repo: root.display().to_string(),
        },
        owner(),
        Timestamp::from_millis(1),
    );
    store
        .append(&[opened])
        .await
        .expect("the open event stores");
    let (tokens, _) = Tokens::load_or_create(&state, &owner()).expect("the tokens load");
    let (_shutdown, shutdown) = tokio::sync::watch::channel(false);
    let repo = Repo::open_with_protected(root, "main")
        .await
        .expect("the checkout opens");
    Host::new(HostParts {
        origin,
        store,
        tokens,
        scan: scan(root).expect("the scan succeeds"),
        ledger: UsageLedger::default(),
        client,
        repo: root.to_path_buf(),
        shutdown,
        git: Some(repo),
        witness_http_base_url: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        probe_concurrency: DEFAULT_PROBE_CONCURRENCY,
        vision: None,
    })
}

/// Posts one order on the `ui` machine as the owner, so its scope draft job is derived. Returns
/// the order and its revision.
async fn post_order(host: &Host<Stub>) -> (OrderId, OrderRev) {
    let events = host
        .submit(Command::Human {
            member: owner(),
            cmd: HumanCmd::PostOrder {
                machine: support::machine(support::UI),
                sentence: RATE_LIMIT_ORDER.to_owned(),
                witnesses: Vec::new(),
            },
        })
        .await
        .expect("the order posts");
    let order = events
        .iter()
        .find_map(|stamped| match &stamped.event {
            Event::OrderPosted { order, .. } => Some(*order),
            _ => None,
        })
        .expect("the order is posted");
    let rev = host.read(|world| world.order(order).expect("the order exists").rev);
    (order, rev)
}

/// Stops the job loop, then drains the writer, so nothing writes after this returns.
async fn stop(host: &Host<Stub>, jobs: JoinHandle<()>) {
    jobs.abort();
    let _ = jobs.await;
    host.shutdown().await.expect("the writer drains");
}

/// The log's events, polled until `done` holds. The job loop writes in the background, so a test
/// waits for the events it expects, and fails after ten seconds.
async fn logged_until(host: &Host<Stub>, done: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    let started = Instant::now();
    loop {
        let events: Vec<Event> = host
            .store()
            .load_from(Seq::new(0))
            .await
            .expect("the stored log reads")
            .into_iter()
            .map(|stamped| stamped.event)
            .collect();
        if done(&events) {
            return events;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the expected events never reached the log: {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A foreman draft that is a valid scope reply is metered under the scope step, with the order and
/// revision it drafts. The plant of this checkout has no machines, so Acan refuses the scope after
/// the call; the call is still recorded.
#[tokio::test]
async fn a_scope_draft_records_model_used_with_step_scope() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let scope = fixture_scope().to_string();
    let client = Stub::default().when(Role::Foreman, |_| true, Reply::text(scope));
    let host = scope_host(dir.path(), client).await;
    let (order, rev) = post_order(&host).await;
    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();

    let events = logged_until(&host, |events| {
        events.iter().any(|event| {
            matches!(
                event,
                Event::ModelUsed {
                    step: Step::Scope,
                    ..
                }
            )
        })
    })
    .await;
    let calls = host.client().ledger().records();
    stop(&host, jobs).await;

    let (model, provider, failure, input, output, detail) = events
        .iter()
        .find_map(|event| match event {
            Event::ModelUsed {
                model,
                provider,
                step: Step::Scope,
                input_tokens,
                output_tokens,
                failure,
                detail,
            } => Some((
                model.clone(),
                provider.clone(),
                *failure,
                *input_tokens,
                *output_tokens,
                detail.clone(),
            )),
            _ => None,
        })
        .expect("the draft's call is recorded");
    let call = calls.first().expect("the stub counted the foreman call");
    assert_eq!(
        model, call.model,
        "the record names the model that answered"
    );
    assert_eq!(
        provider.as_deref(),
        Some(call.provider.as_str()),
        "the record names the provider that answered"
    );
    assert_eq!(failure, None, "a reply is not a failure");
    assert_eq!(
        (input, output),
        (
            Some(call.usage.prompt_tokens),
            Some(call.usage.completion_tokens)
        ),
        "the reply's tokens are recorded"
    );
    assert_eq!(
        detail,
        Some(format!("scope draft for {order} rev {}", rev.get())),
        "the record says which draft the call was for"
    );
}

/// A foreman reply that is not a scope is still a call: it is recorded under the scope step, and
/// the draft then fails and is reported as `ScopeDraftFailed`, as before.
#[tokio::test]
async fn a_failed_scope_draft_still_records_its_call() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let client = Stub::default().when(
        Role::Foreman,
        |_| true,
        Reply::text("this is prose, not the scope draft the schema asks for"),
    );
    let host = scope_host(dir.path(), client).await;
    let (order, rev) = post_order(&host).await;
    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();

    let events = logged_until(&host, |events| {
        events
            .iter()
            .any(|event| matches!(event, Event::ScopeDraftFailed { .. }))
    })
    .await;
    stop(&host, jobs).await;

    let expected = format!("scope draft for {order} rev {}", rev.get());
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ModelUsed { step: Step::Scope, detail, .. }
                if detail.as_deref() == Some(expected.as_str())
        )),
        "the failed draft's call is recorded: {events:?}"
    );
}

/// A foreman that times out is recorded as a failed call under the scope step, with the provider and
/// model of its route, no token counts, and a detail that names the timeout. The draft is attempted
/// five times and each attempt is one call, so the log holds five such entries. A failed call counts
/// nothing: the usage totals and the ledger stay empty.
#[tokio::test]
async fn a_scope_draft_timeout_records_a_failed_entry_with_provider_and_model() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let route = Destination {
        provider: "example-host".to_owned(),
        model: "example-model-1".to_owned(),
    };
    let client = Stub::default().routed(Role::Foreman, route).fails_when(
        Role::Foreman,
        |_| true,
        |provider| LlmError::Timeout { provider },
    );
    let host = scope_host(dir.path(), client).await;
    let (order, rev) = post_order(&host).await;
    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();

    // The failure is reported only after the last attempt, so every attempt is in the log by then.
    let events = logged_until(&host, |events| {
        events
            .iter()
            .any(|event| matches!(event, Event::ScopeDraftFailed { .. }))
    })
    .await;
    let totals = host.read(|world| world.usage().totals.clone());
    let ledger = host.client().ledger().records();
    stop(&host, jobs).await;

    let expected = Event::ModelUsed {
        model: "example-model-1".to_owned(),
        provider: Some("example-host".to_owned()),
        step: Step::Scope,
        input_tokens: None,
        output_tokens: None,
        failure: Some(CallFailure::Timeout),
        detail: Some(format!(
            "scope draft for {order} rev {}: timeout",
            rev.get()
        )),
    };
    let entries: Vec<&Event> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::ModelUsed {
                    step: Step::Scope,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(
        entries.len(),
        5,
        "each of the draft's five attempts is one failed call: {events:?}"
    );
    for entry in entries {
        assert_eq!(entry, &expected);
    }
    assert!(
        totals.is_empty(),
        "a failed call counts nothing: {totals:?}"
    );
    assert!(
        ledger.is_empty(),
        "a failed call sends no reply: {ledger:?}"
    );
}

/// A foreman whose client refuses the call before any request, for want of a key, made no call. No
/// usage row is recorded for it, and the draft still fails as reported.
#[tokio::test]
async fn a_scope_draft_refused_before_its_request_records_no_usage() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let route = Destination {
        provider: "example-host".to_owned(),
        model: "example-model-1".to_owned(),
    };
    let client = Stub::default().routed(Role::Foreman, route).fails_when(
        Role::Foreman,
        |_| true,
        |provider| LlmError::MissingKey {
            provider,
            env_var: "EXAMPLE_KEY".to_owned(),
        },
    );
    let host = scope_host(dir.path(), client).await;
    post_order(&host).await;
    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();

    let events = logged_until(&host, |events| {
        events
            .iter()
            .any(|event| matches!(event, Event::ScopeDraftFailed { .. }))
    })
    .await;
    let totals = host.read(|world| world.usage().totals.clone());
    let ledger = host.client().ledger().records();
    stop(&host, jobs).await;

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Event::ModelUsed { .. })),
        "a call refused before its request is not recorded: {events:?}"
    );
    assert!(totals.is_empty(), "no call counts: {totals:?}");
    assert!(
        ledger.is_empty(),
        "no request reached the provider: {ledger:?}"
    );
}

/// The clock the explain setup and its host share.
fn clock() -> Timestamp {
    Timestamp::from_millis(1_800_000_000_000)
}

/// Runs one setup command through `origin` against the demo's code index, and stores what the
/// origin accepted. The host starts from this log, so its events and its origin agree.
async fn settle(
    origin: &mut Origin,
    store: &EventStore,
    index: &CodeIndex,
    cmd: Command,
) -> Vec<Stamped> {
    let ctx = Ctx {
        now: clock(),
        index,
    };
    let events = origin
        .execute(&ctx, cmd)
        .unwrap_or_else(|rejection| panic!("the origin refused a setup command: {rejection}"));
    store.append(&events).await.expect("the setup events store");
    events
}

/// A host whose log holds one clash: two tasks of one order, each read back, each claimed, and
/// both declaring the same symbol. The foreman is `client`. Returns the host and the clash.
async fn clashed_host(root: &Path, client: Stub) -> (Host<Stub>, ClashId) {
    let state = root.join(".acan");
    fs::create_dir_all(&state).expect("the state directory");
    let store = EventStore::open(&state.join("world.db"))
        .await
        .expect("the event store opens");
    let index = support::code_index();
    let (mut origin, opened) = Origin::open(
        WorldRef {
            id: WorldId::from_str("clash-explain").expect("a world id"),
            repo: root.display().to_string(),
        },
        owner(),
        clock(),
    );
    store
        .append(&[opened])
        .await
        .expect("the open event stores");

    let posted = settle(
        &mut origin,
        &store,
        &index,
        Command::Human {
            member: owner(),
            cmd: HumanCmd::PostOrder {
                machine: support::machine(support::UI),
                sentence: ORDER_SENTENCE.to_owned(),
                witnesses: Vec::new(),
            },
        },
    )
    .await;
    let order = posted
        .iter()
        .find_map(|stamped| match &stamped.event {
            Event::OrderPosted { order, .. } => Some(*order),
            _ => None,
        })
        .expect("posting an order emits OrderPosted");

    let rev = origin.world().order(order).expect("the order exists").rev;
    let spec = |title: &str| TaskSpec {
        title: title.to_owned(),
        brief: format!("{title}, then run the greet tests"),
        scope: vec![support::machine(support::UI)],
    };
    settle(
        &mut origin,
        &store,
        &index,
        Command::System(SystemCmd::SplitReady {
            order,
            rev,
            tasks: vec![spec("Read the greeting"), spec("Add the setting")],
        }),
    )
    .await;
    let tasks: [TaskId; 2] = origin
        .world()
        .tasks_of(order)
        .map(|task| task.id)
        .collect::<Vec<_>>()
        .try_into()
        .expect("the split makes two tasks");

    let agents = [
        AgentId::from_str("agent-a").expect("a valid agent id"),
        AgentId::from_str("agent-b").expect("a valid agent id"),
    ];
    let mut readbacks = BTreeSet::new();
    for (agent, task) in agents.iter().zip(tasks) {
        settle(
            &mut origin,
            &store,
            &index,
            Command::Human {
                member: owner(),
                cmd: HumanCmd::RegisterAgent {
                    agent: agent.clone(),
                },
            },
        )
        .await;
        let posted = settle(
            &mut origin,
            &store,
            &index,
            Command::Agent {
                agent: agent.clone(),
                cmd: AgentCmd::PostReadback {
                    task,
                    summary: "Read the greeting from the settings file.".to_owned(),
                    touches: vec![Locus::Machine {
                        machine: support::machine(support::UI),
                    }],
                },
            },
        )
        .await;
        readbacks.insert(
            posted
                .iter()
                .find_map(|stamped| match &stamped.event {
                    Event::ReadbackPosted { readback } => Some(readback.id),
                    _ => None,
                })
                .expect("posting a readback emits ReadbackPosted"),
        );
    }
    let rev = origin.world().order(order).expect("the order exists").rev;
    let verdicts = readbacks
        .iter()
        .map(|readback| {
            (
                *readback,
                ReadbackVerdict {
                    rev,
                    per_witness: BTreeMap::new(),
                },
            )
        })
        .collect();
    settle(
        &mut origin,
        &store,
        &index,
        Command::System(SystemCmd::ReadbacksReviewed {
            order,
            rev,
            verdicts,
            question: None,
        }),
    )
    .await;

    for (agent, task) in agents.iter().zip(tasks) {
        settle(
            &mut origin,
            &store,
            &index,
            Command::Agent {
                agent: agent.clone(),
                cmd: AgentCmd::Claim { task },
            },
        )
        .await;
        let fence = match &origin.world().task(task).expect("the task exists").state {
            TaskState::Claimed { claim, .. } => claim.fence(task),
            other => panic!("the claim left {task} as {other:?}"),
        };
        settle(
            &mut origin,
            &store,
            &index,
            Command::Agent {
                agent: agent.clone(),
                cmd: AgentCmd::Declare {
                    fence,
                    footprint: DeclarationInput {
                        symbols: vec![support::greet_symbol()],
                        ..DeclarationInput::default()
                    },
                },
            },
        )
        .await;
    }
    let clash = origin
        .world()
        .clashes()
        .next()
        .map(|clash| clash.id)
        .expect("two declarations of one symbol raise a clash");

    let (tokens, _) = Tokens::load_or_create(&state, &owner()).expect("the tokens load");
    let (_shutdown, shutdown) = tokio::sync::watch::channel(false);
    let host = Host::new(HostParts {
        origin,
        store,
        tokens,
        scan: scan(root).expect("the scan succeeds"),
        ledger: UsageLedger::default(),
        client,
        repo: root.to_path_buf(),
        shutdown,
        git: None,
        witness_http_base_url: None,
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        probe_concurrency: DEFAULT_PROBE_CONCURRENCY,
        vision: None,
    });
    (host, clash)
}

/// A foreman reply to an explain call, whose consequence is `consequence`.
fn explain_reply(consequence: &str) -> Reply {
    Reply::text(
        json!({ "consequence": consequence, "breaks": null, "suggested": null }).to_string(),
    )
}

/// How many explain calls the log records, each a `ModelUsed` under the explain step.
fn explain_entries(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::ModelUsed {
                    step: Step::Explain,
                    ..
                }
            )
        })
        .count()
}

/// An explanation over the consequence cap is refused, so the explain job fails and takes its
/// retry. The job makes five attempts, the limit of its retries, and each attempt records its
/// usage before the reply is read. No clash explanation reaches the log.
#[tokio::test]
async fn an_explanation_over_sixty_words_fails_the_job_and_is_retried() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let too_long = vec!["word"; 61].join(" ");
    let client = Stub::default().when(Role::Fast, |_| true, explain_reply(&too_long));
    let (host, _clash) = clashed_host(dir.path(), client).await;
    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();

    let events = logged_until(&host, |events| explain_entries(events) == 5).await;
    stop(&host, jobs).await;

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Event::ClashExplained { .. })),
        "a refused explanation is never recorded: {events:?}"
    );
}

/// An explanation at the consequence cap is recorded as the clash's explanation.
#[tokio::test]
async fn an_explanation_at_sixty_words_is_recorded() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let at_cap = vec!["word"; 60].join(" ");
    let client = Stub::default().when(Role::Fast, |_| true, explain_reply(&at_cap));
    let (host, clash) = clashed_host(dir.path(), client).await;
    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();

    let events = logged_until(&host, |events| {
        events
            .iter()
            .any(|event| matches!(event, Event::ClashExplained { .. }))
    })
    .await;
    stop(&host, jobs).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ClashExplained { clash: recorded, explanation }
                if *recorded == clash && explanation.consequence == at_cap
        )),
        "the explanation at the cap is recorded: {events:?}"
    );
}

/// A scope draft whose open question is forty words is refused. Each retry carries the refusal as
/// its previous error, which names the field and its cap, so the model is told what to shorten.
#[tokio::test]
async fn a_scope_draft_with_a_forty_word_question_fails_with_the_cap_in_previous_error() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut scope = fixture_scope();
    scope["open"][0]["question"] = json!(vec!["word"; 40].join(" "));
    let drafts: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = Arc::clone(&drafts);
    let client = Stub::default().when(
        Role::Foreman,
        move |request| {
            let context = user_json(request);
            // Only a scope draft carries previous_error, so a split call is not counted here.
            let is_draft = context.get("previous_error").is_some();
            if is_draft {
                seen.lock()
                    .expect("the draft log is not poisoned")
                    .push(context);
            }
            is_draft
        },
        Reply::text(scope.to_string()),
    );
    let host = scope_host(dir.path(), client).await;
    post_order(&host).await;
    let jobs = spawn_jobs(host.clone());
    host.wake_jobs();

    logged_until(&host, |events| {
        events
            .iter()
            .any(|event| matches!(event, Event::ScopeDraftFailed { .. }))
    })
    .await;
    stop(&host, jobs).await;

    let drafts = drafts.lock().expect("the draft log is not poisoned");
    assert_eq!(drafts.len(), 5, "each of the five attempts sends the draft");
    assert!(
        drafts[0]["previous_error"].is_null(),
        "a first attempt carries no error"
    );
    for retry in &drafts[1..] {
        let previous = retry["previous_error"]
            .as_str()
            .expect("a retry carries the last error");
        assert!(
            previous.contains("open question has 40 words, more than the 30 allowed"),
            "the retry names the field and the cap: {previous}"
        );
    }
}
