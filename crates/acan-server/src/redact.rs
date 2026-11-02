//! What each principal reads of the event log.
//!
//! A member reads every event as it is stored. An agent reads the same events with the member-only
//! fields set to `null`. Each event keeps its seq, time and actor, so a cursor over the log holds
//! for an agent too. Agents read the scope and the rulings through MCP, not through the raw log, so
//! this changes no path an agent uses.

use acan_core::{Event, Stamped};
use anyhow::anyhow;
use serde_json::Value;

use crate::auth::Principal;

/// The fields of an event that only a member reads. A scope draft is for the member who adopts it,
/// and a failed draft's reason is served only by the member-only draft route.
fn member_only(event: &Event) -> &'static [&'static str] {
    match event {
        Event::ScopeDrafted { .. } => &["draft"],
        Event::ScopeDraftFailed { .. } => &["reason"],
        _ => &[],
    }
}

/// The JSON of `stamped` as `principal` may read it. A member gets the event as stored. Any other
/// principal gets it with each member-only field set to `null`.
///
/// # Errors
///
/// Returns an error when the event does not serialize, or when a member-only field is not where
/// the event's encoding puts it. The view then fails, rather than send a field it did not redact.
pub(crate) fn view(principal: &Principal, stamped: &Stamped) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(stamped)?;
    if matches!(principal, Principal::Member(_)) {
        return Ok(value);
    }
    for field in member_only(&stamped.event) {
        let slot = value
            .get_mut("event")
            .and_then(|event| event.get_mut(*field))
            .ok_or_else(|| anyhow!("the event encoding has no field {field} to redact"))?;
        *slot = Value::Null;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use acan_core::{
        Actor, AgentId, CodeRev, Event, MemberId, OrderId, OrderRev, Seq, Stamped, Timestamp,
    };
    use serde_json::Value;

    use super::view;
    use crate::auth::Principal;

    fn agent() -> Principal {
        Principal::Agent(AgentId::from_str("agent-1").expect("valid agent id"))
    }

    fn member() -> Principal {
        Principal::Member(MemberId::from_str("owner").expect("valid member id"))
    }

    fn failed_draft() -> Stamped {
        Stamped {
            seq: Seq::new(7),
            at: Timestamp::from_millis(7),
            actor: Actor::System,
            event: Event::ScopeDraftFailed {
                order: OrderId::from_str("o-1").expect("valid order id"),
                rev: OrderRev::new(0),
                reason: "the scope draft failed on every attempt".to_owned(),
            },
        }
    }

    #[test]
    fn an_agent_reads_a_failed_draft_without_its_reason() {
        let stamped = failed_draft();
        let mut expected = serde_json::to_value(&stamped).expect("the event encodes");
        expected["event"]["reason"] = Value::Null;
        let seen = view(&agent(), &stamped).expect("the view");
        assert_eq!(seen, expected, "only the reason is redacted");
    }

    #[test]
    fn a_member_reads_a_failed_draft_as_stored() {
        let stamped = failed_draft();
        let seen = view(&member(), &stamped).expect("the view");
        assert_eq!(
            seen,
            serde_json::to_value(&stamped).expect("the event encodes")
        );
    }

    #[test]
    fn an_agent_reads_an_event_without_member_only_fields_as_stored() {
        let stamped = Stamped {
            seq: Seq::new(3),
            at: Timestamp::from_millis(3),
            actor: Actor::System,
            event: Event::IndexUpdated {
                rev: CodeRev::from_str("rev-test").expect("valid revision"),
            },
        };
        let seen = view(&agent(), &stamped).expect("the view");
        assert_eq!(
            seen,
            serde_json::to_value(&stamped).expect("the event encodes")
        );
    }
}
