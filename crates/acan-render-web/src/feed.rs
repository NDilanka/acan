//! Reads the origin's log and stream one event at a time.
//!
//! A page built before an event type existed cannot read that event. Each event is parsed on its
//! own, so the page passes over the events it cannot read, counts them, and keeps folding the rest.

use acan_core::{Seq, Stamped};
use serde_json::Value;

/// One event of the log or the stream: one this build reads, one it does not, or one the origin
/// withheld from this reader.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// An event this build reads.
    Known(Stamped),
    /// An event this build cannot read, by its position. The page passes over it and counts it.
    Unknown(Seq),
    /// An event whose member-only field the origin set to null for this reader, by its position.
    /// The page passes over it without counting it: the origin wrote it, so the page is not older.
    Withheld(Seq),
}

/// The member-only fields the origin nulls for an agent reader, by event type. A replay is read as
/// an agent, so these come back null. This is the list the origin's redaction keeps.
const WITHHELD_FIELDS: &[(&str, &str)] =
    &[("scope_drafted", "draft"), ("scope_draft_failed", "reason")];

/// Reads a JSON array of stamped events, one entry per event.
///
/// # Errors
///
/// Returns the JSON error when the text is not an array, or when an element has no readable seq.
pub fn entries(json: &str) -> Result<Vec<Entry>, serde_json::Error> {
    let values: Vec<Value> = serde_json::from_str(json)?;
    values.into_iter().map(entry_of).collect()
}

/// Reads one streamed event.
///
/// # Errors
///
/// Returns the JSON error when the text is not an event, or when it has no readable seq.
pub fn entry(json: &str) -> Result<Entry, serde_json::Error> {
    entry_of(serde_json::from_str(json)?)
}

fn entry_of(value: Value) -> Result<Entry, serde_json::Error> {
    match serde_json::from_value::<Stamped>(value.clone()) {
        Ok(stamped) => Ok(Entry::Known(stamped)),
        Err(err) => {
            // The position must be readable, or the page could not pass over the event.
            let Ok(seq) = serde_json::from_value::<Seq>(value["seq"].clone()) else {
                return Err(err);
            };
            Ok(if is_withheld(&value) {
                Entry::Withheld(seq)
            } else {
                Entry::Unknown(seq)
            })
        }
    }
}

/// Whether the event is one of the origin's withheld events: a known type whose member-only field
/// is null, as the origin writes it for an agent reader.
fn is_withheld(value: &Value) -> bool {
    let event = &value["event"];
    let Some(kind) = event["type"].as_str() else {
        return false;
    };
    WITHHELD_FIELDS
        .iter()
        .any(|(name, field)| kind == *name && event[*field].is_null())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stamped_json(seq: u64, event: Value) -> Value {
        json!({
            "seq": seq,
            "at": 1_000,
            "actor": { "kind": "system" },
            "event": event,
        })
    }

    #[test]
    fn an_array_splits_into_known_and_unknown_entries() {
        let opened = stamped_json(
            0,
            json!({ "type": "world_opened", "world": { "id": "demo", "repo": "demo" }, "founder": "owner" }),
        );
        let future = stamped_json(1, json!({ "type": "vision_checked", "task": "t-1" }));
        let text = Value::Array(vec![opened, future]).to_string();

        let parsed = entries(&text).expect("the array parses");
        let [Entry::Known(first), Entry::Unknown(second)] = parsed.as_slice() else {
            panic!("the first event is known and the second is not: {parsed:?}");
        };
        assert_eq!(first.seq, Seq::new(0));
        assert_eq!(*second, Seq::new(1));
    }

    #[test]
    fn a_single_unreadable_event_keeps_its_position() {
        let text = stamped_json(7, json!({ "type": "vision_checked" })).to_string();
        assert_eq!(
            entry(&text).expect("the event parses"),
            Entry::Unknown(Seq::new(7))
        );
    }

    #[test]
    fn a_scope_draft_the_origin_withheld_is_passed_over_without_a_count() {
        let text = stamped_json(
            5,
            json!({ "type": "scope_drafted", "order": 0, "rev": 0, "draft": null }),
        )
        .to_string();
        assert_eq!(
            entry(&text).expect("the event parses"),
            Entry::Withheld(Seq::new(5))
        );
    }

    #[test]
    fn a_failed_draft_whose_reason_is_withheld_is_passed_over_without_a_count() {
        let text = stamped_json(
            9,
            json!({ "type": "scope_draft_failed", "order": 0, "rev": 0, "reason": null }),
        )
        .to_string();
        assert_eq!(
            entry(&text).expect("the event parses"),
            Entry::Withheld(Seq::new(9))
        );
    }

    #[test]
    fn a_null_field_that_is_not_withheld_stays_unreadable() {
        let text = stamped_json(
            4,
            json!({ "type": "scope_drafted", "order": 0, "rev": 0, "draft": { "may": "x" } }),
        )
        .to_string();
        assert_eq!(
            entry(&text).expect("the event parses"),
            Entry::Unknown(Seq::new(4))
        );
        let other = stamped_json(6, json!({ "type": "vision_checked", "draft": null })).to_string();
        assert_eq!(
            entry(&other).expect("the event parses"),
            Entry::Unknown(Seq::new(6))
        );
    }

    #[test]
    fn an_event_without_a_readable_seq_is_an_error() {
        let text = json!({ "event": { "type": "vision_checked" } }).to_string();
        assert!(entry(&text).is_err());
        assert!(entries(&Value::Array(vec![json!({ "seq": "two" })]).to_string()).is_err());
    }

    #[test]
    fn text_that_is_not_an_array_is_an_error() {
        assert!(entries("{}").is_err());
    }
}
