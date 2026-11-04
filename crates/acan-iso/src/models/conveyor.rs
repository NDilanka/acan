//! The conveyor: a long belt on two leg frames, with an end drum at each end, eight idler rollers, side guides and a
//! motor housing at the back. Jobs, queues and workers read as a long, low ribbon with rollers laid across it.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, Plane, lit};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 96.0,
    d: 40.0,
    top: 18.0,
};

/// The left edge of each idler roller. Each roller is three units wide and spans the belt bed.
const ROLLERS: [f32; 8] = [15.0, 24.0, 33.0, 42.0, 51.0, 60.0, 69.0, 78.0];

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    motor(&mut p, f);
    legs(&mut p, f);
    guides(&mut p, f, 18.5);
    drum(&mut p, f, 6.0);
    bed(&mut p, f);
    drum(&mut p, f, 84.0);
    guides(&mut p, f, 33.5);
    brackets(&mut p, f);
    axle_dots(&mut p, f);
    end_cap(&mut p, f);
    drum_bolts(&mut p, f);
    p.finish()
}

/// The motor base at the back left, the housing on it with its seam and roof bolts, and the guard that links the
/// housing to the left drum.
fn motor(p: &mut Part, f: Frame) {
    f.block(
        p,
        [8.0, 4.0, 28.0, 18.0],
        [3.0, 5.0],
        0.0,
        0.4,
        Finish::paper(),
    );
    f.block(
        p,
        [9.0, 4.0, 27.0, 17.0],
        [5.0, 18.0],
        0.8,
        0.6,
        Finish::paper(),
    );
    parts::sideline(
        p,
        &f.rect([9.0, 4.0, 27.0, 17.0], 0.8),
        11.0,
        Stroke::new(Ink::Faint).near(),
    );
    for at in [[11.0, 6.0], [25.0, 6.0], [11.0, 15.0], [25.0, 15.0]] {
        f.bolt(p, at, 18.0, [0.0, 0.0, 1.0]);
    }
    f.block(
        p,
        [9.5, 17.0, 14.5, 19.5],
        [10.5, 15.0],
        0.0,
        0.4,
        Finish::paper(),
    );
}

/// Four foot pads centred on the four leg posts, the posts, and the two leg crossbars that join each front and back pair.
fn legs(p: &mut Part, f: Frame) {
    for [x, y] in [[17.0, 18.5], [74.0, 18.5], [17.0, 29.5], [74.0, 29.5]] {
        f.block(
            p,
            [x, y, x + 5.0, y + 5.0],
            [3.0, 3.6],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    for [x, y] in [[18.0, 19.5], [75.0, 19.5], [18.0, 30.5], [75.0, 30.5]] {
        f.block(
            p,
            [x, y, x + 3.0, y + 3.0],
            [3.6, 10.5],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    for x in [18.0, 75.0] {
        f.block(
            p,
            [x, 22.5, x + 3.0, 30.5],
            [6.0, 7.5],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
}

/// A side stringer with the guide rail on top of it, along the belt's edge at `y`.
fn guides(p: &mut Part, f: Frame, y: f32) {
    f.block(
        p,
        [10.0, y, 86.0, y + 1.0],
        [10.5, 13.5],
        0.0,
        0.0,
        Finish::paper(),
    );
    f.block(
        p,
        [12.0, y, 84.0, y + 1.0],
        [13.5, 16.0],
        0.0,
        0.0,
        Finish::paper(),
    );
}

/// An end drum: a bevelled bar across the belt's end, standing for the round drum the kit cannot turn about x.
fn drum(p: &mut Part, f: Frame, x: f32) {
    f.block(
        p,
        [x, 19.5, x + 6.0, 33.5],
        [10.0, 15.0],
        0.0,
        1.6,
        Finish::paper(),
    );
}

/// The belt bed between the drums, and eight idler rollers laid across it.
fn bed(p: &mut Part, f: Frame) {
    f.block(
        p,
        [12.0, 19.5, 84.0, 33.5],
        [11.0, 14.2],
        0.0,
        0.0,
        Finish::paper(),
    );
    for x in ROLLERS {
        f.block(
            p,
            [x, 19.5, x + 3.0, 33.5],
            [14.2, 15.2],
            0.0,
            0.4,
            Finish::paper(),
        );
    }
}

/// The guard brackets on the front guide's face, four along the belt.
fn brackets(p: &mut Part, f: Frame) {
    for x in [18.5, 38.0, 57.5, 77.0] {
        f.block(
            p,
            [x, 34.5, x + 1.5, 35.5],
            [11.0, 15.5],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
}

/// A round cap wrapping the right drum, lit as the figure's one subject.
fn end_cap(p: &mut Part, f: Frame) {
    f.block(p, [85.5, 20.5, 88.5, 35.0], [11.0, 15.8], 0.0, 0.4, lit());
}

/// The roller axles show as small discs on the front stringer. Each is a rivet head, counted as a solid as a bolt head is.
fn axle_dots(p: &mut Part, f: Frame) {
    for x in ROLLERS {
        p.solids += 1;
        f.dot(
            p,
            [x + 1.5, 34.5],
            12.5,
            [0.0, 1.0, 0.0],
            0.45,
            Stroke::new(Ink::Lo).close(),
        );
    }
}

/// The two drum axle bolts: one on the front stringer over the left drum, one on the right cap's front face.
fn drum_bolts(p: &mut Part, f: Frame) {
    let stringer = Plane::new([0.0, 34.5, 12.5], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);
    stringer.bolt(p, f, [0.0, 11.0], [0.5, 0.9], [Ink::Lo, Ink::Lo]);
    let cap = Plane::new([0.0, 35.0, 12.5], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);
    cap.bolt(p, f, [0.0, 87.0], [0.5, 0.9], [Ink::Lo, Ink::Lo]);
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        assert_solids_near(Kind::Conveyor, 52, 15);
    }
}
