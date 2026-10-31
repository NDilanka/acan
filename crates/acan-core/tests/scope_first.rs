//! The scope-first lane, end to end: confirm, read back, claim, prepare, receipt, repair.

mod common;

use std::str::FromStr;

use acan_core::{
    AgentCmd, Blocker, DeclarationInput, EpisodeId, EpisodeState, Event, GateRunId, GateState,
    HumanCmd, Job, Judgement, LEASE_TTL_MS, LaneVerdict, LeaseEnd, OrderRev, Origin, ReadbackId,
    Rejection, Scope, ScopeBinding, ScopeConfirmation, ScopeDraftOutcome, ScopeRev, ScoreOutcome,
    Stage, Stamped, SystemCmd, TaskId, TaskState,
};
use acan_intent::{HumanRef, HumanSpec, ScopeDraft, ScopeV1, UnixMillis};
use common::{Fx, events, stored, sym};
use serde_json::{Value, json};

/// The commit the order's scope sits on.
const BASE: &str = "1111111111111111111111111111111111111111";

fn sha(byte: u8) -> String {
    format!("sha256:{byte:064x}")
}

fn commit(byte: u8) -> String {
    format!("{byte:040x}")
}

fn from<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("the fixture decodes")
}

fn narrow_selector() -> Value {
    json!({
        "surface": "http", "method": "POST", "route_template": "/api/login",
        "step_context": {"kind": "step", "step_id": "s6"},
        "principal": {"kind": "anonymous"}, "channel": "status"
    })
}

fn broad_selector() -> Value {
    json!({
        "surface": "http", "method": "POST", "route_template": "/api/login",
        "step_context": {"kind": "any"},
        "principal": {"kind": "anonymous"}, "channel": "status"
    })
}

fn counts(pairs: &[(&str, u32)]) -> Value {
    let mut all = json!({
        "preserved": 0, "obligation_fulfilled": 0, "obligation_failed": 0,
        "authorized_but_unspecified": 0, "out_of_scope": 0, "violates_ruling": 0,
        "unstable_base": 0, "incomparable": 0, "error": 0
    });
    for (class, count) in pairs {
        all[class] = json!(count);
    }
    all
}

fn changed(probe: u8) -> Value {
    json!({
        "kind": "changed",
        "differences": [{
            "probe_hash": sha(probe), "step_id": "s6", "path": "status",
            "base": {"state": "present", "value": 401},
            "candidate": {"state": "present", "value": 429}
        }]
    })
}

fn item(class: &str, episode: Option<u8>, probe: u8, outcome: Value, result: Value) -> Value {
    let mut value = json!({
        "item_id": "i-limit",
        "class": class,
        "state": "open",
        "machine_id": "api",
        "probe_hash": sha(probe),
        "episode_id": episode.map(sha),
        "attempts": 0,
        "budget": 2,
        "route_path": {"machines": ["api"], "source": "static"},
        "sequence": [],
        "evidence": [],
        "evaluation": {
            "authorization": {"kind": "may_change", "item_id": "i-limit"},
            "outcome": outcome,
            "obligations": [{"obligation_id": "obl-limit", "result": result}]
        }
    });
    // An item that names an episode names where the difference is, as the judge gives it: the
    // selector the episode keys on and the step that reproduced it.
    if episode.is_some() {
        value["selector"] = narrow_selector();
        value["sequence"] = json!([{
            "step_id": "s6", "request": 1, "run_id": "run-1",
            "role": "candidate", "tint": "differed"
        }]);
    }
    value
}

fn passing_item() -> Value {
    item(
        "obligation_fulfilled",
        None,
        7,
        changed(7),
        json!({"result": "pass"}),
    )
}

fn failing_item(episode: u8, probe: u8) -> Value {
    item(
        "obligation_failed",
        Some(episode),
        probe,
        changed(probe),
        json!({"result": "fail", "detail": "no 429"}),
    )
}

fn passed_behavior() -> Value {
    json!({"obligations_total": 1, "passed": 1, "failed": 0, "unknown": 0})
}

fn failed_behavior() -> Value {
    json!({"obligations_total": 1, "passed": 0, "failed": 1, "unknown": 0})
}

fn ruling_check(ruling: &str, seq: u64, status: Value) -> Value {
    json!({
        "ruling_id": ruling,
        "event_seq": seq,
        "label": "login limit holds",
        "machine_ids": ["api"],
        "status": status
    })
}

/// Scope-first helpers on the fixture world. Methods rather than free functions so a field of
/// `fx` can be passed to a helper that borrows `fx`.
trait LaneFx {
    fn scope_json(&self, rev: u32, selector: Value) -> Value;
    fn confirmation_for(&self, scope: &Scope) -> ScopeConfirmation;
    fn revise(&mut self, scope: &Scope) -> Vec<Stamped>;
    fn confirm(&mut self, scope: &Scope);
    fn open_lane(&mut self) -> Scope;
    fn readback(&mut self, task: TaskId, scope: &Scope, predictions: Vec<Value>) -> ReadbackId;
    fn claim_scoped(&mut self, task: TaskId);
    fn gate_run(&mut self, task: TaskId) -> GateRunId;
    fn prepared_json(&self, scope: &Scope, merged: &str) -> Value;
    fn prepare(&mut self, run: GateRunId, prepared: Value);
    fn receipt(
        &self,
        scope: &Scope,
        prepared: &Value,
        items: Vec<Value>,
        counts: Value,
        behavior: Value,
        rulings: Vec<Value>,
    ) -> Value;
    fn passing_receipt(&self, scope: &Scope, prepared: &Value) -> Value;
    fn failing_receipt(&self, scope: &Scope, prepared: &Value, episode: u8, probe: u8) -> Value;
    fn send_receipt(&mut self, run: GateRunId, receipt: Value) -> Result<Vec<Stamped>, Rejection>;
    fn prepared_run(&mut self, scope: &Scope, task: TaskId) -> (GateRunId, Value);
    fn decision_json(&self, seq: u64, ruling: &str, supersedes: Option<u64>) -> Value;
    fn note_json(&self, seq: u64, ruling: &str) -> Value;
    fn record_ruling(&mut self, member: &str, event: Value) -> Result<Vec<Stamped>, Rejection>;
    fn assert_replays(&self);
    fn lane_episode(&self, episode: u8) -> (EpisodeId, EpisodeState);
}

impl LaneFx for Fx {
    fn scope_json(&self, rev: u32, selector: Value) -> Value {
        json!({
            "version": "acan-scope/1",
            "order": self.order.to_string(),
            "rev": rev,
            "base_commit": BASE,
            "body": {
                "may": [{
                    "id": "i-limit",
                    "selector": selector,
                    "change": "add",
                    "acceptance": [{
                        "obligation_id": "obl-limit",
                        "predicate": {"predicate": "present", "at": {"ref": "field", "run": "candidate", "step_id": "s6", "path": "status"}},
                        "text": "the 429 status shows"
                    }],
                    "provenance": "user",
                    "example": {"kind": "target", "step_id": "s6", "path": "status", "after": {"state": "present", "value": 429}},
                    "machine_ids": ["api"]
                }],
                "must_keep": [],
                "open": []
            }
        })
    }

    fn confirmation_for(&self, scope: &Scope) -> ScopeConfirmation {
        let binding = ScopeBinding::of(scope).expect("the scope hashes");
        from(json!({
            "version": "acan-scope-confirmation/1",
            "order": self.order.to_string(),
            "scope_rev": scope.rev,
            "scope_hash": binding.scope_hash,
            "base_commit": BASE,
            "ruling_versions": self.origin.world().ledger().ruling_versions(),
            "selector_hashes": binding.selector_hashes,
            "example_hashes": binding.example_hashes,
            "answers": [],
            "broad_grants": binding.broad_items.iter().map(|(item, hash)| json!({
                "item_id": item, "selector_hash": hash, "by": "owner", "at": 1000
            })).collect::<Vec<_>>(),
            "budget": {"repair_rounds": 2, "wall_time_ms": 1_800_000, "model_cost_micros": 2_000_000, "max_episodes": 6},
            "by": "owner",
            "at": 1000
        }))
    }

    fn revise(&mut self, scope: &Scope) -> Vec<Stamped> {
        let owner = self.owner.clone();
        self.human(
            &owner,
            HumanCmd::ReviseScope {
                order: self.order,
                scope: Box::new(scope.clone()),
            },
        )
        .expect("the revision is recorded")
    }

    fn confirm(&mut self, scope: &Scope) {
        let owner = self.owner.clone();
        let confirmation = self.confirmation_for(scope);
        self.human(
            &owner,
            HumanCmd::ConfirmScope {
                order: self.order,
                confirmation: Box::new(confirmation),
            },
        )
        .expect("the confirmation is recorded");
    }

    fn open_lane(&mut self) -> Scope {
        let scope: Scope = from(self.scope_json(1, narrow_selector()));
        self.revise(&scope);
        self.confirm(&scope);
        scope
    }

    fn readback(&mut self, task: TaskId, scope: &Scope, predictions: Vec<Value>) -> ReadbackId {
        let agent = self.agent_a.clone();
        self.agent(
            &agent,
            AgentCmd::PostScopedReadback {
                task,
                scope_rev: scope.rev,
                scope_hash: scope.scope_hash().expect("hashing"),
                summary: "add the limit, keep login".to_owned(),
                predictions: predictions.into_iter().map(from).collect(),
            },
        )
        .expect("the readback is acknowledged");
        self.origin
            .world()
            .scoped_readbacks_of(task)
            .last()
            .expect("the readback is stored")
            .id
    }

    fn claim_scoped(&mut self, task: TaskId) {
        let agent = self.agent_a.clone();
        self.agent(&agent, AgentCmd::Claim { task })
            .expect("the claim is granted");
    }

    /// Claims when needed, declares on the first run, reports the head, and returns the run.
    fn gate_run(&mut self, task: TaskId) -> GateRunId {
        if matches!(
            self.origin.world().task(task).map(|t| &t.state),
            Some(TaskState::Open)
        ) {
            self.claim_scoped(task);
        }
        let agent = self.agent_a.clone();
        let fence = self.fence(task).expect("the task is claimed");
        if matches!(
            self.origin.world().task(task).map(|t| &t.state),
            Some(TaskState::Claimed {
                stage: Stage::Declaring,
                ..
            })
        ) {
            self.declare(
                &agent,
                fence,
                DeclarationInput {
                    symbols: vec![sym("src/auth/login.rs", "login")],
                    ..DeclarationInput::default()
                },
            )
            .expect("the footprint is declared");
        }
        if self.origin.world().barrier(self.order).is_some() {
            self.run_out_barrier(self.order);
        }
        let batch = self
            .agent(
                &agent,
                AgentCmd::ReportBuilt {
                    fence,
                    head: commit(9).parse().expect("a commit id"),
                },
            )
            .expect("the build is reported");
        batch
            .iter()
            .find_map(|stamped| match &stamped.event {
                Event::BuildReported { run, .. } => Some(*run),
                _ => None,
            })
            .expect("a gate run started")
    }

    fn prepared_json(&self, scope: &Scope, merged: &str) -> Value {
        json!({
            "base_commit": BASE,
            "context": {
                "protected_head": commit(0),
                "merged_commit": merged,
                "overlay_hash": sha(1),
                "effective_artifact_hashes": {},
                "ledger_head": self.origin.world().ledger().head(),
                "scope_hash": scope.scope_hash().expect("hashing"),
                "probe_set_hash": sha(2),
                "fixture_set_hash": sha(3),
                "runner_hash": sha(4),
                "evaluator_version": "eval-1",
                "runtime": "rt-1",
                "controlled_env_hash": sha(5),
                "sandbox_policy_hash": sha(6)
            }
        })
    }

    fn prepare(&mut self, run: GateRunId, prepared: Value) {
        self.system(SystemCmd::CandidatePrepared {
            run,
            prepared: Box::new(from(prepared)),
        })
        .expect("the candidate is prepared");
    }

    fn receipt(
        &self,
        scope: &Scope,
        prepared: &Value,
        items: Vec<Value>,
        counts: Value,
        behavior: Value,
        rulings: Vec<Value>,
    ) -> Value {
        json!({
            "version": "acan-receipt/1",
            "order": self.order.to_string(),
            "scope_rev": scope.rev,
            "scope_hash": scope.scope_hash().expect("hashing"),
            "prepared": prepared,
            "gate_state": "running",
            "items": items,
            "counts": counts,
            "probes": {"total": 1, "run": 1, "errored": 0},
            "routes": {"total": 1, "covered": 1, "uncovered": []},
            "behavior": behavior,
            "masks": [],
            "rulings": rulings,
            "contests": []
        })
    }

    fn passing_receipt(&self, scope: &Scope, prepared: &Value) -> Value {
        self.receipt(
            scope,
            prepared,
            vec![passing_item()],
            counts(&[("obligation_fulfilled", 1)]),
            passed_behavior(),
            vec![],
        )
    }

    fn failing_receipt(&self, scope: &Scope, prepared: &Value, episode: u8, probe: u8) -> Value {
        self.receipt(
            scope,
            prepared,
            vec![failing_item(episode, probe)],
            counts(&[("obligation_failed", 1)]),
            failed_behavior(),
            vec![],
        )
    }

    fn send_receipt(&mut self, run: GateRunId, receipt: Value) -> Result<Vec<Stamped>, Rejection> {
        self.system(SystemCmd::ReceiptReady {
            run,
            receipt: Box::new(from(receipt)),
        })
    }

    /// Runs one candidate all the way: claim, declare, report, prepare.
    fn prepared_run(&mut self, scope: &Scope, task: TaskId) -> (GateRunId, Value) {
        let run = self.gate_run(task);
        let prepared = self.prepared_json(scope, &commit(9));
        self.prepare(run, prepared.clone());
        (run, prepared)
    }

    fn decision_json(&self, seq: u64, ruling: &str, supersedes: Option<u64>) -> Value {
        json!({
            "event": "decision_confirmed",
            "schema_version": "acan-ledger/1",
            "event_seq": seq,
            "prev_hash": self.origin.world().ledger().head().hash,
            "actor": "owner",
            "at": 1000,
            "ruling_id": ruling,
            "order": self.order.to_string(),
            "scope_hash": sha(40),
            "evidence_hashes": [],
            "selector": narrow_selector(),
            "predicate_ast": {"predicate": "present", "at": {"ref": "field", "run": "candidate", "step_id": "s6", "path": "status"}},
            "accepts": [sha(50)],
            "rejects": [],
            "ruled_on": sha(50),
            "normalization_version": "norm/1",
            "normalization_hash": sha(51),
            "applicability": "exact_observation_approval",
            "label": "login limit holds",
            "machine_ids": ["api"],
            "supersedes": supersedes.map(|event_seq| json!({"event_seq": event_seq}))
        })
    }

    fn note_json(&self, seq: u64, ruling: &str) -> Value {
        json!({
            "event": "note_recorded",
            "schema_version": "acan-ledger/1",
            "event_seq": seq,
            "prev_hash": self.origin.world().ledger().head().hash,
            "actor": "owner",
            "at": 1000,
            "ruling_id": ruling,
            "order": self.order.to_string(),
            "text": "error pages read as plain sentences",
            "reason": {"kind": "unexpressible", "detail": "tone has no predicate"},
            "machine_ids": ["errors"]
        })
    }

    fn record_ruling(&mut self, member: &str, event: Value) -> Result<Vec<Stamped>, Rejection> {
        let member = member.parse().expect("a member id");
        self.human(
            &member,
            HumanCmd::RecordRuling {
                event: Box::new(from(event)),
            },
        )
    }

    /// The stored log folds back to the same world, as the host's restart path does.
    fn assert_replays(&self) {
        let replayed = Origin::replay(stored(&self.log)).expect("the stored log replays");
        assert_eq!(replayed.world(), self.origin.world(), "replay matches");
    }

    fn lane_episode(&self, episode: u8) -> (EpisodeId, EpisodeState) {
        let id: EpisodeId = from(json!(sha(episode)));
        let state = self
            .origin
            .world()
            .lane(self.order)
            .and_then(|lane| lane.episodes.get(&id))
            .expect("the episode exists")
            .state;
        (id, state)
    }
}

/// The gate context the host computes on `base` after the protected head moved. The protected
/// inputs are the ones `prepared` was tested under; only the head is new.
fn context_on(prepared: &Value, base: &str) -> Value {
    let mut context = prepared["context"].clone();
    context["protected_head"] = json!(base);
    context
}

#[test]
fn a_moved_protected_head_rebinds_the_scope_and_prepares_the_run_again() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let run = fx.gate_run(fx.task1);
    let prepared = fx.prepared_json(&scope, &commit(9));
    fx.prepare(run, prepared.clone());
    let old_receipt = fx.passing_receipt(&scope, &prepared);
    fx.send_receipt(run, old_receipt.clone())
        .expect("the receipt on the old head is recorded");

    // The protected head moves to commit 7. Nothing the scope binds moved, so the core re-binds.
    let moved = commit(7);
    let rebind = SystemCmd::RebindBase {
        order: fx.order,
        base: moved.parse().expect("a commit sha"),
        context: Box::new(from(context_on(&prepared, &moved))),
    };
    let events = fx
        .system(rebind)
        .expect("the scope re-binds to the moved head");
    assert!(
        events
            .iter()
            .any(|stamped| matches!(stamped.event, Event::ScopeRebound { .. })),
        "the re-bind is recorded"
    );
    let (bound, scope_base, has_receipt) = {
        let lane = fx.origin.world().lane(fx.order).expect("lane");
        (
            lane.confirmation
                .as_ref()
                .map(|c| c.base_commit.to_string()),
            lane.scope.base_commit.to_string(),
            lane.latest_receipt().is_some(),
        )
    };
    assert_eq!(bound.as_deref(), Some(moved.as_str()));
    assert_eq!(scope_base, moved);
    // A receipt accepts only a confirmation whose semantic digest matches the scope, and that
    // digest covers the base, so the re-bound confirmation must carry the re-bound digest.
    let (confirmed_digest, scope_digest) = {
        let lane = fx.origin.world().lane(fx.order).expect("lane");
        (
            lane.confirmation
                .as_ref()
                .and_then(|c| c.semantic_scope_hash),
            lane.scope.semantic_scope_hash().expect("the scope hashes"),
        )
    };
    assert_eq!(
        confirmed_digest,
        Some(scope_digest),
        "the confirmation binds the re-bound scope"
    );
    assert!(!has_receipt, "the receipt on the old head is cleared");
    assert!(
        fx.origin.world().pending_contrasts()[0].prepared.is_none(),
        "the run is prepared again"
    );

    // The old receipt is not reused, and a candidate prepared on the old head is refused.
    assert!(
        fx.send_receipt(run, old_receipt).is_err(),
        "the old receipt is not reused"
    );
    let stale = fx
        .system(SystemCmd::CandidatePrepared {
            run,
            prepared: Box::new(from(fx.prepared_json(&scope, &commit(9)))),
        })
        .unwrap_err();
    assert!(
        matches!(stale, Rejection::StaleEvidence { .. }),
        "{stale:?}"
    );

    // Prepared on the new head, the run is receipted again, and that receipt is the one that counts.
    let rebound = fx
        .origin
        .world()
        .lane(fx.order)
        .expect("lane")
        .scope
        .clone();
    let mut fresh = prepared.clone();
    fresh["base_commit"] = json!(moved);
    fresh["context"] = context_on(&prepared, &moved);
    fresh["context"]["scope_hash"] = json!(rebound.scope_hash().expect("hashing"));
    fx.prepare(run, fresh.clone());
    assert!(fx.origin.world().pending_contrasts()[0].prepared.is_some());
    let receipt = fx.passing_receipt(&rebound, &fresh);
    fx.send_receipt(run, receipt)
        .expect("the re-prepared run is receipted");
    fx.assert_replays();
}

#[test]
fn a_moved_protected_input_or_ruling_needs_the_scope_confirmed_again() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let run = fx.gate_run(fx.task1);
    let prepared = fx.prepared_json(&scope, &commit(9));
    fx.prepare(run, prepared.clone());
    let moved = commit(7);
    let order = fx.order;
    let rebind = |context: Value| SystemCmd::RebindBase {
        order,
        base: moved.parse().expect("a commit sha"),
        context: Box::new(from(context)),
    };

    // The overlay of the protected inputs changed with the head.
    let mut overlay = context_on(&prepared, &moved);
    overlay["overlay_hash"] = json!(sha(9));
    let refused = fx.system(rebind(overlay)).unwrap_err();
    assert!(
        matches!(refused, Rejection::StaleEvidence { .. }),
        "{refused:?}"
    );

    // A probe's normalization plan changed with the head.
    let mut probes = context_on(&prepared, &moved);
    probes["probe_set_hash"] = json!(sha(8));
    let refused = fx.system(rebind(probes)).unwrap_err();
    assert!(
        matches!(refused, Rejection::StaleEvidence { .. }),
        "{refused:?}"
    );

    // A ruling was recorded after the run was prepared, so the context names the old ruling head.
    fx.record_ruling("owner", fx.decision_json(1, "r-1", None))
        .expect("the ruling is recorded");
    let refused = fx
        .system(rebind(context_on(&prepared, &moved)))
        .unwrap_err();
    assert!(
        matches!(refused, Rejection::StaleEvidence { .. }),
        "{refused:?}"
    );

    // Nothing was re-bound: the confirmation still names the old head and the prepared run stays.
    let bound = fx
        .origin
        .world()
        .lane(fx.order)
        .expect("lane")
        .confirmation
        .as_ref()
        .map(|c| c.base_commit.to_string());
    assert_eq!(bound.as_deref(), Some(BASE));
    assert!(
        fx.origin.world().pending_contrasts()[0].prepared.is_some(),
        "the prepared run is kept"
    );
    fx.assert_replays();
}

#[test]
fn a_conflicting_candidate_refuses_the_run_and_reopens_the_task() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let run = fx.gate_run(fx.task1);

    let batch = fx
        .system(SystemCmd::CandidateConflicted {
            run,
            files: vec!["landed.txt".to_owned()],
        })
        .expect("the conflict refuses the run");
    let reason = batch
        .iter()
        .find_map(|stamped| match &stamped.event {
            Event::LandFailed { task, reason } if *task == fx.task1 => Some(reason.clone()),
            _ => None,
        })
        .expect("the refusal is recorded as a failed land");
    assert!(
        reason.contains("landed.txt"),
        "the reason names the file: {reason}"
    );
    assert!(
        batch.iter().any(|stamped| matches!(
            &stamped.event,
            Event::LeaseEnded { task, why: LeaseEnd::Refused, .. } if *task == fx.task1
        )),
        "the lease ends with the refusal"
    );
    assert!(
        matches!(
            fx.origin.world().task(fx.task1).map(|t| &t.state),
            Some(TaskState::Open)
        ),
        "the task reopens"
    );
    assert!(
        fx.origin
            .world()
            .pending_contrasts()
            .iter()
            .all(|pending| pending.run != run),
        "the refused run is no longer pending"
    );
    // A refused run is closed: the same refusal again is rejected.
    let again = fx
        .system(SystemCmd::CandidateConflicted {
            run,
            files: vec!["landed.txt".to_owned()],
        })
        .unwrap_err();
    assert!(matches!(again, Rejection::WrongState { .. }), "{again:?}");
    fx.assert_replays();
}

#[test]
fn a_conflict_refusal_names_at_least_one_file() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let run = fx.gate_run(fx.task1);

    let refused = fx
        .system(SystemCmd::CandidateConflicted {
            run,
            files: Vec::new(),
        })
        .unwrap_err();
    assert!(matches!(refused, Rejection::Invalid { .. }), "{refused:?}");
    assert!(
        matches!(
            fx.origin.world().task(fx.task1).map(|t| &t.state),
            Some(TaskState::Claimed { .. })
        ),
        "a refusal with no file changes nothing"
    );
}

#[test]
fn a_confirmation_is_bound_to_revision_base_rulings_and_hashes() {
    let mut fx = Fx::new();
    let scope: Scope = from(fx.scope_json(1, narrow_selector()));
    fx.revise(&scope);
    let confirmation = fx.confirmation_for(&scope);
    let owner = fx.owner.clone();
    let try_confirm = |fx: &mut Fx, edit: &dyn Fn(&mut Value)| {
        let mut value = serde_json::to_value(&confirmation).expect("serializes");
        edit(&mut value);
        fx.human(
            &owner,
            HumanCmd::ConfirmScope {
                order: fx.order,
                confirmation: Box::new(from(value)),
            },
        )
    };
    let wrong_rev = try_confirm(&mut fx, &|v| v["scope_rev"] = json!(2)).unwrap_err();
    assert_eq!(
        wrong_rev,
        Rejection::StaleScope {
            current_rev: ScopeRev(1),
            current_hash: scope.scope_hash().expect("hashing"),
        }
    );
    assert!(try_confirm(&mut fx, &|v| v["scope_hash"] = json!(sha(99))).is_err());
    assert!(try_confirm(&mut fx, &|v| v["base_commit"] = json!(commit(8))).is_err());
    assert!(
        try_confirm(&mut fx, &|v| v["ruling_versions"] =
            json!([{"ruling_id": "r-9", "event_seq": 1}]))
        .is_err()
    );
    assert!(try_confirm(&mut fx, &|v| v["selector_hashes"] = json!([sha(99)])).is_err());
    assert!(try_confirm(&mut fx, &|v| v["example_hashes"] = json!([])).is_err());
    assert!(try_confirm(&mut fx, &|_| {}).is_ok());
    // The same confirmation again is a no-op; a different one for the same revision is refused.
    assert!(try_confirm(&mut fx, &|_| {}).expect("a no-op").is_empty());
    assert!(try_confirm(&mut fx, &|v| v["at"] = json!(1001)).is_err());
    fx.assert_replays();
}

#[test]
fn a_broad_grant_needs_its_own_acknowledgement() {
    let mut fx = Fx::new();
    let scope: Scope = from(fx.scope_json(1, broad_selector()));
    fx.revise(&scope);
    let owner = fx.owner.clone();
    let mut missing = serde_json::to_value(fx.confirmation_for(&scope)).expect("serializes");
    missing["broad_grants"] = json!([]);
    let refused = fx
        .human(
            &owner,
            HumanCmd::ConfirmScope {
                order: fx.order,
                confirmation: Box::new(from(missing)),
            },
        )
        .unwrap_err();
    assert!(
        matches!(&refused, Rejection::Invalid { why } if why.contains("i-limit")),
        "the broad item is named: {refused}"
    );
    fx.confirm(&scope);
    fx.assert_replays();
}

#[test]
fn a_newer_revision_voids_the_confirmation_and_ends_leases() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    fx.claim_scoped(fx.task1);
    let widened: Scope = from(fx.scope_json(2, broad_selector()));
    let batch = fx.revise(&widened);
    assert!(
        events(&batch).iter().any(|e| matches!(
            e,
            Event::LeaseEnded {
                why: LeaseEnd::Superseded,
                ..
            }
        )),
        "the live lease is superseded"
    );
    let lane = fx.origin.world().lane(fx.order).expect("the lane");
    assert!(lane.confirmation.is_none(), "the confirmation is void");
    assert_eq!(lane.scope.rev, ScopeRev(2));
    assert_eq!(lane.lineage.len(), 2);
    let agent = fx.agent_a.clone();
    let stale = fx
        .agent(
            &agent,
            AgentCmd::PostScopedReadback {
                task: fx.task1,
                scope_rev: scope.rev,
                scope_hash: scope.scope_hash().expect("hashing"),
                summary: "the old plan".to_owned(),
                predictions: vec![],
            },
        )
        .unwrap_err();
    assert_eq!(stale, Rejection::ScopeNotConfirmed { order: fx.order });
    let claim = fx
        .agent(&agent, AgentCmd::Claim { task: fx.task1 })
        .unwrap_err();
    assert_eq!(claim, Rejection::ScopeNotConfirmed { order: fx.order });
    fx.assert_replays();
}

#[test]
fn a_readback_is_acknowledged_at_the_confirmed_revision() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    let agent = fx.agent_a.clone();
    let stale = fx
        .agent(
            &agent,
            AgentCmd::PostScopedReadback {
                task: fx.task1,
                scope_rev: ScopeRev(0),
                scope_hash: scope.scope_hash().expect("hashing"),
                summary: "the old plan".to_owned(),
                predictions: vec![],
            },
        )
        .unwrap_err();
    assert!(matches!(stale, Rejection::StaleScope { .. }));
    let predictions = vec![
        json!({"selector": narrow_selector(), "predicted": {"kind": "change", "change": "add"}}),
        json!({"selector": narrow_selector(), "predicted": {"kind": "unchanged"}}),
    ];
    let id = fx.readback(fx.task1, &scope, predictions);
    let stored = fx.origin.world().scoped_readback(id).expect("stored");
    let ack = stored
        .acknowledgement()
        .expect("the acknowledgement derives");
    assert_eq!(ack.task.as_str(), fx.task1.to_string());
    assert_eq!(ack.scope_rev, scope.rev);
    assert_eq!(ack.scope_hash, scope.scope_hash().expect("hashing"));
    assert_eq!(ack.predictions_recorded, 2);
    fx.assert_replays();
    let replayed = fx.origin.world().scoped_readback(id).expect("replayed");
    assert_eq!(
        replayed.acknowledgement().expect("derives again"),
        ack,
        "the acknowledgement is deterministic"
    );
}

#[test]
fn a_claim_needs_an_acknowledged_readback_at_the_confirmed_revision() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    let agent = fx.agent_a.clone();
    let missing = fx
        .agent(&agent, AgentCmd::Claim { task: fx.task1 })
        .unwrap_err();
    assert_eq!(missing, Rejection::NotReadBack { task: fx.task1 });
    let id = fx.readback(fx.task1, &scope, vec![]);
    fx.claim_scoped(fx.task1);
    let granted = fx
        .log
        .iter()
        .rev()
        .find_map(|stamped| match &stamped.event {
            Event::LeaseGranted { readback, .. } => Some(*readback),
            _ => None,
        });
    assert_eq!(granted, Some(id), "the lease names the scoped readback");
    // A readback at a superseded revision no longer authorizes.
    fx.readback(fx.task2, &scope, vec![]);
    let widened: Scope = from(fx.scope_json(2, narrow_selector()));
    fx.revise(&widened);
    fx.confirm(&widened);
    let stale = fx
        .agent(&agent, AgentCmd::Claim { task: fx.task2 })
        .unwrap_err();
    assert!(matches!(stale, Rejection::StaleScope { .. }));
    fx.readback(fx.task2, &widened, vec![]);
    fx.agent(&agent, AgentCmd::Claim { task: fx.task2 })
        .expect("the fresh readback authorizes the claim");
    fx.assert_replays();
}

#[test]
fn the_legacy_model_review_authorizes_nothing_in_the_lane() {
    let mut fx = Fx::new();
    // A matched model verdict on the task, written before the order went scope-first.
    let agent = fx.agent_a.clone();
    let readback = fx.read_back(&agent, fx.task1);
    fx.review(readback, Judgement::Match).expect("reviewed");
    let _scope = fx.open_lane();
    let refused = fx
        .agent(&agent, AgentCmd::Claim { task: fx.task1 })
        .unwrap_err();
    assert_eq!(
        refused,
        Rejection::NotReadBack { task: fx.task1 },
        "the matched legacy verdict is not consulted"
    );
    let legacy_readback = fx
        .agent(
            &agent,
            AgentCmd::PostReadback {
                task: fx.task1,
                summary: "legacy words".to_owned(),
                touches: vec![],
            },
        )
        .unwrap_err();
    assert!(matches!(legacy_readback, Rejection::WrongState { .. }));
    fx.assert_replays();
}

#[test]
fn a_receipt_at_the_wrong_scope_revision_is_stale() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let run = fx.gate_run(fx.task1);
    let reported = fx
        .log
        .iter()
        .rev()
        .find_map(|stamped| match &stamped.event {
            Event::BuildReported { witnesses, .. } => Some(witnesses.clone()),
            _ => None,
        });
    assert_eq!(
        reported,
        Some(vec![]),
        "a scope-first run freezes no witnesses"
    );
    assert!(
        !fx.origin
            .world()
            .jobs()
            .iter()
            .any(|job| matches!(job, Job::RunWitness { run: r, .. } if *r == run)),
        "no witness jobs are derived for it"
    );
    let pending = fx.origin.world().pending_contrasts();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].run, run);
    assert!(pending[0].prepared.is_none());
    let prepared = fx.prepared_json(&scope, &commit(9));
    fx.prepare(run, prepared.clone());
    assert!(fx.origin.world().pending_contrasts()[0].prepared.is_some());
    let mut wrong_rev = fx.passing_receipt(&scope, &prepared);
    wrong_rev["scope_rev"] = json!(0);
    let stale = fx.send_receipt(run, wrong_rev).unwrap_err();
    assert!(matches!(stale, Rejection::StaleScope { .. }));
    assert!(
        fx.origin
            .world()
            .lane(fx.order)
            .expect("lane")
            .latest_receipt()
            .is_none()
    );
    let good = fx.passing_receipt(&scope, &prepared);
    fx.send_receipt(run, good).expect("recorded");
    fx.assert_replays();
}

#[test]
fn a_duplicate_result_is_a_noop_and_a_different_one_is_refused() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    let receipt = fx.passing_receipt(&scope, &prepared);
    fx.send_receipt(run, receipt.clone()).expect("recorded");
    let again = fx
        .send_receipt(run, receipt)
        .expect("a duplicate is a no-op");
    assert!(again.is_empty(), "nothing is appended twice");
    let mut other = fx.passing_receipt(&scope, &prepared);
    other["items"][0]["probe_hash"] = json!(sha(8));
    let refused = fx.send_receipt(run, other).unwrap_err();
    assert!(
        matches!(&refused, Rejection::WrongState { what, .. } if what == "gate run"),
        "a different receipt for an answered run is refused: {refused}"
    );
    fx.assert_replays();
}

#[test]
fn a_receipt_for_another_merged_commit_is_stale() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    let mut stale = fx.passing_receipt(&scope, &prepared);
    stale["prepared"]["context"]["merged_commit"] = json!(commit(7));
    let refused = fx.send_receipt(run, stale).unwrap_err();
    assert!(
        matches!(refused, Rejection::StaleEvidence { .. }),
        "the tested commit is not the prepared one: {refused}"
    );
    let good = fx.passing_receipt(&scope, &prepared);
    fx.send_receipt(run, good).expect("recorded");
    fx.assert_replays();
}

#[test]
fn two_bounces_of_the_same_item_then_fixed() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    let first = fx.failing_receipt(&scope, &prepared, 10, 7);
    fx.send_receipt(run, first).expect("recorded");
    let (episode, state) = fx.lane_episode(10);
    assert_eq!(state, EpisodeState::Bounced, "the first failure bounces");
    let (run2, prepared2) = fx.prepared_run(&scope, fx.task1);
    let second = fx.failing_receipt(&scope, &prepared2, 10, 7);
    fx.send_receipt(run2, second).expect("recorded");
    let (_, state) = fx.lane_episode(10);
    assert_eq!(
        state,
        EpisodeState::Bounced,
        "the second failure bounces again"
    );
    let (run3, prepared3) = fx.prepared_run(&scope, fx.task1);
    let third = fx.passing_receipt(&scope, &prepared3);
    fx.send_receipt(run3, third).expect("recorded");
    let world = fx.origin.world();
    let lane = world.lane(fx.order).expect("lane");
    assert_eq!(lane.rounds.len(), 2, "two rounds bounced the same episode");
    assert!(lane.rounds.iter().all(|round| round.result.is_some()));
    let episode = &lane.episodes[&episode];
    assert_eq!(episode.state, EpisodeState::Fixed);
    assert_eq!(episode.bounce_count, 2);
    assert_eq!(episode.evidence.len(), 3, "every run's evidence is kept");
    let delivered = fx.log.iter().rev().any(|stamped| {
        matches!(
            &stamped.event,
            Event::LeaseEnded {
                why: LeaseEnd::Delivered,
                ..
            }
        )
    });
    assert!(delivered, "the passing receipt delivers the lease");
    fx.assert_replays();
}

#[test]
fn a_new_probe_spelling_never_resets_the_budget() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    let first = fx.failing_receipt(&scope, &prepared, 10, 7);
    fx.send_receipt(run, first).expect("recorded");
    let (run2, prepared2) = fx.prepared_run(&scope, fx.task1);
    // The same discrepancy under a respelled probe continues the same episode.
    let second = fx.failing_receipt(&scope, &prepared2, 10, 8);
    fx.send_receipt(run2, second).expect("recorded");
    let (episode, state) = fx.lane_episode(10);
    assert_eq!(state, EpisodeState::Bounced);
    {
        let lane = fx.origin.world().lane(fx.order).expect("lane");
        assert_eq!(
            lane.episodes.len(),
            1,
            "no new episode for a new probe hash"
        );
        let tracked = &lane.episodes[&episode];
        assert_eq!(tracked.bounce_count, 2);
        assert_eq!(
            tracked.probe_hash.to_string(),
            sha(8),
            "the latest probe reproduces it"
        );
    }
    let (run3, prepared3) = fx.prepared_run(&scope, fx.task1);
    let third = fx.failing_receipt(&scope, &prepared3, 10, 9);
    fx.send_receipt(run3, third).expect("recorded");
    let (episode, state) = fx.lane_episode(10);
    assert_eq!(
        state,
        EpisodeState::Stopped,
        "the budget ran out after two rounds"
    );
    let world = fx.origin.world();
    let lane = world.lane(fx.order).expect("lane");
    assert_eq!(lane.rounds.len(), 2, "no third round opens");
    assert!(lane.stopped);
    assert_eq!(lane.gate_state(), Some(GateState::Stopped));
    assert_eq!(
        lane.latest_receipt().map(|record| &record.verdict),
        Some(&LaneVerdict::Stopped {
            episodes: vec![episode]
        }),
        "exhaustion shows as stopped, never as passed"
    );
    let agent = fx.agent_a.clone();
    let claim = fx
        .agent(&agent, AgentCmd::Claim { task: fx.task2 })
        .unwrap_err();
    assert_eq!(claim, Rejection::BudgetExhausted { order: fx.order });
    let fence = fx.fence(fx.task1).expect("still claimed");
    let build = fx
        .agent(
            &agent,
            AgentCmd::ReportBuilt {
                fence,
                head: commit(9).parse().expect("a commit id"),
            },
        )
        .unwrap_err();
    assert_eq!(build, Rejection::BudgetExhausted { order: fx.order });
    fx.assert_replays();
}

/// The judge reads each episode with its state and bounce count, so a repeat of one difference is
/// judged as a repeat: bounced, then, once the budget is spent, stopped.
#[test]
fn a_repeat_is_judged_with_its_bounce_count_and_state() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let judged = |fx: &Fx| {
        fx.origin
            .world()
            .judged_episodes(fx.order)
            .expect("the episodes read")
    };
    assert!(
        judged(&fx).is_empty(),
        "a first failure has no episode for the judge yet"
    );

    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    let first = fx.failing_receipt(&scope, &prepared, 10, 7);
    fx.send_receipt(run, first).expect("recorded");
    let (episode, _) = fx.lane_episode(10);
    let seen = judged(&fx);
    assert_eq!(seen.len(), 1, "the failure opened one episode");
    assert_eq!(
        (seen[0].id, seen[0].state, seen[0].bounce_count),
        (episode, EpisodeState::Bounced, 1),
        "the first round bounced it"
    );
    assert!(
        seen[0].obligation_id.is_some(),
        "the episode names the obligation that failed"
    );

    let (run2, prepared2) = fx.prepared_run(&scope, fx.task1);
    let second = fx.failing_receipt(&scope, &prepared2, 10, 8);
    fx.send_receipt(run2, second).expect("recorded");
    let seen = judged(&fx);
    assert_eq!(
        (seen[0].state, seen[0].bounce_count),
        (EpisodeState::Bounced, 2),
        "the repeat counts as the second bounce"
    );

    let (run3, prepared3) = fx.prepared_run(&scope, fx.task1);
    let third = fx.failing_receipt(&scope, &prepared3, 10, 9);
    fx.send_receipt(run3, third).expect("recorded");
    let seen = judged(&fx);
    assert_eq!(
        (seen[0].state, seen[0].bounce_count),
        (EpisodeState::Stopped, 2),
        "the budget stops the third failure, and the judge sees it"
    );
    fx.assert_replays();
}

#[test]
fn a_contest_asks_the_human_and_never_widens_scope() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    let first = fx.failing_receipt(&scope, &prepared, 10, 7);
    fx.send_receipt(run, first).expect("recorded");
    let (episode, _) = fx.lane_episode(10);
    let agent = fx.agent_a.clone();
    let fence = fx.fence(fx.task1).expect("still claimed");
    let filed = fx
        .agent(
            &agent,
            AgentCmd::FileContest {
                fence,
                episode,
                kind: acan_core::ContestKind::Blocked,
                reproducer: from(json!(sha(7))),
                rationale: "the scope blocks the only fix".to_owned(),
            },
        )
        .expect("the contest is filed");
    let contest_id = filed
        .iter()
        .find_map(|stamped| match &stamped.event {
            Event::ContestFiled { contest } => Some(contest.id.clone()),
            _ => None,
        })
        .expect("a contest was recorded");
    let (_, state) = fx.lane_episode(10);
    assert_eq!(state, EpisodeState::Contested);
    // While the human is asked, no receipt can pass.
    let (run2, prepared2) = fx.prepared_run(&scope, fx.task1);
    let second = fx.passing_receipt(&scope, &prepared2);
    fx.send_receipt(run2, second).expect("recorded");
    {
        let world = fx.origin.world();
        let verdict = &world
            .lane(fx.order)
            .expect("lane")
            .latest_receipt()
            .expect("a receipt")
            .verdict;
        assert!(
            matches!(verdict, LaneVerdict::Blocked { blockers } if blockers.iter().any(|b| matches!(b, Blocker::Contest { .. }))),
            "an open contest blocks the pass: {verdict:?}"
        );
    }
    // An agent cannot resolve it, and neither can a member who did not post the order.
    let bob = fx.bob.clone();
    let refused = fx
        .human(
            &bob,
            HumanCmd::ResolveContest {
                contest: contest_id.clone(),
                status: acan_core::ContestStatus::Rejected,
            },
        )
        .unwrap_err();
    assert!(matches!(refused, Rejection::Forbidden { .. }));
    let owner = fx.owner.clone();
    let open = fx
        .human(
            &owner,
            HumanCmd::ResolveContest {
                contest: contest_id.clone(),
                status: acan_core::ContestStatus::Open,
            },
        )
        .unwrap_err();
    assert!(matches!(open, Rejection::Invalid { .. }));
    fx.human(
        &owner,
        HumanCmd::ResolveContest {
            contest: contest_id.clone(),
            status: acan_core::ContestStatus::Upheld,
        },
    )
    .expect("the human upholds it");
    let (_, state) = fx.lane_episode(10);
    assert_eq!(
        state,
        EpisodeState::Residue,
        "an upheld contest leaves residue"
    );
    // A rejected contest restores the state it interrupted.
    let (run3, prepared3) = fx.prepared_run(&scope, fx.task1);
    let third = fx.failing_receipt(&scope, &prepared3, 10, 7);
    fx.send_receipt(run3, third).expect("recorded");
    let fence = fx.fence(fx.task1).expect("still claimed");
    fx.agent(
        &agent,
        AgentCmd::FileContest {
            fence,
            episode,
            kind: acan_core::ContestKind::SpecConflict,
            reproducer: from(json!(sha(7))),
            rationale: "two obligations conflict".to_owned(),
        },
    )
    .expect("residue can be contested");
    let second = fx
        .origin
        .world()
        .lane(fx.order)
        .expect("lane")
        .contests
        .keys()
        .last()
        .cloned()
        .expect("a second contest");
    fx.human(
        &owner,
        HumanCmd::ResolveContest {
            contest: second,
            status: acan_core::ContestStatus::Rejected,
        },
    )
    .expect("the human rejects it");
    let (_, state) = fx.lane_episode(10);
    assert_eq!(
        state,
        EpisodeState::Residue,
        "the interrupted state is restored"
    );
    // None of this widened the scope.
    let lane = fx.origin.world().lane(fx.order).expect("lane");
    assert_eq!(lane.scope, scope, "the scope never changed");
    assert!(lane.confirmation.is_some(), "the confirmation stands");
    let revisions = fx
        .log
        .iter()
        .filter(|stamped| matches!(stamped.event, Event::ScopeRevised { .. }))
        .count();
    assert_eq!(revisions, 1, "no revision was written");
    fx.assert_replays();
}

#[test]
fn only_a_human_records_a_ruling_and_only_the_head_is_superseded() {
    let mut fx = Fx::new();
    let _scope = fx.open_lane();
    let d1 = fx.decision_json(1, "r-1", None);
    fx.record_ruling("owner", d1.clone())
        .expect("the decision is recorded");
    assert_eq!(fx.origin.world().ledger().head().event_seq, 1);
    // A member cannot sign as another member.
    let mut signed_wrong = d1.clone();
    signed_wrong["event_seq"] = json!(2);
    signed_wrong["prev_hash"] = json!(fx.origin.world().ledger().head().hash);
    let forged = fx.record_ruling("bob", signed_wrong).unwrap_err();
    assert!(matches!(forged, Rejection::Forbidden { .. }));
    // A member who did not post the order cannot rule in it, even as themselves.
    let mut bobs = d1.clone();
    bobs["event_seq"] = json!(2);
    bobs["actor"] = json!("bob");
    bobs["prev_hash"] = json!(fx.origin.world().ledger().head().hash);
    let forbidden = fx.record_ruling("bob", bobs).unwrap_err();
    assert!(matches!(forbidden, Rejection::Forbidden { .. }));
    // Re-appending the head is a no-op; a different event at its seq is refused.
    let again = fx
        .record_ruling("owner", d1.clone())
        .expect("a duplicate is a no-op");
    assert!(again.is_empty());
    let mut conflict = d1.clone();
    conflict["label"] = json!("a different decision at seq 1");
    assert!(fx.record_ruling("owner", conflict).is_err());
    // Supersession names the lineage head, once.
    let d2 = fx.decision_json(2, "r-1", Some(1));
    fx.record_ruling("owner", d2)
        .expect("the supersession is recorded");
    let versions = fx.origin.world().ledger().ruling_versions();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].event_seq, 2);
    let d3 = fx.decision_json(3, "r-1", Some(1));
    assert!(
        fx.record_ruling("owner", d3).is_err(),
        "event 1 is no longer the head and cannot be superseded twice"
    );
    // A stored ruling whose writer is not its signer does not replay.
    let forged = Event::RulingRecorded {
        event: Box::new(from(fx.decision_json(3, "r-1", Some(2)))),
    };
    let err = fx.append_stored(forged).unwrap_err();
    assert!(
        matches!(err, acan_core::ReplayError::InvalidEvent { .. }),
        "only the signing member's events replay: {err}"
    );
    fx.assert_replays();
}

#[test]
fn only_an_owner_retires_a_probe() {
    let mut fx = Fx::new();
    let _scope = fx.open_lane();
    let retirement = |seq: u64, actor: &str, prev: Value| {
        json!({
            "event": "probe_retired",
            "schema_version": "acan-ledger/1",
            "event_seq": seq,
            "prev_hash": prev,
            "actor": actor,
            "at": 1000,
            "probe_hash": sha(7),
            "reason": "the probe tests nothing now"
        })
    };
    let prev = json!(fx.origin.world().ledger().head().hash);
    let refused = fx
        .record_ruling("bob", retirement(1, "bob", prev.clone()))
        .unwrap_err();
    assert!(
        matches!(&refused, Rejection::Forbidden { why } if why.contains("owner")),
        "a member cannot narrow the corpus: {refused}"
    );
    fx.record_ruling("owner", retirement(1, "owner", prev))
        .expect("an owner retires the probe");
    assert_eq!(fx.origin.world().ledger().events().len(), 1);
    fx.assert_replays();
}

#[test]
fn a_change_predicted_at_a_must_keep_rule_is_missed_not_held() {
    let mut fx = Fx::new();
    let mut body = fx.scope_json(1, narrow_selector());
    body["body"]["must_keep"] = json!([{
        "id": "mk-keep",
        "selector": narrow_selector(),
        "obligations": [{
            "obligation_id": "obl-keep",
            "predicate": {"predicate": "present", "at": {"ref": "field", "run": "candidate", "step_id": "s6", "path": "status"}},
            "text": "the status stays"
        }],
        "text": "login keeps its status",
        "machine_ids": ["api"]
    }]);
    let scope: Scope = from(body);
    fx.revise(&scope);
    fx.confirm(&scope);
    let predictions = vec![
        json!({"selector": narrow_selector(), "predicted": {"kind": "change", "change": "add"}}),
    ];
    fx.readback(fx.task1, &scope, predictions);
    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    let mut keep = item(
        "violates_ruling",
        Some(12),
        7,
        changed(7),
        json!({"result": "fail", "detail": "the status moved"}),
    );
    keep["evaluation"]["authorization"] = json!({"kind": "must_keep", "must_keep_id": "mk-keep"});
    let receipt = fx.receipt(
        &scope,
        &prepared,
        vec![keep],
        counts(&[("violates_ruling", 1)]),
        failed_behavior(),
        vec![],
    );
    fx.send_receipt(run, receipt).expect("recorded");
    let world = fx.origin.world();
    let record = world
        .lane(fx.order)
        .and_then(|lane| lane.latest_receipt())
        .expect("a receipt");
    assert!(
        matches!(record.scores[0].outcome, ScoreOutcome::Missed { .. }),
        "a must-keep rule allows no change kind, so the prediction is missed: {:?}",
        record.scores
    );
    fx.assert_replays();
}

#[test]
fn a_lease_does_not_expire_while_the_gate_is_deciding() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    fx.advance(3 * LEASE_TTL_MS);
    let tick = fx.system(SystemCmd::Tick).expect("the tick is accepted");
    assert!(
        !events(&tick)
            .iter()
            .any(|e| matches!(e, Event::LeaseEnded { .. })),
        "the gating lease does not run down"
    );
    assert!(
        matches!(
            fx.origin.world().task(fx.task1).map(|t| &t.state),
            Some(TaskState::Claimed {
                stage: Stage::Gating { .. },
                ..
            })
        ),
        "the task is still gating"
    );
    let late = fx.passing_receipt(&scope, &prepared);
    fx.send_receipt(run, late)
        .expect("the late receipt is recorded");
    fx.assert_replays();
}

#[test]
fn an_unexpressible_ruling_is_labelled_not_enforced_and_never_counted() {
    let mut fx = Fx::new();
    let scope: Scope = from(fx.scope_json(1, narrow_selector()));
    fx.revise(&scope);
    let note = fx.note_json(1, "r-2");
    fx.record_ruling("owner", note)
        .expect("the note is recorded");
    fx.confirm(&scope);
    fx.readback(fx.task1, &scope, vec![]);
    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    // The note reported as held is refused: a note is never a check.
    let mut receipt = fx.receipt(
        &scope,
        &prepared,
        vec![passing_item()],
        counts(&[("obligation_fulfilled", 1)]),
        passed_behavior(),
        vec![ruling_check("r-2", 1, json!({"status": "held"}))],
    );
    let refused = fx.send_receipt(run, receipt.clone()).unwrap_err();
    assert!(
        matches!(&refused, Rejection::Invalid { why } if why.contains("not enforced")),
        "a note cannot be labelled held: {refused}"
    );
    receipt["rulings"][0]["status"] = json!({"status": "not_enforced", "reason": {"kind": "unexpressible", "detail": "tone has no predicate"}});
    // The note passes the check, but it never settles the residue its order still owes.
    receipt["items"] = json!([item(
        "authorized_but_unspecified",
        Some(10),
        7,
        changed(7),
        json!({"result": "pass"})
    )]);
    receipt["counts"] = counts(&[("authorized_but_unspecified", 1)]);
    fx.send_receipt(run, receipt).expect("recorded");
    let world = fx.origin.world();
    let lane = world.lane(fx.order).expect("lane");
    let record = lane.latest_receipt().expect("a receipt");
    let check = &record.receipt.rulings[0];
    assert!(
        matches!(check.status, acan_core::RulingStatus::NotEnforced { .. }),
        "the note is labelled not enforced"
    );
    let LaneVerdict::Blocked { blockers } = &record.verdict else {
        panic!("residue blocks the pass, got {:?}", record.verdict);
    };
    assert!(
        !blockers.iter().any(|b| matches!(b, Blocker::Ruling { .. })),
        "the note is never counted: {blockers:?}"
    );
    assert!(
        blockers.iter().any(|b| matches!(
            b,
            Blocker::Episode {
                state: EpisodeState::Residue,
                ..
            }
        )),
        "the residue still blocks: {blockers:?}"
    );
    fx.assert_replays();
}

#[test]
fn a_failed_prediction_is_a_finding_and_a_held_one_is_never_a_pass() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    let predictions = vec![
        json!({"selector": narrow_selector(), "predicted": {"kind": "change", "change": "add"}}),
        json!({"selector": narrow_selector(), "predicted": {"kind": "value", "step_id": "s6", "path": "status", "value": {"state": "present", "value": 200}}}),
    ];
    fx.readback(fx.task1, &scope, predictions);
    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    let first = fx.failing_receipt(&scope, &prepared, 10, 7);
    fx.send_receipt(run, first).expect("recorded");
    {
        let world = fx.origin.world();
        let record = world
            .lane(fx.order)
            .and_then(|lane| lane.latest_receipt())
            .expect("a receipt");
        assert!(
            matches!(record.verdict, LaneVerdict::Blocked { .. }),
            "the failing item blocks despite the held prediction"
        );
        let outcomes: Vec<&ScoreOutcome> = record.scores.iter().map(|s| &s.outcome).collect();
        assert!(
            matches!(outcomes[0], ScoreOutcome::Held { items } if items.len() == 1),
            "the change prediction held: {outcomes:?}"
        );
        assert!(
            matches!(outcomes[1], ScoreOutcome::Missed { items } if items.len() == 1),
            "the value prediction missed: {outcomes:?}"
        );
    }
    let (run2, prepared2) = fx.prepared_run(&scope, fx.task1);
    let second = fx.passing_receipt(&scope, &prepared2);
    fx.send_receipt(run2, second).expect("recorded");
    let world = fx.origin.world();
    let record = world
        .lane(fx.order)
        .and_then(|lane| lane.latest_receipt())
        .expect("a receipt");
    assert_eq!(record.verdict, LaneVerdict::Passed);
    assert!(
        record
            .scores
            .iter()
            .any(|s| matches!(s.outcome, ScoreOutcome::Missed { .. })),
        "the missed prediction is on record next to the pass it never decided"
    );
    fx.assert_replays();
}

#[test]
fn fixed_means_the_original_obligation_passes_fresh() {
    let mut fx = Fx::new();
    let scope = fx.open_lane();
    fx.readback(fx.task1, &scope, vec![]);
    let (run, prepared) = fx.prepared_run(&scope, fx.task1);
    let first = fx.failing_receipt(&scope, &prepared, 10, 7);
    fx.send_receipt(run, first).expect("recorded");
    // The item passes but says nothing about the obligation that failed: not fixed.
    let (run2, prepared2) = fx.prepared_run(&scope, fx.task1);
    let silent = fx.receipt(
        &scope,
        &prepared2,
        vec![item(
            "obligation_fulfilled",
            None,
            7,
            changed(7),
            json!({"result": "unknown", "reason": {"reason": "incomplete"}}),
        )],
        counts(&[("obligation_fulfilled", 1)]),
        json!({"obligations_total": 1, "passed": 0, "failed": 0, "unknown": 1}),
        vec![],
    );
    fx.send_receipt(run2, silent).expect("recorded");
    let (_, state) = fx.lane_episode(10);
    assert_eq!(state, EpisodeState::Bounced, "silence is not a fix");
    // The original obligation passing in a fresh run is.
    let (run3, prepared3) = fx.prepared_run(&scope, fx.task1);
    let third = fx.passing_receipt(&scope, &prepared3);
    fx.send_receipt(run3, third).expect("recorded");
    let (_, state) = fx.lane_episode(10);
    assert_eq!(state, EpisodeState::Fixed);
    fx.assert_replays();
}

/// The draft a model returns for the fixture order: the body `scope_json(1)` holds, at revision
/// 1, with the model that proposed it.
fn draft_of(fx: &Fx) -> ScopeDraft {
    let scope = fx.scope_json(1, narrow_selector());
    from(json!({
        "version": "acan-scope-draft/1",
        "order": fx.order.to_string(),
        "rev": 1,
        "base_commit": BASE,
        "sentence": "limit failed logins to five a minute",
        "machine_id": "api",
        "draft": {
            "provenance": "model_proposal",
            "model": "muse-spark",
            "proposal": scope["body"].clone()
        },
        "drafted_at": 1000
    }))
}

fn drafted(fx: &Fx) -> SystemCmd {
    SystemCmd::ScopeDrafted {
        order: fx.order,
        rev: OrderRev::new(0),
        draft: Box::new(draft_of(fx)),
    }
}

fn draft_job(fx: &Fx) -> Job {
    Job::DraftScope {
        order: fx.order,
        rev: OrderRev::new(0),
    }
}

#[test]
fn a_scopeless_order_derives_a_scope_draft_job() {
    let fx = Fx::new();
    let world = fx.origin.world();
    assert!(
        world.jobs().contains(&draft_job(&fx)),
        "a scope-less order asks for a draft: {:?}",
        world.jobs()
    );
}

#[test]
fn a_stored_draft_confirms_nothing_and_ends_the_draft_job() {
    let mut fx = Fx::new();
    fx.system(drafted(&fx)).expect("the draft is stored");
    let owner = fx.owner.clone();
    let scope: Scope = from(fx.scope_json(1, narrow_selector()));
    let confirmation = fx.confirmation_for(&scope);
    let world = fx.origin.world();
    assert!(
        !world.jobs().contains(&draft_job(&fx)),
        "the draft ends the job"
    );
    assert!(world.lane(fx.order).is_none(), "a draft opens no lane");
    let Some((rev, ScopeDraftOutcome::Drafted { draft })) = world.scope_draft(fx.order) else {
        panic!("the draft is stored: {:?}", world.scope_draft(fx.order));
    };
    assert_eq!(rev, OrderRev::new(0));
    assert_eq!(draft.draft.proposal().may.len(), 1, "the proposal is kept");
    let confirm = fx.human(
        &owner,
        HumanCmd::ConfirmScope {
            order: fx.order,
            confirmation: Box::new(confirmation),
        },
    );
    assert!(confirm.is_err(), "a draft cannot be confirmed: {confirm:?}");
    let agent = fx.agent_a.clone();
    let claim = fx.agent(&agent, AgentCmd::Claim { task: fx.task1 });
    assert!(claim.is_err(), "a draft grants no claim: {claim:?}");
    assert!(matches!(
        fx.origin.world().task(fx.task1).map(|t| &t.state),
        Some(TaskState::Open)
    ));
    fx.assert_replays();
}

#[test]
fn a_human_adopts_the_draft_by_revising_and_confirming_it() {
    let mut fx = Fx::new();
    let draft = draft_of(&fx);
    let adopted = HumanSpec::adopt(
        draft.draft.clone(),
        HumanRef::from_str("owner").expect("a member id"),
        UnixMillis(2000),
    );
    assert_eq!(
        adopted.value(),
        draft.draft.proposal(),
        "adoption keeps the proposal's payload"
    );
    let drafted_cmd = SystemCmd::ScopeDrafted {
        order: fx.order,
        rev: OrderRev::new(0),
        draft: Box::new(draft.clone()),
    };
    fx.system(drafted_cmd).expect("the draft is stored");
    let scope = Scope {
        version: ScopeV1,
        order: draft.order.clone(),
        rev: draft.rev,
        base_commit: draft.base_commit.clone(),
        body: adopted.value().clone(),
    };
    fx.revise(&scope);
    fx.confirm(&scope);
    fx.readback(fx.task1, &scope, vec![]);
    fx.claim_scoped(fx.task1);
    assert!(matches!(
        fx.origin.world().task(fx.task1).map(|t| &t.state),
        Some(TaskState::Claimed { .. })
    ));
    assert!(
        matches!(
            fx.origin.world().scope_draft(fx.order),
            Some((_, ScopeDraftOutcome::Drafted { .. }))
        ),
        "the draft stays the record of what the model proposed"
    );
    fx.assert_replays();
}

#[test]
fn a_draft_for_a_scoped_order_or_an_old_revision_is_refused() {
    let mut fx = Fx::new();
    let old = SystemCmd::ScopeDrafted {
        order: fx.order,
        rev: OrderRev::new(3),
        draft: Box::new(draft_of(&fx)),
    };
    assert!(matches!(
        fx.system(old),
        Err(Rejection::StaleRevision { .. })
    ));
    fx.open_lane();
    let late = fx.system(drafted(&fx));
    assert!(late.is_err(), "a scoped order takes no draft: {late:?}");
    fx.assert_replays();
}

#[test]
fn a_failed_draft_is_recorded_and_the_job_stays_derived() {
    let mut fx = Fx::new();
    fx.system(SystemCmd::ScopeDraftFailed {
        order: fx.order,
        rev: OrderRev::new(0),
        reason: "the foreman's reply did not decode".to_owned(),
    })
    .expect("the failure is recorded");
    let world = fx.origin.world();
    assert!(
        matches!(
            world.scope_draft(fx.order),
            Some((_, ScopeDraftOutcome::Failed { reason })) if reason.contains("did not decode")
        ),
        "the failure is visible"
    );
    assert!(world.lane(fx.order).is_none());
    assert!(
        world.jobs().contains(&draft_job(&fx)),
        "a restart asks the foreman again"
    );
    fx.assert_replays();
}

#[test]
fn the_draft_events_keep_their_log_encoding() {
    let fx = Fx::new();
    let drafted = Event::ScopeDrafted {
        order: fx.order,
        rev: OrderRev::new(0),
        draft: Box::new(draft_of(&fx)),
    };
    let value = serde_json::to_value(&drafted).expect("the draft encodes");
    assert_eq!(value["type"], "scope_drafted");
    let back: Event = serde_json::from_value(value).expect("the draft decodes");
    assert_eq!(back, drafted);
    let failed = Event::ScopeDraftFailed {
        order: fx.order,
        rev: OrderRev::new(0),
        reason: "no reply".to_owned(),
    };
    let value = serde_json::to_value(&failed).expect("the failure encodes");
    assert_eq!(value["type"], "scope_draft_failed");
    assert_eq!(
        serde_json::from_value::<Event>(value).expect("the failure decodes"),
        failed
    );
}
