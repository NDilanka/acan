//! The tank: a storage silo on a skirt, with a hatch on its deck and a ladder up its near side. It is the only round
//! figure, so the store, db and repo modules are found across the map, and it is where a clash on stored data shows.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, lit, polar, rect_on, unit};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 80.0,
    d: 80.0,
    top: 48.0,
};

/// The axis of the silo, in design units.
const CENTRE: [f32; 2] = [40.0, 40.0];

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    skirt(&mut p, f);
    body(&mut p, f);
    deck(&mut p, f);
    ladder(&mut p, f);
    p.finish()
}

/// The skirt the silo stands on, its bolts, four anchor lugs, and an inspection door on its front.
fn skirt(p: &mut Part, f: Frame) {
    f.solid(
        p,
        &f.circle(CENTRE, 32.0, 32),
        [3.0, 11.0],
        0.0,
        Finish::paper(),
    );
    for k in 0..8 {
        let at = polar(CENTRE, 30.0, 22.5 + 45.0 * k as f32);
        f.bolt(p, at, 11.0, [0.0, 0.0, 1.0]);
    }
    for angle in [45.0, 135.0, 225.0, 315.0] {
        let lug = rect_on(
            polar(CENTRE, 31.0, angle),
            unit(angle),
            unit(angle + 90.0),
            [2.0, 1.5],
        );
        f.solid(p, &f.poly(&lug), [3.0, 5.0], 0.0, Finish::paper().near());
    }
    door(p, f);
}

/// The inspection door: a plate on the skirt at 45 degrees, from z 4 to z 10, with two hinges and a latch.
fn door(p: &mut Part, f: Frame) {
    // The door spans four units of arc either side of its centre line, at a radius of 32.
    let half = 4.0 / 32.0_f32;
    let centre_angle = 45.0_f32.to_radians();
    let (lo, hi) = (centre_angle - half, centre_angle + half);
    let at = |radius: f32, angle: f32| polar(CENTRE, radius, angle.to_degrees());
    let plate = [at(32.3, lo), at(32.3, hi), at(31.5, hi), at(31.5, lo)];
    f.solid(
        p,
        &f.poly(&plate),
        [4.0, 10.0],
        0.0,
        Finish::paper().close(),
    );
    for z in [5.0, 9.0] {
        f.bolt(p, at(32.3, lo), z, [lo.cos(), lo.sin(), 0.0]);
    }
    let latch = rect_on(
        at(32.5, hi - half * 0.4),
        unit(45.0),
        unit(135.0),
        [0.5, 0.4],
    );
    f.solid(p, &f.poly(&latch), [6.6, 7.4], 0.0, Finish::paper().close());
}

/// The silo body with its band seams and vertical seams, and the three roof tiers.
fn body(p: &mut Part, f: Frame) {
    f.solid(
        p,
        &f.circle(CENTRE, 30.0, 32),
        [11.0, 40.0],
        0.0,
        Finish::paper(),
    );
    for z in [18.0, 26.0, 34.0] {
        f.circle_line(p, CENTRE, 30.05, z, 32, Stroke::new(Ink::Faint).near());
    }
    for angle in [20.0, 70.0] {
        let at = polar(CENTRE, 30.05, angle);
        f.line3(
            p,
            &[[at[0], at[1], 11.0], [at[0], at[1], 40.0]],
            Stroke::new(Ink::Faint).near(),
        );
    }
    for (radius, z) in [(30.0, 40.0), (24.0, 42.0), (16.0, 44.0)] {
        f.solid(
            p,
            &f.circle(CENTRE, radius, 32),
            [z, z + 2.0],
            0.0,
            Finish::paper(),
        );
    }
}

/// The hatch deck with its bolts, and the hatch lid with its hinge and a lit handle across its top.
fn deck(p: &mut Part, f: Frame) {
    f.block(
        p,
        [26.0, 26.0, 54.0, 54.0],
        [46.0, 46.8],
        1.0,
        0.0,
        Finish::paper(),
    );
    for at in [[28.0, 28.0], [52.0, 28.0], [28.0, 52.0], [52.0, 52.0]] {
        f.bolt(p, at, 46.8, [0.0, 0.0, 1.0]);
    }
    f.solid(
        p,
        &f.circle(CENTRE, 5.0, 24),
        [46.8, 47.4],
        0.0,
        Finish::paper(),
    );
    f.block(
        p,
        [39.0, 40.5, 41.0, 41.5],
        [46.8, 47.4],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.block(p, [37.0, 45.6, 43.0, 46.4], [47.4, 48.0], 0.0, 0.0, lit());
}

/// The ladder on the near side: two rails, seven rungs between them.
fn ladder(p: &mut Part, f: Frame) {
    let rails = [[62.0, 58.0], [66.0, 54.0]];
    for at in rails {
        f.block(
            p,
            [at[0], at[1], at[0] + 1.0, at[1] + 1.0],
            [3.0, 44.0],
            0.0,
            0.0,
            Finish::paper().near(),
        );
    }
    // The rails run down-right, so the rungs run down-right too, across the gap between the rails.
    let mid = [
        (rails[0][0] + rails[1][0]) / 2.0 + 0.5,
        (rails[0][1] + rails[1][1]) / 2.0 + 0.5,
    ];
    let along = unit(-45.0);
    let across = unit(45.0);
    for z in [9.0, 15.0, 21.0, 27.0, 33.0, 39.0, 44.0] {
        let rung = rect_on(mid, along, across, [2.6, 0.4]);
        f.solid(
            p,
            &f.poly(&rung),
            [z - 0.4, z + 0.4],
            0.0,
            Finish::paper().close(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        // The design count is 49 and the figure draws about 41 at two bars, so the tolerance covers it.
        assert_solids_near(Kind::Tank, 49, 17);
    }
}
