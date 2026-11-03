//! Machine kinds from a real scan: the rules on named machines, the override file, and the writer.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use acan_scan::{
    Grouping, Kind, Machine, OVERRIDES_PATH, Plant, ScanError, scan, write_kind_overrides,
};

const FILES: [(&str, &str); 4] = [
    ("src/store.ts", "export const store = 1;\n"),
    ("src/routes.ts", "export const routes = 2;\n"),
    ("src/errors.ts", "export const errors = 3;\n"),
    ("src/panel.ts", "export const panel = 4;\n"),
];

/// A repo with one machine per file, named after the file: store, routes, errors and panel.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    for (path, text) in FILES {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().expect("a parent")).expect("create src");
        fs::write(full, text).expect("write file");
    }
    dir
}

fn named_grouping() -> Grouping {
    Grouping {
        assign: FILES
            .iter()
            .map(|(path, _)| {
                let name = path.trim_start_matches("src/").trim_end_matches(".ts");
                ((*path).to_owned(), name.to_owned())
            })
            .collect(),
    }
}

/// The scan of `dir`, grouped so each machine is named after its file.
fn grouped(dir: &Path) -> Plant {
    let mut plant = scan(dir).expect("scan the repo");
    plant.regroup(&named_grouping());
    plant
}

fn machine<'a>(plant: &'a Plant, name: &str) -> &'a Machine {
    plant
        .machines
        .iter()
        .find(|machine| machine.name == name)
        .unwrap_or_else(|| panic!("no machine named {name}"))
}

#[test]
fn a_scan_gives_each_machine_its_rule_kind_and_reason() {
    let dir = repo();
    let plant = grouped(dir.path());

    let store = machine(&plant, "store");
    assert_eq!(store.kind, Kind::Tank);
    assert!(store.kind_guessed);
    assert_eq!(store.kind_reason, "name word \"store\"");
    assert!(!store.kind_set_by_user);

    assert_eq!(machine(&plant, "routes").kind, Kind::Switchboard);
    assert_eq!(machine(&plant, "errors").kind, Kind::Assembler);
    assert!(!machine(&plant, "errors").kind_guessed);
    assert_eq!(
        machine(&plant, "errors").kind_reason,
        "no rule matched; default"
    );
    assert!(plant.kind_overrides.is_empty());
}

#[test]
fn the_override_file_sets_a_kind_by_machine_name() {
    let dir = repo();
    fs::create_dir_all(dir.path().join(".acan")).expect("create .acan");
    fs::write(
        dir.path().join(OVERRIDES_PATH),
        r#"{"kinds": {"store": "switchboard", "errors": "console", "panel": "not_a_kind"}}"#,
    )
    .expect("write overrides");

    let plant = grouped(dir.path());

    let store = machine(&plant, "store");
    assert_eq!(store.kind, Kind::Switchboard);
    assert!(store.kind_set_by_user);
    assert!(!store.kind_guessed, "a user's kind has no groove break");
    assert_eq!(store.kind_reason, "set by you");

    assert_eq!(machine(&plant, "errors").kind, Kind::Console);
    assert!(machine(&plant, "errors").kind_set_by_user);

    // A name that is no kind is ignored, so the machine keeps its rule's kind.
    let panel = machine(&plant, "panel");
    assert_eq!(panel.kind, Kind::Assembler);
    assert!(!panel.kind_set_by_user);
    assert!(!plant.kind_overrides.contains_key("panel"));
}

#[test]
fn overrides_survive_a_regroup_and_follow_the_name() {
    let dir = repo();
    write_kind_overrides(
        dir.path(),
        &BTreeMap::from([("store".to_owned(), Some(Kind::Gate))]),
    )
    .expect("write overrides");
    let mut plant = grouped(dir.path());
    assert_eq!(machine(&plant, "store").kind, Kind::Gate);

    // Regrouping with the same names keeps the override.
    plant.regroup(&named_grouping());
    assert_eq!(machine(&plant, "store").kind, Kind::Gate);

    // A machine renamed by the grouping no longer matches the override, so it falls back to its rule.
    let mut renamed = named_grouping();
    renamed
        .assign
        .insert("src/store.ts".to_owned(), "shop".to_owned());
    plant.regroup(&renamed);
    let shop = machine(&plant, "shop");
    assert_eq!(shop.kind, Kind::Assembler);
    assert!(!shop.kind_set_by_user);
}

#[test]
fn the_writer_sets_clears_and_keeps_other_entries() {
    let dir = repo();
    fs::create_dir_all(dir.path().join(".acan")).expect("create .acan");
    fs::write(
        dir.path().join(OVERRIDES_PATH),
        r#"{"kinds": {"legacy": "a_kind_from_the_future", "routes": "gate"}}"#,
    )
    .expect("write overrides");

    write_kind_overrides(
        dir.path(),
        &BTreeMap::from([
            ("store".to_owned(), Some(Kind::Conveyor)),
            ("routes".to_owned(), None),
        ]),
    )
    .expect("write overrides");

    let text = fs::read_to_string(dir.path().join(OVERRIDES_PATH)).expect("read overrides");
    let file: serde_json::Value = serde_json::from_str(&text).expect("a JSON file");
    assert_eq!(file["kinds"]["store"], "conveyor");
    assert_eq!(
        file["kinds"]["legacy"], "a_kind_from_the_future",
        "unknown entries stay"
    );
    assert!(
        file["kinds"].get("routes").is_none(),
        "a None clears the entry"
    );

    let plant = grouped(dir.path());
    assert_eq!(machine(&plant, "store").kind, Kind::Conveyor);
    assert!(machine(&plant, "store").kind_set_by_user);
    assert_eq!(
        machine(&plant, "routes").kind,
        Kind::Switchboard,
        "back to its rule"
    );
}

#[test]
fn a_malformed_overrides_file_is_an_error_naming_the_file() {
    let dir = repo();
    fs::create_dir_all(dir.path().join(".acan")).expect("create .acan");
    fs::write(dir.path().join(OVERRIDES_PATH), "{ not json").expect("write overrides");

    let error = scan(dir.path()).expect_err("a broken file fails the scan");
    match error {
        ScanError::Overrides { path, .. } => assert!(path.ends_with("kinds.json")),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_plant_without_kinds_in_its_json_reads_as_before() {
    // Old plant JSON has no kind fields. The new fields are optional, so the machine still parses.
    let dir = repo();
    let plant = grouped(dir.path());
    let mut value = serde_json::to_value(&plant).expect("serialize the plant");
    for machine in value["machines"].as_array_mut().expect("machines") {
        let object = machine.as_object_mut().expect("a machine");
        object.remove("kind");
        object.remove("kind_reason");
    }
    let old: Plant = serde_json::from_value(value).expect("old JSON still parses");
    assert!(
        old.machines
            .iter()
            .all(|machine| machine.kind == Kind::Assembler)
    );
}
