//! The marks a machine carries from its data: the file bars on its top deck, the groove round its plinth, and the failed
//! tick at one corner. They are not part of the kind's model, because the data decides them: the renderer places the
//! bars its file count calls for, the groove its kind's rule calls for, and the tick while a check fails.

use crate::mesh::{Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::{Kind, footprint};
use crate::parts::{Frame, PLINTH, flat};

/// A mark a machine carries besides its model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// File bars on the top deck: one, two or three of them.
    Bars(u8),
    /// The groove round the plinth. Broken when the kind is a guess from the name, closed when it is exact.
    Groove { broken: bool },
    /// The failed tick: a red right triangle at the plinth's corner, shown while a check fails.
    FailedTick,
}

impl Overlay {
    /// Every overlay, in the order [`Overlay::index`] numbers them.
    pub const ALL: [Overlay; 6] = [
        Overlay::Bars(1),
        Overlay::Bars(2),
        Overlay::Bars(3),
        Overlay::Groove { broken: true },
        Overlay::Groove { broken: false },
        Overlay::FailedTick,
    ];

    /// The position of this overlay in [`Overlay::ALL`].
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Overlay::Bars(count) => usize::from(count.clamp(1, 3)) - 1,
            Overlay::Groove { broken: true } => 3,
            Overlay::Groove { broken: false } => 4,
            Overlay::FailedTick => 5,
        }
    }
}

/// How many file bars a machine with `files` source files carries: none for no files, one for one to three, two for
/// four to fifteen, and three for sixteen or more. `None` means no bars.
#[must_use]
pub fn bar_count(files: usize) -> Option<u8> {
    match files {
        0 => None,
        1..=3 => Some(1),
        4..=15 => Some(2),
        _ => Some(3),
    }
}

/// The model of `overlay` on a machine of `kind`. Machines of the kinds without their own figure draw as the
/// assembler, so their marks sit where the assembler's do.
#[must_use]
pub fn overlay_model(kind: Kind, overlay: Overlay) -> Model {
    let figure = figure_of(kind);
    let f = Frame::of(footprint(figure));
    let mut p = Part::new();
    match overlay {
        Overlay::Bars(count) => bars(&mut p, f, deck(figure), count),
        Overlay::Groove { broken } => groove(&mut p, f, broken),
        Overlay::FailedTick => failed_tick(&mut p, f),
    }
    p.finish()
}

/// The kind whose figure a machine of `kind` draws: its own, or the assembler for the kinds without one yet.
fn figure_of(kind: Kind) -> Kind {
    match kind {
        Kind::Intake
        | Kind::Gate
        | Kind::Tank
        | Kind::Splitter
        | Kind::Valve
        | Kind::Crate
        | Kind::Switchboard
        | Kind::Kiosk
        | Kind::Mould
        | Kind::Switchbox
        | Kind::Toolrack
        | Kind::Assembler => kind,
        _ => Kind::Assembler,
    }
}

/// Where a figure's file bars go: the top deck, its back edge, and how long the bars are.
struct Deck {
    /// The back edge of the deck, in design units.
    back: f32,
    /// The height of the deck's top.
    top: f32,
    /// The centre of the bars along x.
    centre: f32,
    /// The length of each bar along x.
    length: f32,
}

fn deck(kind: Kind) -> Deck {
    match kind {
        Kind::Intake => Deck {
            back: 4.0,
            top: 34.0,
            centre: 36.0,
            length: 26.0,
        },
        Kind::Gate => Deck {
            back: 20.0,
            top: 36.0,
            centre: 28.0,
            length: 20.0,
        },
        Kind::Tank => Deck {
            back: 26.0,
            top: 46.8,
            centre: 40.0,
            length: 12.0,
        },
        Kind::Splitter => Deck {
            back: 8.0,
            top: 24.0,
            centre: 40.0,
            length: 10.0,
        },
        Kind::Valve => Deck {
            back: 4.0,
            top: 5.0,
            centre: 20.0,
            length: 10.0,
        },
        Kind::Crate => Deck {
            back: 4.0,
            top: 23.0,
            centre: 24.0,
            length: 12.0,
        },
        Kind::Switchboard => Deck {
            back: 4.0,
            top: 40.0,
            centre: 32.0,
            length: 28.0,
        },
        Kind::Kiosk => Deck {
            back: 10.0,
            top: 44.0,
            centre: 24.0,
            length: 16.0,
        },
        Kind::Mould => Deck {
            back: 30.0,
            top: 11.0,
            centre: 18.0,
            length: 14.0,
        },
        Kind::Switchbox => Deck {
            back: 6.0,
            top: 40.0,
            centre: 15.0,
            length: 8.0,
        },
        Kind::Toolrack => Deck {
            back: 4.0,
            top: 40.0,
            centre: 28.0,
            length: 24.0,
        },
        _ => Deck {
            back: 8.0,
            top: 24.0,
            centre: 32.0,
            length: 24.0,
        },
    }
}

/// The file bars: `count` slabs, each two deep, the first two units behind the deck's back edge and four apart.
fn bars(p: &mut Part, f: Frame, deck: Deck, count: u8) {
    const DEPTH: f32 = 2.0;
    const PROUD: f32 = 0.4;
    for index in 0..count {
        let y0 = deck.back + 2.0 + 4.0 * f32::from(index);
        let at = [
            deck.centre - deck.length / 2.0,
            y0,
            deck.centre + deck.length / 2.0,
            y0 + DEPTH,
        ];
        f.block(p, at, [deck.top, deck.top + PROUD], 0.0, 0.0, flat(Ink::Lo));
        let outline = [
            [at[0], at[1]],
            [at[2], at[1]],
            [at[2], at[3]],
            [at[0], at[3]],
        ];
        f.line(p, &outline, deck.top + PROUD, true, Stroke::new(Ink::Faint));
    }
}

/// The groove on the plinth's top, inset four units: closed, or broken into four straight runs that stop short of
/// each corner.
fn groove(p: &mut Part, f: Frame, broken: bool) {
    let (w, d) = (f.w, f.d);
    if broken {
        let runs = [
            vec![[8.0, 4.0], [w - 8.0, 4.0]],
            vec![[8.0, d - 4.0], [w - 8.0, d - 4.0]],
            vec![[4.0, 8.0], [4.0, d - 8.0]],
            vec![[w - 4.0, 8.0], [w - 4.0, d - 8.0]],
        ];
        for run in runs {
            f.line(p, &run, PLINTH, false, Stroke::new(Ink::Faint));
        }
    } else {
        let ring = [
            [4.0, 4.0],
            [w - 4.0, 4.0],
            [w - 4.0, d - 4.0],
            [4.0, d - 4.0],
        ];
        f.line(p, &ring, PLINTH, true, Stroke::new(Ink::Faint));
    }
}

/// The failed tick: a flat red right triangle on the plinth's top, at its (max x, min y) corner.
fn failed_tick(p: &mut Part, f: Frame) {
    let tick = f.poly(&[[f.w - 2.0, 2.0], [f.w - 8.0, 2.0], [f.w - 2.0, 8.0]]);
    f.solid(p, &tick, [PLINTH, PLINTH + 0.2], 0.0, flat(Ink::Red));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::tests::check_closed;
    use crate::model::Surface;

    fn triangles(model: &Model) -> usize {
        model.meshes.iter().map(|mesh| mesh.indices.len() / 3).sum()
    }

    #[test]
    fn bar_counts_follow_the_file_count() {
        assert_eq!(bar_count(0), None);
        assert_eq!(bar_count(1), Some(1));
        assert_eq!(bar_count(3), Some(1));
        assert_eq!(bar_count(4), Some(2));
        assert_eq!(bar_count(15), Some(2));
        assert_eq!(bar_count(16), Some(3));
        assert_eq!(bar_count(10_000), Some(3));
    }

    #[test]
    fn overlay_indices_follow_their_order() {
        for (index, overlay) in Overlay::ALL.into_iter().enumerate() {
            assert_eq!(overlay.index(), index, "{overlay:?}");
        }
    }

    #[test]
    fn every_overlay_is_a_union_of_closed_solids_inside_its_figure() {
        for kind in Kind::ALL {
            let fp = footprint(kind);
            for overlay in Overlay::ALL {
                let built = overlay_model(kind, overlay);
                for mesh in &built.meshes {
                    check_closed(mesh).unwrap_or_else(|e| panic!("{kind:?} {overlay:?}: {e}"));
                }
                let b = built.bounds;
                let inside = |p: [f32; 3]| {
                    p[0] >= -fp.w / 2.0 - 1e-3
                        && p[0] <= fp.w / 2.0 + 1e-3
                        && p[1] >= -fp.d / 2.0 - 1e-3
                        && p[1] <= fp.d / 2.0 + 1e-3
                };
                assert!(inside(b.min) && inside(b.max), "{kind:?} {overlay:?} {b:?}");
            }
        }
    }

    #[test]
    fn only_the_failed_tick_draws_red() {
        for kind in Kind::ALL {
            for overlay in Overlay::ALL {
                let built = overlay_model(kind, overlay);
                let red = built
                    .meshes
                    .iter()
                    .flat_map(|mesh| mesh.surfaces.iter())
                    .any(|surface| *surface == Surface::Ink(Ink::Red));
                assert_eq!(red, overlay == Overlay::FailedTick, "{kind:?} {overlay:?}");
            }
        }
    }

    #[test]
    fn more_files_draw_more_bars_and_a_broken_groove_is_four_runs() {
        let one = overlay_model(Kind::Assembler, Overlay::Bars(1));
        let three = overlay_model(Kind::Assembler, Overlay::Bars(3));
        assert!(triangles(&three) > triangles(&one));
        assert_eq!(
            overlay_model(Kind::Assembler, Overlay::Groove { broken: true })
                .lines
                .len(),
            4
        );
        assert_eq!(
            overlay_model(Kind::Assembler, Overlay::Groove { broken: false })
                .lines
                .len(),
            1
        );
    }

    #[test]
    fn the_kinds_without_a_figure_take_the_assemblers_marks() {
        for kind in [
            Kind::Conveyor,
            Kind::Horn,
            Kind::Gantry,
            Kind::Console,
            Kind::Antenna,
        ] {
            for overlay in Overlay::ALL {
                assert_eq!(
                    overlay_model(kind, overlay),
                    overlay_model(Kind::Assembler, overlay),
                    "{kind:?} {overlay:?}"
                );
            }
        }
    }
}
