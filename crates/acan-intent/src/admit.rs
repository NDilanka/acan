//! Admission: the only way stored records become evidence the gate may weigh.
//!
//! Storage (S01) reads observations by hash and decodes them as plain records. Before any of
//! them reaches normalization, masking, comparison, a predicate or a receipt, it passes through
//! a function here, which checks it against the stored hashes and the run record and returns an
//! admitted type with private fields. Nothing else constructs one.
//!
//! What a reached step leaves behind, by kind. This is the one completeness rule for runs,
//! retained examples ([`crate::RunManifest`]) and twin runs:
//! - `http` and `observe_side_effect` steps: exactly one stored observation each, which may be
//!   [`crate::Completeness::Incomplete`] but is never left out.
//! - `capture` steps: exactly one [`crate::CaptureOutcome`] in [`Run::captures`] (captured or
//!   missing) naming the step and its binding, and no observation. The outcome proves the step
//!   was reached.
//! - `advance_time` steps: no record. A clock step is reached when the run completed or stopped
//!   after it.
//!
//! Records follow the sequence (the fixture recipe's steps, then the probe's) in order. Which
//! steps were reached follows from [`RunOutcome`]:
//! - `completed`: every step. A completed run missing any observation or capture outcome is
//!   refused.
//! - `timed_out` and `setup_failed` with a step: every step before the named one, which must be
//!   in the sequence (in the recipe for `setup_failed`). The named step may hold its record; no
//!   later step holds one.
//! - `setup_failed` without a step, and `isolation_unavailable`: no step. The run has no record.
//! - `crashed`, `cancelled` and `error`, which name no step: a prefix of the sequence. Every
//!   step before the last record holds its own.
//!
//! [`tour`] admits a model-proposed tour over a plant view by the rule in the
//! [`crate::PlantView`] module docs, and binds its captions.
//!
//! [`retained_example`] admits a retained ruling example against its manifest.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::compare::{self, CompareError, Contrast};
use crate::error::Unsupported;
use crate::hash::{CanonicalError, ObservationHash, ReceiptHash};
use crate::ledger::ContrastExample;
use crate::observation::{Observation, ObservedRun};
use crate::predicate::RetainedExample;
use crate::probe::{CorpusProbe, Fixture, Run, RunOutcome, Step};
use crate::provenance::CoreDerived;
use crate::types::{CatalogVersion, ItemId, MachineId, SlotName, StepId, TemplateId};
use crate::verdict::{Comparison, ItemClass, ItemState};
use crate::view::{
    BindableValue, BoundItemView, CaptionCatalog, Coverage, EvidenceRef, MarkRef, PlantItem,
    PlantView, Tour, TourStep,
};

/// Why stored records were not admitted. Never evidence, never a pass.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AdmitError {
    /// Not implemented in this build.
    #[error(transparent)]
    Unsupported(#[from] Unsupported),
    /// A record has no canonical JSON form, so it has no hash.
    #[error("a record has no canonical JSON form: {detail}")]
    Unhashable {
        /// The serializer's message.
        detail: String,
    },
    /// The run is not of this probe, or the probe not of this fixture.
    #[error("the run is not of this probe and fixture")]
    WrongSpec,
    /// A part of the retained example is not the one the contrast was rebuilt from.
    #[error("the retained example's {what} is not the contrast's")]
    CrossedEvidence {
        /// Which part does not match.
        what: &'static str,
    },
    /// The contrast cannot be compared, so the example has no comparison to check.
    #[error("the contrast cannot be compared: {0}")]
    Uncomparable(#[from] CompareError),
    /// A stored hash is not its observation's.
    #[error("the observation of step {step_id} is not stored under its own hash")]
    HashMismatch {
        /// The observation's step.
        step_id: StepId,
    },
    /// An observation names another run, commit or probe.
    #[error("the observation of step {step_id} names another run, commit or probe")]
    ForeignObservation {
        /// The observation's step.
        step_id: StepId,
    },
    /// A record is out of sequence order, repeated, or names a step that leaves no such record.
    #[error("the record of step {step_id} is out of sequence")]
    OutOfSequence {
        /// The record's step.
        step_id: StepId,
    },
    /// A step the run reached has no record.
    #[error("step {step_id} was reached but has no record")]
    Unrecorded {
        /// The step.
        step_id: StepId,
    },
    /// A step after the one where the run stopped has a record.
    #[error("step {step_id} has a record after the run stopped")]
    AfterStop {
        /// The step.
        step_id: StepId,
    },
    /// The outcome names a step outside the part of the sequence it can stop in.
    #[error("the run stopped at {step_id}, which is not a step it could stop at")]
    UnknownStop {
        /// The step.
        step_id: StepId,
    },
}

impl From<CanonicalError> for AdmitError {
    fn from(err: CanonicalError) -> Self {
        Self::Unhashable {
            detail: err.to_string(),
        }
    }
}

/// Admits one run and its stored observations as evidence.
///
/// Checks, all of which must hold: `probe` starts from `fixture`; `run.probe` is `probe`'s
/// [`crate::ProbeSpec::probe_hash`] and `run.env.fixture_hash` is `fixture`'s
/// [`Fixture::fixture_hash`]; every `stored` hash equals its observation's
/// [`Observation::observation_hash`]; every observation names `run`'s id, commit and probe; and
/// the observations and [`Run::captures`] are exactly the records the module rule asks of the
/// steps the outcome reached, in sequence order.
///
/// # Errors
///
/// Returns [`AdmitError`] when any check fails.
pub fn observed_run(
    probe: &CorpusProbe,
    fixture: &Fixture,
    run: Run,
    stored: Vec<(ObservationHash, Observation)>,
) -> Result<ObservedRun, AdmitError> {
    if probe.spec.fixture != fixture.id
        || run.probe != probe.spec.probe_hash()?
        || run.env.fixture_hash != fixture.fixture_hash()?
    {
        return Err(AdmitError::WrongSpec);
    }
    for (hash, observation) in &stored {
        let step_id = || observation.step_id.clone();
        if *hash != observation.observation_hash()? {
            return Err(AdmitError::HashMismatch { step_id: step_id() });
        }
        if observation.run_id != run.id
            || observation.commit != run.commit
            || observation.probe != run.probe
        {
            return Err(AdmitError::ForeignObservation { step_id: step_id() });
        }
    }
    let sequence: Vec<&Step> = fixture
        .recipe
        .iter()
        .chain(probe.spec.steps.iter())
        .collect();
    let observed = stored.iter().map(|(_, observation)| &observation.step_id);
    check_records(&sequence, fixture.recipe.len(), &run, observed)?;
    Ok(ObservedRun::admitted(run, stored))
}

/// Which steps of the sequence a run's outcome says were reached.
enum Reached {
    /// Every step.
    All,
    /// Every step before this index; the step at it may hold its record.
    Through(usize),
    /// None.
    Nothing,
    /// A prefix the outcome does not name.
    Prefix,
}

fn reached(
    sequence: &[&Step],
    recipe_len: usize,
    outcome: &RunOutcome,
) -> Result<Reached, AdmitError> {
    let stop_in = |steps: &[&Step], step_id: &StepId| {
        steps
            .iter()
            .position(|step| step.id() == step_id)
            .map(Reached::Through)
            .ok_or_else(|| AdmitError::UnknownStop {
                step_id: step_id.clone(),
            })
    };
    match outcome {
        RunOutcome::Completed {} => Ok(Reached::All),
        RunOutcome::TimedOut { step_id } => stop_in(sequence, step_id),
        RunOutcome::SetupFailed {
            step_id: Some(step_id),
            ..
        } => stop_in(&sequence[..recipe_len], step_id),
        RunOutcome::SetupFailed { step_id: None, .. } | RunOutcome::IsolationUnavailable { .. } => {
            Ok(Reached::Nothing)
        }
        RunOutcome::Crashed { .. } | RunOutcome::Cancelled {} | RunOutcome::Error { .. } => {
            Ok(Reached::Prefix)
        }
    }
}

/// Checks the module's completeness rule over the observed steps and the run's captures.
fn check_records<'a>(
    sequence: &[&Step],
    recipe_len: usize,
    run: &Run,
    observed: impl Iterator<Item = &'a StepId>,
) -> Result<(), AdmitError> {
    let mut observed = observed.peekable();
    let mut captures = run.captures.iter().peekable();
    let mut first_gap = None;
    let mut last_record = None;
    for (index, step) in sequence.iter().enumerate() {
        let recorded = match step {
            Step::Http(_) | Step::ObserveSideEffect(_) => {
                observed.next_if(|id| *id == step.id()).is_some()
            }
            Step::Capture(capture) => captures
                .next_if(|outcome| {
                    outcome.step_id() == &capture.id && outcome.binding() == &capture.binding
                })
                .is_some(),
            Step::AdvanceTime(_) => continue,
        };
        if !recorded {
            first_gap.get_or_insert(index);
        } else if let Some(gap) = first_gap {
            return Err(AdmitError::Unrecorded {
                step_id: sequence[gap].id().clone(),
            });
        } else {
            last_record = Some(index);
        }
    }
    let stray = observed
        .next()
        .or_else(|| captures.next().map(|outcome| outcome.step_id()));
    if let Some(step_id) = stray {
        return Err(AdmitError::OutOfSequence {
            step_id: step_id.clone(),
        });
    }
    let unrecorded = |gap: usize| AdmitError::Unrecorded {
        step_id: sequence[gap].id().clone(),
    };
    let after_stop = |last: usize| AdmitError::AfterStop {
        step_id: sequence[last].id().clone(),
    };
    match reached(sequence, recipe_len, &run.outcome)? {
        Reached::All => first_gap.map_or(Ok(()), |gap| Err(unrecorded(gap))),
        Reached::Through(stop) => match (first_gap, last_record) {
            (Some(gap), _) if gap < stop => Err(unrecorded(gap)),
            (_, Some(last)) if last > stop => Err(after_stop(last)),
            _ => Ok(()),
        },
        Reached::Nothing => last_record.map_or(Ok(()), |last| Err(after_stop(last))),
        Reached::Prefix => Ok(()),
    }
}

/// Admits a retained ruling example: the contrast rebuilt from its runs, checked against the
/// manifest it was retained under.
///
/// Checks, all of which must hold:
/// - `contrast`'s probe (normalization plan included) and fixture are the manifest's.
/// - Each role's run belongs to `contrast`'s probe and fixture ([`AdmitError::WrongSpec`]),
///   its run record is the manifest's, and its stored observation hashes, in sequence order,
///   are the manifest's.
/// - The base runs were made at the manifest context's protected head, and the candidate at
///   its merged commit.
/// - `comparison` is exactly [`compare::compare`] of `contrast` under
///   [`compare::derive_masks`], so it is the comparison of that contrast and no other.
///
/// The example's identity is [`ContrastExample::contrast_example_hash`].
///
/// # Errors
///
/// Returns [`AdmitError`] when the contrast does not match the manifest, or cannot be compared.
pub fn retained_example<'a>(
    manifest: &ContrastExample,
    contrast: Contrast<'a>,
    comparison: &'a CoreDerived<Comparison>,
) -> Result<RetainedExample<'a>, AdmitError> {
    if manifest.probe != *contrast.probe {
        return Err(crossed("probe"));
    }
    if manifest.fixture != *contrast.fixture {
        return Err(crossed("fixture"));
    }
    let probe_hash = contrast.probe.spec.probe_hash()?;
    let fixture_hash = contrast.fixture.fixture_hash()?;
    let roles = [
        (
            "base A run record",
            "base A observations",
            &manifest.base_a,
            contrast.base_a,
        ),
        (
            "base B run record",
            "base B observations",
            &manifest.base_b,
            contrast.base_b,
        ),
        (
            "candidate run record",
            "candidate observations",
            &manifest.candidate,
            contrast.candidate,
        ),
    ];
    for (record, observations, retained, observed) in roles {
        let run = observed.run();
        if run.probe != probe_hash || run.env.fixture_hash != fixture_hash {
            return Err(AdmitError::WrongSpec);
        }
        if retained.run != *run {
            return Err(crossed(record));
        }
        let stored: Vec<ObservationHash> = observed.observations().map(|(hash, _)| *hash).collect();
        if retained.observations != stored {
            return Err(crossed(observations));
        }
    }
    let context = &manifest.context;
    if manifest.base_a.run.commit != context.protected_head
        || manifest.base_b.run.commit != context.protected_head
        || manifest.candidate.run.commit != context.merged_commit
    {
        return Err(crossed("context"));
    }
    let masks = compare::derive_masks(&contrast)?;
    if compare::compare(&contrast, &masks)? != *comparison {
        return Err(crossed("comparison"));
    }
    let hash = manifest.contrast_example_hash()?;
    Ok(RetainedExample::admitted(hash, contrast, comparison))
}

fn crossed(what: &'static str) -> AdmitError {
    AdmitError::CrossedEvidence { what }
}

/// Whether a caption slot may be bound to a row that holds no value.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SlotPresence {
    /// The row must hold a present value of the slot's type.
    Required,
    /// The row may also be [`crate::Evidence::Missing`], written as [`MISSING_VALUE_TEXT`].
    Optional,
}

/// What the trusted card writes for a bound row that holds no value.
pub const MISSING_VALUE_TEXT: &str = "no value";

/// How an evidence row does not fit a caption slot ([`crate::SlotSpec::check`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SlotMismatch {
    /// The row is in another column: a base and candidate swap.
    #[error("the row is from another column")]
    Column,
    /// The column does not admit the row's provenance: a hypothetical shown as observed.
    #[error("the column does not admit the row's provenance")]
    Provenance,
    /// The row holds no value and the slot requires one.
    #[error("the row holds no value and the slot requires one")]
    Missing,
    /// The row's value is not of the slot's type.
    #[error("the row's value is not of the slot's type")]
    Type,
}

/// Why a proposed tour was rejected. Any one rejects the whole tour.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TourRejection {
    /// The tour was made for another receipt.
    #[error("the tour is for receipt {tour}, not the view's {view}")]
    StaleReceipt {
        /// The tour's receipt.
        tour: ReceiptHash,
        /// The view's receipt.
        view: ReceiptHash,
    },
    /// The catalog is not the one the view names.
    #[error("the view names catalog {view}, not {catalog}")]
    CatalogMismatch {
        /// The view's catalog version.
        view: CatalogVersion,
        /// The catalog's.
        catalog: CatalogVersion,
    },
    /// One step is invalid.
    #[error("tour step {tour_step}: {fault}")]
    Step {
        /// The step's index, from 0.
        tour_step: usize,
        /// What is wrong with it.
        fault: StepFault,
    },
    /// A mandatory item has no step.
    #[error("mandatory item {item_id} is not reached")]
    Unreached {
        /// The item.
        item_id: ItemId,
    },
}

/// What is wrong with one tour step.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StepFault {
    /// The step's item is not in the view.
    #[error("item {item_id} is not in the view")]
    UnknownItem {
        /// The item.
        item_id: ItemId,
    },
    /// The camera's machine is not in the view.
    #[error("machine {machine_id} is not in the view")]
    UnknownMachine {
        /// The machine.
        machine_id: MachineId,
    },
    /// A staged mark is not one the view draws.
    #[error("the view draws no {mark:?}")]
    UnknownMark {
        /// The mark.
        mark: MarkRef,
    },
    /// A reference names another item than the step's.
    #[error("{reference:?} names another item than the step's")]
    ForeignItem {
        /// The reference.
        reference: EvidenceRef,
    },
    /// A reference names no row of the view.
    #[error("{reference:?} names no evidence row")]
    Unresolved {
        /// The reference.
        reference: EvidenceRef,
    },
    /// The caption's template is not in the catalog.
    #[error("template {template_id} is not in the catalog")]
    UnknownTemplate {
        /// The template.
        template_id: TemplateId,
    },
    /// The template does not describe the item's class and state.
    #[error("template {template_id} does not describe the item's class and state")]
    TemplateDoesNotFit {
        /// The template.
        template_id: TemplateId,
    },
    /// The caption binds other slots than its template declares.
    #[error("the caption does not bind exactly the slots of {template_id}")]
    SlotsDiffer {
        /// The template.
        template_id: TemplateId,
    },
    /// A bound row does not fit its slot.
    #[error("slot {slot}: {mismatch}")]
    Slot {
        /// The slot.
        slot: SlotName,
        /// How the row does not fit.
        mismatch: SlotMismatch,
    },
}

/// Admits a proposed `tour` over `view` and binds every caption against `catalog`, by the rule
/// in the [`crate::PlantView`] module docs. Returns one bound view per tour step, in step order.
///
/// # Errors
///
/// Returns the first [`TourRejection`]; any one rejects the whole tour.
pub fn tour(
    view: &PlantView,
    catalog: &CaptionCatalog,
    tour: &Tour,
) -> Result<Vec<BoundItemView>, TourRejection> {
    if tour.receipt_hash != view.receipt_hash {
        return Err(TourRejection::StaleReceipt {
            tour: tour.receipt_hash,
            view: view.receipt_hash,
        });
    }
    if catalog.catalog_version != view.catalog_version {
        return Err(TourRejection::CatalogMismatch {
            view: view.catalog_version.clone(),
            catalog: catalog.catalog_version.clone(),
        });
    }
    let bound = tour
        .steps
        .iter()
        .enumerate()
        .map(|(tour_step, step)| {
            bind_step(view, catalog, step).map_err(|fault| TourRejection::Step { tour_step, fault })
        })
        .collect::<Result<Vec<_>, _>>()?;
    match view
        .mandatory_items()
        .find(|item| !tour.reaches(&item.item_id))
    {
        Some(item) => Err(TourRejection::Unreached {
            item_id: item.item_id.clone(),
        }),
        None => Ok(bound),
    }
}

fn item<'v>(view: &'v PlantView, item_id: &ItemId) -> Option<&'v PlantItem> {
    view.items.iter().find(|item| &item.item_id == item_id)
}

/// Whether `mark` is one `view` draws.
fn draws(view: &PlantView, mark: &MarkRef) -> bool {
    let item = |item_id: &ItemId| item(view, item_id);
    match mark {
        MarkRef::Footprint { item_id } => view.scope.items.iter().any(|i| &i.item_id == item_id),
        MarkRef::MustKeepPin { must_keep_id } => view
            .scope
            .must_keep
            .iter()
            .any(|rule| &rule.id == must_keep_id),
        MarkRef::QuestionPin { choice_id } => {
            view.scope.open.iter().any(|choice| &choice.id == choice_id)
        }
        MarkRef::NoProbes { machine_id } => view.machines.iter().any(|machine| {
            &machine.machine_id == machine_id && machine.coverage == Coverage::Uncovered
        }),
        MarkRef::Ghost { item_id } | MarkRef::Crates { item_id } => item(item_id).is_some(),
        MarkRef::BouncedChip { item_id } => {
            item(item_id).is_some_and(|item| item.state == ItemState::Bounced)
        }
        MarkRef::ResidueCard { item_id } => {
            item(item_id).is_some_and(|item| item.state == ItemState::Residue)
        }
        MarkRef::Unknown { item_id } => item(item_id).is_some_and(|item| {
            matches!(
                item.class,
                ItemClass::UnstableBase | ItemClass::Incomparable | ItemClass::Error
            )
        }),
        MarkRef::RulingTag { ruling_id } => view
            .machines
            .iter()
            .flat_map(|machine| &machine.rulings)
            .any(|tag| &tag.ruling_id == ruling_id),
        MarkRef::OrderPlate {} => true,
    }
}

fn bind_step(
    view: &PlantView,
    catalog: &CaptionCatalog,
    step: &TourStep,
) -> Result<BoundItemView, StepFault> {
    let item = item(view, &step.item_id).ok_or_else(|| StepFault::UnknownItem {
        item_id: step.item_id.clone(),
    })?;
    let machine_id = &step.camera.machine_id;
    if !view
        .machines
        .iter()
        .any(|machine| &machine.machine_id == machine_id)
    {
        return Err(StepFault::UnknownMachine {
            machine_id: machine_id.clone(),
        });
    }
    if let Some(mark) = step.stage.iter().find(|mark| !draws(view, mark)) {
        return Err(StepFault::UnknownMark { mark: mark.clone() });
    }
    let own_row = |reference: &EvidenceRef| {
        if reference.item_id != item.item_id {
            return Err(StepFault::ForeignItem {
                reference: reference.clone(),
            });
        }
        view.resolve(reference)
            .ok_or_else(|| StepFault::Unresolved {
                reference: reference.clone(),
            })
    };
    for reference in &step.evidence_refs {
        own_row(reference)?;
    }
    let template_id = &step.caption.template_id;
    let template = catalog
        .template(template_id)
        .ok_or_else(|| StepFault::UnknownTemplate {
            template_id: template_id.clone(),
        })?;
    if !template.fits(item) {
        return Err(StepFault::TemplateDoesNotFit {
            template_id: template_id.clone(),
        });
    }
    if !step.caption.slots.keys().eq(template.slots.keys()) {
        return Err(StepFault::SlotsDiffer {
            template_id: template_id.clone(),
        });
    }
    let typed_slot_bindings = template
        .slots
        .iter()
        .zip(step.caption.slots.values())
        .map(|((slot, spec), reference)| {
            let row = own_row(reference)?;
            spec.check(row).map_err(|mismatch| StepFault::Slot {
                slot: slot.clone(),
                mismatch,
            })?;
            let value = BindableValue {
                value_type: spec.value_type,
                unit: spec.unit,
                receipt_hash: view.receipt_hash,
                item_id: item.item_id.clone(),
                row: row.clone(),
            };
            Ok((slot.clone(), value))
        })
        .collect::<Result<_, StepFault>>()?;
    Ok(BoundItemView {
        receipt_hash: view.receipt_hash,
        item_id: item.item_id.clone(),
        typed_slot_bindings,
        catalog_version: view.catalog_version.clone(),
    })
}
