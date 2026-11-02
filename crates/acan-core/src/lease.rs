//! Claim leases and fencing.
//!
//! ```text
//! Open --Claim: registered agent, readback matched at current rev, no open question-->
//!     Claimed, generation = last + 1                         [LeaseGranted]
//!     (scope-first order: the agent's readback acknowledged at the confirmed scope instead)
//! Claimed --Renew(fence): generation current, now < expires_at--> Claimed [LeaseRenewed]
//! Claimed --Release(fence)--> Open                           [LeaseEnded Released]
//! Claimed --now >= expires_at (Tick or any command)--> Open  [LeaseEnded Expired]
//! Claimed --AnswerQuestion on the order--> Open              [LeaseEnded Superseded]
//! Claimed{Gating} --gate passed--> Queued                    [LeaseEnded Delivered]
//! Claimed{Gating} --candidate conflicts with protected head--> Open
//!                                                            [LeaseEnded Refused, LandFailed]
//! Claimed --CancelOrder--> Dropped                           [LeaseEnded Cancelled]
//! Any fenced write whose generation is not current: Rejection::StaleGeneration, no event.
//! ```
//!
//! A claim by the live holder returns no events. A claim against an expired lease emits
//! `LeaseEnded{Expired}` then `LeaseGranted` in one batch, so expiry never waits for a tick.
//!
//! A lease does not expire while its task is gating: the worker is waiting on witnesses it
//! does not control. When the gate fails, or a new clash abandons the run, the lease is
//! renewed from that moment so the worker has a full window to react.

use serde::{Deserialize, Serialize};

use crate::event::{Event, Rejection};
use crate::ids::{AgentId, Generation, ReadbackId, TaskId, Timestamp};
use crate::lane;
use crate::order::{Order, Stage, TaskState};
use crate::world::{Ctx, World};

/// How long a lease lives without a renewal.
///
/// Workers renew well inside this window; a dead worker frees its task this long after its
/// last renewal.
pub const LEASE_TTL_MS: u64 = 60_000;

/// How long an order's declare barrier holds after the order's first declaration.
///
/// Siblings that declare inside this window meet each other's declarations, so an overlap is a
/// clash before either task writes. A sibling that never declares holds the others this long.
pub const DECLARE_BARRIER_MS: u64 = 90_000;

/// A live lease on a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// The agent holding the task.
    pub holder: AgentId,
    /// The fencing token for this grant.
    pub generation: Generation,
    /// When the lease ends unless renewed.
    pub expires_at: Timestamp,
}

/// A task's claim: a lease and the matched readback it was granted on. Neither half is optional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The lease.
    pub lease: Lease,
    /// The readback whose verdict matched the order's revision at grant time.
    pub readback: ReadbackId,
}

/// What an agent sends with every write after a claim. The origin rejects any mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fence {
    /// The claimed task.
    pub task: TaskId,
    /// The generation the agent was granted.
    pub generation: Generation,
}

/// Why a lease ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseEnd {
    /// The holder released it.
    Released,
    /// It was not renewed in time.
    Expired,
    /// A question was answered and the witness set changed under it.
    Superseded,
    /// Its gate passed; the task moved to the land queue.
    Delivered,
    /// The order was cancelled.
    Cancelled,
    /// The candidate could not be merged onto the protected head, so the gate refused it.
    Refused,
}

impl Claim {
    /// The fence an agent must present for this claim.
    #[must_use]
    pub fn fence(&self, task: TaskId) -> Fence {
        Fence {
            task,
            generation: self.lease.generation,
        }
    }
}

impl Lease {
    /// Whether the lease has run out at `now`. Never while its task is gating.
    pub(crate) fn is_due(&self, stage: &Stage, now: Timestamp) -> bool {
        now >= self.expires_at && !matches!(stage, Stage::Gating { .. })
    }
}

pub(crate) fn decide_claim(
    world: &World,
    ctx: &Ctx<'_>,
    agent: &AgentId,
    task: TaskId,
) -> Result<Vec<Event>, Rejection> {
    let t = world
        .task(task)
        .ok_or_else(|| Rejection::unknown("task", task))?;
    match &t.state {
        TaskState::Open => {}
        TaskState::Claimed { claim, .. } if &claim.lease.holder == agent => return Ok(Vec::new()),
        TaskState::Claimed { claim, .. } => {
            return Err(Rejection::HeldBy {
                holder: claim.lease.holder.clone(),
                until: claim.lease.expires_at,
            });
        }
        other => return Err(Rejection::wrong_state("task", other.name())),
    }
    if let Some(question) = world.open_question(t.order) {
        return Err(Rejection::OpenQuestion {
            question: question.id,
        });
    }
    let order = world
        .order(t.order)
        .ok_or_else(|| Rejection::unknown("order", t.order))?;
    // A scope-first claim rests only on the lane's own acknowledgement; a legacy model
    // verdict on the same task is never read.
    let readback = match world.lane(t.order) {
        Some(lane) => lane::acknowledged_readback(world, lane, agent, task)?,
        None => matched_readback(world, order, agent, task)?,
    };
    let generation = t
        .last_generation
        .unwrap_or(Generation::ZERO)
        .checked_next()
        .ok_or_else(|| Rejection::invalid("lease generations for this task are exhausted"))?;
    let lease = Lease {
        holder: agent.clone(),
        generation,
        expires_at: ctx.now.plus_millis(LEASE_TTL_MS),
    };
    Ok(vec![Event::LeaseGranted {
        task,
        lease,
        readback,
    }])
}

/// The agent's latest readback of `task` whose verdict matches the order's current witnesses.
fn matched_readback(
    world: &World,
    order: &Order,
    agent: &AgentId,
    task: TaskId,
) -> Result<ReadbackId, Rejection> {
    let reviewed: Vec<_> = world
        .readbacks_of(task)
        .filter(|r| &r.agent == agent)
        .filter_map(|r| r.verdict.as_ref().map(|v| (r.id, v)))
        .collect();
    if let Some((id, _)) = reviewed.iter().rev().find(|(_, v)| v.matches(order)) {
        return Ok(*id);
    }
    let failing = reviewed
        .last()
        .map_or_else(|| order.witnesses.clone(), |(_, v)| v.failing(order));
    Err(Rejection::NotMatched { failing })
}

pub(crate) fn decide_renew(
    world: &World,
    ctx: &Ctx<'_>,
    agent: &AgentId,
    fence: Fence,
) -> Result<Vec<Event>, Rejection> {
    check_fence(world, ctx, agent, fence)?;
    Ok(vec![Event::LeaseRenewed {
        task: fence.task,
        generation: fence.generation,
        expires_at: ctx.now.plus_millis(LEASE_TTL_MS),
    }])
}

pub(crate) fn decide_release(
    world: &World,
    ctx: &Ctx<'_>,
    agent: &AgentId,
    fence: Fence,
) -> Result<Vec<Event>, Rejection> {
    check_fence(world, ctx, agent, fence)?;
    Ok(vec![Event::LeaseEnded {
        task: fence.task,
        generation: fence.generation,
        why: LeaseEnd::Released,
    }])
}

/// Checks `fence` against the task's live lease at `ctx.now` and returns the claim's stage.
/// Every fenced command calls this first.
pub(crate) fn check_fence<'w>(
    world: &'w World,
    ctx: &Ctx<'_>,
    agent: &AgentId,
    fence: Fence,
) -> Result<&'w Stage, Rejection> {
    let task = world
        .task(fence.task)
        .ok_or_else(|| Rejection::unknown("task", fence.task))?;
    let TaskState::Claimed { claim, stage } = &task.state else {
        return Err(Rejection::StaleGeneration { current: None });
    };
    if claim.lease.generation != fence.generation {
        return Err(Rejection::StaleGeneration {
            current: Some(claim.lease.generation),
        });
    }
    if &claim.lease.holder != agent {
        return Err(Rejection::forbidden("the fence belongs to another agent"));
    }
    if claim.lease.is_due(stage, ctx.now) {
        return Err(Rejection::LeaseExpired);
    }
    Ok(stage)
}

/// `LeaseEnded{Expired}` for every lease past its deadline at `ctx.now`. Runs before every command.
pub(crate) fn expire_due(world: &World, ctx: &Ctx<'_>) -> Vec<Event> {
    world
        .tasks
        .values()
        .filter_map(|t| match &t.state {
            TaskState::Claimed { claim, stage } if claim.lease.is_due(stage, ctx.now) => {
                Some(Event::LeaseEnded {
                    task: t.id,
                    generation: claim.lease.generation,
                    why: LeaseEnd::Expired,
                })
            }
            _ => None,
        })
        .collect()
}
