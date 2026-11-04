//! The toolrack: an open four-post frame with a pegboard, six hooks, a five-bin sloping tray and a lower shelf.
//! Shared utilities and helpers read as a rack with daylight between the shelf and the tray.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, lit};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 56.0,
    d: 32.0,
    top: 40.0,
};

const RAIL: [[f32; 2]; 4] = [[8.0, 21.6], [26.0, 17.6], [26.0, 19.0], [8.0, 23.0]];
const END: [[f32; 2]; 4] = [[16.0, 21.0], [27.0, 17.0], [27.0, 20.0], [16.0, 25.0]];

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    frame(&mut p, f);
    f.prism_x(&mut p, [9.0, 11.0], &RAIL, Finish::paper());
    tray(&mut p, f);
    f.prism_x(&mut p, [45.0, 47.0], &RAIL, Finish::paper());
    for [y, z] in [[9.5, 22.0], [24.8, 18.4]] {
        f.bolt(&mut p, [47.0, y], z, [1.0, 0.0, 0.0]);
    }
    for x in [9.0, 45.0] {
        f.block(
            &mut p,
            [x, 8.0, x + 2.0, 14.0],
            [34.0, 37.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    hooks(&mut p, f);
    cap(&mut p, f);
    p.finish()
}

/// Feet and back posts, a perforated pegboard with battens, the shelf and the shorter front posts.
fn frame(p: &mut Part, f: Frame) {
    for x in [8.5, 44.5] {
        for y in [5.5, 23.5] {
            f.block(
                p,
                [x, y, x + 3.0, y + 3.0],
                [3.0, 4.0],
                0.0,
                0.0,
                Finish::paper(),
            );
        }
    }
    for x in [9.0, 45.0] {
        f.block(
            p,
            [x, 6.0, x + 2.0, 8.0],
            [4.0, 37.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    f.block(
        p,
        [11.0, 6.2, 45.0, 7.4],
        [18.0, 35.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    for column in 0..9 {
        for row in 0..4 {
            f.dot(
                p,
                [14.0 + 3.5 * column as f32, 7.4],
                20.0 + 4.0 * row as f32,
                [0.0, 1.0, 0.0],
                0.3,
                Stroke::new(Ink::Lo).close(),
            );
        }
    }
    for z in [18.0, 33.8] {
        f.block(
            p,
            [11.0, 7.4, 45.0, 8.2],
            [z, z + 1.2],
            0.0,
            0.0,
            Finish::paper().near(),
        );
    }
    f.block(
        p,
        [9.0, 8.0, 47.0, 26.0],
        [8.0, 10.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    f.block(
        p,
        [11.0, 25.4, 45.0, 26.0],
        [10.0, 11.2],
        0.0,
        0.0,
        Finish::paper(),
    );
    for x in [9.0, 45.0] {
        f.block(
            p,
            [x, 24.0, x + 2.0, 26.0],
            [4.0, 17.6],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
}

/// The tray falls four units over eleven; four dividers form five bins behind its single lit front lip.
fn tray(p: &mut Part, f: Frame) {
    let floor = |y: f32| 22.0 - (y - 16.0) * 4.0 / 11.0;
    f.prism_x(
        p,
        [11.0, 45.0],
        &[
            [16.0, floor(16.0)],
            [17.0, floor(17.0)],
            [17.0, floor(17.0) + 3.0],
            [16.0, floor(16.0) + 3.0],
        ],
        Finish::paper(),
    );
    f.prism_x(
        p,
        [11.0, 45.0],
        &[[16.0, 22.0], [27.0, 18.0], [27.0, 17.0], [16.0, 21.0]],
        Finish::paper(),
    );
    f.prism_x(p, [11.0, 11.8], &END, Finish::paper());
    for x in [18.28, 24.76, 31.24, 37.72] {
        f.prism_x(
            p,
            [x, x + 0.6],
            &[
                [17.0, floor(17.0)],
                [26.4, floor(26.4)],
                [26.4, floor(26.4) + 2.0],
                [17.0, floor(17.0) + 2.6],
            ],
            Finish::paper().near(),
        );
    }
    f.prism_x(p, [44.2, 45.0], &END, Finish::paper());
    f.block(p, [11.0, 26.4, 45.0, 27.6], [17.0, 20.2], 0.0, 0.0, lit());
}

/// Straight sloping hooks with a round tip mark at each free end.
fn hooks(p: &mut Part, f: Frame) {
    let length = 10.1_f32.hypot(4.5);
    for x in [14.0, 19.6, 25.2, 30.8, 36.4, 42.0] {
        f.bar(
            p,
            [x, 7.4, 25.0],
            [x, 17.5, 29.5],
            [0.7, 0.7],
            Finish::paper().near(),
        );
    }
    for x in [14.0, 19.6, 25.2, 30.8, 36.4, 42.0] {
        f.dot(
            p,
            [x, 17.5],
            29.5,
            [0.0, 10.1 / length, 4.5 / length],
            0.55,
            Stroke::new(Ink::Mid).near(),
        );
    }
}

/// The rail cap is the top deck, with four bolts outside the bars' clear central band.
fn cap(p: &mut Part, f: Frame) {
    f.block(
        p,
        [7.0, 4.0, 49.0, 16.0],
        [37.0, 40.0],
        1.0,
        0.6,
        Finish::paper(),
    );
    for at in [[9.5, 6.5], [46.5, 6.5], [9.5, 13.5], [46.5, 13.5]] {
        f.bolt(p, at, 40.0, [0.0, 0.0, 1.0]);
    }
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        assert_solids_near(Kind::Toolrack, 49, 15);
    }
}
