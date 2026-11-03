//! Contract tests: every fixture round-trips, unknown fields and versions are refused, the pass
//! rule holds over every class, and the fixtures bind each other. The fail-closed tests of the
//! pure stubs live beside the stubs, in the modules their owners edit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use acan_intent::admit::{
    self, AdmitError, MISSING_VALUE_TEXT, SlotMismatch, SlotPresence, StepFault, TourRejection,
};
use acan_intent::compare;
use acan_intent::fold;
use acan_intent::{
    AppContract, CaptionCatalog, CaptureOutcome, Card, CardRequest, CommitSha, Comparison,
    ContainerPolicy, ContentHash, Contest, ContestKind, ContestRequest, ContestResponse,
    ContestStatus, ContrastExample, CoreDerived, CorpusProbe, Coverage, DiscrepancyEpisode,
    EpisodeKey, Evidence, EvidenceRef, EvidenceRow, EvidenceSlot, FieldPattern, Fixture, GateState,
    GetReceiptRequest, GetReceiptResponse, GetRulingsRequest, GetRulingsResponse, GetScopeRequest,
    GetScopeResponse, ItemClass, ItemState, LedgerEvent, LedgerHash, LedgerHead, MaskablePath,
    McpError, McpErrorCode, NotReached, Observation, ObservationHash, ObservationPath, ObservedRun,
    Outcome, PlantView, PostReadbackRequest, PostReadbackResponse, ProbeCorpus, Receipt,
    RepairRound, RouteSource, RulingId, Run, RunManifest, RunOutcome, RunRole, Scope,
    ScopeConfirmation, ScopeDraft, ScopeItem, Selector, SlotSpec, Step, StepId, TourView,
    TwinError, TwinRunRequest, TwinRunResponse, TwinSaveRequest, TwinSaveResponse, Unsupported,
    ValueType, WindowMs, canonical_episode_selector,
};
use acan_intent::{FixtureSetManifest, ProbeSetManifest, RepoPath};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn raw(name: &str) -> Value {
    let path = fixtures().join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{name} is not JSON: {err}"))
}

fn load<T: DeserializeOwned>(name: &str) -> T {
    serde_json::from_value(raw(name)).unwrap_or_else(|err| panic!("decoding {name}: {err}"))
}

/// Decodes, encodes, decodes again, and checks both the values and the JSON are unchanged.
fn round_trip<T: DeserializeOwned + Serialize + PartialEq + std::fmt::Debug>(name: &str) {
    let original = raw(name);
    let first: T = serde_json::from_value(original.clone())
        .unwrap_or_else(|err| panic!("decoding {name}: {err}"));
    let encoded = serde_json::to_value(&first).expect("encoding");
    if let Some(pointer) = first_difference(&encoded, &original, String::new()) {
        panic!("{name} does not re-encode to the same JSON at '{pointer}'");
    }
    let second: T = serde_json::from_value(encoded).expect("decoding the re-encoded value");
    assert_eq!(first, second, "{name} changed across a round trip");
}

/// The JSON pointer of the first place two values differ.
fn first_difference(left: &Value, right: &Value, pointer: String) -> Option<String> {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            keys.into_iter()
                .find_map(|key| match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => first_difference(x, y, format!("{pointer}/{key}")),
                    _ => Some(format!("{pointer}/{key}")),
                })
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => a
            .iter()
            .zip(b)
            .enumerate()
            .find_map(|(index, (x, y))| first_difference(x, y, format!("{pointer}/{index}"))),
        _ if left == right => None,
        _ => Some(pointer),
    }
}

/// Objects whose keys are data, not field names.
const MAP_KEYS: [&str; 9] = [
    "slots",
    "typed_slot_bindings",
    "env",
    "vars",
    "headers",
    "cookies",
    "side_effects",
    "files",
    "effective_artifact_hashes",
];

/// Every JSON pointer to an object that is a struct, not a map or free JSON. Free JSON is a
/// body, a sink snapshot, or the value of present evidence.
fn struct_pointers(
    value: &Value,
    pointer: String,
    parent_key: Option<&str>,
    out: &mut Vec<String>,
) {
    match value {
        Value::Object(map) => {
            if !parent_key.is_some_and(|key| MAP_KEYS.contains(&key)) {
                out.push(pointer.clone());
            }
            let present = map.get("state") == Some(&json!("present"));
            for (key, child) in map {
                let free = key == "body"
                    || (key == "value" && present)
                    || parent_key == Some("side_effects");
                if free {
                    continue;
                }
                let escaped = key.replace('~', "~0").replace('/', "~1");
                struct_pointers(child, format!("{pointer}/{escaped}"), Some(key), out);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                struct_pointers(child, format!("{pointer}/{index}"), parent_key, out);
            }
        }
        _ => {}
    }
}

/// Inserts an unknown field into every struct of the fixture, one at a time, and checks each
/// insertion makes decoding fail.
fn refuses_unknown_fields<T: DeserializeOwned + std::fmt::Debug>(name: &str) {
    let original = raw(name);
    let mut pointers = Vec::new();
    struct_pointers(&original, String::new(), None, &mut pointers);
    assert!(!pointers.is_empty());
    for pointer in pointers {
        let mut tampered = original.clone();
        let target = tampered
            .pointer_mut(&pointer)
            .and_then(Value::as_object_mut)
            .expect("pointer names an object");
        target.insert("zz_unknown".to_owned(), json!("x"));
        let decoded = serde_json::from_value::<T>(tampered);
        assert!(
            decoded.is_err(),
            "{name} accepted an unknown field at '{pointer}'"
        );
    }
}

/// Replaces the top-level version tag with an unknown one and checks decoding fails.
fn refuses_unknown_version<T: DeserializeOwned + std::fmt::Debug>(name: &str) {
    let mut tampered = raw(name);
    let object = tampered.as_object_mut().expect("an envelope is an object");
    let key = ["version", "schema_version"]
        .into_iter()
        .find(|key| object.contains_key(*key))
        .unwrap_or_else(|| panic!("{name} has no version tag"));
    let current = object[key]
        .as_str()
        .expect("a version is a string")
        .to_owned();
    let (family, _) = current.split_once('/').expect("versions are family/n");
    object.insert(key.to_owned(), json!(format!("{family}/999")));
    let err = serde_json::from_value::<T>(tampered).expect_err("an unknown version decoded");
    assert!(
        err.to_string().contains("unsupported"),
        "{name}: unexpected error {err}"
    );
}

macro_rules! envelopes {
    ($($test:ident: $ty:ty = $file:literal;)+) => {$(
        mod $test {
            use super::*;

            #[test]
            fn round_trips() {
                round_trip::<$ty>($file);
            }

            #[test]
            fn refuses_unknown_fields() {
                super::refuses_unknown_fields::<$ty>($file);
            }

            #[test]
            fn refuses_unknown_version() {
                super::refuses_unknown_version::<$ty>($file);
            }
        }
    )+};
}

envelopes! {
    plant_view: PlantView = "plant-view-v1.json";
    plant_view_plain: PlantView = "plant-view-v1-plain.json";
    scope_draft: ScopeDraft = "scope-draft.json";
    scope: Scope = "scope.json";
    scope_confirmation: ScopeConfirmation = "scope-confirmation.json";
    ledger_decision: LedgerEvent = "ledger-decision.json";
    ledger_decision_dashboard: LedgerEvent = "ledger-decision-dashboard.json";
    ledger_decision_supersede: LedgerEvent = "ledger-decision-supersede.json";
    ledger_note: LedgerEvent = "ledger-note.json";
    ledger_probe_retired: LedgerEvent = "ledger-probe-retired.json";
    episode: DiscrepancyEpisode = "episode.json";
    contest: Contest = "contest.json";
    receipt_envelope: Receipt = "receipt.json";
    probe_corpus: ProbeCorpus = "probe-corpus.json";
    app_contract: AppContract = "app-contract.json";
    run: Run = "run.json";
    run_setup_failed: Run = "run-setup-failed.json";
    observation: Observation = "observation.json";
    observation_incomplete: Observation = "observation-incomplete.json";
    mcp_get_scope_request: GetScopeRequest = "mcp-get-scope-request.json";
    mcp_get_scope_response: GetScopeResponse = "mcp-get-scope-response.json";
    mcp_get_rulings_request: GetRulingsRequest = "mcp-get-rulings-request.json";
    mcp_get_rulings_response: GetRulingsResponse = "mcp-get-rulings-response.json";
    mcp_post_readback_request: PostReadbackRequest = "mcp-post-readback-request.json";
    mcp_post_readback_response: PostReadbackResponse = "mcp-post-readback-response.json";
    mcp_contest_request: ContestRequest = "mcp-contest-request.json";
    mcp_contest_response: ContestResponse = "mcp-contest-response.json";
    mcp_get_receipt_request: GetReceiptRequest = "mcp-get-receipt-request.json";
    mcp_get_receipt_response: GetReceiptResponse = "mcp-get-receipt-response.json";
    mcp_error_stale_scope: McpError = "mcp-error-stale-scope.json";
    mcp_error_forbidden: McpError = "mcp-error-forbidden.json";
    mcp_error_unsupported: McpError = "mcp-error-unsupported.json";
    mcp_error_scope_not_confirmed: McpError = "mcp-error-scope-not-confirmed.json";
    contrast_example: ContrastExample = "contrast-example.json";
    repair_round: RepairRound = "repair-round.json";
    twin_run_request: TwinRunRequest = "twin-run-request.json";
    twin_run_response: TwinRunResponse = "twin-run-response.json";
    twin_save_request: TwinSaveRequest = "twin-save-request.json";
    twin_save_response: TwinSaveResponse = "twin-save-response.json";
    twin_error_cap_exceeded: TwinError = "twin-error-cap-exceeded.json";
    card_request: CardRequest = "card-request.json";
    card: Card = "card.json";
    caption_catalog: CaptionCatalog = "caption-catalog.json";
}

/// Every fixture except the generated schema, by file name, sorted.
fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixtures())
        .expect("listing fixtures")
        .map(|entry| {
            entry
                .expect("a fixture entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".json") && !name.ends_with(".schema.json"))
        .collect();
    names.sort();
    names
}

#[test]
fn every_fixture_is_covered() {
    // A fixture is covered when a test file names it: a round trip here, or a tour test.
    let sources = [include_str!("contract.rs"), include_str!("tour.rs")];
    for name in fixture_names() {
        let named = format!("{name}\"");
        assert!(
            sources.iter().any(|source| source.contains(&named)),
            "{name} has no test"
        );
    }
}

#[test]
fn only_preserved_and_fulfilled_pass() {
    for class in ItemClass::ALL {
        let expected = matches!(class, ItemClass::Preserved | ItemClass::ObligationFulfilled);
        assert_eq!(class.counts_toward_pass(), expected, "{class:?}");
    }
    assert!(!ItemClass::AuthorizedButUnspecified.counts_toward_pass());
    let names: Vec<Value> = ItemClass::ALL
        .iter()
        .map(|class| serde_json::to_value(class).expect("encoding a class"))
        .collect();
    assert_eq!(
        names,
        [
            "preserved",
            "obligation_fulfilled",
            "obligation_failed",
            "authorized_but_unspecified",
            "out_of_scope",
            "violates_ruling",
            "unstable_base",
            "incomparable",
            "error"
        ]
        .map(Value::from)
    );
}

#[test]
fn plant_view_fixture_shows_every_mark() {
    let view: PlantView = load("plant-view-v1.json");
    for class in ItemClass::ALL {
        assert!(
            view.items.iter().any(|item| item.class == class),
            "no {class:?} item"
        );
        assert_eq!(view.counts.by_class.get(class), 1);
    }
    assert_eq!(
        usize::try_from(view.counts.by_class.total()).ok(),
        Some(view.items.len())
    );
    assert_eq!(view.check_counts(), Ok(()));
    for state in [
        ItemState::Open,
        ItemState::Bounced,
        ItemState::Fixed,
        ItemState::Residue,
        ItemState::Stopped,
    ] {
        assert!(
            view.items.iter().any(|item| item.state == state),
            "no {state:?} item"
        );
    }
    let tags: Vec<_> = view
        .machines
        .iter()
        .flat_map(|machine| &machine.rulings)
        .collect();
    assert!(tags.iter().any(|tag| tag.enforced));
    assert!(tags.iter().any(|tag| !tag.enforced));
    assert!(
        view.machines
            .iter()
            .any(|machine| machine.coverage == Coverage::Uncovered && machine.probes == 0)
    );
    assert!(
        view.items
            .iter()
            .any(|item| item.route_path.source == RouteSource::Static)
    );
    assert!(view.counts.masks_applied > 0);
    assert!(!view.scope.items.is_empty() && !view.scope.must_keep.is_empty());
    assert!(!view.scope.open.is_empty());
    let machines: Vec<&str> = view
        .machines
        .iter()
        .map(|m| m.machine_id.as_str())
        .collect();
    assert_eq!(
        machines,
        [
            "auth",
            "dashboard",
            "errors",
            "links",
            "middleware",
            "ratelimit",
            "server",
            "store",
            "test"
        ]
    );
    assert!(matches!(view.tour, Some(TourView::Directed { .. })));
}

#[test]
fn plain_fixture_is_the_same_view_without_a_tour() {
    let directed: PlantView = load("plant-view-v1.json");
    let plain: PlantView = load("plant-view-v1-plain.json");
    assert!(plain.tour.is_none());
    assert_eq!(
        PlantView {
            tour: None,
            ..directed
        },
        plain
    );
}

#[test]
fn views_and_tour_bind_the_receipt_fixture() {
    let receipt: Receipt = load("receipt.json");
    let hash = receipt.receipt_hash().expect("hashing the receipt");
    let view: PlantView = load("plant-view-v1.json");
    assert_eq!(view.receipt_hash, hash);
    let Some(TourView::Directed { tour, .. }) = &view.tour else {
        panic!("the fixture has a directed tour");
    };
    assert_eq!(tour.receipt_hash, hash);
    let response: GetReceiptResponse = load("mcp-get-receipt-response.json");
    assert_eq!(response.receipt_hash, hash);
    assert_eq!(response.receipt, receipt);
    assert_eq!(view.merged_sha, *receipt.merged_commit());
    assert_eq!(view.base_sha, *receipt.base_commit());
    assert_eq!(view.gate_state, GateState::Blocked);
}

#[test]
fn plant_view_schema_is_current() {
    let schema = schemars::schema_for!(PlantView);
    let generated = serde_json::to_string_pretty(&schema).expect("encoding the schema") + "\n";
    let path = fixtures().join("plant-view-v1.schema.json");
    if std::env::var_os("ACAN_BLESS").is_some() {
        std::fs::write(&path, &generated).expect("writing the schema");
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    let first_stale = committed
        .lines()
        .zip(generated.lines())
        .position(|(old, new)| old != new)
        .or_else(|| (committed.lines().count() != generated.lines().count()).then_some(0));
    assert!(
        first_stale.is_none(),
        "plant-view-v1.schema.json is stale from line {}; rerun with ACAN_BLESS=1",
        first_stale.map_or(0, |line| line + 1)
    );
}

#[test]
fn every_mandatory_item_is_reachable_in_the_directed_tour() {
    let view: PlantView = load("plant-view-v1.json");
    let Some(TourView::Directed { tour, .. }) = &view.tour else {
        panic!("the fixture has a directed tour");
    };
    let mandatory: Vec<&str> = view
        .mandatory_items()
        .map(|item| item.item_id.as_str())
        .collect();
    for id in ["d-links-clicks", "chk-links-delete", "chk-signup-store"] {
        assert!(mandatory.contains(&id), "{id} is not mandatory");
    }
    for item in view.mandatory_items() {
        assert!(
            tour.reaches(&item.item_id),
            "{} is unreachable",
            item.item_id
        );
    }
}

#[test]
fn every_caption_slot_resolves_to_a_row_its_template_accepts() {
    let view: PlantView = load("plant-view-v1.json");
    let catalog: CaptionCatalog = load("caption-catalog.json");
    assert_eq!(catalog.catalog_version, view.catalog_version);
    let Some(TourView::Directed { tour, .. }) = &view.tour else {
        panic!("the fixture has a directed tour");
    };
    for step in &tour.steps {
        let template = catalog
            .template(&step.caption.template_id)
            .unwrap_or_else(|| panic!("no template {}", step.caption.template_id));
        let bound: Vec<_> = step.caption.slots.keys().collect();
        let declared: Vec<_> = template.slots.keys().collect();
        assert_eq!(bound, declared, "{}", step.caption.template_id);
        for (slot, reference) in &step.caption.slots {
            let row = view
                .resolve(reference)
                .unwrap_or_else(|| panic!("{slot} does not resolve"));
            assert!(template.slots[slot].accepts(row), "{slot} is not accepted");
        }
        for reference in &step.evidence_refs {
            assert!(
                view.resolve(reference).is_some(),
                "an evidence ref does not resolve"
            );
        }
    }
}

#[test]
fn stale_swapped_and_hypothetical_bindings_are_refused() {
    let view: PlantView = load("plant-view-v1.json");
    let item = view
        .items
        .iter()
        .find(|item| {
            item.evidence
                .iter()
                .any(|row| row.slot == EvidenceSlot::BaseA)
        })
        .expect("an item with base evidence");
    let base = item
        .evidence
        .iter()
        .find(|row| row.slot == EvidenceSlot::BaseA)
        .expect("a base row");
    let reference = EvidenceRef {
        item_id: item.item_id.clone(),
        slot: base.slot,
        step: base.step.clone(),
        field: base.field.clone(),
    };
    assert_eq!(view.resolve(&reference), Some(base));
    let stale = EvidenceRef {
        item_id: "i-gone".parse().expect("an id"),
        ..reference.clone()
    };
    assert!(view.resolve(&stale).is_none());
    let candidate = SlotSpec {
        column: EvidenceSlot::Candidate,
        value_type: ValueType::Status,
        unit: None,
        presence: SlotPresence::Required,
    };
    assert!(
        !candidate.accepts(base),
        "a base row filled a candidate slot"
    );
    let target = SlotSpec {
        column: EvidenceSlot::Target,
        value_type: ValueType::Status,
        unit: None,
        presence: SlotPresence::Required,
    };
    let observed_target = EvidenceRow {
        slot: EvidenceSlot::Target,
        ..base.clone()
    };
    assert!(
        !target.accepts(&observed_target),
        "an observation filled a target slot"
    );
}

/// The directed fixture tour, after `edit` on its JSON steps, admitted over the fixture view.
fn admit_tour_with(
    catalog: &CaptionCatalog,
    edit: impl FnOnce(&mut Vec<Value>),
) -> Result<Vec<acan_intent::BoundItemView>, TourRejection> {
    let mut value = raw("plant-view-v1.json");
    edit(
        value["tour"]["tour"]["steps"]
            .as_array_mut()
            .expect("tour steps"),
    );
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");
    let Some(TourView::Directed { tour, .. }) = &view.tour else {
        panic!("the fixture has a directed tour");
    };
    admit::tour(&view, catalog, tour)
}

fn reference(item: &str, slot: &str, step: &str, field: &str) -> Value {
    json!({ "item_id": item, "slot": slot, "step": step, "field": field })
}

/// The index of the tour step about `item`.
fn step_about(steps: &[Value], item: &str) -> usize {
    steps
        .iter()
        .position(|step| step["item_id"] == item)
        .unwrap_or_else(|| panic!("no step about {item}"))
}

fn slot_fault(tour_step: usize, slot: &str, mismatch: SlotMismatch) -> TourRejection {
    TourRejection::Step {
        tour_step,
        fault: StepFault::Slot {
            slot: slot.parse().expect("a slot name"),
            mismatch,
        },
    }
}

#[test]
fn the_directed_fixture_tour_is_admitted_and_writes_missing_values() {
    let catalog: CaptionCatalog = load("caption-catalog.json");
    let bound = admit_tour_with(&catalog, |_| {}).expect("the fixture tour is admitted");
    let view: PlantView = load("plant-view-v1.json");
    let Some(TourView::Directed { tour, .. }) = &view.tour else {
        panic!("the fixture has a directed tour");
    };
    assert_eq!(bound.len(), tour.steps.iter().count());
    let slot_text = |item: &str, slot: &str| {
        let bound = bound
            .iter()
            .find(|view| view.item_id.as_str() == item)
            .unwrap_or_else(|| panic!("{item} is bound"));
        bound.typed_slot_bindings[&slot.parse().expect("a slot")].to_string()
    };
    assert_eq!(slot_text("d-links-clicks", "base"), "0");
    assert_eq!(slot_text("d-links-clicks", "candidate"), MISSING_VALUE_TEXT);
    assert_eq!(slot_text("i-retry-after", "target"), "60");
}

#[test]
fn a_cross_item_reference_rejects_the_whole_tour() {
    let catalog: CaptionCatalog = load("caption-catalog.json");
    for borrowed in [
        reference("d-login-error-body", "target", "s6", "body.error"),
        reference("i-login-limit", "target", "s6", "status"),
    ] {
        let mut at = 0;
        let rejected = admit_tour_with(&catalog, |steps| {
            at = step_about(steps, "i-retry-after");
            steps[at]["caption"]["slots"]["target"] = borrowed.clone();
        });
        assert!(
            matches!(
                rejected,
                Err(TourRejection::Step { tour_step, fault: StepFault::ForeignItem { .. } })
                    if tour_step == at
            ),
            "{borrowed}: {rejected:?}"
        );
    }
    let rejected = admit_tour_with(&catalog, |steps| {
        let at = step_about(steps, "d-links-sort");
        steps[at]["evidence_refs"][0] =
            reference("d-links-clicks", "candidate", "s4", "body.links[0].clicks");
    });
    assert!(
        matches!(
            rejected,
            Err(TourRejection::Step {
                fault: StepFault::ForeignItem { .. },
                ..
            })
        ),
        "{rejected:?}"
    );
}

#[test]
fn a_text_in_a_status_slot_rejects_the_whole_tour() {
    let catalog: CaptionCatalog = load("caption-catalog.json");
    let mut value = raw("plant-view-v1.json");
    let item = value["items"]
        .as_array_mut()
        .expect("items")
        .iter_mut()
        .find(|item| item["item_id"] == "chk-signup-store")
        .expect("the error item");
    let row = item["evidence"]
        .as_array_mut()
        .expect("evidence")
        .iter_mut()
        .find(|row| row["slot"] == "base_a")
        .expect("a base row");
    row["value"] = json!({ "state": "present", "value": "created" });
    let view: PlantView = serde_json::from_value(value).expect("decoding the view");
    let Some(TourView::Directed { tour, .. }) = &view.tour else {
        panic!("the fixture has a directed tour");
    };
    let at = tour
        .steps
        .iter()
        .position(|step| step.item_id.as_str() == "chk-signup-store")
        .expect("a step about the error item");
    assert_eq!(
        admit::tour(&view, &catalog, tour),
        Err(slot_fault(at, "base_a", SlotMismatch::Type))
    );
}

#[test]
fn a_caption_for_another_class_rejects_the_whole_tour() {
    let catalog: CaptionCatalog = load("caption-catalog.json");
    let mut at = 0;
    let rejected = admit_tour_with(&catalog, |steps| {
        at = step_about(steps, "d-login-error-body");
        steps[at]["caption"] = json!({
            "template_id": "unknown.error",
            "slots": { "base_a": reference("d-login-error-body", "base_a", "s6", "body.error") }
        });
    });
    assert_eq!(
        rejected,
        Err(TourRejection::Step {
            tour_step: at,
            fault: StepFault::TemplateDoesNotFit {
                template_id: "unknown.error".parse().expect("a template id"),
            },
        })
    );
}

#[test]
fn a_stale_swapped_or_missing_binding_rejects_the_whole_tour() {
    let catalog: CaptionCatalog = load("caption-catalog.json");
    let rejected = admit_tour_with(&catalog, |steps| {
        steps[0]["caption"]["slots"]["base"] =
            reference("d-login-error-body", "candidate", "s6", "body.error");
    });
    assert_eq!(
        rejected,
        Err(slot_fault(0, "base", SlotMismatch::Column)),
        "a candidate row filled a base slot"
    );
    let rejected = admit_tour_with(&catalog, |steps| {
        steps[0]["caption"]["slots"]["base"] =
            reference("d-login-error-body", "base_a", "s9", "body.error");
    });
    assert!(
        matches!(
            rejected,
            Err(TourRejection::Step {
                tour_step: 0,
                fault: StepFault::Unresolved { .. }
            })
        ),
        "{rejected:?}"
    );
    let mut value = raw("plant-view-v1.json");
    value["tour"]["tour"]["receipt_hash"] = json!(format!("sha256:{}", "0".repeat(64)));
    let view: PlantView = serde_json::from_value(value).expect("decoding");
    let Some(TourView::Directed { tour, .. }) = &view.tour else {
        panic!("the fixture has a directed tour");
    };
    assert!(matches!(
        admit::tour(&view, &catalog, tour),
        Err(TourRejection::StaleReceipt { .. })
    ));
    let mut strict = raw("caption-catalog.json");
    for template in strict["templates"].as_array_mut().expect("templates") {
        for spec in template["slots"]
            .as_object_mut()
            .expect("slots")
            .values_mut()
        {
            spec["presence"] = json!("required");
        }
    }
    let strict: CaptionCatalog = serde_json::from_value(strict).expect("decoding");
    let mut at = 0;
    let rejected = admit_tour_with(&strict, |steps| at = step_about(steps, "d-links-clicks"));
    assert_eq!(
        rejected,
        Err(slot_fault(at, "candidate", SlotMismatch::Missing))
    );
    let rejected = admit_tour_with(&catalog, |steps| {
        let at = step_about(steps, "chk-signup-store");
        steps.remove(at);
    });
    assert_eq!(
        rejected,
        Err(TourRejection::Unreached {
            item_id: "chk-signup-store".parse().expect("an id")
        })
    );
    let rejected = admit_tour_with(&catalog, |steps| {
        let at = step_about(steps, "i-login-limit");
        steps[at]["stage"] = json!([{ "mark": "bounced_chip", "item_id": "i-login-limit" }]);
    });
    assert!(
        matches!(
            rejected,
            Err(TourRejection::Step {
                fault: StepFault::UnknownMark { .. },
                ..
            })
        ),
        "a bounced chip on a fixed item: {rejected:?}"
    );
}

#[test]
fn a_slot_unit_its_type_is_never_shown_in_is_refused() {
    let spec = |value_type: &str, unit: Option<&str>| {
        let mut value =
            json!({ "column": "candidate", "value_type": value_type, "presence": "required" });
        if let Some(unit) = unit {
            value["unit"] = json!(unit);
        }
        serde_json::from_value::<SlotSpec>(value)
    };
    assert!(spec("duration", Some("ms")).is_ok());
    assert!(spec("duration", None).is_err());
    assert!(spec("count", Some("requests")).is_ok());
    assert!(spec("status", Some("ms")).is_err());
    assert!(spec("text", Some("bytes")).is_err());
    assert!(spec("text", None).is_ok());
}
#[test]
fn the_card_is_the_receipt_s_own_item_and_context() {
    let receipt: Receipt = load("receipt.json");
    let card: Card = load("card.json");
    let request: CardRequest = load("card-request.json");
    let hash = receipt.receipt_hash().expect("hashing the receipt");
    assert_eq!(card.receipt_hash, hash);
    assert_eq!(request.receipt_hash, hash);
    assert_eq!(request.item_id, card.item.item_id);
    let item = receipt
        .items
        .iter()
        .find(|item| item.item_id == card.item.item_id)
        .expect("the card's item is on the receipt");
    assert_eq!(&card.item, item);
    assert_eq!(card.prepared, receipt.prepared);
    assert_eq!(card.behavior, receipt.behavior);
    assert_eq!(card.counts, receipt.counts);
    let view: PlantView = load("plant-view-v1.json");
    assert_eq!(view.counts.behavior, receipt.behavior);
}

#[test]
fn an_unknown_plant_view_version_names_the_problem() {
    let mut value = raw("plant-view-v1.json");
    value["version"] = json!("plant-view/2");
    let err = serde_json::from_value::<PlantView>(value).expect_err("plant-view/2 decoded");
    assert!(err.to_string().contains("plant-view/2"), "{err}");
}

#[test]
fn status_is_a_channel_not_a_body_field() {
    let status: ObservationPath = "status".parse().expect("status");
    let body_status: ObservationPath = "body.status".parse().expect("body.status");
    assert_ne!(status.channel(), body_status.channel());
    assert!("status.code".parse::<ObservationPath>().is_err());
    assert!("body.links[*].code".parse::<ObservationPath>().is_err());
    assert!("status".parse::<MaskablePath>().is_err());
    assert!("body.links[*].code".parse::<MaskablePath>().is_ok());
    let observation: Observation = load("observation.json");
    assert_eq!(
        observation.lookup(&status),
        Evidence::Present { value: json!(429) }
    );
    assert_eq!(
        observation.lookup(&body_status),
        Evidence::Present {
            value: json!("limited")
        }
    );
}

#[test]
fn selectors_refuse_inconsistent_field_paths() {
    let base = json!({
        "surface": "http", "method": "POST", "route_template": "/api/login",
        "step_context": { "kind": "any" }, "principal": { "kind": "anonymous" }
    });
    let with = |channel: &str, field: Option<&str>| {
        let mut value = base.clone();
        value["channel"] = json!(channel);
        if let Some(field) = field {
            value["field_path"] = json!(field);
        }
        serde_json::from_value::<Selector>(value)
    };
    assert!(with("status", None).is_ok());
    assert!(with("status", Some("code")).is_err());
    assert!(with("any", Some("error")).is_err());
    assert!(with("header", Some("retry-after")).is_ok());
    assert!(with("header", Some("a.b")).is_err());
    assert!(with("side_effect", Some("store.users[*].name")).is_ok());
    assert!(with("side_effect", Some("users[*].name")).is_err());
    assert!(with("side_effect", None).is_err());
    assert!("links[*].code".parse::<FieldPattern>().is_ok());
}

#[test]
fn the_selector_schema_states_the_side_effect_path_rule() {
    let schema =
        serde_json::to_value(schemars::schema_for!(Selector)).expect("encoding the schema");
    let description = schema["properties"]["field_path"]["description"]
        .as_str()
        .unwrap_or_default();
    assert!(
        description.contains("`side_effect` path is required")
            && description.contains("`store` or `outbound_http`"),
        "the field_path description states the rule: {description:?}"
    );
}

#[test]
fn a_scope_item_without_acceptance_is_refused() {
    let scope: Scope = load("scope.json");
    let mut item = serde_json::to_value(&scope.body.may[0]).expect("encoding an item");
    item["acceptance"] = json!([]);
    assert!(serde_json::from_value::<ScopeItem>(item).is_err());
}

#[test]
fn evidence_rows_keep_hypotheticals_out_of_run_columns() {
    let observation = "sha256:84f0e02286696758fac47139e88e02483025d34bc3d23bcd11d0ea989cfe77f9";
    let row = |slot: &str, provenance: &str| {
        serde_json::from_value::<EvidenceRow>(json!({
            "slot": slot, "run_id": "run-1", "commit": "0123456789abcdef0123456789abcdef01234567",
            "observation_hash": observation, "step": "s1", "field": "status",
            "value": { "state": "present", "value": 200 }, "provenance": provenance
        }))
    };
    assert!(row("candidate", "observed").is_ok());
    assert!(row("candidate", "model_proposal").is_err());
    assert!(row("base_a", "human_spec").is_err());
    assert!(row("prediction", "observed").is_err());
    assert!(row("target", "human_spec").is_ok());
    let unanchored = json!({
        "slot": "candidate", "step": "s1", "field": "status",
        "value": { "state": "present", "value": 200 }, "provenance": "observed"
    });
    assert!(serde_json::from_value::<EvidenceRow>(unanchored.clone()).is_err());
    let mut no_observation = unanchored.clone();
    no_observation["run_id"] = json!("run-1");
    no_observation["commit"] = json!("0123456789abcdef0123456789abcdef01234567");
    assert!(serde_json::from_value::<EvidenceRow>(no_observation.clone()).is_err());
    no_observation["observation_hash"] = json!(observation);
    assert!(serde_json::from_value::<EvidenceRow>(no_observation).is_ok());
}

#[test]
fn bounded_numbers_refuse_out_of_range_values() {
    assert!(serde_json::from_value::<WindowMs>(json!(0)).is_err());
    assert!(serde_json::from_value::<WindowMs>(json!(86_400_001_u64)).is_err());
    assert!(serde_json::from_value::<WindowMs>(json!(60_000)).is_ok());
}

#[test]
fn episode_identity_ignores_probe_spelling() {
    let episode: DiscrepancyEpisode = load("episode.json");
    let respelled = DiscrepancyEpisode {
        probe_hash: episode.evidence_revisions[0].probe_hash,
        step_id: episode.evidence_revisions[0].step_id.clone(),
        ..episode.clone()
    };
    assert_ne!(respelled.probe_hash, episode.probe_hash);
    let id = episode.key().episode_id().expect("hashing the key");
    assert_eq!(respelled.key().episode_id().expect("hashing the key"), id);
}

#[test]
fn hashes_do_not_depend_on_field_order() {
    let scope: Scope = load("scope.json");
    let mut value = serde_json::to_value(&scope).expect("encoding");
    let reordered: serde_json::Map<String, Value> = value
        .as_object_mut()
        .expect("an object")
        .iter()
        .rev()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let reparsed: Scope = serde_json::from_value(Value::Object(reordered)).expect("decoding");
    assert_eq!(
        scope.scope_hash().expect("hashing"),
        reparsed.scope_hash().expect("hashing")
    );
    let shown = scope.scope_hash().expect("hashing").to_string();
    assert!(shown.starts_with("sha256:") && shown.len() == 71);
}

/// Every `scope_hash` a fixture names for the current revision. Older revisions in a
/// `scope_lineage` and the per-revision `evidence_revisions` are history, not bindings.
fn bound_scope_hashes(value: &Value, current_rev: u64, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match key.as_str() {
                    "evidence_revisions" => {}
                    "scope_lineage" => {
                        for entry in child.as_array().into_iter().flatten() {
                            if entry.get("rev").and_then(Value::as_u64) == Some(current_rev) {
                                out.extend(
                                    entry
                                        .get("scope_hash")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                );
                            }
                        }
                    }
                    _ => {
                        if key == "scope_hash" {
                            out.extend(child.as_str().map(str::to_owned));
                        }
                        bound_scope_hashes(child, current_rev, out);
                    }
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                bound_scope_hashes(child, current_rev, out);
            }
        }
        _ => {}
    }
}

/// Every fixture that binds a scope binds `scope.json` by its hash. The one exception is order
/// o-1, whose scope has no fixture file: its decision and contrast example name it and nothing
/// else does.
#[test]
fn every_fixture_binds_the_scope_file_by_its_hash() {
    const ORDER_ONE_SCOPE: &str =
        "sha256:ab2d2883c994bc3d9c7555b4eeba14752aa8af8fd473544361960cfea13a03ba";
    let scope: Scope = load("scope.json");
    let expected = scope.scope_hash().expect("hashing the scope").to_string();
    let mut bound_to_expected = BTreeSet::new();
    let mut bound_to_order_one = BTreeSet::new();
    for name in fixture_names() {
        let mut bound = Vec::new();
        bound_scope_hashes(&raw(&name), u64::from(scope.rev.0), &mut bound);
        for hash in bound {
            if hash == expected {
                bound_to_expected.insert(name.clone());
            } else if hash == ORDER_ONE_SCOPE {
                bound_to_order_one.insert(name.clone());
            } else {
                panic!("{name} binds {hash}, which is neither scope.json nor the order o-1 scope");
            }
        }
    }
    for name in [
        "card.json",
        "episode.json",
        "ledger-decision-dashboard.json",
        "ledger-decision-supersede.json",
        "mcp-get-receipt-response.json",
        "mcp-get-scope-response.json",
        "receipt.json",
        "scope-confirmation.json",
    ] {
        assert!(bound_to_expected.contains(name), "{name} binds no scope");
    }
    assert_eq!(
        bound_to_order_one,
        BTreeSet::from([
            "contrast-example.json".to_owned(),
            "ledger-decision.json".to_owned()
        ])
    );
}

#[test]
fn the_unsupported_mcp_error_names_the_operation() {
    let built = McpError::unsupported(Unsupported("API01: get_scope"));
    let fixture: McpError = load("mcp-error-unsupported.json");
    assert_eq!(built, fixture);
    assert!(matches!(fixture.error, McpErrorCode::Unsupported { .. }));
}

#[test]
fn the_container_policy_round_trips_and_refuses_unknown_fields() {
    round_trip::<ContainerPolicy>("container-policy.json");
    refuses_unknown_fields::<ContainerPolicy>("container-policy.json");
}

/// The fixture is the policy the runtime runs under by default, so the two cannot drift apart: the
/// worker's test command and the contract name one image, one digest and one set of limits.
#[test]
fn the_fixture_is_the_default_run() {
    let fixture: ContainerPolicy = load("container-policy.json");
    assert_eq!(fixture, ContainerPolicy::default_run(), "fixture drifted");
    assert_eq!(
        fixture.image.digest.as_ref().map(ToString::to_string),
        Some("sha256:1d22548f177f4ff38e0c15baa3c2c46c6c2060ba2d1bc573bb4720b407548f1d".to_owned()),
        "the image is pinned by the digest the H01 spike ran"
    );
}

/// The environment and the probe harness are what the probe runner reads from the policy. The
/// fixture names exactly the variables the demo needs and no credential.
#[test]
fn the_fixture_names_only_the_variables_the_demo_needs() {
    let fixture: ContainerPolicy = load("container-policy.json");
    let names: Vec<&str> = fixture.environment.keys().map(String::as_str).collect();
    assert_eq!(names, vec!["HOME", "PORT"]);
    assert_eq!(fixture.mounts.harness.as_str(), "/harness");
    assert!(
        fixture.role_timeout_ms > fixture.timeout_ms,
        "a role container's limit is longer than one sequence's"
    );
}

#[test]
fn every_tested_context_ran_under_the_container_policy_fixture() {
    let policy: ContainerPolicy = load("container-policy.json");
    let hash = policy.sandbox_policy_hash().expect("hashing the policy");
    let receipt: Receipt = load("receipt.json");
    let card: Card = load("card.json");
    let example: ContrastExample = load("contrast-example.json");
    for (name, context) in [
        ("receipt.json", &receipt.prepared.context),
        ("card.json", &card.prepared.context),
        ("contrast-example.json", &example.context),
    ] {
        assert_eq!(
            context.sandbox_policy_hash, hash,
            "{name} ran under another policy"
        );
    }
    let mut opened = policy.clone();
    opened.mounts.readonly_git = false;
    let mut current = receipt.prepared.context.clone();
    current.sandbox_policy_hash = opened.sandbox_policy_hash().expect("hashing");
    assert_ne!(
        current, receipt.prepared.context,
        "a policy change kept the context"
    );
}

/// Every field of the receipt's prepared block is what the fixtures compute for it. The probe set
/// and the fixture set come from the corpus. The ledger head and the retired probes come from the
/// ruling log folded to the revision the receipt names. The scope comes from `scope.json`, the
/// lockfile, runner and runtime from the run record and the app contract, and the evaluator from
/// this crate. The overlay and the controlled environment have no fixture that recomputes them,
/// so they must agree with the other fixtures that carry them.
#[test]
fn the_receipt_prepared_context_is_what_the_fixtures_compute() {
    let receipt: Receipt = load("receipt.json");
    let prepared = &receipt.prepared;
    let context = &prepared.context;
    let scope: Scope = load("scope.json");
    let confirmation: ScopeConfirmation = load("scope-confirmation.json");
    let run: Run = load("run.json");
    let app: AppContract = load("app-contract.json");
    let corpus: ProbeCorpus = load("probe-corpus.json");

    // The receipt was built at the revision it names, the prefix of the log through that event.
    let events: Vec<LedgerEvent> = LEDGER.into_iter().map(load::<LedgerEvent>).collect();
    let revision = usize::try_from(context.ledger_head.event_seq).expect("a small revision");
    assert!(
        revision <= events.len(),
        "the receipt names revision {revision}, past the {} events of the log",
        events.len()
    );
    let folded = fold::fold(&events[..revision]).expect("the log folds to the receipt's revision");
    let at = folded.value();
    assert_eq!(context.ledger_head, at.head, "ledger head");

    assert_eq!(prepared.base_commit, scope.base_commit, "base commit");
    assert_eq!(
        prepared.base_commit, confirmation.base_commit,
        "base commit"
    );
    assert_eq!(
        context.protected_head, prepared.base_commit,
        "protected head"
    );
    assert_eq!(context.merged_commit, run.commit, "merged commit");

    assert_eq!(context.scope_hash, confirmation.scope_hash, "scope hash");
    assert_eq!(
        context.scope_hash,
        scope.scope_hash().expect("hashing the scope"),
        "scope hash"
    );

    let probe_set = ProbeSetManifest::of(&corpus, &at.retired_probes)
        .and_then(|manifest| manifest.probe_set_hash())
        .expect("the probe set hashes");
    assert_eq!(context.probe_set_hash, probe_set, "probe set hash");
    let fixture_set = FixtureSetManifest::of(&corpus)
        .and_then(|manifest| manifest.fixture_set_hash())
        .expect("the fixture set hashes");
    assert_eq!(context.fixture_set_hash, fixture_set, "fixture set hash");

    let bun_lock: RepoPath = "bun.lock".parse().expect("a repo path");
    assert_eq!(
        context.effective_artifact_hashes.keys().collect::<Vec<_>>(),
        vec![&bun_lock],
        "the artifact inputs are the lockfile the run recorded"
    );
    assert_eq!(
        context.effective_artifact_hashes.get(&bun_lock),
        Some(&run.env.lockfile_hash),
        "lockfile hash"
    );

    assert_eq!(context.runner_hash, run.runner, "runner hash");
    assert_eq!(context.runtime, app.runtime, "runtime");
    assert_eq!(context.runtime, run.env.runtime, "runtime");
    assert_eq!(
        context.evaluator_version.to_string(),
        format!("acan-intent/{}", env!("CARGO_PKG_VERSION")),
        "evaluator version"
    );

    let policy: ContainerPolicy = load("container-policy.json");
    assert_eq!(
        context.sandbox_policy_hash,
        policy.sandbox_policy_hash().expect("hashing the policy"),
        "sandbox policy hash"
    );

    let card: Card = load("card.json");
    let example: ContrastExample = load("contrast-example.json");
    for (name, other) in [
        ("card.json", &card.prepared.context),
        ("contrast-example.json", &example.context),
    ] {
        assert_eq!(context.overlay_hash, other.overlay_hash, "{name} overlay");
        assert_eq!(
            context.controlled_env_hash, other.controlled_env_hash,
            "{name} controlled environment"
        );
        assert_eq!(context.runner_hash, other.runner_hash, "{name} runner hash");
        assert_eq!(context.runtime, other.runtime, "{name} runtime");
        assert_eq!(
            context.evaluator_version, other.evaluator_version,
            "{name} evaluator version"
        );
        assert_eq!(
            context.fixture_set_hash, other.fixture_set_hash,
            "{name} fixture set hash"
        );
        assert_eq!(
            context.sandbox_policy_hash, other.sandbox_policy_hash,
            "{name} sandbox policy hash"
        );
    }
}

/// The demo's ruling log, one fixture per event, in log order.
const LEDGER: [&str; 5] = [
    "ledger-decision.json",
    "ledger-note.json",
    "ledger-decision-dashboard.json",
    "ledger-decision-supersede.json",
    "ledger-probe-retired.json",
];

/// What one fixture says about one event of the demo's ruling log.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Claim {
    /// The chain hash through the event.
    Hash(LedgerHash),
    /// The ruling lineage the event belongs to, or `None` for an event outside every lineage.
    Ruling(Option<RulingId>),
}

type Claims = BTreeMap<u64, BTreeMap<Claim, BTreeSet<String>>>;

fn claim(claims: &mut Claims, seq: u64, what: Claim, name: &str) {
    claims
        .entry(seq)
        .or_default()
        .entry(what)
        .or_default()
        .insert(name.to_owned());
}

/// Records every ledger event, ledger head and ruling reference in `value`.
fn ledger_claims(name: &str, value: &Value, claims: &mut Claims) {
    if let Ok(event) = serde_json::from_value::<LedgerEvent>(value.clone()) {
        let seq = event.event_seq();
        let hash = event.chain_hash().expect("hashing an event");
        claim(claims, seq, Claim::Hash(hash), name);
        if let Some(prev) = event.prev_hash() {
            let before = seq
                .checked_sub(1)
                .unwrap_or_else(|| panic!("{name} is event 0 and names an event before it"));
            claim(claims, before, Claim::Hash(prev), name);
        }
        let (ruling, supersedes) = match &event {
            LedgerEvent::DecisionConfirmed(decision) => {
                (Some(decision.ruling_id.clone()), decision.supersedes)
            }
            LedgerEvent::NoteRecorded(note) => (Some(note.ruling_id.clone()), note.supersedes),
            LedgerEvent::ProbeRetired(_) => (None, None),
        };
        if let Some(replaced) = supersedes {
            assert!(replaced.event_seq < seq, "{name} supersedes a later event");
            claim(
                claims,
                replaced.event_seq,
                Claim::Ruling(ruling.clone()),
                name,
            );
        }
        claim(claims, seq, Claim::Ruling(ruling), name);
    }
    match value {
        Value::Object(map) => {
            if let Some(head) = map.get("ledger_head") {
                let head: LedgerHead = serde_json::from_value(head.clone()).expect("a ledger head");
                assert_eq!(
                    head.hash.is_some(),
                    head.event_seq > 0,
                    "{name} has a ledger head whose hash does not fit its sequence"
                );
                if let Some(hash) = head.hash {
                    claim(claims, head.event_seq, Claim::Hash(hash), name);
                }
            }
            if let (Some(Value::String(ruling)), Some(seq)) = (
                map.get("ruling_id"),
                map.get("event_seq").and_then(Value::as_u64),
            ) {
                let ruling = ruling.parse().expect("a ruling id");
                claim(claims, seq, Claim::Ruling(Some(ruling)), name);
            }
            map.values()
                .for_each(|child| ledger_claims(name, child, claims));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|child| ledger_claims(name, child, claims)),
        _ => {}
    }
}

#[test]
fn the_ledger_fixtures_are_one_chain() {
    let mut prev = None;
    for (name, seq) in LEDGER.into_iter().zip(1_u64..) {
        let event: LedgerEvent = load(name);
        assert_eq!(event.event_seq(), seq, "{name} is out of sequence");
        assert_eq!(
            event.prev_hash(),
            prev,
            "{name} does not chain to the event before it"
        );
        prev = Some(event.chain_hash().expect("hashing an event"));
    }
}

/// Every claimed event that is outside the fixture ledger, or that two claims make into two
/// different events.
fn ledger_problems(claims: &Claims) -> Vec<String> {
    let last = u64::try_from(LEDGER.len()).expect("a small ledger");
    claims
        .iter()
        .filter_map(|(seq, said)| {
            let hashes = said.keys().filter(|c| matches!(c, Claim::Hash(_))).count();
            let rulings = said
                .keys()
                .filter(|c| matches!(c, Claim::Ruling(_)))
                .count();
            if !(1..=last).contains(seq) {
                Some(format!(
                    "event {seq} is not in the fixture ledger: {said:?}"
                ))
            } else if hashes > 1 || rulings > 1 {
                Some(format!("event {seq} is two different events: {said:?}"))
            } else {
                None
            }
        })
        .collect()
}

/// Every fixture describes the one demo repository, so all of them claim the same ruling log:
/// a sequence number names one event, with one chain hash and one ruling lineage.
#[test]
fn fixtures_that_name_a_ledger_event_agree_on_it() {
    let mut claims = Claims::new();
    for name in fixture_names() {
        ledger_claims(&name, &raw(&name), &mut claims);
    }
    let problems = ledger_problems(&claims);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_reused_event_sequence_is_caught() {
    let mut claims = Claims::new();
    ledger_claims("receipt.json", &raw("receipt.json"), &mut claims);
    let mut reused = raw("ledger-decision-supersede.json");
    reused["event_seq"] = json!(3);
    ledger_claims("reused", &reused, &mut claims);
    let problems = ledger_problems(&claims);
    assert!(
        problems
            .iter()
            .any(|problem| problem.starts_with("event 3 is two different events")),
        "{problems:?}"
    );
}

fn reencoded<T: DeserializeOwned + Serialize>(value: Value) -> (T, Value) {
    let decoded: T = serde_json::from_value(value).expect("decoding");
    let encoded = serde_json::to_value(&decoded).expect("encoding");
    (decoded, encoded)
}

/// The fixture with the body of its first HTTP step, under `steps`, set to `body` or removed.
fn with_first_body(name: &str, steps: &str, body: Option<Value>) -> Value {
    let mut value = raw(name);
    let request = value
        .pointer_mut(&format!("{steps}/0/request"))
        .and_then(Value::as_object_mut)
        .expect("the first step is an HTTP step");
    match body {
        Some(body) => request.insert("body".to_owned(), body),
        None => request.remove("body"),
    };
    value
}

fn first_body<'a>(mut steps: impl Iterator<Item = &'a Step>) -> Option<&'a Value> {
    match steps.next() {
        Some(Step::Http(step)) => step.request.body.as_ref(),
        _ => panic!("the first step is an HTTP step"),
    }
}

#[test]
fn a_run_response_keeps_a_null_request_body_apart_from_none() {
    let pointer = "/steps/0/request/body";
    let run = |body| {
        reencoded::<TwinRunResponse>(with_first_body("twin-run-response.json", "/steps", body))
    };
    let (null, encoded) = run(Some(Value::Null));
    assert_eq!(first_body(null.steps.iter()), Some(&Value::Null));
    assert_eq!(
        encoded.pointer(pointer),
        Some(&Value::Null),
        "null was dropped"
    );
    let (absent, encoded) = run(None);
    assert_eq!(first_body(absent.steps.iter()), None);
    assert_eq!(encoded.pointer(pointer), None, "no body became a body");
    assert_ne!(null, absent);
}

#[test]
fn a_saved_null_body_probe_reloads_with_the_same_spec_hash() {
    let saved = |body| {
        let value = with_first_body("twin-save-response.json", "/probe/spec/steps", body);
        let (first, encoded) = reencoded::<TwinSaveResponse>(value);
        let (reloaded, _) = reencoded::<TwinSaveResponse>(encoded);
        let hash = first.probe.spec.probe_hash().expect("hashing");
        assert_eq!(
            reloaded.probe.spec.probe_hash().expect("hashing"),
            hash,
            "the spec hash moved across a save and reload"
        );
        assert_eq!(reloaded, first);
        (hash, first_body(reloaded.probe.spec.steps.iter()).cloned())
    };
    let (null_hash, null_body) = saved(Some(Value::Null));
    let (absent_hash, absent_body) = saved(None);
    assert_eq!(null_body, Some(Value::Null));
    assert_eq!(absent_body, None);
    assert_ne!(
        null_hash, absent_hash,
        "a null body and no body are one spec"
    );
}

/// One stored observation per HTTP and side-effect step of `run`'s sequence, except `skip`.
fn observations_for(
    probe: &CorpusProbe,
    fixture: &Fixture,
    run: &Run,
    skip: &[&str],
) -> Vec<(ObservationHash, Observation)> {
    fixture
        .recipe
        .iter()
        .chain(probe.spec.steps.iter())
        .filter(|step| matches!(step, Step::Http(_) | Step::ObserveSideEffect(_)))
        .filter(|step| !skip.contains(&step.id().as_str()))
        .map(|step| {
            let mut observation: Observation = load("observation.json");
            observation.run_id = run.id.clone();
            observation.commit = run.commit.clone();
            observation.probe = run.probe;
            observation.step_id = step.id().clone();
            (
                observation.observation_hash().expect("hashing"),
                observation,
            )
        })
        .collect()
}

fn step(id: &str) -> StepId {
    id.parse().expect("a step id")
}

/// The shipped example's stored observations, in the order its runs list them. Each record
/// carries the response body or cookie its capture reads.
fn stored_example_observations() -> Vec<Observation> {
    load("contrast-example-observations.json")
}

/// The stored observations of one run, hashed, in sequence order, as admission takes them.
fn stored_for(run: &Run) -> Vec<(ObservationHash, Observation)> {
    stored_example_observations()
        .into_iter()
        .filter(|observation| observation.run_id == run.id)
        .map(|observation| {
            (
                observation.observation_hash().expect("hashing"),
                observation,
            )
        })
        .collect()
}

#[test]
fn every_run_of_the_contrast_example_is_admitted_with_its_listed_observations() {
    let example: ContrastExample = load("contrast-example.json");
    let listed: usize = [&example.base_a, &example.base_b, &example.candidate]
        .iter()
        .map(|manifest| manifest.observations.len())
        .sum();
    assert_eq!(
        stored_example_observations().len(),
        listed,
        "the store holds an observation no run lists, or misses one a run lists"
    );
    for (role, manifest) in [
        ("base_a", &example.base_a),
        ("base_b", &example.base_b),
        ("candidate", &example.candidate),
    ] {
        let stored = stored_for(&manifest.run);
        let hashes: Vec<ObservationHash> = stored.iter().map(|(hash, _)| *hash).collect();
        assert_eq!(
            hashes, manifest.observations,
            "{role} lists other observations than the store holds, in order"
        );
        assert_eq!(
            stored.len(),
            observations_for(&example.probe, &example.fixture, &manifest.run, &[]).len(),
            "{role} lists another number of observations than its HTTP and sink steps"
        );
        let admitted = admit::observed_run(
            &example.probe,
            &example.fixture,
            manifest.run.clone(),
            stored,
        )
        .unwrap_or_else(|err| panic!("{role} was refused: {err}"));
        assert_eq!(admitted.observations().count(), manifest.observations.len());
    }
}

#[test]
fn the_stored_example_observations_round_trip_and_refuse_unknown_fields() {
    round_trip::<Vec<Observation>>("contrast-example-observations.json");
    refuses_unknown_fields::<Vec<Observation>>("contrast-example-observations.json");
}

#[test]
fn the_shipped_contrast_example_compares_end_to_end() {
    let example: ContrastExample = load("contrast-example.json");
    let runs = [&example.base_a, &example.base_b, &example.candidate].map(|manifest| {
        admit::observed_run(
            &example.probe,
            &example.fixture,
            manifest.run.clone(),
            stored_for(&manifest.run),
        )
        .unwrap_or_else(|err| panic!("{} was refused: {err}", manifest.run.id))
    });
    let contrast = contrast_over(&example, &runs);
    compare::check_contrast(&contrast).expect("the three runs are comparable");
    let masks = compare::derive_masks(&contrast).expect("the bases derive their masks");
    assert!(masks.is_empty(), "the bases agree outside the captures");
    let comparison = compare::compare(&contrast, &masks).expect("the contrast compares");
    compare::verify_comparison(&contrast, &comparison).expect("the comparison is recomputed");
    let Outcome::Changed { differences } = &comparison.value().outcome else {
        panic!(
            "the candidate drops a field: {:?}",
            comparison.value().outcome
        );
    };
    let found: Vec<(&str, &str)> = differences
        .iter()
        .map(|difference| (difference.step_id.as_str(), difference.path.as_str()))
        .collect();
    assert_eq!(found, [("s4", "body.links[0].clicks")]);
    admit::retained_example(&example, contrast, &comparison)
        .expect("the example is admitted from its own runs and comparison");
}

/// The example's probe with a clock step and a store read appended, and a base run of it.
fn probe_with_a_sink() -> (CorpusProbe, Fixture, Run) {
    let mut value = raw("contrast-example.json");
    let steps = value["probe"]["spec"]["steps"]
        .as_array_mut()
        .expect("probe steps");
    steps.push(json!({ "step": "advance_time", "id": "s5", "by_ms": 1000 }));
    steps.push(json!({
        "step": "observe_side_effect", "id": "s6", "sink": "store", "correlate": "s4",
        "barrier": { "timeout_ms": 500, "quiet_ms": 50 }
    }));
    let example: ContrastExample = serde_json::from_value(value).expect("decoding");
    let mut run = example.base_a.run.clone();
    run.probe = example.probe.spec.probe_hash().expect("hashing");
    (example.probe, example.fixture, run)
}

#[test]
fn a_completed_run_missing_an_http_or_sink_observation_is_refused() {
    let example: ContrastExample = load("contrast-example.json");
    let (probe, fixture, run) = (&example.probe, &example.fixture, &example.base_a.run);
    let admit = |probe: &CorpusProbe, run: &Run, skip: &[&str]| {
        let stored = observations_for(probe, fixture, run, skip);
        admit::observed_run(probe, fixture, run.clone(), stored).map(|_| ())
    };
    for missing in ["f1", "s3", "s4"] {
        assert_eq!(
            admit(probe, run, &[missing]),
            Err(AdmitError::Unrecorded {
                step_id: step(missing)
            }),
            "{missing}"
        );
    }
    let (with_sink, _, sink_run) = probe_with_a_sink();
    assert_eq!(admit(&with_sink, &sink_run, &[]), Ok(()));
    assert_eq!(
        admit(&with_sink, &sink_run, &["s6"]),
        Err(AdmitError::Unrecorded {
            step_id: step("s6")
        })
    );
    let mut no_capture = run.clone();
    no_capture.captures.pop();
    assert_eq!(
        admit(probe, &no_capture, &[]),
        Err(AdmitError::Unrecorded {
            step_id: step("s2")
        })
    );
}

#[test]
fn a_stopped_run_holds_records_only_up_to_its_stop() {
    let example: ContrastExample = load("contrast-example.json");
    let (probe, fixture) = (&example.probe, &example.fixture);
    let mut run = example.base_a.run.clone();
    run.outcome = serde_json::from_value(json!({ "outcome": "timed_out", "step_id": "s3" }))
        .expect("an outcome");
    let admit = |run: &Run, skip: &[&str]| {
        let stored = observations_for(probe, fixture, run, skip);
        admit::observed_run(probe, fixture, run.clone(), stored).map(|_| ())
    };
    assert_eq!(admit(&run, &["s3", "s4"]), Ok(()));
    assert_eq!(admit(&run, &["s4"]), Ok(()));
    assert_eq!(
        admit(&run, &[]),
        Err(AdmitError::AfterStop {
            step_id: step("s4")
        })
    );
    assert_eq!(
        admit(&run, &["s1", "s3", "s4"]),
        Err(AdmitError::Unrecorded {
            step_id: step("s1")
        })
    );
    run.outcome =
        serde_json::from_value(json!({ "outcome": "setup_failed", "detail": "not ready" }))
            .expect("an outcome");
    run.captures.clear();
    let everything: Vec<&str> = ["f1", "f2", "s1", "s3", "s4"].into();
    assert_eq!(admit(&run, &everything), Ok(()));
    assert_eq!(
        admit(&run, &everything[1..]),
        Err(AdmitError::AfterStop {
            step_id: step("f1")
        })
    );
}

#[test]
fn a_decision_rules_only_on_examples_of_its_own_order() {
    let example: ContrastExample = load("contrast-example.json");
    let hash = example
        .contrast_example_hash()
        .expect("hashing the example");
    let context = &example.context;
    assert_eq!(example.candidate.run.commit, context.merged_commit);
    for base in [&example.base_a.run, &example.base_b.run] {
        assert_eq!(base.commit, context.protected_head);
    }
    let mut ruled = 0;
    for name in LEDGER {
        let LedgerEvent::DecisionConfirmed(decision) = load(name) else {
            continue;
        };
        if decision.accepts.contains(&hash) || decision.rejects.contains(&hash) {
            ruled += 1;
            assert_eq!(
                context.scope_hash, decision.scope_hash,
                "{name} of order {} rules on an example of another order's scope",
                decision.order
            );
        }
    }
    assert!(ruled > 0, "no decision lists the example fixture");
}

#[test]
fn a_decision_rules_only_on_examples_retained_before_it() {
    let example: ContrastExample = load("contrast-example.json");
    let hash = example
        .contrast_example_hash()
        .expect("hashing the example");
    let mut ruled = 0;
    for name in LEDGER {
        let LedgerEvent::DecisionConfirmed(decision) = load(name) else {
            continue;
        };
        if decision.accepts.contains(&hash) || decision.rejects.contains(&hash) {
            ruled += 1;
            assert!(
                example.context.ledger_head.event_seq < decision.event_seq,
                "{name} rules on an example retained after it"
            );
        }
    }
    assert!(ruled > 0, "no decision lists the example fixture");
}

/// A retained example of the corpus's login-rate-limit probe, with its three runs admitted.
///
/// Each run logs in with its own session token, which the capture `alice_session` binds from
/// the cookie of step f2, so every capture reads the fingerprint its run recorded and the
/// comparison can be made. The context is the contrast example fixture's.
fn login_example() -> (ContrastExample, [ObservedRun; 3]) {
    let corpus: ProbeCorpus = load("probe-corpus.json");
    let probe = corpus
        .probes
        .iter()
        .find(|probe| probe.label == "login-rate-limit")
        .expect("the corpus has the login probe")
        .clone();
    let fixture = corpus
        .fixtures
        .iter()
        .find(|fixture| fixture.id == probe.spec.fixture)
        .expect("the probe's fixture is in the corpus")
        .clone();
    let mut example: ContrastExample = load("contrast-example.json");
    let probe_hash = probe.spec.probe_hash().expect("hashing");
    let fixture_hash = fixture.fixture_hash().expect("hashing");
    let run = |id: &str, role: RunRole, commit: CommitSha, token: &str| {
        let mut record: Run = load("run.json");
        record.id = id.parse().expect("a run id");
        record.probe = probe_hash;
        record.commit = commit;
        record.role = role;
        record.env.fixture_hash = fixture_hash;
        record.outcome = RunOutcome::Completed {};
        record.captures = vec![CaptureOutcome::Captured {
            binding: "alice_session".parse().expect("a binding"),
            step_id: step("f3"),
            value_hash: ContentHash::of(&Value::String(token.to_owned()))
                .expect("hashing")
                .into(),
        }];
        let mut stored = observations_for(&probe, &fixture, &record, &[]);
        for (hash, observation) in &mut stored {
            if observation.step_id == step("f2") {
                observation
                    .channels
                    .cookies
                    .insert("session".to_owned(), token.to_owned());
                *hash = observation.observation_hash().expect("hashing");
            }
        }
        let manifest = RunManifest {
            run: record.clone(),
            observations: stored.iter().map(|(hash, _)| *hash).collect(),
        };
        let admitted =
            admit::observed_run(&probe, &fixture, record, stored).expect("the run is admitted");
        (manifest, admitted)
    };
    let protected = example.context.protected_head.clone();
    let merged = example.context.merged_commit.clone();
    let (base_a, run_a) = run(
        "run-base-a",
        RunRole::BaseA,
        protected.clone(),
        "tok-base-a-0001",
    );
    let (base_b, run_b) = run("run-base-b", RunRole::BaseB, protected, "tok-base-b-0002");
    let (candidate, run_c) = run("run-candidate", RunRole::Candidate, merged, "tok-cand-0003");
    example.probe = probe;
    example.fixture = fixture;
    example.base_a = base_a;
    example.base_b = base_b;
    example.candidate = candidate;
    (example, [run_a, run_b, run_c])
}

/// The contrast of the example's probe and fixture over three admitted runs.
fn contrast_over<'a>(
    example: &'a ContrastExample,
    [base_a, base_b, candidate]: &'a [ObservedRun; 3],
) -> compare::Contrast<'a> {
    compare::Contrast {
        probe: &example.probe,
        fixture: &example.fixture,
        base_a,
        base_b,
        candidate,
    }
}

/// The comparison of a contrast under the masks its base runs derive.
fn comparison_of(contrast: &compare::Contrast<'_>) -> CoreDerived<Comparison> {
    let masks = compare::derive_masks(contrast).expect("the bases derive masks");
    compare::compare(contrast, &masks).expect("the contrast compares")
}

fn refused_as(
    manifest: &ContrastExample,
    contrast: compare::Contrast<'_>,
    comparison: &CoreDerived<Comparison>,
) -> Result<(), AdmitError> {
    admit::retained_example(manifest, contrast, comparison).map(|_| ())
}

#[test]
fn a_retained_example_is_admitted_from_its_own_runs_and_comparison() {
    let (example, runs) = login_example();
    let contrast = contrast_over(&example, &runs);
    let comparison = comparison_of(&contrast);
    let retained =
        admit::retained_example(&example, contrast, &comparison).expect("the example is admitted");
    assert_eq!(
        retained.hash(),
        example.contrast_example_hash().expect("hashing")
    );
    assert_eq!(retained.comparison(), &comparison);
    assert_eq!(
        retained.contrast().candidate,
        runs.last().expect("three runs")
    );
}

#[test]
fn crossed_evidence_is_refused_at_admission() {
    let (example, runs) = login_example();
    let contrast = contrast_over(&example, &runs);
    let comparison = comparison_of(&contrast);
    assert_eq!(refused_as(&example, contrast, &comparison), Ok(()));
    let crossed = |what| Err(AdmitError::CrossedEvidence { what });

    let mut other = example.clone();
    other.probe.label = "another-probe".to_owned();
    assert_eq!(refused_as(&other, contrast, &comparison), crossed("probe"));

    let mut other = example.clone();
    other.fixture.recipe.clear();
    assert_eq!(
        refused_as(&other, contrast, &comparison),
        crossed("fixture")
    );

    let mut fixture = example.fixture.clone();
    fixture.recipe.clear();
    let mut other = example.clone();
    other.fixture = fixture.clone();
    let runs_of_another_fixture = compare::Contrast {
        fixture: &fixture,
        ..contrast
    };
    assert_eq!(
        refused_as(&other, runs_of_another_fixture, &comparison),
        Err(AdmitError::WrongSpec)
    );

    let another_candidate = compare::Contrast {
        candidate: &runs[0],
        ..contrast
    };
    assert_eq!(
        refused_as(&example, another_candidate, &comparison),
        crossed("candidate run record")
    );

    let mut stored: Vec<(ObservationHash, Observation)> = runs[2]
        .observations()
        .map(|(hash, observation)| (*hash, observation.clone()))
        .collect();
    let mut value = serde_json::to_value(&stored[0].1).expect("encoding");
    value["channels"]["body"]["error"] = json!("another error");
    let edited: Observation = serde_json::from_value(value).expect("decoding");
    stored[0] = (edited.observation_hash().expect("hashing"), edited);
    let edited_candidate = admit::observed_run(
        &example.probe,
        &example.fixture,
        example.candidate.run.clone(),
        stored,
    )
    .expect("the edited candidate is still a complete run");
    let edited_contrast = compare::Contrast {
        candidate: &edited_candidate,
        ..contrast
    };
    assert_eq!(
        refused_as(&example, edited_contrast, &comparison),
        crossed("candidate observations")
    );

    let mut other = example.clone();
    other.base_b.observations.swap(0, 1);
    assert_eq!(
        refused_as(&other, contrast, &comparison),
        crossed("base B observations")
    );

    let mut other = example.clone();
    other.context.merged_commit = other.context.protected_head.clone();
    assert_eq!(
        refused_as(&other, contrast, &comparison),
        crossed("context")
    );

    let another_comparison = comparison_of(&edited_contrast);
    assert_ne!(
        another_comparison, comparison,
        "the edited candidate must change the comparison"
    );
    assert_eq!(
        refused_as(&example, contrast, &another_comparison),
        crossed("comparison")
    );
}

/// The scope fixture with one edit to its JSON, decoded again.
fn edited_scope(edit: impl FnOnce(&mut Value)) -> Scope {
    let mut value = raw("scope.json");
    edit(&mut value);
    serde_json::from_value(value).expect("the edited scope still decodes")
}

#[test]
fn a_predicate_or_change_kind_moves_the_semantic_scope_digest() {
    let scope: Scope = load("scope.json");
    let digest = scope.semantic_scope_hash().expect("hashing");
    let allowed = edited_scope(|v| {
        v["body"]["may"][0]["acceptance"][0]["predicate"]["policy"]["allowed_count"] = json!(6);
    });
    assert_ne!(allowed.semantic_scope_hash().expect("hashing"), digest);
    let changed = edited_scope(|v| v["body"]["may"][0]["change"] = json!("remove"));
    assert_ne!(changed.semantic_scope_hash().expect("hashing"), digest);
    let moved = edited_scope(|v| v["body"]["must_keep"][0]["machine_ids"] = json!(["plant-b"]));
    assert_ne!(moved.semantic_scope_hash().expect("hashing"), digest);
}

#[test]
fn prose_alone_does_not_move_the_semantic_scope_digest() {
    let scope: Scope = load("scope.json");
    let digest = scope.semantic_scope_hash().expect("hashing");
    let reworded = edited_scope(|v| {
        v["body"]["may"][0]["acceptance"][0]["text"] = json!("Reworded for the card");
        v["body"]["must_keep"][0]["text"] = json!("Reworded rule");
        v["body"]["open"][0]["question"] = json!("Reworded question?");
        v["body"]["open"][0]["options"][0]["label"] = json!("Reworded answer");
    });
    assert_eq!(reworded.semantic_scope_hash().expect("hashing"), digest);
    assert_ne!(
        reworded.scope_hash().expect("hashing"),
        scope.scope_hash().expect("hashing"),
        "the document digest still covers wording"
    );
}

#[test]
fn a_contest_rationale_does_not_move_the_semantic_receipt_identity() {
    let scope: Scope = load("scope.json");
    let receipt: Receipt = load("receipt.json");
    let identity = receipt.semantic_hash(&scope).expect("hashing");
    let mut reasoned = receipt.clone();
    reasoned.contests[0].rationale = "A different reason with the same claim.".to_owned();
    assert_ne!(
        reasoned.receipt_hash().expect("hashing"),
        receipt.receipt_hash().expect("hashing"),
        "the document identity still covers the rationale"
    );
    assert_eq!(reasoned.semantic_hash(&scope).expect("hashing"), identity);

    let mut retargeted = receipt.clone();
    retargeted.contests[0].status = ContestStatus::Rejected;
    assert_ne!(retargeted.semantic_hash(&scope).expect("hashing"), identity);
}

#[test]
fn every_other_contest_field_moves_the_semantic_receipt_identity() {
    let scope: Scope = load("scope.json");
    let receipt: Receipt = load("receipt.json");
    let identity = receipt.semantic_hash(&scope).expect("hashing");
    let moves = |edit: fn(&mut Contest)| {
        let mut changed = receipt.clone();
        edit(&mut changed.contests[0]);
        changed.semantic_hash(&scope).expect("hashing") != identity
    };
    assert!(
        moves(|c| c.id = "k-2".parse().expect("a contest id")),
        "the id"
    );
    assert!(
        moves(|c| c.target = ContentHash::of_bytes(b"another episode").into()),
        "the target"
    );
    assert!(moves(|c| c.kind = ContestKind::ProposedChange), "the kind");
    assert!(
        moves(|c| c.reproducer = ContentHash::of_bytes(b"another probe").into()),
        "the reproducer"
    );
}

#[test]
fn a_receipt_item_episode_id_is_the_hash_of_its_own_selector_and_order() {
    let scope: Scope = load("scope.json");
    let receipt: Receipt = load("receipt.json");
    let selector = canonical_episode_selector(&scope.body.may[0].selector);
    let episode = EpisodeKey {
        order: receipt.order.clone(),
        selector: selector.clone(),
        obligation_id: None,
    }
    .episode_id()
    .expect("hashing the key");
    let mut item = receipt.items[0].clone();
    item.episode_id = Some(episode);
    item.selector = Some(selector);
    let mut consistent = receipt.clone();
    consistent.items = vec![item];
    consistent
        .check_episode_ids()
        .expect("the item's episode id is the hash of its key");

    let mut crossed = consistent.clone();
    crossed.items[0].selector = Some(scope.body.may[1].selector.clone());
    assert!(
        crossed.check_episode_ids().is_err(),
        "an episode id under another selector is refused"
    );
    let mut reordered = consistent.clone();
    reordered.order = serde_json::from_value(json!("o-9")).expect("an order");
    assert!(
        reordered.check_episode_ids().is_err(),
        "an episode id under another order is refused"
    );
    let mut unkeyed = consistent.clone();
    unkeyed.items[0].selector = None;
    unkeyed
        .check_episode_ids()
        .expect("an item with no selector has no key to check");
}

#[test]
fn an_unreached_item_is_named_by_id_with_its_unchecked_obligations() {
    let scope: Scope = load("scope.json");
    let receipt: Receipt = load("receipt.json");
    assert!(
        serde_json::to_value(&receipt)
            .expect("encoding the receipt")
            .get("not_reached")
            .is_none(),
        "a receipt with nothing unreached keeps its bytes"
    );
    let item = &scope.body.may[1];
    let mut unreached = receipt.clone();
    unreached.not_reached.push(NotReached {
        item_id: item.id.clone(),
        selector: canonical_episode_selector(&item.selector),
        obligation_ids: item
            .acceptance
            .iter()
            .map(|p| p.obligation_id.clone())
            .collect(),
    });
    let value = serde_json::to_value(&unreached).expect("encoding the receipt");
    assert_eq!(
        value["not_reached"][0]["item_id"],
        serde_json::to_value(&item.id).expect("encoding the id")
    );
    assert_eq!(
        value["not_reached"][0]["obligation_ids"]
            .as_array()
            .map(Vec::len),
        Some(item.acceptance.iter().count())
    );
    assert_ne!(
        unreached.receipt_hash().expect("hashing"),
        receipt.receipt_hash().expect("hashing")
    );
    let decoded: Receipt = serde_json::from_value(value).expect("decoding the receipt");
    assert_eq!(decoded, unreached);
}

#[test]
fn value_hash_is_sha256_over_the_canonical_json_of_the_value() {
    use sha2::{Digest, Sha256};

    let file = raw("value-hash-vectors.json");
    let vectors = file["vectors"].as_array().expect("the vectors");
    assert!(vectors.len() >= 4, "a string, a number, an escape and f3");
    for vector in vectors {
        let name = vector["name"].as_str().expect("a vector name");
        let canonical = serde_jcs::to_vec(&vector["value"]).expect("the canonical JSON");
        assert_eq!(
            canonical,
            vector["canonical"]
                .as_str()
                .expect("the canonical form")
                .as_bytes(),
            "{name}: the canonical JSON"
        );
        let digest = format!("sha256:{:x}", Sha256::digest(&canonical));
        assert_eq!(
            digest,
            vector["value_hash"].as_str().expect("the value hash"),
            "{name}: the value hash"
        );
    }
}

#[test]
fn a_must_keep_selector_is_keyed_by_its_canonical_form() {
    let scope: Scope = load("scope.json");
    let canonical = canonical_episode_selector(&scope.body.must_keep[0].selector);
    let value = serde_json::to_value(&canonical).expect("encoding the selector");
    assert_eq!(value["step_context"], json!({ "kind": "any" }));
    assert_eq!(value["principal"], json!({ "kind": "authenticated" }));
    assert_eq!(
        canonical_episode_selector(&canonical),
        canonical,
        "canonicalizing twice changes nothing"
    );
}

#[test]
fn the_scope_confirmation_binds_the_scope_semantically() {
    let scope: Scope = load("scope.json");
    let confirmation: ScopeConfirmation = load("scope-confirmation.json");
    let expected = scope.semantic_scope_hash().expect("hashing");
    assert_eq!(
        confirmation.semantic_scope_hash,
        Some(expected),
        "scope-confirmation.json must bind the scope's semantic digest"
    );
}

#[test]
fn scope_wording_does_not_move_the_semantic_receipt_identity() {
    let scope: Scope = load("scope.json");
    let receipt: Receipt = load("receipt.json");
    let reworded = edited_scope(|v| {
        v["body"]["may"][0]["acceptance"][0]["text"] = json!("Reworded for the card");
    });
    assert_eq!(
        receipt.semantic_hash(&reworded).expect("hashing"),
        receipt.semantic_hash(&scope).expect("hashing")
    );
}
