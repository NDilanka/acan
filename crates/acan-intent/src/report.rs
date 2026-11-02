//! The receipt: a pure function of observations, masks, the scope and the expectations.
//!
//! Every count carries its denominator. A route counts as covered when at least one probe
//! reached it; that is a route hit, not adequate behavioral coverage, which the obligation
//! counts in [`BehaviorCounts`] report separately. The receipt is attached to the PR after the
//! commit is frozen, never inside it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::episode::{Contest, ContestKind, ContestStatus, EpisodeKey};
use crate::gate::PreparedCommit;
use crate::hash::{
    CanonicalError, ContentHash, EpisodeId, ObservationHash, ProbeHash, ReceiptHash, ScopeHash,
};
use crate::ledger::NotEnforcedReason;
use crate::observation::Mask;
use crate::probe::{RoutePath, RunRole};
use crate::provenance::{EvidenceAddress, Provenance};
use crate::scope::{Scope, Selector};
use crate::types::{
    AgentRef, CommitSha, ContestId, Evidence, HttpMethod, InvalidValue, ItemId, MachineId,
    NonEmpty, ObligationId, ObservationPath, OrderRef, RouteTemplate, RulingId, RunId, ScopeRev,
    StepId, TaskRef, UnixMillis,
};
use crate::verdict::{CheckResult, Evaluation, ItemClass, ItemState, UnknownReason};
use crate::version::{ContestV1, ReceiptV1};

/// Items per class. Core-derived: the fields are private to this crate and no public
/// constructor exists, so a count can only come from the core's classification. A decoded
/// copy is display data until [`Receipt::check_counts`] or [`crate::PlantView::check_counts`]
/// has matched it against the items.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClassCounts {
    pub(crate) preserved: u32,
    pub(crate) obligation_fulfilled: u32,
    pub(crate) obligation_failed: u32,
    pub(crate) authorized_but_unspecified: u32,
    pub(crate) out_of_scope: u32,
    pub(crate) violates_ruling: u32,
    pub(crate) unstable_base: u32,
    pub(crate) incomparable: u32,
    pub(crate) error: u32,
}

impl ClassCounts {
    /// How many items have `class`.
    #[must_use]
    pub fn get(&self, class: ItemClass) -> u32 {
        match class {
            ItemClass::Preserved => self.preserved,
            ItemClass::ObligationFulfilled => self.obligation_fulfilled,
            ItemClass::ObligationFailed => self.obligation_failed,
            ItemClass::AuthorizedButUnspecified => self.authorized_but_unspecified,
            ItemClass::OutOfScope => self.out_of_scope,
            ItemClass::ViolatesRuling => self.violates_ruling,
            ItemClass::UnstableBase => self.unstable_base,
            ItemClass::Incomparable => self.incomparable,
            ItemClass::Error => self.error,
        }
    }

    /// How many items there are: the denominator of every class count. Summed in `u64`, so
    /// decoded counts near `u32::MAX` neither panic nor wrap.
    #[must_use]
    pub fn total(&self) -> u64 {
        ItemClass::ALL
            .iter()
            .map(|class| u64::from(self.get(*class)))
            .sum()
    }

    /// Whether these counts are exactly the classes of `classes`, one per item.
    #[must_use]
    pub fn matches(&self, classes: impl IntoIterator<Item = ItemClass>) -> bool {
        let mut tally = [0_u64; ItemClass::ALL.len()];
        for class in classes {
            if let Some(slot) = ItemClass::ALL.iter().position(|c| *c == class) {
                tally[slot] += 1;
            }
        }
        ItemClass::ALL
            .iter()
            .zip(tally)
            .all(|(class, count)| u64::from(self.get(*class)) == count)
    }
}

/// The error a reader gets when stored counts do not match the stored items.
pub(crate) fn mismatched_counts(counts: &ClassCounts, items: usize) -> InvalidValue {
    InvalidValue::Malformed {
        what: "class counts",
        raw: format!("{} counted for {items} items", counts.total()),
    }
}

/// Where the gate is for the receipt's candidate. Drawn on the order plate.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum GateState {
    /// The merged commit is prepared; no run has started.
    Prepared,
    /// Runs are in progress.
    Running,
    /// An item does not pass, or a required behavior is not enforced. Land is refused.
    Blocked,
    /// The tested merged commit landed.
    Landed,
    /// The repair budget ran out.
    Stopped,
}

/// Which column of the card an evidence value belongs to.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSlot {
    /// The first base run.
    BaseA,
    /// The second base run.
    BaseB,
    /// The candidate run.
    Candidate,
    /// The scope example's desired target.
    Target,
    /// The agent's prediction.
    Prediction,
}

impl EvidenceSlot {
    /// Whether a value in this slot may carry `provenance`. Run columns hold observed or
    /// core-derived values, the target holds a human spec, and the prediction a model proposal,
    /// so nothing hypothetical is shown as observed.
    #[must_use]
    pub fn admits(self, provenance: Provenance) -> bool {
        match self {
            Self::BaseA | Self::BaseB | Self::Candidate => {
                matches!(provenance, Provenance::Observed | Provenance::CoreDerived)
            }
            Self::Target => provenance == Provenance::HumanSpec,
            Self::Prediction => provenance == Provenance::ModelProposal,
        }
    }
}

/// One value on the card, with the slot it belongs to, who authored it, and for an observed
/// value its full [`EvidenceAddress`]: run, commit, step, field and stored observation.
///
/// The value keeps its structure, and a field the observation lacks is
/// [`Evidence::Missing`], never `null`. Decoding refuses a slot and provenance that do not fit
/// ([`EvidenceSlot::admits`]) and an observed value without its run, commit or observation hash.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(try_from = "EvidenceRowWire")]
pub struct EvidenceRow {
    /// The column.
    pub slot: EvidenceSlot,
    /// The run, for observed values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// The commit the run tested, for observed values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitSha>,
    /// The stored observation the value was read from, for observed values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_hash: Option<ObservationHash>,
    /// The step.
    pub step: StepId,
    /// The field.
    pub field: ObservationPath,
    /// The value, or its absence.
    pub value: Evidence,
    /// Who authored it.
    pub provenance: Provenance,
}

impl EvidenceRow {
    /// Where an observed value was recorded. `None` for targets, predictions and core-derived
    /// values that name no single observation.
    #[must_use]
    pub fn address(&self) -> Option<EvidenceAddress> {
        Some(EvidenceAddress {
            run_id: self.run_id.clone()?,
            commit: self.commit.clone()?,
            step_id: self.step.clone(),
            path: self.field.clone(),
            observation_hash: self.observation_hash?,
        })
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "EvidenceRow")]
struct EvidenceRowWire {
    slot: EvidenceSlot,
    #[serde(default)]
    run_id: Option<RunId>,
    #[serde(default)]
    commit: Option<CommitSha>,
    #[serde(default)]
    observation_hash: Option<ObservationHash>,
    step: StepId,
    field: ObservationPath,
    value: Evidence,
    provenance: Provenance,
}

impl TryFrom<EvidenceRowWire> for EvidenceRow {
    type Error = InvalidValue;

    fn try_from(wire: EvidenceRowWire) -> Result<Self, Self::Error> {
        let unanchored = wire.provenance == Provenance::Observed
            && (wire.run_id.is_none() || wire.commit.is_none() || wire.observation_hash.is_none());
        if !wire.slot.admits(wire.provenance) || unanchored {
            return Err(InvalidValue::Malformed {
                what: "evidence row",
                raw: format!("slot {:?} with provenance {:?}", wire.slot, wire.provenance),
            });
        }
        Ok(Self {
            slot: wire.slot,
            run_id: wire.run_id,
            commit: wire.commit,
            observation_hash: wire.observation_hash,
            step: wire.step,
            field: wire.field,
            value: wire.value,
            provenance: wire.provenance,
        })
    }
}

impl JsonSchema for EvidenceRow {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "EvidenceRow".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        EvidenceRowWire::json_schema(generator)
    }
}

/// How one step of a probe sequence went, for the crate tint and the sequence strip.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StepTint {
    /// The step matched base.
    Same,
    /// The step differed from base.
    Differed,
    /// The step has no answer. Drawn grey, never green.
    Unknown,
}

/// One request of a probe sequence in one run.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SequenceStep {
    /// The step.
    pub step_id: StepId,
    /// The request's position in the sequence, from 1.
    pub request: u32,
    /// The run.
    pub run_id: RunId,
    /// The run's role.
    pub role: RunRole,
    /// The observed HTTP status, if the request completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The tint.
    pub tint: StepTint,
}

/// One classified receipt item.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReceiptItem {
    /// A scope item, or an item the core minted for a check outside the scope.
    pub item_id: ItemId,
    /// Its class.
    pub class: ItemClass,
    /// Where it stands in the repair loop.
    pub state: ItemState,
    /// The machine its mark goes on.
    pub machine_id: MachineId,
    /// The canonical episode selector of what this item checks:
    /// [`crate::canonical_episode_selector`] of its scope selector, or of the difference's
    /// selector for an item no scope selector names. Its episode id, when present, is the hash
    /// of an [`EpisodeKey`] built from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<Selector>,
    /// The probe that checked it.
    pub probe_hash: ProbeHash,
    /// Its episode, if it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_id: Option<EpisodeId>,
    /// The ruling it violates or relies on, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruling_id: Option<RulingId>,
    /// Repair attempts used.
    pub attempts: u32,
    /// Repair attempts allowed.
    pub budget: u32,
    /// The machines its request passes.
    pub route_path: RoutePath,
    /// The sequence strip.
    pub sequence: Vec<SequenceStep>,
    /// The values on the card.
    pub evidence: Vec<EvidenceRow>,
    /// What the core weighed.
    pub evaluation: Evaluation,
}

/// The probe denominator.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProbeCounts {
    /// Probes in the corpus, retired ones excluded.
    pub total: u32,
    /// Probes run on base A, base B and the candidate.
    pub run: u32,
    /// Probes whose runs did not complete.
    pub errored: u32,
}

/// A baseline route no probe exercises.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UncoveredRoute {
    /// The method.
    pub method: HttpMethod,
    /// The route.
    pub route_template: RouteTemplate,
    /// Why, from the corpus.
    pub reason: String,
}

/// The route denominator. `covered` counts route hits only.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteCounts {
    /// Every method-and-route pattern of the baseline route table.
    pub total: u32,
    /// Patterns at least one run probe reached.
    pub covered: u32,
    /// Patterns no probe exercises.
    pub uncovered: Vec<UncoveredRoute>,
}

/// The behavioral denominator: obligations, not route hits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BehaviorCounts {
    /// Acceptance, must-keep and ruling obligations that apply.
    pub obligations_total: u32,
    /// Those that passed.
    pub passed: u32,
    /// Those that failed.
    pub failed: u32,
    /// Those with no answer. Never a pass.
    pub unknown: u32,
}

/// How one ruling fared at the gate.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RulingStatus {
    /// Executed, and every checked probe satisfied it. A standing ruling whose covered fields
    /// never differed between base and candidate holds unchecked.
    Held {},
    /// Executed, and the candidate violated it.
    Violated {
        /// The violating items.
        item_ids: NonEmpty<ItemId>,
    },
    /// Executed, but no probe reached its selector. Not counted as held.
    NotReached {},
    /// Its evidence or normalization version no longer matches. Never a pass.
    Stale {},
    /// Reached and current, but it could not be evaluated: evidence was incomplete or the
    /// evaluator failed. Never a pass, and a required ruling in this state blocks land.
    Unknown {
        /// Why.
        reason: UnknownReason,
    },
    /// A note. Never counted as a check.
    NotEnforced {
        /// Why.
        reason: NotEnforcedReason,
    },
}

impl RulingStatus {
    /// Whether the ruling held. Only [`RulingStatus::Held`] does; every other status blocks a
    /// required check.
    #[must_use]
    pub fn held(&self) -> bool {
        match self {
            Self::Held {} => true,
            Self::Violated { .. }
            | Self::NotReached {}
            | Self::Stale {}
            | Self::Unknown { .. }
            | Self::NotEnforced { .. } => false,
        }
    }
}

/// One ruling's line on the receipt.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RulingCheck {
    /// The ruling lineage.
    pub ruling_id: RulingId,
    /// The decision in force.
    pub event_seq: u64,
    /// The tag label.
    pub label: String,
    /// The machines it is fixed to.
    pub machine_ids: NonEmpty<MachineId>,
    /// How it fared.
    pub status: RulingStatus,
}

/// A scope item or must-keep rule that no probe reached. It has no receipt item, because a receipt
/// item needs the probe that checked it, and it carries no probe hash for the same reason. Its
/// obligations count as unknown in [`BehaviorCounts`], never as passed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotReached {
    /// The scope item or must-keep rule.
    pub item_id: ItemId,
    /// Its canonical episode selector, as on [`ReceiptItem::selector`].
    pub selector: Selector,
    /// The obligations no probe checked.
    pub obligation_ids: Vec<ObligationId>,
}

/// The receipt of one candidate. Core-derived: every class, count and denominator is a pure
/// function of observations, masks, the scope and the expectations.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    /// Always `acan-receipt/1`.
    pub version: ReceiptV1,
    /// The order.
    pub order: OrderRef,
    /// The confirmed scope revision.
    pub scope_rev: ScopeRev,
    /// Its hash.
    pub scope_hash: ScopeHash,
    /// The base, overlay and tested context. The receipt is void for any other merged commit.
    pub prepared: PreparedCommit,
    /// Where the gate is.
    pub gate_state: GateState,
    /// Every classified item.
    pub items: Vec<ReceiptItem>,
    /// Items per class, with the total as denominator.
    pub counts: ClassCounts,
    /// The probe denominator.
    pub probes: ProbeCounts,
    /// The route denominator.
    pub routes: RouteCounts,
    /// The behavioral denominator.
    pub behavior: BehaviorCounts,
    /// Every mask applied, each bound to the two base observations that derived it.
    pub masks: Vec<Mask>,
    /// Every ruling, enforced or not.
    pub rulings: Vec<RulingCheck>,
    /// Every contest filed in the order.
    pub contests: Vec<Contest>,
    /// Scope items and must-keep rules no probe reached, each with its unchecked obligations. They
    /// count as unknown in [`Receipt::behavior`]; this list names them by item id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_reached: Vec<NotReached>,
}

impl Receipt {
    /// The receipt's identity, which a plant view and a tour bind to.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn receipt_hash(&self) -> Result<ReceiptHash, CanonicalError> {
        ContentHash::of(self).map(ReceiptHash::from)
    }

    /// The receipt's semantic identity: the receipt with model prose left out. The scope enters
    /// as its [`Scope::semantic_scope_hash`], so wording does not move it, and each contest's
    /// `rationale` is the only prose it leaves out. `scope` must be the scope this receipt was
    /// built from.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn semantic_hash(&self, scope: &Scope) -> Result<ReceiptHash, CanonicalError> {
        let view = SemanticReceipt::new(self, scope.semantic_scope_hash()?);
        ContentHash::of(&view).map(ReceiptHash::from)
    }

    /// The tested merged commit.
    #[must_use]
    pub fn merged_commit(&self) -> &CommitSha {
        &self.prepared.context.merged_commit
    }

    /// The order's base commit.
    #[must_use]
    pub fn base_commit(&self) -> &CommitSha {
        &self.prepared.base_commit
    }

    /// Checks that every item's episode id is the hash of its [`EpisodeKey`]: the order, the
    /// item's selector, and either no obligation or one of its failing obligations. An item with
    /// no selector has no key to check.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] for the first item whose episode id matches no such key.
    pub fn check_episode_ids(&self) -> Result<(), InvalidValue> {
        for item in &self.items {
            let (Some(episode), Some(selector)) = (item.episode_id, item.selector.as_ref()) else {
                continue;
            };
            let failing = item
                .evaluation
                .obligations
                .iter()
                .filter(|check| matches!(check.result, CheckResult::Fail { .. }))
                .map(|check| Some(check.obligation_id.clone()));
            let matched = std::iter::once(None).chain(failing).any(|obligation_id| {
                EpisodeKey {
                    order: self.order.clone(),
                    selector: selector.clone(),
                    obligation_id,
                }
                .episode_id()
                .is_ok_and(|id| id == episode)
            });
            if !matched {
                return Err(InvalidValue::Malformed {
                    what: "episode id",
                    raw: item.item_id.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Checks that `counts` are exactly the classes of `items`. Every reader of a stored receipt
    /// calls this before trusting a count or a total.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] when they differ.
    pub fn check_counts(&self) -> Result<(), InvalidValue> {
        if self
            .counts
            .matches(self.items.iter().map(|item| item.class))
        {
            Ok(())
        } else {
            Err(mismatched_counts(&self.counts, self.items.len()))
        }
    }
}

/// The receipt's semantic content, digested by [`Receipt::semantic_hash`]. Each projection
/// destructures its struct, so a new field fails to compile until it is digested or named as
/// prose there.
#[derive(Serialize)]
struct SemanticReceipt<'a> {
    version: &'a ReceiptV1,
    order: &'a OrderRef,
    scope_rev: &'a ScopeRev,
    scope_hash: ScopeHash,
    prepared: &'a PreparedCommit,
    gate_state: &'a GateState,
    items: &'a [ReceiptItem],
    counts: &'a ClassCounts,
    probes: &'a ProbeCounts,
    routes: &'a RouteCounts,
    behavior: &'a BehaviorCounts,
    masks: &'a [Mask],
    rulings: &'a [RulingCheck],
    contests: Vec<SemanticContest<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    not_reached: Option<&'a [NotReached]>,
}

/// A contest without its `rationale`, the model's own reason for filing it.
#[derive(Serialize)]
struct SemanticContest<'a> {
    version: &'a ContestV1,
    id: &'a ContestId,
    order: &'a OrderRef,
    agent: &'a AgentRef,
    task: &'a TaskRef,
    target: &'a EpisodeId,
    kind: &'a ContestKind,
    reproducer: &'a ProbeHash,
    status: &'a ContestStatus,
    filed_at: &'a UnixMillis,
}

impl<'a> SemanticReceipt<'a> {
    fn new(receipt: &'a Receipt, scope_hash: ScopeHash) -> Self {
        let Receipt {
            version,
            order,
            scope_rev,
            scope_hash: _,
            prepared,
            gate_state,
            items,
            counts,
            probes,
            routes,
            behavior,
            masks,
            rulings,
            contests,
            not_reached,
        } = receipt;
        Self {
            version,
            order,
            scope_rev,
            scope_hash,
            prepared,
            gate_state,
            items,
            counts,
            probes,
            routes,
            behavior,
            masks,
            rulings,
            contests: contests.iter().map(SemanticContest::from).collect(),
            not_reached: (!not_reached.is_empty()).then_some(not_reached.as_slice()),
        }
    }
}

impl<'a> From<&'a Contest> for SemanticContest<'a> {
    fn from(contest: &'a Contest) -> Self {
        let Contest {
            version,
            id,
            order,
            agent,
            task,
            target,
            kind,
            reproducer,
            rationale: _,
            status,
            filed_at,
        } = contest;
        Self {
            version,
            id,
            order,
            agent,
            task,
            target,
            kind,
            reproducer,
            status,
            filed_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::{Value, json};

    use super::*;

    fn receipt() -> Value {
        serde_json::from_str(include_str!("../fixtures/receipt.json")).expect("the fixture")
    }

    #[test]
    fn decoded_counts_near_the_limit_neither_panic_nor_pass() {
        let mut value = receipt();
        for class in ["preserved", "error", "out_of_scope"] {
            value["counts"][class] = json!(u32::MAX);
        }
        let tampered: Receipt = serde_json::from_value(value).expect("decoding");
        assert!(tampered.counts.total() > u64::from(u32::MAX));
        assert!(tampered.check_counts().is_err());
        let honest: Receipt = serde_json::from_value(receipt()).expect("decoding");
        assert_eq!(honest.check_counts(), Ok(()));
    }

    #[test]
    fn counts_must_match_the_items_not_just_the_total() {
        let mut value = receipt();
        let preserved = value["counts"]["preserved"].as_u64().expect("a count");
        let error = value["counts"]["error"].as_u64().expect("a count");
        value["counts"]["preserved"] = json!(preserved + 1);
        value["counts"]["error"] = json!(error - 1);
        let shifted: Receipt = serde_json::from_value(value).expect("decoding");
        assert!(shifted.check_counts().is_err());
    }

    #[test]
    fn only_a_held_ruling_holds() {
        let statuses: Vec<RulingStatus> = [
            json!({ "status": "held" }),
            json!({ "status": "violated", "item_ids": ["i-1"] }),
            json!({ "status": "not_reached" }),
            json!({ "status": "stale" }),
            json!({ "status": "unknown", "reason": { "reason": "incomplete" } }),
            json!({ "status": "not_enforced", "reason": { "kind": "deferred" } }),
        ]
        .into_iter()
        .map(|value| serde_json::from_value(value).expect("a status"))
        .collect();
        let held: Vec<bool> = statuses.iter().map(RulingStatus::held).collect();
        assert_eq!(held, [true, false, false, false, false, false]);
    }

    #[test]
    fn the_receipt_fixture_has_an_unknown_ruling() {
        let receipt: Receipt = serde_json::from_value(receipt()).expect("decoding");
        assert!(
            receipt
                .rulings
                .iter()
                .any(|check| matches!(check.status, RulingStatus::Unknown { .. }))
        );
    }

    #[test]
    fn evidence_rows_keep_missing_apart_from_null() {
        let row = |value: Value| {
            serde_json::from_value::<EvidenceRow>(json!({
                "slot": "candidate", "run_id": "run-1",
                "commit": "0123456789abcdef0123456789abcdef01234567",
                "observation_hash": ContentHash::of_bytes(b"o").to_string(),
                "step": "s1", "field": "header.retry-after", "value": value,
                "provenance": "observed"
            }))
            .expect("a row")
        };
        let missing = row(json!({ "state": "missing" }));
        let null = row(json!({ "state": "present", "value": null }));
        let empty = row(json!({ "state": "present", "value": [] }));
        assert_eq!(missing.value, Evidence::Missing {});
        assert_ne!(missing.value, null.value);
        assert_eq!(empty.value, Evidence::Present { value: json!([]) });
        assert!(missing.address().is_some());
    }

    #[test]
    fn the_semantic_view_names_every_receipt_field_but_contest_prose() {
        let value = receipt();
        let typed: Receipt = serde_json::from_value(value.clone()).expect("decoding");
        let view =
            serde_json::to_value(SemanticReceipt::new(&typed, typed.scope_hash)).expect("encoding");
        let keys = |object: &Value| -> BTreeSet<String> {
            object
                .as_object()
                .expect("an object")
                .keys()
                .cloned()
                .collect()
        };
        let mut contest = keys(&value["contests"][0]);
        contest.remove("rationale");
        assert_eq!(
            keys(&view),
            keys(&value),
            "the view names every receipt field"
        );
        assert_eq!(
            keys(&view["contests"][0]),
            contest,
            "each contest leaves out only its rationale"
        );
    }
}
