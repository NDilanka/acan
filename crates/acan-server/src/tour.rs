//! The tour director: an ordered walk over one immutable receipt snapshot.
//!
//! The director model proposes a tour. Trusted code admits it only when every step names a mark
//! that exists in the view, every mandatory item is reachable and the tour is bound to the
//! view's receipt hash. A refused or missing tour falls back to the deterministic order, which
//! [`acan_intent::deterministic`] builds for the server and the page alike, and the plain card
//! works with no tour at all. A tour never changes a class, a count or a state.
//!
//! Admission is [`acan_intent::admit::tour`]. This module builds the director's context, and
//! every tour it returns has passed that check.

use acan_intent::admit::TourRejection;
use acan_intent::provenance::ModelProposal;
use acan_intent::{
    CaptionCatalog, DeterministicError, InvalidValue, ModelRef, PlantView, Tour, TourView,
};
use acan_llm::{ChatRequest, LlmError, Message, ModelClient, Role};
use serde_json::{Value, json};

/// The most output one director reply may use.
const DIRECTOR_MAX_TOKENS: u32 = 2_000;

/// The director orders the walk and does not vary its answer.
const DIRECTOR_TEMPERATURE: f32 = 0.0;

/// What the director is asked. Its context follows as JSON: the view, the mandatory item ids and
/// the caption catalog.
const DIRECTOR_PROMPT: &str = "You order a tour of one receipt's plant view for the developer who rules on it. Reply with JSON that matches the Tour schema and nothing else. Copy receipt_hash from the view. Make one step for every item named in mandatory, and at most one step per item. A step's camera names a machine of the view, its stage lists marks the view draws, and its evidence_refs name evidence rows of its own item. Its caption names a template of caption_catalog whose classes and states include the item's class and state, and binds exactly that template's slots to evidence_refs of the same item. You never write a value, a class, a state, a count, a denominator, a verdict or caption text: the view supplies them, and a caption is only a template id and its slot bindings.";

/// Why no tour could be made or admitted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum TourError {
    /// The view has no item a tour could visit.
    #[error("the view has no item for a tour to visit")]
    NothingToVisit,
    /// The director call failed, or its reply is not a tour.
    #[error("the director call failed: {0}")]
    Director(#[from] LlmError),
    /// The director's model is not a model id.
    #[error("the director's model is not a model id: {0}")]
    Model(#[from] InvalidValue),
    /// The tour is refused by admission.
    #[error("the tour is refused: {0}")]
    Refused(#[from] TourRejection),
    /// The deterministic tour cannot be built.
    #[error("the deterministic tour cannot be built: {0}")]
    Deterministic(#[from] DeterministicError),
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
    let catalog = CaptionCatalog::trusted()?;
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
    let catalog = CaptionCatalog::trusted()?;
    let model = proposal.model().clone();
    let tour = proposal.proposal().clone();
    acan_intent::admit::tour(view, &catalog, &tour)?;
    Ok(TourView::Directed { model, tour })
}

/// The deterministic tour of `view`, from [`acan_intent::deterministic`].
///
/// # Errors
///
/// Returns [`TourError::Deterministic`] when no admissible tour exists for the view.
pub(crate) fn deterministic(view: &PlantView) -> Result<TourView, TourError> {
    Ok(TourView::Deterministic {
        tour: acan_intent::deterministic(view)?,
    })
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

#[cfg(test)]
mod tests {
    use acan_intent::admit::StepFault;
    use acan_llm::{LlmError, Reply, Stub};

    use super::*;

    const VIEW: &str = include_str!("../../acan-intent/fixtures/plant-view-v1.json");
    const PLAIN: &str = include_str!("../../acan-intent/fixtures/plant-view-v1-plain.json");

    fn fixture(text: &str) -> PlantView {
        serde_json::from_str(text).expect("the fixture view decodes")
    }

    fn fixture_model() -> ModelRef {
        "muse-spark".parse().expect("a model id")
    }

    fn directed_tour(view: &PlantView) -> Tour {
        let Some(TourView::Directed { tour, .. }) = view.tour.clone() else {
            panic!("the fixture carries a directed tour");
        };
        tour
    }

    #[test]
    fn the_deterministic_tour_is_the_shared_order_of_the_view() {
        let view = fixture(PLAIN);
        let Ok(TourView::Deterministic { tour }) = deterministic(&view) else {
            panic!("the plain view has a deterministic tour");
        };
        assert_eq!(
            tour,
            acan_intent::deterministic(&view).expect("the shared order"),
        );
    }

    #[test]
    fn a_directed_tour_for_the_view_is_admitted_as_directed() {
        let view = fixture(VIEW);
        let tour = directed_tour(&view);

        let admitted = admit(&view, ModelProposal::new(tour.clone(), fixture_model()))
            .expect("the fixture tour is admitted");

        let TourView::Directed {
            model,
            tour: admitted_tour,
        } = admitted
        else {
            panic!("a proposed tour is admitted as directed");
        };
        assert_eq!(admitted_tour, tour);
        assert_eq!(model, fixture_model());
    }

    #[test]
    fn a_tour_for_another_receipt_is_refused() {
        let view = fixture(VIEW);
        let mut tour = directed_tour(&view);
        tour.receipt_hash = format!("sha256:{}", "0".repeat(64))
            .parse()
            .expect("a receipt hash");

        let err = admit(&view, ModelProposal::new(tour, fixture_model()))
            .expect_err("a stale tour is refused");

        assert!(
            matches!(err, TourError::Refused(TourRejection::StaleReceipt { .. })),
            "{err}"
        );
    }

    #[test]
    fn a_step_whose_template_does_not_describe_its_item_is_refused() {
        let view = fixture(VIEW);
        let mut value = serde_json::to_value(directed_tour(&view)).expect("the tour encodes");
        let step = value["steps"]
            .as_array_mut()
            .expect("steps")
            .iter_mut()
            .find(|step| step["item_id"] == "i-login-limit")
            .expect("the fulfilled item has a step");
        step["caption"]["template_id"] = json!("stopped.out_of_scope");
        let tour: Tour = serde_json::from_value(value).expect("the tour decodes");

        let err = admit(&view, ModelProposal::new(tour, fixture_model()))
            .expect_err("a template for another class is refused");

        assert!(
            matches!(
                err,
                TourError::Refused(TourRejection::Step {
                    fault: StepFault::TemplateDoesNotFit { .. },
                    ..
                })
            ),
            "{err}"
        );
    }

    #[tokio::test]
    async fn the_director_is_asked_for_structured_output_and_its_tour_is_admitted() {
        let view = fixture(VIEW);
        let tour = directed_tour(&view);
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
