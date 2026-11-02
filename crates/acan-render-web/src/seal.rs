//! The seal check: whether the origin's confirmation route names the scope the page holds.
//!
//! `GET /api/orders/{order}/scope/confirmation` answers the body a member would send for an
//! order's current revision, whether or not a human has confirmed it. The page shows a Seal only
//! when that body names the same order, revision and hash as the page's own scope. The page's
//! hash is taken here, with the same code the origin hashes with, so a page whose hashing differs
//! from the origin's says so and shows no Seal.

use acan_intent::{ScopeConfirmation, ScopeHash, ScopeRev};
use serde::Serialize;

use crate::lanes::LaneView;

/// The page's scope set against the origin's confirmation body for the same order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SealCheck {
    /// `match` when the body names the page's order, revision and hash, else `mismatch`.
    pub verdict: &'static str,
    /// The revision the page holds.
    pub page_rev: ScopeRev,
    /// The revision the body names.
    pub origin_rev: ScopeRev,
    /// The hash the page computes for its scope, or `None` when the scope cannot be hashed.
    pub page_hash: Option<ScopeHash>,
    /// The hash the body names.
    pub origin_hash: ScopeHash,
}

/// Checks `route`, the origin's confirmation body for `lane`'s order, against the lane's scope.
#[must_use]
pub fn check(lane: &LaneView, route: &ScopeConfirmation) -> SealCheck {
    let page_hash = lane.scope.scope_hash().ok();
    let same = page_hash == Some(route.scope_hash)
        && lane.rev == route.scope_rev
        && lane.scope.order == route.order;
    SealCheck {
        verdict: if same { "match" } else { "mismatch" },
        page_rev: lane.rev,
        origin_rev: route.scope_rev,
        page_hash,
        origin_hash: route.scope_hash,
    }
}
