mod common;

use acan_core::{
    Actor, AgentCmd, Check, Command, CommitId, Ctx, Event, Fence, HumanCmd, Judgement, OrderPhase,
    OrderRev, Origin, ReplayError, Replica, Seq, Settlement, Stage, Stamped, SystemCmd, TaskId,
    TaskSpec, TaskState, UnknownEvent, WitnessOutcome, WitnessSpec,
};
use common::{Fx, events, machine, stored, sym};
use serde_json::{Value, json};

fn commit(raw: &str) -> CommitId {
    raw.parse().expect("valid commit")
}

fn phase(fx: &Fx) -> OrderPhase {
    fx.origin.world().phase(fx.order).expect("order exists")
}

fn gate_once(
    fx: &mut Fx,
    agent: &acan_core::AgentId,
    fence: Fence,
    head: &str,
    outcome: &WitnessOutcome,
) {
    fx.agent(
        agent,
        AgentCmd::ReportBuilt {
            fence,
            head: commit(head),
        },
    )
    .expect("report accepted");
    assert_eq!(phase(fx), OrderPhase::Gating);
    let run = match &fx.origin.world().task(fence.task).expect("task").state {
        TaskState::Claimed {
            stage: Stage::Gating { run, .. },
            ..
        } => *run,
        other => panic!("not gating: {other:?}"),
    };
    let gate = fx.origin.world().gate(run).expect("gate run").clone();
    for witness in gate.witnesses {
        fx.system(SystemCmd::WitnessResult {
            run,
            witness,
            outcome: outcome.clone(),
        })
        .expect("result accepted");
    }
}

fn land(fx: &mut Fx, task: TaskId, head: &str, merged: &str) {
    fx.system(SystemCmd::LandResult {
        task,
        head: commit(head),
        result: Ok(commit(merged)),
    })
    .expect("land accepted");
}

/// Drives one order through every step of the loop and returns the fixture with its full log.
fn full_loop() -> Fx {
    let mut fx = Fx::new();
    let (owner, a, b, t1, t2) = (
        fx.owner.clone(),
        fx.agent_a.clone(),
        fx.agent_b.clone(),
        fx.task1,
        fx.task2,
    );
    assert_eq!(phase(&fx), OrderPhase::ReadingBack);
    fx.system(SystemCmd::IndexUpdated).expect("first scan");

    let first = fx.read_back(&a, t1);
    fx.review(first, Judgement::Match).expect("review accepted");
    let unclear = fx.read_back(&b, t2);
    fx.ask(unclear).expect("question pinned");
    let question = fx
        .origin
        .world()
        .open_question(fx.order)
        .expect("open question")
        .id;
    assert_eq!(phase(&fx), OrderPhase::Asking { question });
    fx.human(
        &owner,
        HumanCmd::AnswerQuestion {
            question,
            option: 0,
        },
    )
    .expect("answer accepted");

    let f1 = fx.claim(&a, t1);
    let f2 = fx.claim(&b, t2);
    let limiter = || acan_core::DeclarationInput {
        symbols: vec![sym("src/api/routes.rs", "rate_limit")],
        ..Default::default()
    };
    fx.declare(&a, f1, limiter()).expect("declare accepted");
    let batch = fx.declare(&b, f2, limiter()).expect("declare accepted");
    assert_eq!(
        events(&batch)
            .iter()
            .filter(|e| matches!(e, Event::ClashFound { .. }))
            .count(),
        1,
        "exactly one clash"
    );
    assert_eq!(phase(&fx), OrderPhase::Clashing);
    let clash = fx.origin.world().clashes().next().expect("clash").id;
    fx.human(
        &owner,
        HumanCmd::SettleClash {
            clash,
            choice: Settlement::Serialize {
                first: t1,
                then: t2,
            },
        },
    )
    .expect("settle accepted");
    assert_eq!(phase(&fx), OrderPhase::Building);

    let fail = WitnessOutcome::Fail {
        detail: "login test failed".to_owned(),
    };
    gate_once(&mut fx, &a, f1, "aaa000", &fail);
    gate_once(&mut fx, &a, f1, "aaa111", &WitnessOutcome::Pass);
    land(&mut fx, t1, "aaa111", "m1");
    gate_once(&mut fx, &b, f2, "bbb222", &WitnessOutcome::Pass);
    assert_eq!(phase(&fx), OrderPhase::Landing);
    land(&mut fx, t2, "bbb222", "m2");
    assert_eq!(phase(&fx), OrderPhase::Landed);
    fx
}

#[test]
fn replaying_the_full_loop_rebuilds_the_same_world() {
    let fx = full_loop();
    let replica = Replica::from_log(fx.log.clone()).expect("log folds");
    assert_eq!(replica.world(), fx.origin.world());
    let rebuilt = Origin::replay(fx.log.clone()).expect("log folds");
    assert_eq!(rebuilt.world(), fx.origin.world());
}

#[test]
fn streaming_the_full_loop_event_by_event_rebuilds_the_same_world() {
    let fx = full_loop();
    let mut replica = Replica::from_log(fx.log[..1].to_vec()).expect("opening folds");
    for stamped in &fx.log[1..] {
        replica.ingest(stamped).expect("in-order event applies");
    }
    assert_eq!(replica.world(), fx.origin.world());
}

#[test]
fn the_stored_json_replays_to_the_same_world() {
    let fx = full_loop();
    let json = serde_json::to_string(&fx.log).expect("log serializes");
    let stored: Vec<acan_core::Stamped> = serde_json::from_str(&json).expect("log parses");
    let replica = Replica::from_log(stored).expect("log folds");
    assert_eq!(replica.world(), fx.origin.world());
}

#[test]
fn a_stored_event_naming_an_absent_order_is_refused_at_replay() {
    let mut fx = Fx::new();
    let task: TaskId = "t-99".parse().expect("valid task id");
    let got = fx.append_stored(Event::OrderSplit {
        order: "o-999".parse().expect("valid order id"),
        rev: OrderRev::new(0),
        tasks: vec![(
            task,
            TaskSpec {
                title: "orphan".to_owned(),
                brief: String::new(),
                scope: vec![],
            },
        )],
    });
    assert!(
        matches!(got, Err(ReplayError::InvalidEvent { .. })),
        "{got:?}"
    );
    let a = fx.agent_a.clone();
    assert!(fx.agent(&a, AgentCmd::Claim { task }).is_err());
}

#[test]
fn a_streamed_event_naming_an_absent_task_is_refused_and_not_applied() {
    let fx = Fx::new();
    let mut replica = Replica::from_log(fx.log.clone()).expect("log folds");
    let forged = Stamped {
        seq: replica.next_seq(),
        at: fx.now,
        actor: Actor::System,
        shareable: false,
        event: Event::BuildCleared {
            task: "t-99".parse().expect("valid task id"),
        },
    };
    assert!(matches!(
        replica.ingest(&forged),
        Err(ReplayError::InvalidEvent { .. })
    ));
    assert_eq!(replica.world(), fx.origin.world());
}

#[test]
fn json_replay_then_further_commands_allocate_the_same_ids() {
    let full = full_loop();
    let mut replayed = Origin::replay(stored(&full.log)).expect("log folds");
    let mut fx = full;
    let cmd = Command::Human {
        member: fx.owner.clone(),
        cmd: HumanCmd::PostOrder {
            machine: machine("api"),
            sentence: "next order".to_owned(),
            witnesses: vec![WitnessSpec {
                check: Check::Test {
                    command: "test".to_owned(),
                },
                reason: "next witness".to_owned(),
            }],
        },
    };
    let ctx = Ctx {
        now: fx.now.plus_millis(1),
        index: &fx.index,
    };
    let expected = fx.origin.execute(&ctx, cmd.clone()).expect("order posted");
    let actual = replayed.execute(&ctx, cmd).expect("order posted");
    assert_eq!(expected, actual);
    assert_eq!(fx.origin.world(), replayed.world());
}

#[test]
fn unknown_events_round_trip_and_fold_without_changing_known_state() {
    let fx = Fx::new();
    let mut json = serde_json::to_value(&fx.log).expect("the log encodes");
    let next = fx.origin.world().next_seq().get();
    let future = json!({
        "cursor": [3, null, {"ready": true}],
        "payload": {"type": "nested", "watermark": u64::MAX}
    });
    let usage =
        json!({"model": "example", "step": "split", "input_tokens": 900, "output_tokens": 120});
    let draft = json!({"order": 999, "rev": 0, "draft": {"proposal": "legacy"}});
    json.as_array_mut().expect("a log array").extend([
        json!({
            "seq": next, "at": 2000, "actor": {"kind": "member", "id": "other"},
            "shareable": true,
            "event": {
                "type": "team_sync_checkpoint",
                "cursor": future["cursor"],
                "payload": future["payload"]
            }
        }),
        json!({
            "seq": next + 1, "at": 2001, "actor": {"kind": "system"}, "shareable": false,
            "event": {"type": "index_updated", "rev": "rev-2"}
        }),
        json!({
            "seq": next + 2, "at": 2002, "actor": {"kind": "system"}, "shareable": false,
            "event": {"type": "model_used", "model": "example", "step": "split",
                      "input_tokens": 900, "output_tokens": 120}
        }),
        json!({
            "seq": next + 3, "at": 2003, "actor": {"kind": "system"}, "shareable": false,
            "event": {"type": "scope_drafted", "order": 999, "rev": 0, "draft": {"proposal": "legacy"}}
        }),
    ]);
    let log: Vec<Stamped> = serde_json::from_value(json.clone()).expect("the mixed log decodes");
    assert_eq!(
        serde_json::to_value(&log).expect("the log re-encodes"),
        json
    );

    let origin = Origin::replay(stored(&log)).expect("the mixed log replays");
    let world = origin.world();
    assert_eq!(world.next_seq(), Seq::new(next + 4));
    assert_eq!(
        world.index_rev().expect("the index advanced").as_str(),
        "rev-2"
    );
    assert_eq!(world.lane(fx.order), None);
    assert_eq!(world.orders().count(), 1);
    assert_eq!(
        world.unknown_events(),
        [
            UnknownEvent {
                seq: Seq::new(next),
                at: acan_core::Timestamp::from_millis(2000),
                actor: Actor::Member("other".parse().expect("valid member id")),
                shareable: true,
                event_type: "team_sync_checkpoint".to_owned(),
                payload: future,
            },
            UnknownEvent {
                seq: Seq::new(next + 2),
                at: acan_core::Timestamp::from_millis(2002),
                actor: Actor::System,
                shareable: false,
                event_type: "model_used".to_owned(),
                payload: usage,
            },
            UnknownEvent {
                seq: Seq::new(next + 3),
                at: acan_core::Timestamp::from_millis(2003),
                actor: Actor::System,
                shareable: false,
                event_type: "scope_drafted".to_owned(),
                payload: draft,
            },
        ]
    );

    let mut replica = Replica::from_log(fx.log).expect("the initial log folds");
    for stamped in &log[next as usize..] {
        replica
            .ingest(stamped)
            .expect("the rest of the log streams");
    }
    assert_eq!(replica.world(), world);
}

#[test]
fn the_recorded_replay_preserves_removed_event_kinds() {
    let recording = include_str!("../../../replay/events.json");
    let entries: Vec<Value> = serde_json::from_str(recording).expect("the recording is JSON");
    let log: Vec<Stamped> = serde_json::from_str(recording).expect("the recording decodes");
    let last = log.last().expect("the recording is not empty");
    let origin = Origin::replay(log.iter().cloned()).expect("the recording replays");
    assert_eq!(origin.world().next_seq().get(), last.seq.get() + 1);
    let unknown: Vec<_> = entries
        .into_iter()
        .filter_map(|mut entry| {
            let mut payload = entry["event"].take();
            let event_type = payload.as_object_mut()?.remove("type")?;
            let event_type = event_type.as_str()?;
            if !matches!(event_type, "model_used" | "scope_drafted") {
                return None;
            }
            Some((entry["seq"].as_u64()?, event_type.to_owned(), payload))
        })
        .collect();
    assert!(unknown.iter().any(|(_, kind, _)| kind == "model_used"));
    assert!(unknown.iter().any(|(_, kind, _)| kind == "scope_drafted"));
    assert_eq!(
        origin
            .world()
            .unknown_events()
            .iter()
            .map(|event| {
                assert!(!event.shareable, "this recording predates sharing");
                (
                    event.seq.get(),
                    event.event_type.clone(),
                    event.payload.clone(),
                )
            })
            .collect::<Vec<_>>(),
        unknown
    );
}
