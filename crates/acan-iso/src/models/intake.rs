//! The intake: a loading hopper over a roller dock, the tallest figure. Files arrive on the dock and fall through the
//! hopper into the plant, so the eye finds the entry point first. Its hopper is a hollow funnel, so from above its
//! mouth narrows to the throat.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, lit};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 72.0,
    d: 88.0,
    top: 34.0,
};

/// The hopper's wall thickness.
const WALL: f32 = 0.6;
/// The hopper's mouth, at the deck, and its throat, at the bottom.
const MOUTH: [[f32; 2]; 4] = [[8.0, 16.0], [64.0, 16.0], [64.0, 40.0], [8.0, 40.0]];
const THROAT: [[f32; 2]; 4] = [[30.0, 36.0], [42.0, 36.0], [42.0, 44.0], [30.0, 44.0]];
const MOUTH_Z: f32 = 31.0;
const THROAT_Z: f32 = 12.0;

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    dock(&mut p, f);
    posts(&mut p, f);
    deck(&mut p, f);
    hopper(&mut p, f);
    rollers(&mut p, f);
    handle(&mut p, f);
    p.finish()
}

/// The roller dock: a bevelled apron with tread ribs and a bumper along its front.
fn dock(p: &mut Part, f: Frame) {
    f.block(
        p,
        [4.0, 48.0, 68.0, 84.0],
        [3.0, 9.0],
        1.0,
        0.6,
        Finish::paper(),
    );
    f.block(
        p,
        [4.0, 83.0, 68.0, 84.4],
        [8.0, 9.6],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    for x in [12.0, 20.0, 28.0, 36.0, 44.0, 52.0] {
        f.line(
            p,
            &[[x, 50.0], [x, 82.0]],
            9.0,
            false,
            Stroke::new(Ink::Faint).close(),
        );
    }
    for at in [[8.0, 52.0], [64.0, 52.0], [8.0, 80.0], [64.0, 80.0]] {
        f.bolt(p, at, 9.0, [0.0, 0.0, 1.0]);
    }
}

/// Four tower posts with their feet, two braces across the back, side rails, and gussets at the joints.
fn posts(p: &mut Part, f: Frame) {
    let corners = [(6.0, 6.0), (63.0, 6.0), (6.0, 41.0), (63.0, 41.0)];
    for (x, y) in corners {
        f.block(
            p,
            [x, y, x + 3.0, y + 3.0],
            [3.0, 31.0],
            0.0,
            0.0,
            Finish::paper(),
        );
        let (cx, cy) = (x + 1.5, y + 1.5);
        f.block(
            p,
            [cx - 2.5, cy - 2.5, cx + 2.5, cy + 2.5],
            [3.0, 4.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    for z in [15.0, 27.0] {
        f.block(
            p,
            [9.0, 6.0, 63.0, 8.0],
            [z, z + 2.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    for x in [6.5, 63.5] {
        f.block(
            p,
            [x, 9.0, x + 2.0, 41.0],
            [27.0, 29.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    for (x, y) in corners {
        let ix = if x < 36.0 { x + 3.0 } else { x - 2.0 };
        let iy = if y < 22.0 { y + 3.0 } else { y - 2.0 };
        for z in [13.0, 25.0] {
            f.block(
                p,
                [ix, iy, ix + 2.0, iy + 2.0],
                [z, z + 2.0],
                0.0,
                0.0,
                Finish::paper().near(),
            );
        }
    }
}

/// The rim deck, with its mouth outlined, and the stiffeners along its long faces.
fn deck(p: &mut Part, f: Frame) {
    // Four blocks make the deck round the mouth, so the mouth stays open.
    let z = [31.0, 34.0];
    f.block(p, [4.0, 4.0, 68.0, 16.0], z, 0.0, 0.0, Finish::paper());
    f.block(p, [4.0, 40.0, 68.0, 44.0], z, 0.0, 0.0, Finish::paper());
    f.block(p, [4.0, 16.0, 8.0, 40.0], z, 0.0, 0.0, Finish::paper());
    f.block(p, [64.0, 16.0, 68.0, 40.0], z, 0.0, 0.0, Finish::paper());
    f.line(p, &MOUTH, 34.0, true, Stroke::new(Ink::Faint).near());
    for x in [16.0, 36.0, 56.0] {
        f.block(
            p,
            [x - 0.5, 44.0, x + 0.5, 45.0],
            [31.0, 33.0],
            0.0,
            0.0,
            Finish::paper().close(),
        );
        f.block(
            p,
            [x - 0.5, 3.0, x + 0.5, 4.0],
            [31.0, 33.0],
            0.0,
            0.0,
            Finish::paper().close(),
        );
    }
}

/// The hopper: a hollow funnel from the mouth down to the throat. Its walls have an outer and an inner face, so from
/// above the inner face shows the funnel, and its bottom is open at the throat.
fn hopper(p: &mut Part, f: Frame) {
    let centre = [36.0, 32.0, 21.5];
    let outer_top = ring(&MOUTH, 0.0, MOUTH_Z);
    let outer_bottom = ring(&THROAT, 0.0, THROAT_Z);
    let inner_top = ring(&MOUTH, WALL, MOUTH_Z);
    let inner_bottom = ring(&THROAT, WALL, THROAT_Z);
    let finish = Finish::paper();
    for i in 0..4 {
        let j = (i + 1) % 4;
        let outside = |a: [f32; 3], b: [f32; 3]| {
            let mid = mid3(a, b);
            [mid[0] - centre[0], mid[1] - centre[1], mid[2] - centre[2]]
        };
        let inside = |a: [f32; 3], b: [f32; 3]| {
            let out = outside(a, b);
            [-out[0], -out[1], -out[2]]
        };
        let wall = [outer_bottom[i], outer_bottom[j], outer_top[j], outer_top[i]];
        f.quad(p, wall, outside(outer_bottom[i], outer_top[j]), finish);
        let inner = [inner_bottom[i], inner_bottom[j], inner_top[j], inner_top[i]];
        f.quad(p, inner, inside(inner_bottom[i], inner_top[j]), finish);
        let top = [outer_top[i], outer_top[j], inner_top[j], inner_top[i]];
        f.quad(p, top, [0.0, 0.0, 1.0], finish);
        let bottom = [
            outer_bottom[i],
            outer_bottom[j],
            inner_bottom[j],
            inner_bottom[i],
        ];
        f.quad(p, bottom, [0.0, 0.0, -1.0], finish);
    }
    // The throat plate closes the funnel's foot. It is lit, so the throat reads from above.
    f.block(
        p,
        [31.0, 37.0, 41.0, 43.0],
        [THROAT_Z, THROAT_Z + 0.3],
        0.0,
        0.0,
        lit(),
    );
    f.block(
        p,
        [30.0, 40.0, 42.0, 48.0],
        [9.0, THROAT_Z],
        0.0,
        0.0,
        Finish::paper().near(),
    );
}

/// Five rollers across the dock, two guides that hold them, and an axle at each end of each roller.
fn rollers(p: &mut Part, f: Frame) {
    for y in [55.0, 61.0, 67.0, 73.0, 79.0] {
        f.block(
            p,
            [10.0, y, 62.0, y + 3.0],
            [9.0, 12.0],
            0.0,
            1.2,
            Finish::paper().near(),
        );
        f.bolt(p, [10.0, y + 1.5], 10.5, [-1.0, 0.0, 0.0]);
        f.bolt(p, [62.0, y + 1.5], 10.5, [1.0, 0.0, 0.0]);
    }
    for x in [9.0, 62.0] {
        f.block(
            p,
            [x, 52.0, x + 1.0, 82.0],
            [9.0, 12.0],
            0.0,
            0.0,
            Finish::paper().near(),
        );
    }
}

/// The pull handle on the deck's front, with a knurled knob at its end.
fn handle(p: &mut Part, f: Frame) {
    f.block(
        p,
        [60.0, 44.0, 66.0, 45.0],
        [30.0, 31.0],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.solid(
        p,
        &f.circle([66.0, 45.5], 1.5, 16),
        [29.5, 31.0],
        0.0,
        Finish::paper().near(),
    );
    f.knurl(
        p,
        [66.0, 45.5],
        1.5,
        [29.5, 31.0],
        12,
        Stroke::new(Ink::Lo).close(),
    );
}

/// A closed ring of the design points `points` at height `z`, inset by `inset` on every side.
fn ring(points: &[[f32; 2]; 4], inset: f32, z: f32) -> [[f32; 3]; 4] {
    let centre = [
        points.iter().map(|p| p[0]).sum::<f32>() / 4.0,
        points.iter().map(|p| p[1]).sum::<f32>() / 4.0,
    ];
    points.map(|[x, y]| {
        let (dx, dy) = (centre[0] - x, centre[1] - y);
        let length = dx.hypot(dy);
        let step = if length > 0.0 { inset / length } else { 0.0 };
        [x + dx * step, y + dy * step, z]
    })
}

/// The midpoint of two points.
fn mid3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        (a[0] + b[0]) / 2.0,
        (a[1] + b[1]) / 2.0,
        (a[2] + b[2]) / 2.0,
    ]
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        // The design count is 66 and the figure draws about 76 at two bars, so the tolerance covers it.
        assert_solids_near(Kind::Intake, 66, 16);
    }
}
