//! What each principal reads of the event log.
//!
//! A member reads every event as it is stored. For an agent, an unrecognized event keeps its tag
//! and log position but not its payload: the host cannot tell which fields are safe to expose.

use acan_core::{Event, Stamped};
use serde_json::{Value, json};

use crate::auth::Principal;

/// The JSON of `stamped` as `principal` may read it.
///
/// # Errors
///
/// Returns an error when the event does not serialize.
pub(crate) fn view(principal: &Principal, stamped: &Stamped) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(stamped)?;
    if let (Principal::Agent(_), Event::Unknown { event_type, .. }) = (principal, &stamped.event) {
        value["event"] = json!({"type": event_type});
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use acan_core::{Actor, AgentId, CodeRev, Event, MemberId, Seq, Stamped, Timestamp};
    use serde_json::json;

    use super::view;
    use crate::auth::Principal;

    fn agent() -> Principal {
        Principal::Agent(AgentId::from_str("agent-1").expect("valid agent id"))
    }

    fn member() -> Principal {
        Principal::Member(MemberId::from_str("owner").expect("valid member id"))
    }

    fn unknown() -> Stamped {
        serde_json::from_value(json!({
            "seq": 7, "at": 7, "actor": {"kind": "system"}, "shareable": false,
            "event": {"type": "team_sync_checkpoint", "note": "the checkpoint payload"}
        }))
        .expect("an unrecognized event decodes")
    }

    #[test]
    fn an_agent_reads_an_unrecognized_event_without_its_payload() {
        let stamped = unknown();
        let mut expected = serde_json::to_value(&stamped).expect("the event encodes");
        expected["event"] = json!({"type": "team_sync_checkpoint"});
        let seen = view(&agent(), &stamped).expect("the view");
        assert_eq!(seen, expected);
    }

    #[test]
    fn a_member_reads_an_unrecognized_event_as_stored() {
        let stamped = unknown();
        let seen = view(&member(), &stamped).expect("the view");
        assert_eq!(
            seen,
            serde_json::to_value(&stamped).expect("the event encodes")
        );
    }

    #[test]
    fn an_agent_reads_a_known_event_as_stored() {
        let stamped = Stamped {
            seq: Seq::new(3),
            at: Timestamp::from_millis(3),
            actor: Actor::System,
            shareable: false,
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
