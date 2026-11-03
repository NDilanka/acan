//! The writer: the one task that owns the origin and the current scan.
//!
//! HTTP handlers and the job loop send requests over a channel and await a oneshot reply.
//! Dropping a reply never cancels an accepted write: the writer stages the next origin on a
//! clone, persists the batch, and only then swaps the origin in and publishes the new snapshot.
//! Readers use the published snapshot, so a slow database write never blocks a read. When a
//! persist fails the staged origin is discarded and the writer reads the log back: if the log
//! still ends at the committed world it keeps serving, and if it cannot tell what is durable it
//! refuses every later write, because its world may no longer match the log.

use std::sync::Arc;

use acan_core::{Command, Ctx, Event, Origin, Rejection, Stamped, SystemCmd, World};
use anyhow::Context as _;
use tokio::sync::{Mutex, Notify, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::host::{SubmitError, now};
use crate::index::Scan;
use crate::store::EventStore;

/// Writes the writer queues before handlers wait. Larger than the live broadcast buffer, so a
/// burst of commands queues on the writer instead of failing.
const QUEUE: usize = 256;

/// The world after the last persisted batch, paired with the scan it was decided against.
#[derive(Debug)]
pub struct Snapshot {
    /// The committed world.
    pub world: World,
    /// The scan matching the world's index revision. A failed rescan never reaches a snapshot.
    pub scan: Arc<Scan>,
}

/// A check the writer runs against the committed world in the same turn that it decides the command
/// it guards. A refusal is the origin's answer: nothing is decided, stored or published.
pub(crate) type Guard = Box<dyn FnOnce(&World) -> Result<(), Rejection> + Send>;

enum Request {
    Command {
        command: Command,
        guard: Option<Guard>,
        reply: oneshot::Sender<Result<Vec<Stamped>, SubmitError>>,
    },
    Advisory {
        command: Command,
        reply: oneshot::Sender<Result<Vec<Stamped>, SubmitError>>,
    },
    Rescan {
        scan: Scan,
        reply: oneshot::Sender<Result<Vec<Stamped>, SubmitError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// How the origin decides a command.
#[derive(Debug, Clone, Copy)]
enum Mode {
    /// Lease, barrier, clash and build maintenance run around the command.
    Ordinary,
    /// Only the command is decided; see [`Origin::execute_advisory`].
    Advisory,
}

/// A handle to the writer task. Cloning shares the same task.
pub struct Writer {
    tx: mpsc::Sender<Request>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer").finish_non_exhaustive()
    }
}

impl Clone for Writer {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            task: Arc::clone(&self.task),
        }
    }
}

impl Writer {
    pub(crate) fn spawn(
        origin: Origin,
        scan: Arc<Scan>,
        store: EventStore,
        events: broadcast::Sender<Stamped>,
        wake: Arc<Notify>,
        snapshots: watch::Sender<Arc<Snapshot>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        let task = tokio::spawn(run(rx, origin, scan, store, events, wake, snapshots));
        Self {
            tx,
            task: Arc::new(Mutex::new(Some(task))),
        }
    }

    /// Whether the writer task has stopped. A stopped writer takes no command, so its project
    /// cannot change until the server restarts.
    pub(crate) fn is_stopped(&self) -> bool {
        self.tx.is_closed()
    }

    pub(crate) async fn submit(&self, command: Command) -> Result<Vec<Stamped>, SubmitError> {
        self.submit_with(command, None).await
    }

    pub(crate) async fn submit_with(
        &self,
        command: Command,
        guard: Option<Guard>,
    ) -> Result<Vec<Stamped>, SubmitError> {
        self.call(|reply| Request::Command {
            command,
            guard,
            reply,
        })
        .await
    }

    pub(crate) async fn submit_advisory(
        &self,
        command: Command,
    ) -> Result<Vec<Stamped>, SubmitError> {
        self.call(|reply| Request::Advisory { command, reply })
            .await
    }

    pub(crate) async fn rescan(&self, scan: Scan) -> Result<Vec<Stamped>, SubmitError> {
        self.call(|reply| Request::Rescan { scan, reply }).await
    }

    async fn call(
        &self,
        request: impl FnOnce(oneshot::Sender<Result<Vec<Stamped>, SubmitError>>) -> Request,
    ) -> Result<Vec<Stamped>, SubmitError> {
        let (reply, wait) = oneshot::channel();
        self.tx
            .send(request(reply))
            .await
            .map_err(|_| SubmitError::Internal(anyhow::anyhow!("the writer task is gone")))?;
        wait.await.map_err(|_| {
            SubmitError::Internal(anyhow::anyhow!("the writer task dropped a reply"))
        })?
    }

    /// Drains every queued write and joins the task. After this returns, no write can still
    /// happen through any handle.
    ///
    /// # Errors
    ///
    /// Returns an error when the writer task panicked.
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        let Some(task) = self.task.lock().await.take() else {
            return Ok(());
        };
        let (reply, wait) = oneshot::channel();
        if self.tx.send(Request::Shutdown { reply }).await.is_ok() {
            let _ = wait.await;
        }
        task.await.context("the writer task did not finish")
    }
}

async fn run(
    mut rx: mpsc::Receiver<Request>,
    mut origin: Origin,
    mut scan: Arc<Scan>,
    store: EventStore,
    events: broadcast::Sender<Stamped>,
    wake: Arc<Notify>,
    snapshots: watch::Sender<Arc<Snapshot>>,
) {
    let mut broken = false;
    while let Some(request) = rx.recv().await {
        match request {
            Request::Shutdown { reply } => {
                let _ = reply.send(());
                return;
            }
            Request::Command {
                command,
                guard,
                reply,
            } => {
                let result = commit(
                    &mut origin,
                    &mut scan,
                    None,
                    command,
                    guard,
                    Mode::Ordinary,
                    &store,
                    &events,
                    &wake,
                    &snapshots,
                    &mut broken,
                )
                .await;
                // A gone receiver means the caller was cancelled; the write still stands.
                let _ = reply.send(result);
            }
            Request::Advisory { command, reply } => {
                let result = commit(
                    &mut origin,
                    &mut scan,
                    None,
                    command,
                    None,
                    Mode::Advisory,
                    &store,
                    &events,
                    &wake,
                    &snapshots,
                    &mut broken,
                )
                .await;
                let _ = reply.send(result);
            }
            Request::Rescan {
                scan: candidate,
                reply,
            } => {
                let result = commit(
                    &mut origin,
                    &mut scan,
                    Some(candidate),
                    Command::System(SystemCmd::IndexUpdated),
                    None,
                    Mode::Ordinary,
                    &store,
                    &events,
                    &wake,
                    &snapshots,
                    &mut broken,
                )
                .await;
                let _ = reply.send(result);
            }
        }
    }
}

/// Decides `command` in `mode`, persists its batch, and publishes the new snapshot, in that
/// order. A `candidate` scan is decided against and committed only together with its batch, so a
/// failed rescan keeps the previous origin and scan pair.
#[expect(clippy::too_many_arguments, reason = "one call site per request kind")]
async fn commit(
    origin: &mut Origin,
    scan: &mut Arc<Scan>,
    candidate: Option<Scan>,
    command: Command,
    guard: Option<Guard>,
    mode: Mode,
    store: &EventStore,
    broadcast_tx: &broadcast::Sender<Stamped>,
    wake: &Notify,
    snapshots: &watch::Sender<Arc<Snapshot>>,
    broken: &mut bool,
) -> Result<Vec<Stamped>, SubmitError> {
    if *broken {
        return Err(SubmitError::Internal(anyhow::anyhow!(
            "the event log no longer matches this server's world; restart the server"
        )));
    }
    let rescan = candidate.is_some();
    let staged_scan = candidate.map_or_else(|| Arc::clone(scan), Arc::new);
    let ctx = Ctx {
        now: now(),
        index: &staged_scan.index,
    };
    if let Some(guard) = guard {
        // Decided against the committed world, in this turn, so no write can land between the
        // check and the decision.
        guard(origin.world()).map_err(SubmitError::Rejected)?;
    }
    let mut staged = origin.clone();
    let batch = match mode {
        Mode::Ordinary => staged.execute(&ctx, command)?,
        Mode::Advisory => staged.execute_advisory(&ctx, command)?,
    };
    if batch.is_empty() {
        // An already-satisfied command changes nothing. A rescan to the same revision still
        // adopts the freshly scanned structures.
        if rescan {
            *scan = staged_scan;
            publish(snapshots, origin, scan);
        }
        return Ok(batch);
    }
    if let Err(err) = store.append(&batch).await {
        match reconcile(store, origin, &batch).await {
            Ok(Landed::No) => {
                tracing::warn!(error = ?err, "persisting a batch failed; nothing was stored");
                return Err(SubmitError::Internal(err));
            }
            Ok(Landed::Yes) => {
                tracing::warn!(error = ?err, "the append reported a failure but the batch is stored");
            }
            Err(reconcile_err) => {
                *broken = true;
                tracing::error!(
                    error = ?err,
                    reconcile = ?reconcile_err,
                    "persisting a batch failed and the log could not be checked; refusing further writes"
                );
                return Err(SubmitError::Internal(err));
            }
        }
    }
    *origin = staged;
    *scan = staged_scan;
    publish(snapshots, origin, scan);
    for event in &batch {
        // A send fails only when nobody is subscribed, which is fine.
        let _ = broadcast_tx.send(event.clone());
    }
    // Accounting-only batches must not wake the job loop: a job whose reply was unusable would
    // otherwise requeue itself and every other failed job without any user activity. An advisory
    // vision summary is no job's input either.
    if batch.iter().any(|stamped| {
        !matches!(
            &stamped.event,
            Event::ModelUsed { .. } | Event::VisionChecked { .. }
        )
    }) {
        wake.notify_one();
    }
    Ok(batch)
}

fn publish(snapshots: &watch::Sender<Arc<Snapshot>>, origin: &Origin, scan: &Arc<Scan>) {
    snapshots.send_replace(Arc::new(Snapshot {
        world: origin.world().clone(),
        scan: Arc::clone(scan),
    }));
}

/// Whether a batch whose append reported an error is in the log after all.
enum Landed {
    Yes,
    No,
}

/// Reads the log past the committed `origin` to learn what a failed append left behind. An
/// error from the append does not prove the transaction rolled back (the commit itself may
/// have reached disk), so the writer trusts only what the log now holds.
///
/// # Errors
///
/// Returns an error when the log cannot be read, or holds events past `origin` that are not
/// exactly `batch`. Either way the writer can no longer tell what is durable.
async fn reconcile(
    store: &EventStore,
    origin: &Origin,
    batch: &[Stamped],
) -> anyhow::Result<Landed> {
    let tail = store.load_from(origin.world().next_seq()).await?;
    if tail.is_empty() {
        Ok(Landed::No)
    } else if tail == batch {
        Ok(Landed::Yes)
    } else {
        anyhow::bail!(
            "the log holds {} unexpected events past the committed world",
            tail.len()
        )
    }
}
