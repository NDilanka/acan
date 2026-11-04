//! The kind of each machine, as the user sets it. The rules that guess a kind are in acan-iso's
//! `classify`, which the renderer also runs, so a plant that carries no kinds draws the same ones.
//! The user's choices live in `.acan/kinds.json`, and this module reads and writes them.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use acan_iso::Kind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ScanError;
use crate::model::Machine;

/// The file, under the repo root, that holds the kinds the user set.
pub const OVERRIDES_PATH: &str = ".acan/kinds.json";

/// Sets a machine's kind to one the user chose. The groove is closed, since the kind is no guess.
pub(crate) fn set_by_user(machine: &mut Machine, kind: Kind) {
    machine.kind = kind;
    machine.kind_guessed = false;
    machine.kind_reason = "set by you".to_owned();
    machine.kind_set_by_user = true;
}

/// The kind with this wire name, such as `switchboard`, or None for a name no kind has.
#[must_use]
pub fn kind_named(name: &str) -> Option<Kind> {
    Kind::ALL
        .into_iter()
        .find(|kind| serde_json::to_value(kind).ok() == Some(Value::String(name.to_owned())))
}

/// The file's shape: `{"kinds": {"<machine name>": "<kind name>"}}`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct OverrideFile {
    #[serde(default)]
    kinds: BTreeMap<String, String>,
}

/// The kinds the user set, by machine name, from `.acan/kinds.json` under `root`. A missing file sets
/// none. An entry that names no kind is ignored, so its machine keeps the kind its rule gives.
pub(crate) fn read_overrides(root: &Path) -> Result<BTreeMap<String, Kind>, ScanError> {
    let path = root.join(OVERRIDES_PATH);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => return Err(ScanError::Read { path, source }),
    };
    let file: OverrideFile =
        serde_json::from_str(&text).map_err(|source| ScanError::Overrides { path, source })?;
    Ok(file
        .kinds
        .iter()
        .filter_map(|(name, kind)| Some((name.clone(), kind_named(kind)?)))
        .collect())
}

/// Sets (`Some`) or clears (`None`) the kind of each named machine in `.acan/kinds.json` under
/// `root`. Other entries stay as they are, including names this build does not know. The file is
/// written through a temporary file and renamed into place, so a reader never sees half of it.
///
/// # Errors
///
/// Returns an error when the file cannot be read as a kinds file or cannot be written.
pub fn write_kind_overrides(
    root: &Path,
    changes: &BTreeMap<String, Option<Kind>>,
) -> Result<(), ScanError> {
    let path = root.join(OVERRIDES_PATH);
    let mut file = match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str::<OverrideFile>(&text).map_err(|source| ScanError::Overrides {
                path: path.clone(),
                source,
            })?
        }
        Err(error) if error.kind() == ErrorKind::NotFound => OverrideFile::default(),
        Err(source) => {
            return Err(ScanError::Read {
                path: path.clone(),
                source,
            });
        }
    };
    for (name, kind) in changes {
        match kind {
            Some(kind) => {
                let wire = serde_json::to_value(kind)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned));
                if let Some(wire) = wire {
                    file.kinds.insert(name.clone(), wire);
                }
            }
            None => {
                file.kinds.remove(name);
            }
        }
    }
    let text = serde_json::to_string_pretty(&file).map_err(|source| ScanError::Overrides {
        path: path.clone(),
        source,
    })?;
    let directory = root.join(".acan");
    std::fs::create_dir_all(&directory).map_err(|source| ScanError::Write {
        path: directory.clone(),
        source,
    })?;
    let temporary = directory.join("kinds.json.tmp");
    std::fs::write(&temporary, format!("{text}\n")).map_err(|source| ScanError::Write {
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, &path).map_err(|source| ScanError::Write { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_round_trip() {
        assert_eq!(kind_named("switchboard"), Some(Kind::Switchboard));
        assert_eq!(kind_named("assembler"), Some(Kind::Assembler));
        assert_eq!(kind_named("Switchboard"), None);
        assert_eq!(kind_named("not_a_kind"), None);
        for kind in Kind::ALL {
            let name = serde_json::to_value(kind).unwrap();
            assert_eq!(kind_named(name.as_str().unwrap()), Some(kind));
        }
    }
}
