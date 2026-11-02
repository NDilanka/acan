//! The tour the page plays over one receipt snapshot: the walk the camera takes, the marks each step
//! stages, and the captions it shows, all admitted from one `plant-view/1` view.
//!
//! The view may carry a tour: the director's proposal. This module admits it through
//! [`admit::tour`], the one admission path. When the view has no tour, or the proposal is not
//! admitted, the page plays [`acan_intent::deterministic`], the same order the server builds from
//! the same view. Captions are catalog templates whose slots take copies of evidence rows, so no
//! model prose is ever shown. The captions show the bindings the tour carries, and only when local
//! admission binds the same values. Nothing here changes a class, count or denominator: the page
//! draws those from the view, and a tour only orders the walk over them.

use acan_intent::admit;
use acan_intent::{
    BoundItemView, CaptionCatalog, CaptionTemplate, CommitSha, DeterministicError, GateState,
    ItemClass, ItemId, ItemState, MachineId, MarkRef, ModelRef, OrderRef, PlantCounts, PlantView,
    ReceiptHash, Tour, TourStep, TourView, ZoomLevel,
};
use serde::Serialize;
use serde_json::Value;

/// Milliseconds one step's camera move takes.
pub const MOVE_MS: f32 = 600.0;

/// Where the camera stands: its pivot on the ground and its zoom.
pub type Pose = ([f32; 2], f32);

/// The zoom of a machine's step, as a share of the fit zoom.
const MACHINE_RATIO: f32 = 2.5;
/// The zoom of a close-up step, as a share of the fit zoom.
const CLOSE_RATIO: f32 = 4.0;

/// Why a view has no tour the page can play. The plate shows these messages, so each says what
/// happened in plain words and that every item is still listed.
#[derive(Debug, thiserror::Error)]
pub enum PlayError {
    /// The text is not a `plant-view/1` view.
    #[error("not a plant-view/1 view: {0}")]
    Malformed(#[from] serde_json::Error),
    /// The counts the view states do not match its own items.
    #[error("the view's counts do not match its items: {0}")]
    Counts(String),
    /// No deterministic order is admitted for this view.
    #[error("This receipt cannot be walked: {0}. Every item is listed below.")]
    NoTour(String),
    /// A mandatory item has no caption its evidence fills, so the walk cannot visit it.
    #[error(
        "This receipt cannot be walked: item {item_id} has no caption its evidence fills. Every item is listed below."
    )]
    Unbindable {
        /// The item.
        item_id: ItemId,
    },
    /// The caption catalog cannot bind a caption for this view.
    #[error("the caption catalog cannot bind this view: {0}")]
    Catalog(String),
}

/// Which order plays: the director's, or the deterministic one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Ordered by the director and admitted.
    Directed,
    /// The deterministic order, from the view or built from it.
    Deterministic,
}

/// One mark a step stages, with the words the page shows for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stage {
    /// What the mark is, in words.
    pub label: String,
    /// The mark itself.
    pub mark: MarkRef,
}

/// One step of the walk, ready to play.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlayStep {
    /// The receipt item the step is about.
    pub item_id: ItemId,
    /// The machine the camera goes to.
    pub machine_id: MachineId,
    /// The zoom the camera goes to.
    pub zoom: ZoomLevel,
    /// The marks the step stages.
    pub stage: Vec<Stage>,
    /// The caption, filled from the bound evidence only.
    pub caption: String,
}

/// One receipt item as the plate lists it, copied from the view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlateItem {
    /// The item.
    pub item_id: ItemId,
    /// Its machine.
    pub machine_id: MachineId,
    /// Its class.
    pub class: ItemClass,
    /// Its state.
    pub state: ItemState,
    /// Whether every tour must reach it.
    pub mandatory: bool,
    /// Whether the tour that plays reaches it.
    pub reached: bool,
}

/// Everything the page shows for one receipt: the view's own facts, and the admitted walk.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Play {
    /// The receipt.
    pub receipt_hash: ReceiptHash,
    /// The order.
    pub order: OrderRef,
    /// The order's base commit.
    pub base_sha: CommitSha,
    /// The tested merged commit.
    pub merged_sha: CommitSha,
    /// Where the gate is.
    pub gate_state: GateState,
    /// The counts, each with its denominator.
    pub counts: PlantCounts,
    /// Every item, in view order.
    pub items: Vec<PlateItem>,
    /// Which order plays.
    pub mode: Mode,
    /// The director, for a directed tour.
    pub model: Option<ModelRef>,
    /// Why this order plays, when it is not the admitted directed tour. Empty otherwise.
    pub note: String,
    /// The walk, in order.
    pub steps: Vec<PlayStep>,
}

/// Decodes a `plant-view/1` view, admits its tour or the deterministic order, and returns what the
/// page plays.
///
/// # Errors
///
/// Returns [`PlayError`] when the view does not decode, its counts disagree with its items, the
/// catalog cannot bind it, or no order can be admitted.
pub fn play(json: &str) -> Result<Play, PlayError> {
    let mut value: Value = serde_json::from_str(json)?;
    let proposed = value
        .as_object_mut()
        .and_then(|object| object.remove("tour"));
    let view: PlantView = serde_json::from_value(value)?;
    view.check_counts()
        .map_err(|err| PlayError::Counts(err.to_string()))?;
    let catalog = catalog()?;
    let note = match proposed {
        None => "This view carries no tour, so the deterministic order plays.".to_owned(),
        Some(value) => match serde_json::from_value::<TourView>(known_tour(value)) {
            Ok(tour_view) => match admit_view(&view, &catalog, tour_view) {
                Ok(admitted) => return assemble(&view, &catalog, admitted, String::new()),
                Err(reason) => not_admitted(&reason),
            },
            Err(err) => format!(
                "The view's tour could not be read ({err}), so the deterministic order plays."
            ),
        },
    };
    let tour_view = deterministic_tour(&view)?;
    let admitted = admit_view(&view, &catalog, tour_view).map_err(PlayError::NoTour)?;
    assemble(&view, &catalog, admitted, note)
}

/// A tour the page plays: who ordered it, the tour, and the bindings its steps show.
struct Admitted {
    mode: Mode,
    model: Option<ModelRef>,
    tour: Tour,
    bindings: Vec<BoundItemView>,
}

/// Checks a tour against the view and returns the bindings the page shows. The bindings the tour
/// carries are the ones shown, and local admission is the check: it must bind the same values, or
/// the tour is refused. The reason is the plain text the note names.
fn admit_view(
    view: &PlantView,
    catalog: &CaptionCatalog,
    tour_view: TourView,
) -> Result<Admitted, String> {
    let (mode, model, tour, sent) = match tour_view {
        TourView::Directed {
            model,
            tour,
            bindings,
        } => (Mode::Directed, Some(model), tour, bindings),
        TourView::Deterministic { tour, bindings } => (Mode::Deterministic, None, tour, bindings),
    };
    let local = admit::tour(view, catalog, &tour).map_err(|rejection| rejection.to_string())?;
    if local != sent {
        return Err("its bindings differ from the values trusted code binds".to_owned());
    }
    Ok(Admitted {
        mode,
        model,
        tour,
        bindings: sent,
    })
}

/// The deterministic order of `view`, built and admitted by acan-intent, with its bindings. The
/// server builds the same walk from the same view.
fn deterministic_tour(view: &PlantView) -> Result<TourView, PlayError> {
    acan_intent::deterministic(view).map_err(|err| match err {
        DeterministicError::Unbindable { item_id, .. } => PlayError::Unbindable { item_id },
        DeterministicError::Catalog(err) => PlayError::Catalog(err.to_string()),
        other => PlayError::NoTour(other.to_string()),
    })
}

/// The note the page shows when the view's tour is refused.
fn not_admitted(reason: &str) -> String {
    format!("The view's tour was not admitted ({reason}), so the deterministic order plays.")
}

/// The zoom level a view names, as the page's JSON spells it: `fit`, `machine` or `close`.
#[must_use]
pub fn zoom_named(name: &str) -> Option<ZoomLevel> {
    serde_json::from_value(Value::String(name.to_owned())).ok()
}

/// How far a step's zoom stands from the fit zoom. `fit` has none: the step goes to the fit pose.
#[must_use]
pub fn zoom_ratio(zoom: ZoomLevel) -> Option<f32> {
    match zoom {
        ZoomLevel::Fit => None,
        ZoomLevel::Machine => Some(MACHINE_RATIO),
        ZoomLevel::Close => Some(CLOSE_RATIO),
    }
}

/// The pose a move is at `elapsed_ms` after it starts: the pivot slides and the zoom scales
/// geometrically, both on the cubic ease-out the turn uses. The move lands on `to` exactly.
#[must_use]
pub fn progress(from: Pose, to: Pose, elapsed_ms: f32) -> Pose {
    let t = (elapsed_ms.max(0.0) / MOVE_MS).min(1.0);
    if t >= 1.0 {
        return to;
    }
    let eased = 1.0 - (1.0 - t).powi(3);
    let pivot = std::array::from_fn(|axis| from.0[axis] + (to.0[axis] - from.0[axis]) * eased);
    let zoom = from.1.powf(1.0 - eased) * to.1.powf(eased);
    (pivot, zoom)
}

fn catalog() -> Result<CaptionCatalog, PlayError> {
    CaptionCatalog::trusted().map_err(|err| PlayError::Catalog(err.to_string()))
}

/// Builds the played result from an admitted tour and the bindings its steps show.
fn assemble(
    view: &PlantView,
    catalog: &CaptionCatalog,
    admitted: Admitted,
    note: String,
) -> Result<Play, PlayError> {
    let Admitted {
        mode,
        model,
        tour,
        bindings,
    } = admitted;
    let steps = tour
        .steps
        .iter()
        .zip(&bindings)
        .map(|(step, bound)| play_step(catalog, step, bound))
        .collect::<Result<Vec<_>, _>>()?;
    let items = view
        .items
        .iter()
        .map(|item| PlateItem {
            item_id: item.item_id.clone(),
            machine_id: item.machine_id.clone(),
            class: item.class,
            state: item.state,
            mandatory: item.is_mandatory(),
            reached: tour.reaches(&item.item_id),
        })
        .collect();
    Ok(Play {
        receipt_hash: view.receipt_hash,
        order: view.order.id.clone(),
        base_sha: view.base_sha.clone(),
        merged_sha: view.merged_sha.clone(),
        gate_state: view.gate_state,
        counts: view.counts.clone(),
        items,
        mode,
        model,
        note,
        steps,
    })
}

fn play_step(
    catalog: &CaptionCatalog,
    step: &TourStep,
    bound: &BoundItemView,
) -> Result<PlayStep, PlayError> {
    let template = catalog.template(&step.caption.template_id).ok_or_else(|| {
        PlayError::Catalog(format!("template {} is missing", step.caption.template_id))
    })?;
    Ok(PlayStep {
        item_id: step.item_id.clone(),
        machine_id: step.camera.machine_id.clone(),
        zoom: step.camera.zoom,
        stage: step
            .stage
            .iter()
            .map(|mark| Stage {
                label: stage_label(mark),
                mark: mark.clone(),
            })
            .collect(),
        caption: caption_text(template, bound)?,
    })
}

/// The caption text: the template with each slot filled from its bound value. A slot the template
/// does not declare is a catalog fault, never a literal.
fn caption_text(template: &CaptionTemplate, bound: &BoundItemView) -> Result<String, PlayError> {
    let mut text = String::new();
    let mut rest = template.text.as_str();
    while let Some(open) = rest.find('{') {
        let close = rest[open..].find('}').map(|at| open + at).ok_or_else(|| {
            PlayError::Catalog(format!("{} has an open slot", template.template_id))
        })?;
        let name = &rest[open + 1..close];
        let value = bound
            .typed_slot_bindings
            .iter()
            .find(|(slot, _)| slot.as_str() == name)
            .map(|(_, value)| value)
            .ok_or_else(|| {
                PlayError::Catalog(format!("{} has no slot {name}", template.template_id))
            })?;
        text.push_str(&rest[..open]);
        text.push_str(&value.to_string());
        rest = &rest[close + 1..];
    }
    text.push_str(rest);
    Ok(text)
}

/// What a mark is, in the words the page shows beside the step.
fn stage_label(mark: &MarkRef) -> String {
    match mark {
        MarkRef::Footprint { item_id } => format!("may change: {item_id}"),
        MarkRef::MustKeepPin { must_keep_id } => format!("must keep: {must_keep_id}"),
        MarkRef::QuestionPin { choice_id } => format!("question: {choice_id}"),
        MarkRef::NoProbes { machine_id } => format!("no probes: {machine_id}"),
        MarkRef::Ghost { item_id } => format!("ghost base: {item_id}"),
        MarkRef::Crates { item_id } => format!("route crates: {item_id}"),
        MarkRef::BouncedChip { item_id } => format!("bounced: {item_id}"),
        MarkRef::ResidueCard { item_id } => format!("residue: {item_id}"),
        MarkRef::RulingTag { ruling_id } => format!("ruling: {ruling_id}"),
        MarkRef::Unknown { item_id } => format!("unknown: {item_id}"),
        MarkRef::OrderPlate {} => "order plate".to_owned(),
    }
}

/// Keeps only the named fields of a JSON object. A tour's structs declare these fields; a field
/// a later contract adds is dropped here, because admission never reads it.
fn keep(value: &mut Value, fields: &[&str]) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| fields.contains(&key.as_str()));
    }
}

const REF_FIELDS: &[&str] = &["item_id", "slot", "step", "field"];

/// Drops the fields a tour's structs do not declare, so a tour from a later contract still
/// decodes. A mark of a kind this build does not know is left whole, so it fails to decode and the
/// page plays the deterministic order.
fn known_tour(mut value: Value) -> Value {
    // The bindings are kept whole: admission compares them with its own, so none is dropped.
    keep(&mut value, &["mode", "model", "tour", "bindings"]);
    if let Some(tour) = value.get_mut("tour") {
        keep(tour, &["receipt_hash", "steps"]);
        if let Some(steps) = tour.get_mut("steps").and_then(Value::as_array_mut) {
            for step in steps {
                known_step(step);
            }
        }
    }
    value
}

fn known_step(step: &mut Value) {
    keep(
        step,
        &["item_id", "camera", "stage", "evidence_refs", "caption"],
    );
    if let Some(camera) = step.get_mut("camera") {
        keep(camera, &["machine_id", "zoom"]);
    }
    if let Some(marks) = step.get_mut("stage").and_then(Value::as_array_mut) {
        for mark in marks {
            known_mark(mark);
        }
    }
    if let Some(refs) = step.get_mut("evidence_refs").and_then(Value::as_array_mut) {
        for reference in refs {
            keep(reference, REF_FIELDS);
        }
    }
    // A caption that carries its own text is a literal, and captions never carry literals. It is left
    // whole, so it fails to decode and the page plays the deterministic order, as the server does.
    let literal = step["caption"].get("text").is_some();
    if let Some(caption) = step.get_mut("caption").filter(|_| !literal) {
        keep(caption, &["template_id", "slots"]);
        if let Some(slots) = caption.get_mut("slots").and_then(Value::as_object_mut) {
            for reference in slots.values_mut() {
                keep(reference, REF_FIELDS);
            }
        }
    }
}

fn known_mark(mark: &mut Value) {
    let fields: &[&str] = match mark.get("mark").and_then(Value::as_str) {
        Some("footprint" | "ghost" | "crates" | "bounced_chip" | "residue_card" | "unknown") => {
            &["item_id"]
        }
        Some("must_keep_pin") => &["must_keep_id"],
        Some("question_pin") => &["choice_id"],
        Some("no_probes") => &["machine_id"],
        Some("ruling_tag") => &["ruling_id"],
        Some("order_plate") => &[],
        _ => return,
    };
    let mut kept = vec!["mark"];
    kept.extend_from_slice(fields);
    keep(mark, &kept);
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIRECTED: &str = include_str!("../../acan-intent/fixtures/plant-view-v1.json");
    const PLAIN: &str = include_str!("../../acan-intent/fixtures/plant-view-v1-plain.json");

    /// The fixture's JSON with `edit` applied to its tour, for the invalid-tour tests.
    fn edited(edit: impl FnOnce(&mut Value)) -> String {
        let mut value: Value = serde_json::from_str(DIRECTED).expect("the fixture is JSON");
        edit(&mut value);
        value.to_string()
    }

    /// The index of the tour step about `item_id`.
    fn step_of(value: &Value, item_id: &str) -> usize {
        value["tour"]["tour"]["steps"]
            .as_array()
            .expect("the fixture has steps")
            .iter()
            .position(|step| step["item_id"] == item_id)
            .expect("the fixture walks the item")
    }

    fn step<'a>(play: &'a Play, item_id: &str) -> &'a PlayStep {
        play.steps
            .iter()
            .find(|step| step.item_id.to_string() == item_id)
            .expect("the play walks the item")
    }

    #[test]
    fn the_directed_fixture_plays_its_admitted_tour() {
        let play = play(DIRECTED).expect("the fixture plays");
        assert_eq!(play.mode, Mode::Directed);
        assert_eq!(
            play.model.as_ref().map(ToString::to_string).as_deref(),
            Some("muse-spark")
        );
        assert!(play.note.is_empty(), "{}", play.note);
        assert_eq!(play.steps.len(), 8);
        assert!(play.steps.iter().all(|step| !step.caption.contains('{')));
        for item in play.items.iter().filter(|item| item.mandatory) {
            assert!(item.reached, "{} is mandatory and reached", item.item_id);
        }
    }

    #[test]
    fn captions_are_filled_from_bound_evidence_and_missing_values_are_named() {
        let play = play(DIRECTED).expect("the fixture plays");
        assert_eq!(
            step(&play, "i-login-limit").caption,
            "Classified: obligation fulfilled. Candidate value here: 429."
        );
        assert_eq!(
            step(&play, "d-links-clicks").caption,
            "Classified: out of scope. Base value here: 0. Candidate value here: no value. Recorded state: stopped."
        );
        assert_eq!(
            step(&play, "d-links-sort").caption,
            "Classified: ruling violation. Candidate value here: a1."
        );
    }

    #[test]
    fn stages_name_the_marks_the_step_draws() {
        let play = play(DIRECTED).expect("the fixture plays");
        let labels: Vec<&str> = step(&play, "d-links-sort")
            .stage
            .iter()
            .map(|stage| stage.label.as_str())
            .collect();
        assert!(labels.contains(&"ruling: r-1"), "{labels:?}");
    }

    #[test]
    fn a_view_without_a_tour_plays_the_built_order_and_says_so() {
        let play = play(PLAIN).expect("the plain fixture plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert_eq!(play.model, None);
        assert!(play.note.contains("no tour"), "{}", play.note);
        let order: Vec<String> = play
            .steps
            .iter()
            .map(|step| step.item_id.to_string())
            .collect();
        assert_eq!(
            order.first().map(String::as_str),
            Some("d-login-error-body")
        );
        assert_eq!(order.last().map(String::as_str), Some("i-login-limit"));
        let residue = play
            .steps
            .iter()
            .position(|s| s.item_id.to_string() == "d-login-error-body");
        let violation = play
            .steps
            .iter()
            .position(|s| s.item_id.to_string() == "d-links-sort");
        let bounced = play
            .steps
            .iter()
            .position(|s| s.item_id.to_string() == "i-retry-after");
        assert!(residue < violation && violation < bounced, "{order:?}");
        for item in play.items.iter().filter(|item| item.mandatory) {
            assert!(item.reached, "{} is mandatory and reached", item.item_id);
        }
    }

    #[test]
    fn the_views_own_deterministic_order_is_admitted_without_a_note() {
        let json = edited(|value| {
            value["tour"]["mode"] = "deterministic".into();
            value["tour"]
                .as_object_mut()
                .expect("a tour")
                .remove("model");
        });
        let play = play(&json).expect("the deterministic order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.is_empty(), "{}", play.note);
    }

    #[test]
    fn a_missing_mandatory_item_is_refused_and_the_built_order_plays() {
        let json = edited(|value| {
            let index = step_of(value, "d-links-sort");
            value["tour"]["tour"]["steps"]
                .as_array_mut()
                .expect("steps")
                .remove(index);
        });
        let play = play(&json).expect("the built order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.contains("is not reached"), "{}", play.note);
        assert!(
            play.steps
                .iter()
                .any(|step| step.item_id.to_string() == "d-links-sort")
        );
    }

    #[test]
    fn an_unknown_evidence_reference_is_refused() {
        let json = edited(|value| {
            let index = step_of(value, "d-links-sort");
            value["tour"]["tour"]["steps"][index]["evidence_refs"][0]["step"] = "s99".into();
        });
        let play = play(&json).expect("the built order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.contains("names no evidence row"), "{}", play.note);
    }

    #[test]
    fn a_caption_naming_a_template_outside_the_catalog_is_refused() {
        let json = edited(|value| {
            let index = step_of(value, "d-links-sort");
            value["tour"]["tour"]["steps"][index]["caption"]["template_id"] = "literal.text".into();
        });
        let play = play(&json).expect("the built order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.contains("not admitted"), "{}", play.note);
    }

    #[test]
    fn a_swapped_base_and_candidate_slot_is_refused() {
        let json = edited(|value| {
            let index = step_of(value, "d-login-error-body");
            value["tour"]["tour"]["steps"][index]["caption"]["slots"]["base"]["slot"] =
                "candidate".into();
        });
        let play = play(&json).expect("the built order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.contains("another column"), "{}", play.note);
    }

    #[test]
    fn a_tour_for_another_receipt_is_refused() {
        let json = edited(|value| {
            value["tour"]["tour"]["receipt_hash"] = format!("sha256:{}", "0".repeat(64)).into();
        });
        let play = play(&json).expect("the built order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.contains("not the view's"), "{}", play.note);
    }

    #[test]
    fn a_mark_this_build_does_not_know_fails_the_tour_to_the_built_order() {
        let json = edited(|value| {
            let index = step_of(value, "d-links-sort");
            value["tour"]["tour"]["steps"][index]["stage"][0] =
                serde_json::json!({"mark": "hologram", "item_id": "d-links-sort"});
        });
        let play = play(&json).expect("the built order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.contains("could not be read"), "{}", play.note);
    }

    #[test]
    fn a_later_contracts_extra_step_fields_are_ignored() {
        let json = edited(|value| {
            let index = step_of(value, "i-login-limit");
            value["tour"]["tour"]["steps"][index]["confidence"] = 0.9.into();
            value["tour"]["tour"]["steps"][index]["camera"]["angle"] = 40.into();
        });
        let play = play(&json).expect("the directed tour plays");
        assert_eq!(play.mode, Mode::Directed);
        assert_eq!(
            step(&play, "i-login-limit").caption,
            "Classified: obligation fulfilled. Candidate value here: 429."
        );
    }

    #[test]
    fn a_caption_that_carries_its_own_text_is_refused_and_the_built_order_plays() {
        let json = edited(|value| {
            let index = step_of(value, "i-login-limit");
            value["tour"]["tour"]["steps"][index]["caption"]["text"] = "Literal words.".into();
        });
        let play = play(&json).expect("the built order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.contains("could not be read"), "{}", play.note);
        assert!(!step(&play, "i-login-limit").caption.contains("Literal"));
    }

    #[test]
    fn a_view_whose_counts_disagree_with_its_items_is_refused() {
        let json = edited(|value| {
            value["counts"]["by_class"]["preserved"] = 2.into();
        });
        assert!(matches!(play(&json), Err(PlayError::Counts(_))));
    }

    #[test]
    fn text_that_is_not_a_view_is_refused() {
        assert!(matches!(play("{}"), Err(PlayError::Malformed(_))));
    }

    #[test]
    fn the_move_lands_on_both_ends_exactly_and_slides_between_them() {
        let from = ([0.0, 0.0], 1.0);
        let to = ([40.0, -10.0], 4.0);
        assert_eq!(progress(from, to, 0.0), from);
        assert_eq!(progress(from, to, MOVE_MS), to);
        assert_eq!(progress(from, to, MOVE_MS * 3.0), to);
        let (pivot, zoom) = progress(from, to, MOVE_MS / 2.0);
        assert!(
            pivot[0] > 30.0 && pivot[0] < 40.0,
            "the pivot is most of the way: {pivot:?}"
        );
        assert!(
            zoom > 1.0 && zoom < 4.0,
            "the zoom is between the ends: {zoom}"
        );
        let quarter = progress(from, to, MOVE_MS / 4.0).0[0];
        let half = progress(from, to, MOVE_MS / 2.0).0[0];
        assert!(quarter < half, "the pivot moves forward");
    }

    #[test]
    fn a_fit_step_has_no_ratio_and_the_other_levels_zoom_in() {
        assert_eq!(zoom_named("close"), Some(ZoomLevel::Close));
        assert_eq!(zoom_named("far"), None);
        assert_eq!(zoom_ratio(ZoomLevel::Fit), None);
        assert!(zoom_ratio(ZoomLevel::Machine).is_some_and(|r| r > 1.0));
        assert!(zoom_ratio(ZoomLevel::Close).is_some_and(|r| r > 2.5));
    }

    #[test]
    fn the_payload_carries_every_field_the_plate_reads() {
        let directed = play(DIRECTED).expect("the fixture plays");
        let json = serde_json::to_value(&directed).expect("the play serializes");
        for field in [
            "receipt_hash",
            "order",
            "base_sha",
            "merged_sha",
            "gate_state",
            "counts",
            "items",
            "mode",
            "model",
            "note",
            "steps",
        ] {
            assert!(json.get(field).is_some(), "the payload has {field}");
        }
        let counts = &json["counts"];
        assert!(counts["by_class"].is_object());
        for field in [
            "probes_run",
            "probes_total",
            "routes_covered",
            "routes_total",
            "masks_applied",
        ] {
            assert!(counts[field].is_u64(), "counts.{field}");
        }
        for field in ["obligations_total", "passed", "failed", "unknown"] {
            assert!(
                counts["behavior"][field].is_u64(),
                "counts.behavior.{field}"
            );
        }
        for field in [
            "item_id",
            "machine_id",
            "class",
            "state",
            "mandatory",
            "reached",
        ] {
            assert!(json["items"][0].get(field).is_some(), "items[].{field}");
        }
        for field in ["item_id", "machine_id", "zoom", "stage", "caption"] {
            assert!(json["steps"][0].get(field).is_some(), "steps[].{field}");
        }
        assert!(json["steps"][0]["stage"][0]["label"].is_string());
        assert!(json["steps"][0]["stage"][0]["mark"].is_object());
        assert_eq!(json["mode"], "directed");
        assert_eq!(json["model"], "muse-spark");
        let plain = play(PLAIN).expect("the plain fixture plays");
        let plain = serde_json::to_value(plain).expect("the plain play serializes");
        assert_eq!(plain["mode"], "deterministic");
        assert!(plain["model"].is_null());
    }

    /// The plain fixture's JSON with `edit` applied, for the tests of the built order.
    fn plain_edited(edit: impl FnOnce(&mut Value)) -> String {
        let mut value: Value = serde_json::from_str(PLAIN).expect("the fixture is JSON");
        edit(&mut value);
        value.to_string()
    }

    /// The item of the plain fixture named `item_id`, for editing.
    fn plain_item<'a>(value: &'a mut Value, item_id: &str) -> &'a mut Value {
        value["items"]
            .as_array_mut()
            .expect("the fixture has items")
            .iter_mut()
            .find(|item| item["item_id"] == item_id)
            .expect("the fixture has the item")
    }

    #[test]
    fn an_open_out_of_scope_item_is_captioned_as_changed_not_as_stopped() {
        let json = plain_edited(|value| {
            plain_item(value, "d-links-clicks")["state"] = "open".into();
        });
        let play = play(&json).expect("the changed-value caption fits an open item");
        let caption = &step(&play, "d-links-clicks").caption;
        assert_eq!(
            caption,
            "Classified: out of scope. Base value here: 0. Candidate value here: no value."
        );
        assert!(!caption.contains("Recorded state"), "{caption}");
    }

    #[test]
    fn an_authorized_item_with_no_check_is_captioned_as_unspecified() {
        let json = plain_edited(|value| {
            plain_item(value, "d-links-clicks")["class"] = "authorized_but_unspecified".into();
            let by_class = &mut value["counts"]["by_class"];
            let out = by_class["out_of_scope"].as_u64().expect("a count");
            by_class["out_of_scope"] = (out - 1).into();
            let authorized = by_class["authorized_but_unspecified"]
                .as_u64()
                .expect("a count");
            by_class["authorized_but_unspecified"] = (authorized + 1).into();
        });
        let play = play(&json).expect("the unspecified caption fits the item");
        let caption = &step(&play, "d-links-clicks").caption;
        assert_eq!(
            caption,
            "Classified: authorized but unspecified. Base value here: 0. Candidate value here: no value."
        );
    }

    #[test]
    fn a_preserved_item_in_a_bounced_state_takes_the_held_caption() {
        let json = plain_edited(|value| {
            plain_item(value, "chk-health")["state"] = "bounced".into();
        });
        let play = play(&json).expect("the held caption fits a preserved item");
        assert_eq!(
            step(&play, "chk-health").caption,
            "Classified: preserved. Must-keep check: held. Another item on the same route is bounced."
        );
    }

    #[test]
    fn a_mandatory_item_with_no_caption_refuses_the_walk_plainly_and_names_it() {
        // A fixed obligation failure is the one mandatory state the catalog leaves without a caption.
        let json = plain_edited(|value| {
            plain_item(value, "i-retry-after")["state"] = "fixed".into();
        });
        let err = play(&json).expect_err("no caption covers a fixed obligation failure");
        assert!(
            matches!(&err, PlayError::Unbindable { item_id } if item_id.to_string() == "i-retry-after"),
            "{err}"
        );
        let message = err.to_string();
        assert!(
            message.starts_with("This receipt cannot be walked: item i-retry-after"),
            "{message}"
        );
        assert!(
            message.ends_with("Every item is listed below."),
            "{message}"
        );
    }

    #[test]
    fn a_directed_tour_whose_bindings_are_not_what_admission_binds_is_refused() {
        let json = edited(|value| {
            value["tour"]["bindings"]
                .as_array_mut()
                .expect("the fixture carries bindings")
                .reverse();
        });
        let play = play(&json).expect("the built order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.contains("bindings differ"), "{}", play.note);
    }

    #[test]
    fn a_directed_tour_without_its_bindings_is_refused_and_the_built_order_plays() {
        let json = edited(|value| {
            value["tour"]
                .as_object_mut()
                .expect("the fixture has a tour")
                .remove("bindings");
        });
        let play = play(&json).expect("the built order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.contains("could not be read"), "{}", play.note);
    }

    /// The directed fixture with its tour replaced by `tour`, one of the spec's invalid tours.
    fn with_tour(tour: &str) -> String {
        let mut value: Value = serde_json::from_str(DIRECTED).expect("the fixture is JSON");
        value["tour"]["tour"] = serde_json::from_str(tour).expect("the invalid tour is JSON");
        value.to_string()
    }

    #[test]
    fn the_four_invalid_tours_from_the_spec_are_refused_and_the_built_order_plays() {
        let cases = [
            (
                include_str!("../../acan-intent/fixtures/invalid-tour-missing-mandatory.json"),
                "is not reached",
            ),
            (
                include_str!("../../acan-intent/fixtures/invalid-tour-unknown-ref.json"),
                "names no evidence row",
            ),
            (
                include_str!("../../acan-intent/fixtures/invalid-tour-literal-caption.json"),
                "could not be read",
            ),
            (
                include_str!("../../acan-intent/fixtures/invalid-tour-swapped-base-candidate.json"),
                "another column",
            ),
        ];
        for (tour, reason) in cases {
            let play = play(&with_tour(tour)).expect("the built order plays");
            assert_eq!(play.mode, Mode::Deterministic, "{reason}");
            assert!(play.note.contains(reason), "{}", play.note);
            for item in play.items.iter().filter(|item| item.mandatory) {
                assert!(item.reached, "{} is mandatory and reached", item.item_id);
            }
        }
    }

    #[test]
    fn a_caption_whose_template_does_not_describe_its_item_is_refused() {
        let json = edited(|value| {
            let index = step_of(value, "d-links-sort");
            value["tour"]["tour"]["steps"][index]["caption"]["template_id"] =
                "stopped.out_of_scope".into();
        });
        let play = play(&json).expect("the built order plays");
        assert_eq!(play.mode, Mode::Deterministic);
        assert!(play.note.contains("does not describe"), "{}", play.note);
    }
}
