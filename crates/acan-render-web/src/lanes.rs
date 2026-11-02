//! The scope-first lanes the page's view carries: one per order whose scope has been set.
//!
//! A may-change item is a slice of behavior over the plant machines it names. It never says where
//! a change's code sits; only the receipt says what a change did. Machine names come through as
//! the scope gives them, so the page resolves them against its own machines and can show a name
//! the plan does not have.

use acan_core::{OrderId, ScopeLane, TaskState, World};
use acan_intent::{
    Change, ChoiceId, ChoiceOption, ItemId, ItemProvenance, MachineId, MethodSelector, MustKeep,
    MustKeepId, NonEmpty, OpenChoice, RulingId, Scope, ScopeConfirmation, ScopeHash, ScopeItem,
    ScopeRev, Selector,
};
use serde::Serialize;

/// One order's scope lane: its current revision, what that revision names, and its
/// confirmation once a human has given one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LaneView {
    /// The order.
    pub order: OrderId,
    /// The current scope revision.
    pub rev: ScopeRev,
    /// The hash of the current revision.
    pub scope_hash: ScopeHash,
    /// The current revision whole, so the page can send it back as the base of a new one.
    pub scope: Scope,
    /// `confirmed` once the current revision has a confirmation, else `unconfirmed`.
    pub state: &'static str,
    /// The may-change items, in scope order.
    pub may: Vec<MayView>,
    /// The must-keep rules, in scope order.
    pub must_keep: Vec<KeepView>,
    /// The open choices, in scope order.
    pub open: Vec<ChoiceView>,
    /// The confirmation of the current revision, as the human gave it.
    pub confirmation: Option<ScopeConfirmation>,
    /// The tasks of the order that hold a lease now. A newer revision ends each one as
    /// `Superseded`. A lease shows that the agent's authority is valid, not that its work is safe.
    pub live_leases: u32,
}

/// One may-change item.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MayView {
    /// The item.
    pub id: ItemId,
    /// The text of the item's first acceptance obligation.
    pub label: String,
    /// The text of every acceptance obligation, in order. A Strike names them before it sends.
    pub obligations: Vec<String>,
    /// The selector's method and route template, such as `POST /api/login`.
    pub selector: String,
    /// What kind of change.
    pub change: Change,
    /// Where the item came from.
    pub provenance: ItemProvenance,
    /// Whether the selector is a broad grant that needs its own acknowledgement.
    pub broad: bool,
    /// The plant machines the item names, as the scope names them.
    pub machines: Vec<String>,
}

/// One must-keep rule.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeepView {
    /// The rule.
    pub id: MustKeepId,
    /// The rule in words.
    pub text: String,
    /// The ruling it restates, if any.
    pub ruling_id: Option<RulingId>,
    /// The plant machines the rule names, as the scope names them.
    pub machines: Vec<String>,
}

/// One open choice the human answers before confirming.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChoiceView {
    /// The choice.
    pub id: ChoiceId,
    /// The machine its question pin goes on, as the scope names it.
    pub machine: String,
    /// The question.
    pub question: String,
    /// The answers, each a `key` and a `label`.
    pub options: Vec<ChoiceOption>,
}

/// An order with at least one recorded receipt: its id as the origin names it (`o-N`), and how
/// many of its gate runs have a receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReceiptCount {
    /// The order, as `o-N`.
    pub order: String,
    /// How many of the order's gate runs have a recorded receipt.
    pub recorded: u32,
}

/// Every scope lane of `world`, in order id order. An order without a scope has none.
#[must_use]
pub fn lane_views(world: &World) -> Vec<LaneView> {
    world.lanes().map(|lane| lane_view(world, lane)).collect()
}

/// The receipt count of each order with at least one recorded receipt, in order id order.
#[must_use]
pub fn receipt_counts(world: &World) -> Vec<ReceiptCount> {
    world.lanes().filter_map(receipt_count).collect()
}

fn receipt_count(lane: &ScopeLane) -> Option<ReceiptCount> {
    let recorded = lane
        .runs
        .values()
        .filter(|run| run.receipt.is_some())
        .count();
    (recorded > 0).then(|| ReceiptCount {
        order: lane.order.to_string(),
        recorded: u32::try_from(recorded).unwrap_or(u32::MAX),
    })
}

fn lane_view(world: &World, lane: &ScopeLane) -> LaneView {
    let body = &lane.scope.body;
    LaneView {
        order: lane.order,
        rev: lane.scope.rev,
        scope_hash: lane.scope_hash,
        scope: lane.scope.clone(),
        state: if lane.confirmation.is_some() {
            "confirmed"
        } else {
            "unconfirmed"
        },
        may: body.may.iter().map(may_view).collect(),
        must_keep: body.must_keep.iter().map(keep_view).collect(),
        open: body.open.iter().map(choice_view).collect(),
        confirmation: lane.confirmation.clone(),
        live_leases: live_leases(world, lane.order),
    }
}

/// The tasks of `order` that hold a lease: the ones a revision ends as `Superseded`.
fn live_leases(world: &World, order: OrderId) -> u32 {
    let held = world
        .tasks_of(order)
        .filter(|task| matches!(task.state, TaskState::Claimed { .. }))
        .count();
    u32::try_from(held).unwrap_or(u32::MAX)
}

fn may_view(item: &ScopeItem) -> MayView {
    MayView {
        id: item.id.clone(),
        label: item.acceptance.first().text.clone(),
        obligations: item.acceptance.iter().map(|o| o.text.clone()).collect(),
        selector: selector_words(&item.selector),
        change: item.change,
        provenance: item.provenance,
        broad: item.selector.is_broad(),
        machines: machine_names(&item.machine_ids),
    }
}

fn keep_view(rule: &MustKeep) -> KeepView {
    KeepView {
        id: rule.id.clone(),
        text: rule.text.clone(),
        ruling_id: rule.ruling_id.clone(),
        machines: machine_names(&rule.machine_ids),
    }
}

fn choice_view(choice: &OpenChoice) -> ChoiceView {
    ChoiceView {
        id: choice.id.clone(),
        machine: choice.machine_id.as_str().to_owned(),
        question: choice.question.clone(),
        options: choice.options.as_slice().to_vec(),
    }
}

fn machine_names(ids: &NonEmpty<MachineId>) -> Vec<String> {
    ids.iter().map(|id| id.as_str().to_owned()).collect()
}

/// The selector's method and route template, each in its wire spelling.
fn selector_words(selector: &Selector) -> String {
    format!(
        "{} {}",
        method_word(selector.method),
        selector.route_template.as_str()
    )
}

/// The method as the contract spells it. The test below pins each word to its serde form.
fn method_word(method: MethodSelector) -> &'static str {
    match method {
        MethodSelector::Any => "ANY",
        MethodSelector::Get => "GET",
        MethodSelector::Head => "HEAD",
        MethodSelector::Post => "POST",
        MethodSelector::Put => "PUT",
        MethodSelector::Patch => "PATCH",
        MethodSelector::Delete => "DELETE",
        MethodSelector::Options => "OPTIONS",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_method_word_is_its_wire_spelling() {
        let methods = [
            MethodSelector::Any,
            MethodSelector::Get,
            MethodSelector::Head,
            MethodSelector::Post,
            MethodSelector::Put,
            MethodSelector::Patch,
            MethodSelector::Delete,
            MethodSelector::Options,
        ];
        for method in methods {
            let wire = serde_json::to_value(method).expect("a method serializes");
            assert_eq!(wire.as_str(), Some(method_word(method)), "{method:?}");
        }
    }
}
