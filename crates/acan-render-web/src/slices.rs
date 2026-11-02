//! The scope lanes on the plant. Each may-change item is a plot on the machines it names: the
//! behavior the order may change there. Each must-keep rule is a keep on its machines: behavior
//! that must hold. A plot is not a fence round code, and an agent's footprint over a keep is not a
//! violation by itself; only a gate run's receipt says what a change did.

use std::collections::BTreeMap;

use acan_core::OrderId;
use serde::Serialize;

use crate::lanes::{LaneView, MayView};
use crate::plan::Plan;

/// How a machine's footprint is tinted for scope. A keep outranks a plot, and a confirmed lane
/// outranks a draft, so where lanes overlap the stronger tint shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeTint {
    PlotDraft,
    Plot,
    KeepDraft,
    Keep,
}

/// One lane's plots and keeps on one plan machine.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Slice {
    /// The order, as `o-N`.
    pub order: String,
    /// The plan index of the machine.
    #[serde(skip)]
    pub machine: usize,
    /// The scope revision.
    pub rev: u32,
    /// Whether a human has confirmed this revision. Until then the plot is a draft and grants
    /// nothing.
    pub confirmed: bool,
    /// Each may-change item on this machine, in words.
    pub plots: Vec<String>,
    /// Each must-keep rule on this machine, in words.
    pub keeps: Vec<String>,
}

impl Slice {
    /// The slice's one-line flag, such as `o-1 · plot + keep · draft rev 1`.
    #[must_use]
    pub fn text(&self) -> String {
        let what = match (self.plots.is_empty(), self.keeps.is_empty()) {
            (false, false) => "plot + keep",
            (false, true) => "plot",
            _ => "keep",
        };
        let state = if self.confirmed { "confirmed" } else { "draft" };
        format!("{} · {what} · {state} rev {}", self.order, self.rev)
    }

    /// The lines Alt adds under the flag: each plot, then each keep.
    #[must_use]
    pub fn detail(&self) -> Vec<String> {
        let plots = self.plots.iter().map(|plot| format!("may {plot}"));
        let keeps = self.keeps.iter().map(|keep| format!("keep {keep}"));
        plots.chain(keeps).collect()
    }

    fn tint(&self) -> ScopeTint {
        match (self.keeps.is_empty(), self.confirmed) {
            (false, true) => ScopeTint::Keep,
            (false, false) => ScopeTint::KeepDraft,
            (true, true) => ScopeTint::Plot,
            (true, false) => ScopeTint::PlotDraft,
        }
    }
}

/// Where the scope lanes fall on the plan.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SliceLayout {
    /// One slice per lane and plan machine it names, in lane order, then machine order.
    pub slices: Vec<Slice>,
    /// For each plan machine, the strongest scope tint on it, if any.
    pub tints: Vec<Option<ScopeTint>>,
    /// Machine names the scopes give that the plan does not have, sorted, without repeats.
    pub unplaced: Vec<String>,
}

/// Lays the scope lanes out on `plan`: which machines each lane's plots and keeps fall on, and the
/// footprint tint that follows.
#[must_use]
pub fn slice_layout(lanes: &[LaneView], plan: &Plan) -> SliceLayout {
    let mut layout = SliceLayout {
        tints: vec![None; plan.machines.len()],
        ..SliceLayout::default()
    };
    for lane in lanes {
        let mut per_machine: BTreeMap<usize, (Vec<String>, Vec<String>)> = BTreeMap::new();
        for item in &lane.may {
            for name in &item.machines {
                match plan.index_of(name) {
                    Some(machine) => per_machine
                        .entry(machine)
                        .or_default()
                        .0
                        .push(plot_words(item)),
                    None => layout.unplaced.push(name.clone()),
                }
            }
        }
        for rule in &lane.must_keep {
            for name in &rule.machines {
                match plan.index_of(name) {
                    Some(machine) => per_machine
                        .entry(machine)
                        .or_default()
                        .1
                        .push(rule.text.clone()),
                    None => layout.unplaced.push(name.clone()),
                }
            }
        }
        for (machine, (plots, keeps)) in per_machine {
            let slice = Slice {
                order: lane.order.to_string(),
                machine,
                rev: lane.rev.0,
                confirmed: lane.confirmation.is_some(),
                plots,
                keeps,
            };
            layout.tints[machine] = layout.tints[machine].max(Some(slice.tint()));
            layout.slices.push(slice);
        }
    }
    layout.unplaced.sort();
    layout.unplaced.dedup();
    layout
}

/// Whether a clash on `machines` between tasks of `orders` sits inside a confirmed scope: the
/// words for the confirmed lanes of those orders with a plot on one of the machines, such as
/// `inside confirmed scope o-1 rev 2`. `None` when no such lane covers it. A draft never counts.
#[must_use]
pub fn clash_scope(
    machines: &[usize],
    orders: &[OrderId],
    lanes: &[LaneView],
    plan: &Plan,
) -> Option<String> {
    let covering: Vec<String> = lanes
        .iter()
        .filter(|lane| lane.confirmation.is_some() && orders.contains(&lane.order))
        .filter(|lane| {
            lane.may.iter().flat_map(|item| &item.machines).any(|name| {
                plan.index_of(name)
                    .is_some_and(|machine| machines.contains(&machine))
            })
        })
        .map(|lane| format!("{} rev {}", lane.order, lane.rev.0))
        .collect();
    (!covering.is_empty()).then(|| format!("inside confirmed scope {}", covering.join(", ")))
}

fn plot_words(item: &MayView) -> String {
    let broad = if item.broad { " · broad grant" } else { "" };
    format!("{}{broad} · {}", item.selector, item.label)
}
