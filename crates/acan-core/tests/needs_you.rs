//! What the project list counts as waiting on the human. Contests and open scope choices are
//! counted in `scope_first.rs`, where the scope-first fixtures live.

mod common;

use acan_core::DeclarationInput;
use common::{Fx, sym};

#[test]
fn a_fresh_order_waits_on_no_one() {
    let fx = Fx::new();
    let needs = fx.origin.world().needs_you();
    assert_eq!(needs.total(), 0, "nothing is asked, clashing or open");
}

#[test]
fn a_pinned_question_counts_as_asking() {
    let mut fx = Fx::new();
    let a = fx.agent_a.clone();
    let readback = fx.read_back(&a, fx.task1);
    fx.ask(readback).expect("question pinned");
    let needs = fx.origin.world().needs_you();
    assert_eq!(needs.asking, 1, "the order waits on its question");
    assert_eq!(needs.clashing, 0);
    assert_eq!(needs.total(), 1);
}

#[test]
fn an_overlapping_declaration_counts_as_clashing() {
    let mut fx = Fx::new();
    let (a, b, t1, t2) = (fx.agent_a.clone(), fx.agent_b.clone(), fx.task1, fx.task2);
    let f1 = fx.claim(&a, t1);
    let f2 = fx.claim(&b, t2);
    let footprint = || DeclarationInput {
        symbols: vec![sym("src/api/routes.rs", "rate_limit")],
        ..DeclarationInput::default()
    };
    fx.declare(&a, f1, footprint()).expect("declare accepted");
    fx.declare(&b, f2, footprint()).expect("declare accepted");
    let needs = fx.origin.world().needs_you();
    assert_eq!(needs.clashing, 1, "the order waits on the clash");
    assert_eq!(needs.asking, 0);
    assert_eq!(needs.total(), 1);
}
