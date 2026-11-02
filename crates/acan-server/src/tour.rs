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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acan_intent::admit::TourRejection;
use acan_intent::provenance::ModelProposal;
use acan_intent::{
    CaptionCatalog, DeterministicError, InvalidValue, ModelRef, PlantView, ReceiptHash, Tour,
    TourView,
};
use acan_llm::{ChatRequest, LlmError, Message, ModelClient, Role};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::host::Host;

/// The most output one director reply may use.
const DIRECTOR_MAX_TOKENS: u32 = 2_000;

/// How long a plant view waits for the director before it serves the deterministic tour. The call
/// keeps running after the wait, and its answer is cached for the receipt.
pub(crate) const DIRECTOR_WAIT: Duration = Duration::from_secs(20);

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
    /// The director's route is not Muse Spark on Meta, so no call is made.
    #[error("director needs a Meta Muse Spark route")]
    NotMuseSpark,
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
/// falls back to the foreman's. A view with no item is not sent, and neither is a view whose
/// director route is not Muse Spark on Meta.
///
/// # Errors
///
/// Returns [`TourError::NothingToVisit`] for a view with no item, [`TourError::NotMuseSpark`]
/// when the route is not Muse Spark, [`TourError::Director`] when the call fails or its reply does
/// not parse as a tour, and [`TourError::Model`] when the reply names a model that is not a model
/// id.
pub(crate) async fn direct<C: ModelClient>(
    client: &C,
    view: &PlantView,
) -> Result<ModelProposal<Tour>, TourError> {
    if view.items.is_empty() {
        return Err(TourError::NothingToVisit);
    }
    if !client
        .destination(Role::Director)
        .is_some_and(|destination| destination.is_muse_spark())
    {
        return Err(TourError::NotMuseSpark);
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

/// Validates a proposed tour against `view` and admits it as a directed tour, with the bindings
/// admission made for its steps. Any one fault rejects the whole tour.
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
    let bindings = acan_intent::admit::tour(view, &catalog, &tour)?;
    Ok(TourView::Directed {
        model,
        tour,
        bindings,
    })
}

/// The deterministic tour of `view` with its bindings, from [`acan_intent::deterministic`].
///
/// # Errors
///
/// Returns [`TourError::Deterministic`] when no admissible tour exists for the view.
pub(crate) fn deterministic(view: &PlantView) -> Result<TourView, TourError> {
    Ok(acan_intent::deterministic(view)?)
}

/// What the director made of one receipt: an admitted tour, or the reason there is none.
#[derive(Clone, Debug)]
pub(crate) enum DirectorAnswer {
    /// The director's tour, admitted.
    Tour(TourView),
    /// The call failed, its reply is not a tour, or admission refused it.
    Refused(String),
}

/// The director's answer for each receipt, kept for the life of the server. A receipt is asked
/// once, so one receipt costs at most one director call.
#[derive(Default)]
pub(crate) struct TourCache {
    jobs: Mutex<HashMap<ReceiptHash, Arc<watch::Sender<Option<DirectorAnswer>>>>>,
}

impl TourCache {
    /// The job that answers for `hash`, and whether this call must start it.
    fn job(&self, hash: &ReceiptHash) -> (Arc<watch::Sender<Option<DirectorAnswer>>>, bool) {
        let mut jobs = self
            .jobs
            .lock()
            .expect("the tour cache lock is not poisoned");
        if let Some(job) = jobs.get(hash) {
            return (Arc::clone(job), false);
        }
        let (answer, _) = watch::channel(None);
        let job = Arc::new(answer);
        jobs.insert(*hash, Arc::clone(&job));
        (job, true)
    }

    /// The director's answer for `hash`, once its call has finished.
    #[cfg(test)]
    pub(crate) fn outcome(&self, hash: &ReceiptHash) -> Option<DirectorAnswer> {
        let jobs = self
            .jobs
            .lock()
            .expect("the tour cache lock is not poisoned");
        jobs.get(hash).and_then(|job| job.borrow().clone())
    }
}

/// The view with its tour filled: the director's admitted tour when it answers in time, else the
/// deterministic tour. The plant view never waits longer than [`DIRECTOR_WAIT`] for the model.
pub(crate) async fn fill<C: ModelClient + 'static>(host: &Host<C>, view: PlantView) -> PlantView {
    fill_within(host, view, DIRECTOR_WAIT).await
}

/// [`fill`] with the wait given. The director's call runs in its own task, so a wait that ends
/// first leaves the call running and its answer cached.
pub(crate) async fn fill_within<C: ModelClient + 'static>(
    host: &Host<C>,
    mut view: PlantView,
    wait: Duration,
) -> PlantView {
    let answer = directed(host, &view, wait).await;
    view.tour = match answer {
        Some(DirectorAnswer::Tour(tour)) => Some(tour),
        _ => match deterministic(&view) {
            Ok(tour) => Some(tour),
            Err(err) => {
                tracing::warn!(error = %err, "no deterministic tour; the plant view has none");
                None
            }
        },
    };
    view
}

/// The director's answer for `view`, starting its call if none has started, or `None` when the
/// call has not answered within `wait`.
async fn directed<C: ModelClient + 'static>(
    host: &Host<C>,
    view: &PlantView,
    wait: Duration,
) -> Option<DirectorAnswer> {
    let (job, started) = host.tours().job(&view.receipt_hash);
    if started {
        spawn(host, view.clone(), job.clone());
    }
    let mut answer = job.subscribe();
    match tokio::time::timeout(wait, answer.wait_for(Option::is_some)).await {
        Ok(Ok(ready)) => ready.clone(),
        Ok(Err(_)) => None,
        Err(_) => {
            tracing::warn!(
                receipt = %view.receipt_hash,
                "the director has not answered in time; the plant view serves the deterministic tour"
            );
            None
        }
    }
}

/// Runs the director's call for `view` in its own task and publishes its answer on `job`.
fn spawn<C: ModelClient + 'static>(
    host: &Host<C>,
    view: PlantView,
    job: Arc<watch::Sender<Option<DirectorAnswer>>>,
) {
    let host = host.clone();
    tokio::spawn(async move {
        let call = tokio::spawn({
            let host = host.clone();
            async move { ask(host.client(), &view).await }
        });
        let answer = call
            .await
            .unwrap_or_else(|err| DirectorAnswer::Refused(format!("the director task: {err}")));
        if let DirectorAnswer::Refused(reason) = &answer {
            tracing::warn!(
                reason = %reason,
                "the director's tour was not served; the plant view serves the deterministic tour"
            );
        }
        job.send_replace(Some(answer));
    });
}

/// One director call and its admission.
async fn ask<C: ModelClient>(client: &C, view: &PlantView) -> DirectorAnswer {
    let proposal = match direct(client, view).await {
        Ok(proposal) => proposal,
        Err(err) => return DirectorAnswer::Refused(err.to_string()),
    };
    match admit(view, proposal) {
        Ok(tour) => DirectorAnswer::Tour(tour),
        Err(err) => DirectorAnswer::Refused(err.to_string()),
    }
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
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use acan_intent::admit::StepFault;
    use acan_llm::{
        ChatResponse, Destination, MUSE_SPARK_MODELS, MUSE_SPARK_PROVIDER, Reply, Stub,
    };

    use super::*;
    use crate::host::testing::host_with;

    const VIEW: &str = include_str!("../../acan-intent/fixtures/plant-view-v1.json");
    const PLAIN: &str = include_str!("../../acan-intent/fixtures/plant-view-v1-plain.json");

    fn fixture(text: &str) -> PlantView {
        serde_json::from_str(text).expect("the fixture view decodes")
    }

    /// The route a director must have for a tour to be asked for: Muse Spark on Meta.
    fn muse_spark_route() -> Destination {
        Destination {
            provider: MUSE_SPARK_PROVIDER.to_owned(),
            model: MUSE_SPARK_MODELS[0].to_owned(),
        }
    }

    fn fixture_model() -> ModelRef {
        MUSE_SPARK_MODELS[0].parse().expect("a model id")
    }

    fn directed_tour(view: &PlantView) -> Tour {
        let Some(TourView::Directed { tour, .. }) = view.tour.clone() else {
            panic!("the fixture carries a directed tour");
        };
        tour
    }

    /// A stub whose director is routed to Muse Spark and answers with `reply`.
    fn director(reply: Reply) -> Stub {
        Stub::default()
            .routed(Role::Director, muse_spark_route())
            .when(Role::Director, |_| true, reply)
    }

    #[test]
    fn the_deterministic_tour_is_the_shared_order_of_the_view() {
        let view = fixture(PLAIN);

        let tour = deterministic(&view).expect("the plain view has a deterministic tour");

        assert_eq!(
            tour,
            acan_intent::deterministic(&view).expect("the shared order"),
        );
        let TourView::Deterministic {
            tour: steps,
            bindings,
        } = &tour
        else {
            panic!("the plain view has no director, so its tour is deterministic");
        };
        assert_eq!(bindings.len(), steps.steps.len(), "one binding per step");
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
            bindings,
        } = admitted
        else {
            panic!("a proposed tour is admitted as directed");
        };
        assert_eq!(admitted_tour, tour);
        assert_eq!(model, fixture_model());
        assert_eq!(bindings.len(), tour.steps.len(), "one binding per step");
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
        let stub = Stub::default()
            .routed(Role::Director, muse_spark_route())
            .when(
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
            ..
        } = admitted
        else {
            panic!("a proposed tour is admitted as directed");
        };
        assert_eq!(admitted_tour, tour);
        assert_eq!(
            model,
            fixture_model(),
            "the model is the route's Muse Spark"
        );
        let calls = stub.ledger().records();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].role, Role::Director);
        assert_eq!(calls[0].provider, MUSE_SPARK_PROVIDER);
        assert_eq!(calls[0].model, MUSE_SPARK_MODELS[0]);
    }

    #[tokio::test]
    async fn a_reply_that_is_not_a_tour_is_an_error_before_admission() {
        let view = fixture(VIEW);
        let stub = director(Reply::text("not json"));

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
        let stub = Stub::default().routed(Role::Director, muse_spark_route());

        let err = direct(&stub, &view)
            .await
            .expect_err("no reply is scripted for the director");

        assert!(
            matches!(err, TourError::Director(LlmError::NoScriptedReply { .. })),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_director_that_is_not_on_muse_spark_is_not_asked() {
        let view = fixture(VIEW);
        let tour = directed_tour(&view);
        let routes = [
            None,
            Some(Destination {
                provider: "baseten".to_owned(),
                model: "meta-models/muse-spark-open-1026".to_owned(),
            }),
            Some(Destination {
                provider: MUSE_SPARK_PROVIDER.to_owned(),
                model: "muse-spark-open".to_owned(),
            }),
        ];
        for route in routes {
            let base = Stub::default().when(Role::Director, |_| true, Reply::json(&tour));
            let stub = match route {
                Some(route) => base.routed(Role::Director, route),
                None => base,
            };

            let err = direct(&stub, &view)
                .await
                .expect_err("only a Muse Spark route is asked");

            assert!(matches!(err, TourError::NotMuseSpark), "{err}");
            assert_eq!(
                err.to_string(),
                "director needs a Meta Muse Spark route",
                "the reason names the route"
            );
            assert!(stub.ledger().records().is_empty(), "no call was made");
        }
    }

    /// The tour as JSON text, with the first step's first caption slot written twice. A JSON value
    /// cannot hold a repeated key, so the text is edited directly.
    fn tour_text_with_a_repeated_slot(tour: &Tour) -> String {
        let value = serde_json::to_value(tour).expect("the tour encodes");
        let slots = value["steps"]
            .as_array()
            .expect("steps")
            .iter()
            .map(|step| &step["caption"]["slots"])
            .find(|slots| slots.as_object().is_some_and(|map| !map.is_empty()))
            .expect("a step binds a slot")
            .clone();
        let (key, first) = slots
            .as_object()
            .expect("the slots are an object")
            .iter()
            .next()
            .expect("the slots are not empty");
        let repeated = format!(
            "{{{}:{},",
            serde_json::to_string(key).expect("a key is JSON"),
            serde_json::to_string(first).expect("a value is JSON"),
        );
        let text = serde_json::to_string(&value).expect("the tour is JSON");
        let slots_text = serde_json::to_string(&slots).expect("the slots are JSON");
        text.replacen(&slots_text, &format!("{repeated}{}", &slots_text[1..]), 1)
    }

    #[tokio::test]
    async fn a_director_reply_with_a_repeated_caption_slot_is_refused_at_decode() {
        let view = fixture(VIEW);
        let text = tour_text_with_a_repeated_slot(&directed_tour(&view));
        let stub = director(Reply::text(text));

        let err = direct(&stub, &view)
            .await
            .expect_err("a repeated slot is not a tour");

        let TourError::Director(LlmError::SchemaValidation { detail, .. }) = &err else {
            panic!("the reply is refused as a schema error: {err}");
        };
        assert!(detail.contains("duplicate caption slot"), "{detail}");
    }

    #[tokio::test]
    async fn a_view_with_no_item_is_not_sent_to_the_director() {
        let mut view = fixture(VIEW);
        view.items.clear();
        let stub = Stub::default().routed(Role::Director, muse_spark_route());

        let err = direct(&stub, &view)
            .await
            .expect_err("an empty view has no tour to order");

        assert!(matches!(err, TourError::NothingToVisit), "{err}");
        assert!(stub.ledger().records().is_empty(), "no call was made");
    }

    /// Counts the calls its client receives, and routes the director as its client does.
    struct Counting<C> {
        inner: C,
        calls: Arc<AtomicUsize>,
    }

    impl<C: ModelClient> ModelClient for Counting<C> {
        fn destination(&self, role: Role) -> Option<Destination> {
            self.inner.destination(role)
        }

        fn chat(
            &self,
            role: Role,
            request: ChatRequest,
        ) -> impl Future<Output = Result<ChatResponse, LlmError>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.chat(role, request)
        }
    }

    /// A client routed to Muse Spark whose calls never answer.
    struct Hanging;

    impl ModelClient for Hanging {
        fn destination(&self, role: Role) -> Option<Destination> {
            (role == Role::Director).then(muse_spark_route)
        }

        fn chat(
            &self,
            _role: Role,
            _request: ChatRequest,
        ) -> impl Future<Output = Result<ChatResponse, LlmError>> + Send {
            std::future::pending()
        }
    }

    /// The fixture view with no tour, as the plant view builds it before the director.
    fn untoured(text: &str) -> PlantView {
        let mut view = fixture(text);
        view.tour = None;
        view
    }

    #[tokio::test]
    async fn an_admitted_director_tour_fills_the_view_and_is_cached_for_its_receipt() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let view = untoured(VIEW);
        let hash = view.receipt_hash;
        let tour = directed_tour(&fixture(VIEW));
        let calls = Arc::new(AtomicUsize::new(0));
        let host = host_with(
            dir.path(),
            Counting {
                inner: director(Reply::json(&tour)),
                calls: Arc::clone(&calls),
            },
        )
        .await;

        let filled = fill_within(&host, view, Duration::from_secs(5)).await;

        let Some(TourView::Directed {
            tour: shown,
            bindings,
            ..
        }) = &filled.tour
        else {
            panic!("the director's tour fills the view: {:?}", filled.tour);
        };
        assert_eq!(*shown, tour);
        assert_eq!(bindings.len(), tour.steps.len(), "one binding per step");
        assert!(matches!(
            host.tours().outcome(&hash),
            Some(DirectorAnswer::Tour(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_director_tour_for_another_receipt_is_refused_and_the_deterministic_tour_is_served() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let view = untoured(VIEW);
        let hash = view.receipt_hash;
        let mut tour = directed_tour(&fixture(VIEW));
        tour.receipt_hash = format!("sha256:{}", "0".repeat(64))
            .parse()
            .expect("a receipt hash");
        let host = host_with(dir.path(), director(Reply::json(&tour))).await;

        let filled = fill_within(&host, view, Duration::from_secs(5)).await;

        assert!(
            matches!(filled.tour, Some(TourView::Deterministic { .. })),
            "{:?}",
            filled.tour
        );
        let Some(DirectorAnswer::Refused(reason)) = host.tours().outcome(&hash) else {
            panic!("the refusal is cached for the receipt");
        };
        assert!(reason.contains("refused"), "{reason}");
    }

    #[tokio::test]
    async fn an_unrouted_director_serves_the_deterministic_tour_with_its_failure_recorded() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let view = untoured(VIEW);
        let hash = view.receipt_hash;
        let calls = Arc::new(AtomicUsize::new(0));
        let host = host_with(
            dir.path(),
            Counting {
                inner: Stub::default(),
                calls: Arc::clone(&calls),
            },
        )
        .await;

        let filled = fill_within(&host, view, Duration::from_secs(5)).await;

        assert!(
            matches!(filled.tour, Some(TourView::Deterministic { .. })),
            "{:?}",
            filled.tour
        );
        let Some(DirectorAnswer::Refused(reason)) = host.tours().outcome(&hash) else {
            panic!("the failure is cached for the receipt");
        };
        assert_eq!(reason, "director needs a Meta Muse Spark route");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no call was made");
    }

    #[tokio::test]
    async fn a_view_the_deterministic_order_cannot_bind_has_no_tour() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // A bounced obligation failure marked fixed has no caption: the one gap the catalog keeps.
        let mut value: Value = serde_json::from_str(VIEW).expect("the fixture decodes");
        value["tour"] = Value::Null;
        let item = value["items"]
            .as_array_mut()
            .expect("items")
            .iter_mut()
            .find(|item| item["item_id"] == "i-retry-after")
            .expect("the failed item is in the view");
        item["state"] = json!("fixed");
        let view: PlantView = serde_json::from_value(value).expect("the view decodes");
        let host = host_with(dir.path(), Stub::default()).await;

        let filled = fill_within(&host, view, Duration::from_secs(5)).await;

        assert!(filled.tour.is_none(), "{:?}", filled.tour);
    }

    #[tokio::test]
    async fn a_receipt_costs_one_director_call_however_often_its_view_is_filled() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let tour = directed_tour(&fixture(VIEW));
        let calls = Arc::new(AtomicUsize::new(0));
        let host = host_with(
            dir.path(),
            Counting {
                inner: director(Reply::json(&tour)),
                calls: Arc::clone(&calls),
            },
        )
        .await;

        let first = fill_within(&host, untoured(VIEW), Duration::from_secs(5)).await;
        let second = fill_within(&host, untoured(VIEW), Duration::from_secs(5)).await;

        assert!(matches!(first.tour, Some(TourView::Directed { .. })));
        assert!(matches!(second.tour, Some(TourView::Directed { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_director_that_is_still_running_is_not_waited_for_and_is_not_asked_again() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let view = untoured(VIEW);
        let hash = view.receipt_hash;
        let calls = Arc::new(AtomicUsize::new(0));
        let host = host_with(
            dir.path(),
            Counting {
                inner: Hanging,
                calls: Arc::clone(&calls),
            },
        )
        .await;

        let first = fill_within(&host, view.clone(), Duration::from_millis(50)).await;
        let second = fill_within(&host, view, Duration::from_millis(50)).await;

        assert!(matches!(first.tour, Some(TourView::Deterministic { .. })));
        assert!(matches!(second.tour, Some(TourView::Deterministic { .. })));
        assert!(host.tours().outcome(&hash).is_none(), "no answer yet");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one call, still running");
    }
}
