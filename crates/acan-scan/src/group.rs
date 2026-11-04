use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::kind;
use crate::model::{Belt, FileId, ImportTarget, Machine, MachineId, Plant, SourceFile};

/// A repo with at most this many source files splits its large directories into modules.
/// A bigger repo groups every file by its directory.
const SMALL_REPO_FILES: usize = 40;
/// In a small repo, a directory with at least this many files gives one machine per file.
const MODULE_DIR_FILES: usize = 5;
/// Directory names whose files stay one machine however many there are.
const TEST_DIRS: [&str; 4] = ["test", "tests", "__tests__", "spec"];

/// Assigns files to machines by name. Files not listed here group by their module or directory.
///
/// The foreman edits this data and calls [`Plant::regroup`]; the file list and symbols stay the same.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grouping {
    /// Repo-relative file path to machine name.
    #[serde(default)]
    pub assign: BTreeMap<String, String>,
}

impl Plant {
    /// Recomputes machines, belts, and each state machine's module from the files under `grouping`.
    pub fn regroup(&mut self, grouping: &Grouping) {
        let (mut machines, belts) = build(&self.files, grouping);
        for machine in &mut machines {
            if let Some(&kind) = self.kind_overrides.get(&machine.name) {
                kind::set_by_user(machine, kind);
            }
        }
        self.machines = machines;
        self.belts = belts;
        assign_modules(self);
    }
}

/// Sets each state machine's module to the name of the plant machine whose files include its file.
fn assign_modules(plant: &mut Plant) {
    let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
    for machine in &plant.machines {
        for id in &machine.files {
            let path = plant.files[id.0 as usize].path.as_str();
            owner.insert(path, machine.name.as_str());
        }
    }
    let modules: Vec<String> = plant
        .state_machines
        .iter()
        .map(|machine| {
            owner
                .get(machine.file.as_str())
                .map_or_else(String::new, |name| (*name).to_string())
        })
        .collect();
    for (machine, module) in plant.state_machines.iter_mut().zip(modules) {
        machine.module = module;
    }
}

fn build(files: &[SourceFile], grouping: &Grouping) -> (Vec<Machine>, Vec<Belt>) {
    let automatic = automatic_names(files);
    let mut groups: BTreeMap<String, Vec<&SourceFile>> = BTreeMap::new();
    for file in files {
        let name = grouping
            .assign
            .get(&file.path)
            .cloned()
            .unwrap_or_else(|| automatic[file.path.as_str()].clone());
        groups.entry(name).or_default().push(file);
    }

    let mut machine_of: BTreeMap<FileId, MachineId> = BTreeMap::new();
    let mut machines = Vec::with_capacity(groups.len());
    for (index, (name, members)) in groups.into_iter().enumerate() {
        let id = MachineId(index as u32);
        for file in &members {
            machine_of.insert(file.id, id);
        }
        let mut lang_mix = BTreeMap::new();
        for file in &members {
            *lang_mix.entry(file.lang).or_insert(0) += 1;
        }
        let dir = common_directory(members.iter().map(|file| file.path.as_str()));
        let paths: Vec<&str> = members.iter().map(|file| file.path.as_str()).collect();
        let decision = acan_iso::classify(&name, &dir, &paths);
        machines.push(Machine {
            id,
            name,
            dir,
            files: members.iter().map(|file| file.id).collect(),
            lang_mix,
            kind: decision.kind,
            kind_guessed: decision.guessed,
            kind_reason: decision.reason,
            kind_set_by_user: false,
        });
    }

    let mut counts: BTreeMap<(MachineId, MachineId), u32> = BTreeMap::new();
    for file in files {
        let Some(&from) = machine_of.get(&file.id) else {
            continue;
        };
        for import in &file.imports {
            if let ImportTarget::File { file: target } = import.target {
                let Some(&to) = machine_of.get(&target) else {
                    continue;
                };
                if to != from {
                    *counts.entry((from, to)).or_insert(0) += 1;
                }
            }
        }
    }
    let belts = counts
        .into_iter()
        .map(|((from, to), imports)| Belt { from, to, imports })
        .collect();
    (machines, belts)
}

/// The machine name each file gets when the grouping does not assign it, keyed by path.
///
/// In a small repo, each file of a large non-test directory is its own machine, named by its
/// file stem. A stem that another machine already uses falls back to the path without its
/// extension, and a name still in use gets a numeric suffix.
fn automatic_names(files: &[SourceFile]) -> BTreeMap<&str, String> {
    let mut dir_sizes: BTreeMap<&str, usize> = BTreeMap::new();
    for file in files {
        *dir_sizes.entry(directory_of(&file.path)).or_insert(0) += 1;
    }
    let small = files.len() <= SMALL_REPO_FILES;
    let splits = |path: &str| {
        small
            && dir_sizes.get(directory_of(path)).copied().unwrap_or(0) >= MODULE_DIR_FILES
            && !is_test_path(path)
    };

    let mut names: BTreeMap<&str, String> = BTreeMap::new();
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut modules: Vec<&str> = Vec::new();
    for file in files {
        let path = file.path.as_str();
        if splits(path) {
            modules.push(path);
        } else {
            let dir = directory_of(path);
            names.insert(path, dir.to_string());
            used.insert(dir.to_string());
        }
    }

    let mut stem_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for path in &modules {
        *stem_counts.entry(stem(path)).or_insert(0) += 1;
    }
    modules.sort_unstable();
    for path in modules {
        let plain = stem(path);
        let candidate = if stem_counts[plain] == 1 && !used.contains(plain) {
            plain.to_string()
        } else {
            without_extension(path).to_string()
        };
        names.insert(path, unique(candidate, &mut used));
    }
    names
}

fn unique(name: String, used: &mut BTreeSet<String>) -> String {
    let mut candidate = name.clone();
    let mut suffix = 2;
    while used.contains(&candidate) {
        candidate = format!("{name}-{suffix}");
        suffix += 1;
    }
    used.insert(candidate.clone());
    candidate
}

fn is_test_path(path: &str) -> bool {
    directory_of(path)
        .split('/')
        .any(|part| TEST_DIRS.contains(&part))
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// The file name without its extension. A leading dot is part of the name, not an extension.
fn stem(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

fn without_extension(path: &str) -> &str {
    let extension = file_name(path).len() - stem(path).len();
    &path[..path.len() - extension]
}

fn directory_of(path: &str) -> &str {
    path.rsplit_once('/').map_or(".", |(dir, _)| dir)
}

fn common_directory<'a>(paths: impl Iterator<Item = &'a str>) -> String {
    let mut common: Option<Vec<&str>> = None;
    for path in paths {
        let dir: Vec<&str> = directory_of(path)
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        common = Some(match common {
            None => dir,
            Some(previous) => previous
                .iter()
                .zip(&dir)
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    let parts = common.unwrap_or_default();
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}
