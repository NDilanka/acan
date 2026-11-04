//! The live mapping of a project: each step of its scan, as a page sees it.
//!
//! A scan reports to a [`ScanFeed`]. The feed keeps the status of the current run, so a page that
//! asks late can catch up, and it sends each step to the open streams as a `scan_progress` frame.
//! A frame is live only. It is not in the log, has no seq, and a resuming stream never replays
//! it, as with transition touches. The log keeps the one `IndexUpdated` each completed scan
//! commits, so replay does not depend on how long the scan took.

use std::sync::{Mutex, PoisonError};

use acan_scan::{ModuleFill, Progress};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::host::LIVE_BUFFER;

/// Where a project's scan is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Stage {
    /// The machines are known, and their modules are not all filled yet.
    Skeleton,
    /// Modules are filling in.
    Fill,
    /// Every module is filled, and the state machines are being resolved.
    States,
    /// The code index is committed, and the project takes writes.
    Done,
    /// The scan failed. The project takes no writes.
    Failed,
}

/// The status of a project's current scan, as `GET /api/plant` reports it and as a stream sends
/// it on connect.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Status {
    pub(crate) stage: Stage,
    /// How many modules have filled so far.
    pub(crate) modules_done: u32,
    /// How many modules the scan groups.
    pub(crate) modules_total: u32,
    /// The modules that have filled, in the order they filled.
    pub(crate) filled: Vec<ModuleFill>,
    /// Why the scan failed. Left out of the JSON unless the stage is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

impl Status {
    /// The status of a project with no scan in progress: its index is committed.
    pub(crate) fn settled() -> Self {
        Self {
            stage: Stage::Done,
            modules_done: 0,
            modules_total: 0,
            filled: Vec::new(),
            error: None,
        }
    }

    /// The status without the filled modules, which the plant route leaves to the stream.
    pub(crate) fn summary(&self) -> Summary {
        Summary {
            stage: self.stage,
            modules_done: self.modules_done,
            modules_total: self.modules_total,
            error: self.error.clone(),
        }
    }
}

/// A scan's status without its filled modules, as `GET /api/plant` reports it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Summary {
    stage: Stage,
    modules_done: u32,
    modules_total: u32,
    /// Why the scan failed. Left out of the JSON unless the stage is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// One step of a scan, as a `scan_progress` frame.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Frame {
    #[serde(rename = "type")]
    kind: &'static str,
    stage: Stage,
    modules_done: u32,
    modules_total: u32,
    /// The module that just filled. Left out of the JSON unless the step filled one.
    #[serde(skip_serializing_if = "Option::is_none")]
    module: Option<ModuleFill>,
    /// Why the scan failed. Left out of the JSON unless the stage is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The status of a stream's connect, as a `scan_status` frame.
#[derive(Serialize)]
pub(crate) struct StatusFrame<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    status: &'a Status,
}

impl<'a> StatusFrame<'a> {
    pub(crate) fn of(status: &'a Status) -> Self {
        Self {
            kind: "scan_status",
            status,
        }
    }
}

/// The live feed of one project's scans. Shared by the scan, the streams and the plant route.
pub(crate) struct ScanFeed {
    frames: broadcast::Sender<Frame>,
    status: Mutex<Status>,
}

impl ScanFeed {
    /// A feed for a project whose index is already committed.
    pub(crate) fn settled() -> Self {
        let (frames, _) = broadcast::channel(LIVE_BUFFER);
        Self {
            frames,
            status: Mutex::new(Status::settled()),
        }
    }

    /// The status now, and a receiver for the frames sent from now on. Both are taken under the
    /// lock every send also holds, so each step is either in the status or in the receiver, never
    /// both and never neither. A receiver that lags skips frames, so a stream that lags joins again.
    pub(crate) fn join(&self) -> (Status, broadcast::Receiver<Frame>) {
        let status = self.lock();
        (status.clone(), self.frames.subscribe())
    }

    /// The status of the current scan.
    pub(crate) fn status(&self) -> Status {
        self.lock().clone()
    }

    /// Records one step of the scan, and sends it as a frame.
    pub(crate) fn observe(&self, progress: Progress<'_>) {
        let mut status = self.lock();
        match progress {
            Progress::Skeleton(plant) => {
                *status = Status {
                    stage: Stage::Skeleton,
                    modules_done: 0,
                    modules_total: count(plant.machines.len()),
                    filled: Vec::new(),
                    error: None,
                };
                self.send(&status, None);
            }
            Progress::Module(fill) => {
                status.stage = Stage::Fill;
                status.modules_done = status.modules_done.saturating_add(1);
                status.filled.push(fill.clone());
                self.send(&status, Some(fill));
            }
            Progress::States { .. } => {
                status.stage = Stage::States;
                self.send(&status, None);
            }
        }
    }

    /// Marks the scan done: the index is committed and the project takes writes.
    pub(crate) fn done(&self) {
        let mut status = self.lock();
        status.stage = Stage::Done;
        status.error = None;
        self.send(&status, None);
    }

    /// Marks the scan failed, with the reason.
    pub(crate) fn failed(&self, error: String) {
        let mut status = self.lock();
        status.stage = Stage::Failed;
        status.error = Some(error);
        self.send(&status, None);
    }

    fn send(&self, status: &Status, module: Option<ModuleFill>) {
        let frame = Frame {
            kind: "scan_progress",
            stage: status.stage,
            modules_done: status.modules_done,
            modules_total: status.modules_total,
            module,
            error: status.error.clone(),
        };
        // A send fails only when no stream is open, which is no reason to stop the scan.
        let _ = self.frames.send(frame);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A count as the `u32` the feed carries.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
