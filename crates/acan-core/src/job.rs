//! Work the host must do outside the core, derived from state.

use serde::{Deserialize, Serialize};

use crate::ids::{ClashId, CommitId, GateRunId, OrderId, OrderRev, TaskId, Timestamp, WitnessId};

/// A job the host runs and reports back as a `SystemCmd`.
///
/// Jobs are derived by [`crate::World::jobs`], never queued, so a crash restarts them. The
/// host keeps an in-flight set keyed by the job value and skips jobs already running. A
/// result for a job that is no longer derived is rejected as stale.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum Job {
    /// Ask the foreman to split an order. Reported as `SplitReady`.
    Split {
        /// The order.
        order: OrderId,
        /// The revision to split at.
        rev: OrderRev,
    },
    /// Ask the foreman to draft the scope of an order that has none. Reported as `ScopeDrafted`,
    /// or as `ScopeDraftFailed` after the last attempt. A draft carries no authority: a human
    /// revises and confirms the scope.
    DraftScope {
        /// The order.
        order: OrderId,
        /// The order revision the draft is for.
        rev: OrderRev,
    },
    /// Ask the foreman to explain a clash. Reported as `ClashExplained`.
    Explain {
        /// The clash.
        clash: ClashId,
    },
    /// Run one witness of a gate run. Reported as `WitnessResult`.
    RunWitness {
        /// The run.
        run: GateRunId,
        /// The witness.
        witness: WitnessId,
    },
    /// Wait until an armed declare barrier's deadline, then report `Tick`. The origin lifts the
    /// barrier on the first command at or after the deadline.
    Barrier {
        /// The order.
        order: OrderId,
        /// When the barrier lifts.
        deadline: Timestamp,
    },
    /// Merge the front of the land queue. Reported as `LandResult`.
    Land {
        /// The task.
        task: TaskId,
        /// The commit that passed its gate.
        head: CommitId,
    },
}
