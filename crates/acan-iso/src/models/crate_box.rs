//! The crate: a sealed, strapped shipping case with a lid, two diagonal straps, two grab handles, four corner caps and a
//! hasp. It stands for generated code, vendored output, tests, and modules with no source files: nothing in it runs.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, flat, lit, rect_on, unit};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 48.0,
    d: 48.0,
    top: 24.0,
};

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    skids(&mut p, f);
    box_body(&mut p, f);
    lid(&mut p, f);
    handles_and_hasp(&mut p, f);
    p.finish()
}

/// Two skids under the box, which lift it off the plinth.
fn skids(p: &mut Part, f: Frame) {
    f.block(
        p,
        [4.0, 6.0, 44.0, 9.0],
        [3.0, 4.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    f.block(
        p,
        [4.0, 39.0, 44.0, 42.0],
        [3.0, 4.0],
        0.0,
        0.0,
        Finish::paper(),
    );
}

/// The box with its slat lines on the two visible faces, eight corner bolts, and a label plate held by two screws.
fn box_body(p: &mut Part, f: Frame) {
    f.block(
        p,
        [4.0, 4.0, 44.0, 44.0],
        [4.0, 21.0],
        0.0,
        0.6,
        Finish::paper(),
    );
    let base = f.rect([4.0, 4.0, 44.0, 44.0], 0.0);
    for z in [9.0, 16.0] {
        parts::sideline(p, &base, z, Stroke::new(Ink::Faint).near());
    }
    // Slats on the front face (y 44) and the right face (x 44), at thirds and the centre.
    for x in [17.3, 24.0, 30.7] {
        f.line3(
            p,
            &[[x, 44.0 + 0.02, 4.6], [x, 44.0 + 0.02, 20.4]],
            Stroke::new(Ink::Faint).close(),
        );
    }
    for y in [17.3, 24.0, 30.7] {
        f.line3(
            p,
            &[[44.0 + 0.02, y, 4.6], [44.0 + 0.02, y, 20.4]],
            Stroke::new(Ink::Faint).close(),
        );
    }
    for z in [7.0, 18.0] {
        for x in [5.5, 42.5] {
            f.bolt(p, [x, 44.0], z, [0.0, 1.0, 0.0]);
        }
        for y in [5.5, 42.5] {
            f.bolt(p, [44.0, y], z, [1.0, 0.0, 0.0]);
        }
    }
    // A label plate on the front face, clear of the slats, held by two screws.
    f.block(
        p,
        [33.0, 44.0, 41.0, 44.4],
        [9.0, 14.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.bolt(p, [34.0, 44.4], 10.0, [0.0, 1.0, 0.0]);
    f.bolt(p, [40.0, 44.4], 13.0, [0.0, 1.0, 0.0]);
}

/// The lid with its two diagonal straps, two grab handles, and four corner caps with their bolts.
fn lid(p: &mut Part, f: Frame) {
    f.block(
        p,
        [4.0, 4.0, 44.0, 44.0],
        [21.0, 23.0],
        1.0,
        0.6,
        Finish::paper(),
    );
    // The straps are rotated rectangles from corner to corner. The second sits a little higher where they cross.
    let half = [25.46, 1.5];
    let first = rect_on([24.0, 24.0], unit(45.0), unit(135.0), half);
    let second = rect_on([24.0, 24.0], unit(-45.0), unit(45.0), half);
    f.solid(p, &f.poly(&first), [23.0, 23.8], 0.0, Finish::paper());
    f.solid(p, &f.poly(&second), [23.0, 23.9], 0.0, Finish::paper());
    for at in [[4.0, 4.0], [39.0, 4.0], [4.0, 39.0], [39.0, 39.0]] {
        f.block(
            p,
            [at[0], at[1], at[0] + 5.0, at[1] + 5.0],
            [23.0, 24.0],
            0.0,
            0.0,
            Finish::paper(),
        );
        f.bolt(p, [at[0] + 2.5, at[1] + 2.5], 24.0, [0.0, 0.0, 1.0]);
    }
    // Two grab handles lie across the lid, one near each long edge, clear of the straps.
    for y in [9.0, 37.8] {
        f.block(
            p,
            [15.0, y, 33.0, y + 1.2],
            [23.0, 23.9],
            0.0,
            0.0,
            Finish::paper().near(),
        );
    }
}

/// The two handle recesses on the side faces, the hasp plate with two screws and its lit staple, and the two hinges
/// on the back.
fn handles_and_hasp(p: &mut Part, f: Frame) {
    f.block(
        p,
        [3.6, 20.0, 4.0, 28.0],
        [11.0, 13.0],
        0.0,
        0.0,
        flat(Ink::Lo),
    );
    f.block(
        p,
        [44.0, 20.0, 44.4, 28.0],
        [11.0, 13.0],
        0.0,
        0.0,
        flat(Ink::Lo),
    );
    f.block(
        p,
        [22.0, 44.0, 26.0, 44.6],
        [17.0, 20.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.block(p, [23.6, 44.6, 24.4, 45.2], [18.0, 19.5], 0.0, 0.0, lit());
    // Two screws hold the hasp plate to the front face.
    for x in [22.5, 25.5] {
        f.bolt(p, [x, 44.6], 17.5, [0.0, 1.0, 0.0]);
    }
    for x in [10.0, 35.0] {
        f.block(
            p,
            [x, 3.4, x + 3.0, 4.0],
            [20.5, 21.5],
            0.0,
            0.0,
            Finish::paper().near(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near, solids_at_two_bars};

    #[test]
    fn solids_match_the_design_and_meet_the_floor() {
        assert!(solids_at_two_bars(Kind::Crate) >= 38);
        assert_solids_near(Kind::Crate, 40, 15);
    }
}
