//! The world: the fold of one log, the origin that extends it, and replicas that follow it.

use std::collections::{BTreeMap, BTreeSet};
use std::{fmt, mem};

use acan_intent::{ContestStatus, DiscrepancyEpisode, EpisodeState, OrderRef, ScopeRevision};
use serde::{Deserialize, Serialize};

use crate::clash::{
    self, Barrier, BarrierHold, Clash, ClashKey, ClashState, Declaration, Settlement,
};
use crate::event::{
    Actor, AgentCmd, Command, Event, HumanCmd, Rejection, ReplayError, Stamped, SystemCmd,
};
use crate::gate::{self, GateRun, GateVerdict};
use crate::ids::{
    AgentId, ClashId, CodeRev, GateRunId, MemberId, OrderId, OrderRev, QuestionId, ReadbackId, Seq,
    TaskId, Timestamp, WitnessId, WorldId,
};
use crate::index::CodeIndex;
use crate::job::Job;
use crate::lane::{
    self, ConfirmedInputs, LaneRun, PendingContrast, ScopeDraftOutcome, ScopeLane, ScopedReadback,
};
use crate::lease::{self, Claim, DECLARE_BARRIER_MS};
use crate::member::{self, AgentPrincipal, Member, Role, SettleRight};
use crate::order::{
    self, Order, OrderPhase, OrderState, Question, Readback, Stage, Task, TaskState, Witness,
};
use crate::repair::{self, LaneVerdict};
use crate::ruling::{self, Append, Ledger};

/// Which repository a world tracks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldRef {
    /// The world id.
    pub id: WorldId,
    /// The repository, as the host names it.
    pub repo: String,
}

/// What the host supplies to every `execute`: the time and the current code index.
#[derive(Debug, Clone, Copy)]
pub struct Ctx<'a> {
    /// The origin's clock.
    pub now: Timestamp,
    /// The latest scan. Detection reads it; replicas never need it.
    pub index: &'a CodeIndex,
}

/// Which loop step made a model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Splitting an order.
    Split,
    /// Reviewing readbacks.
    Review,
    /// Explaining a clash.
    Explain,
    /// Comparing a screenshot witness.
    Screenshot,
}

/// Token totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCount {
    /// Prompt tokens.
    pub input: u64,
    /// Completion tokens.
    pub output: u64,
    /// Number of calls.
    pub calls: u32,
}

/// Model usage per model and step, for the usage panel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Totals keyed by model name, then step. Nested rather than tuple-keyed so it serializes to JSON.
    pub totals: BTreeMap<String, BTreeMap<Step, TokenCount>>,
}

// Wider than the ids they mint, so one past `u32::MAX` marks a kind as exhausted
// instead of saturating onto an id already in use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Counters {
    order: u64,
    task: u64,
    witness: u64,
    readback: u64,
    question: u64,
    clash: u64,
    gate: u64,
}

/// The fold of a world log. Read-only outside the crate; only applying events changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct World {
    pub(crate) world_ref: WorldRef,
    pub(crate) next_seq: Seq,
    // Advanced by apply from the ids it sees, so replay and the origin agree on the next id.
    ids: Counters,
    pub(crate) index_rev: Option<CodeRev>,
    pub(crate) members: BTreeMap<MemberId, Member>,
    pub(crate) agents: BTreeMap<AgentId, AgentPrincipal>,
    pub(crate) witnesses: BTreeMap<WitnessId, Witness>,
    pub(crate) orders: BTreeMap<OrderId, Order>,
    pub(crate) tasks: BTreeMap<TaskId, Task>,
    pub(crate) readbacks: BTreeMap<ReadbackId, Readback>,
    pub(crate) questions: BTreeMap<QuestionId, Question>,
    pub(crate) clashes: BTreeMap<ClashId, Clash>,
    pub(crate) clash_keys: BTreeMap<ClashKey, ClashId>,
    pub(crate) gates: BTreeMap<GateRunId, GateRun>,
    pub(crate) usage: ModelUsage,
    pub(crate) lanes: BTreeMap<OrderId, ScopeLane>,
    // What the scope-draft job reported, by order and the order revision it was derived at. A
    // drafted scope carries no authority; the lane holds the only scope a claim may rest on.
    pub(crate) scope_drafts: BTreeMap<(OrderId, OrderRev), ScopeDraftOutcome>,
    // Shares the readback id counter with the legacy readbacks, so a claim's readback id
    // names exactly one readback of either kind.
    pub(crate) scoped_readbacks: BTreeMap<ReadbackId, ScopedReadback>,
    pub(crate) ledger: Ledger,
}

impl World {
    /// The world this log belongs to.
    #[must_use]
    pub fn world_ref(&self) -> &WorldRef {
        &self.world_ref
    }

    /// The seq the next event will carry. A client resumes its stream from here.
    #[must_use]
    pub fn next_seq(&self) -> Seq {
        self.next_seq
    }

    /// The index revision of the last `IndexUpdated`, if any.
    #[must_use]
    pub fn index_rev(&self) -> Option<&CodeRev> {
        self.index_rev.as_ref()
    }

    /// A member by id.
    #[must_use]
    pub fn member(&self, id: &MemberId) -> Option<&Member> {
        self.members.get(id)
    }

    /// An agent principal by id.
    #[must_use]
    pub fn agent(&self, id: &AgentId) -> Option<&AgentPrincipal> {
        self.agents.get(id)
    }

    /// A witness by id.
    #[must_use]
    pub fn witness(&self, id: WitnessId) -> Option<&Witness> {
        self.witnesses.get(&id)
    }

    /// An order by id.
    #[must_use]
    pub fn order(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    /// Every order, in id order.
    pub fn orders(&self) -> impl Iterator<Item = &Order> {
        self.orders.values()
    }

    /// A task by id.
    #[must_use]
    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.get(&id)
    }

    /// The tasks of an order, in id order.
    pub fn tasks_of(&self, order: OrderId) -> impl Iterator<Item = &Task> {
        self.tasks.values().filter(move |t| t.order == order)
    }

    /// A readback by id.
    #[must_use]
    pub fn readback(&self, id: ReadbackId) -> Option<&Readback> {
        self.readbacks.get(&id)
    }

    /// The readbacks of a task, in id order.
    pub fn readbacks_of(&self, task: TaskId) -> impl Iterator<Item = &Readback> {
        self.readbacks.values().filter(move |r| r.task == task)
    }

    /// The order's unanswered question, if any. At most one exists.
    #[must_use]
    pub fn open_question(&self, order: OrderId) -> Option<&Question> {
        self.questions
            .values()
            .find(|q| q.order == order && q.answer.is_none())
    }

    /// A clash by id.
    #[must_use]
    pub fn clash(&self, id: ClashId) -> Option<&Clash> {
        self.clashes.get(&id)
    }

    /// Every clash ever found, including settled and dissolved ones.
    pub fn clashes(&self) -> impl Iterator<Item = &Clash> {
        self.clashes.values()
    }

    /// The clashes a task is party to.
    pub fn clashes_for(&self, task: TaskId) -> impl Iterator<Item = &Clash> {
        self.clashes.values().filter(move |c| c.key.involves(task))
    }

    /// A gate run by id.
    #[must_use]
    pub fn gate(&self, id: GateRunId) -> Option<&GateRun> {
        self.gates.get(&id)
    }

    /// Model usage so far.
    #[must_use]
    pub fn usage(&self) -> &ModelUsage {
        &self.usage
    }

    /// The scope-first state of an order. `None` for an order in the legacy lane.
    #[must_use]
    pub fn lane(&self, order: OrderId) -> Option<&ScopeLane> {
        self.lanes.get(&order)
    }

    /// The latest scope-draft outcome of an order, with the order revision it was for. A drafted
    /// scope confirms nothing: the scope a claim rests on is [`World::lane`]'s.
    #[must_use]
    pub fn scope_draft(&self, order: OrderId) -> Option<(OrderRev, &ScopeDraftOutcome)> {
        self.scope_drafts
            .range((order, OrderRev::ZERO)..=(order, OrderRev::new(u32::MAX)))
            .next_back()
            .map(|((_, rev), outcome)| (*rev, outcome))
    }

    /// The scope-draft outcome recorded for an order at one of its revisions.
    pub(crate) fn scope_draft_at(
        &self,
        order: OrderId,
        rev: OrderRev,
    ) -> Option<&ScopeDraftOutcome> {
        self.scope_drafts.get(&(order, rev))
    }

    /// Every scope-first order's lane, in order id order.
    pub fn lanes(&self) -> impl Iterator<Item = &ScopeLane> {
        self.lanes.values()
    }

    /// The ruling log.
    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// A scope-first readback by id.
    #[must_use]
    pub fn scoped_readback(&self, id: ReadbackId) -> Option<&ScopedReadback> {
        self.scoped_readbacks.get(&id)
    }

    /// The scope-first readbacks of a task, in id order.
    pub fn scoped_readbacks_of(&self, task: TaskId) -> impl Iterator<Item = &ScopedReadback> {
        self.scoped_readbacks
            .values()
            .filter(move |r| r.task == task)
    }

    /// How many contests were ever filed in this world.
    #[must_use]
    pub fn contests_filed(&self) -> usize {
        self.lanes.values().map(|lane| lane.contests.len()).sum()
    }

    /// The scope-first runs the host still has to prepare or answer with a receipt: every run
    /// without a receipt that awaits one, gating or queued by a re-bind. The host's side of the
    /// lane; no [`Job`] covers it.
    #[must_use]
    pub fn pending_contrasts(&self) -> Vec<PendingContrast<'_>> {
        self.lanes
            .values()
            .flat_map(|l| {
                l.runs
                    .iter()
                    .filter(|(run, r)| {
                        r.receipt.is_none() && lane::awaits_receipt(self, r.task, **run)
                    })
                    .map(|(run, r)| PendingContrast {
                        order: l.order,
                        task: r.task,
                        run: *run,
                        head: &r.head,
                        prepared: r.prepared.as_ref(),
                    })
            })
            .collect()
    }

    /// The episodes of `order`'s lane as the judge reads them, for `Order::episodes`. A repeat of
    /// a failure is judged with its state and bounce count, not as a first failure. Empty for an
    /// order without a scope-first lane.
    ///
    /// # Errors
    ///
    /// Returns [`Rejection`] when the order id has no contract spelling.
    pub fn judged_episodes(&self, order: OrderId) -> Result<Vec<DiscrepancyEpisode>, Rejection> {
        let Some(lane) = self.lane(order) else {
            return Ok(Vec::new());
        };
        let order_ref = lane::order_ref(order)?;
        Ok(lane
            .episodes
            .values()
            .map(|episode| episode.judged(&order_ref, &lane.lineage))
            .collect())
    }

    /// Whether `run` belongs to a scope-first lane and has no receipt. Only a re-bind leaves a
    /// queued run without one: it must be receipted on the new head before it can land.
    fn is_unreceipted(&self, run: GateRunId) -> bool {
        self.lane_of_run(run)
            .and_then(|lane| lane.runs.get(&run))
            .is_some_and(|lane_run| lane_run.receipt.is_none())
    }

    fn lane_of_run(&self, run: GateRunId) -> Option<&ScopeLane> {
        let task = self.gates.get(&run)?.task;
        let order = self.tasks.get(&task)?.order;
        self.lanes.get(&order)
    }

    fn lane_of_run_mut(&mut self, run: GateRunId) -> Option<&mut LaneRun> {
        let task = self.gates.get(&run)?.task;
        let order = self.tasks.get(&task)?.order;
        self.lanes.get_mut(&order)?.runs.get_mut(&run)
    }

    /// The order's lifecycle phase, derived from its tasks, questions and clashes.
    /// See [`OrderPhase`] for the rules. `None` for an unknown order.
    #[must_use]
    pub fn phase(&self, order: OrderId) -> Option<OrderPhase> {
        let order = self.orders.get(&order)?;
        let phase = match order.state {
            OrderState::Cancelled => OrderPhase::Cancelled,
            OrderState::Splitting => OrderPhase::Splitting,
            OrderState::Split => self.split_phase(order.id),
        };
        Some(phase)
    }

    fn split_phase(&self, order: OrderId) -> OrderPhase {
        let tasks: Vec<&Task> = self.tasks_of(order).collect();
        let any = |pred: fn(&TaskState) -> bool| tasks.iter().any(|t| pred(&t.state));
        if tasks
            .iter()
            .all(|t| matches!(t.state, TaskState::Landed { .. }))
        {
            return OrderPhase::Landed;
        }
        if let Some(question) = self.open_question(order) {
            return OrderPhase::Asking {
                question: question.id,
            };
        }
        if tasks
            .iter()
            .any(|t| self.clashes_for(t.id).any(Clash::blocking))
        {
            return OrderPhase::Clashing;
        }
        let queued = any(|s| matches!(s, TaskState::Queued { .. }));
        let active = any(|s| matches!(s, TaskState::Open | TaskState::Claimed { .. }));
        if queued && !active {
            OrderPhase::Landing
        } else if any(|s| {
            matches!(
                s,
                TaskState::Claimed {
                    stage: Stage::Gating { .. },
                    ..
                }
            )
        }) {
            OrderPhase::Gating
        } else if any(|s| {
            matches!(
                s,
                TaskState::Claimed {
                    stage: Stage::Building { .. },
                    ..
                }
            )
        }) {
            OrderPhase::Building
        } else {
            OrderPhase::ReadingBack
        }
    }

    /// What the order's declare barrier waits for, while it holds. A declared task of the
    /// order is waiting for its siblings to declare exactly while this is `Some`. `None` for an
    /// unknown order, and for a barrier that is unarmed, lifted, or has no sibling left to wait
    /// for. A barrier past its deadline holds until the origin records `BarrierExpired`.
    #[must_use]
    pub fn barrier(&self, order: OrderId) -> Option<BarrierHold> {
        let Barrier::Armed { deadline } = self.orders.get(&order)?.barrier else {
            return None;
        };
        let waiting_for: Vec<TaskId> = self
            .tasks_of(order)
            .filter(|t| {
                matches!(
                    t.state,
                    TaskState::Open
                        | TaskState::Claimed {
                            stage: Stage::Declaring,
                            ..
                        }
                )
            })
            .map(|t| t.id)
            .collect();
        (!waiting_for.is_empty()).then_some(BarrierHold {
            deadline,
            waiting_for,
        })
    }

    /// Queued tasks in merge order: by the seq at which each was queued.
    #[must_use]
    pub fn land_queue(&self) -> Vec<TaskId> {
        let mut queued: Vec<(Seq, TaskId)> = self
            .tasks
            .values()
            .filter_map(|t| match t.state {
                TaskState::Queued { queued_at, .. } => Some((queued_at, t.id)),
                _ => None,
            })
            .collect();
        queued.sort_unstable();
        queued.into_iter().map(|(_, task)| task).collect()
    }

    /// The jobs the host should be running now. See [`Job`] for the contract.
    ///
    /// `Split` for each splitting order; `Barrier` for each split order whose declare barrier is
    /// armed; `DraftScope` for each order that still needs a scope draft (see
    /// `wants_scope_draft`). The legacy model review is not derived: no order gets a `Review`
    /// job. `Explain` for each blocking clash without an explanation; `RunWitness` for each
    /// frozen witness of a current gate run without a result; `Land` for the front of the land
    /// queue only.
    #[must_use]
    pub fn jobs(&self) -> Vec<Job> {
        let mut jobs = Vec::new();
        for order in self.orders.values() {
            match order.state {
                OrderState::Splitting => jobs.push(Job::Split {
                    order: order.id,
                    rev: order.rev,
                }),
                OrderState::Split => {
                    if let Barrier::Armed { deadline } = order.barrier {
                        jobs.push(Job::Barrier {
                            order: order.id,
                            deadline,
                        });
                    }
                }
                OrderState::Cancelled => {}
            }
            if self.wants_scope_draft(order) {
                jobs.push(Job::DraftScope {
                    order: order.id,
                    rev: order.rev,
                });
            }
        }
        jobs.extend(
            self.clashes
                .values()
                .filter(|c| c.blocking() && c.explanation.is_none())
                .map(|c| Job::Explain { clash: c.id }),
        );
        for task in self.tasks.values() {
            let TaskState::Claimed {
                stage: Stage::Gating { run, .. },
                ..
            } = &task.state
            else {
                continue;
            };
            let Some(gate) = self.gates.get(run) else {
                continue;
            };
            jobs.extend(
                gate.witnesses
                    .iter()
                    .filter(|w| !gate.results.contains_key(w))
                    .map(|&witness| Job::RunWitness { run: *run, witness }),
            );
        }
        let front = self
            .land_queue()
            .first()
            .and_then(|&id| self.tasks.get(&id));
        if let Some(Task {
            id,
            state: TaskState::Queued { head, run, .. },
            ..
        }) = front
            && !self.is_unreceipted(*run)
        {
            jobs.push(Job::Land {
                task: *id,
                head: head.clone(),
            });
        }
        jobs
    }

    /// Whether an order needs a scope draft: it is not cancelled, has no scope, has no drafted
    /// scope at its current revision, and has no claimed or landed work. A draft is requested
    /// once per revision; a failed draft is retried by the job loop and not stored as a draft.
    fn wants_scope_draft(&self, order: &Order) -> bool {
        order.state != OrderState::Cancelled
            && self.lane(order.id).is_none()
            && !matches!(
                self.scope_draft_at(order.id, order.rev),
                Some(ScopeDraftOutcome::Drafted { .. })
            )
            && matches!(
                self.phase(order.id),
                Some(OrderPhase::Splitting | OrderPhase::ReadingBack)
            )
    }

    /// The readbacks of the order that the legacy review command may still judge, ignoring
    /// whether a question is open. None for a scope-first order: the model review authorizes
    /// nothing there. No job asks for these verdicts any more.
    pub(crate) fn unreviewed(&self, order: &Order) -> BTreeSet<ReadbackId> {
        if self.lanes.contains_key(&order.id) {
            return BTreeSet::new();
        }
        self.readbacks
            .values()
            .filter(|r| {
                self.tasks.get(&r.task).is_some_and(|t| {
                    t.order == order.id
                        && matches!(t.state, TaskState::Open | TaskState::Claimed { .. })
                })
            })
            .filter(|r| r.verdict.as_ref().is_none_or(|v| v.rev != order.rev))
            .map(|r| r.id)
            .collect()
    }

    /// What `member` sending `SettleClash { clash, choice }` would do. `None` when the member
    /// is unknown or the choice is not on the clash's menu.
    ///
    /// Owners settle. A member settles when the choice's yielding task is held by one of
    /// their own agents. Otherwise the member may only propose.
    #[must_use]
    pub fn settle_right(
        &self,
        member: &MemberId,
        clash: &Clash,
        choice: &Settlement,
    ) -> Option<SettleRight> {
        let role = self.members.get(member)?.role;
        if !clash.menu().contains(choice) {
            return None;
        }
        if role == Role::Owner {
            return Some(SettleRight::Settle);
        }
        let own = choice
            .yielder(&clash.key)
            .and_then(|task| self.holder(task))
            .and_then(|agent| self.agents.get(agent))
            .is_some_and(|principal| &principal.owner == member);
        Some(if own {
            SettleRight::Settle
        } else {
            SettleRight::Propose
        })
    }

    /// The agent holding `task` under a live claim.
    pub(crate) fn holder(&self, task: TaskId) -> Option<&AgentId> {
        match &self.tasks.get(&task)?.state {
            TaskState::Claimed { claim, .. } => Some(&claim.lease.holder),
            _ => None,
        }
    }

    /// Declarations of every claimed task that has one. Detection runs over these.
    pub(crate) fn live_declarations(&self) -> impl Iterator<Item = &Declaration> {
        self.tasks.values().filter_map(|t| match &t.state {
            TaskState::Claimed { stage, .. } => stage.declaration(),
            _ => None,
        })
    }

    pub(crate) fn next_order_id(&self) -> Result<OrderId, Rejection> {
        mint(self.ids.order, "order ids are exhausted").map(OrderId::new)
    }

    pub(crate) fn next_task_ids(&self, n: usize) -> Result<Vec<TaskId>, Rejection> {
        mint_many(self.ids.task, n, "task ids are exhausted", TaskId::new)
    }

    pub(crate) fn next_witness_ids(&self, n: usize) -> Result<Vec<WitnessId>, Rejection> {
        mint_many(
            self.ids.witness,
            n,
            "witness ids are exhausted",
            WitnessId::new,
        )
    }

    pub(crate) fn next_witness_id(&self) -> Result<WitnessId, Rejection> {
        mint(self.ids.witness, "witness ids are exhausted").map(WitnessId::new)
    }

    pub(crate) fn next_readback_id(&self) -> Result<ReadbackId, Rejection> {
        mint(self.ids.readback, "readback ids are exhausted").map(ReadbackId::new)
    }

    pub(crate) fn next_question_id(&self) -> Result<QuestionId, Rejection> {
        mint(self.ids.question, "question ids are exhausted").map(QuestionId::new)
    }

    pub(crate) fn next_clash_ids(&self, n: usize) -> Result<Vec<ClashId>, Rejection> {
        mint_many(self.ids.clash, n, "clash ids are exhausted", ClashId::new)
    }

    pub(crate) fn next_gate_id(&self) -> Result<GateRunId, Rejection> {
        mint(self.ids.gate, "gate run ids are exhausted").map(GateRunId::new)
    }

    fn opened(world_ref: WorldRef) -> Self {
        Self {
            world_ref,
            next_seq: Seq::ZERO,
            ids: Counters::default(),
            index_rev: None,
            members: BTreeMap::new(),
            agents: BTreeMap::new(),
            witnesses: BTreeMap::new(),
            orders: BTreeMap::new(),
            tasks: BTreeMap::new(),
            readbacks: BTreeMap::new(),
            questions: BTreeMap::new(),
            clashes: BTreeMap::new(),
            clash_keys: BTreeMap::new(),
            gates: BTreeMap::new(),
            usage: ModelUsage::default(),
            lanes: BTreeMap::new(),
            scope_drafts: BTreeMap::new(),
            scoped_readbacks: BTreeMap::new(),
            ledger: Ledger::default(),
        }
    }

    /// Folds a whole log. The first event must be `WorldOpened` at seq 0.
    fn from_log(log: impl IntoIterator<Item = Stamped>) -> Result<Self, ReplayError> {
        let mut log = log.into_iter();
        let first = log.next().ok_or(ReplayError::NotOpened)?;
        let Event::WorldOpened { world, .. } = &first.event else {
            return Err(ReplayError::NotOpened);
        };
        if first.seq != Seq::ZERO {
            return Err(ReplayError::NotOpened);
        }
        let mut folded = Self::opened(world.clone());
        folded.apply(&first);
        for stamped in log {
            if stamped.seq != folded.next_seq {
                return Err(ReplayError::SeqGap {
                    expected: folded.next_seq,
                    got: stamped.seq,
                });
            }
            folded.check(&stamped)?;
            folded.apply(&stamped);
        }
        Ok(folded)
    }

    /// Rejects a stored or streamed event that refers to an entity the log never created, or
    /// creates one twice. The origin never records one; a damaged or forged log can, and the
    /// decide functions rely on these references.
    fn check(&self, stamped: &Stamped) -> Result<(), ReplayError> {
        check_signer(stamped)
            .and_then(|()| self.check_refs(&stamped.event))
            .map_err(|why| ReplayError::InvalidEvent {
                seq: stamped.seq,
                why,
            })
    }

    fn check_refs(&self, event: &Event) -> Result<(), String> {
        match event {
            Event::WorldOpened { .. } => Err("the world is already open".to_owned()),
            Event::MemberAdded { .. }
            | Event::AgentRegistered { .. }
            | Event::IndexUpdated { .. }
            | Event::ModelUsed { .. } => Ok(()),
            Event::OrderPosted {
                order, witnesses, ..
            } => {
                fresh(&self.orders, order, "order")?;
                witnesses
                    .iter()
                    .try_for_each(|w| fresh(&self.witnesses, &w.id, "witness"))
            }
            Event::OrderSplit { order, tasks, .. } => {
                known(&self.orders, order, "order")?;
                tasks
                    .iter()
                    .try_for_each(|(id, _)| fresh(&self.tasks, id, "task"))
            }
            Event::ReadbackPosted { readback } => {
                known(&self.tasks, &readback.task, "task")?;
                fresh(&self.readbacks, &readback.id, "readback")
            }
            Event::ReadbacksReviewed {
                order, verdicts, ..
            } => {
                known(&self.orders, order, "order")?;
                verdicts
                    .iter()
                    .try_for_each(|(id, _)| known(&self.readbacks, id, "readback"))
            }
            Event::QuestionPinned { question } => {
                known(&self.orders, &question.order, "order")?;
                fresh(&self.questions, &question.id, "question")
            }
            Event::QuestionAnswered {
                question, witness, ..
            } => {
                known(&self.questions, question, "question")?;
                fresh(&self.witnesses, &witness.id, "witness")
            }
            Event::OrderRevised {
                order, witnesses, ..
            } => {
                known(&self.orders, order, "order")?;
                witnesses
                    .iter()
                    .try_for_each(|w| known(&self.witnesses, w, "witness"))
            }
            Event::LeaseGranted { task, readback, .. } => {
                known(&self.tasks, task, "task")?;
                if self.scoped_readbacks.contains_key(readback) {
                    return Ok(());
                }
                known(&self.readbacks, readback, "readback")
            }
            Event::LeaseRenewed { task, .. }
            | Event::LeaseEnded { task, .. }
            | Event::BuildCleared { task }
            | Event::Landed { task, .. }
            | Event::LandFailed { task, .. } => known(&self.tasks, task, "task"),
            Event::Declared { decl } => known(&self.tasks, &decl.task, "task"),
            Event::ClashFound { clash, key, .. } => {
                fresh(&self.clashes, clash, "clash")?;
                if key.a() >= key.b() {
                    return Err(format!("clash {clash} has an unordered key"));
                }
                known(&self.tasks, &key.a(), "task")?;
                known(&self.tasks, &key.b(), "task")
            }
            Event::ClashDissolved { clash }
            | Event::ClashExplained { clash, .. }
            | Event::ClashProposed { clash, .. }
            | Event::ClashSettled { clash, .. } => known(&self.clashes, clash, "clash"),
            Event::BuildReported {
                task,
                run,
                witnesses,
                ..
            } => {
                known(&self.tasks, task, "task")?;
                fresh(&self.gates, run, "gate run")?;
                witnesses
                    .iter()
                    .try_for_each(|w| known(&self.witnesses, w, "witness"))
            }
            Event::WitnessReported { run, .. } | Event::GateClosed { run, .. } => {
                known(&self.gates, run, "gate run")
            }
            Event::OrderCancelled { order } | Event::BarrierExpired { order } => {
                known(&self.orders, order, "order")
            }
            Event::ScopeRevised {
                order,
                scope,
                scope_hash,
            } => {
                known(&self.orders, order, "order")?;
                if scope.scope_hash().ok() != Some(*scope_hash) {
                    return Err(format!("the scope of order {order} does not have its hash"));
                }
                match self.lanes.get(order) {
                    Some(lane) if scope.rev <= lane.scope.rev => {
                        Err(format!("order {order} revises its scope backwards"))
                    }
                    _ => Ok(()),
                }
            }
            Event::ScopeDrafted { order, rev, draft } => {
                known(&self.orders, order, "order")?;
                if draft.order.as_str() != order.to_string() {
                    return Err(format!("the draft of order {order} names another order"));
                }
                if self.lanes.contains_key(order) {
                    return Err(format!("order {order} is drafted after it has a scope"));
                }
                if matches!(
                    self.scope_draft_at(*order, *rev),
                    Some(ScopeDraftOutcome::Drafted { .. })
                ) {
                    return Err(format!("order {order} is drafted twice at {rev:?}"));
                }
                Ok(())
            }
            Event::ScopeDraftFailed { order, .. } => known(&self.orders, order, "order"),
            Event::ScopeConfirmed {
                order,
                confirmation,
            } => {
                let lane = self
                    .lanes
                    .get(order)
                    .ok_or_else(|| format!("order {order} has no scope"))?;
                if confirmation.scope_rev != lane.scope.rev
                    || confirmation.scope_hash != lane.scope_hash
                {
                    return Err(format!("order {order} confirms another scope revision"));
                }
                Ok(())
            }
            Event::ConfirmedInputsRecorded {
                order,
                revision,
                base,
                ..
            } => {
                let lane = self
                    .lanes
                    .get(order)
                    .ok_or_else(|| format!("order {order} has no scope"))?;
                let confirmed_base = lane.confirmation.as_ref().map(|found| &found.base_commit);
                if *revision != lane.scope.rev || confirmed_base != Some(base) {
                    return Err(format!(
                        "order {order} records inputs its confirmation does not name"
                    ));
                }
                Ok(())
            }
            Event::ScopeRebound {
                order,
                scope,
                scope_hash,
                confirmation,
                runs,
            } => {
                let lane = self
                    .lanes
                    .get(order)
                    .ok_or_else(|| format!("order {order} has no scope"))?;
                if scope.rev != lane.scope.rev
                    || scope.body != lane.scope.body
                    || scope.scope_hash().ok() != Some(*scope_hash)
                    || confirmation.scope_hash != *scope_hash
                    || confirmation.base_commit != scope.base_commit
                {
                    return Err(format!("order {order} re-binds a scope it does not hold"));
                }
                if let Some(run) = runs.iter().find(|run| !lane.runs.contains_key(*run)) {
                    return Err(format!("order {order} re-binds run {run}, which it lacks"));
                }
                Ok(())
            }
            Event::ScopedReadbackPosted { readback } => {
                known(&self.tasks, &readback.task, "task")?;
                fresh(&self.readbacks, &readback.id, "readback")?;
                fresh(&self.scoped_readbacks, &readback.id, "readback")
            }
            Event::CandidatePrepared { run, .. } | Event::ReceiptRecorded { run, .. } => self
                .lane_of_run(*run)
                .filter(|lane| lane.runs.contains_key(run))
                .map(drop)
                .ok_or_else(|| format!("unknown scope-first run {run}")),
            Event::ContestFiled { contest } => {
                let lane = lane_named(&self.lanes, &contest.order)?;
                if !lane.episodes.contains_key(&contest.target) {
                    return Err(format!("unknown episode {}", contest.target));
                }
                if self
                    .lanes
                    .values()
                    .any(|l| l.contests.contains_key(&contest.id))
                {
                    return Err(format!("contest {} already exists", contest.id));
                }
                Ok(())
            }
            Event::ContestResolved { order, contest, .. } => self
                .lanes
                .get(order)
                .filter(|lane| lane.contests.contains_key(contest))
                .map(drop)
                .ok_or_else(|| format!("unknown contest {contest}")),
            Event::RulingRecorded { event } => match self.ledger.check_append(event) {
                Ok(Append::Next) => Ok(()),
                Ok(Append::Duplicate) => Err("the ruling is already recorded".to_owned()),
                Err(refusal) => Err(refusal.to_string()),
            },
        }
    }

    /// Stamps `events` at the next seqs, applies each, and appends it to `batch`.
    fn record(
        &mut self,
        at: Timestamp,
        actor: &Actor,
        events: Vec<Event>,
        batch: &mut Vec<Stamped>,
    ) {
        for event in events {
            let stamped = Stamped {
                seq: self.next_seq,
                at,
                actor: actor.clone(),
                event,
            };
            self.apply(&stamped);
            batch.push(stamped);
        }
    }

    /// The only writer of world state. Never fails, reads a clock, or runs detection;
    /// `seq` continuity is checked by the caller.
    fn apply(&mut self, stamped: &Stamped) {
        match &stamped.event {
            Event::WorldOpened { founder, .. } => {
                let owner = Member {
                    id: founder.clone(),
                    role: Role::Owner,
                };
                self.members.insert(founder.clone(), owner);
            }
            Event::MemberAdded { member } => {
                self.members.insert(member.id.clone(), member.clone());
            }
            Event::AgentRegistered { agent } => {
                self.agents.insert(agent.id.clone(), agent.clone());
            }
            Event::IndexUpdated { rev } => self.index_rev = Some(rev.clone()),
            Event::OrderPosted {
                order,
                machine,
                sentence,
                witnesses,
                posted_by,
            } => {
                bump(&mut self.ids.order, order.get());
                for witness in witnesses {
                    bump(&mut self.ids.witness, witness.id.get());
                    self.witnesses.insert(witness.id, witness.clone());
                }
                let posted = Order {
                    id: *order,
                    machine: machine.clone(),
                    sentence: sentence.clone(),
                    rev: OrderRev::ZERO,
                    witnesses: witnesses.iter().map(|w| w.id).collect(),
                    posted_by: posted_by.clone(),
                    state: OrderState::Splitting,
                    barrier: Barrier::Unarmed,
                };
                self.orders.insert(*order, posted);
            }
            Event::OrderSplit { order, tasks, .. } => {
                if let Some(o) = self.orders.get_mut(order) {
                    o.state = OrderState::Split;
                }
                for (id, spec) in tasks {
                    bump(&mut self.ids.task, id.get());
                    let task = Task {
                        id: *id,
                        order: *order,
                        spec: spec.clone(),
                        state: TaskState::Open,
                        last_generation: None,
                    };
                    self.tasks.insert(*id, task);
                }
            }
            Event::ReadbackPosted { readback } => {
                bump(&mut self.ids.readback, readback.id.get());
                self.readbacks.insert(readback.id, readback.clone());
            }
            Event::ReadbacksReviewed { verdicts, .. } => {
                for (id, verdict) in verdicts {
                    if let Some(readback) = self.readbacks.get_mut(id) {
                        readback.verdict = Some(verdict.clone());
                    }
                }
            }
            Event::QuestionPinned { question } => {
                bump(&mut self.ids.question, question.id.get());
                self.questions.insert(question.id, question.clone());
            }
            Event::QuestionAnswered {
                question,
                option,
                witness,
            } => {
                if let Some(q) = self.questions.get_mut(question) {
                    q.answer = Some(*option);
                }
                bump(&mut self.ids.witness, witness.id.get());
                self.witnesses.insert(witness.id, witness.clone());
            }
            Event::OrderRevised {
                order,
                rev,
                witnesses,
            } => {
                if let Some(o) = self.orders.get_mut(order) {
                    o.rev = *rev;
                    o.witnesses.clone_from(witnesses);
                }
            }
            Event::LeaseGranted {
                task,
                lease,
                readback,
            } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.last_generation = Some(lease.generation);
                    t.state = TaskState::Claimed {
                        claim: Claim {
                            lease: lease.clone(),
                            readback: *readback,
                        },
                        stage: Stage::Declaring,
                    };
                }
            }
            Event::LeaseRenewed {
                task,
                generation,
                expires_at,
            } => {
                if let Some(TaskState::Claimed { claim, .. }) =
                    self.tasks.get_mut(task).map(|t| &mut t.state)
                    && claim.lease.generation == *generation
                {
                    claim.lease.expires_at = *expires_at;
                }
            }
            Event::LeaseEnded {
                task, generation, ..
            } => {
                if let Some(t) = self.tasks.get_mut(task)
                    && matches!(&t.state, TaskState::Claimed { claim, .. } if claim.lease.generation == *generation)
                {
                    t.state = TaskState::Open;
                }
            }
            Event::Declared { decl } => {
                self.restage(decl.task, |_| Stage::Declared { decl: decl.clone() });
                let deadline = stamped.at.plus_millis(DECLARE_BARRIER_MS);
                self.rebar(decl.task, |barrier| match barrier {
                    Barrier::Unarmed => Barrier::Armed { deadline },
                    other => other,
                });
            }
            Event::BarrierExpired { order } => {
                if let Some(o) = self.orders.get_mut(order) {
                    o.barrier = Barrier::Lifted;
                }
            }
            Event::ClashFound {
                clash,
                key,
                kind,
                found_at,
                contract,
            } => {
                bump(&mut self.ids.clash, clash.get());
                let found = Clash {
                    id: *clash,
                    key: key.clone(),
                    kind: *kind,
                    found_at: found_at.clone(),
                    contract: contract.clone(),
                    explanation: None,
                    state: ClashState::Open,
                };
                self.clashes.insert(*clash, found);
                self.clash_keys.insert(key.clone(), *clash);
                // A gating party's run is abandoned, so a late result cannot queue it past
                // the clash.
                for party in [key.a(), key.b()] {
                    self.restage(party, |stage| match stage {
                        Stage::Building { decl } | Stage::Gating { decl, .. } => {
                            Stage::Declared { decl }
                        }
                        other => other,
                    });
                }
            }
            Event::ClashDissolved { clash } => {
                if let Some(c) = self.clashes.get_mut(clash) {
                    c.state = ClashState::Dissolved;
                }
            }
            Event::ClashExplained { clash, explanation } => {
                if let Some(c) = self.clashes.get_mut(clash) {
                    c.explanation = Some(explanation.clone());
                }
            }
            Event::ClashProposed { clash, choice, by } => {
                if let Some(c) = self.clashes.get_mut(clash) {
                    c.state = ClashState::Proposed {
                        choice: choice.clone(),
                        by: by.clone(),
                    };
                }
            }
            Event::ClashSettled { clash, choice, by } => {
                if let Some(c) = self.clashes.get_mut(clash) {
                    c.state = ClashState::Settled {
                        choice: choice.clone(),
                        by: by.clone(),
                    };
                }
                if let Settlement::Narrow { task } = choice {
                    self.restage(*task, |stage| match stage {
                        Stage::Declared { .. } | Stage::Building { .. } => Stage::Declaring,
                        other => other,
                    });
                }
            }
            Event::BuildCleared { task } => {
                self.restage(*task, |stage| match stage {
                    Stage::Declared { decl } => Stage::Building { decl },
                    other => other,
                });
                self.rebar(*task, |_| Barrier::Lifted);
            }
            Event::BuildReported {
                task,
                generation,
                head,
                run,
                rev,
                witnesses,
            } => {
                bump(&mut self.ids.gate, run.get());
                let order = self.tasks.get(task).map(|t| t.order);
                if let Some(lane) = order.and_then(|o| self.lanes.get_mut(&o)) {
                    lane.runs.insert(
                        *run,
                        LaneRun {
                            task: *task,
                            head: head.clone(),
                            prepared: None,
                            receipt: None,
                        },
                    );
                }
                let started = GateRun {
                    id: *run,
                    task: *task,
                    generation: *generation,
                    rev: *rev,
                    head: head.clone(),
                    witnesses: witnesses.clone(),
                    results: BTreeMap::new(),
                };
                self.gates.insert(*run, started);
                self.restage(*task, |stage| match stage {
                    Stage::Building { decl } => Stage::Gating {
                        decl,
                        run: *run,
                        head: head.clone(),
                    },
                    other => other,
                });
            }
            Event::WitnessReported {
                run,
                witness,
                outcome,
            } => {
                if let Some(gate) = self.gates.get_mut(run) {
                    gate.results.insert(*witness, outcome.clone());
                }
            }
            Event::GateClosed { run, verdict } => self.close_gate(*run, verdict, stamped.seq),
            Event::Landed { task, commit } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.state = TaskState::Landed {
                        commit: commit.clone(),
                    };
                }
            }
            Event::LandFailed { task, .. } => {
                if let Some(t) = self.tasks.get_mut(task) {
                    t.state = TaskState::Open;
                }
            }
            Event::OrderCancelled { order } => {
                if let Some(o) = self.orders.get_mut(order) {
                    o.state = OrderState::Cancelled;
                }
                for t in self.tasks.values_mut().filter(|t| t.order == *order) {
                    if !matches!(t.state, TaskState::Landed { .. }) {
                        t.state = TaskState::Dropped;
                    }
                }
            }
            Event::ModelUsed {
                model,
                step,
                input_tokens,
                output_tokens,
            } => {
                let count = self
                    .usage
                    .totals
                    .entry(model.clone())
                    .or_default()
                    .entry(*step)
                    .or_default();
                count.input += u64::from(*input_tokens);
                count.output += u64::from(*output_tokens);
                count.calls = count.calls.saturating_add(1);
            }
            Event::ScopeDrafted { order, rev, draft } => {
                self.scope_drafts.insert(
                    (*order, *rev),
                    ScopeDraftOutcome::Drafted {
                        draft: draft.clone(),
                    },
                );
            }
            Event::ScopeDraftFailed { order, rev, reason } => {
                self.scope_drafts.insert(
                    (*order, *rev),
                    ScopeDraftOutcome::Failed {
                        reason: reason.clone(),
                    },
                );
            }
            Event::ScopeRevised {
                order,
                scope,
                scope_hash,
            } => match self.lanes.get_mut(order) {
                Some(lane) => {
                    lane.lineage.push(ScopeRevision {
                        rev: scope.rev,
                        scope_hash: *scope_hash,
                    });
                    lane.scope = (**scope).clone();
                    lane.scope_hash = *scope_hash;
                    lane.confirmation = None;
                }
                None => {
                    let lane = ScopeLane::opened(*order, (**scope).clone(), *scope_hash);
                    self.lanes.insert(*order, lane);
                }
            },
            Event::ScopeConfirmed {
                order,
                confirmation,
            } => {
                let ledger_head = self.ledger().head();
                if let Some(lane) = self.lanes.get_mut(order) {
                    lane.budget.get_or_insert(confirmation.budget);
                    lane.confirmation = Some((**confirmation).clone());
                    lane.confirmed_ledger_head = Some(ledger_head);
                }
            }
            Event::ScopeRebound {
                order,
                scope,
                scope_hash,
                confirmation,
                runs,
            } => {
                if let Some(lane) = self.lanes.get_mut(order) {
                    lane.lineage.push(ScopeRevision {
                        rev: scope.rev,
                        scope_hash: *scope_hash,
                    });
                    lane.scope = (**scope).clone();
                    lane.scope_hash = *scope_hash;
                    lane.confirmation = Some((**confirmation).clone());
                    for run in runs {
                        if let Some(lane_run) = lane.runs.get_mut(run) {
                            lane_run.prepared = None;
                            lane_run.receipt = None;
                        }
                    }
                }
            }
            Event::ConfirmedInputsRecorded {
                order,
                revision,
                base,
                inputs,
            } => {
                if let Some(lane) = self.lanes.get_mut(order) {
                    lane.confirmed_inputs = Some(ConfirmedInputs {
                        revision: *revision,
                        base: base.clone(),
                        inputs: (**inputs).clone(),
                    });
                }
            }
            Event::ScopedReadbackPosted { readback } => {
                bump(&mut self.ids.readback, readback.id.get());
                self.scoped_readbacks
                    .insert(readback.id, (**readback).clone());
            }
            Event::CandidatePrepared { run, prepared } => {
                if let Some(lane_run) = self.lane_of_run_mut(*run) {
                    lane_run.prepared = Some((**prepared).clone());
                }
            }
            Event::ReceiptRecorded { run, record } => self.record_receipt(*run, record),
            Event::ContestFiled { contest } => {
                if let Some(lane) = order_named(&contest.order).and_then(|o| self.lanes.get_mut(&o))
                {
                    if let Some(episode) = lane.episodes.get_mut(&contest.target) {
                        episode.contested_from = Some(episode.state);
                        episode.state = EpisodeState::Contested;
                    }
                    lane.contests
                        .insert(contest.id.clone(), (**contest).clone());
                }
            }
            Event::ContestResolved {
                order,
                contest,
                status,
                ..
            } => {
                if let Some(lane) = self.lanes.get_mut(order)
                    && let Some(filed) = lane.contests.get_mut(contest)
                {
                    filed.status = *status;
                    if let Some(episode) = lane.episodes.get_mut(&filed.target) {
                        let before = episode.contested_from.take();
                        episode.state = match status {
                            ContestStatus::Upheld => EpisodeState::Residue,
                            ContestStatus::Rejected | ContestStatus::Open => {
                                before.unwrap_or(episode.state)
                            }
                        };
                    }
                }
            }
            Event::RulingRecorded { event } => self.ledger.append((**event).clone()),
        }
        self.next_seq = stamped.seq.next();
    }

    fn record_receipt(&mut self, run: GateRunId, record: &repair::ReceiptRecord) {
        let task = self.gates.get(&run).map(|g| g.task);
        let order = task.and_then(|t| self.tasks.get(&t)).map(|t| t.order);
        let Some(lane) = order.and_then(|o| self.lanes.get_mut(&o)) else {
            return;
        };
        for episode in &record.episodes {
            lane.episodes.insert(episode.id, episode.clone());
        }
        if let Some(answer) = &record.answered
            && let Some(round) = lane.rounds.last_mut()
            && round.key.round == answer.round
        {
            round.result = Some(answer.result.clone());
        }
        if let Some(opened) = &record.opened {
            lane.rounds.push(opened.clone());
        }
        if matches!(record.verdict, LaneVerdict::Stopped { .. }) {
            lane.stopped = true;
        }
        if let Some(lane_run) = lane.runs.get_mut(&run) {
            lane_run.receipt = Some(record.clone());
        }
    }

    /// Replaces a claimed task's stage with `f(stage)`. Does nothing for an unclaimed task.
    fn restage(&mut self, task: TaskId, f: impl FnOnce(Stage) -> Stage) {
        if let Some(TaskState::Claimed { stage, .. }) =
            self.tasks.get_mut(&task).map(|t| &mut t.state)
        {
            let old = mem::replace(stage, Stage::Declaring);
            *stage = f(old);
        }
    }

    /// Replaces the barrier of `task`'s order with `f(barrier)`.
    fn rebar(&mut self, task: TaskId, f: impl FnOnce(Barrier) -> Barrier) {
        let order = self.tasks.get(&task).map(|t| t.order);
        if let Some(o) = order.and_then(|id| self.orders.get_mut(&id)) {
            o.barrier = f(o.barrier);
        }
    }

    fn close_gate(&mut self, run: GateRunId, verdict: &GateVerdict, seq: Seq) {
        let Some(task) = self.gates.get(&run).map(|g| g.task) else {
            return;
        };
        let Some(t) = self.tasks.get_mut(&task) else {
            return;
        };
        t.state = match mem::replace(&mut t.state, TaskState::Open) {
            TaskState::Claimed {
                claim,
                stage:
                    Stage::Gating {
                        decl,
                        run: gating,
                        head,
                    },
            } if gating == run => match verdict {
                GateVerdict::Passed => TaskState::Queued {
                    head,
                    run,
                    queued_at: seq,
                },
                // Back through `build_clear`, the one authority on build permission.
                GateVerdict::Failed { .. } => TaskState::Claimed {
                    claim,
                    stage: Stage::Declared { decl },
                },
                GateVerdict::Running => TaskState::Claimed {
                    claim,
                    stage: Stage::Gating { decl, run, head },
                },
            },
            other => other,
        };
    }
}

/// Refuses a stored human-only event whose actor is not the member it names.
fn check_signer(stamped: &Stamped) -> Result<(), String> {
    let signer = match &stamped.event {
        Event::RulingRecorded { event } => Some(event.actor().as_str()),
        Event::ScopeConfirmed { confirmation, .. } => Some(confirmation.by.as_str()),
        Event::ContestResolved { by, .. } => Some(by.as_str()),
        Event::ScopeRevised { .. } => None,
        _ => return Ok(()),
    };
    match (&stamped.actor, signer) {
        (Actor::Member(member), Some(signer)) if member.as_str() == signer => Ok(()),
        (Actor::Member(_), None) => Ok(()),
        _ => Err("only the member who signs it records this event".to_owned()),
    }
}

fn order_named(order: &OrderRef) -> Option<OrderId> {
    order.as_str().parse().ok()
}

fn lane_named<'w>(
    lanes: &'w BTreeMap<OrderId, ScopeLane>,
    order: &OrderRef,
) -> Result<&'w ScopeLane, String> {
    order_named(order)
        .and_then(|id| lanes.get(&id))
        .ok_or_else(|| format!("order {order} has no scope"))
}

fn bump(counter: &mut u64, seen: u32) {
    *counter = (*counter).max(u64::from(seen) + 1);
}

fn mint(next: u64, exhausted: &'static str) -> Result<u32, Rejection> {
    u32::try_from(next).map_err(|_| Rejection::invalid(exhausted))
}

/// `n` consecutive ids from `next`, or a rejection if any of them does not fit.
fn mint_many<T>(
    next: u64,
    n: usize,
    exhausted: &'static str,
    id: fn(u32) -> T,
) -> Result<Vec<T>, Rejection> {
    (next..)
        .take(n)
        .map(|raw| mint(raw, exhausted).map(id))
        .collect()
}

fn known<K: Ord + fmt::Display, V>(map: &BTreeMap<K, V>, id: &K, what: &str) -> Result<(), String> {
    if map.contains_key(id) {
        Ok(())
    } else {
        Err(format!("unknown {what} {id}"))
    }
}

fn fresh<K: Ord + fmt::Display, V>(map: &BTreeMap<K, V>, id: &K, what: &str) -> Result<(), String> {
    if map.contains_key(id) {
        Err(format!("{what} {id} already exists"))
    } else {
        Ok(())
    }
}

/// The single decider for a world. Only the origin appends events.
///
/// A solo desktop runs it in-process; a team runs it in the shared `acan` server. The host
/// holds it behind one lock, persists the returned events before acknowledging, then
/// broadcasts them.
#[derive(Debug, Clone)]
pub struct Origin {
    world: World,
}

impl Origin {
    /// Starts a new world. Returns the `WorldOpened` event (seq 0, founder as owner) to persist.
    #[must_use]
    pub fn open(world: WorldRef, founder: MemberId, now: Timestamp) -> (Self, Stamped) {
        let opened = Stamped {
            seq: Seq::ZERO,
            at: now,
            actor: Actor::Member(founder.clone()),
            event: Event::WorldOpened {
                world: world.clone(),
                founder,
            },
        };
        let mut state = World::opened(world);
        state.apply(&opened);
        (Self { world: state }, opened)
    }

    /// Rebuilds the origin from its stored log.
    ///
    /// # Errors
    /// Returns [`ReplayError`] if the log does not start with `WorldOpened`, has a gap, or
    /// holds an event that refers to an entity the log never created.
    pub fn replay(log: impl IntoIterator<Item = Stamped>) -> Result<Self, ReplayError> {
        Ok(Self {
            world: World::from_log(log)?,
        })
    }

    /// Decides `cmd`, applies the resulting events, and returns them stamped.
    ///
    /// Order of work: expire due leases, lift declare barriers past their deadline, decide the
    /// command, then clear any declared task whose blockers are gone. All of it is one batch: either the command is accepted and
    /// every event is returned, or it is rejected and nothing changes (not even expiry).
    /// An already-satisfied command returns an empty batch.
    ///
    /// # Errors
    /// Returns [`Rejection`] when the command is not allowed in the current state.
    pub fn execute(&mut self, ctx: &Ctx<'_>, cmd: Command) -> Result<Vec<Stamped>, Rejection> {
        let mut next = self.world.clone();
        let mut batch = Vec::new();
        let expired = lease::expire_due(&next, ctx);
        next.record(ctx.now, &Actor::System, expired, &mut batch);
        let lifted = clash::expire_barriers(&next, ctx);
        next.record(ctx.now, &Actor::System, lifted, &mut batch);
        let (actor, decided) = decide(&next, ctx, cmd)?;
        next.record(ctx.now, &actor, decided, &mut batch);
        let orphaned = clash::dissolve_orphans(&next);
        next.record(ctx.now, &Actor::System, orphaned, &mut batch);
        let cleared = clash::build_clear(&next);
        next.record(ctx.now, &Actor::System, cleared, &mut batch);
        self.world = next;
        Ok(batch)
    }

    /// The current state.
    #[must_use]
    pub fn world(&self) -> &World {
        &self.world
    }
}

/// A read-only follower of a world log: the browser, a joined desktop, the hosted replay.
///
/// It has no `execute`; a joined desktop sends commands to the origin over HTTP.
#[derive(Debug, Clone)]
pub struct Replica {
    world: World,
}

impl Replica {
    /// Folds a log fetched from the origin. The first event must be `WorldOpened` at seq 0.
    ///
    /// # Errors
    /// Returns [`ReplayError`] if the log does not start with `WorldOpened`, has a gap, or
    /// holds an event that refers to an entity the log never created.
    pub fn from_log(log: impl IntoIterator<Item = Stamped>) -> Result<Self, ReplayError> {
        Ok(Self {
            world: World::from_log(log)?,
        })
    }

    /// Applies one streamed event. An event already applied (seq below `next_seq`) is ignored.
    ///
    /// # Errors
    /// Returns [`ReplayError::SeqGap`] if events were missed; refetch from `next_seq`.
    /// Returns [`ReplayError::InvalidEvent`], applying nothing, if the event refers to an
    /// entity the log never created.
    pub fn ingest(&mut self, stamped: &Stamped) -> Result<(), ReplayError> {
        let expected = self.world.next_seq;
        if stamped.seq < expected {
            return Ok(());
        }
        if stamped.seq > expected {
            return Err(ReplayError::SeqGap {
                expected,
                got: stamped.seq,
            });
        }
        self.world.check(stamped)?;
        self.world.apply(stamped);
        Ok(())
    }

    /// The seq to resume the stream from.
    #[must_use]
    pub fn next_seq(&self) -> Seq {
        self.world.next_seq
    }

    /// The current state.
    #[must_use]
    pub fn world(&self) -> &World {
        &self.world
    }
}

fn decide(world: &World, ctx: &Ctx<'_>, cmd: Command) -> Result<(Actor, Vec<Event>), Rejection> {
    match cmd {
        Command::Human { member, cmd } => {
            if world.member(&member).is_none() {
                return Err(Rejection::forbidden("not a member of this world"));
            }
            let events = decide_human(world, &member, cmd)?;
            Ok((Actor::Member(member), events))
        }
        Command::Agent { agent, cmd } => {
            if world.agent(&agent).is_none() {
                return Err(Rejection::forbidden("agent is not registered"));
            }
            let events = decide_agent(world, ctx, &agent, cmd)?;
            Ok((Actor::Agent(agent), events))
        }
        Command::System(cmd) => Ok((Actor::System, decide_system(world, ctx, cmd)?)),
    }
}

fn decide_human(world: &World, member: &MemberId, cmd: HumanCmd) -> Result<Vec<Event>, Rejection> {
    match cmd {
        HumanCmd::AddMember {
            member: added,
            role,
        } => member::decide_add_member(world, member, added, role),
        HumanCmd::RegisterAgent { agent } => member::decide_register_agent(world, member, agent),
        HumanCmd::PostOrder {
            machine,
            sentence,
            witnesses,
        } => order::decide_post_order(world, member, machine, sentence, witnesses),
        HumanCmd::AnswerQuestion { question, option } => {
            order::decide_answer(world, member, question, option)
        }
        HumanCmd::SettleClash { clash, choice } => {
            clash::decide_settle(world, member, clash, choice)
        }
        HumanCmd::CancelOrder { order } => order::decide_cancel(world, member, order),
        HumanCmd::ReviseScope { order, scope } => lane::decide_revise(world, member, order, *scope),
        HumanCmd::ConfirmScope {
            order,
            confirmation,
        } => lane::decide_confirm(world, member, order, *confirmation),
        HumanCmd::ResolveContest { contest, status } => {
            repair::decide_resolve_contest(world, member, &contest, status)
        }
        HumanCmd::RecordRuling { event } => ruling::decide_record(world, member, *event),
    }
}

fn decide_agent(
    world: &World,
    ctx: &Ctx<'_>,
    agent: &AgentId,
    cmd: AgentCmd,
) -> Result<Vec<Event>, Rejection> {
    match cmd {
        AgentCmd::PostReadback {
            task,
            summary,
            touches,
        } => order::decide_readback(world, agent, task, summary, touches),
        AgentCmd::Claim { task } => lease::decide_claim(world, ctx, agent, task),
        AgentCmd::Renew { fence } => lease::decide_renew(world, ctx, agent, fence),
        AgentCmd::Release { fence } => lease::decide_release(world, ctx, agent, fence),
        AgentCmd::Declare { fence, footprint } => {
            clash::decide_declare(world, ctx, agent, fence, footprint)
        }
        AgentCmd::ReportBuilt { fence, head } => {
            gate::decide_report_built(world, ctx, agent, fence, head)
        }
        AgentCmd::PostScopedReadback {
            task,
            scope_rev,
            scope_hash,
            summary,
            predictions,
        } => lane::decide_readback(
            world,
            agent,
            task,
            scope_rev,
            scope_hash,
            summary,
            predictions,
        ),
        AgentCmd::FileContest {
            fence,
            episode,
            kind,
            reproducer,
            rationale,
        } => repair::decide_file_contest(
            world,
            ctx,
            agent,
            fence,
            repair::ContestDraft {
                episode,
                kind,
                reproducer,
                rationale,
            },
        ),
    }
}

fn decide_system(world: &World, ctx: &Ctx<'_>, cmd: SystemCmd) -> Result<Vec<Event>, Rejection> {
    match cmd {
        SystemCmd::IndexUpdated => clash::decide_index_updated(world, ctx),
        SystemCmd::SplitReady { order, rev, tasks } => {
            order::decide_split_ready(world, order, rev, tasks)
        }
        SystemCmd::ReadbacksReviewed {
            order,
            rev,
            verdicts,
            question,
        } => order::decide_reviewed(world, order, rev, verdicts, question),
        SystemCmd::ClashExplained { clash, explanation } => {
            clash::decide_explained(world, clash, explanation)
        }
        SystemCmd::WitnessResult {
            run,
            witness,
            outcome,
        } => gate::decide_witness_result(world, ctx, run, witness, outcome),
        SystemCmd::LandResult { task, head, result } => {
            gate::decide_land_result(world, task, &head, result)
        }
        SystemCmd::CandidateConflicted { run, files } => gate::decide_conflicted(world, run, files),
        SystemCmd::RefuseRun { run, reason } => gate::decide_refused(world, run, reason),
        SystemCmd::ModelUsed {
            model,
            step,
            input_tokens,
            output_tokens,
        } => Ok(vec![Event::ModelUsed {
            model,
            step,
            input_tokens,
            output_tokens,
        }]),
        SystemCmd::Tick => Ok(Vec::new()),
        SystemCmd::CandidatePrepared { run, prepared } => {
            lane::decide_prepared(world, run, *prepared)
        }
        SystemCmd::ReceiptReady { run, receipt } => {
            repair::decide_receipt(world, ctx, run, *receipt)
        }
        SystemCmd::RebindBase {
            order,
            base,
            context,
        } => lane::decide_rebind(world, order, &base, &context),
        SystemCmd::RecordConfirmedInputs {
            order,
            revision,
            base,
            inputs,
        } => lane::decide_record_inputs(world, order, revision, &base, *inputs),
        SystemCmd::ScopeDrafted { order, rev, draft } => {
            lane::decide_drafted(world, order, rev, *draft)
        }
        SystemCmd::ScopeDraftFailed { order, rev, reason } => {
            lane::decide_draft_failed(world, order, rev, reason)
        }
    }
}
