//! The data routes of one project. `GET .../settings/data/export` gives the owner a ZIP archive of
//! the project's receipts, its recorded run, or both. `POST .../settings/data/clear` removes the
//! project's local derived data: the installed-dependency cache and the in-memory tour cache. Only
//! the owner may use either route.
//!
//! The archive is written without compression. Its text passes the bug report's redaction, and no
//! entry may hold the text of a value in the repository's `.env` file: such a value fails the
//! request, and nothing is sent. The clear keeps the event log, the receipts, the saved examples
//! and observations, the vision folder, the tokens, and the repository itself.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use acan_core::{MemberId, TaskId};
use acan_intent::{GetReceiptRequest, McpErrorCode, McpV1};
use acan_llm::ModelClient;
use anyhow::{Context, anyhow, bail, ensure};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::Principal;
use crate::host::Host;
use crate::http::{ApiError, authenticate, bearer, task_ref};
use crate::intent_api::{self, IntentError};
use crate::jobs::DEPS_DIR;
use crate::release::{self, Redaction};
use crate::replay;
use crate::{DB_FILE, STATE_DIR};

/// Why anyone but the owner may not export the data.
const ONLY_OWNER_EXPORTS: &str = "only the owner may export data";

/// Why anyone but the owner may not clear the local data.
const ONLY_OWNER_CLEARS: &str = "only the owner may clear data";

/// What `what` must name, as the `400` says.
const WHAT_NAMES: &str = "what must be receipts, replay or all";

/// The version of the archive's manifest format.
const FORMAT: u32 = 1;

/// The fewest characters of a `.env` value that count as a secret. A shorter value is too common
/// to replace, so the bug report leaves it alone too.
const SECRET_MIN_LEN: usize = 8;

/// The three files of a recorded run, as `acan export-replay` writes them.
const REPLAY_FILES: [&str; 3] = ["plant.json", "events.json", "manifest.json"];

/// What a clear removes, as its response names it.
const CLEARED: [&str; 2] = ["deps", "tour_cache"];

/// What a clear keeps, as its response names it. A clear never removes any of these.
const KEPT: [&str; 7] = [
    "receipts",
    "event_log",
    "repo",
    "tokens",
    "examples",
    "observations",
    "vision",
];

/// The owner of one project's data: the member whose token may export or clear it, and the
/// project's id, which names the archive.
#[derive(Debug, Clone)]
pub(crate) struct DataOwner {
    /// The project's id.
    pub(crate) project: String,
    /// The member the owner's token belongs to.
    pub(crate) founder: MemberId,
}

/// What an export holds, as its `what` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Receipts,
    Replay,
    All,
}

impl What {
    /// The kind `raw` names, or `None` when it names none, or is missing.
    fn parse(raw: Option<&str>) -> Option<Self> {
        match raw? {
            "receipts" => Some(Self::Receipts),
            "replay" => Some(Self::Replay),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// The name the manifest records.
    fn name(self) -> &'static str {
        match self {
            Self::Receipts => "receipts",
            Self::Replay => "replay",
            Self::All => "all",
        }
    }

    /// Whether the archive holds the receipts.
    fn receipts(self) -> bool {
        self != Self::Replay
    }

    /// Whether the archive holds the recorded run.
    fn replay(self) -> bool {
        self != Self::Receipts
    }
}

/// The query of `GET .../settings/data/export`. A missing `what` reads as `None`.
#[derive(Debug, Deserialize)]
pub(crate) struct ExportQuery {
    what: Option<String>,
}

/// The manifest, the first entry of every archive. Its field names are the format a reader reads.
#[derive(Debug, Serialize)]
struct Manifest<'a> {
    format: u32,
    what: &'static str,
    project: &'a str,
    version: &'static str,
    commit: &'static str,
    exported_at: u64,
    receipts: usize,
    replay: bool,
}

/// `GET .../settings/data/export?what=receipts|replay|all`. Only the owner may export. The archive
/// holds the manifest, then one `receipts/<task>.json` for each task with a receipt, and then the
/// three `replay/` files of the recorded run.
///
/// Status codes: `200` with the archive. `400` when `what` is missing or names no kind. `401` for
/// an unknown token. `403` for any token but the owner's. `409` with `no_recorded_run` when the
/// export holds the recorded run and the project has none. `500` when an entry still holds a
/// configured secret, or the archive cannot be written. Nothing is sent on `500`.
pub(crate) async fn get_export<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    Extension(owner): Extension<DataOwner>,
    headers: HeaderMap,
    Query(query): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    require_owner(&host, &headers, &owner.founder, ONLY_OWNER_EXPORTS)?;
    let what = What::parse(query.what.as_deref()).ok_or(ApiError::BadRequest(WHAT_NAMES))?;
    if what.replay() && !recorded(host.repo()) {
        return Err(ApiError::Conflict("no_recorded_run"));
    }
    let (redaction, secrets) = redaction_of(host.repo())?;
    let receipts = if what.receipts() {
        receipt_entries(&host, &redaction)?
    } else {
        Vec::new()
    };
    let replay = if what.replay() {
        replay_entries(&host, &redaction).await?
    } else {
        Vec::new()
    };

    let exported_at = unix_seconds();
    let manifest = Manifest {
        format: FORMAT,
        what: what.name(),
        project: &owner.project,
        version: release::version(),
        commit: release::commit(),
        exported_at,
        receipts: receipts.len(),
        replay: what.replay(),
    };
    let mut entries = vec![(
        "manifest.json".to_owned(),
        serde_json::to_vec_pretty(&manifest).context("the manifest does not serialize")?,
    )];
    entries.extend(receipts);
    entries.extend(replay);
    ensure_no_secret(&entries, &secrets)?;

    let archive = store_zip(&entries)?;
    let disposition = format!(
        "attachment; filename=\"acan-export-{}-{exported_at}.zip\"",
        owner.project
    );
    Ok((
        [
            (CONTENT_TYPE, "application/zip"),
            (CONTENT_DISPOSITION, disposition.as_str()),
        ],
        archive,
    )
        .into_response())
}

/// The redaction an export's text passes: the bug report's for this repository, with the `.env`
/// values added, so each one is removed wherever it appears. Returns the values too, so the
/// export can check that none is left.
fn redaction_of(repo: &Path) -> anyhow::Result<(Redaction, Vec<String>)> {
    let secrets = env_secrets(repo)?;
    let mut redaction = Redaction::for_repo(repo);
    for secret in &secrets {
        redaction.add_secret(secret);
    }
    Ok((redaction, secrets))
}

/// The values of the repository's `.env` file that are long enough to be secrets. The file is read
/// and not loaded, so the process environment does not change. A missing file holds none. A file
/// that cannot be read or does not parse is an error, so the export never guesses what it holds.
/// The error names no part of the file, since a parse error may quote a line.
fn env_secrets(repo: &Path) -> anyhow::Result<Vec<String>> {
    let entries = match dotenvy::from_path_iter(repo.join(".env")) {
        Ok(entries) => entries,
        Err(dotenvy::Error::Io(err)) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(_) => bail!(".env cannot be read"),
    };
    let mut secrets = Vec::new();
    for entry in entries {
        let (_, value) = entry.map_err(|_| anyhow::anyhow!(".env does not parse"))?;
        if value.len() >= SECRET_MIN_LEN {
            secrets.push(value);
        }
    }
    Ok(secrets)
}

/// Whether the repository has a recorded run: its event database file.
fn recorded(repo: &Path) -> bool {
    repo.join(STATE_DIR).join(DB_FILE).is_file()
}

/// Refuses anyone but the owner with `why`. An unknown token is refused as unauthorized, since
/// `authenticate` answers it first.
fn require_owner<C: ModelClient>(
    host: &Host<C>,
    headers: &HeaderMap,
    founder: &MemberId,
    why: &'static str,
) -> Result<(), ApiError> {
    match authenticate(host, bearer(headers))? {
        Principal::Member(member) if &member == founder => Ok(()),
        Principal::Member(_) | Principal::Agent(_) => Err(ApiError::Forbidden(why)),
    }
}

/// The receipt of every task that has one, as `GET .../tasks/{task}/receipt` answers it, with its
/// text scrubbed. Tasks are read from the world, in id order. A task with no receipt has no entry.
fn receipt_entries<C: ModelClient>(
    host: &Host<C>,
    redaction: &Redaction,
) -> Result<Vec<(String, Vec<u8>)>, ApiError> {
    let mut tasks: Vec<TaskId> = host.read(|world| {
        world
            .orders()
            .flat_map(|order| world.tasks_of(order.id).map(|task| task.id))
            .collect()
    });
    tasks.sort();
    let mut entries = Vec::new();
    for task in tasks {
        let request = GetReceiptRequest {
            version: McpV1,
            task: task_ref(&task.to_string())?,
        };
        let response = match intent_api::get_receipt(host, &request) {
            Ok(response) => response,
            Err(IntentError::Refused(err)) if is_no_receipt(&err.error) => continue,
            Err(err) => return Err(err.into()),
        };
        let mut body = serde_json::to_value(&response).context("the receipt does not serialize")?;
        scrub(redaction, &mut body);
        let bytes = serde_json::to_vec(&body).context("the receipt does not serialize")?;
        entries.push((format!("receipts/{task}.json"), bytes));
    }
    Ok(entries)
}

/// Whether a refusal is the one a task with no receipt gets.
fn is_no_receipt(code: &McpErrorCode) -> bool {
    matches!(code, McpErrorCode::NotFound { what } if what == "receipt")
}

/// Scrubs every string and every key of `value`. Each piece is scrubbed on its own, so the JSON
/// keeps its shape: a scrub of the whole text could break a string's quotes.
fn scrub(redaction: &Redaction, value: &mut Value) {
    match value {
        Value::String(text) => *text = redaction.scrub(text),
        Value::Array(items) => items.iter_mut().for_each(|item| scrub(redaction, item)),
        Value::Object(fields) => {
            let taken = std::mem::take(fields);
            for (key, mut field) in taken {
                scrub(redaction, &mut field);
                fields.insert(redaction.scrub(&key), field);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// The three files of the recorded run, as `acan export-replay` writes them. They are written to a
/// temporary folder, read back into memory, and deleted when this returns. `replay::export` refuses
/// any file that still holds a configured secret, so that refusal reaches the caller as an error.
async fn replay_entries<C: ModelClient>(
    host: &Host<C>,
    redaction: &Redaction,
) -> Result<Vec<(String, Vec<u8>)>, ApiError> {
    let folder = tempfile::tempdir().context("creating the export folder")?;
    let exported = replay::export(host.repo(), &folder.path().join("replay"), redaction)
        .await
        .map_err(no_run_or_internal)?;
    let mut entries = Vec::with_capacity(REPLAY_FILES.len());
    for name in REPLAY_FILES {
        let bytes = fs::read(exported.dir.join(name)).with_context(|| format!("reading {name}"))?;
        entries.push((format!("replay/{name}"), bytes));
    }
    Ok(entries)
}

/// The error of a failed replay export, as a response. A repository with no recorded run is a
/// conflict: the export names the missing run, and nothing else about the run. Any other failure
/// is internal, and its detail stays in the log.
fn no_run_or_internal(err: anyhow::Error) -> ApiError {
    if err.to_string().starts_with("no recorded run") {
        ApiError::Conflict("no_recorded_run")
    } else {
        ApiError::Internal(err)
    }
}

/// Fails when an entry still holds a `.env` value, either as it is or as JSON writes it. The error
/// names the entry and never the value.
fn ensure_no_secret(entries: &[(String, Vec<u8>)], secrets: &[String]) -> anyhow::Result<()> {
    for (name, bytes) in entries {
        for secret in secrets {
            if holds(bytes, secret) || holds(bytes, &json_escaped(secret)) {
                bail!("the export entry {name} still holds a configured secret");
            }
        }
    }
    Ok(())
}

/// Whether `haystack` holds `needle`. An empty needle is held by nothing.
fn holds(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// The text as JSON writes it inside a string: its escapes, without the quotes.
fn json_escaped(text: &str) -> String {
    serde_json::to_string(text)
        .ok()
        .as_deref()
        .and_then(|quoted| quoted.strip_prefix('"')?.strip_suffix('"'))
        .unwrap_or(text)
        .to_owned()
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The body of `POST .../settings/data/clear`. It is the whole request: `what` must be `local`,
/// and `confirm` must be the JSON boolean `true`. An extra key is refused.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClearBody {
    what: String,
    confirm: bool,
}

/// `POST .../settings/data/clear`. Only the owner may clear. The body is read as bytes and parsed
/// here, so a bad body is never echoed. The clear removes this project's installed-dependency
/// cache and empties its tour cache. It removes nothing else, and a missing cache is not an error.
///
/// Status codes: `200` with what was cleared and what was kept. `400` for a body that is not JSON,
/// names anything but `local`, does not confirm with `true`, or has an extra key. `401` for an
/// unknown token. `403` for any token but the owner's. `409` when the dependency cache is a link.
pub(crate) async fn post_clear<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    Extension(owner): Extension<DataOwner>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    require_owner(&host, &headers, &owner.founder, ONLY_OWNER_CLEARS)?;
    let body: ClearBody = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("a JSON body with what and confirm"))?;
    if body.what != "local" || !body.confirm {
        return Err(ApiError::BadRequest(
            "what must be local, and confirm must be true",
        ));
    }
    clear_deps(&host.repo().join(STATE_DIR))?;
    host.tours().clear();
    Ok((
        StatusCode::OK,
        Json(json!({ "cleared": CLEARED, "kept": KEPT })),
    )
        .into_response())
}

/// Removes the installed-dependency cache under the state directory `state`. A missing cache is
/// not an error. A link at the cache's path is refused and left as it is. So is a cache that does
/// not resolve inside the state directory, so the removal reaches only the cache.
fn clear_deps(state: &Path) -> Result<(), ApiError> {
    let target = state.join(DEPS_DIR);
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(ApiError::Internal(err.into())),
    };
    if metadata.file_type().is_symlink() {
        return Err(ApiError::Conflict("the dependency cache is a link"));
    }
    let root = fs::canonicalize(state).context("the state directory cannot be read")?;
    let real = fs::canonicalize(&target).context("the dependency cache cannot be read")?;
    if !real.starts_with(&root) || real == root {
        return Err(ApiError::Internal(anyhow!(
            "the dependency cache is outside the state directory"
        )));
    }
    if metadata.is_dir() {
        fs::remove_dir_all(&target).context("removing the dependency cache")?;
    } else {
        fs::remove_file(&target).context("removing the dependency cache")?;
    }
    Ok(())
}

/// The DOS time every entry carries: midnight.
const DOS_TIME: u16 = 0;
/// The DOS date every entry carries: 1980-01-01, the first date the format holds. Entries carry no
/// time of their own, so the same data always writes the same archive.
const DOS_DATE: u16 = 0x0021;
/// The signature of a local file header.
const LOCAL_HEADER: u32 = 0x0403_4b50;
/// The signature of a central directory header.
const CENTRAL_HEADER: u32 = 0x0201_4b50;
/// The signature of the end of central directory record.
const END_OF_CENTRAL: u32 = 0x0605_4b50;
/// The version a reader needs to open an entry: 2.0, which covers stored entries. The same value
/// names the version that wrote the entry.
const VERSION: u16 = 20;
/// The compression method of every entry: stored, with no compression.
const STORED: u16 = 0;

/// Builds a ZIP archive of `entries`, each stored with no compression, in the order given. The
/// archive uses only the fields the format's first version defines, so any reader opens it. A
/// name must be plain ASCII. The archive must fit the format's 32-bit sizes and 16-bit count.
///
/// # Errors
///
/// Returns an error when a name is empty or not ASCII, or when the archive would be too large.
fn store_zip(entries: &[(String, Vec<u8>)]) -> anyhow::Result<Vec<u8>> {
    let mut archive = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        ensure!(
            !name.is_empty() && name.is_ascii(),
            "an entry name is not a plain ASCII name"
        );
        let offset = u32::try_from(archive.len()).context("the archive is too large")?;
        let size = u32::try_from(data.len()).context("an entry is too large")?;
        let name_len = u16::try_from(name.len()).context("an entry name is too long")?;
        let crc = crc32(data);

        put_u32(&mut archive, LOCAL_HEADER);
        put_u16(&mut archive, VERSION);
        put_u16(&mut archive, 0);
        put_u16(&mut archive, STORED);
        put_u16(&mut archive, DOS_TIME);
        put_u16(&mut archive, DOS_DATE);
        put_u32(&mut archive, crc);
        put_u32(&mut archive, size);
        put_u32(&mut archive, size);
        put_u16(&mut archive, name_len);
        put_u16(&mut archive, 0);
        archive.extend_from_slice(name.as_bytes());
        archive.extend_from_slice(data);

        put_u32(&mut central, CENTRAL_HEADER);
        put_u16(&mut central, VERSION);
        put_u16(&mut central, VERSION);
        put_u16(&mut central, 0);
        put_u16(&mut central, STORED);
        put_u16(&mut central, DOS_TIME);
        put_u16(&mut central, DOS_DATE);
        put_u32(&mut central, crc);
        put_u32(&mut central, size);
        put_u32(&mut central, size);
        put_u16(&mut central, name_len);
        put_u16(&mut central, 0);
        put_u16(&mut central, 0);
        put_u16(&mut central, 0);
        put_u16(&mut central, 0);
        put_u32(&mut central, 0);
        put_u32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }

    let count = u16::try_from(entries.len()).context("the archive has too many entries")?;
    let central_offset = u32::try_from(archive.len()).context("the archive is too large")?;
    let central_size = u32::try_from(central.len()).context("the archive is too large")?;
    archive.extend_from_slice(&central);
    put_u32(&mut archive, END_OF_CENTRAL);
    put_u16(&mut archive, 0);
    put_u16(&mut archive, 0);
    put_u16(&mut archive, count);
    put_u16(&mut archive, count);
    put_u32(&mut archive, central_size);
    put_u32(&mut archive, central_offset);
    put_u16(&mut archive, 0);
    Ok(archive)
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// The CRC-32 table of the reflected IEEE polynomial, which ZIP uses: one entry for each byte.
const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
}

/// The CRC-32 of `data`, as ZIP stores it.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in data {
        let index = ((crc ^ u32::from(*byte)) & 0xFF) as usize;
        crc = CRC_TABLE[index] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The length of an archive's end record when it has no comment.
    const END_RECORD_LEN: usize = 22;

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// Reads an archive back the way a reader does: the end record, then the central directory,
    /// then each entry's local header and bytes. Returns each name and its bytes. Fails the test
    /// when a signature, a size, an offset or a CRC does not match.
    fn read_back(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
        let end = archive.len() - END_RECORD_LEN;
        assert_eq!(
            u32_at(archive, end),
            END_OF_CENTRAL,
            "the end record closes it"
        );
        let count = usize::from(u16_at(archive, end + 10));
        let central_size = u32_at(archive, end + 12) as usize;
        let central_start = u32_at(archive, end + 16) as usize;
        assert_eq!(
            central_start + central_size,
            end,
            "the directory ends where the end record starts"
        );

        let mut at = central_start;
        let mut files = Vec::new();
        for _ in 0..count {
            assert_eq!(u32_at(archive, at), CENTRAL_HEADER, "a central header");
            let crc = u32_at(archive, at + 16);
            let size = u32_at(archive, at + 24) as usize;
            let name_len = usize::from(u16_at(archive, at + 28));
            let extra_len = usize::from(u16_at(archive, at + 30));
            let comment_len = usize::from(u16_at(archive, at + 32));
            let local = u32_at(archive, at + 42) as usize;
            let name = String::from_utf8(archive[at + 46..at + 46 + name_len].to_vec())
                .expect("a UTF-8 name");

            assert_eq!(
                u32_at(archive, local),
                LOCAL_HEADER,
                "{name} has a local header"
            );
            let local_name_len = usize::from(u16_at(archive, local + 26));
            assert_eq!(
                &archive[local + 30..local + 30 + local_name_len],
                name.as_bytes(),
                "{name}: the local header names it too"
            );
            assert_eq!(u16_at(archive, local + 8), STORED, "{name} is stored");
            assert_eq!(u32_at(archive, local + 14), crc, "{name}: the local CRC");
            let data_at = local + 30 + local_name_len;
            let data = archive[data_at..data_at + size].to_vec();
            assert_eq!(crc32(&data), crc, "{name}: the CRC of its bytes");
            assert_eq!(
                u32_at(archive, local + 22) as usize,
                size,
                "{name}: its size"
            );

            files.push((name, data));
            at += 46 + name_len + extra_len + comment_len;
        }
        assert_eq!(at, end, "the directory lists every entry and nothing else");
        files
    }

    #[test]
    fn the_crc_of_the_check_string_is_the_standard_one() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn two_entries_read_back_with_their_names_sizes_and_crcs() {
        let entries = vec![
            ("manifest.json".to_owned(), b"{\"format\": 1}".to_vec()),
            ("receipts/t-12.json".to_owned(), b"123456789".to_vec()),
        ];
        let archive = store_zip(&entries).expect("the archive is written");
        assert_eq!(read_back(&archive), entries);
    }

    #[test]
    fn an_archive_with_no_entries_is_an_end_record_alone() {
        let archive = store_zip(&[]).expect("the archive is written");
        assert_eq!(archive.len(), END_RECORD_LEN);
        assert!(read_back(&archive).is_empty());
    }

    #[test]
    fn a_name_that_is_not_plain_ascii_is_refused() {
        assert!(store_zip(&[("résumé.json".to_owned(), Vec::new())]).is_err());
        assert!(store_zip(&[(String::new(), Vec::new())]).is_err());
    }

    #[test]
    fn the_export_says_which_kinds_it_holds() {
        assert_eq!(What::parse(Some("all")), Some(What::All));
        assert_eq!(What::parse(Some("replay")), Some(What::Replay));
        assert_eq!(What::parse(Some("zip")), None);
        assert_eq!(What::parse(None), None);
        assert!(What::Receipts.receipts() && !What::Receipts.replay());
        assert!(!What::Replay.receipts() && What::Replay.replay());
        assert!(What::All.receipts() && What::All.replay());
    }

    #[test]
    fn scrubbing_keeps_the_json_shape_and_removes_the_secret_text() {
        let secret = "fake-key-for-data-test";
        let mut redaction = Redaction::EMPTY;
        redaction.add_secret(secret);
        let mut value =
            json!({ "fake-key-for-data-test": ["a fake-key-for-data-test b", 1, null, true] });
        scrub(&redaction, &mut value);
        let text = value.to_string();
        assert!(
            !text.contains(secret),
            "no copy of the secret remains: {text}"
        );
        let fields = value.as_object().expect("still an object");
        assert_eq!(fields.len(), 1, "the key is kept, scrubbed");
        let items = fields
            .values()
            .next()
            .and_then(Value::as_array)
            .expect("the array");
        assert_eq!(items.len(), 4);
        assert_eq!(items[1], json!(1));
        assert_eq!(items[2], Value::Null);
        assert_eq!(items[3], json!(true));
    }

    #[test]
    fn a_leftover_secret_fails_the_archive_and_is_not_named_in_the_error() {
        let secret = "fake\"key-for-data-test";
        let escaped = "fake\\\"key-for-data-test";
        let entries = vec![(
            "replay/events.json".to_owned(),
            format!("{{\"note\":\"{escaped}\"}}").into_bytes(),
        )];
        let error = ensure_no_secret(&entries, &[secret.to_owned()])
            .expect_err("the secret in its JSON form fails");
        let message = error.to_string();
        assert!(message.contains("replay/events.json"), "{message}");
        assert!(!message.contains("key-for-data-test"), "{message}");

        let clean = vec![("manifest.json".to_owned(), b"{}".to_vec())];
        assert!(ensure_no_secret(&clean, &[secret.to_owned()]).is_ok());
    }

    #[test]
    fn a_dotenv_file_gives_its_long_values_and_a_broken_one_gives_no_text() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(
            env_secrets(dir.path())
                .expect("a missing file holds none")
                .is_empty()
        );
        fs::write(
            dir.path().join(".env"),
            "SHORT=abc\nLONG_NAME=fake-key-for-data-test\n",
        )
        .expect(".env is written");
        assert_eq!(
            env_secrets(dir.path()).expect("the file parses"),
            vec!["fake-key-for-data-test".to_owned()]
        );

        fs::write(
            dir.path().join(".env"),
            "LONG_NAME=fake-key-for-data-test\nnot a line\n",
        )
        .expect(".env is written");
        let error = env_secrets(dir.path()).expect_err("a line that does not parse fails");
        assert!(!error.to_string().contains("fake-key-for-data-test"));
    }
}
