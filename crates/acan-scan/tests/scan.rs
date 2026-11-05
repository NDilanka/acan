use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use acan_scan::{FileId, Grouping, ImportRef, ImportTarget, Plant, SourceFile, SymbolKind, scan};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn file<'a>(plant: &'a Plant, path: &str) -> &'a SourceFile {
    plant
        .files
        .iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("no scanned file {path}"))
}

/// (machine name, machine dir, member file paths)
fn machines(plant: &Plant) -> Vec<(String, String, Vec<String>)> {
    plant
        .machines
        .iter()
        .map(|machine| {
            let files = machine
                .files
                .iter()
                .map(|id| plant.files[id.0 as usize].path.clone())
                .collect();
            (machine.name.clone(), machine.dir.clone(), files)
        })
        .collect()
}

/// "from -> to xN" for each belt, using machine names.
fn belts(plant: &Plant) -> Vec<String> {
    plant
        .belts
        .iter()
        .map(|belt| {
            format!(
                "{} -> {} x{}",
                plant.machines[belt.from.0 as usize].name,
                plant.machines[belt.to.0 as usize].name,
                belt.imports
            )
        })
        .collect()
}

/// (specifier, target) for each import in a file. The target is the resolved path, `external`, or `unresolved`.
fn import_targets(plant: &Plant, path: &str) -> Vec<(String, String)> {
    file(plant, path)
        .imports
        .iter()
        .map(|import| {
            let target = match &import.target {
                ImportTarget::File { file } => plant.files[file.0 as usize].path.clone(),
                ImportTarget::External => "external".to_string(),
                ImportTarget::Unresolved => "unresolved".to_string(),
            };
            (import.specifier.clone(), target)
        })
        .collect()
}

/// Every exported symbol as (path, name, kind), in file order.
fn exported(plant: &Plant) -> Vec<(String, String, SymbolKind)> {
    plant
        .files
        .iter()
        .flat_map(|file| {
            file.symbols
                .iter()
                .filter(|symbol| symbol.exported)
                .map(|symbol| (file.path.clone(), symbol.name.clone(), symbol.kind))
        })
        .collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

#[test]
fn ts_fixture_has_expected_machines_belts_and_exports() {
    let plant = scan(fixture("ts-app")).expect("scan the TypeScript fixture");

    assert_eq!(plant.files.len(), 8);
    assert_eq!(
        machines(&plant),
        vec![
            (
                "model".to_string(),
                "model".to_string(),
                strings(&["model/store.ts", "model/todo.ts"]),
            ),
            (
                "ui".to_string(),
                "ui".to_string(),
                strings(&["ui/button.tsx", "ui/index.ts", "ui/list.ts"]),
            ),
            (
                "util".to_string(),
                "util".to_string(),
                strings(&["util/format.ts", "util/index.ts", "util/legacy.js"]),
            ),
        ]
    );
    assert_eq!(
        belts(&plant),
        vec![
            "model -> util x1",
            "ui -> model x1",
            "ui -> util x1",
            "util -> model x1",
        ]
    );

    let expected: Vec<(String, String, SymbolKind)> = [
        ("model/store.ts", "Store", SymbolKind::Class),
        ("model/store.ts", "add", SymbolKind::Method),
        ("model/store.ts", "titles", SymbolKind::Method),
        ("model/todo.ts", "Todo", SymbolKind::Interface),
        ("model/todo.ts", "TodoId", SymbolKind::Type),
        ("model/todo.ts", "makeTodo", SymbolKind::Function),
        ("model/todo.ts", "Clock", SymbolKind::Class),
        ("model/todo.ts", "tick", SymbolKind::Method),
        ("model/todo.ts", "Timer", SymbolKind::Export),
        ("ui/button.tsx", "ButtonProps", SymbolKind::Interface),
        ("ui/button.tsx", "Button", SymbolKind::Class),
        ("ui/button.tsx", "render", SymbolKind::Method),
        ("ui/index.ts", "list", SymbolKind::Export),
        ("ui/list.ts", "renderList", SymbolKind::Function),
        ("util/format.ts", "format", SymbolKind::Function),
        ("util/index.ts", "format", SymbolKind::Export),
    ]
    .iter()
    .map(|(path, name, kind)| (path.to_string(), name.to_string(), *kind))
    .collect();
    assert_eq!(exported(&plant), expected);
}

#[test]
fn ts_fixture_keeps_private_symbols_and_package_imports_out_of_belts() {
    let plant = scan(fixture("ts-app")).expect("scan the TypeScript fixture");

    let todo = file(&plant, "model/todo.ts");
    assert!(
        todo.symbols.iter().any(|symbol| symbol.name == "nextId"
            && !symbol.exported
            && symbol.kind == SymbolKind::Const),
        "nextId should be a non-exported const"
    );
    for name in ["Hidden", "secret"] {
        let symbol = todo
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("{name} is indexed"));
        assert!(
            !symbol.exported,
            "{name} is not reachable from outside the module"
        );
    }

    assert_eq!(
        import_targets(&plant, "ui/button.tsx"),
        vec![
            ("react".to_string(), "external".to_string()),
            ("../util".to_string(), "util/index.ts".to_string()),
        ]
    );
    assert_eq!(
        import_targets(&plant, "util/index.ts"),
        vec![("./format".to_string(), "util/format.ts".to_string())]
    );
    assert_eq!(
        import_targets(&plant, "util/legacy.js"),
        vec![("../model/todo".to_string(), "model/todo.ts".to_string())]
    );
}

#[test]
fn rust_fixture_resolves_mod_and_crate_use_edges() {
    let plant = scan(fixture("rust-crate")).expect("scan the Rust fixture");

    assert_eq!(
        machines(&plant),
        vec![
            (
                "src".to_string(),
                "src".to_string(),
                strings(&["src/c.rs", "src/lib.rs"]),
            ),
            (
                "src/a".to_string(),
                "src/a".to_string(),
                strings(&["src/a/b.rs", "src/a/mod.rs"]),
            ),
        ]
    );
    assert_eq!(belts(&plant), vec!["src -> src/a x4", "src/a -> src x1"]);

    assert_eq!(
        import_targets(&plant, "src/lib.rs"),
        vec![
            ("a".to_string(), "src/a/mod.rs".to_string()),
            ("c".to_string(), "src/c.rs".to_string()),
            ("crate::a::b::Thing".to_string(), "src/a/b.rs".to_string()),
        ]
    );
    assert_eq!(
        import_targets(&plant, "src/c.rs"),
        vec![
            ("crate::a::b".to_string(), "src/a/b.rs".to_string()),
            ("crate::a::Thing".to_string(), "src/a/mod.rs".to_string()),
        ]
    );
    assert_eq!(
        import_targets(&plant, "src/a/b.rs"),
        vec![("crate::c::helper".to_string(), "src/c.rs".to_string())]
    );

    let expected: Vec<(String, String, SymbolKind)> = [
        ("src/a/b.rs", "Thing", SymbolKind::Struct),
        ("src/a/b.rs", "label", SymbolKind::Method),
        ("src/a/b.rs", "make", SymbolKind::Function),
        ("src/c.rs", "helper", SymbolKind::Function),
        ("src/lib.rs", "LIMIT", SymbolKind::Const),
    ]
    .iter()
    .map(|(path, name, kind)| (path.to_string(), name.to_string(), *kind))
    .collect();
    assert_eq!(exported(&plant), expected);

    let hidden = file(&plant, "src/a/b.rs")
        .symbols
        .iter()
        .find(|symbol| symbol.name == "hidden")
        .expect("private method is indexed");
    assert!(!hidden.exported);
    assert_eq!(hidden.kind, SymbolKind::Method);
}

#[test]
fn scan_respects_gitignore_and_skips_vendored_dirs() {
    let repo = tempfile::tempdir().expect("create temp repo");
    let write = |path: &str, text: &str| {
        let full = repo.path().join(path);
        fs::create_dir_all(full.parent().expect("parent dir")).expect("create dirs");
        fs::write(full, text).expect("write fixture file");
    };
    write(".gitignore", "generated/\n");
    write("src/a.ts", "export const a = 1;\n");
    write(
        "src/b.ts",
        "import { a } from './a';\nimport { x } from './missing';\nimport pkg from 'some-package';\nexport const b = a;\n",
    );
    write("generated/g.ts", "export const g = 1;\n");
    write("node_modules/pkg/index.ts", "export const p = 1;\n");
    write("dist/out.js", "export const o = 1;\n");

    let plant = scan(repo.path()).expect("scan the temp repo");

    let paths: Vec<&str> = plant.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, vec!["src/a.ts", "src/b.ts"]);
    assert_eq!(
        import_targets(&plant, "src/b.ts"),
        vec![
            ("./a".to_string(), "src/a.ts".to_string()),
            ("./missing".to_string(), "unresolved".to_string()),
            ("some-package".to_string(), "external".to_string()),
        ]
    );
}

#[test]
fn regroup_moves_files_between_machines_without_rescanning() {
    let mut plant = scan(fixture("ts-app")).expect("scan the TypeScript fixture");

    let grouping = Grouping {
        assign: BTreeMap::from([("util/legacy.js".to_string(), "ui".to_string())]),
    };
    plant.regroup(&grouping);

    assert_eq!(
        machines(&plant),
        vec![
            (
                "model".to_string(),
                "model".to_string(),
                strings(&["model/store.ts", "model/todo.ts"]),
            ),
            (
                "ui".to_string(),
                ".".to_string(),
                strings(&[
                    "ui/button.tsx",
                    "ui/index.ts",
                    "ui/list.ts",
                    "util/legacy.js"
                ]),
            ),
            (
                "util".to_string(),
                "util".to_string(),
                strings(&["util/format.ts", "util/index.ts"]),
            ),
        ]
    );
    assert_eq!(
        belts(&plant),
        vec!["model -> util x1", "ui -> model x2", "ui -> util x1"]
    );
}

#[test]
fn default_export_of_a_local_binding_exports_that_binding() {
    let repo = tempfile::tempdir().expect("create temp repo");
    fs::write(
        repo.path().join("bench.js"),
        "class Bench {}\nfunction helper() {}\nexport default Bench;\n",
    )
    .expect("write bench.js");

    let plant = scan(repo.path()).expect("scan the temp repo");

    assert_eq!(
        exported(&plant),
        vec![
            (
                "bench.js".to_string(),
                "Bench".to_string(),
                SymbolKind::Class
            ),
            (
                "bench.js".to_string(),
                "default".to_string(),
                SymbolKind::Export
            ),
        ]
    );
}

#[test]
fn regroup_ignores_imports_that_point_outside_the_plant() {
    let mut plant = scan(fixture("ts-app")).expect("scan the TypeScript fixture");
    let list = plant
        .files
        .iter_mut()
        .find(|file| file.path == "ui/list.ts")
        .expect("ui/list.ts is scanned");
    list.imports.push(ImportRef {
        specifier: "./ghost".to_string(),
        line: 1,
        target: ImportTarget::File { file: FileId(99) },
    });

    plant.regroup(&Grouping::default());

    assert_eq!(
        belts(&plant),
        vec![
            "model -> util x1",
            "ui -> model x1",
            "ui -> util x1",
            "util -> model x1",
        ]
    );
}

#[test]
fn revision_moves_with_content_and_holds_without_changes() {
    let repo = tempfile::tempdir().expect("create temp repo");
    let a = repo.path().join("a.ts");
    let b = repo.path().join("b.ts");
    fs::write(&a, "export const a = 1;\n").expect("write a.ts");
    fs::write(&b, "export const b = 2;\n").expect("write b.ts");

    let first = scan(repo.path()).expect("scan the temp repo");
    let again = scan(repo.path()).expect("rescan the unchanged repo");
    assert_eq!(
        again.revision, first.revision,
        "revision holds when nothing changed"
    );

    fs::write(&b, "export const b = 3;\n").expect("edit b.ts");
    let edited = scan(repo.path()).expect("scan the edited repo");
    assert_ne!(
        edited.revision, first.revision,
        "revision moves when b.ts changes"
    );
    assert_eq!(
        file(&edited, "a.ts").content_hash,
        file(&first, "a.ts").content_hash,
        "a.ts content hash holds"
    );
    assert_ne!(
        file(&edited, "b.ts").content_hash,
        file(&first, "b.ts").content_hash,
        "b.ts content hash moves"
    );

    fs::write(&b, "export const b = 2;\n").expect("restore b.ts");
    let restored = scan(repo.path()).expect("scan the restored repo");
    assert_eq!(
        restored.revision, first.revision,
        "revision depends on content, not on write time"
    );
}

#[test]
fn plant_round_trips_through_json() {
    let plant = scan(fixture("rust-crate")).expect("scan the Rust fixture");

    let json = serde_json::to_string(&plant).expect("serialize plant");
    assert!(
        json.contains(&format!("\"revision\":\"{}\"", plant.revision)),
        "revision serializes as a hex string"
    );
    let back: Plant = serde_json::from_str(&json).expect("deserialize plant");

    assert_eq!(back, plant);
}

#[test]
fn deeply_nested_use_tree_does_not_abort_the_scan() {
    let repo = tempfile::tempdir().unwrap();
    let source = format!(
        "use {}leaf{};\npub fn good() {{}}",
        "a::{".repeat(16_000),
        "}".repeat(16_000)
    );
    fs::write(repo.path().join("deep.rs"), source).unwrap();
    fs::write(repo.path().join("good.rs"), "pub fn survives() {}").unwrap();
    let plant = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || scan(repo.path()).unwrap())
        .unwrap()
        .join()
        .unwrap();
    assert!(
        file(&plant, "good.rs")
            .symbols
            .iter()
            .any(|s| s.name == "survives")
    );
    if let Some(deep) = plant.files.iter().find(|f| f.path == "deep.rs") {
        assert!(deep.imports.is_empty());
    } else {
        assert!(plant.skipped.iter().any(|f| f.path == "deep.rs"));
    }
}
