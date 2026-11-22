//! Path scopes: the files and folders one order may change, and where orders meet.
//!
//! A [`PathScope`] lists the places in one repo that one order may change. An entry is a file,
//! which may be one the order creates, a folder with everything under it, or the whole repo.
//! [`RepoRules`] holds the per-repo rules that refuse some paths whatever the scope says:
//! protected folders, check files, test files and shared lockfiles. Matching is segment-wise and
//! ignores ASCII case on every platform, so `src/foo` never covers `src/foobar.rs`.
//!
//! Every function here is pure. The hook, the landing check and the draft flow pass in the facts
//! they read from the worktree.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use crate::types::{InvalidValue, RepoPath};

/// Why a raw path or a scope was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PathScopeError {
    /// The input was empty.
    #[error("path is empty")]
    EmptyPath,
    /// The input is an absolute path. Entries are repo-relative.
    #[error("path is absolute, give a path inside the repo: {raw:?}")]
    Absolute {
        /// The rejected input.
        raw: String,
    },
    /// The input is a UNC share path.
    #[error("path is a UNC share path: {raw:?}")]
    Unc {
        /// The rejected input.
        raw: String,
    },
    /// The input starts with a drive letter.
    #[error("path starts with a drive letter: {raw:?}")]
    DriveLetter {
        /// The rejected input.
        raw: String,
    },
    /// The input has a `..` segment, which could leave the repo.
    #[error("path has a .. segment: {raw:?}")]
    ParentSegment {
        /// The rejected input.
        raw: String,
    },
    /// The input does not follow the portable repo path grammar.
    #[error("path is not a portable repo path: {raw:?} ({source})")]
    NotPortable {
        /// The rejected input.
        raw: String,
        /// The grammar rule it broke.
        source: InvalidValue,
    },
    /// A scope needs at least one entry.
    #[error("a scope needs at least one entry")]
    EmptyScope,
}

/// A raw entry input after boundary normalization.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NormalizedInput {
    /// The repo root, written `.` or `./`.
    Root,
    /// A repo-relative path, with no `./` prefix and no trailing slash.
    Path {
        /// The validated path.
        path: RepoPath,
        /// True when the input ended in `/`, which marks a folder.
        folder: bool,
    },
}

/// Normalizes one raw entry input at the boundary.
///
/// Backslashes become `/`. Leading `./` segments are dropped, and `.` or `./` alone means the
/// repo root. A trailing `/` marks a folder. Absolute paths, UNC paths, drive letters and `..`
/// segments are refused, and whatever is left must pass [`RepoPath`]. The input is never trimmed,
/// so a trailing space is refused rather than silently dropped.
///
/// # Errors
///
/// Returns the first reason the input is not a repo-relative path.
pub fn normalize_entry_input(raw: &str) -> Result<NormalizedInput, PathScopeError> {
    if raw.is_empty() {
        return Err(PathScopeError::EmptyPath);
    }
    let slashed = raw.replace('\\', "/");
    if slashed.starts_with("//") {
        return Err(PathScopeError::Unc {
            raw: raw.to_owned(),
        });
    }
    let mut rest = slashed.as_str();
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }
    if rest.starts_with('/') {
        return Err(PathScopeError::Absolute {
            raw: raw.to_owned(),
        });
    }
    if has_drive_prefix(rest) {
        return Err(PathScopeError::DriveLetter {
            raw: raw.to_owned(),
        });
    }
    if rest.is_empty() || rest == "." {
        return Ok(NormalizedInput::Root);
    }
    let (body, folder) = match rest.strip_suffix('/') {
        Some(body) => (body, true),
        None => (rest, false),
    };
    if body.split('/').any(|segment| segment == "..") {
        return Err(PathScopeError::ParentSegment {
            raw: raw.to_owned(),
        });
    }
    let path = body
        .parse::<RepoPath>()
        .map_err(|source| PathScopeError::NotPortable {
            raw: raw.to_owned(),
            source,
        })?;
    Ok(NormalizedInput::Path { path, folder })
}

/// True for input that starts with an ASCII letter and a colon, such as `C:` or `c:/src`.
fn has_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// One place an order may change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ScopeEntry {
    /// One file. `new` marks a file the order creates, which may not exist at the base commit.
    File {
        /// The repo-relative path.
        path: RepoPath,
        /// Whether the order creates this file.
        new: bool,
    },
    /// A folder and everything under it, including files created later.
    Folder {
        /// The repo-relative folder.
        path: RepoPath,
        /// Whether the folder covers test files.
        tests: bool,
    },
    /// The whole repo. `tests` lets it cover test files.
    RepoRoot {
        /// Whether the root covers test files.
        tests: bool,
    },
}

impl ScopeEntry {
    /// Builds an entry from one raw input with both flags off: a file is not new, and a folder or
    /// the root does not cover test files. Callers set the flags they were given.
    ///
    /// # Errors
    ///
    /// Returns the reason the input is not a repo-relative path.
    pub fn from_input(raw: &str) -> Result<Self, PathScopeError> {
        Ok(match normalize_entry_input(raw)? {
            NormalizedInput::Root => Self::RepoRoot { tests: false },
            NormalizedInput::Path { path, folder: true } => Self::Folder { path, tests: false },
            NormalizedInput::Path {
                path,
                folder: false,
            } => Self::File { path, new: false },
        })
    }
}

impl fmt::Display for ScopeEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File { path, new: true } => write!(f, "new file {path}"),
            Self::File { path, new: false } => write!(f, "file {path}"),
            Self::Folder { path, tests: true } => write!(f, "folder {path}/ (tests)"),
            Self::Folder { path, tests: false } => write!(f, "folder {path}/"),
            Self::RepoRoot { tests: true } => f.write_str("the repo root (tests)"),
            Self::RepoRoot { tests: false } => f.write_str("the repo root"),
        }
    }
}

/// The places one order may change: sorted, with entries for the same place merged, and never
/// empty.
///
/// Redundant entries stay until [`PathScope::merge_redundant`] runs. Merging needs the repo rules,
/// and a draft is checked against the entries as written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<ScopeEntry>", into = "Vec<ScopeEntry>")]
pub struct PathScope {
    entries: Vec<ScopeEntry>,
}

/// The canonical position of an entry: its kind (root, folder, file), then its case-folded path.
type EntryKey = (u8, Vec<String>);

fn entry_key(entry: &ScopeEntry) -> EntryKey {
    match entry {
        ScopeEntry::RepoRoot { .. } => (0, Vec::new()),
        ScopeEntry::Folder { path, .. } => (1, fold(path)),
        ScopeEntry::File { path, .. } => (2, fold(path)),
    }
}

/// Splits a path into its segments with ASCII case folded: the form every comparison uses.
fn fold(path: &RepoPath) -> Vec<String> {
    path.as_str()
        .split('/')
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Combines two entries that name the same place. Test access is granted if either grants it,
/// and a file is new only if both say so. The smaller spelling of the path is kept, so the result
/// does not depend on input order.
fn merged_with(kept: &ScopeEntry, incoming: &ScopeEntry) -> ScopeEntry {
    match (kept, incoming) {
        (
            ScopeEntry::File {
                path: a,
                new: new_a,
            },
            ScopeEntry::File {
                path: b,
                new: new_b,
            },
        ) => ScopeEntry::File {
            path: a.min(b).clone(),
            new: *new_a && *new_b,
        },
        (
            ScopeEntry::Folder {
                path: a,
                tests: tests_a,
            },
            ScopeEntry::Folder {
                path: b,
                tests: tests_b,
            },
        ) => ScopeEntry::Folder {
            path: a.min(b).clone(),
            tests: *tests_a || *tests_b,
        },
        (ScopeEntry::RepoRoot { tests: tests_a }, ScopeEntry::RepoRoot { tests: tests_b }) => {
            ScopeEntry::RepoRoot {
                tests: *tests_a || *tests_b,
            }
        }
        // An equal key means the same kind, so the arms above cover every reachable pair.
        _ => kept.clone(),
    }
}

impl PathScope {
    /// Builds a scope from entries. Entries for the same place merge, and the result is sorted
    /// into canonical order: the repo root, then folders, then files, each by path.
    ///
    /// # Errors
    ///
    /// Returns [`PathScopeError::EmptyScope`] when no entry is given.
    pub fn new(entries: impl IntoIterator<Item = ScopeEntry>) -> Result<Self, PathScopeError> {
        let mut merged: BTreeMap<EntryKey, ScopeEntry> = BTreeMap::new();
        for entry in entries {
            let key = entry_key(&entry);
            let combined = match merged.get(&key) {
                Some(kept) => merged_with(kept, &entry),
                None => entry,
            };
            merged.insert(key, combined);
        }
        if merged.is_empty() {
            return Err(PathScopeError::EmptyScope);
        }
        Ok(Self {
            entries: merged.into_values().collect(),
        })
    }

    /// The entries in canonical order.
    #[must_use]
    pub fn entries(&self) -> &[ScopeEntry] {
        &self.entries
    }
}

impl TryFrom<Vec<ScopeEntry>> for PathScope {
    type Error = PathScopeError;

    fn try_from(entries: Vec<ScopeEntry>) -> Result<Self, Self::Error> {
        Self::new(entries)
    }
}

impl From<PathScope> for Vec<ScopeEntry> {
    fn from(scope: PathScope) -> Self {
        scope.entries
    }
}

impl JsonSchema for PathScope {
    fn schema_name() -> Cow<'static, str> {
        "PathScope".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "array",
            "items": generator.subschema_for::<ScopeEntry>(),
            "minItems": 1
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_path(raw: &str) -> RepoPath {
        raw.parse().expect("test path is a portable repo path")
    }

    fn file(raw: &str) -> ScopeEntry {
        ScopeEntry::File {
            path: repo_path(raw),
            new: false,
        }
    }

    fn new_file(raw: &str) -> ScopeEntry {
        ScopeEntry::File {
            path: repo_path(raw),
            new: true,
        }
    }

    fn folder(raw: &str) -> ScopeEntry {
        ScopeEntry::Folder {
            path: repo_path(raw),
            tests: false,
        }
    }

    fn refusal(raw: &str) -> PathScopeError {
        normalize_entry_input(raw).expect_err("input should be refused")
    }

    #[test]
    fn backslashes_and_leading_dot_slash_normalize_to_the_same_file() {
        for raw in [
            "src/main.rs",
            "src\\main.rs",
            "./src/main.rs",
            ".\\src\\main.rs",
            "././src/main.rs",
        ] {
            assert_eq!(
                normalize_entry_input(raw).expect("input should normalize"),
                NormalizedInput::Path {
                    path: repo_path("src/main.rs"),
                    folder: false,
                },
                "{raw:?}"
            );
        }
    }

    #[test]
    fn trailing_slash_marks_a_folder_and_a_dot_marks_the_root() {
        for raw in ["src/", "src\\", "./src/"] {
            assert_eq!(
                normalize_entry_input(raw).expect("input should normalize"),
                NormalizedInput::Path {
                    path: repo_path("src"),
                    folder: true,
                },
                "{raw:?}"
            );
        }
        for raw in [".", "./", ".\\"] {
            assert_eq!(
                normalize_entry_input(raw).expect("input should normalize"),
                NormalizedInput::Root,
                "{raw:?}"
            );
        }
    }

    #[test]
    fn refuses_empty_absolute_drive_unc_and_parent_inputs() {
        assert!(matches!(refusal(""), PathScopeError::EmptyPath));
        for raw in ["/etc/passwd", "\\rooted"] {
            assert!(
                matches!(refusal(raw), PathScopeError::Absolute { .. }),
                "{raw:?}"
            );
        }
        for raw in ["C:\\outside", "c:/outside", "C:outside"] {
            assert!(
                matches!(refusal(raw), PathScopeError::DriveLetter { .. }),
                "{raw:?}"
            );
        }
        for raw in ["\\\\server\\share\\file", "//server/share/file"] {
            assert!(
                matches!(refusal(raw), PathScopeError::Unc { .. }),
                "{raw:?}"
            );
        }
        for raw in [
            "src/../outside",
            "../outside",
            "src\\..\\outside",
            "./../outside",
        ] {
            assert!(
                matches!(refusal(raw), PathScopeError::ParentSegment { .. }),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn refuses_names_the_portable_grammar_rejects() {
        for raw in [
            "src/a.",
            "src/a ",
            "CON",
            "nul.txt",
            "src/com1.txt",
            "lpt9.log",
            "file.txt:stream",
            "src//a",
            "src//",
            "src/a?b",
            "src/./a",
        ] {
            assert!(
                matches!(refusal(raw), PathScopeError::NotPortable { .. }),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn entry_input_takes_its_kind_from_the_spelling() {
        assert_eq!(
            ScopeEntry::from_input("src/a.rs").expect("input should parse"),
            file("src/a.rs")
        );
        assert_eq!(
            ScopeEntry::from_input("src/").expect("input should parse"),
            folder("src")
        );
        assert_eq!(
            ScopeEntry::from_input("./").expect("input should parse"),
            ScopeEntry::RepoRoot { tests: false }
        );
    }

    #[test]
    fn scope_sorts_entries_and_merges_entries_for_the_same_place() {
        let forward = vec![
            new_file("src/b.rs"),
            ScopeEntry::RepoRoot { tests: false },
            folder("src"),
            ScopeEntry::Folder {
                path: repo_path("SRC"),
                tests: true,
            },
            file("SRC/B.rs"),
        ];
        let mut backward = forward.clone();
        backward.reverse();
        let scope = PathScope::new(forward).expect("scope is non-empty");
        assert_eq!(
            scope.entries().to_vec(),
            vec![
                ScopeEntry::RepoRoot { tests: false },
                ScopeEntry::Folder {
                    path: repo_path("SRC"),
                    tests: true,
                },
                ScopeEntry::File {
                    path: repo_path("SRC/B.rs"),
                    new: false,
                },
            ]
        );
        assert_eq!(
            PathScope::new(backward).expect("scope is non-empty"),
            scope,
            "input order must not change the scope"
        );
    }

    #[test]
    fn empty_scope_is_refused() {
        assert_eq!(
            PathScope::new(Vec::<ScopeEntry>::new()).expect_err("empty scope is refused"),
            PathScopeError::EmptyScope
        );
    }

    #[test]
    fn scope_serializes_as_snake_case_and_refuses_unknown_fields() {
        let scope = PathScope::new([
            ScopeEntry::RepoRoot { tests: false },
            new_file("docs/new.md"),
        ])
        .expect("scope is non-empty");
        let json = serde_json::to_string(&scope).expect("scope serializes");
        assert_eq!(
            json,
            r#"[{"kind":"repo_root","tests":false},{"kind":"file","path":"docs/new.md","new":true}]"#
        );
        let decoded: PathScope = serde_json::from_str(&json).expect("scope decodes");
        assert_eq!(decoded, scope);
        assert!(serde_json::from_str::<PathScope>("[]").is_err());
        assert!(
            serde_json::from_str::<ScopeEntry>(
                r#"{"kind":"file","path":"a.rs","new":false,"extra":1}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ScopeEntry>(r#"{"kind":"repo_root","tests":false,"extra":1}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<ScopeEntry>(
                r#"{"kind":"folder","path":"src/../x","tests":false}"#
            )
            .is_err()
        );
    }
}
