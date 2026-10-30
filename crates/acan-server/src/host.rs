//! The host: the face of one world. Writes go to the writer task; reads come from the latest
//! published snapshot, so a slow persist never blocks a read and a cancelled caller never
//! cancels an accepted write.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use acan_core::{Command, HumanCmd, Origin, Rejection, Stamped, Timestamp, World};
use acan_intent::{CoreDerived, FoldedLedger, fold};
use acan_llm::{ModelClient, UsageLedger};
use anyhow::Context as _;
use tokio::sync::{Notify, Semaphore, broadcast, watch};
use tokio::task::JoinSet;

use crate::auth::Tokens;
use crate::git::Workspace;
use crate::index::Scan;
use crate::store::EventStore;
use crate::writer::{Snapshot, Writer};

/// How many events a live subscriber may fall behind before the server disconnects it.
///
/// A subscriber that falls further behind must resume from its last seq. Dropping events
/// silently would leave its replica wrong.
pub const LIVE_BUFFER: usize = 256;

/// Why a command did not take effect.
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    /// The origin refused the command. Nothing was stored.
    #[error(transparent)]
    Rejected(#[from] Rejection),
    /// Persisting the accepted events failed, or the writer is gone.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Everything a [`Host`] owns at startup.
pub struct HostParts<C> {
    /// The origin, already opened or replayed from the stored log.
    pub origin: Origin,
    /// The stored log the origin writes to.
    pub store: EventStore,
    /// The bearer tokens.
    pub tokens: Tokens,
    /// The scan the origin reads for detection.
    pub scan: Scan,
    /// The usage ledger the model client records into.
    pub ledger: UsageLedger,
    /// The model client that runs derived jobs.
    pub client: C,
    /// The repository being served.
    pub repo: PathBuf,
    /// Signalled when the server shuts down; streams close and new work stops.
    pub shutdown: watch::Receiver<bool>,
    /// The git repository behind task worktrees and landing. `None` when the directory is not a
    /// usable repository; those jobs then report that they cannot run.
    pub git: Option<acan_git::Repo>,
    /// The base URL HTTP witnesses target. `None` makes every HTTP check report an error.
    pub witness_http_base_url: Option<String>,
    /// How many witness runs may run at once.
    pub witness_concurrency: usize,
    /// How many probes of one contrast run at once. Must be at least 1, and 1 runs the probes one
    /// after another. Their container runs still take permits from the witness pool, so this does
    /// not raise how many containers run at once.
    pub probe_concurrency: usize,
}

/// One world's host. Cloning shares the same writer, log and broadcast.
pub struct Host<C> {
    inner: Arc<Inner<C>>,
}

struct Inner<C> {
    writer: Writer,
    snapshots: watch::Receiver<Arc<Snapshot>>,
    store: EventStore,
    tokens: Tokens,
    events: broadcast::Sender<Stamped>,
    wake: Arc<Notify>,
    ledger: UsageLedger,
    client: C,
    repo: PathBuf,
    shutdown: watch::Receiver<bool>,
    streams: Arc<Mutex<JoinSet<()>>>,
    workspace: Option<Arc<Workspace>>,
    witness_http_base_url: Option<String>,
    witness_slots: Arc<Semaphore>,
    probe_concurrency: usize,
    /// Held by a land from its final check through its install, and by every command that
    /// [`invalidates_landing`]. A command cannot change what a land is checking while the land
    /// checks and installs.
    landing: tokio::sync::Mutex<()>,
}

impl<C> Clone for Host<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<C: ModelClient> Host<C> {
    /// Builds a host from its parts and starts the writer task. The caller decides when to run
    /// jobs.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime, or when `probe_concurrency` is zero.
    #[must_use]
    pub fn new(parts: HostParts<C>) -> Self {
        assert!(
            parts.probe_concurrency > 0,
            "probe_concurrency must allow at least one probe, got {}",
            parts.probe_concurrency
        );
        let (events, _) = broadcast::channel(LIVE_BUFFER);
        let wake = Arc::new(Notify::new());
        let scan = Arc::new(parts.scan);
        let (publish, snapshots) = watch::channel(Arc::new(Snapshot {
            world: parts.origin.world().clone(),
            scan: Arc::clone(&scan),
        }));
        let writer = Writer::spawn(
            parts.origin,
            scan,
            parts.store.clone(),
            events.clone(),
            Arc::clone(&wake),
            publish,
        );
        Self {
            inner: Arc::new(Inner {
                writer,
                snapshots,
                store: parts.store,
                tokens: parts.tokens,
                events,
                wake,
                ledger: parts.ledger,
                client: parts.client,
                repo: parts.repo,
                shutdown: parts.shutdown,
                streams: Arc::new(Mutex::new(JoinSet::new())),
                workspace: parts.git.map(|repo| Arc::new(Workspace::new(repo))),
                witness_http_base_url: parts.witness_http_base_url,
                witness_slots: Arc::new(Semaphore::new(parts.witness_concurrency)),
                probe_concurrency: parts.probe_concurrency,
                landing: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// The git workspace, or `None` when the served directory is not a usable repository.
    pub(crate) fn workspace(&self) -> Option<Arc<Workspace>> {
        self.inner.workspace.clone()
    }

    /// The ruling state folded from the core ledger, as the host reads it now. The core ledger is
    /// the one authority for rulings: the gate and land read it here, and no file holds a copy
    /// that could disagree with what the origin accepted.
    ///
    /// # Errors
    ///
    /// Returns an error when the accepted rulings do not fold, or fold to another head than the
    /// ledger holds.
    pub fn folded_rulings(&self) -> anyhow::Result<CoreDerived<FoldedLedger>> {
        let (folded, head) =
            self.read(|world| (fold::fold(world.ledger().events()), world.ledger().head()));
        let folded = folded?;
        anyhow::ensure!(
            folded.value().head == head,
            "the rulings fold to {:?}, not the ledger head {head:?}",
            folded.value().head,
        );
        Ok(folded)
    }

    /// The limits every witness run uses. Only the HTTP base URL comes from configuration.
    pub(crate) fn witness_limits(&self) -> acan_witness::Limits {
        acan_witness::Limits {
            http_base_url: self.inner.witness_http_base_url.clone(),
            ..acan_witness::Limits::default()
        }
    }

    /// One permit per witness run allowed at once. A contrast's container runs take from it too.
    pub(crate) fn witness_slots(&self) -> Arc<Semaphore> {
        Arc::clone(&self.inner.witness_slots)
    }

    /// How many probes of one contrast run at once.
    pub(crate) fn probe_concurrency(&self) -> usize {
        self.inner.probe_concurrency
    }

    /// The bearer tokens.
    #[must_use]
    pub fn tokens(&self) -> &Tokens {
        &self.inner.tokens
    }

    /// The stored log.
    #[must_use]
    pub fn store(&self) -> &EventStore {
        &self.inner.store
    }

    /// Model usage recorded by the client.
    #[must_use]
    pub fn ledger(&self) -> &UsageLedger {
        &self.inner.ledger
    }

    /// The model client that runs derived jobs.
    #[must_use]
    pub fn client(&self) -> &C {
        &self.inner.client
    }

    /// The repository being served.
    #[must_use]
    pub fn repo(&self) -> &Path {
        &self.inner.repo
    }

    /// The scan of the last committed world. Readers keep the `Arc` for as long as they need a
    /// consistent view.
    #[must_use]
    pub fn scan(&self) -> Arc<Scan> {
        Arc::clone(&self.inner.snapshots.borrow().scan)
    }

    /// A receiver for events accepted from now on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Stamped> {
        self.inner.events.subscribe()
    }

    /// Reads the last committed world. Never waits on a write.
    pub fn read<R>(&self, read: impl FnOnce(&World) -> R) -> R {
        read(&self.inner.snapshots.borrow().world)
    }

    /// Sends `command` to the writer. The write is decided against the current scan, persisted,
    /// published and broadcast before this resolves. Dropping the returned future never cancels
    /// a write the writer has accepted. A refused command changes nothing.
    ///
    /// A command that [`invalidates_landing`] waits for any land in its final check and install
    /// to finish, then writes. The land therefore never checks one state and installs another.
    ///
    /// # Errors
    ///
    /// Returns [`SubmitError::Rejected`] when the origin refuses the command, and
    /// [`SubmitError::Internal`] when the events cannot be stored or the writer is gone.
    pub async fn submit(&self, command: Command) -> Result<Vec<Stamped>, SubmitError> {
        let _fence = if invalidates_landing(&command) {
            Some(self.inner.landing.lock().await)
        } else {
            None
        };
        self.inner.writer.submit(command).await
    }

    /// Holds the landing fence until the guard drops. A land takes it before it checks the
    /// queued delivery and keeps it through the install, so no invalidating command writes in
    /// between.
    pub(crate) async fn fence_landings(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.landing.lock().await
    }

    /// Scans the repository again and commits the new code index together with its
    /// `IndexUpdated` batch. A failed persist keeps the previous scan.
    ///
    /// # Errors
    ///
    /// Returns an error when the scan fails, or when the `IndexUpdated` event cannot be stored.
    pub async fn rescan(&self) -> Result<Vec<Stamped>, SubmitError> {
        let repo = self.inner.repo.clone();
        let scan = tokio::task::spawn_blocking(move || crate::index::scan(&repo))
            .await
            .context("the scan task did not finish")??;
        self.inner.writer.rescan(scan).await
    }

    /// Drains the writer and joins its task. After this returns, no write can still happen.
    ///
    /// # Errors
    ///
    /// Returns an error when the writer task panicked.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.inner.writer.shutdown().await
    }

    /// Asks the job loop to look for derived jobs again.
    pub fn wake_jobs(&self) {
        self.inner.wake.notify_one();
    }

    /// Resolves when the job loop should look for derived jobs.
    pub async fn jobs_woken(&self) {
        self.inner.wake.notified().await;
    }

    /// Signalled when the server shuts down.
    pub(crate) fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.inner.shutdown.clone()
    }

    /// The set of live stream tasks, joined during shutdown.
    pub(crate) fn streams(&self) -> &Arc<Mutex<JoinSet<()>>> {
        &self.inner.streams
    }

    /// The writer handle, so the server can drain it during shutdown.
    pub(crate) fn writer(&self) -> Writer {
        self.inner.writer.clone()
    }
}

/// Whether `command` can change what a queued scope-first land checks: the scope and its
/// confirmation, the order itself, or the ruling ledger. Such a command runs under the landing
/// fence.
fn invalidates_landing(command: &Command) -> bool {
    matches!(
        command,
        Command::Human {
            cmd: HumanCmd::ReviseScope { .. }
                | HumanCmd::ConfirmScope { .. }
                | HumanCmd::CancelOrder { .. }
                | HumanCmd::RecordRuling { .. },
            ..
        }
    )
}

/// The origin's clock: unix milliseconds at the moment of the call.
pub(crate) fn now() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the unix epoch")
        .as_millis();
    Timestamp::from_millis(u64::try_from(millis).expect("clock is within u64 milliseconds"))
}
