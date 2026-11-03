//! Prompts, reply shapes, and the mapping from a model reply to the commands the origin accepts.
//!
//! Ids cross the model boundary in their display form (`t-3`, `w-1`). Counter ids serialize as
//! bare numbers, so replies are parsed from these strings.
//!
//! Scope and probe drafts are different: a reply becomes an `acan_intent` proposal that only a
//! human can adopt. [`scope_draft`] and [`probe_draft`] refuse a draft that names a route, machine
//! or capture the plant does not have, a step id or side-effect witness the probe corpus does not
//! define, or that exceeds the run bounds. A failed foreman call for a draft is an error, never an
//! offline answer.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::str::FromStr;

use acan_core::{
    Clash, ClashId, CodeIndex, Explanation, MachineKey, OrderId, OrderRev, ParseIdError,
    Settlement, Stage, SystemCmd, Task, TaskId, TaskSpec, TaskState, Witness, WitnessId, World,
};
use acan_intent::{
    Channel, CorpusProbe, Evidence, Fixture, HttpMethod, MachineId, MethodSelector, ModelProposal,
    ModelRef, Operand, Predicate, ProbeProposal, RequestTemplate, RouteEntry, RouteTemplate,
    RunLimits, ScopeBody, ScopeItem, Selector, Step, StepContext, StepId, Template, TrustedSink,
    ValueRef, WindowAnchor, WindowObligation,
};
use acan_llm::{ChatRequest, Message};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::Deserialize;
use serde_json::{Value, json};

pub const SPLIT_PROMPT: &str = "You split one order into tasks for AI coding workers. Reply with one JSON object with a tasks list of two to four tasks. Each task has a title of a few words, a brief that a worker can start from without the order, and a scope key holding a list of the machines the task changes, even when there is one machine. Copy every scope name exactly from the machines list in the context. Split by intent: each task is one feature or concern that one worker can finish alone. Tasks may touch shared code when their feature needs it, because Acan detects overlapping changes and resolves them. Every witness must still hold once all the tasks have landed.";

pub const REVIEW_PROMPT: &str = "You review each worker readback against the order's witnesses. Reply with one JSON object with two keys. readbacks is a list with one entry per readback in the context. Each entry has a readback key, holding the readback id, and a witnesses key: a list with one entry per witness of the order. Each witness entry has a witness key, holding the witness id, and a judgement key. A judgement is an object with a judgement key, which is match when the readback respects the witness, disagrees when it contradicts the witness, or unclear when it does not say enough. Give a reason key for every judgement that is not match. Judge each readback on its own. question is null when every judgement is match. Otherwise write one only when the readbacks disagree about what the order means; then it has text and exactly two options.";

pub const EXPLAIN_PROMPT: &str = "You explain a clash between two tasks that declared overlapping changes. Reply with one JSON object with three keys. consequence is one or two plain sentences on what goes wrong if both proceed, naming the declared files and symbols the tasks share. breaks is the id of the witness that would break, such as w-2, or null when none would. suggested is one choice copied exactly from the menu, or null when no choice fits. Never suggest a choice that is not on the menu.";

/// The scope draft prompt. The reply is an `acan_intent::ScopeBody`, which Acan wraps as a
/// `ModelProposal` that gains no authority until a human confirms it.
pub const SCOPE_PROMPT: &str = "You draft the scope of one order for a code change. Reply with one JSON object that has three keys, may, must_keep and open, that matches the JSON schema under schema in the context. may lists what the order allows to change. Each may item names one HTTP route, copied exactly from the routes in the context, and the machines it touches, copied exactly from the machines in the context. Give every item at least one acceptance obligation, each with a predicate and one line of text. Give every item an example: one target value for one field of one step, written before any code exists. An example's expected status must be one that the item's acceptance allows that request to get. Set provenance to user when the order states the item, model when you interpret the order, and open when a human must still answer it. must_keep lists behavior the order must not break. open lists questions a human must answer, each with two or more options. Never state an observed value, a verdict or a pass. Never invent a route or a machine. A side_effect selector needs a field_path that starts with store or outbound_http. Its sink, method and route_template match one side_effects entry in the context, and its step_context is that entry's step_id or any. An item that changes a response status also covers that response's body and headers: give it channel any, or add body and header items on the same route, because a changed status usually changes them too. Name only step ids listed in the steps in the context. When previous_error in the context is not null, the last attempt failed for that reason: fix it and reply again.";

/// The probe prompt. The reply is an input-only `acan_intent::ProbeProposal`.
pub const PROBE_PROMPT: &str = "You propose input only probe steps for one order. Reply with one JSON object that has four keys, fixture, steps, route_template and rationale, that matches the JSON schema under schema in the context. fixture is one id copied from the fixtures in the context. steps is a non empty list of steps. An http step sends one request, and its path, headers and body may use ${name} for a value that an earlier capture step bound. A capture step reads a value from an earlier http step. An advance_time step moves the controlled clock. route_template is one route copied exactly from the routes in the context, and the probe sends no more http requests than max_requests. Propose inputs only. Never write an expected value, a count, a mask, a class, an observed value or a verdict. rationale is one sentence.";

/// Output budget for a draft. Muse Spark spends hidden reasoning tokens from this cap, and a cap
/// of 64 returned an empty reply (receipt P00F). A live scope draft spent 5687 reasoning tokens
/// and stopped at an 8000 cap mid-reply (finish_reason `length`), so drafts get 16000.
pub const DRAFT_MAX_TOKENS: u32 = 16_000;

/// Word caps on the text a person reads. A reply over its cap is refused, so the job fails and
/// takes its usual retry. Each cap is at least the length its prompt asks for: an explanation is
/// one or two sentences, and an obligation is one line.
pub(crate) mod caps {
    /// The most words an explanation's consequence holds.
    pub const CONSEQUENCE_WORDS: usize = 60;
    /// The most words a question holds.
    pub const QUESTION_WORDS: usize = 30;
    /// The most words an option holds.
    pub const OPTION_WORDS: usize = 8;
    /// The most words an acceptance or must-keep obligation's line holds.
    pub const OBLIGATION_WORDS: usize = 30;
    /// The most words a card or a usage detail holds.
    pub const CARD_WORDS: usize = 100;

    /// The number of words in `text`. Text splits on whitespace, except that a run inside a
    /// backtick span is one word, so `a b c` counts once. A backtick with no partner is ordinary
    /// text. Backticks pair up in order, so the last one of an odd count has none.
    pub(crate) fn words(text: &str) -> usize {
        let ticks = text.matches('`').count();
        let mut seen = 0_usize;
        let mut in_span = false;
        let mut in_word = false;
        let mut count = 0_usize;
        for ch in text.chars() {
            if ch == '`' {
                seen += 1;
                // An odd backtick with a partner opens a span, and an even one closes it.
                in_span = seen % 2 == 1 && seen < ticks;
            }
            if ch.is_whitespace() && !in_span {
                in_word = false;
            } else if !in_word {
                in_word = true;
                count += 1;
            }
        }
        count
    }
}

/// Which job a model request was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ask {
    Split,
    Review,
    Explain,
    Scope,
    Probe,
}

/// The job `request` was built for, read from its system prompt.
pub(crate) fn ask_of(request: &ChatRequest) -> Option<Ask> {
    request.messages.iter().find_map(|message| match message {
        Message::System { content } => match content.as_str() {
            SPLIT_PROMPT => Some(Ask::Split),
            REVIEW_PROMPT => Some(Ask::Review),
            EXPLAIN_PROMPT => Some(Ask::Explain),
            SCOPE_PROMPT => Some(Ask::Scope),
            PROBE_PROMPT => Some(Ask::Probe),
            _ => None,
        },
        _ => None,
    })
}

/// Why a model reply cannot become a command.
#[derive(Debug, thiserror::Error)]
pub enum ReplyError {
    /// The reply names an id or machine that is not valid.
    #[error("reply names an invalid id: {0}")]
    Id(#[from] ParseIdError),
    /// The reply names a machine that is not on the plant.
    #[error("reply names machine {0:?}, which is not on the plant")]
    UnknownMachine(String),
    /// A split reply has no tasks, so the order would have nothing to do.
    #[error("split reply has no tasks")]
    NoTasks,
    /// A reply's text holds more words than its cap allows.
    #[error("{field} has {words} words, more than the {cap} allowed; shorten it")]
    TooLong {
        /// The field that is over its cap.
        field: &'static str,
        /// The words the field holds.
        words: usize,
        /// The most words the field may hold.
        cap: usize,
    },
}

/// What a draft must fit. The machines are the plant's, the routes and fixtures come from the
/// protected probe corpus, and the limits come from the protected app contract. A model never
/// sets any of them.
#[derive(Debug, Clone, Copy)]
pub struct DraftBounds<'a> {
    /// The plant's machines.
    pub machines: &'a [MachineKey],
    /// The app's route table, from the probe corpus.
    pub routes: &'a [RouteEntry],
    /// The fixtures the probe corpus defines. A probe runs after its fixture's recipe, so the
    /// recipe's requests, captures and step ids count against the probe.
    pub fixtures: &'a [Fixture],
    /// The probes of the corpus. A scope may name only the step ids these probes and their
    /// fixtures define, and a side effect only where one of these probes observes its sink after
    /// a request.
    pub probes: &'a [CorpusProbe],
    /// Per-sequence bounds from the app contract.
    pub limits: RunLimits,
}

/// Why a scope or probe draft is not a proposal Acan keeps.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DraftError {
    /// The draft has no may-change item. An empty reply is a provider error, never an empty scope.
    #[error("draft has no may-change items")]
    EmptyScope,
    /// The draft names a machine that is not on the plant.
    #[error("draft names machine {0:?}, which is not on the plant")]
    UnknownMachine(String),
    /// The draft names a route that is not in the app's route table.
    #[error("draft names route {0:?}, which is not in the route table")]
    UnknownRoute(String),
    /// The probe names a fixture that the corpus does not define.
    #[error("probe names fixture {0:?}, which the corpus does not define")]
    UnknownFixture(String),
    /// The scope names a step id that no probe or fixture recipe of the corpus defines.
    #[error(
        "scope names step {0:?}, which the probe corpus does not define; name a step from the steps in the context"
    )]
    UndefinedStep(String),
    /// The scope reads a side effect that no probe observes after a request to its route.
    #[error(
        "scope names a {sink} side effect after {route:?}, which no probe observes after a request to that route; name a side_effects entry from the context"
    )]
    UnwitnessedSideEffect { sink: String, route: String },
    /// The scope reads events of a sink that no probe of the corpus observes.
    #[error("scope reads {0} events, which no probe observes")]
    UnobservedSink(String),
    /// A status or body predicate reads a step that no probe sends to the rule's route, so it
    /// compares a request the rule does not cover.
    #[error(
        "scope reads step {step:?} for {route:?}, but no probe sends that step to that route; name a step that sends a request to {route:?}"
    )]
    StepOffRoute { step: String, route: String },
    /// An example expects a status that the item's window requires its request to receive.
    #[error(
        "example expects status {status} at step {step:?}, but the acceptance allows the first {allowed} requests of its window, so that request must not be rejected; expect the allowed status or name a later request"
    )]
    ExampleContradictsAcceptance {
        step: String,
        status: u16,
        allowed: u32,
    },
    /// A step uses a capture that no earlier capture step binds.
    #[error("step {step:?} uses capture {name:?}, which no earlier step binds")]
    UnknownCapture { step: String, name: String },
    /// A capture or side-effect step reads a step that is not an earlier http step.
    #[error("step {0:?} reads a step that is not an earlier http step")]
    UnknownStep(String),
    /// Two steps share an id.
    #[error("step id {0:?} is used twice")]
    DuplicateStep(String),
    /// The sequence sends more requests than the run allows, the fixture's recipe included.
    #[error("sequence sends {requests} requests, more than the {max} allowed")]
    Oversized { requests: usize, max: u32 },
    /// A template holds a placeholder that is not a valid capture name.
    #[error("template {0:?} has a malformed placeholder")]
    MalformedTemplate(String),
    /// A question, option label or obligation line holds more words than its cap allows. The
    /// retry carries this text as the previous error, so the model shortens the field it names.
    #[error("{field} has {words} words, more than the {cap} allowed; shorten it")]
    TooLong {
        /// The field that is over its cap.
        field: &'static str,
        /// The words the field holds.
        words: usize,
        /// The most words the field may hold.
        cap: usize,
    },
    /// The reply does not decode as the proposal type. Unknown fields are refused here, so a
    /// draft cannot set a count, mask, class or expected value.
    #[error("reply does not decode as a proposal: {0}")]
    Malformed(String),
}

fn request(system: &str, context: &Value) -> ChatRequest {
    ChatRequest {
        messages: vec![Message::system(system), Message::user(context.to_string())],
        ..ChatRequest::default()
    }
}

fn draft_request(system: &str, context: &Value) -> ChatRequest {
    ChatRequest {
        max_tokens: Some(DRAFT_MAX_TOKENS),
        ..request(system, context)
    }
}

fn witness_json(witness: &Witness) -> Value {
    json!({
        "id": witness.id.to_string(),
        "kind": witness.kind,
        "reason": witness.reason,
    })
}

fn witnesses_json(world: &World, ids: &[WitnessId]) -> Vec<Value> {
    ids.iter()
        .filter_map(|id| world.witness(*id))
        .map(witness_json)
        .collect()
}

fn task_json(task: &Task) -> Value {
    json!({
        "id": task.id.to_string(),
        "title": task.spec.title,
        "brief": task.spec.brief,
        "scope": task.spec.scope,
    })
}

/// A task with the footprint it declared. A clash only exists between declared tasks, so the
/// footprint is null only for a task that has since lost its declaration.
fn declared_task_json(task: &Task) -> Value {
    let declared = match &task.state {
        TaskState::Claimed {
            stage: Stage::Declared { decl } | Stage::Building { decl } | Stage::Gating { decl, .. },
            ..
        } => json!({
            "files": decl.footprint.files,
            "symbols": decl.footprint.symbols,
            "interfaces": decl.footprint.interfaces,
            "inserts": decl.footprint.inserts,
        }),
        _ => Value::Null,
    };
    let mut value = task_json(task);
    value["declared"] = declared;
    value
}

fn settlement_json(choice: &Settlement) -> Value {
    match choice {
        Settlement::Narrow { task } => json!({ "narrow": { "task": task.to_string() } }),
        Settlement::Serialize { first, then } => json!({
            "serialize": { "first": first.to_string(), "then": then.to_string() }
        }),
        Settlement::SplitAtContract { owner, contract } => json!({
            "split_at_contract": { "owner": owner.to_string(), "contract": contract }
        }),
    }
}

/// Every machine the code index names, in name order: the owner of each file and both ends of
/// each belt. The split prompt lists these, and a split may name only these.
#[must_use]
pub fn plant_machines(index: &CodeIndex) -> Vec<MachineKey> {
    let mut machines: BTreeSet<&MachineKey> =
        index.files.values().map(|file| &file.machine).collect();
    for belt in &index.belts {
        machines.insert(&belt.from);
        machines.insert(&belt.to);
    }
    machines.into_iter().cloned().collect()
}

/// The split prompt for `order`, or `None` when the order is gone. `machines` is the plant's
/// machine list, from [`plant_machines`].
#[must_use]
pub fn split_request(
    world: &World,
    order: OrderId,
    machines: &[MachineKey],
) -> Option<ChatRequest> {
    let order = world.order(order)?;
    Some(request(
        SPLIT_PROMPT,
        &json!({
            "order": order.sentence,
            "machine": order.machine,
            "machines": machines,
            "witnesses": witnesses_json(world, &order.witnesses),
        }),
    ))
}

/// The explain prompt for `clash`, or `None` when the clash or a party to it is gone. It carries
/// both tasks with their declared footprints, the order's witnesses, and the menu.
#[must_use]
pub fn explain_request(world: &World, clash: ClashId) -> Option<ChatRequest> {
    let clash: &Clash = world.clash(clash)?;
    let first = world.task(clash.key.a())?;
    let second = world.task(clash.key.b())?;
    let order = world.order(first.order)?;
    let menu: Vec<Value> = clash.menu().iter().map(settlement_json).collect();
    Some(request(
        EXPLAIN_PROMPT,
        &json!({
            "clash": clash.id.to_string(),
            "kind": clash.kind,
            "at": clash.key.at(),
            "order": order.sentence,
            "witnesses": witnesses_json(world, &order.witnesses),
            "tasks": [declared_task_json(first), declared_task_json(second)],
            "menu": menu,
        }),
    ))
}

/// A split reply: the tasks the foreman cut the order into.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SplitReply {
    /// Two to four tasks, each one feature or concern that one worker can finish alone. Tasks may
    /// touch shared code when their feature needs it: an overlap becomes a clash, not a refusal.
    pub tasks: Vec<SplitTask>,
}

/// One task in a split reply.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SplitTask {
    /// A short title of a few words.
    pub title: String,
    /// What the worker should do, complete enough to start without the order.
    pub brief: String,
    /// The machines this task changes. Copy each name exactly from the machines list in the
    /// context.
    pub scope: Vec<String>,
}

/// Maps a split reply to the `SplitReady` command for `order` at `rev`. Every scope name must be
/// one of `machines`.
///
/// # Errors
///
/// Returns [`ReplyError`] when the reply has no tasks, or names a machine that is not on the
/// plant.
pub fn split_result(
    order: OrderId,
    rev: OrderRev,
    machines: &[MachineKey],
    reply: SplitReply,
) -> Result<SystemCmd, ReplyError> {
    if reply.tasks.is_empty() {
        return Err(ReplyError::NoTasks);
    }
    let tasks = reply
        .tasks
        .into_iter()
        .map(|task| task_spec(task, machines))
        .collect::<Result<Vec<_>, ReplyError>>()?;
    Ok(SystemCmd::SplitReady { order, rev, tasks })
}

fn task_spec(task: SplitTask, machines: &[MachineKey]) -> Result<TaskSpec, ReplyError> {
    let scope = task
        .scope
        .iter()
        .map(|name| -> Result<MachineKey, ReplyError> {
            let machine = MachineKey::from_str(name)?;
            if machines.contains(&machine) {
                Ok(machine)
            } else {
                Err(ReplyError::UnknownMachine(name.clone()))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TaskSpec {
        title: task.title,
        brief: task.brief,
        scope,
    })
}

/// An explain reply: what goes wrong when both tasks proceed, and one choice if one fits.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExplainReply {
    /// What goes wrong if both tasks proceed, in one or two plain sentences.
    pub consequence: String,
    /// The witness id, such as `w-2`, that would break. Null when no witness would break.
    pub breaks: Option<String>,
    /// One choice from the menu, copied exactly. Null when no choice fits.
    pub suggested: Option<SettlementReply>,
}

/// A settlement as the model writes it. Task ids are copied from the context, such as `t-2`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SettlementReply {
    /// One task narrows its declaration so the overlap goes away.
    Narrow {
        /// The task that narrows.
        task: String,
    },
    /// One task builds after the other lands.
    Serialize {
        /// The task that goes first.
        first: String,
        /// The task that waits.
        then: String,
    },
    /// The owner lands the named contract first, and the other task builds against it.
    SplitAtContract {
        /// The task that owns the contract.
        owner: String,
        /// The contract name, copied from the menu.
        contract: String,
    },
}

/// Maps an explain reply to an [`Explanation`].
///
/// # Errors
///
/// Returns [`ReplyError::TooLong`] when the consequence is over [`caps::CONSEQUENCE_WORDS`], and
/// [`ReplyError`] when a named id is invalid.
pub fn explanation(reply: ExplainReply) -> Result<Explanation, ReplyError> {
    let words = caps::words(&reply.consequence);
    if words > caps::CONSEQUENCE_WORDS {
        return Err(ReplyError::TooLong {
            field: "consequence",
            words,
            cap: caps::CONSEQUENCE_WORDS,
        });
    }
    let breaks = reply
        .breaks
        .as_deref()
        .map(WitnessId::from_str)
        .transpose()?;
    let suggested = reply.suggested.map(settlement).transpose()?;
    Ok(Explanation {
        consequence: reply.consequence,
        breaks,
        suggested,
    })
}

fn settlement(reply: SettlementReply) -> Result<Settlement, ParseIdError> {
    Ok(match reply {
        SettlementReply::Narrow { task } => Settlement::Narrow {
            task: TaskId::from_str(&task)?,
        },
        SettlementReply::Serialize { first, then } => Settlement::Serialize {
            first: TaskId::from_str(&first)?,
            then: TaskId::from_str(&then)?,
        },
        SettlementReply::SplitAtContract { owner, contract } => Settlement::SplitAtContract {
            owner: TaskId::from_str(&owner)?,
            contract,
        },
    })
}

/// A draft reply as the provider returns it.
///
/// The provider is given a bare object schema. The schema the reply must match travels in the
/// request context, and [`scope_from_reply`] or [`probe_from_reply`] checks the decode.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct DraftReply(pub Value);

impl JsonSchema for DraftReply {
    fn schema_name() -> Cow<'static, str> {
        "DraftReply".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "object" })
    }
}

/// The JSON schema of `T`, as a draft request carries it in its context.
fn schema_value<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("a schema serializes to JSON")
}

/// The scope draft prompt for an order's `sentence`, the order as the human wrote it, with the
/// plant's machines, the routes a scope may name, the step ids and side effects the corpus
/// defines, and the schema of the scope body. `previous_error` is why the last attempt failed, or
/// none for a first attempt.
#[must_use]
pub fn scope_request(
    sentence: &str,
    bounds: &DraftBounds<'_>,
    previous_error: Option<&str>,
) -> ChatRequest {
    let corpus = CorpusSteps::of(bounds);
    draft_request(
        SCOPE_PROMPT,
        &json!({
            "order": sentence,
            "machines": bounds.machines,
            "routes": bounds.routes,
            "steps": corpus.ids,
            "side_effects": corpus.side_effects(bounds.routes),
            "schema": schema_value::<ScopeBody>(),
            "previous_error": previous_error,
        }),
    )
}

/// The probe prompt for `order`, with the fixtures, the routes, the request budget, and the
/// schema of the probe.
#[must_use]
pub fn probe_request(order: &str, bounds: &DraftBounds<'_>) -> ChatRequest {
    draft_request(
        PROBE_PROMPT,
        &json!({
            "order": order,
            "fixtures": bounds.fixtures.iter().map(fixture_context).collect::<Vec<Value>>(),
            "routes": bounds.routes,
            "max_requests": bounds.limits.max_requests,
            "schema": schema_value::<ProbeProposal>(),
        }),
    )
}

/// A fixture as a probe's context shows it: its id, the captures its recipe binds, and the
/// requests its recipe sends. The recipe's own steps stay out of the prompt, since they can carry
/// fixture credentials.
fn fixture_context(fixture: &Fixture) -> Value {
    let captures: Vec<&str> = fixture
        .recipe
        .iter()
        .filter_map(|step| match step {
            Step::Capture(capture) => Some(capture.binding.as_str()),
            Step::Http(_) | Step::AdvanceTime(_) | Step::ObserveSideEffect(_) => None,
        })
        .collect();
    let requests = fixture
        .recipe
        .iter()
        .filter(|step| matches!(step, Step::Http(_)))
        .count();
    json!({
        "id": fixture.id,
        "captures": captures,
        "requests": requests,
    })
}

/// Decodes a scope reply as a [`ScopeBody`], checks it, and wraps it as a model proposal.
///
/// # Errors
///
/// Returns [`DraftError::Malformed`] when the reply does not decode, which includes any field
/// the body does not have. Otherwise returns the errors of [`scope_draft`].
pub fn scope_from_reply(
    reply: DraftReply,
    model: ModelRef,
    bounds: &DraftBounds<'_>,
) -> Result<ModelProposal<ScopeBody>, DraftError> {
    let body: ScopeBody =
        serde_json::from_value(reply.0).map_err(|err| DraftError::Malformed(err.to_string()))?;
    scope_draft(body, model, bounds)
}

/// Decodes a probe reply as a [`ProbeProposal`], checks it, and wraps it as a model proposal.
///
/// # Errors
///
/// Returns [`DraftError::Malformed`] when the reply does not decode, which includes any field the
/// probe does not have, such as an expected value. Otherwise returns the errors of
/// [`probe_draft`].
pub fn probe_from_reply(
    reply: DraftReply,
    model: ModelRef,
    bounds: &DraftBounds<'_>,
) -> Result<ModelProposal<ProbeProposal>, DraftError> {
    let probe: ProbeProposal =
        serde_json::from_value(reply.0).map_err(|err| DraftError::Malformed(err.to_string()))?;
    probe_draft(probe, model, bounds)
}

/// Checks a scope draft and wraps it as a model proposal. The proposal has no authority until a
/// human confirms it through `HumanSpec::adopt`.
///
/// # Errors
///
/// Returns [`DraftError`] when the draft has no may-change items, holds a question, option label
/// or obligation line over its word cap, names a machine or route that the plant or the route
/// table does not have, or names a step id or side effect that the probe corpus does not define.
pub fn scope_draft(
    body: ScopeBody,
    model: ModelRef,
    bounds: &DraftBounds<'_>,
) -> Result<ModelProposal<ScopeBody>, DraftError> {
    check_scope(&body, bounds)?;
    Ok(ModelProposal::new(body, model))
}

/// Checks a probe proposal and wraps it as a model proposal. A probe carries inputs only.
///
/// # Errors
///
/// Returns [`DraftError`] when the probe names a fixture or route the corpus does not have, a
/// capture or step that is not bound or earlier, a duplicate step id, or more requests than the
/// run allows.
pub fn probe_draft(
    probe: ProbeProposal,
    model: ModelRef,
    bounds: &DraftBounds<'_>,
) -> Result<ModelProposal<ProbeProposal>, DraftError> {
    check_probe(&probe, bounds)?;
    Ok(ModelProposal::new(probe, model))
}

/// Refuses a scope whose person-facing text is over its cap: the obligation lines of the may-change
/// items and the must-keep rules, each open question, and each option label. It runs before any
/// reference is checked, so the retry is told the length first.
fn check_text(body: &ScopeBody) -> Result<(), DraftError> {
    let obligations = body
        .may
        .iter()
        .flat_map(|item| item.acceptance.as_slice())
        .chain(
            body.must_keep
                .iter()
                .flat_map(|rule| rule.obligations.as_slice()),
        );
    for obligation in obligations {
        check_words("obligation text", &obligation.text, caps::OBLIGATION_WORDS)?;
    }
    for choice in &body.open {
        check_words("open question", &choice.question, caps::QUESTION_WORDS)?;
        for option in choice.options.as_slice() {
            check_words("option label", &option.label, caps::OPTION_WORDS)?;
        }
    }
    Ok(())
}

/// Refuses `text` when it holds more than `cap` words, naming `field`.
fn check_words(field: &'static str, text: &str, cap: usize) -> Result<(), DraftError> {
    let words = caps::words(text);
    if words > cap {
        return Err(DraftError::TooLong { field, words, cap });
    }
    Ok(())
}

fn check_scope(body: &ScopeBody, bounds: &DraftBounds<'_>) -> Result<(), DraftError> {
    if body.may.is_empty() {
        return Err(DraftError::EmptyScope);
    }
    check_text(body)?;
    let corpus = CorpusSteps::of(bounds);
    for item in &body.may {
        check_route(&item.selector.route_template, bounds.routes)?;
        check_machines(item.machine_ids.as_slice(), bounds.machines)?;
        check_selector(&item.selector, &corpus)?;
        corpus.check_step(&item.example.step_id)?;
        check_example(item, &corpus)?;
        for obligation in item.acceptance.as_slice() {
            check_predicate(&obligation.predicate, &item.selector, &corpus)?;
        }
    }
    for rule in &body.must_keep {
        check_route(&rule.selector.route_template, bounds.routes)?;
        check_machines(rule.machine_ids.as_slice(), bounds.machines)?;
        check_selector(&rule.selector, &corpus)?;
        for obligation in rule.obligations.as_slice() {
            check_predicate(&obligation.predicate, &rule.selector, &corpus)?;
        }
    }
    for choice in &body.open {
        check_machines(std::slice::from_ref(&choice.machine_id), bounds.machines)?;
    }
    Ok(())
}

/// What a scope may name: the step ids the corpus defines, and the sequences its probes run. A
/// sequence is the probe's fixture recipe followed by the probe's own steps.
struct CorpusSteps<'a> {
    ids: BTreeSet<&'a str>,
    sequences: Vec<Vec<&'a Step>>,
}

impl<'a> CorpusSteps<'a> {
    fn of(bounds: &DraftBounds<'a>) -> Self {
        let fixtures: &'a [Fixture] = bounds.fixtures;
        let probes: &'a [CorpusProbe] = bounds.probes;
        let recipe_steps = fixtures.iter().flat_map(|fixture| fixture.recipe.iter());
        let probe_steps = probes
            .iter()
            .flat_map(|probe| probe.spec.steps.as_slice().iter());
        let ids = recipe_steps
            .chain(probe_steps)
            .map(|step| step.id().as_str())
            .collect();
        let sequences = probes
            .iter()
            .map(|probe| {
                let recipe: &[Step] = fixtures
                    .iter()
                    .find(|fixture| fixture.id == probe.spec.fixture)
                    .map_or(&[], |fixture| fixture.recipe.as_slice());
                recipe.iter().chain(probe.spec.steps.as_slice()).collect()
            })
            .collect();
        Self { ids, sequences }
    }

    fn check_step(&self, step: &StepId) -> Result<(), DraftError> {
        if self.ids.contains(step.as_str()) {
            Ok(())
        } else {
            Err(DraftError::UndefinedStep(step.as_str().to_owned()))
        }
    }

    /// Whether some probe observes `sink` at all.
    fn observes(&self, sink: TrustedSink) -> bool {
        self.sequences
            .iter()
            .flatten()
            .any(|step| matches!(step, Step::ObserveSideEffect(effect) if effect.sink == sink))
    }

    /// Whether some probe reads `sink` after a request that `method` and `route` admit, for the
    /// steps `context` admits. The read is correlated with the request it follows, as the
    /// corpus writes it.
    fn witnesses(
        &self,
        context: &StepContext,
        sink: TrustedSink,
        method: MethodSelector,
        route: &RouteTemplate,
    ) -> bool {
        self.sequences.iter().any(|sequence| {
            sequence.iter().any(|step| {
                let Step::ObserveSideEffect(effect) = step else {
                    return false;
                };
                let admitted = match context {
                    StepContext::Any {} => true,
                    StepContext::Step { step_id } => *step_id == effect.id,
                };
                admitted
                    && effect.sink == sink
                    && sequence.iter().any(|request| {
                        matches!(request, Step::Http(http)
                            if http.id == effect.correlate
                                && method_matches(method, http.request.method)
                                && route_matches(http.request.path.as_str(), route))
                    })
            })
        })
    }

    /// Whether some probe sends `step` as a request that `method` and `route` admit.
    fn sends(&self, step: &StepId, method: MethodSelector, route: &RouteTemplate) -> bool {
        self.sequences.iter().any(|sequence| {
            sequence.iter().any(|request| {
                matches!(request, Step::Http(http)
                    if http.id == *step
                        && method_matches(method, http.request.method)
                        && route_matches(http.request.path.as_str(), route))
            })
        })
    }

    /// The smallest position of `step` among the requests a window on `method` and `route`
    /// counts, over the probes that send it: 1 for the first such request in its sequence. Every
    /// requester counts, so the position is never less than the one a single requester gets.
    fn position_on_route(
        &self,
        step: &StepId,
        method: MethodSelector,
        route: Option<&RouteTemplate>,
    ) -> Option<u32> {
        self.sequences
            .iter()
            .filter_map(|sequence| {
                let mut seen = 0_u32;
                for request in sequence {
                    let Step::Http(http) = request else {
                        continue;
                    };
                    let counted = method_matches(method, http.request.method)
                        && route
                            .is_none_or(|route| route_matches(http.request.path.as_str(), route));
                    if !counted {
                        continue;
                    }
                    seen = seen.saturating_add(1);
                    if http.id == *step {
                        return Some(seen);
                    }
                }
                None
            })
            .min()
    }

    /// Each side effect the corpus reads, with the step a selector names for it and the method
    /// and route of the request it follows, as the route table writes them.
    fn side_effects(&self, routes: &[RouteEntry]) -> Vec<Value> {
        let mut found: Vec<Value> = Vec::new();
        for sequence in &self.sequences {
            for step in sequence {
                let Step::ObserveSideEffect(effect) = step else {
                    continue;
                };
                for request in sequence {
                    let Step::Http(http) = request else {
                        continue;
                    };
                    if http.id != effect.correlate {
                        continue;
                    }
                    for entry in routes {
                        if entry.method != http.request.method
                            || !route_matches(http.request.path.as_str(), &entry.route_template)
                        {
                            continue;
                        }
                        let value = json!({
                            "step_id": effect.id,
                            "sink": sink_name(effect.sink),
                            "after": {
                                "method": entry.method,
                                "route_template": entry.route_template,
                            },
                        });
                        if !found.contains(&value) {
                            found.push(value);
                        }
                    }
                }
            }
        }
        found
    }
}

/// Refuses an example that expects a status the item's window says its request must not get. A
/// window with the first-allowed obligation admits its first `allowed_count` matching requests, so
/// an example step at or before that position must not expect a rejected status. The position
/// counts every requester on the route, which is never less than one requester's position, so a
/// refusal is always a real contradiction.
fn check_example(item: &ScopeItem, corpus: &CorpusSteps<'_>) -> Result<(), DraftError> {
    let example = &item.example;
    if example.path.as_str() != "status" {
        return Ok(());
    }
    let Evidence::Present { value } = &example.after else {
        return Ok(());
    };
    let Some(status) = value.as_u64().and_then(|status| u16::try_from(status).ok()) else {
        return Ok(());
    };
    for obligation in item.acceptance.as_slice() {
        let Predicate::Window { policy } = &obligation.predicate else {
            continue;
        };
        if !policy
            .obligations
            .contains(&WindowObligation::FirstAllowed {})
            || !policy
                .rejected_attempts
                .status_in
                .as_slice()
                .contains(&status)
        {
            continue;
        }
        let allowed = policy.allowed_count.get();
        let route = policy.event_filter.route_template.as_ref();
        if let Some(position) =
            corpus.position_on_route(&example.step_id, policy.event_filter.method, route)
            && position <= allowed
        {
            return Err(DraftError::ExampleContradictsAcceptance {
                step: example.step_id.as_str().to_owned(),
                status,
                allowed,
            });
        }
    }
    Ok(())
}

/// Checks the step a selector names, and for a side-effect selector that a probe observes its
/// sink after a request the selector admits.
fn check_selector(selector: &Selector, corpus: &CorpusSteps<'_>) -> Result<(), DraftError> {
    if let StepContext::Step { step_id } = &selector.step_context {
        corpus.check_step(step_id)?;
    }
    if selector.channel != Channel::SideEffect {
        return Ok(());
    }
    // The decoder requires a side-effect selector to carry a path that starts with its sink.
    let Some(sink) = selector
        .field_path
        .as_ref()
        .and_then(|path| sink_of(path.as_str()))
    else {
        return Ok(());
    };
    if corpus.witnesses(
        &selector.step_context,
        sink,
        selector.method,
        &selector.route_template,
    ) {
        Ok(())
    } else {
        Err(DraftError::UnwitnessedSideEffect {
            sink: sink_name(sink).to_owned(),
            route: selector.route_template.as_str().to_owned(),
        })
    }
}

/// Checks each step a predicate reads, and that a probe observes each sink whose events it reads.
/// A status or body comparison must read a step that sends a request to the selector's route, so
/// a step named for another request, such as a signup for a login rule, is refused.
fn check_predicate(
    predicate: &Predicate,
    selector: &Selector,
    corpus: &CorpusSteps<'_>,
) -> Result<(), DraftError> {
    let mut steps = Vec::new();
    let mut sinks = Vec::new();
    predicate_refs(predicate, &mut steps, &mut sinks);
    for step in steps {
        corpus.check_step(step)?;
    }
    let mut compared = Vec::new();
    status_steps(predicate, &mut compared);
    for step in compared {
        if !corpus.sends(step, selector.method, &selector.route_template) {
            return Err(DraftError::StepOffRoute {
                step: step.as_str().to_owned(),
                route: selector.route_template.as_str().to_owned(),
            });
        }
    }
    for sink in sinks {
        if !corpus.observes(sink) {
            return Err(DraftError::UnobservedSink(sink_name(sink).to_owned()));
        }
    }
    Ok(())
}

/// The step ids and trusted sinks a predicate reads, in the order it names them.
fn predicate_refs<'p>(
    predicate: &'p Predicate,
    steps: &mut Vec<&'p StepId>,
    sinks: &mut Vec<TrustedSink>,
) {
    match predicate {
        Predicate::EqualsBase { step_id, .. } | Predicate::StatusIn { step_id, .. } => {
            steps.push(step_id);
        }
        Predicate::Equals { at, .. }
        | Predicate::Present { at }
        | Predicate::Absent { at }
        | Predicate::MatchesSchema { at, .. } => value_refs(at, steps, sinks),
        Predicate::AfterNWithin { then, .. } => predicate_refs(then, steps, sinks),
        Predicate::All { of } => {
            for inner in of.as_slice() {
                predicate_refs(inner, steps, sinks);
            }
        }
        Predicate::Compare { left, right, .. } => {
            value_refs(left, steps, sinks);
            if let Operand::Ref { value_ref } = right {
                value_refs(value_ref, steps, sinks);
            }
        }
        Predicate::Count { .. } => {}
        Predicate::Window { policy } => {
            if let WindowAnchor::Step { step_id } = &policy.anchor {
                steps.push(step_id);
            }
        }
    }
}

fn value_refs<'p>(value: &'p ValueRef, steps: &mut Vec<&'p StepId>, sinks: &mut Vec<TrustedSink>) {
    match value {
        ValueRef::Field { step_id, .. } => steps.push(step_id),
        ValueRef::Capture { .. } => {}
        ValueRef::Event { sink, .. } => sinks.push(*sink),
    }
}

/// Every step a status or body comparison reads in `predicate`, at any depth.
fn status_steps<'p>(predicate: &'p Predicate, steps: &mut Vec<&'p StepId>) {
    match predicate {
        Predicate::EqualsBase { step_id, .. } | Predicate::StatusIn { step_id, .. } => {
            steps.push(step_id);
        }
        Predicate::AfterNWithin { then, .. } => status_steps(then, steps),
        Predicate::All { of } => {
            for inner in of.as_slice() {
                status_steps(inner, steps);
            }
        }
        Predicate::Equals { .. }
        | Predicate::Present { .. }
        | Predicate::Absent { .. }
        | Predicate::MatchesSchema { .. }
        | Predicate::Compare { .. }
        | Predicate::Count { .. }
        | Predicate::Window { .. } => {}
    }
}

/// The trusted sink a side-effect field path starts with.
fn sink_of(path: &str) -> Option<TrustedSink> {
    match path.split(['.', '[']).next()? {
        "store" => Some(TrustedSink::Store),
        "outbound_http" => Some(TrustedSink::OutboundHttp),
        _ => None,
    }
}

fn sink_name(sink: TrustedSink) -> &'static str {
    match sink {
        TrustedSink::Store => "store",
        TrustedSink::OutboundHttp => "outbound_http",
    }
}

/// Whether a selector's method admits a request's method.
fn method_matches(selector: MethodSelector, method: HttpMethod) -> bool {
    match selector {
        MethodSelector::Any => true,
        MethodSelector::Get => method == HttpMethod::Get,
        MethodSelector::Head => method == HttpMethod::Head,
        MethodSelector::Post => method == HttpMethod::Post,
        MethodSelector::Put => method == HttpMethod::Put,
        MethodSelector::Patch => method == HttpMethod::Patch,
        MethodSelector::Delete => method == HttpMethod::Delete,
        MethodSelector::Options => method == HttpMethod::Options,
    }
}

/// Whether a request path is one request of a route template. Both split into segments that
/// match one for one. A `:name` segment matches any segment, and so does a path segment with a
/// `${capture}` placeholder. A literal segment must be equal. A query string is ignored.
fn route_matches(path: &str, route: &RouteTemplate) -> bool {
    let path = path.split('?').next().unwrap_or_default();
    let requested: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let templated: Vec<&str> = route.as_str().trim_start_matches('/').split('/').collect();
    requested.len() == templated.len()
        && requested.iter().zip(&templated).all(|(part, pattern)| {
            pattern.starts_with(':') || part.contains("${") || part == pattern
        })
}

fn check_probe(probe: &ProbeProposal, bounds: &DraftBounds<'_>) -> Result<(), DraftError> {
    let Some(fixture) = bounds
        .fixtures
        .iter()
        .find(|fixture| fixture.id == probe.fixture)
    else {
        return Err(DraftError::UnknownFixture(
            probe.fixture.as_str().to_owned(),
        ));
    };
    check_route(&probe.route_template, bounds.routes)?;
    check_steps(&fixture.recipe, probe.steps.as_slice(), bounds.limits)
}

fn check_machines(machines: &[MachineId], plant: &[MachineKey]) -> Result<(), DraftError> {
    for machine in machines {
        if !plant.iter().any(|known| known.as_str() == machine.as_str()) {
            return Err(DraftError::UnknownMachine(machine.as_str().to_owned()));
        }
    }
    Ok(())
}

fn check_route(route: &RouteTemplate, routes: &[RouteEntry]) -> Result<(), DraftError> {
    if routes.iter().any(|entry| entry.route_template == *route) {
        Ok(())
    } else {
        Err(DraftError::UnknownRoute(route.as_str().to_owned()))
    }
}

/// Checks one probe as the run executes it: the fixture's recipe, then the probe's own steps.
/// A capture may read only an earlier http step, a placeholder may use only a name an earlier
/// capture bound (the recipe's captures count), step ids are unique across the sequence, and
/// every request counts against the run's limit.
fn check_steps(recipe: &[Step], steps: &[Step], limits: RunLimits) -> Result<(), DraftError> {
    let sequence: Vec<&Step> = recipe.iter().chain(steps).collect();
    let requests = sequence
        .iter()
        .filter(|step| matches!(step, Step::Http(_)))
        .count();
    if requests > usize::try_from(limits.max_requests).unwrap_or(usize::MAX) {
        return Err(DraftError::Oversized {
            requests,
            max: limits.max_requests,
        });
    }

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut requested: BTreeSet<&str> = BTreeSet::new();
    let mut bound: BTreeSet<&str> = BTreeSet::new();
    for step in sequence {
        let id = step.id().as_str();
        if !seen.insert(id) {
            return Err(DraftError::DuplicateStep(id.to_owned()));
        }
        match step {
            Step::Http(http) => {
                for name in placeholders(&http.request)? {
                    if !bound.contains(name.as_str()) {
                        return Err(DraftError::UnknownCapture {
                            step: id.to_owned(),
                            name,
                        });
                    }
                }
                requested.insert(id);
            }
            Step::Capture(capture) => {
                check_earlier(&requested, &capture.from)?;
                bound.insert(capture.binding.as_str());
            }
            Step::ObserveSideEffect(effect) => check_earlier(&requested, &effect.correlate)?,
            Step::AdvanceTime(_) => {}
        }
    }
    Ok(())
}

fn check_earlier(requested: &BTreeSet<&str>, step: &StepId) -> Result<(), DraftError> {
    if requested.contains(step.as_str()) {
        Ok(())
    } else {
        Err(DraftError::UnknownStep(step.as_str().to_owned()))
    }
}

/// The `${name}` placeholders a request template uses. A path and its headers were checked when
/// they decoded as templates. A body is free JSON, so its string leaves are checked here.
fn placeholders(request: &RequestTemplate) -> Result<Vec<String>, DraftError> {
    let mut names: Vec<String> = request.path.placeholders().map(str::to_owned).collect();
    for template in request.headers.values() {
        names.extend(template.placeholders().map(str::to_owned));
    }
    if let Some(body) = &request.body {
        let mut leaves = Vec::new();
        string_leaves(body, &mut leaves);
        for raw in leaves {
            let template = Template::try_from(raw.to_owned())
                .map_err(|_| DraftError::MalformedTemplate(raw.to_owned()))?;
            names.extend(template.placeholders().map(str::to_owned));
        }
    }
    Ok(names)
}

/// Every string inside a JSON body, so a placeholder in any field is checked.
fn string_leaves<'v>(value: &'v Value, out: &mut Vec<&'v str>) {
    match value {
        Value::String(text) => out.push(text.as_str()),
        Value::Array(items) => {
            for item in items {
                string_leaves(item, out);
            }
        }
        Value::Object(fields) => {
            for field in fields.values() {
                string_leaves(field, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(id: &str) -> OrderId {
        OrderId::from_str(id).expect("valid order id")
    }

    fn plant() -> Vec<MachineKey> {
        vec![MachineKey::from_str("ui").expect("valid key")]
    }

    fn split(machines: &[&str]) -> SplitReply {
        SplitReply {
            tasks: vec![SplitTask {
                title: "Rename the handler".to_owned(),
                brief: "Rename it and update callers".to_owned(),
                scope: machines.iter().map(|m| (*m).to_owned()).collect(),
            }],
        }
    }

    #[test]
    fn split_reply_becomes_split_ready_with_machine_keys() {
        let cmd = split_result(order("o-3"), OrderRev::new(1), &plant(), split(&["ui"]))
            .expect("valid reply");
        assert_eq!(
            cmd,
            SystemCmd::SplitReady {
                order: order("o-3"),
                rev: OrderRev::new(1),
                tasks: vec![TaskSpec {
                    title: "Rename the handler".to_owned(),
                    brief: "Rename it and update callers".to_owned(),
                    scope: vec![MachineKey::from_str("ui").expect("valid key")],
                }],
            }
        );
    }

    #[test]
    fn split_reply_with_an_empty_machine_name_is_refused() {
        let err = split_result(order("o-3"), OrderRev::new(1), &plant(), split(&[""]))
            .expect_err("empty machine name");
        assert!(matches!(err, ReplyError::Id(_)), "got {err:?}");
    }

    #[test]
    fn split_reply_naming_a_machine_off_the_plant_is_refused() {
        let err = split_result(order("o-3"), OrderRev::new(1), &plant(), split(&["db"]))
            .expect_err("db is not on the plant");
        assert!(
            matches!(&err, ReplyError::UnknownMachine(name) if name == "db"),
            "got {err:?}"
        );
    }

    #[test]
    fn split_reply_without_tasks_is_refused() {
        let err = split_result(
            order("o-3"),
            OrderRev::new(1),
            &plant(),
            SplitReply { tasks: Vec::new() },
        )
        .expect_err("no tasks");
        assert!(matches!(err, ReplyError::NoTasks), "got {err:?}");
    }

    #[test]
    fn a_route_template_matches_its_requests_and_no_others() {
        let route = RouteTemplate::from_str("/api/links/:code").expect("a route template");

        assert!(route_matches("/api/links/${code}", &route));
        assert!(route_matches("/api/links/abc?expand=1", &route));
        assert!(!route_matches("/api/links", &route), "one segment short");
        assert!(!route_matches("/api/links/a/b", &route), "one segment long");
        assert!(
            !route_matches("/api/users/abc", &route),
            "a literal differs"
        );
    }

    #[test]
    fn explanation_maps_a_suggested_narrow_to_a_task_id() {
        let reply = ExplainReply {
            consequence: "Both edit the same handler.".to_owned(),
            breaks: Some("w-2".to_owned()),
            suggested: Some(SettlementReply::Narrow {
                task: "t-2".to_owned(),
            }),
        };
        let explanation = explanation(reply).expect("valid reply");
        assert_eq!(
            explanation.suggested,
            Some(Settlement::Narrow {
                task: TaskId::from_str("t-2").expect("valid task id"),
            })
        );
        assert_eq!(
            explanation.breaks,
            Some(WitnessId::from_str("w-2").expect("valid id"))
        );
    }

    #[test]
    fn explanation_refuses_a_settlement_with_a_bad_task_id() {
        let reply = ExplainReply {
            consequence: String::new(),
            breaks: None,
            suggested: Some(SettlementReply::Serialize {
                first: "task-2".to_owned(),
                then: "t-3".to_owned(),
            }),
        };
        assert!(explanation(reply).is_err(), "task-2 is not a task id");
    }

    #[test]
    fn explain_reply_in_the_documented_shape_parses() {
        let reply: ExplainReply = serde_json::from_value(json!({
            "consequence": "Both edit the greet function.",
            "breaks": "w-0",
            "suggested": { "narrow": { "task": "t-2" } }
        }))
        .expect("the documented shape parses");

        assert!(matches!(
            reply.suggested,
            Some(SettlementReply::Narrow { task }) if task == "t-2"
        ));
    }

    #[test]
    fn words_counts_a_backtick_span_as_one_word() {
        assert_eq!(caps::words("`a b c`"), 1, "a span is one word");
        assert_eq!(
            caps::words("a `b c` d"),
            3,
            "a span inside text is one word"
        );
        assert_eq!(
            caps::words("use `a b"),
            3,
            "an unmatched backtick is ordinary text"
        );
        assert_eq!(
            caps::words("`a b` c `d e"),
            4,
            "a span, then an unmatched backtick"
        );
        assert_eq!(
            caps::words("one two\tthree\nfour"),
            4,
            "any whitespace splits"
        );
        assert_eq!(caps::words("   "), 0, "blank text has no words");
    }

    #[test]
    fn an_explanation_over_the_consequence_cap_is_refused() {
        let reply = ExplainReply {
            consequence: vec!["word"; caps::CONSEQUENCE_WORDS + 1].join(" "),
            breaks: None,
            suggested: None,
        };

        let err = explanation(reply).expect_err("61 words is over the 60-word cap");

        assert!(
            matches!(
                err,
                ReplyError::TooLong {
                    field: "consequence",
                    words: 61,
                    cap: 60,
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn a_too_long_draft_names_its_field_and_its_cap() {
        let err = DraftError::TooLong {
            field: "open question",
            words: 40,
            cap: 30,
        };

        assert_eq!(
            err.to_string(),
            "open question has 40 words, more than the 30 allowed; shorten it"
        );
    }

    #[test]
    fn a_too_long_reply_names_its_field_and_its_cap() {
        let err = ReplyError::TooLong {
            field: "consequence",
            words: 61,
            cap: 60,
        };

        assert_eq!(
            err.to_string(),
            "consequence has 61 words, more than the 60 allowed; shorten it"
        );
    }
}
