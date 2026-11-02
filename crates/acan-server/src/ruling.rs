//! The ruling route. A member rules on one discrepancy episode: the member names the decision and
//! the predicate, the server reads the episode's ruling draft and builds the ledger event from it,
//! and the origin records it.
//!
//! Two checks run before anything is recorded. The draft the member read must still be the one in
//! force: the ledger head and the example the ruling is on must not have moved, or the ruling is
//! refused with 409. And the predicate must give the decision's answer on the example: an accepted
//! example needs the predicate to pass on it, a rejected one needs it to fail
//! ([`predicate::check_examples`]). The origin records what it is sent and does not run that check,
//! so the server rebuilds the example from the observations it was judged on and runs it here.

use std::path::Path;
use std::str::FromStr;

use acan_core::{Command, GateRunId, HumanCmd, MemberId, OrderId, Rejection, Stamped, World};
use acan_intent::compare::{self, Contrast};
use acan_intent::predicate::{self, PredicateError, RetainedExample};
use acan_intent::{
    Applicability, ContrastExample, ContrastExampleHash, DecisionConfirmed, EpisodeId,
    EvidenceHash, HumanRef, LedgerEvent, LedgerHead, LedgerV1, MachineId, McpError, McpErrorCode,
    NonEmpty, NormalizationHash, NormalizationVersion, ObservedRun, OrderRef, Predicate, ProbeHash,
    RulingId, RunManifest, ScopeHash, Selector, Supersedes, UnixMillis, admit,
};
use acan_llm::ModelClient;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};

use crate::host::{Host, SubmitError, now};
use crate::http::ApiError;
use crate::writer::Guard;
use crate::{STATE_DIR, examples, jobs, observations};

/// What a member needs to rule on one discrepancy episode: the contrast examples kept for it, the
/// example the ruling is on, and the hashes and versions a decision names. The draft confirms
/// nothing. [`record`] checks the decision a member sends.
#[derive(Serialize)]
pub(crate) struct RulingDraftView {
    order: String,
    episode: EpisodeId,
    scope_hash: ScopeHash,
    ledger_head: LedgerHead,
    selector: Selector,
    probe_hash: ProbeHash,
    /// The gate run whose evidence is the episode's latest.
    evidence_run: GateRunId,
    /// How many gate runs' evidence the episode holds, the latest included.
    evidence_revision: u32,
    /// The example the ruling is on: the episode's latest candidate run, for its probe.
    ruled_on: ContrastExampleHash,
    normalization_version: NormalizationVersion,
    normalization_hash: NormalizationHash,
    /// Every example kept for the episode, by run and then hash.
    examples: Vec<DraftExample>,
}

/// One example kept for an episode, with the normalization it was compared under.
#[derive(Serialize)]
struct DraftExample {
    run: String,
    hash: ContrastExampleHash,
    probe_hash: ProbeHash,
    normalization_version: NormalizationVersion,
    normalization_hash: NormalizationHash,
    example: ContrastExample,
}

/// The ruling draft of an episode. The episode's latest evidence names the candidate run the
/// ruling is on, so the example of that run and the episode's probe is `ruled_on`.
pub(crate) fn draft<C: ModelClient>(
    host: &Host<C>,
    order: OrderId,
    episode: EpisodeId,
) -> Result<RulingDraftView, ApiError> {
    let (scope_hash, ledger_head, selector, latest, revision) = host.read(|world| {
        let lane = world
            .lane(order)
            .ok_or(ApiError::NotFound("scope for this order"))?;
        let found = lane
            .episodes
            .get(&episode)
            .ok_or(ApiError::NotFound("episode"))?;
        let latest = found
            .evidence
            .last()
            .ok_or(ApiError::NotFound("evidence for this episode"))?;
        Ok::<_, ApiError>((
            lane.scope_hash,
            world.ledger().head(),
            found.selector.clone(),
            (latest.run, latest.probe_hash),
            found.evidence.len(),
        ))
    })?;
    let (run, probe_hash) = latest;
    let evidence_revision = u32::try_from(revision).map_err(internal)?;
    let dir = host.repo().join(STATE_DIR).join(examples::DIR);
    let kept = examples::read(&dir, &episode).map_err(internal)?;
    let examples = kept
        .into_iter()
        .map(|stored| {
            let probe = stored.example.probe.spec.probe_hash().map_err(internal)?;
            let normalization = &stored.example.probe.normalization;
            Ok(DraftExample {
                run: stored.run,
                hash: stored.hash,
                probe_hash: probe,
                normalization_version: normalization.version.clone(),
                normalization_hash: normalization.normalization_hash().map_err(internal)?,
                example: stored.example,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let ruled = ruled_example(&examples, &run.to_string(), probe_hash)?;
    Ok(RulingDraftView {
        order: order.to_string(),
        episode,
        scope_hash,
        ledger_head,
        selector,
        probe_hash,
        evidence_run: run,
        evidence_revision,
        ruled_on: ruled.hash,
        normalization_version: ruled.normalization_version.clone(),
        normalization_hash: ruled.normalization_hash,
        examples,
    })
}

/// The example a ruling is on: the one kept for the episode's latest run and probe. A run that
/// crashed before its receipt was recorded is run again under the same id, and the store then holds
/// an example from each attempt. The receipt does not say which attempt it rests on, so when more
/// than one example matches, the draft refuses rather than pick one.
fn ruled_example<'a>(
    examples: &'a [DraftExample],
    run: &str,
    probe_hash: ProbeHash,
) -> Result<&'a DraftExample, ApiError> {
    let mut matching = examples
        .iter()
        .filter(|kept| kept.run == run && kept.probe_hash == probe_hash);
    match (matching.next(), matching.next()) {
        (None, _) => Err(ApiError::NotFound(
            "retained example for the episode's latest run",
        )),
        (Some(kept), None) => Ok(kept),
        (Some(_), Some(_)) => {
            let message = format!(
                "two retained examples match run {run}: the receipt does not say which \
                 attempt it rests on, so the ruling draft refuses"
            );
            Err(ApiError::Refused(McpError::new(
                McpErrorCode::StaleEvidence {
                    detail: message.clone(),
                },
                message,
            )))
        }
    }
}

/// The member's choice on the example the ruling is on: whether the predicate must hold there
/// (`allow`) or must not (`forbid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Decision {
    /// The predicate holds on the example, so the example is accepted.
    Allow,
    /// The predicate fails on the example, so the example is rejected.
    Forbid,
}

/// The body of a ruling: the member's explicit choice, and the draft it was made from.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RulingBody {
    /// Allow or forbid the example the ruling is on.
    decision: Decision,
    /// The ledger head the draft was read at. Any other head is stale.
    draft_head: LedgerHead,
    /// The example the draft named as ruled on. Any other example is stale.
    ruled_on: ContrastExampleHash,
    /// The scope revision the draft was read under. A newer revision moves it.
    scope_hash: ScopeHash,
    /// The gate run whose evidence the draft named as the episode's latest.
    evidence_run: GateRunId,
    /// How many gate runs' evidence the episode held when the draft was read.
    evidence_revision: u32,
    /// What must hold on the example. The member chooses it; the server never infers it.
    predicate: Predicate,
    /// How far the decision reaches.
    applicability: Applicability,
    /// The brass tag's label.
    label: String,
}

/// Records a member's ruling on one episode, built from the draft the member read.
///
/// The draft is checked twice. First against a read of the committed world, which answers with a
/// 409 that names what moved. Then the writer checks it again in the turn it decides the ruling,
/// against the world as it is then, so a receipt or a scope revision that lands in between is
/// refused rather than ruled on.
///
/// # Errors
///
/// Returns 400 when the label is empty or the predicate does not give the decision's answer on the
/// example it is ruled on, 409 when the draft is stale or the origin refuses the chain, 403 when
/// the member is neither the order's poster nor an owner, and 404 when the order, episode or
/// example is missing.
pub(crate) async fn record<C: ModelClient + 'static>(
    host: &Host<C>,
    member: &MemberId,
    order: OrderId,
    episode: EpisodeId,
    body: RulingBody,
) -> Result<Vec<Stamped>, ApiError> {
    let label = body.label.trim().to_owned();
    if label.is_empty() {
        return Err(ApiError::BadRequest("label"));
    }
    let draft = draft(host, order, episode)?;
    if body.scope_hash != draft.scope_hash {
        return Err(ApiError::Conflict(
            "the scope moved since the draft was read",
        ));
    }
    if body.draft_head != draft.ledger_head {
        return Err(ApiError::Conflict(
            "the ruling log moved since the draft was read",
        ));
    }
    if body.evidence_run != draft.evidence_run || body.evidence_revision != draft.evidence_revision
    {
        return Err(ApiError::Conflict(
            "the evidence the ruling is on moved since the draft was read",
        ));
    }
    if body.ruled_on != draft.ruled_on {
        return Err(ApiError::Conflict(
            "the example the ruling is on moved since the draft was read",
        ));
    }
    let ruled = draft
        .examples
        .iter()
        .find(|kept| kept.hash == draft.ruled_on)
        .ok_or_else(|| internal(anyhow!("the draft names an example it does not keep")))?;
    let dir = host.repo().join(STATE_DIR).join(jobs::OBSERVATIONS_DIR);
    let evidence = check_predicate(&dir, &body.predicate, &ruled.example, body.decision)?;
    let ruling_id = RulingId::from_str(&format!("episode:{episode}")).map_err(internal)?;
    let (machine, supersedes) = host.read(|world| {
        let found = world.order(order).ok_or(ApiError::NotFound("order"))?;
        let supersedes = world
            .ledger()
            .lineage_heads()
            .get(&ruling_id)
            .map(|event| Supersedes {
                event_seq: event.event_seq(),
            });
        Ok::<_, ApiError>((found.machine.clone(), supersedes))
    })?;
    let (accepts, rejects) = match body.decision {
        Decision::Allow => (vec![draft.ruled_on], Vec::new()),
        Decision::Forbid => (Vec::new(), vec![draft.ruled_on]),
    };
    let event_seq = body
        .draft_head
        .event_seq
        .checked_add(1)
        .ok_or_else(|| internal(anyhow!("the ruling log has no next sequence number")))?;
    let decision = DecisionConfirmed {
        schema_version: LedgerV1,
        event_seq,
        prev_hash: body.draft_head.hash,
        actor: HumanRef::from_str(member.as_str()).map_err(internal)?,
        at: UnixMillis(now().as_millis()),
        ruling_id,
        order: OrderRef::from_str(&order.to_string()).map_err(internal)?,
        scope_hash: draft.scope_hash,
        episode_id: Some(episode),
        evidence_hashes: evidence,
        selector: draft.selector,
        predicate_ast: body.predicate,
        accepts,
        rejects,
        ruled_on: draft.ruled_on,
        normalization_version: draft.normalization_version,
        normalization_hash: draft.normalization_hash,
        applicability: body.applicability,
        supersedes,
        label,
        machine_ids: NonEmpty::new(
            MachineId::from_str(machine.as_str()).map_err(internal)?,
            Vec::new(),
        ),
    };
    let event = LedgerEvent::DecisionConfirmed(Box::new(decision));
    let expect = Expect {
        order,
        episode,
        scope_hash: draft.scope_hash,
        head: draft.ledger_head,
        evidence_run: draft.evidence_run,
        evidence_revision: draft.evidence_revision,
    };
    #[cfg(test)]
    host.ruling_hold().wait().await;
    host.submit_guarded(
        Command::Human {
            member: member.clone(),
            cmd: HumanCmd::RecordRuling {
                event: Box::new(event),
            },
        },
        expect.guard(),
    )
    .await
    .map_err(|err| match err {
        SubmitError::Rejected(Rejection::Forbidden { .. }) => {
            ApiError::Forbidden("only the order's poster or an owner rules in it")
        }
        other => ApiError::from(other),
    })
}

/// What a ruling was built from: the scope revision, the ledger head and the latest evidence of
/// the episode. The writer checks them against the committed world in the turn it decides the
/// ruling, so a change after the draft was read refuses the ruling.
struct Expect {
    order: OrderId,
    episode: EpisodeId,
    scope_hash: ScopeHash,
    head: LedgerHead,
    evidence_run: GateRunId,
    evidence_revision: u32,
}

impl Expect {
    fn guard(self) -> Guard {
        Box::new(move |world| self.check(world))
    }

    fn check(&self, world: &World) -> Result<(), Rejection> {
        let lane = world
            .lane(self.order)
            .ok_or_else(|| moved("the order's scope is gone"))?;
        if lane.scope_hash != self.scope_hash {
            return Err(Rejection::StaleScope {
                current_rev: lane.scope.rev,
                current_hash: lane.scope_hash,
            });
        }
        if world.ledger().head() != self.head {
            return Err(moved("the ruling log moved since the draft was read"));
        }
        let latest = lane.episodes.get(&self.episode).and_then(|found| {
            found
                .evidence
                .last()
                .map(|last| (last.run, found.evidence.len()))
        });
        match latest {
            Some((run, revision))
                if run == self.evidence_run
                    && u32::try_from(revision) == Ok(self.evidence_revision) =>
            {
                Ok(())
            }
            _ => Err(moved(
                "the evidence the ruling is on moved since the draft was read",
            )),
        }
    }
}

fn moved(detail: &str) -> Rejection {
    Rejection::StaleEvidence {
        detail: detail.to_owned(),
    }
}

/// Runs the predicate check on the example the ruling is on. The example is rebuilt from the
/// observations it was judged on, admitted against its manifest, and then the predicate must give
/// the decision's answer on it. Returns the raw evidence the example's runs kept.
fn check_predicate(
    dir: &Path,
    predicate: &Predicate,
    example: &ContrastExample,
    decision: Decision,
) -> Result<Vec<EvidenceHash>, ApiError> {
    let base_a = admit_run(dir, example, &example.base_a)?;
    let base_b = admit_run(dir, example, &example.base_b)?;
    let candidate = admit_run(dir, example, &example.candidate)?;
    let contrast = Contrast {
        probe: &example.probe,
        fixture: &example.fixture,
        base_a: &base_a,
        base_b: &base_b,
        candidate: &candidate,
    };
    let masks = compare::derive_masks(&contrast).map_err(internal)?;
    let comparison = compare::compare(&contrast, &masks).map_err(internal)?;
    let ruled = admit::retained_example(example, contrast, &comparison).map_err(internal)?;
    let listed: &[RetainedExample<'_>] = std::slice::from_ref(&ruled);
    let none: &[RetainedExample<'_>] = &[];
    let (accepts, rejects) = match decision {
        Decision::Allow => (listed, none),
        Decision::Forbid => (none, listed),
    };
    predicate::check_examples(predicate, &ruled, accepts, rejects).map_err(refusal)?;
    Ok(raw_evidence([&base_a, &base_b, &candidate]))
}

/// One run of an example, rebuilt from its stored observations and admitted against its manifest.
fn admit_run(
    dir: &Path,
    example: &ContrastExample,
    manifest: &RunManifest,
) -> Result<ObservedRun, ApiError> {
    let stored = manifest
        .observations
        .iter()
        .map(|hash| {
            let observation = observations::get(dir, hash).map_err(internal)?;
            Ok((*hash, observation))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    admit::observed_run(
        &example.probe,
        &example.fixture,
        manifest.run.clone(),
        stored,
    )
    .map_err(internal)
}

/// The raw responses the runs kept, each once, in run order.
fn raw_evidence(runs: [&ObservedRun; 3]) -> Vec<EvidenceHash> {
    let mut hashes: Vec<EvidenceHash> = Vec::new();
    for run in runs {
        for raw in run
            .observations()
            .filter_map(|(_, observation)| observation.raw_ref)
        {
            if !hashes.contains(&raw) {
                hashes.push(raw);
            }
        }
    }
    hashes
}

/// A predicate check failure. A predicate that misjudges the example or names one it does not
/// list is the caller's to fix; anything else is the server's.
fn refusal(err: PredicateError) -> ApiError {
    match err {
        PredicateError::Misjudged { .. } => {
            ApiError::BadRequest("predicate that gives the decision's answer on the example")
        }
        PredicateError::NotListed { .. } => {
            ApiError::BadRequest("predicate that names only the examples the decision lists")
        }
        other => internal(other),
    }
}

fn internal(err: impl Into<anyhow::Error>) -> ApiError {
    ApiError::Internal(err.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kept example of the fixture's probe, filed under `run`. `digit` names its hash.
    fn kept(run: &str, digit: &str) -> DraftExample {
        let example: ContrastExample = serde_json::from_str(include_str!(
            "../../acan-intent/fixtures/contrast-example.json"
        ))
        .expect("the fixture decodes");
        DraftExample {
            run: run.to_owned(),
            hash: ContrastExampleHash::from_str(&format!("sha256:{}", digit.repeat(64)))
                .expect("a hash"),
            probe_hash: example.probe.spec.probe_hash().expect("hashing"),
            normalization_version: example.probe.normalization.version.clone(),
            normalization_hash: example
                .probe
                .normalization
                .normalization_hash()
                .expect("hashing"),
            example,
        }
    }

    #[test]
    fn the_one_example_of_the_latest_run_and_probe_is_the_ruled_one() {
        let examples = [kept("run-1", "a"), kept("run-2", "b")];
        let probe = examples[0].probe_hash;
        let ruled = ruled_example(&examples, "run-1", probe).expect("one example matches");
        assert_eq!(ruled.hash, examples[0].hash);
    }

    #[test]
    fn two_examples_of_the_latest_run_refuse_rather_than_pick_one() {
        // A run that crashed before its receipt was recorded runs again under the same id, and
        // the store then keeps an example from each attempt.
        let examples = [kept("run-1", "a"), kept("run-1", "b")];
        let probe = examples[0].probe_hash;
        match ruled_example(&examples, "run-1", probe).err() {
            Some(ApiError::Refused(err)) => assert!(
                matches!(err.error, McpErrorCode::StaleEvidence { .. }),
                "two matches are stale evidence: {:?}",
                err.error
            ),
            other => panic!("two matching examples must refuse, got {other:?}"),
        }
    }

    #[test]
    fn no_example_of_the_latest_run_is_not_found() {
        let examples = [kept("run-1", "a")];
        let probe = examples[0].probe_hash;
        assert!(matches!(
            ruled_example(&examples, "run-2", probe).err(),
            Some(ApiError::NotFound(_))
        ));
    }
}
