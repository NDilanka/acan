use std::str::FromStr;

use acan_core::{Actor, Event, Member, MemberId, Role, Seq, Stamped, Timestamp};
use acan_server::EventStore;

fn stamped(seq: u64) -> Stamped {
    Stamped {
        seq: Seq::new(seq),
        at: Timestamp::from_millis(1_000 + seq),
        actor: Actor::System,
        shareable: false,
        event: Event::MemberAdded {
            member: Member {
                id: MemberId::from_str(&format!("member-{seq}")).expect("valid member id"),
                role: Role::Member,
            },
        },
    }
}

#[tokio::test]
async fn appended_events_load_back_unchanged_in_seq_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = EventStore::open(&dir.path().join("world.db"))
        .await
        .expect("open store");
    let events: Vec<Stamped> = (0..3).map(stamped).collect();

    store.append(&events).await.expect("append");

    let loaded = store.load_from(Seq::new(0)).await.expect("load all");
    assert_eq!(loaded, events, "every appended event comes back unchanged");
    let tail = store.load_from(Seq::new(2)).await.expect("load tail");
    assert_eq!(
        tail,
        events[2..],
        "a resume from seq 2 returns exactly the tail"
    );
}

#[tokio::test]
async fn events_survive_reopening_the_database() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("world.db");
    let events: Vec<Stamped> = (0..2).map(stamped).collect();
    {
        let store = EventStore::open(&path).await.expect("open store");
        store.append(&events).await.expect("append");
    }

    let reopened = EventStore::open(&path).await.expect("reopen store");

    let loaded = reopened
        .load_from(Seq::new(0))
        .await
        .expect("load after restart");
    assert_eq!(loaded, events, "a restarted server sees the same log");
}

#[tokio::test]
async fn an_unknown_event_and_its_sharing_flag_survive_storage() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = EventStore::open(&dir.path().join("world.db"))
        .await
        .expect("open store");
    let entry = serde_json::json!({
        "seq": 0,
        "at": 1000,
        "actor": {"kind": "system"},
        "shareable": true,
        "event": {"type": "team_sync_checkpoint", "items": [17, null, {"key": "value"}]}
    });
    let event: Stamped = serde_json::from_value(entry.clone()).expect("the event decodes");
    store.append(&[event]).await.expect("append");
    let loaded = store.load_from(Seq::new(0)).await.expect("load the log");
    assert_eq!(
        serde_json::to_value(loaded).expect("encodes"),
        serde_json::json!([entry])
    );
}

#[tokio::test]
async fn a_batch_that_fails_midway_stores_none_of_its_events() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = EventStore::open(&dir.path().join("world.db"))
        .await
        .expect("open store");
    store
        .append(&[stamped(0), stamped(1)])
        .await
        .expect("first batch commits");

    // Seq 2 is stored, then seq 1 repeats a key already in the table, so the batch fails after
    // its first insert ran.
    let failing = [stamped(2), stamped(1)];
    assert!(
        store.append(&failing).await.is_err(),
        "a duplicate seq fails the batch"
    );

    let loaded = store
        .load_from(Seq::new(0))
        .await
        .expect("load after failure");
    assert_eq!(
        loaded,
        vec![stamped(0), stamped(1)],
        "only the earlier committed batch remains, with no partial second batch"
    );
}
