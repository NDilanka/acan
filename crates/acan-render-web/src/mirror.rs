//! Folds the origin's event log into the state the product screen draws.
//!
//! [`Mirror`] wraps an [`acan_core::Replica`], so the screen shows exactly what the origin
//! decided. [`Mirror::view`] reads that state into a [`View`], the one shape the page renders.

use std::collections::{BTreeMap, BTreeSet};

use acan_core::{
    Actor, AgentId, Clash, ClashId, ClashState, Declaration, DeclarationInput, Event, FilePath,
    GateRunId, GateVerdict, Locus, OrderId, OrderPhase, OverlapKind, Question, QuestionId,
    ReplayError, Replica, Seq, Settlement, Stage, Stamped, Step, Task, TaskId, TaskState, Witness,
    WitnessOutcome, World,
};
use serde::Serialize;

use crate::feed::Entry;
use crate::lanes::{LaneView, ReceiptCount, lane_views, receipt_counts};
use crate::plan::Plan;
use crate::slices::clash_scope;

/// Where the first agent's body sits inside its machine, in world units from the corner.
const BODY_OFFSET: [f32; 2] = [14.0, 44.0];
/// Horizontal step between two agent bodies on one machine.
const BODY_STEP: f32 = 40.0;
/// Size of an agent body, in world units.
const BODY_SIZE: [f32; 2] = [30.0, 20.0];
/// The most usage history rows the view carries. The usage panel's label room in `labels.rs` is
/// measured with this many rows plus the "+N earlier" line, so more rows would run past it.
pub const USAGE_HISTORY_ROWS: usize = 3;

/// The origin's state, folded from its log.
#[derive(Debug, Clone)]
pub struct Mirror {
    replica: Replica,
    /// Registered agents, in registration order. Their position fixes their color.
    agents: Vec<AgentId>,
    /// The agent holding each task's lease, from the last grant.
    holders: BTreeMap<TaskId, AgentId>,
    /// The gate run each task last reported, so its check reads after the lease has ended.
    runs: BTreeMap<TaskId, GateRunId>,
    /// The word a task's change shows: the first symbol its last declaration named.
    changes: BTreeMap<TaskId, String>,
    /// The seq of the last model call for each model and step, so the usage history reads newest
    /// first.
    last_used: BTreeMap<(String, Step), Seq>,
    /// Events passed over because this build cannot read them, or because the world refused one
    /// after such an event. Above zero, the page is older than the origin.
    not_shown: u32,
    /// The agent holding each live lease, from its grant until it ends. Unlike `holders`, an
    /// ended lease leaves no holder, so a later event of the task is not heard from the agent.
    leases: BTreeMap<TaskId, AgentId>,
    /// When each agent was last heard of, in log milliseconds. The page's rail reads the same rule
    /// from the raw log in `lanes.js`.
    heard: BTreeMap<AgentId, u64>,
    /// When each task landed, in log milliseconds.
    landed_at: BTreeMap<TaskId, u64>,
}

impl Mirror {
    /// Folds a log that starts with `WorldOpened` at seq 0.
    ///
    /// # Errors
    ///
    /// Returns the replay error when the log does not replay.
    pub fn from_log(log: Vec<Stamped>) -> Result<Self, ReplayError> {
        Self::from_entries(log.into_iter().map(Entry::Known))
    }

    /// Folds a log read one event at a time. An event this build cannot read is passed over and
    /// counted in [`Mirror::not_shown`].
    ///
    /// # Errors
    ///
    /// Returns the replay error when the log does not replay, or when its first event is not a
    /// readable `WorldOpened`.
    pub fn from_entries(entries: impl IntoIterator<Item = Entry>) -> Result<Self, ReplayError> {
        let mut entries = entries.into_iter();
        let Some(Entry::Known(first)) = entries.next() else {
            return Err(ReplayError::NotOpened);
        };
        let mut mirror = Self {
            replica: Replica::from_log([first.clone()])?,
            agents: Vec::new(),
            holders: BTreeMap::new(),
            runs: BTreeMap::new(),
            changes: BTreeMap::new(),
            last_used: BTreeMap::new(),
            not_shown: 0,
            leases: BTreeMap::new(),
            heard: BTreeMap::new(),
            landed_at: BTreeMap::new(),
        };
        mirror.note(&first);
        for entry in entries {
            mirror.ingest_entry(entry)?;
        }
        Ok(mirror)
    }

    /// Applies one streamed event. An event already applied is ignored.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::SeqGap`] when events were missed, and the replica's error for an
    /// event that refers to something the log never created. Nothing is applied in either case.
    pub fn ingest(&mut self, stamped: &Stamped) -> Result<(), ReplayError> {
        let fresh = stamped.seq == self.replica.next_seq();
        self.replica.ingest(stamped)?;
        if fresh {
            self.note(stamped);
        }
        Ok(())
    }

    /// Applies one entry of the log or the stream. An event this build cannot read is passed over
    /// and counted. Once one has been passed over, an event the world refuses is passed over too:
    /// the event it depends on may be one this build cannot read.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::SeqGap`] when events were missed. Returns the replica's error for an
    /// event the world refuses when nothing has been passed over yet.
    pub fn ingest_entry(&mut self, entry: Entry) -> Result<(), ReplayError> {
        match entry {
            Entry::Unknown(seq) => self.pass_over(seq),
            Entry::Known(stamped) => match self.ingest(&stamped) {
                Err(ReplayError::InvalidEvent { .. }) if self.not_shown > 0 => {
                    self.pass_over(stamped.seq)
                }
                other => other,
            },
        }
    }

    /// The number of events this page passed over. Above zero, the page is older than the origin.
    #[must_use]
    pub fn not_shown(&self) -> u32 {
        self.not_shown
    }

    /// Moves the replica past one position without applying its event.
    fn pass_over(&mut self, seq: Seq) -> Result<(), ReplayError> {
        let before = self.replica.next_seq();
        self.replica.skip(seq)?;
        if self.replica.next_seq() != before {
            self.not_shown += 1;
        }
        Ok(())
    }

    /// The seq the next event will carry. The stream resumes from here.
    #[must_use]
    pub fn next_seq(&self) -> Seq {
        self.replica.next_seq()
    }

    /// The origin's world, read-only.
    #[must_use]
    pub fn world(&self) -> &World {
        self.replica.world()
    }

    /// Records what the world does not keep: which agents are registered and who holds each
    /// lease. A released lease keeps its holder; the task's state says whether it still counts.
    fn note(&mut self, stamped: &Stamped) {
        match &stamped.event {
            Event::AgentRegistered { agent } => self.agents.push(agent.id.clone()),
            Event::LeaseGranted { task, lease, .. } => {
                self.holders.insert(*task, lease.holder.clone());
            }
            Event::BuildReported { task, run, .. } => {
                self.runs.insert(*task, *run);
            }
            Event::Declared { decl } => {
                if let Some(word) = change_word(&decl.footprint) {
                    self.changes.insert(decl.task, word);
                }
            }
            Event::ModelUsed { model, step, .. } => {
                self.last_used.insert((model.clone(), *step), stamped.seq);
            }
            _ => {}
        }
        self.hear(stamped);
    }

    /// Notes who an event was heard from and when, as `lastActivity` in `lanes.js` reads it: the
    /// acting agent, a registered agent, a lease's holder when it is granted and when it ends, and
    /// the holder of the task any other event names. A landing also notes when its task landed.
    fn hear(&mut self, stamped: &Stamped) {
        let at = stamped.at.as_millis();
        let mut heard: Vec<AgentId> = Vec::new();
        if let Actor::Agent(agent) = &stamped.actor {
            heard.push(agent.clone());
        }
        match &stamped.event {
            Event::AgentRegistered { agent } => heard.push(agent.id.clone()),
            Event::LeaseGranted { task, lease, .. } => {
                self.leases.insert(*task, lease.holder.clone());
                heard.push(lease.holder.clone());
            }
            Event::LeaseEnded { task, .. } => heard.extend(self.leases.remove(task)),
            event => {
                if let Event::Landed { task, .. } = event {
                    self.landed_at.insert(*task, at);
                }
                if let Some(task) = event_task(event) {
                    heard.extend(self.leases.get(&task).cloned());
                }
            }
        }
        for agent in heard {
            self.heard.insert(agent, at);
        }
    }

    /// Reads the folded state into the view the page draws, laid out on `plan`.
    #[must_use]
    pub fn view(&self, plan: &Plan) -> View {
        let world = self.world();
        let tasks: Vec<&Task> = world
            .orders()
            .flat_map(|order| world.tasks_of(order.id))
            .collect();

        let abnormal: BTreeSet<usize> = world
            .clashes()
            .filter(|clash| unsettled(clash))
            .flat_map(|clash| plan.machines_at(clash.key.at()))
            .collect();
        let mut landed_at: BTreeMap<usize, u64> = BTreeMap::new();
        for (task, &at) in &self.landed_at {
            let Some(machine) = world
                .task(*task)
                .and_then(|task| task_machine(world, plan, task))
            else {
                continue;
            };
            let latest = landed_at.entry(machine).or_insert(at);
            *latest = (*latest).max(at);
        }
        let machines = plan
            .machines
            .iter()
            .enumerate()
            .map(|(i, m)| MachineView {
                name: m.name.clone(),
                min: m.min,
                size: m.size,
                abnormal: abnormal.contains(&i),
                landed_at: landed_at.get(&i).copied(),
            })
            .collect();

        let orders = world
            .orders()
            .map(|order| OrderView {
                id: order.id,
                machine: order.machine.as_str().to_owned(),
                sentence: order.sentence.clone(),
                phase: match world.phase(order.id) {
                    Some(phase) => phase_key(&phase),
                    None => "unknown",
                },
                tasks: world.tasks_of(order.id).count(),
                at: plan
                    .index_of(order.machine.as_str())
                    .map(|i| plan.machines[i].center()),
            })
            .collect();

        let task_views = tasks
            .iter()
            .map(|task| TaskView {
                id: task.id,
                order: task.order,
                title: task.spec.title.clone(),
                state: task_state_key(world, task),
                agent: match task.state {
                    TaskState::Open | TaskState::Dropped => None,
                    _ => self.holders.get(&task.id).cloned(),
                },
                machine: task_machine(world, plan, task).map(|m| plan.machines[m].name.clone()),
                change: self.changes.get(&task.id).cloned(),
                check: self.check_view(world, task),
                paths: task_paths(task),
            })
            .collect();

        let agents = self.agent_views(plan, world, &tasks);

        let questions = world
            .orders()
            .filter_map(|order| world.open_question(order.id))
            .map(|question| question_view(plan, question))
            .collect();

        let lanes = lane_views(world);
        let clashes = world
            .clashes()
            .map(|clash| {
                let orders: Vec<OrderId> = [clash.key.a(), clash.key.b()]
                    .into_iter()
                    .filter_map(|task| world.task(task).map(|task| task.order))
                    .collect();
                let machines = plan.machines_at(clash.key.at());
                ClashView {
                    scope: clash_scope(&machines, &orders, &lanes, plan),
                    ..clash_view(plan, clash)
                }
            })
            .collect();

        let (usage, usage_earlier) = self.usage_rows(world);
        View {
            next_seq: self.next_seq(),
            machines,
            orders,
            tasks: task_views,
            agents,
            questions,
            clashes,
            usage,
            usage_earlier,
            lanes,
            receipts: receipt_counts(world),
        }
    }

    /// The check a task answers to. Its gate's witnesses once it has reported a build, else its
    /// readback's verdict, else the order's first gated witness, which it has yet to pass. `None`
    /// when the order has neither a gated witness nor the task has a readback.
    fn check_view(&self, world: &World, task: &Task) -> Option<CheckView> {
        let order = world.order(task.order)?;
        if let Some(gate) = self.runs.get(&task.id).and_then(|run| world.gate(*run))
            && let Some(&first) = gate.witnesses.first()
        {
            let failed = gate
                .results
                .iter()
                .find(|(_, outcome)| !matches!(outcome, WitnessOutcome::Pass))
                .map(|(witness, _)| *witness);
            let (witness, state) = match (failed, gate.verdict()) {
                (Some(witness), _) => (witness, "failed"),
                (None, GateVerdict::Passed) => (first, "passed"),
                (None, GateVerdict::Failed { .. }) => (first, "failed"),
                (None, GateVerdict::Running) => (first, "pending"),
            };
            return Some(CheckView {
                name: witness.to_string(),
                state,
            });
        }
        if let Some(readback) = world.readbacks_of(task.id).last() {
            let state = match &readback.verdict {
                None => "pending",
                Some(verdict) if verdict.rev != order.rev => "pending",
                Some(verdict) if verdict.matches(order) => "passed",
                Some(_) => "failed",
            };
            return Some(CheckView {
                name: "readback".to_owned(),
                state,
            });
        }
        let first = order
            .witnesses
            .iter()
            .copied()
            .find(|&witness| world.witness(witness).is_some_and(Witness::gated))?;
        Some(CheckView {
            name: first.to_string(),
            state: "pending",
        })
    }

    fn agent_views(&self, plan: &Plan, world: &World, tasks: &[&Task]) -> Vec<AgentView> {
        let mut seats: BTreeMap<usize, usize> = BTreeMap::new();
        self.agents
            .iter()
            .map(|agent| {
                let focus = self.focus(world, tasks, agent);
                let machine = focus.and_then(|(task, _)| task_machine(world, plan, task));
                let body = machine.map(|m| {
                    let seat = seats.entry(m).or_insert(0);
                    let at = *seat;
                    *seat += 1;
                    let corner = plan.machines[m].min;
                    let x = corner[0] + BODY_OFFSET[0] + at as f32 * BODY_STEP;
                    let y = corner[1] + BODY_OFFSET[1];
                    [x, y, BODY_SIZE[0], BODY_SIZE[1]]
                });
                AgentView {
                    id: agent.clone(),
                    task: focus.map(|(task, _)| task.id),
                    activity: focus.map_or("idle", |(_, activity)| activity),
                    machine: machine.map(|m| plan.machines[m].name.clone()),
                    body,
                    heard_at: self.heard.get(agent).copied(),
                }
            })
            .collect()
    }

    /// What `agent` is doing now: the task it holds, else the open task it read back.
    fn focus<'a>(
        &self,
        world: &'a World,
        tasks: &[&'a Task],
        agent: &AgentId,
    ) -> Option<(&'a Task, &'static str)> {
        let held = tasks.iter().copied().find(|task| {
            self.holders.get(&task.id) == Some(agent)
                && !matches!(
                    task.state,
                    TaskState::Open | TaskState::Dropped | TaskState::Landed { .. }
                )
        });
        if let Some(task) = held {
            return Some((task, task_state_key(world, task)));
        }
        tasks
            .iter()
            .copied()
            .find(|task| {
                matches!(task.state, TaskState::Open)
                    && world.readbacks_of(task.id).any(|r| &r.agent == agent)
            })
            .map(|task| (task, "readback"))
    }
}

/// The screen's view of the origin's state. Ids keep their origin shape, so a click can send
/// them back unchanged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct View {
    /// The seq the mirror has applied up to.
    pub next_seq: Seq,
    /// Every machine of the plan, in plan order.
    pub machines: Vec<MachineView>,
    /// Every order.
    pub orders: Vec<OrderView>,
    /// Every task of every order.
    pub tasks: Vec<TaskView>,
    /// Every registered agent.
    pub agents: Vec<AgentView>,
    /// Open questions, one per order at most.
    pub questions: Vec<QuestionView>,
    /// Every clash the origin recorded, including resolved ones.
    pub clashes: Vec<ClashView>,
    /// Model calls so far, one row per model and step, newest call first, at most
    /// [`USAGE_HISTORY_ROWS`] rows. Empty until the first call.
    pub usage: Vec<UsageRow>,
    /// The rows left out of `usage` because they are older than the ones shown.
    pub usage_earlier: usize,
    /// One scope lane per order whose scope has been set, in order id order.
    pub lanes: Vec<LaneView>,
    /// Orders with a recorded receipt, in order id order, and their receipt counts. The page
    /// fetches an order's plant view again when its count moves.
    pub receipts: Vec<ReceiptCount>,
}

/// One model's calls for one step of the loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageRow {
    /// The loop step that made the calls, in snake case: `split`, `review`, `explain` or
    /// `screenshot`.
    pub role: &'static str,
    /// The model name the calls went to.
    pub model: String,
    /// How many calls.
    pub calls: u32,
    /// Prompt tokens, summed over the calls.
    pub input: u64,
    /// Completion tokens, summed over the calls.
    pub output: u64,
}

/// The check a task answers to, and where it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckView {
    /// The witness the check is for, such as `w-3`, or `readback` when a readback is the check.
    pub name: String,
    /// `pending`, `passed` or `failed`.
    pub state: &'static str,
}

/// A machine footprint and whether a clash stands on it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MachineView {
    /// The machine name.
    pub name: String,
    /// Top-left corner, in world units.
    pub min: [f32; 2],
    /// Width and height, in world units.
    pub size: [f32; 2],
    /// Whether an unsettled clash touches this machine.
    pub abnormal: bool,
    /// When the last of its tasks landed, in log milliseconds.
    pub landed_at: Option<u64>,
}

/// An order pinned on a machine.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrderView {
    /// The order id.
    pub id: OrderId,
    /// The machine it is pinned on.
    pub machine: String,
    /// What the human asked for.
    pub sentence: String,
    /// The derived phase, in snake case.
    pub phase: &'static str,
    /// How many tasks the foreman split it into.
    pub tasks: usize,
    /// Where the pin sits, in world units.
    pub at: Option<[f32; 2]>,
}

/// A task and what it is doing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskView {
    /// The task id.
    pub id: TaskId,
    /// The order it belongs to.
    pub order: OrderId,
    /// The task title.
    pub title: String,
    /// The task's activity, in snake case.
    pub state: &'static str,
    /// The agent holding the task's lease, if any.
    pub agent: Option<AgentId>,
    /// The machine the task works on, by name.
    pub machine: Option<String>,
    /// The word the task's change shows once it lands: the first symbol it declared.
    pub change: Option<String>,
    /// The check the task answers to, if the order has one or the task has a readback.
    pub check: Option<CheckView>,
    /// The repo paths its declaration names, while it still holds one. Sorted, without repeats.
    pub paths: Vec<String>,
}

/// A registered agent, with its body on the plan when it has a task.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentView {
    /// The agent id.
    pub id: AgentId,
    /// The task the agent is on.
    pub task: Option<TaskId>,
    /// What the agent is doing. `idle` when it has no task.
    pub activity: &'static str,
    /// The machine the agent works on.
    pub machine: Option<String>,
    /// The body rectangle `[x, y, w, h]`, in world units.
    pub body: Option<[f32; 4]>,
    /// When the agent was last heard of, in log milliseconds.
    pub heard_at: Option<u64>,
}

/// An open question with its two answers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuestionView {
    /// The question id.
    pub id: QuestionId,
    /// The order it belongs to.
    pub order: OrderId,
    /// What is being asked.
    pub text: String,
    /// The two answers.
    pub options: [String; 2],
    /// Where the pin sits, in world units.
    pub at: Option<[f32; 2]>,
}

/// A clash: its pin, its explanation, and the choices it offers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClashView {
    /// The clash id.
    pub id: ClashId,
    /// `open`, `proposed`, `settled` or `dissolved`.
    pub state: &'static str,
    /// The overlap kind, in snake case.
    pub kind: &'static str,
    /// The lower task of the pair.
    pub a: TaskId,
    /// The higher task of the pair.
    pub b: TaskId,
    /// The pin label, such as `c-1 · breaks w-3`.
    pub label: String,
    /// Where the pin sits, in world units.
    pub at: Option<[f32; 2]>,
    /// What goes wrong if both proceed, once the explain job has run.
    pub consequence: Option<String>,
    /// The witness that would break, if the explanation names one.
    pub breaks: Option<String>,
    /// The legal choices, numbered from 1. Empty once the clash is resolved.
    pub options: Vec<OptionView>,
    /// How the clash was resolved, when it has been.
    pub resolved: Option<String>,
    /// The confirmed scope the clash sits inside, in words, such as
    /// `inside confirmed scope o-1 rev 2`: a confirmed lane of one of its tasks' orders has a
    /// plot on the clash's machine. `None` otherwise, and always while the scope is a draft.
    pub scope: Option<String>,
}

/// One numbered choice on a clash card.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OptionView {
    /// The key that picks it, 1 for the first option.
    pub key: usize,
    /// The choice in words.
    pub label: String,
    /// The command payload for the choice.
    pub choice: Settlement,
}

/// The task an event names in its `task` field, as the page reads it from the event's JSON.
fn event_task(event: &Event) -> Option<TaskId> {
    let value = serde_json::to_value(event).ok()?;
    serde_json::from_value(value.get("task")?.clone()).ok()
}

fn unsettled(clash: &Clash) -> bool {
    matches!(clash.state, ClashState::Open | ClashState::Proposed { .. })
}

fn clash_view(plan: &Plan, clash: &Clash) -> ClashView {
    let (state, options, resolved) = match &clash.state {
        ClashState::Open => ("open", menu(clash), None),
        ClashState::Proposed { .. } => ("proposed", menu(clash), None),
        ClashState::Settled { choice, by } => (
            "settled",
            Vec::new(),
            Some(format!("{} by {by}", option_label(choice))),
        ),
        ClashState::Dissolved => ("dissolved", Vec::new(), Some("dissolved".to_owned())),
    };
    let explanation = clash.explanation.as_ref();
    let breaks = explanation.and_then(|e| e.breaks);
    let label = match breaks {
        Some(witness) => format!("{} · breaks {witness}", clash.id),
        None => format!("{} · overlap", clash.id),
    };
    ClashView {
        id: clash.id,
        state,
        kind: overlap_key(&clash.kind),
        a: clash.key.a(),
        b: clash.key.b(),
        label,
        at: plan.point_at(clash.key.at()),
        consequence: explanation.map(|e| e.consequence.clone()),
        breaks: breaks.map(|witness| witness.to_string()),
        options,
        resolved,
        scope: None,
    }
}

fn menu(clash: &Clash) -> Vec<OptionView> {
    clash
        .menu()
        .into_iter()
        .enumerate()
        .map(|(i, choice)| OptionView {
            key: i + 1,
            label: option_label(&choice),
            choice,
        })
        .collect()
}

fn option_label(choice: &Settlement) -> String {
    match choice {
        Settlement::Narrow { task } => format!("narrow {task}"),
        Settlement::Serialize { first, then } => format!("{first} lands first, {then} waits"),
        Settlement::SplitAtContract { owner, contract } => {
            format!("split at {contract}, {owner} lands it first")
        }
    }
}

fn question_view(plan: &Plan, question: &Question) -> QuestionView {
    QuestionView {
        id: question.id,
        order: question.order,
        text: question.text.clone(),
        options: question.options.clone(),
        at: plan.point_at(&question.at),
    }
}

fn phase_key(phase: &OrderPhase) -> &'static str {
    match phase {
        OrderPhase::Splitting => "splitting",
        OrderPhase::ReadingBack => "reading_back",
        OrderPhase::Asking { .. } => "asking",
        OrderPhase::Clashing => "clashing",
        OrderPhase::Building => "building",
        OrderPhase::Gating => "gating",
        OrderPhase::Landing => "landing",
        OrderPhase::Landed => "landed",
        OrderPhase::Cancelled => "cancelled",
    }
}

fn overlap_key(kind: &OverlapKind) -> &'static str {
    match kind {
        OverlapKind::SharedImplementation => "shared_implementation",
        OverlapKind::Contract => "contract",
        OverlapKind::GeneratedOutput => "generated_output",
        OverlapKind::SharedInsertion => "shared_insertion",
        OverlapKind::WholeFile => "whole_file",
        OverlapKind::DisjointRegions => "disjoint_regions",
    }
}

/// The activity a task shows: its readback, its claim stage, or its land state.
fn task_state_key(world: &World, task: &Task) -> &'static str {
    match &task.state {
        TaskState::Open => {
            if world.readbacks_of(task.id).next().is_some() {
                "readback"
            } else {
                "open"
            }
        }
        TaskState::Claimed { stage, .. } => stage_key(stage),
        TaskState::Queued { .. } => "queued",
        TaskState::Landed { .. } => "landed",
        TaskState::Dropped => "dropped",
    }
}

fn stage_key(stage: &Stage) -> &'static str {
    match stage {
        Stage::Declaring => "claimed",
        Stage::Declared { .. } => "declared",
        Stage::Building { .. } => "building",
        Stage::Gating { .. } => "gating",
    }
}

/// The paths a task's declaration names, while the task still holds one. Sorted, without repeats.
fn task_paths(task: &Task) -> Vec<String> {
    let TaskState::Claimed { stage, .. } = &task.state else {
        return Vec::new();
    };
    let Some(decl) = stage_declaration(stage) else {
        return Vec::new();
    };
    let mut paths: Vec<String> = footprint_paths(&decl.footprint)
        .map(|path| path.as_str().to_owned())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn stage_declaration(stage: &Stage) -> Option<&Declaration> {
    match stage {
        Stage::Declaring => None,
        Stage::Declared { decl } | Stage::Building { decl } | Stage::Gating { decl, .. } => {
            Some(decl)
        }
    }
}

/// The machine a task works on: its declared files, else where its readbacks say, else the
/// machine its order is pinned on.
fn task_machine(world: &World, plan: &Plan, task: &Task) -> Option<usize> {
    if let TaskState::Claimed { stage, .. } = &task.state
        && let Some(decl) = stage_declaration(stage)
        && let Some(machine) =
            footprint_paths(&decl.footprint).find_map(|path| plan.machine_of_path(path.as_str()))
    {
        return Some(machine);
    }
    let touched = world
        .readbacks_of(task.id)
        .flat_map(|readback| readback.touches.iter())
        .find_map(|locus: &Locus| plan.machines_at(locus).into_iter().next());
    touched.or_else(|| {
        world
            .order(task.order)
            .and_then(|order| plan.index_of(order.machine.as_str()))
    })
}

fn footprint_paths(footprint: &DeclarationInput) -> impl Iterator<Item = &FilePath> {
    footprint
        .files
        .iter()
        .chain(&footprint.inserts)
        .chain(footprint.symbols.iter().map(|symbol| &symbol.file))
        .chain(footprint.interfaces.iter().map(|symbol| &symbol.file))
}

/// The word a change shows: the first symbol its declaration names, else the file it touches.
fn change_word(footprint: &DeclarationInput) -> Option<String> {
    footprint
        .symbols
        .first()
        .map(|symbol| symbol.name.clone())
        .or_else(|| {
            footprint
                .files
                .first()
                .and_then(|path| path.as_str().rsplit('/').next())
                .map(str::to_owned)
        })
}

impl Mirror {
    /// The usage history, newest call first and cut to [`USAGE_HISTORY_ROWS`], and how many older
    /// rows were cut.
    fn usage_rows(&self, world: &World) -> (Vec<UsageRow>, usize) {
        let mut rows: Vec<(Option<Seq>, UsageRow)> = world
            .usage()
            .totals
            .iter()
            .flat_map(|(model, steps)| {
                steps.iter().map(move |(step, count)| {
                    let last = self.last_used.get(&(model.clone(), *step)).copied();
                    let row = UsageRow {
                        role: step_key(*step),
                        model: model.clone(),
                        calls: count.calls,
                        input: count.input,
                        output: count.output,
                    };
                    (last, row)
                })
            })
            .collect();
        rows.sort_by_key(|(last, _)| std::cmp::Reverse(*last));
        let earlier = rows.len().saturating_sub(USAGE_HISTORY_ROWS);
        rows.truncate(USAGE_HISTORY_ROWS);
        (rows.into_iter().map(|(_, row)| row).collect(), earlier)
    }
}

fn step_key(step: Step) -> &'static str {
    match step {
        Step::Split => "split",
        Step::Review => "review",
        Step::Explain => "explain",
        Step::Screenshot => "screenshot",
        Step::Vision => "vision",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::str::FromStr;

    use acan_core::{
        AgentCmd, Belt, Check, CodeIndex, CodeRev, Command, CommitId, Ctx, FilePath, HumanCmd,
        IndexedFile, IndexedSymbol, MachineKey, MemberId, Origin, Scope, ScopeBinding,
        ScopeConfirmation, Seq, Step, SystemCmd, TaskSpec, Timestamp, WitnessSpec, WorldId,
        WorldRef,
    };
    use serde_json::json;

    use super::*;
    use crate::feed;
    use crate::plant::from_scan_json;
    use crate::scenario::{self, Target};
    use crate::seal;
    use crate::slices;

    const PLANT: &str = r#"{
        "root": "/repo",
        "machines": [
            {"id": 1, "name": "src/api", "dir": "src/api", "files": [1]},
            {"id": 2, "name": "src/ui", "dir": "src/ui", "files": [2]}
        ],
        "belts": [{"from": 2, "to": 1, "imports": 1}],
        "files": [
            {"id": 1, "path": "src/api/auth.ts"},
            {"id": 2, "path": "src/ui/dashboard.ts"}
        ]
    }"#;

    fn plan() -> Plan {
        let plant = from_scan_json(PLANT).expect("the plant parses");
        Plan::new(&plant, |name| {
            if name == "src/api" {
                [0.0, 0.0, 96.0, 86.0]
            } else {
                [200.0, 0.0, 296.0, 86.0]
            }
        })
    }

    fn machine_index(view: &View, name: &str) -> usize {
        view.machines
            .iter()
            .position(|machine| machine.name == name)
            .expect("the plan has the machine")
    }

    fn founder() -> MemberId {
        MemberId::from_str("owner").expect("a member id")
    }

    fn open_world() -> (Origin, Stamped) {
        let world = WorldRef {
            id: WorldId::from_str("demo").expect("a world id"),
            repo: "demo".to_owned(),
        };
        Origin::open(world, founder(), Timestamp::from_millis(1_000))
    }

    fn index() -> CodeIndex {
        let api = MachineKey::from_str("src/api").expect("a machine key");
        let ui = MachineKey::from_str("src/ui").expect("a machine key");
        let symbol = |name: &str| IndexedSymbol {
            name: name.to_owned(),
            bytes: 0..40,
            exported: true,
        };
        let files = BTreeMap::from([
            (
                FilePath::from_str("src/api/auth.ts").expect("a file path"),
                IndexedFile {
                    machine: api.clone(),
                    symbols: vec![symbol("login")],
                },
            ),
            (
                FilePath::from_str("src/ui/dashboard.ts").expect("a file path"),
                IndexedFile {
                    machine: ui.clone(),
                    symbols: vec![symbol("render")],
                },
            ),
        ]);
        CodeIndex {
            rev: CodeRev::from_str("demo-rev").expect("a code rev"),
            files,
            belts: BTreeSet::from([Belt { from: ui, to: api }]),
        }
    }

    /// Runs the demo script on a fresh origin. Returns the origin, the whole log, and the index.
    fn scripted() -> (Origin, Vec<Stamped>, CodeIndex) {
        let index = index();
        let (mut origin, opened) = open_world();
        let target = Target::first_symbol(&index, "Make login return a session")
            .expect("the index holds a symbol");
        let events = scenario::clash_script(
            &mut origin,
            &index,
            &founder(),
            &target,
            Timestamp::from_millis(2_000),
        )
        .expect("the script runs");
        let log = std::iter::once(opened).chain(events).collect();
        (origin, log, index)
    }

    #[test]
    fn a_new_world_shows_the_plan_and_nothing_else() {
        let (_origin, opened) = open_world();
        let view = Mirror::from_log(vec![opened])
            .expect("the log replays")
            .view(&plan());

        assert_eq!(view.machines.len(), 2);
        assert!(view.machines.iter().all(|machine| !machine.abnormal));
        assert!(view.orders.is_empty());
        assert!(view.agents.is_empty());
        assert!(view.questions.is_empty());
        assert!(view.clashes.is_empty());
        assert!(view.lanes.is_empty());
        assert!(view.receipts.is_empty());
        assert_eq!(view.next_seq, Seq::new(1));
    }

    #[test]
    fn the_scripted_clash_is_pinned_on_its_machine_with_both_agents() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);

        let api = machine_index(&view, "src/api");
        assert!(view.machines[api].abnormal, "the clash stands on src/api");
        assert!(!view.machines[machine_index(&view, "src/ui")].abnormal);

        let [order] = view.orders.as_slice() else {
            panic!("the script posts one order");
        };
        assert_eq!(order.machine, "src/api");
        assert_eq!(order.phase, "clashing");
        assert_eq!(order.tasks, 2);
        assert_eq!(order.at, Some(plan.machines[api].center()));

        let [first, second] = view.agents.as_slice() else {
            panic!("the script registers two agents");
        };
        for agent in [first, second] {
            assert_eq!(agent.machine.as_deref(), Some("src/api"));
            assert_eq!(agent.activity, "declared");
            assert!(agent.body.is_some());
        }
        assert_ne!(first.body, second.body, "two bodies sit side by side");

        let [clash] = view.clashes.as_slice() else {
            panic!("the overlap is one clash");
        };
        assert_eq!(clash.state, "open");
        assert_eq!(clash.kind, "shared_implementation");
        assert_eq!(clash.label, format!("{} · overlap", clash.id));
        assert!(clash.at.is_some());
        assert!(
            clash.consequence.is_none(),
            "no explain job has run on the script"
        );
        assert!(
            (2..=3).contains(&clash.options.len()),
            "a clash offers two or three settlements"
        );
        let keys: Vec<usize> = clash.options.iter().map(|option| option.key).collect();
        assert_eq!(keys, (1..=clash.options.len()).collect::<Vec<_>>());
    }

    #[test]
    fn a_settle_event_resolves_the_pin_as_it_arrives() {
        let (mut origin, log, index) = scripted();
        let plan = plan();
        let mut mirror = Mirror::from_log(log).expect("the log replays");

        let before = mirror.view(&plan);
        let [clash] = before.clashes.as_slice() else {
            panic!("the script leaves one open clash");
        };
        let option = clash
            .options
            .first()
            .expect("the clash offers a settlement");
        let payload = json!({"cmd": "settle_clash", "clash": clash.id, "choice": option.choice});
        let cmd: HumanCmd =
            serde_json::from_value(payload).expect("the page's settle payload is a human command");

        let ctx = Ctx {
            now: Timestamp::from_millis(3_000),
            index: &index,
        };
        let events = origin
            .execute(
                &ctx,
                Command::Human {
                    member: founder(),
                    cmd,
                },
            )
            .expect("the settle is accepted");
        for stamped in &events {
            mirror
                .ingest(stamped)
                .expect("a live event applies in order");
        }

        let after = mirror.view(&plan);
        let [clash] = after.clashes.as_slice() else {
            panic!("the settled clash stays listed");
        };
        assert_eq!(clash.state, "settled");
        assert!(clash.options.is_empty());
        assert!(clash.resolved.is_some());
        assert!(!after.machines[machine_index(&after, "src/api")].abnormal);
    }

    #[test]
    fn ingest_ignores_a_replayed_event_and_refuses_a_gap() {
        let (_origin, log, _index) = scripted();
        let last = log.last().cloned().expect("the log has events");
        let mut mirror = Mirror::from_log(log).expect("the log replays");
        let next = mirror.next_seq();

        mirror
            .ingest(&last)
            .expect("an event already applied is ignored");
        assert_eq!(mirror.next_seq(), next);

        let mut ahead = last;
        ahead.seq = Seq::new(next.get() + 1);
        assert!(matches!(
            mirror.ingest(&ahead),
            Err(ReplayError::SeqGap { .. })
        ));
        assert_eq!(mirror.next_seq(), next);
    }

    /// The event at `stamped`'s position, as this build does not know it.
    fn unread_at(stamped: &Stamped) -> serde_json::Value {
        let mut value = serde_json::to_value(stamped).expect("an event serializes");
        value["event"] = json!({ "type": "vision_checked", "task": "t-1" });
        value
    }

    #[test]
    fn a_log_with_an_event_this_build_cannot_read_folds_past_it() {
        let (_origin, log, _index) = scripted();
        let count = log.len();
        let mut values: Vec<serde_json::Value> = log
            .iter()
            .map(|stamped| serde_json::to_value(stamped).expect("an event serializes"))
            .collect();
        values[count - 2] = unread_at(&log[count - 2]);

        let entries = feed::entries(&serde_json::Value::Array(values).to_string())
            .expect("the log parses one event at a time");
        let mirror = Mirror::from_entries(entries).expect("the log folds past the unread event");
        assert_eq!(mirror.not_shown(), 1);
        assert_eq!(
            mirror.next_seq(),
            Seq::new(u64::try_from(count).expect("a short log"))
        );
    }

    #[test]
    fn a_streamed_event_this_build_cannot_read_does_not_stop_the_next_one() {
        let (_origin, log, _index) = scripted();
        let count = log.len();
        let mut mirror = Mirror::from_log(log[..count - 2].to_vec()).expect("the prefix replays");

        let unread = feed::entry(&unread_at(&log[count - 2]).to_string()).expect("it parses");
        let next = feed::entry(&serde_json::to_string(&log[count - 1]).expect("it serializes"))
            .expect("it parses");
        mirror
            .ingest_entry(unread)
            .expect("the unread event is passed over");
        mirror.ingest_entry(next).expect("the event after it folds");
        assert_eq!(mirror.not_shown(), 1);
        assert_eq!(
            mirror.next_seq(),
            Seq::new(u64::try_from(count).expect("a short log"))
        );
    }

    #[test]
    fn a_refused_event_is_passed_over_only_after_one_this_build_cannot_read() {
        let (_origin, log, _index) = scripted();
        let next = Seq::new(u64::try_from(log.len()).expect("a short log"));
        // A second `WorldOpened` is refused by the world wherever it sits.
        let mut refused = log[0].clone();
        refused.seq = next;
        let mut mirror = Mirror::from_log(log).expect("the log replays");

        assert!(matches!(
            mirror.ingest_entry(Entry::Known(refused.clone())),
            Err(ReplayError::InvalidEvent { .. })
        ));
        assert_eq!(mirror.next_seq(), next);
        assert_eq!(mirror.not_shown(), 0);

        mirror
            .ingest_entry(Entry::Unknown(next))
            .expect("the unread event is passed over");
        refused.seq = Seq::new(next.get() + 1);
        mirror
            .ingest_entry(Entry::Known(refused))
            .expect("the refused event is passed over once one was");
        assert_eq!(mirror.not_shown(), 2);
        assert_eq!(mirror.next_seq(), Seq::new(next.get() + 2));
    }

    /// Runs one command as `actor` at a fixed time, and returns the events it appended.
    fn accepted(origin: &mut Origin, index: &CodeIndex, command: Command) -> Vec<Stamped> {
        let ctx = Ctx {
            now: Timestamp::from_millis(3_000),
            index,
        };
        origin
            .execute(&ctx, command)
            .expect("the command is accepted")
    }

    /// The scripted clash settled so that the first task goes first, and the first task's
    /// build reported, so its gate is running. Returns the first task's id.
    fn first_build_reported() -> (Origin, Vec<Stamped>, CodeIndex, TaskId) {
        let (mut origin, mut log, index) = scripted();
        let order = origin.world().orders().next().expect("an order").id;
        let tasks: Vec<TaskId> = origin.world().tasks_of(order).map(|t| t.id).collect();
        let first = tasks[0];
        let clash = origin.world().clashes().next().expect("a clash").clone();
        let choice = clash
            .menu()
            .into_iter()
            .find(|choice| matches!(choice, Settlement::Serialize { first: f, .. } if *f == first))
            .expect("the clash offers the first task going first");
        log.extend(accepted(
            &mut origin,
            &index,
            Command::Human {
                member: founder(),
                cmd: HumanCmd::SettleClash {
                    clash: clash.id,
                    choice,
                },
            },
        ));
        let claim = match &origin.world().task(first).expect("the task").state {
            TaskState::Claimed { claim, .. } => claim.clone(),
            other => panic!("the first task is claimed, not {other:?}"),
        };
        log.extend(accepted(
            &mut origin,
            &index,
            Command::Agent {
                agent: AgentId::from_str(scenario::AGENTS[0]).expect("an agent id"),
                cmd: AgentCmd::ReportBuilt {
                    fence: claim.fence(first),
                    head: CommitId::from_str("3f9c2e1").expect("a commit id"),
                },
            },
        ));
        (origin, log, index, first)
    }

    fn check_of(view: &View, task: TaskId) -> CheckView {
        view.tasks
            .iter()
            .find(|candidate| candidate.id == task)
            .and_then(|candidate| candidate.check.clone())
            .expect("the task has a check")
    }

    #[test]
    fn a_matched_readback_is_the_check_before_any_build() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);

        assert!(!view.tasks.is_empty());
        for task in &view.tasks {
            let check = task.check.clone().expect("each task was read back");
            assert_eq!(check.name, "readback");
            assert_eq!(check.state, "passed", "the readback matched the witnesses");
        }
    }

    #[test]
    fn a_running_gate_shows_its_witness_as_pending_and_a_failed_one_as_failed() {
        let plan = plan();
        let (mut origin, log, index, first) = first_build_reported();
        let mut mirror = Mirror::from_log(log.clone()).expect("the log replays");
        let view = mirror.view(&plan);
        let pending = check_of(&view, first);
        assert_eq!(pending.state, "pending");
        assert!(pending.name.starts_with('w'), "the check names a witness");

        let gate = origin
            .world()
            .task(first)
            .and_then(|task| match &task.state {
                TaskState::Claimed {
                    stage: Stage::Gating { run, .. },
                    ..
                } => Some(*run),
                _ => None,
            })
            .expect("the first build is gating");
        let witness = origin
            .world()
            .gate(gate)
            .expect("the gate run exists")
            .witnesses[0];
        let failing = accepted(
            &mut origin,
            &index,
            Command::System(SystemCmd::WitnessResult {
                run: gate,
                witness,
                outcome: WitnessOutcome::Fail {
                    detail: "npm test exited 1".to_owned(),
                },
            }),
        );
        for stamped in &failing {
            mirror
                .ingest(stamped)
                .expect("a live event applies in order");
        }
        let failed = check_of(&mirror.view(&plan), first);
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.name, witness.to_string());
    }

    #[test]
    fn a_landed_task_keeps_its_passed_check_and_its_change_word() {
        let plan = plan();
        let (mut origin, mut log, index) = scripted();
        let events = scenario::land_script(
            &mut origin,
            &index,
            &founder(),
            Timestamp::from_millis(2_000),
        )
        .expect("the story runs");
        log.extend(events);
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);

        assert_eq!(view.orders[0].phase, "landed");
        for task in &view.tasks {
            assert_eq!(task.state, "landed");
            assert_eq!(task.change.as_deref(), Some("login"));
            assert_eq!(task.machine.as_deref(), Some("src/api"));
            let check = task.check.clone().expect("the gate ran");
            assert_eq!(check.state, "passed");
            assert!(check.name.starts_with('w'));
        }
        for agent in &view.agents {
            assert_eq!(agent.task, None, "a landed task releases its agent");
            assert!(agent.body.is_none(), "the landed bot leaves the plant");
        }
    }

    /// The demo through both landings, with each event a second after the one before.
    fn landed_log() -> Vec<Stamped> {
        let (mut origin, mut log, index) = scripted();
        let events = scenario::land_script(
            &mut origin,
            &index,
            &founder(),
            Timestamp::from_millis(2_000),
        )
        .expect("the story runs");
        log.extend(events);
        for (i, stamped) in log.iter_mut().enumerate() {
            stamped.at = Timestamp::from_millis(1_000 * (i as u64 + 1));
        }
        log
    }

    /// When each agent was last heard of, read from the log's JSON by the rule `lastActivity` in
    /// `lanes.js` follows.
    fn heard_as_the_page_reads_it(log: &[Stamped]) -> BTreeMap<String, u64> {
        let mut holders: BTreeMap<String, String> = BTreeMap::new();
        let mut last = BTreeMap::new();
        for stamped in log {
            let json = serde_json::to_value(stamped).expect("a stamped event serializes");
            let at = json["at"].as_u64().expect("at is in milliseconds");
            let event = &json["event"];
            let mut touch = |agent: Option<&str>| {
                if let Some(agent) = agent {
                    last.insert(agent.to_owned(), at);
                }
            };
            if json["actor"]["kind"] == "agent" {
                touch(json["actor"]["id"].as_str());
            }
            // A task id is a number in the JSON, and the page keys its map by it as it is.
            let task = (!event["task"].is_null()).then(|| event["task"].to_string());
            match event["type"].as_str() {
                Some("agent_registered") => touch(event["agent"]["id"].as_str()),
                Some("lease_granted") => {
                    let holder = event["lease"]["holder"].as_str().map(str::to_owned);
                    touch(holder.as_deref());
                    if let (Some(task), Some(holder)) = (task, holder) {
                        holders.insert(task, holder);
                    }
                }
                Some("lease_ended") => {
                    let holder = task.and_then(|task| holders.remove(&task));
                    touch(holder.as_deref());
                }
                _ => touch(task.and_then(|task| holders.get(&task)).map(String::as_str)),
            }
        }
        last
    }

    #[test]
    fn an_agent_is_heard_of_when_the_page_rail_says_it_was() {
        let plan = plan();
        let log = landed_log();
        let page = heard_as_the_page_reads_it(&log);
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        assert!(!view.agents.is_empty());
        for agent in &view.agents {
            assert_eq!(
                agent.heard_at,
                page.get(&agent.id.to_string()).copied(),
                "{}",
                agent.id
            );
        }
    }

    #[test]
    fn a_machine_keeps_when_its_last_task_landed() {
        let plan = plan();
        let log = landed_log();
        let landed: Vec<u64> = log
            .iter()
            .filter(|stamped| matches!(stamped.event, Event::Landed { .. }))
            .map(|stamped| stamped.at.as_millis())
            .collect();
        assert_eq!(landed.len(), 2, "both tasks land");
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let api = machine_index(&view, "src/api");
        assert_eq!(view.machines[api].landed_at, landed.iter().max().copied());
        let ui = machine_index(&view, "src/ui");
        assert_eq!(
            view.machines[ui].landed_at, None,
            "nothing landed on src/ui"
        );
    }

    #[test]
    fn usage_is_empty_until_a_model_call_is_recorded() {
        let plan = plan();
        let (mut origin, mut log, index) = scripted();
        let events = scenario::land_script(
            &mut origin,
            &index,
            &founder(),
            Timestamp::from_millis(2_000),
        )
        .expect("the story runs");
        log.extend(events);
        let before = Mirror::from_log(log.clone())
            .expect("the log replays")
            .view(&plan);
        assert!(
            before.usage.is_empty(),
            "the offline story records no model call"
        );

        // The call is recorded here, in the test, so its numbers are test values and not demo ones.
        log.extend(accepted(
            &mut origin,
            &index,
            Command::System(SystemCmd::ModelUsed {
                model: "test-model".to_owned(),
                step: Step::Explain,
                input_tokens: 30,
                output_tokens: 4,
            }),
        ));
        let after = Mirror::from_log(log).expect("the log replays").view(&plan);
        let [row] = after.usage.as_slice() else {
            panic!("one recorded call is one row");
        };
        assert_eq!(row.role, "explain");
        assert_eq!(row.model, "test-model");
        assert_eq!((row.calls, row.input, row.output), (1, 30, 4));
    }

    #[test]
    fn the_usage_history_keeps_the_latest_three_rows_and_counts_the_rest() {
        let plan = plan();
        let (mut origin, mut log, index) = scripted();
        log.extend(
            scenario::land_script(
                &mut origin,
                &index,
                &founder(),
                Timestamp::from_millis(2_000),
            )
            .expect("the story runs"),
        );
        let mut call = |log: &mut Vec<Stamped>, model: &str| {
            log.extend(accepted(
                &mut origin,
                &index,
                Command::System(SystemCmd::ModelUsed {
                    model: model.to_owned(),
                    step: Step::Explain,
                    input_tokens: 10,
                    output_tokens: 1,
                }),
            ));
        };
        for i in 0..10 {
            call(&mut log, &format!("test-model-{i}"));
        }
        let view = Mirror::from_log(log.clone())
            .expect("the log replays")
            .view(&plan);
        let models: Vec<&str> = view.usage.iter().map(|row| row.model.as_str()).collect();
        assert_eq!(models, ["test-model-9", "test-model-8", "test-model-7"]);
        assert_eq!(view.usage_earlier, 7, "ten rows show three and count seven");

        // A second call to the oldest model makes its row the newest.
        call(&mut log, "test-model-0");
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let models: Vec<&str> = view.usage.iter().map(|row| row.model.as_str()).collect();
        assert_eq!(models, ["test-model-0", "test-model-9", "test-model-8"]);
        assert_eq!(view.usage[0].calls, 2);
        assert_eq!(view.usage_earlier, 7);
    }

    /// The commit the lane fixtures' scopes sit on.
    const BASE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A selector on the status of `POST /api/login` for anonymous callers, at step `s1`, or at
    /// every step when `every_step` is set, which makes it broad.
    fn login_selector(every_step: bool) -> serde_json::Value {
        let step = if every_step {
            json!({"kind": "any"})
        } else {
            json!({"kind": "step", "step_id": "s1"})
        };
        json!({
            "surface": "http", "method": "POST", "route_template": "/api/login",
            "step_context": step, "principal": {"kind": "anonymous"}, "channel": "status"
        })
    }

    fn obligation(id: &str, text: &str) -> serde_json::Value {
        json!({
            "obligation_id": id,
            "predicate": {
                "predicate": "present",
                "at": {"ref": "field", "run": "candidate", "step_id": "s1", "path": "status"}
            },
            "text": text
        })
    }

    /// A may-change item with two acceptance obligations, naming `machines` as given.
    fn may_item(id: &str, selector: serde_json::Value, machines: &[&str]) -> serde_json::Value {
        json!({
            "id": id,
            "selector": selector,
            "change": "add",
            "acceptance": [
                obligation(&format!("{id}-limit"), "the sixth failed login gets a 429"),
                obligation(&format!("{id}-retry"), "the 429 says when to retry")
            ],
            "provenance": "user",
            "example": {
                "kind": "target", "step_id": "s1", "path": "status",
                "after": {"state": "present", "value": 429}
            },
            "machine_ids": machines
        })
    }

    /// Revision 1 of a scope for `order`: `may`, one must-keep rule and one open choice.
    fn scope_for(order: OrderId, may: Vec<serde_json::Value>) -> Scope {
        serde_json::from_value(json!({
            "version": "acan-scope/1",
            "order": order.to_string(),
            "rev": 1,
            "base_commit": BASE_COMMIT,
            "body": {
                "may": may,
                "must_keep": [{
                    "id": "k-login",
                    "selector": login_selector(false),
                    "obligations": [obligation("k-login-ok", "a correct password still logs in")],
                    "text": "login keeps working for a correct password",
                    "machine_ids": ["src/api"]
                }],
                "open": [{
                    "id": "c-count",
                    "machine_id": "src/api",
                    "question": "Count failed logins per account or per address?",
                    "options": [
                        {"key": "account", "label": "Per account"},
                        {"key": "address", "label": "Per address"}
                    ]
                }]
            }
        }))
        .expect("the scope fixture decodes")
    }

    /// Posts an order on `src/api` as the founder. Returns its id and the events.
    fn post_order(
        origin: &mut Origin,
        index: &CodeIndex,
        sentence: &str,
    ) -> (OrderId, Vec<Stamped>) {
        let events = accepted(
            origin,
            index,
            Command::Human {
                member: founder(),
                cmd: HumanCmd::PostOrder {
                    machine: MachineKey::from_str("src/api").expect("a machine key"),
                    sentence: sentence.to_owned(),
                    witnesses: vec![WitnessSpec {
                        check: Check::Test {
                            command: "npm test".to_owned(),
                        },
                        reason: "the existing tests keep passing".to_owned(),
                    }],
                },
            },
        );
        let order = events
            .iter()
            .find_map(|stamped| match &stamped.event {
                Event::OrderPosted { order, .. } => Some(*order),
                _ => None,
            })
            .expect("the post mints an order");
        (order, events)
    }

    /// A confirmation of `scope` by the founder, bound the way the origin checks it.
    fn confirmation_for(origin: &Origin, scope: &Scope) -> ScopeConfirmation {
        let binding = ScopeBinding::of(scope).expect("the scope hashes");
        serde_json::from_value(json!({
            "version": "acan-scope-confirmation/1",
            "order": scope.order,
            "scope_rev": scope.rev,
            "scope_hash": binding.scope_hash,
            "base_commit": scope.base_commit,
            "ruling_versions": origin.world().ledger().ruling_versions(),
            "selector_hashes": binding.selector_hashes,
            "example_hashes": binding.example_hashes,
            "answers": [{"choice_id": "c-count", "option": "account"}],
            "broad_grants": [],
            "budget": {
                "repair_rounds": 2, "wall_time_ms": 1_800_000,
                "model_cost_micros": 2_000_000, "max_episodes": 6
            },
            "by": founder().as_str(),
            "at": 3_000
        }))
        .expect("the confirmation fixture decodes")
    }

    /// A world with one order on `src/api` whose scope is revision 1 with `may`, not yet
    /// confirmed.
    struct LaneFixture {
        origin: Origin,
        log: Vec<Stamped>,
        index: CodeIndex,
        order: OrderId,
        scope: Scope,
    }

    fn lane_fixture(may: Vec<serde_json::Value>) -> LaneFixture {
        let index = index();
        let (mut origin, opened) = open_world();
        let mut log = vec![opened];
        let (order, posted) = post_order(&mut origin, &index, "Limit failed logins");
        log.extend(posted);
        let scope = scope_for(order, may);
        log.extend(accepted(
            &mut origin,
            &index,
            Command::Human {
                member: founder(),
                cmd: HumanCmd::ReviseScope {
                    order,
                    scope: Box::new(scope.clone()),
                },
            },
        ));
        LaneFixture {
            origin,
            log,
            index,
            order,
            scope,
        }
    }

    fn narrow_item() -> serde_json::Value {
        may_item("i-limit", login_selector(false), &["src/api"])
    }

    #[test]
    fn an_order_with_a_scope_folds_into_one_unconfirmed_lane() {
        let fx = lane_fixture(vec![narrow_item()]);
        let view = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan());

        let [lane] = view.lanes.as_slice() else {
            panic!("the one order with a scope has one lane");
        };
        assert_eq!(lane.order, fx.order);
        assert_eq!(lane.state, "unconfirmed");
        assert!(lane.confirmation.is_none());
        assert_eq!(lane.rev, fx.scope.rev);
        assert_eq!(
            lane.scope_hash,
            fx.scope.scope_hash().expect("the scope hashes")
        );
        assert_eq!(lane.scope, fx.scope);

        let [item] = lane.may.as_slice() else {
            panic!("the scope has one may-change item");
        };
        assert_eq!(item.label, "the sixth failed login gets a 429");
        assert_eq!(item.selector, "POST /api/login");
        assert_eq!(item.machines, ["src/api"]);
        assert!(!item.broad);

        let [rule] = lane.must_keep.as_slice() else {
            panic!("the scope has one must-keep rule");
        };
        assert_eq!(rule.text, "login keeps working for a correct password");
        assert_eq!(rule.ruling_id, None);
        assert_eq!(rule.machines, ["src/api"]);

        let [choice] = lane.open.as_slice() else {
            panic!("the scope has one open choice");
        };
        assert_eq!(choice.machine, "src/api");
        assert_eq!(choice.options.len(), 2);
        assert!(view.receipts.is_empty());
    }

    #[test]
    fn a_confirmed_scope_carries_its_confirmation_and_the_member_who_gave_it() {
        let mut fx = lane_fixture(vec![narrow_item()]);
        let confirmation = confirmation_for(&fx.origin, &fx.scope);
        let events = accepted(
            &mut fx.origin,
            &fx.index,
            Command::Human {
                member: founder(),
                cmd: HumanCmd::ConfirmScope {
                    order: fx.order,
                    confirmation: Box::new(confirmation.clone()),
                },
            },
        );
        let recorded = events
            .iter()
            .any(|stamped| matches!(stamped.event, Event::ScopeConfirmed { .. }));
        assert!(recorded, "the confirmation is recorded");
        fx.log.extend(events);
        let view = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan());

        let [lane] = view.lanes.as_slice() else {
            panic!("the one order with a scope has one lane");
        };
        assert_eq!(lane.state, "confirmed");
        let shown = lane
            .confirmation
            .as_ref()
            .expect("the lane carries its confirmation");
        assert_eq!(shown, &confirmation);
        assert_eq!(shown.by.as_str(), founder().as_str());
        let json = serde_json::to_value(lane).expect("a lane serializes");
        assert_eq!(json["confirmation"]["by"], founder().as_str());
    }

    #[test]
    fn an_order_without_a_scope_has_no_lane() {
        let mut fx = lane_fixture(vec![narrow_item()]);
        let (other, posted) = post_order(&mut fx.origin, &fx.index, "Show the session");
        fx.log.extend(posted);
        let view = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan());

        assert_eq!(view.orders.len(), 2);
        let [lane] = view.lanes.as_slice() else {
            panic!("only the order with a scope has a lane");
        };
        assert_eq!(lane.order, fx.order);
        assert_ne!(lane.order, other);
    }

    #[test]
    fn a_broad_selector_marks_its_item_broad() {
        let fx = lane_fixture(vec![
            narrow_item(),
            may_item("i-every-step", login_selector(true), &["src/api"]),
        ]);
        let view = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan());

        let broad: Vec<(&str, bool)> = view.lanes[0]
            .may
            .iter()
            .map(|item| (item.id.as_str(), item.broad))
            .collect();
        assert_eq!(broad, [("i-limit", false), ("i-every-step", true)]);
        for (shown, item) in view.lanes[0].may.iter().zip(&fx.scope.body.may) {
            assert_eq!(shown.broad, item.selector.is_broad());
        }
    }

    #[test]
    fn a_machine_name_the_plan_lacks_comes_through_unchanged() {
        let plan = plan();
        assert_eq!(plan.index_of("billing"), None);
        let item = may_item("i-limit", login_selector(false), &["src/api", "billing"]);
        let fx = lane_fixture(vec![item]);
        let view = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan);

        assert_eq!(view.lanes[0].may[0].machines, ["src/api", "billing"]);
    }

    /// Confirms the fixture's scope as the founder.
    fn confirm(fx: &mut LaneFixture) {
        let confirmation = confirmation_for(&fx.origin, &fx.scope);
        let events = accepted(
            &mut fx.origin,
            &fx.index,
            Command::Human {
                member: founder(),
                cmd: HumanCmd::ConfirmScope {
                    order: fx.order,
                    confirmation: Box::new(confirmation),
                },
            },
        );
        fx.log.extend(events);
    }

    #[test]
    fn a_draft_scope_lays_plots_and_keeps_on_its_machines_as_a_draft() {
        let plan = plan();
        let fx = lane_fixture(vec![
            narrow_item(),
            may_item("i-banner", login_selector(true), &["src/ui", "billing"]),
        ]);
        let view = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan);
        let layout = slices::slice_layout(&view.lanes, &plan);

        let api = plan.index_of("src/api").expect("api");
        let ui = plan.index_of("src/ui").expect("ui");
        let [on_api, on_ui] = layout.slices.as_slice() else {
            panic!("one slice per named machine on the plan");
        };
        assert_eq!((on_api.machine, on_ui.machine), (api, ui));
        assert!(!on_api.confirmed, "a draft grants nothing yet");
        assert_eq!(
            on_api.text(),
            format!("{} · api · plot + keep · draft rev 1", fx.order)
        );
        assert_eq!(
            on_ui.text(),
            format!("{} · ui · plot · draft rev 1", fx.order)
        );
        assert_eq!(
            on_api.detail(),
            [
                "may POST /api/login · the sixth failed login gets a 429",
                "keep login keeps working for a correct password"
            ]
        );
        assert_eq!(
            on_ui.detail(),
            ["may POST /api/login · broad grant · the sixth failed login gets a 429"]
        );
        assert_eq!(layout.tints[api], Some(slices::ScopeTint::KeepDraft));
        assert_eq!(layout.tints[ui], Some(slices::ScopeTint::PlotDraft));
        assert_eq!(
            layout.unplaced,
            ["billing"],
            "a name the plan lacks is not drawn"
        );
    }

    #[test]
    fn a_confirmed_scope_tints_its_machines_as_confirmed() {
        let plan = plan();
        let mut fx = lane_fixture(vec![may_item(
            "i-banner",
            login_selector(false),
            &["src/ui"],
        )]);
        confirm(&mut fx);
        let view = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan);
        let layout = slices::slice_layout(&view.lanes, &plan);

        let api = plan.index_of("src/api").expect("api");
        let ui = plan.index_of("src/ui").expect("ui");
        assert_eq!(layout.tints[api], Some(slices::ScopeTint::Keep));
        assert_eq!(layout.tints[ui], Some(slices::ScopeTint::Plot));
        let on_ui = layout
            .slices
            .iter()
            .find(|slice| slice.machine == ui)
            .expect("the plot on ui");
        assert!(on_ui.confirmed);
        assert_eq!(
            on_ui.text(),
            format!("{} · ui · plot · confirmed rev 1", view.lanes[0].order)
        );
    }

    #[test]
    fn a_clash_is_inside_a_confirmed_scope_only_when_a_confirmed_plot_of_its_order_covers_it() {
        let plan = plan();
        let api = plan.index_of("src/api").expect("api");
        let ui = plan.index_of("src/ui").expect("ui");
        let mut fx = lane_fixture(vec![narrow_item()]);
        let order = fx.order;
        let draft = Mirror::from_log(fx.log.clone())
            .expect("the log replays")
            .view(&plan);
        assert_eq!(
            slices::clash_scope(&[api], &[order], &draft.lanes, &plan),
            None,
            "a draft never counts"
        );

        confirm(&mut fx);
        let (other, posted) = post_order(&mut fx.origin, &fx.index, "Show the session");
        fx.log.extend(posted);
        let confirmed = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan);
        assert_eq!(
            slices::clash_scope(&[api], &[order], &confirmed.lanes, &plan),
            Some(format!("inside confirmed scope {order} rev 1"))
        );
        assert_eq!(
            slices::clash_scope(&[ui], &[order], &confirmed.lanes, &plan),
            None,
            "the plot is on api, not ui"
        );
        assert_eq!(
            slices::clash_scope(&[api], &[other], &confirmed.lanes, &plan),
            None,
            "another order's scope does not cover this clash"
        );
    }

    /// Opens a lane on an order with two tasks, confirms it, and has agent A claim the first task
    /// through a readback at the confirmed hash. Returns the fixture with the claim in its log,
    /// the order's first task, and the scope.
    fn claimed_lane() -> (LaneFixture, TaskId, Scope) {
        let index = index();
        let (mut origin, opened) = open_world();
        let mut log = vec![opened];
        let (order, posted) = post_order(&mut origin, &index, "Limit failed logins");
        log.extend(posted);
        let rev = origin.world().order(order).expect("the order").rev;
        let machine = MachineKey::from_str("src/api").expect("a machine key");
        let tasks = vec![
            TaskSpec {
                title: "Limit the login".to_owned(),
                brief: "Add the 429.".to_owned(),
                scope: vec![machine.clone()],
            },
            TaskSpec {
                title: "Keep the session".to_owned(),
                brief: "Keep the session.".to_owned(),
                scope: vec![machine],
            },
        ];
        log.extend(accepted(
            &mut origin,
            &index,
            Command::System(SystemCmd::SplitReady { order, rev, tasks }),
        ));
        let task = origin.world().tasks_of(order).next().expect("a task").id;
        let agent = AgentId::from_str(scenario::AGENTS[0]).expect("an agent id");
        log.extend(accepted(
            &mut origin,
            &index,
            Command::Human {
                member: founder(),
                cmd: HumanCmd::RegisterAgent {
                    agent: agent.clone(),
                },
            },
        ));
        let scope = scope_for(order, vec![narrow_item()]);
        log.extend(accepted(
            &mut origin,
            &index,
            Command::Human {
                member: founder(),
                cmd: HumanCmd::ReviseScope {
                    order,
                    scope: Box::new(scope.clone()),
                },
            },
        ));
        let confirmation = confirmation_for(&origin, &scope);
        log.extend(accepted(
            &mut origin,
            &index,
            Command::Human {
                member: founder(),
                cmd: HumanCmd::ConfirmScope {
                    order,
                    confirmation: Box::new(confirmation),
                },
            },
        ));
        log.extend(accepted(
            &mut origin,
            &index,
            Command::Agent {
                agent: agent.clone(),
                cmd: AgentCmd::PostScopedReadback {
                    task,
                    scope_rev: scope.rev,
                    scope_hash: scope.scope_hash().expect("the scope hashes"),
                    summary: "add the 429".to_owned(),
                    predictions: Vec::new(),
                },
            },
        ));
        log.extend(accepted(
            &mut origin,
            &index,
            Command::Agent {
                agent,
                cmd: AgentCmd::Claim { task },
            },
        ));
        let fixture = LaneFixture {
            origin,
            log,
            index,
            order,
            scope: scope.clone(),
        };
        (fixture, task, scope)
    }

    #[test]
    fn a_lane_counts_its_live_leases_and_a_newer_revision_ends_them() {
        let (mut fx, _task, scope) = claimed_lane();
        let before = Mirror::from_log(fx.log.clone())
            .expect("the log replays")
            .view(&plan());
        assert_eq!(
            before.lanes[0].live_leases, 1,
            "the claimed task holds a lease"
        );

        // A Strike sends the same scope one revision on, with its one plot removed.
        let mut struck = serde_json::to_value(&scope).expect("a scope serializes");
        struck["rev"] = json!(2);
        struck["body"]["may"] = json!([]);
        let revised = accepted(
            &mut fx.origin,
            &fx.index,
            Command::Human {
                member: founder(),
                cmd: HumanCmd::ReviseScope {
                    order: fx.order,
                    scope: Box::new(serde_json::from_value(struck).expect("a scope decodes")),
                },
            },
        );
        let ended = revised
            .iter()
            .filter(|stamped| matches!(stamped.event, Event::LeaseEnded { .. }))
            .count();
        assert_eq!(ended, 1, "the revision ends the one live lease");
        fx.log.extend(revised);

        let after = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan());
        let [lane] = after.lanes.as_slice() else {
            panic!("the order keeps its lane");
        };
        assert_eq!(serde_json::to_value(lane.rev).expect("a rev"), json!(2));
        assert_eq!(lane.live_leases, 0);
        assert!(lane.may.is_empty(), "the struck plot is gone");
    }

    #[test]
    fn the_origin_route_names_the_page_scope_and_a_different_hash_or_revision_does_not() {
        let fx = lane_fixture(vec![narrow_item()]);
        let route = acan_core::confirmation_body(
            fx.origin.world(),
            fx.order,
            &founder(),
            Timestamp::from_millis(3_000),
        )
        .expect("the route body");
        let view = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan());
        let [lane] = view.lanes.as_slice() else {
            panic!("the one order with a scope has one lane");
        };

        let same = seal::check(lane, &route);
        assert_eq!(same.verdict, "match");
        assert_eq!(same.page_hash, Some(lane.scope_hash));
        assert_eq!(same.origin_hash, lane.scope_hash);

        let mut other_hash = serde_json::to_value(&route).expect("a body serializes");
        other_hash["scope_hash"] = json!(format!("sha256:{}", "0".repeat(64)));
        let other_hash: ScopeConfirmation =
            serde_json::from_value(other_hash).expect("a body decodes");
        assert_eq!(seal::check(lane, &other_hash).verdict, "mismatch");

        let mut other_rev = serde_json::to_value(&route).expect("a body serializes");
        other_rev["scope_rev"] = json!(2);
        let other_rev: ScopeConfirmation =
            serde_json::from_value(other_rev).expect("a body decodes");
        let shown = seal::check(lane, &other_rev);
        assert_eq!(shown.verdict, "mismatch");
        assert_eq!(
            serde_json::to_value(shown.origin_rev).expect("a rev"),
            json!(2)
        );
        assert_eq!(shown.page_rev, lane.rev);
    }

    fn keys(value: &serde_json::Value) -> BTreeSet<&str> {
        value
            .as_object()
            .expect("a JSON object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn a_lane_serializes_with_exactly_its_keys() {
        let fx = lane_fixture(vec![narrow_item()]);
        let view = Mirror::from_log(fx.log)
            .expect("the log replays")
            .view(&plan());
        let lane = serde_json::to_value(&view.lanes[0]).expect("a lane serializes");

        assert_eq!(
            keys(&lane),
            BTreeSet::from([
                "order",
                "rev",
                "scope_hash",
                "scope",
                "state",
                "may",
                "must_keep",
                "open",
                "confirmation",
                "live_leases",
                "stopped",
            ])
        );
        assert_eq!(
            keys(&lane["may"][0]),
            BTreeSet::from([
                "id",
                "label",
                "obligations",
                "selector",
                "change",
                "provenance",
                "broad",
                "machines",
            ])
        );
        assert_eq!(
            keys(&lane["must_keep"][0]),
            BTreeSet::from(["id", "text", "ruling_id", "machines"])
        );
        assert_eq!(
            keys(&lane["open"][0]),
            BTreeSet::from(["id", "machine", "question", "options"])
        );
        assert_eq!(
            keys(&lane["open"][0]["options"][0]),
            BTreeSet::from(["key", "label"])
        );

        assert_eq!(
            lane["order"],
            serde_json::to_value(fx.order).expect("an order id serializes")
        );
        assert_eq!(lane["rev"], 1);
        assert_eq!(lane["state"], "unconfirmed");
        assert!(lane["confirmation"].is_null());
        assert_eq!(lane["live_leases"], 0);
        assert_eq!(
            lane["scope"],
            serde_json::to_value(&fx.scope).expect("a scope serializes")
        );
        assert_eq!(
            lane["may"][0],
            json!({
                "id": "i-limit",
                "label": "the sixth failed login gets a 429",
                "obligations": [
                    "the sixth failed login gets a 429",
                    "the 429 says when to retry"
                ],
                "selector": "POST /api/login",
                "change": "add",
                "provenance": "user",
                "broad": false,
                "machines": ["src/api"]
            })
        );
        assert_eq!(
            lane["must_keep"][0],
            json!({
                "id": "k-login",
                "text": "login keeps working for a correct password",
                "ruling_id": null,
                "machines": ["src/api"]
            })
        );
        assert_eq!(
            lane["open"][0],
            json!({
                "id": "c-count",
                "machine": "src/api",
                "question": "Count failed logins per account or per address?",
                "options": [
                    {"key": "account", "label": "Per account"},
                    {"key": "address", "label": "Per address"}
                ]
            })
        );
    }
}
