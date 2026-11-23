//! Flat, propose-only content for generative UI views.
//!
//! Tool schemas describe the argument shape. Acan checks every proposal before it can be shown.
//! A validated proposal is still only a proposal: it does not change an order or turn a claim
//! into evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;
use std::net::Ipv6Addr;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::RepoPath;

/// The most blocks one proposal may carry.
pub const MAX_BLOCKS: usize = 24;
/// The largest proposal, in JSON bytes.
pub const MAX_TOTAL_BYTES: usize = 32 * 1024;
/// The most characters in a short label or title.
pub const MAX_LABEL_CHARS: usize = 200;
/// The most characters in prose, notes, and other display text.
pub const MAX_PROSE_CHARS: usize = 2_000;
/// The most characters in each code, diff, or output field.
pub const MAX_SNIPPET_CHARS: usize = 8_192;
/// The most characters in a path or link target.
pub const MAX_TARGET_CHARS: usize = 2_000;
/// The most items in one list block.
pub const MAX_LIST_ITEMS: usize = 32;
/// The most columns in one table.
pub const MAX_TABLE_COLUMNS: usize = 8;
/// The most rows in one table.
pub const MAX_TABLE_ROWS: usize = 50;
/// The most scope paths in a draft order.
pub const MAX_SCOPE_PATHS: usize = 64;
/// The most checks in a draft order.
pub const MAX_CHECKS: usize = 16;
/// The most calls one place and view may receive in a turn.
pub const MAX_CALLS_PER_VIEW_PER_TURN: u8 = 3;

/// Where proposed blocks are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenUiPlace {
    /// An order draft, before approval.
    DraftOrder,
    /// A card's proposed explanation of evidence.
    CardEvidence,
    /// Notes accompanying a receipt.
    ReceiptNotes,
    /// A proposed report.
    Report,
}

impl GenUiPlace {
    /// The MCP tool that accepts proposals for this place.
    #[must_use]
    pub const fn tool_name(self) -> &'static str {
        match self {
            Self::DraftOrder => "draft_order",
            Self::CardEvidence => "card_evidence",
            Self::ReceiptNotes => "receipt_notes",
            Self::Report => "report",
        }
    }

    /// The proposal place of an exact MCP tool name.
    #[must_use]
    pub fn from_tool_name(name: &str) -> Option<Self> {
        match name {
            "draft_order" => Some(Self::DraftOrder),
            "card_evidence" => Some(Self::CardEvidence),
            "receipt_notes" => Some(Self::ReceiptNotes),
            "report" => Some(Self::Report),
            _ => None,
        }
    }
}

/// The display tone of a callout or badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Info,
    Success,
    Warning,
    Danger,
}

/// One key and its displayed value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyValueItem {
    pub key: String,
    pub value: String,
}

/// A proposed checklist entry, not a recorded check result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckItem {
    pub label: String,
    pub passed: bool,
    pub detail: Option<String>,
}

/// One display block. Blocks carry no identity, children, or actions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Block {
    /// A single-line heading, from level 1 to level 3.
    Heading { level: u8, text: String },
    /// Plain text, possibly spanning lines.
    Text { text: String },
    /// Emphasized text with an optional title.
    Callout {
        tone: Tone,
        title: Option<String>,
        text: String,
    },
    /// A list of labelled values.
    KeyValue {
        #[serde(deserialize_with = "object_list")]
        items: Vec<KeyValueItem>,
    },
    /// A list of plain text items.
    BulletList { items: Vec<String> },
    /// A list of proposed check statuses.
    CheckList {
        #[serde(deserialize_with = "object_list")]
        items: Vec<CheckItem>,
    },
    /// A short status label with a tone.
    StatusBadge { tone: Tone, label: String },
    /// A value and its optional change, both shown as text.
    Metric {
        label: String,
        value: String,
        delta: Option<String>,
    },
    /// A repo-relative file reference with an optional one-based line number.
    FileRef {
        path: String,
        line: Option<u32>,
        note: Option<String>,
    },
    /// Before and after excerpts from one repo-relative path.
    DiffSnippet {
        path: String,
        before: String,
        after: String,
        note: Option<String>,
    },
    /// A proposed code excerpt.
    CodeSnippet {
        language: Option<String>,
        code: String,
        caption: Option<String>,
    },
    /// Display-only command and output; the command is not executed.
    CommandOutput {
        command: String,
        exit_code: i32,
        stdout: String,
        stderr: Option<String>,
    },
    /// Column labels and equally wide rows of text.
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// A count out of a positive total.
    Progress {
        label: String,
        current: u32,
        total: u32,
    },
    /// A labelled HTTP(S) URL or literal repo-relative path.
    Link { label: String, target: String },
    /// A visual separator.
    Divider {},
}

/// Arguments to `draft_order`. They propose an order without opening or approving it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftOrderProposal {
    pub intent: String,
    pub scope_paths: Vec<String>,
    pub checks: Vec<String>,
    pub summary: Option<String>,
    #[serde(deserialize_with = "object_list")]
    pub blocks: Vec<Block>,
}

/// Arguments to `card_evidence`. The summary and blocks do not become evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardEvidenceProposal {
    pub summary: String,
    #[serde(deserialize_with = "object_list")]
    pub blocks: Vec<Block>,
}

/// Arguments to `receipt_notes`. They accompany a receipt without changing it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptNotesProposal {
    pub summary: Option<String>,
    #[serde(deserialize_with = "object_list")]
    pub blocks: Vec<Block>,
}

/// Arguments to `report`. They supply a title and proposed report content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportProposal {
    pub title: String,
    pub summary: Option<String>,
    #[serde(deserialize_with = "object_list")]
    pub blocks: Vec<Block>,
}

/// The in-memory proposal envelope. On the wire the tool name selects the argument type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proposal {
    DraftOrder(DraftOrderProposal),
    CardEvidence(CardEvidenceProposal),
    ReceiptNotes(ReceiptNotesProposal),
    Report(ReportProposal),
}

impl Proposal {
    /// Where this proposal belongs.
    #[must_use]
    pub const fn place(&self) -> GenUiPlace {
        match self {
            Self::DraftOrder(_) => GenUiPlace::DraftOrder,
            Self::CardEvidence(_) => GenUiPlace::CardEvidence,
            Self::ReceiptNotes(_) => GenUiPlace::ReceiptNotes,
            Self::Report(_) => GenUiPlace::Report,
        }
    }

    /// The proposed blocks, in display order.
    #[must_use]
    pub fn blocks(&self) -> &[Block] {
        match self {
            Self::DraftOrder(proposal) => &proposal.blocks,
            Self::CardEvidence(proposal) => &proposal.blocks,
            Self::ReceiptNotes(proposal) => &proposal.blocks,
            Self::Report(proposal) => &proposal.blocks,
        }
    }
}

/// A proposal that passed Acan's checks. This type cannot be deserialized directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedProposal(Proposal);

impl ValidatedProposal {
    /// Reads the validated proposal.
    #[must_use]
    pub fn proposal(&self) -> &Proposal {
        &self.0
    }

    /// Where this proposal belongs.
    #[must_use]
    pub fn place(&self) -> GenUiPlace {
        self.0.place()
    }

    /// The validated blocks, in display order.
    #[must_use]
    pub fn blocks(&self) -> &[Block] {
        self.0.blocks()
    }

    /// Unwraps the proposal, discarding the validation guarantee.
    #[must_use]
    pub fn into_inner(self) -> Proposal {
        self.0
    }
}

/// Why a proposed tool call was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GenUiError {
    /// An object reused a decoded key.
    #[error("duplicate JSON object key {key:?}")]
    DuplicateKey { key: String },
    /// The JSON was malformed or did not match the tool's argument type.
    #[error("invalid proposal JSON: {message}")]
    InvalidJson { message: String },
    /// The input exceeded the JSON byte limit.
    #[error("proposal exceeds {limit} JSON bytes")]
    PayloadTooLarge { limit: usize },
    /// The proposal had too many display blocks.
    #[error("proposal contains more than {limit} blocks")]
    TooManyBlocks { limit: usize },
    /// A field exceeded a bound or contained invalid text.
    #[error("invalid {field}: {reason}")]
    InvalidField { field: &'static str, reason: String },
    /// A file reference or draft scope path was not portable and repo-relative.
    #[error("{field} must be a portable repo-relative path within the length limit")]
    InvalidPath { field: &'static str },
    /// A link was neither an allowed web URL nor a safe repo-relative path.
    #[error("link target must be an HTTP(S) URL with a host or a portable repo-relative path")]
    InvalidLink,
    /// A row had a different width from the column list. The row index starts at zero.
    #[error("table row {row} has {actual} cells, expected {expected}")]
    TableShapeMismatch {
        row: usize,
        expected: usize,
        actual: usize,
    },
    /// Progress had no positive total or exceeded that total.
    #[error("invalid progress: {current} of {total}")]
    InvalidProgress { current: u32, total: u32 },
    /// This place and view reached its call limit for the turn.
    #[error("{} call budget of {limit} per view per turn exceeded", place.tool_name())]
    BudgetExceeded { place: GenUiPlace, limit: u8 },
}

/// Checks raw MCP arguments before decoding the proposal for the given tool.
///
/// # Errors
///
/// Refuses oversized input, duplicate object keys at any depth, malformed or incorrectly
/// shaped JSON, and proposals outside the content limits.
pub fn parse_and_validate_proposal(
    place: GenUiPlace,
    raw_json: &str,
) -> Result<ValidatedProposal, GenUiError> {
    validate_size(raw_json.len())?;
    check_duplicate_keys(raw_json)?;
    let proposal = match place {
        GenUiPlace::DraftOrder => decode_object(raw_json).map(Proposal::DraftOrder),
        GenUiPlace::CardEvidence => decode_object(raw_json).map(Proposal::CardEvidence),
        GenUiPlace::ReceiptNotes => decode_object(raw_json).map(Proposal::ReceiptNotes),
        GenUiPlace::Report => decode_object(raw_json).map(Proposal::Report),
    }
    .map_err(invalid_json)?;
    validate_fields(&proposal)?;
    Ok(ValidatedProposal(proposal))
}

/// Checks an in-memory proposal using the same field rules as the MCP boundary.
///
/// # Errors
///
/// Refuses invalid content or an envelope whose compact JSON exceeds the byte limit.
pub fn validate_proposal(proposal: Proposal) -> Result<ValidatedProposal, GenUiError> {
    validate_fields(&proposal)?;
    let json = match &proposal {
        Proposal::DraftOrder(proposal) => serde_json::to_vec(proposal),
        Proposal::CardEvidence(proposal) => serde_json::to_vec(proposal),
        Proposal::ReceiptNotes(proposal) => serde_json::to_vec(proposal),
        Proposal::Report(proposal) => serde_json::to_vec(proposal),
    }
    .map_err(invalid_json)?;
    validate_size(json.len())?;
    Ok(ValidatedProposal(proposal))
}

fn validate_size(bytes: usize) -> Result<(), GenUiError> {
    if bytes > MAX_TOTAL_BYTES {
        return Err(GenUiError::PayloadTooLarge {
            limit: MAX_TOTAL_BYTES,
        });
    }
    Ok(())
}

fn validate_fields(proposal: &Proposal) -> Result<(), GenUiError> {
    let blocks = proposal.blocks();
    if blocks.len() > MAX_BLOCKS {
        return Err(GenUiError::TooManyBlocks { limit: MAX_BLOCKS });
    }
    match proposal {
        Proposal::DraftOrder(draft) => {
            required_text("intent", &draft.intent, MAX_PROSE_CHARS)?;
            count("scope_paths", draft.scope_paths.len(), 0, MAX_SCOPE_PATHS)?;
            for path in &draft.scope_paths {
                validate_path("scope_paths", path)?;
            }
            count("checks", draft.checks.len(), 0, MAX_CHECKS)?;
            for check in &draft.checks {
                required_text("checks", check, MAX_PROSE_CHARS)?;
            }
            optional_text("summary", draft.summary.as_deref(), MAX_PROSE_CHARS)?;
        }
        Proposal::CardEvidence(card) => {
            required_text("summary", &card.summary, MAX_PROSE_CHARS)?;
        }
        Proposal::ReceiptNotes(notes) => {
            if let Some(summary) = &notes.summary {
                required_text("summary", summary, MAX_PROSE_CHARS)?;
            } else if blocks.is_empty() {
                return Err(invalid_field("summary", "required when blocks is empty"));
            }
        }
        Proposal::Report(report) => {
            required_text("title", &report.title, MAX_LABEL_CHARS)?;
            optional_text("summary", report.summary.as_deref(), MAX_PROSE_CHARS)?;
        }
    }
    blocks.iter().try_for_each(validate_block)
}

fn validate_block(block: &Block) -> Result<(), GenUiError> {
    match block {
        Block::Heading { level, text } => {
            if !(1..=3).contains(level) {
                return Err(invalid_field("level", "must be 1, 2, or 3"));
            }
            required_text("text", text, MAX_LABEL_CHARS)?;
        }
        Block::Text { text } => multiline_text("text", text, MAX_PROSE_CHARS, true)?,
        Block::Callout { title, text, .. } => {
            optional_text("title", title.as_deref(), MAX_LABEL_CHARS)?;
            multiline_text("text", text, MAX_PROSE_CHARS, true)?;
        }
        Block::KeyValue { items } => {
            count("items", items.len(), 1, MAX_LIST_ITEMS)?;
            for item in items {
                required_text("key", &item.key, MAX_LABEL_CHARS)?;
                single_line("value", &item.value, MAX_PROSE_CHARS, false)?;
            }
        }
        Block::BulletList { items } => {
            count("items", items.len(), 1, MAX_LIST_ITEMS)?;
            for item in items {
                required_text("items", item, MAX_PROSE_CHARS)?;
            }
        }
        Block::CheckList { items } => {
            count("items", items.len(), 1, MAX_LIST_ITEMS)?;
            for item in items {
                required_text("label", &item.label, MAX_LABEL_CHARS)?;
                optional_text("detail", item.detail.as_deref(), MAX_PROSE_CHARS)?;
            }
        }
        Block::StatusBadge { label, .. } => required_text("label", label, MAX_LABEL_CHARS)?,
        Block::Metric {
            label,
            value,
            delta,
        } => {
            required_text("label", label, MAX_LABEL_CHARS)?;
            required_text("value", value, MAX_LABEL_CHARS)?;
            optional_text("delta", delta.as_deref(), MAX_LABEL_CHARS)?;
        }
        Block::FileRef { path, line, note } => {
            validate_path("path", path)?;
            if *line == Some(0) {
                return Err(invalid_field("line", "must be at least 1"));
            }
            optional_text("note", note.as_deref(), MAX_PROSE_CHARS)?;
        }
        Block::DiffSnippet {
            path,
            before,
            after,
            note,
        } => {
            validate_path("path", path)?;
            multiline_text("before", before, MAX_SNIPPET_CHARS, false)?;
            multiline_text("after", after, MAX_SNIPPET_CHARS, false)?;
            optional_text("note", note.as_deref(), MAX_PROSE_CHARS)?;
        }
        Block::CodeSnippet {
            language,
            code,
            caption,
        } => {
            optional_text("language", language.as_deref(), MAX_LABEL_CHARS)?;
            multiline_text("code", code, MAX_SNIPPET_CHARS, true)?;
            optional_text("caption", caption.as_deref(), MAX_PROSE_CHARS)?;
        }
        Block::CommandOutput {
            command,
            stdout,
            stderr,
            ..
        } => {
            required_text("command", command, MAX_PROSE_CHARS)?;
            multiline_text("stdout", stdout, MAX_SNIPPET_CHARS, false)?;
            if let Some(stderr) = stderr {
                multiline_text("stderr", stderr, MAX_SNIPPET_CHARS, false)?;
            }
        }
        Block::Table { columns, rows } => {
            count("columns", columns.len(), 1, MAX_TABLE_COLUMNS)?;
            count("rows", rows.len(), 0, MAX_TABLE_ROWS)?;
            for column in columns {
                required_text("columns", column, MAX_LABEL_CHARS)?;
            }
            for (row, cells) in rows.iter().enumerate() {
                if cells.len() != columns.len() {
                    return Err(GenUiError::TableShapeMismatch {
                        row,
                        expected: columns.len(),
                        actual: cells.len(),
                    });
                }
                for cell in cells {
                    single_line("rows", cell, MAX_PROSE_CHARS, false)?;
                }
            }
        }
        Block::Progress {
            label,
            current,
            total,
        } => {
            required_text("label", label, MAX_LABEL_CHARS)?;
            if *total == 0 || current > total {
                return Err(GenUiError::InvalidProgress {
                    current: *current,
                    total: *total,
                });
            }
        }
        Block::Link { label, target } => {
            required_text("label", label, MAX_LABEL_CHARS)?;
            if !valid_link(target) {
                return Err(GenUiError::InvalidLink);
            }
        }
        Block::Divider {} => {}
    }
    Ok(())
}

fn invalid_field(field: &'static str, reason: impl Into<String>) -> GenUiError {
    GenUiError::InvalidField {
        field,
        reason: reason.into(),
    }
}

fn count(field: &'static str, len: usize, min: usize, max: usize) -> Result<(), GenUiError> {
    if !(min..=max).contains(&len) {
        return Err(invalid_field(
            field,
            format!("must contain {min} to {max} entries"),
        ));
    }
    Ok(())
}

fn required_text(field: &'static str, value: &str, max: usize) -> Result<(), GenUiError> {
    single_line(field, value, max, true)
}

fn optional_text(field: &'static str, value: Option<&str>, max: usize) -> Result<(), GenUiError> {
    if let Some(value) = value {
        single_line(field, value, max, false)?;
    }
    Ok(())
}

fn single_line(
    field: &'static str,
    value: &str,
    max: usize,
    required: bool,
) -> Result<(), GenUiError> {
    validate_text(field, value, max, required, false)
}

fn multiline_text(
    field: &'static str,
    value: &str,
    max: usize,
    required: bool,
) -> Result<(), GenUiError> {
    validate_text(field, value, max, required, true)
}

fn validate_text(
    field: &'static str,
    value: &str,
    max: usize,
    required: bool,
    multiline: bool,
) -> Result<(), GenUiError> {
    if exceeds_chars(value, max) {
        return Err(invalid_field(field, format!("exceeds {max} characters")));
    }
    if value
        .chars()
        .any(|c| c.is_control() && !(multiline && matches!(c, '\n' | '\t')))
    {
        return Err(invalid_field(field, "contains a control character"));
    }
    if required && value.trim().is_empty() {
        return Err(invalid_field(field, "must not be empty"));
    }
    Ok(())
}

fn exceeds_chars(value: &str, max: usize) -> bool {
    value.chars().nth(max).is_some()
}

fn validate_path(field: &'static str, path: &str) -> Result<(), GenUiError> {
    if exceeds_chars(path, MAX_TARGET_CHARS) || path.parse::<RepoPath>().is_err() {
        return Err(GenUiError::InvalidPath { field });
    }
    Ok(())
}

fn valid_link(target: &str) -> bool {
    if exceeds_chars(target, MAX_TARGET_CHARS)
        || target
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return false;
    }
    if let Some((scheme, rest)) = target.split_once("://") {
        return (scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http"))
            && has_web_host(rest);
    }
    // RepoPath rejects colons, so schemes without `://` cannot pass as paths either.
    target.parse::<RepoPath>().is_ok()
}

fn has_web_host(rest: &str) -> bool {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    if let Some(ipv6) = host_port.strip_prefix('[') {
        return ipv6.split_once(']').is_some_and(|(host, suffix)| {
            host.parse::<Ipv6Addr>().is_ok()
                && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port))
        });
    }
    let (host, port) = host_port.split_once(':').unwrap_or((host_port, ""));
    !host.is_empty() && !host.contains(['[', ']', '<', '>', '"', '|', '^']) && valid_port(port)
}

fn valid_port(port: &str) -> bool {
    port.is_empty() || (port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok())
}

fn invalid_json(error: serde_json::Error) -> GenUiError {
    GenUiError::InvalidJson {
        message: error.to_string(),
    }
}

// Serde's derived structs can also decode from positional arrays. Require named fields at
// the tool boundary, including for the objects within block lists.
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = Object<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object with named fields")
            }

            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(Object)
            }
        }

        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

fn decode_object<'de, T: Deserialize<'de>>(raw: &'de str) -> serde_json::Result<T> {
    serde_json::from_str::<Object<T>>(raw).map(|object| object.0)
}

fn object_list<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Vec<T>, D::Error> {
    Vec::<Object<T>>::deserialize(deserializer)
        .map(|objects| objects.into_iter().map(|object| object.0).collect())
}

fn check_duplicate_keys(raw: &str) -> Result<(), GenUiError> {
    let mut json = serde_json::Deserializer::from_str(raw);
    let mut duplicate = None;
    let result = CheckKeys(&mut duplicate).deserialize(&mut json);
    if let Some(key) = duplicate {
        return Err(GenUiError::DuplicateKey { key });
    }
    result.and_then(|()| json.end()).map_err(invalid_json)
}

// Walk the syntax before typed decoding can discard anything. Only keys are retained, and
// each object has its own set. The shared slot carries a structured error out of Serde.
struct CheckKeys<'a>(&'a mut Option<String>);

impl<'de> DeserializeSeed<'de> for CheckKeys<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for CheckKeys<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON without duplicate object keys")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if keys.contains(&key) {
                *self.0 = Some(key);
                return Err(de::Error::custom("duplicate object key"));
            }
            keys.insert(key);
            map.next_value_seed(CheckKeys(self.0))?;
        }
        Ok(())
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        while seq.next_element_seed(CheckKeys(self.0))?.is_some() {}
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
}

/// Counts tool calls by place and view for one turn.
#[derive(Clone, Debug, Default)]
pub struct TurnBudget {
    calls: BTreeMap<GenUiPlace, BTreeMap<String, u8>>,
}

impl TurnBudget {
    /// Starts a turn with no calls recorded.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one call and returns how many more are allowed for this place and view.
    ///
    /// # Errors
    ///
    /// Returns [`GenUiError::BudgetExceeded`] after the first three calls.
    pub fn record_call(&mut self, place: GenUiPlace, view_key: &str) -> Result<u8, GenUiError> {
        let views = self.calls.entry(place).or_default();
        let calls = views.entry(view_key.to_owned()).or_default();
        if *calls == MAX_CALLS_PER_VIEW_PER_TURN {
            return Err(GenUiError::BudgetExceeded {
                place,
                limit: MAX_CALLS_PER_VIEW_PER_TURN,
            });
        }
        *calls += 1;
        Ok(MAX_CALLS_PER_VIEW_PER_TURN - *calls)
    }

    /// How many calls remain for this place and view in the current turn.
    #[must_use]
    pub fn remaining(&self, place: GenUiPlace, view_key: &str) -> u8 {
        let calls = self
            .calls
            .get(&place)
            .and_then(|views| views.get(view_key))
            .copied()
            .unwrap_or_default();
        MAX_CALLS_PER_VIEW_PER_TURN - calls
    }

    /// Clears every budget for the next turn.
    pub fn reset_turn(&mut self) {
        self.calls.clear();
    }
}

/// A model-facing, propose-only MCP tool definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpToolSpec {
    pub name: &'static str,
    pub place: GenUiPlace,
    pub description: &'static str,
    pub input_schema: Value,
}

/// Builds the loose argument schema for one place. Block rules are enforced by Acan, not MCP.
#[must_use]
pub fn mcp_tool_spec(place: GenUiPlace) -> McpToolSpec {
    let (description, mut properties, required): (_, _, &[_]) = match place {
        GenUiPlace::DraftOrder => (
            "Propose an order draft for review without opening or approving it.",
            json!({
                "intent": {"type": "string"},
                "scope_paths": {"type": "array", "items": {"type": "string"}},
                "checks": {"type": "array", "items": {"type": "string"}},
                "summary": {"type": "string"}
            }),
            &["intent", "scope_paths", "checks", "blocks"],
        ),
        GenUiPlace::CardEvidence => (
            "Propose a summary and display blocks for a card without changing evidence.",
            json!({"summary": {"type": "string"}}),
            &["summary", "blocks"],
        ),
        GenUiPlace::ReceiptNotes => (
            "Propose notes and display blocks without altering the receipt.",
            json!({"summary": {"type": "string"}}),
            &["blocks"],
        ),
        GenUiPlace::Report => (
            "Propose a report title, summary, and display blocks for review.",
            json!({"title": {"type": "string"}, "summary": {"type": "string"}}),
            &["title", "blocks"],
        ),
    };
    properties["blocks"] = json!({"type": "array", "items": {"type": "object"}});
    McpToolSpec {
        name: place.tool_name(),
        place,
        description,
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }),
    }
}

/// The four propose-only MCP tools.
#[must_use]
pub fn mcp_tool_specs() -> [McpToolSpec; 4] {
    [
        GenUiPlace::DraftOrder,
        GenUiPlace::CardEvidence,
        GenUiPlace::ReceiptNotes,
        GenUiPlace::Report,
    ]
    .map(mcp_tool_spec)
}
