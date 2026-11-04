//! The switchboard: a wide telephone cabinet with an angled jack panel, a row of patch plugs and a cord tray.
//! Routes, controllers and handlers read as a shallow wedge with a regular grid of sockets.

use crate::mesh::{Finish, Part};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, Plane, lit, rectangle};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 64.0,
    d: 40.0,
    top: 40.0,
};

const CHEEK: [[f32; 2]; 5] = [
    [5.0, 4.0],
    [32.0, 4.0],
    [32.0, 15.0],
    [19.6, 36.0],
    [5.0, 36.0],
];

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    for x in [5.5, 55.5] {
        for y in [9.0, 26.0] {
            f.block(
                &mut p,
                [x, y, x + 3.0, y + 4.0],
                [3.0, 4.0],
                0.0,
                0.0,
                Finish::paper(),
            );
        }
    }
    f.prism_x(&mut p, [6.0, 8.0], &CHEEK, Finish::paper());
    f.prism_x(
        &mut p,
        [8.0, 56.0],
        &[
            [6.0, 4.0],
            [30.0, 4.0],
            [30.0, 14.0],
            [17.0, 36.0],
            [6.0, 36.0],
        ],
        Finish::paper(),
    );
    f.block(
        &mut p,
        [8.0, 30.0, 56.0, 30.6],
        [4.0, 7.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.block(
        &mut p,
        [12.0, 30.6, 52.0, 35.0],
        [4.0, 8.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    Plane::top(8.0).recess(&mut p, f, &rectangle([13.0, 31.6, 51.0, 34.0]));
    panel(&mut p, f);
    f.block(
        &mut p,
        [8.0, 29.5, 56.0, 32.0],
        [13.0, 15.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    for column in 0..8 {
        let x = 13.0 + 38.0 * column as f32 / 7.0;
        f.solid(
            &mut p,
            &f.circle([x, 30.8], 0.9, 12),
            [15.0, 17.2],
            0.0,
            lit(),
        );
    }
    f.prism_x(&mut p, [56.0, 58.0], &CHEEK, Finish::paper());
    for y in [10.0, 26.0] {
        f.bolt(&mut p, [58.0, y], 8.0, [1.0, 0.0, 0.0]);
    }
    cap(&mut p, f);
    p.finish()
}

/// The flat jack plate and its three rows of eight sockets on the cabinet's sloped face.
fn panel(p: &mut Part, f: Frame) {
    let length = 13.0_f32.hypot(22.0);
    let plane = Plane::new(
        [8.0, 30.0, 14.0],
        [0.0, -13.0 / length, 22.0 / length],
        [1.0, 0.0, 0.0],
    );
    plane.face(
        p,
        f,
        &rectangle([2.5, 3.0, 19.5, 45.0]),
        Finish::paper().near(),
    );
    for s in [5.5, 11.0, 16.5] {
        for column in 0..8 {
            let t = 5.0 + 38.0 * column as f32 / 7.0;
            plane
                .lifted(crate::mesh::LIFT)
                .bolt(p, f, [s, t], [0.55, 0.9], [Ink::Hi, Ink::Mid]);
        }
    }
}

/// The capped back rail is the top deck; its clear middle holds the data-driven file bars.
fn cap(p: &mut Part, f: Frame) {
    f.block(
        p,
        [6.0, 4.0, 58.0, 18.0],
        [36.0, 40.0],
        1.0,
        0.6,
        Finish::paper(),
    );
    for at in [[8.0, 6.0], [56.0, 6.0], [8.0, 16.0], [56.0, 16.0]] {
        f.bolt(p, at, 40.0, [0.0, 0.0, 1.0]);
    }
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        assert_solids_near(Kind::Switchboard, 54, 15);
    }
}
