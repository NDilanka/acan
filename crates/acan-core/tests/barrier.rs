//! The declare barrier: no task of an order builds until every sibling has declared, or is
//! queued, landed or dropped, or the barrier's deadline has passed.

mod common;

use acan_core::{
    AgentCmd, Barrier, DECLARE_BARRIER_MS, DeclarationInput, Event, HumanCmd, Job, LEASE_TTL_MS,
    OrderId, Replica, Stage, SystemCmd, TaskId, TaskSpec, TaskState, Timestamp,
};
use common::{Fx, events, machine, stored, sym};

fn routes(name: &str) -> DeclarationInput {
    DeclarationInput {
        symbols: vec![sym("src/api/routes.rs", name)],
        ..DeclarationInput::default()
    }
}

fn stage_of(fx: &Fx, task: TaskId) -> &'static str {
    match &fx.origin.world().task(task).expect("task").state {
        TaskState::Claimed { stage, .. } => match stage {
            Stage::Declaring => "declaring",
            Stage::Declared { .. } => "declared",
            Stage::Building { .. } => "building",
            Stage::Gating { .. } => "gating",
        },
        TaskState::Open => "open",
        TaskState::Queued { .. } => "queued",
        TaskState::Landed { .. } => "landed",
        TaskState::Dropped => "dropped",
    }
}

fn barrier_of(fx: &Fx, order: OrderId) -> Barrier {
    fx.origin.world().order(order).expect("order").barrier
}

fn barrier_jobs(fx: &Fx) -> Vec<Job> {
    fx.origin
        .world()
        .jobs()
        .into_iter()
        .filter(|job| matches!(job, Job::Barrier { .. }))
        .collect()
}

fn cleared(batch: &[acan_core::Stamped]) -> Vec<TaskId> {
    events(batch)
        .into_iter()
        .filter_map(|e| match e {
            Event::BuildCleared { task } => Some(*task),
            _ => None,
        })
        .collect()
}

/// Posts a second order split into `n` tasks and makes it the fixture's order, so `claim`
/// reviews against it.
fn order_of(fx: &mut Fx, n: usize) -> Vec<TaskId> {
    let owner = fx.owner.clone();
    fx.human(
        &owner,
        HumanCmd::PostOrder {
            machine: machine("api"),
            sentence: "Tidy the API".to_owned(),
            witnesses: Vec::new(),
        },
    )
    .expect("order posted");
    let order = fx.origin.world().orders().last().expect("order").id;
    let rev = fx.origin.world().order(order).expect("order").rev;
    let tasks = (0..n)
        .map(|i| TaskSpec {
            title: format!("part {i}"),
            brief: String::new(),
            scope: vec![machine("api")],
        })
        .collect();
    fx.human(&owner, HumanCmd::SplitOrder { order, rev, tasks })
        .expect("split accepted");
    fx.order = order;
    fx.origin.world().tasks_of(order).map(|t| t.id).collect()
}

#[test]
fn a_declared_task_waits_while_a_sibling_has_not_declared() {
    let mut fx = Fx::new();
    let (a, b, t1, t2, order) = (
        fx.agent_a.clone(),
        fx.agent_b.clone(),
        fx.task1,
        fx.task2,
        fx.order,
    );
    let f1 = fx.claim(&a, t1);
    let declared_at = fx.now;
    let batch = fx
        .declare(&a, f1, routes("rate_limit"))
        .expect("declare accepted");
    assert!(cleared(&batch).is_empty(), "nothing clears: {batch:?}");
    assert_eq!(stage_of(&fx, t1), "declared");

    let deadline = declared_at.plus_millis(DECLARE_BARRIER_MS);
    let hold = fx.origin.world().barrier(order).expect("the barrier holds");
    assert_eq!(hold.waiting_for, vec![t2], "it waits for the open sibling");
    assert_eq!(hold.deadline, deadline);
    assert_eq!(barrier_jobs(&fx), vec![Job::Barrier { order, deadline }]);
    let replica = Replica::from_log(stored(&fx.log)).expect("the log replays");
    assert_eq!(
        replica.world().barrier(order),
        Some(hold),
        "a replica sees the task waiting for its sibling"
    );

    fx.claim(&b, t2);
    let hold = fx.origin.world().barrier(order).expect("the barrier holds");
    assert_eq!(
        hold.waiting_for,
        vec![t2],
        "a claimed sibling that has not declared still holds it"
    );
    assert_eq!(stage_of(&fx, t1), "declared");
}

#[test]
fn the_last_sibling_declaring_releases_the_barrier() {
    let mut fx = Fx::new();
    let (a, b, t1, t2, order) = (
        fx.agent_a.clone(),
        fx.agent_b.clone(),
        fx.task1,
        fx.task2,
        fx.order,
    );
    let f1 = fx.claim(&a, t1);
    let f2 = fx.claim(&b, t2);
    fx.declare(&a, f1, routes("rate_limit"))
        .expect("declare accepted");
    let batch = fx
        .declare(&b, f2, routes("health"))
        .expect("declare accepted");
    assert_eq!(cleared(&batch), vec![t1, t2], "both clear together");
    assert_eq!(fx.origin.world().barrier(order), None);
    assert_eq!(barrier_of(&fx, order), Barrier::Lifted);
    assert!(
        barrier_jobs(&fx).is_empty(),
        "no deadline is left to wait for"
    );
}

#[test]
fn the_deadline_releases_the_barrier() {
    let mut fx = Fx::new();
    let (a, t1, order) = (fx.agent_a.clone(), fx.task1, fx.order);
    let f1 = fx.claim(&a, t1);
    let deadline = fx.now.plus_millis(DECLARE_BARRIER_MS);
    fx.declare(&a, f1, routes("rate_limit"))
        .expect("declare accepted");

    fx.advance(LEASE_TTL_MS / 2);
    fx.agent(&a, AgentCmd::Renew { fence: f1 })
        .expect("renew accepted");
    fx.now = Timestamp::from_millis(deadline.as_millis() - 1);
    fx.agent(&a, AgentCmd::Renew { fence: f1 })
        .expect("renew accepted");
    let early = fx.system(SystemCmd::Tick).expect("tick accepted");
    assert!(
        early.is_empty(),
        "the barrier holds until its deadline: {early:?}"
    );
    assert_eq!(stage_of(&fx, t1), "declared");

    fx.now = deadline;
    let batch = fx.system(SystemCmd::Tick).expect("tick accepted");
    let got = events(&batch);
    assert!(
        matches!(
            got.as_slice(),
            [Event::BarrierExpired { order: lifted }, Event::BuildCleared { task }]
                if *lifted == order && *task == t1
        ),
        "{got:?}"
    );
    assert_eq!(stage_of(&fx, t1), "building");
    assert_eq!(fx.origin.world().barrier(order), None);
    assert!(barrier_jobs(&fx).is_empty());
    let replica = Replica::from_log(stored(&fx.log)).expect("the log replays");
    assert_eq!(
        replica.world().order(order).expect("order").barrier,
        Barrier::Lifted
    );
}

#[test]
fn overlapping_siblings_clash_before_either_builds_and_stay_out_past_the_deadline() {
    let mut fx = Fx::new();
    let (a, b) = (fx.agent_a.clone(), fx.agent_b.clone());
    let tasks = order_of(&mut fx, 3);
    let (t1, t2, t3) = (tasks[0], tasks[1], tasks[2]);
    let order = fx.order;
    let f1 = fx.claim(&a, t1);
    let f2 = fx.claim(&b, t2);
    fx.declare(&a, f1, routes("rate_limit"))
        .expect("declare accepted");
    assert_eq!(
        stage_of(&fx, t1),
        "declared",
        "the first task has not built"
    );
    let batch = fx
        .declare(&b, f2, routes("rate_limit"))
        .expect("declare accepted");
    assert!(
        events(&batch)
            .iter()
            .any(|e| matches!(e, Event::ClashFound { .. })),
        "the overlap is a clash while both only hold declarations: {batch:?}"
    );
    assert!(cleared(&batch).is_empty());

    fx.run_out_barrier(order);
    assert_eq!(barrier_of(&fx, order), Barrier::Lifted);
    assert_eq!(stage_of(&fx, t1), "declared", "the clash still holds task1");
    assert_eq!(stage_of(&fx, t2), "declared", "the clash still holds task2");
    assert_eq!(stage_of(&fx, t3), "open");
    assert!(fx.origin.world().clashes().any(acan_core::Clash::blocking));

    let f3 = fx.claim(&a, t3);
    let batch = fx
        .declare(&a, f3, routes("health"))
        .expect("declare accepted");
    assert_eq!(
        cleared(&batch),
        vec![t3],
        "a lifted barrier does not hold a late sibling"
    );
}

#[test]
fn a_single_task_order_clears_on_its_own_declaration() {
    let mut fx = Fx::new();
    let a = fx.agent_a.clone();
    let task = order_of(&mut fx, 1)[0];
    let order = fx.order;
    let fence = fx.claim(&a, task);
    let batch = fx
        .declare(&a, fence, routes("rate_limit"))
        .expect("declare accepted");
    let got = events(&batch);
    assert!(
        matches!(
            got.as_slice(),
            [Event::Declared { .. }, Event::BuildCleared { task: t }] if *t == task
        ),
        "{got:?}"
    );
    assert_eq!(barrier_of(&fx, order), Barrier::Lifted);
    assert!(barrier_jobs(&fx).is_empty());
}
