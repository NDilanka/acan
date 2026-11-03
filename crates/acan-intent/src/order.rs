//! The deterministic tour: one fixed order over one plant view.
//!
//! The server and the page both build this tour with [`deterministic`], so they show the same
//! walk. The order is residue items, then ruling violations, then bounced items, then the other
//! mandatory items, then fulfilled items, then a few open preserved checks, and each bucket keeps
//! the view's order. Every mandatory item is visited. An optional item whose caption cannot be
//! bound is left out. A step's caption is the first catalog template that describes the item and
//! binds its evidence, and the whole tour then passes [`admit::tour`].

use std::collections::BTreeMap;

use crate::admit::{self, TourRejection};
use crate::report::EvidenceRow;
use crate::types::{ItemId, NonEmpty, SlotName};
use crate::verdict::{ItemClass, ItemState};
use crate::view::{
    Camera, Caption, CaptionCatalog, CaptionTemplate, EvidenceRef, MarkRef, PlantItem, PlantView,
    SlotSpec, Tour, TourStep, TourView, ZoomLevel,
};

/// Why the deterministic tour could not be built.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DeterministicError {
    /// The view has no item a tour could visit.
    #[error("the view has no item for a tour to visit")]
    NothingToVisit,
    /// A mandatory item has no caption its evidence fills.
    #[error("item {item_id} has no caption its evidence fills: {detail}")]
    Unbindable {
        /// The item.
        item_id: ItemId,
        /// What is missing.
        detail: String,
    },
    /// The built tour is refused by admission.
    #[error("the deterministic tour is refused: {0}")]
    Refused(#[from] TourRejection),
    /// The trusted caption catalog does not parse.
    #[error("the trusted caption catalog does not parse: {0}")]
    Catalog(#[from] serde_json::Error),
}

/// The deterministic tour of `view`, over the trusted caption catalog, with the bindings
/// admission made for its steps. Every step is admitted before the tour is returned.
///
/// # Errors
///
/// Returns [`DeterministicError::NothingToVisit`] when no item has a place in the tour, and
/// [`DeterministicError::Unbindable`] or [`DeterministicError::Refused`] when a mandatory item
/// has no admissible step.
pub fn deterministic(view: &PlantView) -> Result<TourView, DeterministicError> {
    let catalog = CaptionCatalog::trusted()?;
    let tour = Tour {
        receipt_hash: view.receipt_hash,
        steps: steps(view, &catalog)?,
    };
    let bindings = admit::tour(view, &catalog, &tour)?;
    Ok(TourView::Deterministic { tour, bindings })
}

/// How many open preserved checks the deterministic tour visits, after the other items. A check
/// that passed is no fault, so the walk shows a few of them and keeps the rest out.
const PRESERVED_STEPS: usize = 3;

/// The items in deterministic order: residue, then ruling violations, then bounced, then the
/// other mandatory items, then fulfilled, then up to [`PRESERVED_STEPS`] open preserved checks.
/// View order holds within each bucket, and an item that fits no bucket is left out.
fn ordered_items(view: &PlantView) -> Vec<&PlantItem> {
    let mut preserved = 0;
    let mut bucket = |item: &PlantItem| -> Option<u8> {
        if item.state == ItemState::Residue {
            Some(0)
        } else if item.class == ItemClass::ViolatesRuling {
            Some(1)
        } else if item.state == ItemState::Bounced {
            Some(2)
        } else if item.is_mandatory() {
            Some(3)
        } else if item.state == ItemState::Fixed || item.class == ItemClass::ObligationFulfilled {
            Some(4)
        } else if item.class == ItemClass::Preserved && preserved < PRESERVED_STEPS {
            preserved += 1;
            Some(5)
        } else {
            None
        }
    };
    let mut ranked: Vec<(u8, &PlantItem)> = view
        .items
        .iter()
        .filter_map(|item| bucket(item).map(|rank| (rank, item)))
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, item)| item).collect()
}

/// The steps of the deterministic tour. A mandatory item without an admissible step fails the
/// tour. An optional item without one is left out.
fn steps(
    view: &PlantView,
    catalog: &CaptionCatalog,
) -> Result<NonEmpty<TourStep>, DeterministicError> {
    let mut built = Vec::new();
    for item in ordered_items(view) {
        match step_for(view, catalog, item) {
            Ok(step) => built.push(step),
            Err(err) if item.is_mandatory() => return Err(err),
            Err(_) => {}
        }
    }
    let mut built = built.into_iter();
    let first = built.next().ok_or(DeterministicError::NothingToVisit)?;
    Ok(NonEmpty::new(first, built))
}

/// The step that visits `item`, captioned by the first catalog template that describes it and
/// binds its evidence.
fn step_for(
    view: &PlantView,
    catalog: &CaptionCatalog,
    item: &PlantItem,
) -> Result<TourStep, DeterministicError> {
    let mut failure = None;
    for template in catalog
        .templates
        .iter()
        .filter(|template| template.fits(item))
    {
        match bind_slots(item, template) {
            Ok(slots) => {
                let step = TourStep {
                    item_id: item.item_id.clone(),
                    camera: Camera {
                        machine_id: item.machine_id.clone(),
                        zoom: zoom(item),
                    },
                    stage: stage(view, item),
                    evidence_refs: slots.values().cloned().collect(),
                    caption: Caption {
                        template_id: template.template_id.clone(),
                        slots,
                    },
                };
                return admitted_alone(view, catalog, step);
            }
            Err(err) => failure = Some(err),
        }
    }
    Err(failure
        .unwrap_or_else(|| unbindable(item, "no caption template describes its class and state")))
}

/// The camera zoom for the step about `item`.
fn zoom(item: &PlantItem) -> ZoomLevel {
    if item.class == ItemClass::ViolatesRuling || is_stopped_out_of_scope(item) {
        ZoomLevel::Close
    } else if item.class == ItemClass::ObligationFulfilled {
        ZoomLevel::Fit
    } else {
        ZoomLevel::Machine
    }
}

fn is_stopped_out_of_scope(item: &PlantItem) -> bool {
    item.class == ItemClass::OutOfScope && item.state == ItemState::Stopped
}

/// The marks the step about `item` stages. Each is one the view draws for the item, so
/// admission accepts it.
fn stage(view: &PlantView, item: &PlantItem) -> Vec<MarkRef> {
    let item_id = item.item_id.clone();
    match item.class {
        ItemClass::ViolatesRuling => {
            let mut marks: Vec<MarkRef> = item
                .ruling_id
                .iter()
                .filter(|ruling_id| {
                    view.machines
                        .iter()
                        .flat_map(|machine| &machine.rulings)
                        .any(|tag| &tag.ruling_id == *ruling_id)
                })
                .map(|ruling_id| MarkRef::RulingTag {
                    ruling_id: ruling_id.clone(),
                })
                .collect();
            marks.push(MarkRef::Ghost { item_id });
            marks
        }
        ItemClass::UnstableBase | ItemClass::Incomparable | ItemClass::Error => {
            vec![MarkRef::Unknown { item_id }]
        }
        _ if is_stopped_out_of_scope(item) => vec![
            MarkRef::Ghost {
                item_id: item_id.clone(),
            },
            MarkRef::Crates { item_id },
        ],
        _ => {
            let mut marks = Vec::new();
            match item.state {
                ItemState::Residue => marks.push(MarkRef::ResidueCard {
                    item_id: item_id.clone(),
                }),
                ItemState::Bounced => marks.push(MarkRef::BouncedChip {
                    item_id: item_id.clone(),
                }),
                _ => {}
            }
            marks.push(MarkRef::Crates { item_id });
            marks
        }
    }
}

/// The reference that fills each slot of `template`. The comparative slots (base and candidate)
/// take the rows of one step and field, the first address that holds a row every one of them
/// accepts, so the caption compares one difference. Any other slot takes the item's first row it
/// accepts. A slot that no row fills leaves the item unbindable.
fn bind_slots(
    item: &PlantItem,
    template: &CaptionTemplate,
) -> Result<BTreeMap<SlotName, EvidenceRef>, DeterministicError> {
    let mut bound = BTreeMap::new();
    let comparative: Vec<(&SlotName, &SlotSpec)> = template
        .slots
        .iter()
        .filter(|(_, spec)| spec.column.compares())
        .collect();
    if !comparative.is_empty() {
        let joint = item
            .evidence
            .iter()
            .map(|row| (&row.step, &row.field))
            .find_map(|address| {
                comparative
                    .iter()
                    .map(|(slot, spec)| {
                        item.evidence
                            .iter()
                            .find(|row| (&row.step, &row.field) == address && spec.accepts(row))
                            .map(|row| ((*slot).clone(), row))
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| {
                unbindable(
                    item,
                    "no step and field holds a row for every comparative slot",
                )
            })?;
        for (slot, row) in joint {
            bound.insert(slot, reference(item, row));
        }
    }
    let unbound: Vec<(&SlotName, &SlotSpec)> = template
        .slots
        .iter()
        .filter(|(slot, _)| !bound.contains_key(*slot))
        .collect();
    for (slot, spec) in unbound {
        let row = item
            .evidence
            .iter()
            .find(|row| spec.accepts(row))
            .ok_or_else(|| unbindable(item, format!("no evidence row fills {slot}")))?;
        bound.insert(slot.clone(), reference(item, row));
    }
    Ok(bound)
}

/// The reference to `row` of `item`, the address a caption slot binds.
fn reference(item: &PlantItem, row: &EvidenceRow) -> EvidenceRef {
    EvidenceRef {
        item_id: item.item_id.clone(),
        slot: row.slot,
        step: row.step.clone(),
        field: row.field.clone(),
    }
}

/// Returns `step` when admission accepts it as a tour of its own. Admission reports the mandatory
/// items the single step leaves out, and that is no fault of the step, so only a step fault
/// refuses it.
fn admitted_alone(
    view: &PlantView,
    catalog: &CaptionCatalog,
    step: TourStep,
) -> Result<TourStep, DeterministicError> {
    let alone = Tour {
        receipt_hash: view.receipt_hash,
        steps: NonEmpty::new(step.clone(), std::iter::empty()),
    };
    match admit::tour(view, catalog, &alone) {
        Ok(_) | Err(TourRejection::Unreached { .. }) => Ok(step),
        Err(rejection) => Err(DeterministicError::Refused(rejection)),
    }
}

fn unbindable(item: &PlantItem, detail: impl Into<String>) -> DeterministicError {
    DeterministicError::Unbindable {
        item_id: item.item_id.clone(),
        detail: detail.into(),
    }
}
