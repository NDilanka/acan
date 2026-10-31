//! The scope-first lane: one confirmed scope per order, deterministic readback
//! acknowledgements, and the claims that rest on them.
//!
//! ```text
//! (legacy) --ReviseScope: every task open--> unconfirmed            [ScopeRevised]
//! unconfirmed --ConfirmScope: bound to revision, hash, base commit, ruling versions,
//!     selector and example hashes, one answer per open choice and one
//!     acknowledgement per broad item--> confirmed                    [ScopeConfirmed]
//! confirmed --ReviseScope: any newer revision--> unconfirmed         [ScopeRevised, LeaseEnded Superseded]
//! confirmed --RecordConfirmedInputs: the confirmed revision at its base--> recorded
//!                                                                    [ConfirmedInputsRecorded]
//! PostScopedReadback at the confirmed revision and hash --> acknowledged [ScopedReadbackPosted]
//! Claim --> needs the agent's acknowledged readback at the confirmed revision and hash
//! ReportBuilt --> a run with no witnesses that waits for its receipt  [BuildReported]
//! CandidatePrepared: same base, scope and ruling head --> prepared    [CandidatePrepared]
//! RebindBase: protected head moved, every protected input the same as a candidate prepared on
//!     the confirmed base, or else as the confirmation's record --> re-bound, live prepared runs
//!     cleared, so each is prepared and receipted again; any other move is refused  [ScopeRebound]
//! ```
//!
//! The legacy model review never authorizes anything here. The lane reads only its own
//! acknowledgements, the `Review` job is never derived for its orders, and a stored
//! `ReadbacksReviewed` verdict on one of its tasks still replays but is never consulted.

use std::collections::BTreeMap;

use acan_intent::{
    BroadGrantAck, CanonicalError, ChoiceAnswer, CommitSha, Contest, ContestId, ContestStatus,
    EnvHash, EpisodeId, EvaluatorVersion, ExampleHash, FixtureSetHash, GateContext, GateState,
    HumanRef, InvalidValue, ItemId, LedgerHead, McpV1, OrderRef, OverlayHash, PostReadbackResponse,
    Prediction, PreparedCommit, ProbeSetHash, RepairBudget, RepairRound, RepairRounds, RunnerHash,
    RuntimeId, SandboxPolicyHash, Scope, ScopeConfirmation, ScopeConfirmationV1, ScopeDraft,
    ScopeHash, ScopeRev, ScopeRevision, SelectorHash, UnixMillis,
};
use serde::{Deserialize, Serialize};

use crate::event::{Event, Rejection};
use crate::ids::{
    AgentId, CommitId, GateRunId, MemberId, OrderId, OrderRev, ReadbackId, TaskId, Timestamp,
};
use crate::lease::LeaseEnd;
use crate::member::Role;
use crate::order::{self, Order, OrderPhase, OrderState, TaskState};
use crate::repair::{Episode, LaneVerdict, ReceiptRecord};
use crate::world::World;

/// Most predictions one readback may carry.
pub const MAX_PREDICTIONS: usize = 256;

/// An agent's readback of a scope-first task: the scope revision it read and what it predicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedReadback {
    /// The readback.
    pub id: ReadbackId,
    /// The task.
    pub task: TaskId,
    /// The agent that posted it.
    pub agent: AgentId,
    /// The scope revision the agent read.
    pub scope_rev: ScopeRev,
    /// The scope hash the agent read.
    pub scope_hash: ScopeHash,
    /// The plan in the agent's words.
    pub summary: String,
    /// What the agent predicts its change does. Agent proposals: the core scores them against
    /// the receipt, and they are never evidence and never a pass.
    pub predictions: Vec<Prediction>,
}

impl ScopedReadback {
    /// The deterministic acknowledgement the agent gets back. It authorizes nothing but the
    /// claim the confirmed scope already allows.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] if the task id has no contract spelling, which a core id never
    /// lacks.
    pub fn acknowledgement(&self) -> Result<PostReadbackResponse, InvalidValue> {
        Ok(PostReadbackResponse {
            version: McpV1,
            task: self.task.to_string().parse()?,
            scope_rev: self.scope_rev,
            scope_hash: self.scope_hash,
            predictions_recorded: u32::try_from(self.predictions.len()).unwrap_or(u32::MAX),
        })
    }
}

/// What a confirmation of one scope revision must bind, computed from the scope alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeBinding {
    /// The scope's hash.
    pub scope_hash: ScopeHash,
    /// Every selector, may-change items first, then must-keep rules, in scope order.
    pub selector_hashes: Vec<SelectorHash>,
    /// Every may-change item's example, in scope order.
    pub example_hashes: Vec<ExampleHash>,
    /// The broad may-change items, each needing its own acknowledgement.
    pub broad_items: Vec<(ItemId, SelectorHash)>,
}

impl ScopeBinding {
    /// The binding of `scope`.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if a part of the scope cannot be hashed.
    pub fn of(scope: &Scope) -> Result<Self, CanonicalError> {
        let may = &scope.body.may;
        let mut selector_hashes = Vec::with_capacity(may.len() + scope.body.must_keep.len());
        let mut broad_items = Vec::new();
        for item in may {
            let hash = item.selector.selector_hash()?;
            if item.selector.is_broad() {
                broad_items.push((item.id.clone(), hash));
            }
            selector_hashes.push(hash);
        }
        for keep in &scope.body.must_keep {
            selector_hashes.push(keep.selector.selector_hash()?);
        }
        let example_hashes = may
            .iter()
            .map(|item| item.example.example_hash())
            .collect::<Result<_, _>>()?;
        Ok(Self {
            scope_hash: scope.scope_hash()?,
            selector_hashes,
            example_hashes,
            broad_items,
        })
    }
}

/// The protected inputs a scope was confirmed under: the gate-context inputs that no candidate
/// decides. The protected head, the merged commit, the scope hash and the candidate's artifacts
/// are left out, because a re-bind changes them or derives them from the candidate. The overlay is
/// kept: it hashes the protected files of the base, which no candidate decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedInputs {
    /// The protected files of the base, as the overlay hash names them.
    pub overlay_hash: OverlayHash,
    /// The ruling log revision in force.
    pub ledger_head: LedgerHead,
    /// The effective probes with their normalization plans.
    pub probe_set_hash: ProbeSetHash,
    /// The fixtures.
    pub fixture_set_hash: FixtureSetHash,
    /// The runner build and the app contract.
    pub runner_hash: RunnerHash,
    /// The evaluator that classifies the runs.
    pub evaluator_version: EvaluatorVersion,
    /// The app runtime.
    pub runtime: RuntimeId,
    /// The controlled environment.
    pub controlled_env_hash: EnvHash,
    /// The container policy the runs use.
    pub sandbox_policy_hash: SandboxPolicyHash,
}

impl ProtectedInputs {
    /// The protected inputs of `context`. Every context field is either taken or named as left
    /// out, so a new field must be classified here to compile.
    #[must_use]
    pub fn of(context: &GateContext) -> Self {
        let GateContext {
            protected_head: _,
            merged_commit: _,
            overlay_hash,
            effective_artifact_hashes: _,
            scope_hash: _,
            ledger_head,
            probe_set_hash,
            fixture_set_hash,
            runner_hash,
            evaluator_version,
            runtime,
            controlled_env_hash,
            sandbox_policy_hash,
        } = context;
        Self {
            overlay_hash: *overlay_hash,
            ledger_head: *ledger_head,
            probe_set_hash: *probe_set_hash,
            fixture_set_hash: *fixture_set_hash,
            runner_hash: *runner_hash,
            evaluator_version: evaluator_version.clone(),
            runtime: runtime.clone(),
            controlled_env_hash: *controlled_env_hash,
            sandbox_policy_hash: *sandbox_policy_hash,
        }
    }
}

/// The protected inputs recorded when a human confirmed one scope revision. A re-bind with no
/// candidate prepared on the confirmed base checks the moved head against this record. A re-bind
/// keeps the record: it only moves the confirmation's base after the inputs match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedInputs {
    /// The scope revision the confirmation covers.
    pub revision: ScopeRev,
    /// The base the confirmation named when the inputs were recorded.
    pub base: CommitSha,
    /// The protected inputs at that base.
    pub inputs: ProtectedInputs,
}

/// One gate run of a scope-first task. Its verdict comes only from its receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRun {
    /// The task under test.
    pub task: TaskId,
    /// The commit the worker reported.
    pub head: CommitId,
    /// The prepared merged commit and its tested context, once prepared.
    pub prepared: Option<PreparedCommit>,
    /// The recorded receipt, once the run is classified.
    pub receipt: Option<ReceiptRecord>,
}

/// The scope-first state of one order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeLane {
    /// The order.
    pub order: OrderId,
    /// The current scope revision.
    pub scope: Scope,
    /// Its hash.
    pub scope_hash: ScopeHash,
    /// Every revision the order went through, oldest first.
    pub lineage: Vec<ScopeRevision>,
    /// The human confirmation of the current revision. A newer revision clears it.
    pub confirmation: Option<ScopeConfirmation>,
    /// The protected inputs the origin recorded when the current revision was confirmed, if any.
    pub confirmed_inputs: Option<ConfirmedInputs>,
    /// The ruling log head when the current revision was confirmed. The inputs recorded for the
    /// revision must name this head, so a ruling recorded after the confirmation is never taken
    /// as part of it.
    pub confirmed_ledger_head: Option<LedgerHead>,
    /// The repair budget, fixed by the first confirmation and never replenished.
    pub budget: Option<RepairBudget>,
    /// Every gate run of the order's tasks.
    pub runs: BTreeMap<GateRunId, LaneRun>,
    /// Every discrepancy episode, by identity.
    pub episodes: BTreeMap<EpisodeId, Episode>,
    /// The order-wide repair rounds, oldest first.
    pub rounds: Vec<RepairRound>,
    /// Every contest filed in the order.
    pub contests: BTreeMap<ContestId, Contest>,
    /// Whether the repair budget ran out. A stopped lane takes no more claims or builds.
    pub stopped: bool,
}

impl ScopeLane {
    pub(crate) fn opened(order: OrderId, scope: Scope, scope_hash: ScopeHash) -> Self {
        Self {
            order,
            lineage: vec![ScopeRevision {
                rev: scope.rev,
                scope_hash,
            }],
            scope,
            scope_hash,
            confirmation: None,
            confirmed_inputs: None,
            confirmed_ledger_head: None,
            budget: None,
            runs: BTreeMap::new(),
            episodes: BTreeMap::new(),
            rounds: Vec::new(),
            contests: BTreeMap::new(),
            stopped: false,
        }
    }

    /// The repair rounds opened so far.
    #[must_use]
    pub fn rounds_used(&self) -> usize {
        self.rounds.len()
    }

    /// Contests still waiting for the human.
    pub fn open_contests(&self) -> impl Iterator<Item = &Contest> {
        self.contests
            .values()
            .filter(|contest| contest.status == ContestStatus::Open)
    }

    /// The receipt of the latest classified run.
    #[must_use]
    pub fn latest_receipt(&self) -> Option<&ReceiptRecord> {
        self.runs
            .values()
            .rev()
            .find_map(|run| run.receipt.as_ref())
    }

    /// Where the gate stands, for the order plate: stopped once the budget ran out, blocked
    /// when the latest receipt did not pass, running while a prepared run waits for its
    /// receipt. `None` when the lane has nothing to draw: no run yet, or the latest one passed
    /// and its task's state tells the rest.
    #[must_use]
    pub fn gate_state(&self) -> Option<GateState> {
        if self.stopped {
            return Some(GateState::Stopped);
        }
        let latest = self.runs.values().next_back()?;
        match &latest.receipt {
            Some(record) => match record.verdict {
                LaneVerdict::Passed => None,
                LaneVerdict::Blocked { .. } => Some(GateState::Blocked),
                LaneVerdict::Stopped { .. } => Some(GateState::Stopped),
            },
            None => latest.prepared.as_ref().map(|_| GateState::Running),
        }
    }
}

/// What the scope-draft job reported for one order revision. A draft is a proposal: it does not
/// open the lane, confirm anything, or authorize a claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ScopeDraftOutcome {
    /// The foreman's draft. A human adopts it by sending `ReviseScope` and `ConfirmScope`.
    Drafted {
        /// The draft, with the model that proposed it.
        draft: Box<ScopeDraft>,
    },
    /// The last attempt failed. Nothing was drafted and nothing falls back to another draft.
    Failed {
        /// Why the draft failed.
        reason: String,
    },
}

/// `ScopeDrafted`: stores a foreman's draft for an order revision that still has no scope.
pub(crate) fn decide_drafted(
    world: &World,
    order: OrderId,
    rev: OrderRev,
    draft: ScopeDraft,
) -> Result<Vec<Event>, Rejection> {
    check_draft_target(world, order, rev)?;
    if draft.order != order_ref(order)? {
        return Err(Rejection::invalid("the draft names another order"));
    }
    Ok(vec![Event::ScopeDrafted {
        order,
        rev,
        draft: Box::new(draft),
    }])
}

/// `ScopeDraftFailed`: records that the draft for an order revision failed after its last
/// attempt.
pub(crate) fn decide_draft_failed(
    world: &World,
    order: OrderId,
    rev: OrderRev,
    reason: String,
) -> Result<Vec<Event>, Rejection> {
    check_draft_target(world, order, rev)?;
    Ok(vec![Event::ScopeDraftFailed { order, rev, reason }])
}

/// A draft result is accepted only for an open order at its current revision, with no scope and
/// no drafted scope yet at that revision.
fn check_draft_target(world: &World, order: OrderId, rev: OrderRev) -> Result<(), Rejection> {
    let o = world
        .order(order)
        .ok_or_else(|| Rejection::unknown("order", order))?;
    if o.state == OrderState::Cancelled {
        return Err(Rejection::wrong_state("order", "cancelled"));
    }
    check_open(world, order)?;
    if world.lane(order).is_some() {
        return Err(Rejection::wrong_state("order", "with a scope"));
    }
    if rev != o.rev {
        return Err(Rejection::StaleRevision { current: o.rev });
    }
    if matches!(
        world.scope_draft_at(order, rev),
        Some(ScopeDraftOutcome::Drafted { .. })
    ) {
        return Err(Rejection::wrong_state("order", "drafted at this revision"));
    }
    Ok(())
}

pub(crate) fn order_ref(order: OrderId) -> Result<OrderRef, Rejection> {
    order
        .to_string()
        .parse()
        .map_err(|_| Rejection::invalid("the order id has no contract spelling"))
}

pub(crate) fn hash_failed(_: CanonicalError) -> Rejection {
    Rejection::invalid("a value has no canonical JSON form")
}

/// Whether `member` may decide for `order`: its poster or an owner.
pub(crate) fn may_decide(world: &World, member: &MemberId, order: &Order) -> bool {
    &order.posted_by == member || world.member(member).is_some_and(|m| m.role == Role::Owner)
}

fn check_open(world: &World, order: OrderId) -> Result<(), Rejection> {
    match world.phase(order) {
        None => Err(Rejection::unknown("order", order)),
        Some(OrderPhase::Cancelled) => Err(Rejection::wrong_state("order", "cancelled")),
        Some(OrderPhase::Landed) => Err(Rejection::wrong_state("order", "landed")),
        Some(_) => Ok(()),
    }
}

fn stale(lane: &ScopeLane) -> Rejection {
    Rejection::StaleScope {
        current_rev: lane.scope.rev,
        current_hash: lane.scope_hash,
    }
}

/// The lane's confirmation, or why work may not start.
pub(crate) fn confirmed(lane: &ScopeLane) -> Result<&ScopeConfirmation, Rejection> {
    if lane.stopped {
        return Err(Rejection::BudgetExhausted { order: lane.order });
    }
    lane.confirmation
        .as_ref()
        .ok_or(Rejection::ScopeNotConfirmed { order: lane.order })
}

/// `ReviseScope`: the first revision moves an order with no claimed work into the lane; a
/// newer one clears the confirmation and ends every live lease as `Superseded`.
pub(crate) fn decide_revise(
    world: &World,
    member: &MemberId,
    order: OrderId,
    scope: Scope,
) -> Result<Vec<Event>, Rejection> {
    let o = world
        .order(order)
        .ok_or_else(|| Rejection::unknown("order", order))?;
    if !may_decide(world, member, o) {
        return Err(Rejection::forbidden(
            "only the poster or an owner sets an order's scope",
        ));
    }
    check_open(world, order)?;
    if scope.order != order_ref(order)? {
        return Err(Rejection::invalid("the scope names another order"));
    }
    let scope_hash = scope.scope_hash().map_err(hash_failed)?;
    let lane = world.lane(order);
    match lane {
        None if world
            .tasks_of(order)
            .any(|t| !matches!(t.state, TaskState::Open)) =>
        {
            return Err(Rejection::wrong_state("order", "already claimed"));
        }
        None => {}
        Some(lane) if lane.scope_hash == scope_hash => return Ok(Vec::new()),
        Some(lane) if scope.rev <= lane.scope.rev => return Err(stale(lane)),
        Some(_) => {}
    }
    let mut events = vec![Event::ScopeRevised {
        order,
        scope: Box::new(scope),
        scope_hash,
    }];
    if lane.is_some() {
        events.extend(order::end_leases(world, order, LeaseEnd::Superseded));
    }
    Ok(events)
}

/// `ConfirmScope`: a human binds the current revision. Re-sending the same confirmation is a
/// no-op; a different one for a confirmed revision is refused.
pub(crate) fn decide_confirm(
    world: &World,
    member: &MemberId,
    order: OrderId,
    confirmation: ScopeConfirmation,
) -> Result<Vec<Event>, Rejection> {
    let o = world
        .order(order)
        .ok_or_else(|| Rejection::unknown("order", order))?;
    if !may_decide(world, member, o) {
        return Err(Rejection::forbidden(
            "only the poster or an owner confirms an order's scope",
        ));
    }
    check_open(world, order)?;
    let lane = world
        .lane(order)
        .ok_or_else(|| Rejection::wrong_state("order", "without a scope"))?;
    if confirmation.by.as_str() != member.as_str() {
        return Err(Rejection::forbidden(
            "a confirmation is signed by the member who sends it",
        ));
    }
    if let Some(current) = &lane.confirmation {
        return if *current == confirmation {
            Ok(Vec::new())
        } else {
            Err(Rejection::wrong_state("scope", "confirmed"))
        };
    }
    check_binding(world, lane, &confirmation)?;
    Ok(vec![Event::ScopeConfirmed {
        order,
        confirmation: Box::new(confirmation),
    }])
}

/// The body a human sends as `ConfirmScope` for the lane's current revision. Every field the
/// origin checks is filled in from the lane, the ledger and the scope: the revision and hash, the
/// base commit, the ruling versions in force, the selector and example hashes, and the repair
/// budget the order already fixed or else the offered one. The body confirms nothing. The
/// answers to open choices and the broad-item acknowledgements are the human's own decisions, so
/// they come back empty, and the origin refuses the body until they are added.
///
/// # Errors
///
/// Returns a [`Rejection`] if the order has no scope, if `by` is not a human reference, or if a
/// part of the scope cannot be hashed.
pub fn confirmation_body(
    world: &World,
    order: OrderId,
    by: &MemberId,
    at: Timestamp,
) -> Result<ScopeConfirmation, Rejection> {
    let lane = world
        .lane(order)
        .ok_or_else(|| Rejection::wrong_state("order", "without a scope"))?;
    let binding = ScopeBinding::of(&lane.scope).map_err(hash_failed)?;
    let mut ruling_versions = world.ledger().ruling_versions();
    ruling_versions.sort();
    let by = HumanRef::try_from(by.as_str().to_owned())
        .map_err(|_| Rejection::invalid("the member id is not a human reference"))?;
    Ok(ScopeConfirmation {
        version: ScopeConfirmationV1,
        order: order_ref(order)?,
        scope_rev: lane.scope.rev,
        scope_hash: lane.scope_hash,
        semantic_scope_hash: Some(lane.scope.semantic_scope_hash().map_err(hash_failed)?),
        base_commit: lane.scope.base_commit.clone(),
        ruling_versions,
        selector_hashes: binding.selector_hashes,
        example_hashes: binding.example_hashes,
        answers: Vec::new(),
        broad_grants: Vec::new(),
        budget: lane.budget.unwrap_or_else(offered_budget),
        by,
        at: UnixMillis(at.as_millis()),
    })
}

/// The repair rounds a body offers before the order's first confirmation fixes its own. This is
/// the default, not the v1 cap, which is [`RepairRounds::MAX`].
const OFFERED_REPAIR_ROUNDS: u32 = 2;

/// The repair budget a body offers before the order's first confirmation fixes one: two repair
/// rounds, ten minutes of wall time, a two-dollar model cost cap, and four new episodes.
fn offered_budget() -> RepairBudget {
    RepairBudget {
        repair_rounds: RepairRounds::try_from(OFFERED_REPAIR_ROUNDS)
            .expect("the offered repair round count is in range"),
        wall_time_ms: 600_000,
        model_cost_micros: 2_000_000,
        max_episodes: 4,
    }
}

fn mismatch(what: &str, lane: &ScopeLane) -> Rejection {
    Rejection::Invalid {
        why: format!(
            "the confirmation's {what} do not match scope revision {}",
            lane.scope.rev.0
        ),
    }
}

fn check_binding(
    world: &World,
    lane: &ScopeLane,
    confirmation: &ScopeConfirmation,
) -> Result<(), Rejection> {
    if confirmation.order != order_ref(lane.order)? {
        return Err(Rejection::invalid("the confirmation names another order"));
    }
    if confirmation.scope_rev != lane.scope.rev || confirmation.scope_hash != lane.scope_hash {
        return Err(stale(lane));
    }
    if confirmation.base_commit != lane.scope.base_commit {
        return Err(mismatch("base commit", lane));
    }
    let mut versions = confirmation.ruling_versions.clone();
    versions.sort();
    if versions != world.ledger().ruling_versions() {
        return Err(mismatch("ruling versions", lane));
    }
    let binding = ScopeBinding::of(&lane.scope).map_err(hash_failed)?;
    if confirmation.selector_hashes != binding.selector_hashes {
        return Err(mismatch("selector hashes", lane));
    }
    if confirmation.example_hashes != binding.example_hashes {
        return Err(mismatch("example hashes", lane));
    }
    check_answers(&lane.scope, &confirmation.answers)?;
    check_broad_grants(world, &binding, &confirmation.broad_grants)?;
    if lane
        .budget
        .is_some_and(|budget| budget != confirmation.budget)
    {
        return Err(Rejection::invalid(
            "the repair budget is fixed by the order's first confirmation",
        ));
    }
    Ok(())
}

fn check_answers(scope: &Scope, answers: &[ChoiceAnswer]) -> Result<(), Rejection> {
    let open = &scope.body.open;
    if answers.len() != open.len() {
        return Err(Rejection::invalid(
            "a confirmation answers every open choice once",
        ));
    }
    for choice in open {
        let mut given = answers.iter().filter(|a| a.choice_id == choice.id);
        let (Some(answer), None) = (given.next(), given.next()) else {
            return Err(Rejection::Invalid {
                why: format!("open choice {} needs exactly one answer", choice.id),
            });
        };
        if !choice
            .options
            .iter()
            .any(|option| option.key == answer.option)
        {
            return Err(Rejection::Invalid {
                why: format!("open choice {} has no option {}", choice.id, answer.option),
            });
        }
    }
    Ok(())
}

fn check_broad_grants(
    world: &World,
    binding: &ScopeBinding,
    grants: &[BroadGrantAck],
) -> Result<(), Rejection> {
    let mut acked: BTreeMap<&ItemId, &BroadGrantAck> = BTreeMap::new();
    for ack in grants {
        let member: Option<MemberId> = ack.by.as_str().parse().ok();
        if member.is_none_or(|m| world.member(&m).is_none()) {
            return Err(Rejection::forbidden(
                "a broad grant is acknowledged by a member",
            ));
        }
        if acked.insert(&ack.item_id, ack).is_some() {
            return Err(Rejection::Invalid {
                why: format!("broad item {} is acknowledged twice", ack.item_id),
            });
        }
    }
    for (item, hash) in &binding.broad_items {
        match acked.remove(item) {
            Some(ack) if ack.selector_hash == *hash => {}
            Some(_) => {
                return Err(Rejection::Invalid {
                    why: format!("the acknowledgement of broad item {item} names another selector"),
                });
            }
            None => {
                return Err(Rejection::Invalid {
                    why: format!("broad item {item} needs its own acknowledgement"),
                });
            }
        }
    }
    match acked.keys().next() {
        Some(item) => Err(Rejection::Invalid {
            why: format!("item {item} is not a broad grant of this revision"),
        }),
        None => Ok(()),
    }
}

/// `PostScopedReadback`: acknowledged only at the confirmed revision and hash. Allowed while
/// the task is open or held by another agent, so a standby can claim when the lease ends.
pub(crate) fn decide_readback(
    world: &World,
    agent: &AgentId,
    task: TaskId,
    scope_rev: ScopeRev,
    scope_hash: ScopeHash,
    summary: String,
    predictions: Vec<Prediction>,
) -> Result<Vec<Event>, Rejection> {
    let t = world
        .task(task)
        .ok_or_else(|| Rejection::unknown("task", task))?;
    if !matches!(t.state, TaskState::Open | TaskState::Claimed { .. }) {
        return Err(Rejection::wrong_state("task", t.state.name()));
    }
    let lane = world
        .lane(t.order)
        .ok_or_else(|| Rejection::wrong_state("order", "not scope-first"))?;
    let confirmation = confirmed(lane)?;
    if scope_rev != confirmation.scope_rev || scope_hash != confirmation.scope_hash {
        return Err(stale(lane));
    }
    if summary.trim().is_empty() {
        return Err(Rejection::invalid("a readback needs a summary"));
    }
    if predictions.len() > MAX_PREDICTIONS {
        return Err(Rejection::invalid(
            "a readback carries at most 256 predictions",
        ));
    }
    Ok(vec![Event::ScopedReadbackPosted {
        readback: Box::new(ScopedReadback {
            id: world.next_readback_id()?,
            task,
            agent: agent.clone(),
            scope_rev,
            scope_hash,
            summary,
            predictions,
        }),
    }])
}

/// The agent's latest readback of `task` acknowledged at the confirmed revision and hash. The
/// only authority a scope-first claim rests on.
pub(crate) fn acknowledged_readback(
    world: &World,
    lane: &ScopeLane,
    agent: &AgentId,
    task: TaskId,
) -> Result<ReadbackId, Rejection> {
    let confirmation = confirmed(lane)?;
    let mut mine = world
        .scoped_readbacks_of(task)
        .filter(|r| &r.agent == agent)
        .peekable();
    if mine.peek().is_none() {
        return Err(Rejection::NotReadBack { task });
    }
    mine.filter(|r| {
        r.scope_rev == confirmation.scope_rev && r.scope_hash == confirmation.scope_hash
    })
    .last()
    .map(|r| r.id)
    .ok_or_else(|| stale(lane))
}

/// Refuses a build in a lane that is unconfirmed or stopped.
pub(crate) fn check_buildable(lane: &ScopeLane) -> Result<(), Rejection> {
    confirmed(lane).map(drop)
}

/// The lane and run of a scope-first gate run.
pub(crate) fn lane_run(world: &World, run: GateRunId) -> Result<(&ScopeLane, &LaneRun), Rejection> {
    let gate = world
        .gate(run)
        .ok_or_else(|| Rejection::unknown("gate run", run))?;
    let order = world
        .task(gate.task)
        .ok_or_else(|| Rejection::unknown("task", gate.task))?
        .order;
    let lane = world
        .lane(order)
        .ok_or_else(|| Rejection::wrong_state("gate run", "not scope-first"))?;
    let lane_run = lane
        .runs
        .get(&run)
        .ok_or_else(|| Rejection::unknown("scope-first run", run))?;
    Ok((lane, lane_run))
}

/// Whether `run` is the task's current gating run.
pub(crate) fn is_gating(world: &World, task: TaskId, run: GateRunId) -> bool {
    world.task(task).is_some_and(|t| {
        matches!(&t.state, TaskState::Claimed { stage: crate::order::Stage::Gating { run: r, .. }, .. } if *r == run)
    })
}

/// Whether `run` still waits for its prepared commit and receipt: its task is gating, or it is
/// queued. A queued run waits only after a re-bind cleared its receipt, so a passed run that is
/// queued and still receipted waits for nothing.
pub(crate) fn awaits_receipt(world: &World, task: TaskId, run: GateRunId) -> bool {
    is_gating(world, task, run)
        || world.task(task).is_some_and(
            |t| matches!(&t.state, TaskState::Queued { run: queued, .. } if *queued == run),
        )
}

/// `CandidatePrepared`: the merged commit a gating run tests. It must sit on the confirmed
/// base, carry the confirmed scope, and name the current ruling head.
pub(crate) fn decide_prepared(
    world: &World,
    run: GateRunId,
    prepared: PreparedCommit,
) -> Result<Vec<Event>, Rejection> {
    let (lane, lane_run) = lane_run(world, run)?;
    if let Some(existing) = &lane_run.prepared {
        return if *existing == prepared {
            Ok(Vec::new())
        } else {
            Err(Rejection::wrong_state("gate run", "prepared"))
        };
    }
    if !awaits_receipt(world, lane_run.task, run) {
        return Err(Rejection::wrong_state("gate run", "closed"));
    }
    let confirmation = confirmed(lane)?;
    // A moved protected head is re-bound first (`RebindBase`), and only then is the candidate
    // prepared on it. A candidate is never prepared under a confirmation that names another base.
    if prepared.base_commit != confirmation.base_commit {
        return Err(Rejection::StaleEvidence {
            detail: format!(
                "prepared on base {}, scope confirmed against {}",
                prepared.base_commit, confirmation.base_commit
            ),
        });
    }
    if prepared.context.scope_hash != confirmation.scope_hash {
        return Err(stale(lane));
    }
    if prepared.context.ledger_head != world.ledger().head() {
        return Err(Rejection::StaleEvidence {
            detail: "the ruling log moved since the candidate was prepared".to_owned(),
        });
    }
    if prepared
        .context
        .merged_commit
        .as_str()
        .parse::<CommitId>()
        .is_err()
    {
        return Err(Rejection::invalid("the merged commit is not a commit id"));
    }
    Ok(vec![Event::CandidatePrepared {
        run,
        prepared: Box::new(prepared),
    }])
}

/// `RecordConfirmedInputs`: the origin records the protected inputs a confirmed scope revision was
/// confirmed under. Only the current revision's confirmation is recorded, and only at the base it
/// names. A moved head or a newer revision is refused, so nothing is recorded for another base.
/// The ruling log head must be the one in force at the confirmation. A repeat for the recorded
/// revision is a no-op.
pub(crate) fn decide_record_inputs(
    world: &World,
    order: OrderId,
    revision: ScopeRev,
    base: &CommitSha,
    inputs: ProtectedInputs,
) -> Result<Vec<Event>, Rejection> {
    let lane = world
        .lane(order)
        .ok_or_else(|| Rejection::unknown("order", order))?;
    if lane.scope.rev != revision {
        return Err(stale(lane));
    }
    let confirmation = confirmed(lane)?;
    if confirmation.base_commit != *base {
        return Err(Rejection::StaleEvidence {
            detail: format!(
                "the scope was confirmed on {}, not {base}: nothing is recorded",
                confirmation.base_commit
            ),
        });
    }
    // The inputs must be the ones in force at the confirmation. A ruling recorded after it is
    // not part of the confirmation, so the record is refused rather than taking that ruling in.
    if lane.confirmed_ledger_head != Some(inputs.ledger_head) {
        return Err(Rejection::StaleEvidence {
            detail: "a ruling was recorded after the scope was confirmed: nothing is recorded"
                .to_owned(),
        });
    }
    if lane
        .confirmed_inputs
        .as_ref()
        .is_some_and(|record| record.revision == revision)
    {
        return Ok(Vec::new());
    }
    Ok(vec![Event::ConfirmedInputsRecorded {
        order,
        revision,
        base: base.clone(),
        inputs: Box::new(inputs),
    }])
}

/// `RebindBase`: the protected head moved after the scope was confirmed. The confirmation moves
/// to `base` only when the scope's executable content, the ruling log and every protected input
/// are unchanged. The protected inputs are checked against each candidate prepared on the
/// confirmed base: that candidate recorded what the scope was tested under, and a landed or
/// dropped candidate is such a record too. Then each live gating run that was prepared loses its
/// prepared commit and receipt, so no old receipt is reused. With no such candidate, the inputs
/// are checked against the record the origin kept when this revision was confirmed. A move with
/// neither cannot be checked, so the scope must be confirmed again by a human.
pub(crate) fn decide_rebind(
    world: &World,
    order: OrderId,
    base: &CommitSha,
    context: &GateContext,
) -> Result<Vec<Event>, Rejection> {
    let lane = world
        .lane(order)
        .ok_or_else(|| Rejection::unknown("order", order))?;
    let confirmation = confirmed(lane)?;
    if confirmation.base_commit == *base {
        return Ok(Vec::new());
    }
    if context.protected_head != *base {
        return Err(Rejection::invalid(
            "the gate context was computed on another protected head",
        ));
    }
    if context.ledger_head != world.ledger().head() {
        return Err(Rejection::StaleEvidence {
            detail: "the ruling log moved since the candidate was prepared".to_owned(),
        });
    }
    let mut runs = Vec::new();
    let mut recorded = 0_usize;
    for (run, lane_run) in &lane.runs {
        // Only a candidate prepared on the confirmed base records what the scope was tested under.
        let Some(prepared) = lane_run
            .prepared
            .as_ref()
            .filter(|prepared| prepared.base_commit == confirmation.base_commit)
        else {
            continue;
        };
        if !same_inputs(&prepared.context, context) {
            return Err(Rejection::StaleEvidence {
                detail: format!(
                    "a protected input of run {run} moved: the scope must be confirmed again"
                ),
            });
        }
        recorded += 1;
        // A landed or dropped task keeps its record: only a live run is re-bound.
        let live = world.task(lane_run.task).is_some_and(|task| {
            !matches!(task.state, TaskState::Landed { .. } | TaskState::Dropped)
        });
        if live {
            runs.push(*run);
        }
    }
    // No candidate was prepared on the confirmed base, so the inputs the scope was confirmed under
    // come from the record the origin kept at confirmation. The record must be for this revision,
    // and every protected input must be the one it recorded.
    if recorded == 0 {
        let record = lane
            .confirmed_inputs
            .as_ref()
            .filter(|record| record.revision == lane.scope.rev)
            .ok_or_else(|| Rejection::StaleEvidence {
                detail: "no candidate was prepared on the confirmed base and no protected inputs \
                         were recorded for this revision: the scope must be confirmed again"
                    .to_owned(),
            })?;
        if record.inputs != ProtectedInputs::of(context) {
            return Err(Rejection::StaleEvidence {
                detail: "a protected input moved since the scope was confirmed: the scope must be \
                         confirmed again"
                    .to_owned(),
            });
        }
    }
    let scope = Scope {
        base_commit: base.clone(),
        ..lane.scope.clone()
    };
    let scope_hash = scope.scope_hash().map_err(hash_failed)?;
    // The semantic digest covers the base too, so the confirmation carries the digest of the
    // re-bound scope. Only the base changed, so its executable content is the confirmed one.
    let semantic_scope_hash = scope.semantic_scope_hash().map_err(hash_failed)?;
    let rebound = ScopeConfirmation {
        base_commit: base.clone(),
        scope_hash,
        semantic_scope_hash: Some(semantic_scope_hash),
        ..confirmation.clone()
    };
    Ok(vec![Event::ScopeRebound {
        order,
        scope: Box::new(scope),
        scope_hash,
        confirmation: Box::new(rebound),
        runs,
    }])
}

/// Whether two gate contexts agree on every input a re-bind must keep. The protected head, the
/// merged commit and the scope hash are left out: a re-bind changes exactly those.
fn same_inputs(prepared: &GateContext, current: &GateContext) -> bool {
    // Destructured so that a new field must be compared, or named as left out, to compile.
    let GateContext {
        protected_head: _,
        merged_commit: _,
        scope_hash: _,
        overlay_hash,
        effective_artifact_hashes,
        ledger_head,
        probe_set_hash,
        fixture_set_hash,
        runner_hash,
        evaluator_version,
        runtime,
        controlled_env_hash,
        sandbox_policy_hash,
    } = prepared;
    *overlay_hash == current.overlay_hash
        && *effective_artifact_hashes == current.effective_artifact_hashes
        && *ledger_head == current.ledger_head
        && *probe_set_hash == current.probe_set_hash
        && *fixture_set_hash == current.fixture_set_hash
        && *runner_hash == current.runner_hash
        && *evaluator_version == current.evaluator_version
        && *runtime == current.runtime
        && *controlled_env_hash == current.controlled_env_hash
        && *sandbox_policy_hash == current.sandbox_policy_hash
}

/// A scope-first run the host still has to prepare or contrast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingContrast<'w> {
    /// The order.
    pub order: OrderId,
    /// The gating task.
    pub task: TaskId,
    /// The run.
    pub run: GateRunId,
    /// The commit the worker reported.
    pub head: &'w CommitId,
    /// The prepared merged commit, once recorded.
    pub prepared: Option<&'w PreparedCommit>,
}
