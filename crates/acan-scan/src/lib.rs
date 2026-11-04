//! Scans a source repository with tree-sitter into a plant: machines, the belts between them,
//! per-file symbols and imports for clash detection, and the state machines found in the code.

mod extract;
mod group;
mod hash;
mod kind;
mod lang;
mod model;
mod progress;
mod resolve;
mod states;
mod touch;
mod walk;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hash::Digest;

pub use acan_iso::Kind;
pub use group::Grouping;
pub use hash::ContentHash;
pub use kind::{OVERRIDES_PATH, kind_named, write_kind_overrides};
pub use lang::Lang;
pub use model::{
    Belt, Confidence, FileId, ImportRef, ImportTarget, LineRange, Machine, MachineId, Plant,
    Source, SourceFile, State, StateCandidate, StateMachine, Symbol, SymbolKind, Transition,
};
pub use progress::{MAX_EXPORT_NAMES, ModuleFill, Progress};
pub use touch::{Change, Touched, hunks, touched};

/// Why a scan failed.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    Walk(#[from] ignore::Error),
    #[error("path is not valid UTF-8: {}", .0.display())]
    NonUtf8Path(PathBuf),
    #[error("failed to read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not a valid kinds file: {source}", path.display())]
    Overrides {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to load the {lang:?} grammar: {source}")]
    Language {
        lang: Lang,
        #[source]
        source: tree_sitter::LanguageError,
    },
    #[error("failed to compile the {lang:?} query: {source}")]
    Query {
        lang: Lang,
        #[source]
        source: tree_sitter::QueryError,
    },
    #[error("parsing {path} did not finish")]
    ParseCancelled { path: String },
}

/// Scans the repo at `root` and returns its plant. A repo of up to forty files gets one machine per
/// module in its large directories; a bigger repo groups by directory.
///
/// # Errors
///
/// Returns an error when the directory walk fails, a source file is not readable as UTF-8 text, or a tree-sitter grammar or query fails to load.
pub fn scan(root: impl AsRef<Path>) -> Result<Plant, ScanError> {
    scan_with(root, |_| {})
}

/// Scans the repo at `root` like [`scan`], and reports each step to `observe` as it happens: the
/// [`Progress::Skeleton`] first, a [`Progress::Module`] when each module's files are all parsed, and
/// [`Progress::States`] last. The returned plant is the same as [`scan`] returns.
///
/// # Errors
///
/// Returns the errors [`scan`] returns.
pub fn scan_with(
    root: impl AsRef<Path>,
    mut observe: impl FnMut(Progress<'_>),
) -> Result<Plant, ScanError> {
    let root = root.as_ref();
    let sources = walk::source_files(root)?;
    let skeleton = skeleton_of(root, &sources, kind::read_overrides(root)?);
    observe(Progress::Skeleton(&skeleton));
    // The machine of each file, and how many of a machine's files are still to parse.
    let owner = progress::owners(&skeleton);
    let mut unparsed: Vec<usize> = skeleton
        .machines
        .iter()
        .map(|machine| machine.files.len())
        .collect();

    let index = resolve::PathIndex::from_paths(sources.iter().map(|source| source.rel.as_str()));
    let mut extractor = extract::Extractor::new()?;

    let mut files = Vec::with_capacity(sources.len());
    let mut file_states = Vec::with_capacity(sources.len());
    for (position, source) in sources.iter().enumerate() {
        let text = std::fs::read_to_string(&source.abs).map_err(|error| ScanError::Read {
            path: source.abs.clone(),
            source: error,
        })?;
        let extracted = extractor.extract(source.lang, &source.rel, &text)?;
        let imports = extracted
            .imports
            .into_iter()
            .map(|raw| resolve::resolve_import(&source.rel, raw, &index))
            .collect();
        files.push(SourceFile {
            id: FileId(position as u32),
            path: source.rel.clone(),
            lang: source.lang,
            content_hash: ContentHash::of(text.as_bytes()),
            symbols: extracted.symbols,
            imports,
        });
        file_states.push(extracted.states);

        let machine = owner[position].0 as usize;
        unparsed[machine] -= 1;
        if unparsed[machine] == 0 {
            let fill = progress::module_fill(&skeleton.machines[machine], &files, &owner);
            observe(Progress::Module(fill));
        }
    }

    let revision = revision_of(&files);
    let (state_machines, state_candidates) = states::resolve_all(file_states);
    observe(Progress::States {
        machines: state_machines.len(),
        candidates: state_candidates.len(),
    });
    let mut plant = Plant {
        root: root.to_path_buf(),
        revision,
        machines: Vec::new(),
        belts: Vec::new(),
        files,
        state_machines,
        state_candidates,
        kind_overrides: skeleton.kind_overrides,
    };
    // Regrouping also sets each state machine's module.
    plant.regroup(&Grouping::default());
    Ok(plant)
}

/// The machines and kinds of the repo at `root`, from its file names and paths alone. Nothing is
/// read or parsed, so this is the first thing a live map can show. Its files carry no symbols or
/// imports, and no belts are drawn yet.
///
/// # Errors
///
/// Returns an error when the directory walk fails or the kinds file cannot be read.
pub fn skeleton(root: impl AsRef<Path>) -> Result<Plant, ScanError> {
    let root = root.as_ref();
    let sources = walk::source_files(root)?;
    Ok(skeleton_of(root, &sources, kind::read_overrides(root)?))
}

/// The plant the walk's sources make before any file is read: each file has its path and language
/// and no content hash yet, so the machines and kinds are final.
fn skeleton_of(
    root: &Path,
    sources: &[walk::SourceEntry],
    kind_overrides: BTreeMap<String, Kind>,
) -> Plant {
    let files: Vec<SourceFile> = sources
        .iter()
        .enumerate()
        .map(|(position, source)| SourceFile {
            id: FileId(position as u32),
            path: source.rel.clone(),
            lang: source.lang,
            content_hash: ContentHash::UNREAD,
            symbols: Vec::new(),
            imports: Vec::new(),
        })
        .collect();
    let mut plant = Plant {
        root: root.to_path_buf(),
        revision: revision_of(&files),
        machines: Vec::new(),
        belts: Vec::new(),
        files,
        state_machines: Vec::new(),
        state_candidates: Vec::new(),
        kind_overrides,
    };
    plant.regroup(&Grouping::default());
    plant
}

/// Digests each file's path and content hash in path order, so any content or file-set change moves it.
fn revision_of(files: &[SourceFile]) -> ContentHash {
    let mut digest = Digest::new();
    for file in files {
        digest.update(file.path.as_bytes());
        // A path cannot contain NUL, so this byte ends each path unambiguously.
        digest.update(&[0]);
        digest.update_hash(file.content_hash);
    }
    digest.finish()
}
