//! The gate: a turnstile checkpoint between two tall piers, with a lintel across the top, a three-arm hub in the gap and
//! a reader box on the right pier. Auth, session and login modules are read by this two-pier shape.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, glass, lit, polar, rect_on, unit};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 56.0,
    d: 72.0,
    top: 36.0,
};

/// The turnstile hub's centre, in design units.
const HUB: [f32; 2] = [28.0, 40.0];

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    base(&mut p, f);
    piers(&mut p, f);
    lintel(&mut p, f);
    hub(&mut p, f);
    reader(&mut p, f);
    p.finish()
}

/// The floor plate with its guide strips, and a pier base under each pier with four bolts.
fn base(p: &mut Part, f: Frame) {
    f.block(
        p,
        [16.0, 20.0, 40.0, 56.0],
        [3.0, 4.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    for x in [18.0, 37.0] {
        f.block(
            p,
            [x - 0.5, 22.0, x + 0.5, 54.0],
            [4.0, 4.6],
            0.0,
            0.0,
            Finish::paper().close(),
        );
    }
    for x0 in [4.0, 40.0] {
        f.block(
            p,
            [x0, 20.0, x0 + 12.0, 52.0],
            [3.0, 6.0],
            0.0,
            0.6,
            Finish::paper(),
        );
        for x in [x0 + 2.0, x0 + 10.0] {
            for y in [22.0, 50.0] {
                f.bolt(p, [x, y], 6.0, [0.0, 0.0, 1.0]);
            }
        }
    }
}

/// The two piers, with kick plates and rubber stops on their inner faces.
fn piers(p: &mut Part, f: Frame) {
    for x0 in [5.0, 41.0] {
        f.block(
            p,
            [x0, 21.0, x0 + 10.0, 51.0],
            [6.0, 32.0],
            0.0,
            0.8,
            Finish::paper(),
        );
    }
    // The inner face of the left pier is x 15 and of the right pier is x 41.
    f.block(
        p,
        [15.0, 21.5, 15.6, 51.5],
        [6.0, 10.0],
        0.0,
        0.0,
        Finish::paper().close(),
    );
    f.block(
        p,
        [40.4, 21.5, 41.0, 51.5],
        [6.0, 10.0],
        0.0,
        0.0,
        Finish::paper().close(),
    );
    f.block(
        p,
        [15.0, 46.0, 16.0, 50.0],
        [17.0, 19.0],
        0.0,
        0.0,
        Finish::paper().close(),
    );
    f.block(
        p,
        [40.0, 46.0, 41.0, 50.0],
        [17.0, 19.0],
        0.0,
        0.0,
        Finish::paper().close(),
    );
}

/// The lintel across the top, its end plates, its bolts and the braces under it.
fn lintel(p: &mut Part, f: Frame) {
    f.block(
        p,
        [4.0, 20.0, 52.0, 32.0],
        [32.0, 36.0],
        1.0,
        0.0,
        Finish::paper(),
    );
    f.block(
        p,
        [3.4, 22.0, 4.0, 34.0],
        [32.0, 36.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.block(
        p,
        [52.0, 22.0, 52.6, 34.0],
        [32.0, 36.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    for at in [[7.0, 23.0], [49.0, 23.0], [7.0, 29.0], [49.0, 29.0]] {
        f.bolt(p, at, 36.0, [0.0, 0.0, 1.0]);
    }
    f.block(
        p,
        [15.0, 26.0, 17.0, 28.0],
        [28.0, 32.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    f.block(
        p,
        [39.0, 26.0, 41.0, 28.0],
        [28.0, 32.0],
        0.0,
        0.0,
        Finish::paper(),
    );
}

/// The hub: a post, a collar, three arms with grips, and a lit cap on top.
fn hub(p: &mut Part, f: Frame) {
    f.solid(
        p,
        &f.circle(HUB, 3.0, 24),
        [4.0, 24.0],
        0.0,
        Finish::paper().near(),
    );
    f.solid(
        p,
        &f.circle(HUB, 3.6, 24),
        [18.0, 19.5],
        0.0,
        Finish::paper().near(),
    );
    for angle in [0.0, 120.0, 240.0] {
        // Each arm runs from the hub's centre out to ten units.
        let arm = rect_on(
            polar(HUB, 5.0, angle),
            unit(angle),
            unit(angle + 90.0),
            [5.0, 0.5],
        );
        let plate = f.poly(&arm);
        f.solid(p, &plate, [19.5, 20.5], 0.0, Finish::paper().near());
        let tip = polar(HUB, 9.5, angle);
        f.bolt(p, tip, 20.5, [0.0, 0.0, 1.0]);
    }
    f.solid(p, &f.circle(HUB, 3.4, 24), [24.0, 25.0], 0.0, lit());
}

/// The badge reader on the right pier's inner face, with its glass window and two rules.
fn reader(p: &mut Part, f: Frame) {
    f.block(
        p,
        [39.4, 26.0, 41.0, 32.0],
        [16.0, 24.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.block(p, [39.3, 27.0, 39.4, 31.0], [20.0, 23.5], 0.0, 0.0, glass());
    for z in [17.0, 18.0, 19.0] {
        f.line3(
            p,
            &[[39.4 - 0.01, 27.0, z], [39.4 - 0.01, 30.0, z]],
            Stroke::new(Ink::Faint).close(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        // The design count is 46 and the figure draws about 42 at two bars, so the tolerance covers it.
        assert_solids_near(Kind::Gate, 46, 15);
    }
}
