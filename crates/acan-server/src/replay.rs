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
    let root = repo
        .canonicalize()
        .with_context(|| format!("opening the repository {}", repo.display()))?;
    let db = root.join(STATE_DIR).join(DB_FILE);
    ensure!(
        db.is_file(),
        "no recorded run: {} does not exist",
        db.display()
    );
    let store = EventStore::open(&db).await?;
    let stamped = store.load_from(Seq::new(0)).await?;
    let scan_root = root.clone();
    let scan = tokio::task::spawn_blocking(move || index::scan(&scan_root))
        .await
        .context("the scan task did not finish")??;
    write_recording(out, repo, &root, &scan.plant, &stamped, redaction)
}

/// The folder name of `repo`, which stands in for its path on the page.
fn folder_name(repo: &Path) -> String {
    repo.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_owned()
}

/// The ways the repository's path may be spelled in the log and the plant, longest first. The
/// log may hold the path as it was given, and the plant the canonical path. On Windows the
/// canonical path carries the verbatim prefix `\\?\`, which the log may not, and either form may
/// use forward slashes. Each spelling is listed once.
fn repo_spellings(given: &Path, canonical: &Path) -> Vec<String> {
    let mut spellings = Vec::new();
    for path in [given, canonical] {
        let Some(text) = path.to_str() else {
            continue;
        };
        let plain = text
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| format!(r"\\{rest}"))
            .or_else(|| text.strip_prefix(r"\\?\").map(str::to_owned))
            .unwrap_or_else(|| text.to_owned());
        for spelling in [text.to_owned(), plain.replace('\\', "/"), plain] {
            if !spelling.is_empty() && !spellings.contains(&spelling) {
                spellings.push(spelling);
            }
        }
    }
    spellings.sort_by_key(|spelling| std::cmp::Reverse(spelling.len()));
    spellings
}

/// Replaces the repository's path, in any of `spellings`, wherever a string carries it, with its
/// folder name. The log records the path when a world opens, and the plant records its root.
fn name_the_repo(value: &mut Value, spellings: &[String], folder: &str) {
    match value {
        Value::String(text) => {
            for spelling in spellings {
                if text.contains(spelling.as_str()) {
                    *text = text.replace(spelling.as_str(), folder);
                }
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| name_the_repo(item, spellings, folder)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| name_the_repo(field, spellings, folder)),
        _ => {}
    }
}

/// Builds the three files in memory, checks them, and writes them to `out`.
fn write_recording(
    out: &Path,
    given: &Path,
    repo: &Path,
    plant: &Plant,
    stamped: &[Stamped],
    redaction: &Redaction,
) -> Result<Exported> {
    let reader = Principal::Agent(AgentId::from_str(READER)?);
    let spellings = repo_spellings(given, repo);
    let folder = folder_name(repo);
    let mut events = Value::Array(
        stamped
            .iter()
            .map(|event| redact::view(&reader, event))
            .collect::<Result<Vec<Value>>>()?,
    );
    let mut plant = serde_json::to_value(plant)?;
    name_the_repo(&mut events, &spellings, &folder);
    name_the_repo(&mut plant, &spellings, &folder);

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
    // The values are checked first, so the message can say where the text is. The text is checked
    // only when they show nothing, for a form the values do not hold.
    let located: Vec<String> = [("plant.json", &plant), ("events.json", &events)]
        .into_iter()
        .filter_map(|(name, value)| {
            find_leak(redaction, value, "")
                .map(|at| format!("{name} holds a key, a token or a private path, at {at}"))
        })
        .collect();
    let leaks = if located.is_empty() {
        files
            .iter()
            .filter_map(|(name, text)| leak(redaction, name, text))
            .collect()
    } else {
        located
    };
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

/// Where in `value` the redaction would change a string or an object key, as a path such as
/// `[3].payload.note`. `at` is the path of `value` itself. The text is not returned, and a key
/// that would change is shown as `<key>`, so the path cannot carry what it found.
fn find_leak(redaction: &Redaction, value: &Value, at: &str) -> Option<String> {
    match value {
        Value::String(text) => (redaction.scrub(text) != *text).then(|| at.to_owned()),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(index, item)| find_leak(redaction, item, &format!("{at}[{index}]"))),
        Value::Object(fields) => fields.iter().find_map(|(key, field)| {
            if redaction.scrub(key) != *key {
                return Some(if at.is_empty() {
                    "<key>".to_owned()
                } else {
                    format!("{at}.<key>")
                });
            }
            let next = if at.is_empty() {
                key.clone()
            } else {
                format!("{at}.{key}")
            };
            find_leak(redaction, field, &next)
        }),
        _ => None,
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use std::path::Path;

    use super::{Redaction, find_leak, leak, name_the_repo, repo_spellings};

    #[test]
    fn the_repository_path_becomes_its_folder_name_wherever_it_appears() {
        let mut value = json!({
            "world": { "repo": "/work/app", "id": "app" },
            "paths": ["/work/app/src/lib.rs"],
            "count": 2,
        });
        name_the_repo(&mut value, &["/work/app".to_owned()], "app");
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
    fn a_windows_repository_is_named_in_its_verbatim_and_plain_forms() {
        // The log may hold the path as it was typed, and the plant the canonical path, which
        // Windows writes with the verbatim prefix.
        let spellings = repo_spellings(
            Path::new(r"D:\dev\takes\take-1"),
            Path::new(r"\\?\D:\dev\takes\take-1"),
        );
        let mut value = json!({
            "log": [
                r"D:\dev\takes\take-1\model\store.ts",
                r"\\?\D:\dev\takes\take-1\model\store.ts",
                "D:/dev/takes/take-1/model/store.ts",
            ],
            "plant": { "root": r"\\?\D:\dev\takes\take-1" },
        });
        name_the_repo(&mut value, &spellings, "take-1");
        assert_eq!(
            value,
            json!({
                "log": [
                    r"take-1\model\store.ts",
                    r"take-1\model\store.ts",
                    "take-1/model/store.ts",
                ],
                "plant": { "root": "take-1" },
            })
        );
    }

    #[test]
    fn a_unc_repository_drops_its_verbatim_prefix_in_the_plain_form() {
        let spellings = repo_spellings(
            Path::new(r"\\server\share\take"),
            Path::new(r"\\?\UNC\server\share\take"),
        );
        assert!(spellings.contains(&r"\\server\share\take".to_owned()));
        assert!(spellings.contains(&"//server/share/take".to_owned()));
    }

    #[test]
    fn the_longest_spelling_comes_first_so_a_shorter_one_cannot_cut_it() {
        let spellings = repo_spellings(Path::new("/w/app"), Path::new("/w/app"));
        assert_eq!(spellings, vec!["/w/app".to_owned()], "listed once");
        let spellings = repo_spellings(Path::new(r"D:\a\b"), Path::new(r"\\?\D:\a\b"));
        let lengths: Vec<usize> = spellings.iter().map(String::len).collect();
        let mut sorted = lengths.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(lengths, sorted);
    }

    #[test]
    fn a_leak_is_located_by_its_path_and_the_text_is_not_repeated() {
        let value = json!({
            "events": [
                { "seq": 1 },
                { "payload": { "note": "acan_0123456789abcdef0123456789abcdef" } },
            ],
        });
        assert_eq!(
            find_leak(&Redaction::EMPTY, &value, ""),
            Some("events[1].payload.note".to_owned())
        );
    }

    #[test]
    fn a_leaking_key_is_located_without_being_repeated() {
        let value = json!({ "acan_0123456789abcdef0123456789abcdef": 1 });
        assert_eq!(
            find_leak(&Redaction::EMPTY, &value, ""),
            Some("<key>".to_owned())
        );
    }

    #[test]
    fn a_clean_value_has_no_leak_location() {
        let value = json!({ "name": "src/api", "symbols": ["handle_request"] });
        assert_eq!(find_leak(&Redaction::EMPTY, &value, ""), None);
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
