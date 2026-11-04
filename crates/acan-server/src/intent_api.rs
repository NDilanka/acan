//! The scope-first lane's operations, behind the MCP tools and the HTTP routes.
//!
//! Agents reach the scope reads, the readback and the contest through MCP. Humans reach the
//! reads through HTTP, and the human writes (revise, confirm, resolve, rule) go straight to the
//! origin as member commands. The transports authenticate the caller and pick the principal, so
//! the functions here take an agent or nothing; a member command never reaches them.
//!
//! A refusal is an [`McpError`] with a closed code, and the message is the core's own
//! [`Rejection`] text. A value the core decided is passed through unchanged. No function here
//! calls a model, and none writes a verdict, count, class, mask or observed value.

use std::fmt;
use std::str::FromStr;

use acan_core::{
    AgentCmd, AgentId, Command, Event, Fence, OrderId, Rejection, Stamped, Task, TaskId, TaskState,
    World,
};
use acan_intent::fold;
use acan_intent::{
    ContestRequest, ContestResponse, Enforcement, Expectation, ExpectationStatus,
    GetReceiptRequest, GetReceiptResponse, GetRulingsRequest, GetRulingsResponse, GetScopeRequest,
    GetScopeResponse, MachineId, McpError, McpErrorCode, McpV1, OrderRef, PostReadbackRequest,
    PostReadbackResponse, RulingNote, RulingSummary, RulingTagState, TaskRef,
};
use acan_llm::ModelClient;

use crate::host::{Host, SubmitError};

/// Why an intent call did not answer.
#[derive(Debug)]
pub(crate) enum IntentError {
    /// The call was refused with a typed reason the caller can act on.
    Refused(McpError),
    /// The origin or its log failed. The detail goes to the log only.
    Internal(anyhow::Error),
    /// The codebase is still mapping, so the project takes no writes yet.
    NotMapped,
}

/// The answer of one intent call.
pub(crate) type IntentResult<T> = Result<T, IntentError>;

impl From<McpError> for IntentError {
    fn from(err: McpError) -> Self {
        Self::Refused(err)
    }
}

/// The scope of `task`'s order is read at its confirmed revision. Reads are open to every
/// authenticated caller, because the transports already checked the token.
///
/// # Errors
///
/// Returns a refusal when the task does not exist, or when its order has no confirmed scope.
pub(crate) fn get_scope<C: ModelClient>(
    host: &Host<C>,
    request: &GetScopeRequest,
) -> IntentResult<GetScopeResponse> {
    let task = task_of(&request.task)?;
    host.read(|world| {
        let found = found_task(world, task, &request.task)?;
        let lane = world
            .lane(found.order)
            .ok_or_else(|| not_confirmed(found.order))?;
        let confirmation = lane
            .confirmation
            .as_ref()
            .ok_or_else(|| not_confirmed(found.order))?;
        Ok(GetScopeResponse {
            version: McpV1,
            task: request.task.clone(),
            scope: lane.scope.clone(),
            scope_hash: lane.scope_hash,
            confirmed_at: confirmation.at,
            budget: confirmation.budget,
        })
    })
}

/// The ruling log of the world, folded, with the rulings filtered to `machine_ids` when the
/// request names any. The fold covers the whole log: a ruling's selector applies to every order
/// that changes the machine, so the list is not narrowed to one order.
///
/// # Errors
///
/// Returns a refusal when the task does not exist. A log that does not fold is an internal error.
pub(crate) fn get_rulings<C: ModelClient>(
    host: &Host<C>,
    request: &GetRulingsRequest,
) -> IntentResult<GetRulingsResponse> {
    let task = task_of(&request.task)?;
    host.read(|world| {
        found_task(world, task, &request.task)?;
        // The origin accepted every event in this log, so one that does not fold is the server's
        // fault. It goes to the log as an internal error, not to the caller as a bad request.
        let derived =
            fold::fold(world.ledger().events()).map_err(|err| IntentError::Internal(err.into()))?;
        let folded = derived.value();
        let rulings: Vec<RulingSummary> = folded
            .expectations
            .iter()
            .map(expectation_summary)
            .chain(folded.notes.iter().map(note_summary))
            .filter(|ruling| applies_to(ruling, &request.machine_ids))
            .collect();
        Ok(GetRulingsResponse {
            version: McpV1,
            ledger_head: folded.head,
            rulings,
        })
    })
}

/// Records the agent's readback of a scope-first task at the revision and hash it read.
///
/// # Errors
///
/// Returns a refusal when the revision or hash is not the confirmed one, when the scope is not
/// confirmed, or when the caller does not hold the task's readback rights.
pub(crate) async fn post_scoped_readback<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    request: &PostReadbackRequest,
) -> IntentResult<PostReadbackResponse> {
    let task = task_of(&request.task)?;
    let cmd = AgentCmd::PostScopedReadback {
        task,
        scope_rev: request.scope_rev,
        scope_hash: request.scope_hash,
        summary: request.summary.clone(),
        predictions: request.predictions.clone(),
    };
    let events = submit(host, agent, cmd, "post_scoped_readback").await?;
    let readback = events
        .iter()
        .find_map(|stamped| match &stamped.event {
            Event::ScopedReadbackPosted { readback } => Some(readback),
            _ => None,
        })
        .ok_or_else(|| {
            refuse(
                McpErrorCode::InvalidRequest {
                    detail: "the readback was not recorded".to_owned(),
                },
                "the readback was not recorded",
            )
        })?;
    readback
        .acknowledgement()
        .map_err(|err| IntentError::Internal(err.into()))
}

/// Files a contest against one episode of the task's order, as the agent that holds the task.
///
/// The fence is the live claim the agent holds on `request.task`; the agent never names a
/// generation. The origin still checks the lease when it decides.
///
/// # Errors
///
/// Returns a refusal when the task does not exist, the agent does not hold it, or the origin
/// refuses the contest, such as for an unknown episode.
pub(crate) async fn file_contest<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    request: &ContestRequest,
) -> IntentResult<ContestResponse> {
    let task = task_of(&request.task)?;
    let fence = host.read(|world| held_fence(world, task, &request.task, agent))?;
    let cmd = AgentCmd::FileContest {
        fence,
        episode: request.episode_id,
        kind: request.kind,
        reproducer: request.reproducer,
        rationale: request.rationale.clone(),
    };
    let events = submit(host, agent, cmd, "file_contest").await?;
    events
        .iter()
        .find_map(|stamped| match &stamped.event {
            Event::ContestFiled { contest } => Some(ContestResponse {
                version: McpV1,
                contest_id: contest.id.clone(),
                target: contest.target,
                status: contest.status,
            }),
            _ => None,
        })
        .ok_or_else(|| {
            refuse(
                McpErrorCode::InvalidRequest {
                    detail: "the contest was not recorded".to_owned(),
                },
                "the contest was not recorded",
            )
        })
}

/// The latest receipt of the task's gate runs.
///
/// # Errors
///
/// Returns a refusal when the task does not exist, or when none of its runs has a receipt yet.
pub(crate) fn get_receipt<C: ModelClient>(
    host: &Host<C>,
    request: &GetReceiptRequest,
) -> IntentResult<GetReceiptResponse> {
    let task = task_of(&request.task)?;
    host.read(|world| {
        let found = found_task(world, task, &request.task)?;
        let lane = world.lane(found.order).ok_or_else(|| no_receipt(task))?;
        let record = lane
            .runs
            .values()
            .rev()
            .filter(|run| run.task == task)
            .find_map(|run| run.receipt.as_ref())
            .ok_or_else(|| no_receipt(task))?;
        Ok(GetReceiptResponse {
            version: McpV1,
            receipt_hash: record.receipt_hash,
            receipt: record.receipt.clone(),
        })
    })
}

/// Sends one agent command to the origin and returns the events it accepted. `action` names the
/// tool, so a refusal of the agent's permission says what it was refused.
async fn submit<C: ModelClient>(
    host: &Host<C>,
    agent: &AgentId,
    cmd: AgentCmd,
    action: &str,
) -> IntentResult<Vec<Stamped>> {
    host.submit(Command::Agent {
        agent: agent.clone(),
        cmd,
    })
    .await
    .map_err(|err| match err {
        SubmitError::Rejected(rejection) => IntentError::Refused(refusal(rejection, action)),
        SubmitError::Internal(err) => IntentError::Internal(err),
        SubmitError::NotMapped => IntentError::NotMapped,
    })
}

/// The live fence of the task's claim when `agent` holds it.
fn held_fence(world: &World, task: TaskId, raw: &TaskRef, agent: &AgentId) -> IntentResult<Fence> {
    let found = found_task(world, task, raw)?;
    match &found.state {
        TaskState::Claimed { claim, .. } if &claim.lease.holder == agent => Ok(Fence {
            task,
            generation: claim.lease.generation,
        }),
        _ => Err(refuse(
            McpErrorCode::Forbidden {
                action: "file_contest".to_owned(),
            },
            format!("task {raw} is not claimed by you"),
        )),
    }
}

/// The task with `task`'s id in the current world, or a not-found refusal.
fn found_task<'w>(world: &'w World, task: TaskId, raw: &TaskRef) -> IntentResult<&'w Task> {
    world.task(task).ok_or_else(|| not_found("task", raw))
}

fn task_of(raw: &TaskRef) -> IntentResult<TaskId> {
    TaskId::from_str(raw.as_str()).map_err(|_| not_found("task", raw))
}

fn order_ref(order: OrderId) -> IntentResult<OrderRef> {
    order
        .to_string()
        .parse()
        .map_err(|err: acan_intent::InvalidValue| {
            refuse(
                McpErrorCode::InvalidRequest {
                    detail: err.to_string(),
                },
                "the order id has no contract spelling",
            )
        })
}

fn not_confirmed(order: OrderId) -> IntentError {
    match order_ref(order) {
        Ok(order) => {
            let message = format!("the scope of order {order} is not confirmed");
            refuse(McpErrorCode::ScopeNotConfirmed { order }, message)
        }
        Err(err) => err,
    }
}

fn no_receipt(task: TaskId) -> IntentError {
    refuse(
        McpErrorCode::NotFound {
            what: "receipt".to_owned(),
        },
        format!("task {task} has no receipt yet"),
    )
}

fn not_found(what: &str, id: impl fmt::Display) -> IntentError {
    refuse(
        McpErrorCode::NotFound {
            what: what.to_owned(),
        },
        format!("no such {what}: {id}"),
    )
}

fn refuse(code: McpErrorCode, message: impl Into<String>) -> IntentError {
    IntentError::Refused(McpError::new(code, message))
}

/// The closed error code for a core refusal. The message stays the core's own text, and a
/// permission refusal names `action`, the tool the agent called.
fn refusal(rejection: Rejection, action: &str) -> McpError {
    let message = rejection.to_string();
    let code = match rejection {
        Rejection::Forbidden { .. } => McpErrorCode::Forbidden {
            action: action.to_owned(),
        },
        Rejection::Unknown { what, .. } => McpErrorCode::NotFound {
            what: what.into_owned(),
        },
        Rejection::NotReadBack { .. } => McpErrorCode::NotFound {
            what: "readback".to_owned(),
        },
        Rejection::StaleScope {
            current_rev,
            current_hash,
        } => McpErrorCode::StaleScope {
            current_rev,
            current_hash,
        },
        Rejection::ScopeNotConfirmed { order } => match order.to_string().parse::<OrderRef>() {
            Ok(order) => McpErrorCode::ScopeNotConfirmed { order },
            Err(_) => McpErrorCode::InvalidRequest {
                detail: message.clone(),
            },
        },
        Rejection::StaleEvidence { detail } => McpErrorCode::StaleEvidence { detail },
        // A refusal with no closed code yet is an invalid request. BudgetExhausted names an
        // order, but McpErrorCode::BudgetExhausted takes an episode, so it lands here too.
        other => McpErrorCode::InvalidRequest {
            detail: other.to_string(),
        },
    };
    McpError::new(code, message)
}

fn expectation_summary(expectation: &Expectation) -> RulingSummary {
    RulingSummary {
        ruling_id: expectation.ruling_id.clone(),
        event_seq: expectation.event_seq,
        label: expectation.label.clone(),
        machine_ids: expectation.machine_ids.clone(),
        // A stale ruling is still in force, and the receipt names it. RulingTagState has no
        // stale state, so it is shown as active until the contract gains one.
        state: match expectation.status {
            ExpectationStatus::Active | ExpectationStatus::Stale => RulingTagState::Active,
            ExpectationStatus::Superseded => RulingTagState::Superseded,
        },
        enforcement: Enforcement::Enforced {
            selector: expectation.selector.clone(),
            predicate: Box::new(expectation.predicate.clone()),
        },
    }
}

/// A note has no label of its own, so its text is the label.
fn note_summary(note: &RulingNote) -> RulingSummary {
    RulingSummary {
        ruling_id: note.ruling_id.clone(),
        event_seq: note.event_seq,
        label: note.text.clone(),
        machine_ids: note.machine_ids.clone(),
        state: if note.superseded {
            RulingTagState::Superseded
        } else {
            RulingTagState::Active
        },
        enforcement: Enforcement::NotEnforced {
            text: note.text.clone(),
            reason: note.reason.clone(),
        },
    }
}

fn applies_to(ruling: &RulingSummary, machines: &[MachineId]) -> bool {
    machines.is_empty() || ruling.machine_ids.iter().any(|id| machines.contains(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The core raises stale evidence only while it prepares a candidate, and no agent or human
    /// route reaches that step. This pins the closed code and the core's text for the day one does.
    #[test]
    fn a_stale_observation_keeps_its_closed_code_and_the_core_text() {
        let rejection = Rejection::StaleEvidence {
            detail: "the ruling log moved since the candidate was prepared".to_owned(),
        };
        let text = rejection.to_string();
        let err = refusal(rejection, "post_scoped_readback");
        assert_eq!(
            err.error,
            McpErrorCode::StaleEvidence {
                detail: "the ruling log moved since the candidate was prepared".to_owned(),
            }
        );
        assert_eq!(err.message, text);
    }

    #[test]
    fn a_permission_refusal_names_the_tool_the_agent_called() {
        let err = refusal(
            Rejection::Forbidden {
                why: "only the poster or an owner".into(),
            },
            "file_contest",
        );
        assert_eq!(
            err.error,
            McpErrorCode::Forbidden {
                action: "file_contest".to_owned(),
            }
        );
    }
}
