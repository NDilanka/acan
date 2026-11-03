mod common;

use acan_core::{
    Actor, AgentCmd, Check, Command, CommitId, Ctx, Event, Explanation, Fence, HumanCmd, Judgement,
    OrderPhase, OrderRev, Origin, Rejection, ReplayError, Replica, Settlement, Stage, Stamped,
    Step, SystemCmd, TaskId, TaskSpec, TaskState, VisionCounts, VisionStatus, WitnessOutcome,
    WitnessSpec,
};
use common::{Fx, events, machine, stored, sym};

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
    fx.system(SystemCmd::ModelUsed {
        model: "muse-spark".to_owned(),
        provider: None,
        step: Step::Split,
        input_tokens: Some(900),
        output_tokens: Some(120),
        failure: None,
        detail: None,
    })
    .expect("usage recorded");

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
    fx.system(SystemCmd::ClashExplained {
        clash,
        explanation: Explanation {
            consequence: "both rewrite the limiter".to_owned(),
            breaks: None,
            suggested: Some(Settlement::Serialize {
                first: t1,
                then: t2,
            }),
        },
    })
    .expect("explanation accepted");
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
fn a_log_with_the_old_model_used_shape_replays() {
    // A log as the server wrote it before providers, failures and details: a usage row has no
    // provider, failure or detail key, and its token counts are plain numbers. The log is written
    // out here, so the old shape does not depend on the current writer.
    let old = r#"[
        {"seq":0,"at":1000,"actor":{"kind":"system"},"event":{"type":"world_opened","world":{"id":"w","repo":"demo"},"founder":"owner"}},
        {"seq":1,"at":2000,"actor":{"kind":"system"},"event":{"type":"model_used","model":"muse-spark","step":"split","input_tokens":900,"output_tokens":120}}
    ]"#;
    let stored: Vec<Stamped> = serde_json::from_str(old).expect("the old shape parses");
    assert!(
        matches!(
            stored[1].event,
            Event::ModelUsed {
                provider: None,
                failure: None,
                detail: None,
                ..
            }
        ),
        "an old row decodes with no provider, failure or detail: {:?}",
        stored[1].event
    );

    let replica = Replica::from_log(stored).expect("the old shape folds");
    let count = replica.world().usage().totals["muse-spark"][&Step::Split];
    assert_eq!(
        (count.input, count.output, count.calls),
        (900, 120, 1),
        "an old row is counted as a reply"
    );
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
fn a_vision_check_names_its_own_gate_run_and_changes_nothing_the_gate_reads() {
    let mut fx = full_loop();
    let (task, run) = fx
        .log
        .iter()
        .find_map(|stamped| match &stamped.event {
            Event::BuildReported { task, run, .. } => Some((*task, *run)),
            _ => None,
        })
        .expect("the loop reported a build");
    let other = if task == fx.task1 { fx.task2 } else { fx.task1 };
    let check = |task| SystemCmd::VisionChecked {
        task,
        gate_run: run,
        status: VisionStatus::Done,
        counts: VisionCounts {
            inconsistent: 1,
            ..VisionCounts::default()
        },
        human_questions: 1,
    };
    assert!(
        matches!(fx.system(check(other)), Err(Rejection::Unknown { .. })),
        "a check naming another task's run is refused"
    );
    let gate = fx.origin.world().gate(run).cloned();
    let phase_before = phase(&fx);
    let batch = fx.system(check(task)).expect("the check is recorded");
    assert!(matches!(
        events(&batch).as_slice(),
        [Event::VisionChecked { .. }]
    ));
    assert_eq!(fx.origin.world().gate(run).cloned(), gate);
    assert_eq!(phase(&fx), phase_before);

    let json = serde_json::to_string(&fx.log).expect("log serializes");
    assert!(json.contains(r#""type":"vision.checked""#), "{json}");
    let stored: Vec<Stamped> = serde_json::from_str(&json).expect("log parses");
    let replica = Replica::from_log(stored).expect("log folds");
    assert_eq!(replica.world(), fx.origin.world());
}
