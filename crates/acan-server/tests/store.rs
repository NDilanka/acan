use std::str::FromStr;

use acan_core::{Actor, Event, Member, MemberId, Role, Seq, Stamped, Timestamp};
use acan_server::EventStore;

fn stamped(seq: u64) -> Stamped {
    Stamped {
        seq: Seq::new(seq),
        at: Timestamp::from_millis(1_000 + seq),
        actor: Actor::System,
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

async fn connection(path: &std::path::Path) -> sqlx::SqliteConnection {
    use sqlx::Connection;
    sqlx::SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(path))
        .await
        .expect("database connection")
}

#[tokio::test]
async fn tampering_with_a_stored_member_is_rejected_even_on_a_resumed_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.db");
    let store = EventStore::open(&path).await.unwrap();
    store.append(&[stamped(0), stamped(1)]).await.unwrap();
    let mut forged = stamped(0);
    let Event::MemberAdded { member } = &mut forged.event else {
        unreachable!()
    };
    member.role = Role::Owner;
    sqlx::query("UPDATE events SET json = ? WHERE seq = 0")
        .bind(serde_json::to_string(&forged).unwrap())
        .execute(&mut connection(&path).await)
        .await
        .unwrap();

    for from in [0, 1, 99] {
        let error = store.load_from(Seq::new(from)).await.unwrap_err();
        assert!(
            error.to_string().contains("integrity mismatch"),
            "{error:#}"
        );
    }
    assert!(EventStore::open(&path).await.is_err());
}

#[tokio::test]
async fn unsigned_insertion_deletion_and_mac_replacement_are_rejected() {
    for tamper in [
        "INSERT INTO events (seq, json) SELECT 2, json FROM events WHERE seq = 1",
        "DELETE FROM events WHERE seq = 0",
        "DELETE FROM events WHERE seq = 1",
        "UPDATE events SET mac = zeroblob(32) WHERE seq = 0",
        "UPDATE event_integrity SET next_seq = 0, mac = zeroblob(32)",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("world.db");
        let store = EventStore::open(&path).await.unwrap();
        store.append(&[stamped(0), stamped(1)]).await.unwrap();
        sqlx::query(tamper)
            .execute(&mut connection(&path).await)
            .await
            .unwrap();
        assert!(store.load_from(Seq::new(0)).await.is_err(), "{tamper}");
        assert!(EventStore::open(&path).await.is_err(), "{tamper}");
    }
}

#[tokio::test]
async fn a_legacy_unsigned_log_is_sealed_once_and_keeps_its_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.db");
    std::fs::File::create(&path).unwrap();
    let mut db = connection(&path).await;
    let migrator = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            sqlx::migrate!("./migrations")
                .iter()
                .filter(|migration| migration.version == 1)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    migrator.run(&mut db).await.unwrap();
    let json = serde_json::to_string_pretty(&stamped(0)).unwrap();
    sqlx::query("INSERT INTO events (seq, json) VALUES (0, ?)")
        .bind(&json)
        .execute(&mut db)
        .await
        .unwrap();
    let store = EventStore::open(&path).await.unwrap();
    assert_eq!(store.load_from(Seq::new(0)).await.unwrap(), [stamped(0)]);
    let stored: String = sqlx::query_scalar("SELECT json FROM events WHERE seq = 0")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(stored, json, "migration does not rewrite legacy event JSON");
    // JSON whitespace and object key order are not part of the authenticated value.
    sqlx::query("UPDATE events SET json = ? WHERE seq = 0")
        .bind(
            serde_json::to_string(&serde_json::from_str::<serde_json::Value>(&json).unwrap())
                .unwrap(),
        )
        .execute(&mut db)
        .await
        .unwrap();
    store.append(&[stamped(1)]).await.unwrap();
    assert_eq!(
        store.load_from(Seq::new(0)).await.unwrap(),
        [stamped(0), stamped(1)]
    );
    sqlx::query("UPDATE events SET mac = NULL")
        .execute(&mut db)
        .await
        .unwrap();
    assert!(
        EventStore::open(&path).await.is_err(),
        "never reseal unsigned rows"
    );
}

#[tokio::test]
async fn missing_or_replaced_keys_never_reauthenticate_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.db");
    let key = path.with_extension("key");
    let store = EventStore::open(&path).await.unwrap();
    store.append(&[stamped(0)]).await.unwrap();
    std::fs::remove_file(&key).unwrap();
    assert!(EventStore::open(&path).await.is_err());
    assert!(
        !key.exists(),
        "a missing key is never regenerated for a sealed log"
    );
    std::fs::write(&key, [7; 32]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(EventStore::open(&path).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn the_key_is_private_and_symlinked_or_public_keys_are_refused() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.db");
    let key = path.with_extension("key");
    let store = EventStore::open(&path).await.unwrap();
    store.append(&[stamped(0)]).await.unwrap();
    assert_eq!(
        std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let saved = dir.path().join("saved");
    std::fs::rename(&key, &saved).unwrap();
    symlink(&saved, &key).unwrap();
    assert!(EventStore::open(&path).await.is_err());
    std::fs::remove_file(&key).unwrap();
    std::fs::rename(&saved, &key).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(EventStore::open(&path).await.is_err());
}

#[tokio::test]
async fn independent_store_handles_append_to_the_same_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.db");
    let first = EventStore::open(&path).await.unwrap();
    let second = EventStore::open(&path).await.unwrap();
    first.append(&[stamped(0)]).await.unwrap();
    second.append(&[stamped(1)]).await.unwrap();
    assert!(first.append(&[stamped(1)]).await.is_err());
    first.append(&[stamped(2)]).await.unwrap();
    assert_eq!(
        second.load_from(Seq::new(0)).await.unwrap(),
        (0..3).map(stamped).collect::<Vec<_>>()
    );
}
