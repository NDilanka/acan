//! The replay export: the three files of a recorded run, read as an agent reads them, and the
//! refusal of a repository with no recording.

use std::fs;
use std::path::Path;
use std::str::FromStr;

use acan_core::{Actor, CodeRev, Event, OrderId, OrderRev, Seq, Stamped, Timestamp};
use acan_server::EventStore;
use acan_server::release::Redaction;
use acan_server::replay::{self, FORMAT, LABEL};
use serde_json::Value;

fn temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(dir.path().join("src")).expect("the source folder");
    fs::write(dir.path().join("src/lib.rs"), "pub fn handle() {}\n").expect("a source file");
    dir
}

fn stamped(seq: u64, event: Event) -> Stamped {
    Stamped {
        seq: Seq::new(seq),
        at: Timestamp::from_millis(seq * 10),
        actor: Actor::System,
        event,
    }
}

async fn record(repo: &Path, events: &[Stamped]) {
    let state = repo.join(".acan");
    fs::create_dir_all(&state).expect("the state folder");
    let store = EventStore::open(&state.join("world.db"))
        .await
        .expect("the event store opens");
    store.append(events).await.expect("the events are stored");
}

fn read_json(dir: &Path, name: &str) -> Value {
    let text = fs::read_to_string(dir.join(name)).expect("the exported file");
    serde_json::from_str(&text).expect("the exported file is JSON")
}

#[tokio::test]
async fn the_export_writes_three_files_read_as_an_agent_reads_them() {
    let repo = temp_repo();
    let order = OrderId::from_str("o-1").expect("a valid order id");
    record(
        repo.path(),
        &[
            stamped(
                0,
                Event::IndexUpdated {
                    rev: CodeRev::from_str("rev-1").expect("a valid revision"),
                },
            ),
            stamped(
                1,
                Event::ScopeDraftFailed {
                    order,
                    rev: OrderRev::new(0),
                    reason: "the draft failed".to_owned(),
                },
            ),
        ],
    )
    .await;

    let out = tempfile::tempdir().expect("an export folder");
    let dir = out.path().join("replay");
    let redaction = Redaction::for_repo(repo.path());
    let exported = replay::export(repo.path(), &dir, &redaction)
        .await
        .expect("the export succeeds");

    assert_eq!(exported.events, 2);
    assert_eq!(exported.last_seq, Some(1));

    let manifest = read_json(&dir, "manifest.json");
    assert_eq!(manifest["format"], FORMAT);
    assert_eq!(manifest["label"], LABEL);
    assert_eq!(manifest["events"], 2);
    assert_eq!(manifest["last_seq"], 1);

    let events = read_json(&dir, "events.json");
    let events = events.as_array().expect("the events are an array");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["seq"], 0);
    assert_eq!(
        events[1]["event"]["reason"],
        Value::Null,
        "a failed draft's reason is member-only, so the replay does not show it"
    );

    let plant = read_json(&dir, "plant.json");
    let folder = repo
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a unicode folder name");
    assert_eq!(
        plant["root"], folder,
        "the plant names the folder, not the path"
    );
    let all = fs::read_to_string(dir.join("plant.json")).expect("the plant")
        + &fs::read_to_string(dir.join("events.json")).expect("the events");
    assert!(
        !all.contains(repo.path().to_str().expect("a unicode temp path")),
        "the repository path never reaches the page"
    );
}

#[tokio::test]
async fn a_repository_without_a_recording_is_refused_and_nothing_is_written() {
    let repo = temp_repo();
    let out = tempfile::tempdir().expect("an export folder");
    let dir = out.path().join("replay");
    let err = replay::export(repo.path(), &dir, &Redaction::EMPTY)
        .await
        .expect_err("there is no recorded run to export");
    assert!(
        err.to_string().contains("no recorded run"),
        "the refusal names the cause: {err}"
    );
    assert!(!dir.exists(), "no folder is created for a refused export");
}
