use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A 64-bit FNV-1a digest, stable across runs, platforms, and toolchains.
///
/// Serialized as 16 hex digits. JSON numbers above 2^53 lose bits when a JavaScript consumer parses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash(u64);

impl Serialize for ContentHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        u64::from_str_radix(&text, 16)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl ContentHash {
    /// The stand-in for a file no one has read yet. The skeleton of a scan uses it, so its
    /// revision digests the paths alone.
    pub(crate) const UNREAD: Self = Self(0);

    pub(crate) fn of(bytes: &[u8]) -> Self {
        let mut digest = Digest::new();
        digest.update(bytes);
        digest.finish()
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Incremental FNV-1a over byte strings, for digests built from several parts.
pub(crate) struct Digest(u64);

impl Digest {
    pub(crate) fn new() -> Self {
        Self(FNV_OFFSET)
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    pub(crate) fn update_hash(&mut self, hash: ContentHash) {
        self.update(&hash.0.to_be_bytes());
    }

    pub(crate) fn finish(self) -> ContentHash {
        ContentHash(self.0)
    }
}
