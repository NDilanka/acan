//! Commands in, events out: the persisted and streamed schema.
//!
//! [`Stamped`] is stored in the host's event table and sent on the WebSocket as the same
//! JSON. Renaming a variant or field breaks stored logs.

use std::borrow::Cow;
use std::fmt;

use acan_intent::{
    CommitSha, Contest, ContestId, ContestKind, ContestStatus, EpisodeId, GateContext, LedgerEvent,
    Prediction, PreparedCommit, ProbeHash, Receipt, RepoPath, Scope, ScopeConfirmation, ScopeHash,
    ScopeRev,
};
use serde::de::{
    DeserializeSeed, Error as _, MapAccess, Visitor,
    value::{MapAccessDeserializer, StrDeserializer, StringDeserializer},
};
use serde::ser::{Error as _, SerializeMap};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::clash::{ClashKey, ContractRef, Declaration, DeclarationInput, OverlapKind, Settlement};
use crate::gate::{GateVerdict, WitnessOutcome};
use crate::ids::{
    AgentId, ClashId, CodeRev, CommitId, GateRunId, Generation, MachineKey, MemberId, OrderId,
    OrderRev, QuestionId, ReadbackId, Seq, TaskId, Timestamp, WitnessId,
};
use crate::index::Locus;
use crate::lane::{ProtectedInputs, ScopedReadback};
use crate::lease::{Fence, Lease, LeaseEnd};
use crate::member::{AgentPrincipal, Member, Role};
use crate::order::{
    Question, QuestionDraft, Readback, ReadbackVerdict, TaskSpec, Witness, WitnessSpec,
};
use crate::repair::ReceiptRecord;
use crate::world::WorldRef;

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
    /// Poster or owner. Splits the order into tasks at its current revision.
    SplitOrder {
        /// The order.
        order: OrderId,
        /// The order revision.
        rev: OrderRev,
        /// The tasks.
        tasks: Vec<TaskSpec>,
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
    /// The host records the protected inputs of a scope revision the human just confirmed, at the
    /// base the confirmation names. The origin refuses it when the revision or base is no longer
    /// current.
    RecordConfirmedInputs {
        /// The order.
        order: OrderId,
        /// The scope revision the confirmation covers.
        revision: ScopeRev,
        /// The base the confirmation names.
        base: CommitSha,
        /// The protected inputs at that base.
        inputs: Box<ProtectedInputs>,
    },
    /// The host could not merge a gating run's candidate onto the protected head. The run is
    /// refused: the task reopens with the conflicting files named, and nothing is installed.
    CandidateConflicted {
        /// The gating run.
        run: GateRunId,
        /// The conflicting repo-relative paths, in the order git reported them. Never empty.
        files: Vec<RepoPath>,
    },
    /// The host could not carry a gating run forward, and the origin refused the run with
    /// `reason`. The lease ends and the task reopens with the reason, the way a conflict does.
    /// Nothing is installed. Used when the origin refuses a re-bind of the scope to a moved
    /// protected head, so the task does not wait for its lease to expire.
    RefuseRun {
        /// The gating run.
        run: GateRunId,
        /// Why the run is refused, in the words the task records. Never empty.
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
    /// Whether this event may be shared outside its local world log.
    #[serde(default)]
    pub shareable: bool,
    /// What happened.
    pub event: Event,
}

// Derive the known tags from the same variants as the wire schema. The fallback must
// never accept a known tag whose fields failed to decode.
macro_rules! event_schema {
    ($($(#[$attr:meta])* $variant:ident { $($fields:tt)* },)*) => {
        /// A fact the origin accepted. Applying it never fails, reads a clock, or runs detection.
        #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum Event {
            $($(#[$attr])* $variant { $($fields)* },)*
            /// An event kind this version does not interpret.
            #[serde(untagged, serialize_with = "encode_unknown")]
            Unknown {
                /// The original wire tag.
                event_type: String,
                /// The other fields of the event object, excluding `type`.
                payload: Map<String, Value>,
            },
        }

        #[derive(Deserialize)]
        #[serde(remote = "Event", tag = "type", rename_all = "snake_case")]
        enum WireEvent {
            $($variant { $($fields)* },)*
            #[serde(untagged, deserialize_with = "decode_unknown")]
            Unknown {
                event_type: String,
                payload: Map<String, Value>,
            },
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum EventType {
            $($variant,)*
            #[serde(other)]
            Unrecognized,
        }
    };
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EventObject;

        impl<'de> Visitor<'de> for EventObject {
            type Value = Event;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an event object with a string type tag")
            }

            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Event, M::Error> {
                WireEvent::deserialize(MapAccessDeserializer::new(EventFields { map, tag: false }))
            }
        }

        // Internally tagged enums can also decode sequences; the log only accepts objects.
        deserializer.deserialize_map(EventObject)
    }
}

// Serde's enum identifiers also accept numeric variant indexes. Require a string tag
// without buffering the fields into a JSON map, which would hide duplicate known fields.
struct EventFields<M> {
    map: M,
    tag: bool,
}

impl<'de, M: MapAccess<'de>> MapAccess<'de> for EventFields<M> {
    type Error = M::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let Some(key) = self.map.next_key::<String>()? else {
            return Ok(None);
        };
        self.tag = key == "type";
        seed.deserialize(StringDeserializer::new(key)).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        if self.tag {
            seed.deserialize(StringDeserializer::new(self.map.next_value::<String>()?))
        } else {
            self.map.next_value_seed(seed)
        }
    }
}

fn decode_unknown<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<(String, Map<String, Value>), D::Error> {
    #[derive(Deserialize)]
    struct UnknownFields {
        #[serde(rename = "type")]
        event_type: String,
        #[serde(flatten)]
        payload: Map<String, Value>,
    }

    let UnknownFields {
        event_type,
        payload,
    } = UnknownFields::deserialize(deserializer)?;
    match EventType::deserialize(StrDeserializer::<D::Error>::new(&event_type))? {
        EventType::Unrecognized => Ok((event_type, payload)),
        _ => Err(D::Error::custom("invalid fields for a known event type")),
    }
}

fn encode_unknown<S: Serializer>(
    event_type: &str,
    payload: &Map<String, Value>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if payload.contains_key("type") {
        return Err(S::Error::custom(
            "event payload must not contain its type tag",
        ));
    }
    let mut map = serializer.serialize_map(Some(payload.len() + 1))?;
    map.serialize_entry("type", event_type)?;
    for (key, value) in payload {
        map.serialize_entry(key, value)?;
    }
    map.end()
}

event_schema! {
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
    /// The protected inputs a confirmed scope revision was confirmed under, at the base it names.
    /// The host records them when the human confirms, so a re-bind of that revision can check a
    /// moved head against them when no candidate was prepared on the base.
    ConfirmedInputsRecorded {
        /// The order.
        order: OrderId,
        /// The confirmed scope revision.
        revision: ScopeRev,
        /// The base the confirmation names.
        base: CommitSha,
        /// The protected inputs at that base.
        inputs: Box<ProtectedInputs>,
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
        let mut original: Value = serde_json::from_str(line).expect("valid JSON");
        original["shareable"] = Value::Bool(false);
        assert_eq!(
            again, original,
            "a legacy line re-encodes with the default flag"
        );
        stamped
    }

    #[test]
    fn stamped_events_are_local_by_default_and_preserve_explicit_sharing() {
        use crate::{CodeIndex, Ctx, Origin};

        let founder: MemberId = "owner".parse().expect("valid member");
        let world = WorldRef {
            id: "w".parse().expect("valid world"),
            repo: "demo".to_owned(),
        };
        let now = Timestamp::from_millis(1000);
        let (mut origin, opened) = Origin::open(world, founder, now);
        assert!(!opened.shareable, "the origin opens a local world log");

        let mut json = serde_json::to_value(&opened).expect("the opening encodes");
        json.as_object_mut().expect("an object").remove("shareable");
        assert_eq!(
            serde_json::from_value::<Stamped>(json.clone()).expect("legacy opening decodes"),
            opened
        );
        for flag in [true, false] {
            json["shareable"] = Value::Bool(flag);
            let decoded: Stamped = serde_json::from_value(json.clone()).expect("flag decodes");
            assert_eq!(decoded.shareable, flag);
            assert_eq!(serde_json::to_value(decoded).expect("flag encodes"), json);
        }

        let index = CodeIndex::new("rev-test".parse().expect("valid revision"));
        let batch = origin
            .execute(
                &Ctx { now, index: &index },
                Command::System(SystemCmd::IndexUpdated),
            )
            .expect("the index update is accepted");
        assert!(!batch.is_empty());
        assert!(batch.iter().all(|stamped| !stamped.shareable));
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
