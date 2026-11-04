//! The gantry: a crane on two A-frame legs over ground rails, with a bridge girder, a hoist trolley and hook block, and a
//! cab box at one end. Infra, deploy and CI modules read as a wide frame over open ground that lifts things into place.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, Plane, glass, lit, rectangle};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 88.0,
    d: 56.0,
    top: 44.0,
};

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    rails(&mut p, f);
    bogies(&mut p, f);
    leg(&mut p, f, 11.0, [8.0, 19.5, 14.0, 32.5]);
    leg(&mut p, f, 77.0, [74.0, 18.0, 80.0, 34.0]);
    hoist(&mut p, f);
    girder(&mut p, f);
    cab(&mut p, f);
    trolley(&mut p, f);
    p.finish()
}

/// Two ground rails, each with a stop at both ends and two bolts on its top.
fn rails(p: &mut Part, f: Frame) {
    for x0 in [8.0, 74.0] {
        f.block(
            p,
            [x0, 6.0, x0 + 6.0, 50.0],
            [3.0, 5.0],
            0.0,
            0.4,
            Finish::paper(),
        );
        for y0 in [6.0, 48.0] {
            f.block(
                p,
                [x0, y0, x0 + 6.0, y0 + 2.0],
                [5.0, 7.0],
                0.0,
                0.0,
                Finish::paper(),
            );
        }
        for y in [9.5, 46.5] {
            f.bolt(p, [x0 + 3.0, y], 5.0, [0.0, 0.0, 1.0]);
        }
    }
}

/// Four sill blocks that carry the legs on the rails, nearest the front first.
fn bogies(p: &mut Part, f: Frame) {
    for y0 in [10.0, 34.0] {
        for x0 in [7.0, 73.0] {
            f.block(
                p,
                [x0, y0, x0 + 8.0, y0 + 8.0],
                [5.0, 8.0],
                0.0,
                0.4,
                Finish::paper(),
            );
        }
    }
}

/// One A-frame leg at `x`: two slanted struts from the bogies up to the saddle, a tie bar between them, and the saddle
/// that holds the girder.
fn leg(p: &mut Part, f: Frame, x: f32, saddle: [f32; 4]) {
    for (near, far) in [(14.0, 22.5), (38.0, 29.5)] {
        f.bar(
            p,
            [x, near, 8.0],
            [x, far, 34.0],
            [3.4, 3.4],
            Finish::paper(),
        );
    }
    f.block(
        p,
        [x - 1.5, 18.0, x + 1.5, 34.0],
        [19.0, 21.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    f.block(p, saddle, [34.0, 36.0], 0.0, 0.4, Finish::paper());
}

/// The hoist: two yokes under the girder, two cables down to the sheave block, the sheave with its pin bolts, the
/// hook neck, and the lit hook hanging under the trolley centre.
fn hoist(p: &mut Part, f: Frame) {
    for x0 in [32.0, 41.0] {
        f.block(
            p,
            [x0, 24.5, x0 + 3.0, 29.5],
            [29.0, 36.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    for x in [33.5, 42.5] {
        f.solid(
            p,
            &f.circle([x, 27.0], 0.45, 12),
            [18.0, 29.5],
            0.0,
            Finish::paper(),
        );
    }
    f.block(
        p,
        [32.5, 25.5, 43.5, 28.5],
        [14.0, 18.0],
        0.6,
        0.4,
        Finish::paper(),
    );
    for x in [34.5, 41.5] {
        f.bolt(p, [x, 28.5], 16.0, [0.0, 1.0, 0.0]);
    }
    f.solid(
        p,
        &f.circle([38.0, 27.0], 1.0, 16),
        [12.5, 14.0],
        0.0,
        Finish::paper(),
    );
    f.block(p, [36.0, 24.5, 40.0, 30.0], [9.0, 12.5], 0.0, 0.4, lit());
}

/// The bridge girder with its web seam, its two end plates and the two trolley rails on its top.
fn girder(p: &mut Part, f: Frame) {
    // The bevel is 0.5 rather than the table's 0.6: a corner radius equal to the bevel shrinks to nothing at the bevel,
    // and the sweep needs the same outline count at every station.
    f.block(
        p,
        [6.0, 20.0, 82.0, 32.0],
        [36.0, 41.0],
        0.6,
        0.5,
        Finish::paper(),
    );
    parts::sideline(
        p,
        &f.rect([6.0, 20.0, 82.0, 32.0], 0.6),
        38.5,
        Stroke::new(Ink::Faint).near(),
    );
    for x0 in [5.2, 82.0] {
        f.block(
            p,
            [x0, 20.0, x0 + 0.8, 32.0],
            [36.0, 41.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    for y0 in [21.5, 29.3] {
        f.block(
            p,
            [8.0, y0, 60.0, y0 + 1.2],
            [41.0, 42.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
}

/// The cab box at the right end, with four roof bolts and a glass window on its front face.
fn cab(p: &mut Part, f: Frame) {
    f.block(
        p,
        [60.0, 18.0, 82.0, 34.0],
        [36.0, 44.0],
        0.8,
        0.6,
        Finish::paper(),
    );
    for at in [[62.0, 20.0], [80.0, 20.0], [62.0, 32.0], [80.0, 32.0]] {
        f.bolt(p, at, 44.0, [0.0, 0.0, 1.0]);
    }
    let front = Plane::new([0.0, 34.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);
    front.face(p, f, &rectangle([38.0, 64.0, 42.5, 76.0]), glass());
}

/// The trolley carriage that straddles the girder, with four bolts on its top.
fn trolley(p: &mut Part, f: Frame) {
    f.block(
        p,
        [30.0, 19.0, 46.0, 35.0],
        [42.0, 44.0],
        0.6,
        0.5,
        Finish::paper(),
    );
    for at in [[32.0, 21.0], [44.0, 21.0], [32.0, 33.0], [44.0, 33.0]] {
        f.bolt(p, at, 44.0, [0.0, 0.0, 1.0]);
    }
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        assert_solids_near(Kind::Gantry, 52, 15);
    }
}
