//! The valve: a gate valve in a pipe run, with two wide flanges, a pinched waist and a four-spoke handwheel. It is the
//! smallest figure, so a rate limit reads as a narrow module, and its wheel is a shape no other kind has.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, polar, rect_on, unit};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 40.0,
    d: 40.0,
    top: 20.0,
};

/// The centre of the handwheel and the stem.
const HUB: [f32; 2] = [20.0, 22.0];

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    pad(&mut p, f);
    flanges(&mut p, f);
    body(&mut p, f);
    wheel(&mut p, f);
    p.finish()
}

/// The tag pad on the plinth at the back, with four screws, and a foot under each flange.
fn pad(p: &mut Part, f: Frame) {
    f.block(
        p,
        [8.0, 4.0, 32.0, 16.0],
        [3.0, 5.0],
        0.6,
        0.0,
        Finish::paper(),
    );
    for at in [[9.5, 5.5], [30.5, 5.5], [9.5, 14.5], [30.5, 14.5]] {
        f.bolt(p, at, 5.0, [0.0, 0.0, 1.0]);
    }
    for x0 in [4.0, 32.0] {
        f.block(
            p,
            [x0, 14.0, x0 + 4.0, 30.0],
            [3.0, 3.6],
            0.0,
            0.0,
            Finish::paper().close(),
        );
    }
}

/// The two flanges, each with four bolts and a gasket pad on its outer face.
fn flanges(p: &mut Part, f: Frame) {
    for (x0, outward) in [(4.0, -1.0), (32.0, 1.0)] {
        f.block(
            p,
            [x0, 14.0, x0 + 4.0, 30.0],
            [3.6, 15.0],
            0.0,
            0.6,
            Finish::paper(),
        );
        let face = if outward < 0.0 { x0 } else { x0 + 4.0 };
        for y in [15.5, 28.5] {
            for z in [5.1, 13.5] {
                f.bolt(p, [face, y], z, [outward, 0.0, 0.0]);
            }
        }
        // The gasket pad sits on the outer face, between the two rows of bolts.
        let (gx0, gx1) = if outward < 0.0 {
            (x0 - 0.6, x0)
        } else {
            (x0 + 4.0, x0 + 4.6)
        };
        f.block(
            p,
            [gx0, 15.0, gx1, 29.0],
            [7.5, 9.5],
            0.0,
            0.0,
            Finish::paper(),
        );
        parts::sideline(
            p,
            &f.rect([x0, 14.0, x0 + 4.0, 30.0], 0.0),
            9.0,
            Stroke::new(Ink::Faint).close(),
        );
    }
}

/// The body between the flanges: two ends and a waist, with the seams where they meet, and the bonnet on top.
fn body(p: &mut Part, f: Frame) {
    for x0 in [8.0, 24.0] {
        f.block(
            p,
            [x0, 16.0, x0 + 8.0, 28.0],
            [5.0, 13.0],
            0.0,
            0.6,
            Finish::paper().near(),
        );
    }
    f.block(
        p,
        [16.0, 18.0, 24.0, 26.0],
        [6.0, 12.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.line3(
        p,
        &[[16.0, 28.02, 6.0], [16.0, 28.02, 12.0]],
        Stroke::new(Ink::Faint).close(),
    );
    f.line3(
        p,
        &[[24.0, 26.02, 6.0], [24.0, 26.02, 12.0]],
        Stroke::new(Ink::Faint).close(),
    );
    f.solid(
        p,
        &f.circle(HUB, 4.0, 24),
        [12.0, 16.0],
        0.0,
        Finish::paper().near(),
    );
    for k in 0..4 {
        f.bolt(
            p,
            polar(HUB, 3.0, 45.0 + 90.0 * k as f32),
            16.0,
            [0.0, 0.0, 1.0],
        );
    }
    f.solid(
        p,
        &f.circle(HUB, 2.0, 16),
        [16.0, 17.0],
        0.0,
        Finish::paper().close(),
    );
    f.solid(
        p,
        &f.circle(HUB, 0.8, 12),
        [17.0, 19.0],
        0.0,
        Finish::paper().close(),
    );
    f.solid(
        p,
        &f.circle(HUB, 1.2, 12),
        [19.0, 19.6],
        0.0,
        Finish::paper().close(),
    );
}

/// The handwheel with its knurled rim, four raised spokes, and the lit hub.
fn wheel(p: &mut Part, f: Frame) {
    f.solid(
        p,
        &f.circle(HUB, 5.0, 32),
        [17.8, 19.2],
        0.0,
        Finish::paper().near(),
    );
    f.knurl(p, HUB, 5.0, [17.8, 19.2], 24, Stroke::new(Ink::Lo).close());
    // Each spoke is a flat bar that stands on the wheel's top, from just off the hub out to the rim.
    for angle in [0.0, 90.0, 180.0, 270.0] {
        let spoke = rect_on(
            polar(HUB, 3.1, angle),
            unit(angle),
            unit(angle + 90.0),
            [1.6, 0.45],
        );
        f.solid(
            p,
            &f.poly(&spoke),
            [19.2, 20.0],
            0.0,
            Finish::paper().near(),
        );
    }
    // The hub is the figure's one lit mark, a flat black disc.
    f.dot(
        p,
        HUB,
        19.6,
        [0.0, 0.0, 1.0],
        0.8,
        Stroke::new(Ink::Hi).near(),
    );
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near, solids_at_two_bars};

    #[test]
    fn solids_match_the_design_and_meet_the_floor() {
        assert!(solids_at_two_bars(Kind::Valve) >= 38);
        assert_solids_near(Kind::Valve, 42, 15);
    }
}
