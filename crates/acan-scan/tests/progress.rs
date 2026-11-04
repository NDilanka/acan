use std::fs;
use std::path::{Path, PathBuf};

use acan_scan::{MAX_EXPORT_NAMES, ModuleFill, Plant, Progress, scan, scan_with, skeleton};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Everything a scan reports, in the order it reports it.
#[derive(Debug)]
enum Step {
    Skeleton(Plant),
    Module(ModuleFill),
    States { machines: usize, candidates: usize },
}

fn observed(root: &Path) -> (Plant, Vec<Step>) {
    let mut steps = Vec::new();
    let plant = scan_with(root, |progress| {
        steps.push(match progress {
            Progress::Skeleton(plant) => Step::Skeleton(plant.clone()),
            Progress::Module(fill) => Step::Module(fill),
            Progress::States {
                machines,
                candidates,
            } => Step::States {
                machines,
                candidates,
            },
        });
    })
    .expect("scan the repo");
    (plant, steps)
}

/// A repo of `files` TypeScript files spread over `dirs` directories, each exporting one function.
fn repo_with(files: usize, dirs: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    for index in 0..files {
        let folder = dir.path().join(format!("area{}", index % dirs));
        fs::create_dir_all(&folder).expect("create the folder");
        let text = format!(
            "import {{ f{} }} from \"./f{}\";\nexport function f{index}() {{}}\n",
            (index + 1) % files,
            (index + 1) % files
        );
        fs::write(folder.join(format!("f{index}.ts")), text).expect("write the file");
    }
    dir
}

#[test]
fn the_observer_sees_the_skeleton_first_then_each_module_then_the_states() {
    for name in ["ts-app", "rust-crate", "states"] {
        let (plant, steps) = observed(&fixture(name));
        assert!(
            matches!(steps.first(), Some(Step::Skeleton(_))),
            "{name}: the skeleton comes first"
        );
        let Some(Step::States {
            machines,
            candidates,
        }) = steps.last()
        else {
            panic!("{name}: the states come last");
        };
        assert_eq!(
            *machines,
            plant.state_machines.len(),
            "{name}: state machines"
        );
        assert_eq!(
            *candidates,
            plant.state_candidates.len(),
            "{name}: candidates"
        );
        let modules: Vec<&ModuleFill> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Module(fill) => Some(fill),
                _ => None,
            })
            .collect();
        assert_eq!(
            modules.len(),
            plant.machines.len(),
            "{name}: one module fill per machine"
        );
        let mut seen: Vec<u32> = modules.iter().map(|fill| fill.machine.0).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            plant.machines.len(),
            "{name}: each machine fills once"
        );
    }
}

#[test]
fn the_skeleton_has_the_final_machines_and_kinds_before_any_file_is_parsed() {
    for root in [fixture("ts-app"), fixture("states")] {
        let (plant, steps) = observed(&root);
        let Some(Step::Skeleton(skeleton_plant)) = steps.first() else {
            panic!("the skeleton comes first");
        };
        assert_eq!(
            skeleton_plant.machines,
            plant.machines,
            "{}: machines and kinds are final in the skeleton",
            root.display()
        );
        assert!(
            skeleton_plant
                .files
                .iter()
                .all(|file| file.symbols.is_empty() && file.imports.is_empty()),
            "the skeleton's files carry no symbols or imports"
        );
        assert!(
            skeleton_plant.belts.is_empty(),
            "the skeleton draws no belts"
        );
    }
}

#[test]
fn the_skeleton_matches_the_final_machines_on_a_repo_grouped_by_directory() {
    // Forty-five files is past the small-repo limit, so the scan groups by directory.
    let repo = repo_with(45, 3);
    let (plant, steps) = observed(repo.path());
    let Some(Step::Skeleton(skeleton_plant)) = steps.first() else {
        panic!("the skeleton comes first");
    };
    assert_eq!(skeleton_plant.machines, plant.machines);
    assert_eq!(plant.machines.len(), 3, "one machine per directory");
}

#[test]
fn the_module_fills_add_up_to_the_final_plant() {
    for name in ["ts-app", "rust-crate", "states"] {
        let (plant, steps) = observed(&fixture(name));
        let mut fills: Vec<&ModuleFill> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Module(fill) => Some(fill),
                _ => None,
            })
            .collect();
        fills.sort_by_key(|fill| fill.machine);

        let symbols: u32 = fills.iter().map(|fill| fill.symbols).sum();
        let total_symbols: usize = plant.files.iter().map(|file| file.symbols.len()).sum();
        assert_eq!(symbols as usize, total_symbols, "{name}: symbols add up");

        let imports: u32 = fills.iter().map(|fill| fill.imports).sum();
        let total_imports: usize = plant.files.iter().map(|file| file.imports.len()).sum();
        assert_eq!(imports as usize, total_imports, "{name}: imports add up");

        let belts: Vec<_> = fills.iter().flat_map(|fill| fill.belts.clone()).collect();
        assert_eq!(
            belts, plant.belts,
            "{name}: the belts leaving each module are the plant's belts"
        );
    }
}

#[test]
fn a_module_names_its_exports_up_to_the_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let text: String = (0..MAX_EXPORT_NAMES + 4)
        .map(|index| format!("export function exported{index}() {{}}\n"))
        .collect();
    fs::write(dir.path().join("many.ts"), text).expect("write the file");
    let (_, steps) = observed(dir.path());
    let fill = steps
        .iter()
        .find_map(|step| match step {
            Step::Module(fill) => Some(fill),
            _ => None,
        })
        .expect("one module");
    assert_eq!(
        fill.exported as usize,
        MAX_EXPORT_NAMES + 4,
        "every export is counted"
    );
    assert_eq!(
        fill.exports.len(),
        MAX_EXPORT_NAMES,
        "only the cap is named"
    );
    assert_eq!(
        fill.exports[0], "exported0",
        "names come in file and line order"
    );
}

#[test]
fn the_returned_plant_is_the_plant_scan_returns() {
    for name in ["ts-app", "rust-crate", "states"] {
        let root = fixture(name);
        let plain = scan(&root).expect("scan");
        let (observed_plant, _) = observed(&root);
        assert_eq!(
            observed_plant, plain,
            "{name}: observing does not change the plant"
        );
    }
}

#[test]
fn the_skeleton_reads_no_file_contents() {
    let dir = tempfile::tempdir().expect("temp dir");
    // Not UTF-8, so a scan that reads the file fails. The skeleton only needs its name.
    fs::write(dir.path().join("bad.ts"), [0xff_u8, 0xfe, 0x00]).expect("write the file");
    let plant = skeleton(dir.path()).expect("the skeleton of an unreadable file");
    assert_eq!(plant.files.len(), 1);
    assert_eq!(plant.machines.len(), 1);
    assert!(
        scan(dir.path()).is_err(),
        "the full scan still reads the file"
    );
}
