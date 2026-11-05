//! Scans a source repository with tree-sitter into a plant: machines, the belts between them,
//! and per-file symbols and imports for clash detection.

mod extract;
mod group;
mod hash;
mod lang;
mod model;
mod resolve;
mod walk;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hash::Digest;

pub use group::Grouping;
pub use hash::ContentHash;
pub use lang::Lang;
pub use model::{
    Belt, FileId, ImportRef, ImportTarget, LineRange, Machine, MachineId, Plant, SkippedFile,
    SourceFile, Symbol, SymbolKind,
};

struct ScanLimits {
    file_bytes: u64,
    files: usize,
    total_bytes: u64,
    parse_time: Duration,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            file_bytes: 4 * 1024 * 1024,
            files: 10_000,
            total_bytes: 64 * 1024 * 1024,
            parse_time: Duration::from_millis(250),
        }
    }
}

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
/// Returns an error when the directory walk fails or a tree-sitter grammar or query fails to load.
/// Unreadable, oversized and timed-out files are recorded in `skipped`. The scan reads at most
/// 10,000 files, 4 MiB per file and 64 MiB in total, with a 250 ms parse deadline per file.
pub fn scan(root: impl AsRef<Path>) -> Result<Plant, ScanError> {
    scan_with_limits(root.as_ref(), &ScanLimits::default())
}

fn scan_with_limits(root: &Path, limits: &ScanLimits) -> Result<Plant, ScanError> {
    let (sources, truncated) = walk::source_files(root, limits.files)?;
    let mut extractor = extract::Extractor::new()?;
    let mut skipped = Vec::new();
    let mut remaining = limits.total_bytes;
    let mut scanned = Vec::new();

    for source in sources {
        let result = (|| -> Result<_, String> {
            let file = std::fs::File::open(&source.abs).map_err(|e| e.to_string())?;
            let size = file.metadata().map_err(|e| e.to_string())?.len();
            if size > limits.file_bytes {
                return Err("file byte limit exceeded".into());
            }
            // Reserve the sentinel byte too, so growth after metadata cannot exceed the budget.
            if remaining == 0 || size >= remaining {
                return Err("total byte limit exceeded".into());
            }
            let cap = limits.file_bytes.saturating_add(1).min(remaining);
            let mut bytes = Vec::new();
            file.take(cap)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            remaining -= bytes.len() as u64;
            if bytes.len() as u64 > limits.file_bytes || bytes.len() as u64 == cap {
                return Err("read byte limit exceeded".into());
            }
            let text = String::from_utf8(bytes).map_err(|_| "invalid UTF-8")?;
            let extracted = extractor
                .extract_until(
                    source.lang,
                    &source.rel,
                    &text,
                    std::time::Instant::now() + limits.parse_time,
                )
                .map_err(|e| e.to_string())?;
            Ok((ContentHash::of(text.as_bytes()), extracted))
        })();
        match result {
            Ok((hash, extracted)) => scanned.push((source, hash, extracted)),
            Err(reason) => skipped.push(SkippedFile {
                path: source.rel,
                reason,
            }),
        }
    }

    let index =
        resolve::PathIndex::from_paths(scanned.iter().map(|(source, _, _)| source.rel.as_str()));
    let mut files = Vec::with_capacity(scanned.len());
    for (position, (source, content_hash, extracted)) in scanned.into_iter().enumerate() {
        let imports = extracted
            .imports
            .into_iter()
            .map(|raw| resolve::resolve_import(&source.rel, raw, &index))
            .collect();
        files.push(SourceFile {
            id: FileId(position as u32),
            path: source.rel.clone(),
            lang: source.lang,
            content_hash,
            symbols: extracted.symbols,
            imports,
        });
    }

    let revision = revision_of(&files);
    let mut plant = Plant {
        root: root.to_path_buf(),
        revision,
        machines: Vec::new(),
        belts: Vec::new(),
        files,
        skipped,
        truncated,
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_oversized_and_invalid_files_and_keeps_file_ids_contiguous() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::File::create(repo.path().join("a.rs"))
            .unwrap()
            .set_len(4 * 1024 * 1024 + 1)
            .unwrap();
        std::fs::write(repo.path().join("b.rs"), [0xff]).unwrap();
        std::fs::write(repo.path().join("c.ts"), "import {good} from './d';").unwrap();
        std::fs::write(repo.path().join("d.ts"), "export function good() {}").unwrap();
        let plant = scan(repo.path()).unwrap();
        assert_eq!(
            plant
                .skipped
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["a.rs", "b.rs"]
        );
        assert_eq!(plant.files.len(), 2);
        assert_eq!(plant.files[0].id, FileId(0));
        assert_eq!(
            plant.files[0].imports[0].target,
            ImportTarget::File { file: FileId(1) }
        );
    }

    #[test]
    fn enforces_total_bytes_and_file_count() {
        let repo = tempfile::tempdir().unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(repo.path().join(name), "//1234567\n").unwrap();
        }
        let mut limits = ScanLimits {
            total_bytes: 21,
            ..ScanLimits::default()
        };
        let plant = scan_with_limits(repo.path(), &limits).unwrap();
        assert_eq!(plant.files.len(), 2);
        assert_eq!(plant.skipped.len(), 1);
        assert!(plant.skipped[0].reason.contains("total byte limit"));
        limits.files = 2;
        let plant = scan_with_limits(repo.path(), &limits).unwrap();
        assert_eq!(plant.files.len(), 2);
        assert!(plant.truncated);
    }

    #[test]
    fn cancelled_parses_are_skipped_and_parser_can_be_reused() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("a.rs"), "pub fn good() {}").unwrap();
        let plant = scan_with_limits(
            repo.path(),
            &ScanLimits {
                parse_time: Duration::ZERO,
                ..ScanLimits::default()
            },
        )
        .unwrap();
        assert!(plant.files.is_empty());
        assert!(plant.skipped[0].reason.contains("did not finish"));
        let mut extractor = extract::Extractor::new().unwrap();
        let source = "fn good() {}";
        assert!(
            extractor
                .extract_until(Lang::Rust, "a.rs", source, std::time::Instant::now())
                .is_err()
        );
        assert!(
            extractor
                .extract_until(
                    Lang::Rust,
                    "a.rs",
                    source,
                    std::time::Instant::now() + Duration::from_secs(1)
                )
                .is_ok()
        );
    }
}
