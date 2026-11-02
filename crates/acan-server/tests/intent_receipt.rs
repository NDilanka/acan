//! Tests for the portable receipt: the JSON and Markdown renderers and the plant view.
//!
//! The server keeps `receipt` private, so this crate compiles the module in by path. Every test
//! reads the contract fixtures, so the expected values are the contract's own. Set
//! `ACAN_UPDATE_GOLDEN=1` to rewrite the Markdown golden file after a reviewed change.

#[path = "../src/receipt.rs"]
mod receipt;

use std::fs;

use acan_intent::{
    DeterministicError, ItemClass, Outcome, PlantView, Receipt, RulingStatus, Scope, TourView,
    UnknownReason,
};
use receipt::{
    MachineInput, PlantViewError, PlateOrder, RenderError, plant_view, render_json, render_markdown,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const RECEIPT: &str = include_str!("../../acan-intent/fixtures/receipt.json");
const SCOPE: &str = include_str!("../../acan-intent/fixtures/scope.json");
const PLAIN_VIEW: &str = include_str!("../../acan-intent/fixtures/plant-view-v1-plain.json");
const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/receipt/fixture-receipt.md"
);

fn decode<T: DeserializeOwned>(json: &str) -> T {
    serde_json::from_str(json).expect("the fixture decodes")
}

fn decode_value<T: DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).expect("the fixture value decodes")
}

fn fixture_receipt() -> Receipt {
    decode(RECEIPT)
}

fn fixture_scope() -> Scope {
    decode(SCOPE)
}

fn plain_view() -> Value {
    decode(PLAIN_VIEW)
}

/// The plate and machine list the plain fixture was built with, read from the fixture itself.
fn plate_and_machines(plain: &Value) -> (PlateOrder, Vec<MachineInput>) {
    let order = &plain["order"];
    let plate = PlateOrder {
        sentence: order["sentence"]
            .as_str()
            .expect("the order has a sentence")
            .to_owned(),
        machine_id: decode_value(&order["machine_id"]),
        confirmed_at: decode_value(&order["confirmed_at"]),
    };
    let machines = plain["machines"]
        .as_array()
        .expect("the plain view lists its machines")
        .iter()
        .map(|machine| MachineInput {
            machine_id: decode_value(&machine["machine_id"]),
            routes: decode_value(&machine["routes"]),
        })
        .collect();
    (plate, machines)
}

#[test]
fn the_fixture_receipt_has_counts_that_match_its_items() {
    fixture_receipt()
        .check_counts()
        .expect("the fixture's class counts match its items");
}

#[test]
fn json_renders_the_receipt_and_repeats_byte_for_byte() {
    let receipt = fixture_receipt();
    let first = render_json(&receipt).expect("the fixture renders");
    let second = render_json(&receipt).expect("the fixture renders");
    assert_eq!(first, second);
    assert!(first.ends_with("}\n"), "JSON ends with one newline");
    let decoded: Value = serde_json::from_str(&first).expect("the JSON decodes");
    let expected = serde_json::to_value(&receipt).expect("the receipt serializes");
    assert_eq!(decoded, expected, "the JSON is the receipt");
}

#[test]
fn markdown_repeats_byte_for_byte() {
    let receipt = fixture_receipt();
    let first = render_markdown(&receipt).expect("the fixture renders");
    let second = render_markdown(&receipt).expect("the fixture renders");
    assert_eq!(first, second);
}

#[test]
fn markdown_matches_the_golden_file() {
    let rendered = render_markdown(&fixture_receipt()).expect("the fixture renders");
    if std::env::var_os("ACAN_UPDATE_GOLDEN").is_some() {
        fs::write(GOLDEN, &rendered).expect("the golden file is writable");
    }
    // A Windows checkout with autocrlf turns the golden file's line endings into CRLF. The
    // renderer writes LF, so the comparison reads the file with LF endings.
    let golden = fs::read_to_string(GOLDEN)
        .expect("the golden file exists; run with ACAN_UPDATE_GOLDEN=1 to write it")
        .replace("\r\n", "\n");
    assert_eq!(rendered, golden);
}

#[test]
fn markdown_names_both_commits_and_every_section() {
    let receipt = fixture_receipt();
    let md = render_markdown(&receipt).expect("the fixture renders");
    assert!(md.contains(&format!(
        "Base commit: `{}`",
        receipt.base_commit().as_str()
    )));
    assert!(md.contains(&format!(
        "Merged commit (tested): `{}`",
        receipt.merged_commit().as_str()
    )));
    for heading in [
        "## Items by class",
        "## Probes, routes and behavior",
        "## Unknown and error results",
        "## Stale evidence",
        "## Masks",
        "## Rulings",
        "## Items",
        "## Contests",
        "## Tested context",
    ] {
        assert!(md.contains(heading), "the report prints {heading}");
    }
}

#[test]
fn markdown_prints_probe_and_route_denominators_apart_from_behavior() {
    let md = render_markdown(&fixture_receipt()).expect("the fixture renders");
    assert!(md.contains("Probes run: 6 of 6 (1 errored; errored probes never pass)."));
    assert!(
        md.contains("Route hits: 7 of 9 baseline routes. A route hit is not behavioral coverage.")
    );
    assert!(md.contains(
        "Behavioral obligations: 7 total; passed 4, failed 2, unknown 1. These count obligations, not route hits."
    ));
    assert!(md.contains("| **total items** | 9 | - |"));
    assert!(md.contains("| `obligation_fulfilled` | 1 | yes |"));
    assert!(md.contains("| `unstable_base` | 1 | no |"));
}

#[test]
fn markdown_lists_every_uncovered_route() {
    let md = render_markdown(&fixture_receipt()).expect("the fixture renders");
    assert!(
        md.contains("| `GET` | `/:code` | redirect probes wait for the follow-redirect runner |")
    );
    assert!(md.contains("| `POST` | `/api/logout` | no probe yet |"));
}

#[test]
fn markdown_prints_each_mask_and_its_hashes() {
    let receipt = fixture_receipt();
    let md = render_markdown(&receipt).expect("the fixture renders");
    assert!(md.contains("Masks applied: 2."));
    for mask in &receipt.masks {
        assert!(md.contains(&format!("`{}`", mask.normalization_hash)));
        assert!(md.contains(&format!("`{}`", mask.base_a_observation_hash)));
        assert!(md.contains(&format!("`{}`", mask.base_b_observation_hash)));
    }
}

#[test]
fn markdown_labels_a_not_enforced_ruling_as_a_note() {
    let md = render_markdown(&fixture_receipt()).expect("the fixture renders");
    assert!(md.contains(
        "| `r-2` | 2 | Error pages read as plain sentences | `errors` | NOT ENFORCED | NOT ENFORCED: unexpressible: tone of error text has no predicate |"
    ));
    assert!(md.contains(
        "- NOT ENFORCED `r-2` Error pages read as plain sentences: unexpressible: tone of error text has no predicate"
    ));
}

#[test]
fn markdown_prints_bounce_attempts_of_the_budget() {
    let md = render_markdown(&fixture_receipt()).expect("the fixture renders");
    assert!(
        md.contains("| `i-retry-after` | `obligation_failed` | `bounced` | `ratelimit` | 1 of 2 |")
    );
    assert!(md.contains("- Bounce attempts: 1 of 2"));
}

#[test]
fn markdown_prints_unknown_and_error_results_with_their_reasons() {
    let md = render_markdown(&fixture_receipt()).expect("the fixture renders");
    assert!(md.contains("- Item `chk-dashboard`: class `unstable_base`; outcome unstable base"));
    assert!(md.contains("- Item `chk-signup-store`: class `error`"));
}

#[test]
fn markdown_prints_stale_evidence_when_the_receipt_has_it() {
    let mut receipt = fixture_receipt();
    receipt.items[2].evaluation.outcome = Outcome::Unknown {
        reason: UnknownReason::Stale {},
    };
    receipt.rulings[2].status = RulingStatus::Stale {};
    let md = render_markdown(&receipt).expect("the fixture renders");
    assert!(md.contains("- Item `chk-health`: stale evidence"));
    assert!(md.contains("- Ruling `r-3`: stale"));
}

#[test]
fn markdown_says_none_for_an_empty_section() {
    let mut receipt = fixture_receipt();
    receipt.contests.clear();
    let md = render_markdown(&receipt).expect("the fixture renders");
    let contests = md
        .split("## Contests")
        .nth(1)
        .expect("the report prints a contests section");
    assert!(contests.contains("Contests: 0."));
    assert!(contests.contains("None."));
}

#[test]
fn markdown_prints_the_contest_and_its_rationale() {
    let md = render_markdown(&fixture_receipt()).expect("the fixture renders");
    assert!(md.contains("Contest `k-1`: `spec_conflict` against episode"));
    assert!(md.contains("Rationale: The clicks field is dropped because the new limiter reads links before counting; the scope does not allow changing the counter."));
}

#[test]
fn free_text_cannot_open_markup_or_end_a_table_cell() {
    let mut receipt = fixture_receipt();
    receipt.rulings[0].label = "a|b <i>*x*</i>".to_owned();
    let md = render_markdown(&receipt).expect("the fixture renders");
    assert!(md.contains(r"| `r-1` | 1 | a\|b \<i\>\*x\*\</i\> |"));
}

#[test]
fn a_receipt_whose_counts_do_not_match_its_items_is_refused() {
    let mut receipt = fixture_receipt();
    receipt.items[0].class = ItemClass::Error;
    assert!(matches!(render_json(&receipt), Err(RenderError::Counts(_))));
    assert!(matches!(
        render_markdown(&receipt),
        Err(RenderError::Counts(_))
    ));
    let plain = plain_view();
    let (plate, machines) = plate_and_machines(&plain);
    assert!(matches!(
        plant_view(&receipt, &fixture_scope(), &plate, &machines),
        Err(PlantViewError::Counts(_))
    ));
}

#[test]
fn the_receipt_hash_is_the_one_the_plain_view_names() {
    let plain = plain_view();
    let hash = fixture_receipt()
        .receipt_hash()
        .expect("the receipt hashes");
    assert_eq!(Value::String(hash.to_string()), plain["receipt_hash"]);
}

#[test]
fn plant_view_refuses_a_scope_that_is_not_the_one_the_receipt_binds() {
    let plain = plain_view();
    let (plate, machines) = plate_and_machines(&plain);
    let mut scope: Value = decode(SCOPE);
    scope["body"]["may"][0]["change"] = json!("add");
    let edited: Scope = serde_json::from_value(scope).expect("the scope decodes");
    assert!(matches!(
        plant_view(&fixture_receipt(), &edited, &plate, &machines),
        Err(PlantViewError::ScopeMismatch("hash"))
    ));
}

#[test]
fn the_fixture_pair_binds_and_builds_a_view() {
    let plain = plain_view();
    let (plate, machines) = plate_and_machines(&plain);
    assert!(plant_view(&fixture_receipt(), &fixture_scope(), &plate, &machines).is_ok());
}

#[test]
fn plant_view_matches_the_plain_fixture_where_the_receipt_decides_it() {
    let plain = plain_view();
    let (plate, machines) = plate_and_machines(&plain);
    let receipt = fixture_receipt();
    let view = plant_view(&receipt, &fixture_scope(), &plate, &machines)
        .expect("the fixture builds a view");
    let built = serde_json::to_value(&view).expect("the view serializes");

    let hash = receipt.receipt_hash().expect("the receipt hashes");
    assert_eq!(built["receipt_hash"], Value::String(hash.to_string()));
    for key in [
        "version",
        "base_sha",
        "merged_sha",
        "gate_state",
        "order",
        "scope",
        "items",
        "counts",
        "catalog_version",
    ] {
        assert_eq!(
            built[key], plain[key],
            "{key} differs from the plain fixture"
        );
    }
    assert!(
        built.get("tour").is_none_or(Value::is_null),
        "the view carries no tour"
    );

    let built_machines = built["machines"]
        .as_array()
        .expect("the view lists machines");
    let plain_machines = plain["machines"]
        .as_array()
        .expect("the fixture lists machines");
    assert_eq!(built_machines.len(), plain_machines.len());
    for (built_machine, plain_machine) in built_machines.iter().zip(plain_machines) {
        let name = &plain_machine["machine_id"];
        assert_eq!(built_machine["machine_id"], *name);
        assert_eq!(
            built_machine["routes"], plain_machine["routes"],
            "{name} routes"
        );
        assert_eq!(
            built_machine["rulings"], plain_machine["rulings"],
            "{name} rulings"
        );
        assert_eq!(
            built_machine["coverage"], plain_machine["coverage"],
            "{name} coverage"
        );
        assert_eq!(
            built_machine["probes"], plain_machine["probes"],
            "{name} probes"
        );
    }
}

#[test]
fn plant_view_counts_errors_probes_from_the_receipt_alone() {
    // A machine's probes are the distinct probes of its items' route paths. The receipt carries
    // no route for a probe that produced no item, so `errors` has the one probe its item names.
    let plain = plain_view();
    let (plate, machines) = plate_and_machines(&plain);
    let view = plant_view(&fixture_receipt(), &fixture_scope(), &plate, &machines)
        .expect("the fixture builds a view");
    let errors = view
        .machines
        .iter()
        .find(|machine| machine.machine_id.as_str() == "errors")
        .expect("the plant lists errors");
    assert_eq!(errors.probes, 1);
}

#[test]
fn plant_view_refuses_a_scope_at_another_revision() {
    let plain = plain_view();
    let (plate, machines) = plate_and_machines(&plain);
    let mut scope: Value = decode(SCOPE);
    scope["rev"] = json!(3);
    let newer: Scope = serde_json::from_value(scope).expect("the scope decodes");
    assert!(matches!(
        plant_view(&fixture_receipt(), &newer, &plate, &machines),
        Err(PlantViewError::ScopeMismatch("revision"))
    ));
}

#[test]
fn plant_view_refuses_a_machine_the_plant_does_not_list() {
    let plain = plain_view();
    let (plate, mut machines) = plate_and_machines(&plain);
    machines.retain(|machine| machine.machine_id.as_str() != "errors");
    assert!(matches!(
        plant_view(&fixture_receipt(), &fixture_scope(), &plate, &machines),
        Err(PlantViewError::UnknownMachine(machine)) if machine.as_str() == "errors"
    ));
}

#[test]
fn plant_view_refuses_a_machine_listed_twice() {
    let plain = plain_view();
    let (plate, mut machines) = plate_and_machines(&plain);
    machines.push(machines[0].clone());
    assert!(matches!(
        plant_view(&fixture_receipt(), &fixture_scope(), &plate, &machines),
        Err(PlantViewError::DuplicateMachine(_))
    ));
}

/// The states an item can hold, as the receipt writes them.
const STATES: [&str; 5] = ["open", "bounced", "fixed", "residue", "stopped"];

/// The fixture receipt's JSON with the item at `index` in `state`. Counts are by class, so they
/// still match the items.
fn receipt_with_state(index: usize, state: &str) -> Value {
    let mut value: Value = decode(RECEIPT);
    value["items"][index]["state"] = json!(state);
    value
}

/// The receipt item `item_id` in the receipt's JSON.
fn item_mut<'a>(value: &'a mut Value, item_id: &str) -> &'a mut Value {
    value["items"]
        .as_array_mut()
        .expect("the receipt lists its items")
        .iter_mut()
        .find(|item| item["item_id"] == item_id)
        .expect("the receipt has the item")
}

/// The plant view of `receipt` with the fixture scope, plate and machines, and no tour.
fn view_of(receipt: &Receipt) -> PlantView {
    let plain = plain_view();
    let (plate, machines) = plate_and_machines(&plain);
    plant_view(receipt, &fixture_scope(), &plate, &machines).expect("the receipt builds a view")
}

/// The caption template and the slots bound in the step for `item_id`.
fn caption_of(tour: &TourView, item_id: &str) -> (String, Vec<String>) {
    let steps = match tour {
        TourView::Deterministic { tour, .. } | TourView::Directed { tour, .. } => &tour.steps,
    };
    let step = steps
        .iter()
        .find(|step| step.item_id.as_str() == item_id)
        .expect("the item has a step");
    (
        step.caption.template_id.as_str().to_owned(),
        step.caption.slots.keys().map(ToString::to_string).collect(),
    )
}

#[test]
fn every_item_in_every_state_has_a_deterministic_tour_but_a_fixed_obligation_failure() {
    // The receipt builder can hold any state on any item, so each item is set to each state. A
    // failed obligation that is fixed has no caption, because a fixed check passed. The catalog
    // keeps that one gap on purpose, so its view has no deterministic tour.
    let plain = plain_view();
    let (plate, machines) = plate_and_machines(&plain);
    let scope = fixture_scope();
    let items = fixture_receipt().items.len();
    let mut unbindable = Vec::new();
    for index in 0..items {
        for state in STATES {
            let receipt: Receipt = decode_value(&receipt_with_state(index, state));
            let view = plant_view(&receipt, &scope, &plate, &machines).expect("the view builds");
            match acan_intent::deterministic(&view) {
                Ok(_) => {}
                Err(DeterministicError::Unbindable { item_id, .. }) => {
                    unbindable.push((item_id.to_string(), state));
                }
                Err(other) => panic!("the deterministic tour failed: {other}"),
            }
        }
    }
    assert_eq!(unbindable, [("i-retry-after".to_owned(), "fixed")]);
}

#[test]
fn an_out_of_scope_item_with_no_base_rows_is_captioned_from_its_candidate_alone() {
    // An addition has no base to compare, so its item carries the candidate rows only.
    let mut value: Value = decode(RECEIPT);
    item_mut(&mut value, "d-links-clicks")["evidence"]
        .as_array_mut()
        .expect("the item lists its evidence")
        .retain(|row| !matches!(row["slot"].as_str(), Some("base_a" | "base_b")));
    let view = view_of(&decode_value(&value));

    let tour = acan_intent::deterministic(&view).expect("an addition has a caption");

    let (template, slots) = caption_of(&tour, "d-links-clicks");
    assert_eq!(template, "out_of_scope.candidate");
    assert_eq!(slots, ["candidate"], "only the candidate is bound");
}

#[test]
fn unknown_probe_items_with_no_evidence_are_captioned_by_their_class() {
    // An unknown probe that produced no evidence rows names its class and nothing else.
    let mut value: Value = decode(RECEIPT);
    for item in ["chk-dashboard", "chk-links-delete", "chk-signup-store"] {
        item_mut(&mut value, item)["evidence"] = json!([]);
    }
    let view = view_of(&decode_value(&value));

    let tour = acan_intent::deterministic(&view).expect("the bare captions fit");

    for (item, template) in [
        ("chk-dashboard", "unknown.unstable_base.bare"),
        ("chk-links-delete", "unknown.incomparable.bare"),
        ("chk-signup-store", "unknown.error.bare"),
    ] {
        let (bound, slots) = caption_of(&tour, item);
        assert_eq!(bound, template, "{item}");
        assert!(slots.is_empty(), "{item} binds nothing");
    }
}
