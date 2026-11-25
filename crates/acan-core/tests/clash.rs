mod common;

use acan_core::{
    AgentCmd, Clash, ClashKey, ClashState, CodeRev, CommitId, ContractRef, Declaration,
    DeclarationInput, Event, Generation, HumanCmd, IndexedSymbol, LEASE_TTL_MS, Locus, Overlap,
    OverlapKind, Rejection, SettleRight, Settlement, Stage, SystemCmd, TaskId, TaskState,
    WitnessOutcome, detect,
};
use common::{Fx, events, index, machine, path, sym};

fn task(n: u32) -> TaskId {
    format!("t-{n}").parse().expect("valid task id")
}

fn decl(task_no: u32, footprint: DeclarationInput) -> Declaration {
    let rev: CodeRev = "rev-1".parse().expect("valid rev");
    Declaration {
        task: task(task_no),
        generation: Generation::new(1),
        index: rev,
        footprint,
    }
}

fn symbols(refs: &[(&str, &str)]) -> DeclarationInput {
    DeclarationInput {
        symbols: refs.iter().map(|&(f, n)| sym(f, n)).collect(),
        ..DeclarationInput::default()
    }
}

fn one(kind: OverlapKind, at: Locus) -> Vec<Overlap> {
    vec![Overlap {
        key: ClashKey::new(task(1), task(2), at),
        kind,
    }]
}

fn clash_of(kind: OverlapKind, at: Locus) -> Clash {
    Clash {
        id: "c-0".parse().expect("valid clash id"),
        key: ClashKey::new(task(1), task(2), at),
        kind,
        found_at: "rev-1".parse().expect("valid rev"),
        contract: None,
        state: ClashState::Open,
    }
}

#[test]
fn a_shared_implementation_menu_offers_narrow_either_or_serialize() {
    let at = Locus::Symbol {
        file: path("src/api/routes.rs"),
        name: "rate_limit".to_owned(),
    };
    let menu = clash_of(OverlapKind::SharedImplementation, at).menu();
    let want = vec![
        Settlement::Narrow { task: task(1) },
        Settlement::Narrow { task: task(2) },
        Settlement::Serialize {
            first: task(1),
            then: task(2),
        },
    ];
    assert_eq!(menu, want);
}

#[test]
fn yielder_names_the_task_that_gives_way() {
    let key = ClashKey::new(
        task(1),
        task(2),
        Locus::File {
            file: path("src/api/mod.rs"),
        },
    );
    assert_eq!(
        Settlement::Narrow { task: task(2) }.yielder(&key),
        Some(task(2))
    );
    assert_eq!(
        Settlement::Serialize {
            first: task(1),
            then: task(2)
        }
        .yielder(&key),
        Some(task(2))
    );
    assert_eq!(Settlement::Narrow { task: task(9) }.yielder(&key), None);
}
#[test]
fn clash_key_puts_the_lower_task_first() {
    let at = Locus::File {
        file: path("src/api/routes.rs"),
    };
    let key = ClashKey::new(task(7), task(3), at.clone());
    assert_eq!((key.a(), key.b()), (task(3), task(7)));
    assert_eq!(key, ClashKey::new(task(3), task(7), at));
}

#[test]
fn same_symbol_is_shared_implementation() {
    let d1 = decl(1, symbols(&[("src/api/routes.rs", "rate_limit")]));
    let d2 = decl(2, symbols(&[("src/api/routes.rs", "rate_limit")]));
    let at = Locus::Symbol {
        file: path("src/api/routes.rs"),
        name: "rate_limit".to_owned(),
    };
    assert_eq!(
        detect(&[&d1, &d2], &index()),
        one(OverlapKind::SharedImplementation, at)
    );
}

#[test]
fn disjoint_symbols_in_one_file_are_advisory() {
    let d1 = decl(1, symbols(&[("src/api/routes.rs", "rate_limit")]));
    let d2 = decl(2, symbols(&[("src/api/routes.rs", "health")]));
    let got = detect(&[&d1, &d2], &index());
    assert_eq!(
        got,
        one(
            OverlapKind::DisjointRegions,
            Locus::File {
                file: path("src/api/routes.rs")
            }
        )
    );
    assert!(!got[0].kind.blocking());
}

#[test]
fn two_appends_to_one_registry_are_shared_insertion() {
    let inserts = || DeclarationInput {
        inserts: vec![path("src/api/mod.rs")],
        ..DeclarationInput::default()
    };
    let (d1, d2) = (decl(1, inserts()), decl(2, inserts()));
    let want = one(
        OverlapKind::SharedInsertion,
        Locus::File {
            file: path("src/api/mod.rs"),
        },
    );
    assert_eq!(detect(&[&d1, &d2], &index()), want);
}

#[test]
fn whole_file_write_meets_any_symbol_in_it() {
    let d1 = decl(
        1,
        DeclarationInput {
            files: vec![path("src/api/routes.rs")],
            ..DeclarationInput::default()
        },
    );
    let d2 = decl(2, symbols(&[("src/api/routes.rs", "health")]));
    let want = one(
        OverlapKind::WholeFile,
        Locus::File {
            file: path("src/api/routes.rs"),
        },
    );
    assert_eq!(detect(&[&d1, &d2], &index()), want);
}

#[test]
fn unresolved_symbol_counts_as_a_whole_file_write() {
    let d1 = decl(1, symbols(&[("src/api/routes.rs", "does_not_exist")]));
    let d2 = decl(2, symbols(&[("src/api/routes.rs", "health")]));
    let want = one(
        OverlapKind::WholeFile,
        Locus::File {
            file: path("src/api/routes.rs"),
        },
    );
    assert_eq!(detect(&[&d1, &d2], &index()), want);
}

#[test]
fn two_lockfile_writers_are_generated_output() {
    let lock = || DeclarationInput {
        files: vec![path("Cargo.lock")],
        ..DeclarationInput::default()
    };
    let (d1, d2) = (decl(1, lock()), decl(2, lock()));
    let want = one(
        OverlapKind::GeneratedOutput,
        Locus::File {
            file: path("Cargo.lock"),
        },
    );
    assert_eq!(detect(&[&d1, &d2], &index()), want);
}

#[test]
fn interface_change_meets_an_importing_machine_as_contract() {
    let d1 = decl(
        1,
        DeclarationInput {
            interfaces: vec![sym("src/auth/session.rs", "verify")],
            ..DeclarationInput::default()
        },
    );
    let d2 = decl(2, symbols(&[("src/api/routes.rs", "rate_limit")]));
    let want = one(
        OverlapKind::Contract,
        Locus::Belt {
            from: machine("api"),
            to: machine("auth"),
        },
    );
    assert_eq!(detect(&[&d1, &d2], &index()), want);
}

#[test]
fn interface_change_without_an_importing_belt_is_no_overlap() {
    let d1 = decl(
        1,
        DeclarationInput {
            interfaces: vec![sym("src/api/routes.rs", "router")],
            ..DeclarationInput::default()
        },
    );
    let d2 = decl(2, symbols(&[("src/auth/login.rs", "login")]));
    assert_eq!(detect(&[&d1, &d2], &index()), vec![]);
}

#[test]
fn a_task_never_clashes_with_itself() {
    let d1 = decl(1, symbols(&[("src/api/routes.rs", "rate_limit")]));
    let d1_again = decl(1, symbols(&[("src/api/routes.rs", "rate_limit")]));
    assert_eq!(detect(&[&d1, &d1_again], &index()), vec![]);
}

#[test]
fn detection_does_not_depend_on_input_order() {
    let d1 = decl(1, symbols(&[("src/api/routes.rs", "rate_limit")]));
    let d2 = decl(
        2,
        symbols(&[
            ("src/api/routes.rs", "rate_limit"),
            ("src/api/routes.rs", "health"),
        ]),
    );
    let d3 = decl(
        3,
        DeclarationInput {
            files: vec![path("src/api/routes.rs")],
            ..DeclarationInput::default()
        },
    );
    let idx = index();
    let forward = detect(&[&d1, &d2, &d3], &idx);
    assert!(!forward.is_empty());
    assert_eq!(forward, detect(&[&d3, &d1, &d2], &idx));
    assert!(
        forward.windows(2).all(|w| w[0] <= w[1]),
        "output is sorted: {forward:?}"
    );
}

/// Claims both tasks (task1 by owner's agent, task2 by bob's agent) and declares the same symbol.
fn clashing() -> Fx {
    let mut fx = Fx::new();
    let (a, b, t1, t2) = (fx.agent_a.clone(), fx.agent_b.clone(), fx.task1, fx.task2);
    let f1 = fx.claim(&a, t1);
    let f2 = fx.claim(&b, t2);
    fx.declare(&a, f1, symbols(&[("src/api/routes.rs", "rate_limit")]))
        .expect("declare accepted");
    fx.declare(&b, f2, symbols(&[("src/api/routes.rs", "rate_limit")]))
        .expect("declare accepted");
    fx
}

#[test]
fn overlapping_declaration_records_a_blocking_clash_and_holds_both_tasks() {
    let fx = clashing();
    let clashes: Vec<_> = fx.origin.world().clashes().collect();
    assert_eq!(clashes.len(), 1);
    assert_eq!(clashes[0].kind, OverlapKind::SharedImplementation);
    assert_eq!(clashes[0].state, ClashState::Open);
    for t in [fx.task1, fx.task2] {
        assert!(matches!(
            fx.origin.world().task(t).expect("task").state,
            TaskState::Claimed {
                stage: Stage::Declared { .. },
                ..
            }
        ));
    }
}

#[test]
fn advisory_overlap_is_not_recorded_and_both_tasks_build() {
    let mut fx = Fx::new();
    let (a, b, t1, t2) = (fx.agent_a.clone(), fx.agent_b.clone(), fx.task1, fx.task2);
    let f1 = fx.claim(&a, t1);
    let f2 = fx.claim(&b, t2);
    fx.declare(&a, f1, symbols(&[("src/api/routes.rs", "rate_limit")]))
        .expect("declare accepted");
    let batch = fx
        .declare(&b, f2, symbols(&[("src/api/routes.rs", "health")]))
        .expect("declare accepted");
    assert!(
        events(&batch)
            .iter()
            .any(|e| matches!(e, Event::BuildCleared { task } if *task == t2))
    );
    assert_eq!(fx.origin.world().clashes().count(), 0);
}

#[test]
fn narrowing_redeclaration_dissolves_the_clash() {
    let mut fx = clashing();
    let (b, t2) = (fx.agent_b.clone(), fx.task2);
    let f2 = fx.fence(t2).expect("claimed");
    let batch = fx
        .declare(&b, f2, symbols(&[("src/api/routes.rs", "health")]))
        .expect("redeclare accepted");
    assert!(
        events(&batch)
            .iter()
            .any(|e| matches!(e, Event::ClashDissolved { .. }))
    );
    assert!(
        fx.origin
            .world()
            .clashes()
            .all(|c| c.state == ClashState::Dissolved)
    );
}

#[test]
fn member_settles_their_own_agents_yield() {
    let mut fx = clashing();
    let (bob, t2) = (fx.bob.clone(), fx.task2);
    let clash = fx.origin.world().clashes().next().expect("clash").clone();
    let choice = Settlement::Narrow { task: t2 };
    assert_eq!(
        fx.origin.world().settle_right(&bob, &clash, &choice),
        Some(SettleRight::Settle)
    );
    fx.human(
        &bob,
        HumanCmd::SettleClash {
            clash: clash.id,
            choice: choice.clone(),
        },
    )
    .expect("settle accepted");
    assert_eq!(
        fx.origin.world().clash(clash.id).expect("clash").state,
        ClashState::Settled { choice, by: bob }
    );
    assert!(matches!(
        fx.origin.world().task(t2).expect("task").state,
        TaskState::Claimed {
            stage: Stage::Declaring,
            ..
        }
    ));
}

#[test]
fn member_may_only_propose_another_members_yield_and_owner_confirms() {
    let mut fx = clashing();
    let (bob, owner, t1) = (fx.bob.clone(), fx.owner.clone(), fx.task1);
    let clash = fx.origin.world().clashes().next().expect("clash").clone();
    let choice = Settlement::Narrow { task: t1 };
    assert_eq!(
        fx.origin.world().settle_right(&bob, &clash, &choice),
        Some(SettleRight::Propose)
    );
    fx.human(
        &bob,
        HumanCmd::SettleClash {
            clash: clash.id,
            choice: choice.clone(),
        },
    )
    .expect("proposal accepted");
    let state = fx
        .origin
        .world()
        .clash(clash.id)
        .expect("clash")
        .state
        .clone();
    assert_eq!(
        state,
        ClashState::Proposed {
            choice: choice.clone(),
            by: bob
        }
    );
    fx.human(
        &owner,
        HumanCmd::SettleClash {
            clash: clash.id,
            choice: choice.clone(),
        },
    )
    .expect("owner settles");
    assert_eq!(
        fx.origin.world().clash(clash.id).expect("clash").state,
        ClashState::Settled { choice, by: owner }
    );
}

#[test]
fn a_choice_off_the_menu_is_rejected() {
    let mut fx = clashing();
    let owner = fx.owner.clone();
    let clash = fx.origin.world().clashes().next().expect("clash").id;
    let outsider: TaskId = "t-99".parse().expect("valid task id");
    let got = fx.human(
        &owner,
        HumanCmd::SettleClash {
            clash,
            choice: Settlement::Narrow { task: outsider },
        },
    );
    assert!(matches!(got, Err(Rejection::Invalid { .. })), "got {got:?}");
}

#[test]
fn serialize_clears_first_and_holds_then_until_first_lands() {
    let mut fx = clashing();
    let (owner, t1, t2) = (fx.owner.clone(), fx.task1, fx.task2);
    let clash = fx.origin.world().clashes().next().expect("clash").id;
    let batch = fx
        .human(
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
    let cleared: Vec<TaskId> = events(&batch)
        .into_iter()
        .filter_map(|e| match e {
            Event::BuildCleared { task } => Some(*task),
            _ => None,
        })
        .collect();
    assert_eq!(cleared, vec![t1]);
    assert!(matches!(
        fx.origin.world().task(t2).expect("task").state,
        TaskState::Claimed {
            stage: Stage::Declared { .. },
            ..
        }
    ));
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

#[test]
fn releasing_a_party_dissolves_the_clash_and_clears_the_other_at_the_barrier() {
    let mut fx = clashing();
    let (a, t1, t2) = (fx.agent_a.clone(), fx.task1, fx.task2);
    let fence = fx.fence(t1).expect("claimed");
    let batch = fx
        .agent(&a, AgentCmd::Release { fence })
        .expect("release accepted");
    let got = events(&batch);
    assert!(
        got.iter()
            .any(|e| matches!(e, Event::ClashDissolved { .. })),
        "{got:?}"
    );
    assert_eq!(
        stage_of(&fx, t2),
        "declared",
        "the released task is open again, so the barrier waits for it to declare"
    );
    fx.run_out_barrier(fx.order);
    assert_eq!(stage_of(&fx, t2), "building");
}

#[test]
fn ignoring_a_narrow_settlement_raises_a_new_clash() {
    let mut fx = clashing();
    let (bob, b, t2) = (fx.bob.clone(), fx.agent_b.clone(), fx.task2);
    let first = fx.origin.world().clashes().next().expect("clash").id;
    fx.human(
        &bob,
        HumanCmd::SettleClash {
            clash: first,
            choice: Settlement::Narrow { task: t2 },
        },
    )
    .expect("settle accepted");
    let f2 = fx.fence(t2).expect("claimed");
    fx.declare(&b, f2, symbols(&[("src/api/routes.rs", "rate_limit")]))
        .expect("redeclare accepted");
    let blocking: Vec<_> = fx
        .origin
        .world()
        .clashes()
        .filter(|c| c.blocking())
        .map(|c| c.id)
        .collect();
    assert_eq!(blocking.len(), 1, "one new blocking clash");
    assert_ne!(blocking[0], first);
}

#[test]
fn a_contract_clash_offers_to_split_at_the_interface() {
    let mut fx = Fx::new();
    let (a, b, bob, t1, t2) = (
        fx.agent_a.clone(),
        fx.agent_b.clone(),
        fx.bob.clone(),
        fx.task1,
        fx.task2,
    );
    let f1 = fx.claim(&a, t1);
    let f2 = fx.claim(&b, t2);
    let interface = DeclarationInput {
        interfaces: vec![sym("src/auth/session.rs", "verify")],
        ..DeclarationInput::default()
    };
    fx.declare(&a, f1, interface).expect("declare accepted");
    fx.declare(&b, f2, symbols(&[("src/api/routes.rs", "rate_limit")]))
        .expect("declare accepted");
    let clash = fx.origin.world().clashes().next().expect("clash").clone();
    assert_eq!(clash.kind, OverlapKind::Contract);
    assert_eq!(
        clash.contract,
        Some(ContractRef {
            owner: t1,
            interface: sym("src/auth/session.rs", "verify"),
        })
    );
    let split = Settlement::SplitAtContract {
        owner: t1,
        contract: "verify".to_owned(),
    };
    assert_eq!(
        clash.menu(),
        vec![
            split.clone(),
            Settlement::Serialize {
                first: t1,
                then: t2
            }
        ]
    );
    assert_eq!(
        fx.origin.world().settle_right(&bob, &clash, &split),
        Some(SettleRight::Settle),
        "bob's agent yields under a split"
    );
    fx.human(
        &bob,
        HumanCmd::SettleClash {
            clash: clash.id,
            choice: split,
        },
    )
    .expect("settle accepted");
    assert_eq!(
        (stage_of(&fx, t1), stage_of(&fx, t2)),
        ("building", "declared")
    );
}

#[test]
fn the_waiting_task_clears_once_its_predecessor_lands() {
    let mut fx = clashing();
    let (owner, a, t1, t2) = (fx.owner.clone(), fx.agent_a.clone(), fx.task1, fx.task2);
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
    let fence = fx.fence(t1).expect("claimed");
    let head: CommitId = "aaa111".parse().expect("valid commit");
    fx.agent(
        &a,
        AgentCmd::ReportBuilt {
            fence,
            head: head.clone(),
        },
    )
    .expect("report accepted");
    let run = match &fx.origin.world().task(t1).expect("task").state {
        TaskState::Claimed {
            stage: Stage::Gating { run, .. },
            ..
        } => *run,
        other => panic!("not gating: {other:?}"),
    };
    let witness = fx.witnesses()[0];
    fx.system(SystemCmd::WitnessResult {
        run,
        witness,
        outcome: WitnessOutcome::Pass,
    })
    .expect("pass accepted");
    assert_eq!(stage_of(&fx, t2), "declared", "waits while t1 is queued");
    let batch = fx
        .system(SystemCmd::LandResult {
            task: t1,
            head,
            result: Ok("m1".parse().expect("valid commit")),
        })
        .expect("land accepted");
    assert!(
        events(&batch)
            .iter()
            .any(|e| matches!(e, Event::BuildCleared { task } if *task == t2))
    );
}

#[test]
fn an_index_update_finds_new_overlaps_once_per_revision() {
    let mut fx = Fx::new();
    let (a, b, t1, t2) = (fx.agent_a.clone(), fx.agent_b.clone(), fx.task1, fx.task2);
    let f1 = fx.claim(&a, t1);
    let f2 = fx.claim(&b, t2);
    fx.declare(&a, f1, symbols(&[("src/api/routes.rs", "rate_limit")]))
        .expect("declare accepted");
    fx.declare(&b, f2, symbols(&[("src/api/routes.rs", "health")]))
        .expect("declare accepted");
    assert_eq!(fx.origin.world().clashes().count(), 0);
    fx.index.rev = "rev-2".parse().expect("valid rev");
    let routes = fx
        .index
        .files
        .get_mut(&path("src/api/routes.rs"))
        .expect("routes file");
    routes.symbols.retain(|s| s.name != "health");
    routes.symbols.push(IndexedSymbol {
        name: "health".to_owned(),
        bytes: 150..260,
        exported: false,
    });
    fx.system(SystemCmd::IndexUpdated).expect("rescan accepted");
    let clashes: Vec<_> = fx.origin.world().clashes().collect();
    assert_eq!(clashes.len(), 1);
    assert_eq!(clashes[0].kind, OverlapKind::SharedImplementation);
    assert_eq!(
        (stage_of(&fx, t1), stage_of(&fx, t2)),
        ("declared", "declared"),
        "a new clash stops both builders"
    );
    let again = fx
        .system(SystemCmd::IndexUpdated)
        .expect("same revision accepted");
    assert!(again.is_empty(), "got {again:?}");
}

#[test]
fn an_empty_declaration_is_rejected_and_a_repeat_is_a_no_op() {
    let mut fx = Fx::new();
    let (a, t1) = (fx.agent_a.clone(), fx.task1);
    let fence = fx.claim(&a, t1);
    let got = fx.declare(&a, fence, DeclarationInput::default());
    assert!(matches!(got, Err(Rejection::Invalid { .. })), "got {got:?}");
    let footprint = symbols(&[("src/api/routes.rs", "rate_limit")]);
    fx.declare(&a, fence, footprint.clone())
        .expect("declare accepted");
    let batch = fx.declare(&a, fence, footprint).expect("repeat accepted");
    assert!(batch.is_empty(), "got {batch:?}");
}

/// `clashing`, settled by the owner so task1 lands before task2.
fn serialized() -> Fx {
    let mut fx = clashing();
    let (owner, t1, t2) = (fx.owner.clone(), fx.task1, fx.task2);
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
    assert_eq!(stage_of(&fx, t2), "declared", "task2 waits on task1");
    fx
}

/// Lets task1's lease run out while task2 keeps renewing.
fn expire_task1(fx: &mut Fx) {
    let b = fx.agent_b.clone();
    let f2 = fx.fence(fx.task2).expect("claimed");
    fx.advance(LEASE_TTL_MS / 2);
    fx.agent(&b, AgentCmd::Renew { fence: f2 })
        .expect("renew accepted");
    fx.advance(LEASE_TTL_MS / 2);
}

#[test]
fn releasing_a_serialized_predecessor_clears_the_waiter() {
    let mut fx = serialized();
    let (a, t1, t2) = (fx.agent_a.clone(), fx.task1, fx.task2);
    let fence = fx.fence(t1).expect("claimed");
    fx.agent(&a, AgentCmd::Release { fence })
        .expect("release accepted");
    let clash = fx.origin.world().clashes().next().expect("clash");
    assert_eq!(clash.state, ClashState::Dissolved);
    assert_eq!(stage_of(&fx, t2), "building");
}

#[test]
fn expiring_a_serialized_predecessor_clears_the_waiter() {
    let mut fx = serialized();
    let t2 = fx.task2;
    expire_task1(&mut fx);
    fx.system(SystemCmd::Tick).expect("tick accepted");
    assert_eq!(stage_of(&fx, fx.task1), "open");
    assert_eq!(stage_of(&fx, t2), "building");
}

#[test]
fn redeclaring_away_from_a_serialized_overlap_clears_the_waiter() {
    let mut fx = serialized();
    let (a, t1, t2) = (fx.agent_a.clone(), fx.task1, fx.task2);
    let fence = fx.fence(t1).expect("claimed");
    fx.declare(
        &a,
        fence,
        DeclarationInput {
            files: vec![path("unrelated.rs")],
            ..DeclarationInput::default()
        },
    )
    .expect("redeclare accepted");
    let clash = fx.origin.world().clashes().next().expect("clash");
    assert_eq!(clash.state, ClashState::Dissolved);
    assert_eq!(stage_of(&fx, t2), "building");
}

#[test]
fn a_settlement_arriving_with_the_predecessors_expiry_does_not_strand_the_waiter() {
    let mut fx = clashing();
    let (owner, t1, t2) = (fx.owner.clone(), fx.task1, fx.task2);
    let clash = fx.origin.world().clashes().next().expect("clash").id;
    expire_task1(&mut fx);
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
    assert_eq!(stage_of(&fx, t2), "declared", "the barrier waits for task1");
    fx.run_out_barrier(fx.order);
    assert_eq!(stage_of(&fx, t2), "building");
}

#[test]
fn an_expired_party_dissolves_an_open_clash_and_clears_the_other_at_the_barrier() {
    let mut fx = clashing();
    let t2 = fx.task2;
    expire_task1(&mut fx);
    fx.system(SystemCmd::Tick).expect("tick accepted");
    let clash = fx.origin.world().clashes().next().expect("clash");
    assert!(!clash.blocking());
    assert_eq!(stage_of(&fx, t2), "declared", "the barrier waits for task1");
    fx.run_out_barrier(fx.order);
    assert_eq!(stage_of(&fx, t2), "building");
}
