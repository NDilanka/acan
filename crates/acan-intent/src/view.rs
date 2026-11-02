//! The plant view, `plant-view/1`: what the UI draws for one receipt snapshot.
//!
//! Trusted code builds every fact here from the receipt. The optional [`Tour`] only orders
//! camera moves over those facts: it refers to items, marks and evidence that exist in the
//! snapshot, binds caption slots to evidence references, and carries no literal values. Trusted
//! code rejects a whole tour on any invalid reference and the UI falls back to the plain card.
//!
//! L0 marks, one per fact: may-change items draw footprints, must-keep rules pins, open choices
//! question pins, machines with `coverage: uncovered` a `no probes` chip, evidence slots the
//! ghost base, `route_path` the belt crates tinted by `sequence`, `state: bounced` the
//! `bounced attempts/budget` chip, `state: residue` the amber card, `class: violates_ruling` the
//! flashing tag of `ruling_id`, enforced rulings brass tags, not-enforced rulings hollow tags,
//! unknown classes a grey mark, and `gate_state` with `merged_sha` the order plate.
//!
//! Ownership: R01 builds the plant view and every [`Card`] deterministically from one receipt;
//! API01 serves them; T01 admits tours; the UI only decodes. Transport, frozen here:
//! `GET /api/orders/{order}/plant-view` answers the order's latest [`PlantView`], and
//! `GET /api/receipts/{receipt_hash}/cards/{item_id}` answers one [`Card`] ([`CardRequest`] names
//! the same two keys). A card is addressed by the receipt hash, so it never changes after the
//! view that links to it; an unknown receipt or item fails as `not_found` in the
//! [`crate::McpError`] envelope, never as an empty card.
//!
//! The view carries what the plant draws and the totals and denominators no layout may drop.
//! The card carries the rest of the receipt for one item: its evaluation and reasons, every
//! denominator, the applied masks, every ruling with its predicate and examples, the contests,
//! and the full gate context.
//!
//! Tour admission (T01), binding, implemented once by [`crate::admit::tour`]: a proposed tour
//! is admitted only when its receipt hash is the view's, the catalog is the view's
//! [`PlantView::catalog_version`], every step's item, camera machine and staged mark exist in the
//! view (a bounced chip on a bounced item, a residue card on a residue item, a grey mark on an
//! unknown class, a `no probes` chip on an uncovered machine), every evidence reference and
//! caption slot of a step names the step's own item and resolves through
//! [`PlantView::resolve`], every caption names a template of the catalog and binds exactly its
//! slots, every resolved row fits its [`SlotSpec`] ([`SlotSpec::check`]: the declared column, a
//! provenance that column admits, a value of the declared [`ValueType`], and a present value
//! unless the slot is [`SlotPresence::Optional`]), and every mandatory item
//! ([`PlantItem::is_mandatory`]) is reached ([`Tour::reaches`]). A caption's template must also
//! describe its item's class and state ([`CaptionTemplate::fits`]), so a caption never says of an
//! item what the item is not. A cross-item, stale, swapped,
//! mistyped or hypothetical binding rejects the whole tour, and the UI falls back to the
//! deterministic tour and the plain card. Admitted slots become a [`BoundItemView`] whose values
//! are copies of the resolved rows, never the model's text; the card writes each with
//! [`BindableValue`]'s `Display`, which writes a missing value as [`MISSING_VALUE_TEXT`].

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::admit::{MISSING_VALUE_TEXT, SlotMismatch, SlotPresence};
use crate::episode::Contest;
use crate::expr::Predicate;
use crate::gate::PreparedCommit;
use crate::hash::{ContrastExampleHash, EpisodeId, NormalizationHash, ProbeHash, ReceiptHash};
use crate::ledger::Applicability;
use crate::observation::Mask;
use crate::probe::RoutePath;
use crate::report::{
    BehaviorCounts, ClassCounts, EvidenceRow, EvidenceSlot, GateState, ProbeCounts, ReceiptItem,
    RouteCounts, RulingCheck, SequenceStep, mismatched_counts,
};
use crate::scope::{ChoiceOption, ItemProvenance, Selector, TargetKind};
use crate::types::{
    CatalogVersion, ChoiceId, CommitSha, Evidence, InvalidValue, ItemId, MachineId, ModelRef,
    MustKeepId, NonEmpty, ObligationId, ObservationPath, OrderRef, RouteTemplate, RulingId,
    ScopeRev, SlotName, StepId, TemplateId, UnixMillis,
};
use crate::verdict::{ItemClass, ItemState};
use crate::version::{CaptionCatalogV2, CardV1, PlantViewV1};

/// The order on the plate.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlantOrder {
    /// The order.
    pub id: OrderRef,
    /// Its sentence.
    pub sentence: String,
    /// The machine it is pinned on.
    pub machine_id: MachineId,
    /// The confirmed scope revision.
    pub scope_rev: ScopeRev,
    /// When the human confirmed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_at: Option<UnixMillis>,
}

/// How well probes cover a machine.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    /// Every route through it has a probe.
    Covered,
    /// Some do.
    Partial,
    /// None do. Drawn hatched with a `no probes` chip.
    Uncovered,
}

/// Whether a ruling's tag is current.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RulingTagState {
    /// In force.
    Active,
    /// Replaced by a later decision.
    Superseded,
}

/// A ruling's tag on a machine.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RulingTag {
    /// The ruling lineage.
    pub ruling_id: RulingId,
    /// Brass when true, hollow `NOT ENFORCED` when false.
    pub enforced: bool,
    /// Whether it is current.
    pub state: RulingTagState,
    /// The label.
    pub label: String,
}

/// One machine of the plant.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlantMachine {
    /// The machine.
    pub machine_id: MachineId,
    /// Its probe coverage.
    pub coverage: Coverage,
    /// How many corpus probes pass through it.
    pub probes: u32,
    /// The routes whose static path passes through it.
    pub routes: Vec<RouteTemplate>,
    /// Its ruling tags.
    pub rulings: Vec<RulingTag>,
}

/// A scope example as the card shows it: a target, never an observation.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExampleView {
    /// Always `target`.
    pub kind: TargetKind,
    /// What the human expects today, if they said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Evidence>,
    /// What the human wants, or [`Evidence::Missing`] for a removal.
    pub after: Evidence,
}

/// A may-change item's footprint.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeItemView {
    /// The item.
    pub item_id: ItemId,
    /// The machines it covers.
    pub machine_ids: NonEmpty<MachineId>,
    /// Its selector in the core's words.
    pub selector_text: String,
    /// Its acceptance obligations.
    pub obligations: NonEmpty<ObligationId>,
    /// Where it came from.
    pub provenance: ItemProvenance,
    /// Its example.
    pub example: ExampleView,
}

/// A must-keep pin.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MustKeepView {
    /// The rule.
    pub id: MustKeepId,
    /// The machines it guards.
    pub machine_ids: NonEmpty<MachineId>,
    /// The rule in words.
    pub text: String,
    /// The ruling it restates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruling_id: Option<RulingId>,
}

/// A question pin.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenChoiceView {
    /// The choice.
    pub id: ChoiceId,
    /// Where the pin goes.
    pub machine_id: MachineId,
    /// The question.
    pub question: String,
    /// One key per answer.
    pub options: NonEmpty<ChoiceOption>,
}

/// The confirmed scope's marks.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeView {
    /// The revision, shown as `MAY CHANGE · rev N`.
    pub rev: ScopeRev,
    /// The may-change items.
    pub items: Vec<ScopeItemView>,
    /// The must-keep rules.
    pub must_keep: Vec<MustKeepView>,
    /// The open choices.
    pub open: Vec<OpenChoiceView>,
}

/// One receipt item on the plant.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlantItem {
    /// The item.
    pub item_id: ItemId,
    /// Its class.
    pub class: ItemClass,
    /// The machine its mark goes on.
    pub machine_id: MachineId,
    /// The probe that checked it.
    pub probe_id: ProbeHash,
    /// Its episode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_id: Option<EpisodeId>,
    /// The ruling it violates or relies on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruling_id: Option<RulingId>,
    /// Repair attempts used.
    pub attempts: u32,
    /// Repair attempts allowed.
    pub budget: u32,
    /// Where it stands.
    pub state: ItemState,
    /// The machines its request passes.
    pub route_path: RoutePath,
    /// The sequence strip.
    pub sequence: Vec<SequenceStep>,
    /// The values on the card.
    pub evidence: Vec<EvidenceRow>,
}

impl PlantItem {
    /// Whether every tour must reach this item: a class that does not pass, or a bounced,
    /// residue or stopped item.
    #[must_use]
    pub fn is_mandatory(&self) -> bool {
        !self.class.counts_toward_pass()
            || matches!(
                self.state,
                ItemState::Bounced | ItemState::Residue | ItemState::Stopped
            )
    }
}

/// The plant's counts. Each has its denominator beside it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlantCounts {
    /// Items per class; their total is the item denominator.
    pub by_class: ClassCounts,
    /// Probes run, of `probes_total`.
    pub probes_run: u32,
    /// Probes in the corpus.
    pub probes_total: u32,
    /// Routes hit by a probe, of `routes_total`. A hit is not behavioral coverage.
    pub routes_covered: u32,
    /// Baseline routes.
    pub routes_total: u32,
    /// Masks applied.
    pub masks_applied: u32,
    /// Obligations, the behavioral denominator. Drawn beside the route hits, never merged.
    pub behavior: BehaviorCounts,
}

/// The view of one receipt snapshot.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlantView {
    /// Always `plant-view/1`.
    pub version: PlantViewV1,
    /// The receipt it shows.
    pub receipt_hash: ReceiptHash,
    /// The order's base commit.
    pub base_sha: CommitSha,
    /// The tested merged commit.
    pub merged_sha: CommitSha,
    /// Where the gate is.
    pub gate_state: GateState,
    /// The order.
    pub order: PlantOrder,
    /// Every machine.
    pub machines: Vec<PlantMachine>,
    /// The confirmed scope.
    pub scope: ScopeView,
    /// Every receipt item.
    pub items: Vec<PlantItem>,
    /// The counts.
    pub counts: PlantCounts,
    /// The caption catalog tours are checked against.
    pub catalog_version: CatalogVersion,
    /// The tour, if one was made and validated. Absent means the plain card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tour: Option<TourView>,
}

impl PlantView {
    /// The evidence row `reference` names: the row of item `item_id` with the same slot, step
    /// and field. `None` when there is none, which rejects the tour that used it.
    #[must_use]
    pub fn resolve(&self, reference: &EvidenceRef) -> Option<&EvidenceRow> {
        self.item(&reference.item_id)?.evidence.iter().find(|row| {
            row.slot == reference.slot && row.step == reference.step && row.field == reference.field
        })
    }

    fn item(&self, item_id: &ItemId) -> Option<&PlantItem> {
        self.items.iter().find(|item| &item.item_id == item_id)
    }

    /// The items every tour must reach, in view order.
    pub fn mandatory_items(&self) -> impl Iterator<Item = &PlantItem> {
        self.items.iter().filter(|item| item.is_mandatory())
    }

    /// Checks that the class counts are exactly the classes of the items. The UI calls this
    /// before drawing a count.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] when they differ.
    pub fn check_counts(&self) -> Result<(), InvalidValue> {
        let counts = &self.counts.by_class;
        if counts.matches(self.items.iter().map(|item| item.class)) {
            Ok(())
        } else {
            Err(mismatched_counts(counts, self.items.len()))
        }
    }
}

/// A zoom level of the camera catalog.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ZoomLevel {
    /// The whole plant.
    Fit,
    /// One machine and its neighbours.
    Machine,
    /// One machine up close.
    Close,
}

/// Where the camera goes for one tour step.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Camera {
    /// The machine.
    pub machine_id: MachineId,
    /// The zoom.
    pub zoom: ZoomLevel,
}

/// A mark of the snapshot a tour step stages. Every reference must exist in the snapshot.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "mark", rename_all = "snake_case", deny_unknown_fields)]
pub enum MarkRef {
    /// A may-change footprint.
    Footprint {
        /// The scope item.
        item_id: ItemId,
    },
    /// A must-keep pin.
    MustKeepPin {
        /// The rule.
        must_keep_id: MustKeepId,
    },
    /// A question pin.
    QuestionPin {
        /// The choice.
        choice_id: ChoiceId,
    },
    /// A `no probes` chip.
    NoProbes {
        /// The machine.
        machine_id: MachineId,
    },
    /// The ghost base machine beside an item's machine.
    Ghost {
        /// The receipt item.
        item_id: ItemId,
    },
    /// The crates of an item's sequence on its route path.
    Crates {
        /// The receipt item.
        item_id: ItemId,
    },
    /// An item's `bounced` chip.
    BouncedChip {
        /// The receipt item.
        item_id: ItemId,
    },
    /// An item's amber residue card.
    ResidueCard {
        /// The receipt item.
        item_id: ItemId,
    },
    /// A ruling's tag.
    RulingTag {
        /// The ruling.
        ruling_id: RulingId,
    },
    /// An item's grey unknown mark.
    Unknown {
        /// The receipt item.
        item_id: ItemId,
    },
    /// The order plate.
    OrderPlate {},
}

/// A reference to one evidence row of one item in the snapshot.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    /// The receipt item.
    pub item_id: ItemId,
    /// The column.
    pub slot: EvidenceSlot,
    /// The step.
    pub step: StepId,
    /// The field.
    pub field: ObservationPath,
}

/// A caption from the trusted template catalog, its slots bound to evidence references.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Caption {
    /// The template.
    pub template_id: TemplateId,
    /// Each slot's evidence. Never a literal.
    pub slots: BTreeMap<SlotName, EvidenceRef>,
}

/// One step of a tour.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TourStep {
    /// The receipt item the step is about.
    pub item_id: ItemId,
    /// Where the camera goes.
    pub camera: Camera,
    /// The marks it stages.
    pub stage: Vec<MarkRef>,
    /// The evidence it points at.
    pub evidence_refs: Vec<EvidenceRef>,
    /// The caption.
    pub caption: Caption,
}

/// An ordered walk over one immutable receipt snapshot.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tour {
    /// The snapshot. A tour for any other receipt is rejected.
    pub receipt_hash: ReceiptHash,
    /// The steps. Every mandatory item must be reachable.
    pub steps: NonEmpty<TourStep>,
}

impl Tour {
    /// Whether a step of the tour is about `item_id`.
    #[must_use]
    pub fn reaches(&self, item_id: &ItemId) -> bool {
        self.steps.iter().any(|step| &step.item_id == item_id)
    }
}

/// A validated tour and who ordered it. The UI shows a `directed` or `plain` toggle.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum TourView {
    /// Ordered by the director model and validated by trusted code.
    Directed {
        /// The director.
        model: ModelRef,
        /// The tour.
        tour: Tour,
    },
    /// The deterministic order: residue, then violations, then bounced, then fulfilled.
    Deterministic {
        /// The tour.
        tour: Tour,
    },
}

/// What kind of value a bound slot holds.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    /// An HTTP status code.
    Status,
    /// A count.
    Count,
    /// A duration.
    Duration,
    /// A commit.
    Commit,
    /// An identifier.
    Identifier,
    /// Free text.
    Text,
    /// A number.
    Number,
    /// A boolean.
    Boolean,
    /// A present value of any type, for a caption that names a value without reading it.
    Any,
}

impl ValueType {
    /// Whether a present JSON value is of this type: a status is an integer from 100 to 599, a
    /// count or duration a non-negative integer, a commit a commit hash string, an identifier a
    /// non-empty string, text any string, a number any number, a boolean a boolean, and any
    /// value but `null`. JSON `null` is of no type.
    #[must_use]
    pub fn admits(self, value: &Value) -> bool {
        match self {
            Self::Status => value
                .as_u64()
                .is_some_and(|code| (100..=599).contains(&code)),
            Self::Count | Self::Duration => value.is_u64(),
            Self::Commit => value
                .as_str()
                .is_some_and(|raw| raw.parse::<CommitSha>().is_ok()),
            Self::Identifier => value.as_str().is_some_and(|raw| !raw.is_empty()),
            Self::Text => value.is_string(),
            Self::Number => value.is_number(),
            Self::Boolean => value.is_boolean(),
            Self::Any => !value.is_null(),
        }
    }

    /// Whether values of this type may be shown in `unit`: a duration always in
    /// milliseconds, a count in requests, bytes or no unit, a number in bytes or no unit, and
    /// every other type in no unit.
    #[must_use]
    pub fn admits_unit(self, unit: Option<ValueUnit>) -> bool {
        match self {
            Self::Duration => unit == Some(ValueUnit::Ms),
            Self::Count => matches!(unit, None | Some(ValueUnit::Requests | ValueUnit::Bytes)),
            Self::Number => matches!(unit, None | Some(ValueUnit::Bytes)),
            Self::Status
            | Self::Commit
            | Self::Identifier
            | Self::Text
            | Self::Boolean
            | Self::Any => unit.is_none(),
        }
    }
}

/// The unit a bound value is shown in.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ValueUnit {
    /// Milliseconds.
    Ms,
    /// Requests.
    Requests,
    /// Bytes.
    Bytes,
}

/// A value trusted code resolved from a snapshot for a caption slot: a copy of the evidence
/// row the slot's reference named, with the row's provenance, column and full address. Model
/// layout cannot substitute a proposal for an observation or swap base and candidate, because
/// the row decodes only with a provenance its column admits and an observed row only with its
/// run, commit and observation hash.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BindableValue {
    /// What kind of value, from the template's slot.
    pub value_type: ValueType,
    /// Its unit, from the template's slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<ValueUnit>,
    /// The snapshot.
    pub receipt_hash: ReceiptHash,
    /// The receipt item.
    pub item_id: ItemId,
    /// The resolved row.
    pub row: EvidenceRow,
}

/// The caption text of the value: a string as itself, any other JSON value as JSON, then the
/// unit; a missing value as [`MISSING_VALUE_TEXT`], without a unit.
impl fmt::Display for BindableValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Evidence::Present { value } = &self.row.value else {
            return f.write_str(MISSING_VALUE_TEXT);
        };
        match value {
            Value::String(text) => f.write_str(text)?,
            other => write!(f, "{other}")?,
        }
        match self.unit {
            None => Ok(()),
            Some(ValueUnit::Ms) => f.write_str(" ms"),
            Some(ValueUnit::Requests) => f.write_str(" requests"),
            Some(ValueUnit::Bytes) => f.write_str(" bytes"),
        }
    }
}

/// What one slot of a caption template takes. Decoding refuses a unit its value type is never
/// shown in ([`ValueType::admits_unit`]).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "SlotSpecWire")]
pub struct SlotSpec {
    /// The card column the bound row must come from.
    pub column: EvidenceSlot,
    /// What kind of value.
    pub value_type: ValueType,
    /// Its unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<ValueUnit>,
    /// Whether the row may hold no value.
    pub presence: SlotPresence,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "SlotSpec")]
struct SlotSpecWire {
    column: EvidenceSlot,
    value_type: ValueType,
    #[serde(default)]
    unit: Option<ValueUnit>,
    presence: SlotPresence,
}

impl TryFrom<SlotSpecWire> for SlotSpec {
    type Error = InvalidValue;

    fn try_from(wire: SlotSpecWire) -> Result<Self, Self::Error> {
        if !wire.value_type.admits_unit(wire.unit) {
            return Err(InvalidValue::Malformed {
                what: "slot unit",
                raw: format!("{:?} in {:?}", wire.value_type, wire.unit),
            });
        }
        Ok(Self {
            column: wire.column,
            value_type: wire.value_type,
            unit: wire.unit,
            presence: wire.presence,
        })
    }
}

impl SlotSpec {
    /// Whether `row` may fill this slot. See [`SlotSpec::check`].
    #[must_use]
    pub fn accepts(&self, row: &EvidenceRow) -> bool {
        self.check(row).is_ok()
    }

    /// Checks that `row` may fill this slot: it is in the declared column, its provenance is one
    /// that column admits, and it holds a value of the declared type, or no value when the slot
    /// is optional. A row from another column is a swap; a target or prediction in a run
    /// column is a hypothetical.
    ///
    /// # Errors
    ///
    /// Returns the first [`SlotMismatch`].
    pub fn check(&self, row: &EvidenceRow) -> Result<(), SlotMismatch> {
        if row.slot != self.column {
            return Err(SlotMismatch::Column);
        }
        if !self.column.admits(row.provenance) {
            return Err(SlotMismatch::Provenance);
        }
        match &row.value {
            Evidence::Present { value } if self.value_type.admits(value) => Ok(()),
            Evidence::Present { .. } => Err(SlotMismatch::Type),
            Evidence::Missing {} if self.presence == SlotPresence::Optional => Ok(()),
            Evidence::Missing {} => Err(SlotMismatch::Missing),
        }
    }
}

/// One caption template of the trusted catalog.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptionTemplate {
    /// The template.
    pub template_id: TemplateId,
    /// The text, with `{slot}` placeholders the trusted card fills.
    pub text: String,
    /// The item classes the text describes. A caption on an item of another class is refused.
    pub classes: Vec<ItemClass>,
    /// The item states the text describes. A caption on an item in another state is refused.
    pub states: Vec<ItemState>,
    /// Every slot, each of which a caption must bind.
    pub slots: BTreeMap<SlotName, SlotSpec>,
}

impl CaptionTemplate {
    /// Whether the text describes `item`: its class and its state are both among the template's.
    #[must_use]
    pub fn fits(&self, item: &PlantItem) -> bool {
        self.classes.contains(&item.class) && self.states.contains(&item.state)
    }
}

/// The trusted caption catalog, shipped with the server and the UI.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptionCatalog {
    /// Always `acan-caption-catalog/2`.
    pub version: CaptionCatalogV2,
    /// The catalog's version, which a plant view names.
    pub catalog_version: CatalogVersion,
    /// The templates.
    pub templates: Vec<CaptionTemplate>,
}

/// The catalog this build ships. The server and the page admit tours against this copy.
const TRUSTED_CATALOG: &str = include_str!("../fixtures/caption-catalog.json");

impl CaptionCatalog {
    /// The trusted catalog, decoded from the copy this crate ships.
    ///
    /// # Errors
    ///
    /// Returns the decoder's error when the shipped copy does not parse.
    pub fn trusted() -> Result<Self, serde_json::Error> {
        serde_json::from_str(TRUSTED_CATALOG)
    }

    /// The template `template_id` names.
    #[must_use]
    pub fn template(&self, template_id: &TemplateId) -> Option<&CaptionTemplate> {
        self.templates
            .iter()
            .find(|template| &template.template_id == template_id)
    }
}

/// One item's caption slots bound by trusted code. Rendered by the trusted card; any invalid
/// binding rejects the whole view.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundItemView {
    /// The snapshot.
    pub receipt_hash: ReceiptHash,
    /// The receipt item.
    pub item_id: ItemId,
    /// Each slot's value.
    pub typed_slot_bindings: BTreeMap<SlotName, BindableValue>,
    /// The template catalog version.
    pub catalog_version: CatalogVersion,
}

/// `GET /api/receipts/{receipt_hash}/cards/{item_id}`: one item's card.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CardRequest {
    /// Always `acan-card/1`.
    pub version: CardV1,
    /// The snapshot.
    pub receipt_hash: ReceiptHash,
    /// The receipt item.
    pub item_id: ItemId,
}

/// An executable decision as the card shows it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DecisionDetail {
    /// Where it applies.
    pub selector: Selector,
    /// What must hold.
    pub predicate: Predicate,
    /// The examples it accepts.
    pub accepts: Vec<ContrastExampleHash>,
    /// The examples it rejects.
    pub rejects: Vec<ContrastExampleHash>,
    /// The example it was ruled on.
    pub ruled_on: ContrastExampleHash,
    /// How far it reaches.
    pub applicability: Applicability,
    /// The normalization plan it was confirmed under.
    pub normalization_hash: NormalizationHash,
}

/// One ruling on a card: how it fared and, for an executable one, what it is.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RulingDetail {
    /// The receipt's line for it.
    pub check: RulingCheck,
    /// The decision in force. Absent for a note, whose reason is in `check`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionDetail>,
}

/// Everything the card shows for one receipt item. Built by R01 from the receipt and the folded
/// log alone; every value is the receipt's own, never recomputed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Card {
    /// Always `acan-card/1`.
    pub version: CardV1,
    /// The snapshot.
    pub receipt_hash: ReceiptHash,
    /// The receipt's item, with its evaluation, reasons, sequence and evidence.
    pub item: ReceiptItem,
    /// Items per class.
    pub counts: ClassCounts,
    /// The probe denominator.
    pub probes: ProbeCounts,
    /// The route denominator.
    pub routes: RouteCounts,
    /// The behavioral denominator.
    pub behavior: BehaviorCounts,
    /// The masks applied to the item's probe.
    pub masks: Vec<Mask>,
    /// The rulings the item violates or relies on, and every ruling on its machine.
    pub rulings: Vec<RulingDetail>,
    /// The contests filed against the item's episode.
    pub contests: Vec<Contest>,
    /// The base, overlay and full tested context.
    pub prepared: PreparedCommit,
}
