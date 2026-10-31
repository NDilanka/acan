//! Scopes: what an order may change, what it must keep, and the human confirmation that binds it.
//!
//! Precedence, highest first: active rulings, then must-keep obligations, then may-change items.
//! A may-change item never licenses a difference that a ruling or a must-keep obligation rejects.
//! Permission is never a pass: an authorized difference passes only through its acceptance
//! obligations.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};

use crate::expr::Predicate;
use crate::hash::{CanonicalError, ContentHash, ExampleHash, ScopeHash, SelectorHash};
use crate::provenance::ModelProposal;
use crate::types::{
    ActorName, Channel, ChoiceId, CommitSha, Evidence, FieldPattern, HumanRef, InvalidValue,
    ItemId, MachineId, MethodSelector, MustKeepId, NonEmpty, ObligationId, ObservationPath,
    OptionKey, OrderRef, RouteTemplate, RulingId, ScopeRev, StepId, Surface, UnixMillis, bounded,
};
use crate::version::{ScopeConfirmationV1, ScopeDraftV1, ScopeV1};

/// Which step of a probe sequence a selector applies to.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepContext {
    /// Every step.
    Any {},
    /// One step.
    Step {
        /// The step.
        step_id: StepId,
    },
}

/// Whose request a selector applies to.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Principal {
    /// Anyone.
    Any {},
    /// Requests without a session.
    Anonymous {},
    /// Requests with any session.
    Authenticated {},
    /// Requests sent as one fixture actor.
    Actor {
        /// The actor.
        name: ActorName,
    },
}

/// Where in the app a scope item, must-keep rule or ruling applies.
///
/// `field_path` is relative to `channel` and absent for `status` and `any`. A header or cookie
/// path is the single lowercase name. A `side_effect` path starts with the sink
/// (`store.users[*].name`, `outbound_http`) and is required. Decoding refuses any other mix.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(try_from = "SelectorWire")]
pub struct Selector {
    /// The interface kind.
    pub surface: Surface,
    /// The method.
    pub method: MethodSelector,
    /// The route pattern.
    pub route_template: RouteTemplate,
    /// Which steps.
    pub step_context: StepContext,
    /// Whose requests.
    pub principal: Principal,
    /// Which part of the response.
    pub channel: Channel,
    /// Which field within the channel, or the whole channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_path: Option<FieldPattern>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Selector")]
struct SelectorWire {
    surface: Surface,
    method: MethodSelector,
    route_template: RouteTemplate,
    step_context: StepContext,
    principal: Principal,
    channel: Channel,
    #[serde(default)]
    field_path: Option<FieldPattern>,
}

impl TryFrom<SelectorWire> for Selector {
    type Error = InvalidValue;

    fn try_from(wire: SelectorWire) -> Result<Self, Self::Error> {
        let path = wire.field_path.as_ref().map(FieldPattern::as_str);
        let consistent = match wire.channel {
            Channel::Status | Channel::Any => path.is_none(),
            Channel::Header | Channel::Cookie => path.is_none_or(|p| !p.contains(['.', '['])),
            Channel::Body => true,
            Channel::SideEffect => path.is_some_and(|p| {
                let sink = p.split(['.', '[']).next().unwrap_or_default();
                matches!(sink, "store" | "outbound_http")
            }),
        };
        if !consistent {
            return Err(InvalidValue::Malformed {
                what: "selector",
                raw: format!("channel {:?} with field_path {path:?}", wire.channel),
            });
        }
        Ok(Self {
            surface: wire.surface,
            method: wire.method,
            route_template: wire.route_template,
            step_context: wire.step_context,
            principal: wire.principal,
            channel: wire.channel,
            field_path: wire.field_path,
        })
    }
}

impl JsonSchema for Selector {
    fn schema_name() -> Cow<'static, str> {
        "Selector".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        SelectorWire::json_schema(generator)
    }
}

impl Selector {
    /// The selector's identity, bound into a scope confirmation.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails, which a valid selector never does.
    pub fn selector_hash(&self) -> Result<SelectorHash, CanonicalError> {
        ContentHash::of(self).map(SelectorHash::from)
    }

    /// Whether the selector is a broad grant that needs its own acknowledgement: any method,
    /// any step, any principal, or any channel.
    #[must_use]
    pub fn is_broad(&self) -> bool {
        self.method == MethodSelector::Any
            || self.step_context == StepContext::Any {}
            || self.principal == Principal::Any {}
            || self.channel == Channel::Any
    }
}

/// What a may-change item does to the selected behavior.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// New behavior where there was none.
    Add,
    /// Different behavior.
    Modify,
    /// Behavior that goes away.
    Remove,
}

/// Where a scope item came from. Drawn as a chip on the card.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ItemProvenance {
    /// The user's own requirement.
    User,
    /// An existing ruling.
    Ruling,
    /// The model's interpretation of the order. Needs the human's confirmation like the rest.
    Model,
    /// An open choice the human answered.
    Open,
}

/// One acceptance obligation: a predicate the change must satisfy.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PredicateRef {
    /// The obligation.
    pub obligation_id: ObligationId,
    /// The predicate, evaluated by the core at the gate.
    pub predicate: Predicate,
    /// One line for the card.
    pub text: String,
}

/// Marks an example as a desired target. A pre-code example is never an observation.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Debug,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// A desired target.
    #[default]
    Target,
}

/// An example of the intended behavior, written before any code exists.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetExample {
    /// Always `target`.
    pub kind: TargetKind,
    /// The probe step the example describes.
    pub step_id: StepId,
    /// The field.
    pub path: ObservationPath,
    /// What the human expects at the field today, if they said: a value with its structure, or
    /// [`Evidence::Missing`] for a field that should not exist yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Evidence>,
    /// What the human wants there after the change, or [`Evidence::Missing`] for a removal.
    pub after: Evidence,
}

impl TargetExample {
    /// The example's identity, bound into a scope confirmation.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn example_hash(&self) -> Result<ExampleHash, CanonicalError> {
        ContentHash::of(self).map(ExampleHash::from)
    }
}

/// One thing an order may change.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeItem {
    /// The item.
    pub id: ItemId,
    /// Where it applies.
    pub selector: Selector,
    /// What kind of change.
    pub change: Change,
    /// What the change must satisfy. Never empty: permission alone is never a pass.
    pub acceptance: NonEmpty<PredicateRef>,
    /// Where the item came from.
    pub provenance: ItemProvenance,
    /// The intended behavior, as a target.
    pub example: TargetExample,
    /// The plant machines it touches.
    pub machine_ids: NonEmpty<MachineId>,
}

/// Behavior an order must keep. Takes precedence over every may-change item.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MustKeep {
    /// The rule.
    pub id: MustKeepId,
    /// Where it applies.
    pub selector: Selector,
    /// The obligations that express it.
    pub obligations: NonEmpty<PredicateRef>,
    /// The rule in words, for the pin and the card.
    pub text: String,
    /// The ruling it restates, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruling_id: Option<RulingId>,
    /// The plant machines it guards.
    pub machine_ids: NonEmpty<MachineId>,
}

/// One answer to an open choice.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChoiceOption {
    /// The answer key.
    pub key: OptionKey,
    /// The answer in words.
    pub label: String,
}

/// A question the scope cannot settle. A human must answer it before confirming.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenChoice {
    /// The choice.
    pub id: ChoiceId,
    /// Where the question pin goes.
    pub machine_id: MachineId,
    /// The question.
    pub question: String,
    /// The answers, one key each.
    pub options: NonEmpty<ChoiceOption>,
}

/// The body of a scope: may-change items, must-keep rules, and open choices.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeBody {
    /// What the order may change.
    pub may: Vec<ScopeItem>,
    /// What it must keep.
    pub must_keep: Vec<MustKeep>,
    /// Questions for the human.
    pub open: Vec<OpenChoice>,
}

/// The foreman's scope draft for one order. Every part of it is a model proposal.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeDraft {
    /// Always `acan-scope-draft/1`.
    pub version: ScopeDraftV1,
    /// The order.
    pub order: OrderRef,
    /// The revision this draft proposes.
    pub rev: ScopeRev,
    /// The commit the draft was written against.
    pub base_commit: CommitSha,
    /// The order's sentence.
    pub sentence: String,
    /// The machine the order is pinned on.
    pub machine_id: MachineId,
    /// The proposed body.
    pub draft: ModelProposal<ScopeBody>,
    /// When the foreman returned it.
    pub drafted_at: UnixMillis,
}

/// A scope revision as the human will confirm it. Its hash is the [`ScopeHash`].
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// Always `acan-scope/1`.
    pub version: ScopeV1,
    /// The order. A confirmed scope is per order; later orders inherit the resulting
    /// contract, never the permission.
    pub order: OrderRef,
    /// The revision.
    pub rev: ScopeRev,
    /// The commit the scope was confirmed against.
    pub base_commit: CommitSha,
    /// The body.
    pub body: ScopeBody,
}

impl Scope {
    /// The scope's document identity, which a confirmation binds. It covers every byte of the
    /// document, wording included.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn scope_hash(&self) -> Result<ScopeHash, CanonicalError> {
        ContentHash::of(self).map(ScopeHash::from)
    }

    /// The scope's executable identity: every field a selector, acceptance predicate, must-keep
    /// obligation, change kind or open choice reads. It leaves out exactly the presentation
    /// fields, [`PredicateRef::text`], [`MustKeep::text`], [`OpenChoice::question`] and
    /// [`ChoiceOption::label`]. Each projection destructures its struct, so a new field fails to
    /// compile until it is digested or named there. Answers to open choices live on the
    /// confirmation, not in the scope.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn semantic_scope_hash(&self) -> Result<ScopeHash, CanonicalError> {
        ContentHash::of(&SemanticScope::from(self)).map(ScopeHash::from)
    }
}

/// The executable content of a [`Scope`], digested by [`Scope::semantic_scope_hash`].
#[derive(Serialize)]
struct SemanticScope<'a> {
    version: &'a ScopeV1,
    order: &'a OrderRef,
    rev: ScopeRev,
    base_commit: &'a CommitSha,
    may: Vec<SemanticItem<'a>>,
    must_keep: Vec<SemanticRule<'a>>,
    open: Vec<SemanticChoice<'a>>,
}

#[derive(Serialize)]
struct SemanticItem<'a> {
    id: &'a ItemId,
    selector: &'a Selector,
    change: Change,
    acceptance: Vec<SemanticObligation<'a>>,
    provenance: ItemProvenance,
    example: &'a TargetExample,
    machine_ids: &'a NonEmpty<MachineId>,
}

#[derive(Serialize)]
struct SemanticRule<'a> {
    id: &'a MustKeepId,
    selector: &'a Selector,
    obligations: Vec<SemanticObligation<'a>>,
    ruling_id: Option<&'a RulingId>,
    machine_ids: &'a NonEmpty<MachineId>,
}

#[derive(Serialize)]
struct SemanticObligation<'a> {
    obligation_id: &'a ObligationId,
    predicate: &'a Predicate,
}

#[derive(Serialize)]
struct SemanticChoice<'a> {
    id: &'a ChoiceId,
    machine_id: &'a MachineId,
    options: Vec<&'a OptionKey>,
}

impl<'a> From<&'a Scope> for SemanticScope<'a> {
    fn from(scope: &'a Scope) -> Self {
        let Scope {
            version,
            order,
            rev,
            base_commit,
            body:
                ScopeBody {
                    may,
                    must_keep,
                    open,
                },
        } = scope;
        Self {
            version,
            order,
            rev: *rev,
            base_commit,
            may: may.iter().map(SemanticItem::from).collect(),
            must_keep: must_keep.iter().map(SemanticRule::from).collect(),
            open: open.iter().map(SemanticChoice::from).collect(),
        }
    }
}

impl<'a> From<&'a ScopeItem> for SemanticItem<'a> {
    fn from(item: &'a ScopeItem) -> Self {
        let ScopeItem {
            id,
            selector,
            change,
            acceptance,
            provenance,
            example,
            machine_ids,
        } = item;
        Self {
            id,
            selector,
            change: *change,
            acceptance: acceptance.iter().map(SemanticObligation::from).collect(),
            provenance: *provenance,
            example,
            machine_ids,
        }
    }
}

impl<'a> From<&'a MustKeep> for SemanticRule<'a> {
    fn from(rule: &'a MustKeep) -> Self {
        let MustKeep {
            id,
            selector,
            obligations,
            text: _,
            ruling_id,
            machine_ids,
        } = rule;
        Self {
            id,
            selector,
            obligations: obligations.iter().map(SemanticObligation::from).collect(),
            ruling_id: ruling_id.as_ref(),
            machine_ids,
        }
    }
}

impl<'a> From<&'a PredicateRef> for SemanticObligation<'a> {
    fn from(obligation: &'a PredicateRef) -> Self {
        let PredicateRef {
            obligation_id,
            predicate,
            text: _,
        } = obligation;
        Self {
            obligation_id,
            predicate,
        }
    }
}

impl<'a> From<&'a OpenChoice> for SemanticChoice<'a> {
    fn from(choice: &'a OpenChoice) -> Self {
        let OpenChoice {
            id,
            machine_id,
            question: _,
            options,
        } = choice;
        Self {
            id,
            machine_id,
            options: options
                .iter()
                .map(|ChoiceOption { key, label: _ }| key)
                .collect(),
        }
    }
}

/// The ruling version a confirmation was made against.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RulingVersion {
    /// The ruling.
    pub ruling_id: RulingId,
    /// Its latest decision's ledger sequence number.
    pub event_seq: u64,
}

/// The human's answer to one open choice.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChoiceAnswer {
    /// The choice.
    pub choice_id: ChoiceId,
    /// The chosen answer.
    pub option: OptionKey,
}

/// A separate acknowledgement of one broad may-change item (see [`Selector::is_broad`]).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BroadGrantAck {
    /// The broad item.
    pub item_id: ItemId,
    /// Its selector, so widening it voids the acknowledgement.
    pub selector_hash: SelectorHash,
    /// Who acknowledged it.
    pub by: HumanRef,
    /// When.
    pub at: UnixMillis,
}

bounded!(
    /// Repair rounds one order may use: at most eight in v1. The offered default is two.
    RepairRounds(u32), 0 ..= 8, "repair rounds per order"
);

/// The repair budget of one order, fixed before the first run.
///
/// Rounds are counted per order, never per episode: see [`crate::RepairRound`]. Retries,
/// new commits, new workers and new episodes never replenish a counter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepairBudget {
    /// Batched repair rounds for the whole order. Each round bounces every open episode at once
    /// and is answered by one candidate and one full-corpus run.
    pub repair_rounds: RepairRounds,
    /// Wall time cap for the whole order.
    pub wall_time_ms: u64,
    /// Model cost cap for the whole order, in millionths of a US dollar.
    pub model_cost_micros: u64,
    /// Cap on new discrepancy episodes the order may open, so new items cannot dodge the budget.
    pub max_episodes: u32,
}

/// One human confirmation of one scope revision. Any widening voids it.
///
/// It binds the scope revision and hash, the base commit, the ruling versions, every selector
/// and example hash, the answers to open choices, the broad-grant acknowledgements, and the
/// repair budget.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeConfirmation {
    /// Always `acan-scope-confirmation/1`.
    pub version: ScopeConfirmationV1,
    /// The order.
    pub order: OrderRef,
    /// The confirmed revision.
    pub scope_rev: ScopeRev,
    /// The hash of the confirmed [`Scope`].
    pub scope_hash: ScopeHash,
    /// The [`Scope::semantic_scope_hash`] of the confirmed scope. A receipt accepts only a scope
    /// whose executable content matches it, and accepts a wording edit. A confirmation without it
    /// predates the semantic digest and is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_scope_hash: Option<ScopeHash>,
    /// The commit the scope was confirmed against.
    pub base_commit: CommitSha,
    /// The ruling versions in force.
    pub ruling_versions: Vec<RulingVersion>,
    /// The hash of every selector in the scope, in scope order.
    pub selector_hashes: Vec<SelectorHash>,
    /// The hash of every example in the scope, in scope order.
    pub example_hashes: Vec<ExampleHash>,
    /// One answer per open choice.
    pub answers: Vec<ChoiceAnswer>,
    /// One acknowledgement per broad item.
    pub broad_grants: Vec<BroadGrantAck>,
    /// The repair budget.
    pub budget: RepairBudget,
    /// The confirming human. Never an agent or a model.
    pub by: HumanRef,
    /// When.
    pub at: UnixMillis,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn selector(method: &str, step: serde_json::Value, principal: &str, channel: &str) -> Selector {
        let mut value = json!({
            "surface": "http", "method": method, "route_template": "/api/login",
            "step_context": step, "principal": { "kind": principal }, "channel": channel
        });
        if channel == "body" {
            value["field_path"] = json!("error");
        }
        serde_json::from_value(value).expect("a selector")
    }

    #[test]
    fn each_wildcard_alone_makes_a_selector_broad() {
        let one_step = json!({ "kind": "step", "step_id": "s1" });
        let any_step = json!({ "kind": "any" });
        let table = [
            (
                selector("POST", one_step.clone(), "anonymous", "body"),
                false,
            ),
            (selector("ANY", one_step.clone(), "anonymous", "body"), true),
            (
                selector("POST", any_step.clone(), "anonymous", "body"),
                true,
            ),
            (selector("POST", one_step.clone(), "any", "body"), true),
            (selector("POST", one_step, "anonymous", "any"), true),
            (selector("ANY", any_step, "any", "any"), true),
        ];
        for (index, (selector, broad)) in table.iter().enumerate() {
            assert_eq!(selector.is_broad(), *broad, "row {index}");
        }
    }

    #[test]
    fn an_order_has_at_most_eight_repair_rounds() {
        let budget = |rounds: u32| {
            serde_json::from_value::<RepairBudget>(json!({
                "repair_rounds": rounds, "wall_time_ms": 1, "model_cost_micros": 1,
                "max_episodes": 1
            }))
        };
        assert_eq!(RepairRounds::MAX, 8);
        assert!(budget(0).is_ok());
        assert!(budget(8).is_ok());
        assert!(budget(9).is_err());
    }

    #[test]
    fn a_target_keeps_missing_null_and_empty_apart() {
        let target = |after: serde_json::Value| {
            serde_json::from_value::<TargetExample>(json!({
                "kind": "target", "step_id": "s1", "path": "body.links", "after": after
            }))
            .expect("a target")
        };
        let removed = target(json!({ "state": "missing" }));
        let null = target(json!({ "state": "present", "value": null }));
        let empty = target(json!({ "state": "present", "value": [] }));
        assert_ne!(removed.after, null.after);
        assert_ne!(null.after, empty.after);
        assert!(removed.before.is_none());
    }
}
