//! Path scopes: the files and folders one order may change, and where orders meet.
//!
//! A [`PathScope`] lists the places in one repo that one order may change. An entry is a file,
//! which may be one the order creates, a folder with everything under it, or the whole repo.
//! [`RepoRules`] holds the per-repo rules that refuse some paths whatever the scope says:
//! protected folders, check files, test files and shared lockfiles. Matching is segment-wise and
//! ignores ASCII case on every platform, so `src/foo` never covers `src/foobar.rs`.
//!
//! [`validate_draft`] checks a proposed scope against the base commit, and
//! [`PathScope::overlaps`] finds where two scopes meet.
//!
//! Every function here is pure. The hook, the landing check and the draft flow pass in the facts
//! they read from the worktree.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
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

    /// What the scope and the repo rules say about one path.
    #[must_use]
    pub fn coverage(&self, path: &RepoPath, rules: &RepoRules) -> PathCoverage {
        coverage_in(&self.entries, path, rules)
    }

    /// The scope without the entries that the other entries already grant. A File drops out when
    /// the others cover its path, and a Folder drops out when an outer folder or the root grants
    /// the same access. The root never drops out. Shared and test files are kept unless the
    /// others cover them, so an explicit File entry for one survives under a plain folder.
    #[must_use]
    pub fn merge_redundant(&self, rules: &RepoRules) -> Self {
        let entries: Vec<ScopeEntry> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.is_redundant(*index, rules))
            .map(|(_, entry)| entry.clone())
            .collect();
        debug_assert!(
            !entries.is_empty(),
            "the root, or the outermost folder or file, never drops out"
        );
        Self { entries }
    }

    /// True when the other entries grant everything the entry at `index` grants.
    fn is_redundant(&self, index: usize, rules: &RepoRules) -> bool {
        let others: Vec<ScopeEntry> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(position, _)| *position != index)
            .map(|(_, entry)| entry.clone())
            .collect();
        match &self.entries[index] {
            ScopeEntry::RepoRoot { .. } => false,
            ScopeEntry::File { path, .. } => {
                coverage_in(&others, path, rules) == PathCoverage::Covered
            }
            ScopeEntry::Folder { path, tests } => {
                let inner = fold(path);
                others.iter().any(|outer| match outer {
                    ScopeEntry::RepoRoot { tests: outer_tests } => *outer_tests || !*tests,
                    ScopeEntry::Folder {
                        path: outer_path,
                        tests: outer_tests,
                    } => {
                        let outer = fold(outer_path);
                        inner.len() > outer.len()
                            && inner.starts_with(&outer)
                            && (*outer_tests || !*tests)
                    }
                    ScopeEntry::File { .. } => false,
                })
            }
        }
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

/// The per-repo rules that refuse some paths whatever the scope says.
///
/// `protected_folders` and `check_files` are anchored at the repo root, so `src/.acan/x.md` is
/// not protected. The other lists match at any depth. A missing field takes its default, so a
/// partial rules file changes only the lists it names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RepoRules {
    /// Folders no order may change, with everything under them.
    pub protected_folders: Vec<RepoPath>,
    /// Files whose checks are frozen at approval, so no order may change them.
    pub check_files: Vec<RepoPath>,
    /// Folder names whose contents are test files. Matched ignoring ASCII case.
    pub test_folders: Vec<String>,
    /// File name patterns that mark test files. `*` matches any run of characters.
    pub test_file_patterns: Vec<String>,
    /// Lockfile names that only a File entry can cover. Matched ignoring ASCII case.
    pub shared_files: Vec<String>,
}

impl Default for RepoRules {
    fn default() -> Self {
        Self {
            protected_folders: repo_paths(&[".acan", ".factory"]),
            check_files: Vec::new(),
            test_folders: strings(&["test", "tests", "__tests__", "spec"]),
            test_file_patterns: strings(&["*.test.*", "*.spec.*", "*_test.*"]),
            shared_files: strings(&[
                "Cargo.lock",
                "package-lock.json",
                "pnpm-lock.yaml",
                "yarn.lock",
                "bun.lock",
                "go.sum",
                "poetry.lock",
                "uv.lock",
                "Gemfile.lock",
            ]),
        }
    }
}

fn repo_paths(raw: &[&str]) -> Vec<RepoPath> {
    raw.iter()
        .map(|path| {
            path.parse::<RepoPath>()
                .expect("default rule is a portable repo path")
        })
        .collect()
}

fn strings(raw: &[&str]) -> Vec<String> {
    raw.iter().map(|text| (*text).to_owned()).collect()
}

/// What the scope and the repo rules say about one path.
///
/// The name avoids `view::Coverage`, which the crate root exports for the plant view.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathCoverage {
    /// An entry covers the path, and no rule refuses it.
    Covered,
    /// A protected folder holds the path. No order may change it.
    Protected {
        /// The protected folder, repo-relative.
        rule: RepoPath,
    },
    /// The path is a check file, whose checks are frozen at approval.
    CheckFile,
    /// The path is a test file that no entry allows.
    TestFile,
    /// The path is a shared file, which only a File entry can name.
    SharedFile,
    /// No entry covers the path.
    Outside,
}

impl fmt::Display for PathCoverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Covered => f.write_str("covered by the scope"),
            Self::Protected { rule } => {
                write!(f, "protected folder {rule}/ is never writable by an order")
            }
            Self::CheckFile => f.write_str("a check file, whose checks are frozen at approval"),
            Self::TestFile => f.write_str(
                "a test file: name it with a file entry, or use a folder entry with tests on",
            ),
            Self::SharedFile => f.write_str("a shared file: only a file entry can name it"),
            Self::Outside => f.write_str("outside the scope"),
        }
    }
}

/// Matches a file name against a pattern whose `*` stands for any run of characters. Case is
/// ignored on both sides.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let parts: Vec<&str> = pattern.split('*').collect();
    let Some((first, after_first)) = parts.split_first() else {
        return false;
    };
    let Some((last, middle)) = after_first.split_last() else {
        return name == pattern;
    };
    let Some(mut rest) = name.strip_prefix(*first) else {
        return false;
    };
    for part in middle {
        let Some(found) = rest.find(*part) else {
            return false;
        };
        rest = &rest[found + part.len()..];
    }
    rest.ends_with(*last)
}

/// True when a folder name above the path is a test folder, or the file name matches a test
/// pattern.
fn is_test_file(rules: &RepoRules, path: &RepoPath) -> bool {
    let segments = fold(path);
    let Some((name, folders)) = segments.split_last() else {
        return false;
    };
    folders.iter().any(|folder| {
        rules
            .test_folders
            .iter()
            .any(|test| test.eq_ignore_ascii_case(folder))
    }) || rules
        .test_file_patterns
        .iter()
        .any(|pattern| glob_matches(pattern, name))
}

/// The protected folder that holds the path, if any. Protected folders are anchored at the repo
/// root, so a folder of the same name deeper down is not protected.
fn protected_rule(rules: &RepoRules, path: &RepoPath) -> Option<RepoPath> {
    let segments = fold(path);
    rules
        .protected_folders
        .iter()
        .find(|folder| segments.starts_with(&fold(folder)))
        .cloned()
}

/// True when a check file rule names the path, ignoring case.
fn is_check_file(rules: &RepoRules, path: &RepoPath) -> bool {
    let segments = fold(path);
    rules.check_files.iter().any(|file| fold(file) == segments)
}

/// Decides one path in a fixed order: protected folders, check files, a File entry that names the
/// path, shared files, then the broad entries. The rules before the entries win over them.
fn coverage_in(entries: &[ScopeEntry], path: &RepoPath, rules: &RepoRules) -> PathCoverage {
    if let Some(rule) = protected_rule(rules, path) {
        return PathCoverage::Protected { rule };
    }
    if is_check_file(rules, path) {
        return PathCoverage::CheckFile;
    }
    let segments = fold(path);
    let named = entries.iter().any(
        |entry| matches!(entry, ScopeEntry::File { path: named, .. } if fold(named) == segments),
    );
    if named {
        return PathCoverage::Covered;
    }
    let name = segments.last().map_or("", String::as_str);
    if rules
        .shared_files
        .iter()
        .any(|shared| shared.eq_ignore_ascii_case(name))
    {
        return PathCoverage::SharedFile;
    }
    let test = is_test_file(rules, path);
    let broad = entries.iter().any(|entry| match entry {
        ScopeEntry::Folder {
            path: folder,
            tests,
        } => {
            let folder = fold(folder);
            segments.len() > folder.len() && segments.starts_with(&folder) && (*tests || !test)
        }
        ScopeEntry::RepoRoot { tests } => *tests || !test,
        ScopeEntry::File { .. } => false,
    });
    if broad {
        PathCoverage::Covered
    } else if test {
        PathCoverage::TestFile
    } else {
        PathCoverage::Outside
    }
}

/// Pairs of paths that differ only by ASCII case. Each pair names the smallest spelling first and
/// then one other spelling of the same path, so the result does not depend on input order.
#[must_use]
pub fn detect_case_collisions(paths: &[RepoPath]) -> Vec<(RepoPath, RepoPath)> {
    let mut spellings: BTreeMap<String, BTreeSet<RepoPath>> = BTreeMap::new();
    for path in paths {
        spellings
            .entry(path.as_str().to_ascii_lowercase())
            .or_default()
            .insert(path.clone());
    }
    let mut pairs = Vec::new();
    for group in spellings.into_values() {
        let mut group = group.into_iter();
        if let Some(first) = group.next() {
            pairs.extend(group.map(|other| (first.clone(), other)));
        }
    }
    pairs
}

/// Why a draft scope cannot be approved as written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DraftError {
    /// An entry sits inside a protected folder.
    #[error("entry {path} is inside protected folder {rule}/, which no order may change")]
    Protected {
        /// The refused entry.
        path: RepoPath,
        /// The protected folder that holds the entry.
        rule: RepoPath,
    },
    /// A file entry names a check file.
    #[error("entry {path} is a check file, whose checks are frozen at approval")]
    CheckFile {
        /// The refused entry.
        path: RepoPath,
    },
    /// A file entry is neither tracked at the base commit nor marked new.
    #[error("file {path} is not tracked at the base commit and is not marked new")]
    MissingFile {
        /// The file entry.
        path: RepoPath,
    },
    /// A folder entry does not exist at the base commit.
    #[error("folder {path}/ does not exist at the base commit")]
    MissingFolder {
        /// The folder entry.
        path: RepoPath,
    },
    /// The folder of a new file neither exists at the base commit nor is covered by a folder
    /// entry.
    #[error("new file {path} needs its folder in the repo or a folder entry that covers it")]
    MissingParent {
        /// The new file.
        path: RepoPath,
    },
}

/// What a valid draft covers. The app shows it before the developer approves.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DraftSummary {
    /// Tracked files at the base commit that the scope covers.
    pub covered_file_count: usize,
    /// Tracked files at the base commit, each path counted once ignoring case.
    pub total_tracked_files: usize,
    /// The top-level folders the scope touches, sorted, with one spelling each.
    pub top_level_folders: Vec<String>,
    /// True when the scope has the repo root, or covers more than the threshold share of the
    /// tracked files.
    pub needs_broad_confirmation: bool,
}

/// Checks a proposed scope against the base commit before the developer approves it.
///
/// `tracked_files` and `existing_dirs` hold the paths at the base commit. A file entry that is not
/// marked new must be tracked, a folder entry must exist, and the folder of a new file must exist
/// or be covered by a folder entry in the same scope. Entries inside a protected folder, and file
/// entries that name a check file, are refused. Case is ignored throughout.
///
/// On success the summary sizes the scope. `needs_broad_confirmation` is set when the scope has the
/// repo root, or when it covers more than `broad_threshold_pct` percent of the tracked files.
///
/// # Errors
///
/// Returns every problem found, in scope order.
pub fn validate_draft(
    scope: &PathScope,
    tracked_files: &[RepoPath],
    existing_dirs: &[RepoPath],
    rules: &RepoRules,
    broad_threshold_pct: u8,
) -> Result<DraftSummary, Vec<DraftError>> {
    let tracked: BTreeSet<Vec<String>> = tracked_files.iter().map(fold).collect();
    let dirs: BTreeSet<Vec<String>> = existing_dirs.iter().map(fold).collect();
    let mut errors = Vec::new();
    for entry in scope.entries() {
        match entry {
            ScopeEntry::RepoRoot { .. } => {}
            ScopeEntry::File { path, new } => {
                if let Some(rule) = protected_rule(rules, path) {
                    errors.push(DraftError::Protected {
                        path: path.clone(),
                        rule,
                    });
                } else if is_check_file(rules, path) {
                    errors.push(DraftError::CheckFile { path: path.clone() });
                } else if !*new && !tracked.contains(&fold(path)) {
                    errors.push(DraftError::MissingFile { path: path.clone() });
                } else if *new && !parent_is_available(scope, &dirs, path) {
                    errors.push(DraftError::MissingParent { path: path.clone() });
                }
            }
            ScopeEntry::Folder { path, .. } => {
                if let Some(rule) = protected_rule(rules, path) {
                    errors.push(DraftError::Protected {
                        path: path.clone(),
                        rule,
                    });
                } else if !dirs.contains(&fold(path)) {
                    errors.push(DraftError::MissingFolder { path: path.clone() });
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(summarize(scope, tracked_files, rules, broad_threshold_pct))
}

/// Counts the tracked files the scope covers and names the top-level folders it touches.
fn summarize(
    scope: &PathScope,
    tracked_files: &[RepoPath],
    rules: &RepoRules,
    broad_threshold_pct: u8,
) -> DraftSummary {
    let mut folders: BTreeMap<String, String> = BTreeMap::new();
    for entry in scope.entries() {
        match entry {
            ScopeEntry::RepoRoot { .. } => {}
            ScopeEntry::Folder { path, .. } => remember_folder(&mut folders, first_segment(path)),
            ScopeEntry::File { path, .. } => {
                if let Some(name) = directory_top(path) {
                    remember_folder(&mut folders, name);
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut total_tracked_files: usize = 0;
    let mut covered_file_count: usize = 0;
    for path in tracked_files {
        if !seen.insert(fold(path)) {
            continue;
        }
        total_tracked_files += 1;
        if scope.coverage(path, rules) == PathCoverage::Covered {
            covered_file_count += 1;
            if let Some(name) = directory_top(path) {
                remember_folder(&mut folders, name);
            }
        }
    }
    let has_root = scope
        .entries()
        .iter()
        .any(|entry| matches!(entry, ScopeEntry::RepoRoot { .. }));
    let over_threshold = covered_file_count.saturating_mul(100)
        > total_tracked_files.saturating_mul(usize::from(broad_threshold_pct));
    DraftSummary {
        covered_file_count,
        total_tracked_files,
        top_level_folders: folders.into_values().collect(),
        needs_broad_confirmation: has_root || over_threshold,
    }
}

/// Keeps one spelling for each top-level folder: the smallest, so the result does not depend on
/// the order of the entries.
fn remember_folder(folders: &mut BTreeMap<String, String>, name: &str) {
    let slot = folders
        .entry(name.to_ascii_lowercase())
        .or_insert_with(|| name.to_owned());
    if name < slot.as_str() {
        *slot = name.to_owned();
    }
}

/// The first segment of a folder path, which is the folder itself at the top level.
fn first_segment(path: &RepoPath) -> &str {
    path.as_str().split('/').next().unwrap_or_default()
}

/// The top-level folder that holds a path, or `None` when the path sits at the repo root.
fn directory_top(path: &RepoPath) -> Option<&str> {
    path.as_str().split_once('/').map(|(top, _)| top)
}

/// True when the folder above a new file is the repo root, a folder at the base commit, or a
/// folder that a folder entry in the scope covers.
fn parent_is_available(scope: &PathScope, dirs: &BTreeSet<Vec<String>>, path: &RepoPath) -> bool {
    let mut parent = fold(path);
    parent.pop();
    if parent.is_empty() || dirs.contains(&parent) {
        return true;
    }
    scope.entries().iter().any(|entry| {
        matches!(entry, ScopeEntry::Folder { path: folder, .. } if parent.starts_with(&fold(folder)))
    })
}

/// Two orders name the same place, so the interlock holds one of them back.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Overlap {
    /// Both scopes name this file, or one names it and the other names a folder above it.
    File {
        /// The shared file, in the spelling of the smaller entry.
        path: RepoPath,
    },
    /// Both scopes name this folder, or one names a folder inside the other's folder. The
    /// narrower folder is reported.
    Folder {
        /// The narrower folder, in the spelling of the smaller entry.
        path: RepoPath,
    },
    /// Both scopes name the whole repo.
    Root,
}

impl fmt::Display for Overlap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File { path } => write!(f, "file {path}"),
            Self::Folder { path } => write!(f, "folder {path}/"),
            Self::Root => f.write_str("the repo root"),
        }
    }
}

impl Overlap {
    /// The reported path, or `None` for the whole repo.
    fn path(&self) -> Option<&RepoPath> {
        match self {
            Self::File { path } | Self::Folder { path } => Some(path),
            Self::Root => None,
        }
    }

    /// The position that reports each place once: the root, then folders, then files, each by
    /// its case-folded path.
    fn key(&self) -> (u8, Vec<String>) {
        match self {
            Self::Root => (0, Vec::new()),
            Self::Folder { path } => (1, fold(path)),
            Self::File { path } => (2, fold(path)),
        }
    }
}

impl PathScope {
    /// The places where this scope and the other scope may both write. An entry of one scope
    /// overlaps an entry of the other when one covers the other, ignoring case. Each place is
    /// reported once, with the smaller spelling. The test and new flags do not change the answer,
    /// so the check is conservative.
    #[must_use]
    pub fn overlaps(&self, other: &PathScope) -> Vec<Overlap> {
        let mut found: BTreeMap<(u8, Vec<String>), Overlap> = BTreeMap::new();
        for mine in &self.entries {
            for theirs in &other.entries {
                let Some(overlap) = overlap_between(mine, theirs) else {
                    continue;
                };
                let replace = found
                    .get(&overlap.key())
                    .is_none_or(|kept| overlap.path() < kept.path());
                if replace {
                    found.insert(overlap.key(), overlap);
                }
            }
        }
        found.into_values().collect()
    }
}

/// The overlap between two entries, or `None` when they name disjoint places.
fn overlap_between(mine: &ScopeEntry, theirs: &ScopeEntry) -> Option<Overlap> {
    match (mine, theirs) {
        (ScopeEntry::RepoRoot { .. }, ScopeEntry::RepoRoot { .. }) => Some(Overlap::Root),
        (ScopeEntry::RepoRoot { .. }, other) | (other, ScopeEntry::RepoRoot { .. }) => {
            Some(named_place(other))
        }
        (ScopeEntry::File { path: a, .. }, ScopeEntry::File { path: b, .. }) => {
            (fold(a) == fold(b)).then(|| Overlap::File {
                path: a.min(b).clone(),
            })
        }
        (ScopeEntry::Folder { path: folder, .. }, ScopeEntry::File { path: file, .. })
        | (ScopeEntry::File { path: file, .. }, ScopeEntry::Folder { path: folder, .. }) => {
            fold(file)
                .starts_with(&fold(folder))
                .then(|| Overlap::File { path: file.clone() })
        }
        (ScopeEntry::Folder { path: a, .. }, ScopeEntry::Folder { path: b, .. }) => {
            let (fa, fb) = (fold(a), fold(b));
            if !(fa.starts_with(&fb) || fb.starts_with(&fa)) {
                return None;
            }
            let narrower = match fa.len().cmp(&fb.len()) {
                Ordering::Greater => a,
                Ordering::Less => b,
                Ordering::Equal => a.min(b),
            };
            Some(Overlap::Folder {
                path: narrower.clone(),
            })
        }
    }
}

/// The place an entry names, for an overlap with the whole repo.
fn named_place(entry: &ScopeEntry) -> Overlap {
    match entry {
        ScopeEntry::File { path, .. } => Overlap::File { path: path.clone() },
        ScopeEntry::Folder { path, .. } => Overlap::Folder { path: path.clone() },
        ScopeEntry::RepoRoot { .. } => Overlap::Root,
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

    fn scope(entries: impl IntoIterator<Item = ScopeEntry>) -> PathScope {
        PathScope::new(entries).expect("test scope is non-empty")
    }

    fn cover(scope: &PathScope, raw: &str) -> PathCoverage {
        scope.coverage(&repo_path(raw), &RepoRules::default())
    }

    fn tests_folder(raw: &str) -> ScopeEntry {
        ScopeEntry::Folder {
            path: repo_path(raw),
            tests: true,
        }
    }

    #[test]
    fn default_rules_protect_acan_and_factory_and_name_the_shared_lockfiles() {
        let rules = RepoRules::default();
        assert_eq!(
            rules.protected_folders,
            vec![repo_path(".acan"), repo_path(".factory")]
        );
        assert!(rules.check_files.is_empty());
        assert_eq!(rules.test_folders, ["test", "tests", "__tests__", "spec"]);
        assert_eq!(
            rules.test_file_patterns,
            ["*.test.*", "*.spec.*", "*_test.*"]
        );
        assert_eq!(
            rules.shared_files,
            [
                "Cargo.lock",
                "package-lock.json",
                "pnpm-lock.yaml",
                "yarn.lock",
                "bun.lock",
                "go.sum",
                "poetry.lock",
                "uv.lock",
                "Gemfile.lock",
            ]
        );
    }

    #[test]
    fn partial_rules_json_fills_the_defaults_and_refuses_unknown_fields() {
        let rules: RepoRules = serde_json::from_str(r#"{"check_files":["ci/checks.yml"]}"#)
            .expect("partial rules decode");
        assert_eq!(rules.check_files, vec![repo_path("ci/checks.yml")]);
        assert_eq!(
            rules.protected_folders,
            RepoRules::default().protected_folders
        );
        assert_eq!(rules.shared_files, RepoRules::default().shared_files);
        assert!(serde_json::from_str::<RepoRules>(r#"{"protected":[".acan"]}"#).is_err());
        assert!(serde_json::from_str::<RepoRules>(r#"{"check_files":["../x"]}"#).is_err());
    }

    #[test]
    fn rules_round_trip_through_json() {
        let rules = RepoRules {
            check_files: vec![repo_path("ci/checks.yml")],
            ..RepoRules::default()
        };
        let json = serde_json::to_string(&rules).expect("rules serialize");
        let decoded: RepoRules = serde_json::from_str(&json).expect("rules decode");
        assert_eq!(decoded, rules);
    }

    #[test]
    fn glob_patterns_match_whole_file_names_ignoring_case() {
        assert!(glob_matches("*.test.*", "app.test.ts"));
        assert!(!glob_matches("*.test.*", "app.ts"));
        assert!(!glob_matches("*.test.*", "app.testing.ts"));
        assert!(glob_matches("*_test.*", "foo_test.go"));
        assert!(glob_matches("*.spec.*", "Foo.SPEC.tsx"));
        assert!(glob_matches("Cargo.lock", "cargo.LOCK"));
        assert!(glob_matches("a*b*c", "axxbyyc"));
        assert!(!glob_matches("a*b*c", "axxbyy"));
        assert!(glob_matches("*", ""));
        assert!(glob_matches("a*", "a"));
        assert!(!glob_matches("a*a", "a"));
    }

    #[test]
    fn folder_entries_match_whole_segments_and_ignore_case() {
        let covered = scope([folder("src/foo")]);
        assert_eq!(cover(&covered, "src/foo/a.rs"), PathCoverage::Covered);
        assert_eq!(cover(&covered, "SRC/FOO/a.rs"), PathCoverage::Covered);
        assert_eq!(cover(&covered, "src/foobar.rs"), PathCoverage::Outside);
        let upper = scope([folder("SRC")]);
        assert_eq!(cover(&upper, "src/app.rs"), PathCoverage::Covered);
    }

    #[test]
    fn a_folder_covers_new_files_below_it_and_a_file_covers_only_itself() {
        let covered = scope([folder("src"), file("docs/readme.md")]);
        assert_eq!(cover(&covered, "src/deep/new.rs"), PathCoverage::Covered);
        assert_eq!(cover(&covered, "docs/readme.md"), PathCoverage::Covered);
        assert_eq!(cover(&covered, "docs/other.md"), PathCoverage::Outside);
    }

    #[test]
    fn repo_root_covers_every_path_that_is_not_a_test_file() {
        let root = scope([ScopeEntry::RepoRoot { tests: false }]);
        assert_eq!(cover(&root, "a/b/c.rs"), PathCoverage::Covered);
        assert_eq!(cover(&root, "Cargo.toml"), PathCoverage::Covered);
    }

    #[test]
    fn protected_folders_refuse_whatever_the_scope_says_and_match_from_the_root() {
        let root = scope([ScopeEntry::RepoRoot { tests: true }]);
        assert_eq!(
            cover(&root, ".acan/notes.md"),
            PathCoverage::Protected {
                rule: repo_path(".acan")
            }
        );
        assert_eq!(
            cover(&root, ".FACTORY/hooks.json"),
            PathCoverage::Protected {
                rule: repo_path(".factory")
            }
        );
        let named = scope([new_file(".acan/world.db")]);
        assert_eq!(
            cover(&named, ".acan/world.db"),
            PathCoverage::Protected {
                rule: repo_path(".acan")
            }
        );
        assert_eq!(cover(&root, "src/.acan/notes.md"), PathCoverage::Covered);
    }

    #[test]
    fn check_files_refuse_even_with_a_file_entry() {
        let rules = RepoRules {
            check_files: vec![repo_path("ci/checks.yml")],
            ..RepoRules::default()
        };
        let named = scope([file("ci/checks.yml")]);
        assert_eq!(
            named.coverage(&repo_path("ci/checks.yml"), &rules),
            PathCoverage::CheckFile
        );
        let root = scope([ScopeEntry::RepoRoot { tests: true }]);
        assert_eq!(
            root.coverage(&repo_path("ci/checks.yml"), &rules),
            PathCoverage::CheckFile
        );
        assert_eq!(
            root.coverage(&repo_path("src/ci/checks.yml"), &rules),
            PathCoverage::Covered
        );
    }

    #[test]
    fn test_files_need_a_file_entry_or_a_folder_with_tests_on() {
        let plain = scope([folder("src")]);
        assert_eq!(cover(&plain, "src/app.testing.ts"), PathCoverage::Covered);
        assert_eq!(cover(&plain, "src/tests/unit.rs"), PathCoverage::TestFile);
        assert_eq!(cover(&plain, "src/app.test.ts"), PathCoverage::TestFile);
        assert_eq!(cover(&plain, "src/Foo.TEST.ts"), PathCoverage::TestFile);
        assert_eq!(cover(&plain, "src/foo_test.go"), PathCoverage::TestFile);
        let lib = scope([folder("lib")]);
        assert_eq!(cover(&lib, "lib/spec/x.rb"), PathCoverage::TestFile);
        let lib_tests = scope([tests_folder("lib")]);
        assert_eq!(
            cover(&lib_tests, "lib/__tests__/a.ts"),
            PathCoverage::Covered
        );
        assert_eq!(cover(&lib_tests, "lib/spec/x.rb"), PathCoverage::Covered);
        let named = scope([file("src/tests/unit.rs"), new_file("src/app.test.ts")]);
        assert_eq!(cover(&named, "src/tests/unit.rs"), PathCoverage::Covered);
        assert_eq!(cover(&named, "src/app.test.ts"), PathCoverage::Covered);
        let root_tests = scope([ScopeEntry::RepoRoot { tests: true }]);
        assert_eq!(
            cover(&root_tests, "src/tests/unit.rs"),
            PathCoverage::Covered
        );
        let root_plain = scope([ScopeEntry::RepoRoot { tests: false }]);
        assert_eq!(
            cover(&root_plain, "src/tests/unit.rs"),
            PathCoverage::TestFile
        );
    }

    #[test]
    fn custom_test_folders_replace_the_defaults() {
        let rules = RepoRules {
            test_folders: vec!["fixtures".to_owned()],
            ..RepoRules::default()
        };
        let lib = scope([folder("lib")]);
        assert_eq!(
            lib.coverage(&repo_path("lib/tests/a.rs"), &rules),
            PathCoverage::Covered
        );
        assert_eq!(
            lib.coverage(&repo_path("lib/fixtures/a.rs"), &rules),
            PathCoverage::TestFile
        );
    }

    #[test]
    fn shared_files_need_their_own_file_entry_at_any_depth() {
        let root = scope([ScopeEntry::RepoRoot { tests: true }]);
        assert_eq!(cover(&root, "Cargo.lock"), PathCoverage::SharedFile);
        assert_eq!(
            cover(&root, "web/package-lock.json"),
            PathCoverage::SharedFile
        );
        let web = scope([folder("web")]);
        assert_eq!(cover(&web, "web/yarn.lock"), PathCoverage::SharedFile);
        let named = scope([new_file("web/yarn.lock"), file("Cargo.lock")]);
        assert_eq!(cover(&named, "web/yarn.lock"), PathCoverage::Covered);
        assert_eq!(cover(&named, "Cargo.lock"), PathCoverage::Covered);
    }

    #[test]
    fn coverage_reasons_name_the_rule() {
        assert_eq!(PathCoverage::Covered.to_string(), "covered by the scope");
        assert_eq!(
            PathCoverage::Protected {
                rule: repo_path(".acan")
            }
            .to_string(),
            "protected folder .acan/ is never writable by an order"
        );
        assert_eq!(
            PathCoverage::CheckFile.to_string(),
            "a check file, whose checks are frozen at approval"
        );
        assert_eq!(
            PathCoverage::TestFile.to_string(),
            "a test file: name it with a file entry, or use a folder entry with tests on"
        );
        assert_eq!(
            PathCoverage::SharedFile.to_string(),
            "a shared file: only a file entry can name it"
        );
        assert_eq!(PathCoverage::Outside.to_string(), "outside the scope");
    }

    #[test]
    fn merge_drops_entries_the_scope_already_grants_and_is_idempotent() {
        let merged = scope([
            folder("src"),
            file("src/app.rs"),
            folder("src/x"),
            file("src/x/y.rs"),
            file("docs/readme.md"),
        ])
        .merge_redundant(&RepoRules::default());
        assert_eq!(
            merged.entries().to_vec(),
            vec![folder("src"), file("docs/readme.md")]
        );
        assert_eq!(
            merged.merge_redundant(&RepoRules::default()),
            merged,
            "merging twice changes nothing"
        );
    }

    #[test]
    fn merge_keeps_test_and_shared_files_under_a_plain_folder() {
        let before = scope([
            folder("src"),
            file("src/Cargo.lock"),
            file("src/tests/unit.rs"),
        ]);
        assert_eq!(before.merge_redundant(&RepoRules::default()), before);
    }

    #[test]
    fn merge_keeps_a_child_folder_that_adds_test_access_and_drops_one_that_adds_none() {
        let kept = scope([folder("src"), tests_folder("src/x")]);
        assert_eq!(kept.merge_redundant(&RepoRules::default()), kept);
        let dropped = scope([tests_folder("lib"), folder("lib/x")]);
        assert_eq!(
            dropped
                .merge_redundant(&RepoRules::default())
                .entries()
                .to_vec(),
            vec![tests_folder("lib")]
        );
    }

    #[test]
    fn merge_keeps_the_root_and_a_root_level_shared_file() {
        let root = scope([
            ScopeEntry::RepoRoot { tests: false },
            new_file("Cargo.lock"),
            file("src/app.rs"),
        ]);
        assert_eq!(
            root.merge_redundant(&RepoRules::default())
                .entries()
                .to_vec(),
            vec![
                ScopeEntry::RepoRoot { tests: false },
                new_file("Cargo.lock")
            ]
        );
    }

    #[test]
    fn case_collisions_pair_each_spelling_with_the_smallest() {
        let paths = [
            repo_path("src/main.rs"),
            repo_path("src/Main.rs"),
            repo_path("docs/a.md"),
            repo_path("SRC/MAIN.rs"),
            repo_path("src/main.rs"),
        ];
        assert_eq!(
            detect_case_collisions(&paths),
            vec![
                (repo_path("SRC/MAIN.rs"), repo_path("src/Main.rs")),
                (repo_path("SRC/MAIN.rs"), repo_path("src/main.rs")),
            ]
        );
        assert!(detect_case_collisions(&[repo_path("src/a.rs"), repo_path("src/b.rs")]).is_empty());
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

    fn path_list(raw: &[&str]) -> Vec<RepoPath> {
        raw.iter().copied().map(repo_path).collect()
    }

    fn draft(
        proposed: &PathScope,
        tracked: &[&str],
        dirs: &[&str],
    ) -> Result<DraftSummary, Vec<DraftError>> {
        validate_draft(
            proposed,
            &path_list(tracked),
            &path_list(dirs),
            &RepoRules::default(),
            50,
        )
    }

    fn broad_flag(proposed: &PathScope, tracked: &[&str], threshold: u8) -> bool {
        validate_draft(
            proposed,
            &path_list(tracked),
            &path_list(&["a", "b"]),
            &RepoRules::default(),
            threshold,
        )
        .expect("draft is valid")
        .needs_broad_confirmation
    }

    fn overlap_names(left: &PathScope, right: &PathScope) -> Vec<Overlap> {
        let forward = left.overlaps(right);
        assert_eq!(forward, right.overlaps(left), "overlap is symmetric");
        forward
    }

    #[test]
    fn draft_accepts_new_files_under_an_existing_or_covered_folder() {
        let proposed = scope([
            folder("src"),
            new_file("src/app.rs"),
            new_file("src/deep/new.rs"),
            new_file("README.new.md"),
        ]);
        let summary = draft(&proposed, &["src/old.rs"], &["src"]).expect("draft is valid");
        assert_eq!(summary.covered_file_count, 1);
        assert_eq!(summary.total_tracked_files, 1);
    }

    #[test]
    fn draft_rejects_a_file_that_is_missing_and_not_marked_new() {
        let proposed = scope([file("src/gone.rs")]);
        assert_eq!(
            draft(&proposed, &["src/app.rs"], &["src"]),
            Err(vec![DraftError::MissingFile {
                path: repo_path("src/gone.rs"),
            }])
        );
        let spelled = scope([file("SRC/APP.rs")]);
        assert!(
            draft(&spelled, &["src/app.rs"], &["src"]).is_ok(),
            "existence ignores case"
        );
    }

    #[test]
    fn draft_rejects_a_new_file_whose_folder_is_missing_and_uncovered() {
        let proposed = scope([new_file("docs/new/x.md")]);
        assert_eq!(
            draft(&proposed, &[], &[]),
            Err(vec![DraftError::MissingParent {
                path: repo_path("docs/new/x.md"),
            }])
        );
        assert!(draft(&scope([new_file("docs/x.md")]), &[], &["docs"]).is_ok());
        assert!(
            draft(&scope([new_file("notes.md")]), &[], &[]).is_ok(),
            "the root always exists"
        );
    }

    #[test]
    fn draft_rejects_missing_folders() {
        let proposed = scope([folder("src/gone")]);
        assert_eq!(
            draft(&proposed, &["src/app.rs"], &["src"]),
            Err(vec![DraftError::MissingFolder {
                path: repo_path("src/gone"),
            }])
        );
    }

    #[test]
    fn draft_refuses_protected_and_check_entries_in_scope_order() {
        let rules = RepoRules {
            check_files: vec![repo_path("ci/checks.yml")],
            ..RepoRules::default()
        };
        let proposed = scope([
            file(".factory/hooks.json"),
            folder(".acan"),
            file("ci/checks.yml"),
            file("src/app.rs"),
        ]);
        assert_eq!(
            validate_draft(
                &proposed,
                &path_list(&["ci/checks.yml", "src/app.rs"]),
                &path_list(&["src"]),
                &rules,
                50,
            ),
            Err(vec![
                DraftError::Protected {
                    path: repo_path(".acan"),
                    rule: repo_path(".acan"),
                },
                DraftError::Protected {
                    path: repo_path(".factory/hooks.json"),
                    rule: repo_path(".factory"),
                },
                DraftError::CheckFile {
                    path: repo_path("ci/checks.yml"),
                },
            ])
        );
    }

    #[test]
    fn draft_summary_counts_covered_tracked_files_and_names_top_level_folders() {
        let tracked = [
            "src/a.rs",
            "src/tests/t.rs",
            "docs/x.md",
            "README.md",
            "Cargo.lock",
        ];
        let proposed = scope([folder("src"), file("docs/x.md")]);
        let summary = draft(&proposed, &tracked, &["src", "docs"]).expect("draft is valid");
        assert_eq!(
            summary,
            DraftSummary {
                covered_file_count: 2,
                total_tracked_files: 5,
                top_level_folders: vec!["docs".to_owned(), "src".to_owned()],
                needs_broad_confirmation: false,
            }
        );
    }

    #[test]
    fn broad_confirmation_needs_more_than_the_threshold_share() {
        let tracked = ["a/1.rs", "a/2.rs", "b/3.rs", "b/4.rs"];
        let three = scope([folder("a"), file("b/3.rs")]);
        let two = scope([folder("a")]);
        assert!(broad_flag(&three, &tracked, 50));
        assert!(
            !broad_flag(&two, &tracked, 50),
            "exactly half is not more than half"
        );
        assert!(broad_flag(&two, &tracked, 49));
        assert!(
            !broad_flag(&three, &tracked, 75),
            "three of four is exactly 75 percent"
        );
    }

    #[test]
    fn the_repo_root_always_needs_confirmation() {
        let root = scope([ScopeEntry::RepoRoot { tests: false }]);
        // One of the two files is a test file, so the share is 50 percent and the root is the
        // only reason for the flag at a 100 percent threshold.
        assert!(broad_flag(&root, &["a/1.rs", "a/t_test.rs"], 100));
    }

    #[test]
    fn root_summary_lists_every_top_level_folder_it_covers() {
        let root = scope([ScopeEntry::RepoRoot { tests: true }]);
        let summary =
            draft(&root, &["src/a.rs", "tests/t.rs", "README.md"], &[]).expect("root is valid");
        assert_eq!(summary.covered_file_count, 3);
        assert_eq!(summary.top_level_folders, ["src", "tests"]);
        assert!(summary.needs_broad_confirmation);
    }

    #[test]
    fn a_valid_draft_merges_away_redundant_files_and_keeps_test_and_shared_files() {
        let proposed = scope([
            folder("src"),
            file("src/a.rs"),
            file("src/Cargo.lock"),
            file("src/tests/unit.rs"),
        ]);
        let tracked = ["src/a.rs", "src/Cargo.lock", "src/tests/unit.rs"];
        draft(&proposed, &tracked, &["src"]).expect("draft is valid");
        assert_eq!(
            proposed
                .merge_redundant(&RepoRules::default())
                .entries()
                .to_vec(),
            vec![
                folder("src"),
                file("src/Cargo.lock"),
                file("src/tests/unit.rs")
            ]
        );
    }

    #[test]
    fn draft_and_overlap_reasons_read_as_lowercase_sentences() {
        assert_eq!(
            DraftError::MissingFile {
                path: repo_path("src/gone.rs"),
            }
            .to_string(),
            "file src/gone.rs is not tracked at the base commit and is not marked new"
        );
        assert_eq!(
            DraftError::Protected {
                path: repo_path(".factory/hooks.json"),
                rule: repo_path(".factory"),
            }
            .to_string(),
            "entry .factory/hooks.json is inside protected folder .factory/, which no order may change"
        );
        assert_eq!(
            Overlap::Folder {
                path: repo_path("src"),
            }
            .to_string(),
            "folder src/"
        );
        assert_eq!(
            Overlap::File {
                path: repo_path("src/app.rs"),
            }
            .to_string(),
            "file src/app.rs"
        );
        assert_eq!(Overlap::Root.to_string(), "the repo root");
    }

    #[test]
    fn overlap_at_file_level_ignores_case_and_names_the_smaller_spelling() {
        let left = scope([file("src/App.rs")]);
        let right = scope([file("SRC/app.rs")]);
        assert_eq!(
            overlap_names(&left, &right),
            vec![Overlap::File {
                path: repo_path("SRC/app.rs"),
            }]
        );
        let other = scope([file("src/app.rs.bak")]);
        assert!(overlap_names(&left, &other).is_empty());
    }

    #[test]
    fn file_overlaps_a_folder_only_when_the_folder_holds_it() {
        let named = scope([file("src/app.rs")]);
        assert_eq!(
            overlap_names(&named, &scope([folder("src")])),
            vec![Overlap::File {
                path: repo_path("src/app.rs"),
            }]
        );
        assert!(overlap_names(&named, &scope([folder("lib")])).is_empty());
    }

    #[test]
    fn folder_against_nested_folder_names_the_narrower_one() {
        let outer = scope([folder("src")]);
        let inner = scope([folder("SRC/Deep")]);
        assert_eq!(
            overlap_names(&outer, &inner),
            vec![Overlap::Folder {
                path: repo_path("SRC/Deep"),
            }]
        );
    }

    #[test]
    fn disjoint_siblings_do_not_overlap() {
        assert!(overlap_names(&scope([folder("src/a")]), &scope([folder("src/ab")])).is_empty());
        assert!(overlap_names(&scope([file("src/a")]), &scope([folder("src/ab")])).is_empty());
        assert!(overlap_names(&scope([folder("src/a")]), &scope([file("src/ab")])).is_empty());
        assert!(overlap_names(&scope([file("src/a")]), &scope([file("src/ab")])).is_empty());
    }

    #[test]
    fn the_repo_root_overlaps_any_entry_and_names_that_entry() {
        let root = scope([ScopeEntry::RepoRoot { tests: false }]);
        assert_eq!(
            overlap_names(&root, &scope([file("x.md")])),
            vec![Overlap::File {
                path: repo_path("x.md"),
            }]
        );
        assert_eq!(
            overlap_names(&root, &scope([folder("src")])),
            vec![Overlap::Folder {
                path: repo_path("src"),
            }]
        );
        assert_eq!(overlap_names(&root, &root), vec![Overlap::Root]);
    }

    #[test]
    fn overlaps_report_each_place_once_across_entry_pairs() {
        let left = scope([folder("SRC"), file("src/a.rs")]);
        let right = scope([folder("src"), file("SRC/A.rs")]);
        assert_eq!(
            overlap_names(&left, &right),
            vec![
                Overlap::Folder {
                    path: repo_path("SRC"),
                },
                Overlap::File {
                    path: repo_path("SRC/A.rs"),
                },
            ]
        );
    }
}
