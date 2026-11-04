//! The console: a control pedestal with a sloped panel, one long lever with a lit knob at rest, two push buttons and a foot
//! bar. CLI, command and script modules read as a desk a person works by hand, not a door traffic comes through.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, Plane, lit, rectangle};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 40.0,
    d: 48.0,
    top: 31.0,
};

/// The sloped panel falls this far in y from its back edge to its front edge.
const RUN: f32 = 18.0;
/// The panel rises this far in z from its front edge to its back edge.
const RISE: f32 = 3.6;

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    base(&mut p, f);
    column(&mut p, f);
    desk(&mut p, f);
    buttons(&mut p, f);
    lever(&mut p, f);
    p.finish()
}

/// The pedestal base with its bolts, the foot bar in front of it and its two brackets and end caps.
fn base(p: &mut Part, f: Frame) {
    f.block(
        p,
        [8.0, 8.0, 32.0, 36.0],
        [3.0, 5.0],
        0.8,
        0.5,
        Finish::paper(),
    );
    for at in [[10.5, 10.5], [29.5, 10.5], [10.5, 33.5], [29.5, 33.5]] {
        f.bolt(p, at, 5.0, [0.0, 0.0, 1.0]);
    }
    for x0 in [11.0, 27.0] {
        f.block(
            p,
            [x0, 36.0, x0 + 2.0, 41.0],
            [3.6, 4.8],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    f.block(
        p,
        [9.0, 39.0, 31.0, 41.5],
        [4.2, 5.6],
        0.0,
        0.5,
        Finish::paper(),
    );
    for x0 in [8.0, 30.5] {
        f.block(
            p,
            [x0, 38.8, x0 + 1.5, 41.7],
            [4.0, 5.8],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
}

/// The pedestal column with its seam, the access door in its front face with two hinge pins and a latch, the three vent
/// slats on its right face, and the collar under the desk.
fn column(p: &mut Part, f: Frame) {
    f.block(
        p,
        [11.0, 10.0, 29.0, 32.0],
        [5.0, 17.0],
        0.0,
        0.6,
        Finish::paper(),
    );
    parts::sideline(
        p,
        &f.rect([11.0, 10.0, 29.0, 32.0], 0.0),
        11.5,
        Stroke::new(Ink::Faint).near(),
    );
    let front = Plane::new([0.0, 32.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);
    front.recess(p, f, &rectangle([7.0, 15.0, 14.5, 25.0]));
    for z in [8.5, 13.0] {
        // Each hinge pin is a rivet head, counted as a solid as a bolt head is.
        p.solids += 1;
        f.dot(
            p,
            [15.6, 32.0],
            z,
            [0.0, 1.0, 0.0],
            0.7,
            Stroke::new(Ink::Lo).close(),
        );
    }
    f.block(
        p,
        [23.2, 32.0, 24.4, 32.6],
        [10.2, 11.2],
        0.0,
        0.0,
        Finish::paper(),
    );
    for z0 in [11.5, 13.0, 14.5] {
        f.block(
            p,
            [29.0, 16.0, 29.5, 26.0],
            [z0, z0 + 0.7],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    f.block(
        p,
        [9.0, 8.0, 31.0, 34.0],
        [17.0, 18.5],
        0.0,
        0.4,
        Finish::paper(),
    );
}

/// The sloped control panel with its two cheek plates, its front lip and its back ledge. The ledge is the top deck.
fn desk(p: &mut Part, f: Frame) {
    let cheek = [
        [6.0, 18.5],
        [36.5, 18.5],
        [36.5, 19.8],
        [18.0, 23.2],
        [6.0, 23.2],
    ];
    for x0 in [5.2, 34.0] {
        f.prism_x(p, [x0, x0 + 0.8], &cheek, Finish::paper());
    }
    f.prism_x(
        p,
        [6.0, 34.0],
        &[[18.0, 18.5], [36.0, 18.5], [36.0, 19.4], [18.0, 23.0]],
        Finish::paper(),
    );
    f.block(
        p,
        [6.0, 35.2, 34.0, 36.6],
        [18.5, 20.0],
        0.0,
        0.3,
        Finish::paper(),
    );
    f.block(
        p,
        [6.0, 6.0, 34.0, 18.0],
        [18.5, 23.0],
        0.5,
        0.4,
        Finish::paper(),
    );
    for at in [[8.0, 8.0], [32.0, 8.0], [8.0, 16.0], [32.0, 16.0]] {
        f.bolt(p, at, 23.0, [0.0, 0.0, 1.0]);
    }
    for [u, v] in [[2.5, 2.2], [25.5, 2.2], [2.5, 15.2], [25.5, 15.2]] {
        panel().bolt(p, f, [u, v], [0.4, 0.65], [Ink::Lo, Ink::Lo]);
    }
}

/// The two unlit push buttons on the panel: a bezel, and a neutral cap above it.
fn buttons(p: &mut Part, f: Frame) {
    for at in [[11.0, 31.5], [29.0, 31.5]] {
        f.solid(
            p,
            &f.circle(at, 1.8, 20),
            [19.8, 21.1],
            0.0,
            Finish::paper(),
        );
        f.solid(
            p,
            &f.circle(at, 1.2, 16),
            [21.1, 21.9],
            0.2,
            Finish::paper(),
        );
    }
}

/// The lever: its gate housing on the panel with four bolts, the slot in it, the pivot boss, the shaft up to the collar,
/// and the lit knob at its top.
fn lever(p: &mut Part, f: Frame) {
    panel().slab(
        p,
        f,
        &rectangle([10.0, 2.2, 18.0, 9.8]),
        [0.0, 1.0],
        Finish::paper(),
    );
    for [u, v] in [[11.1, 3.3], [16.9, 3.3], [11.1, 8.7], [16.9, 8.7]] {
        panel()
            .lifted(1.0)
            .bolt(p, f, [u, v], [0.3, 0.45], [Ink::Lo, Ink::Lo]);
    }
    panel()
        .lifted(1.0)
        .recess(p, f, &rectangle([13.3, 3.4, 14.7, 8.6]));
    panel().slab(
        p,
        f,
        &rectangle([12.6, 4.8, 15.4, 7.2]),
        [1.0, 1.7],
        Finish::paper(),
    );
    f.bar(
        p,
        on_panel(14.0, 6.0, 1.5),
        [20.0, 29.5, 28.0],
        [3.4, 3.4],
        Finish::paper(),
    );
    f.solid(
        p,
        &f.circle([20.0, 29.5], 1.6, 24),
        [27.4, 28.0],
        0.0,
        Finish::paper(),
    );
    f.solid(
        p,
        &f.circle([20.0, 29.5], 2.4, 24),
        [27.8, 30.8],
        0.7,
        lit(),
    );
}

/// The sloped panel as a plane. Its `u` runs from the right end (x 34) toward x 6, and its `v` runs up the slope from
/// the front edge (y 36, z 19.4) toward the back edge. Its normal points up and forward.
fn panel() -> Plane {
    let length = RUN.hypot(RISE);
    Plane::new(
        [34.0, 36.0, 19.4],
        [-1.0, 0.0, 0.0],
        [0.0, -RUN / length, RISE / length],
    )
}

/// The design point at panel coordinates `(u, v)`, lifted `h` off the panel along its normal.
fn on_panel(u: f32, v: f32, h: f32) -> [f32; 3] {
    let length = RUN.hypot(RISE);
    [
        34.0 - u,
        36.0 - RUN / length * v + RISE / length * h,
        19.4 + RISE / length * v + RUN / length * h,
    ]
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        assert_solids_near(Kind::Console, 51, 15);
    }
}
