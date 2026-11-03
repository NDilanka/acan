use std::collections::BTreeMap;
use std::ops::Range;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::hash::ContentHash;
use crate::lang::Lang;

/// Identifies a file within one plant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileId(pub u32);

/// Identifies a machine within one plant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MachineId(pub u32);

/// A scanned repo. `revision` changes whenever a scanned file's path or content changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plant {
    pub root: PathBuf,
    pub revision: ContentHash,
    pub machines: Vec<Machine>,
    pub belts: Vec<Belt>,
    pub files: Vec<SourceFile>,
    /// State machines found in the files, sorted by file and line.
    #[serde(default)]
    pub state_machines: Vec<StateMachine>,
}

/// A state machine the scanner found: a status-like type with its states and the transitions between them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateMachine {
    /// `{file}#{name}`, stable across scans.
    pub id: String,
    /// The declared type name, for example `OrderState`.
    pub name: String,
    /// Repo-relative, forward slashes.
    pub file: String,
    /// 1-based line of the declaration.
    pub line: u32,
    /// The name of the plant machine (map building) that owns `file`. Filled after regroup.
    pub module: String,
    pub states: Vec<State>,
    pub transitions: Vec<Transition>,
    pub confidence: Confidence,
}

/// One state of a [`StateMachine`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub name: String,
    /// 1-based line where the state is declared.
    pub line: u32,
}

/// A move from one state to another that the code performs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    /// The source state, or "*" when the extractor cannot tell it.
    pub from: String,
    pub to: String,
    /// Repo-relative file of the transition site (may differ from the declaration file).
    pub file: String,
    /// 1-based first line of the assignment or match arm.
    pub line: u32,
    /// 1-based last line, inclusive.
    pub end_line: u32,
    /// Enclosing function name, or None outside a function.
    pub via: Option<String>,
}

/// How much of a [`StateMachine`] the scanner could read. `High` when every transition has a known source state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Low,
}

/// A replaceable module, a set of files that the foreman can regroup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub id: MachineId,
    pub name: String,
    /// Common ancestor directory of the member files, repo-relative with forward slashes. `.` for the root.
    pub dir: String,
    pub files: Vec<FileId>,
    pub lang_mix: BTreeMap<Lang, u32>,
}

/// Import edges from one machine to another, aggregated from file-level imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Belt {
    pub from: MachineId,
    pub to: MachineId,
    pub imports: u32,
}

/// A scanned file with its symbols and imports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub id: FileId,
    /// Repo-relative, forward slashes.
    pub path: String,
    pub lang: Lang,
    pub content_hash: ContentHash,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<ImportRef>,
}

/// A named definition in a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub byte_range: Range<usize>,
    pub line_range: LineRange,
    pub exported: bool,
}

/// 1-based, inclusive on both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

/// The kind of a [`Symbol`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Class,
    Method,
    Struct,
    Enum,
    Trait,
    Interface,
    Type,
    Const,
    /// A `let` or `var` binding that is not a function.
    Variable,
    /// An exported name with no local declaration, such as a re-export, a `default` export, or an export of an import.
    Export,
}

/// One import or module declaration in a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportRef {
    /// The module specifier as written, or the expanded `use` path for Rust.
    pub specifier: String,
    pub line: u32,
    pub target: ImportTarget,
}

/// Where an import points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImportTarget {
    /// Resolved to a scanned file in this repo.
    File { file: FileId },
    /// Package or crate outside this repo. Never a belt.
    External,
    /// A relative or crate-local path that does not resolve to a scanned file.
    Unresolved,
}
