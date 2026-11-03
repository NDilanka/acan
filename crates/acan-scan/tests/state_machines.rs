//! State machine extraction over the state fixtures, the pinned acan-core files, and the demo repo.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use acan_scan::{Grouping, Plant, scan};
use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// The hand-checked `expected.json` in a fixture directory, parsed.
fn golden(dir: &str) -> Value {
    let path = fixture(dir).join("expected.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn machines_json(plant: &Plant) -> Value {
    serde_json::to_value(&plant.state_machines).expect("serialize the state machines")
}

#[test]
fn state_fixture_finds_the_expected_machines() {
    let plant = scan(fixture("states")).expect("scan the state fixture");

    assert_eq!(machines_json(&plant), golden("states"));
}

#[test]
fn state_machines_round_trip_through_plant_json() {
    let plant = scan(fixture("states")).expect("scan the state fixture");
    assert!(!plant.state_machines.is_empty());

    let json = serde_json::to_string(&plant).expect("serialize plant");
    let back: Plant = serde_json::from_str(&json).expect("deserialize plant");

    assert_eq!(back, plant);
}

/// The golden lists the moves the scanner reads today: direct assignments, match arms, and the
/// `restage` closure arms at `world.rs` 1191 and 1228. It omits `|_| Stage::Declared {..}` at
/// `world.rs` 1115, whose closure body is a value and not a match arm; the or-pattern arms at 1150
/// and 1184; the gate-verdict arms at 1478-1492, whose pattern and value name different enums; the
/// outer arm at 1477, whose value is a `match`; and the gate-result assignment at 1468, which
/// assigns a `match` expression.
#[test]
fn dogfood_acan_core_files_find_the_order_and_task_machines() {
    let plant = scan(fixture("dogfood")).expect("scan the dogfood files");

    assert_eq!(machines_json(&plant), golden("dogfood"));
}

#[test]
fn regroup_refreshes_the_module_of_each_state_machine() {
    let mut plant = scan(fixture("states")).expect("scan the state fixture");
    let module_of = |plant: &Plant, id: &str| {
        plant
            .state_machines
            .iter()
            .find(|machine| machine.id == id)
            .unwrap_or_else(|| panic!("no state machine {id}"))
            .module
            .clone()
    };
    assert_eq!(module_of(&plant, "ts/order.ts#OrderStatus"), "ts");

    let grouping = Grouping {
        assign: BTreeMap::from([("ts/order.ts".to_string(), "checkout".to_string())]),
    };
    plant.regroup(&grouping);

    assert_eq!(module_of(&plant, "ts/order.ts#OrderStatus"), "checkout");
    assert_eq!(module_of(&plant, "ts/ticket.ts#TicketStatus"), "ts");
    assert_eq!(module_of(&plant, "rust/order.rs#OrderState"), "rust");
}

#[test]
fn demo_repo_has_no_state_machines() {
    let Some(dir) = std::env::var_os("ACAN_DEMO_DIR") else {
        eprintln!("skipping the demo state machine check: ACAN_DEMO_DIR is not set");
        return;
    };
    let plant = scan(&dir).expect("scan the demo repo");

    let ids: Vec<&str> = plant
        .state_machines
        .iter()
        .map(|machine| machine.id.as_str())
        .collect();
    assert!(
        ids.is_empty(),
        "the demo repo has no state-machine code, found {ids:?}"
    );
    let json = serde_json::to_string(&plant).expect("serialize plant");
    assert!(
        json.contains("\"state_machines\":[]"),
        "an empty list still serializes"
    );
}
