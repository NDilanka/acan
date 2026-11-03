//! The advisory vision witness.
//!
//! After a scope-first run records its receipt, the dashboard page is captured from the base and
//! from the candidate, inside the same witness containers every probe runs in. Each page is
//! rendered on the host with scripts and the network off (see [`browser`]), and one Meta vision
//! call compares the two screenshots against the scope's must-keep items (see [`judge`]).
//!
//! The verdict is advisory. It is written to the vision record and summarized in one
//! `vision.checked` event, and nothing the gate or a land decides reads either. Every failure
//! ends as a `not_checked` check or a `failed` record, never as a pass or a fail. Records and
//! screenshots live in the vision data directory, which must be outside the repository.

mod browser;
mod judge;

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use acan_core::{Command, GateRunId, Step, SystemCmd, TaskId, VisionCounts, VisionStatus, World};
use acan_git::Preparation;
use acan_intent::{ContentHash, CorpusProbe, ProbeCorpus, RunRole, Scope, StepId};
use acan_llm::ModelClient;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::host::Host;
use crate::observe::{self, Plan};

pub use browser::{CHROME, EDGE, default_browser};

/// The record version the routes serve.
pub(crate) const VERSION: &str = "vision_witness/1";
/// The corpus probe whose page is captured, and the step whose response is that page: the
/// dashboard as a signed-in user sees it.
const PROBE: &str = "dashboard-session";
const STEP: &str = "s3";
/// The longest reason a check keeps, in characters.
const REASON_CHARS: usize = 200;
const RECORD_FILE: &str = "witness.json";

/// Where the vision witness renders pages and keeps its records.
#[derive(Debug, Clone)]
pub struct VisionConfig {
    /// The headless Chromium browser that renders captured pages.
    pub browser: PathBuf,
    /// The directory records and screenshots are kept in, one folder per served repository. It
    /// must not be inside the repository.
    pub data_dir: PathBuf,
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            browser: default_browser(),
            data_dir: default_data_dir(),
        }
    }
}

/// The per-user data directory: `%LOCALAPPDATA%\acan\vision`, else `$XDG_DATA_HOME/acan/vision`,
/// else the temporary directory.
#[must_use]
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("acan")
        .join("vision")
}

/// An open vision witness: the browser it renders with and the folder this repository's records
/// are kept in.
#[derive(Debug, Clone)]
pub struct Vision {
    browser: PathBuf,
    root: PathBuf,
    /// The runs this process is witnessing now. A stored `pending` record outside this set was
    /// left by a witness that a restart interrupted.
    running: Arc<Mutex<BTreeSet<GateRunId>>>,
}

impl Vision {
    /// Opens the records folder of `repo` under the configured data directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the folder cannot be created, or when it is inside `repo`.
    pub fn open(config: &VisionConfig, repo: &Path) -> anyhow::Result<Self> {
        let lexical_repo = std::path::absolute(repo)?;
        let repo = std::fs::canonicalize(repo)
            .with_context(|| format!("resolving the repository {}", repo.display()))?;
        let root = config.data_dir.join(world_key(&repo));
        // Checked before creating anything, so a refused directory leaves nothing in the
        // repository. The check after creation catches links the lexical path cannot see.
        anyhow::ensure!(
            !std::path::absolute(&root)?.starts_with(&lexical_repo),
            "the vision data directory {} is inside the repository {}; screenshots are never kept in a repository",
            root.display(),
            repo.display()
        );
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating the vision directory {}", root.display()))?;
        let root = std::fs::canonicalize(&root)?;
        anyhow::ensure!(
            !root.starts_with(&repo),
            "the vision data directory {} is inside the repository {}; screenshots are never kept in a repository",
            root.display(),
            repo.display()
        );
        Ok(Self {
            browser: config.browser.clone(),
            root,
            running: Arc::default(),
        })
    }

    /// The folder of one gate run's record and screenshots. Both ids are parsed values, so the
    /// path never leaves the root.
    fn run_dir(&self, task: TaskId, run: GateRunId) -> PathBuf {
        self.root.join(task.to_string()).join(run.to_string())
    }

    /// Marks `run` as witnessed by this process until the returned guard drops.
    fn witnessing(&self, run: GateRunId) -> Witnessing {
        self.running
            .lock()
            .expect("the running set is not poisoned")
            .insert(run);
        Witnessing {
            running: Arc::clone(&self.running),
            run,
        }
    }

    fn is_witnessed(&self, run: GateRunId) -> bool {
        self.running
            .lock()
            .expect("the running set is not poisoned")
            .contains(&run)
    }

    /// The stored record of `run`, if one was written.
    ///
    /// # Errors
    ///
    /// Returns an error when the record exists but cannot be read or does not parse.
    pub(crate) async fn record(
        &self,
        task: TaskId,
        run: GateRunId,
    ) -> anyhow::Result<Option<VisionWitness>> {
        let path = self.run_dir(task, run).join(RECORD_FILE);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("reading the vision record {}", path.display())
            })?)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The stored screenshot of one side of check `index`, if there is one.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read.
    pub(crate) async fn image(
        &self,
        task: TaskId,
        run: GateRunId,
        index: u32,
        side: Side,
    ) -> io::Result<Option<Vec<u8>>> {
        let path = self.run_dir(task, run).join(side.file(index));
        match tokio::fs::read(path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Writes the record of `witness`'s run. It is written aside and renamed into place, so a
    /// reader never sees half a record.
    async fn save(&self, task: TaskId, run: GateRunId, witness: &VisionWitness) -> io::Result<()> {
        let dir = self.run_dir(task, run);
        tokio::fs::create_dir_all(&dir).await?;
        let partial = dir.join(format!("{RECORD_FILE}.partial"));
        let bytes = serde_json::to_vec_pretty(witness).map_err(io::Error::other)?;
        tokio::fs::write(&partial, bytes).await?;
        tokio::fs::rename(&partial, dir.join(RECORD_FILE)).await
    }

    async fn save_image(
        &self,
        task: TaskId,
        run: GateRunId,
        index: u32,
        side: Side,
        png: &[u8],
    ) -> io::Result<()> {
        let dir = self.run_dir(task, run);
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::write(dir.join(side.file(index)), png).await
    }
}

/// Which page of a check a screenshot shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Baseline,
    Candidate,
}

impl Side {
    fn file(self, index: u32) -> String {
        match self {
            Self::Baseline => format!("{index}.baseline.png"),
            Self::Candidate => format!("{index}.png"),
        }
    }
}

/// The folder name of one repository: a short hash of its resolved path, so two servers on two
/// repositories never share records.
fn world_key(repo: &Path) -> String {
    let hash = sha256_hex(repo.to_string_lossy().as_bytes());
    hash[..16].to_owned()
}

/// The lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let hash = ContentHash::of_bytes(bytes).to_string();
    match hash.split_once(':') {
        Some((_, hex)) => hex.to_owned(),
        None => hash,
    }
}

/// `text` cut to at most [`REASON_CHARS`] characters.
pub(crate) fn clip(text: &str) -> String {
    text.chars().take(REASON_CHARS).collect()
}

/// The vision record of one gate run, as `GET /api/tasks/{task}/vision` serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisionWitness {
    /// Always `vision_witness/1`.
    pub version: String,
    /// The task, as `t-N`.
    pub task: String,
    /// The gate run, as `g-N`.
    pub gate_run: String,
    pub status: WitnessStatus,
    /// The model that answered, once a call returned.
    pub model: Option<String>,
    /// The provider of that same call; null when no call returned.
    pub provider: Option<String>,
    pub failed_reason: Option<String>,
    pub checks: Vec<VisionCheck>,
}

/// Whether a vision record is still running, ran, or could not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WitnessStatus {
    Pending,
    Done,
    Failed,
}

/// One page compared against the must-keep items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisionCheck {
    pub index: u32,
    pub route: String,
    pub comparison: Comparison,
    pub must_keep: Vec<MustKeepItem>,
    pub baseline_image_sha256: Option<String>,
    /// `None` only when the candidate page has no screenshot; the check is then `not_checked`.
    pub candidate_image_sha256: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub verdict: Verdict,
    pub reason: Option<String>,
    /// The model's text, kept only when it did not parse.
    pub raw_reply: Option<String>,
    pub request_id: Option<String>,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub latency_ms: u64,
    pub not_checked_reason: Option<String>,
    /// The wonder role's second look, when the first look was `inconsistent` or `unclear`.
    /// The fields above stay the first look's, so the verdict, counts and event never change.
    #[serde(default)]
    pub second_look: Option<SecondLook>,
}

/// The advisory second look at one check: one call to the wonder role, with the same images and
/// must-keep items as the first look.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondLook {
    /// The model that judged the second look; null when no call returned.
    pub model: Option<String>,
    /// The provider of that call; null when no call returned.
    pub provider: Option<String>,
    pub verdict: Verdict,
    /// Whether the second look reached the first look's verdict; null when it is `not_checked`.
    pub agrees: Option<bool>,
    pub reason: Option<String>,
    /// The model's text, kept only when it did not parse.
    pub raw_reply: Option<String>,
    pub request_id: Option<String>,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub latency_ms: u64,
    pub not_checked_reason: Option<String>,
}

impl SecondLook {
    fn of(first: Verdict, judged: judge::Judgement) -> Self {
        let call = judged.call.as_ref();
        Self {
            model: judged.model,
            provider: call.map(|call| call.provider.clone()),
            verdict: judged.verdict,
            agrees: (judged.verdict != Verdict::NotChecked).then_some(judged.verdict == first),
            reason: judged.reason,
            raw_reply: judged.raw_reply,
            request_id: judged.request_id,
            prompt_tokens: call.map_or(0, |call| call.usage.prompt_tokens),
            completion_tokens: call.map_or(0, |call| call.usage.completion_tokens),
            latency_ms: call.map_or(0, latency_ms),
            not_checked_reason: judged.not_checked_reason,
        }
    }
}

fn latency_ms(call: &acan_llm::CallRecord) -> u64 {
    u64::try_from(call.latency.as_millis()).unwrap_or(u64::MAX)
}

/// What the candidate was compared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    BaselineVsCandidate,
    CandidateOnly,
}

/// One must-keep item of the scope, in the words the check sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MustKeepItem {
    pub id: String,
    pub text: String,
}

/// An advisory verdict. None of them is a pass or a fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Consistent,
    Inconsistent,
    Unclear,
    NotChecked,
}

impl VisionWitness {
    fn new(task: TaskId, run: GateRunId, status: WitnessStatus) -> Self {
        Self {
            version: VERSION.to_owned(),
            task: task.to_string(),
            gate_run: run.to_string(),
            status,
            model: None,
            provider: None,
            failed_reason: None,
            checks: Vec::new(),
        }
    }

    fn failed(task: TaskId, run: GateRunId, reason: String) -> Self {
        Self {
            failed_reason: Some(reason),
            ..Self::new(task, run, WitnessStatus::Failed)
        }
    }

    /// How many checks reached each verdict.
    #[must_use]
    pub fn counts(&self) -> VisionCounts {
        let count = |verdict: Verdict| {
            let checks = self.checks.iter().filter(|check| check.verdict == verdict);
            u32::try_from(checks.count()).unwrap_or(u32::MAX)
        };
        VisionCounts {
            consistent: count(Verdict::Consistent),
            inconsistent: count(Verdict::Inconsistent),
            unclear: count(Verdict::Unclear),
            not_checked: count(Verdict::NotChecked),
        }
    }
}

/// The record `GET /api/tasks/{task}/vision` serves for `run`: the stored one, `pending` when
/// none is stored yet, or `failed` when this server runs no vision witness or a stored check was
/// interrupted by a restart.
///
/// # Errors
///
/// Returns an error when a stored record cannot be read.
pub(crate) async fn served(
    vision: Option<&Vision>,
    task: TaskId,
    run: GateRunId,
) -> anyhow::Result<VisionWitness> {
    let Some(vision) = vision else {
        return Ok(VisionWitness::failed(
            task,
            run,
            "this server runs no vision witness".to_owned(),
        ));
    };
    let Some(record) = vision.record(task, run).await? else {
        return Ok(VisionWitness::new(task, run, WitnessStatus::Pending));
    };
    if matches!(record.status, WitnessStatus::Pending) && !vision.is_witnessed(run) {
        return Ok(VisionWitness::failed(
            task,
            run,
            "the server stopped before the vision check finished".to_owned(),
        ));
    }
    Ok(record)
}

/// The latest scope-first gate run of `task`, or `None` when it has none.
pub(crate) fn latest_run(world: &World, task: TaskId) -> Option<GateRunId> {
    let order = world.task(task)?.order;
    world
        .lane(order)?
        .runs
        .iter()
        .rev()
        .find(|(_, lane_run)| lane_run.task == task)
        .map(|(run, _)| *run)
}

/// Keeps `run` in [`Vision::witnessing`] until it drops, including on an early return.
struct Witnessing {
    running: Arc<Mutex<BTreeSet<GateRunId>>>,
    run: GateRunId,
}

impl Drop for Witnessing {
    fn drop(&mut self) {
        // A destructor never panics. The set's critical sections cannot panic, so a poisoned
        // lock is not expected here; skipping the removal is the only safe thing left to do.
        if let Ok(mut running) = self.running.lock() {
            running.remove(&self.run);
        }
    }
}

/// Runs the vision witness of `run` and records its verdicts. Nothing here returns an error or
/// changes the gate: a failure is a `failed` record or a `not_checked` check, and is logged.
pub(crate) async fn witness<C: ModelClient>(
    host: &Host<C>,
    vision: &Vision,
    plan: &Plan<'_>,
    task: TaskId,
    run: GateRunId,
    scope: &Scope,
) {
    let _witnessing = vision.witnessing(run);
    let pending = VisionWitness::new(task, run, WitnessStatus::Pending);
    if let Err(err) = vision.save(task, run, &pending).await {
        tracing::warn!(%task, %run, error = %err, "the vision record could not be written");
    }
    let record = match checks(host, vision, plan, task, run, scope).await {
        Ok((checks, model, provider)) => VisionWitness {
            model,
            provider,
            checks,
            ..VisionWitness::new(task, run, WitnessStatus::Done)
        },
        Err(reason) => {
            tracing::warn!(%task, %run, %reason, "the vision witness could not run");
            VisionWitness::failed(task, run, reason)
        }
    };
    if let Err(err) = vision.save(task, run, &record).await {
        tracing::warn!(%task, %run, error = %err, "the vision record could not be written");
    }
    let counts = record.counts();
    let status = match record.status {
        WitnessStatus::Failed => VisionStatus::Failed,
        WitnessStatus::Pending | WitnessStatus::Done => VisionStatus::Done,
    };
    tracing::info!(%task, %run, ?status, ?counts, "the vision witness finished");
    let command = SystemCmd::VisionChecked {
        task,
        gate_run: run,
        status,
        counts,
        human_questions: counts.inconsistent.saturating_add(counts.unclear),
    };
    if let Err(err) = host.submit_advisory(Command::System(command)).await {
        tracing::warn!(%task, %run, error = %err, "recording the vision verdicts failed");
    }
}

/// The check of a vision role that is off, or `None` when the call may be made. An off role makes
/// no call and renders no screenshot. Its one check is `not_checked`, with the reason.
pub(crate) fn off_check<C: ModelClient>(
    client: &C,
    route: &str,
    must_keep: &[MustKeepItem],
) -> Option<VisionCheck> {
    let reason = judge::off_reason(client, acan_llm::Role::Vision)?;
    Some(VisionCheck {
        index: 0,
        route: route.to_owned(),
        comparison: Comparison::CandidateOnly,
        must_keep: must_keep.to_vec(),
        baseline_image_sha256: None,
        candidate_image_sha256: None,
        width: None,
        height: None,
        verdict: Verdict::NotChecked,
        reason: None,
        raw_reply: None,
        request_id: None,
        prompt_tokens: 0,
        completion_tokens: 0,
        latency_ms: 0,
        not_checked_reason: Some(clip(&reason)),
        second_look: None,
    })
}

/// The checks of `run`, and the model and provider of the call that answered them, when one
/// returned. `Err` is why the witness could not run.
async fn checks<C: ModelClient>(
    host: &Host<C>,
    vision: &Vision,
    plan: &Plan<'_>,
    task: TaskId,
    run: GateRunId,
    scope: &Scope,
) -> Result<(Vec<VisionCheck>, Option<String>, Option<String>), String> {
    let (probe, route) = dashboard(plan.corpus)?;
    let step = StepId::from_str(STEP).map_err(|err| err.to_string())?;
    let must_keep: Vec<MustKeepItem> = scope
        .body
        .must_keep
        .iter()
        .map(|rule| MustKeepItem {
            id: rule.id.as_str().to_owned(),
            text: rule.text.clone(),
        })
        .collect();
    if must_keep.is_empty() {
        return Err("the scope has no must_keep items to check".to_owned());
    }
    if let Some(check) = off_check(host.client(), &route, &must_keep) {
        return Ok((vec![check], None, None));
    }
    let dir = plan.scratch;
    let (baseline, candidate) = tokio::join!(
        page(vision, plan, probe, &step, Page::base(plan, dir)),
        page(vision, plan, probe, &step, Page::candidate(plan, dir)),
    );
    let index = 0;
    let mut check = VisionCheck {
        index,
        route,
        comparison: if baseline.is_ok() {
            Comparison::BaselineVsCandidate
        } else {
            Comparison::CandidateOnly
        },
        must_keep,
        baseline_image_sha256: None,
        candidate_image_sha256: None,
        width: None,
        height: None,
        verdict: Verdict::NotChecked,
        reason: None,
        raw_reply: None,
        request_id: None,
        prompt_tokens: 0,
        completion_tokens: 0,
        latency_ms: 0,
        not_checked_reason: None,
        second_look: None,
    };
    if let Err(reason) = &baseline {
        tracing::info!(%task, %run, %reason, "the vision check has no baseline");
    }
    if let Ok(shot) = &baseline {
        check.baseline_image_sha256 = Some(sha256_hex(&shot.png));
        keep_image(vision, task, run, index, Side::Baseline, &shot.png).await;
    }
    let shot = match candidate {
        Ok(shot) => shot,
        Err(reason) => {
            check.not_checked_reason = Some(clip(&reason));
            return Ok((vec![check], None, None));
        }
    };
    check.candidate_image_sha256 = Some(sha256_hex(&shot.png));
    check.width = Some(shot.width);
    check.height = Some(shot.height);
    keep_image(vision, task, run, index, Side::Candidate, &shot.png).await;

    let pair = judge::Pair {
        route: &check.route,
        must_keep: &check.must_keep,
        baseline: baseline.as_ref().ok().map(|shot| shot.png.as_slice()),
        candidate: &shot.png,
    };
    let judged = judge::judge(host.client(), &pair).await;
    let provider = judged.call.as_ref().map(|call| call.provider.clone());
    check.verdict = judged.verdict;
    check.reason = judged.reason;
    check.raw_reply = judged.raw_reply;
    check.not_checked_reason = judged.not_checked_reason;
    check.request_id = judged.request_id;
    if let Some(call) = &judged.call {
        check.prompt_tokens = call.usage.prompt_tokens;
        check.completion_tokens = call.usage.completion_tokens;
        check.latency_ms = latency_ms(call);
        record_usage(host, task, run, call).await;
    }
    if let Some(second) = judge::second_look(host.client(), &pair, check.verdict).await {
        if let Some(call) = &second.call {
            record_usage(host, task, run, call).await;
        }
        let second = SecondLook::of(check.verdict, second);
        tracing::info!(%task, %run, verdict = ?second.verdict, agrees = ?second.agrees, "the vision check had a second look");
        check.second_look = Some(second);
    }
    Ok((vec![check], judged.model, provider))
}

/// The dashboard probe of `corpus` and the route of its captured step.
fn dashboard(corpus: &ProbeCorpus) -> Result<(&CorpusProbe, String), String> {
    let probe = corpus
        .probes
        .iter()
        .find(|probe| probe.label == PROBE)
        .ok_or_else(|| format!("the probe corpus has no {PROBE} probe"))?;
    let route = probe
        .spec
        .steps
        .iter()
        .find_map(|step| match step {
            acan_intent::Step::Http(http) if http.id.as_str() == STEP => {
                Some(http.request.path.as_str().to_owned())
            }
            _ => None,
        })
        .ok_or_else(|| format!("the {PROBE} probe has no HTTP step {STEP}"))?;
    Ok((probe, route))
}

/// One side of a check: the commit it runs and the folder it stages in.
struct Page<'a> {
    preparation: &'a Preparation,
    role: RunRole,
    dir: PathBuf,
    what: &'static str,
}

impl<'a> Page<'a> {
    fn base(plan: &Plan<'a>, dir: &Path) -> Self {
        Self {
            preparation: plan.base,
            role: RunRole::BaseA,
            dir: dir.join("base"),
            what: "base",
        }
    }

    fn candidate(plan: &Plan<'a>, dir: &Path) -> Self {
        Self {
            preparation: plan.candidate,
            role: RunRole::Candidate,
            dir: dir.join("candidate"),
            what: "candidate",
        }
    }
}

/// Captures `page`'s HTML in the witness sandbox and renders it on the host. `Err` is why the
/// page has no screenshot.
async fn page(
    vision: &Vision,
    plan: &Plan<'_>,
    probe: &CorpusProbe,
    step: &StepId,
    page: Page<'_>,
) -> Result<browser::Screenshot, String> {
    let what = page.what;
    let kept = observe::keep_page(plan, probe, step, page.preparation, page.role, &page.dir)
        .await
        .map_err(|err| format!("the {what} page was not captured: {err}"))?
        .ok_or_else(|| format!("the {what} run did not reach step {STEP}"))?;
    let html = kept
        .content_type
        .as_deref()
        .is_some_and(|kind| kind.to_ascii_lowercase().contains("text/html"));
    if kept.status != Some(200) || !html {
        return Err(format!(
            "the {what} page answered {} {}, not an HTML page",
            kept.status
                .map_or_else(|| "no status".to_owned(), |s| s.to_string()),
            kept.content_type
                .as_deref()
                .unwrap_or("with no content type"),
        ));
    }
    if kept.truncated {
        return Err(format!(
            "the {what} page was longer than the capture limit, so it was not judged"
        ));
    }
    browser::render(&vision.browser, &kept.body)
        .await
        .map_err(|err| format!("the {what} page was not rendered: {err}"))
}

async fn keep_image(
    vision: &Vision,
    task: TaskId,
    run: GateRunId,
    index: u32,
    side: Side,
    png: &[u8],
) {
    if let Err(err) = vision.save_image(task, run, index, side, png).await {
        tracing::warn!(%task, %run, ?side, error = %err, "a vision screenshot could not be written");
    }
}

/// Meters a vision call in the usage panel. A second look is metered under the vision step too,
/// so it gets its own row by its model's name. Metering is best-effort, as for every model call.
async fn record_usage<C: ModelClient>(
    host: &Host<C>,
    task: TaskId,
    run: GateRunId,
    call: &acan_llm::CallRecord,
) {
    let command = SystemCmd::ModelUsed {
        model: call.model.clone(),
        provider: Some(call.provider.clone()),
        step: Step::Vision,
        input_tokens: Some(call.usage.prompt_tokens),
        output_tokens: Some(call.usage.completion_tokens),
        failure: None,
        detail: None,
    };
    if let Err(err) = host.submit_advisory(Command::System(command)).await {
        tracing::warn!(%task, %run, error = %err, "recording the vision call's usage failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_off_vision_role_records_one_not_checked_check_and_makes_no_call() {
        let client = judge::counting::CountingClient::new(true);
        let must_keep = vec![MustKeepItem {
            id: "keep-totals".to_owned(),
            text: "The total clicks column stays.".to_owned(),
        }];
        let check = off_check(&client, "/dashboard", &must_keep).expect("an off role has a check");
        assert_eq!(check.verdict, Verdict::NotChecked);
        assert_eq!(
            check.not_checked_reason.as_deref(),
            Some("vision is off: it needs a Meta key")
        );
        assert_eq!(check.route, "/dashboard");
        assert_eq!(check.must_keep, must_keep);
        assert_eq!(
            check.candidate_image_sha256, None,
            "no screenshot is rendered"
        );
        assert_eq!(client.calls(), 0);
    }

    #[test]
    fn an_on_vision_role_has_no_off_check() {
        let client = judge::counting::CountingClient::new(false);
        assert!(off_check(&client, "/dashboard", &[]).is_none());
        assert_eq!(client.calls(), 0, "the check itself sends nothing");
    }

    fn ids() -> (TaskId, GateRunId) {
        (
            "t-3".parse().expect("a task id"),
            "g-5".parse().expect("a gate run id"),
        )
    }

    fn check(verdict: Verdict) -> VisionCheck {
        VisionCheck {
            index: 0,
            route: "/dashboard".to_owned(),
            comparison: Comparison::BaselineVsCandidate,
            must_keep: vec![MustKeepItem {
                id: "keep-totals".to_owned(),
                text: "The total clicks column stays".to_owned(),
            }],
            baseline_image_sha256: Some("ab".repeat(32)),
            candidate_image_sha256: Some("cd".repeat(32)),
            width: Some(1280),
            height: Some(800),
            verdict,
            reason: Some("The column is shown.".to_owned()),
            raw_reply: None,
            request_id: Some("req-1".to_owned()),
            prompt_tokens: 10,
            completion_tokens: 2,
            latency_ms: 300,
            not_checked_reason: None,
            second_look: None,
        }
    }

    fn judged(verdict: Verdict, call: bool) -> judge::Judgement {
        judge::Judgement {
            verdict,
            reason: call.then(|| "The summary line is shown.".to_owned()),
            raw_reply: None,
            not_checked_reason: (!call).then(|| "the second look failed: no route".to_owned()),
            request_id: call.then(|| "req-2".to_owned()),
            model: call.then(|| "muse-wonder-1.0".to_owned()),
            call: call.then(|| acan_llm::CallRecord {
                role: acan_llm::Role::Wonder,
                provider: "meta".to_owned(),
                model: "muse-wonder-1.0".to_owned(),
                usage: acan_llm::Usage {
                    prompt_tokens: 40,
                    completion_tokens: 700,
                },
                latency: std::time::Duration::from_millis(6_100),
            }),
        }
    }

    #[test]
    fn a_second_look_records_its_model_and_whether_it_agrees() {
        let disagrees = SecondLook::of(Verdict::Inconsistent, judged(Verdict::Consistent, true));
        assert_eq!(disagrees.model.as_deref(), Some("muse-wonder-1.0"));
        assert_eq!(disagrees.provider.as_deref(), Some("meta"));
        assert_eq!(disagrees.agrees, Some(false));
        assert_eq!(
            (
                disagrees.prompt_tokens,
                disagrees.completion_tokens,
                disagrees.latency_ms
            ),
            (40, 700, 6_100)
        );

        let agrees = SecondLook::of(Verdict::Unclear, judged(Verdict::Unclear, true));
        assert_eq!(agrees.agrees, Some(true));

        let failed = SecondLook::of(Verdict::Inconsistent, judged(Verdict::NotChecked, false));
        assert_eq!(failed.agrees, None);
        assert_eq!(failed.model, None);
        assert_eq!(failed.provider, None);
        assert_eq!((failed.prompt_tokens, failed.latency_ms), (0, 0));
        assert!(failed.not_checked_reason.is_some());
    }

    #[test]
    fn a_second_look_is_kept_beside_the_first_verdict() {
        let (task, run) = ids();
        let mut escalated = check(Verdict::Inconsistent);
        escalated.second_look = Some(SecondLook::of(
            Verdict::Inconsistent,
            judged(Verdict::Consistent, true),
        ));
        let record = VisionWitness {
            checks: vec![check(Verdict::Consistent), escalated],
            ..VisionWitness::new(task, run, WitnessStatus::Done)
        };
        let json = serde_json::to_value(&record).expect("serializes");
        assert_eq!(json["checks"][0]["second_look"], serde_json::Value::Null);
        let second = &json["checks"][1]["second_look"];
        assert_eq!(json["checks"][1]["verdict"], "inconsistent");
        assert_eq!(second["verdict"], "consistent");
        assert_eq!(second["agrees"], false);
        assert_eq!(second["model"], "muse-wonder-1.0");
        assert_eq!(
            record.counts().inconsistent,
            1,
            "the first look stays the counted verdict"
        );
        assert_eq!(
            serde_json::from_value::<VisionWitness>(json).expect("parses back"),
            record
        );
    }

    #[test]
    fn a_record_written_before_second_looks_still_reads() {
        let (task, run) = ids();
        let record = VisionWitness {
            checks: vec![check(Verdict::Unclear)],
            ..VisionWitness::new(task, run, WitnessStatus::Done)
        };
        let mut json = serde_json::to_value(&record).expect("serializes");
        json["checks"][0]
            .as_object_mut()
            .expect("an object")
            .remove("second_look");
        assert_eq!(
            serde_json::from_value::<VisionWitness>(json).expect("parses"),
            record
        );
    }

    #[test]
    fn a_record_serializes_in_the_contract_shape() {
        let (task, run) = ids();
        let record = VisionWitness {
            model: Some("muse-spark-1.3".to_owned()),
            provider: Some("meta".to_owned()),
            checks: vec![check(Verdict::NotChecked)],
            ..VisionWitness::new(task, run, WitnessStatus::Done)
        };
        let json = serde_json::to_value(&record).expect("serializes");
        assert_eq!(json["version"], "vision_witness/1");
        assert_eq!(json["task"], "t-3");
        assert_eq!(json["gate_run"], "g-5");
        assert_eq!(json["status"], "done");
        assert_eq!(json["provider"], "meta");
        let unanswered = VisionWitness::new(task, run, WitnessStatus::Pending);
        let unanswered_json = serde_json::to_value(&unanswered).expect("serializes");
        assert_eq!(unanswered_json["provider"], serde_json::Value::Null);
        assert_eq!(json["failed_reason"], serde_json::Value::Null);
        let check = &json["checks"][0];
        assert_eq!(check["comparison"], "baseline_vs_candidate");
        assert_eq!(check["verdict"], "not_checked");
        assert_eq!(check["must_keep"][0]["id"], "keep-totals");
        assert_eq!(check["raw_reply"], serde_json::Value::Null);
        let keys: Vec<&str> = check
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        for key in [
            "index",
            "route",
            "comparison",
            "must_keep",
            "baseline_image_sha256",
            "candidate_image_sha256",
            "width",
            "height",
            "verdict",
            "reason",
            "raw_reply",
            "request_id",
            "prompt_tokens",
            "completion_tokens",
            "latency_ms",
            "not_checked_reason",
            "second_look",
        ] {
            assert!(keys.contains(&key), "{key} in {keys:?}");
        }
        assert_eq!(
            serde_json::from_value::<VisionWitness>(json).expect("parses back"),
            record
        );
    }

    #[test]
    fn counts_tally_each_verdict() {
        let (task, run) = ids();
        let record = VisionWitness {
            checks: vec![
                check(Verdict::Consistent),
                check(Verdict::Inconsistent),
                check(Verdict::Unclear),
                check(Verdict::Unclear),
                check(Verdict::NotChecked),
            ],
            ..VisionWitness::new(task, run, WitnessStatus::Done)
        };
        assert_eq!(
            record.counts(),
            VisionCounts {
                consistent: 1,
                inconsistent: 1,
                unclear: 2,
                not_checked: 1,
            }
        );
    }

    #[test]
    fn a_data_directory_inside_the_repository_is_refused() {
        let repo = tempfile::tempdir().expect("a repo dir");
        let inside = VisionConfig {
            browser: PathBuf::from(EDGE),
            data_dir: repo.path().join(".acan").join("vision"),
        };
        let err = Vision::open(&inside, repo.path()).expect_err("refused");
        assert!(err.to_string().contains("inside the repository"), "{err}");
        assert!(
            !repo.path().join(".acan").exists(),
            "a refused directory is never created"
        );

        let data = tempfile::tempdir().expect("a data dir");
        let outside = VisionConfig {
            browser: PathBuf::from(EDGE),
            data_dir: data.path().to_path_buf(),
        };
        let vision = Vision::open(&outside, repo.path()).expect("opens");
        assert!(
            vision
                .root
                .starts_with(std::fs::canonicalize(data.path()).expect("resolves"))
        );
    }

    #[tokio::test]
    async fn a_pending_record_that_no_witness_runs_is_reported_as_interrupted() {
        let (task, run) = ids();
        let repo = tempfile::tempdir().expect("a repo dir");
        let data = tempfile::tempdir().expect("a data dir");
        let vision = Vision::open(
            &VisionConfig {
                browser: PathBuf::from(EDGE),
                data_dir: data.path().to_path_buf(),
            },
            repo.path(),
        )
        .expect("opens");
        let pending = VisionWitness::new(task, run, WitnessStatus::Pending);
        vision.save(task, run, &pending).await.expect("saves");
        let interrupted = served(Some(&vision), task, run).await.expect("serves");
        assert_eq!(interrupted.status, WitnessStatus::Failed);
        assert!(
            interrupted
                .failed_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("stopped")),
            "{interrupted:?}"
        );
        let witnessing = vision.witnessing(run);
        let running = served(Some(&vision), task, run).await.expect("serves");
        assert_eq!(running.status, WitnessStatus::Pending);
        drop(witnessing);
    }

    #[tokio::test]
    async fn records_and_screenshots_round_trip_under_the_run_folder() {
        let (task, run) = ids();
        let repo = tempfile::tempdir().expect("a repo dir");
        let data = tempfile::tempdir().expect("a data dir");
        let vision = Vision::open(
            &VisionConfig {
                browser: PathBuf::from(EDGE),
                data_dir: data.path().to_path_buf(),
            },
            repo.path(),
        )
        .expect("opens");
        assert_eq!(vision.record(task, run).await.expect("reads"), None);
        let record = VisionWitness {
            checks: vec![check(Verdict::Consistent)],
            ..VisionWitness::new(task, run, WitnessStatus::Done)
        };
        vision.save(task, run, &record).await.expect("saves");
        vision
            .save_image(task, run, 0, Side::Candidate, b"png")
            .await
            .expect("saves the image");
        assert_eq!(vision.record(task, run).await.expect("reads"), Some(record));
        assert_eq!(
            vision
                .image(task, run, 0, Side::Candidate)
                .await
                .expect("reads"),
            Some(b"png".to_vec())
        );
        assert_eq!(
            vision
                .image(task, run, 0, Side::Baseline)
                .await
                .expect("reads"),
            None
        );
        assert!(
            vision
                .run_dir(task, run)
                .ends_with(Path::new("t-3").join("g-5"))
        );
    }

    #[tokio::test]
    async fn a_server_without_the_witness_serves_a_failed_record() {
        let (task, run) = ids();
        let served = served(None, task, run).await.expect("serves");
        assert_eq!(served.status, WitnessStatus::Failed);
        assert!(served.failed_reason.is_some());
    }

    #[test]
    fn a_hash_is_bare_hex() {
        let hex = sha256_hex(b"");
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
