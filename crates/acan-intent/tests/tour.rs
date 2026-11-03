//! The deterministic tour and tour admission, over the fixture views and the trusted catalog.

use acan_intent::admit::{self, SlotMismatch, StepFault, TourRejection};
use acan_intent::{
    Caption, CaptionCatalog, CaptionTemplate, DeterministicError, EvidenceRef, EvidenceSlot,
    ItemClass, ItemState, PlantView, Tour, TourStep, TourView, ValueType, deterministic,
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

/// Phrases a caption must not use: each claims more than the recorded classification shows.
const OVERCLAIMS: [&str; 18] = [
    "another item",
    "no obligation checks",
    "which the ruling forbids",
    "missed the target",
    "as the scope asked",
    "did not complete",
    "cannot be compared",
    "the candidate did not",
    "does not answer",
    "requested",
    "is right",
    "is wrong",
    "causes",
    "violates",
    "budget",
    "covers",
    "wants",
    "should",
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
    match deterministic(view).expect("the view has a deterministic tour") {
        TourView::Deterministic { tour, .. } => tour,
        TourView::Directed { .. } => panic!("deterministic returns the deterministic tour"),
    }
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

/// The text of the trusted template `template_id`.
fn text_of(template_id: &str) -> String {
    trusted()
        .template(&template_id.parse().expect("a template id"))
        .unwrap_or_else(|| panic!("no template {template_id}"))
        .text
        .clone()
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
            "chk-health",
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
    assert_eq!(tour.steps.len(), 8);
}

#[test]
fn a_fixed_obligation_failure_is_captioned_as_a_failure() {
    // A failed check can be fixed, and then regress: the receipt keeps the fixed state on the
    // same item. The caption still names the failed class, and quotes only what it binds.
    let mut value = raw_view();
    item_mut(&mut value, "i-retry-after")["state"] = json!("fixed");
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");

    let tour = deterministic_tour(&view);

    assert!(tour.reaches(&"i-retry-after".parse().expect("an id")));
    assert!(matches!(
        template_of(&tour, "i-retry-after"),
        "failed.obligation" | "failed.obligation.bare"
    ));
    assert!(admit::tour(&view, &trusted(), &tour).is_ok());
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
fn a_view_of_passing_checks_only_still_has_a_tour_of_its_open_preserved_checks() {
    // A clean receipt: nothing to rule on, every check passed and is open. The walk still has a
    // step, so the page has a tour before the director answers.
    let mut view = view(DIRECTED);
    view.items.retain(|item| item.class == ItemClass::Preserved);

    let tour = deterministic_tour(&view);

    assert_eq!(order(&tour), ["chk-health"]);
    assert_eq!(template_of(&tour, "chk-health"), "preserved.open");
    assert!(admit::tour(&view, &trusted(), &tour).is_ok());
}

/// The directed fixture with `count` more open preserved checks, each a copy of `chk-health`.
fn with_open_preserved_checks(count: usize) -> PlantView {
    let mut value = raw_view();
    item_mut(&mut value, "chk-health")["state"] = json!("open");
    let copy = item_mut(&mut value, "chk-health").clone();
    let items = value["items"].as_array_mut().expect("items");
    for number in 1..=count {
        let mut item = copy.clone();
        item["item_id"] = json!(format!("chk-health-{number}"));
        items.push(item);
    }
    serde_json::from_value(value).expect("decoding the view")
}

#[test]
fn the_fallback_visits_at_most_three_open_preserved_checks() {
    let view = with_open_preserved_checks(4);

    let tour = deterministic_tour(&view);

    let preserved: Vec<&str> = order(&tour)
        .into_iter()
        .filter(|item| item.starts_with("chk-health"))
        .collect();
    assert_eq!(preserved, ["chk-health", "chk-health-1", "chk-health-2"]);
    assert!(admit::tour(&view, &trusted(), &tour).is_ok());
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
    assert!(
        text_of("stopped.out_of_scope").ends_with("Recorded state: stopped."),
        "the stopped template names the recorded state"
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

/// The deterministic tour of the fixture with the held must-keep check `chk-health` moved to
/// `state`.
fn held_check_in(state: ItemState) -> (PlantView, Tour) {
    let mut value = raw_view();
    item_mut(&mut value, "chk-health")["state"] = json!(state);
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");
    let tour = deterministic_tour(&view);
    (view, tour)
}

#[test]
fn a_held_must_keep_check_in_bounced_records_the_bounced_state() {
    let (view, tour) = held_check_in(ItemState::Bounced);

    assert_eq!(template_of(&tour, "chk-health"), "preserved.bounced");
    assert!(step_of(&tour, "chk-health").caption.slots.is_empty());
    assert!(admit::tour(&view, &trusted(), &tour).is_ok());
    assert!(text_of("preserved.bounced").ends_with("Recorded state: bounced."));
}

#[test]
fn a_held_must_keep_check_in_residue_records_the_residue_state() {
    let (view, tour) = held_check_in(ItemState::Residue);

    assert_eq!(template_of(&tour, "chk-health"), "preserved.residue");
    assert!(admit::tour(&view, &trusted(), &tour).is_ok());
    assert!(text_of("preserved.residue").ends_with("Recorded state: residue."));
}

#[test]
fn a_held_must_keep_check_when_stopped_records_the_stopped_state() {
    let (view, tour) = held_check_in(ItemState::Stopped);

    assert_eq!(template_of(&tour, "chk-health"), "preserved.stopped");
    assert!(admit::tour(&view, &trusted(), &tour).is_ok());
    assert!(text_of("preserved.stopped").ends_with("Recorded state: stopped."));
}

#[test]
fn every_mandatory_class_and_state_has_a_caption() {
    // A mandatory item the catalog cannot caption makes the deterministic tour fail, so every
    // gap is listed here. A fixed obligation failure is captioned by the bare failure template.
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
    assert!(uncaptioned.is_empty(), "uncaptioned: {uncaptioned:?}");
}

#[test]
fn every_class_and_state_has_a_caption() {
    // Not only the mandatory pairs: a tour may visit any item. A check that passed and is open
    // is the usual state of a clean receipt, and no template described it before captions-5.
    let catalog = trusted();
    let mut uncaptioned = Vec::new();
    for class in ItemClass::ALL {
        for state in ALL_STATES {
            let fitted = catalog.templates.iter().any(|template| {
                template.classes.contains(&class) && template.states.contains(&state)
            });
            if !fitted {
                uncaptioned.push((class, state));
            }
        }
    }
    assert!(uncaptioned.is_empty(), "uncaptioned: {uncaptioned:?}");
}

#[test]
fn an_open_preserved_check_is_captioned_and_its_step_is_admitted() {
    // The director's step on a check that passed and is open. Admission may still refuse the
    // whole tour for a mandatory item it does not reach, but not for this step.
    let mut value = raw_view();
    item_mut(&mut value, "chk-health")["state"] = json!("open");
    let view: PlantView = serde_json::from_value(value.clone()).expect("decoding the view");
    let tour: Tour = serde_json::from_value(json!({
        "receipt_hash": value["receipt_hash"],
        "steps": [{
            "item_id": "chk-health",
            "camera": { "machine_id": "server", "zoom": "machine" },
            "stage": [],
            "evidence_refs": [],
            "caption": { "template_id": "preserved.open", "slots": {} }
        }]
    }))
    .expect("the tour decodes");

    assert_eq!(template_of(&tour, "chk-health"), "preserved.open");
    assert!(text_of("preserved.open").ends_with("Recorded state: open."));
    assert!(!matches!(
        admit::tour(&view, &trusted(), &tour),
        Err(TourRejection::Step { .. })
    ));
}

#[test]
fn an_unknown_probe_item_with_no_evidence_is_captioned_by_its_class() {
    // The receipt holds an unknown probe with no evidence rows: its caption names the class only.
    let mut value = raw_view();
    for item in ["chk-dashboard", "chk-links-delete", "chk-signup-store"] {
        item_mut(&mut value, item)["evidence"] = json!([]);
    }
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");

    let tour = deterministic_tour(&view);

    for (item, template) in [
        ("chk-dashboard", "unknown.unstable_base.bare"),
        ("chk-links-delete", "unknown.incomparable.bare"),
        ("chk-signup-store", "unknown.error.bare"),
    ] {
        assert_eq!(template_of(&tour, item), template, "{item}");
    }
    assert!(admit::tour(&view, &trusted(), &tour).is_ok());
}

#[test]
fn an_out_of_scope_addition_with_no_base_step_is_captioned_by_its_candidate() {
    // The base run has no step where the candidate added a value, so there is no base row to
    // quote. The caption quotes the candidate alone, whatever the state.
    for state in ["open", "stopped"] {
        let mut value = raw_view();
        let clicks = item_mut(&mut value, "d-links-clicks");
        clicks["state"] = json!(state);
        clicks["evidence"]
            .as_array_mut()
            .expect("evidence")
            .retain(|row| row["slot"] != "base_a");
        let view: PlantView = serde_json::from_value(value).expect("decoding the view");

        let tour = deterministic_tour(&view);

        assert_eq!(
            template_of(&tour, "d-links-clicks"),
            "out_of_scope.candidate",
            "{state}"
        );
        assert!(
            admit::tour(&view, &trusted(), &tour).is_ok(),
            "{state}: the tour is admitted"
        );
    }
}

#[test]
fn deterministic_binds_base_and_candidate_to_one_address() {
    // The base run's first row is at another field than the candidate's. The binder must pick the
    // base row at the candidate's field, or the caption compares two differences.
    let mut value = raw_view();
    let clicks = item_mut(&mut value, "d-links-clicks");
    let mut earlier = row_mut(clicks, "base_a").clone();
    earlier["field"] = json!("body.links[0].code");
    clicks["evidence"]
        .as_array_mut()
        .expect("evidence")
        .insert(0, earlier);
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");

    let tour = deterministic_tour(&view);

    let step = step_of(&tour, "d-links-clicks");
    assert_eq!(bound(step, "base").field, bound(step, "candidate").field);
    assert!(admit::tour(&view, &trusted(), &tour).is_ok());
}

/// The directed fixture with an extra copy of the `d-links-clicks` candidate row, changed by
/// `change`, and the tour's candidate slot pointed at that copy by `slot_change`.
fn comparing_extra_candidate(
    change: impl Fn(&mut Value),
    slot_change: impl Fn(&mut Value),
) -> (PlantView, Tour) {
    let mut value = raw_view();
    let clicks = item_mut(&mut value, "d-links-clicks");
    let mut copy = row_mut(clicks, "candidate").clone();
    copy["value"] = json!({ "state": "present", "value": "a1" });
    change(&mut copy);
    clicks["evidence"]
        .as_array_mut()
        .expect("evidence")
        .push(copy);
    let step = value["tour"]["tour"]["steps"]
        .as_array_mut()
        .expect("tour steps")
        .iter_mut()
        .find(|step| step["item_id"] == "d-links-clicks")
        .expect("a step about the stopped item");
    slot_change(&mut step["caption"]["slots"]["candidate"]);
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");
    let tour = directed_tour(&view);
    (view, tour)
}

#[test]
fn a_caption_comparing_two_fields_is_refused_at_the_candidate_slot() {
    let (view, tour) = comparing_extra_candidate(
        |copy| copy["field"] = json!("body.links[0].code"),
        |slot| slot["field"] = json!("body.links[0].code"),
    );

    assert_eq!(
        admit::tour(&view, &trusted(), &tour),
        Err(TourRejection::Step {
            tour_step: index_of(&tour, "d-links-clicks"),
            fault: StepFault::MismatchedAddress {
                slot: "candidate".parse().expect("a slot"),
            },
        })
    );
}

#[test]
fn a_caption_comparing_two_steps_is_refused_at_the_candidate_slot() {
    let (view, tour) = comparing_extra_candidate(
        |copy| copy["step"] = json!("s9"),
        |slot| slot["step"] = json!("s9"),
    );

    assert_eq!(
        admit::tour(&view, &trusted(), &tour),
        Err(TourRejection::Step {
            tour_step: index_of(&tour, "d-links-clicks"),
            fault: StepFault::MismatchedAddress {
                slot: "candidate".parse().expect("a slot"),
            },
        })
    );
}

#[test]
fn a_present_null_is_quoted_as_null_in_an_any_slot() {
    // `violation.ruling` takes any value, so a JSON null binds and reads as `null`, which is
    // what the run observed. It is not the missing-value text.
    let mut value = raw_view();
    row_mut(item_mut(&mut value, "d-links-sort"), "candidate")["value"] =
        json!({ "state": "present", "value": null });
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");

    let TourView::Deterministic { tour, bindings } =
        deterministic(&view).expect("the view has a deterministic tour")
    else {
        panic!("deterministic returns the deterministic tour");
    };

    assert_eq!(template_of(&tour, "d-links-sort"), "violation.ruling");
    let candidate = "candidate".parse().expect("a slot");
    let value = &bindings[index_of(&tour, "d-links-sort")].typed_slot_bindings[&candidate];
    assert_eq!(value.to_string(), "null");
}

#[test]
fn a_present_null_in_a_typed_slot_is_refused_and_an_optional_item_is_left_out() {
    // `fulfilled.obligations` takes a status, which a JSON null is not, and the fixed item has no
    // other caption.
    let mut value = raw_view();
    row_mut(item_mut(&mut value, "i-login-limit"), "candidate")["value"] =
        json!({ "state": "present", "value": null });
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");

    let tour = deterministic_tour(&view);

    assert!(!tour.reaches(&"i-login-limit".parse().expect("an id")));
}

#[test]
fn only_an_any_slot_admits_a_json_null() {
    assert!(ValueType::Any.admits(&Value::Null));
    for kind in [
        ValueType::Status,
        ValueType::Count,
        ValueType::Duration,
        ValueType::Commit,
        ValueType::Identifier,
        ValueType::Text,
        ValueType::Number,
        ValueType::Boolean,
    ] {
        assert!(!kind.admits(&Value::Null), "{kind:?} admits null");
    }
}

#[test]
fn every_caption_says_its_classification_before_any_value() {
    for template in &trusted().templates {
        assert!(
            template.text.starts_with("Classified: "),
            "{} does not open with its classification",
            template.template_id
        );
    }
}

#[test]
fn no_caption_claims_more_than_its_classification_shows() {
    for template in &trusted().templates {
        for claim in OVERCLAIMS {
            assert!(
                !template.text.contains(claim),
                "{} claims {claim}",
                template.template_id
            );
        }
    }
}

#[test]
fn a_caption_that_repeats_a_slot_does_not_decode() {
    let text = r#"{"template_id":"unspecified.authorized","slots":{
        "base":{"item_id":"d-login-error-body","slot":"base_a","step":"s6","field":"body.error"},
        "base":{"item_id":"d-login-error-body","slot":"candidate","step":"s6","field":"body.error"}}}"#;

    let err = serde_json::from_str::<Caption>(text).expect_err("a repeated slot is refused");

    assert!(
        err.to_string().contains("duplicate caption slot base"),
        "{err}"
    );
}

#[test]
fn a_template_that_repeats_a_slot_does_not_decode() {
    let text = r#"{"template_id":"violation.ruling","text":"Classified: violates a ruling.",
        "classes":["violates_ruling"],"states":["open"],"slots":{
        "candidate":{"column":"candidate","value_type":"any","presence":"optional"},
        "candidate":{"column":"base_a","value_type":"any","presence":"optional"}}}"#;

    let err = serde_json::from_str::<CaptionTemplate>(text)
        .expect_err("a repeated template slot is refused");

    assert!(
        err.to_string().contains("duplicate caption slot candidate"),
        "{err}"
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
