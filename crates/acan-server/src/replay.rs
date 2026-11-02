//! Exports one recorded run as static files for the replay page.
//!
//! The export writes three files into a folder: `plant.json`, the plant of the checkout as the
//! page draws it; `events.json`, the stored log as an agent reads it; and `manifest.json`, what
//! the page needs to label itself. Events are read through the agent view, so scope drafts and
//! their failure reasons are null. The plant's root is the folder name, not the path. Before
//! anything is written, every file goes through the bug report's redaction. A file that still
//! holds a secret, a token or a private path fails the export, and nothing is written.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use acan_core::{AgentId, Seq, Stamped};
use acan_scan::Plant;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;

use crate::auth::Principal;
use crate::release::{self, Redaction};
use crate::{DB_FILE, EventStore, STATE_DIR, index, redact};

/// The format of the three files. The page reads it before anything else.
pub const FORMAT: u32 = 1;

/// The label the replay page shows, so a reader knows the page is not live.
pub const LABEL: &str = "Replay of a recorded run";

/// The agent the recording is read as. An agent sees the log without member-only fields, which
/// is the view a public page may show.
const READER: &str = "replay";

/// What one export wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exported {
    /// The folder the files went to.
    pub dir: PathBuf,
    /// How many events the recording holds.
    pub events: usize,
    /// The seq of the last event, or `None` when the log is empty.
    pub last_seq: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Manifest {
    format: u32,
    label: &'static str,
    version: &'static str,
    commit: &'static str,
    exported_at: u64,
    events: usize,
    last_seq: Option<u64>,
}

/// Exports the stored log and the plant of `repo` into the folder `out`, which is created when it
/// is missing and whose three files are replaced.
///
/// # Errors
///
/// Returns an error when `repo` has no recorded run, when the log or the scan cannot be read, or
/// when a file still holds text the redaction would remove. Nothing is written in that last case.
pub async fn export(repo: &Path, out: &Path, redaction: &Redaction) -> Result<Exported> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("opening the repository {}", repo.display()))?;
    let db = repo.join(STATE_DIR).join(DB_FILE);
    ensure!(
        db.is_file(),
        "no recorded run: {} does not exist",
        db.display()
    );
    let store = EventStore::open(&db).await?;
    let stamped = store.load_from(Seq::new(0)).await?;
    let scan_repo = repo.clone();
    let scan = tokio::task::spawn_blocking(move || index::scan(&scan_repo))
        .await
        .context("the scan task did not finish")??;
    write_recording(out, &repo, &scan.plant, &stamped, redaction)
}

/// The folder name of `repo`, which stands in for its path on the page.
fn folder_name(repo: &Path) -> String {
    repo.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_owned()
}

/// Replaces the repository's path, wherever a string carries it, with its folder name. The log
/// records the path when a world opens, and the plant records its root.
fn name_the_repo(value: &mut Value, repo: &str, folder: &str) {
    match value {
        Value::String(text) if text.contains(repo) => *text = text.replace(repo, folder),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| name_the_repo(item, repo, folder)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| name_the_repo(field, repo, folder)),
        _ => {}
    }
}

/// Builds the three files in memory, checks them, and writes them to `out`.
fn write_recording(
    out: &Path,
    repo: &Path,
    plant: &Plant,
    stamped: &[Stamped],
    redaction: &Redaction,
) -> Result<Exported> {
    let reader = Principal::Agent(AgentId::from_str(READER)?);
    let path = repo
        .to_str()
        .context("the repository path is not unicode")?;
    let folder = folder_name(repo);
    let mut events = Value::Array(
        stamped
            .iter()
            .map(|event| redact::view(&reader, event))
            .collect::<Result<Vec<Value>>>()?,
    );
    let mut plant = serde_json::to_value(plant)?;
    name_the_repo(&mut events, path, &folder);
    name_the_repo(&mut plant, path, &folder);

    let last_seq = stamped.last().map(|event| event.seq.get());
    let manifest = Manifest {
        format: FORMAT,
        label: LABEL,
        version: release::version(),
        commit: release::commit(),
        exported_at: unix_seconds(),
        events: stamped.len(),
        last_seq,
    };
    let files = [
        ("plant.json", serde_json::to_string(&plant)?),
        ("events.json", serde_json::to_string(&events)?),
        ("manifest.json", serde_json::to_string_pretty(&manifest)?),
    ];
    let leaks: Vec<String> = files
        .iter()
        .filter_map(|(name, text)| leak(redaction, name, text))
        .collect();
    ensure!(
        leaks.is_empty(),
        "the recording must not be published as it is: {}",
        leaks.join("; ")
    );

    std::fs::create_dir_all(out)
        .with_context(|| format!("creating the export folder {}", out.display()))?;
    for (name, text) in &files {
        std::fs::write(out.join(name), text).with_context(|| format!("writing {name}"))?;
    }
    Ok(Exported {
        dir: out.to_path_buf(),
        events: stamped.len(),
        last_seq,
    })
}

/// A message naming `name` when `redaction` would change its text. The text itself is not
/// repeated, so the message cannot leak what it found.
fn leak(redaction: &Redaction, name: &str, text: &str) -> Option<String> {
    (redaction.scrub(text) != text)
        .then(|| format!("{name} holds a key, a token or a private path"))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Redaction, leak, name_the_repo};

    #[test]
    fn the_repository_path_becomes_its_folder_name_wherever_it_appears() {
        let mut value = json!({
            "world": { "repo": "/work/app", "id": "app" },
            "paths": ["/work/app/src/lib.rs"],
            "count": 2,
        });
        name_the_repo(&mut value, "/work/app", "app");
        assert_eq!(
            value,
            json!({
                "world": { "repo": "app", "id": "app" },
                "paths": ["app/src/lib.rs"],
                "count": 2,
            })
        );
    }

    #[test]
    fn a_token_shaped_text_is_a_leak() {
        let text = r#"{"note":"acan_0123456789abcdef0123456789abcdef"}"#;
        assert_eq!(
            leak(&Redaction::EMPTY, "events.json", text),
            Some("events.json holds a key, a token or a private path".to_owned())
        );
    }

    #[test]
    fn a_private_home_path_is_a_leak() {
        let text = r#"{"path":"/home/someone/work/src/lib.rs"}"#;
        assert!(leak(&Redaction::EMPTY, "plant.json", text).is_some());
    }

    #[test]
    fn ordinary_text_is_not_a_leak() {
        let text = r#"{"name":"src/api","symbols":["handle_request"]}"#;
        assert_eq!(leak(&Redaction::EMPTY, "plant.json", text), None);
    }
}
