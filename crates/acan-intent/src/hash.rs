//! Content addresses: SHA-256 over canonical JSON (RFC 8785), written `sha256:<64 hex>`.
//!
//! Every identity that must survive a restart or a second machine (a probe, a scope, an
//! observation, a receipt) is the hash of its canonical JSON. Canonical form sorts keys and
//! fixes number formatting, so the hash does not depend on field order or on which crate
//! enabled `serde_json/preserve_order`.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::types::InvalidValue;

const PREFIX: &str = "sha256:";

/// Error returned when a value has no canonical JSON form, such as a map with non-string keys.
#[derive(Debug, thiserror::Error)]
#[error("value has no canonical JSON form: {0}")]
pub struct CanonicalError(#[from] serde_json::Error);

/// The canonical JSON bytes of `value` (RFC 8785: sorted keys, no whitespace, fixed numbers).
///
/// # Errors
///
/// Returns [`CanonicalError`] when `value` cannot be represented as JSON.
pub fn canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, CanonicalError> {
    Ok(serde_jcs::to_vec(value)?)
}

/// A SHA-256 digest. Displays and serializes as `sha256:` followed by 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// The digest of raw bytes, such as a file's contents.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// The digest of `value`'s canonical JSON.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] when `value` cannot be represented as JSON.
    pub fn of<T: Serialize + ?Sized>(value: &T) -> Result<Self, CanonicalError> {
        Ok(Self::of_bytes(&canonical_json(value)?))
    }

    /// The raw digest.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PREFIX)?;
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for ContentHash {
    type Err = InvalidValue;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let malformed = || InvalidValue::Malformed {
            what: "content hash",
            raw: raw.to_owned(),
        };
        let hex = raw.strip_prefix(PREFIX).ok_or_else(malformed)?;
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(malformed());
        }
        let mut digest = [0u8; 32];
        for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| malformed())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| malformed())?;
        }
        Ok(Self(digest))
    }
}

impl Serialize for ContentHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ContentHash {
    fn schema_name() -> Cow<'static, str> {
        "ContentHash".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "pattern": "^sha256:[0-9a-f]{64}$" })
    }
}

/// Defines a digest newtype, so a probe hash cannot be passed where a scope hash is expected.
macro_rules! digest {
    ($($(#[$meta:meta])* $name:ident;)+) => {$(
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(ContentHash);

        impl From<ContentHash> for $name {
            fn from(hash: ContentHash) -> Self {
                Self(hash)
            }
        }

        impl $name {
            /// The underlying digest.
            #[must_use]
            pub fn content(self) -> ContentHash {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = InvalidValue;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                raw.parse().map(Self)
            }
        }
    )+};
}

digest! {
    /// A probe's identity: the hash of its [`crate::ProbeSpec`].
    ProbeHash;
    /// The hash of a [`crate::ProbeSetManifest`]: every effective probe whole, normalization
    /// plan included.
    ProbeSetHash;
    /// The hash of one [`crate::NormalizationPlan`], bound into masks and decisions.
    NormalizationHash;
    /// A confirmed scope's identity: the hash of its [`crate::Scope`].
    ScopeHash;
    /// The hash of one [`crate::Selector`].
    SelectorHash;
    /// The hash of one [`crate::TargetExample`].
    ExampleHash;
    /// The hash of one [`crate::Observation`].
    ObservationHash;
    /// The hash of one retained [`crate::ContrastExample`] a ruling was checked against.
    ContrastExampleHash;
    /// The hash of a raw evidence blob kept in the local store: a response body, a header set,
    /// a captured value, or a sink dump.
    EvidenceHash;
    /// The hash of a [`crate::Receipt`].
    ReceiptHash;
    /// The hash of a predicate AST.
    PredicateHash;
    /// The hash of a [`crate::Fixture`].
    FixtureHash;
    /// The hash of a [`crate::FixtureSetManifest`]: every fixture of the corpus, by id.
    FixtureSetHash;
    /// The hash of a [`crate::RunnerManifest`]: the runner build and the app contract.
    RunnerHash;
    /// The hash of one file of the tested artifact, such as a lockfile.
    ArtifactHash;
    /// The hash of the ledger up to and including one event.
    LedgerHash;
    /// The hash of an [`crate::OverlayManifest`]: the protected files materialized onto the
    /// prepared commit.
    OverlayHash;
    /// The hash of an [`crate::EnvManifest`]: variables, clock, and seed.
    EnvHash;
    /// The hash of a [`crate::ContainerPolicy`]: the isolation every run and test command had.
    SandboxPolicyHash;
    /// A container image's registry digest, which pins the exact image content.
    ImageDigest;
    /// A discrepancy episode's identity: the hash of its [`crate::EpisodeKey`].
    EpisodeId;
    /// A repair round's identity: the hash of its [`crate::RoundKey`].
    RoundId;
    /// A round result's identity: the hash of its [`crate::ResultKey`].
    ResultId;
}
