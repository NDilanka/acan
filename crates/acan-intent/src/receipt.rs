//! Classification and receipt counts: the receipt as a pure function of its inputs.
//!
//! Every count carries its denominator. A route counts as covered when at least one probe
//! reached it; that is a route hit, not adequate behavioral coverage, which the obligation
//! counts in [`BehaviorCounts`] report separately. The receipt is attached to the PR after the
//! commit is frozen, never inside it.
//!
//! How items are classified:
//! - A must-keep rule claims every difference its selector covers, of any kind. It is
//!   `obligation_failed` when it claims a difference or an obligation fails, unknown by the
//!   reason when an obligation cannot be answered, and `preserved` otherwise.
//! - A may-change item claims the differences its selector covers that no must-keep rule
//!   claimed and whose observed change kind equals its `change`. The observed kind comes from
//!   base and candidate (added, removed or modified), never from the model's prediction. It is
//!   `obligation_failed` when an acceptance predicate fails, unknown by reason when one cannot be
//!   answered, `authorized_but_unspecified` when a claimed difference is not covered by the step,
//!   channel, path or requests that an acceptance predicate reads, and `obligation_fulfilled`
//!   otherwise.
//! - A difference no rule or item claims is `out_of_scope`: one item per probe, step and path.
//! - An active ruling whose predicate fails on a reached probe where a field it covers differs
//!   is `violates_ruling`: one item per failing probe. A reached probe whose covered fields agree
//!   with the base is not checked and holds the ruling. A ruling whose normalization plan no
//!   longer matches the corpus, or an exact approval whose evidence is not one it accepted, is
//!   `stale` and never held.
//!
//! A scope item that no probe reaches gets no receipt item, because a receipt item needs the
//! probe that checked it. Its acceptance obligations are counted as unknown in
//! [`BehaviorCounts`], so the receipt still refuses to pass. A probe whose result is not a pass and
//! that no scope item reaches gets an item of its own. A result that does not check against its
//! prepared context is refused, or, when only its contrast or comparison fails to check, gets a
//! non-pass item.

use std::str::FromStr;

use serde::Serialize;
use serde_json::json;

use crate::compare::{self, CompareError, Contrast};
use crate::episode::{Contest, DiscrepancyEpisode, EpisodeState, canonical_episode_selector};
use crate::expr::{EventFilter, Operand, Predicate, ValueRef};
use crate::gate::{FixtureSetManifest, PreparedCommit, ProbeSetManifest};
use crate::hash::{CanonicalError, ContentHash, ProbeHash};
use crate::ledger::{Applicability, Expectation, ExpectationStatus, FoldedLedger};
use crate::observation::{Mask, ObservedRun, RequestRecord};
use crate::predicate::{self, RetainedExample, method_admits, principal_admits};
use crate::probe::{ProbeCorpus, RouteStatus, RunOutcome, RunRole};
use crate::provenance::{CoreDerived, Provenance};
use crate::report::{
    BehaviorCounts, ClassCounts, EvidenceRow, EvidenceSlot, GateState, NotReached, ProbeCounts,
    Receipt, ReceiptItem, RouteCounts, RulingCheck, RulingStatus, SequenceStep, StepTint,
    UncoveredRoute,
};
use crate::scope::{Change, PredicateRef, Scope, ScopeConfirmation, ScopeItem, Selector};
use crate::types::{
    Channel, Evidence, HttpMethod, ItemId, NonEmpty, ObligationId, ObservationPath, RouteTemplate,
    StepId, Surface,
};
use crate::verdict::{
    Authorization, CheckResult, Comparison, Difference, Evaluation, ItemClass, ItemState,
    ObligationCheck, Outcome, UnknownReason,
};
use crate::version::ReceiptV1;

/// Why a receipt could not be built. Never a green receipt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReceiptError {
    /// The scope, its confirmation and the prepared commit do not belong together.
    #[error("the scope, its confirmation and the prepared commit do not belong together: {detail}")]
    Inconsistent {
        /// What does not match.
        detail: String,
    },
    /// A value a receipt names has no canonical JSON form.
    #[error("a receipt value has no canonical JSON form: {detail}")]
    Hashing {
        /// The serializer's message.
        detail: String,
    },
}

/// One probe's contrast, masks and comparison.
#[derive(Debug, Clone, Copy)]
pub struct ProbeResult<'a> {
    /// The runs.
    pub contrast: Contrast<'a>,
    /// The masks derived from its base runs.
    pub masks: &'a [CoreDerived<Mask>],
    /// The comparison.
    pub comparison: &'a CoreDerived<Comparison>,
}

/// Everything a receipt is a function of.
#[derive(Debug, Clone, Copy)]
pub struct ReceiptInputs<'a> {
    /// The confirmed scope.
    pub scope: &'a Scope,
    /// Its confirmation.
    pub confirmation: &'a ScopeConfirmation,
    /// The prepared candidate.
    pub prepared: &'a PreparedCommit,
    /// The folded ruling log.
    pub ledger: &'a CoreDerived<FoldedLedger>,
    /// The protected probe corpus.
    pub corpus: &'a ProbeCorpus,
    /// One result per probe run.
    pub results: &'a [ProbeResult<'a>],
    /// The retained examples fixed `equals_base` anchors name.
    pub anchors: &'a [RetainedExample<'a>],
    /// The order's episodes.
    pub episodes: &'a [DiscrepancyEpisode],
    /// The order's contests.
    pub contests: &'a [Contest],
    /// Where the gate is.
    pub gate_state: GateState,
}

/// Classifies every item and counts the classes. The counts always equal the items
/// ([`Receipt::check_counts`]).
///
/// # Errors
///
/// Returns [`ReceiptError`] when the scope, its confirmation and the prepared commit do not
/// belong together, or a value the receipt names cannot be hashed.
pub fn build_receipt(inputs: &ReceiptInputs<'_>) -> Result<CoreDerived<Receipt>, ReceiptError> {
    bind(inputs)?;
    bind_context(inputs)?;
    let hashes = inputs
        .results
        .iter()
        .map(|result| result.contrast.probe.spec.probe_hash().map_err(hashing))
        .collect::<Result<Vec<_>, _>>()?;
    inventory(inputs, &hashes)?;
    let faults: Vec<Option<Fault>> = inputs.results.iter().map(result_fault).collect();
    let usable: Vec<bool> = faults.iter().map(Option::is_none).collect();
    let claims = claims(inputs, &usable);
    let mut owners = vec![Owner::Unclaimed; claims.len()];
    let mut behavior = Behavior::default();
    let mut must_items = Vec::new();
    let mut may_items = Vec::new();
    let mut ruling_items = Vec::new();
    let mut not_reached = Vec::new();
    let mut reached = vec![false; inputs.results.len()];

    for rule in &inputs.scope.body.must_keep {
        let item_id = ItemId::from_str(rule.id.as_str()).map_err(|_| {
            inconsistent(&format!(
                "must-keep id {} is not an item id",
                rule.id.as_str()
            ))
        })?;
        let reaching = reaching_probes(inputs, &usable, &rule.selector);
        for &index in &reaching {
            reached[index] = true;
        }
        let judged = judge_obligations(inputs, rule.obligations.iter(), &reaching);
        if reaching.is_empty() {
            not_reached.push(unreached(item_id.clone(), &rule.selector, &judged));
        }
        let covered: Vec<usize> = (0..claims.len())
            .filter(|&index| claims[index].covered_by(&rule.selector))
            .collect();
        for &index in &covered {
            owners[index] = Owner::Must;
        }
        for judgment in &judged {
            behavior.record(if covered.is_empty() {
                verdict(&judgment.result)
            } else {
                Verdict::Fail
            });
        }
        if reaching.is_empty() && covered.is_empty() {
            continue;
        }
        let class = must_class(&judged, !covered.is_empty());
        let probe = failing_probe(&judged)
            .or_else(|| covered.first().map(|&index| claims[index].probe))
            .or_else(|| reaching.first().copied());
        let Some(probe) = probe else { continue };
        let claimed: Vec<&Claim> = covered.iter().map(|&index| &claims[index]).collect();
        let episode = episode_for(inputs.episodes, &rule.selector, &failing_ids(&judged));
        must_items.push(ReceiptItem {
            item_id,
            class,
            state: state_of(episode),
            machine_id: rule.machine_ids.first().clone(),
            selector: Some(canonical_episode_selector(&rule.selector)),
            probe_hash: hashes[probe],
            episode_id: episode.map(|episode| episode.id),
            ruling_id: None,
            attempts: attempts_of(episode),
            budget: budget(inputs),
            route_path: inputs.results[probe].contrast.probe.route_path.clone(),
            sequence: sequence(&inputs.results[probe]),
            evidence: evidence(&inputs.results[probe], &claimed_in(&claimed, probe)),
            evaluation: Evaluation {
                authorization: Authorization::MustKeep {
                    must_keep_id: rule.id.clone(),
                },
                outcome: item_outcome(inputs, &reaching, &claimed),
                obligations: obligation_checks(&judged),
            },
        });
    }

    for item in &inputs.scope.body.may {
        let reaching = reaching_probes(inputs, &usable, &item.selector);
        for &index in &reaching {
            reached[index] = true;
        }
        let judged = judge_obligations(inputs, item.acceptance.iter(), &reaching);
        if reaching.is_empty() {
            not_reached.push(unreached(item.id.clone(), &item.selector, &judged));
        }
        let covered: Vec<usize> = (0..claims.len())
            .filter(|&index| {
                owners[index] == Owner::Unclaimed
                    && claims[index].owned_by(&item.selector)
                    && observed_change(&claims[index].difference) == item.change
            })
            .collect();
        for &index in &covered {
            owners[index] = Owner::May;
        }
        for judgment in &judged {
            behavior.record(verdict(&judgment.result));
        }
        if reaching.is_empty() && covered.is_empty() {
            continue;
        }
        let claimed: Vec<&Claim> = covered.iter().map(|&index| &claims[index]).collect();
        let class = may_class(&judged, &claimed, &reads_of(item));
        let probe = failing_probe(&judged)
            .or_else(|| covered.first().map(|&index| claims[index].probe))
            .or_else(|| reaching.first().copied());
        let Some(probe) = probe else { continue };
        let episode = episode_for(inputs.episodes, &item.selector, &failing_ids(&judged));
        may_items.push(item_for(
            inputs,
            Placement {
                item,
                class,
                probe,
                episode,
                hashes: &hashes,
                reaching: &reaching,
                claimed: &claimed,
                judged: &judged,
            },
        ));
    }

    let mut rulings = Vec::new();
    for expectation in &inputs.ledger.value().expectations {
        if expectation.status == ExpectationStatus::Superseded {
            continue;
        }
        let (status, items) = ruling(
            inputs,
            expectation,
            &claims,
            &mut owners,
            &hashes,
            &usable,
            &mut behavior,
        )?;
        ruling_items.extend(items);
        rulings.push(RulingCheck {
            ruling_id: expectation.ruling_id.clone(),
            event_seq: expectation.event_seq,
            label: expectation.label.clone(),
            machine_ids: expectation.machine_ids.clone(),
            status,
        });
    }
    for note in &inputs.ledger.value().notes {
        if note.superseded {
            continue;
        }
        rulings.push(RulingCheck {
            ruling_id: note.ruling_id.clone(),
            event_seq: note.event_seq,
            label: format!("note at event {}", note.event_seq),
            machine_ids: note.machine_ids.clone(),
            status: RulingStatus::NotEnforced {
                reason: note.reason.clone(),
            },
        });
    }

    let mut oos_items = Vec::new();
    for (index, claim) in claims.iter().enumerate() {
        if owners[index] != Owner::Unclaimed {
            continue;
        }
        oos_items.push(out_of_scope(inputs, claim, &hashes, inputs.episodes)?);
    }

    let mut probe_items = Vec::new();
    for (index, fault) in faults.iter().enumerate() {
        let (class, reason) = match fault {
            Some(fault) => (fault.class, fault.reason.clone()),
            None if reached[index] => continue,
            None => match &inputs.results[index].comparison.value().outcome {
                Outcome::Unknown { reason } => (class_of(reason), reason.clone()),
                Outcome::Changed { .. } | Outcome::Unchanged {} => continue,
            },
        };
        probe_items.push(probe_item(inputs, index, &hashes, class, &reason)?);
    }

    let mut items = Vec::new();
    items.extend(must_items);
    items.extend(may_items);
    items.extend(ruling_items);
    items.extend(oos_items);
    items.extend(probe_items);

    let receipt = Receipt {
        version: ReceiptV1,
        order: inputs.scope.order.clone(),
        scope_rev: inputs.scope.rev,
        scope_hash: inputs.confirmation.scope_hash,
        prepared: inputs.prepared.clone(),
        gate_state: inputs.gate_state,
        counts: tally(&items),
        items,
        probes: probe_counts(inputs)?,
        routes: route_counts(inputs)?,
        behavior: behavior.counts(),
        masks: inputs
            .results
            .iter()
            .flat_map(|result| result.masks.iter().map(|mask| mask.value().clone()))
            .collect(),
        rulings,
        contests: inputs.contests.to_vec(),
        not_reached,
    };
    receipt
        .check_counts()
        .map_err(|err| inconsistent(&err.to_string()))?;
    receipt
        .check_episode_ids()
        .map_err(|err| inconsistent(&err.to_string()))?;
    Ok(CoreDerived::derived(receipt))
}

fn inconsistent(detail: &str) -> ReceiptError {
    ReceiptError::Inconsistent {
        detail: detail.to_owned(),
    }
}

fn hashing(err: CanonicalError) -> ReceiptError {
    ReceiptError::Hashing {
        detail: err.to_string(),
    }
}

/// The scope, its confirmation and the prepared commit must name the same revision and base,
/// and the confirmation must list the scope's selectors and examples in scope order. The
/// confirmation must also bind the scope's semantic digest, which covers every executable field,
/// so a changed predicate or change kind is refused under the old confirmation. None of these
/// hashes covers wording, so a wording edit cannot change them.
fn bind(inputs: &ReceiptInputs<'_>) -> Result<(), ReceiptError> {
    let (scope, confirmation) = (inputs.scope, inputs.confirmation);
    if confirmation.order != scope.order {
        return Err(inconsistent("the confirmation is for another order"));
    }
    if confirmation.scope_rev != scope.rev {
        return Err(inconsistent(
            "the confirmation is for another scope revision",
        ));
    }
    let executable = scope.semantic_scope_hash().map_err(hashing)?;
    if confirmation.semantic_scope_hash != Some(executable) {
        return Err(inconsistent(
            "the confirmation does not bind this scope's executable content",
        ));
    }
    if confirmation.base_commit != scope.base_commit
        || inputs.prepared.base_commit != scope.base_commit
    {
        return Err(inconsistent(
            "the scope, its confirmation and the prepared commit name different bases",
        ));
    }
    let selectors = scope
        .body
        .may
        .iter()
        .map(|item| &item.selector)
        .chain(scope.body.must_keep.iter().map(|rule| &rule.selector))
        .map(|selector| selector.selector_hash().map_err(hashing))
        .collect::<Result<Vec<_>, _>>()?;
    if selectors != confirmation.selector_hashes {
        return Err(inconsistent(
            "the confirmation does not list the scope's selectors",
        ));
    }
    let examples = scope
        .body
        .may
        .iter()
        .map(|item| item.example.example_hash().map_err(hashing))
        .collect::<Result<Vec<_>, _>>()?;
    if examples != confirmation.example_hashes {
        return Err(inconsistent(
            "the confirmation does not list the scope's examples",
        ));
    }
    Ok(())
}

/// The runs, ledger, corpus and scope must be the ones the prepared context tested. Base runs
/// must be at the prepared base, the candidate at the merged commit, and every run on the context's
/// runtime and runner. A mismatch refuses the receipt: evidence for another commit is never scored.
fn bind_context(inputs: &ReceiptInputs<'_>) -> Result<(), ReceiptError> {
    let (prepared, folded) = (inputs.prepared, inputs.ledger.value());
    let context = &prepared.context;
    if context.protected_head != prepared.base_commit {
        return Err(inconsistent("the protected head is not the prepared base"));
    }
    if context.scope_hash != inputs.confirmation.scope_hash {
        return Err(inconsistent("the prepared context names another scope"));
    }
    if context.ledger_head != folded.head {
        return Err(inconsistent(
            "the prepared context names another ruling log revision",
        ));
    }
    let probe_set = ProbeSetManifest::of(inputs.corpus, &folded.retired_probes)
        .and_then(|manifest| manifest.probe_set_hash())
        .map_err(hashing)?;
    if context.probe_set_hash != probe_set {
        return Err(inconsistent("the prepared context names another probe set"));
    }
    let fixtures = FixtureSetManifest::of(inputs.corpus)
        .and_then(|manifest| manifest.fixture_set_hash())
        .map_err(hashing)?;
    if context.fixture_set_hash != fixtures {
        return Err(inconsistent(
            "the prepared context names another fixture set",
        ));
    }
    for result in inputs.results {
        let expected = [
            (result.contrast.base_a, &prepared.base_commit),
            (result.contrast.base_b, &prepared.base_commit),
            (result.contrast.candidate, &context.merged_commit),
        ];
        for (run, commit) in expected {
            let record = run.run();
            if record.commit != *commit {
                return Err(inconsistent(&format!(
                    "run {} is not at the prepared commit",
                    record.id
                )));
            }
            if record.env.runtime != context.runtime {
                return Err(inconsistent(&format!(
                    "run {} is on another runtime than the prepared context",
                    record.id
                )));
            }
            if record.runner != context.runner_hash {
                return Err(inconsistent(&format!(
                    "run {} is on another runner than the prepared context",
                    record.id
                )));
            }
        }
    }
    Ok(())
}

/// The results name distinct probes of the corpus, retired probes excluded, each with the
/// normalization plan the corpus holds. A duplicate or foreign result is refused, so a missing
/// result stays not run and no other result can stand in for it.
fn inventory(inputs: &ReceiptInputs<'_>, hashes: &[ProbeHash]) -> Result<(), ReceiptError> {
    let retired = &inputs.ledger.value().retired_probes;
    let mut active = Vec::new();
    for probe in &inputs.corpus.probes {
        let hash = probe.spec.probe_hash().map_err(hashing)?;
        if !retired.contains(&hash) {
            let plan = probe.normalization.normalization_hash().map_err(hashing)?;
            active.push((hash, plan));
        }
    }
    for (index, result) in inputs.results.iter().enumerate() {
        let hash = hashes[index];
        if hashes[..index].contains(&hash) {
            return Err(inconsistent(&format!("probe {hash} has two results")));
        }
        let plan = result
            .contrast
            .probe
            .normalization
            .normalization_hash()
            .map_err(hashing)?;
        if !active.contains(&(hash, plan)) {
            return Err(inconsistent(&format!(
                "probe {hash} is not an active probe of the corpus"
            )));
        }
    }
    Ok(())
}

/// A probe result that does not check: its item's class and the reason its outcome is unknown.
#[derive(Clone)]
struct Fault {
    class: ItemClass,
    reason: UnknownReason,
}

/// The fault of one result, if its contrast or comparison does not check. A result that does
/// not check is never a pass and never contributes a claim.
fn result_fault(result: &ProbeResult<'_>) -> Option<Fault> {
    let error = checked(result).err()?;
    Some(match error {
        CompareError::Incompatible { detail } => Fault {
            class: ItemClass::Incomparable,
            reason: UnknownReason::Incomparable { detail },
        },
        other => Fault {
            class: ItemClass::Error,
            reason: UnknownReason::Error {
                detail: other.to_string(),
            },
        },
    })
}

/// Checks a result's runs against their roles and each other, its masks against the ones its
/// base runs derive, and its comparison against the one these runs make.
fn checked(result: &ProbeResult<'_>) -> Result<(), CompareError> {
    compare::check_contrast(&result.contrast)?;
    let derived = compare::derive_masks(&result.contrast)?;
    let supplied: Vec<&Mask> = result.masks.iter().map(CoreDerived::value).collect();
    if derived.iter().map(CoreDerived::value).ne(supplied) {
        return Err(CompareError::ForeignComparison {
            detail: "the masks are not the ones these runs derive".to_owned(),
        });
    }
    compare::verify_comparison(&result.contrast, result.comparison)
}

/// The receipt item of one probe that no scope item reaches, or that does not check. It names
/// the probe, carries its unknown outcome and claims nothing.
fn probe_item(
    inputs: &ReceiptInputs<'_>,
    index: usize,
    hashes: &[ProbeHash],
    class: ItemClass,
    reason: &UnknownReason,
) -> Result<ReceiptItem, ReceiptError> {
    let result = &inputs.results[index];
    Ok(ReceiptItem {
        item_id: minted("probe", &hashes[index])?,
        class,
        state: ItemState::Open,
        machine_id: result.contrast.probe.route_path.machines.first().clone(),
        selector: None,
        probe_hash: hashes[index],
        episode_id: None,
        ruling_id: None,
        attempts: 0,
        budget: budget(inputs),
        route_path: result.contrast.probe.route_path.clone(),
        sequence: sequence(result),
        evidence: Vec::new(),
        evaluation: Evaluation {
            authorization: Authorization::None {},
            outcome: Outcome::Unknown {
                reason: reason.clone(),
            },
            obligations: Vec::new(),
        },
    })
}

/// Who owns a difference. A must-keep rule owns every difference it covers, then a may-change
/// item owns the rest it covers, then a ruling takes the rest its approval covers on the probes
/// it holds or violates. Whatever no rule owns is out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Unclaimed,
    Must,
    May,
    Ruling,
}

/// One difference, the probe it was found in, and the request that sent its step.
struct Claim {
    probe: usize,
    difference: Difference,
    request: Option<RequestRecord>,
}

impl Claim {
    /// Whether a must-keep rule covers the difference: the field it names, any field above it
    /// and any field below it, since a replaced ancestor changes what the rule checks.
    fn covered_by(&self, selector: &Selector) -> bool {
        self.covered_with(selector, compare::overlaps)
    }

    /// Whether a may-change grant owns the difference. A grant owns the field it names and what
    /// lies below it. A replacement of an ancestor is not owned by a grant on one field of it.
    fn owned_by(&self, selector: &Selector) -> bool {
        self.covered_with(selector, compare::at_or_below)
    }

    fn covered_with(&self, selector: &Selector, field: impl Fn(&str, &str) -> bool) -> bool {
        covers(
            selector,
            &self.difference.step_id,
            self.request.as_ref(),
            &self.difference.path,
            field,
        )
    }
}

fn claims(inputs: &ReceiptInputs<'_>, usable: &[bool]) -> Vec<Claim> {
    let mut claims = Vec::new();
    for (probe, result) in inputs.results.iter().enumerate() {
        if !usable[probe] {
            continue;
        }
        let Outcome::Changed { differences } = &result.comparison.value().outcome else {
            continue;
        };
        for difference in differences {
            let request = request_at(result.contrast.candidate, &difference.step_id)
                .or_else(|| request_at(result.contrast.base_a, &difference.step_id));
            claims.push(Claim {
                probe,
                difference: difference.clone(),
                request,
            });
        }
    }
    claims
}

/// The request that sent `step`: its own, or the last one before it in sequence order.
fn request_at(run: &ObservedRun, step: &StepId) -> Option<RequestRecord> {
    let mut last: Option<&RequestRecord> = None;
    for (_, observation) in run.observations() {
        if let Some(record) = &observation.request {
            last = Some(record);
        }
        if observation.step_id == *step {
            return last.cloned();
        }
    }
    None
}

fn reaching_probes(inputs: &ReceiptInputs<'_>, usable: &[bool], selector: &Selector) -> Vec<usize> {
    (0..inputs.results.len())
        .filter(|&index| usable[index] && reaches(selector, &inputs.results[index].contrast))
        .collect()
}

fn reaches(selector: &Selector, contrast: &Contrast<'_>) -> bool {
    [contrast.base_a, contrast.base_b, contrast.candidate]
        .into_iter()
        .any(|run| {
            let mut last: Option<&RequestRecord> = None;
            run.observations().any(|(_, observation)| {
                if let Some(record) = &observation.request {
                    last = Some(record);
                }
                step_context_admits(selector, &observation.step_id)
                    && request_admits(selector, last)
            })
        })
}

fn step_context_admits(selector: &Selector, step: &StepId) -> bool {
    match &selector.step_context {
        crate::scope::StepContext::Any {} => true,
        crate::scope::StepContext::Step { step_id } => step_id == step,
    }
}

fn request_admits(selector: &Selector, request: Option<&RequestRecord>) -> bool {
    let Some(request) = request else {
        return false;
    };
    selector.surface == Surface::Http
        && method_admits(selector.method, request.method)
        && selector.route_template == request.route_template
        && principal_admits(&selector.principal, request.actor.as_ref())
}

/// Whether a selector covers a difference at `path` of `step`, sent by `request`. `field` decides
/// how the selector's field pattern meets the path.
fn covers(
    selector: &Selector,
    step: &StepId,
    request: Option<&RequestRecord>,
    path: &ObservationPath,
    field: impl Fn(&str, &str) -> bool,
) -> bool {
    step_context_admits(selector, step)
        && request_admits(selector, request)
        && (selector.channel == Channel::Any || selector.channel == path.channel())
        && match &selector.field_path {
            None => true,
            Some(name) => field(
                &channel_pattern(selector.channel, name.as_str()),
                path.as_str(),
            ),
        }
}

fn channel_pattern(channel: Channel, field: &str) -> String {
    match channel {
        Channel::Header => format!("header.{field}"),
        Channel::Cookie => format!("cookie.{field}"),
        Channel::Body => join("body", field),
        Channel::SideEffect => join("side_effect", field),
        Channel::Status | Channel::Any => "status".to_owned(),
    }
}

fn join(root: &str, field: &str) -> String {
    if field.starts_with('[') {
        format!("{root}{field}")
    } else {
        format!("{root}.{field}")
    }
}

/// The change a difference makes, read from base and candidate.
fn observed_change(difference: &Difference) -> Change {
    match (&difference.base, &difference.candidate) {
        (Evidence::Missing {}, Evidence::Present { .. }) => Change::Add,
        (Evidence::Present { .. }, Evidence::Missing {}) => Change::Remove,
        _ => Change::Modify,
    }
}

/// The selector a difference names, as the episode key builds it: the request's method and
/// route, any step, an authenticated or anonymous principal, and the field with every index
/// written `[*]`.
fn selector_of(
    path: &ObservationPath,
    request: Option<&RequestRecord>,
) -> Result<Option<Selector>, ReceiptError> {
    let Some(request) = request else {
        return Ok(None);
    };
    let principal = if request.actor.is_some() {
        "authenticated"
    } else {
        "anonymous"
    };
    let mut value = json!({
        "surface": "http",
        "method": request.method,
        "route_template": request.route_template,
        "step_context": { "kind": "any" },
        "principal": { "kind": principal },
        "channel": path.channel(),
    });
    if let Some(field) = field_of(path) {
        value["field_path"] = json!(field);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|err| inconsistent(&format!("a difference names no selector: {err}")))
}

fn field_of(path: &ObservationPath) -> Option<String> {
    let text = path.as_str();
    let rest = match path.channel() {
        Channel::Status | Channel::Any => return None,
        Channel::Header => text.strip_prefix("header.")?,
        Channel::Cookie => text.strip_prefix("cookie.")?,
        Channel::Body => {
            let rest = text.strip_prefix("body")?;
            if rest.is_empty() {
                return None;
            }
            rest.strip_prefix('.').unwrap_or(rest)
        }
        Channel::SideEffect => text.strip_prefix("side_effect.")?,
    };
    Some(wildcard_indices(rest))
}

fn wildcard_indices(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find(']') else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str("[*]");
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

/// What the acceptance predicates of a scope item read, as domains. A claimed difference that no
/// domain covers is not checked by any obligation, so it cannot be fulfilled.
#[derive(Default)]
struct Reads {
    domains: Vec<Domain>,
}

/// One read: a step or every step, a channel or every channel, a path under a pattern or every
/// path of the channel, and the requests an event filter admits or every request.
struct Domain {
    step: Option<StepId>,
    channel: Option<Channel>,
    path: Option<String>,
    requests: Option<EventFilter>,
}

impl Domain {
    fn at(step: &StepId, channel: Channel, path: Option<&str>) -> Self {
        Self {
            step: Some(step.clone()),
            channel: Some(channel),
            path: path.map(str::to_owned),
            requests: None,
        }
    }

    fn of_channel(channel: Channel, requests: Option<EventFilter>) -> Self {
        Self {
            step: None,
            channel: Some(channel),
            path: None,
            requests,
        }
    }

    /// Whether the read sees the difference. A read of a path sees the difference on that path
    /// and on any path above or below it, since a changed ancestor changes what it reads.
    fn covers(&self, difference: &Difference, request: Option<&RequestRecord>) -> bool {
        self.step
            .as_ref()
            .is_none_or(|step| *step == difference.step_id)
            && self
                .channel
                .is_none_or(|channel| channel == difference.path.channel())
            && self
                .path
                .as_deref()
                .is_none_or(|pattern| compare::overlaps(pattern, difference.path.as_str()))
            && self
                .requests
                .as_ref()
                .is_none_or(|filter| request.is_some_and(|request| filter_admits(filter, request)))
    }
}

fn filter_admits(filter: &EventFilter, request: &RequestRecord) -> bool {
    method_admits(filter.method, request.method)
        && filter
            .route_template
            .as_ref()
            .is_none_or(|route| *route == request.route_template)
        && principal_admits(&filter.principal, request.actor.as_ref())
        && filter
            .client
            .as_ref()
            .is_none_or(|client| *client == request.client)
}

impl Reads {
    fn covers(&self, claim: &Claim) -> bool {
        self.domains
            .iter()
            .any(|domain| domain.covers(&claim.difference, claim.request.as_ref()))
    }

    fn value(&mut self, value: &ValueRef) {
        match value {
            ValueRef::Field { step_id, path, .. } => {
                self.domains
                    .push(Domain::at(step_id, path.channel(), Some(path.as_str())));
            }
            ValueRef::Event { .. } => self
                .domains
                .push(Domain::of_channel(Channel::SideEffect, None)),
            // A capture names its source only through the fixture, which this reads do not have,
            // so a capture reads nothing and authorizes nothing.
            ValueRef::Capture { .. } => {}
        }
    }

    fn predicate(&mut self, predicate: &Predicate) {
        match predicate {
            Predicate::EqualsBase { step_id, path, .. } => {
                self.domains
                    .push(Domain::at(step_id, path.channel(), Some(path.as_str())));
            }
            Predicate::StatusIn { step_id, .. } => {
                self.domains
                    .push(Domain::at(step_id, Channel::Status, None));
            }
            Predicate::Equals { at, .. }
            | Predicate::Present { at }
            | Predicate::Absent { at }
            | Predicate::MatchesSchema { at, .. } => self.value(at),
            Predicate::AfterNWithin { then, .. } => self.predicate(then),
            Predicate::All { of } => of.iter().for_each(|part| self.predicate(part)),
            Predicate::Compare { left, right, .. } => {
                self.value(left);
                if let Operand::Ref { value_ref } = right {
                    self.value(value_ref);
                }
            }
            // A count reads the statuses of the requests it counts only when it filters on them.
            Predicate::Count { filter, .. } => {
                if filter.status_in.is_some() {
                    self.domains
                        .push(Domain::of_channel(Channel::Status, Some(filter.clone())));
                }
            }
            Predicate::Window { policy } => self.domains.push(Domain::of_channel(
                Channel::Status,
                Some(policy.event_filter.clone()),
            )),
        }
    }
}

fn reads_of(item: &ScopeItem) -> Reads {
    let mut reads = Reads::default();
    for acceptance in item.acceptance.iter() {
        reads.predicate(&acceptance.predicate);
    }
    reads
}

/// One obligation's answer over the probes that reach its selector.
struct Judged {
    obligation_id: ObligationId,
    result: CheckResult,
    probe: Option<usize>,
}

fn judge_obligations<'p>(
    inputs: &ReceiptInputs<'_>,
    obligations: impl Iterator<Item = &'p PredicateRef>,
    reaching: &[usize],
) -> Vec<Judged> {
    obligations
        .map(|obligation| {
            let answers = reaching
                .iter()
                .map(|&index| (index, answer(inputs, &obligation.predicate, index)))
                .collect();
            let (result, probe) = aggregate(answers);
            Judged {
                obligation_id: obligation.obligation_id.clone(),
                result,
                probe,
            }
        })
        .collect()
}

/// One predicate's answer on one probe. An evaluation error is an unknown, never a pass.
fn answer(inputs: &ReceiptInputs<'_>, predicate: &Predicate, index: usize) -> CheckResult {
    let result = &inputs.results[index];
    predicate::evaluate(
        predicate,
        &result.contrast,
        result.comparison,
        inputs.anchors,
    )
    .unwrap_or_else(|error| CheckResult::Unknown {
        reason: UnknownReason::Error {
            detail: error.to_string(),
        },
    })
}

/// Combines the answers over several probes. A failure wins, then the worst unknown, then a
/// pass. No answer at all is an unknown.
fn aggregate(answers: Vec<(usize, CheckResult)>) -> (CheckResult, Option<usize>) {
    if answers.is_empty() {
        return (
            CheckResult::Unknown {
                reason: UnknownReason::Error {
                    detail: "no probe reaches this selector".to_owned(),
                },
            },
            None,
        );
    }
    if let Some((index, failure)) = answers
        .iter()
        .find(|(_, result)| matches!(result, CheckResult::Fail { .. }))
    {
        return (failure.clone(), Some(*index));
    }
    let mut worst: Option<(usize, UnknownReason)> = None;
    for (index, result) in &answers {
        if let CheckResult::Unknown { reason } = result
            && worst
                .as_ref()
                .is_none_or(|(_, held)| rank(reason) > rank(held))
        {
            worst = Some((*index, reason.clone()));
        }
    }
    match worst {
        Some((index, reason)) => (CheckResult::Unknown { reason }, Some(index)),
        None => (
            CheckResult::Pass {},
            answers.first().map(|(index, _)| *index),
        ),
    }
}

fn rank(reason: &UnknownReason) -> u8 {
    match reason {
        UnknownReason::UnstableBase {} => 3,
        UnknownReason::Incomparable { .. } => 2,
        UnknownReason::Incomplete {}
        | UnknownReason::RunFailed {}
        | UnknownReason::Stale {}
        | UnknownReason::Skipped {}
        | UnknownReason::Error { .. } => 1,
    }
}

fn failing_probe(judged: &[Judged]) -> Option<usize> {
    judged
        .iter()
        .find(|judgment| matches!(judgment.result, CheckResult::Fail { .. }))
        .and_then(|judgment| judgment.probe)
}

fn failing_ids(judged: &[Judged]) -> Vec<ObligationId> {
    judged
        .iter()
        .filter(|judgment| matches!(judgment.result, CheckResult::Fail { .. }))
        .map(|judgment| judgment.obligation_id.clone())
        .collect()
}

fn worst_unknown(judged: &[Judged]) -> Option<&UnknownReason> {
    let mut worst: Option<&UnknownReason> = None;
    for judgment in judged {
        if let CheckResult::Unknown { reason } = &judgment.result
            && worst.is_none_or(|held| rank(reason) > rank(held))
        {
            worst = Some(reason);
        }
    }
    worst
}

fn class_of(reason: &UnknownReason) -> ItemClass {
    match reason {
        UnknownReason::UnstableBase {} => ItemClass::UnstableBase,
        UnknownReason::Incomparable { .. } => ItemClass::Incomparable,
        UnknownReason::Incomplete {}
        | UnknownReason::RunFailed {}
        | UnknownReason::Stale {}
        | UnknownReason::Skipped {}
        | UnknownReason::Error { .. } => ItemClass::Error,
    }
}

fn is_fail(result: &CheckResult) -> bool {
    matches!(result, CheckResult::Fail { .. })
}

fn may_class(judged: &[Judged], claimed: &[&Claim], reads: &Reads) -> ItemClass {
    if judged.iter().any(|judgment| is_fail(&judgment.result)) {
        ItemClass::ObligationFailed
    } else if let Some(reason) = worst_unknown(judged) {
        class_of(reason)
    } else if claimed.iter().any(|claim| !reads.covers(claim)) {
        ItemClass::AuthorizedButUnspecified
    } else {
        ItemClass::ObligationFulfilled
    }
}

fn must_class(judged: &[Judged], claimed: bool) -> ItemClass {
    if claimed || judged.iter().any(|judgment| is_fail(&judgment.result)) {
        ItemClass::ObligationFailed
    } else if let Some(reason) = worst_unknown(judged) {
        class_of(reason)
    } else {
        ItemClass::Preserved
    }
}

/// The episode a failing item belongs to: one keyed by its selector and a failed obligation,
/// else one keyed by its selector alone.
fn episode_for<'e>(
    episodes: &'e [DiscrepancyEpisode],
    selector: &Selector,
    failing: &[ObligationId],
) -> Option<&'e DiscrepancyEpisode> {
    let canonical = canonical_episode_selector(selector);
    let same: Vec<&DiscrepancyEpisode> = episodes
        .iter()
        .filter(|episode| episode.selector == canonical)
        .collect();
    same.iter()
        .copied()
        .find(|episode| {
            episode
                .obligation_id
                .as_ref()
                .is_some_and(|id| failing.contains(id))
        })
        .or_else(|| {
            same.iter()
                .copied()
                .find(|episode| episode.obligation_id.is_none())
        })
}

fn state_of(episode: Option<&DiscrepancyEpisode>) -> ItemState {
    match episode.map(|episode| episode.state) {
        None | Some(EpisodeState::Ruled) => ItemState::Open,
        Some(EpisodeState::Open) => ItemState::Open,
        Some(EpisodeState::Bounced) => ItemState::Bounced,
        Some(EpisodeState::Fixed) => ItemState::Fixed,
        Some(EpisodeState::Residue | EpisodeState::Contested) => ItemState::Residue,
        Some(EpisodeState::Stopped) => ItemState::Stopped,
    }
}

fn attempts_of(episode: Option<&DiscrepancyEpisode>) -> u32 {
    episode.map_or(0, |episode| episode.bounce_count)
}

fn budget(inputs: &ReceiptInputs<'_>) -> u32 {
    inputs.confirmation.budget.repair_rounds.get()
}

fn claimed_in<'c>(claimed: &[&'c Claim], probe: usize) -> Vec<&'c Claim> {
    claimed
        .iter()
        .copied()
        .filter(|claim| claim.probe == probe)
        .collect()
}

/// A scope item or rule no probe reached, with every obligation it has, all unchecked.
fn unreached(item_id: ItemId, selector: &Selector, judged: &[Judged]) -> NotReached {
    NotReached {
        item_id,
        selector: canonical_episode_selector(selector),
        obligation_ids: judged
            .iter()
            .map(|judgment| judgment.obligation_id.clone())
            .collect(),
    }
}

fn obligation_checks(judged: &[Judged]) -> Vec<ObligationCheck> {
    judged
        .iter()
        .map(|judgment| ObligationCheck {
            obligation_id: judgment.obligation_id.clone(),
            result: judgment.result.clone(),
        })
        .collect()
}

fn item_outcome(inputs: &ReceiptInputs<'_>, reaching: &[usize], claimed: &[&Claim]) -> Outcome {
    for &index in reaching {
        if let Outcome::Unknown { reason } = &inputs.results[index].comparison.value().outcome {
            return Outcome::Unknown {
                reason: reason.clone(),
            };
        }
    }
    if claimed.is_empty() {
        Outcome::Unchanged {}
    } else {
        Outcome::Changed {
            differences: claimed
                .iter()
                .map(|claim| claim.difference.clone())
                .collect(),
        }
    }
}

/// What a may-change item's receipt item is built from.
struct Placement<'a, 'c> {
    item: &'a ScopeItem,
    class: ItemClass,
    probe: usize,
    episode: Option<&'a DiscrepancyEpisode>,
    hashes: &'a [ProbeHash],
    reaching: &'a [usize],
    claimed: &'a [&'c Claim],
    judged: &'a [Judged],
}

fn item_for(inputs: &ReceiptInputs<'_>, placement: Placement<'_, '_>) -> ReceiptItem {
    let Placement {
        item,
        class,
        probe,
        episode,
        hashes,
        reaching,
        claimed,
        judged,
    } = placement;
    ReceiptItem {
        item_id: item.id.clone(),
        class,
        state: state_of(episode),
        machine_id: item.machine_ids.first().clone(),
        selector: Some(canonical_episode_selector(&item.selector)),
        probe_hash: hashes[probe],
        episode_id: episode.map(|episode| episode.id),
        ruling_id: None,
        attempts: attempts_of(episode),
        budget: budget(inputs),
        route_path: inputs.results[probe].contrast.probe.route_path.clone(),
        sequence: sequence(&inputs.results[probe]),
        evidence: evidence(&inputs.results[probe], &claimed_in(claimed, probe)),
        evaluation: Evaluation {
            authorization: Authorization::MayChange {
                item_id: item.id.clone(),
            },
            outcome: item_outcome(inputs, reaching, claimed),
            obligations: obligation_checks(judged),
        },
    }
}

/// Whether a ruling's approval covers one probe's evidence. The probe's normalization plan must be
/// the one the ruling was confirmed under. An exact approval also needs the evidence to be one it
/// accepted; a standing approval runs on whatever evidence arrives.
fn applies(inputs: &ReceiptInputs<'_>, expectation: &Expectation, index: usize) -> bool {
    let contrast = &inputs.results[index].contrast;
    let current = contrast
        .probe
        .normalization
        .normalization_hash()
        .is_ok_and(|plan| plan == expectation.normalization_hash);
    if !current {
        return false;
    }
    match expectation.applicability {
        Applicability::StandingPredicateApproval => true,
        Applicability::ExactObservationApproval => inputs.anchors.iter().any(|anchor| {
            expectation.accepts.contains(&anchor.hash())
                && same_evidence(contrast, anchor.contrast())
        }),
    }
}

/// Whether two contrasts hold the same probe and the same three runs, observation by observation.
fn same_evidence(left: &Contrast<'_>, right: &Contrast<'_>) -> bool {
    left.probe.spec == right.probe.spec
        && left.base_a == right.base_a
        && left.base_b == right.base_b
        && left.candidate == right.candidate
}

/// The status of one ruling, and the receipt items it adds. A failing probe becomes a
/// `violates_ruling` item, so the violation has a row of its own, and the differences it covers
/// there are the ruling's, not out of scope. A ruling whose approval covers some reached evidence
/// only, or none, is `stale` and never held.
///
/// A standing ruling rules on change: its predicate runs only on a probe where a field it covers
/// differs between base and candidate. A reached probe whose covered fields agree with the base
/// neither violates the ruling nor makes it stale; the ruling holds there without a check. A held
/// standing ruling takes the differences it covers that no must-keep rule or may-change item
/// owns, one fulfilled item per probe it takes them on. A ruling never takes over an item's
/// difference.
fn ruling(
    inputs: &ReceiptInputs<'_>,
    expectation: &Expectation,
    claims: &[Claim],
    owners: &mut [Owner],
    hashes: &[ProbeHash],
    usable: &[bool],
    behavior: &mut Behavior,
) -> Result<(RulingStatus, Vec<ReceiptItem>), ReceiptError> {
    if expectation.status == ExpectationStatus::Stale {
        behavior.record(Verdict::Unknown);
        return Ok((RulingStatus::Stale {}, Vec::new()));
    }
    let reaching = reaching_probes(inputs, usable, &expectation.selector);
    if reaching.is_empty() {
        behavior.record(Verdict::Unknown);
        return Ok((RulingStatus::NotReached {}, Vec::new()));
    }
    let current: Vec<usize> = reaching
        .iter()
        .copied()
        .filter(|&index| applies(inputs, expectation, index))
        .collect();
    let stale = current.len() < reaching.len();
    let checked: Vec<usize> = match expectation.applicability {
        Applicability::StandingPredicateApproval => current
            .iter()
            .copied()
            .filter(|&index| {
                claims
                    .iter()
                    .any(|claim| claim.probe == index && claim.covered_by(&expectation.selector))
            })
            .collect(),
        // An exact approval runs on the evidence it accepted, changed or not.
        Applicability::ExactObservationApproval => current,
    };
    let answers: Vec<(usize, CheckResult)> = checked
        .iter()
        .map(|&index| (index, answer(inputs, &expectation.predicate, index)))
        .collect();
    let failing: Vec<usize> = answers
        .iter()
        .filter(|(_, result)| is_fail(result))
        .map(|(index, _)| *index)
        .collect();
    if failing.is_empty() {
        if stale {
            behavior.record(Verdict::Unknown);
            return Ok((RulingStatus::Stale {}, Vec::new()));
        }
        if checked.is_empty() {
            // Nothing the ruling covers differs on any reached probe: it holds.
            behavior.record(Verdict::Pass);
            return Ok((RulingStatus::Held {}, Vec::new()));
        }
        return Ok(match aggregate(answers).0 {
            CheckResult::Unknown { reason } => {
                behavior.record(Verdict::Unknown);
                (RulingStatus::Unknown { reason }, Vec::new())
            }
            CheckResult::Pass {} | CheckResult::Fail { .. } => {
                behavior.record(Verdict::Pass);
                let items = match expectation.applicability {
                    Applicability::StandingPredicateApproval => {
                        held_items(inputs, expectation, claims, owners, hashes, &checked)?
                    }
                    Applicability::ExactObservationApproval => Vec::new(),
                };
                (RulingStatus::Held {}, items)
            }
        });
    }
    behavior.record(Verdict::Fail);
    let mut items = Vec::new();
    for &index in &failing {
        // The violation's row is the ruling's: the same difference is not also out of scope.
        take_for_ruling(owners, claims, index, &expectation.selector);
        let claimed: Vec<&Claim> = claims
            .iter()
            .filter(|claim| claim.probe == index && claim.covered_by(&expectation.selector))
            .collect();
        // A violation names no obligation, so its episode is the one keyed by its selector alone.
        // Without one the origin refuses the failing item and the candidate cannot be recorded.
        let episode = episode_for(inputs.episodes, &expectation.selector, &[]);
        items.push(ReceiptItem {
            item_id: minted("ruling", &(&expectation.ruling_id, hashes[index]))?,
            class: ItemClass::ViolatesRuling,
            state: state_of(episode),
            machine_id: expectation.machine_ids.first().clone(),
            selector: Some(canonical_episode_selector(&expectation.selector)),
            probe_hash: hashes[index],
            episode_id: episode.map(|episode| episode.id),
            ruling_id: Some(expectation.ruling_id.clone()),
            attempts: attempts_of(episode),
            budget: budget(inputs),
            route_path: inputs.results[index].contrast.probe.route_path.clone(),
            sequence: sequence(&inputs.results[index]),
            evidence: evidence(&inputs.results[index], &claimed),
            evaluation: Evaluation {
                authorization: Authorization::Ruling {
                    ruling_id: expectation.ruling_id.clone(),
                },
                outcome: inputs.results[index].comparison.value().outcome.clone(),
                obligations: Vec::new(),
            },
        });
    }
    let mut ids = items.iter().map(|item| item.item_id.clone());
    let first = ids
        .next()
        .ok_or_else(|| inconsistent("a violated ruling has no failing probe"))?;
    let item_ids = NonEmpty::new(first, ids);
    Ok((RulingStatus::Violated { item_ids }, items))
}

/// Marks the differences on `probe` that `selector` covers and no item owns yet as the ruling's,
/// and returns their indices.
fn take_for_ruling(
    owners: &mut [Owner],
    claims: &[Claim],
    probe: usize,
    selector: &Selector,
) -> Vec<usize> {
    let taken: Vec<usize> = (0..claims.len())
        .filter(|&index| {
            owners[index] == Owner::Unclaimed
                && claims[index].probe == probe
                && claims[index].covered_by(selector)
        })
        .collect();
    for &index in &taken {
        owners[index] = Owner::Ruling;
    }
    taken
}

/// The fulfilled items of a held standing approval: on each checked probe, the differences it
/// takes over, one item per probe that has any.
fn held_items(
    inputs: &ReceiptInputs<'_>,
    expectation: &Expectation,
    claims: &[Claim],
    owners: &mut [Owner],
    hashes: &[ProbeHash],
    probes: &[usize],
) -> Result<Vec<ReceiptItem>, ReceiptError> {
    let mut items = Vec::new();
    for &probe in probes {
        let taken = take_for_ruling(owners, claims, probe, &expectation.selector);
        if taken.is_empty() {
            continue;
        }
        let claimed: Vec<&Claim> = taken.iter().map(|&index| &claims[index]).collect();
        items.push(ruling_item(inputs, expectation, hashes, probe, &claimed)?);
    }
    Ok(items)
}

/// The fulfilled item of one probe a standing approval holds. It names the ruling, not an
/// obligation: the ruling's own predicate is the check that held.
fn ruling_item(
    inputs: &ReceiptInputs<'_>,
    expectation: &Expectation,
    hashes: &[ProbeHash],
    probe: usize,
    claimed: &[&Claim],
) -> Result<ReceiptItem, ReceiptError> {
    let result = &inputs.results[probe];
    let episode = episode_for(inputs.episodes, &expectation.selector, &[]);
    Ok(ReceiptItem {
        item_id: minted("ruling", &(&expectation.ruling_id, hashes[probe]))?,
        class: ItemClass::ObligationFulfilled,
        state: state_of(episode),
        machine_id: expectation.machine_ids.first().clone(),
        selector: Some(canonical_episode_selector(&expectation.selector)),
        probe_hash: hashes[probe],
        episode_id: episode.map(|episode| episode.id),
        ruling_id: Some(expectation.ruling_id.clone()),
        attempts: attempts_of(episode),
        budget: budget(inputs),
        route_path: result.contrast.probe.route_path.clone(),
        sequence: sequence(result),
        evidence: evidence(result, claimed),
        evaluation: Evaluation {
            authorization: Authorization::Ruling {
                ruling_id: expectation.ruling_id.clone(),
            },
            outcome: item_outcome(inputs, &[probe], claimed),
            obligations: Vec::new(),
        },
    })
}

fn out_of_scope(
    inputs: &ReceiptInputs<'_>,
    claim: &Claim,
    hashes: &[ProbeHash],
    episodes: &[DiscrepancyEpisode],
) -> Result<ReceiptItem, ReceiptError> {
    let result = &inputs.results[claim.probe];
    let selector = selector_of(&claim.difference.path, claim.request.as_ref())?;
    let episode = selector
        .as_ref()
        .and_then(|selector| episode_for(episodes, selector, &[]));
    let difference = claim.difference.clone();
    Ok(ReceiptItem {
        item_id: minted(
            "oos",
            &(
                &hashes[claim.probe],
                &claim.difference.step_id,
                &claim.difference.path,
            ),
        )?,
        class: ItemClass::OutOfScope,
        state: state_of(episode),
        machine_id: result.contrast.probe.route_path.machines.first().clone(),
        selector: selector.as_ref().map(canonical_episode_selector),
        probe_hash: hashes[claim.probe],
        episode_id: episode.map(|episode| episode.id),
        ruling_id: None,
        attempts: attempts_of(episode),
        budget: budget(inputs),
        route_path: result.contrast.probe.route_path.clone(),
        sequence: sequence(result),
        evidence: evidence(result, &[claim]),
        evaluation: Evaluation {
            authorization: Authorization::None {},
            outcome: Outcome::Changed {
                differences: vec![difference],
            },
            obligations: Vec::new(),
        },
    })
}

/// A minted item id: the prefix and the digest of what the item is about.
fn minted(prefix: &str, value: &impl Serialize) -> Result<ItemId, ReceiptError> {
    let digest = ContentHash::of(value).map_err(hashing)?;
    ItemId::from_str(&format!("{prefix}:{digest}"))
        .map_err(|_| inconsistent("a minted item id is not an item id"))
}

fn sequence(result: &ProbeResult<'_>) -> Vec<SequenceStep> {
    let outcome = &result.comparison.value().outcome;
    let runs = [
        (RunRole::BaseA, result.contrast.base_a),
        (RunRole::BaseB, result.contrast.base_b),
        (RunRole::Candidate, result.contrast.candidate),
    ];
    let mut steps = Vec::new();
    for (role, run) in runs {
        let mut request = 0_u32;
        for (_, observation) in run.observations() {
            if observation.request.is_none() {
                continue;
            }
            request += 1;
            steps.push(SequenceStep {
                step_id: observation.step_id.clone(),
                request,
                run_id: run.run().id.clone(),
                role,
                status: observation.channels.status,
                tint: tint(outcome, &observation.step_id),
            });
        }
    }
    steps
}

fn tint(outcome: &Outcome, step: &StepId) -> StepTint {
    match outcome {
        Outcome::Unknown { .. } => StepTint::Unknown,
        Outcome::Changed { differences }
            if differences
                .iter()
                .any(|difference| difference.step_id == *step) =>
        {
            StepTint::Differed
        }
        Outcome::Changed { .. } | Outcome::Unchanged {} => StepTint::Same,
    }
}

/// The evidence rows of the differences in one probe: base A's and the candidate's observed
/// values, each addressed by its run, commit, observation and field. The values are the
/// canonical ones a difference carries.
fn evidence(result: &ProbeResult<'_>, claims: &[&Claim]) -> Vec<EvidenceRow> {
    let mut rows = Vec::new();
    for claim in claims {
        let difference = &claim.difference;
        let sides = [
            (
                EvidenceSlot::BaseA,
                result.contrast.base_a,
                &difference.base,
            ),
            (
                EvidenceSlot::Candidate,
                result.contrast.candidate,
                &difference.candidate,
            ),
        ];
        for (slot, run, value) in sides {
            let Some(observed) = run.observed(&difference.step_id, &difference.path) else {
                continue;
            };
            let address = observed.address();
            rows.push(EvidenceRow {
                slot,
                run_id: Some(address.run_id),
                commit: Some(address.commit),
                observation_hash: Some(address.observation_hash),
                step: difference.step_id.clone(),
                field: difference.path.clone(),
                value: value.clone(),
                provenance: Provenance::Observed,
            });
        }
    }
    rows
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
    Unknown,
}

fn verdict(result: &CheckResult) -> Verdict {
    match result {
        CheckResult::Pass {} => Verdict::Pass,
        CheckResult::Fail { .. } => Verdict::Fail,
        CheckResult::Unknown { .. } => Verdict::Unknown,
    }
}

#[derive(Default)]
struct Behavior {
    passed: u32,
    failed: u32,
    unknown: u32,
}

impl Behavior {
    fn record(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Pass => self.passed += 1,
            Verdict::Fail => self.failed += 1,
            Verdict::Unknown => self.unknown += 1,
        }
    }

    fn counts(&self) -> BehaviorCounts {
        BehaviorCounts {
            obligations_total: self.passed + self.failed + self.unknown,
            passed: self.passed,
            failed: self.failed,
            unknown: self.unknown,
        }
    }
}

fn tally(items: &[ReceiptItem]) -> ClassCounts {
    let mut counts = ClassCounts {
        preserved: 0,
        obligation_fulfilled: 0,
        obligation_failed: 0,
        authorized_but_unspecified: 0,
        out_of_scope: 0,
        violates_ruling: 0,
        unstable_base: 0,
        incomparable: 0,
        error: 0,
    };
    for item in items {
        let slot = match item.class {
            ItemClass::Preserved => &mut counts.preserved,
            ItemClass::ObligationFulfilled => &mut counts.obligation_fulfilled,
            ItemClass::ObligationFailed => &mut counts.obligation_failed,
            ItemClass::AuthorizedButUnspecified => &mut counts.authorized_but_unspecified,
            ItemClass::OutOfScope => &mut counts.out_of_scope,
            ItemClass::ViolatesRuling => &mut counts.violates_ruling,
            ItemClass::UnstableBase => &mut counts.unstable_base,
            ItemClass::Incomparable => &mut counts.incomparable,
            ItemClass::Error => &mut counts.error,
        };
        *slot += 1;
    }
    counts
}

/// A count as a receipt states it. A count that does not fit is refused, never saturated.
fn receipt_count(value: usize) -> Result<u32, ReceiptError> {
    u32::try_from(value).map_err(|_| inconsistent("a count does not fit in a receipt"))
}

fn probe_counts(inputs: &ReceiptInputs<'_>) -> Result<ProbeCounts, ReceiptError> {
    let retired = &inputs.ledger.value().retired_probes;
    let mut active = 0_usize;
    for probe in &inputs.corpus.probes {
        let hash = probe.spec.probe_hash().map_err(hashing)?;
        if !retired.contains(&hash) {
            active += 1;
        }
    }
    let errored = inputs
        .results
        .iter()
        .filter(|result| {
            [
                &result.contrast.base_a.run().outcome,
                &result.contrast.base_b.run().outcome,
                &result.contrast.candidate.run().outcome,
            ]
            .iter()
            .any(|outcome| !matches!(outcome, RunOutcome::Completed {}))
        })
        .count();
    Ok(ProbeCounts {
        total: receipt_count(active)?,
        run: receipt_count(inputs.results.len())?,
        errored: receipt_count(errored)?,
    })
}

fn route_counts(inputs: &ReceiptInputs<'_>) -> Result<RouteCounts, ReceiptError> {
    let mut reached: Vec<(HttpMethod, RouteTemplate)> = Vec::new();
    for result in inputs.results {
        for run in [
            result.contrast.base_a,
            result.contrast.base_b,
            result.contrast.candidate,
        ] {
            for (_, observation) in run.observations() {
                if let Some(record) = &observation.request {
                    let key = (record.method, record.route_template.clone());
                    if !reached.contains(&key) {
                        reached.push(key);
                    }
                }
            }
        }
    }
    let mut covered = 0_usize;
    let mut uncovered = Vec::new();
    for entry in &inputs.corpus.routes {
        let key = (entry.method, entry.route_template.clone());
        if reached.contains(&key) {
            covered += 1;
        } else {
            uncovered.push(UncoveredRoute {
                method: entry.method,
                route_template: entry.route_template.clone(),
                reason: match &entry.status {
                    RouteStatus::Uncovered { reason } => reason.clone(),
                    RouteStatus::Covered {} => "no run probe reached it".to_owned(),
                },
            });
        }
    }
    Ok(RouteCounts {
        total: receipt_count(inputs.corpus.routes.len())?,
        covered: receipt_count(covered)?,
        uncovered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_index_matches_every_index() {
        assert_eq!(wildcard_indices("links[3].code"), "links[*].code");
        assert_eq!(wildcard_indices("[0][12].x"), "[*][*].x");
        assert_eq!(wildcard_indices("plain"), "plain");
    }

    #[test]
    fn an_unclosed_bracket_is_copied_once() {
        assert_eq!(wildcard_indices("links[3"), "links[3");
        assert_eq!(wildcard_indices("a[0].b[x"), "a[*].b[x");
    }

    #[test]
    fn a_failure_outranks_an_unknown_and_an_unknown_outranks_a_pass() {
        let unknown = CheckResult::Unknown {
            reason: UnknownReason::Incomparable {
                detail: "x".to_owned(),
            },
        };
        let fail = CheckResult::Fail {
            detail: "x".to_owned(),
        };
        let (result, _) = aggregate(vec![(0, CheckResult::Pass {}), (1, unknown.clone())]);
        assert_eq!(result, unknown);
        let (result, probe) = aggregate(vec![(0, unknown), (1, fail.clone())]);
        assert_eq!(result, fail);
        assert_eq!(probe, Some(1));
        let (result, probe) = aggregate(vec![(3, CheckResult::Pass {}), (4, CheckResult::Pass {})]);
        assert_eq!(result, CheckResult::Pass {});
        assert_eq!(probe, Some(3));
    }
}
