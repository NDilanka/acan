mod common;

use acan_core::{
    AgentCmd, Check, Event, HumanCmd, Job, Judgement, LeaseEnd, OrderId, OrderPhase, OrderRev,
    OrderState, ReadbackVerdict, Rejection, ReplayError, Replica, SystemCmd, TaskSpec, TaskState,
    WitnessKind, WitnessSpec,
};
use common::{Fx, events, machine};

#[test]
fn a_posted_order_mints_locked_witnesses_and_derives_a_split_job() {
    let mut fx = Fx::new();
    let owner = fx.owner.clone();
    let witnesses = vec![WitnessSpec {
        check: Check::Test {
            command: "cargo test".to_owned(),
        },
        reason: "suite stays green".to_owned(),
    }];
    fx.human(
        &owner,
        HumanCmd::PostOrder {
            machine: machine("auth"),
            sentence: "Rotate session keys".to_owned(),
            witnesses,
        },
    )
    .expect("order posted");
    let world = fx.origin.world();
    let order = world.orders().last().expect("second order");
    assert_eq!(
        (order.rev, order.state),
        (OrderRev::new(0), OrderState::Splitting)
    );
    assert_eq!(world.phase(order.id), Some(OrderPhase::Splitting));
    let witness = world.witness(order.witnesses[0]).expect("witness minted");
    assert!(matches!(witness.kind, WitnessKind::Check { .. }));
    assert_eq!(witness.pinned_by, fx.owner);
    assert!(world.jobs().contains(&Job::Split {
        order: order.id,
        rev: OrderRev::new(0)
    }));
}

#[test]
fn a_split_order_reads_back_and_derives_no_review_job() {
    let mut fx = Fx::new();
    assert_eq!(
        fx.origin.world().phase(fx.order),
        Some(OrderPhase::ReadingBack)
    );
    let a = fx.agent_a.clone();
    fx.read_back(&a, fx.task1);
    assert_eq!(
        fx.origin.world().jobs(),
        vec![Job::DraftScope {
            order: fx.order,
            rev: OrderRev::new(0)
        }],
        "the legacy review is retired: a readback derives no review job, only the scope draft"
    );
}

#[test]
fn a_second_split_result_is_rejected() {
    let mut fx = Fx::new();
    let spec = TaskSpec {
        title: "again".to_owned(),
        brief: String::new(),
        scope: vec![machine("api")],
    };
    let got = fx.system(SystemCmd::SplitReady {
        order: fx.order,
        rev: OrderRev::new(0),
        tasks: vec![spec],
    });
    assert!(
        matches!(got, Err(Rejection::WrongState { .. })),
        "got {got:?}"
    );
}

#[test]
fn a_review_for_an_old_revision_is_rejected() {
    let mut fx = Fx::new();
    let a = fx.agent_a.clone();
    let readback = fx.read_back(&a, fx.task1);
    fx.ask(readback).expect("question pinned");
    let question = fx
        .origin
        .world()
        .open_question(fx.order)
        .expect("open question")
        .id;
    let owner = fx.owner.clone();
    fx.human(
        &owner,
        HumanCmd::AnswerQuestion {
            question,
            option: 0,
        },
    )
    .expect("answer accepted");
    let got = fx.system(SystemCmd::ReadbacksReviewed {
        order: fx.order,
        rev: OrderRev::new(0),
        verdicts: vec![],
        question: None,
    });
    assert_eq!(
        got,
        Err(Rejection::StaleRevision {
            current: OrderRev::new(1)
        })
    );
}

#[test]
fn disagreeing_readbacks_pin_exactly_one_question() {
    let mut fx = Fx::new();
    let (a, b) = (fx.agent_a.clone(), fx.agent_b.clone());
    let first = fx.read_back(&a, fx.task1);
    let second = fx.read_back(&b, fx.task2);
    fx.ask(first).expect("first question pinned");
    let question = fx
        .origin
        .world()
        .open_question(fx.order)
        .expect("open question")
        .id;
    let late = fx.ask(second);
    assert_eq!(late, Err(Rejection::OpenQuestion { question }));
    let pinned = events(&fx.log)
        .iter()
        .filter(|e| matches!(e, Event::QuestionPinned { .. }))
        .count();
    assert_eq!(pinned, 1, "a second pin");
    assert_eq!(
        fx.origin.world().phase(fx.order),
        Some(OrderPhase::Asking { question })
    );
}

#[test]
fn a_claim_while_a_question_is_open_is_rejected() {
    let mut fx = Fx::new();
    let (a, b, t1) = (fx.agent_a.clone(), fx.agent_b.clone(), fx.task1);
    let matched = fx.read_back(&a, t1);
    fx.review(matched, Judgement::Match)
        .expect("review accepted");
    let unclear = fx.read_back(&b, fx.task2);
    fx.ask(unclear).expect("question pinned");
    let question = fx
        .origin
        .world()
        .open_question(fx.order)
        .expect("open question")
        .id;
    assert_eq!(
        fx.agent(&a, AgentCmd::Claim { task: t1 }),
        Err(Rejection::OpenQuestion { question })
    );
}

#[test]
fn answering_adds_a_ruling_bumps_the_revision_and_supersedes_live_leases() {
    let mut fx = Fx::new();
    let (a, b, t1) = (fx.agent_a.clone(), fx.agent_b.clone(), fx.task1);
    fx.claim(&a, t1);
    let unclear = fx.read_back(&b, fx.task2);
    fx.ask(unclear).expect("question pinned");
    let question = fx
        .origin
        .world()
        .open_question(fx.order)
        .expect("open question")
        .id;
    let owner = fx.owner.clone();
    let batch = fx
        .human(
            &owner,
            HumanCmd::AnswerQuestion {
                question,
                option: 1,
            },
        )
        .expect("answer accepted");
    let got = events(&batch);
    assert!(
        got.iter()
            .any(|e| matches!(e, Event::QuestionAnswered { witness, .. }
        if matches!(&witness.kind, WitnessKind::Ruling { text, .. } if text == "Retry once")))
    );
    assert!(
        got.iter()
            .any(|e| matches!(e, Event::OrderRevised { rev, .. } if *rev == OrderRev::new(1)))
    );
    assert!(got.iter().any(
        |e| matches!(e, Event::LeaseEnded { why: LeaseEnd::Superseded, task, .. } if *task == t1)
    ));
    let order = fx.origin.world().order(fx.order).expect("order");
    assert_eq!((order.rev, order.witnesses.len()), (OrderRev::new(1), 2));
    assert_eq!(
        fx.origin.world().task(t1).expect("task").state,
        TaskState::Open
    );
    assert!(fx.origin.world().open_question(fx.order).is_none());
}

#[test]
fn a_verdict_from_an_old_revision_no_longer_lets_its_agent_claim() {
    let mut fx = Fx::new();
    let (a, b, t1) = (fx.agent_a.clone(), fx.agent_b.clone(), fx.task1);
    let matched = fx.read_back(&a, t1);
    fx.review(matched, Judgement::Match)
        .expect("review accepted");
    let unclear = fx.read_back(&b, fx.task2);
    fx.ask(unclear).expect("question pinned");
    let question = fx
        .origin
        .world()
        .open_question(fx.order)
        .expect("open question")
        .id;
    let owner = fx.owner.clone();
    fx.human(
        &owner,
        HumanCmd::AnswerQuestion {
            question,
            option: 0,
        },
    )
    .expect("answer accepted");
    assert_eq!(
        fx.agent(&a, AgentCmd::Claim { task: t1 }),
        Err(Rejection::NotMatched {
            failing: fx.witnesses()
        })
    );
}

#[test]
fn cancelling_drops_every_task_and_ends_live_leases() {
    let mut fx = Fx::new();
    let (a, t1, t2) = (fx.agent_a.clone(), fx.task1, fx.task2);
    fx.claim(&a, t1);
    let owner = fx.owner.clone();
    let batch = fx
        .human(&owner, HumanCmd::CancelOrder { order: fx.order })
        .expect("cancel accepted");
    assert!(events(&batch).iter().any(|e| matches!(
        e,
        Event::LeaseEnded {
            why: LeaseEnd::Cancelled,
            ..
        }
    )));
    for t in [t1, t2] {
        assert_eq!(
            fx.origin.world().task(t).expect("task").state,
            TaskState::Dropped
        );
    }
    assert_eq!(
        fx.origin.world().phase(fx.order),
        Some(OrderPhase::Cancelled)
    );
}

#[test]
fn a_replica_folding_the_log_agrees_with_the_origin() {
    let mut fx = Fx::new();
    let a = fx.agent_a.clone();
    fx.claim(&a, fx.task1);
    let replica = Replica::from_log(fx.log.clone()).expect("log folds");
    assert_eq!(replica.world(), fx.origin.world());
    assert_eq!(replica.next_seq(), fx.origin.world().next_seq());
}

#[test]
fn a_replica_ignores_duplicates_and_reports_gaps() {
    let fx = Fx::new();
    let mut replica = Replica::from_log(fx.log[..2].to_vec()).expect("prefix folds");
    replica.ingest(&fx.log[1]).expect("a duplicate is ignored");
    assert_eq!(
        replica.ingest(&fx.log[3]),
        Err(ReplayError::SeqGap {
            expected: fx.log[2].seq,
            got: fx.log[3].seq
        })
    );
}

#[test]
fn a_log_that_does_not_open_the_world_is_refused() {
    let fx = Fx::new();
    assert_eq!(
        Replica::from_log(fx.log[1..].to_vec()).err(),
        Some(ReplayError::NotOpened)
    );
}

#[test]
fn only_the_poster_or_an_owner_cancels() {
    let mut fx = Fx::new();
    let bob = fx.bob.clone();
    let got = fx.human(&bob, HumanCmd::CancelOrder { order: fx.order });
    assert!(
        matches!(got, Err(Rejection::Forbidden { .. })),
        "got {got:?}"
    );
}

#[test]
fn an_empty_split_is_rejected() {
    let mut fx = Fx::new();
    let owner = fx.owner.clone();
    fx.human(
        &owner,
        HumanCmd::PostOrder {
            machine: machine("auth"),
            sentence: "Rotate session keys".to_owned(),
            witnesses: vec![],
        },
    )
    .expect("order posted");
    let order = fx.origin.world().orders().last().expect("second order").id;
    let got = fx.system(SystemCmd::SplitReady {
        order,
        rev: OrderRev::new(0),
        tasks: vec![],
    });
    assert!(matches!(got, Err(Rejection::Invalid { .. })), "got {got:?}");
}

#[test]
fn a_late_review_cannot_rewrite_the_verdict_a_live_claim_stands_on() {
    let mut fx = Fx::new();
    let (a, t1) = (fx.agent_a.clone(), fx.task1);
    fx.claim(&a, t1);
    let TaskState::Claimed { claim, .. } = &fx.origin.world().task(t1).expect("task").state else {
        panic!("claimed");
    };
    let readback = claim.readback;
    let late = fx.review(
        readback,
        Judgement::Unclear {
            reason: "late retry".to_owned(),
        },
    );
    assert!(late.is_err(), "late review accepted: {late:?}");
    let world = fx.origin.world();
    let order = world.order(fx.order).expect("order");
    let verdict = world
        .readback(readback)
        .and_then(|r| r.verdict.as_ref())
        .expect("verdict");
    assert!(verdict.matches(order), "the live claim's verdict changed");
}

#[test]
fn a_review_naming_a_readback_twice_is_rejected() {
    let mut fx = Fx::new();
    let a = fx.agent_a.clone();
    let readback = fx.read_back(&a, fx.task1);
    let order = fx.origin.world().order(fx.order).expect("order");
    let verdict = ReadbackVerdict {
        rev: order.rev,
        per_witness: order
            .witnesses
            .iter()
            .map(|&w| (w, Judgement::Match))
            .collect(),
    };
    let rev = order.rev;
    let got = fx.system(SystemCmd::ReadbacksReviewed {
        order: fx.order,
        rev,
        verdicts: vec![(readback, verdict.clone()), (readback, verdict)],
        question: None,
    });
    assert!(matches!(got, Err(Rejection::Invalid { .. })), "{got:?}");
}

#[test]
fn revision_exhaustion_in_a_stored_log_rejects_the_answer() {
    let mut fx = Fx::new();
    let a = fx.agent_a.clone();
    let readback = fx.read_back(&a, fx.task1);
    fx.ask(readback).expect("question pinned");
    let question = fx
        .origin
        .world()
        .open_question(fx.order)
        .expect("open question")
        .id;
    let (order, witnesses) = (fx.order, fx.witnesses());
    fx.append_stored(Event::OrderRevised {
        order,
        rev: OrderRev::new(u32::MAX),
        witnesses,
    })
    .expect("the stored log replays");
    let owner = fx.owner.clone();
    let got = fx.human(
        &owner,
        HumanCmd::AnswerQuestion {
            question,
            option: 0,
        },
    );
    assert!(matches!(got, Err(Rejection::Invalid { .. })), "{got:?}");
}

#[test]
fn an_exhausted_order_counter_rejects_a_new_order_instead_of_reusing_an_id() {
    let mut fx = Fx::new();
    let max: OrderId = "o-4294967295".parse().expect("valid order id");
    let owner = fx.owner.clone();
    fx.append_stored(Event::OrderPosted {
        order: max,
        machine: machine("api"),
        sentence: "keep this order".to_owned(),
        witnesses: vec![],
        posted_by: owner.clone(),
    })
    .expect("the stored log replays");
    let before = fx.origin.world().order(max).expect("order").clone();
    let got = fx.human(
        &owner,
        HumanCmd::PostOrder {
            machine: machine("api"),
            sentence: "new order".to_owned(),
            witnesses: vec![],
        },
    );
    assert!(matches!(got, Err(Rejection::Invalid { .. })), "{got:?}");
    assert_eq!(fx.origin.world().order(max), Some(&before));
}
