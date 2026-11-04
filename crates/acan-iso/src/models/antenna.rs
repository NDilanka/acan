//! The antenna: a dish on a square base, with a mast, an elevation yoke, a dish built as stacked elliptical tiers and a
//! feed horn at its focus. External clients, SDKs and integrations read as a dish that talks to things off the plant.

use std::f32::consts::TAU;

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, Plane, lit, polar, rect_on, unit};
use crate::plan::Point;

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 56.0,
    d: 56.0,
    top: 52.0,
};

/// The mast's axis, at the foot of the dish.
const MAST: Point = [28.0, 30.0];
/// The dish's vertex, forward of the mast: the centre of its tiers and of the feed.
const VERTEX: Point = [28.0, 36.0];
/// Points on each ellipse of the dish.
const ELLIPSE_STEPS: usize = 28;

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    base(&mut p, f);
    receiver(&mut p, f);
    mast(&mut p, f);
    yoke(&mut p, f);
    dish(&mut p, f);
    feed(&mut p, f);
    p.finish()
}

/// The base pad and the base block on it, with four anchor bolts, and the flange and its bolts round the mast foot.
fn base(p: &mut Part, f: Frame) {
    f.block(
        p,
        [10.0, 6.0, 46.0, 42.0],
        [3.0, 5.5],
        1.0,
        0.5,
        Finish::paper(),
    );
    f.block(
        p,
        [12.0, 6.0, 44.0, 40.0],
        [5.5, 9.0],
        1.0,
        0.6,
        Finish::paper(),
    );
    for at in [[14.5, 8.5], [41.5, 8.5], [14.5, 37.5], [41.5, 37.5]] {
        f.bolt(p, at, 9.0, [0.0, 0.0, 1.0]);
    }
    f.solid(
        p,
        &f.circle(MAST, 5.2, 32),
        [9.0, 11.0],
        0.0,
        Finish::paper(),
    );
    for degrees in [45.0, 135.0, 225.0, 315.0] {
        f.bolt(p, polar(MAST, 4.0, degrees), 11.0, [0.0, 0.0, 1.0]);
    }
}

/// The receiver box on the left of the mast, its two lid bolts, and the waveguide bridge that links it to the mast.
fn receiver(p: &mut Part, f: Frame) {
    // The bevel is 0.3 rather than the table's 0.4, because a corner radius equal to the bevel shrinks to nothing at it.
    f.block(
        p,
        [13.0, 16.0, 21.0, 26.0],
        [9.0, 17.0],
        0.4,
        0.3,
        Finish::paper(),
    );
    for at in [[15.0, 18.0], [19.0, 24.0]] {
        f.bolt(p, at, 17.0, [0.0, 0.0, 1.0]);
    }
    f.block(
        p,
        [21.0, 23.0, 26.0, 26.0],
        [12.0, 14.5],
        0.0,
        0.0,
        Finish::paper(),
    );
}

/// The mast column with its two band seams, and four radial gusset ribs round its foot.
fn mast(p: &mut Part, f: Frame) {
    f.solid(
        p,
        &f.circle(MAST, 2.8, 24),
        [11.0, 35.0],
        0.0,
        Finish::paper(),
    );
    for z in [19.0, 27.0] {
        f.circle_line(p, MAST, 2.85, z, 24, Stroke::new(Ink::Faint).near());
    }
    for degrees in [0.0, 90.0, 180.0, 270.0] {
        let fin = rect_on(
            polar(MAST, 3.1, degrees),
            unit(degrees),
            unit(degrees + 90.0),
            [1.1, 0.6],
        );
        f.solid(p, &f.poly(&fin), [11.0, 15.0], 0.0, Finish::paper());
    }
}

/// The elevation yoke: a base block on the mast's top and two cheeks either side, with a pivot bolt on each outer face.
fn yoke(p: &mut Part, f: Frame) {
    f.block(
        p,
        [23.5, 27.0, 32.5, 35.0],
        [35.0, 37.0],
        0.0,
        0.4,
        Finish::paper(),
    );
    for x0 in [22.5, 31.5] {
        f.block(
            p,
            [x0, 28.0, x0 + 2.0, 36.0],
            [37.0, 40.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    f.bolt(p, [22.5, 32.0], 38.5, [-1.0, 0.0, 0.0]);
    f.bolt(p, [33.5, 32.0], 38.5, [1.0, 0.0, 0.0]);
}

/// The dish: a backing boss and three tiers that step out to the rim, each a flat elliptical prism. Its two cavity
/// steps are drawn flat on the rim's top, and a vertex hub with four bolts sits at its centre.
fn dish(p: &mut Part, f: Frame) {
    f.solid(
        p,
        &f.poly(&ellipse([28.0, 34.5], 7.0, 5.0)),
        [38.0, 40.0],
        0.3,
        Finish::paper(),
    );
    f.solid(
        p,
        &f.poly(&ellipse([28.0, 35.0], 11.0, 7.8)),
        [40.0, 42.2],
        0.4,
        Finish::paper(),
    );
    f.solid(
        p,
        &f.poly(&ellipse([28.0, 35.5], 14.8, 10.4)),
        [42.2, 44.5],
        0.4,
        Finish::paper(),
    );
    f.solid(
        p,
        &f.poly(&ellipse(VERTEX, 18.0, 12.5)),
        [44.5, 47.0],
        0.5,
        Finish::paper(),
    );
    Plane::top(47.0).recess(p, f, &ellipse(VERTEX, 15.2, 10.5));
    Plane::top(47.0).recess(p, f, &ellipse(VERTEX, 10.5, 7.2));
    f.solid(
        p,
        &f.circle(VERTEX, 3.2, 24),
        [47.0, 48.0],
        0.0,
        Finish::paper(),
    );
    for degrees in [45.0, 135.0, 225.0, 315.0] {
        f.bolt(p, polar(VERTEX, 2.1, degrees), 48.0, [0.0, 0.0, 1.0]);
    }
}

/// The feed: three flat struts from the hub out to the rim, a support mast, a collar, and the lit feed horn at the focus.
fn feed(p: &mut Part, f: Frame) {
    for degrees in [90.0, 210.0, 330.0] {
        let strut = rect_on(
            polar(VERTEX, 7.0, degrees),
            unit(degrees),
            unit(degrees + 90.0),
            [4.0, 0.5],
        );
        f.solid(p, &f.poly(&strut), [47.0, 48.6], 0.0, Finish::paper());
    }
    f.solid(
        p,
        &f.circle(VERTEX, 0.9, 16),
        [48.0, 50.2],
        0.0,
        Finish::paper(),
    );
    f.solid(
        p,
        &f.circle(VERTEX, 1.6, 16),
        [49.6, 50.4],
        0.0,
        Finish::paper(),
    );
    f.solid(p, &f.circle(VERTEX, 2.4, 20), [50.4, 52.0], 0.4, lit());
}

/// An ellipse with `rx` and `ry` as its radii, as a convex polygon of [`ELLIPSE_STEPS`] points.
fn ellipse(centre: Point, rx: f32, ry: f32) -> Vec<Point> {
    (0..ELLIPSE_STEPS)
        .map(|step| {
            let angle = step as f32 / ELLIPSE_STEPS as f32 * TAU;
            [centre[0] + rx * angle.cos(), centre[1] + ry * angle.sin()]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        assert_solids_near(Kind::Antenna, 48, 15);
    }
}
