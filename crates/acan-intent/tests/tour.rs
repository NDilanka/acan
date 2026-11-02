//! The deterministic tour and tour admission, over the fixture views and the trusted catalog.

use acan_intent::admit::{self, SlotMismatch, StepFault, TourRejection};
use acan_intent::{
    CaptionCatalog, DeterministicError, EvidenceRef, EvidenceSlot, ItemClass, ItemState, PlantView,
    Tour, TourStep, TourView, deterministic,
};
use serde_json::{Value, json};

const DIRECTED: &str = include_str!("../fixtures/plant-view-v1.json");
const PLAIN: &str = include_str!("../fixtures/plant-view-v1-plain.json");
const MISSING_MANDATORY: &str = include_str!("../fixtures/invalid-tour-missing-mandatory.json");
const UNKNOWN_REF: &str = include_str!("../fixtures/invalid-tour-unknown-ref.json");
const LITERAL_CAPTION: &str = include_str!("../fixtures/invalid-tour-literal-caption.json");
const SWAPPED: &str = include_str!("../fixtures/invalid-tour-swapped-base-candidate.json");

const ALL_STATES: [ItemState; 5] = [
    ItemState::Open,
    ItemState::Bounced,
    ItemState::Fixed,
    ItemState::Residue,
    ItemState::Stopped,
];

fn view(text: &str) -> PlantView {
    serde_json::from_str(text).expect("the fixture view decodes")
}

fn raw_view() -> Value {
    serde_json::from_str(DIRECTED).expect("the fixture view is JSON")
}

fn trusted() -> CaptionCatalog {
    CaptionCatalog::trusted().expect("the trusted catalog parses")
}

fn directed_tour(view: &PlantView) -> Tour {
    let Some(TourView::Directed { tour, .. }) = &view.tour else {
        panic!("the fixture has a directed tour");
    };
    tour.clone()
}

fn deterministic_tour(view: &PlantView) -> Tour {
    deterministic(view).expect("the view has a deterministic tour")
}

/// The item ids of `tour`, in step order.
fn order(tour: &Tour) -> Vec<&str> {
    tour.steps
        .iter()
        .map(|step| step.item_id.as_str())
        .collect()
}

/// The step about `item`.
fn step_of<'a>(tour: &'a Tour, item: &str) -> &'a TourStep {
    tour.steps
        .iter()
        .find(|step| step.item_id.as_str() == item)
        .unwrap_or_else(|| panic!("no step about {item}"))
}

/// The evidence the step binds to its caption slot `slot`.
fn bound<'a>(step: &'a TourStep, slot: &str) -> &'a EvidenceRef {
    step.caption
        .slots
        .iter()
        .find(|(name, _)| name.as_str() == slot)
        .map(|(_, reference)| reference)
        .unwrap_or_else(|| panic!("no {slot} slot"))
}

/// The template the step about `item` names.
fn template_of<'a>(tour: &'a Tour, item: &str) -> &'a str {
    step_of(tour, item).caption.template_id.as_str()
}

fn index_of(tour: &Tour, item: &str) -> usize {
    tour.steps
        .iter()
        .position(|step| step.item_id.as_str() == item)
        .unwrap_or_else(|| panic!("no step about {item}"))
}

/// The view item `id` in the raw JSON of the directed fixture.
fn item_mut<'a>(value: &'a mut Value, id: &str) -> &'a mut Value {
    value["items"]
        .as_array_mut()
        .expect("items")
        .iter_mut()
        .find(|item| item["item_id"] == id)
        .unwrap_or_else(|| panic!("no item {id}"))
}

/// The evidence row of `item` in `slot`.
fn row_mut<'a>(item: &'a mut Value, slot: &str) -> &'a mut Value {
    item["evidence"]
        .as_array_mut()
        .expect("evidence")
        .iter_mut()
        .find(|row| row["slot"] == slot)
        .unwrap_or_else(|| panic!("no {slot} row"))
}

#[test]
fn the_deterministic_order_is_residue_then_violations_then_bounced_then_the_rest() {
    let view = view(DIRECTED);
    assert_eq!(
        order(&deterministic_tour(&view)),
        [
            "d-login-error-body",
            "d-links-sort",
            "i-retry-after",
            "d-links-clicks",
            "chk-dashboard",
            "chk-links-delete",
            "chk-signup-store",
            "i-login-limit",
        ]
    );
}

#[test]
fn every_mandatory_item_is_reached_by_the_deterministic_tour() {
    let view = view(DIRECTED);
    let tour = deterministic_tour(&view);
    let mandatory: Vec<_> = view.mandatory_items().collect();
    assert_eq!(mandatory.len(), 7);
    for item in mandatory {
        assert!(tour.reaches(&item.item_id), "{} is unreached", item.item_id);
    }
}

#[test]
fn each_class_gets_the_caption_that_says_what_it_is() {
    let tour = deterministic_tour(&view(DIRECTED));
    for (item, template) in [
        ("d-login-error-body", "unspecified.authorized"),
        ("d-links-sort", "violation.ruling"),
        ("i-retry-after", "bounced.obligation_failed"),
        ("chk-dashboard", "unknown.unstable_base"),
        ("d-links-clicks", "stopped.out_of_scope"),
        ("chk-links-delete", "unknown.incomparable"),
        ("chk-signup-store", "unknown.error"),
        ("i-login-limit", "fulfilled.obligations"),
    ] {
        assert_eq!(template_of(&tour, item), template, "{item}");
    }
}

#[test]
fn a_caption_binds_base_to_the_base_run_and_candidate_to_the_candidate_run() {
    let tour = deterministic_tour(&view(DIRECTED));
    let step = step_of(&tour, "d-links-clicks");
    assert_eq!(
        bound(step, "base").slot,
        EvidenceSlot::BaseA,
        "base is the base run"
    );
    assert_eq!(
        bound(step, "candidate").slot,
        EvidenceSlot::Candidate,
        "candidate is the candidate run"
    );
}

#[test]
fn the_plain_fixture_gets_the_same_deterministic_tour() {
    assert_eq!(
        deterministic_tour(&view(PLAIN)),
        deterministic_tour(&view(DIRECTED))
    );
}

#[test]
fn the_directed_fixture_tour_is_admitted() {
    let view = view(DIRECTED);
    let tour = directed_tour(&view);
    let bound = admit::tour(&view, &trusted(), &tour).expect("the fixture tour is admitted");
    assert_eq!(bound.len(), tour.steps.len());
}

#[test]
fn an_optional_item_that_cannot_be_bound_is_left_out() {
    let mut value = raw_view();
    row_mut(item_mut(&mut value, "i-login-limit"), "candidate")["value"] =
        json!({ "state": "present", "value": "too many" });
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");

    let tour = deterministic_tour(&view);

    assert!(!tour.reaches(&"i-login-limit".parse().expect("an id")));
    assert_eq!(tour.steps.len(), 7);
}

#[test]
fn a_mandatory_item_that_cannot_be_bound_fails_the_deterministic_tour() {
    let mut value = raw_view();
    row_mut(item_mut(&mut value, "chk-signup-store"), "base_a")["value"] =
        json!({ "state": "present", "value": "created" });
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");

    let err = deterministic(&view).expect_err("a mandatory item has no caption");

    assert!(
        matches!(
            &err,
            DeterministicError::Unbindable { item_id, .. }
                if item_id.as_str() == "chk-signup-store"
        ),
        "{err}"
    );
}

#[test]
fn a_view_with_nothing_to_visit_has_no_tour() {
    let mut view = view(DIRECTED);
    view.items.clear();

    assert!(matches!(
        deterministic(&view),
        Err(DeterministicError::NothingToVisit)
    ));
}

#[test]
fn an_authorized_item_is_captioned_as_unspecified_in_every_state() {
    for state in ALL_STATES {
        let mut value = raw_view();
        item_mut(&mut value, "d-login-error-body")["state"] = json!(state);
        let view: PlantView = serde_json::from_value(value).expect("decoding the view");

        let tour = deterministic_tour(&view);

        assert_eq!(
            template_of(&tour, "d-login-error-body"),
            "unspecified.authorized",
            "{state:?}"
        );
    }
}

#[test]
fn an_out_of_scope_item_that_is_open_or_bounced_is_not_captioned_as_stopped() {
    let catalog = trusted();
    let stopped = catalog
        .template(&"stopped.out_of_scope".parse().expect("a template id"))
        .expect("the stopped template");
    assert!(
        stopped.text.contains("repair budget is spent"),
        "the stopped template says the budget is spent"
    );
    for state in [ItemState::Open, ItemState::Bounced] {
        let mut value = raw_view();
        item_mut(&mut value, "d-links-clicks")["state"] = json!(state);
        let view: PlantView = serde_json::from_value(value).expect("decoding the view");

        let tour = deterministic_tour(&view);

        assert_eq!(
            template_of(&tour, "d-links-clicks"),
            "out_of_scope.changed_value",
            "{state:?}"
        );
        let step = step_of(&tour, "d-links-clicks");
        assert_eq!(bound(step, "base").slot, EvidenceSlot::BaseA);
    }
}

#[test]
fn an_obligation_failure_with_no_target_is_captioned_by_its_candidate() {
    for state in ["open", "bounced"] {
        let mut value = raw_view();
        let item = item_mut(&mut value, "i-retry-after");
        item["state"] = json!(state);
        item["evidence"]
            .as_array_mut()
            .expect("evidence")
            .retain(|row| row["slot"] != "target");
        let view: PlantView = serde_json::from_value(value).expect("decoding the view");

        let tour = deterministic_tour(&view);

        assert_eq!(
            template_of(&tour, "i-retry-after"),
            "failed.obligation",
            "{state}"
        );
        assert!(
            admit::tour(&view, &trusted(), &tour).is_ok(),
            "{state}: the tour is admitted"
        );
    }
}

#[test]
fn a_template_that_does_not_describe_its_item_is_refused() {
    // The scope wants 429 here, said of an item that passed: the words are true of no item here.
    let mut value = raw_view();
    let steps = value["tour"]["tour"]["steps"]
        .as_array_mut()
        .expect("tour steps");
    let step = steps
        .iter_mut()
        .find(|step| step["item_id"] == "i-login-limit")
        .expect("the fulfilled item has a step");
    step["caption"] = json!({
        "template_id": "bounced.obligation_failed",
        "slots": {
            "target": { "item_id": "i-login-limit", "slot": "target", "step": "s6", "field": "status" }
        }
    });
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");
    let tour = directed_tour(&view);

    let rejected = admit::tour(&view, &trusted(), &tour);

    assert_eq!(
        rejected,
        Err(TourRejection::Step {
            tour_step: index_of(&tour, "i-login-limit"),
            fault: StepFault::TemplateDoesNotFit {
                template_id: "bounced.obligation_failed".parse().expect("a template id"),
            },
        })
    );
}

#[test]
fn the_mandatory_classes_and_states_without_a_caption_are_exactly_the_known_gaps() {
    // A mandatory item the catalog cannot caption makes the deterministic tour fail, so every
    // gap is listed here until the catalog covers it.
    let catalog = trusted();
    let mut uncaptioned = Vec::new();
    for class in ItemClass::ALL {
        for state in ALL_STATES {
            let mandatory = !class.counts_toward_pass()
                || matches!(
                    state,
                    ItemState::Bounced | ItemState::Residue | ItemState::Stopped
                );
            let fitted = catalog.templates.iter().any(|template| {
                template.classes.contains(&class) && template.states.contains(&state)
            });
            if mandatory && !fitted {
                uncaptioned.push((class, state));
            }
        }
    }
    assert_eq!(
        uncaptioned,
        [
            (ItemClass::Preserved, ItemState::Bounced),
            (ItemClass::Preserved, ItemState::Residue),
            (ItemClass::Preserved, ItemState::Stopped),
            (ItemClass::ObligationFailed, ItemState::Fixed),
        ]
    );
}

#[test]
fn the_missing_mandatory_item_fixture_is_refused_for_that_item() {
    let view = view(DIRECTED);
    let tour: Tour = serde_json::from_str(MISSING_MANDATORY).expect("the fixture tour decodes");

    assert_eq!(
        admit::tour(&view, &trusted(), &tour),
        Err(TourRejection::Unreached {
            item_id: "chk-signup-store".parse().expect("an id"),
        })
    );
}

#[test]
fn the_unknown_ref_fixture_is_refused_at_its_step() {
    let view = view(DIRECTED);
    let tour: Tour = serde_json::from_str(UNKNOWN_REF).expect("the fixture tour decodes");

    let rejected = admit::tour(&view, &trusted(), &tour);

    assert!(
        matches!(
            rejected,
            Err(TourRejection::Step { tour_step, fault: StepFault::Unresolved { .. } })
                if tour_step == index_of(&tour, "d-links-sort")
        ),
        "{rejected:?}"
    );
}

#[test]
fn the_literal_caption_fixture_does_not_decode() {
    let err = serde_json::from_str::<Tour>(LITERAL_CAPTION)
        .expect_err("a caption with its own text is refused");

    assert!(err.to_string().contains("text"), "{err}");
}

#[test]
fn the_swapped_base_and_candidate_fixture_is_refused_at_the_base_slot() {
    let view = view(DIRECTED);
    let tour: Tour = serde_json::from_str(SWAPPED).expect("the fixture tour decodes");

    assert_eq!(
        admit::tour(&view, &trusted(), &tour),
        Err(TourRejection::Step {
            tour_step: index_of(&tour, "d-login-error-body"),
            fault: StepFault::Slot {
                slot: "base".parse().expect("a slot"),
                mismatch: SlotMismatch::Column,
            },
        })
    );
}
