//! Runs the origin's derived jobs and records each result as a `SystemCmd`.
//!
//! The loop starts each derived job once per key. A job fails when its work errors (a model
//! call, a reply that cannot become a command, or a git step) or when the origin refuses its
//! result. A failed job is retried with backoff up to [`MAX_JOB_ATTEMPTS`]. When the last
//! attempt fails on git, a land or a witness run reports that failure as its own result, so
//! the queue or the gate moves on. Other failed jobs wait until their key leaves the derived
//! set and returns, so a broken model can neither spin the loop nor burn provider credits
//! without user activity. A refused result is never retried: the origin decided it against the
//! world the job read, and a changed world derives a new key. Accounting-only batches never wake
//! the loop; the writer guarantees that.
//!
//! The loop also keeps each task's worktree in step with its live generation. A claimed task's
//! generation is its lease generation; a queued task keeps the generation of the gate run that
//! passed. The worktree stays while either holds, and a different generation resets it before
//! creating it again, so a reclaim starts at the current protected head. The worktree goes only
//! when the task has no live generation. That step is not a job, because the core derives no key
//! for it. After a land, and before its result is recorded, every claimed task that is not
//! building yet is fast-forwarded onto the landed commit, so a serialized waiter builds on what
//! its predecessor landed.
//!
//! A `Barrier` job sleeps until its order's declare barrier is due and then sends a `Tick`,
//! the same command the lease expiry runs on.
//!
//! A `DraftScope` job asks the foreman for a scope of an order that has none. The draft is stored
//! as `ScopeDrafted` and confirms nothing. A failed draft is retried and then recorded as
//! `ScopeDraftFailed`; no offline draft stands in for it.
//!
//! A scope-first run is not a job: the core derives no key for it. The loop reads
//! `World::pending_contrasts` instead. Each pending run is prepared once (`CandidatePrepared`),
//! observed and judged (`ReceiptReady`). A run already running is never started again. The
//! state lives in the origin, so a restart reads the same pending runs and continues each from
//! the recorded step: a run with a recorded preparation is judged, never prepared again. A
//! scope-first land runs the final gate and installs the tested merged commit only when the gate
//! passes. An order with no scope lane cannot claim: a claim needs a legacy verdict, and no job
//! produces one any more. A scope-first land whose evidence is missing or no longer confirmed is
//! refused, so nothing installs.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use acan_core::{
    Check, ClashId, Command, CommitId, GateRunId, Generation, Job, OrderId, OrderRev, Rejection,
    Stage, Step, SystemCmd, TaskId, TaskState, Timestamp, WitnessId, WitnessKind, WitnessOutcome,
    World,
};
use acan_git::{Preparation, PrepareOutcome, ProtectedExport, Repo};
use acan_intent::{
    AppContract, CommitSha, ContainerPolicy, DiscrepancyEpisode, GateContext, MachineId, ModelRef,
    OrderRef, PreparedCommit, ProbeCorpus, RepoPath, Scope, ScopeConfirmation, ScopeDraft,
    ScopeDraftV1, ScopeRev, UnixMillis,
};
use acan_llm::{CallRecord, LlmError, ModelClient, Role};
use acan_witness::RunnerConfig;
use tokio::sync::Semaphore;
use tokio::task::{self, JoinHandle, JoinSet};

use crate::STATE_DIR;
use crate::examples;
use crate::git::{LandFailure, Workspace};
use crate::host::{self, Host, SubmitError};
use crate::land::{self, Delivery, LandError};
use crate::model::{self, ReplyError};
use crate::observe::{self, ObserveError, Order, Plan};
use crate::vision;

/// Total attempts one job key gets before the loop stops retrying it. Chosen so a broken model
/// shows up in seconds during a demo while a live provider is called at most this many times
/// for the same stuck job.
const MAX_JOB_ATTEMPTS: u32 = 5;

/// The delay before the first retry of a failed job. Each later retry waits twice as long.
const RETRY_BACKOFF: Duration = Duration::from_millis(20);

/// The program the containers run under.
const RUNNER_PROGRAM: &str = "docker";

/// The port the app listens on inside its container.
const APP_PORT: u16 = 3000;

/// The state directory's folder that holds one directory per observe pass.
const CONTRAST_DIR: &str = "contrasts";

/// The state directory's folder that holds the stored observations, by hash.
const OBSERVATIONS_DIR: &str = "observations";

/// The state directory's folder that holds one protected-head snapshot per commit.
const SNAPSHOT_DIR: &str = "snapshots";

/// The scope revision a draft proposes for an order that has no scope yet. The first scope of
/// every order is revision 1 (the take and the lane tests write it so), and a new lane accepts
/// any revision. The draft's outcome is keyed by the order's own revision, not by this one.
const FIRST_SCOPE_REV: ScopeRev = ScopeRev(1);

/// Starts the job loop for `host`.
pub fn spawn<C: ModelClient + 'static>(host: Host<C>) -> JoinHandle<()> {
    tokio::spawn(run(host))
}

#[derive(Debug, thiserror::Error)]
enum JobError {
    #[error(transparent)]
    Model(#[from] LlmError),
    #[error(transparent)]
    Reply(#[from] ReplyError),
    #[error(transparent)]
    Git(#[from] acan_git::Error),
    #[error("the origin refused the result: {0}")]
    Rejected(#[source] Rejection),
    #[error("recording the result failed: {0}")]
    Store(#[source] anyhow::Error),
    #[error("the ruling log could not be read: {0}")]
    Rulings(#[source] anyhow::Error),
    #[error(transparent)]
    Observe(#[from] ObserveError),
    #[error("{0}")]
    Unavailable(String),
}

impl From<SubmitError> for JobError {
    fn from(err: SubmitError) -> Self {
        match err {
            SubmitError::Rejected(rejection) => Self::Rejected(rejection),
            SubmitError::Internal(err) => Self::Store(err),
        }
    }
}

/// How often a job key failed, and when it may run again.
struct Retry {
    attempts: u32,
    not_before: Instant,
}

/// What a task worktree step does.
#[derive(Debug, Clone, Copy)]
enum Change {
    /// The worktree now recorded at this generation.
    Create(Generation),
    /// The worktree of this generation was removed.
    Remove(Generation),
    /// The create found the task's live generation had moved and changed nothing.
    Stale,
}

/// The worktrees of the tasks, as the loop last left them, and the steps still running.
#[derive(Default)]
struct WorktreeSync {
    /// The generation recorded for each task whose worktree exists on disk.
    present: HashMap<TaskId, Generation>,
    /// Creations and removals in flight. A task waits for its own step before its next one.
    ops: JoinSet<(TaskId, Change, Result<(), acan_git::Error>)>,
    running: HashMap<task::Id, TaskId>,
}

async fn run<C: ModelClient + 'static>(host: Host<C>) {
    // The sets are owned by this loop, so aborting the loop aborts every task it started. A job
    // must never write to the log after its server has stopped.
    let mut tasks: JoinSet<(Job, Result<(), JobError>)> = JoinSet::new();
    let mut in_flight: HashMap<task::Id, Job> = HashMap::new();
    let mut retries: HashMap<Job, Retry> = HashMap::new();
    let mut contrasts = Contrasts::default();
    let mut worktrees = WorktreeSync::default();
    if let Some(workspace) = host.workspace() {
        // Worktrees left by an earlier run belong to tasks that may no longer hold a claim.
        match workspace.worktree_tasks().await {
            Ok(present) => {
                for (task, generation) in present {
                    // A worktree with no recorded generation predates this and cannot be matched
                    // to a live claim. Recording zero makes the first reconcile remove it; a live
                    // claim's generation is never zero, so the removal cannot match it.
                    worktrees
                        .present
                        .insert(task, generation.unwrap_or(Generation::new(0)));
                }
            }
            Err(err) => tracing::warn!(
                error = %err,
                "listing the task worktrees failed; leftovers from an earlier run stay"
            ),
        }
    }
    loop {
        let next_retry = next_retry(&retries, |job| {
            in_flight.values().any(|running| running == job)
        })
        .into_iter()
        .chain(next_retry(&contrasts.retries, |run| {
            contrasts.running.values().any(|running| running == run)
        }))
        .min();
        tokio::select! {
            () = host.jobs_woken() => {
                start_derived(&host, &mut tasks, &mut in_flight, &mut retries);
                start_contrasts(&host, &mut contrasts);
                reconcile(&host, &mut worktrees);
            }
            Some(finished) = tasks.join_next() => match finished {
                Ok((job, Ok(()))) => {
                    in_flight.retain(|_, running| *running != job);
                    retries.remove(&job);
                }
                Ok((job, Err(err))) => {
                    in_flight.retain(|_, running| *running != job);
                    record_failure(&mut retries, job, &err);
                }
                Err(err) => {
                    tracing::error!(error = %err, "a job task failed");
                    in_flight.remove(&err.id());
                }
            },
            Some(finished) = contrasts.tasks.join_next() => match finished {
                Ok((run, Ok(()))) => {
                    contrasts.running.retain(|_, running| *running != run);
                    contrasts.retries.remove(&run);
                    start_contrasts(&host, &mut contrasts);
                }
                Ok((run, Err(err))) => {
                    contrasts.running.retain(|_, running| *running != run);
                    record_failure(&mut contrasts.retries, run, &err);
                }
                // The run is not recorded as done. The next wake starts it again from the
                // origin's state.
                Err(err) => {
                    tracing::error!(error = %err, "a scope-first run task failed");
                    contrasts.running.remove(&err.id());
                }
            },
            Some(finished) = worktrees.ops.join_next() => match finished {
                Ok((task, change, Ok(()))) => {
                    worktrees.running.retain(|_, running| *running != task);
                    match change {
                        Change::Create(generation) => {
                            worktrees.present.insert(task, generation);
                        }
                        Change::Remove(generation) => {
                            // A newer generation may have been recorded while this removal ran.
                            if worktrees.present.get(&task) == Some(&generation) {
                                worktrees.present.remove(&task);
                            }
                        }
                        // Nothing was created; the reconcile below retries at the generation
                        // the task actually holds now.
                        Change::Stale => {}
                    }
                    reconcile(&host, &mut worktrees);
                }
                Ok((task, change, Err(err))) => {
                    worktrees.running.retain(|_, running| *running != task);
                    tracing::warn!(
                        %task,
                        ?change,
                        error = %err,
                        "a task worktree step failed; the next change retries it"
                    );
                }
                // The task is lost with the panic, so its step is not recorded as done. The
                // next change finds the worktree state unchanged and tries again.
                Err(err) => {
                    worktrees.running.remove(&err.id());
                    tracing::error!(error = %err, "a task worktree job failed");
                }
            },
            () = wait_until(next_retry) => {
                start_derived(&host, &mut tasks, &mut in_flight, &mut retries);
                start_contrasts(&host, &mut contrasts);
            }
        }
    }
}

/// The scope-first runs in progress and the ones waiting to run again.
#[derive(Default)]
struct Contrasts {
    tasks: JoinSet<(GateRunId, Result<(), JobError>)>,
    /// The run each started task works on, by task id.
    running: HashMap<task::Id, GateRunId>,
    retries: HashMap<GateRunId, Retry>,
}

/// The earliest time a failed key may run again. A key still running, or one that used up its
/// attempts, has none.
fn next_retry<K: Eq + std::hash::Hash>(
    retries: &HashMap<K, Retry>,
    is_running: impl Fn(&K) -> bool,
) -> Option<Instant> {
    retries
        .iter()
        .filter(|(key, retry)| retry.attempts < MAX_JOB_ATTEMPTS && !is_running(key))
        .map(|(_, retry)| retry.not_before)
        .min()
}

/// Starts one task for each pending scope-first run that is not already running and is due.
fn start_contrasts<C: ModelClient + 'static>(host: &Host<C>, contrasts: &mut Contrasts) {
    let pending: Vec<GateRunId> = host.read(|world| {
        world
            .pending_contrasts()
            .iter()
            .map(|contrast| contrast.run)
            .collect()
    });
    contrasts.retries.retain(|run, _| pending.contains(run));
    if !pending.is_empty() {
        tracing::debug!(
            pending = pending.len(),
            running = contrasts.running.len(),
            "scope-first runs are pending"
        );
    }
    let now = Instant::now();
    for run in pending {
        if contrasts.running.values().any(|running| *running == run) {
            continue;
        }
        match contrasts.retries.get(&run) {
            Some(retry) if retry.attempts >= MAX_JOB_ATTEMPTS => continue,
            Some(retry) if retry.not_before > now => continue,
            _ => {}
        }
        let host = host.clone();
        let id = contrasts
            .tasks
            .spawn(async move {
                let result = contrast(&host, run).await;
                (run, result)
            })
            .id();
        contrasts.running.insert(id, run);
    }
}

/// Waits for the next retry time, or forever when nothing is waiting to retry.
async fn wait_until(next_retry: Option<Instant>) {
    match next_retry {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn record_failure<K: Eq + std::hash::Hash + std::fmt::Debug>(
    retries: &mut HashMap<K, Retry>,
    job: K,
    err: &JobError,
) {
    if matches!(err, JobError::Rejected(_)) {
        // Running the job again would repeat the same decision. A witness run is too costly to
        // repeat for nothing, so the key waits until the world moves it out of the derived set.
        tracing::warn!(?job, error = %err, "the origin refused a job result; not retrying it");
        retries.insert(
            job,
            Retry {
                attempts: MAX_JOB_ATTEMPTS,
                not_before: Instant::now(),
            },
        );
        return;
    }
    let attempts = retries.get(&job).map_or(0, |retry| retry.attempts) + 1;
    if attempts >= MAX_JOB_ATTEMPTS {
        tracing::error!(?job, attempts, error = %err, "job kept failing; leaving it until it changes");
    } else {
        tracing::warn!(?job, attempt = attempts, error = %err, "job failed; retrying with backoff");
    }
    retries.insert(
        job,
        Retry {
            attempts,
            not_before: Instant::now() + RETRY_BACKOFF * 2u32.pow(attempts.saturating_sub(1)),
        },
    );
}

fn start_derived<C: ModelClient + 'static>(
    host: &Host<C>,
    tasks: &mut JoinSet<(Job, Result<(), JobError>)>,
    in_flight: &mut HashMap<task::Id, Job>,
    retries: &mut HashMap<Job, Retry>,
) {
    let jobs = host.read(World::jobs);
    // A key that left the derived set and came back is a fresh job and earns fresh attempts.
    retries.retain(|job, _| jobs.contains(job));
    let now = Instant::now();
    for job in jobs {
        if in_flight.values().any(|running| *running == job) {
            continue;
        }
        let attempt = match retries.get(&job) {
            Some(retry) if retry.attempts >= MAX_JOB_ATTEMPTS => continue,
            Some(retry) if retry.not_before > now => continue,
            Some(retry) => retry.attempts + 1,
            None => 1,
        };
        let id = tasks
            .spawn(run_one(host.clone(), job.clone(), attempt))
            .id();
        in_flight.insert(id, job);
    }
}

/// Brings each task's worktree in line with its live generation: created while the task is
/// claimed or queued, reset and created again when the generation changed, removed once the task
/// has none. A task with a step in flight waits for that step, which reconciles again when it
/// finishes.
fn reconcile<C: ModelClient + 'static>(host: &Host<C>, worktrees: &mut WorktreeSync) {
    let Some(workspace) = host.workspace() else {
        return;
    };
    let live = host.read(live_generations);
    let create: Vec<(TaskId, Generation)> = live
        .iter()
        .filter(|(task, generation)| worktrees.present.get(task) != Some(generation))
        .map(|(task, generation)| (*task, *generation))
        .collect();
    let remove: Vec<(TaskId, Generation)> = worktrees
        .present
        .iter()
        .filter(|(task, _)| !live.contains_key(task))
        .map(|(task, generation)| (*task, *generation))
        .collect();
    for (task, generation) in create {
        if is_running(worktrees, task) {
            continue;
        }
        let workspace = Arc::clone(&workspace);
        let host = host.clone();
        let id = worktrees
            .ops
            .spawn(async move {
                // The world may have moved between the check above and this step. Under the
                // task lock the generation is still what reconcile captured, so a witness of
                // the old generation cannot be running in the worktree this step replaces,
                // and a newer claim's create has already won the lock.
                let _task = workspace.lock_task(task).await;
                let still_live =
                    host.read(|world| live_generations(world).get(&task) == Some(&generation));
                if !still_live {
                    return (task, Change::Stale, Ok(()));
                }
                let result = workspace.worktree(task, generation).await.map(drop);
                (task, Change::Create(generation), result)
            })
            .id();
        worktrees.running.insert(id, task);
    }
    for (task, generation) in remove {
        if is_running(worktrees, task) {
            continue;
        }
        let workspace = Arc::clone(&workspace);
        let id = worktrees
            .ops
            .spawn(async move {
                let result = workspace.reset(task, generation).await;
                (task, Change::Remove(generation), result)
            })
            .id();
        worktrees.running.insert(id, task);
    }
}

fn is_running(worktrees: &WorktreeSync, task: TaskId) -> bool {
    worktrees.running.values().any(|running| *running == task)
}

/// The generation each task's worktree must sit at. A claimed task uses its lease generation. A
/// queued task keeps the generation of the gate run that passed, so its branch survives until it
/// lands. Every other state has none, and its worktree is removed.
fn live_generations(world: &World) -> HashMap<TaskId, Generation> {
    world
        .orders()
        .flat_map(move |order| world.tasks_of(order.id))
        .filter_map(|task| {
            let generation = match &task.state {
                TaskState::Claimed { claim, .. } => Some(claim.lease.generation),
                TaskState::Queued { run, .. } => world.gate(*run).map(|gate| gate.generation),
                TaskState::Open | TaskState::Landed { .. } | TaskState::Dropped => None,
            };
            generation.map(|generation| (task.id, generation))
        })
        .collect()
}

async fn run_one<C: ModelClient>(
    host: Host<C>,
    job: Job,
    attempt: u32,
) -> (Job, Result<(), JobError>) {
    let result = match execute(&host, &job).await {
        Ok(Some(command)) => host
            .submit(Command::System(command))
            .await
            .map(drop)
            .map_err(JobError::from),
        Ok(None) => Ok(()),
        Err(err) => {
            if attempt >= MAX_JOB_ATTEMPTS {
                report_exhausted(&host, &job, &err).await;
            }
            Err(err)
        }
    };
    if let Err(err) = &result {
        tracing::debug!(?job, attempt, error = %err, "job attempt failed");
    }
    (job, result)
}

/// Reports a failure that used up every attempt as the job's own result. A git failure moves the
/// land queue or the gate on, instead of waiting for a change that may never come. A scope draft
/// that never checked is recorded as `ScopeDraftFailed`, so the failure is visible and nothing
/// stands in for it.
async fn report_exhausted<C: ModelClient>(host: &Host<C>, job: &Job, err: &JobError) {
    let command = match job {
        Job::Land { task, head } if matches!(err, JobError::Git(_)) => SystemCmd::LandResult {
            task: *task,
            head: head.clone(),
            result: Err(format!("git failed on every attempt: {err}")),
        },
        Job::RunWitness { run, witness } if matches!(err, JobError::Git(_)) => {
            SystemCmd::WitnessResult {
                run: *run,
                witness: *witness,
                outcome: WitnessOutcome::Error {
                    detail: format!("git failed on every attempt: {err}"),
                },
            }
        }
        // A draft that never checked is recorded, so the member sees why no scope came.
        Job::DraftScope { order, rev } => SystemCmd::ScopeDraftFailed {
            order: *order,
            rev: *rev,
            reason: format!("the scope draft failed on every attempt: {err}"),
        },
        _ => return,
    };
    if let Err(submit_err) = host.submit(Command::System(command)).await {
        tracing::error!(
            ?job,
            error = %submit_err,
            "reporting the failed job to the origin failed"
        );
    }
}

async fn execute<C: ModelClient>(host: &Host<C>, job: &Job) -> Result<Option<SystemCmd>, JobError> {
    match job {
        Job::Split { order, rev } => split(host, job, *order, *rev).await,
        Job::DraftScope { order, rev } => draft_scope(host, job, *order, *rev).await,
        Job::Explain { clash } => explain(host, job, *clash).await,
        Job::Barrier { deadline, .. } => {
            sleep_until(*deadline).await;
            Ok(Some(SystemCmd::Tick))
        }
        Job::RunWitness { run, witness } => run_witness(host, *run, *witness).await,
        Job::Land { task, head } => land(host, *task, head).await,
    }
}

/// Sleeps until the origin's clock reaches `deadline`. The writer stamps commands with the same
/// clock, so the `Tick` sent afterwards finds the barrier due.
async fn sleep_until(deadline: Timestamp) {
    loop {
        let left = deadline.as_millis().saturating_sub(host::now().as_millis());
        if left == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(left)).await;
    }
}

async fn split<C: ModelClient>(
    host: &Host<C>,
    job: &Job,
    order: OrderId,
    rev: OrderRev,
) -> Result<Option<SystemCmd>, JobError> {
    let machines = model::plant_machines(&host.scan().index);
    let Some(request) = host.read(|world| model::split_request(world, order, &machines)) else {
        return Ok(None);
    };
    let reply = host
        .client()
        .chat_json::<model::SplitReply>(Role::Foreman, request)
        .await?;
    record_usage(host, job, &reply.call, Step::Split).await;
    Ok(Some(model::split_result(
        order,
        rev,
        &machines,
        reply.value,
    )?))
}

/// Asks the foreman to draft the scope of `order` at `rev`. The reply is checked against the
/// plant's machines, the protected route table and the protected limits, then reported as
/// `ScopeDrafted`. The draft confirms nothing: a human revises and confirms the scope. No offline
/// draft stands in for a failed call; a failure is retried and then reported as
/// `ScopeDraftFailed`.
async fn draft_scope<C: ModelClient>(
    host: &Host<C>,
    job: &Job,
    order: OrderId,
    rev: OrderRev,
) -> Result<Option<SystemCmd>, JobError> {
    let Some(workspace) = host.workspace() else {
        return Err(JobError::Unavailable(
            "the server has no git repository to draft a scope against".to_owned(),
        ));
    };
    let Some((sentence, machine)) = host.read(|world| draft_facts(world, job, order)) else {
        return Ok(None);
    };
    let order_ref = OrderRef::from_str(&order.to_string()).map_err(|err| {
        JobError::Unavailable(format!("the order id has no contract spelling: {err}"))
    })?;
    let machine_id = MachineId::from_str(&machine)
        .map_err(|err| JobError::Unavailable(format!("the order's machine: {err}")))?;
    let base = protected_sha(workspace.repo()).await?;
    let snapshot = protected_snapshot(workspace.repo(), &host.repo().join(STATE_DIR))
        .await
        .map_err(|err| JobError::Unavailable(format!("the protected snapshot: {err}")))?;
    let (app, corpus) = observe::read_protected(&snapshot)?;
    let machines = model::plant_machines(&host.scan().index);
    let bounds = model::DraftBounds {
        machines: &machines,
        routes: &corpus.routes,
        fixtures: &corpus.fixtures,
        limits: app.limits,
    };
    // The foreman drafts from the order's sentence, what the human asked for. The order ref is
    // only the draft's key, so it never stands in for the request.
    let request = model::scope_request(&sentence, &bounds);
    let reply = host
        .client()
        .chat_json::<model::DraftReply>(Role::Foreman, request)
        .await?;
    // The draft's usage is not a `ModelUsed` event: `Step` has no scope variant, and the usage
    // panel matches `Step` exhaustively in a crate this change does not touch. The client's own
    // ledger still counts the call.
    let model = ModelRef::from_str(&reply.call.model)
        .map_err(|err| JobError::Unavailable(format!("the model id: {err}")))?;
    let proposal = model::scope_from_reply(reply.value, model, &bounds)
        .map_err(|err| JobError::Unavailable(format!("the scope draft does not check: {err}")))?;
    let draft = ScopeDraft {
        version: ScopeDraftV1,
        order: order_ref,
        rev: FIRST_SCOPE_REV,
        base_commit: base,
        sentence,
        machine_id,
        draft: proposal,
        drafted_at: UnixMillis(host::now().as_millis()),
    };
    Ok(Some(SystemCmd::ScopeDrafted {
        order,
        rev,
        draft: Box::new(draft),
    }))
}

/// What a scope draft of `order` is written from: the order's sentence and machine. None when
/// the job is no longer derived, so no draft is asked for a scoped or superseded order.
fn draft_facts(world: &World, job: &Job, order: OrderId) -> Option<(String, String)> {
    if !world.jobs().contains(job) {
        return None;
    }
    let found = world.order(order)?;
    Some((found.sentence.clone(), found.machine.as_str().to_owned()))
}

async fn explain<C: ModelClient>(
    host: &Host<C>,
    job: &Job,
    clash: ClashId,
) -> Result<Option<SystemCmd>, JobError> {
    let Some(request) = host.read(|world| model::explain_request(world, clash)) else {
        return Ok(None);
    };
    let reply = host
        .client()
        .chat_json::<model::ExplainReply>(Role::Fast, request)
        .await?;
    record_usage(host, job, &reply.call, Step::Explain).await;
    Ok(Some(SystemCmd::ClashExplained {
        clash,
        explanation: model::explanation(reply.value)?,
    }))
}

/// Runs one frozen witness against the commit its gate tests, and reports what it found.
async fn run_witness<C: ModelClient>(
    host: &Host<C>,
    run: GateRunId,
    witness: WitnessId,
) -> Result<Option<SystemCmd>, JobError> {
    let Some(task) = host.read(|world| world.gate(run).map(|gate| gate.task)) else {
        return Ok(None);
    };
    let Some(outcome) = check_at(host, task, run, witness).await? else {
        return Ok(None);
    };
    Ok(Some(SystemCmd::WitnessResult {
        run,
        witness,
        outcome,
    }))
}

/// Runs the witness in the task's worktree, once that worktree sits at the gated commit.
///
/// The world is read again under the task lock. A gate run that is no longer the task's current
/// run, or a generation that no longer matches, returns no result and creates no worktree. A
/// worktree at another commit is an error outcome, which the gate counts as a failure. A git
/// failure is an error of the job and follows the retry path.
async fn check_at<C: ModelClient>(
    host: &Host<C>,
    task: TaskId,
    run: GateRunId,
    witness: WitnessId,
) -> Result<Option<WitnessOutcome>, JobError> {
    let Some(workspace) = host.workspace() else {
        return Ok(Some(WitnessOutcome::Error {
            detail: "this server has no git repository to run witnesses in".to_owned(),
        }));
    };
    // Two runs of one task would share its worktree directory, so they take turns. The task lock
    // comes before the global slot, so a run waiting for its task never holds a slot.
    let _task = workspace.lock_task(task).await;
    let current = host.read(|world| current_witness(world, task, run, witness));
    let Some((generation, head, check)) = current else {
        return Ok(None);
    };
    let _slot = host
        .witness_slots()
        .acquire_owned()
        .await
        .expect("the witness semaphore is never closed");
    let worktree = workspace.worktree(task, generation).await?;
    let built = worktree.head().await?;
    if built != head {
        return Ok(Some(WitnessOutcome::Error {
            detail: format!("the task branch is at {built}, not the gated commit {head}"),
        }));
    }
    Ok(Some(
        acan_witness::run(&check, worktree.path(), &host.witness_limits()).await,
    ))
}

/// The generation, commit and check of `run`, when it is still `task`'s current gate run and
/// `witness` is still frozen in it. A run that closed, or a generation that moved on, is `None`.
fn current_witness(
    world: &World,
    task: TaskId,
    run: GateRunId,
    witness: WitnessId,
) -> Option<(Generation, CommitId, Check)> {
    let gate = world.gate(run)?;
    let current = world.task(task).is_some_and(|found| {
        matches!(
            &found.state,
            TaskState::Claimed {
                stage: Stage::Gating { run: live, .. },
                ..
            } if *live == run
        )
    });
    let generation_holds = world.task(task).is_some_and(|found| {
        matches!(
            &found.state,
            TaskState::Claimed { claim, .. } if claim.lease.generation == gate.generation
        )
    });
    if !current || !generation_holds {
        return None;
    }
    let WitnessKind::Check { check } = &world.witness(witness)?.kind else {
        return None;
    };
    Some((gate.generation, gate.head.clone(), check.clone()))
}

/// Merges the gated commit at the front of the land queue into the protected branch. The core
/// derives a `Land` only for that front, so merges happen one at a time, in queue order.
///
/// A scope-first task lands through the final gate instead: see [`gated_land`].
async fn land<C: ModelClient>(
    host: &Host<C>,
    task: TaskId,
    head: &CommitId,
) -> Result<Option<SystemCmd>, JobError> {
    let result = match host.workspace() {
        Some(workspace) => match host.read(|world| landing(world, task)) {
            Landing::Gated(delivery) => {
                match gated_land(host, &workspace, &delivery, head).await? {
                    Some(result) => result,
                    // The scope was re-bound to the new head. The candidate is prepared again, and
                    // this land decided nothing, so it records no result.
                    None => return Ok(None),
                }
            }
            Landing::Legacy => legacy_land(host, &workspace, task, head).await?,
            Landing::Refused(reason) => Err(reason),
        },
        None => Err("this server has no git repository to land in".to_owned()),
    };
    if let Err(reason) = &result {
        tracing::warn!(%task, %head, %reason, "the land was refused");
    }
    Ok(Some(SystemCmd::LandResult {
        task,
        head: head.clone(),
        result,
    }))
}

/// The legacy land: merges the worker's head as it is. `Err` is a refusal the origin records;
/// the outer error is a git failure the job retries.
async fn legacy_land<C: ModelClient>(
    host: &Host<C>,
    workspace: &Arc<Workspace>,
    task: TaskId,
    head: &CommitId,
) -> Result<Result<CommitId, String>, JobError> {
    match workspace.land(task, head).await {
        Ok(commit) => {
            catch_up(host, workspace, &commit).await;
            Ok(Ok(commit))
        }
        Err(LandFailure::Refused(reason)) => Ok(Err(reason)),
        Err(LandFailure::Git(err)) => Err(JobError::Git(err)),
    }
}

/// Moves the worktree of every task not yet cleared to write onto the commit that just landed.
///
/// This runs before the land result is recorded. A `Serialize` or `SplitAtContract` waiter is
/// cleared to build only once that result marks its predecessor landed, so its agent starts on
/// the landed code and its own land does not conflict with it. A worker writes nothing before
/// its task is building, and a worktree with work of its own is left where it is. A failure is
/// logged and does not undo the land: the waiter then builds on its old base, and a conflicting
/// land is refused and reopens it.
async fn catch_up<C: ModelClient>(host: &Host<C>, workspace: &Workspace, landed: &CommitId) {
    for (task, generation) in host.read(not_yet_building) {
        // Under the task lock the generation cannot move, so the worktree of a newer claim is
        // never reset back to this one.
        let _task = workspace.lock_task(task).await;
        let still = host.read(|world| not_yet_building(world).get(&task) == Some(&generation));
        if !still {
            continue;
        }
        match workspace.fast_forward(task, generation, landed).await {
            Ok(true) => tracing::debug!(%task, %landed, "moved a waiting worktree onto the land"),
            Ok(false) => tracing::info!(
                %task,
                %landed,
                "a waiting worktree has work of its own; it stays on its base"
            ),
            Err(err) => tracing::warn!(
                %task,
                error = %err,
                "moving a waiting worktree onto the land failed; it stays on its base"
            ),
        }
    }
}

/// The lease generation of each claimed task that is not building yet.
fn not_yet_building(world: &World) -> HashMap<TaskId, Generation> {
    world
        .orders()
        .flat_map(move |order| world.tasks_of(order.id))
        .filter_map(|task| match &task.state {
            TaskState::Claimed {
                claim,
                stage: Stage::Declaring | Stage::Declared { .. },
            } => Some((task.id, claim.lease.generation)),
            _ => None,
        })
        .collect()
}

/// One scope-first run, copied out of the world so no lock is held while it runs.
struct Contrast {
    order: OrderId,
    task: TaskId,
    head: CommitId,
    prepared: Option<PreparedCommit>,
    scope: Scope,
    confirmation: ScopeConfirmation,
}

/// The pending run `run`, when it still is pending and its order is confirmed.
fn contrast_of(world: &World, run: GateRunId) -> Option<Contrast> {
    let pending = world
        .pending_contrasts()
        .into_iter()
        .find(|pending| pending.run == run)?;
    let lane = world.lane(pending.order)?;
    Some(Contrast {
        order: pending.order,
        task: pending.task,
        head: pending.head.clone(),
        prepared: pending.prepared.cloned(),
        scope: lane.scope.clone(),
        confirmation: lane.confirmation.clone()?,
    })
}

/// How a queued task lands. Only an order with no scope lane takes the legacy merge; a
/// scope-first order whose evidence is missing is refused, so nothing installs.
enum Landing {
    /// No scope lane: the legacy merge of the worker's head.
    Legacy,
    /// Scope-first: the receipt the run passed with, and the scope it was judged against.
    Gated(Box<Delivery>),
    /// Scope-first evidence is missing or no longer confirmed. The reason is the land refusal.
    Refused(String),
}

fn landing(world: &World, task: TaskId) -> Landing {
    let Some(found) = world.task(task) else {
        return Landing::Refused(format!("task {task} is not in the origin"));
    };
    if world.lane(found.order).is_none() {
        return Landing::Legacy;
    }
    match Delivery::of(world, task) {
        Ok(delivery) => Landing::Gated(Box::new(delivery)),
        Err(reason) => Landing::Refused(reason),
    }
}

/// The files one observe pass works in: the base it starts from, the protected inputs of that
/// base, and directories no other pass uses.
struct Pass {
    base: Preparation,
    snapshot: PathBuf,
    app: AppContract,
    corpus: ProbeCorpus,
    policy: ContainerPolicy,
    runner: RunnerConfig,
    scratch: PathBuf,
    observations: PathBuf,
    /// The host's container permits, shared with the witness runs.
    slots: Arc<Semaphore>,
    /// How many probes of the contrast run at once.
    probes_at_once: usize,
}

impl Pass {
    /// Starts a pass on the protected head as it is now. `label` names the pass directory, so a
    /// gate run and a scope confirmation each get their own.
    async fn new<C: ModelClient>(
        host: &Host<C>,
        repo: &Repo,
        label: impl std::fmt::Display,
    ) -> Result<Self, JobError> {
        let policy = observe::runner_policy();
        let base = protected_base(repo, &policy).await?;
        let state = host.repo().join(STATE_DIR);
        let dir = state
            .join(CONTRAST_DIR)
            .join(format!("{label}-{}", unique_suffix()));
        let snapshot = dir.join("snapshot");
        observe::snapshot(repo, &base, &snapshot).await?;
        let (app, corpus) = observe::read_protected(&snapshot)?;
        Ok(Self {
            base,
            snapshot,
            app,
            corpus,
            policy,
            runner: RunnerConfig::new(RUNNER_PROGRAM, APP_PORT),
            scratch: dir.join("scratch"),
            observations: state.join(OBSERVATIONS_DIR),
            slots: host.witness_slots(),
            probes_at_once: host.probe_concurrency(),
        })
    }

    /// The plan that observes `candidate` on this pass's base, writing into `scratch`.
    fn plan<'a>(
        &'a self,
        repo: &'a Repo,
        candidate: &'a Preparation,
        scratch: &'a Path,
    ) -> Plan<'a> {
        Plan {
            repo,
            base: &self.base,
            candidate,
            snapshot: &self.snapshot,
            scratch,
            observations: &self.observations,
            app: &self.app,
            corpus: &self.corpus,
            policy: &self.policy,
            runner: &self.runner,
            slots: self.slots.as_ref(),
            probes_at_once: self.probes_at_once,
        }
    }
}

/// The episodes of `order` as the judge reads them. A repeat of a failure is judged with the state
/// and bounce count the repair loop holds, so a second or third bounce counts as a repeat of the
/// same episode and not as a first failure.
fn judged_episodes<C: ModelClient>(
    host: &Host<C>,
    order: OrderId,
    run: GateRunId,
) -> Result<Vec<DiscrepancyEpisode>, JobError> {
    host.read(|world| world.judged_episodes(order))
        .map_err(|err| {
            JobError::Unavailable(format!("the episodes of run {run} do not read: {err:?}"))
        })
}

/// Runs one pending scope-first run as far as the origin's state allows.
///
/// A run without a recorded preparation is prepared and its context recorded. A run with one
/// keeps that preparation: the commit it recorded must still be the one git prepares, or the run
/// fails rather than judging a different commit. Then the candidate is observed and judged, and
/// the receipt is recorded. The context must still match the recorded one, or the origin would
/// refuse the receipt.
async fn contrast<C: ModelClient>(host: &Host<C>, run: GateRunId) -> Result<(), JobError> {
    let Some(workspace) = host.workspace() else {
        return Err(JobError::Unavailable(
            "this server has no git repository to gate candidates in".to_owned(),
        ));
    };
    let Some(contrast) = host.read(|world| contrast_of(world, run)) else {
        tracing::info!(%run, "a scope-first run is no longer pending");
        return Ok(());
    };
    tracing::debug!(%run, prepared = contrast.prepared.is_some(), "a scope-first run starts");
    let repo = workspace.repo();
    let pass = Pass::new(host, repo, run).await?;
    let candidate = match prepare_candidate(repo, &pass, &contrast.head).await? {
        Ok(candidate) => candidate,
        Err(files) => {
            // The candidate cannot be tested on the protected head. The origin refuses the run,
            // names the files, and reopens the task. Nothing is installed.
            tracing::info!(%run, files = files.len(), "the candidate conflicts with the protected head");
            host.submit(Command::System(SystemCmd::CandidateConflicted {
                run,
                files,
            }))
            .await?;
            return Ok(());
        }
    };
    tracing::debug!(%run, "the candidate is prepared on the protected head");
    let ledger = host.folded_rulings().map_err(JobError::Rulings)?;
    let episodes = judged_episodes(host, contrast.order, run)?;
    let order = Order {
        scope: &contrast.scope,
        confirmation: &contrast.confirmation,
        ledger: &ledger,
        episodes: &episodes,
    };
    // The protected head moved after the scope was confirmed. The run is not prepared on the new
    // head under the old confirmation. The scope is re-bound to the new head first, and the next
    // pass prepares the run under the re-bound confirmation. The core refuses the re-bind when
    // a protected input moved, so a human must confirm again.
    if pass.base.protected_head() != &contrast.confirmation.base_commit {
        let scratch = pass.scratch.join("context");
        let plan = pass.plan(repo, &candidate, &scratch);
        let context = observe::current(&plan, &order).await?;
        let rebind = SystemCmd::RebindBase {
            order: contrast.order,
            base: pass.base.protected_head().clone(),
            context: Box::new(context),
        };
        match host.submit(Command::System(rebind)).await {
            Ok(_) => tracing::info!(%run, "the scope is re-bound to the moved protected head"),
            Err(SubmitError::Rejected(rejection)) => {
                // The origin will not re-bind the scope, so the run cannot be tested on the moved
                // head. It is refused now: the lease ends and the task reopens with the reason.
                // Waiting for the lease to expire would hold the task for nothing.
                tracing::info!(%run, "the origin refused the re-bind to the moved protected head");
                host.submit(Command::System(SystemCmd::RefuseRun {
                    run,
                    reason: format!(
                        "the protected head moved and the scope could not be re-bound to it: {rejection}"
                    ),
                }))
                .await?;
            }
            Err(err) => return Err(err.into()),
        }
        return Ok(());
    }
    let recorded = match contrast.prepared {
        Some(prepared) => {
            if candidate.merged_commit() != &prepared.context.merged_commit
                || pass.base.protected_head() != &prepared.base_commit
            {
                return Err(JobError::Unavailable(format!(
                    "run {run} was prepared on another commit or base than git prepares now"
                )));
            }
            prepared
        }
        None => {
            let scratch = pass.scratch.join("context");
            let plan = pass.plan(repo, &candidate, &scratch);
            let context = observe::current(&plan, &order).await?;
            let prepared = PreparedCommit {
                base_commit: pass.base.protected_head().clone(),
                context,
            };
            host.submit(Command::System(SystemCmd::CandidatePrepared {
                run,
                prepared: Box::new(prepared.clone()),
            }))
            .await?;
            prepared
        }
    };
    let scratch = pass.scratch.join("judge");
    let plan = pass.plan(repo, &candidate, &scratch);
    tracing::debug!(%run, "the candidate is observed and judged");
    // The judge's future is large, so it is boxed where it is awaited.
    let judged = Box::pin(observe::judge(&plan, &order)).await?;
    tracing::info!(%run, items = judged.receipt.items.len(), "the receipt is built");
    if judged.context != recorded.context {
        return Err(JobError::Unavailable(format!(
            "the context of run {run} moved while the candidate was judged"
        )));
    }
    // An item with no selector cannot name an episode, and the origin refuses a failing item
    // without one. That item is an unknown probe outcome, not a verdict, so the run is judged
    // again instead of recording a receipt the origin would refuse.
    let unnamed: Vec<String> = judged
        .receipt
        .items
        .iter()
        .filter(|item| !item.class.counts_toward_pass() && item.episode_id.is_none())
        .map(|item| format!("{} ({:?})", item.item_id, item.class))
        .collect();
    if !unnamed.is_empty() {
        return Err(JobError::Unavailable(format!(
            "run {run} has failing items that name no episode: {}",
            unnamed.join(", ")
        )));
    }
    // The examples the receipt's episodes name are kept before the receipt is recorded, so a human
    // can rule on each episode. Only the observed runs of this pass are kept. A store that fails
    // fails the run, so no receipt names an example that is not kept.
    examples::retain(
        &host.repo().join(STATE_DIR).join(examples::DIR),
        run,
        &judged,
    )
    .map_err(|err| JobError::Unavailable(format!("run {run}'s examples do not keep: {err}")))?;
    host.submit(Command::System(SystemCmd::ReceiptReady {
        run,
        receipt: Box::new(judged.receipt),
    }))
    .await?;
    // The receipt is recorded, so the gate's outcome is fixed before the advisory witness runs,
    // and the witness reports its own failures instead of returning them.
    if let Some(vision) = host.vision() {
        let scratch = pass.scratch.join("vision");
        let plan = pass.plan(repo, &candidate, &scratch);
        Box::pin(vision::witness(
            host,
            vision,
            &plan,
            contrast.task,
            run,
            &contrast.scope,
        ))
        .await;
    }
    Ok(())
}

/// The command that records the protected inputs of the scope `order` was just confirmed under, at
/// the base the confirmation names. `None` when nothing is recorded: there is no repository, or
/// the protected head is no longer that base, so the inputs would describe another commit.
///
/// # Errors
///
/// Returns an error when git or the ruling log cannot be read.
pub(crate) async fn confirmed_inputs_command<C: ModelClient>(
    host: &Host<C>,
    order: OrderId,
    confirmation: &ScopeConfirmation,
) -> anyhow::Result<Option<SystemCmd>> {
    let Some(workspace) = host.workspace() else {
        return Ok(None);
    };
    let repo = workspace.repo();
    let pass = Pass::new(host, repo, format!("confirm-{order}")).await?;
    let command = confirmed_inputs(host, &pass, repo, order, confirmation);
    // The pass is only read for its inputs, so its directory goes once the record is built.
    discard_pass(&pass);
    command
}

/// The record of the protected inputs `confirmation` was confirmed under, read from `pass` on the
/// protected head as it is now.
fn confirmed_inputs<C: ModelClient>(
    host: &Host<C>,
    pass: &Pass,
    repo: &Repo,
    order: OrderId,
    confirmation: &ScopeConfirmation,
) -> anyhow::Result<Option<SystemCmd>> {
    if pass.base.protected_head() != &confirmation.base_commit {
        tracing::info!(
            %order,
            "the protected head moved after the scope was confirmed: nothing is recorded"
        );
        return Ok(None);
    }
    let Some(scope) = host.read(|world| world.lane(order).map(|lane| lane.scope.clone())) else {
        return Ok(None);
    };
    let ledger = host.folded_rulings().map_err(JobError::Rulings)?;
    let plan = pass.plan(repo, &pass.base, &pass.scratch);
    let view = Order {
        scope: &scope,
        confirmation,
        ledger: &ledger,
        episodes: &[],
    };
    let inputs = observe::protected_inputs(&plan, &view)?;
    Ok(Some(SystemCmd::RecordConfirmedInputs {
        order,
        revision: confirmation.scope_rev,
        base: confirmation.base_commit.clone(),
        inputs: Box::new(inputs),
    }))
}

/// Removes the directory of a pass that is finished with. The pass is only read, so nothing is
/// lost. A failed removal is logged and not returned, as a partial snapshot's is.
fn discard_pass(pass: &Pass) {
    let Some(dir) = pass.snapshot.parent() else {
        return;
    };
    if let Err(err) = fs::remove_dir_all(dir) {
        tracing::warn!(path = %dir.display(), error = %err, "a finished pass directory stays behind");
    }
}

/// Lands a scope-first task. The delivery is checked again under the landing fence before any
/// git work, so a revision or cancel that landed after the queue was read refuses it. The final
/// gate then reads the receipt its run passed with and the context computed now, and the tested
/// merged commit is installed only when both clear it. A refusal is a land result the origin
/// records, so the queue moves on. A git failure is the job's error and follows the retry path.
async fn gated_land<C: ModelClient>(
    host: &Host<C>,
    workspace: &Arc<Workspace>,
    delivery: &Delivery,
    head: &CommitId,
) -> Result<Option<Result<CommitId, String>>, JobError> {
    let repo = workspace.repo();
    // The fence holds every command that can change the delivery from the check below through
    // the install, so the check never passes for a state the install then ignores. The writes
    // lock keeps a legacy land from moving the branch in between. Install still compares the
    // head itself.
    let fence = host.fence_landings().await;
    let writes = workspace.lock_writes().await;
    if let Err(reason) = host.read(|world| delivery.stands(world)) {
        return Ok(Some(Err(reason)));
    }
    // The receipt names the merged commit the land installs. It was persisted before any install,
    // so it is the install intent. When the branch already holds that commit, an earlier install
    // ran and a crash kept its result from being recorded. That install is this land, so it is
    // reported as landed, and nothing is prepared again.
    let tested = &delivery.receipt.prepared.context.merged_commit;
    if protected_sha(repo).await? == *tested {
        let blockers = land::blockers(&delivery.receipt);
        if !blockers.is_empty() {
            return Ok(Some(Err(refusal(&LandError::Blocked(blockers)))));
        }
        drop(writes);
        drop(fence);
        tracing::info!(
            task = %delivery.task,
            commit = tested.as_str(),
            "the protected branch holds the tested commit; the install is recorded as landed"
        );
        let landed = landed_commit(tested)?;
        catch_up(host, workspace, &landed).await;
        return Ok(Some(Ok(landed)));
    }
    let pass = Pass::new(host, repo, delivery.run).await?;
    let candidate = match prepare_candidate(repo, &pass, head).await? {
        Ok(candidate) => candidate,
        // A conflict refuses the land, so the origin reopens the task with the files named.
        Err(files) => {
            drop(writes);
            drop(fence);
            return Ok(Some(Err(conflict_reason(head, &files))));
        }
    };
    let ledger = host.folded_rulings().map_err(JobError::Rulings)?;
    let episodes = judged_episodes(host, delivery.order, delivery.run)?;
    let order = Order {
        scope: &delivery.scope,
        confirmation: &delivery.confirmation,
        ledger: &ledger,
        episodes: &episodes,
    };
    let scratch = pass.scratch.join("land");
    let plan = pass.plan(repo, &candidate, &scratch);
    let current = observe::current(&plan, &order).await?;
    // Only the protected head moved since the receipt: the core may re-bind the scope to it, and
    // the candidate is then prepared and receipted again. The receipt here is never installed.
    if current.protected_head != delivery.receipt.prepared.base_commit {
        let rebound = rebind(host, delivery, &current).await;
        drop(writes);
        drop(fence);
        return rebound;
    }
    let installed = match land::final_gate(&delivery.receipt, &current, &candidate) {
        Ok(clearance) => land::install(repo, clearance).await,
        Err(err) => Err(err),
    };
    drop(writes);
    drop(fence);
    match installed {
        Ok(commit) => {
            let landed = landed_commit(&commit)?;
            catch_up(host, workspace, &landed).await;
            Ok(Some(Ok(landed)))
        }
        Err(LandError::Git(err)) => Err(JobError::Git(err)),
        Err(err) => Ok(Some(Err(refusal(&err)))),
    }
}

/// The protected head moved after the candidate was prepared. The core re-binds the confirmed
/// scope to `current`'s head only when nothing else the run was tested under moved. The re-bind
/// clears the run's prepared commit and receipt, so the run is prepared and receipted again on
/// the new head; the old receipt is never reused. Otherwise the core refuses, and a human must
/// confirm the scope again.
///
/// Returns `None` when the scope was re-bound: the land decided nothing, so it records no result.
async fn rebind<C: ModelClient>(
    host: &Host<C>,
    delivery: &Delivery,
    current: &GateContext,
) -> Result<Option<Result<CommitId, String>>, JobError> {
    let command = SystemCmd::RebindBase {
        order: delivery.order,
        base: current.protected_head.clone(),
        context: Box::new(current.clone()),
    };
    match host.submit(Command::System(command)).await {
        Ok(_) => Ok(None),
        Err(SubmitError::Rejected(rejection)) => Ok(Some(Err(format!(
            "the protected head moved and the scope must be confirmed again: {rejection}"
        )))),
        Err(SubmitError::Internal(err)) => Err(JobError::Unavailable(format!(
            "re-binding the scope to the protected head: {err}"
        ))),
    }
}

/// Prepares `head` on the pass's protected head. The inner `Err` is a conflict: the files that
/// clash with the protected head. The candidate then has no merged commit to test, and the
/// caller refuses it. The outer error is a git failure or a head that is not a commit, which the
/// job retries.
async fn prepare_candidate(
    repo: &Repo,
    pass: &Pass,
    head: &CommitId,
) -> Result<Result<Preparation, Vec<RepoPath>>, JobError> {
    let commit = CommitSha::from_str(&head.to_string())
        .map_err(|err| JobError::Unavailable(format!("the candidate is not a commit: {err}")))?;
    match repo
        .prepare(&commit, &observe::protected_paths(&pass.policy))
        .await?
    {
        PrepareOutcome::Prepared(candidate) => Ok(Ok(candidate)),
        PrepareOutcome::Conflict { files, .. } => Ok(Err(files)),
        // The outcome enum is non-exhaustive: an outcome this build does not know is not a
        // candidate to test, so it fails closed and is retried.
        _ => Err(JobError::Unavailable(format!(
            "candidate {head} has no prepared outcome on the protected head"
        ))),
    }
}

/// Why a candidate that conflicts with the protected head was refused, in the words the origin
/// records. It names the commit and the conflicting files.
fn conflict_reason(head: &CommitId, files: &[RepoPath]) -> String {
    let names: Vec<String> = files.iter().map(ToString::to_string).collect();
    format!(
        "candidate {head} conflicts with the protected head: {}",
        names.join(", ")
    )
}

/// The protected head as a commit sha.
async fn protected_sha(repo: &Repo) -> Result<CommitSha, JobError> {
    let head = repo.protected_head().await?;
    CommitSha::from_str(&head.to_string())
        .map_err(|err| JobError::Unavailable(format!("the protected head: {err}")))
}

/// The protected head prepared on itself: the base every gate pass starts from.
async fn protected_base(repo: &Repo, policy: &ContainerPolicy) -> Result<Preparation, JobError> {
    let head = protected_sha(repo).await?;
    let PrepareOutcome::Prepared(base) = repo
        .prepare(&head, &observe::protected_paths(policy))
        .await?
    else {
        return Err(JobError::Unavailable(
            "the protected head does not prepare onto itself".to_owned(),
        ));
    };
    Ok(base)
}

/// The protected-head snapshot a task's worktree is tested against: the protected paths of the
/// protected head as it is now, read from the head's own tree.
///
/// The snapshot sits under `state`, one directory per protected-head commit. A head that has not
/// moved returns the directory already written. A new head is written under a temporary name and
/// renamed into place once complete, so a directory a container mounts is never rewritten.
///
/// # Errors
///
/// Returns an error when git cannot read the protected head or the snapshot cannot be written.
pub(crate) async fn protected_snapshot(repo: &Repo, state: &Path) -> anyhow::Result<PathBuf> {
    let head = protected_sha(repo).await?;
    let export = repo
        .export_protected(&head, &observe::protected_paths(&observe::runner_policy()))
        .await?;
    snapshot_of(repo, state, &export, &format!("head-{head}")).await
}

/// The protected paths a receipt ran against, read from the receipt's base commit and written
/// under `state`. A plant view reads the corpus through this, so a receipt keeps its own route
/// table after the protected head moves on.
///
/// The paths are read from `prepared.base_commit`, never from the protected head now. They are
/// refused unless they hash to the overlay the receipt recorded, so a snapshot that does not
/// match the receipt is never written.
///
/// # Errors
///
/// Returns an error when the receipt does not name its base as the protected head it ran on,
/// when the paths at its base do not hash to its overlay, or when git or the snapshot fails.
pub(crate) async fn receipt_snapshot(
    repo: &Repo,
    state: &Path,
    prepared: &PreparedCommit,
) -> anyhow::Result<PathBuf> {
    let base = &prepared.base_commit;
    if prepared.context.protected_head != *base {
        anyhow::bail!("the receipt's protected head is not its base commit {base}");
    }
    let export = repo
        .export_protected(base, &observe::protected_paths(&observe::runner_policy()))
        .await?;
    if export.overlay_hash() != prepared.context.overlay_hash {
        anyhow::bail!(
            "the protected paths at {base} do not hash to the overlay the receipt ran against"
        );
    }
    snapshot_of(repo, state, &export, &format!("receipt-{base}")).await
}

/// Writes `export`'s protected paths under `state`, one directory per name. A name that is
/// already written returns its directory. The names keep the snapshots of heads and of receipts
/// apart, since they are read through different checks.
async fn snapshot_of(
    repo: &Repo,
    state: &Path,
    export: &ProtectedExport,
    name: &str,
) -> anyhow::Result<PathBuf> {
    let root = state.join(SNAPSHOT_DIR);
    let dir = root.join(name);
    if dir.is_dir() {
        return Ok(dir);
    }
    fs::create_dir_all(&root)?;
    let partial = root.join(format!("{name}-{}.partial", unique_suffix()));
    observe::export_snapshot(repo, export, &partial).await?;
    match fs::rename(&partial, &dir) {
        Ok(()) => Ok(dir),
        // Another caller renamed the same head's snapshot into place first. Its copy is complete.
        Err(_) if dir.is_dir() => {
            discard_partial(&partial);
            Ok(dir)
        }
        Err(err) => {
            discard_partial(&partial);
            Err(err.into())
        }
    }
}

/// Removes a partial snapshot that did not reach its final name. The leftover is only clutter, so
/// a failed removal is logged and not returned.
fn discard_partial(partial: &Path) {
    if let Err(err) = fs::remove_dir_all(partial) {
        tracing::warn!(path = %partial.display(), error = %err, "a partial snapshot stays behind");
    }
}

/// The commit a land records as landed, from the commit it installed.
fn landed_commit(commit: &CommitSha) -> Result<CommitId, JobError> {
    CommitId::from_str(commit.as_str())
        .map_err(|err| JobError::Unavailable(format!("the installed commit: {err}")))
}

/// Why a land was refused, one reason per line, in the words the origin records.
fn refusal(err: &LandError) -> String {
    match err {
        LandError::Blocked(blockers) => blockers
            .iter()
            .map(|blocker| match &blocker.item {
                Some(item) => format!("{item}: {}", blocker.reason),
                None => blocker.reason.clone(),
            })
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    }
}

/// A suffix no earlier pass directory has, so a pass never reuses another pass's worktrees.
fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

async fn record_usage<C: ModelClient>(host: &Host<C>, job: &Job, call: &CallRecord, step: Step) {
    let command = SystemCmd::ModelUsed {
        model: call.model.clone(),
        step,
        input_tokens: call.usage.prompt_tokens,
        output_tokens: call.usage.completion_tokens,
    };
    // Metering is best-effort: a lost usage event must not retry the model call it paid for.
    if let Err(err) = host.submit(Command::System(command)).await {
        tracing::warn!(?job, error = %err, "recording model usage failed");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use acan_intent::OverlayHash;
    use serde_json::{Value, json};

    use super::*;

    const RECEIPT: &str = include_str!("../../acan-intent/fixtures/receipt.json");
    const ROUTES: &str = ".acan/probes/core.json";

    /// Runs git in `dir`, failing the test on a non-zero exit.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Writes `files` into `root` and commits them on the current branch.
    fn commit(root: &Path, files: &[(&str, &str)], message: &str) -> CommitSha {
        for (name, content) in files {
            let full = root.join(name);
            fs::create_dir_all(full.parent().expect("a parent directory")).expect("mkdir");
            fs::write(full, content).expect("write file");
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", message]);
        CommitSha::from_str(&git(root, &["rev-parse", "HEAD"])).expect("a commit sha")
    }

    /// A repository on `main` whose protected paths carry two routes, and whose head then gains a
    /// third route with the same count: the route table changes, its size does not. Returns the
    /// repository, the commit the receipt ran on, and the head now.
    async fn advanced_head() -> (tempfile::TempDir, Repo, CommitSha, CommitSha) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.name", "Tester"]);
        git(root, &["config", "user.email", "tester@example.com"]);
        let base = commit(
            root,
            &[(".acan/app.json", "{}\n"), (ROUTES, "routes: a, b\n")],
            "protected inputs",
        );
        let head = commit(
            root,
            &[(ROUTES, "routes: a, c\n"), ("src/app.ts", "app v2\n")],
            "the routes change",
        );
        let repo = Repo::open(root).await.expect("open the repository");
        (dir, repo, base, head)
    }

    /// A receipt's prepared commit that names `base` and `overlay`, built from the fixture.
    fn prepared_at(base: &CommitSha, overlay: OverlayHash) -> PreparedCommit {
        let receipt: Value = serde_json::from_str(RECEIPT).expect("the fixture decodes");
        let mut prepared = receipt["prepared"].clone();
        prepared["base_commit"] = json!(base.as_str());
        prepared["context"]["protected_head"] = json!(base.as_str());
        prepared["context"]["overlay_hash"] = serde_json::to_value(overlay).expect("a hash");
        serde_json::from_value(prepared).expect("the prepared commit decodes")
    }

    fn policy_paths() -> acan_git::ProtectedPaths {
        observe::protected_paths(&observe::runner_policy())
    }

    #[tokio::test]
    async fn a_receipt_reads_the_protected_paths_of_its_base_after_the_head_moves() {
        let (dir, repo, base, head) = advanced_head().await;
        let at_base = repo
            .export_protected(&base, &policy_paths())
            .await
            .expect("the base exports");
        let prepared = prepared_at(&base, at_base.overlay_hash());
        let state = dir.path().join("state");

        let snapshot = receipt_snapshot(&repo, &state, &prepared)
            .await
            .expect("the receipt's snapshot");
        let current = protected_snapshot(&repo, &state)
            .await
            .expect("the head's snapshot");

        assert_eq!(
            fs::read_to_string(snapshot.join(ROUTES)).expect("the routes"),
            "routes: a, b\n",
            "the receipt keeps the routes it ran against"
        );
        assert_eq!(
            fs::read_to_string(current.join(ROUTES)).expect("the routes"),
            "routes: a, c\n",
            "the head keeps its own routes"
        );
        assert_ne!(base, head);
    }

    #[tokio::test]
    async fn a_receipt_whose_overlay_the_base_does_not_hold_is_refused() {
        let (dir, repo, base, head) = advanced_head().await;
        let at_head = repo
            .export_protected(&head, &policy_paths())
            .await
            .expect("the head exports");
        // The receipt names the old base but carries the head's overlay, the mismatch the snapshot
        // used to hide by reading the head's files under the base's name.
        let prepared = prepared_at(&base, at_head.overlay_hash());
        let state = dir.path().join("state");

        let err = receipt_snapshot(&repo, &state, &prepared)
            .await
            .expect_err("the overlay does not match the base");

        assert!(
            err.to_string().contains("do not hash to the overlay"),
            "{err}"
        );
        assert!(
            !state
                .join(SNAPSHOT_DIR)
                .join(format!("receipt-{base}"))
                .exists(),
            "nothing is written for a receipt that does not match"
        );
    }
}
