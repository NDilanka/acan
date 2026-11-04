//! The mould: a low two-part casting plate, with its matching I-shaped cavities, locating pins and a pouring cup.
//! Models, schemas and types read as a long flat slab whose back half is propped open on two stays.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, Plane, lit, rectangle};
use crate::plan::{Point, circle_ring};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 72.0,
    d: 56.0,
    top: 16.0,
};

const CAVITY: [Point; 12] = [
    [34.0, 34.0],
    [60.0, 34.0],
    [60.0, 37.0],
    [50.0, 37.0],
    [50.0, 45.0],
    [60.0, 45.0],
    [60.0, 48.0],
    [34.0, 48.0],
    [34.0, 45.0],
    [44.0, 45.0],
    [44.0, 37.0],
    [34.0, 37.0],
];
const PINS: [Point; 4] = [[31.0, 33.0], [63.0, 33.0], [31.0, 49.0], [63.0, 49.0]];

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    for y in [33.0, 46.0] {
        f.block(
            &mut p,
            [10.0, y, 62.0, y + 3.0],
            [3.0, 5.0],
            0.0,
            0.0,
            Finish::paper(),
        );
    }
    pivots(&mut p, f, 5.0, -1.0);
    stay(&mut p, f, 5.5);
    open_half(&mut p, f);
    closed_half(&mut p, f);
    hinges(&mut p, f);
    let mut pins = PINS;
    pins.sort_by(|a, b| (a[0] + a[1]).total_cmp(&(b[0] + b[1])));
    for at in pins {
        f.solid(
            &mut p,
            &f.circle(at, 1.2, 16),
            [11.0, 13.5],
            0.0,
            Finish::paper().near(),
        );
    }
    cup(&mut p, f);
    for x in [14.0, 50.0] {
        f.block(
            &mut p,
            [x, 52.0, x + 8.0, 53.2],
            [7.0, 9.0],
            0.0,
            0.0,
            Finish::paper().near(),
        );
    }
    stay(&mut p, f, 66.5);
    pivots(&mut p, f, 67.0, 1.0);
    p.finish()
}

fn stay(p: &mut Part, f: Frame, x: f32) {
    f.bar(
        p,
        [x, 40.0, 8.0],
        [x, 17.8, 11.2],
        [0.8, 1.0],
        Finish::paper(),
    );
}

fn pivots(p: &mut Part, f: Frame, x: f32, normal: f32) {
    for [y, z] in [[40.0, 8.0], [17.8, 11.2]] {
        f.bolt(p, [x, y], z, [normal, 0.0, 0.0]);
    }
}

/// The lid rises four units over twenty-two, so the mould stays the lowest figure.
fn open_half(p: &mut Part, f: Frame) {
    f.prism_x(
        p,
        [6.0, 66.0],
        &[[30.0, 11.0], [8.0, 15.0], [7.28, 11.06], [29.28, 7.06]],
        Finish::paper(),
    );
    f.block(
        p,
        [28.0, 6.0, 44.0, 7.4],
        [11.4, 13.4],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    let length = 22.0_f32.hypot(4.0);
    let lid = Plane::new(
        [0.0, 30.0, 11.0],
        [0.0, -22.0 / length, 4.0 / length],
        [1.0, 0.0, 0.0],
    );
    let mirror = |points: &[Point]| -> Vec<Point> {
        points
            .iter()
            .map(|p| [(p[1] - 30.0) * length / 22.0, p[0]])
            .collect()
    };
    lid.recess(p, f, &mirror(&CAVITY));
    lid.recess(p, f, &mirror(&rectangle([21.0, 44.5, 34.0, 46.5])));
    for at in PINS {
        lid.recess(p, f, &mirror(&circle_ring(at[0], at[1], 1.4, 12)));
    }
}

/// The closed half has one I-shaped floor, two ejector holes, a runner and vents that end on its front edge.
fn closed_half(p: &mut Part, f: Frame) {
    f.block(
        p,
        [6.0, 30.0, 66.0, 52.0],
        [5.0, 11.0],
        0.0,
        0.6,
        Finish::paper(),
    );
    let top = Plane::top(11.0);
    top.recess(p, f, &CAVITY);
    for at in [[47.0, 36.0], [47.0, 46.0]] {
        top.lifted(crate::mesh::LIFT)
            .recess(p, f, &circle_ring(at[0], at[1], 0.9, 12));
    }
    top.recess(p, f, &rectangle([21.0, 44.5, 34.0, 46.5]));
    for x in [40.0, 54.0] {
        f.line(
            p,
            &[[x, 48.0], [x, 52.0]],
            11.0,
            false,
            Stroke::new(Ink::Faint).close(),
        );
    }
}

/// Four knuckles along x and a pin at each outside end join the two halves.
fn hinges(p: &mut Part, f: Frame) {
    for [a, b] in [[12.0, 17.0], [17.0, 22.0], [50.0, 55.0], [55.0, 60.0]] {
        f.prism_x(
            p,
            [a, b],
            &circle_ring(29.6, 11.0, 1.3, 12),
            Finish::paper().near(),
        );
    }
    f.bolt(p, [12.0, 29.6], 11.0, [-1.0, 0.0, 0.0]);
    f.bolt(p, [60.0, 29.6], 11.0, [1.0, 0.0, 0.0]);
}

/// The pouring cup is three upright cylinders; only the rim is lit, with a dark recessed mouth inside it.
fn cup(p: &mut Part, f: Frame) {
    f.block(
        p,
        [13.0, 42.5, 23.0, 50.5],
        [11.0, 11.6],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    for at in [[14.0, 43.5], [22.0, 43.5], [14.0, 49.5], [22.0, 49.5]] {
        f.bolt(p, at, 11.6, [0.0, 0.0, 1.0]);
    }
    let centre = [18.0, 46.5];
    f.solid(
        p,
        &f.circle(centre, 3.0, 20),
        [11.6, 12.5],
        0.0,
        Finish::paper(),
    );
    f.solid(
        p,
        &f.circle(centre, 4.2, 32),
        [12.5, 15.0],
        0.0,
        Finish::paper(),
    );
    f.solid(p, &f.circle(centre, 4.8, 32), [15.0, 16.0], 0.0, lit());
    Plane::top(16.0).recess(p, f, &circle_ring(centre[0], centre[1], 3.6, 24));
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        assert_solids_near(Kind::Mould, 47, 15);
    }
}
