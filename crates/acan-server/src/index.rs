//! Scans the repository and adapts the scan into the code index that clash detection reads.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;

use acan_core::{
    Belt, CodeIndex, CodeRev, FilePath, IndexedFile, IndexedSymbol, MachineKey, ParseIdError,
};
use acan_scan::{FileId, MachineId, Plant, Symbol};
use anyhow::Context;

/// A scanned repository: the plant the UI draws and the code index detection reads.
#[derive(Debug)]
pub struct Scan {
    /// Machines, belts, files and symbols, as the scanner groups them.
    pub plant: Plant,
    /// The same code, keyed the way acan-core reads it.
    pub index: CodeIndex,
}

/// Scans `repo` and builds both views of it.
///
/// # Errors
///
/// Returns an error when the scan fails or the plant cannot be expressed as a code index.
pub fn scan(repo: &Path) -> anyhow::Result<Scan> {
    scan_observed(repo, |_| {})
}

/// Scans `repo` like [`scan`], and reports each step of the scan to `observe` as it happens.
///
/// # Errors
///
/// Returns the errors [`scan`] returns.
pub fn scan_observed(
    repo: &Path,
    observe: impl FnMut(acan_scan::Progress<'_>),
) -> anyhow::Result<Scan> {
    let plant = acan_scan::scan_with(repo, observe)
        .with_context(|| format!("scanning {}", repo.display()))?;
    let index = code_index(&plant)?;
    Ok(Scan { plant, index })
}

/// The machines and kinds of `repo`, from its file names and paths alone. Its index has no
/// symbols and no belts, so it is the first scan a server can serve while the full one runs.
///
/// # Errors
///
/// Returns an error when the directory walk fails or the plant cannot be expressed as an index.
pub fn skeleton(repo: &Path) -> anyhow::Result<Scan> {
    let plant = acan_scan::skeleton(repo).with_context(|| format!("mapping {}", repo.display()))?;
    let index = code_index(&plant)?;
    Ok(Scan { plant, index })
}

/// Why a plant cannot be expressed as a [`CodeIndex`].
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The plant's revision is not a valid code revision.
    #[error("plant revision is not a valid code revision")]
    Revision(#[source] ParseIdError),
    /// A machine name is not a valid machine key.
    #[error("machine name {name:?} is not a valid machine key")]
    Machine {
        /// The rejected name.
        name: String,
        #[source]
        source: ParseIdError,
    },
    /// A file path is not a valid repo-relative path.
    #[error("file path {path:?} is not a valid repo-relative path")]
    Path {
        /// The rejected path.
        path: String,
        #[source]
        source: ParseIdError,
    },
    /// A file belongs to no machine. The scanner always groups every file, so this is a scanner bug.
    #[error("file {path:?} belongs to no machine")]
    Ungrouped {
        /// The file.
        path: String,
    },
    /// A belt names a machine that the plant does not list.
    #[error("belt names machine {id:?}, which the plant does not list")]
    UnknownMachine {
        /// The machine id.
        id: MachineId,
    },
    /// A symbol's byte range does not fit in the `u32` offsets the index uses.
    #[error("symbol {symbol:?} in {path:?} has a byte range beyond u32")]
    Range {
        /// The file.
        path: String,
        /// The symbol.
        symbol: String,
    },
}

/// Builds the code index for `plant`. Equal plant revisions give equal index revisions.
///
/// # Errors
///
/// Returns [`IndexError`] when a name, path or byte range cannot be represented in the index.
pub fn code_index(plant: &Plant) -> Result<CodeIndex, IndexError> {
    let rev = CodeRev::from_str(&plant.revision.to_string()).map_err(IndexError::Revision)?;

    let mut machine_keys: BTreeMap<MachineId, MachineKey> = BTreeMap::new();
    let mut file_machines: BTreeMap<FileId, MachineKey> = BTreeMap::new();
    for machine in &plant.machines {
        let key = MachineKey::from_str(&machine.name).map_err(|source| IndexError::Machine {
            name: machine.name.clone(),
            source,
        })?;
        for file in &machine.files {
            file_machines.insert(*file, key.clone());
        }
        machine_keys.insert(machine.id, key);
    }

    let mut files = BTreeMap::new();
    for file in &plant.files {
        let path = FilePath::from_str(&file.path).map_err(|source| IndexError::Path {
            path: file.path.clone(),
            source,
        })?;
        let machine =
            file_machines
                .get(&file.id)
                .cloned()
                .ok_or_else(|| IndexError::Ungrouped {
                    path: file.path.clone(),
                })?;
        let symbols = file
            .symbols
            .iter()
            .map(|symbol| indexed_symbol(&file.path, symbol))
            .collect::<Result<Vec<_>, _>>()?;
        files.insert(path, IndexedFile { machine, symbols });
    }

    let belts = plant
        .belts
        .iter()
        .map(|belt| {
            Ok(Belt {
                from: machine_key(&machine_keys, belt.from)?,
                to: machine_key(&machine_keys, belt.to)?,
            })
        })
        .collect::<Result<BTreeSet<_>, IndexError>>()?;

    Ok(CodeIndex { rev, files, belts })
}

fn indexed_symbol(path: &str, symbol: &Symbol) -> Result<IndexedSymbol, IndexError> {
    let start = u32::try_from(symbol.byte_range.start);
    let end = u32::try_from(symbol.byte_range.end);
    let (Ok(start), Ok(end)) = (start, end) else {
        return Err(IndexError::Range {
            path: path.to_owned(),
            symbol: symbol.name.clone(),
        });
    };
    Ok(IndexedSymbol {
        name: symbol.name.clone(),
        bytes: start..end,
        exported: symbol.exported,
    })
}

fn machine_key(
    keys: &BTreeMap<MachineId, MachineKey>,
    id: MachineId,
) -> Result<MachineKey, IndexError> {
    keys.get(&id)
        .cloned()
        .ok_or(IndexError::UnknownMachine { id })
}
