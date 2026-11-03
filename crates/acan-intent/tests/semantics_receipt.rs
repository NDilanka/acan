//! Receipt construction end to end, through `build_receipt`: the prepared context, the scope
//! confirmation, the probe inventory, ruling approvals, claimed differences and contest identity.
//!
//! Every test starts from a prepared context that the standard runs satisfy and changes one
//! thing, so the refusal or the classification it asserts comes from that change.

use std::collections::BTreeMap;

use acan_intent::compare::{self, Contrast};
use acan_intent::predicate::RetainedExample;
use acan_intent::receipt::{self, ProbeResult, ReceiptError, ReceiptInputs};
use acan_intent::{
    Authorization, CaptureOutcome, Channels, Comparison, Completeness, ContentHash, Contest,
    ContrastExample, CoreDerived, CorpusProbe, DiscrepancyEpisode, EpisodeKey, Fixture,
    FixtureSetManifest, FoldedLedger, GateState, HttpMethod, ItemClass, LedgerEvent, Mask,
    MaskablePath, Observation, ObservationHash, ObservedRun, Outcome, PreparedCommit, ProbeCorpus,
    ProbeHash, ProbeSetManifest, Receipt, ReceiptItem, RequestRecord, RulingStatus, Run,
    RunOutcome, RunRole, Scope, ScopeConfirmation, Selector, canonical_episode_selector,
};
use acan_intent::{admit, fold};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const PROBE_CORPUS: &str = include_str!("../fixtures/probe-corpus.json");
const RUN: &str = include_str!("../fixtures/run.json");
const OBSERVATION: &str = include_str!("../fixtures/observation.json");
const SCOPE: &str = include_str!("../fixtures/scope.json");
const CONFIRMATION: &str = include_str!("../fixtures/scope-confirmation.json");
const RECEIPT: &str = include_str!("../fixtures/receipt.json");
const LEDGER_DECISION: &str = include_str!("../fixtures/ledger-decision.json");
const CONTEST: &str = include_str!("../fixtures/contest.json");
const EPISODE: &str = include_str!("../fixtures/episode.json");

/// The base commit scope.json confirms against.
const BASE: &str = "178c1b07097a9f510882d139061ed50b4b44b1d4";
/// The commit the candidate was merged to.
const MERGED: &str = "a27a5f30da19543ee34362516e29bf7561214156";
/// Base A, base B and the candidate, in that order.
const STANDARD: [&str; 3] = [BASE, BASE, MERGED];
const SIX_FAILURES: [u16; 6] = [401; 6];
const FIVE_THEN_LIMITED: [u16; 6] = [401, 401, 401, 401, 401, 429];

fn parse<T: DeserializeOwned>(text: &str) -> T {
    serde_json::from_str(text).expect("a fixture decodes")
}

fn to_value(value: &impl Serialize) -> Value {
    serde_json::to_value(value).expect("the value serializes")
}

/// The corpus probe with `label`, and the fixture it starts from.
fn load(label: &str) -> (CorpusProbe, Fixture) {
    let corpus: ProbeCorpus = parse(PROBE_CORPUS);
    let probe = corpus
        .probes
        .iter()
        .find(|probe| probe.label == label)
        .expect("the corpus has the probe")
        .clone();
    let fixture = corpus
        .fixtures
        .iter()
        .find(|fixture| fixture.id == probe.spec.fixture)
        .expect("the probe's fixture is in the corpus")
        .clone();
    (probe, fixture)
}

/// One HTTP step as the runner records it.
#[derive(Clone)]
struct Hit {
    step: &'static str,
    method: HttpMethod,
    route: &'static str,
    client: &'static str,
    sent_at_ms: u64,
    status: u16,
    body: Value,
    cookies: Vec<(&'static str, String)>,
    headers: Vec<(&'static str, String)>,
}

impl Hit {
    /// A POST from client-a to `route`, answered `status` with the body that status carries.
    fn post(step: &'static str, route: &'static str, status: u16, sent_at_ms: u64) -> Self {
        Self {
            step,
            method: HttpMethod::Post,
            route,
            client: "client-a",
            sent_at_ms,
            status,
            body: body_for(status),
            cookies: Vec::new(),
            headers: Vec::new(),
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
            method: self.method,
            route_template: self.route.parse().expect("a route template"),
            actor: None,
            client: self.client.parse().expect("a client"),
            sent_at_ms: self.sent_at_ms,
        });
        observation.completeness = Completeness::Complete {};
        let mut headers: BTreeMap<String, String> = self
            .headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect();
        if self.status == 429 {
            headers.insert("retry-after".to_owned(), "60".to_owned());
        }
        observation.channels = Channels {
            status: Some(self.status),
            headers,
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

/// Six logins at one-second steps, then the correct login at 61 seconds.
fn sequence(statuses: [u16; 6]) -> Vec<Hit> {
    let steps = ["s1", "s2", "s3", "s4", "s5", "s6"];
    let mut hits: Vec<Hit> = steps
        .into_iter()
        .zip(statuses)
        .zip([1000, 2000, 3000, 4000, 5000, 6000])
        .map(|((step, status), at)| Hit::post(step, "/api/login", status, at))
        .collect();
    hits.push(Hit::post("s8", "/api/login", 200, 61_000));
    hits
}

fn hit_mut<'h>(hits: &'h mut [Hit], step: &str) -> &'h mut Hit {
    hits.iter_mut()
        .find(|hit| hit.step == step)
        .expect("the step is in the sequence")
}

/// The login probe's recipe: a signup, then the login that sets the session cookie `token`.
fn recipe(token: &str) -> Vec<Hit> {
    vec![
        Hit::post("f1", "/api/signup", 201, 0),
        Hit {
            cookies: vec![("session", token.to_owned())],
            ..Hit::post("f2", "/api/login", 200, 0)
        },
    ]
}

/// The capture the login probe's recipe binds: the session token, as its hash.
fn captured(token: &str) -> CaptureOutcome {
    CaptureOutcome::Captured {
        binding: "alice_session".parse().expect("a binding"),
        step_id: "f3".parse().expect("a step id"),
        value_hash: ContentHash::of(&Value::String(token.to_owned()))
            .expect("the value hashes")
            .into(),
    }
}

/// Admits one run of `probe` from its stored steps, as the gate admits it.
fn admit_run(
    probe: &CorpusProbe,
    fixture: &Fixture,
    role: RunRole,
    id: &str,
    commit: &str,
    steps: &[Hit],
    captures: Vec<CaptureOutcome>,
) -> ObservedRun {
    let probe_hash = probe.spec.probe_hash().expect("the probe hashes");
    let stored: Vec<(ObservationHash, Observation)> = steps
        .iter()
        .map(|hit| {
            let observation = hit.observation(id, probe_hash, commit);
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
    record.probe = probe_hash;
    record.commit = commit.parse().expect("a commit");
    record.role = role;
    record.env.fixture_hash = fixture.fixture_hash().expect("the fixture hashes");
    record.outcome = RunOutcome::Completed {};
    record.captures = captures;
    admit::observed_run(probe, fixture, record, stored).expect("the run is admitted")
}

/// One login run: the recipe, then `hits`, with the session `token` captured.
fn admit_login(
    probe: &CorpusProbe,
    fixture: &Fixture,
    role: RunRole,
    id: &str,
    commit: &str,
    token: &str,
    hits: Vec<Hit>,
) -> ObservedRun {
    let mut steps = recipe(token);
    steps.extend(hits);
    admit_run(
        probe,
        fixture,
        role,
        id,
        commit,
        &steps,
        vec![captured(token)],
    )
}

/// A probe's three admitted runs, with the masks and comparison their contrast derives.
struct Measured {
    probe: CorpusProbe,
    fixture: Fixture,
    base_a: ObservedRun,
    base_b: ObservedRun,
    candidate: ObservedRun,
    masks: Vec<CoreDerived<Mask>>,
    comparison: CoreDerived<Comparison>,
}

impl Measured {
    fn new(
        probe: CorpusProbe,
        fixture: Fixture,
        base_a: ObservedRun,
        base_b: ObservedRun,
        candidate: ObservedRun,
    ) -> Self {
        let contrast = Contrast {
            probe: &probe,
            fixture: &fixture,
            base_a: &base_a,
            base_b: &base_b,
            candidate: &candidate,
        };
        let masks = compare::derive_masks(&contrast).expect("the masks derive");
        let comparison = compare::compare(&contrast, &masks).expect("the comparison runs");
        Self {
            probe,
            fixture,
            base_a,
            base_b,
            candidate,
            masks,
            comparison,
        }
    }

    fn contrast(&self) -> Contrast<'_> {
        Contrast {
            probe: &self.probe,
            fixture: &self.fixture,
            base_a: &self.base_a,
            base_b: &self.base_b,
            candidate: &self.candidate,
        }
    }

    fn result(&self) -> ProbeResult<'_> {
        ProbeResult {
            contrast: self.contrast(),
            masks: &self.masks,
            comparison: &self.comparison,
        }
    }
}

/// The login probe measured: base A, base B and the candidate, each at its commit.
fn login(commits: [&str; 3], base_a: Vec<Hit>, base_b: Vec<Hit>, candidate: Vec<Hit>) -> Measured {
    let (probe, fixture) = load("login-rate-limit");
    login_on(probe, fixture, commits, base_a, base_b, candidate)
}

fn login_on(
    probe: CorpusProbe,
    fixture: Fixture,
    commits: [&str; 3],
    base_a: Vec<Hit>,
    base_b: Vec<Hit>,
    candidate: Vec<Hit>,
) -> Measured {
    let [commit_a, commit_b, commit_c] = commits;
    let run_a = admit_login(
        &probe,
        &fixture,
        RunRole::BaseA,
        "run-base-a",
        commit_a,
        "tok-base-a-0001",
        base_a,
    );
    let run_b = admit_login(
        &probe,
        &fixture,
        RunRole::BaseB,
        "run-base-b",
        commit_b,
        "tok-base-b-0002",
        base_b,
    );
    let run_c = admit_login(
        &probe,
        &fixture,
        RunRole::Candidate,
        "run-candidate",
        commit_c,
        "tok-cand-0003",
        candidate,
    );
    Measured::new(probe, fixture, run_a, run_b, run_c)
}

/// The login probe's twin: the same fixture, steps and route, with one step's deadline changed so
/// its spec hashes apart, and a normalization plan of its own. Both reach the login route.
fn login_twin() -> CorpusProbe {
    let (login, _) = load("login-rate-limit");
    let mut spec = to_value(&login.spec);
    spec["steps"][0]["deadline_ms"] = json!(2001);
    let mut plan = to_value(&login.normalization);
    plan["maskable_paths"]
        .as_array_mut()
        .expect("the plan lists its maskable paths")
        .push(json!("body.error"));
    CorpusProbe {
        label: "login-rate-limit-twin".to_owned(),
        spec: serde_json::from_value(spec).expect("the twin's spec decodes"),
        normalization: serde_json::from_value(plan).expect("the twin's plan decodes"),
        ..login
    }
}

/// The probe corpus with the login twin added.
fn corpus_with_login_twin() -> ProbeCorpus {
    let mut corpus: ProbeCorpus = parse(PROBE_CORPUS);
    corpus.probes.push(login_twin());
    corpus
}

/// As [`build`], for the probe corpus `corpus`, with no exact-approval anchors or contests.
fn build_on(
    corpus: &ProbeCorpus,
    scope: &Scope,
    confirmation: &ScopeConfirmation,
    prepared: &PreparedCommit,
    ledger: &CoreDerived<FoldedLedger>,
    results: &[ProbeResult<'_>],
) -> Result<Receipt, ReceiptError> {
    let inputs = ReceiptInputs {
        scope,
        confirmation,
        prepared,
        ledger,
        corpus,
        results,
        anchors: &[],
        episodes: &[],
        contests: &[],
        gate_state: GateState::Blocked,
    };
    receipt::build_receipt(&inputs).map(|receipt| receipt.value().clone())
}

/// One health step: GET /health from client-a, answered `status` with `body`.
fn health_hit(status: u16, body: Value) -> Hit {
    Hit {
        method: HttpMethod::Get,
        route: "/health",
        body,
        ..Hit::post("s1", "/health", status, 0)
    }
}

/// The health probe measured: a single step, with no recipe and no capture.
fn health(commits: [&str; 3], base_a: Hit, base_b: Hit, candidate: Hit) -> Measured {
    let (probe, fixture) = load("health");
    let [commit_a, commit_b, commit_c] = commits;
    let run_a = admit_run(
        &probe,
        &fixture,
        RunRole::BaseA,
        "run-health-base-a",
        commit_a,
        &[base_a],
        Vec::new(),
    );
    let run_b = admit_run(
        &probe,
        &fixture,
        RunRole::BaseB,
        "run-health-base-b",
        commit_b,
        &[base_b],
        Vec::new(),
    );
    let run_c = admit_run(
        &probe,
        &fixture,
        RunRole::Candidate,
        "run-health-candidate",
        commit_c,
        &[candidate],
        Vec::new(),
    );
    Measured::new(probe, fixture, run_a, run_b, run_c)
}

/// The prepared context of the standard runs, under `confirmation` and `ledger`. Every field
/// is what the gate would record for these runs, this corpus and this ledger revision.
fn prepared_context(
    confirmation: &ScopeConfirmation,
    ledger: &CoreDerived<FoldedLedger>,
) -> PreparedCommit {
    prepared_context_on(&parse(PROBE_CORPUS), confirmation, ledger)
}

/// As [`prepared_context`], for the probe corpus `corpus`.
fn prepared_context_on(
    corpus: &ProbeCorpus,
    confirmation: &ScopeConfirmation,
    ledger: &CoreDerived<FoldedLedger>,
) -> PreparedCommit {
    let folded = ledger.value();
    let probe_set = ProbeSetManifest::of(corpus, &folded.retired_probes)
        .and_then(|manifest| manifest.probe_set_hash())
        .expect("the probe set hashes");
    let fixture_set = FixtureSetManifest::of(corpus)
        .and_then(|manifest| manifest.fixture_set_hash())
        .expect("the fixture set hashes");
    let run: Value = parse(RUN);
    let mut value: Value = parse::<Value>(RECEIPT)["prepared"].clone();
    value["base_commit"] = json!(BASE);
    value["context"]["protected_head"] = json!(BASE);
    value["context"]["merged_commit"] = json!(MERGED);
    value["context"]["ledger_head"] = to_value(&folded.head);
    value["context"]["scope_hash"] = to_value(&confirmation.scope_hash);
    value["context"]["probe_set_hash"] = to_value(&probe_set);
    value["context"]["fixture_set_hash"] = to_value(&fixture_set);
    value["context"]["runner_hash"] = run["runner"].clone();
    value["context"]["runtime"] = run["env"]["runtime"].clone();
    serde_json::from_value(value).expect("the prepared commit decodes")
}

/// The prepared commit with its JSON edited by `edit`.
fn edited_prepared(prepared: &PreparedCommit, edit: impl FnOnce(&mut Value)) -> PreparedCommit {
    let mut value = to_value(prepared);
    edit(&mut value);
    serde_json::from_value(value).expect("the edited prepared commit decodes")
}

/// Builds the receipt of `results` under the scope, confirmation, prepared commit and ledger.
fn build(
    scope: &Scope,
    confirmation: &ScopeConfirmation,
    prepared: &PreparedCommit,
    ledger: &CoreDerived<FoldedLedger>,
    results: &[ProbeResult<'_>],
) -> Result<Receipt, ReceiptError> {
    build_with(scope, confirmation, prepared, ledger, results, &[], &[])
}

/// As [`build`], with the exact-approval anchors and the order's contests.
fn build_with(
    scope: &Scope,
    confirmation: &ScopeConfirmation,
    prepared: &PreparedCommit,
    ledger: &CoreDerived<FoldedLedger>,
    results: &[ProbeResult<'_>],
    anchors: &[RetainedExample<'_>],
    contests: &[Contest],
) -> Result<Receipt, ReceiptError> {
    let corpus: ProbeCorpus = parse(PROBE_CORPUS);
    let inputs = ReceiptInputs {
        scope,
        confirmation,
        prepared,
        ledger,
        corpus: &corpus,
        results,
        anchors,
        episodes: &[],
        contests,
        gate_state: GateState::Blocked,
    };
    receipt::build_receipt(&inputs).map(|receipt| receipt.value().clone())
}

fn scope() -> Scope {
    parse(SCOPE)
}

/// The scope, its JSON edited by `edit`.
fn edited_scope(edit: impl FnOnce(&mut Value)) -> Scope {
    let mut value: Value = parse(SCOPE);
    edit(&mut value);
    serde_json::from_value(value).expect("the edited scope decodes")
}

/// The fixture's confirmation, listing the hashes of `scope`'s selectors and examples and the
/// scope's executable digest.
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
        .map(|hash| to_value(&hash.expect("the selector hashes")))
        .collect();
    let examples: Vec<Value> = scope
        .body
        .may
        .iter()
        .map(|item| item.example.example_hash())
        .map(|hash| to_value(&hash.expect("the example hashes")))
        .collect();
    value["semantic_scope_hash"] = to_value(
        &scope
            .semantic_scope_hash()
            .expect("the scope's executable content hashes"),
    );
    value["selector_hashes"] = Value::Array(selectors);
    value["example_hashes"] = Value::Array(examples);
    serde_json::from_value(value).expect("the confirmation decodes")
}

fn empty_ledger() -> CoreDerived<FoldedLedger> {
    fold::fold(&[]).expect("the empty log folds")
}

/// The normalization plan hash of the login probe, as JSON.
fn login_plan() -> Value {
    let (probe, _) = load("login-rate-limit");
    to_value(
        &probe
            .normalization
            .normalization_hash()
            .expect("the plan hashes"),
    )
}

/// A ruling on the login route: the sixth login stays 401. It carries the given normalization
/// identity, applicability and accepted contrast examples.
fn login_ruling(normalization: Value, applicability: &str, accepts: Value) -> LedgerEvent {
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
    value["normalization_version"] = json!("norm/1");
    value["normalization_hash"] = normalization;
    value["applicability"] = json!(applicability);
    value["accepts"] = accepts;
    serde_json::from_value(value).expect("the ruling decodes")
}

/// The retained contrast of `measured`, recorded under `context`.
fn contrast_example(measured: &Measured, context: &PreparedCommit) -> ContrastExample {
    serde_json::from_value(json!({
        "version": "acan-contrast-example/1",
        "probe": to_value(&measured.probe),
        "fixture": to_value(&measured.fixture),
        "context": to_value(&context.context),
        "base_a": run_manifest(&measured.base_a),
        "base_b": run_manifest(&measured.base_b),
        "candidate": run_manifest(&measured.candidate),
    }))
    .expect("the contrast example decodes")
}

fn run_manifest(run: &ObservedRun) -> Value {
    let observations: Vec<Value> = run.observations().map(|(hash, _)| to_value(hash)).collect();
    json!({ "run": to_value(run.run()), "observations": observations })
}

fn ruling_status<'r>(receipt: &'r Receipt, id: &str) -> &'r RulingStatus {
    &receipt
        .rulings
        .iter()
        .find(|ruling| ruling.ruling_id.as_str() == id)
        .expect("the ruling is checked")
        .status
}

fn item<'r>(receipt: &'r Receipt, id: &str) -> &'r ReceiptItem {
    receipt
        .items
        .iter()
        .find(|item| item.item_id.as_str() == id)
        .unwrap_or_else(|| panic!("the receipt has no item {id}"))
}

/// The login route's body error is a string a rule and an item both read.
fn body_error_item_accepting_status() -> Value {
    json!([{
        "obligation_id": "obl-login-status",
        "predicate": { "predicate": "status_in", "step_id": "s6", "statuses": [401] },
        "text": "The sixth login stays 401"
    }])
}

// Prepared context (B01I refuses a receipt that does not match its context).

#[test]
fn a_prepared_context_that_matches_the_runs_builds_the_receipt() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[measured.result()],
    )
    .expect("the receipt builds");
    assert_eq!(receipt.prepared, context);
}

#[test]
fn a_changed_prepared_commit_refuses_the_receipt() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let moved = edited_prepared(&context, |value| {
        value["context"]["merged_commit"] = json!("0123456789abcdef0123456789abcdef01234567");
    });
    let result = build(&scope, &confirmation, &moved, &ledger, &[measured.result()]);
    assert!(
        matches!(&result, Err(ReceiptError::Inconsistent { detail }) if detail.contains("prepared commit")),
        "a changed prepared commit was not refused"
    );
}

#[test]
fn a_base_run_at_the_candidate_commit_is_refused() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let crossed = login(
        [MERGED, MERGED, MERGED],
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let result = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[crossed.result()],
    );
    assert!(
        matches!(&result, Err(ReceiptError::Inconsistent { detail }) if detail.contains("run-base-a")),
        "a base run at the candidate's commit was not refused: {:?}",
        result.as_ref().err()
    );
}

#[test]
fn a_changed_ledger_revision_refuses_the_receipt() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ruled = fold::fold(&[login_ruling(
        login_plan(),
        "standing_predicate_approval",
        json!([]),
    )])
    .expect("the ruling folds");
    let context = prepared_context(&confirmation, &empty_ledger());
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let result = build(
        &scope,
        &confirmation,
        &context,
        &ruled,
        &[measured.result()],
    );
    assert!(
        matches!(&result, Err(ReceiptError::Inconsistent { detail }) if detail.contains("ruling log")),
        "a ruling log other than the prepared one was not refused"
    );
}

#[test]
fn a_changed_runtime_or_runner_refuses_the_receipt() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let other_runtime = edited_prepared(&context, |value| {
        value["context"]["runtime"] = json!("bun-9.9.9");
    });
    let other_runner = edited_prepared(&context, |value| {
        value["context"]["runner_hash"] = json!(format!("sha256:{}", "0".repeat(64)));
    });
    for changed in [&other_runtime, &other_runner] {
        let result = build(
            &scope,
            &confirmation,
            changed,
            &ledger,
            &[measured.result()],
        );
        assert!(
            matches!(&result, Err(ReceiptError::Inconsistent { .. })),
            "a run on another runtime or runner was not refused"
        );
    }
}

// Confirmation: the old confirmation binds the executable scope only.

#[test]
fn a_predicate_substituted_under_an_old_confirmation_is_refused() {
    let original = scope();
    let old = confirmation_for(&original);
    let ledger = empty_ledger();
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let substituted = edited_scope(|value| {
        value["body"]["may"][0]["acceptance"][0]["predicate"]["policy"]["allowed_count"] = json!(6);
    });
    let context = prepared_context(&old, &ledger);
    let result = build(&substituted, &old, &context, &ledger, &[measured.result()]);
    assert!(
        matches!(&result, Err(ReceiptError::Inconsistent { detail }) if detail.contains("executable")),
        "a substituted predicate was accepted under the old confirmation"
    );
    let fresh = confirmation_for(&substituted);
    let fresh_context = prepared_context(&fresh, &ledger);
    build(
        &substituted,
        &fresh,
        &fresh_context,
        &ledger,
        &[measured.result()],
    )
    .expect("the substituted scope builds under its own confirmation");
}

#[test]
fn a_change_kind_substituted_under_an_old_confirmation_is_refused() {
    let original = scope();
    let old = confirmation_for(&original);
    let ledger = empty_ledger();
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let substituted = edited_scope(|value| value["body"]["may"][0]["change"] = json!("add"));
    let context = prepared_context(&old, &ledger);
    let result = build(&substituted, &old, &context, &ledger, &[measured.result()]);
    assert!(
        matches!(&result, Err(ReceiptError::Inconsistent { detail }) if detail.contains("executable")),
        "a substituted change kind was accepted under the old confirmation"
    );
}

// Probe inventory: every result is a distinct, active probe of the corpus.

#[test]
fn an_unscoped_unstable_probe_is_an_item() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let login_run = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    let health_run = health(
        STANDARD,
        health_hit(200, json!({ "ok": true })),
        health_hit(500, json!({ "error": "down" })),
        health_hit(500, json!({ "error": "down" })),
    );
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[login_run.result(), health_run.result()],
    )
    .expect("the receipt builds");
    let health_hash = health_run
        .probe
        .spec
        .probe_hash()
        .expect("the probe hashes");
    let unstable = receipt
        .items
        .iter()
        .find(|item| item.probe_hash == health_hash)
        .expect("the unstable probe that no item reaches still has an item");
    assert_eq!(unstable.class, ItemClass::UnstableBase);
    assert!(unstable.selector.is_none());
    assert_eq!(receipt.counts.get(ItemClass::UnstableBase), 1);
}

#[test]
fn a_duplicate_probe_result_is_refused() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let result = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[measured.result(), measured.result()],
    );
    assert!(
        matches!(&result, Err(ReceiptError::Inconsistent { detail }) if detail.contains("two results")),
        "a second result for one probe was not refused"
    );
}

#[test]
fn a_foreign_probe_result_is_refused() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let (mut probe, fixture) = load("login-rate-limit");
    probe
        .normalization
        .maskable_paths
        .push("body.ok".parse::<MaskablePath>().expect("a path"));
    let foreign = login_on(
        probe,
        fixture,
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let result = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[foreign.result()],
    );
    assert!(
        matches!(&result, Err(ReceiptError::Inconsistent { detail }) if detail.contains("not an active probe")),
        "a result whose plan is not the corpus's was not refused"
    );
}

#[test]
fn a_missing_result_stays_not_run() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let corpus: ProbeCorpus = parse(PROBE_CORPUS);
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[measured.result()],
    )
    .expect("the receipt builds");
    assert_eq!(receipt.probes.run, 1);
    assert_eq!(
        receipt.probes.total,
        u32::try_from(corpus.probes.len()).expect("the corpus is small")
    );
}

#[test]
fn a_swapped_role_result_is_an_error_item_and_never_passes() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let swapped = ProbeResult {
        contrast: Contrast {
            probe: &measured.probe,
            fixture: &measured.fixture,
            base_a: &measured.base_b,
            base_b: &measured.base_a,
            candidate: &measured.candidate,
        },
        masks: &measured.masks,
        comparison: &measured.comparison,
    };
    let receipt =
        build(&scope, &confirmation, &context, &ledger, &[swapped]).expect("the receipt builds");
    assert_eq!(receipt.counts.get(ItemClass::Error), 1);
    assert_eq!(receipt.behavior.passed, 0);
}

// Ruling approvals: exact, standing and same-version different-plan.

#[test]
fn a_standing_approval_runs_on_new_evidence() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = fold::fold(&[login_ruling(
        login_plan(),
        "standing_predicate_approval",
        json!([]),
    )])
    .expect("the ruling folds");
    let context = prepared_context(&confirmation, &ledger);

    let limited = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let violated = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[limited.result()],
    )
    .expect("the receipt builds");
    assert!(matches!(
        ruling_status(&violated, "r-login"),
        RulingStatus::Violated { .. }
    ));
    assert!(
        violated
            .items
            .iter()
            .any(|item| item.class == ItemClass::ViolatesRuling
                && item.ruling_id.as_ref().map(|id| id.as_str()) == Some("r-login"))
    );

    let held = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    let receipt = build(&scope, &confirmation, &context, &ledger, &[held.result()])
        .expect("the receipt builds");
    assert!(matches!(
        ruling_status(&receipt, "r-login"),
        RulingStatus::Held {}
    ));
}

#[test]
fn a_violated_standing_ruling_names_the_episode_its_selector_keys() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = fold::fold(&[login_ruling(
        login_plan(),
        "standing_predicate_approval",
        json!([]),
    )])
    .expect("the ruling folds");
    let context = prepared_context(&confirmation, &ledger);
    let limited = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let episodes = [open_episode(&scope, &login_selector())];
    let results = [limited.result()];
    let corpus: ProbeCorpus = parse(PROBE_CORPUS);
    let inputs = ReceiptInputs {
        scope: &scope,
        confirmation: &confirmation,
        prepared: &context,
        ledger: &ledger,
        corpus: &corpus,
        results: &results,
        anchors: &[],
        episodes: &episodes,
        contests: &[],
        gate_state: GateState::Blocked,
    };
    let receipt = receipt::build_receipt(&inputs)
        .expect("the receipt builds")
        .value()
        .clone();
    let violation = receipt
        .items
        .iter()
        .find(|item| item.class == ItemClass::ViolatesRuling)
        .expect("the violation is a receipt item");
    assert_eq!(
        violation.episode_id.as_ref(),
        Some(&episodes[0].id),
        "a violated ruling names no episode, so the origin refuses the failing item"
    );
}

#[test]
fn an_incomparable_item_names_the_episode_keyed_by_no_obligation() {
    // A must-keep obligation the probe cannot judge makes the item incomparable, not a failure, so
    // no obligation fails. The order keys the item's episode by none, and the receipt must name it:
    // the origin refuses a failing item that names no episode.
    let scope = edited_scope(|value| {
        let rule = &mut value["body"]["must_keep"][0];
        rule["selector"] = json!({
            "surface": "http",
            "method": "POST",
            "route_template": "/api/login",
            "step_context": { "kind": "any" },
            "principal": { "kind": "anonymous" },
            "channel": "status"
        });
        rule["obligations"] = json!([{
            "obligation_id": "obl-login-missing",
            "predicate": {
                "predicate": "equals_base",
                "step_id": "no-such-step",
                "path": "status",
                "anchor": { "kind": "fresh" }
            },
            "text": "The login status stays identical to base."
        }]);
    });
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    let results = [measured.result()];
    let episodes = [open_episode(&scope, &scope.body.must_keep[0].selector)];
    let corpus: ProbeCorpus = parse(PROBE_CORPUS);
    let inputs = ReceiptInputs {
        scope: &scope,
        confirmation: &confirmation,
        prepared: &context,
        ledger: &ledger,
        corpus: &corpus,
        results: &results,
        anchors: &[],
        episodes: &episodes,
        contests: &[],
        gate_state: GateState::Prepared,
    };
    let receipt = receipt::build_receipt(&inputs)
        .expect("the receipt builds")
        .value()
        .clone();
    let rule = item(&receipt, "mk-login-ok");
    assert_eq!(rule.class, ItemClass::Incomparable);
    assert_eq!(
        rule.episode_id.as_ref(),
        Some(&episodes[0].id),
        "an incomparable item names the episode keyed by no obligation"
    );
    receipt
        .check_episode_ids()
        .expect("every named episode is the key of its item");
}

/// The selector of the login ruling: the sixth login's status, on any login request.
fn login_selector() -> Selector {
    serde_json::from_value(json!({
        "surface": "http",
        "method": "POST",
        "route_template": "/api/login",
        "step_context": { "kind": "any" },
        "principal": { "kind": "anonymous" },
        "channel": "status"
    }))
    .expect("the selector decodes")
}

/// The open episode the order holds for `selector`, keyed as a violation of a ruling on it is: no
/// obligation, and the canonical selector.
fn open_episode(scope: &Scope, selector: &Selector) -> DiscrepancyEpisode {
    let canonical = canonical_episode_selector(selector);
    let id = EpisodeKey {
        order: scope.order.clone(),
        selector: canonical.clone(),
        obligation_id: None,
    }
    .episode_id()
    .expect("the key hashes");
    let mut value: Value = parse(EPISODE);
    value["id"] = to_value(&id);
    value["order"] = to_value(&scope.order);
    value["selector"] = to_value(&canonical);
    value["state"] = json!("open");
    value["bounce_count"] = json!(0);
    value
        .as_object_mut()
        .expect("the episode is an object")
        .remove("obligation_id");
    serde_json::from_value(value).expect("the episode decodes")
}

/// A standing ruling on the login route: the request id a login is answered with is an opaque
/// uuid. Its field is a header the standard runs never send, so a test decides who sends it.
fn request_id_ruling() -> LedgerEvent {
    let mut value: Value = parse(LEDGER_DECISION);
    value["ruling_id"] = json!("r-request-id");
    value["selector"] = json!({
        "surface": "http",
        "method": "POST",
        "route_template": "/api/login",
        "step_context": { "kind": "any" },
        "principal": { "kind": "anonymous" },
        "channel": "header",
        "field_path": "x-request-id"
    });
    value["predicate_ast"] = json!({
        "predicate": "matches_schema",
        "at": { "ref": "field", "run": "candidate", "step_id": "s1", "path": "header.x-request-id" },
        "schema": { "type": "string", "format": "uuid" }
    });
    value["label"] = json!("The request id is an opaque uuid");
    value["machine_ids"] = json!(["auth"]);
    value["normalization_version"] = json!("norm/1");
    value["normalization_hash"] = login_plan();
    value["applicability"] = json!("standing_predicate_approval");
    value["accepts"] = json!([]);
    serde_json::from_value(value).expect("the ruling decodes")
}

#[test]
fn a_standing_ruling_is_held_where_base_and_candidate_agree_on_its_field() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = fold::fold(&[request_id_ruling()]).expect("the ruling folds");
    let context = prepared_context(&confirmation, &ledger);
    // No run sends x-request-id: the candidate agrees with the base on the ruling's field, so the
    // ruling has nothing to check and holds.
    let agreeing = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[agreeing.result()],
    )
    .expect("the receipt builds");
    assert!(
        matches!(
            ruling_status(&receipt, "r-request-id"),
            RulingStatus::Held {}
        ),
        "a ruling whose field never differs was not held: {:?}",
        ruling_status(&receipt, "r-request-id")
    );
    assert!(
        receipt
            .items
            .iter()
            .all(|item| item.class != ItemClass::ViolatesRuling),
        "a field the candidate did not change violated the ruling"
    );
}

#[test]
fn a_standing_ruling_is_violated_where_its_field_differs_and_breaks_it() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = fold::fold(&[request_id_ruling()]).expect("the ruling folds");
    let context = prepared_context(&confirmation, &ledger);
    // The candidate answers the first login with the username as its request id: the field
    // differs from the base, and the value is no uuid.
    let mut candidate = sequence(SIX_FAILURES);
    hit_mut(&mut candidate, "s1")
        .headers
        .push(("x-request-id", "alice".to_owned()));
    let broken = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        candidate,
    );
    let receipt = build(&scope, &confirmation, &context, &ledger, &[broken.result()])
        .expect("the receipt builds");
    assert!(
        matches!(
            ruling_status(&receipt, "r-request-id"),
            RulingStatus::Violated { .. }
        ),
        "a wrong request id where the field differs did not violate: {:?}",
        ruling_status(&receipt, "r-request-id")
    );
    assert!(
        receipt
            .items
            .iter()
            .any(|item| item.class == ItemClass::ViolatesRuling
                && item.ruling_id.as_ref().map(|id| id.as_str()) == Some("r-request-id")),
        "the violation does not name the ruling"
    );
}

/// An opaque request id of the form the request-id ruling accepts.
const REQUEST_ID: &str = "3f2b9c1e-8a7d-4e2b-9c3d-1a2b3c4d5e6f";

/// The candidate's sixth-login sequence with its first login answered with `request_id`. The
/// base never sends the header, so the candidate's field is added and differs from the base.
fn with_request_id(request_id: &str) -> Vec<Hit> {
    let mut candidate = sequence(SIX_FAILURES);
    hit_mut(&mut candidate, "s1")
        .headers
        .push(("x-request-id", request_id.to_owned()));
    candidate
}

#[test]
fn a_held_standing_ruling_takes_its_difference_as_one_fulfilled_item() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = fold::fold(&[request_id_ruling()]).expect("the ruling folds");
    let context = prepared_context(&confirmation, &ledger);
    let held = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        with_request_id(REQUEST_ID),
    );
    let receipt = build(&scope, &confirmation, &context, &ledger, &[held.result()])
        .expect("the receipt builds");
    assert!(
        matches!(
            ruling_status(&receipt, "r-request-id"),
            RulingStatus::Held {}
        ),
        "a uuid where the field differs was not held: {:?}",
        ruling_status(&receipt, "r-request-id")
    );
    assert_eq!(
        receipt.counts.get(ItemClass::OutOfScope),
        0,
        "a held ruling left its difference out of scope"
    );
    let rows: Vec<&ReceiptItem> = receipt
        .items
        .iter()
        .filter(|item| item.ruling_id.as_ref().map(|id| id.as_str()) == Some("r-request-id"))
        .collect();
    let [row] = rows.as_slice() else {
        panic!(
            "one probe holds the ruling, so one ruling item: found {}",
            rows.len()
        );
    };
    assert_eq!(row.class, ItemClass::ObligationFulfilled);
    assert!(row.class.counts_toward_pass());
    assert!(
        matches!(row.evaluation.authorization, Authorization::Ruling { .. }),
        "the fulfilled row is not authorized by the ruling"
    );
    assert!(
        row.evidence
            .iter()
            .any(|evidence| evidence.field.as_str() == "header.x-request-id"),
        "the fulfilled row carries no evidence of the difference it takes"
    );
}

#[test]
fn a_held_standing_ruling_leaves_a_difference_a_may_item_owns_to_that_item() {
    // A may-change item that names the request id owns the difference, so the ruling finds it
    // already owned and takes nothing: the ruling still holds, but no ruling item is made.
    let scope = edited_scope(|value| {
        value["body"]["may"]
            .as_array_mut()
            .expect("the scope has may items")
            .push(json!({
                "id": "i-request-id",
                "selector": {
                    "surface": "http",
                    "method": "POST",
                    "route_template": "/api/login",
                    "step_context": { "kind": "any" },
                    "principal": { "kind": "anonymous" },
                    "channel": "header",
                    "field_path": "x-request-id"
                },
                "change": "add",
                "acceptance": [{
                    "obligation_id": "obl-request-id",
                    "predicate": {
                        "predicate": "present",
                        "at": {
                            "ref": "field",
                            "run": "candidate",
                            "step_id": "s1",
                            "path": "header.x-request-id"
                        }
                    },
                    "text": "A login carries a request id"
                }],
                "provenance": "model",
                "example": {
                    "kind": "target",
                    "step_id": "s1",
                    "path": "header.x-request-id",
                    "after": { "state": "present", "value": REQUEST_ID }
                },
                "machine_ids": ["auth"]
            }));
    });
    let confirmation = confirmation_for(&scope);
    let ledger = fold::fold(&[request_id_ruling()]).expect("the ruling folds");
    let context = prepared_context(&confirmation, &ledger);
    let candidate = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        with_request_id(REQUEST_ID),
    );
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[candidate.result()],
    )
    .expect("the receipt builds");
    assert!(
        matches!(
            ruling_status(&receipt, "r-request-id"),
            RulingStatus::Held {}
        ),
        "the ruling was not held: {:?}",
        ruling_status(&receipt, "r-request-id")
    );
    assert!(
        receipt
            .items
            .iter()
            .all(|item| item.ruling_id.as_ref().map(|id| id.as_str()) != Some("r-request-id")),
        "the ruling took a difference the may item owns"
    );
    let owned = item(&receipt, "i-request-id");
    assert!(
        matches!(
            owned.evaluation.authorization,
            Authorization::MayChange { .. }
        ),
        "the may item does not own its difference: {:?}",
        owned.evaluation.authorization
    );
    assert_eq!(receipt.counts.get(ItemClass::OutOfScope), 0);
}

#[test]
fn a_violated_standing_ruling_takes_its_difference_so_it_is_not_also_out_of_scope() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = fold::fold(&[request_id_ruling()]).expect("the ruling folds");
    let context = prepared_context(&confirmation, &ledger);
    let broken = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        with_request_id("alice"),
    );
    let receipt = build(&scope, &confirmation, &context, &ledger, &[broken.result()])
        .expect("the receipt builds");
    assert!(
        matches!(
            ruling_status(&receipt, "r-request-id"),
            RulingStatus::Violated { .. }
        ),
        "a name where the field differs did not violate: {:?}",
        ruling_status(&receipt, "r-request-id")
    );
    assert_eq!(
        receipt.counts.get(ItemClass::ViolatesRuling),
        1,
        "the violation has one row"
    );
    assert_eq!(
        receipt.counts.get(ItemClass::OutOfScope),
        0,
        "the violated difference was also counted out of scope"
    );
}

#[test]
fn a_ruling_with_the_same_version_and_another_plan_hash_is_stale_never_held() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    // The links probe's plan hash, under the same normalization version as the login probe.
    let other_plan = parse::<Value>(LEDGER_DECISION)["normalization_hash"].clone();
    let ledger = fold::fold(&[login_ruling(
        other_plan,
        "standing_predicate_approval",
        json!([]),
    )])
    .expect("the ruling folds");
    let context = prepared_context(&confirmation, &ledger);
    let held_on_the_evidence = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[held_on_the_evidence.result()],
    )
    .expect("the receipt builds");
    assert!(
        matches!(ruling_status(&receipt, "r-login"), RulingStatus::Stale {}),
        "a ruling with another plan hash was not stale: {:?}",
        ruling_status(&receipt, "r-login")
    );
}

#[test]
fn a_ruling_holds_on_its_own_plan_and_leaves_another_plans_probe_outside_it() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let corpus = corpus_with_login_twin();
    let ledger = fold::fold(&[login_ruling(
        login_plan(),
        "standing_predicate_approval",
        json!([]),
    )])
    .expect("the ruling folds");
    let prepared = prepared_context_on(&corpus, &confirmation, &ledger);
    let on_its_plan = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    // The twin's sixth login is answered 429, which the ruling's predicate does not allow. A
    // header that no scope item names appears on the candidate only.
    let mut limited = sequence(FIVE_THEN_LIMITED);
    hit_mut(&mut limited, "s6")
        .headers
        .push(("x-trace", "one".to_owned()));
    let (_, fixture) = load("login-rate-limit");
    let other_plan = login_on(
        login_twin(),
        fixture,
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        limited,
    );
    let twin_hash = login_twin().spec.probe_hash().expect("the twin hashes");
    let receipt = build_on(
        &corpus,
        &scope,
        &confirmation,
        &prepared,
        &ledger,
        &[on_its_plan.result(), other_plan.result()],
    )
    .expect("the receipt builds");
    assert!(
        matches!(ruling_status(&receipt, "r-login"), RulingStatus::Held {}),
        "a ruling on its own plan did not hold: {:?}",
        ruling_status(&receipt, "r-login")
    );
    assert_eq!(
        receipt.counts.get(ItemClass::ViolatesRuling),
        0,
        "the probe on another plan was checked against the ruling"
    );
    assert!(
        !receipt
            .items
            .iter()
            .any(|item| item.ruling_id.is_some() && item.probe_hash == twin_hash),
        "the ruling took rows from the probe on another plan"
    );
    assert!(
        receipt
            .items
            .iter()
            .any(|item| item.class == ItemClass::OutOfScope && item.probe_hash == twin_hash),
        "the other plan's header row is not out of scope"
    );
}

#[test]
fn a_ruling_is_stale_only_when_no_reached_probe_has_its_plan() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let corpus = corpus_with_login_twin();
    // The links probe's plan hash: neither login probe has it.
    let other_plan = parse::<Value>(LEDGER_DECISION)["normalization_hash"].clone();
    let ledger = fold::fold(&[login_ruling(
        other_plan,
        "standing_predicate_approval",
        json!([]),
    )])
    .expect("the ruling folds");
    let prepared = prepared_context_on(&corpus, &confirmation, &ledger);
    let on_login = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    let (_, fixture) = load("login-rate-limit");
    let twin = login_on(
        login_twin(),
        fixture,
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    let receipt = build_on(
        &corpus,
        &scope,
        &confirmation,
        &prepared,
        &ledger,
        &[on_login.result(), twin.result()],
    )
    .expect("the receipt builds");
    assert!(
        matches!(ruling_status(&receipt, "r-login"), RulingStatus::Stale {}),
        "a ruling with no reached probe on its plan was not stale: {:?}",
        ruling_status(&receipt, "r-login")
    );
}

#[test]
fn an_exact_approval_holds_only_for_the_evidence_it_accepted() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let accepted = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let manifest = contrast_example(&accepted, &prepared_context(&confirmation, &empty_ledger()));
    let anchor = admit::retained_example(&manifest, accepted.contrast(), &accepted.comparison)
        .expect("the retained example is admitted");
    let ledger = fold::fold(&[login_ruling(
        login_plan(),
        "exact_observation_approval",
        json!([anchor.hash()]),
    )])
    .expect("the ruling folds");
    let context = prepared_context(&confirmation, &ledger);
    let anchors = [anchor];

    let same = build_with(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[accepted.result()],
        &anchors,
        &[],
    )
    .expect("the receipt builds");
    assert!(matches!(
        ruling_status(&same, "r-login"),
        RulingStatus::Violated { .. }
    ));

    let other = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
    );
    let different = build_with(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[other.result()],
        &anchors,
        &[],
    )
    .expect("the receipt builds");
    assert!(
        matches!(ruling_status(&different, "r-login"), RulingStatus::Stale {}),
        "an exact approval held on evidence it did not accept"
    );
}

// Claimed differences: each obligation reads only what it names.

#[test]
fn a_same_step_difference_in_an_unread_field_is_authorized_but_unspecified() {
    let scope = edited_scope(|value| {
        value["body"]["may"][2]["acceptance"] = body_error_item_accepting_status();
    });
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let mut candidate = sequence(SIX_FAILURES);
    hit_mut(&mut candidate, "s6").body = json!({ "error": "too many attempts" });
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        candidate,
    );
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[measured.result()],
    )
    .expect("the receipt builds");
    assert_eq!(
        item(&receipt, "i-login-body").class,
        ItemClass::AuthorizedButUnspecified
    );
}

#[test]
fn an_unrelated_count_does_not_authorize_a_body_change() {
    let scope = edited_scope(|value| {
        value["body"]["may"][2]["acceptance"] = json!([{
            "obligation_id": "obl-signup-count",
            "predicate": {
                "predicate": "count",
                "filter": {
                    "run": "candidate",
                    "method": "POST",
                    "route_template": "/api/signup",
                    "status_in": [201],
                    "principal": { "kind": "anonymous" }
                },
                "op": "ge",
                "value": 1
            },
            "text": "One signup succeeds"
        }]);
    });
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let mut candidate = sequence(SIX_FAILURES);
    hit_mut(&mut candidate, "s6").body = json!({ "error": "too many attempts" });
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        candidate,
    );
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[measured.result()],
    )
    .expect("the receipt builds");
    assert_eq!(
        item(&receipt, "i-login-body").class,
        ItemClass::AuthorizedButUnspecified
    );
}

#[test]
fn a_grant_on_one_field_does_not_own_the_replacement_of_its_ancestor() {
    let scope = edited_scope(|value| {
        let mut item = value["body"]["may"][2].clone();
        item["id"] = json!("i-user-name");
        item["selector"]["field_path"] = json!("user.name");
        item["acceptance"] = body_error_item_accepting_status();
        value["body"]["may"] = json!([item]);
    });
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let mut base = sequence(SIX_FAILURES);
    hit_mut(&mut base, "s6").body = json!({ "user": { "name": "alice" } });
    let mut candidate = sequence(SIX_FAILURES);
    hit_mut(&mut candidate, "s6").body = json!({ "user": "alice" });
    let measured = login(STANDARD, base.clone(), base, candidate);
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[measured.result()],
    )
    .expect("the receipt builds");
    assert_eq!(
        item(&receipt, "i-user-name").evaluation.outcome,
        Outcome::Unchanged {},
        "a grant on one field claimed its ancestor's replacement"
    );
    assert!(
        receipt
            .items
            .iter()
            .any(|item| item.class == ItemClass::OutOfScope
                && matches!(&item.evaluation.outcome, Outcome::Changed { differences }
                if differences.iter().any(|d| d.path.as_str() == "body.user"))),
        "the ancestor's replacement was not reported out of scope"
    );
}

#[test]
fn a_whole_body_item_covers_every_body_field_at_its_step_and_nothing_at_another() {
    let scope = edited_scope(|value| {
        let mut whole = value["body"]["may"][2].clone();
        whole["id"] = json!("i-login-whole-body");
        whole["selector"]["step_context"] = json!({ "kind": "step", "step_id": "s6" });
        whole["selector"]
            .as_object_mut()
            .expect("a selector is an object")
            .remove("field_path");
        whole["acceptance"] = body_error_item_accepting_status();
        value["body"]["may"] = json!([whole]);
    });
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let mut base = sequence(SIX_FAILURES);
    let mut candidate = sequence(SIX_FAILURES);
    for step in ["s5", "s6"] {
        hit_mut(&mut base, step).body = json!({ "error": "invalid credentials", "links": ["a"] });
        hit_mut(&mut candidate, step).body =
            json!({ "error": "too many attempts", "links": ["b"] });
    }
    let measured = login(STANDARD, base.clone(), base, candidate);
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[measured.result()],
    )
    .expect("the receipt builds");
    let Outcome::Changed { differences } = &item(&receipt, "i-login-whole-body").evaluation.outcome
    else {
        panic!("the whole-body item claims no difference");
    };
    let at_s6: Vec<&str> = differences
        .iter()
        .filter(|difference| difference.step_id.as_str() == "s6")
        .map(|difference| difference.path.as_str())
        .collect();
    assert!(
        at_s6.iter().any(|path| path.starts_with("body.error")),
        "the whole-body item misses body.error at s6: {at_s6:?}"
    );
    assert!(
        at_s6.iter().any(|path| path.starts_with("body.links")),
        "the whole-body item misses body.links at s6: {at_s6:?}"
    );
    assert!(
        differences
            .iter()
            .all(|difference| difference.step_id.as_str() == "s6"),
        "the whole-body item claims a difference at another step: {differences:?}"
    );
    assert!(
        receipt
            .items
            .iter()
            .any(|entry| entry.class == ItemClass::OutOfScope
                && matches!(
                    &entry.evaluation.outcome,
                    Outcome::Changed { differences }
                        if differences.iter().any(|difference| difference.step_id.as_str() == "s5")
                )),
        "the body difference at s5 is not out of scope"
    );
}

// Unreached items and contests.

#[test]
fn an_unreached_item_gets_a_not_reached_entry_and_every_item_names_its_selector() {
    let scope = edited_scope(|value| {
        value["body"]["may"][2]["selector"]["method"] = json!("DELETE");
    });
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let receipt = build(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[measured.result()],
    )
    .expect("the receipt builds");
    assert!(
        receipt.not_reached.iter().any(
            |entry| entry.item_id.as_str() == "i-login-body" && entry.obligation_ids.len() == 1
        ),
        "an unreached item has no not_reached entry"
    );
    assert!(item(&receipt, "i-login-limit").selector.is_some());
}

#[test]
fn contest_prose_does_not_change_the_semantic_identity() {
    let scope = scope();
    let confirmation = confirmation_for(&scope);
    let ledger = empty_ledger();
    let context = prepared_context(&confirmation, &ledger);
    let measured = login(
        STANDARD,
        sequence(SIX_FAILURES),
        sequence(SIX_FAILURES),
        sequence(FIVE_THEN_LIMITED),
    );
    let as_filed: Contest = parse(CONTEST);
    let reworded: Contest = {
        let mut value: Value = parse(CONTEST);
        value["rationale"] = json!("A different argument, with the same claim.");
        serde_json::from_value(value).expect("the reworded contest decodes")
    };
    let first = build_with(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[measured.result()],
        &[],
        &[as_filed],
    )
    .expect("the receipt builds");
    let second = build_with(
        &scope,
        &confirmation,
        &context,
        &ledger,
        &[measured.result()],
        &[],
        &[reworded],
    )
    .expect("the receipt builds");
    assert_eq!(
        first.semantic_hash(&scope).expect("the semantic hash"),
        second.semantic_hash(&scope).expect("the semantic hash")
    );
    assert_ne!(
        first.receipt_hash().expect("the receipt hash"),
        second.receipt_hash().expect("the receipt hash"),
        "the contest prose is part of the receipt, so the receipt hash moves"
    );
}
