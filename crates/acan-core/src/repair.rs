//! Receipts, discrepancy episodes, order-wide repair rounds, and contests.
//!
//! ```text
//! ReceiptReady(run): the run is gating and prepared; the receipt's counts match its items, and
//!   it names the confirmed scope, the prepared context and every ruling at the current head
//!     --> ReceiptRecorded, GateClosed, LeaseEnded Delivered | LeaseRenewed
//! failing item obligation_failed | out_of_scope | violates_ruling --> episode open, bounced
//!   by the next round while the order has rounds left, else stopped and the lane stops
//! failing item authorized_but_unspecified | unstable_base | incomparable --> residue
//! failing item error --> stays open: an infrastructure retry never spends a round
//! episode not failing, its item passing with every originally failing obligation passing
//!   --> fixed
//! FileContest(fence) --> contested; ResolveContest (human) --> residue | the prior state
//! ```
//!
//! Rounds are the order's, never an episode's: a round bounces every open episode at once, and
//! the next receipt answers it. An episode's identity comes from the receipt and excludes the
//! probe, commit and worker, so a new probe spelling, commit or worker continues the same
//! episode and its budget. Missing evidence, a contest or a note never makes an episode fixed.
//! A prediction score is a finding only: the gate reads the classes, never the scores.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use acan_intent::{
    Authorization, Change, CheckResult, CommitSha, Contest, ContestId, ContestKind, ContestStatus,
    ContestV1, DiscrepancyEpisode, EpisodeId, EpisodeState, EpisodeV1, Evidence, EvidenceSlot,
    ItemClass, ItemId, LedgerEvent, NonEmpty, ObligationId, ObservationPath, OrderRef, Outcome,
    Predicted, Prediction, ProbeHash, Receipt, ReceiptHash, ReceiptItem, RepairRound,
    RepairRoundV1, ResultKey, RoundKey, RoundNumber, RoundResult, RulingId, RulingStatus, Scope,
    ScopeRevision, Selector, StepId, UnixMillis,
};
use serde::{Deserialize, Serialize};

use crate::event::{Event, Rejection};
use crate::gate::{self, GateVerdict};
use crate::ids::{AgentId, GateRunId, MemberId, TaskId};
use crate::lane::{self, ScopeLane};
use crate::lease::{self, Fence};
use crate::order::TaskState;
use crate::ruling::Ledger;
use crate::world::{Ctx, World};

/// One gate run's evidence for an episode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeEvidence {
    /// The run.
    pub run: GateRunId,
    /// The tested merged commit.
    pub merged_commit: CommitSha,
    /// The probe that checked the item.
    pub probe_hash: ProbeHash,
    /// The class the receipt assigned.
    pub class: ItemClass,
}

/// One discrepancy the repair loop tracks across commits, workers and probe spellings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    /// Its identity, from the receipt.
    pub id: EpisodeId,
    /// The receipt item it was found on.
    pub item_id: ItemId,
    /// Where the difference is: the selector the identity keys on.
    pub selector: Selector,
    /// The step that reproduced it when it opened. Not part of its identity.
    pub step_id: StepId,
    /// The obligations that did not pass when it opened. Fixed means each passes fresh.
    pub failing: Vec<ObligationId>,
    /// The latest class.
    pub class: ItemClass,
    /// The probe that reproduces it now. Not part of its identity.
    pub probe_hash: ProbeHash,
    /// Every run's evidence, oldest first.
    pub evidence: Vec<EpisodeEvidence>,
    /// How many of the order's rounds included it.
    pub bounce_count: u32,
    /// Where it stands.
    pub state: EpisodeState,
    /// The state a contest interrupted, restored if the human rejects the contest.
    pub contested_from: Option<EpisodeState>,
}

impl Episode {
    /// This episode as the judge reads it, under the order's scope lineage. The judge matches a
    /// failing item to its episode by selector and failed obligation, and shows a repeat by its
    /// state and bounce count. The evidence stays with the core: the judge does not read it.
    #[must_use]
    pub fn judged(&self, order: &OrderRef, lineage: &[ScopeRevision]) -> DiscrepancyEpisode {
        DiscrepancyEpisode {
            version: EpisodeV1,
            id: self.id,
            order: order.clone(),
            scope_lineage: lineage.to_vec(),
            probe_hash: self.probe_hash,
            step_id: self.step_id.clone(),
            selector: self.selector.clone(),
            obligation_id: self.failing.first().cloned(),
            evidence_revisions: Vec::new(),
            candidate_attempts: Vec::new(),
            bounce_count: self.bounce_count,
            state: self.state,
        }
    }
}

/// The answer a receipt gave to the order's open round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundAnswer {
    /// The round.
    pub round: RoundNumber,
    /// The result.
    pub result: RoundResult,
}

/// Why a receipt did not pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Blocker {
    /// The receipt has no items: an empty check is never a pass.
    NoItems,
    /// An item's class does not count toward a pass.
    Item {
        /// The item.
        item_id: ItemId,
        /// Its class.
        class: ItemClass,
    },
    /// An executable ruling did not hold.
    Ruling {
        /// The ruling lineage.
        ruling_id: RulingId,
        /// The decision in force.
        event_seq: u64,
    },
    /// Obligations failed or had no answer.
    Behavior {
        /// Failed obligations.
        failed: u32,
        /// Obligations with no answer.
        unknown: u32,
    },
    /// An episode is neither fixed nor ruled.
    Episode {
        /// The episode.
        episode: EpisodeId,
        /// Where it stands.
        state: EpisodeState,
    },
    /// A contest waits for the human.
    Contest {
        /// The contest.
        contest: ContestId,
    },
}

/// The core's verdict on one receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum LaneVerdict {
    /// Every item counts toward a pass, every executable ruling held, every episode is fixed
    /// or ruled, and no contest is open.
    Passed,
    /// Not a pass.
    Blocked {
        /// Why.
        blockers: Vec<Blocker>,
    },
    /// The repair budget ran out with these episodes unresolved. Shown as stopped, never as
    /// passed.
    Stopped {
        /// The stopped episodes.
        episodes: Vec<EpisodeId>,
    },
}

/// How one prediction fared against the receipt. A finding, never a pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "score", rename_all = "snake_case")]
pub enum ScoreOutcome {
    /// Every item that answers it shows what was predicted.
    Held {
        /// The items.
        items: Vec<ItemId>,
    },
    /// An item contradicts it.
    Missed {
        /// The contradicting items.
        items: Vec<ItemId>,
    },
    /// No item answers it, or an answering item's outcome is unknown.
    Unscored,
}

/// One scored prediction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionScore {
    /// The agent's prediction, as proposed.
    pub prediction: Prediction,
    /// How it fared.
    pub outcome: ScoreOutcome,
}

/// Everything the core decided about one receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptRecord {
    /// The receipt's hash.
    pub receipt_hash: ReceiptHash,
    /// The receipt.
    pub receipt: Receipt,
    /// The claim readback's predictions, scored.
    pub scores: Vec<PredictionScore>,
    /// Every episode the receipt changed, as it stands after.
    pub episodes: Vec<Episode>,
    /// The open round this receipt answered.
    pub answered: Option<RoundAnswer>,
    /// The round this receipt opened.
    pub opened: Option<RepairRound>,
    /// The verdict.
    pub verdict: LaneVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Bounce,
    Residue,
    Retry,
}

fn route(class: ItemClass) -> Route {
    match class {
        ItemClass::ObligationFailed | ItemClass::OutOfScope | ItemClass::ViolatesRuling => {
            Route::Bounce
        }
        ItemClass::AuthorizedButUnspecified | ItemClass::UnstableBase | ItemClass::Incomparable => {
            Route::Residue
        }
        ItemClass::Error | ItemClass::Preserved | ItemClass::ObligationFulfilled => Route::Retry,
    }
}

pub(crate) fn state_name(state: EpisodeState) -> &'static str {
    match state {
        EpisodeState::Open => "open",
        EpisodeState::Bounced => "bounced",
        EpisodeState::Fixed => "fixed",
        EpisodeState::Residue => "residue",
        EpisodeState::Contested => "contested",
        EpisodeState::Ruled => "ruled",
        EpisodeState::Stopped => "stopped",
    }
}

fn stale_evidence(detail: impl Into<String>) -> Rejection {
    Rejection::StaleEvidence {
        detail: detail.into(),
    }
}

/// `ReceiptReady`: records the receipt of a prepared gating run and closes the run. The same
/// receipt again is a no-op; a different receipt for a run that has one is refused.
pub(crate) fn decide_receipt(
    world: &World,
    ctx: &Ctx<'_>,
    run: GateRunId,
    receipt: Receipt,
) -> Result<Vec<Event>, Rejection> {
    let (lane, lane_run) = lane::lane_run(world, run)?;
    let receipt_hash = receipt.receipt_hash().map_err(lane::hash_failed)?;
    if let Some(record) = &lane_run.receipt {
        return if record.receipt_hash == receipt_hash {
            Ok(Vec::new())
        } else {
            Err(Rejection::wrong_state(
                "gate run",
                "answered by another receipt",
            ))
        };
    }
    if !lane::awaits_receipt(world, lane_run.task, run) {
        return Err(Rejection::wrong_state("gate run", "closed"));
    }
    let prepared = lane_run
        .prepared
        .as_ref()
        .ok_or_else(|| Rejection::wrong_state("gate run", "not prepared"))?;
    let confirmation = lane::confirmed(lane)?;
    receipt
        .check_counts()
        .map_err(|_| Rejection::invalid("the receipt's counts do not match its items"))?;
    if receipt.order != lane::order_ref(lane.order)? {
        return Err(Rejection::invalid("the receipt names another order"));
    }
    if receipt.scope_rev != confirmation.scope_rev || receipt.scope_hash != confirmation.scope_hash
    {
        return Err(Rejection::StaleScope {
            current_rev: confirmation.scope_rev,
            current_hash: confirmation.scope_hash,
        });
    }
    if receipt.merged_commit() != &prepared.context.merged_commit {
        return Err(stale_evidence(format!(
            "the receipt tested {}, run {run} prepared {}",
            receipt.merged_commit(),
            prepared.context.merged_commit
        )));
    }
    if receipt.prepared != *prepared {
        return Err(stale_evidence(
            "the receipt's tested context is not the prepared one",
        ));
    }
    check_rulings(world.ledger(), &receipt)?;
    let gate_run = world
        .gate(run)
        .ok_or_else(|| Rejection::unknown("gate run", run))?;
    let scores = claim_predictions(world, lane_run.task)
        .map(|predictions| score_predictions(&lane.scope, predictions, &receipt))
        .unwrap_or_default();
    let record = classify(
        lane,
        world.ledger(),
        run,
        receipt,
        receipt_hash,
        scores,
        ctx,
    )?;
    let verdict = if record.verdict == LaneVerdict::Passed {
        GateVerdict::Passed
    } else {
        GateVerdict::Failed {
            failing: Vec::new(),
        }
    };
    let fence = Fence {
        task: gate_run.task,
        generation: gate_run.generation,
    };
    let mut events = vec![Event::ReceiptRecorded {
        run,
        record: Box::new(record),
    }];
    events.extend(gate::close(ctx, fence, run, verdict));
    Ok(events)
}

/// The receipt must report every ruling at the head of its lineage and nothing else, and must
/// label every note not enforced and no decision so.
fn check_rulings(ledger: &Ledger, receipt: &Receipt) -> Result<(), Rejection> {
    if receipt.prepared.context.ledger_head != ledger.head() {
        return Err(stale_evidence(
            "the receipt was built on another ruling log revision",
        ));
    }
    let heads = ledger.lineage_heads();
    for (ruling, event) in &heads {
        let seq = event.event_seq();
        if !receipt
            .rulings
            .iter()
            .any(|check| &check.ruling_id == *ruling && check.event_seq == seq)
        {
            return Err(stale_evidence(format!(
                "the receipt does not report ruling {ruling} at event {seq}"
            )));
        }
    }
    for check in &receipt.rulings {
        let Some(event) = heads
            .get(&check.ruling_id)
            .filter(|event| event.event_seq() == check.event_seq)
        else {
            return Err(stale_evidence(format!(
                "the receipt reports ruling {} at event {}, which is not in force",
                check.ruling_id, check.event_seq
            )));
        };
        let note = matches!(event, LedgerEvent::NoteRecorded(_));
        let labelled = matches!(check.status, RulingStatus::NotEnforced { .. });
        if note != labelled {
            return Err(Rejection::Invalid {
                why: format!(
                    "ruling {} must be labelled {}",
                    check.ruling_id,
                    if note {
                        "not enforced"
                    } else {
                        "with its check"
                    }
                ),
            });
        }
    }
    Ok(())
}

/// The episodes a decision at the head of its lineage settles, where the receipt shows it held.
/// A note never settles one.
fn ruled_episodes(ledger: &Ledger, receipt: &Receipt) -> BTreeSet<EpisodeId> {
    let heads = ledger.lineage_heads();
    receipt
        .rulings
        .iter()
        .filter(|check| check.status.held())
        .filter_map(|check| match heads.get(&check.ruling_id) {
            Some(LedgerEvent::DecisionConfirmed(decision)) => decision.episode_id,
            _ => None,
        })
        .collect()
}

fn claim_predictions(world: &World, task: TaskId) -> Option<&[Prediction]> {
    let TaskState::Claimed { claim, .. } = &world.task(task)?.state else {
        return None;
    };
    world
        .scoped_readback(claim.readback)
        .map(|readback| readback.predictions.as_slice())
}

fn failing_obligations(item: &ReceiptItem) -> Vec<ObligationId> {
    item.evaluation
        .obligations
        .iter()
        .filter(|check| !matches!(check.result, CheckResult::Pass {}))
        .map(|check| check.obligation_id.clone())
        .collect()
}

/// The item that shows `episode` fixed: present in this fresh receipt, counting toward a pass,
/// with every originally failing obligation passing. Missing evidence is never a fix.
fn fixed_by<'r>(episode: &Episode, receipt: &'r Receipt) -> Option<&'r ReceiptItem> {
    let item = receipt
        .items
        .iter()
        .find(|item| item.item_id == episode.item_id)?;
    let passes = item.class.counts_toward_pass()
        && episode.failing.iter().all(|obligation| {
            item.evaluation.obligations.iter().any(|check| {
                &check.obligation_id == obligation && matches!(check.result, CheckResult::Pass {})
            })
        });
    passes.then_some(item)
}

fn classify(
    lane: &ScopeLane,
    ledger: &Ledger,
    run: GateRunId,
    receipt: Receipt,
    receipt_hash: ReceiptHash,
    scores: Vec<PredictionScore>,
    ctx: &Ctx<'_>,
) -> Result<ReceiptRecord, Rejection> {
    let budget = lane
        .budget
        .ok_or(Rejection::ScopeNotConfirmed { order: lane.order })?;
    let merged = receipt.merged_commit().clone();
    let now = UnixMillis(ctx.now.as_millis());
    let mut episodes = lane.episodes.clone();
    let mut touched = BTreeSet::new();
    let mut failing_now = BTreeSet::new();
    for item in receipt
        .items
        .iter()
        .filter(|item| !item.class.counts_toward_pass())
    {
        let id = item.episode_id.ok_or_else(|| Rejection::Invalid {
            why: format!("failing item {} names no episode", item.item_id),
        })?;
        failing_now.insert(id);
        touched.insert(id);
        let evidence = EpisodeEvidence {
            run,
            merged_commit: merged.clone(),
            probe_hash: item.probe_hash,
            class: item.class,
        };
        let residue = route(item.class) == Route::Residue;
        match episodes.entry(id) {
            Entry::Vacant(slot) => {
                let selector = item.selector.clone().ok_or_else(|| Rejection::Invalid {
                    why: format!("failing item {} names no selector", item.item_id),
                })?;
                let step_id = item
                    .sequence
                    .first()
                    .map(|step| step.step_id.clone())
                    .ok_or_else(|| Rejection::Invalid {
                        why: format!("failing item {} has no sequence", item.item_id),
                    })?;
                slot.insert(Episode {
                    id,
                    item_id: item.item_id.clone(),
                    selector,
                    step_id,
                    failing: failing_obligations(item),
                    class: item.class,
                    probe_hash: item.probe_hash,
                    evidence: vec![evidence],
                    bounce_count: 0,
                    state: if residue {
                        EpisodeState::Residue
                    } else {
                        EpisodeState::Open
                    },
                    contested_from: None,
                });
            }
            Entry::Occupied(slot) => {
                let episode = slot.into_mut();
                episode.class = item.class;
                episode.probe_hash = item.probe_hash;
                episode.evidence.push(evidence);
                episode.state = match episode.state {
                    EpisodeState::Fixed
                    | EpisodeState::Ruled
                    | EpisodeState::Open
                    | EpisodeState::Bounced
                        if residue =>
                    {
                        EpisodeState::Residue
                    }
                    EpisodeState::Fixed | EpisodeState::Ruled => EpisodeState::Open,
                    other => other,
                };
            }
        }
    }
    let ruled = ruled_episodes(ledger, &receipt);
    // Residue waits for a human ruling or a scope revision; a fresh pass never settles it.
    for episode in episodes
        .values_mut()
        .filter(|e| !failing_now.contains(&e.id))
    {
        let pending = matches!(episode.state, EpisodeState::Open | EpisodeState::Bounced);
        if !pending {
            continue;
        }
        if ruled.contains(&episode.id) {
            episode.state = EpisodeState::Ruled;
            touched.insert(episode.id);
        } else if let Some(item) = fixed_by(episode, &receipt) {
            episode.state = EpisodeState::Fixed;
            episode.class = item.class;
            episode.probe_hash = item.probe_hash;
            episode.evidence.push(EpisodeEvidence {
                run,
                merged_commit: merged.clone(),
                probe_hash: item.probe_hash,
                class: item.class,
            });
            touched.insert(episode.id);
        }
    }
    let answered = match lane.rounds.last() {
        Some(round) if round.result.is_none() => {
            let key = ResultKey {
                round_id: round.round_id,
                merged_commit: merged.clone(),
                receipt_hash,
            };
            Some(RoundAnswer {
                round: round.key.round,
                result: RoundResult {
                    result_id: key.result_id().map_err(lane::hash_failed)?,
                    key,
                    recorded_at: now,
                },
            })
        }
        _ => None,
    };
    let over_cap = usize::try_from(budget.max_episodes).is_ok_and(|cap| episodes.len() > cap);
    let bounce: Vec<EpisodeId> = episodes
        .values()
        .filter(|e| {
            matches!(e.state, EpisodeState::Open | EpisodeState::Bounced)
                && route(e.class) == Route::Bounce
        })
        .map(|e| e.id)
        .collect();
    let rounds_used = u32::try_from(lane.rounds.len()).unwrap_or(u32::MAX);
    let exhausted = !bounce.is_empty() && rounds_used >= budget.repair_rounds.get();
    let mut opened = None;
    let mut stopped = Vec::new();
    if over_cap || exhausted {
        for episode in episodes
            .values_mut()
            .filter(|e| matches!(e.state, EpisodeState::Open | EpisodeState::Bounced))
        {
            episode.state = EpisodeState::Stopped;
            stopped.push(episode.id);
            touched.insert(episode.id);
        }
    } else if let Ok(list) = NonEmpty::try_from(bounce.clone()) {
        let round = RoundNumber::try_from(rounds_used + 1)
            .map_err(|_| Rejection::invalid("an order has at most eight repair rounds"))?;
        let key = RoundKey {
            order: lane::order_ref(lane.order)?,
            round,
        };
        for id in &bounce {
            if let Some(episode) = episodes.get_mut(id) {
                episode.bounce_count += 1;
                episode.state = EpisodeState::Bounced;
            }
            touched.insert(*id);
        }
        opened = Some(RepairRound {
            version: RepairRoundV1,
            round_id: key.round_id().map_err(lane::hash_failed)?,
            key,
            episodes: list,
            opened_at: now,
            result: None,
        });
    }
    let verdict = if stopped.is_empty() {
        let blockers = blockers(&receipt, ledger, &episodes, lane);
        if blockers.is_empty() {
            LaneVerdict::Passed
        } else {
            LaneVerdict::Blocked { blockers }
        }
    } else {
        LaneVerdict::Stopped { episodes: stopped }
    };
    let changed = touched
        .iter()
        .filter_map(|id| episodes.get(id).cloned())
        .collect();
    Ok(ReceiptRecord {
        receipt_hash,
        receipt,
        scores,
        episodes: changed,
        answered,
        opened,
        verdict,
    })
}

fn blockers(
    receipt: &Receipt,
    ledger: &Ledger,
    episodes: &BTreeMap<EpisodeId, Episode>,
    lane: &ScopeLane,
) -> Vec<Blocker> {
    let mut blockers = Vec::new();
    if receipt.items.is_empty() {
        blockers.push(Blocker::NoItems);
    }
    blockers.extend(
        receipt
            .items
            .iter()
            .filter(|item| !item.class.counts_toward_pass())
            .map(|item| Blocker::Item {
                item_id: item.item_id.clone(),
                class: item.class,
            }),
    );
    let heads = ledger.lineage_heads();
    // Notes are never counted: only executable decisions must hold.
    blockers.extend(
        receipt
            .rulings
            .iter()
            .filter(|check| {
                matches!(
                    heads.get(&check.ruling_id),
                    Some(LedgerEvent::DecisionConfirmed(_))
                ) && !check.status.held()
            })
            .map(|check| Blocker::Ruling {
                ruling_id: check.ruling_id.clone(),
                event_seq: check.event_seq,
            }),
    );
    let behavior = receipt.behavior;
    if behavior.failed > 0 || behavior.unknown > 0 {
        blockers.push(Blocker::Behavior {
            failed: behavior.failed,
            unknown: behavior.unknown,
        });
    }
    blockers.extend(
        episodes
            .values()
            .filter(|e| !matches!(e.state, EpisodeState::Fixed | EpisodeState::Ruled))
            .map(|e| Blocker::Episode {
                episode: e.id,
                state: e.state,
            }),
    );
    blockers.extend(lane.open_contests().map(|contest| Blocker::Contest {
        contest: contest.id.clone(),
    }));
    blockers
}

/// Scores `predictions` against `receipt`. A prediction is answered by the receipt items whose
/// authorization names a scope item or must-keep rule with the prediction's exact selector.
/// The scores are findings: they never change a class, a count or the gate.
#[must_use]
pub fn score_predictions(
    scope: &Scope,
    predictions: &[Prediction],
    receipt: &Receipt,
) -> Vec<PredictionScore> {
    predictions
        .iter()
        .map(|prediction| PredictionScore {
            prediction: prediction.clone(),
            outcome: score(scope, prediction, receipt),
        })
        .collect()
}

fn score(scope: &Scope, prediction: &Prediction, receipt: &Receipt) -> ScoreOutcome {
    let mut held = Vec::new();
    let mut missed = Vec::new();
    let mut unanswered = false;
    for item in &receipt.items {
        let change = match &item.evaluation.authorization {
            Authorization::MayChange { item_id } => scope
                .body
                .may
                .iter()
                .find(|may| &may.id == item_id && may.selector == prediction.selector)
                .map(|may| Some(may.change)),
            Authorization::MustKeep { must_keep_id } => scope
                .body
                .must_keep
                .iter()
                .find(|keep| &keep.id == must_keep_id && keep.selector == prediction.selector)
                .map(|_| None),
            Authorization::Ruling { .. } | Authorization::None {} => None,
        };
        let Some(change) = change else {
            continue;
        };
        match judge(&prediction.predicted, change, item) {
            Some(true) => held.push(item.item_id.clone()),
            Some(false) => missed.push(item.item_id.clone()),
            None => unanswered = true,
        }
    }
    if !missed.is_empty() {
        ScoreOutcome::Missed { items: missed }
    } else if held.is_empty() || unanswered {
        ScoreOutcome::Unscored
    } else {
        ScoreOutcome::Held { items: held }
    }
}

/// Whether `item` shows what was predicted; `None` when it cannot say. `change` is the kind
/// the scope item allows, or `None` for a must-keep rule, which allows no kind.
fn judge(predicted: &Predicted, change: Option<Change>, item: &ReceiptItem) -> Option<bool> {
    let outcome = &item.evaluation.outcome;
    if matches!(outcome, Outcome::Unknown { .. }) {
        return None;
    }
    let changed = matches!(outcome, Outcome::Changed { .. });
    match predicted {
        Predicted::Unchanged {} => Some(!changed),
        Predicted::Change { change: kind } => Some(changed && change == Some(*kind)),
        Predicted::Value {
            step_id,
            path,
            value,
        } => observed(item, step_id, path).map(|seen| seen == value),
    }
}

/// The candidate's value at `step` and `path`: from the item's differences, else from its
/// candidate evidence row.
fn observed<'r>(
    item: &'r ReceiptItem,
    step: &StepId,
    path: &ObservationPath,
) -> Option<&'r Evidence> {
    if let Outcome::Changed { differences } = &item.evaluation.outcome
        && let Some(difference) = differences
            .iter()
            .find(|d| &d.step_id == step && &d.path == path)
    {
        return Some(&difference.candidate);
    }
    item.evidence
        .iter()
        .find(|row| row.slot == EvidenceSlot::Candidate && &row.step == step && &row.field == path)
        .map(|row| &row.value)
}

/// What an agent files against one episode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContestDraft {
    /// The episode objected to.
    pub(crate) episode: EpisodeId,
    /// What the agent claims.
    pub(crate) kind: ContestKind,
    /// The probe that shows the problem.
    pub(crate) reproducer: ProbeHash,
    /// Why, in the agent's words.
    pub(crate) rationale: String,
}

/// `FileContest`: the holder of one of the order's tasks objects to one episode. It asks the
/// human; it never changes the scope and never counts as fixed.
pub(crate) fn decide_file_contest(
    world: &World,
    ctx: &Ctx<'_>,
    agent: &AgentId,
    fence: Fence,
    draft: ContestDraft,
) -> Result<Vec<Event>, Rejection> {
    let ContestDraft {
        episode,
        kind,
        reproducer,
        rationale,
    } = draft;
    lease::check_fence(world, ctx, agent, fence)?;
    let order = world
        .task(fence.task)
        .ok_or_else(|| Rejection::unknown("task", fence.task))?
        .order;
    let lane = world
        .lane(order)
        .ok_or_else(|| Rejection::wrong_state("order", "not scope-first"))?;
    let target = lane
        .episodes
        .get(&episode)
        .ok_or_else(|| Rejection::unknown("episode", episode))?;
    if matches!(
        target.state,
        EpisodeState::Fixed | EpisodeState::Ruled | EpisodeState::Contested
    ) {
        return Err(Rejection::wrong_state("episode", state_name(target.state)));
    }
    if rationale.trim().is_empty() {
        return Err(Rejection::invalid("a contest needs a rationale"));
    }
    let id: ContestId = format!("k-{}", world.contests_filed() + 1)
        .parse()
        .map_err(|_| Rejection::invalid("contest ids are exhausted"))?;
    let contest = Contest {
        version: ContestV1,
        id,
        order: lane::order_ref(order)?,
        agent: agent
            .as_str()
            .parse()
            .map_err(|_| Rejection::invalid("the agent id has no contract spelling"))?,
        task: fence
            .task
            .to_string()
            .parse()
            .map_err(|_| Rejection::invalid("the task id has no contract spelling"))?,
        target: episode,
        kind,
        reproducer,
        rationale,
        status: ContestStatus::Open,
        filed_at: UnixMillis(ctx.now.as_millis()),
    };
    Ok(vec![Event::ContestFiled {
        contest: Box::new(contest),
    }])
}

/// `ResolveContest`: only the order's poster or an owner moves a contest out of `open`.
/// Upheld leaves the episode as residue for a ruling or a scope revision; rejected restores
/// the state the contest interrupted.
pub(crate) fn decide_resolve_contest(
    world: &World,
    member: &MemberId,
    contest: &ContestId,
    status: ContestStatus,
) -> Result<Vec<Event>, Rejection> {
    let (lane, filed) = world
        .lanes()
        .find_map(|lane| lane.contests.get(contest).map(|c| (lane, c)))
        .ok_or_else(|| Rejection::unknown("contest", contest))?;
    let order = world
        .order(lane.order)
        .ok_or_else(|| Rejection::unknown("order", lane.order))?;
    if !lane::may_decide(world, member, order) {
        return Err(Rejection::forbidden(
            "only the poster or an owner resolves a contest",
        ));
    }
    if filed.status != ContestStatus::Open {
        return Err(Rejection::wrong_state("contest", "resolved"));
    }
    if status == ContestStatus::Open {
        return Err(Rejection::invalid(
            "a contest is resolved as upheld or rejected",
        ));
    }
    Ok(vec![Event::ContestResolved {
        order: lane.order,
        contest: contest.clone(),
        status,
        by: member.clone(),
    }])
}
