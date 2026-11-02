//! The plant view the page reads for one order, decoded from the `plant-view/1` contract.
//!
//! The origin builds each view from one receipt and serves it, so the page only decodes. [`decode`]
//! refuses what the contract refuses: another version, unknown fields, and counts that do not add up
//! to the view's own items. Nothing here draws or decides. The page places the marks.

use acan_intent::PlantView;
use serde_json::{Value, json};

/// Why a plant view did not decode.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The text is not a `plant-view/1` view.
    #[error("not a plant-view/1 view: {0}")]
    Malformed(#[from] serde_json::Error),
    /// The counts the view states do not match its own items.
    #[error("the view's counts do not match its items: {0}")]
    Counts(String),
}

/// Decodes one view and checks the counts it states against its items.
///
/// # Errors
///
/// Returns [`DecodeError::Malformed`] for text that is not a `plant-view/1` view, and
/// [`DecodeError::Counts`] when the counts disagree with the items.
pub fn decode(json: &str) -> Result<PlantView, DecodeError> {
    let view: PlantView = serde_json::from_str(json)?;
    view.check_counts()
        .map_err(|err| DecodeError::Counts(err.to_string()))?;
    Ok(view)
}

/// What the page's first check shows about a view: the identifiers the receipt states and the
/// sizes the page lays out. Every value is copied from the view.
#[must_use]
pub fn summarize(view: &PlantView) -> Value {
    json!({
        "version": view.version,
        "receipt_hash": view.receipt_hash,
        "base_sha": view.base_sha,
        "merged_sha": view.merged_sha,
        "gate_state": view.gate_state,
        "order": view.order.id,
        "scope_rev": view.order.scope_rev,
        "machines": view.machines.len(),
        "items": view.items.len(),
        "mandatory_items": view.mandatory_items().count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIRECTED: &str = include_str!("../../acan-intent/fixtures/plant-view-v1.json");
    const PLAIN: &str = include_str!("../../acan-intent/fixtures/plant-view-v1-plain.json");

    #[test]
    fn the_directed_fixture_decodes_and_its_counts_hold() {
        let view = decode(DIRECTED).expect("the fixture decodes");
        assert_eq!(view.order.id.to_string(), "o-3");
        assert!(view.tour.is_some(), "the directed fixture carries a tour");
    }

    #[test]
    fn the_plain_fixture_decodes_without_a_tour() {
        let view = decode(PLAIN).expect("the plain fixture decodes");
        assert!(view.tour.is_none());
    }

    #[test]
    fn the_summary_copies_the_fixture_numbers() {
        let view = decode(DIRECTED).expect("the fixture decodes");
        let summary = summarize(&view);
        assert_eq!(summary["version"], "plant-view/1");
        assert_eq!(summary["order"], "o-3");
        assert_eq!(summary["gate_state"], "blocked");
        assert_eq!(summary["machines"], view.machines.len());
        assert_eq!(summary["items"], view.items.len());
    }

    #[test]
    fn another_version_is_refused_by_name() {
        let other = DIRECTED.replace("\"plant-view/1\"", "\"plant-view/2\"");
        let err = decode(&other).expect_err("plant-view/2 decoded");
        assert!(err.to_string().contains("plant-view/2"), "{err}");
    }

    #[test]
    fn text_that_is_not_a_view_is_refused() {
        assert!(matches!(decode("{}"), Err(DecodeError::Malformed(_))));
    }
}
