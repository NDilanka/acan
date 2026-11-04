//! The horn: a loudspeaker bell on a tall mast over a heavy base, with a swivel bracket and a driver housing, and a flared
//! bell built as stacked rings that points front right. Events, messaging and hooks read as a point that broadcasts.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, Plane, lit, polar};
use crate::plan::{Point, circle_ring};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 40.0,
    d: 40.0,
    top: 56.0,
};

/// The mast's axis.
const MAST: Point = [18.0, 25.0];
/// The driver housing's centre, held by the swivel bracket.
const DRIVER: Point = [21.5, 23.5];
/// The bell's mouth centre, front right of the mast.
const MOUTH: Point = [25.5, 27.5];

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    base(&mut p, f);
    junction(&mut p, f);
    mast(&mut p, f);
    swivel(&mut p, f);
    driver(&mut p, f);
    bell(&mut p, f);
    mouth(&mut p, f);
    p.finish()
}

/// The base pad and the base block on it, four corner bolts, and the flange and its bolts round the mast foot.
fn base(p: &mut Part, f: Frame) {
    f.block(
        p,
        [8.0, 6.0, 32.0, 32.0],
        [3.0, 5.5],
        0.8,
        0.4,
        Finish::paper(),
    );
    f.block(
        p,
        [9.0, 6.0, 31.0, 31.0],
        [5.5, 9.0],
        0.8,
        0.6,
        Finish::paper(),
    );
    for at in [[11.5, 8.5], [28.5, 8.5], [11.5, 28.5], [28.5, 28.5]] {
        f.bolt(p, at, 9.0, [0.0, 0.0, 1.0]);
    }
    f.solid(
        p,
        &f.circle(MAST, 4.2, 24),
        [9.0, 10.8],
        0.0,
        Finish::paper(),
    );
    for degrees in [45.0, 135.0, 225.0, 315.0] {
        f.bolt(p, polar(MAST, 3.1, degrees), 10.8, [0.0, 0.0, 1.0]);
    }
}

/// The junction box on the base block beside the mast, its two bolts, and the conduit boss that joins it to the flange.
fn junction(p: &mut Part, f: Frame) {
    f.block(
        p,
        [10.0, 18.0, 14.5, 24.0],
        [9.0, 13.5],
        0.0,
        0.3,
        Finish::paper(),
    );
    for at in [[11.5, 19.5], [13.0, 22.5]] {
        f.bolt(p, at, 13.5, [0.0, 0.0, 1.0]);
    }
    f.block(
        p,
        [14.5, 21.5, 16.0, 23.5],
        [10.0, 12.0],
        0.0,
        0.0,
        Finish::paper(),
    );
}

/// The mast post with three band seams, and its collar under the swivel.
fn mast(p: &mut Part, f: Frame) {
    f.solid(
        p,
        &f.circle(MAST, 2.2, 20),
        [10.8, 33.5],
        0.0,
        Finish::paper(),
    );
    for z in [16.0, 22.0, 28.0] {
        f.circle_line(p, MAST, 2.25, z, 20, Stroke::new(Ink::Faint).near());
    }
    f.solid(
        p,
        &f.circle(MAST, 2.8, 24),
        [33.5, 35.0],
        0.0,
        Finish::paper(),
    );
}

/// The swivel: a pedestal and a bracket yoke on the collar, two bolts on the yoke, two trunnion cheeks either side of the
/// driver, and a lock bolt on each cheek's outer face.
fn swivel(p: &mut Part, f: Frame) {
    f.block(
        p,
        [15.5, 22.5, 20.5, 27.5],
        [35.0, 36.5],
        0.0,
        0.3,
        Finish::paper(),
    );
    f.block(
        p,
        [16.5, 21.5, 24.0, 26.5],
        [36.5, 38.0],
        0.0,
        0.4,
        Finish::paper(),
    );
    for at in [[17.5, 23.0], [17.5, 25.5]] {
        f.bolt(p, at, 38.0, [0.0, 0.0, 1.0]);
    }
    for y0 in [20.5, 25.5] {
        f.block(
            p,
            [19.5, y0, 23.5, y0 + 1.0],
            [38.0, 41.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    f.bolt(p, [21.5, 20.5], 39.5, [0.0, -1.0, 0.0]);
    f.bolt(p, [21.5, 26.5], 39.5, [0.0, 1.0, 0.0]);
}

/// The driver housing held in the bracket, with four bolts on its top.
fn driver(p: &mut Part, f: Frame) {
    f.solid(
        p,
        &f.circle(DRIVER, 3.2, 24),
        [38.0, 40.5],
        0.4,
        Finish::paper(),
    );
    for degrees in [45.0, 135.0, 225.0, 315.0] {
        f.bolt(p, polar(DRIVER, 2.2, degrees), 40.5, [0.0, 0.0, 1.0]);
    }
}

/// The flared bell: five stacked rings whose centres step from the driver out to the mouth. The kit turns round parts
/// only about z, so the bell is built this way rather than as a cone.
fn bell(p: &mut Part, f: Frame) {
    let rings = [
        ([22.3, 24.3], 3.8, [40.5, 43.2], 0.0),
        ([23.1, 25.1], 4.8, [43.2, 46.0], 0.0),
        ([23.9, 25.9], 6.0, [46.0, 49.0], 0.0),
        ([24.7, 26.7], 7.2, [49.0, 52.0], 0.0),
        (MOUTH, 8.3, [52.0, 54.5], 0.3),
    ];
    for (centre, radius, z, bevel) in rings {
        f.solid(p, &f.circle(centre, radius, 28), z, bevel, Finish::paper());
    }
}

/// The mouth: a lit rim crowning the bell, two dark recesses inside it, the phase plug and its tip.
fn mouth(p: &mut Part, f: Frame) {
    f.solid(p, &f.circle(MOUTH, 8.8, 32), [54.5, 56.0], 0.3, lit());
    Plane::top(56.0).recess(p, f, &circle_ring(MOUTH[0], MOUTH[1], 7.3, 28));
    Plane::top(56.0).recess(p, f, &circle_ring(25.0, 27.0, 4.6, 24));
    f.solid(
        p,
        &f.circle([24.6, 26.6], 2.2, 20),
        [55.2, 56.0],
        0.3,
        Finish::paper(),
    );
    // The tip is a rivet-like disc, counted as a solid as a bolt head is.
    p.solids += 1;
    f.dot(
        p,
        [24.6, 26.6],
        56.0,
        [0.0, 0.0, 1.0],
        0.6,
        Stroke::new(Ink::Lo).close(),
    );
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        assert_solids_near(Kind::Horn, 47, 15);
    }
}
