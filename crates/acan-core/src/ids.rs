//! Typed identifiers, revisions and counters.
//!
//! Entity ids are minted by the origin from per-world counters, so a replay is deterministic
//! and the core needs no random source. Principals, commits, paths and machine names come
//! from outside and are validated once at the boundary.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Unexpected, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// Error returned when an identifier fails boundary validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseIdError {
    /// The input was empty.
    #[error("identifier is empty")]
    Empty,
    /// The input contained a control character.
    #[error("identifier contains a control character: {0:?}")]
    Control(String),
    /// The input did not have the `prefix-number` form.
    #[error("expected an id like `{prefix}-12`, got {raw:?}")]
    Malformed {
        /// The expected prefix, such as `t` for a task.
        prefix: &'static str,
        /// The rejected input.
        raw: String,
    },
    /// The path was absolute, used backslashes, or contained an empty or `..` segment.
    #[error("path must be repo-relative with forward slashes and no `..`: {0:?}")]
    BadPath(String),
}

macro_rules! counter_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize)]
        #[serde(transparent)]
        pub struct $name(u32);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_any(CounterVisitor).map(Self)
            }
        }

        impl $name {
            pub(crate) const fn new(n: u32) -> Self {
                Self(n)
            }

            pub(crate) const fn get(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "-{}"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                raw.strip_prefix(concat!($prefix, "-"))
                    .and_then(|n| n.parse().ok())
                    .map(Self)
                    .ok_or_else(|| ParseIdError::Malformed { prefix: $prefix, raw: raw.to_owned() })
            }
        }
    };
}

/// Reads a counter id from a JSON number, or from a numeric string when the id is a map key.
///
/// JSON map keys are strings, and serde buffers the content of an internally tagged enum
/// such as `Event`, so a key reaches the id as `"3"` rather than `3`.
struct CounterVisitor;

impl Visitor<'_> for CounterVisitor {
    type Value = u32;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a non-negative integer id")
    }

    fn visit_u64<E: de::Error>(self, n: u64) -> Result<u32, E> {
        u32::try_from(n).map_err(|_| E::invalid_value(Unexpected::Unsigned(n), &self))
    }

    fn visit_i64<E: de::Error>(self, n: i64) -> Result<u32, E> {
        u32::try_from(n).map_err(|_| E::invalid_value(Unexpected::Signed(n), &self))
    }

    fn visit_str<E: de::Error>(self, raw: &str) -> Result<u32, E> {
        raw.parse()
            .map_err(|_| E::invalid_value(Unexpected::Str(raw), &self))
    }
}

counter_id!(
    /// An order posted on a machine. Displays as `o-1`.
    OrderId,
    "o"
);
counter_id!(
    /// A task split from an order. Displays as `t-1`.
    TaskId,
    "t"
);
counter_id!(
    /// A locked witness. Displays as `w-1`.
    WitnessId,
    "w"
);
counter_id!(
    /// A worker's readback of a task. Displays as `r-1`.
    ReadbackId,
    "r"
);
counter_id!(
    /// A question pinned when readbacks disagree. Displays as `q-1`.
    QuestionId,
    "q"
);
counter_id!(
    /// A blocking overlap between two declarations. Displays as `c-1`.
    ClashId,
    "c"
);
counter_id!(
    /// One run of a task's witnesses against one commit. Displays as `g-1`.
    GateRunId,
    "g"
);

macro_rules! ordinal {
    ($(#[$meta:meta])* $name:ident($inner:ty)) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name($inner);

        impl $name {
            pub(crate) const ZERO: Self = Self(0);

            /// Wraps a raw value. The origin, not the caller, decides which values are current.
            #[must_use]
            pub const fn new(n: $inner) -> Self {
                Self(n)
            }

            /// Returns the raw value.
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

ordinal!(
    /// Position of an event in its world log. Gap-free, starting at 0.
    Seq(u64)
);
ordinal!(
    /// Revision of an order's locked witness set. Bumps when a question is answered.
    OrderRev(u32)
);
ordinal!(
    /// Fencing token of a task's lease. Bumps on every grant and never resets.
    Generation(u32)
);

impl Seq {
    // A log is gap-free from 0, so reaching u64::MAX would take 2^64 events.
    pub(crate) const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl OrderRev {
    /// The revision after this one, or `None` when revisions are exhausted.
    pub(crate) fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl Generation {
    /// The generation after this one, or `None` when generations are exhausted. Never wraps:
    /// a reused generation would let a stale fence pass.
    pub(crate) fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// Unix time in milliseconds, supplied by the host in [`crate::Ctx`]. The core never reads a clock.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    /// Wraps a unix time in milliseconds.
    #[must_use]
    pub const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }

    /// Returns the unix time in milliseconds.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// Returns this time plus `ms` milliseconds, saturating at the maximum.
    #[must_use]
    pub const fn plus_millis(self, ms: u64) -> Self {
        Self(self.0.saturating_add(ms))
    }
}

macro_rules! name_id {
    ($(#[$meta:meta])* $name:ident, $validate:path) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Returns the validated string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ParseIdError;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                $validate(&raw)?;
                Ok(Self(raw))
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                Self::try_from(raw.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

name_id!(
    /// A world: one repository's shared order log.
    WorldId,
    validate_name
);
name_id!(
    /// A human member, as resolved by the origin's token store.
    MemberId,
    validate_name
);
name_id!(
    /// An agent principal owned by a member, as resolved by the origin's token store.
    AgentId,
    validate_name
);
name_id!(
    /// A git commit id in the repository's object format.
    CommitId,
    validate_name
);
name_id!(
    /// A machine on the plant, keyed by its name. Scanner machine indices are not stable across regrouping.
    MachineKey,
    validate_name
);
name_id!(
    /// An opaque content revision of a [`crate::CodeIndex`], computed by the host.
    CodeRev,
    validate_name
);
name_id!(
    /// A repo-relative file path with forward slashes.
    FilePath,
    validate_path
);

fn validate_name(raw: &str) -> Result<(), ParseIdError> {
    if raw.is_empty() {
        return Err(ParseIdError::Empty);
    }
    if raw.chars().any(char::is_control) {
        return Err(ParseIdError::Control(raw.to_owned()));
    }
    Ok(())
}

fn validate_path(raw: &str) -> Result<(), ParseIdError> {
    validate_name(raw)?;
    let bad = raw.contains('\\')
        || raw.starts_with('/')
        || raw.contains(':')
        || raw.split('/').any(|seg| seg.is_empty() || seg == "..");
    if bad {
        return Err(ParseIdError::BadPath(raw.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_ids_round_trip_through_display() {
        let id: TaskId = "t-12".parse().expect("valid task id");
        assert_eq!(id.to_string(), "t-12");
        assert!(
            "o-12".parse::<TaskId>().is_err(),
            "prefix must match the type"
        );
    }

    #[test]
    fn counter_ids_deserialize_from_numbers_and_map_keys() {
        let keyed: std::collections::BTreeMap<TaskId, u8> =
            serde_json::from_str(r#"{"3":1}"#).expect("string key parses");
        assert_eq!(keyed.keys().next(), Some(&TaskId::new(3)));
        assert_eq!(
            serde_json::from_str::<TaskId>("3").expect("number parses"),
            TaskId::new(3)
        );
        assert!(serde_json::from_str::<TaskId>("-1").is_err());
        assert!(serde_json::from_str::<TaskId>(r#""t-3""#).is_err());
    }

    #[test]
    fn file_paths_must_be_repo_relative() {
        assert!("src/api/routes.rs".parse::<FilePath>().is_ok());
        for bad in ["", "/etc/passwd", "src\\a.rs", "src/../x", "C:/x", "a//b"] {
            assert!(bad.parse::<FilePath>().is_err(), "accepted {bad:?}");
        }
    }
}
