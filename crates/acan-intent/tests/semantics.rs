//! The intent semantics, end to end: canonical comparison and masks, predicate evaluation over
//! admitted runs, the rate window's boundaries, and receipt construction.
//!
//! Every run is admitted through `admit::observed_run`, so the tests judge what the gate judges.
//! The runs are the login-rate-limit probe of the corpus: the recipe's signup and login, then six
//! failed logins and a correct one after the window.

use std::collections::BTreeMap;

use acan_intent::compare::{self, Contrast, RunContext};
use acan_intent::receipt::{self, ProbeResult, ReceiptInputs};
use acan_intent::{
    CaptureOutcome, Channels, CheckResult, Comparison, Completeness, ContentHash, CoreDerived,
    CorpusProbe, Fixture, FixtureSetManifest, FoldedLedger, GateState, HttpMethod,
    IncompleteReason, ItemClass, LedgerEvent, Mask, NonEmpty, Observation, ObservationHash,
    ObservedRun, Outcome, Predicate, PreparedCommit, ProbeCorpus, ProbeHash, ProbeSetManifest,
    Receipt, RequestRecord, RulingStatus, Run, RunOutcome, RunRole, Scope, ScopeConfirmation,
    UnknownReason, WindowObligation,
};
use acan_intent::{admit, fold, predicate};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const PROBE_CORPUS: &str = include_str!("../fixtures/probe-corpus.json");
const RUN: &str = include_str!("../fixtures/run.json");
const OBSERVATION: &str = include_str!("../fixtures/observation.json");
const SCOPE: &str = include_str!("../fixtures/scope.json");
const CONFIRMATION: &str = include_str!("../fixtures/scope-confirmation.json");
const RECEIPT: &str = include_str!("../fixtures/receipt.json");
const LEDGER_DECISION: &str = include_str!("../fixtures/ledger-decision.json");
/// The commit the base runs were taken at: the prepared base that scope.json confirms against.
const BASE: &str = "178c1b07097a9f510882d139061ed50b4b44b1d4";
/// The commit the candidate was merged to.
const MERGED: &str = "a27a5f30da19543ee34362516e29bf7561214156";
const SIX_FAILURES: [u16; 6] = [401; 6];
const FIVE_THEN_LIMITED: [u16; 6] = [401, 401, 401, 401, 401, 429];

fn parse<T: DeserializeOwned>(text: &str) -> T {
    serde_json::from_str(text).expect("a fixture decodes")
}

/// The login probe and its fixture, as the corpus holds them.
struct Login {
    probe: CorpusProbe,
    fixture: Fixture,
}

impl Login {
    fn load() -> Self {
        let corpus: ProbeCorpus = parse(PROBE_CORPUS);
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
        Self { probe, fixture }
    }
}

/// One HTTP step as the runner records it.
#[derive(Clone)]
struct Hit {
    step: &'static str,
    route: &'static str,
    client: &'static str,
    actor: Option<&'static str>,
    sent_at_ms: u64,
    status: u16,
    body: Value,
    cookies: Vec<(&'static str, String)>,
    incomplete: Option<IncompleteReason>,
}

impl Hit {
    /// A login from client-a to the login route, answered `status`, by no signed-in user.
    fn at(step: &'static str, status: u16, sent_at_ms: u64) -> Self {
        Self {
            step,
            route: "/api/login",
            client: "client-a",
            actor: None,
            sent_at_ms,
            status,
            body: body_for(status),
            cookies: Vec::new(),
            incomplete: None,
        }
    }

    fn observation(&self, run_id: &str, probe: ProbeHash, commit: &str) -> Observation {
        let mut observation: Observation = parse(OBSERVATION);
        observation.run_id = run_id.parse().expect("a run id");
        observation.commit = commit.parse().expect("a commit");
        observation.probe = probe;
        observation.step_id = self.step.parse().expect("a step id");
        observation.raw_ref = None;
        observation.request = Some(RequestRecord {
            method: HttpMethod::Post,
            route_template: self.route.parse().expect("a route template"),
            actor: self.actor.map(|name| name.parse().expect("an actor name")),
            client: self.client.parse().expect("a client"),
            sent_at_ms: self.sent_at_ms,
        });
        observation.completeness = match &self.incomplete {
            None => Completeness::Complete {},
            Some(reason) => Completeness::Incomplete {
                reasons: NonEmpty::new(reason.clone(), std::iter::empty()),
            },
        };
        observation.channels = Channels {
            status: Some(self.status),
            headers: if self.status == 429 {
                BTreeMap::from([("retry-after".to_owned(), "60".to_owned())])
            } else {
                BTreeMap::new()
            },
            cookies: self
                .cookies
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
            body: Some(self.body.clone()),
            side_effects: BTreeMap::new(),
        };
        observation
    }
}

fn body_for(status: u16) -> Value {
    match status {
        401 => json!({ "error": "invalid credentials" }),
        429 => json!({ "status": "limited", "error": "too many attempts" }),
        _ => json!({ "ok": true }),
    }
}

/// Six failed logins at one-second steps, then the correct login at 61 seconds.
fn sequence(statuses: [u16; 6]) -> Vec<Hit> {
    sequence_at(statuses, [1000, 2000, 3000, 4000, 5000, 6000])
}

fn sequence_at(statuses: [u16; 6], times: [u64; 6]) -> Vec<Hit> {
    let steps = ["s1", "s2", "s3", "s4", "s5", "s6"];
    let mut hits: Vec<Hit> = steps
        .iter()
        .zip(statuses)
        .zip(times)
        .map(|((step, status), at)| Hit::at(step, status, at))
        .collect();
    hits.push(Hit::at("s8", 200, 61_000));
    hits
}

fn hit_mut<'h>(hits: &'h mut [Hit], step: &str) -> &'h mut Hit {
    hits.iter_mut()
        .find(|hit| hit.step == step)
        .expect("the step is in the sequence")
}

fn with_cookie(mut hits: Vec<Hit>, step: &str, value: &str) -> Vec<Hit> {
    hit_mut(&mut hits, step)
        .cookies
        .push(("session", value.to_owned()));
    hits
}

/// Admits one run of the login probe. The recipe's signup and login come first, and the login
/// sets the session cookie `token`, which the capture `alice_session` binds. A base run is taken
/// at the prepared base and the candidate at the merged commit.
fn admitted(
    login: &Login,
    id: &str,
    role: RunRole,
    token: &str,
    hits: Vec<Hit>,
    outcome: RunOutcome,
) -> ObservedRun {
    let commit = if matches!(role, RunRole::Candidate) {
        MERGED
    } else {
        BASE
    };
    let probe = login.probe.spec.probe_hash().expect("the probe hashes");
    let signup = Hit {
        route: "/api/signup",
        ..Hit::at("f1", 201, 0)
    };
    let login_hit = Hit::at("f2", 200, 0);
    let login_hit = Hit {
        cookies: vec![("session", token.to_owned())],
        ..login_hit
    };
    let mut all = vec![signup, login_hit];
    all.extend(hits);
    let stored: Vec<(ObservationHash, Observation)> = all
        .iter()
        .map(|hit| {
            let observation = hit.observation(id, probe, commit);
            (
                observation
                    .observation_hash()
                    .expect("the observation hashes"),
                observation,
            )
        })
        .collect();
    let mut record: Run = parse(RUN);
    record.id = id.parse().expect("a run id");
    record.probe = probe;
    record.commit = commit.parse().expect("a commit");
    record.role = role;
    record.env.fixture_hash = login.fixture.fixture_hash().expect("the fixture hashes");
    record.outcome = outcome;
    record.captures = vec![CaptureOutcome::Captured {
        binding: "alice_session".parse().expect("a binding"),
        step_id: "f3".parse().expect("a step id"),
        value_hash: ContentHash::of(&Value::String(token.to_owned()))
            .expect("the value hashes")
            .into(),
    }];
    admit::observed_run(&login.probe, &login.fixture, record, stored).expect("the run is admitted")
}

/// Three admitted runs of the login probe: base A, base B and the candidate. Each run logs in
/// with its own session token, so captured identities differ across runs.
struct Trio {
    login: Login,
    base_a: ObservedRun,
    base_b: ObservedRun,
    candidate: ObservedRun,
}

impl Trio {
    fn new(base_a: Vec<Hit>, base_b: Vec<Hit>, candidate: Vec<Hit>) -> Self {
        let login = Login::load();
        let base_a = admitted(
            &login,
            "run-base-a",
            RunRole::BaseA,
            "tok-base-a-0001",
            base_a,
            RunOutcome::Completed {},
        );
        let base_b = admitted(
            &login,
            "run-base-b",
            RunRole::BaseB,
            "tok-base-b-0002",
            base_b,
            RunOutcome::Completed {},
        );
        let candidate = admitted(
            &login,
            "run-candidate",
            RunRole::Candidate,
            "tok-cand-0003",
            candidate,
            RunOutcome::Completed {},
        );
        Self {
            login,
            base_a,
            base_b,
            candidate,
        }
    }

    fn contrast(&self) -> Contrast<'_> {
        Contrast {
            probe: &self.login.probe,
            fixture: &self.login.fixture,
            base_a: &self.base_a,
            base_b: &self.base_b,
            candidate: &self.candidate,
        }
    }

    fn masks(&self) -> Vec<CoreDerived<Mask>> {
        compare::derive_masks(&self.contrast()).expect("the masks derive")
    }

    fn comparison(&self) -> CoreDerived<Comparison> {
        compare::compare(&self.contrast(), &self.masks()).expect("the comparison runs")
    }

    fn verdict(&self, predicate: &Predicate) -> CheckResult {
        predicate::evaluate(predicate, &self.contrast(), &self.comparison(), &[])
            .expect("the predicate evaluates")
    }
}

/// The differences of a changed comparison, as `(step, path)` pairs.
fn differences(comparison: &CoreDerived<Comparison>) -> Vec<(String, String)> {
    match &comparison.value().outcome {
        Outcome::Changed { differences } => differences
            .iter()
            .map(|difference| {
                (
                    difference.step_id.as_str().to_owned(),
                    difference.path.as_str().to_owned(),
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn scope() -> Scope {
    parse(SCOPE)
}

/// The predicate of the acceptance obligation at `index` of the `may` item at `item`.
fn accepted(scope: &Scope, item: usize, index: usize) -> Predicate {
    scope.body.may[item]
        .acceptance
        .iter()
        .nth(index)
        .expect("the acceptance exists")
        .predicate
        .clone()
}

/// The scope's rate window, with its JSON edited by `edit`.
fn window(edit: impl FnOnce(&mut Value)) -> Predicate {
    let mut value =
        serde_json::to_value(accepted(&scope(), 0, 0)).expect("the predicate serializes");
    edit(&mut value);
    serde_json::from_value(value).expect("the edited window decodes")
}

fn policy(value: &mut Value) -> &mut Value {
    &mut value["policy"]
}

/// The scope's JSON, edited by `edit`.
fn edited_scope(edit: impl FnOnce(&mut Value)) -> Scope {
    let mut value: Value = parse(SCOPE);
    edit(&mut value);
    serde_json::from_value(value).expect("the edited scope decodes")
}

/// The fixture's confirmation, listing the hashes of `scope`'s selectors and examples.
fn confirmation_for(scope: &Scope) -> ScopeConfirmation {
    let mut value: Value = parse(CONFIRMATION);
    let selectors: Vec<Value> = scope
        .body
        .may
        .iter()
        .map(|item| item.selector.selector_hash())
        .chain(
            scope
                .body
                .must_keep
                .iter()
                .map(|rule| rule.selector.selector_hash()),
        )
        .map(|hash| serde_json::to_value(hash.expect("the selector hashes")).expect("serializes"))
        .collect();
    let examples: Vec<Value> = scope
        .body
        .may
        .iter()
        .map(|item| item.example.example_hash())
        .map(|hash| serde_json::to_value(hash.expect("the example hashes")).expect("serializes"))
        .collect();
    let executable = scope
        .semantic_scope_hash()
        .expect("the scope's executable content hashes");
    value["semantic_scope_hash"] = serde_json::to_value(executable).expect("serializes");
    value["selector_hashes"] = Value::Array(selectors);
    value["example_hashes"] = Value::Array(examples);
    serde_json::from_value(value).expect("the confirmation decodes")
}

/// The receipt of one trio under `scope`, with an empty ledger.
fn receipt_of(scope: &Scope, confirmation: &ScopeConfirmation, trio: &Trio) -> Receipt {
    let ledger = fold::fold(&[]).expect("the empty log folds");
    receipt_under(scope, confirmation, trio, &ledger).expect("the receipt builds")
}

/// The prepared context of the standard runs, for `confirmation` and `ledger`. The binding
/// hashes are computed from this corpus and this ledger revision, as the gate records them; the
/// artifact hashes come from the fixture.
fn prepared_context(
    confirmation: &ScopeConfirmation,
    ledger: &CoreDerived<FoldedLedger>,
) -> PreparedCommit {
    let corpus: ProbeCorpus = parse(PROBE_CORPUS);
    let folded = ledger.value();
    let probe_set = ProbeSetManifest::of(&corpus, &folded.retired_probes)
        .and_then(|manifest| manifest.probe_set_hash())
        .expect("the probe set hashes");
    let fixture_set = FixtureSetManifest::of(&corpus)
        .and_then(|manifest| manifest.fixture_set_hash())
        .expect("the fixture set hashes");
    let run: Value = parse(RUN);
    let mut value: Value = parse::<Value>(RECEIPT)["prepared"].clone();
    value["base_commit"] = json!(BASE);
    value["context"]["protected_head"] = json!(BASE);
    value["context"]["merged_commit"] = json!(MERGED);
    value["context"]["ledger_head"] =
        serde_json::to_value(folded.head).expect("the ledger head serializes");
    value["context"]["scope_hash"] =
        serde_json::to_value(confirmation.scope_hash).expect("the scope hash serializes");
    value["context"]["probe_set_hash"] =
        serde_json::to_value(probe_set).expect("the probe set hash serializes");
    value["context"]["fixture_set_hash"] =
        serde_json::to_value(fixture_set).expect("the fixture set hash serializes");
    value["context"]["runner_hash"] = run["runner"].clone();
    value["context"]["runtime"] = run["env"]["runtime"].clone();
    serde_json::from_value(value).expect("the prepared commit decodes")
}

/// The receipt of one trio under `scope` and a folded ledger, or the error it refuses with.
fn receipt_under(
    scope: &Scope,
    confirmation: &ScopeConfirmation,
    trio: &Trio,
    ledger: &CoreDerived<FoldedLedger>,
) -> Result<Receipt, receipt::ReceiptError> {
    let corpus: ProbeCorpus = parse(PROBE_CORPUS);
    let prepared = prepared_context(confirmation, ledger);
    let masks = trio.masks();
    let comparison = trio.comparison();
    let results = [ProbeResult {
        contrast: trio.contrast(),
        masks: &masks,
        comparison: &comparison,
    }];
    let inputs = ReceiptInputs {
        scope,
        confirmation,
        prepared: &prepared,
        ledger,
        corpus: &corpus,
        results: &results,
        anchors: &[],
        episodes: &[],
        contests: &[],
        gate_state: GateState::Blocked,
    };
    receipt::build_receipt(&inputs).map(|receipt| receipt.value().clone())
}

/// The normalization plan hash of the login probe, as JSON.
fn login_plan() -> Value {
    let login = Login::load();
    serde_json::to_value(
        login
            .probe
            .normalization
            .normalization_hash()
            .expect("the plan hashes"),
    )
    .expect("the plan hash serializes")
}

/// A ruling on the login route: the sixth login must stay 401. It is the fixture decision with
/// its selector, predicate, label and plan replaced, so it folds as an active expectation of the
/// login probe.
fn login_ruling() -> LedgerEvent {
    let mut value: Value = parse(LEDGER_DECISION);
    value["ruling_id"] = json!("r-login");
    value["selector"] = json!({
        "surface": "http",
        "method": "POST",
        "route_template": "/api/login",
        "step_context": { "kind": "any" },
        "principal": { "kind": "anonymous" },
        "channel": "status"
    });
    value["predicate_ast"] = json!({
        "predicate": "status_in",
        "step_id": "s6",
        "statuses": [401]
    });
    value["label"] = json!("The sixth login stays 401");
    value["machine_ids"] = json!(["auth"]);
    value["normalization_hash"] = login_plan();
    serde_json::from_value(value).expect("the ruling decodes")
}

fn item<'r>(receipt: &'r Receipt, id: &str) -> &'r acan_intent::ReceiptItem {
    receipt
        .items
        .iter()
        .find(|item| item.item_id.as_str() == id)
        .unwrap_or_else(|| panic!("the receipt has no item {id}"))
}

/// The scope in which the candidate adds the rate limit as a status change: the window item
/// claims the status difference, so its change kind is `modify`.
fn scope_that_claims_the_status() -> Scope {
    edited_scope(|value| value["body"]["may"][0]["change"] = json!("modify"))
}

// Comparison and masks.

#[test]
fn a_candidate_only_difference_never_creates_a_mask() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    assert!(
        trio.masks().is_empty(),
        "a candidate-only change produced a mask"
    );
    let changed = differences(&trio.comparison());
    assert!(changed.contains(&("s6".to_owned(), "status".to_owned())));
    assert!(changed.contains(&("s6".to_owned(), "body.error".to_owned())));
}

#[test]
fn a_base_only_difference_on_a_maskable_field_is_masked_and_the_candidate_is_still_compared() {
    let base_a = with_cookie(sequence(SIX_FAILURES), "s8", "tok-alpha-1");
    let base_b = with_cookie(sequence(SIX_FAILURES), "s8", "tok-bravo-2");
    let candidate = with_cookie(sequence(FIVE_THEN_LIMITED), "s8", "tok-charlie-3");
    let trio = Trio::new(base_a, base_b, candidate);
    let masks = trio.masks();
    assert_eq!(masks.len(), 1, "one base-only difference, one mask");
    assert_eq!(masks[0].value().selector.step_id.as_str(), "s8");
    assert_eq!(masks[0].value().selector.path.as_str(), "cookie.session");
    let changed = differences(&trio.comparison());
    assert!(
        changed
            .iter()
            .all(|(step, path)| step == "s6" && path != "cookie.session"),
        "the masked cookie or an unmasked step was reported: {changed:?}"
    );
    assert!(changed.contains(&("s6".to_owned(), "status".to_owned())));
}

#[test]
fn a_masked_field_keeps_its_format_against_the_candidate() {
    let base_a = with_cookie(sequence(SIX_FAILURES), "s8", "tok-alpha-1");
    let base_b = with_cookie(sequence(SIX_FAILURES), "s8", "tok-bravo-2");
    let candidate = with_cookie(
        sequence(SIX_FAILURES),
        "s8",
        "3f2b8c1e-5d4a-4b6c-8d9e-0f1a2b3c4d5e",
    );
    let trio = Trio::new(base_a, base_b, candidate);
    assert_eq!(trio.masks().len(), 1);
    assert!(
        differences(&trio.comparison()).contains(&("s8".to_owned(), "cookie.session".to_owned())),
        "a uuid where base holds free text is a format change, not a masked value"
    );
}

#[test]
fn a_masked_field_still_passes_through_its_uniqueness_relation() {
    let with_codes = |codes: [&str; 2]| {
        let mut hits = sequence(SIX_FAILURES);
        hit_mut(&mut hits, "s8").body = json!({
            "links": [{ "code": codes[0] }, { "code": codes[1] }]
        });
        hits
    };
    let trio = Trio::new(
        with_codes(["aaaa1111", "bbbb2222"]),
        with_codes(["cccc3333", "dddd4444"]),
        with_codes(["eeee5555", "eeee5555"]),
    );
    assert_eq!(trio.masks().len(), 2, "both base-only codes are maskable");
    assert_eq!(
        differences(&trio.comparison()),
        vec![("s8".to_owned(), "body.links[1].code".to_owned())],
        "a duplicate code breaks the unique relation although the codes are masked"
    );

    let distinct = Trio::new(
        with_codes(["aaaa1111", "bbbb2222"]),
        with_codes(["cccc3333", "dddd4444"]),
        with_codes(["eeee5555", "ffff6666"]),
    );
    assert!(matches!(
        distinct.comparison().value().outcome,
        Outcome::Unchanged {}
    ));
}

#[test]
fn a_declared_maskable_field_the_candidate_changes_in_the_bases_format_is_masked() {
    // The bases agree on the session cookie at s8; the candidate's cookie differs but keeps the
    // base's token format. The plan declares cookie.session maskable, so the change is masked.
    let base_a = with_cookie(sequence(SIX_FAILURES), "s8", "aaaa1111bbbb22");
    let base_b = with_cookie(sequence(SIX_FAILURES), "s8", "aaaa1111bbbb22");
    let candidate = with_cookie(sequence(SIX_FAILURES), "s8", "cccc3333dddd44");
    let trio = Trio::new(base_a, base_b, candidate);
    let masks = trio.masks();
    assert_eq!(masks.len(), 1, "the declared maskable field is not masked");
    assert_eq!(masks[0].value().selector.step_id.as_str(), "s8");
    assert_eq!(masks[0].value().selector.path.as_str(), "cookie.session");
    assert!(
        differences(&trio.comparison()).is_empty(),
        "a masked change was reported: {:?}",
        differences(&trio.comparison())
    );
    // The receipt lists the mask, and no item reports the field as a difference.
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let receipt = receipt_of(&scope, &confirmation, &trio);
    assert_eq!(
        receipt.masks.len(),
        1,
        "the receipt does not count the mask"
    );
    let reported = receipt
        .items
        .iter()
        .any(|item| match &item.evaluation.outcome {
            Outcome::Changed { differences } => differences.iter().any(|difference| {
                difference.step_id.as_str() == "s8" && difference.path.as_str() == "cookie.session"
            }),
            _ => false,
        });
    assert!(!reported, "a masked field was reported as a difference");
}

#[test]
fn a_declared_maskable_field_is_a_difference_when_the_candidate_changes_its_format() {
    let base_a = with_cookie(sequence(SIX_FAILURES), "s8", "aaaa1111bbbb22");
    let base_b = with_cookie(sequence(SIX_FAILURES), "s8", "aaaa1111bbbb22");
    let candidate = with_cookie(
        sequence(SIX_FAILURES),
        "s8",
        "3f2b8c1e-5d4a-4b6c-8d9e-0f1a2b3c4d5e",
    );
    let trio = Trio::new(base_a, base_b, candidate);
    assert!(trio.masks().is_empty(), "a format change was masked");
    assert!(
        differences(&trio.comparison()).contains(&("s8".to_owned(), "cookie.session".to_owned())),
        "a uuid where the base holds a token is a difference"
    );
}

#[test]
fn a_declared_maskable_field_absent_in_the_base_is_a_difference_not_a_mask() {
    let candidate = with_cookie(sequence(SIX_FAILURES), "s8", "aaaa1111bbbb22");
    let trio = Trio::new(sequence(SIX_FAILURES), sequence(SIX_FAILURES), candidate);
    assert!(
        trio.masks().is_empty(),
        "a field the base does not have was masked"
    );
    assert!(
        differences(&trio.comparison()).contains(&("s8".to_owned(), "cookie.session".to_owned())),
        "a field absent in the base is a difference that needs a ruling"
    );
}

#[test]
fn a_status_difference_between_bases_is_unstable_and_never_masks() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
        sequence(SIX_FAILURES),
    );
    assert!(trio.masks().is_empty(), "status was masked");
    assert!(matches!(
        trio.comparison().value().outcome,
        Outcome::Unknown {
            reason: UnknownReason::UnstableBase {}
        }
    ));
}

#[test]
fn a_field_present_in_one_base_only_is_unstable_and_never_masks() {
    let trio = Trio::new(
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-alpha-1"),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    assert!(trio.masks().is_empty(), "presence was masked");
    assert!(matches!(
        trio.comparison().value().outcome,
        Outcome::Unknown {
            reason: UnknownReason::UnstableBase {}
        }
    ));
}

#[test]
fn captured_identities_compare_by_binding_not_by_their_literal_values() {
    let trio = Trio::new(
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-base-a-0001"),
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-base-b-0002"),
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-cand-0003"),
    );
    assert!(
        trio.masks().is_empty(),
        "identities that match need no mask"
    );
    assert!(matches!(
        trio.comparison().value().outcome,
        Outcome::Unchanged {}
    ));
    let canonical = compare::canonicalize(&RunContext {
        probe: &trio.login.probe,
        fixture: &trio.login.fixture,
        run: &trio.candidate,
    })
    .expect("the candidate canonicalizes");
    let session = canonical
        .value()
        .observations
        .iter()
        .find(|observation| observation.step_id.as_str() == "s8")
        .and_then(|observation| observation.channels.cookies.get("session"))
        .cloned();
    assert_eq!(session.as_deref(), Some("<capture:alice_session>"));
}

#[test]
fn a_mask_derived_from_other_bases_is_refused() {
    let first = Trio::new(
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-alpha-1"),
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-bravo-2"),
        sequence(SIX_FAILURES),
    );
    let second = Trio::new(
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-delta-4"),
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-echo-5"),
        sequence(SIX_FAILURES),
    );
    let masks = first.masks();
    assert!(matches!(
        compare::compare(&second.contrast(), &masks),
        Err(compare::CompareError::ForeignMask { .. })
    ));
}

#[test]
fn a_run_that_did_not_complete_never_passes() {
    let mut trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    trio.candidate = admitted(
        &trio.login,
        "run-candidate",
        RunRole::Candidate,
        "tok-cand-0003",
        sequence(SIX_FAILURES),
        RunOutcome::Crashed {
            detail: "the app exited".to_owned(),
        },
    );
    let never_passes = accepted(&scope(), 0, 0);
    assert!(matches!(
        trio.verdict(&never_passes),
        CheckResult::Unknown {
            reason: UnknownReason::RunFailed {}
        }
    ));
}

#[test]
fn an_incomplete_or_skipped_observation_is_named_and_never_passes() {
    let cases = [
        (
            IncompleteReason::DeadlineExceeded {},
            UnknownReason::Incomplete {},
        ),
        (IncompleteReason::StepSkipped {}, UnknownReason::Skipped {}),
    ];
    for (reason, expected) in cases {
        let mut candidate = sequence(FIVE_THEN_LIMITED);
        hit_mut(&mut candidate, "s6").incomplete = Some(reason);
        let trio = Trio::new(sequence(SIX_FAILURES), sequence(SIX_FAILURES), candidate);
        let verdict = trio.verdict(&accepted(&scope(), 0, 0));
        assert_eq!(verdict, CheckResult::Unknown { reason: expected });
    }
}

// Predicates over admitted runs.

#[test]
fn the_window_passes_when_the_sixth_failure_is_rejected() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    assert_eq!(
        trio.verdict(&accepted(&scope(), 0, 0)),
        CheckResult::Pass {}
    );
}

#[test]
fn the_window_fails_when_the_sixth_failure_is_not_rejected() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    assert!(matches!(
        trio.verdict(&accepted(&scope(), 0, 0)),
        CheckResult::Fail { detail } if detail.contains("beyond the 5 allowed")
    ));
}

#[test]
fn the_window_edge_is_inclusive_under_the_scope_policy() {
    // The first attempt opens the window at 1 s. A sixth attempt at 61 s is exactly on its
    // edge: inside when the boundary is inclusive, so it must be rejected.
    let at_the_edge = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence_at(SIX_FAILURES, [1000, 2000, 3000, 4000, 5000, 61_000]),
    );
    assert!(matches!(
        at_the_edge.verdict(&accepted(&scope(), 0, 0)),
        CheckResult::Fail { .. }
    ));
    let just_inside = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence_at(SIX_FAILURES, [1000, 2000, 3000, 4000, 5000, 60_999]),
    );
    assert!(matches!(
        just_inside.verdict(&accepted(&scope(), 0, 0)),
        CheckResult::Fail { .. }
    ));
}

#[test]
fn the_window_edge_is_outside_for_an_exclusive_boundary() {
    let exclusive = window(|value| {
        policy(value)["boundary"] = json!("exclusive");
    });
    let at_the_edge = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence_at(SIX_FAILURES, [1000, 2000, 3000, 4000, 5000, 61_000]),
    );
    assert_eq!(at_the_edge.verdict(&exclusive), CheckResult::Pass {});
    let just_inside = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence_at(SIX_FAILURES, [1000, 2000, 3000, 4000, 5000, 60_999]),
    );
    assert!(matches!(
        just_inside.verdict(&exclusive),
        CheckResult::Fail { .. }
    ));
}

#[test]
fn a_rejected_attempt_uses_up_the_quota_only_when_the_policy_counts_it() {
    // Four failures, one 429, then a failure. Counted: the 429 is the fifth counted attempt, so
    // the failure after it is beyond the quota and not rejected. Not counted: the 429 does not
    // use up the quota, so the failure is the fifth allowed attempt.
    let candidate = sequence([401, 401, 401, 401, 429, 401]);
    let trio = Trio::new(sequence(SIX_FAILURES), sequence(SIX_FAILURES), candidate);
    assert!(matches!(
        trio.verdict(&accepted(&scope(), 0, 0)),
        CheckResult::Fail { .. }
    ));
    let not_counted = window(|value| {
        policy(value)["rejected_attempts"]["counted"] = json!("not_counted");
    });
    assert_eq!(trio.verdict(&not_counted), CheckResult::Pass {});
}

#[test]
fn the_window_counts_each_group_on_its_own() {
    // Five failures from client-a and one from client-b. Grouped by client, client-a is at its
    // quota and client-b has one attempt; ungrouped, the sixth attempt is over the quota.
    let mut candidate = sequence(SIX_FAILURES);
    hit_mut(&mut candidate, "s6").client = "client-b";
    let trio = Trio::new(sequence(SIX_FAILURES), sequence(SIX_FAILURES), candidate);
    let by_client = window(|value| {
        policy(value)["event_filter"]
            .as_object_mut()
            .expect("an event filter")
            .remove("client");
    });
    assert_eq!(trio.verdict(&by_client), CheckResult::Pass {});
    let ungrouped = window(|value| {
        policy(value)["event_filter"]
            .as_object_mut()
            .expect("an event filter")
            .remove("client");
        policy(value)["group_by"] = json!("none");
    });
    assert!(matches!(trio.verdict(&ungrouped), CheckResult::Fail { .. }));
}

#[test]
fn a_window_that_matches_no_request_is_unknown_not_a_pass() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let no_match = window(|value| {
        policy(value)["event_filter"]["route_template"] = json!("/api/signup");
    });
    assert!(matches!(
        trio.verdict(&no_match),
        CheckResult::Unknown {
            reason: UnknownReason::Incomparable { .. }
        }
    ));
}

#[test]
fn an_unstable_base_makes_the_window_unknown() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
        sequence(FIVE_THEN_LIMITED),
    );
    assert!(matches!(
        trio.verdict(&accepted(&scope(), 0, 0)),
        CheckResult::Unknown {
            reason: UnknownReason::UnstableBase {}
        }
    ));
}

#[test]
fn a_count_predicate_reads_the_candidate_requests() {
    let count = accepted(&scope(), 0, 1);
    let limited = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    assert_eq!(limited.verdict(&count), CheckResult::Pass {});
    let never_limited = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    assert!(matches!(
        never_limited.verdict(&count),
        CheckResult::Fail { .. }
    ));
}

#[test]
fn equals_base_fails_on_a_changed_field_and_passes_on_an_unchanged_one() {
    let equals_base: Predicate = serde_json::from_value(json!({
        "predicate": "equals_base",
        "step_id": "s6",
        "path": "body.error",
        "anchor": { "kind": "fresh" }
    }))
    .expect("the predicate decodes");
    let same = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    assert_eq!(same.verdict(&equals_base), CheckResult::Pass {});
    let changed = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    assert!(matches!(
        changed.verdict(&equals_base),
        CheckResult::Fail { .. }
    ));
}

#[test]
fn presence_reads_a_header_of_the_candidate() {
    let present: Predicate = serde_json::from_value(json!({
        "predicate": "present",
        "at": { "ref": "field", "run": "candidate", "step_id": "s6", "path": "header.retry-after" }
    }))
    .expect("the predicate decodes");
    let limited = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    assert_eq!(limited.verdict(&present), CheckResult::Pass {});
    let failed = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    assert!(matches!(failed.verdict(&present), CheckResult::Fail { .. }));
}

#[test]
fn a_predicate_naming_a_step_with_no_observation_is_unknown() {
    let at_a_time_step: Predicate = serde_json::from_value(json!({
        "predicate": "equals_base",
        "step_id": "s7",
        "path": "status",
        "anchor": { "kind": "fresh" }
    }))
    .expect("the predicate decodes");
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    assert!(matches!(
        trio.verdict(&at_a_time_step),
        CheckResult::Unknown {
            reason: UnknownReason::Incomparable { .. }
        }
    ));
}

#[test]
fn after_n_within_is_unknown_because_the_contract_names_no_filter_for_it() {
    let after: Predicate = serde_json::from_value(json!({
        "predicate": "after_n_within",
        "n": 5,
        "window_ms": 60000,
        "then": {
            "predicate": "absent",
            "at": { "ref": "field", "run": "candidate", "step_id": "s8", "path": "header.retry-after" }
        }
    }))
    .expect("the predicate decodes");
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    assert!(matches!(
        trio.verdict(&after),
        CheckResult::Unknown {
            reason: UnknownReason::Error { detail }
        } if detail.contains("unsupported")
    ));
}

// Window obligations: the first N are allowed, a second user keeps its own count, and a
// window's end is followed by an allowed request.

/// The scope's rate window with the obligations `names` listed in its policy.
fn with_obligations(names: &[&str]) -> Predicate {
    window(|value| {
        policy(value)["obligations"] = names.iter().map(|name| json!({ "kind": name })).collect();
    })
}

const FIRST_ALLOWED: &[&str] = &["first_allowed"];
const RECOVERS: &[&str] = &["recovers"];
const BOTH: &[&str] = &["first_allowed", "recovers"];

/// The scope's rate window, counting the correct login at 61 s as well, anchored at the first
/// failure so the recipe's own login is not in it, and exclusive at its end so that login opens
/// the next window. `names` are the obligations listed in its policy.
fn recovering_window(names: &[&str]) -> Predicate {
    window(|value| {
        policy(value)["event_filter"]["status_in"] = json!([401, 429, 200]);
        policy(value)["anchor"] = json!({ "kind": "step", "step_id": "s1" });
        policy(value)["boundary"] = json!("exclusive");
        policy(value)["obligations"] = names.iter().map(|name| json!({ "kind": name })).collect();
    })
}

/// The six failures, the first five sent by alice and the sixth by bob.
fn split_users() -> Vec<Hit> {
    let mut hits = sequence(SIX_FAILURES);
    for hit in &mut hits[..5] {
        hit.actor = Some("alice");
    }
    hits[5].actor = Some("bob");
    hits
}

#[test]
fn the_nth_attempt_is_allowed_when_the_first_n_are_required() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    assert_eq!(
        trio.verdict(&with_obligations(FIRST_ALLOWED)),
        CheckResult::Pass {}
    );
}

#[test]
fn a_premature_rejection_at_the_nth_attempt_fails_when_the_first_n_are_required() {
    // Four allowed, then 429 at the fifth and sixth. The upper bound alone holds: the fifth
    // rejection uses up the quota and the sixth is rejected. Only the obligation sees that the
    // fifth attempt, one of the first five, was turned away.
    let candidate = sequence([401, 401, 401, 401, 429, 429]);
    let trio = Trio::new(sequence(SIX_FAILURES), sequence(SIX_FAILURES), candidate);
    assert_eq!(
        trio.verdict(&accepted(&scope(), 0, 0)),
        CheckResult::Pass {}
    );
    assert!(matches!(
        trio.verdict(&with_obligations(FIRST_ALLOWED)),
        CheckResult::Fail { detail } if detail.contains("within the first 5")
    ));
}

#[test]
fn a_window_that_rejects_every_attempt_fails_when_the_first_n_are_required() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence([429; 6]),
    );
    assert!(matches!(
        trio.verdict(&accepted(&scope(), 0, 0)),
        CheckResult::Unknown {
            reason: UnknownReason::Incomparable { .. }
        }
    ));
    assert!(matches!(
        trio.verdict(&with_obligations(FIRST_ALLOWED)),
        CheckResult::Fail { .. }
    ));
    assert!(matches!(
        trio.verdict(&with_obligations(BOTH)),
        CheckResult::Fail { .. }
    ));
}

#[test]
fn a_second_user_keeps_a_separate_count() {
    // Alice sends five failures and bob the sixth, at quota for her and within it for him.
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        split_users(),
    );
    let by_principal = window(|value| {
        policy(value)["event_filter"]["principal"] = json!({ "kind": "any" });
        policy(value)["group_by"] = json!("principal");
    });
    assert_eq!(trio.verdict(&by_principal), CheckResult::Pass {});
    let ungrouped = window(|value| {
        policy(value)["event_filter"]["principal"] = json!({ "kind": "any" });
        policy(value)["group_by"] = json!("none");
    });
    assert!(matches!(trio.verdict(&ungrouped), CheckResult::Fail { .. }));
}

#[test]
fn recovery_is_shown_by_an_allowed_request_after_the_window() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    assert_eq!(
        trio.verdict(&recovering_window(RECOVERS)),
        CheckResult::Pass {}
    );
    assert_eq!(trio.verdict(&recovering_window(BOTH)), CheckResult::Pass {});
}

#[test]
fn a_rejected_request_after_the_window_fails_recovery() {
    let mut candidate = sequence(FIVE_THEN_LIMITED);
    hit_mut(&mut candidate, "s8").status = 429;
    hit_mut(&mut candidate, "s8").body = body_for(429);
    let trio = Trio::new(sequence(SIX_FAILURES), sequence(SIX_FAILURES), candidate);
    assert!(matches!(
        trio.verdict(&recovering_window(RECOVERS)),
        CheckResult::Fail { detail } if detail.contains("opens the next window")
    ));
}

#[test]
fn recovery_without_a_request_after_the_window_is_unknown() {
    // The shipped filter does not count the correct login, so nothing after the window is
    // counted and recovery is not shown.
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    assert!(matches!(
        trio.verdict(&with_obligations(RECOVERS)),
        CheckResult::Unknown {
            reason: UnknownReason::Incomparable { .. }
        }
    ));
}

#[test]
fn recovery_is_not_shown_by_a_window_that_never_rejected_anything() {
    let roomy = |names: &[&str]| {
        window(|value| {
            policy(value)["allowed_count"] = json!(10);
            policy(value)["obligations"] =
                names.iter().map(|name| json!({ "kind": name })).collect();
        })
    };
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    assert_eq!(trio.verdict(&roomy(&[])), CheckResult::Pass {});
    assert!(matches!(
        trio.verdict(&roomy(RECOVERS)),
        CheckResult::Unknown {
            reason: UnknownReason::Incomparable { .. }
        }
    ));
}

#[test]
fn a_window_anchored_after_every_matching_request_is_unknown() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let anchored = window(|value| {
        policy(value)["anchor"] = json!({ "kind": "step", "step_id": "s8" });
        policy(value)["obligations"] = json!([{ "kind": "first_allowed" }, { "kind": "recovers" }]);
    });
    assert!(matches!(
        trio.verdict(&anchored),
        CheckResult::Unknown {
            reason: UnknownReason::Incomparable { .. }
        }
    ));
}

#[test]
fn the_upper_bound_form_serializes_without_obligations_and_they_round_trip() {
    let plain = accepted(&scope(), 0, 0);
    let value = serde_json::to_value(&plain).expect("the predicate serializes");
    assert!(
        value["policy"].get("obligations").is_none(),
        "the upper-bound form serializes as before the field existed"
    );
    let with = with_obligations(BOTH);
    let encoded = serde_json::to_value(&with).expect("the predicate serializes");
    let decoded: Predicate = serde_json::from_value(encoded).expect("the predicate decodes");
    assert_eq!(decoded, with);
    assert_ne!(
        with.predicate_hash().expect("the predicate hashes"),
        plain.predicate_hash().expect("the predicate hashes")
    );
    assert!(serde_json::from_value::<WindowObligation>(json!({ "kind": "always" })).is_err());
}

// Receipts.

#[test]
fn a_receipt_is_a_pure_function_of_its_inputs() {
    let scope = scope_that_claims_the_status();
    let confirmation = confirmation_for(&scope);
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let first = receipt_of(&scope, &confirmation, &trio);
    let second = receipt_of(&scope, &confirmation, &trio);
    assert_eq!(
        serde_json::to_value(&first).expect("serializes"),
        serde_json::to_value(&second).expect("serializes")
    );
    assert_eq!(
        first.receipt_hash().expect("hashes"),
        second.receipt_hash().expect("hashes")
    );
}

#[test]
fn wording_edits_leave_the_receipt_and_its_hash_unchanged() {
    let scope = scope_that_claims_the_status();
    let confirmation = confirmation_for(&scope);
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let reworded = {
        let mut value = serde_json::to_value(&scope).expect("the scope serializes");
        reword(&mut value);
        serde_json::from_value::<Scope>(value).expect("the reworded scope decodes")
    };
    let original = receipt_of(&scope, &confirmation, &trio);
    let edited = receipt_of(&reworded, &confirmation, &trio);
    assert_eq!(
        serde_json::to_value(&original).expect("serializes"),
        serde_json::to_value(&edited).expect("serializes")
    );
    assert_eq!(
        original.receipt_hash().expect("hashes"),
        edited.receipt_hash().expect("hashes")
    );
}

/// Replaces every `text` string in a JSON value with other wording.
fn reword(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key == "text" {
                    *child = json!("Reworded by a model, with different prose.");
                } else {
                    reword(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(reword),
        _ => {}
    }
}

#[test]
fn an_item_claims_a_difference_only_when_its_change_kind_matches() {
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    // scope.json says `modify` for the window item. The status moves 401 to 429, a change of
    // the kind the item claims, so the difference is not out of scope.
    let as_written = scope();
    let as_written_confirmation = confirmation_for(&as_written);
    let claimed = receipt_of(&as_written, &as_written_confirmation, &trio);
    assert!(
        !claimed
            .items
            .iter()
            .any(|item| item.class == ItemClass::OutOfScope
                && matches!(&item.evaluation.outcome, Outcome::Changed { differences }
                if differences.iter().any(|d| d.path.as_str() == "status"))),
        "a claimed status change was still reported as out of scope"
    );
    assert_eq!(
        item(&claimed, "i-login-limit").class,
        ItemClass::ObligationFulfilled
    );

    // The same item as `add` does not claim a modification, so the status change is out of scope.
    let added = edited_scope(|value| value["body"]["may"][0]["change"] = json!("add"));
    let added_confirmation = confirmation_for(&added);
    let receipt = receipt_of(&added, &added_confirmation, &trio);
    assert!(
        receipt
            .items
            .iter()
            .any(|item| item.class == ItemClass::OutOfScope
                && matches!(&item.evaluation.outcome, Outcome::Changed { differences }
                if differences.iter().any(|d| d.path.as_str() == "status"))),
        "the status change was not reported as out of scope"
    );
}

#[test]
fn a_must_keep_rule_claims_its_difference_before_a_may_change_item_does() {
    let scope = edited_scope(|value| {
        value["body"]["must_keep"] = json!([{
            "id": "mk-login-status",
            "selector": {
                "surface": "http",
                "method": "POST",
                "route_template": "/api/login",
                "step_context": { "kind": "any" },
                "principal": { "kind": "anonymous" },
                "channel": "status"
            },
            "obligations": [{
                "obligation_id": "obl-status-kept",
                "predicate": { "predicate": "status_in", "step_id": "s6", "statuses": [401] },
                "text": "The sixth login stays 401"
            }],
            "text": "Login statuses stay as they were",
            "machine_ids": ["auth"]
        }]);
        value["body"]["may"][0]["change"] = json!("modify");
    });
    let confirmation = confirmation_for(&scope);
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let receipt = receipt_of(&scope, &confirmation, &trio);
    assert_eq!(
        item(&receipt, "mk-login-status").class,
        ItemClass::ObligationFailed
    );
    assert_eq!(
        item(&receipt, "i-login-limit").evaluation.outcome,
        Outcome::Unchanged {},
        "the window item claimed a difference the rule already owns"
    );
}

#[test]
fn an_item_no_probe_reaches_gets_no_receipt_item_and_counts_as_unknown() {
    let scope = edited_scope(|value| {
        value["body"]["may"][2]["selector"]["method"] = json!("DELETE");
    });
    let confirmation = confirmation_for(&scope);
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let receipt = receipt_of(&scope, &confirmation, &trio);
    assert!(
        receipt
            .items
            .iter()
            .all(|item| item.item_id.as_str() != "i-login-body"),
        "an unreached item got a receipt item"
    );
    assert!(
        receipt.behavior.unknown >= 1,
        "the unreached obligation was not counted unknown"
    );
}

#[test]
fn counts_and_denominators_add_up_to_the_items_and_the_corpus() {
    let scope = scope_that_claims_the_status();
    let confirmation = confirmation_for(&scope);
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let receipt = receipt_of(&scope, &confirmation, &trio);
    assert_eq!(receipt.counts.total(), receipt.items.len() as u64);
    let behavior = &receipt.behavior;
    assert_eq!(
        behavior.obligations_total,
        behavior.passed + behavior.failed + behavior.unknown
    );
    // Two must-keep rules reach no run of this probe, so their two obligations are unknown.
    assert_eq!(behavior.unknown, 2);
    let corpus: ProbeCorpus = parse(PROBE_CORPUS);
    assert_eq!(receipt.probes.total, corpus.probes.len() as u32);
    assert_eq!(receipt.probes.run, 1);
    assert_eq!(receipt.probes.errored, 0);
    assert_eq!(receipt.routes.total, corpus.routes.len() as u32);
    // The signup and the login are the two corpus routes this probe reaches.
    assert_eq!(receipt.routes.covered, 2);
    assert_eq!(
        receipt.routes.uncovered.len() as u32,
        receipt.routes.total - receipt.routes.covered
    );
}

#[test]
fn a_confirmation_that_does_not_list_the_scope_is_refused() {
    let scope = scope_that_claims_the_status();
    let mut confirmation = confirmation_for(&scope);
    confirmation.selector_hashes.pop();
    let trio = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let ledger = fold::fold(&[]).expect("the empty log folds");
    assert!(matches!(
        receipt_under(&scope, &confirmation, &trio, &ledger),
        Err(receipt::ReceiptError::Inconsistent { .. })
    ));
}

#[test]
fn an_active_ruling_that_fails_on_a_reached_probe_is_a_violation_item() {
    let scope = scope_that_claims_the_status();
    let confirmation = confirmation_for(&scope);
    let ledger = fold::fold(&[login_ruling()]).expect("the ruling folds");
    let limited = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let receipt =
        receipt_under(&scope, &confirmation, &limited, &ledger).expect("the receipt builds");
    let ruling = receipt
        .rulings
        .iter()
        .find(|ruling| ruling.ruling_id.as_str() == "r-login")
        .expect("the ruling is checked");
    let RulingStatus::Violated { item_ids } = &ruling.status else {
        panic!("the failing ruling is not violated: {:?}", ruling.status);
    };
    assert_eq!(item_ids.len(), 1);
    let violation = item(&receipt, item_ids.first().as_str());
    assert_eq!(violation.class, ItemClass::ViolatesRuling);
    assert_eq!(
        violation.ruling_id.as_ref().map(|id| id.as_str()),
        Some("r-login")
    );
    assert_eq!(receipt.counts.get(ItemClass::ViolatesRuling), 1);

    let held = Trio::new(
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    let receipt = receipt_under(&scope, &confirmation, &held, &ledger).expect("the receipt builds");
    assert!(
        receipt
            .rulings
            .iter()
            .any(|ruling| ruling.ruling_id.as_str() == "r-login"
                && matches!(ruling.status, RulingStatus::Held {})),
        "a ruling the candidate keeps is not held"
    );
    assert_eq!(receipt.counts.get(ItemClass::ViolatesRuling), 0);
}

#[test]
fn the_receipt_lists_the_masks_its_comparison_derived() {
    let trio = Trio::new(
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-alpha-1"),
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-bravo-2"),
        with_cookie(sequence(SIX_FAILURES), "s8", "tok-charlie-3"),
    );
    let scope = scope_that_claims_the_status();
    let confirmation = confirmation_for(&scope);
    let receipt = receipt_of(&scope, &confirmation, &trio);
    assert_eq!(receipt.masks.len(), trio.masks().len());
    assert_eq!(receipt.masks.len(), 1);
}
