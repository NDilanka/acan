//! Commands in, events out: the persisted and streamed schema.
//!
//! [`Stamped`] is stored in the host's event table and sent on the WebSocket as the same
//! JSON. Renaming a variant or field breaks stored logs.

use std::borrow::Cow;
use std::fmt;

use acan_intent::{
    CommitSha, Contest, ContestId, ContestKind, ContestStatus, EpisodeId, GateContext, LedgerEvent,
    Prediction, PreparedCommit, ProbeHash, Receipt, RepoPath, Scope, ScopeConfirmation, ScopeDraft,
    ScopeHash, ScopeRev,
};
use serde::{Deserialize, Serialize};

use crate::clash::{
    ClashKey, ContractRef, Declaration, DeclarationInput, Explanation, OverlapKind, Settlement,
};
use crate::gate::{GateVerdict, WitnessOutcome};
use crate::ids::{
    AgentId, ClashId, CodeRev, CommitId, GateRunId, Generation, MachineKey, MemberId, OrderId,
    OrderRev, QuestionId, ReadbackId, Seq, TaskId, Timestamp, WitnessId,
};
use crate::index::Locus;
use crate::lane::ScopedReadback;
use crate::lease::{Fence, Lease, LeaseEnd};
use crate::member::{AgentPrincipal, Member, Role};
use crate::order::{
    Question, QuestionDraft, Readback, ReadbackVerdict, TaskSpec, Witness, WitnessSpec,
};
use crate::repair::ReceiptRecord;
use crate::world::{Step, WorldRef};

/// A command with the principal the host authenticated.
///
/// Not deserializable: the host builds it from an authenticated session, so a request can
/// never choose its own principal, and `System` commands can never arrive over the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// From the UI, as a member.
    Human {
        /// The authenticated member.
        member: MemberId,
        /// What they asked for.
        cmd: HumanCmd,
    },
    /// From an MCP tool, as an agent.
    Agent {
        /// The authenticated agent.
        agent: AgentId,
        /// What it asked for.
        cmd: AgentCmd,
    },
    /// From the origin's own job loop.
    System(SystemCmd),
}

/// What a member can ask for. This is the HTTP request body a desktop sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum HumanCmd {
    /// Owner only. Adds or changes a member.
    AddMember {
        /// The member.
        member: MemberId,
        /// Their role.
        role: Role,
    },
    /// Registers an agent owned by the caller.
    RegisterAgent {
        /// The agent's id from the token store.
        agent: AgentId,
    },
    /// Posts an order with its locked witnesses.
    PostOrder {
        /// The machine to pin it on.
        machine: MachineKey,
        /// What to do.
        sentence: String,
        /// The checks that must hold.
        witnesses: Vec<WitnessSpec>,
    },
    /// Answers an open question with option 0 or 1.
    AnswerQuestion {
        /// The question.
        question: QuestionId,
        /// The chosen option index.
        option: usize,
    },
    /// Settles or proposes a choice from the clash's menu.
    SettleClash {
        /// The clash.
        clash: ClashId,
        /// The choice. Must be on [`crate::Clash::menu`].
        choice: Settlement,
    },
    /// Cancels an order that has not landed.
    CancelOrder {
        /// The order.
        order: OrderId,
    },
    /// Poster or owner. Sets the order's scope revision. The first one moves an order with no
    /// claimed task into the scope-first lane; a newer one voids the confirmation.
    ReviseScope {
        /// The order.
        order: OrderId,
        /// The new revision.
        scope: Box<Scope>,
    },
    /// Poster or owner. Confirms the order's current scope revision.
    ConfirmScope {
        /// The order.
        order: OrderId,
        /// The confirmation, signed by the caller.
        confirmation: Box<ScopeConfirmation>,
    },
    /// Poster or owner. Answers a contest.
    ResolveContest {
        /// The contest.
        contest: ContestId,
        /// `upheld` or `rejected`.
        status: ContestStatus,
    },
    /// Appends one ruling to the ruling log, signed by the caller.
    RecordRuling {
        /// The ruling.
        event: Box<LedgerEvent>,
    },
}

/// What an agent can ask for. These are the MCP tool arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum AgentCmd {
    /// Reads back an open task.
    PostReadback {
        /// The task.
        task: TaskId,
        /// What the agent will do.
        summary: String,
        /// Where it expects to work.
        touches: Vec<Locus>,
    },
    /// Claims an open task. Needs a matched readback at the current revision.
    Claim {
        /// The task.
        task: TaskId,
    },
    /// Extends the caller's lease.
    Renew {
        /// The caller's fence.
        fence: Fence,
    },
    /// Gives the task back.
    Release {
        /// The caller's fence.
        fence: Fence,
    },
    /// Declares or redeclares the footprint.
    Declare {
        /// The caller's fence.
        fence: Fence,
        /// What it will change.
        footprint: DeclarationInput,
    },
    /// Reports a built commit and starts the gate.
    ReportBuilt {
        /// The caller's fence.
        fence: Fence,
        /// The commit to test.
        head: CommitId,
    },
    /// Reads back a scope-first task at the scope revision the agent read.
    PostScopedReadback {
        /// The task.
        task: TaskId,
        /// The revision read.
        scope_rev: ScopeRev,
        /// Its hash.
        scope_hash: ScopeHash,
        /// The plan in the agent's words.
        summary: String,
        /// What the agent predicts. Scored, never evidence.
        predictions: Vec<Prediction>,
    },
    /// Objects to one discrepancy episode of the caller's order.
    FileContest {
        /// The caller's fence.
        fence: Fence,
        /// The episode.
        episode: EpisodeId,
        /// What the agent claims.
        kind: ContestKind,
        /// The probe that shows it.
        reproducer: ProbeHash,
        /// Why.
        rationale: String,
    },
}

/// Results from the origin's job loop and timers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemCmd {
    /// The host rescanned; `Ctx::index` is the new index.
    IndexUpdated,
    /// The foreman split an order.
    SplitReady {
        /// The order.
        order: OrderId,
        /// The revision the job was derived at.
        rev: OrderRev,
        /// The tasks.
        tasks: Vec<TaskSpec>,
    },
    /// The foreman reviewed readbacks. Legacy lane only; see [`Event::ReadbacksReviewed`].
    ReadbacksReviewed {
        /// The order.
        order: OrderId,
        /// The revision the job was derived at.
        rev: OrderRev,
        /// One verdict per reviewed readback.
        verdicts: Vec<(ReadbackId, ReadbackVerdict)>,
        /// A question, when readbacks disagree.
        question: Option<QuestionDraft>,
    },
    /// The foreman explained a clash.
    ClashExplained {
        /// The clash.
        clash: ClashId,
        /// The explanation.
        explanation: Explanation,
    },
    /// A witness finished.
    WitnessResult {
        /// The run.
        run: GateRunId,
        /// The witness.
        witness: WitnessId,
        /// Its outcome.
        outcome: WitnessOutcome,
    },
    /// A merge finished.
    LandResult {
        /// The task.
        task: TaskId,
        /// The commit the job merged.
        head: CommitId,
        /// The merge commit, or why it failed.
        result: Result<CommitId, String>,
    },
    /// A model call finished, for the usage panel.
    ModelUsed {
        /// The model name.
        model: String,
        /// Which step called it.
        step: Step,
        /// Prompt tokens.
        input_tokens: u32,
        /// Completion tokens.
        output_tokens: u32,
    },
    /// A timer tick. Expires due leases and lifts declare barriers past their deadline.
    Tick,
    /// The host prepared the merged commit a scope-first run tests.
    CandidatePrepared {
        /// The run.
        run: GateRunId,
        /// The prepared commit and its tested context.
        prepared: Box<PreparedCommit>,
    },
    /// The receipt of a prepared scope-first run.
    ReceiptReady {
        /// The run.
        run: GateRunId,
        /// The receipt.
        receipt: Box<Receipt>,
    },
    /// The protected head moved after the scope was confirmed, and nothing the scope binds
    /// moved. The host computed the gate context on `base`; the core re-binds the confirmation
    /// to `base` and checks that context against what each prepared run was tested under.
    RebindBase {
        /// The order.
        order: OrderId,
        /// The protected head the confirmation moves to.
        base: CommitSha,
        /// The gate context computed on `base` with the current scope and rulings.
        context: Box<GateContext>,
    },
    /// The host could not merge a gating run's candidate onto the protected head. The run is
    /// refused: the task reopens with the conflicting files named, and nothing is installed.
    CandidateConflicted {
        /// The gating run.
        run: GateRunId,
        /// The conflicting repo-relative paths, in the order git reported them. Never empty.
        files: Vec<RepoPath>,
    },
    /// The foreman drafted the scope of an order revision.
    ScopeDrafted {
        /// The order.
        order: OrderId,
        /// The revision the job was derived at.
        rev: OrderRev,
        /// The draft, checked against the plant and the route table.
        draft: Box<ScopeDraft>,
    },
    /// The scope draft of an order revision failed after its last attempt.
    ScopeDraftFailed {
        /// The order.
        order: OrderId,
        /// The revision the job was derived at.
        rev: OrderRev,
        /// Why the draft failed.
        reason: String,
    },
}

/// Who caused an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Actor {
    /// A member.
    Member(MemberId),
    /// An agent.
    Agent(AgentId),
    /// The origin itself.
    System,
}

/// An event with its log position. The unit of storage, streaming and replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamped {
    /// Position in the world log.
    pub seq: Seq,
    /// When the origin accepted it.
    pub at: Timestamp,
    /// Who caused it.
    pub actor: Actor,
    /// What happened.
    pub event: Event,
}

/// A fact the origin accepted. Applying it never fails, reads a clock, or runs detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The first event of every log.
    WorldOpened {
        world: WorldRef,
        founder: MemberId,
    },
    MemberAdded {
        member: Member,
    },
    AgentRegistered {
        agent: AgentPrincipal,
    },
    IndexUpdated {
        rev: CodeRev,
    },
    OrderPosted {
        order: OrderId,
        machine: MachineKey,
        sentence: String,
        witnesses: Vec<Witness>,
        posted_by: MemberId,
    },
    OrderSplit {
        order: OrderId,
        rev: OrderRev,
        tasks: Vec<(TaskId, TaskSpec)>,
    },
    ReadbackPosted {
        readback: Readback,
    },
    /// The legacy model review of readbacks. Recorded logs keep it and replay it unchanged.
    /// It gates claims only in the legacy lane. The scope-first lane never takes it as
    /// authority: a model verdict is neither an observation nor a human decision.
    ReadbacksReviewed {
        order: OrderId,
        rev: OrderRev,
        verdicts: Vec<(ReadbackId, ReadbackVerdict)>,
    },
    QuestionPinned {
        question: Question,
    },
    QuestionAnswered {
        question: QuestionId,
        option: usize,
        witness: Witness,
    },
    OrderRevised {
        order: OrderId,
        rev: OrderRev,
        witnesses: Vec<WitnessId>,
    },
    LeaseGranted {
        task: TaskId,
        lease: Lease,
        readback: ReadbackId,
    },
    LeaseRenewed {
        task: TaskId,
        generation: Generation,
        expires_at: Timestamp,
    },
    LeaseEnded {
        task: TaskId,
        generation: Generation,
        why: LeaseEnd,
    },
    Declared {
        decl: Declaration,
    },
    BarrierExpired {
        order: OrderId,
    },
    ClashFound {
        clash: ClashId,
        key: ClashKey,
        kind: OverlapKind,
        found_at: CodeRev,
        contract: Option<ContractRef>,
    },
    ClashDissolved {
        clash: ClashId,
    },
    ClashExplained {
        clash: ClashId,
        explanation: Explanation,
    },
    ClashProposed {
        clash: ClashId,
        choice: Settlement,
        by: MemberId,
    },
    ClashSettled {
        clash: ClashId,
        choice: Settlement,
        by: MemberId,
    },
    BuildCleared {
        task: TaskId,
    },
    BuildReported {
        task: TaskId,
        generation: Generation,
        head: CommitId,
        run: GateRunId,
        rev: OrderRev,
        witnesses: Vec<WitnessId>,
    },
    WitnessReported {
        run: GateRunId,
        witness: WitnessId,
        outcome: WitnessOutcome,
    },
    GateClosed {
        run: GateRunId,
        verdict: GateVerdict,
    },
    Landed {
        task: TaskId,
        commit: CommitId,
    },
    LandFailed {
        task: TaskId,
        reason: String,
    },
    OrderCancelled {
        order: OrderId,
    },
    ModelUsed {
        model: String,
        step: Step,
        input_tokens: u32,
        output_tokens: u32,
    },
    /// A human set the order's scope revision. Clears any confirmation.
    ScopeRevised {
        order: OrderId,
        scope: Box<Scope>,
        scope_hash: ScopeHash,
    },
    /// A human confirmed the current scope revision.
    ScopeConfirmed {
        order: OrderId,
        confirmation: Box<ScopeConfirmation>,
    },
    /// The confirmed scope moved to a new protected head. Its executable content is unchanged.
    /// Each run in `runs` lost its prepared commit and receipt, so none of them is reused: each
    /// is prepared on the new base and receipted again.
    ScopeRebound {
        order: OrderId,
        scope: Box<Scope>,
        scope_hash: ScopeHash,
        confirmation: Box<ScopeConfirmation>,
        runs: Vec<GateRunId>,
    },
    /// An agent read back a scope-first task; the acknowledgement is derived from it.
    ScopedReadbackPosted {
        readback: Box<ScopedReadback>,
    },
    /// The merged commit a scope-first run tests.
    CandidatePrepared {
        run: GateRunId,
        prepared: Box<PreparedCommit>,
    },
    /// A scope-first run's receipt and everything the core decided from it.
    ReceiptRecorded {
        run: GateRunId,
        record: Box<ReceiptRecord>,
    },
    /// An agent contested an episode.
    ContestFiled {
        contest: Box<Contest>,
    },
    /// A human answered a contest.
    ContestResolved {
        order: OrderId,
        contest: ContestId,
        status: ContestStatus,
        by: MemberId,
    },
    /// A human ruling appended to the ruling log.
    RulingRecorded {
        event: Box<LedgerEvent>,
    },
    /// The foreman drafted a scope for an order revision. The draft carries no authority: it
    /// never confirms a scope and never sets a verdict, count, class, mask or observed value.
    /// Only a human's `ReviseScope` and `ConfirmScope` do that.
    ScopeDrafted {
        /// The order.
        order: OrderId,
        /// The order revision the draft was for.
        rev: OrderRev,
        /// The draft, with the model that proposed it.
        draft: Box<ScopeDraft>,
    },
    /// The scope draft of an order revision failed after every attempt. Nothing was drafted and
    /// nothing falls back to an offline draft.
    ScopeDraftFailed {
        /// The order.
        order: OrderId,
        /// The order revision the draft was for.
        rev: OrderRev,
        /// Why the draft failed.
        reason: String,
    },
}

/// Why the origin refused a command. A refused command appends nothing.
///
/// Serialized in HTTP error bodies and parsed back by a joined desktop, so it round-trips.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Rejection {
    /// The principal may not do this.
    #[error("forbidden: {why}")]
    Forbidden { why: Cow<'static, str> },
    /// The referenced entity does not exist.
    #[error("unknown {what} {id}")]
    Unknown { what: Cow<'static, str>, id: String },
    /// A job result or command was for an older order revision.
    #[error("stale order revision; current is {current:?}")]
    StaleRevision { current: OrderRev },
    /// A fenced write carried a generation that is not the task's live one.
    #[error("stale generation; current is {current:?}")]
    StaleGeneration { current: Option<Generation> },
    /// The caller's lease has expired.
    #[error("lease expired")]
    LeaseExpired,
    /// The caller's readback does not match the current witnesses.
    #[error("readback does not match witnesses {failing:?}")]
    NotMatched { failing: Vec<WitnessId> },
    /// Another agent holds the task.
    #[error("held by {holder} until {until:?}")]
    HeldBy { holder: AgentId, until: Timestamp },
    /// The order has an open question.
    #[error("order has an open question {question}")]
    OpenQuestion { question: QuestionId },
    /// The entity is in the wrong state for this command.
    #[error("{what} is {found}")]
    WrongState {
        what: Cow<'static, str>,
        found: Cow<'static, str>,
    },
    /// The command is malformed.
    #[error("invalid command: {why}")]
    Invalid { why: String },
    /// The command names a scope revision that is not the order's current one.
    #[error("stale scope; current is revision {} ({current_hash})", current_rev.0)]
    StaleScope {
        current_rev: ScopeRev,
        current_hash: ScopeHash,
    },
    /// The order's current scope revision has no confirmation.
    #[error("the scope of order {order} is not confirmed")]
    ScopeNotConfirmed { order: OrderId },
    /// The order's repair budget ran out.
    #[error("the repair budget of order {order} is exhausted")]
    BudgetExhausted { order: OrderId },
    /// The agent has no scope-first readback of the task.
    #[error("no readback of task {task}")]
    NotReadBack { task: TaskId },
    /// The evidence is for another commit, context or ruling log revision.
    #[error("stale evidence: {detail}")]
    StaleEvidence { detail: String },
}

impl Rejection {
    pub(crate) fn forbidden(why: &'static str) -> Self {
        Self::Forbidden { why: why.into() }
    }

    pub(crate) fn unknown(what: &'static str, id: impl fmt::Display) -> Self {
        Self::Unknown {
            what: what.into(),
            id: id.to_string(),
        }
    }

    pub(crate) fn wrong_state(what: &'static str, found: &'static str) -> Self {
        Self::WrongState {
            what: what.into(),
            found: found.into(),
        }
    }

    pub(crate) fn invalid(why: &'static str) -> Self {
        Self::Invalid { why: why.into() }
    }
}

/// Why a log could not be folded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    /// The log does not start with `WorldOpened` at seq 0.
    #[error("log does not start with WorldOpened at seq 0")]
    NotOpened,
    /// An event arrived out of order. The caller refetches from `expected`.
    #[error("expected seq {expected:?}, got {got:?}")]
    SeqGap { expected: Seq, got: Seq },
    /// An event refers to an entity the log never created, or creates one twice. The log is
    /// damaged or forged; nothing from this event on was applied.
    #[error("invalid event at seq {seq:?}: {why}")]
    InvalidEvent { seq: Seq, why: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A readback reviewed by the legacy model review, as a log written before the scope-first
    /// lane stored it. These bytes must keep decoding so old logs replay.
    const LEGACY_REVIEW: &str = r#"{"seq":7,"at":1000,"actor":{"kind":"system"},"event":{"type":"readbacks_reviewed","order":0,"rev":1,"verdicts":[[0,{"rev":1,"per_witness":{"0":{"judgement":"match"},"1":{"judgement":"disagrees","reason":"drops the default"}}}]]}}"#;

    const LEGACY_READBACK: &str = r#"{"seq":8,"at":1001,"actor":{"kind":"agent","id":"worker-1"},"event":{"type":"readback_posted","readback":{"id":1,"task":0,"agent":"worker-1","summary":"keep the default","touches":[],"verdict":{"rev":1,"per_witness":{"0":{"judgement":"unclear","reason":"says too little"}}}}}}"#;

    fn round_trips(line: &str) -> Stamped {
        let stamped: Stamped = serde_json::from_str(line).expect("a legacy line decodes");
        let again = serde_json::to_value(&stamped).expect("it encodes");
        let original: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert_eq!(again, original, "a legacy line re-encodes to the same JSON");
        stamped
    }

    #[test]
    fn a_legacy_model_review_still_decodes() {
        let stamped = round_trips(LEGACY_REVIEW);
        let Event::ReadbacksReviewed { verdicts, .. } = stamped.event else {
            panic!("the line is a legacy review, got {:?}", stamped.event);
        };
        let [(readback, verdict)] = verdicts.as_slice() else {
            panic!("one verdict, got {verdicts:?}");
        };
        assert_eq!(*readback, ReadbackId::new(0));
        assert_eq!(
            verdict.per_witness.get(&WitnessId::new(1)),
            Some(&crate::order::Judgement::Disagrees {
                reason: "drops the default".to_owned()
            })
        );
    }

    #[test]
    fn a_legacy_readback_with_a_model_verdict_still_decodes() {
        let stamped = round_trips(LEGACY_READBACK);
        let Event::ReadbackPosted { readback } = stamped.event else {
            panic!("the line is a readback, got {:?}", stamped.event);
        };
        assert!(readback.verdict.is_some(), "the legacy verdict is kept");
    }
}
