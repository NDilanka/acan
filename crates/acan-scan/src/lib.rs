//! Scans a source repository with tree-sitter into a plant: machines, the belts between them,
//! per-file symbols and imports for clash detection, and the state machines found in the code.

mod extract;
mod group;
mod hash;
mod lang;
mod model;
mod resolve;
mod states;
mod touch;
mod walk;

use std::path::{Path, PathBuf};

use hash::Digest;

pub use group::Grouping;
pub use hash::ContentHash;
pub use lang::Lang;
pub use model::{
    Belt, Confidence, FileId, ImportRef, ImportTarget, LineRange, Machine, MachineId, Plant,
    SourceFile, State, StateMachine, Symbol, SymbolKind, Transition,
};
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
    let root = root.as_ref();
    let sources = walk::source_files(root)?;
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
    }

    let revision = revision_of(&files);
    let mut plant = Plant {
        root: root.to_path_buf(),
        revision,
        machines: Vec::new(),
        belts: Vec::new(),
        files,
        state_machines: states::resolve(file_states),
    };
    // Regrouping also sets each state machine's module.
    plant.regroup(&Grouping::default());
    Ok(plant)
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
