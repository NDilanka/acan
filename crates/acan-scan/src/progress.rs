//! Progress of a scan, reported while it runs: the skeleton first, then each module as its files
//! finish, then the state machines. The plant a scan returns is the same whether or not anything
//! observes it.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::model::{Belt, ImportTarget, Machine, MachineId, Plant, SourceFile};

/// The most export names one module fill carries. The rest are counted, not named.
pub const MAX_EXPORT_NAMES: usize = 16;

/// One step of a scan, reported to the observer of [`crate::scan_with`].
#[derive(Debug)]
pub enum Progress<'a> {
    /// The machines and their kinds, before any file is read. A kind comes from the names and paths
    /// only, so it does not change later. The files carry no symbols or imports yet.
    Skeleton(&'a Plant),
    /// A module whose files are all parsed. Its body and its outgoing belts are final.
    Module(ModuleFill),
    /// Every file is parsed and the state machines are resolved.
    States {
        /// The state machines found.
        machines: usize,
        /// The status-like enums found without a transition.
        candidates: usize,
    },
}

/// A module whose files are all parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModuleFill {
    pub machine: MachineId,
    pub name: String,
    /// How many files the module groups.
    pub files: u32,
    /// How many symbols its files define.
    pub symbols: u32,
    /// How many of those symbols are exported.
    pub exported: u32,
    /// How many imports its files make, to any target.
    pub imports: u32,
    /// Up to [`MAX_EXPORT_NAMES`] exported names, in file and line order.
    pub exports: Vec<String>,
    /// The belts that leave this module, one per module it imports from, sorted by target.
    pub belts: Vec<Belt>,
}

/// Builds the fill of `machine` from its files. `files` holds every file parsed so far, indexed by
/// [`crate::FileId`], and every file of `machine` must be among them. `owner` gives each file's
/// machine, indexed the same way.
pub(crate) fn module_fill(
    machine: &Machine,
    files: &[SourceFile],
    owner: &[MachineId],
) -> ModuleFill {
    let mut symbols = 0_u32;
    let mut exported = 0_u32;
    let mut imports = 0_u32;
    let mut exports = Vec::new();
    let mut counts: BTreeMap<MachineId, u32> = BTreeMap::new();
    for id in &machine.files {
        let file = &files[id.0 as usize];
        symbols = symbols.saturating_add(count(file.symbols.len()));
        for symbol in &file.symbols {
            if symbol.exported {
                exported = exported.saturating_add(1);
                if exports.len() < MAX_EXPORT_NAMES {
                    exports.push(symbol.name.clone());
                }
            }
        }
        imports = imports.saturating_add(count(file.imports.len()));
        for import in &file.imports {
            if let ImportTarget::File { file: target } = import.target {
                let to = owner[target.0 as usize];
                if to != machine.id {
                    let entry = counts.entry(to).or_insert(0);
                    *entry = entry.saturating_add(1);
                }
            }
        }
    }
    let belts = counts
        .into_iter()
        .map(|(to, imports)| Belt {
            from: machine.id,
            to,
            imports,
        })
        .collect();
    ModuleFill {
        machine: machine.id,
        name: machine.name.clone(),
        files: count(machine.files.len()),
        symbols,
        exported,
        imports,
        exports,
        belts,
    }
}

/// A count as the `u32` the fill carries. A count that large cannot come from a repo this tool scans.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The machine of each file, indexed by file id.
pub(crate) fn owners(plant: &Plant) -> Vec<MachineId> {
    let mut owner = vec![MachineId(0); plant.files.len()];
    for machine in &plant.machines {
        for id in &machine.files {
            owner[id.0 as usize] = machine.id;
        }
    }
    owner
}
