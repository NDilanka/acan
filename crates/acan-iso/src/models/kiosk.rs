//! The kiosk: a slim display cabinet on a heavy base, with an empty sloping glass screen and a card slot.
//! Components, pages and frontend modules read as a tall slab whose upper face leans back.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, Plane, glass, lit, rectangle};
use crate::plan::circle_ring;

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 48.0,
    d: 56.0,
    top: 44.0,
};

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    base(&mut p, f);
    f.prism_x(
        &mut p,
        [12.0, 36.0],
        &[
            [12.0, 10.0],
            [42.0, 10.0],
            [42.0, 22.0],
            [30.0, 40.0],
            [12.0, 40.0],
        ],
        Finish::paper(),
    );
    service_door(&mut p, f);
    f.block(
        &mut p,
        [12.0, 42.0, 36.0, 42.6],
        [10.0, 11.8],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    front(&mut p, f);
    screen(&mut p, f);
    roof(&mut p, f);
    p.finish()
}

/// Four levelling feet, the bevelled base and its anchor bolts.
fn base(p: &mut Part, f: Frame) {
    for at in [[11.0, 14.0], [37.0, 14.0], [11.0, 43.0], [37.0, 43.0]] {
        f.solid(p, &f.circle(at, 1.5, 16), [3.0, 4.0], 0.0, Finish::paper());
    }
    f.block(
        p,
        [8.0, 10.0, 40.0, 46.0],
        [4.0, 10.0],
        1.0,
        0.8,
        Finish::paper(),
    );
    for at in [[38.0, 16.0], [38.0, 38.0], [15.0, 44.0], [33.0, 44.0]] {
        f.bolt(p, at, 10.0, [0.0, 0.0, 1.0]);
    }
}

/// The side service door has two hinges, four screws and a short twelve-sided lock along x.
fn service_door(p: &mut Part, f: Frame) {
    let side = Plane::new([36.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]);
    side.face(
        p,
        f,
        &rectangle([15.0, 13.0, 33.0, 34.0]),
        Finish::paper().near(),
    );
    for z in [16.0, 31.0] {
        f.block(
            p,
            [36.0, 14.2, 36.6, 15.4],
            [z, z + 2.0],
            0.0,
            0.0,
            Finish::paper().close(),
        );
    }
    for [y, z] in [[17.0, 15.0], [31.0, 15.0], [17.0, 32.0], [31.0, 32.0]] {
        f.bolt(p, [36.0, y], z, [1.0, 0.0, 0.0]);
    }
    f.prism_x(
        p,
        [36.0, 37.0],
        &circle_ring(30.0, 23.5, 1.1, 12),
        Finish::paper().near(),
    );
}

/// Two grilles with three slots apiece, then the single lit card-slot bezel and its dark mouth.
fn front(p: &mut Part, f: Frame) {
    let front = Plane::new([0.0, 42.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);
    for x in [14.0, 20.0] {
        front.recess(p, f, &rectangle([12.5, x, 20.0, x + 5.0]));
        for z in [14.4, 16.25, 18.1] {
            front.lifted(crate::mesh::LIFT).line(
                p,
                f,
                &[[z, x], [z, x + 5.0]],
                false,
                Stroke::new(Ink::Faint).close(),
            );
        }
    }
    f.block(p, [27.0, 42.0, 34.0, 43.2], [15.0, 18.5], 0.0, 0.0, lit());
    front
        .lifted(1.2)
        .recess(p, f, &rectangle([16.4, 28.0, 17.1, 33.0]));
}

/// The screen is an empty glass face on a thick bezel, not a lamp or a static reading.
fn screen(p: &mut Part, f: Frame) {
    let length = 12.0_f32.hypot(18.0);
    let plane = Plane::new(
        [12.0, 42.0, 22.0],
        [0.0, -12.0 / length, 18.0 / length],
        [1.0, 0.0, 0.0],
    );
    let s = (length - 18.5) / 2.0;
    plane.slab(
        p,
        f,
        &rectangle([s, 1.5, s + 18.5, 22.5]),
        [0.0, 0.8],
        Finish::paper(),
    );
    let pane = plane.lifted(0.8);
    pane.face(p, f, &rectangle([s + 1.5, 3.0, s + 17.0, 21.0]), glass());
    for at in [
        [s + 0.7, 2.2],
        [s + 17.8, 2.2],
        [s + 0.7, 21.8],
        [s + 17.8, 21.8],
    ] {
        pane.bolt(p, f, at, [0.45, 0.7], [Ink::Lo, Ink::Lo]);
    }
}

/// The roof is the bar deck. Its three-rule vent stays in front of all three possible bars.
fn roof(p: &mut Part, f: Frame) {
    f.block(
        p,
        [11.0, 10.0, 37.0, 30.0],
        [40.0, 44.0],
        1.0,
        0.6,
        Finish::paper(),
    );
    for at in [[13.0, 12.0], [35.0, 12.0], [13.0, 28.0], [35.0, 28.0]] {
        f.bolt(p, at, 44.0, [0.0, 0.0, 1.0]);
    }
    let roof = Plane::top(44.0);
    roof.recess(p, f, &rectangle([18.0, 23.5, 30.0, 28.0]));
    for y in [24.6, 25.75, 26.9] {
        roof.line(
            p,
            f,
            &[[18.0, y], [30.0, y]],
            false,
            Stroke::new(Ink::Faint).close(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        // The grille slots and groove are not solids.
        assert_solids_near(Kind::Kiosk, 45, 16);
    }
}
