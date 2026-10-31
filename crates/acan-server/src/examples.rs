//! The contrast examples a human rules on, kept under the host state dir by episode and run.
//!
//! The examples come only from the observed runs of a judged candidate (see
//! [`crate::observe::Retained`]). Each one is the file `examples/<episode>/<run>/<hash>.json`,
//! whose canonical JSON hashes to its name, so a missing or altered file is an error when it is
//! read and never a silent stand-in. Nothing here reads a model's output.

use std::fmt::Display;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::str::FromStr;

use acan_core::GateRunId;
use acan_intent::{
    CanonicalError, ContrastExample, ContrastExampleHash, EpisodeId, ProbeHash, canonical_json,
};

use crate::observations;
use crate::observe::{Judged, Retained};

/// The directory under the host state dir the examples are kept in.
pub(crate) const DIR: &str = "examples";

/// The suffix of a stored example file.
const EXTENSION: &str = ".json";

/// The suffix of a file a crash left half written. It is never read.
const TEMP_SUFFIX: &str = ".tmp";

/// Why an example could not be kept or read. Every variant refuses: none of them stands for an
/// example that is missing from the store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ExampleError {
    /// The store directory or a file in it could not be read or written.
    #[error("cannot read or write {}", path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// A file in the store is not named as an example is.
    #[error("{} is not named as an example", path.display())]
    Name {
        /// The file.
        path: PathBuf,
    },
    /// A stored file does not decode as an example.
    #[error("stored example {} does not decode", path.display())]
    Malformed {
        /// The file.
        path: PathBuf,
        /// The decode error.
        #[source]
        source: serde_json::Error,
    },
    /// A stored file's content does not have the hash its name gives.
    #[error("stored example {} does not have the hash its name gives", path.display())]
    Altered {
        /// The file.
        path: PathBuf,
    },
    /// An example has no canonical JSON form.
    #[error(transparent)]
    Canonical(#[from] CanonicalError),
}

/// One example kept for an episode: the run it was built from, and the example with its hash.
#[derive(Debug, Clone)]
pub(crate) struct Stored {
    /// The gate run whose observed runs the example was built from, as its directory names it.
    pub(crate) run: String,
    /// The example's identity, as its file name gives it.
    pub(crate) hash: ContrastExampleHash,
    /// The example, checked against `hash` when it was read.
    pub(crate) example: ContrastExample,
}

/// Keeps the examples of `judged` that the episodes of its receipt name, under `dir`, by episode
/// and by `run`. An example already kept under its name is left as it is.
///
/// # Errors
///
/// Returns [`ExampleError`] when an example has no canonical form or the store cannot be written.
pub(crate) fn retain(dir: &Path, run: GateRunId, judged: &Judged) -> Result<(), ExampleError> {
    let named: Vec<(EpisodeId, ProbeHash)> = judged
        .receipt
        .items
        .iter()
        .filter_map(|item| item.episode_id.map(|episode| (episode, item.probe_hash)))
        .collect();
    let run = run.to_string();
    for (episode, probe) in named {
        for retained in judged.retained.iter().filter(|kept| kept.probe == probe) {
            write(dir, &episode, &run, retained)?;
        }
    }
    Ok(())
}

/// The examples kept for `episode`, by run directory name and then by hash. An episode with none
/// kept reads as an empty list. Each file is checked against its name before it is returned.
///
/// # Errors
///
/// Returns [`ExampleError`] when the store cannot be read, a file is not named as an example, or
/// a file is altered or does not decode.
pub(crate) fn read(dir: &Path, episode: &EpisodeId) -> Result<Vec<Stored>, ExampleError> {
    let episode_dir = dir.join(stem(episode));
    let mut stored = Vec::new();
    for run in entries(&episode_dir)? {
        let run_dir = episode_dir.join(&run);
        for file in entries(&run_dir)? {
            if file.ends_with(TEMP_SUFFIX) {
                continue;
            }
            let path = run_dir.join(&file);
            let hash = hash_of_file(&path, &file)?;
            let bytes = fs::read(&path).map_err(|source| io_error(&path, source))?;
            let example: ContrastExample =
                serde_json::from_slice(&bytes).map_err(|source| ExampleError::Malformed {
                    path: path.clone(),
                    source,
                })?;
            if example.contrast_example_hash()? != hash {
                return Err(ExampleError::Altered { path });
            }
            stored.push(Stored {
                run: run.clone(),
                hash,
                example,
            });
        }
    }
    Ok(stored)
}

/// Writes one retained example under `dir/<episode>/<run>/`, unless the same bytes are there.
fn write(
    dir: &Path,
    episode: &EpisodeId,
    run: &str,
    retained: &Retained,
) -> Result<(), ExampleError> {
    let run_dir = dir.join(stem(episode)).join(run);
    fs::create_dir_all(&run_dir).map_err(|source| io_error(&run_dir, source))?;
    let name = format!("{}{EXTENSION}", stem(&retained.hash));
    let path = run_dir.join(&name);
    let bytes = canonical_json(&retained.example)?;
    match fs::read(&path) {
        Ok(stored) if stored == bytes => return Ok(()),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(&path, source)),
    }
    let temp = run_dir.join(format!("{name}.{}{TEMP_SUFFIX}", process::id()));
    observations::replace_file(&path, &temp, &bytes).map_err(|source| io_error(&path, source))
}

/// The hash a stored file is named for. Only `<64 hex digits>.json` names an example.
fn hash_of_file(path: &Path, file: &str) -> Result<ContrastExampleHash, ExampleError> {
    let not_named = || ExampleError::Name {
        path: path.to_path_buf(),
    };
    let digits = file.strip_suffix(EXTENSION).ok_or_else(not_named)?;
    ContrastExampleHash::from_str(&format!("sha256:{digits}")).map_err(|_| not_named())
}

/// A hash without its `sha256:` prefix, which Windows file names cannot hold the colon of.
fn stem(hash: &impl Display) -> String {
    let text = hash.to_string();
    text.strip_prefix("sha256:").unwrap_or(&text).to_owned()
}

/// The names in `dir`, sorted. A missing directory has no entries.
fn entries(dir: &Path) -> Result<Vec<String>, ExampleError> {
    let reader = match fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(dir, source)),
    };
    let mut names = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|source| io_error(dir, source))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn io_error(path: &Path, source: io::Error) -> ExampleError {
    ExampleError::Io {
        path: path.to_path_buf(),
        source,
    }
}
