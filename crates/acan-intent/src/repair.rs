//! Repair rounds: the order-wide repair budget and idempotent round results.
//!
//! Rules, binding on the conductor (C01) and the store (I01) that implement them:
//! - An order has at most [`crate::RepairBudget::repair_rounds`] rounds, eight in v1. The count is
//!   per order; a new episode, a new commit, a new worker or a retry never adds a round.
//! - A round bounces one batch of the episodes open at that moment, at most
//!   [`crate::RepairBudget::max_episodes`] of them, and is answered by one candidate and one
//!   full-corpus run. Opening a round is the only increment.
//! - An episode's `bounce_count` counts the rounds that included it, so it never exceeds the
//!   order's rounds.
//! - A round's identity is the [`RoundId`] of its [`RoundKey`], and a result's the [`ResultId`]
//!   of its [`ResultKey`]. Both are durable across restarts. Recording a result whose id the
//!   round already holds is a no-op; recording a different result for a round that has one is
//!   refused. A worker retry that reports the same merged commit and receipt is the same result.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::hash::{CanonicalError, ContentHash, EpisodeId, ReceiptHash, ResultId, RoundId};
use crate::types::{CommitSha, NonEmpty, OrderRef, UnixMillis, bounded};
use crate::version::RepairRoundV1;

bounded!(
    /// A round of one order, from 1. Never more than the v1 cap of eight.
    RoundNumber(u32), 1 ..= 8, "repair round number"
);

/// What identifies a round: the order and its number. Never a commit, worker, probe or run.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RoundKey {
    /// The order.
    pub order: OrderRef,
    /// The round.
    pub round: RoundNumber,
}

impl RoundKey {
    /// The round's identity.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn round_id(&self) -> Result<RoundId, CanonicalError> {
        ContentHash::of(self).map(RoundId::from)
    }
}

/// What identifies a round's result: the round, the candidate and the receipt of its
/// full-corpus run. The worker and the attempt are deliberately left out.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ResultKey {
    /// The round.
    pub round_id: RoundId,
    /// The prepared merged commit that answered it.
    pub merged_commit: CommitSha,
    /// The receipt of its full-corpus run.
    pub receipt_hash: ReceiptHash,
}

impl ResultKey {
    /// The result's identity.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn result_id(&self) -> Result<ResultId, CanonicalError> {
        ContentHash::of(self).map(ResultId::from)
    }
}

/// The recorded answer to one round.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoundResult {
    /// The hash of `key`.
    pub result_id: ResultId,
    /// What identifies it.
    pub key: ResultKey,
    /// When it was first recorded. A duplicate never moves it.
    pub recorded_at: UnixMillis,
}

/// One batched repair round of one order.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepairRound {
    /// Always `acan-repair-round/1`.
    pub version: RepairRoundV1,
    /// The hash of `key`.
    pub round_id: RoundId,
    /// What identifies it.
    pub key: RoundKey,
    /// Every episode bounced in this round.
    pub episodes: NonEmpty<EpisodeId>,
    /// When it was opened.
    pub opened_at: UnixMillis,
    /// Its result, once one is recorded. At most one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RoundResult>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn round() -> RepairRound {
        serde_json::from_str(include_str!("../fixtures/repair-round.json")).expect("the fixture")
    }

    #[test]
    fn a_ninth_round_does_not_decode() {
        let key = |round: u32| {
            serde_json::from_value::<RoundKey>(json!({ "order": "o-3", "round": round }))
        };
        assert_eq!(RoundNumber::MAX, 8);
        assert!(key(0).is_err());
        assert!(key(1).is_ok());
        assert!(key(4).is_ok());
        assert!(key(8).is_ok());
        assert!(key(9).is_err());
    }

    #[test]
    fn the_fixture_ids_are_the_hashes_of_their_keys() {
        let round = round();
        assert_eq!(round.key.round_id().expect("hashing"), round.round_id);
        let result = round.result.expect("a result");
        assert_eq!(result.key.round_id, round.round_id);
        assert_eq!(result.key.result_id().expect("hashing"), result.result_id);
    }

    #[test]
    fn a_retried_report_is_the_same_result_and_a_new_candidate_is_not() {
        let result = round().result.expect("a result").key;
        let retried = result.clone();
        assert_eq!(
            retried.result_id().expect("hashing"),
            result.result_id().expect("hashing")
        );
        let other = ResultKey {
            merged_commit: "1111111111111111111111111111111111111111"
                .parse()
                .expect("a sha"),
            ..result.clone()
        };
        assert_ne!(
            other.result_id().expect("hashing"),
            result.result_id().expect("hashing")
        );
    }

    #[test]
    fn rounds_are_per_order() {
        let key = |order: &str, round: u32| RoundKey {
            order: order.parse().expect("an order"),
            round: RoundNumber::try_from(round).expect("a round"),
        };
        let first = key("o-3", 1).round_id().expect("hashing");
        assert_eq!(first, key("o-3", 1).round_id().expect("hashing"));
        assert_ne!(first, key("o-3", 2).round_id().expect("hashing"));
        assert_ne!(first, key("o-4", 1).round_id().expect("hashing"));
    }
}
