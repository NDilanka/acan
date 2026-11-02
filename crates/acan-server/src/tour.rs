//! The tour director: an ordered walk over one immutable receipt snapshot.
//!
//! The director model proposes a tour. Trusted code admits it only when every step names a mark
//! that exists in the view, every mandatory item is reachable and the tour is bound to the
//! view's receipt hash. A refused or missing tour falls back to the deterministic order, and
//! the plain card works with no tour at all. A tour never changes a class, a count or a state.
//!
//! Admission is [`acan_intent::admit::tour`]. This module builds the director's context, the
//! deterministic order and its captions, and every tour it returns has passed that check.

use std::collections::BTreeMap;

use acan_intent::admit::TourRejection;
use acan_intent::provenance::ModelProposal;
use acan_intent::{
    Camera, Caption, CaptionCatalog, CaptionTemplate, EvidenceRef, InvalidValue, ItemClass, ItemId,
    ItemState, MarkRef, ModelRef, NonEmpty, PlantItem, PlantView, SlotName, Tour, TourStep,
    TourView, ZoomLevel,
};
use acan_llm::{ChatRequest, LlmError, Message, ModelClient, Role};
use serde_json::{Value, json};

/// The trusted caption catalog. Every plant view names its version, and admission checks it.
const CAPTION_CATALOG: &str = include_str!("../../acan-intent/fixtures/caption-catalog.json");

/// The most output one director reply may use.
const DIRECTOR_MAX_TOKENS: u32 = 2_000;

/// The director orders the walk and does not vary its answer.
const DIRECTOR_TEMPERATURE: f32 = 0.0;

/// What the director is asked. Its context follows as JSON: the view, the mandatory item ids and
/// the caption catalog.
const DIRECTOR_PROMPT: &str = "You order a tour of one receipt's plant view for the developer who rules on it. Reply with JSON that matches the Tour schema and nothing else. Copy receipt_hash from the view. Make one step for every item named in mandatory, and at most one step per item. A step's camera names a machine of the view, its stage lists marks the view draws, and its evidence_refs name evidence rows of its own item. Its caption names a template of caption_catalog and binds exactly that template's slots to evidence_refs of the same item. You never write a value, a class, a state, a count, a denominator, a verdict or caption text: the view supplies them, and a caption is only a template id and its slot bindings.";

/// Why no tour could be made or admitted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum TourError {
    /// The view has no item a tour could visit.
    #[error("the view has no item for a tour to visit")]
    NothingToVisit,
    /// A mandatory item has no caption its evidence can fill.
    #[error("item {item_id} has no caption its evidence fills: {detail}")]
    Unbindable {
        /// The item.
        item_id: ItemId,
        /// What is missing.
        detail: String,
    },
    /// The director call failed, or its reply is not a tour.
    #[error("the director call failed: {0}")]
    Director(#[from] LlmError),
    /// The director's model is not a model id.
    #[error("the director's model is not a model id: {0}")]
    Model(#[from] InvalidValue),
    /// The tour is refused by admission.
    #[error("the tour is refused: {0}")]
    Refused(#[from] TourRejection),
    /// The context or the catalog is not JSON.
    #[error("the tour's JSON does not parse or serialize: {0}")]
    Json(#[from] serde_json::Error),
}

/// Asks the director model for a tour of `view`. The call goes to the director's route, which
/// falls back to the foreman's. A view with no item is not sent.
///
/// # Errors
///
/// Returns [`TourError::NothingToVisit`] for a view with no item, [`TourError::Director`] when the
/// call fails or its reply does not parse as a tour, and [`TourError::Model`] when the reply
/// names a model that is not a model id.
pub(crate) async fn direct<C: ModelClient>(
    client: &C,
    view: &PlantView,
) -> Result<ModelProposal<Tour>, TourError> {
    if view.items.is_empty() {
        return Err(TourError::NothingToVisit);
    }
    let catalog = caption_catalog()?;
    let request = ChatRequest {
        messages: vec![
            Message::system(DIRECTOR_PROMPT),
            Message::user(context(view, &catalog)?),
        ],
        temperature: Some(DIRECTOR_TEMPERATURE),
        max_tokens: Some(DIRECTOR_MAX_TOKENS),
        ..ChatRequest::default()
    };
    let reply = client.chat_json::<Tour>(Role::Director, request).await?;
    let model: ModelRef = reply.call.model.parse()?;
    Ok(ModelProposal::new(reply.value, model))
}

/// Validates a proposed tour against `view` and admits it as a directed tour. Any one fault
/// rejects the whole tour.
///
/// # Errors
///
/// Returns [`TourError::Refused`] with the first rule the tour breaks.
pub(crate) fn admit(
    view: &PlantView,
    proposal: ModelProposal<Tour>,
) -> Result<TourView, TourError> {
    let catalog = caption_catalog()?;
    let model = proposal.model().clone();
    let tour = proposal.proposal().clone();
    acan_intent::admit::tour(view, &catalog, &tour)?;
    Ok(TourView::Directed { model, tour })
}

/// The deterministic tour of `view`: residue, then violations, then bounced, then the other
/// mandatory items, then fulfilled. Every mandatory item is visited. An optional item whose
/// caption cannot be bound is left out.
///
/// # Errors
///
/// Returns [`TourError::NothingToVisit`] when no item has a place in the tour, and
/// [`TourError::Unbindable`] or [`TourError::Refused`] when a mandatory item has no admissible
/// step.
pub(crate) fn deterministic(view: &PlantView) -> Result<TourView, TourError> {
    let catalog = caption_catalog()?;
    let tour = Tour {
        receipt_hash: view.receipt_hash,
        steps: steps(view, &catalog)?,
    };
    acan_intent::admit::tour(view, &catalog, &tour)?;
    Ok(TourView::Deterministic { tour })
}

fn caption_catalog() -> Result<CaptionCatalog, TourError> {
    Ok(serde_json::from_str(CAPTION_CATALOG)?)
}

/// The director's context: the view without any tour, the mandatory item ids and the catalog.
fn context(view: &PlantView, catalog: &CaptionCatalog) -> Result<String, TourError> {
    let mut view_json = serde_json::to_value(view)?;
    if let Value::Object(fields) = &mut view_json {
        fields.remove("tour");
    }
    let mandatory: Vec<&str> = view
        .mandatory_items()
        .map(|item| item.item_id.as_str())
        .collect();
    let caption_catalog = serde_json::to_value(catalog)?;
    Ok(json!({
        "view": view_json,
        "mandatory": mandatory,
        "caption_catalog": caption_catalog,
    })
    .to_string())
}

/// The items in deterministic order: residue, then ruling violations, then bounced, then the
/// other mandatory items, then fulfilled. View order holds within each bucket, and an item that
/// fits no bucket is left out.
fn ordered_items(view: &PlantView) -> Vec<&PlantItem> {
    let bucket = |item: &PlantItem| -> Option<u8> {
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
fn steps(view: &PlantView, catalog: &CaptionCatalog) -> Result<NonEmpty<TourStep>, TourError> {
    let mut built = Vec::new();
    for item in ordered_items(view) {
        match step_for(view, catalog, item) {
            Ok(step) => built.push(step),
            Err(err) if item.is_mandatory() => return Err(err),
            Err(_) => {}
        }
    }
    let mut built = built.into_iter();
    let first = built.next().ok_or(TourError::NothingToVisit)?;
    Ok(NonEmpty::new(first, built))
}

/// The step that visits `item`: its camera, marks, evidence and caption.
fn step_for(
    view: &PlantView,
    catalog: &CaptionCatalog,
    item: &PlantItem,
) -> Result<TourStep, TourError> {
    let name = template_for(item)
        .ok_or_else(|| unbindable(item, "no caption fits its class and state"))?;
    let template = catalog
        .templates
        .iter()
        .find(|template| template.template_id.as_str() == name)
        .ok_or_else(|| unbindable(item, format!("the catalog has no {name}")))?;
    let slots = bind_slots(item, template)?;
    let step = TourStep {
        item_id: item.item_id.clone(),
        camera: Camera {
            machine_id: item.machine_id.clone(),
            zoom: zoom(name),
        },
        stage: stage(view, item, name),
        evidence_refs: slots.values().cloned().collect(),
        caption: Caption {
            template_id: template.template_id.clone(),
            slots,
        },
    };
    admitted_alone(view, catalog, step)
}

/// The caption template that says what `item` is, by its class and state. `None` when no
/// template fits.
fn template_for(item: &PlantItem) -> Option<&'static str> {
    let name = if item.state == ItemState::Residue {
        "residue.changed_value"
    } else if item.class == ItemClass::ViolatesRuling {
        "violation.ruling"
    } else if item.class == ItemClass::ObligationFailed {
        "bounced.obligation_failed"
    } else if item.class == ItemClass::UnstableBase {
        "unknown.unstable_base"
    } else if item.class == ItemClass::Incomparable {
        "unknown.incomparable"
    } else if item.class == ItemClass::Error {
        "unknown.error"
    } else if item.class == ItemClass::OutOfScope {
        "stopped.out_of_scope"
    } else if item.class == ItemClass::ObligationFulfilled || item.state == ItemState::Fixed {
        "fulfilled.obligations"
    } else {
        return None;
    };
    Some(name)
}

/// The camera zoom for a step of the template `template`.
fn zoom(template: &str) -> ZoomLevel {
    match template {
        "violation.ruling" | "stopped.out_of_scope" => ZoomLevel::Close,
        "fulfilled.obligations" => ZoomLevel::Fit,
        _ => ZoomLevel::Machine,
    }
}

/// The marks a step stages: the item's own mark for its template, and its ruling tag when the
/// view draws one.
fn stage(view: &PlantView, item: &PlantItem, template: &str) -> Vec<MarkRef> {
    let item_id = item.item_id.clone();
    match template {
        "residue.changed_value" => vec![MarkRef::ResidueCard { item_id }],
        "violation.ruling" => {
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
        "bounced.obligation_failed" if item.state == ItemState::Bounced => vec![
            MarkRef::BouncedChip {
                item_id: item_id.clone(),
            },
            MarkRef::Crates { item_id },
        ],
        "unknown.unstable_base" | "unknown.incomparable" | "unknown.error" => {
            vec![MarkRef::Unknown { item_id }]
        }
        "stopped.out_of_scope" => vec![
            MarkRef::Ghost {
                item_id: item_id.clone(),
            },
            MarkRef::Crates { item_id },
        ],
        _ => vec![MarkRef::Crates { item_id }],
    }
}

/// The reference that fills each slot of `template`: the item's first row the slot accepts. A
/// slot that no row fills leaves the item unbindable.
fn bind_slots(
    item: &PlantItem,
    template: &CaptionTemplate,
) -> Result<BTreeMap<SlotName, EvidenceRef>, TourError> {
    template
        .slots
        .iter()
        .map(
            |(slot, spec)| -> Result<(SlotName, EvidenceRef), TourError> {
                let row = item
                    .evidence
                    .iter()
                    .find(|row| spec.accepts(row))
                    .ok_or_else(|| unbindable(item, format!("no evidence row fills {slot}")))?;
                let reference = EvidenceRef {
                    item_id: item.item_id.clone(),
                    slot: row.slot,
                    step: row.step.clone(),
                    field: row.field.clone(),
                };
                Ok((slot.clone(), reference))
            },
        )
        .collect()
}

/// Returns `step` when admission accepts it as a tour of its own. Admission reports the mandatory
/// items the single step leaves out, and that is no fault of the step, so only a step fault
/// refuses it.
fn admitted_alone(
    view: &PlantView,
    catalog: &CaptionCatalog,
    step: TourStep,
) -> Result<TourStep, TourError> {
    let alone = Tour {
        receipt_hash: view.receipt_hash,
        steps: NonEmpty::new(step.clone(), std::iter::empty()),
    };
    match acan_intent::admit::tour(view, catalog, &alone) {
        Ok(_) | Err(TourRejection::Unreached { .. }) => Ok(step),
        Err(rejection) => Err(TourError::Refused(rejection)),
    }
}

fn unbindable(item: &PlantItem, detail: impl Into<String>) -> TourError {
    TourError::Unbindable {
        item_id: item.item_id.clone(),
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use acan_intent::admit::{SlotMismatch, StepFault};
    use acan_llm::{LlmError, Reply, Stub};

    use super::*;

    const VIEW: &str = include_str!("../../acan-intent/fixtures/plant-view-v1.json");
    const PLAIN: &str = include_str!("../../acan-intent/fixtures/plant-view-v1-plain.json");

    fn fixture(text: &str) -> PlantView {
        serde_json::from_str(text).expect("the fixture view decodes")
    }

    fn deterministic_tour(view: &PlantView) -> Tour {
        match deterministic(view).expect("the view has a deterministic tour") {
            TourView::Deterministic { tour } => tour,
            other => panic!("expected a deterministic tour, got {other:?}"),
        }
    }

    fn step_of<'a>(tour: &'a Tour, item: &str) -> &'a TourStep {
        tour.steps
            .iter()
            .find(|step| step.item_id.as_str() == item)
            .unwrap_or_else(|| panic!("no step about {item}"))
    }

    fn item_ids(tour: &Tour) -> Vec<String> {
        tour.steps
            .iter()
            .map(|step| step.item_id.to_string())
            .collect()
    }

    /// The fixture view's own tour JSON, after `edit` on its steps.
    fn tour_json(edit: impl FnOnce(&mut Vec<Value>)) -> Value {
        let mut value: Value = serde_json::from_str(VIEW).expect("the fixture decodes");
        edit(
            value["tour"]["tour"]["steps"]
                .as_array_mut()
                .expect("tour steps"),
        );
        value["tour"]["tour"].clone()
    }

    /// The fixture view's own tour after `edit` on its steps, decoded as a tour.
    fn edited(edit: impl FnOnce(&mut Vec<Value>)) -> Tour {
        serde_json::from_value(tour_json(edit)).expect("the edited tour decodes")
    }

    fn index(steps: &[Value], item: &str) -> usize {
        steps
            .iter()
            .position(|step| step["item_id"] == item)
            .unwrap_or_else(|| panic!("no step about {item}"))
    }

    fn reference(item: &str, slot: &str, step: &str, field: &str) -> Value {
        json!({ "item_id": item, "slot": slot, "step": step, "field": field })
    }

    fn fixture_model() -> ModelRef {
        "muse-spark".parse().expect("a model id")
    }

    /// The rejection admission gives `tour` over `view`. Panics when the tour is admitted.
    fn refusal(view: &PlantView, tour: Tour) -> TourRejection {
        match admit(view, ModelProposal::new(tour, fixture_model())) {
            Err(TourError::Refused(rejection)) => rejection,
            other => panic!("the tour must be refused, got {other:?}"),
        }
    }

    #[test]
    fn the_deterministic_tour_is_residue_then_violations_then_bounced_then_the_rest() {
        let view = fixture(VIEW);
        let tour = deterministic_tour(&view);

        assert_eq!(
            item_ids(&tour),
            [
                "d-login-error-body",
                "d-links-sort",
                "i-retry-after",
                "d-links-clicks",
                "chk-dashboard",
                "chk-links-delete",
                "chk-signup-store",
                "i-login-limit",
            ],
            "residue, violations, bounced, the other mandatory items in view order, then fulfilled"
        );
        assert_eq!(tour.receipt_hash, view.receipt_hash);
    }

    #[test]
    fn every_mandatory_item_is_reached_by_the_deterministic_tour() {
        let view = fixture(VIEW);
        let tour = deterministic_tour(&view);

        let mandatory: Vec<&ItemId> = view.mandatory_items().map(|item| &item.item_id).collect();
        assert_eq!(mandatory.len(), 7, "the fixture has seven mandatory items");
        for item_id in mandatory {
            assert!(
                tour.reaches(item_id),
                "{item_id} is mandatory and has no step"
            );
        }
    }

    #[test]
    fn each_class_of_the_fixture_gets_its_own_caption() {
        let view = fixture(VIEW);
        let tour = deterministic_tour(&view);

        for (item, template) in [
            ("d-login-error-body", "residue.changed_value"),
            ("d-links-sort", "violation.ruling"),
            ("i-retry-after", "bounced.obligation_failed"),
            ("chk-dashboard", "unknown.unstable_base"),
            ("d-links-clicks", "stopped.out_of_scope"),
            ("chk-links-delete", "unknown.incomparable"),
            ("chk-signup-store", "unknown.error"),
            ("i-login-limit", "fulfilled.obligations"),
        ] {
            assert_eq!(
                step_of(&tour, item).caption.template_id.as_str(),
                template,
                "the caption of {item}"
            );
        }
        let preserved: ItemId = "chk-health".parse().expect("an item id");
        assert!(
            !tour.reaches(&preserved),
            "a preserved item is optional and not fulfilled, so the tour leaves it out"
        );
    }

    #[test]
    fn a_caption_binds_base_to_the_base_run_and_candidate_to_the_candidate_run() {
        let view = fixture(VIEW);
        let tour = deterministic_tour(&view);
        let residue = step_of(&tour, "d-login-error-body");

        let slot_of = |name: &str| {
            let (_, reference) = residue
                .caption
                .slots
                .iter()
                .find(|(slot, _)| slot.as_str() == name)
                .unwrap_or_else(|| panic!("the residue caption binds {name}"));
            serde_json::to_value(reference.slot).expect("a slot serializes")
        };
        assert_eq!(slot_of("base"), json!("base_a"));
        assert_eq!(slot_of("candidate"), json!("candidate"));
    }

    #[test]
    fn the_plain_fixture_gets_the_same_deterministic_tour() {
        let plain = fixture(PLAIN);
        assert!(plain.tour.is_none(), "the plain fixture carries no tour");

        assert_eq!(
            deterministic_tour(&plain),
            deterministic_tour(&fixture(VIEW))
        );
    }

    #[test]
    fn the_directed_fixture_is_admitted_as_directed() {
        let view = fixture(VIEW);
        let Some(TourView::Directed { model, tour }) = view.tour.clone() else {
            panic!("the fixture carries a directed tour");
        };

        let admitted = admit(&view, ModelProposal::new(tour.clone(), model.clone()))
            .expect("the fixture's tour is admitted");

        assert_eq!(admitted, TourView::Directed { model, tour });
    }

    #[test]
    fn an_optional_item_that_cannot_be_bound_is_left_out() {
        let mut value: Value = serde_json::from_str(VIEW).expect("the fixture decodes");
        for item in value["items"].as_array_mut().expect("items") {
            if item["item_id"] == "i-login-limit" {
                item["evidence"]
                    .as_array_mut()
                    .expect("evidence")
                    .retain(|row| row["slot"] != "candidate");
            }
        }
        let view = fixture(&value.to_string());

        let tour = deterministic_tour(&view);

        assert!(
            !tour.reaches(&"i-login-limit".parse().expect("an item id")),
            "a fulfilled item with no candidate row is optional and left out"
        );
        assert!(tour.reaches(&"d-login-error-body".parse().expect("an item id")));
    }

    #[test]
    fn a_mandatory_item_that_cannot_be_bound_fails_the_deterministic_tour() {
        let mut value: Value = serde_json::from_str(VIEW).expect("the fixture decodes");
        for item in value["items"].as_array_mut().expect("items") {
            if item["item_id"] == "d-login-error-body" {
                item["evidence"]
                    .as_array_mut()
                    .expect("evidence")
                    .retain(|row| row["slot"] != "base_a");
            }
        }
        let view = fixture(&value.to_string());

        let err = deterministic(&view).expect_err("a residue item with no base row is unbindable");

        assert!(
            matches!(&err, TourError::Unbindable { item_id, .. } if item_id.as_str() == "d-login-error-body"),
            "{err}"
        );
    }

    #[test]
    fn a_view_with_nothing_to_visit_has_no_tour() {
        let mut view = fixture(VIEW);
        view.items.clear();
        assert!(matches!(
            deterministic(&view),
            Err(TourError::NothingToVisit)
        ));

        let mut plain = fixture(VIEW);
        plain
            .items
            .retain(|item| item.class == ItemClass::Preserved);
        assert!(matches!(
            deterministic(&plain),
            Err(TourError::NothingToVisit)
        ));
    }

    #[test]
    fn a_missing_mandatory_item_rejects_the_whole_tour() {
        let view = fixture(VIEW);
        let tour = edited(|steps| {
            let at = index(steps, "d-links-clicks");
            steps.remove(at);
        });

        assert_eq!(
            refusal(&view, tour),
            TourRejection::Unreached {
                item_id: "d-links-clicks".parse().expect("an item id"),
            }
        );
    }

    #[test]
    fn an_unknown_reference_rejects_the_whole_tour() {
        let view = fixture(VIEW);
        let mut at = 0;
        let tour = edited(|steps| {
            at = index(steps, "d-login-error-body");
            steps[at]["evidence_refs"][0]["step"] = json!("s9");
        });

        let rejection = refusal(&view, tour);

        assert!(
            matches!(
                rejection,
                TourRejection::Step { tour_step, fault: StepFault::Unresolved { .. } }
                    if tour_step == at
            ),
            "{rejection:?}"
        );
    }

    #[test]
    fn a_literal_caption_is_never_admitted() {
        let view = fixture(VIEW);
        let mut at = 0;
        let with_text = tour_json(|steps| {
            at = index(steps, "d-login-error-body");
            steps[at]["caption"]["text"] = json!("Base answered 0; the candidate answers 7.");
        });
        assert!(
            serde_json::from_value::<Tour>(with_text).is_err(),
            "a caption carries template and slots, never text"
        );

        let unknown = edited(|steps| {
            steps[at]["caption"]["template_id"] = json!("literal.text");
        });
        let rejection = refusal(&view, unknown);
        assert!(
            matches!(
                rejection,
                TourRejection::Step { tour_step, fault: StepFault::UnknownTemplate { .. } }
                    if tour_step == at
            ),
            "{rejection:?}"
        );
    }

    #[test]
    fn a_swapped_base_and_candidate_rejects_the_whole_tour() {
        let view = fixture(VIEW);
        let mut at = 0;
        let tour = edited(|steps| {
            at = index(steps, "d-login-error-body");
            steps[at]["caption"]["slots"]["base"] =
                reference("d-login-error-body", "candidate", "s6", "body.error");
        });

        let rejection = refusal(&view, tour);

        assert!(
            matches!(
                &rejection,
                TourRejection::Step {
                    tour_step,
                    fault: StepFault::Slot { slot, mismatch: SlotMismatch::Column },
                } if *tour_step == at && slot.as_str() == "base"
            ),
            "{rejection:?}"
        );
    }

    #[test]
    fn a_tour_for_another_receipt_is_refused() {
        let view = fixture(VIEW);
        let mut json = tour_json(|_| {});
        json["receipt_hash"] = json!(format!("sha256:{}", "0".repeat(64)));
        let tour: Tour = serde_json::from_value(json).expect("the tour decodes");

        assert!(matches!(
            refusal(&view, tour),
            TourRejection::StaleReceipt { .. }
        ));
    }

    #[tokio::test]
    async fn the_director_is_asked_for_structured_output_and_its_tour_is_admitted() {
        let view = fixture(VIEW);
        let Some(TourView::Directed { tour, .. }) = view.tour.clone() else {
            panic!("the fixture carries a directed tour");
        };
        let stub = Stub::default().when(
            Role::Director,
            |request| {
                request.temperature == Some(0.0)
                    && request.max_tokens == Some(2_000)
                    && request
                        .response_format
                        .as_ref()
                        .is_some_and(|format| format.name == "Tour")
            },
            Reply::json(&tour),
        );

        let proposal = direct(&stub, &view)
            .await
            .expect("the stub's tour is proposed");
        let admitted = admit(&view, proposal).expect("the proposed tour is admitted");

        let TourView::Directed {
            model,
            tour: admitted_tour,
        } = admitted
        else {
            panic!("a proposed tour is admitted as directed");
        };
        assert_eq!(admitted_tour, tour);
        assert_eq!(model.as_str(), "stub", "the model is the one that answered");
        let calls = stub.ledger().records();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].role, Role::Director);
    }

    #[tokio::test]
    async fn a_reply_that_is_not_a_tour_is_an_error_before_admission() {
        let view = fixture(VIEW);
        let stub = Stub::default().when(Role::Director, |_| true, Reply::text("not json"));

        let err = direct(&stub, &view)
            .await
            .expect_err("a reply that is not a tour is refused");

        assert!(
            matches!(err, TourError::Director(LlmError::SchemaValidation { .. })),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_unscripted_director_is_an_error() {
        let view = fixture(VIEW);

        let err = direct(&Stub::default(), &view)
            .await
            .expect_err("no reply is scripted for the director");

        assert!(
            matches!(err, TourError::Director(LlmError::NoScriptedReply { .. })),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_view_with_no_item_is_not_sent_to_the_director() {
        let mut view = fixture(VIEW);
        view.items.clear();
        let stub = Stub::default();

        let err = direct(&stub, &view)
            .await
            .expect_err("an empty view has no tour to order");

        assert!(matches!(err, TourError::NothingToVisit), "{err}");
        assert!(stub.ledger().records().is_empty(), "no call was made");
    }
}
