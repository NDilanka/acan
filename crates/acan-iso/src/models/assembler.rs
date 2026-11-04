//! The assembler: a low workbench on four legs, with a drawer front, three parts bins and a vice at the front right.
//! It is the default kind, so it is deliberately the plainest and lowest figure: the eye skips the ordinary benches.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, flat, lit};

/// The bench top is at 24; the vice's jaws rise to 30, so the footprint's top is 30.
pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 64.0,
    d: 64.0,
    top: 30.0,
};

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    legs(&mut p, f);
    bench(&mut p, f);
    bins(&mut p, f);
    vice(&mut p, f);
    p.finish()
}

/// Four legs with their foot pads, two stretchers along the long sides, and a lower shelf with its bolts.
fn legs(p: &mut Part, f: Frame) {
    for (x, y) in [(6.0, 10.0), (55.0, 10.0), (6.0, 51.0), (55.0, 51.0)] {
        f.block(
            p,
            [x, y, x + 3.0, y + 3.0],
            [3.6, 20.0],
            0.0,
            0.0,
            Finish::paper(),
        );
        let (cx, cy) = (x + 1.5, y + 1.5);
        f.block(
            p,
            [cx - 2.0, cy - 2.0, cx + 2.0, cy + 2.0],
            [3.0, 3.6],
            0.0,
            0.0,
            Finish::paper().close(),
        );
    }
    for y in [9.5, 50.5] {
        f.block(
            p,
            [9.0, y, 55.0, y + 2.0],
            [8.0, 10.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    f.block(
        p,
        [8.0, 12.0, 56.0, 56.0],
        [10.0, 12.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    for at in [[10.0, 14.0], [54.0, 14.0], [10.0, 54.0], [54.0, 54.0]] {
        f.bolt(p, at, 12.0, [0.0, 0.0, 1.0]);
    }
}

/// The bench top, and the front apron with its two drawers, their fronts and pulls.
fn bench(p: &mut Part, f: Frame) {
    f.block(
        p,
        [4.0, 8.0, 60.0, 60.0],
        [20.0, 24.0],
        1.0,
        0.8,
        Finish::paper(),
    );
    for x in [10.0, 36.0] {
        f.block(
            p,
            [x, 60.0, x + 14.0, 60.8],
            [14.0, 19.0],
            0.0,
            0.0,
            Finish::paper().near(),
        );
        f.block(
            p,
            [x + 5.0, 60.8, x + 9.0, 61.6],
            [16.2, 17.0],
            0.0,
            0.0,
            Finish::paper().near(),
        );
    }
    // The work mat is a thin plate on the bench, outlined, with a screw at each corner.
    f.block(
        p,
        [12.0, 22.0, 44.0, 56.0],
        [24.0, 24.2],
        0.0,
        0.0,
        flat(Ink::Lo),
    );
    f.line(
        p,
        &[[12.0, 22.0], [44.0, 22.0], [44.0, 56.0], [12.0, 56.0]],
        24.2,
        true,
        Stroke::new(Ink::Faint).near(),
    );
    for at in [[13.5, 23.5], [42.5, 23.5], [13.5, 54.5], [42.5, 54.5]] {
        f.bolt(p, at, 24.2, [0.0, 0.0, 1.0]);
    }
}

/// Three open parts bins on the left: a floor and four walls each.
fn bins(p: &mut Part, f: Frame) {
    for y in [24.0, 32.0, 40.0] {
        let (x0, x1, y0, y1, z) = (6.0, 12.0, y, y + 6.0, [24.0, 27.0]);
        let wall = 0.6;
        f.block(
            p,
            [x0, y0, x1, y0 + wall],
            z,
            0.0,
            0.0,
            Finish::paper().near(),
        );
        f.block(
            p,
            [x0, y1 - wall, x1, y1],
            z,
            0.0,
            0.0,
            Finish::paper().near(),
        );
        f.block(
            p,
            [x0, y0, x0 + wall, y1],
            z,
            0.0,
            0.0,
            Finish::paper().near(),
        );
        f.block(
            p,
            [x1 - wall, y0, x1, y1],
            z,
            0.0,
            0.0,
            Finish::paper().near(),
        );
        f.block(
            p,
            [x0 + wall, y0 + wall, x1 - wall, y1 - wall],
            [24.0, 24.6],
            0.0,
            0.0,
            Finish::paper().near(),
        );
    }
}

/// The vice: a base, a fixed jaw, a moving jaw with its lit pad, a lead screw and a T handle.
fn vice(p: &mut Part, f: Frame) {
    f.block(
        p,
        [46.0, 40.0, 58.0, 56.0],
        [24.0, 26.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.block(
        p,
        [46.0, 40.0, 58.0, 43.0],
        [26.0, 30.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.block(
        p,
        [46.0, 47.0, 58.0, 50.0],
        [26.0, 30.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.block(p, [46.0, 46.6, 58.0, 47.0], [26.0, 30.0], 0.0, 0.0, lit());
    f.block(
        p,
        [51.6, 50.0, 52.4, 58.0],
        [27.6, 28.4],
        0.0,
        0.3,
        Finish::paper().near(),
    );
    f.block(
        p,
        [48.0, 58.0, 56.0, 58.8],
        [27.6, 28.4],
        0.0,
        0.0,
        Finish::paper().close(),
    );
    for x in [48.0, 56.0] {
        f.bolt(p, [x, 58.4], 28.0, [0.0, 1.0, 0.0]);
    }
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        // The design count is 40 and the figure draws about 51 at two bars, so the tolerance covers it.
        assert_solids_near(Kind::Assembler, 40, 28);
    }
}
