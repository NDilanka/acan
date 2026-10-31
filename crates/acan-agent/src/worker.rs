//! The worker's lifecycle: list, read back, claim, declare, wait for Building, edit, commit, and
//! report. The origin decides every rule. This module sequences the calls, renews the lease, and
//! keeps the worker's own writes behind the origin's build clearance and its fence.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use acan_core::{ClashState, Locus, MAX_PREDICTIONS, Settlement};
use acan_intent::{
    Change, CheckResult, ContestKind, ContestResponse, Enforcement, EpisodeId, GetReceiptResponse,
    GetRulingsResponse, GetScopeResponse, ItemClass, ItemState, MachineId, MustKeep, NonEmpty,
    NotEnforcedReason, Predicted, Prediction, ProbeHash, Receipt, ReceiptHash, ReceiptItem,
    RulingSummary, RulingTagState, Scope, ScopeItem,
};
use acan_llm::{ChatRequest, Message, ModelClient, Role, ToolCall, ToolSpec};
use anyhow::{Context, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::grounding::RepoFiles;
use crate::mcp::{McpClient, Reply};
use crate::tools::Worktree;

const PLAN_PROMPT: &str = "You plan one task for an AI coding worker in a shared repository. Reply with JSON: summary, one or two sentences on the change you will make; files, the existing repository files you will edit, each copied exactly from the repository file list; and new_files, the files you will create, only when they do not exist yet and their directory is already in the repository. Do not invent paths. List every file you will write and no other.";

const EDIT_PROMPT: &str = "You are an AI coding worker. Your task is cleared to build, and you may change only the files you declared. Read a file before you write it. Run the tests when you can. When the change is complete, call commit once with a short message, then stop.";

/// The version every scope-first request carries.
const MCP_VERSION: &str = "acan-mcp/1";

/// The bounce that makes a difference residue. The third bounce of the same difference stops the
/// claim, so the worker never starts a fourth build on it.
const REPEAT_LIMIT: u32 = 3;

/// How a worker is run. The token is never printed, so `Debug` leaves it out.
#[derive(Clone)]
pub struct WorkerConfig {
    /// The origin's base URL, such as `http://127.0.0.1:7311`.
    pub server: String,
    /// The agent's bearer token.
    pub token: String,
    /// The repository the origin serves. Its tracked files ground each plan, so a plan can only
    /// declare paths the repository holds, or new files in its existing directories.
    pub repo: PathBuf,
    /// The command `run_tests` runs in the worktree. `None` means no tests are configured.
    pub test_command: Option<String>,
    /// The most model calls the edit loop may make before the worker gives up.
    pub max_steps: usize,
    /// How long one wait may last: for work to appear, or build clearance.
    pub wait: Duration,
    /// How long the worker waits for the gate's receipt after it reports a build. A receipt that
    /// does not come in time leaves the build reported, and the step log says so.
    pub receipt_wait: Duration,
    /// How often the worker renews its lease. Keep it well under the origin's 60 second lease.
    pub heartbeat: Duration,
    /// How often a wait polls the origin.
    pub poll: Duration,
}

impl fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("server", &self.server)
            .field("token", &"<redacted>")
            .field("repo", &self.repo)
            .field("test_command", &self.test_command)
            .field("max_steps", &self.max_steps)
            .field("wait", &self.wait)
            .field("receipt_wait", &self.receipt_wait)
            .field("heartbeat", &self.heartbeat)
            .field("poll", &self.poll)
            .finish()
    }
}

/// Where a worker run ended.
///
/// Every exit except `Built` and `Lost` releases the claim first. If that release cannot reach
/// the origin, the lease ends on its own once renewals stop, after the origin's 60 second lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// The build was reported. The origin's gate runs next.
    Built {
        /// The task.
        task: String,
        /// The generation the build was reported under.
        generation: u64,
        /// The commit that was reported.
        head: String,
    },
    /// No open task could be claimed before the wait ended.
    NoTask,
    /// The task did not clear to build before the wait ended. The worker released it.
    NotCleared {
        /// The task.
        task: String,
        /// The stage the task was last in.
        stage: String,
    },
    /// The origin stopped renewing the claim, so it is no longer this worker's. The worker
    /// stopped its work at once and did not release a claim the origin had already ended.
    Lost {
        /// The task.
        task: String,
        /// Why the origin refused the renewal, or why the claim is no longer held.
        reason: String,
    },
    /// The edit loop ended without a commit. The worker released the task.
    NoCommit {
        /// The task.
        task: String,
        /// Why the loop ended.
        reason: String,
    },
    /// A Narrow settlement took files the task needs, and the task cannot carry on without
    /// them. The worker released the task.
    Narrowed {
        /// The task.
        task: String,
        /// Why the work cannot go on without the files it gave up.
        reason: String,
    },
    /// The gate bounced the build, and the repair loop cannot take another round: the same
    /// difference bounced the third time, or the core stopped the item. The work is residue for a
    /// human. The worker released the task.
    Residue {
        /// The task.
        task: String,
        /// The difference the loop stopped on, in the core's words.
        reason: String,
    },
    /// A call was refused or a dependency failed.
    Failed {
        /// What failed.
        reason: String,
    },
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Built {
                task,
                generation,
                head,
            } => write!(
                f,
                "built {task} at generation {generation}; reported {head}"
            ),
            Self::NoTask => f.write_str("no task could be claimed"),
            Self::NotCleared { task, stage } => {
                write!(f, "{task} did not clear to build; it was {stage}")
            }
            Self::Lost { task, reason } => write!(f, "lost {task}: {reason}"),
            Self::NoCommit { task, reason } => write!(f, "{task} was not committed: {reason}"),
            Self::Narrowed { task, reason } => {
                write!(
                    f,
                    "{task} could not carry on after a narrow settlement: {reason}"
                )
            }
            Self::Residue { task, reason } => write!(f, "{task} is residue: {reason}"),
            Self::Failed { reason } => write!(f, "failed: {reason}"),
        }
    }
}

/// One worker run: where it ended, and the step log it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Where the run ended.
    pub exit: Exit,
    /// Every step, in order, as the log printed it.
    pub steps: Vec<String>,
}

/// Runs one worker to an exit. It never panics on a refused call or a failed model step; each of
/// those ends the run with an [`Exit`] that says what happened.
pub async fn run<C: ModelClient>(config: &WorkerConfig, model: &C) -> Outcome {
    let mut steps = Vec::new();
    let exit = match drive(config, model, &mut steps).await {
        Ok(exit) => exit,
        Err(err) => Exit::Failed {
            reason: format!("{err:#}"),
        },
    };
    tracing::info!(exit = %exit, "the worker finished");
    Outcome { exit, steps }
}

/// What the model plans for a task. Its files are the footprint the worker declares.
#[derive(Debug, Deserialize, JsonSchema)]
struct Plan {
    /// One or two sentences on the change.
    summary: String,
    /// The existing files the change edits, each copied exactly from the repository file list.
    files: Vec<String>,
    /// The files the change creates. List a file here only when it does not exist yet.
    #[serde(default)]
    new_files: Vec<String>,
}

/// A plan the repository accepted: the summary, the footprint, and the fixes the check made.
#[derive(Debug)]
struct Planned {
    summary: String,
    footprint: Vec<String>,
    fixes: Vec<String>,
}

/// A claimed task and the generation the origin granted. `files` is the footprint as declared,
/// already in the origin's normalized form. `gave_up` is every place a Narrow settlement took from
/// this claim, in the order taken. No later plan of the claim may declare one of them again.
/// `repair_rounds` is the confirmed scope's budget: how many bounced builds may be repaired.
/// `rulings` are the rulings the origin held for the task's machines when the claim was made. They
/// are text for the build brief, never a verdict on a build. `scope` is the confirmed scope the
/// claim was planned against; a re-plan after a narrow settlement plans against the same scope.
#[derive(Debug, Clone)]
struct Claim {
    task: String,
    generation: u64,
    files: Vec<String>,
    gave_up: Vec<Given>,
    repair_rounds: u32,
    rulings: Vec<RulingSummary>,
    scope: Scope,
}

/// What one attempt at a task produced.
enum Attempt {
    // Boxed: a claim carries the confirmed scope, which is far larger than the other variants.
    Claimed(Box<Claim>),
    /// The task cannot be claimed yet, such as before the human confirms its scope. The next
    /// poll tries it again.
    Pending(String),
    /// The task was refused or could not be planned. It is not tried again in this run.
    Skipped(String),
}

/// A place a Narrow settlement took from the claim, and why. The place is the clash's locus.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Given {
    /// The place the claim gave up.
    at: Locus,
    /// The clash, the task that keeps the place, and the settlement, for the re-plan and the log.
    why: String,
}

/// Where the origin stands on clearing a claim to build.
#[derive(Debug, PartialEq, Eq)]
enum Clearance {
    /// The origin has cleared the claim to build, and this worker still holds that generation.
    Cleared,
    /// The claim is held but not building yet. The text is the stage it is in.
    Waiting(String),
    /// The claim is no longer this worker's at this generation. The text says why.
    Lost(String),
    /// A Narrow settlement sent the claim back to declaring and took these places. The worker
    /// must declare the files it keeps.
    Narrowed(Vec<Given>),
}

/// The end of the edit loop.
enum Edit {
    Committed(String),
    Stopped(String),
    NotCleared(String),
    Lost(String),
    Narrowed(String),
}

/// What one tool call produced.
enum ToolStep {
    Answer(String),
    Committed(String),
    NotCleared(String),
    Lost(String),
    Narrowed(Vec<Given>),
}

/// How many calls one tool got in an edit, and how many of them were answered with a failure.
#[derive(Debug, Default)]
struct Count {
    calls: usize,
    failed: usize,
}

/// The tool calls one edit made, by name.
#[derive(Debug, Default)]
struct Tally(BTreeMap<String, Count>);

impl Tally {
    fn record(&mut self, name: &str, failed: bool) {
        let count = self.0.entry(name.to_owned()).or_default();
        count.calls += 1;
        if failed {
            count.failed += 1;
        }
    }

    /// The counts in name order, such as `read_file 9, run_tests 4 (4 failed)`.
    fn summary(&self) -> String {
        if self.0.is_empty() {
            return "no tool calls".to_owned();
        }
        self.0
            .iter()
            .map(|(name, count)| {
                if count.failed == 0 {
                    format!("{name} {}", count.calls)
                } else {
                    format!("{name} {} ({} failed)", count.calls, count.failed)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

async fn drive<C: ModelClient>(
    config: &WorkerConfig,
    model: &C,
    steps: &mut Vec<String>,
) -> anyhow::Result<Exit> {
    let mcp = McpClient::new(&config.server, config.token.clone());
    mcp.connect().await?;
    log(steps, format!("connected to {} over MCP", config.server));
    let Some(claim) = claim_next(config, model, &mcp, steps).await? else {
        return Ok(Exit::NoTask);
    };
    log(
        steps,
        format!("claimed {} at generation {}", claim.task, claim.generation),
    );
    // The lease only survives while renewals arrive, so the heartbeat runs beside the work. When
    // the origin refuses a renewal, the claim is no longer ours and the work stops at once.
    let mut renewals = tokio::spawn(heartbeat(mcp.clone(), claim.clone(), config.heartbeat));
    let finished = tokio::select! {
        result = work(config, model, &mcp, claim.clone(), steps) => result,
        refused = &mut renewals => {
            let reason = refused.context("the heartbeat task stopped unexpectedly")?;
            return Ok(Exit::Lost { task: claim.task.clone(), reason });
        }
    };
    renewals.abort();
    match finished {
        Ok(exit @ (Exit::Built { .. } | Exit::Lost { .. })) => Ok(exit),
        Ok(exit) => {
            release(&mcp, &claim, &exit.to_string(), steps).await;
            Ok(exit)
        }
        Err(err) => {
            release(&mcp, &claim, &format!("{err:#}"), steps).await;
            Err(err)
        }
    }
}

/// Renews the lease until the origin refuses a renewal, and returns the origin's words for the
/// refusal. A renewal that does not reach the origin is tried again on the next beat.
async fn heartbeat(mcp: McpClient, claim: Claim, every: Duration) -> String {
    loop {
        tokio::time::sleep(every).await;
        let renewal = mcp
            .call(
                "heartbeat_task",
                json!({ "task": claim.task, "generation": claim.generation }),
            )
            .await;
        match renewal {
            Ok(Reply::Done(_)) => {}
            Ok(Reply::Refused(reason)) => {
                tracing::warn!(task = %claim.task, %reason, "the origin refused a heartbeat");
                return reason;
            }
            Err(err) => {
                tracing::warn!(task = %claim.task, error = ?err, "a heartbeat did not reach the origin");
            }
        }
    }
}

/// Tries open tasks in the origin's order until one is claimed or the wait ends. A task that is
/// refused or cannot be planned is not tried again in this run. A task whose scope is not yet
/// confirmed is tried again on the next poll.
async fn claim_next<C: ModelClient>(
    config: &WorkerConfig,
    model: &C,
    mcp: &McpClient,
    steps: &mut Vec<String>,
) -> anyhow::Result<Option<Claim>> {
    let deadline = Instant::now() + config.wait;
    let mut skipped = HashSet::new();
    let mut waiting = HashSet::new();
    loop {
        for row in open_tasks(mcp).await? {
            let task = string_field(&row, "task");
            if skipped.contains(&task) {
                continue;
            }
            match try_claim(config, model, mcp, &row, steps).await? {
                Attempt::Claimed(claim) => return Ok(Some(*claim)),
                Attempt::Pending(reason) => {
                    if waiting.insert(task) {
                        log(steps, format!("waiting on {reason}"));
                    }
                }
                Attempt::Skipped(reason) => {
                    log(steps, format!("skipped {reason}"));
                    skipped.insert(task);
                }
            }
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(config.poll).await;
    }
}

async fn try_claim<C: ModelClient>(
    config: &WorkerConfig,
    model: &C,
    mcp: &McpClient,
    row: &Value,
    steps: &mut Vec<String>,
) -> anyhow::Result<Attempt> {
    // The readback is the scope-first lane's: it restates the confirmed scope at the revision and
    // hash the worker read, with predictions. The origin acknowledges it, and only that
    // acknowledgement lets the claim go through. Every attempt plans afresh, so the files the claim
    // declares are the files the readback describes.
    let task = string_field(row, "task");
    let confirmed = match get_scope(mcp, &task).await? {
        Ok(confirmed) => confirmed,
        Err(reason) => {
            return Ok(Attempt::Pending(format!(
                "{task}: its scope is not confirmed yet: {reason}"
            )));
        }
    };
    // The rulings in force on the task's machines are read with the scope, before the plan. A
    // ruling is text the build brief shows the model; no ruling passes or fails anything here.
    let rulings = match get_rulings(mcp, &task, row).await? {
        Ok(rulings) => rulings,
        Err(reason) => {
            return Ok(Attempt::Skipped(format!(
                "{task}: rulings refused: {reason}"
            )));
        }
    };
    log(
        steps,
        format!(
            "read {} ruling(s) for {task} at ledger event {}",
            rulings.rulings.len(),
            rulings.ledger_head.event_seq
        ),
    );
    let repo = RepoFiles::load(&config.repo).await?;
    let planned = match plan_work(model, &repo, row, &confirmed.scope, &[], steps).await {
        Ok(planned) => planned,
        Err(reasons) => {
            return Ok(Attempt::Skipped(format!(
                "{task}: plan refused after {PLAN_ATTEMPTS} tries: {reasons}"
            )));
        }
    };
    for fix in &planned.fixes {
        log(steps, format!("plan fix on {task}: {fix}"));
    }
    let repair_rounds = confirmed.budget.repair_rounds.get();
    let predictions = scope_predictions(&confirmed.scope);
    let withheld = scope_items(&confirmed.scope) - predictions.len();
    if withheld > 0 {
        log(
            steps,
            format!(
                "{task}: {withheld} scope item(s) past the readback's limit of {MAX_PREDICTIONS} predictions"
            ),
        );
    }
    let posted = mcp
        .call(
            "post_scoped_readback",
            json!({
                "version": MCP_VERSION,
                "task": task,
                "scope_rev": confirmed.scope.rev,
                "scope_hash": confirmed.scope_hash,
                "summary": planned.summary,
                "predictions": predictions,
            }),
        )
        .await?;
    if let Reply::Refused(reason) = posted {
        return Ok(Attempt::Skipped(format!(
            "{task}: readback refused: {reason}"
        )));
    }
    log(
        steps,
        format!(
            "read back {task} at scope rev {} with {} prediction(s): {}",
            confirmed.scope.rev.0,
            predictions.len(),
            planned.summary
        ),
    );
    match mcp.call("claim_task", json!({ "task": task })).await? {
        Reply::Refused(reason) => Ok(Attempt::Skipped(format!("{task}: claim refused: {reason}"))),
        Reply::Done(value) => {
            let generation = value["generation"]
                .as_u64()
                .context("claim_task named no generation")?;
            Ok(Attempt::Claimed(Box::new(Claim {
                task,
                generation,
                files: planned.footprint,
                gave_up: Vec::new(),
                repair_rounds,
                rulings: rulings.rulings,
                scope: confirmed.scope,
            })))
        }
    }
}

/// The confirmed scope of `task`, or the refusal the origin gave. A refusal is an answer the caller
/// acts on, such as a scope that is not confirmed yet.
async fn get_scope(
    mcp: &McpClient,
    task: &str,
) -> anyhow::Result<Result<GetScopeResponse, String>> {
    match mcp
        .call("get_scope", json!({ "version": MCP_VERSION, "task": task }))
        .await?
    {
        Reply::Done(value) => Ok(Ok(serde_json::from_value(value).with_context(|| {
            format!("get_scope answered a scope the worker does not read for {task}")
        })?)),
        Reply::Refused(reason) => Ok(Err(reason)),
    }
}

/// The rulings the origin holds for the task's scope machines, or the refusal it gave. The
/// machines are the row's scope list; the origin answers only the rulings that apply to them.
async fn get_rulings(
    mcp: &McpClient,
    task: &str,
    row: &Value,
) -> anyhow::Result<Result<GetRulingsResponse, String>> {
    let machines: Vec<Value> = row["scope"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|machine| machine.is_string())
        .cloned()
        .collect();
    match mcp
        .call(
            "get_rulings",
            json!({ "version": MCP_VERSION, "task": task, "machine_ids": machines }),
        )
        .await?
    {
        Reply::Done(value) => Ok(Ok(serde_json::from_value(value).with_context(|| {
            format!("get_rulings answered rulings the worker does not read for {task}")
        })?)),
        Reply::Refused(reason) => Ok(Err(reason)),
    }
}

/// The predictions a readback posts, one for each scope item. They come from the confirmed scope
/// alone, never from a model: a may-change item predicts the change the scope declares, and a
/// must-keep rule predicts no change. The core scores them against what the gate observes. A
/// prediction is never evidence, and a right one never passes anything.
fn scope_predictions(scope: &Scope) -> Vec<Prediction> {
    let may = scope.body.may.iter().map(|item| Prediction {
        selector: item.selector.clone(),
        predicted: Predicted::Change {
            change: item.change,
        },
    });
    let keep = scope.body.must_keep.iter().map(|rule| Prediction {
        selector: rule.selector.clone(),
        predicted: Predicted::Unchanged {},
    });
    may.chain(keep).take(MAX_PREDICTIONS).collect()
}

/// How many scope items a readback could carry a prediction for.
fn scope_items(scope: &Scope) -> usize {
    scope.body.may.len() + scope.body.must_keep.len()
}

/// Declares, waits for build clearance, and runs the edit loop. Ends with a commit reported, or
/// with an exit the caller settles: a stop that releases the claim, or a loss the origin already
/// made.
async fn work<C: ModelClient>(
    config: &WorkerConfig,
    model: &C,
    mcp: &McpClient,
    mut claim: Claim,
    steps: &mut Vec<String>,
) -> anyhow::Result<Exit> {
    let declared = mcp
        .call(
            "declare_changes",
            json!({
                "task": claim.task,
                "generation": claim.generation,
                "files": claim.files,
            }),
        )
        .await?;
    let view = match declared {
        Reply::Done(view) => view,
        Reply::Refused(reason) => {
            return Ok(Exit::Failed {
                reason: format!("declare_changes refused on {}: {reason}", claim.task),
            });
        }
    };
    let clashes = view["clashes_found"].as_array().map_or(0, Vec::len);
    log(
        steps,
        format!(
            "declared {} file(s) on {}; {clashes} new clash(es)",
            claim.files.len(),
            claim.task
        ),
    );
    // One round is a wait for build clearance, one edit, and one report. The gate's receipt decides
    // whether the round ends the work or starts the next one with the core's reproducer.
    let mut bounced: Vec<Reproducer> = Vec::new();
    let mut repeats: BTreeMap<String, u32> = BTreeMap::new();
    let mut round: u32 = 1;
    loop {
        if let Err(exit) = clear_to_build(config, model, mcp, &mut claim, steps).await? {
            return Ok(exit);
        }
        let mut tools = match open_worktree(config, mcp, &claim, steps).await? {
            Ok(tools) => tools,
            Err(reason) => return Ok(Exit::Failed { reason }),
        };
        let brief = build_brief(&claim, round, &bounced);
        let head = match edit(config, model, mcp, &mut claim, &mut tools, brief, steps).await? {
            Edit::Committed(head) => head,
            Edit::Stopped(reason) => {
                return Ok(Exit::NoCommit {
                    task: claim.task.clone(),
                    reason,
                });
            }
            Edit::NotCleared(stage) => {
                return Ok(Exit::NotCleared {
                    task: claim.task.clone(),
                    stage,
                });
            }
            Edit::Lost(reason) => {
                return Ok(Exit::Lost {
                    task: claim.task.clone(),
                    reason,
                });
            }
            Edit::Narrowed(reason) => {
                return Ok(Exit::Narrowed {
                    task: claim.task.clone(),
                    reason,
                });
            }
        };
        // The receipt that was latest before this report is not this build's, so the wait below
        // takes only a receipt the report produced.
        let before = latest_receipt(mcp, &claim.task)
            .await?
            .map(|found| found.receipt_hash);
        if let Err(reason) = report_built(mcp, &claim, &head, steps).await? {
            return Ok(Exit::Failed { reason });
        }
        let Some(found) = await_receipt(config, mcp, &claim.task, before.as_ref(), steps).await?
        else {
            return Ok(Exit::Built {
                task: claim.task.clone(),
                generation: claim.generation,
                head,
            });
        };
        match repair(&found.receipt, &mut repeats) {
            Repair::Passed => {
                return Ok(Exit::Built {
                    task: claim.task.clone(),
                    generation: claim.generation,
                    head,
                });
            }
            Repair::Bounced(next) => {
                // The confirmed budget counts the repair rounds after the first build. A bounce
                // after the last allowed round has no round left to repair in.
                if round > claim.repair_rounds {
                    let reason = format!(
                        "build {round} bounced, and the confirmed budget of {} repair round(s) is spent",
                        claim.repair_rounds
                    );
                    log(steps, format!("{} went to residue: {reason}", claim.task));
                    contest_residue(mcp, &claim, &next, &reason, steps).await;
                    return Ok(Exit::Residue {
                        task: claim.task.clone(),
                        reason,
                    });
                }
                log(
                    steps,
                    format!(
                        "the gate bounced build {round} of {}: {} item(s) to repair; building again",
                        claim.task,
                        next.len()
                    ),
                );
                bounced = next;
                round += 1;
            }
            Repair::Residue { reason, given_up } => {
                log(steps, format!("{} went to residue: {reason}", claim.task));
                contest_residue(mcp, &claim, &given_up, &reason, steps).await;
                return Ok(Exit::Residue {
                    task: claim.task.clone(),
                    reason,
                });
            }
        }
    }
}

/// Files one contest for each item the worker gave up on that has an episode and a contest kind.
/// A contest asks the human. It never widens the scope and never passes the item. The residue
/// stands whatever the contest's fate: a refused contest, one that does not reach the origin, or
/// one answered in a shape the worker does not read is logged, and the run still ends as residue.
async fn contest_residue(
    mcp: &McpClient,
    claim: &Claim,
    given_up: &[Reproducer],
    reason: &str,
    steps: &mut Vec<String>,
) {
    for reproducer in given_up {
        let (Some(episode), Some(kind)) = (reproducer.episode, contest_kind(reproducer.class))
        else {
            continue;
        };
        let reply = mcp
            .call(
                "file_contest",
                json!({
                    "version": MCP_VERSION,
                    "task": claim.task,
                    "episode_id": episode,
                    "kind": kind,
                    "reproducer": reproducer.probe,
                    "rationale": reason,
                }),
            )
            .await;
        match reply {
            Ok(Reply::Done(value)) => match serde_json::from_value::<ContestResponse>(value) {
                Ok(filed) => log(
                    steps,
                    format!(
                        "filed contest {} on {} against episode {episode}",
                        filed.contest_id, claim.task
                    ),
                ),
                Err(err) => log(
                    steps,
                    format!(
                        "contest on {} answered in a shape the worker does not read: {err}",
                        claim.task
                    ),
                ),
            },
            Ok(Reply::Refused(refusal)) => log(
                steps,
                format!("contest on {} refused: {refusal}", claim.task),
            ),
            Err(err) => log(
                steps,
                format!(
                    "contest on {} did not reach the origin: {err:#}",
                    claim.task
                ),
            ),
        }
    }
}

/// The kind of contest an item the worker gave up on files. A ruling the item violates is a spec
/// conflict. A difference no scope item allows, and an obligation the build could not meet inside
/// the scope, are blocked work. Other classes are not contested by the worker.
fn contest_kind(class: ItemClass) -> Option<ContestKind> {
    match class {
        ItemClass::ViolatesRuling => Some(ContestKind::SpecConflict),
        ItemClass::OutOfScope | ItemClass::ObligationFailed => Some(ContestKind::Blocked),
        ItemClass::Preserved
        | ItemClass::ObligationFulfilled
        | ItemClass::AuthorizedButUnspecified
        | ItemClass::UnstableBase
        | ItemClass::Incomparable
        | ItemClass::Error => None,
    }
}

/// Waits for build clearance, and re-plans when a Narrow settlement takes places the claim needs.
/// The error is the exit the work ends with: the claim is not cleared, or it is lost or narrowed
/// past repair.
async fn clear_to_build<C: ModelClient>(
    config: &WorkerConfig,
    model: &C,
    mcp: &McpClient,
    claim: &mut Claim,
    steps: &mut Vec<String>,
) -> anyhow::Result<Result<(), Exit>> {
    loop {
        match wait_for_build(config, mcp, claim, steps).await? {
            Clearance::Cleared => return Ok(Ok(())),
            Clearance::Waiting(stage) => {
                return Ok(Err(Exit::NotCleared {
                    task: claim.task.clone(),
                    stage,
                }));
            }
            Clearance::Lost(reason) => {
                return Ok(Err(Exit::Lost {
                    task: claim.task.clone(),
                    reason,
                }));
            }
            Clearance::Narrowed(given) => {
                if let Err(reason) = redeclare(config, model, mcp, claim, &given, steps).await? {
                    return Ok(Err(Exit::Narrowed {
                        task: claim.task.clone(),
                        reason,
                    }));
                }
            }
        }
    }
}

/// The tools for one round, from the origin's view of the task's worktree: the worktree, the files
/// the task declares, and the protected-head snapshot when the view names one.
async fn open_worktree(
    config: &WorkerConfig,
    mcp: &McpClient,
    claim: &Claim,
    steps: &mut Vec<String>,
) -> anyhow::Result<Result<Worktree, String>> {
    let view = match mcp
        .call("get_worktree", json!({ "task": claim.task }))
        .await?
    {
        Reply::Done(view) => view,
        Reply::Refused(reason) => {
            return Ok(Err(format!(
                "get_worktree refused on {}: {reason}",
                claim.task
            )));
        }
    };
    Ok(Ok(tools_for(config, claim, &view, steps)))
}

/// Builds the tools from a worktree view. The protected-head snapshot is passed on when the view
/// names one. Without it `run_tests` refuses, so the worker never runs tests against protected
/// paths it cannot pin.
fn tools_for(
    config: &WorkerConfig,
    claim: &Claim,
    view: &Value,
    steps: &mut Vec<String>,
) -> Worktree {
    let path = string_field(view, "path");
    log(steps, format!("worktree for {}: {path}", claim.task));
    let tools = Worktree::new(
        PathBuf::from(path),
        &claim.files,
        config.test_command.clone(),
    );
    match view["snapshot"].as_str() {
        Some(snapshot) => {
            log(
                steps,
                format!("protected-head snapshot for {}: {snapshot}", claim.task),
            );
            tools.with_snapshot(snapshot)
        }
        None => {
            log(
                steps,
                format!(
                    "no protected-head snapshot for {}; run_tests will refuse",
                    claim.task
                ),
            );
            tools
        }
    }
}

/// The brief for one build round. Round one names the task and its files. A later round names the
/// bounce it answers and each reproducer the gate gave: the item, its class, the probe that
/// checked it, and the obligations that failed, all in the core's words.
fn build_brief(claim: &Claim, round: u32, bounced: &[Reproducer]) -> String {
    let files = claim.files.join(", ");
    let rulings = rulings_section(&claim.rulings);
    if bounced.is_empty() {
        return format!("Task {}. Declared files: {files}.\n\n{rulings}", claim.task);
    }
    let reproducers: Vec<String> = bounced
        .iter()
        .map(|reproducer| {
            format!(
                "- item {} ({}) on probe {}: {}",
                reproducer.item,
                class_name(reproducer.class),
                reproducer.probe,
                reproducer.failed.join("; ")
            )
        })
        .collect();
    format!(
        "Build {round} of {}. Declared files: {files}.\n\nThe gate bounced build {}. Repair only what it reports, in the files you declared:\n{}\n\n{rulings}",
        claim.task,
        round - 1,
        reproducers.join("\n")
    )
}

/// The rulings in force, one line each, labelled as the origin's text. Superseded rulings are
/// left out. A ruling the gate checks is named as such; one it does not is a note. Neither one is
/// a verdict on this build.
fn rulings_section(rulings: &[RulingSummary]) -> String {
    let lines: Vec<String> = rulings
        .iter()
        .filter(|ruling| ruling.state == RulingTagState::Active)
        .map(ruling_line)
        .collect();
    if lines.is_empty() {
        return "No rulings are in force on this task's machines.".to_owned();
    }
    format!(
        "Rulings in force on this task's machines (the origin's text; the gate decides what passes):\n{}",
        lines.join("\n")
    )
}

/// One ruling as a brief line: its label, its lineage, and whether the gate checks it.
fn ruling_line(ruling: &RulingSummary) -> String {
    let gate = match &ruling.enforcement {
        Enforcement::Enforced { .. } => "the gate checks it".to_owned(),
        Enforcement::NotEnforced { reason, .. } => match reason {
            NotEnforcedReason::Unexpressible { detail } => {
                format!("a note the gate does not check: {detail}")
            }
            NotEnforcedReason::Deferred {} => {
                "a note the gate does not check: the decision is deferred".to_owned()
            }
        },
    };
    format!("- {} ({}): {gate}", ruling.label, ruling.ruling_id)
}

/// The core's reproducer for one bounced item: what it checks, the probe that showed the
/// difference, the obligations that failed in the core's words, and the episode the core filed
/// the difference under, when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reproducer {
    /// The receipt item's id.
    item: String,
    /// The class the core gave the item.
    class: ItemClass,
    /// The probe that checked the item.
    probe: ProbeHash,
    /// The episode of the difference, which a contest names.
    episode: Option<EpisodeId>,
    /// Each failed obligation, with the core's detail.
    failed: Vec<String>,
}

impl Reproducer {
    fn of(item: &ReceiptItem) -> Self {
        let failed: Vec<String> = item
            .evaluation
            .obligations
            .iter()
            .filter_map(|check| match &check.result {
                CheckResult::Fail { detail } => {
                    Some(format!("{} failed: {detail}", check.obligation_id))
                }
                CheckResult::Pass {} | CheckResult::Unknown { .. } => None,
            })
            .collect();
        Self {
            item: item.item_id.to_string(),
            class: item.class,
            probe: item.probe_hash,
            episode: item.episode_id,
            failed: if failed.is_empty() {
                vec![format!(
                    "no obligation failed; the core classed the difference as {}",
                    class_name(item.class)
                )]
            } else {
                failed
            },
        }
    }
}

/// What a receipt asks of the next round.
#[derive(Debug, PartialEq, Eq)]
enum Repair {
    /// Every item passed or was preserved, so the build stands.
    Passed,
    /// Items to repair, each with its reproducer. The next round builds again with them.
    Bounced(Vec<Reproducer>),
    /// The loop cannot take another round. `reason` is the difference it stopped on. `given_up`
    /// holds the items the worker repaired without success, which it contests when they have an
    /// episode. The core's own stops give up nothing: the core already decided them.
    Residue {
        reason: String,
        given_up: Vec<Reproducer>,
    },
}

/// Reads a receipt into the next round. A bounce is a difference the worker can repair. A residue
/// is one it cannot: a difference the core stopped, one that needs a human ruling, or an item that
/// could not be run. The same difference bounced `REPEAT_LIMIT` times is residue, so the loop
/// never runs a fourth build on it.
fn repair(receipt: &Receipt, repeats: &mut BTreeMap<String, u32>) -> Repair {
    let mut bounced = Vec::new();
    for item in &receipt.items {
        if matches!(item.state, ItemState::Stopped | ItemState::Residue) {
            return Repair::Residue {
                reason: format!(
                    "{} is {:?}: the core stopped it, so a human decides",
                    item.item_id, item.state
                ),
                given_up: Vec::new(),
            };
        }
        match item.class {
            ItemClass::Preserved | ItemClass::ObligationFulfilled => {}
            ItemClass::ObligationFailed | ItemClass::OutOfScope | ItemClass::ViolatesRuling => {
                let key = format!(
                    "{}/{}/{}",
                    item.item_id,
                    class_name(item.class),
                    item.probe_hash
                );
                let seen = repeats.entry(key).or_insert(0);
                *seen += 1;
                if *seen >= REPEAT_LIMIT {
                    return Repair::Residue {
                        reason: format!(
                            "{} bounced {} times on the same difference (probe {}), and the third repeat is residue",
                            item.item_id, *seen, item.probe_hash
                        ),
                        given_up: vec![Reproducer::of(item)],
                    };
                }
                bounced.push(Reproducer::of(item));
            }
            ItemClass::AuthorizedButUnspecified
            | ItemClass::UnstableBase
            | ItemClass::Incomparable
            | ItemClass::Error => {
                return Repair::Residue {
                    reason: format!(
                        "{} is {}, which no build repairs",
                        item.item_id,
                        class_name(item.class)
                    ),
                    given_up: Vec::new(),
                };
            }
        }
    }
    if bounced.is_empty() {
        Repair::Passed
    } else {
        Repair::Bounced(bounced)
    }
}

/// The wire spelling of a receipt class, such as `obligation_failed`.
fn class_name(class: ItemClass) -> String {
    serde_json::to_value(class)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{class:?}"))
}

/// The latest receipt of the task's gate runs, or `None` when there is none yet.
async fn latest_receipt(mcp: &McpClient, task: &str) -> anyhow::Result<Option<GetReceiptResponse>> {
    match mcp
        .call(
            "get_receipt",
            json!({ "version": MCP_VERSION, "task": task }),
        )
        .await?
    {
        Reply::Done(value) => Ok(Some(serde_json::from_value(value).with_context(|| {
            format!("get_receipt answered a receipt the worker does not read for {task}")
        })?)),
        Reply::Refused(_) => Ok(None),
    }
}

/// Polls for a receipt whose hash differs from `before`, the receipt that was latest before the
/// report. Returns `None` when the wait ends first, which leaves the build reported.
async fn await_receipt(
    config: &WorkerConfig,
    mcp: &McpClient,
    task: &str,
    before: Option<&ReceiptHash>,
    steps: &mut Vec<String>,
) -> anyhow::Result<Option<GetReceiptResponse>> {
    let deadline = Instant::now() + config.receipt_wait;
    loop {
        if let Some(found) = latest_receipt(mcp, task).await?
            && Some(&found.receipt_hash) != before
        {
            log(
                steps,
                format!("the gate answered {task}: receipt {}", found.receipt_hash),
            );
            return Ok(Some(found));
        }
        if Instant::now() >= deadline {
            log(
                steps,
                format!(
                    "no receipt for {task} within {:?}; the build stays reported while the gate runs",
                    config.receipt_wait
                ),
            );
            return Ok(None);
        }
        tokio::time::sleep(config.poll).await;
    }
}

/// Reads the task's own view and says whether this worker may write. The origin's `you_hold` and
/// `generation` are the fence: a write is allowed only while this worker holds the generation it
/// was granted, and the task is building.
fn clearance(detail: &Value, claim: &Claim) -> anyhow::Result<Clearance> {
    let state = string_field(detail, "state");
    let generation = detail["generation"].as_u64();
    let holds = detail["you_hold"].as_bool() == Some(true) && generation == Some(claim.generation);
    if state != "claimed" || !holds {
        let now = match generation {
            Some(generation) => generation.to_string(),
            None => "none".to_owned(),
        };
        return Ok(Clearance::Lost(format!(
            "{} is {state} at generation {now}; this worker holds generation {}",
            claim.task, claim.generation
        )));
    }
    let stage = string_field(detail, "stage");
    if stage == "building" {
        return Ok(Clearance::Cleared);
    }
    // A Narrow settlement sends the claim back to declaring. Its clash names the places it gave up.
    if stage == "declaring" {
        let given = narrowed(detail, claim)?;
        if !given.is_empty() {
            return Ok(Clearance::Narrowed(given));
        }
    }
    Ok(Clearance::Waiting(stage))
}

/// The places a Narrow settlement took from `claim`: every settled clash that names the claim's
/// task as the one that narrows, at a place the claim still declares. A clash at a place that is
/// not a file is kept, so the re-plan can name it as the reason the work cannot go on.
///
/// # Errors
///
/// Returns the reason when a clash's state or place does not read as the origin's types.
fn narrowed(detail: &Value, claim: &Claim) -> anyhow::Result<Vec<Given>> {
    let mut given = Vec::new();
    for clash in detail["clashes"].as_array().into_iter().flatten() {
        let id = clash["clash"].as_str().unwrap_or_default();
        let state: ClashState = serde_json::from_value(clash["state"].clone())
            .with_context(|| format!("clash {id} has a state the worker does not read"))?;
        let ClashState::Settled {
            choice: Settlement::Narrow { task },
            ..
        } = state
        else {
            continue;
        };
        if task.to_string() != claim.task {
            continue;
        }
        let at: Locus = serde_json::from_value(clash["at"].clone())
            .with_context(|| format!("clash {id} has a place the worker does not read"))?;
        if file_of(&at).is_some_and(|file| !claim.files.contains(&file)) {
            continue;
        }
        let other = if clash["a"].as_str() == Some(claim.task.as_str()) {
            clash["b"].as_str()
        } else {
            clash["a"].as_str()
        };
        let why = format!(
            "clash {id} with {} was settled as narrow, so {} gives up its place",
            other.unwrap_or("another task"),
            claim.task
        );
        given.push(Given { at, why });
    }
    Ok(given)
}

/// The file a place lies in, for a file or a symbol. A machine or a belt lies in no file.
fn file_of(at: &Locus) -> Option<String> {
    match at {
        Locus::File { file } | Locus::Symbol { file, .. } => Some(file.to_string()),
        Locus::Machine { .. } | Locus::Belt { .. } => None,
    }
}

/// The distinct files the narrowing took. A place that is not a file cannot be re-planned around,
/// so the error names the clash that took it.
fn given_files(given: &[Given]) -> Result<Vec<String>, String> {
    let mut files: Vec<String> = Vec::new();
    for taken in given {
        let file = file_of(&taken.at).ok_or_else(|| {
            format!(
                "{}: the place is not a file, so the work cannot be re-planned around it",
                taken.why
            )
        })?;
        if !files.contains(&file) {
            files.push(file);
        }
    }
    Ok(files)
}

/// Polls until the origin clears the claim to build, the claim is lost, or the wait ends.
async fn wait_for_build(
    config: &WorkerConfig,
    mcp: &McpClient,
    claim: &Claim,
    steps: &mut Vec<String>,
) -> anyhow::Result<Clearance> {
    let deadline = Instant::now() + config.wait;
    let mut last = String::new();
    loop {
        let detail = get_task(mcp, &claim.task).await?;
        let stage = match clearance(&detail, claim)? {
            Clearance::Waiting(stage) => stage,
            decided => return Ok(decided),
        };
        if stage != last {
            log(
                steps,
                format!(
                    "{} is {stage}; waiting for the origin to clear it to build",
                    claim.task
                ),
            );
            last = stage.clone();
        }
        if Instant::now() >= deadline {
            return Ok(Clearance::Waiting(stage));
        }
        tokio::time::sleep(config.poll).await;
    }
}

/// The tool-calling loop. The model edits through the local tools. A write or a commit waits
/// for build clearance first, so no change lands before the origin allows it.
async fn edit<C: ModelClient>(
    config: &WorkerConfig,
    model: &C,
    mcp: &McpClient,
    claim: &mut Claim,
    tools: &mut Worktree,
    brief: String,
    steps: &mut Vec<String>,
) -> anyhow::Result<Edit> {
    let mut messages = vec![Message::system(EDIT_PROMPT), Message::user(brief)];
    let mut specs = Worktree::specs();
    specs.push(get_task_spec());
    let mut tally = Tally::default();
    for _ in 0..config.max_steps {
        let request = ChatRequest {
            messages: messages.clone(),
            tools: specs.clone(),
            ..ChatRequest::default()
        };
        let response = model
            .chat(Role::Worker, request)
            .await
            .context("the edit step failed")?;
        messages.push(Message::Assistant {
            content: response.content.clone(),
            tool_calls: response.tool_calls.clone(),
        });
        if response.tool_calls.is_empty() {
            let reason = response
                .content
                .unwrap_or_else(|| "the model stopped without a commit".to_owned());
            return Ok(Edit::Stopped(reason));
        }
        for call in response.tool_calls {
            let name = call.function.name.clone();
            match run_tool(config, mcp, claim, tools, &call, steps).await? {
                ToolStep::Answer(text) => {
                    // A failing test run is an answer too, so it counts as a failed call.
                    let failed = text.starts_with("error:") || text.starts_with("fail:");
                    tally.record(&name, failed);
                    messages.push(Message::Tool {
                        tool_call_id: call.id,
                        content: text,
                    });
                }
                ToolStep::Committed(head) => return Ok(Edit::Committed(head)),
                ToolStep::NotCleared(stage) => return Ok(Edit::NotCleared(stage)),
                ToolStep::Lost(reason) => return Ok(Edit::Lost(reason)),
                ToolStep::Narrowed(given) => {
                    // The write or commit that met the narrowing did not run. The task keeps
                    // going on the files it still declares, so the model is told what changed.
                    tally.record(&name, true);
                    match narrow_in_edit(config, model, mcp, claim, tools, &given, steps).await? {
                        Ok(text) => messages.push(Message::Tool {
                            tool_call_id: call.id,
                            content: text,
                        }),
                        Err(reason) => return Ok(Edit::Narrowed(reason)),
                    }
                }
            }
        }
    }
    Ok(Edit::Stopped(format!(
        "the step limit ran out before a commit after {} model calls: {}",
        config.max_steps,
        tally.summary()
    )))
}

/// Gives up the files a narrowing took while the task was building or waiting to build. Restores
/// them, re-plans and declares the files that remain, and returns the answer for the call that met
/// the narrowing. The error is the reason the work cannot go on.
async fn narrow_in_edit<C: ModelClient>(
    config: &WorkerConfig,
    model: &C,
    mcp: &McpClient,
    claim: &mut Claim,
    tools: &mut Worktree,
    given: &[Given],
    steps: &mut Vec<String>,
) -> anyhow::Result<Result<String, String>> {
    let lost = match given_files(given) {
        Ok(lost) => lost,
        Err(reason) => return Ok(Err(reason)),
    };
    if let Err(reason) = tools.restore(&lost).await {
        return Ok(Err(format!(
            "could not restore {} after the narrowing: {reason}",
            lost.join(", ")
        )));
    }
    if let Err(reason) = redeclare(config, model, mcp, claim, given, steps).await? {
        return Ok(Err(reason));
    }
    tools.declare(&claim.files);
    Ok(Ok(format!(
        "The declaration changed. {} is no longer yours and was restored, so your writes to it are refused now. You may write only: {}. The write or commit that met the change did not run; continue with the files you may write.",
        lost.join(", "),
        claim.files.join(", ")
    )))
}

/// Re-plans a claim that a Narrow settlement cut, and declares the files it keeps. The re-plan may
/// not name a place the claim gave up, in this narrowing or an earlier one, and it is told which
/// places it lost and why. The error is the reason the work cannot go on: no plan keeps to the
/// places that remain, the plan keeps no file, or the origin refuses the declaration. Every such
/// reason releases the claim.
async fn redeclare<C: ModelClient>(
    config: &WorkerConfig,
    model: &C,
    mcp: &McpClient,
    claim: &mut Claim,
    given: &[Given],
    steps: &mut Vec<String>,
) -> anyhow::Result<Result<(), String>> {
    remember(&mut claim.gave_up, given);
    let lost = match given_files(&claim.gave_up) {
        Ok(lost) => lost,
        Err(reason) => return Ok(Err(reason)),
    };
    let row = get_task(mcp, &claim.task).await?;
    let repo = RepoFiles::load(&config.repo).await?;
    let started = Instant::now();
    let planned = match plan_work(model, &repo, &row, &claim.scope, &claim.gave_up, steps).await {
        Ok(planned) => planned,
        Err(reasons) => {
            return Ok(Err(format!(
                "no plan without {} was accepted: {reasons}",
                lost.join(", ")
            )));
        }
    };
    for fix in &planned.fixes {
        log(steps, format!("plan fix on {}: {fix}", claim.task));
    }
    if planned.footprint.is_empty() {
        return Ok(Err(format!(
            "the work cannot be done without {}: {}",
            lost.join(", "),
            planned.summary
        )));
    }
    let declared = mcp
        .call(
            "declare_changes",
            json!({
                "task": claim.task,
                "generation": claim.generation,
                "files": planned.footprint,
            }),
        )
        .await?;
    let view = match declared {
        Reply::Done(view) => view,
        Reply::Refused(reason) => {
            return Ok(Err(format!(
                "declare_changes refused on {}: {reason}",
                claim.task
            )));
        }
    };
    let clashes = view["clashes_found"].as_array().map_or(0, Vec::len);
    log(
        steps,
        format!(
            "narrowed {}: gave up {}; declared {} file(s) without them; {clashes} new clash(es); re-planned in {:.1}s",
            claim.task,
            lost.join(", "),
            planned.footprint.len(),
            started.elapsed().as_secs_f64()
        ),
    );
    claim.files = planned.footprint;
    Ok(Ok(()))
}

/// Adds the places a narrowing took to the places the claim already gave up. A place is listed
/// once, in the order it was first taken.
fn remember(gave_up: &mut Vec<Given>, taken: &[Given]) {
    for place in taken {
        if !gave_up.iter().any(|known| known.at == place.at) {
            gave_up.push(place.clone());
        }
    }
}

/// Waits for build clearance before a write or a commit. The error carries the step that ends
/// the edit: the write may not run.
async fn cleared(
    config: &WorkerConfig,
    mcp: &McpClient,
    claim: &Claim,
    steps: &mut Vec<String>,
) -> anyhow::Result<Result<(), ToolStep>> {
    Ok(match wait_for_build(config, mcp, claim, steps).await? {
        Clearance::Cleared => Ok(()),
        Clearance::Waiting(stage) => Err(ToolStep::NotCleared(stage)),
        Clearance::Lost(reason) => Err(ToolStep::Lost(reason)),
        Clearance::Narrowed(given) => Err(ToolStep::Narrowed(given)),
    })
}

async fn run_tool(
    config: &WorkerConfig,
    mcp: &McpClient,
    claim: &Claim,
    tools: &Worktree,
    call: &ToolCall,
    steps: &mut Vec<String>,
) -> anyhow::Result<ToolStep> {
    let args: Value = match serde_json::from_str(&call.function.arguments) {
        Ok(args) => args,
        Err(err) => {
            return Ok(ToolStep::Answer(format!(
                "error: the arguments are not JSON: {err}"
            )));
        }
    };
    let answer = match call.function.name.as_str() {
        "read_file" => tools.read_file(&string_arg(&args, "path")).await,
        "list_files" => tools.list_files(&string_arg(&args, "path")),
        "run_tests" => tools.run_tests().await,
        "write_file" => {
            if let Err(step) = cleared(config, mcp, claim, steps).await? {
                return Ok(step);
            }
            let path = string_arg(&args, "path");
            let written = tools.write_file(&path, &string_arg(&args, "content"));
            match &written {
                Ok(_) => log(steps, format!("wrote {path} on {}", claim.task)),
                Err(reason) => log(
                    steps,
                    format!("write to {path} refused on {}: {reason}", claim.task),
                ),
            }
            written
        }
        "commit" => {
            if let Err(step) = cleared(config, mcp, claim, steps).await? {
                return Ok(step);
            }
            match tools.commit(&string_arg(&args, "message")).await {
                Ok(head) => {
                    log(steps, format!("committed {head} on {}", claim.task));
                    return Ok(ToolStep::Committed(head));
                }
                Err(reason) => {
                    log(steps, format!("commit refused on {}: {reason}", claim.task));
                    Err(reason)
                }
            }
        }
        "get_task" => get_task(mcp, &claim.task)
            .await
            .map(|detail| detail.to_string())
            .map_err(|err| format!("{err:#}")),
        other => Err(format!("there is no tool named {other}")),
    };
    Ok(ToolStep::Answer(match answer {
        Ok(text) => text,
        Err(reason) => format!("error: {reason}"),
    }))
}

/// Reports the commit as the build. The error is the refusal the origin gave.
async fn report_built(
    mcp: &McpClient,
    claim: &Claim,
    head: &str,
    steps: &mut Vec<String>,
) -> anyhow::Result<Result<(), String>> {
    let reported = mcp
        .call(
            "report_built",
            json!({
                "task": claim.task,
                "generation": claim.generation,
                "head": head,
            }),
        )
        .await?;
    match reported {
        Reply::Done(_) => {
            log(steps, format!("reported {head} on {}", claim.task));
            Ok(Ok(()))
        }
        Reply::Refused(reason) => Ok(Err(format!(
            "report_built refused on {}: {reason}",
            claim.task
        ))),
    }
}

/// Gives the claim back and logs why. The reason is the exit the run ended with, or the error it
/// failed on, so every release in the log says what caused it.
async fn release(mcp: &McpClient, claim: &Claim, why: &str, steps: &mut Vec<String>) {
    let released = mcp
        .call(
            "release_task",
            json!({ "task": claim.task, "generation": claim.generation }),
        )
        .await;
    match released {
        Ok(Reply::Done(_)) => log(steps, format!("released {}: {why}", claim.task)),
        Ok(Reply::Refused(reason)) => {
            log(
                steps,
                format!("release of {} refused: {reason}", claim.task),
            );
        }
        Err(err) => log(
            steps,
            format!(
                "release of {} did not reach the origin: {err:#}",
                claim.task
            ),
        ),
    }
}

/// How many planning calls a task gets. A live model can miss the requested schema, or name a path
/// the repository does not hold. Each miss is sent back with its reasons, and after the last miss
/// the task is skipped, so a plan that cannot be grounded never reaches the origin.
const PLAN_ATTEMPTS: usize = 3;

/// Plans one task and checks the plan against the repository and the confirmed scope. A refused
/// plan is sent back with the reasons and the file list. The error is the reasons of the last
/// refusal.
///
/// `given` holds the places a Narrow settlement took from the task. A plan may not name them. A
/// narrowed task whose plan keeps no file at all gets that plan back with its summary, so the
/// caller can release the claim with the reason.
///
/// The footprint must also keep every module file a may-change item's selector routes through:
/// a change the item allows cannot be built without them. A file the claim gave up under a
/// narrow settlement is not required, since it may not be declared again.
async fn plan_work<C: ModelClient>(
    model: &C,
    repo: &RepoFiles,
    row: &Value,
    scope: &Scope,
    given: &[Given],
    steps: &mut Vec<String>,
) -> Result<Planned, String> {
    let task = string_field(row, "task");
    let brief = planning_brief(repo, row, scope, given).await;
    let lost: Vec<String> = given
        .iter()
        .filter_map(|taken| file_of(&taken.at))
        .collect();
    let mut refusal: Vec<String> = Vec::new();
    for attempt in 1..=PLAN_ATTEMPTS {
        let mut messages = vec![Message::system(PLAN_PROMPT), Message::user(brief.clone())];
        if !refusal.is_empty() {
            messages.push(Message::user(format!(
                "Your last plan was refused:\n{}\nReply with a corrected plan. Copy each existing file's path exactly from the repository file list, and list a new file under new_files only when its directory is in that list.",
                refusal.join("\n")
            )));
        }
        let request = ChatRequest {
            messages,
            ..ChatRequest::default()
        };
        refusal = match model.chat_json::<Plan>(Role::Worker, request).await {
            Ok(structured) => {
                let plan = structured.value;
                if !given.is_empty() && plan.files.is_empty() && plan.new_files.is_empty() {
                    return Ok(Planned {
                        summary: plan.summary,
                        footprint: Vec::new(),
                        fixes: Vec::new(),
                    });
                }
                match repo.check(&plan.files, &plan.new_files) {
                    Ok(checked) => {
                        let taken: Vec<String> = checked
                            .footprint
                            .iter()
                            .filter(|file| lost.contains(file))
                            .cloned()
                            .collect();
                        if !taken.is_empty() {
                            taken
                                .iter()
                                .map(|file| {
                                    format!(
                                        "{file} was given up under a narrow settlement, so it may not be declared again"
                                    )
                                })
                                .collect()
                        } else {
                            let missing = unrouted(repo, scope, &checked.footprint, &lost);
                            if missing.is_empty() {
                                return Ok(Planned {
                                    summary: plan.summary,
                                    footprint: checked.footprint,
                                    fixes: checked.fixes,
                                });
                            }
                            missing
                        }
                    }
                    Err(reasons) => reasons,
                }
            }
            Err(err) => vec![format!(
                "the planning call failed, or its reply missed the plan schema: {err}"
            )],
        };
        log(
            steps,
            format!(
                "plan refused on {task} (attempt {attempt} of {PLAN_ATTEMPTS}): {}",
                refusal.join("; ")
            ),
        );
    }
    Err(refusal.join("; "))
}

/// One refusal for each module file a may-change item routes through that the footprint leaves
/// out. A place the claim gave up under a narrow settlement is not required: it may not be
/// declared again.
fn unrouted(repo: &RepoFiles, scope: &Scope, footprint: &[String], lost: &[String]) -> Vec<String> {
    let mut reasons = Vec::new();
    for item in &scope.body.may {
        for machine in item.machine_ids.iter() {
            let one = [machine.as_str().to_owned()];
            for file in repo.routed(&one) {
                let named = footprint.iter().any(|declared| declared == file);
                let given_up = lost.iter().any(|place| place == file);
                if !named && !given_up {
                    reasons.push(format!(
                        "`{file}` is the module file of machine `{machine}`, which scope item {} routes through; the plan must name it",
                        item.id.as_str()
                    ));
                }
            }
        }
    }
    reasons.sort();
    reasons.dedup();
    reasons
}

/// The planning message for one task: its title and brief, the confirmed scope (its machines,
/// selectors, and must-keep and may items), the repository's tracked files, and the full text of
/// the files the scope machines hold. A narrowed task also gets the places it gave up and why.
async fn planning_brief(repo: &RepoFiles, row: &Value, scope: &Scope, given: &[Given]) -> String {
    let machines = scope_machines(scope);
    let brief = format!(
        "Task: {}\nBrief: {}\n\n{}\n\nScope machines: {}\n\nRepository files (tracked; use these paths exactly):\n{}\n\nFull text of the files in the scope machines:\n{}",
        string_field(row, "title"),
        string_field(row, "brief"),
        scope_section(scope),
        machines.join(", "),
        repo.listing(),
        repo.scope_text(&machines).await,
    );
    if given.is_empty() {
        return brief;
    }
    let gave_up: Vec<String> = given
        .iter()
        .map(|taken| {
            let place = file_of(&taken.at).unwrap_or_else(|| format!("{:?}", taken.at));
            format!("- {place}: {}", taken.why)
        })
        .collect();
    format!(
        "{brief}\n\nYour declaration was narrowed, so you gave up these places, and you may not declare them again:\n{}\nPlan the change without them. List only the files the change still needs. If the change cannot be done without them, reply with an empty files list and say why in summary.",
        gave_up.join("\n")
    )
}

/// The machines the confirmed scope names, in id order with duplicates removed.
fn scope_machines(scope: &Scope) -> Vec<String> {
    let mut machines: Vec<String> = scope
        .body
        .may
        .iter()
        .flat_map(|item| item.machine_ids.iter())
        .chain(
            scope
                .body
                .must_keep
                .iter()
                .flat_map(|rule| rule.machine_ids.iter()),
        )
        .map(|id| id.as_str().to_owned())
        .collect();
    machines.sort();
    machines.dedup();
    machines
}

/// The confirmed scope as the planner sees it: what may change, with each item's selector and
/// acceptance obligations, and what must keep. The gate, not the planner, decides what passes.
fn scope_section(scope: &Scope) -> String {
    let may = if scope.body.may.is_empty() {
        "- (nothing may change)".to_owned()
    } else {
        scope
            .body
            .may
            .iter()
            .map(may_line)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let keep = if scope.body.must_keep.is_empty() {
        "- (nothing is pinned to keep)".to_owned()
    } else {
        scope
            .body
            .must_keep
            .iter()
            .map(keep_line)
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Confirmed scope (rev {} of {}):\nMay change:\n{may}\nMust keep:\n{keep}",
        scope.rev.0,
        scope.order.as_str()
    )
}

/// One may-change item as a brief line: its id and kind, its machines, its selector as the
/// scope wrote it, and each acceptance obligation's id and text.
fn may_line(item: &ScopeItem) -> String {
    format!(
        "- {} ({}), machines {}: selector {}; it must hold: {}",
        item.id.as_str(),
        change_name(item.change),
        machine_list(&item.machine_ids),
        serde_json::to_string(&item.selector)
            .unwrap_or_else(|_| "(a selector that does not print)".to_owned()),
        item.acceptance
            .iter()
            .map(|obligation| format!("{}: {}", obligation.obligation_id.as_str(), obligation.text))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// One must-keep rule as a brief line: its id, its machines, and the rule in the human's words.
fn keep_line(rule: &MustKeep) -> String {
    format!(
        "- {}, machines {}: {}",
        rule.id.as_str(),
        machine_list(&rule.machine_ids),
        rule.text
    )
}

/// A machine list as the brief shows it.
fn machine_list(machines: &NonEmpty<MachineId>) -> String {
    machines
        .iter()
        .map(|id| id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The change kind as the scope draft words it.
fn change_name(change: Change) -> &'static str {
    match change {
        Change::Add => "add",
        Change::Modify => "modify",
        Change::Remove => "remove",
    }
}

async fn open_tasks(mcp: &McpClient) -> anyhow::Result<Vec<Value>> {
    match mcp.call("list_tasks", json!({})).await? {
        Reply::Done(Value::Array(rows)) => Ok(rows
            .into_iter()
            .filter(|row| row["state"] == "open")
            .collect()),
        Reply::Done(other) => bail!("list_tasks answered {other}"),
        Reply::Refused(reason) => bail!("list_tasks was refused: {reason}"),
    }
}

async fn get_task(mcp: &McpClient, task: &str) -> anyhow::Result<Value> {
    match mcp.call("get_task", json!({ "task": task })).await? {
        Reply::Done(detail) => Ok(detail),
        Reply::Refused(reason) => bail!("get_task on {task} was refused: {reason}"),
    }
}

fn get_task_spec() -> ToolSpec {
    ToolSpec {
        name: "get_task".to_owned(),
        description: "Read the current state of your task, including its stage and clashes."
            .to_owned(),
        parameters: json!({ "type": "object", "properties": {} }),
    }
}

fn string_field(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

fn string_arg(args: &Value, key: &str) -> String {
    args[key].as_str().unwrap_or_default().to_owned()
}

fn log(steps: &mut Vec<String>, text: String) {
    tracing::info!("{text}");
    steps.push(text);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    use acan_llm::{ChatResponse, FunctionCall, LlmError, Reply, Stub, ToolKind};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    fn repo() -> RepoFiles {
        RepoFiles::from_paths("unused", vec!["src/a.ts".to_owned()])
    }

    /// The confirmed scope of the intent fixtures: may-change items on machines `ratelimit`,
    /// `auth` and `errors`, and must-keep rules on `auth` and `links`.
    fn scope() -> Scope {
        serde_json::from_str::<GetScopeResponse>(SCOPE)
            .expect("the scope fixture reads")
            .scope
    }

    /// The text of every message in a request, so a test can look for a reason in it.
    fn text_of(request: &ChatRequest) -> String {
        serde_json::to_string(&request.messages).unwrap_or_default()
    }

    #[tokio::test]
    async fn a_planning_reply_that_misses_its_schema_is_asked_again() {
        let calls = AtomicUsize::new(0);
        let model = Stub::default()
            .when(
                Role::Worker,
                move |_| calls.fetch_add(1, Ordering::SeqCst) == 0,
                Reply::text("not a plan"),
            )
            .when(
                Role::Worker,
                |_| true,
                Reply::text(r#"{"summary":"write the file","files":["src/a.ts"]}"#),
            );
        let row = json!({ "title": "t", "brief": "b", "scope": ["src"], "task": "t-1" });
        let planned = plan_work(&model, &repo(), &row, &scope(), &[], &mut Vec::new())
            .await
            .expect("the second reply is a plan");
        assert_eq!(planned.footprint, vec!["src/a.ts".to_owned()]);
    }

    #[tokio::test]
    async fn a_refused_plan_is_sent_back_with_its_reasons_and_the_file_list() {
        // The first reply names a path the repository does not hold. The second call must carry
        // that refusal and the file list, and the corrected reply then passes.
        let model = Stub::default()
            .when(
                Role::Worker,
                |request| !text_of(request).contains("was refused"),
                Reply::text(r#"{"summary":"s","files":["middleware/a.ts"]}"#),
            )
            .when(
                Role::Worker,
                |request| {
                    let text = text_of(request);
                    text.contains("is not a tracked file") && text.contains("- src/a.ts")
                },
                Reply::text(r#"{"summary":"s","files":["src/a.ts"]}"#),
            );
        let row = json!({ "title": "t", "brief": "b", "scope": ["src"], "task": "t-1" });
        let mut steps = Vec::new();
        let planned = plan_work(&model, &repo(), &row, &scope(), &[], &mut steps)
            .await
            .expect("the corrected plan passes");
        assert_eq!(planned.footprint, vec!["src/a.ts".to_owned()]);
        assert!(
            steps
                .iter()
                .any(|step| step.starts_with("plan refused on t-1 (attempt 1 of 3)")),
            "{steps:?}"
        );
    }

    #[tokio::test]
    async fn a_plan_that_never_passes_is_refused_after_its_tries_with_the_reason() {
        let model = Stub::default().when(
            Role::Worker,
            |_| true,
            Reply::text(r#"{"summary":"s","files":["nope.ts"]}"#),
        );
        let row = json!({ "title": "t", "brief": "b", "scope": ["src"], "task": "t-1" });
        let mut steps = Vec::new();
        let reasons = plan_work(&model, &repo(), &row, &scope(), &[], &mut steps)
            .await
            .expect_err("no attempt names a tracked file");
        assert!(
            reasons.contains("`nope.ts` is not a tracked file"),
            "{reasons}"
        );
        let refusals = steps
            .iter()
            .filter(|step| step.starts_with("plan refused"))
            .count();
        assert_eq!(refusals, PLAN_ATTEMPTS);
    }

    /// A repository the fixture scope routes through: one module file per may machine, and one
    /// file no item touches.
    fn routed_repo() -> RepoFiles {
        RepoFiles::from_paths(
            "unused",
            vec![
                "src/auth.ts".to_owned(),
                "src/errors.ts".to_owned(),
                "src/ratelimit.ts".to_owned(),
                "src/store.ts".to_owned(),
            ],
        )
    }

    #[tokio::test]
    async fn a_plan_that_leaves_out_a_file_the_scope_routes_through_is_sent_back_with_it() {
        // The first plan names the store and one routed module, leaving out the auth and errors
        // modules the may items route through. The refusal names them, and the next call shows
        // the planner the confirmed scope. The corrected plan passes.
        let model = Stub::default()
            .when(
                Role::Worker,
                |request| !text_of(request).contains("routes through"),
                Reply::text(r#"{"summary":"s","files":["src/store.ts","src/ratelimit.ts"]}"#),
            )
            .when(
                Role::Worker,
                |request| {
                    let text = text_of(request);
                    text.contains("`src/auth.ts` is the module file of machine `auth`")
                        && text.contains("`src/errors.ts` is the module file of machine `errors`")
                        && text.contains("/api/login")
                        && text.contains("mk-links-order")
                },
                Reply::text(
                    r#"{"summary":"s","files":["src/store.ts","src/ratelimit.ts","src/auth.ts","src/errors.ts"]}"#,
                ),
            );
        let row = json!({ "title": "t", "brief": "b", "scope": ["src"], "task": "t-1" });
        let mut steps = Vec::new();
        let planned = plan_work(&model, &routed_repo(), &row, &scope(), &[], &mut steps)
            .await
            .expect("the corrected plan passes");
        assert_eq!(
            planned.footprint,
            vec![
                "src/auth.ts".to_owned(),
                "src/errors.ts".to_owned(),
                "src/ratelimit.ts".to_owned(),
                "src/store.ts".to_owned(),
            ]
        );
        let refusal = steps
            .iter()
            .find(|step| step.starts_with("plan refused on t-1 (attempt 1 of 3)"))
            .expect("the first plan was refused");
        assert!(refusal.contains("src/auth.ts"), "{refusal}");
        assert!(refusal.contains("src/errors.ts"), "{refusal}");
        assert!(!refusal.contains("src/store.ts"), "{refusal}");
    }

    #[tokio::test]
    async fn a_replan_is_not_asked_for_a_routed_file_the_narrowing_took() {
        // The narrowing took the auth and errors modules, so the re-plan may not name them and
        // is not asked for them, though the scope routes through them.
        let model = Stub::default().when(
            Role::Worker,
            |_| true,
            Reply::text(r#"{"summary":"s","files":["src/ratelimit.ts"]}"#),
        );
        let row = json!({ "title": "t", "brief": "b", "scope": ["src"], "task": "t-1" });
        let given = vec![
            taken("src/auth.ts", "clash c-1"),
            taken("src/errors.ts", "clash c-1"),
        ];
        let planned = plan_work(
            &model,
            &routed_repo(),
            &row,
            &scope(),
            &given,
            &mut Vec::new(),
        )
        .await
        .expect("the given-up files are not required");
        assert_eq!(planned.footprint, vec!["src/ratelimit.ts".to_owned()]);
    }

    fn claim() -> Claim {
        Claim {
            task: "t-1".to_owned(),
            generation: 3,
            files: vec!["model/store.ts".to_owned()],
            gave_up: Vec::new(),
            repair_rounds: 2,
            rulings: Vec::new(),
            scope: scope(),
        }
    }

    fn taken(file: &str, why: &str) -> Given {
        Given {
            at: Locus::File {
                file: file.parse().expect("a file path"),
            },
            why: why.to_owned(),
        }
    }

    #[test]
    fn the_places_a_claim_gave_up_accumulate_across_narrowings() {
        let mut gave_up = Vec::new();
        remember(&mut gave_up, &[taken("src/a.ts", "first")]);
        remember(&mut gave_up, &[taken("src/b.ts", "second")]);
        // A place taken again is listed once, with the reason it was first taken for.
        remember(&mut gave_up, &[taken("src/a.ts", "third")]);
        let files = given_files(&gave_up).expect("both places are files");
        assert_eq!(files, vec!["src/a.ts".to_owned(), "src/b.ts".to_owned()]);
        assert_eq!(gave_up[0].why, "first");
    }

    fn view(state: &str, stage: Value, holder: bool, generation: Value) -> Value {
        json!({ "state": state, "stage": stage, "you_hold": holder, "generation": generation })
    }

    fn decide(detail: &Value, claim: &Claim) -> Clearance {
        clearance(detail, claim).expect("the view reads")
    }

    /// A clash row as `get_task` reports it. The state is the origin's `ClashState` in JSON.
    fn clash_row(id: &str, a: &str, b: &str, at: Value, state: Value) -> Value {
        json!({ "clash": id, "a": a, "b": b, "at": at, "state": state })
    }

    /// A clash that a settlement narrowed onto task `task`, by its number.
    fn narrowed_onto(task: u64) -> Value {
        json!({
            "state": "settled",
            "choice": { "choice": "narrow", "task": task },
            "by": "owner",
        })
    }

    fn two_file_repo() -> RepoFiles {
        RepoFiles::from_paths("unused", vec!["src/a.ts".to_owned(), "src/b.ts".to_owned()])
    }

    /// The place `t-1` gave up in a narrowing: `src/a.ts`.
    fn gave_up_a() -> Vec<Given> {
        vec![Given {
            at: Locus::File {
                file: "src/a.ts".parse().expect("a file path"),
            },
            why: "clash c-1 with t-2 was settled as narrow, so t-1 gives up its place".to_owned(),
        }]
    }

    #[test]
    fn only_the_holder_at_its_generation_may_write_and_only_once_building() {
        let claim = claim();
        assert_eq!(
            decide(&view("claimed", json!("building"), true, json!(3)), &claim),
            Clearance::Cleared
        );
        assert_eq!(
            decide(&view("claimed", json!("declared"), true, json!(3)), &claim),
            Clearance::Waiting("declared".to_owned())
        );
        // A replacement holds a newer generation.
        assert!(matches!(
            decide(&view("claimed", json!("building"), false, json!(4)), &claim),
            Clearance::Lost(_)
        ));
        // The same generation with another holder is still not this worker's.
        assert!(matches!(
            decide(&view("claimed", json!("building"), false, json!(3)), &claim),
            Clearance::Lost(_)
        ));
        // A released task is open again, with no holder.
        assert!(matches!(
            decide(&view("open", Value::Null, false, Value::Null), &claim),
            Clearance::Lost(_)
        ));
    }

    #[test]
    fn a_narrowing_of_this_claim_names_the_place_it_gave_up() {
        let claim = claim();
        let detail = json!({
            "state": "claimed",
            "stage": "declaring",
            "you_hold": true,
            "generation": 3,
            "clashes": [
                clash_row("c-1", "t-1", "t-2", json!({ "at": "file", "file": "model/store.ts" }), narrowed_onto(1)),
                // Narrowed onto the other task, so this claim keeps its place.
                clash_row("c-2", "t-1", "t-2", json!({ "at": "file", "file": "model/store.ts" }), narrowed_onto(2)),
                // A place the claim does not declare is not one it gave up.
                clash_row("c-3", "t-1", "t-2", json!({ "at": "file", "file": "model/other.ts" }), narrowed_onto(1)),
                // An open clash narrows nothing yet.
                clash_row("c-4", "t-1", "t-2", json!({ "at": "file", "file": "model/store.ts" }), json!({ "state": "open" })),
            ],
        });
        let Clearance::Narrowed(given) = clearance(&detail, &claim).expect("the view reads") else {
            panic!("a narrowing of this claim is reported");
        };
        assert_eq!(given.len(), 1, "{given:?}");
        assert_eq!(
            given[0].at,
            Locus::File {
                file: "model/store.ts".parse().expect("a file path"),
            }
        );
        assert!(given[0].why.contains("c-1"), "{}", given[0].why);
    }

    #[test]
    fn a_place_that_is_not_a_file_cannot_be_planned_around() {
        let machine = Given {
            at: Locus::Machine {
                machine: "model".parse().expect("a machine key"),
            },
            why: "clash c-1 with t-2 was settled as narrow, so t-1 gives up its place".to_owned(),
        };
        let reason = given_files(&[machine]).expect_err("a machine is not a file");
        assert!(reason.contains("not a file"), "{reason}");
    }

    #[test]
    fn the_tally_counts_each_tool_by_name_and_its_failures() {
        let mut tally = Tally::default();
        assert_eq!(tally.summary(), "no tool calls");
        tally.record("run_tests", true);
        tally.record("read_file", false);
        tally.record("run_tests", false);
        assert_eq!(tally.summary(), "read_file 1, run_tests 2 (1 failed)");
    }

    #[tokio::test]
    async fn a_replan_that_names_a_given_up_file_is_sent_back_without_it() {
        // The first reply names the file the narrowing took. The second call carries that refusal,
        // and its reply declares the other file.
        let model = Stub::default()
            .when(
                Role::Worker,
                |request| !text_of(request).contains("was given up under a narrow settlement"),
                Reply::text(r#"{"summary":"s","files":["src/a.ts"]}"#),
            )
            .when(
                Role::Worker,
                |_| true,
                Reply::text(r#"{"summary":"s","files":["src/b.ts"]}"#),
            );
        let row = json!({ "title": "t", "brief": "b", "scope": ["src"], "task": "t-1" });
        let mut steps = Vec::new();
        let planned = plan_work(
            &model,
            &two_file_repo(),
            &row,
            &scope(),
            &gave_up_a(),
            &mut steps,
        )
        .await
        .expect("the second plan passes");
        assert_eq!(planned.footprint, vec!["src/b.ts".to_owned()]);
        assert!(
            steps
                .iter()
                .any(|step| step.starts_with("plan refused on t-1 (attempt 1 of 3)")),
            "{steps:?}"
        );
    }

    #[tokio::test]
    async fn a_replan_that_keeps_no_file_returns_its_summary_for_the_release() {
        let model = Stub::default().when(
            Role::Worker,
            |_| true,
            Reply::text(r#"{"summary":"the loader needs src/a.ts","files":[]}"#),
        );
        let row = json!({ "title": "t", "brief": "b", "scope": ["src"], "task": "t-1" });
        let planned = plan_work(
            &model,
            &two_file_repo(),
            &row,
            &scope(),
            &gave_up_a(),
            &mut Vec::new(),
        )
        .await
        .expect("an empty re-plan is returned, not refused");
        assert!(planned.footprint.is_empty(), "{:?}", planned.footprint);
        assert_eq!(planned.summary, "the loader needs src/a.ts");
    }

    /// A real gate answer from the intent fixtures: one bounced item, `i-retry-after`, failing its
    /// obligation on the probe below.
    const RECEIPT: &str = include_str!("../../acan-intent/fixtures/mcp-get-receipt-response.json");
    /// A real scope answer from the intent fixtures, confirmed at revision 2.
    const SCOPE: &str = include_str!("../../acan-intent/fixtures/mcp-get-scope-response.json");
    const PROBE: &str = "sha256:762703905cfc5ffa376dfb1b89d53c9ce693dc5f3e1628b39b06935364f62b45";
    /// The episode `i-retry-after` is filed under in the same fixture.
    const EPISODE: &str = "sha256:1acc401ccb3ea61be044f545a3612c535b8aebe6d8333b3d55cc56773f59e736";
    /// The rulings the origin holds for the task, from the intent fixtures: one enforced, one note.
    const RULINGS: &str = include_str!("../../acan-intent/fixtures/mcp-get-rulings-response.json");
    const STORE: &str = "model/store.ts";

    /// The gate's answer for build `build`, with only the item `keep` among its items. Each build
    /// gets its own receipt hash, as each build gets its own run.
    fn receipt_of(keep: &str, build: u32) -> Value {
        let mut value: Value = serde_json::from_str(RECEIPT).expect("the receipt fixture is JSON");
        value["receipt_hash"] = json!(format!("sha256:{build:064x}"));
        value["receipt"]["items"]
            .as_array_mut()
            .expect("the fixture has items")
            .retain(|item| item["item_id"] == keep);
        value
    }

    /// The receipt of build `build` as the worker reads it.
    fn receipt_items(keep: &str, build: u32) -> Receipt {
        let response: GetReceiptResponse =
            serde_json::from_value(receipt_of(keep, build)).expect("the receipt decodes");
        response.receipt
    }

    #[test]
    fn a_bounce_names_its_failing_obligation_and_probe_for_the_next_build() {
        let mut repeats = BTreeMap::new();
        let Repair::Bounced(next) = repair(&receipt_items("i-retry-after", 1), &mut repeats) else {
            panic!("a failing item bounces");
        };
        assert_eq!(next.len(), 1, "{next:?}");
        assert_eq!(next[0].item, "i-retry-after");
        assert_eq!(next[0].class, ItemClass::ObligationFailed);
        assert_eq!(next[0].probe.to_string(), PROBE);
        assert_eq!(
            next[0]
                .episode
                .map(|episode| episode.to_string())
                .as_deref(),
            Some(EPISODE)
        );
        assert_eq!(
            next[0].failed,
            vec!["obl-retry-after failed: header.retry-after is absent at step s6".to_owned()]
        );
        let brief = build_brief(&claim(), 2, &next);
        assert!(
            brief.starts_with("Build 2 of t-1. Declared files: model/store.ts."),
            "{brief}"
        );
        assert!(brief.contains("The gate bounced build 1."), "{brief}");
        assert!(
            brief.contains(&format!(
                "- item i-retry-after (obligation_failed) on probe {PROBE}: obl-retry-after failed: header.retry-after is absent at step s6"
            )),
            "{brief}"
        );
    }

    #[test]
    fn the_third_repeat_of_the_same_difference_is_residue() {
        let receipt = receipt_items("i-retry-after", 1);
        let mut repeats = BTreeMap::new();
        assert!(matches!(repair(&receipt, &mut repeats), Repair::Bounced(_)));
        assert!(matches!(repair(&receipt, &mut repeats), Repair::Bounced(_)));
        let Repair::Residue { reason, given_up } = repair(&receipt, &mut repeats) else {
            panic!("the third repeat is residue");
        };
        assert!(reason.contains("third repeat is residue"), "{reason}");
        // The item the worker gave up on is the one it contests, under its own episode.
        assert_eq!(given_up.len(), 1, "{given_up:?}");
        assert_eq!(given_up[0].item, "i-retry-after");
        assert!(given_up[0].episode.is_some(), "{given_up:?}");
    }

    #[test]
    fn a_passing_item_builds_nothing_more_and_a_stopped_or_unspecified_item_is_residue() {
        assert_eq!(
            repair(&receipt_items("chk-health", 1), &mut BTreeMap::new()),
            Repair::Passed
        );
        let Repair::Residue {
            reason: stopped,
            given_up,
        } = repair(&receipt_items("d-links-clicks", 1), &mut BTreeMap::new())
        else {
            panic!("a stopped item is residue");
        };
        assert!(stopped.contains("the core stopped it"), "{stopped}");
        // The core stopped the item, so the worker has nothing to contest.
        assert!(given_up.is_empty(), "{given_up:?}");
        assert!(matches!(
            repair(
                &receipt_items("d-login-error-body", 1),
                &mut BTreeMap::new()
            ),
            Repair::Residue { .. }
        ));
    }

    #[test]
    fn only_the_difference_classes_the_worker_repairs_are_contested_and_with_their_kind() {
        assert_eq!(
            contest_kind(ItemClass::ViolatesRuling),
            Some(ContestKind::SpecConflict)
        );
        assert_eq!(
            contest_kind(ItemClass::OutOfScope),
            Some(ContestKind::Blocked)
        );
        assert_eq!(
            contest_kind(ItemClass::ObligationFailed),
            Some(ContestKind::Blocked)
        );
        assert_eq!(contest_kind(ItemClass::Preserved), None);
        assert_eq!(contest_kind(ItemClass::AuthorizedButUnspecified), None);
    }

    #[test]
    fn the_rulings_in_force_are_briefed_as_the_origins_text_and_superseded_ones_are_left_out() {
        let response: GetRulingsResponse =
            serde_json::from_str(RULINGS).expect("the rulings fixture decodes");
        let section = rulings_section(&response.rulings);
        assert!(
            section.starts_with("Rulings in force on this task's machines (the origin's text; the gate decides what passes):"),
            "{section}"
        );
        assert!(
            section.contains("- Links list stays newest first (r-1): the gate checks it"),
            "{section}"
        );
        assert!(
            section.contains("- Error pages read as plain sentences (r-2): a note the gate does not check: tone of error text has no predicate"),
            "{section}"
        );
        // A superseded ruling is no longer in force, so the brief does not name it.
        let mut superseded: Value = serde_json::from_str(RULINGS).expect("JSON");
        superseded["rulings"][0]["state"] = json!("superseded");
        let response: GetRulingsResponse =
            serde_json::from_value(superseded).expect("the edited rulings decode");
        let section = rulings_section(&response.rulings);
        assert!(
            !section.contains("Links list stays newest first"),
            "{section}"
        );
        assert!(
            section.contains("Error pages read as plain sentences"),
            "{section}"
        );
        assert_eq!(
            rulings_section(&[]),
            "No rulings are in force on this task's machines."
        );
    }

    /// A repository with `model/store.ts` committed on `main`, and the worker's identity set.
    fn bounce_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a repository");
        let root = dir.path();
        git_in(root, &["init", "-q", "-b", "main"]);
        git_in(root, &["config", "user.name", "Test Worker"]);
        git_in(root, &["config", "user.email", "worker@example.test"]);
        git_in(root, &["config", "core.autocrlf", "false"]);
        fs::create_dir_all(root.join("model")).expect("the machine directory");
        fs::write(root.join(STORE), "export function load() { return 1; }\n").expect("the fixture");
        git_in(root, &["add", "-A"]);
        git_in(root, &["commit", "-q", "-m", "initial"]);
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// The tools one round sees, from a view that names a snapshot or does not. A test run needs
    /// the snapshot, so the view without one is refused and the view with one is not.
    #[test]
    fn a_worktree_view_with_a_snapshot_arms_the_tests_and_one_without_refuses_them() {
        let config = WorkerConfig {
            server: String::new(),
            token: String::new(),
            repo: PathBuf::new(),
            test_command: Some("cargo test".to_owned()),
            max_steps: 1,
            wait: Duration::ZERO,
            receipt_wait: Duration::ZERO,
            heartbeat: Duration::from_secs(1),
            poll: Duration::from_millis(1),
        };
        let root = tempfile::tempdir().expect("a worktree");
        let snapshot = tempfile::tempdir().expect("a snapshot");
        let mut steps = Vec::new();
        let armed = tools_for(
            &config,
            &claim(),
            &json!({ "path": root.path().display().to_string(), "snapshot": snapshot.path().display().to_string() }),
            &mut steps,
        );
        let unarmed = tools_for(
            &config,
            &claim(),
            &json!({ "path": root.path().display().to_string() }),
            &mut steps,
        );
        let refused = unarmed
            .test_container_args("probe")
            .expect_err("a test run without a snapshot is refused");
        assert!(
            refused.contains("no protected-head snapshot is set"),
            "{refused}"
        );
        // The snapshot here is empty, so the policy may refuse for the protected paths it lacks,
        // but never for want of a snapshot.
        if let Err(reason) = armed.test_container_args("probe") {
            assert!(
                !reason.contains("no protected-head snapshot is set"),
                "{reason}"
            );
        }
        assert!(
            steps
                .iter()
                .any(|step| step.starts_with("protected-head snapshot for t-1: ")),
            "{steps:?}"
        );
        assert!(
            steps.iter().any(|step| step
                .starts_with("no protected-head snapshot for t-1; run_tests will refuse")),
            "{steps:?}"
        );
    }

    /// What the fake origin holds for one open task that its gate answers build by build.
    #[derive(Default)]
    struct Origin {
        /// The stage `get_task` reports. A declared task is cleared to build on the next read.
        stage: String,
        /// Builds reported so far. Build `n` is answered by receipt `n`.
        reports: usize,
        /// The gate's receipts, in build order.
        receipts: Vec<Value>,
        /// The scope `get_scope` answers, with the repair budget the test sets.
        scope: Value,
        /// The rulings `get_rulings` answers.
        rulings: Value,
        /// The contests the worker filed, as it sent them.
        contests: Vec<Value>,
        /// Whether the origin refuses every contest, as a test of the residue that stands anyway.
        refuse_contests: bool,
        /// The worktree and its snapshot, as `get_worktree` names them.
        worktree: String,
        snapshot: String,
        /// Every tool the worker called, in order.
        calls: Vec<String>,
        /// The tasks the worker released.
        released: Vec<String>,
    }

    /// Answers one MCP message for the fake origin. A notification has no answer.
    fn dispatch(origin: &StdMutex<Origin>, message: &Value) -> Option<Value> {
        let id = message["id"].clone();
        match message["method"].as_str()? {
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "protocolVersion": "2025-06-18" },
            })),
            "tools/call" => {
                let name = message["params"]["name"].as_str().unwrap_or_default();
                let mut guard = origin.lock().expect("the fake origin is not poisoned");
                let answer = tool(&mut guard, name, &message["params"]["arguments"]);
                let result = match answer {
                    Ok(value) => json!({
                        "content": [{ "type": "text", "text": value.to_string() }],
                        "isError": false,
                    }),
                    Err(refusal) => json!({
                        "content": [{ "type": "text", "text": refusal }],
                        "isError": true,
                    }),
                };
                Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            }
            _ => None,
        }
    }

    /// One tool call answered as the origin would for a single open task.
    fn tool(origin: &mut Origin, name: &str, args: &Value) -> Result<Value, String> {
        origin.calls.push(name.to_owned());
        match name {
            "heartbeat_task" | "post_scoped_readback" => Ok(json!({})),
            "list_tasks" => Ok(json!([{
                "task": "t-1",
                "order": "o-1",
                "state": "open",
                "title": "Change the loader",
                "brief": "change the loader",
                "scope": ["model"],
            }])),
            "get_scope" => Ok(origin.scope.clone()),
            "get_rulings" => Ok(origin.rulings.clone()),
            "file_contest" => {
                origin.contests.push(args.clone());
                if origin.refuse_contests {
                    return Err("the contest was not recorded".to_owned());
                }
                Ok(json!({
                    "version": "acan-mcp/1",
                    "contest_id": "k-1",
                    "target": args["episode_id"],
                    "status": "open",
                }))
            }
            "claim_task" => Ok(json!({ "task": "t-1", "generation": 1 })),
            "declare_changes" => Ok(json!({ "clashes_found": [] })),
            "get_task" => {
                let shown = origin.stage.clone();
                if origin.stage == "declared" {
                    origin.stage = "building".to_owned();
                }
                Ok(json!({
                    "task": "t-1",
                    "state": "claimed",
                    "stage": shown,
                    "you_hold": true,
                    "generation": 1,
                    "clashes": [],
                }))
            }
            "get_worktree" => Ok(json!({
                "task": "t-1",
                "path": origin.worktree,
                "branch": "unit/t-1",
                "snapshot": origin.snapshot,
            })),
            "report_built" => {
                origin.reports += 1;
                origin.stage = "gating".to_owned();
                Ok(json!({ "task": "t-1", "run": origin.reports }))
            }
            "get_receipt" => {
                let receipt = origin
                    .reports
                    .checked_sub(1)
                    .and_then(|built| origin.receipts.get(built))
                    .cloned()
                    .ok_or_else(|| "no receipt for t-1 yet".to_owned())?;
                // The gate closes the run, so the task is declared again until it is cleared.
                origin.stage = "declared".to_owned();
                Ok(receipt)
            }
            "release_task" => {
                origin.released.push(string_arg(args, "task"));
                origin.stage = "open".to_owned();
                Ok(json!({}))
            }
            other => Err(format!("the fake origin has no tool named {other}")),
        }
    }

    /// Serves the fake origin on a loopback port and returns its base URL. Each connection carries
    /// one message, which is how the MCP client sends them.
    async fn serve(origin: Arc<StdMutex<Origin>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the fake origin binds");
        let address = listener
            .local_addr()
            .expect("the fake origin has an address");
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let origin = Arc::clone(&origin);
                tokio::spawn(async move {
                    // A connection that fails mid-message only ends that connection.
                    let _ = answer_http(socket, &origin).await;
                });
            }
        });
        format!("http://{address}")
    }

    /// Reads one HTTP request, answers its JSON-RPC message, and closes the connection.
    async fn answer_http(mut socket: TcpStream, origin: &StdMutex<Origin>) -> std::io::Result<()> {
        let mut buf = Vec::new();
        let (body_start, length) = loop {
            if let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break (end + 4, length);
            }
            if read_more(&mut socket, &mut buf).await? == 0 {
                return Ok(());
            }
        };
        while buf.len() < body_start + length {
            if read_more(&mut socket, &mut buf).await? == 0 {
                return Ok(());
            }
        }
        let message: Value = serde_json::from_slice(&buf[body_start..body_start + length])
            .expect("the worker sends JSON");
        let (status, body) = match dispatch(origin, &message) {
            Some(answer) => ("200 OK", answer.to_string()),
            None => ("202 Accepted", String::new()),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await?;
        socket.shutdown().await
    }

    async fn read_more(socket: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        let mut chunk = [0u8; 4096];
        let read = socket.read(&mut chunk).await?;
        buf.extend_from_slice(&chunk[..read]);
        Ok(read)
    }

    /// The model the bounce runs use. It records the brief each edit starts with, and answers from
    /// a script.
    struct Recording {
        script: Stub,
        briefs: StdMutex<Vec<String>>,
    }

    impl ModelClient for Recording {
        async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
            // An edit's first tool request carries its brief and no tool replies yet.
            if !request.tools.is_empty()
                && replies(&request) == 0
                && let Some(brief) = request.last_user_text()
            {
                self.briefs
                    .lock()
                    .expect("the brief log is not poisoned")
                    .push(brief.to_owned());
            }
            self.script.chat(role, request).await
        }
    }

    /// The build a tool request belongs to: `n` when its brief says `Build n of`, otherwise 1.
    fn build_of(request: &ChatRequest) -> u32 {
        let text = request.last_user_text().unwrap_or_default();
        (2..=9)
            .find(|build| text.contains(&format!("Build {build} of ")))
            .unwrap_or(1)
    }

    /// The tool replies one edit has received, so a request knows which step of the build it is.
    fn replies(request: &ChatRequest) -> usize {
        request
            .messages
            .iter()
            .filter(|message| matches!(message, Message::Tool { .. }))
            .count()
    }

    fn tool_call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            kind: ToolKind::Function,
            function: FunctionCall {
                name: name.to_owned(),
                arguments: arguments.to_string(),
            },
        }
    }

    /// The scripted worker. It plans the store file, then in build `n` writes version `n + 1` and
    /// commits it. It never stops early, so the loop's own stops decide how the run ends.
    fn bounce_script() -> Stub {
        let plan = Reply::json(&json!({ "summary": "change the loader", "files": [STORE] }));
        let mut script =
            Stub::default().when(Role::Worker, |request| request.tools.is_empty(), plan);
        for build in 1..=3 {
            script = script
                .when(
                    Role::Worker,
                    move |request| build_of(request) == build && replies(request) == 0,
                    Reply {
                        tool_calls: vec![tool_call(
                            "write_file",
                            json!({
                                "path": STORE,
                                "content": format!("export function load() {{ return {}; }}\n", build + 1),
                            }),
                        )],
                        ..Reply::default()
                    },
                )
                .when(
                    Role::Worker,
                    move |request| build_of(request) == build && replies(request) == 1,
                    Reply {
                        tool_calls: vec![tool_call(
                            "commit",
                            json!({ "message": format!("build {build}") }),
                        )],
                        ..Reply::default()
                    },
                );
        }
        script.when(
            Role::Worker,
            |_| true,
            Reply::text("the build is committed"),
        )
    }

    /// What one bounce run ended with.
    struct Ran {
        outcome: Outcome,
        /// The brief of every edit, in build order.
        briefs: Vec<String>,
        /// Builds the worker reported to the origin.
        reports: usize,
        /// The tasks the worker released.
        released: Vec<String>,
        /// Every tool the worker called.
        calls: Vec<String>,
        /// The contests the worker filed.
        contests: Vec<Value>,
    }

    /// Runs one worker against the fake origin. The gate answers build `n` with `receipts[n - 1]`,
    /// and the confirmed scope allows `budget` repair rounds.
    async fn run_bounces(repo: &Path, budget: u32, receipts: Vec<Value>) -> Ran {
        run_bounces_as(repo, budget, receipts, false).await
    }

    /// As [`run_bounces`], and the origin refuses every contest when `refuse_contests` is set.
    async fn run_bounces_as(
        repo: &Path,
        budget: u32,
        receipts: Vec<Value>,
        refuse_contests: bool,
    ) -> Ran {
        let snapshot = tempfile::tempdir().expect("a protected-head snapshot");
        let mut scope: Value = serde_json::from_str(SCOPE).expect("the scope fixture is JSON");
        scope["budget"]["repair_rounds"] = json!(budget);
        let origin = Arc::new(StdMutex::new(Origin {
            stage: "declared".to_owned(),
            receipts,
            scope,
            rulings: serde_json::from_str(RULINGS).expect("the rulings fixture is JSON"),
            refuse_contests,
            worktree: repo.display().to_string(),
            snapshot: snapshot.path().display().to_string(),
            ..Origin::default()
        }));
        let server = serve(Arc::clone(&origin)).await;
        let config = WorkerConfig {
            server,
            token: "acan_agent_fake".to_owned(),
            repo: repo.to_path_buf(),
            test_command: None,
            max_steps: 5,
            wait: Duration::from_secs(10),
            receipt_wait: Duration::from_secs(10),
            heartbeat: Duration::from_secs(3600),
            poll: Duration::from_millis(5),
        };
        let model = Recording {
            script: bounce_script(),
            briefs: StdMutex::new(Vec::new()),
        };
        let outcome = run(&config, &model).await;
        let briefs = model
            .briefs
            .into_inner()
            .expect("the brief log is not poisoned");
        let origin = origin.lock().expect("the fake origin is not poisoned");
        Ran {
            outcome,
            briefs,
            reports: origin.reports,
            released: origin.released.clone(),
            calls: origin.calls.clone(),
            contests: origin.contests.clone(),
        }
    }

    /// One build bounces, the second build gets the reproducer, the third repeats the same
    /// difference and stops. The loop reports three builds, commits three times, and releases the
    /// claim as residue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bounced_build_is_repaired_with_its_reproducer_and_the_third_repeat_is_residue() {
        let repo = bounce_repo();
        let ran = run_bounces(
            repo.path(),
            2,
            vec![
                receipt_of("i-retry-after", 1),
                receipt_of("i-retry-after", 2),
                receipt_of("i-retry-after", 3),
            ],
        )
        .await;
        let Exit::Residue { task, reason } = &ran.outcome.exit else {
            panic!("the third repeat stops the claim: {}", ran.outcome.exit);
        };
        assert_eq!(task, "t-1");
        assert!(reason.contains("third repeat is residue"), "{reason}");
        assert_eq!(ran.reports, 3, "{:?}", ran.calls);
        assert_eq!(
            git_in(repo.path(), &["rev-list", "--count", "HEAD"]).trim(),
            "4",
            "one commit per build, on top of the fixture"
        );
        // Build 1 has no reproducer. Builds 2 and 3 each carry the one the gate gave.
        assert_eq!(ran.briefs.len(), 3, "{:#?}", ran.briefs);
        assert!(!ran.briefs[0].contains("bounced"), "{}", ran.briefs[0]);
        for (index, brief) in ran.briefs.iter().enumerate().skip(1) {
            assert!(
                brief.starts_with(&format!("Build {} of t-1.", index + 1)),
                "{brief}"
            );
            assert!(
                brief.contains(&format!("The gate bounced build {index}.")),
                "{brief}"
            );
            assert!(
                brief.contains(&format!(
                    "- item i-retry-after (obligation_failed) on probe {PROBE}: obl-retry-after failed: header.retry-after is absent at step s6"
                )),
                "{brief}"
            );
        }
        let steps = &ran.outcome.steps;
        assert!(
            steps.iter().any(|step| step
                == "the gate bounced build 1 of t-1: 1 item(s) to repair; building again"),
            "{steps:#?}"
        );
        assert!(
            steps.iter().any(|step| step
                == "the gate bounced build 2 of t-1: 1 item(s) to repair; building again"),
            "{steps:#?}"
        );
        assert!(
            steps
                .iter()
                .any(|step| step.starts_with("t-1 went to residue: ")),
            "{steps:#?}"
        );
        assert!(
            steps
                .iter()
                .any(|step| step.starts_with("released t-1: t-1 is residue: ")),
            "{steps:#?}"
        );
        assert!(
            steps
                .iter()
                .any(|step| step.starts_with("protected-head snapshot for t-1: ")),
            "{steps:#?}"
        );
        assert_eq!(ran.released, vec!["t-1".to_owned()]);
        // The rulings are read at claim time and briefed as the origin's text.
        assert!(
            ran.calls.iter().any(|call| call == "get_rulings"),
            "{:?}",
            ran.calls
        );
        assert!(
            ran.briefs[0].contains("- Links list stays newest first (r-1): the gate checks it"),
            "{}",
            ran.briefs[0]
        );
        assert!(
            steps
                .iter()
                .any(|step| step == "read 2 ruling(s) for t-1 at ledger event 2"),
            "{steps:#?}"
        );
        // The worker contests the residue under the episode of the item it gave up on. The
        // contest names the probe and carries the residue's reason as its rationale.
        assert_eq!(ran.contests.len(), 1, "{:?}", ran.contests);
        assert_eq!(ran.contests[0]["task"], json!("t-1"));
        assert_eq!(ran.contests[0]["episode_id"], json!(EPISODE));
        assert_eq!(ran.contests[0]["kind"], json!("blocked"));
        assert_eq!(ran.contests[0]["reproducer"], json!(PROBE));
        assert!(
            ran.contests[0]["rationale"]
                .as_str()
                .is_some_and(|reason| reason.contains("third repeat is residue")),
            "{}",
            ran.contests[0]
        );
        assert!(
            steps
                .iter()
                .any(|step| step == &format!("filed contest k-1 on t-1 against episode {EPISODE}")),
            "{steps:#?}"
        );
    }

    /// The second build gets the reproducer and passes, so the run is built and nothing bounces
    /// again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_repaired_build_that_passes_is_built() {
        let repo = bounce_repo();
        let ran = run_bounces(
            repo.path(),
            2,
            vec![receipt_of("i-retry-after", 1), receipt_of("chk-health", 2)],
        )
        .await;
        assert!(
            matches!(&ran.outcome.exit, Exit::Built { task, .. } if task == "t-1"),
            "{}",
            ran.outcome.exit
        );
        assert_eq!(ran.reports, 2, "{:?}", ran.calls);
        assert_eq!(ran.briefs.len(), 2, "{:#?}", ran.briefs);
        assert!(
            ran.briefs[1].contains("- item i-retry-after (obligation_failed)"),
            "{}",
            ran.briefs[1]
        );
        assert!(ran.released.is_empty(), "{:?}", ran.released);
    }

    /// The confirmed scope allows no repair round, so the first bounce is the last word and the
    /// worker never starts a second build.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bounce_past_the_confirmed_repair_budget_is_residue_before_the_next_build() {
        let repo = bounce_repo();
        let ran = run_bounces(repo.path(), 0, vec![receipt_of("i-retry-after", 1)]).await;
        let Exit::Residue { reason, .. } = &ran.outcome.exit else {
            panic!("the bounce is residue: {}", ran.outcome.exit);
        };
        assert!(
            reason.contains("the confirmed budget of 0 repair round(s) is spent"),
            "{reason}"
        );
        assert_eq!(ran.reports, 1, "{:?}", ran.calls);
        assert_eq!(ran.briefs.len(), 1, "{:#?}", ran.briefs);
        assert_eq!(ran.released, vec!["t-1".to_owned()]);
        // The bounced item had no round left, so the worker contests it under its episode.
        assert_eq!(ran.contests.len(), 1, "{:?}", ran.contests);
        assert_eq!(ran.contests[0]["episode_id"], json!(EPISODE));
        assert_eq!(ran.contests[0]["kind"], json!("blocked"));
    }

    /// The origin refuses the contest. The run still ends as residue, the refusal is in the step
    /// log, and the claim is released: a contest never changes what the residue is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_contest_leaves_the_residue_standing() {
        let repo = bounce_repo();
        let ran = run_bounces_as(
            repo.path(),
            2,
            vec![
                receipt_of("i-retry-after", 1),
                receipt_of("i-retry-after", 2),
                receipt_of("i-retry-after", 3),
            ],
            true,
        )
        .await;
        assert!(
            matches!(&ran.outcome.exit, Exit::Residue { task, .. } if task == "t-1"),
            "{}",
            ran.outcome.exit
        );
        assert_eq!(ran.contests.len(), 1, "{:?}", ran.contests);
        assert!(
            ran.outcome
                .steps
                .iter()
                .any(|step| step.starts_with("contest on t-1 refused: ")
                    && step.contains("the contest was not recorded")),
            "{:#?}",
            ran.outcome.steps
        );
        assert_eq!(ran.released, vec!["t-1".to_owned()]);
    }
}
