//! Identifiers, commits, routes, field paths and scalar values shared by every envelope.
//!
//! Each type validates once when it is parsed, so the rest of the gate can trust it.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Error returned when a value fails boundary validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidValue {
    /// The input was empty.
    #[error("{what} is empty")]
    Empty {
        /// What was being parsed.
        what: &'static str,
    },
    /// The input was longer than the limit.
    #[error("{what} is longer than {max} characters")]
    TooLong {
        /// What was being parsed.
        what: &'static str,
        /// The limit.
        max: usize,
    },
    /// The input did not follow the grammar.
    #[error("{what} is malformed: {raw:?}")]
    Malformed {
        /// What was being parsed.
        what: &'static str,
        /// The rejected input.
        raw: String,
    },
}

/// Longest identifier accepted anywhere in the contract. Long enough for a Fireworks model path.
const ID_MAX: usize = 128;

fn id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@')
}

fn validate_id(what: &'static str, raw: &str) -> Result<(), InvalidValue> {
    if raw.is_empty() {
        return Err(InvalidValue::Empty { what });
    }
    if raw.chars().count() > ID_MAX {
        return Err(InvalidValue::TooLong { what, max: ID_MAX });
    }
    if !raw.chars().all(id_char) {
        return Err(InvalidValue::Malformed {
            what,
            raw: raw.to_owned(),
        });
    }
    Ok(())
}

/// Implements string serialization, parsing and a pattern schema for a validated string newtype.
macro_rules! string_newtype {
    ($name:ident, $what:literal, $pattern:literal, $check:expr) => {
        impl $name {
            /// The validated text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidValue;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                let check: fn(&str) -> Result<(), InvalidValue> = $check;
                check(&raw)?;
                Ok(Self(raw))
            }
        }

        impl FromStr for $name {
            type Err = InvalidValue;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                Self::try_from(raw.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({ "type": "string", "pattern": $pattern, "description": $what })
            }
        }
    };
}

/// Defines identifier newtypes: 1 to 128 characters of `A-Z a-z 0-9 . _ - : / @`.
macro_rules! text_id {
    ($($(#[$meta:meta])* $name:ident, $what:literal;)+) => {$(
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct $name(String);

        string_newtype!($name, $what, "^[A-Za-z0-9._:/@-]{1,128}$", |raw| validate_id($what, raw));
    )+};
}

text_id! {
    /// An order, as the core displays it (`o-3`).
    OrderRef, "order id";
    /// A task, as the core displays it (`t-4`).
    TaskRef, "task id";
    /// A scope item, or a receipt item the core minted for a check outside the scope.
    ItemId, "item id";
    /// One acceptance obligation of a scope item or must-keep rule.
    ObligationId, "obligation id";
    /// A must-keep rule of a scope.
    MustKeepId, "must-keep id";
    /// An open choice a human must answer before confirming a scope.
    ChoiceId, "choice id";
    /// One answer key of an open choice.
    OptionKey, "option key";
    /// A machine of the plant: an acan-demo module from the scan, such as `ratelimit`.
    MachineId, "machine id";
    /// A ruling lineage. Every supersession keeps the id and appends a new decision.
    RulingId, "ruling id";
    /// One app run: a probe against one commit in one role.
    RunId, "run id";
    /// One step of a probe or fixture recipe.
    StepId, "step id";
    /// A filed contest.
    ContestId, "contest id";
    /// An authenticated agent principal.
    AgentRef, "agent id";
    /// An authenticated human member.
    HumanRef, "member id";
    /// A model, as the provider names it.
    ModelRef, "model id";
    /// A named user a fixture signs up and logs in, such as `alice`.
    ActorName, "actor name";
    /// A named client identity, mapped by the runner to a client address.
    ClientName, "client name";
    /// A run-local capture binding, such as `alice_session`.
    CaptureName, "capture name";
    /// A fixture of the probe corpus.
    FixtureId, "fixture id";
    /// Version of a normalization plan, such as `norm/1`.
    NormalizationVersion, "normalization version";
    /// Version of the evaluator that classified a run, such as `acan-intent/0.1.0`.
    EvaluatorVersion, "evaluator version";
    /// The app runtime, such as `bun-1.4.4`.
    RuntimeId, "runtime id";
    /// A caption template of the trusted catalog.
    TemplateId, "template id";
    /// A named slot of a caption template.
    SlotName, "slot name";
    /// Version of the trusted view catalog.
    CatalogVersion, "catalog version";
    /// The runner build, such as `acan-witness/0.1.0`, bound into the runner hash.
    RunnerBuild, "runner build";
}

/// A git commit: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CommitSha(String);

string_newtype!(
    CommitSha,
    "git commit",
    "^([0-9a-f]{40}|[0-9a-f]{64})$",
    |raw| {
        let hex = raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if hex && matches!(raw.len(), 40 | 64) {
            Ok(())
        } else {
            Err(InvalidValue::Malformed {
                what: "git commit",
                raw: raw.to_owned(),
            })
        }
    }
);

/// A repo-relative file path in a portable grammar: segments joined by `/`, never a root, a
/// drive or UNC prefix, a `.` or `..` segment, a colon (drive letters and NTFS alternate data
/// streams), a backslash, a character Windows refuses in names, a segment ending in a dot or a
/// space, or a reserved device name such as `NUL` or `com1.txt`.
///
/// The grammar alone does not prove containment: the tool boundary still resolves the joined
/// path and checks it stays inside the worktree without following a symlink out.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RepoPath(String);

/// Device names Windows reserves in every directory, with or without an extension.
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn portable_segment(segment: &str) -> bool {
    let refused_char =
        |c: char| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|');
    let stem = segment.split('.').next().unwrap_or(segment);
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.ends_with(['.', ' '])
        && !segment.chars().any(refused_char)
        && !RESERVED_NAMES
            .iter()
            .any(|name| stem.trim_end().eq_ignore_ascii_case(name))
}

string_newtype!(
    RepoPath,
    "repo-relative path",
    "^[^/\\\\:*?\"<>|\\u0000-\\u001f]+(/[^/\\\\:*?\"<>|\\u0000-\\u001f]+)*$",
    |raw| {
        if raw.split('/').all(portable_segment) {
            Ok(())
        } else {
            Err(InvalidValue::Malformed {
                what: "repo-relative path",
                raw: raw.to_owned(),
            })
        }
    }
);

/// A container image name with an optional tag, as the registry spells it: `oven/bun:1`.
///
/// It starts with a letter or digit, so it can never read as a command-line flag, and holds
/// only `A-Z a-z 0-9 . _ / : -`, so it can never carry a digest, a space, or a mount field
/// separator.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ImageName(String);

/// Longest image name accepted, the registry's own limit.
const IMAGE_NAME_MAX: usize = 255;

string_newtype!(
    ImageName,
    "container image name",
    "^[A-Za-z0-9][A-Za-z0-9._/:-]{0,254}$",
    |raw| {
        let what = "container image name";
        if raw.is_empty() {
            return Err(InvalidValue::Empty { what });
        }
        if raw.len() > IMAGE_NAME_MAX {
            return Err(InvalidValue::TooLong {
                what,
                max: IMAGE_NAME_MAX,
            });
        }
        let leads = raw.starts_with(|c: char| c.is_ascii_alphanumeric());
        let rest = raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | ':' | '-'));
        if leads && rest {
            Ok(())
        } else {
            Err(InvalidValue::Malformed {
                what,
                raw: raw.to_owned(),
            })
        }
    }
);

/// An absolute path inside a container, such as `/work`: one or more `/`-led segments of
/// `A-Z a-z 0-9 . _ -`, never `.` or `..`, and never the root itself.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ContainerPath(String);

string_newtype!(
    ContainerPath,
    "container path",
    "^(/[A-Za-z0-9._-]+)+$",
    |raw| {
        let segment_ok = |segment: &str| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        };
        match raw.strip_prefix('/') {
            Some(rest) if rest.split('/').all(segment_ok) => Ok(()),
            _ => Err(InvalidValue::Malformed {
                what: "container path",
                raw: raw.to_owned(),
            }),
        }
    }
);

/// A route pattern from the app's route table: `/`, `/health`, `/api/links/:code`.
///
/// Segments are literals of `A-Z a-z 0-9 . _ ~ -` or a `:name` parameter.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RouteTemplate(String);

string_newtype!(
    RouteTemplate,
    "route template",
    "^/(([A-Za-z0-9._~-]+|:[A-Za-z_][A-Za-z0-9_]*)(/([A-Za-z0-9._~-]+|:[A-Za-z_][A-Za-z0-9_]*))*)?$",
    |raw| {
        let malformed = || InvalidValue::Malformed {
            what: "route template",
            raw: raw.to_owned(),
        };
        let rest = raw.strip_prefix('/').ok_or_else(malformed)?;
        if rest.is_empty() {
            return Ok(());
        }
        let segment_ok = |seg: &str| match seg.strip_prefix(':') {
            Some(name) => {
                name.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            }
            None => {
                !seg.is_empty()
                    && seg
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-'))
            }
        };
        if rest.split('/').all(segment_ok) {
            Ok(())
        } else {
            Err(malformed())
        }
    }
);

/// Whether a field grammar admits the `[*]` wildcard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wildcards {
    Allowed,
    Refused,
}

fn key_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'$'))
        .count()
}

/// Checks the field grammar: a key or index, then any of `.key`, `[n]`, and (patterns only) `[*]`.
fn valid_field(raw: &str, wildcards: Wildcards) -> bool {
    let bytes = raw.as_bytes();
    let mut i = 0;
    let mut first = true;
    while i < bytes.len() {
        match bytes[i] {
            b'[' => {
                let Some(close) = raw[i..].find(']') else {
                    return false;
                };
                let inner = &raw[i + 1..i + close];
                let index = !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_digit());
                let any = inner == "*" && wildcards == Wildcards::Allowed;
                if !(index || any) {
                    return false;
                }
                i += close + 1;
            }
            b'.' if !first => {
                let len = key_len(&bytes[i + 1..]);
                if len == 0 {
                    return false;
                }
                i += 1 + len;
            }
            _ if first => {
                let len = key_len(&bytes[i..]);
                if len == 0 {
                    return false;
                }
                i += len;
            }
            _ => return false,
        }
        first = false;
    }
    !first
}

/// Which part of a response or side effect a path addresses.
///
/// `Any` appears only in a [`crate::Selector`]; a concrete path always names one channel.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Every channel.
    Any,
    /// The HTTP status code. Never a body field, and never maskable.
    Status,
    /// A response header, by lowercase name.
    Header,
    /// A cookie the response set, by name.
    Cookie,
    /// The JSON response body.
    Body,
    /// A trusted side-effect sink. Its selector's `field_path` is required and starts with
    /// `store` or `outbound_http`.
    SideEffect,
}

/// A sink the runner observes on the app's behalf. The app cannot write into it directly.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TrustedSink {
    /// A snapshot of the app's store through the boot fixture's trusted hook.
    Store,
    /// Outbound HTTP calls captured by the runner's stub server.
    OutboundHttp,
}

/// Splits `channel.rest` into its channel and the field part, checking the channel grammar.
fn parse_channel_path(raw: &str, wildcards: Wildcards) -> Option<Channel> {
    let split = raw.find(['.', '[']).unwrap_or(raw.len());
    let (head, rest) = raw.split_at(split);
    let single_key = |rest: &str| {
        rest.strip_prefix('.')
            .is_some_and(|key| !key.is_empty() && key_len(key.as_bytes()) == key.len())
    };
    // A field part is `.key...` or `[n]...`; `.[n]` is refused so an array root has one
    // spelling, the one the lookup walks.
    let optional_field = |rest: &str| {
        rest.is_empty()
            || rest
                .strip_prefix('.')
                .is_some_and(|f| !f.starts_with('[') && valid_field(f, wildcards))
            || (rest.starts_with('[') && valid_field(rest, wildcards))
    };
    match head {
        "status" if rest.is_empty() => Some(Channel::Status),
        "header" if single_key(rest) => Some(Channel::Header),
        "cookie" if single_key(rest) => Some(Channel::Cookie),
        "body" if optional_field(rest) => Some(Channel::Body),
        "side_effect" => {
            let rest = rest.strip_prefix('.')?;
            let sink_len = key_len(rest.as_bytes());
            let (sink, field) = rest.split_at(sink_len);
            let known = matches!(sink, "store" | "outbound_http");
            (known && optional_field(field)).then_some(Channel::SideEffect)
        }
        _ => None,
    }
}

/// A concrete observation path: `status`, `header.x-ratelimit-remaining`, `cookie.session`,
/// `body.links[0].code`, or `side_effect.store.users[1].name`.
///
/// The channel prefix keeps the HTTP status apart from any body field called `status`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ObservationPath(String);

string_newtype!(
    ObservationPath,
    "observation path",
    "^(status|(header|cookie)\\.[A-Za-z0-9_$-]+|body((\\.[A-Za-z0-9_$-]|\\[).*)?|side_effect\\.(store|outbound_http)((\\.[A-Za-z0-9_$-]|\\[).*)?)$",
    |raw| {
        parse_channel_path(raw, Wildcards::Refused)
            .map(|_| ())
            .ok_or_else(|| InvalidValue::Malformed {
                what: "observation path",
                raw: raw.to_owned(),
            })
    }
);

impl ObservationPath {
    /// The channel this path addresses. Never [`Channel::Any`].
    ///
    /// # Panics
    ///
    /// Never in practice: every constructor, including decoding, validates the path.
    #[must_use]
    pub fn channel(&self) -> Channel {
        parse_channel_path(&self.0, Wildcards::Refused)
            .expect("an ObservationPath is validated when it is built")
    }
}

/// An observation path that may use `[*]` for every element of an array.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PathPattern(String);

string_newtype!(
    PathPattern,
    "observation path pattern",
    "^(status|(header|cookie)\\.[A-Za-z0-9_$-]+|body((\\.[A-Za-z0-9_$-]|\\[).*)?|side_effect\\.(store|outbound_http)((\\.[A-Za-z0-9_$-]|\\[).*)?)$",
    |raw| {
        parse_channel_path(raw, Wildcards::Allowed)
            .map(|_| ())
            .ok_or_else(|| InvalidValue::Malformed {
                what: "observation path pattern",
                raw: raw.to_owned(),
            })
    }
);

impl PathPattern {
    /// The channel this pattern addresses. Never [`Channel::Any`].
    ///
    /// # Panics
    ///
    /// Never in practice: every constructor, including decoding, validates the pattern.
    #[must_use]
    pub fn channel(&self) -> Channel {
        parse_channel_path(&self.0, Wildcards::Allowed)
            .expect("a PathPattern is validated when it is built")
    }
}

/// A path pattern inside one channel, without the channel prefix: `links[*].code`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FieldPattern(String);

string_newtype!(
    FieldPattern,
    "field pattern",
    "^([A-Za-z0-9_$-]+|\\[([0-9]+|\\*)\\])(\\.[A-Za-z0-9_$-]+|\\[([0-9]+|\\*)\\])*$",
    |raw| {
        if valid_field(raw, Wildcards::Allowed) {
            Ok(())
        } else {
            Err(InvalidValue::Malformed {
                what: "field pattern",
                raw: raw.to_owned(),
            })
        }
    }
);

/// A concrete field path inside one channel, without the channel prefix: `links[0].code`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FieldPath(String);

string_newtype!(
    FieldPath,
    "field path",
    "^([A-Za-z0-9_$-]+|\\[[0-9]+\\])(\\.[A-Za-z0-9_$-]+|\\[[0-9]+\\])*$",
    |raw| {
        if valid_field(raw, Wildcards::Refused) {
            Ok(())
        } else {
            Err(InvalidValue::Malformed {
                what: "field path",
                raw: raw.to_owned(),
            })
        }
    }
);

/// A path pattern that base-versus-base disagreement may mask. Never the status channel.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MaskablePath(String);

string_newtype!(
    MaskablePath,
    "maskable path pattern (any channel but status)",
    "^((header|cookie)\\.[A-Za-z0-9_$-]+|body((\\.[A-Za-z0-9_$-]|\\[).*)?|side_effect\\.(store|outbound_http)((\\.[A-Za-z0-9_$-]|\\[).*)?)$",
    |raw| {
        match parse_channel_path(raw, Wildcards::Allowed) {
            Some(Channel::Status) | None => Err(InvalidValue::Malformed {
                what: "maskable path pattern",
                raw: raw.to_owned(),
            }),
            Some(_) => Ok(()),
        }
    }
);

/// A list with at least one element. Decoding an empty list fails.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize)]
#[serde(transparent)]
pub struct NonEmpty<T>(Vec<T>);

impl<T> NonEmpty<T> {
    /// Builds a list from its first element and the rest.
    pub fn new(first: T, rest: impl IntoIterator<Item = T>) -> Self {
        let mut items = vec![first];
        items.extend(rest);
        Self(items)
    }

    /// The first element.
    #[must_use]
    pub fn first(&self) -> &T {
        &self.0[0]
    }

    /// The elements as a slice, never empty.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// How many elements there are, at least one.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always false. Present because `len` is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Iterates over the elements.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }
}

impl<T> TryFrom<Vec<T>> for NonEmpty<T> {
    type Error = InvalidValue;

    fn try_from(items: Vec<T>) -> Result<Self, Self::Error> {
        if items.is_empty() {
            Err(InvalidValue::Empty { what: "list" })
        } else {
            Ok(Self(items))
        }
    }
}

impl<T> From<NonEmpty<T>> for Vec<T> {
    fn from(list: NonEmpty<T>) -> Self {
        list.0
    }
}

impl<'a, T> IntoIterator for &'a NonEmpty<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NonEmpty<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let items = Vec::<T>::deserialize(deserializer)?;
        Self::try_from(items).map_err(serde::de::Error::custom)
    }
}

impl<T: JsonSchema> JsonSchema for NonEmpty<T> {
    fn schema_name() -> Cow<'static, str> {
        format!("NonEmpty_{}", T::schema_name()).into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "array",
            "items": generator.subschema_for::<T>(),
            "minItems": 1
        })
    }
}

/// Milliseconds since the Unix epoch.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct UnixMillis(pub u64);

/// A scope revision of one order. Revision 1 is the first draft.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ScopeRev(pub u32);

/// A JSON scalar a human or a model wrote as a literal: null, a boolean, a number, or a string.
///
/// Observed values keep their structure as [`Evidence`]; a scalar is only ever a literal.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Scalar {
    /// JSON `null`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A number, kept exactly as `serde_json` parsed it.
    Number(serde_json::Number),
    /// A string.
    Text(String),
}

/// What one path held in one observation, target or prediction: a present JSON value, which may
/// be `null`, an empty object or an empty array, or nothing at all.
///
/// A missing header and a header observed as `null` are different evidence, and so are a body
/// with `"links": []` and a body with no `links` key.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Evidence {
    /// The path held this value.
    Present {
        /// The value, with its structure.
        value: serde_json::Value,
    },
    /// The path held nothing: no such header, cookie, key or element.
    Missing {},
}

/// Decodes a present field as `Some`, so JSON `null` stays a value and only a missing field
/// (with `#[serde(default)]`) is `None`.
pub(crate) fn present_json<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

/// Defines an integer newtype bounded at decode time.
macro_rules! bounded {
    ($(#[$meta:meta])* $name:ident($inner:ty), $min:literal ..= $max:literal, $what:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name($inner);

        impl $name {
            /// The smallest accepted value.
            pub const MIN: $inner = $min;
            /// The largest accepted value.
            pub const MAX: $inner = $max;

            /// The value.
            #[must_use]
            pub fn get(self) -> $inner {
                self.0
            }
        }

        impl TryFrom<$inner> for $name {
            type Error = $crate::types::InvalidValue;

            fn try_from(value: $inner) -> Result<Self, Self::Error> {
                if (Self::MIN..=Self::MAX).contains(&value) {
                    Ok(Self(value))
                } else {
                    Err($crate::types::InvalidValue::Malformed {
                        what: $what,
                        raw: value.to_string(),
                    })
                }
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::try_from(<$inner as serde::Deserialize>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }

        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "integer", "minimum": $min, "maximum": $max })
            }
        }
    };
}

pub(crate) use bounded;

/// An HTTP method a probe step sends.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// GET.
    Get,
    /// HEAD.
    Head,
    /// POST.
    Post,
    /// PUT.
    Put,
    /// PATCH.
    Patch,
    /// DELETE.
    Delete,
    /// OPTIONS.
    Options,
}

/// The method part of a selector. `ANY` is a broad grant and needs its own acknowledgement.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "UPPERCASE")]
pub enum MethodSelector {
    /// Every method.
    Any,
    /// GET.
    Get,
    /// HEAD.
    Head,
    /// POST.
    Post,
    /// PUT.
    Put,
    /// PATCH.
    Patch,
    /// DELETE.
    Delete,
    /// OPTIONS.
    Options,
}

/// The kind of interface a probe drives. v1 runs `http` only; the others decode and refuse.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// HTTP requests against the app's port.
    Http,
    /// A command-line invocation.
    Cli,
    /// A rendered page.
    Page,
    /// A test command.
    TestCmd,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_paths_refuse_windows_prefixes_on_every_host() {
        for raw in [
            "C:/outside",
            "C:outside",
            "c:\\outside",
            "\\\\server\\share\\file",
            "//server/share",
            "/etc/passwd",
            "\\rooted",
            "src/../../outside",
            "./src/main.rs",
            "src/./main.rs",
            "file.txt:stream",
            "file.txt::$DATA",
            "src\\main.rs",
            "NUL",
            "src/com1.txt",
            "lpt9",
            "con .txt",
            "trailing.",
            "trailing ",
            "a//b",
            "",
            "a/",
            "what?",
            "pipe|name",
            "tab\tname",
        ] {
            assert!(raw.parse::<RepoPath>().is_err(), "{raw:?} was accepted");
        }
        for raw in [
            "src/main.rs",
            ".acan/probes/login.json",
            "bun.lock",
            "console.ts",
            "a/b/c.d.e",
            "com10.txt",
            ".gitignore",
        ] {
            assert!(raw.parse::<RepoPath>().is_ok(), "{raw:?} was refused");
        }
    }
}
