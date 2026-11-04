//! The splitter: an octagonal manifold with one inlet header at the back, a T of channels across its top and three
//! outlet flanges at the edges. The only octagon, so middleware, proxies and gateways read as one-in, three-out.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, flat, lit, polar, rect_on, unit};

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 80.0,
    d: 80.0,
    top: 30.0,
};

/// The body's eight-sided outline, in design units, counter-clockwise.
const OCTAGON: [[f32; 2]; 8] = [
    [16.0, 8.0],
    [64.0, 8.0],
    [72.0, 16.0],
    [72.0, 64.0],
    [64.0, 72.0],
    [16.0, 72.0],
    [8.0, 64.0],
    [8.0, 16.0],
];

/// The centre of the boss where the channels meet.
const CENTRE: [f32; 2] = [40.0, 40.0];

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    body(&mut p, f);
    header(&mut p, f);
    troughs(&mut p, f);
    flanges(&mut p, f);
    boss(&mut p, f);
    p.finish()
}

/// The octagonal body with its seam and the four lugs on its chamfers, and four screws at the corners.
fn body(p: &mut Part, f: Frame) {
    let octagon = f.poly(&OCTAGON);
    f.solid(p, &octagon, [3.0, 18.0], 0.8, Finish::paper());
    parts::sideline(p, &octagon, 10.0, Stroke::new(Ink::Faint).near());
    // Each lug sits on a chamfer, its normal pointing out of the corner.
    let lugs = [
        ([12.0, 12.0], -45.0, 225.0),
        ([68.0, 12.0], 45.0, 315.0),
        ([68.0, 68.0], 135.0, 45.0),
        ([12.0, 68.0], 225.0, 135.0),
    ];
    for (centre, along, across) in lugs {
        let lug = rect_on(centre, unit(along), unit(across), [2.0, 1.0]);
        f.solid(p, &f.poly(&lug), [13.0, 15.0], 0.0, Finish::paper().near());
    }
    for at in [[12.0, 12.0], [68.0, 12.0], [12.0, 68.0], [68.0, 68.0]] {
        f.bolt(p, at, 18.0, [0.0, 0.0, 1.0]);
    }
}

/// The inlet header at the back, its bolts and its mouth on the back face.
fn header(p: &mut Part, f: Frame) {
    f.block(
        p,
        [28.0, 8.0, 52.0, 20.0],
        [18.0, 24.0],
        0.0,
        0.6,
        Finish::paper(),
    );
    for at in [[30.0, 10.0], [50.0, 10.0], [30.0, 18.0], [50.0, 18.0]] {
        f.bolt(p, at, 24.0, [0.0, 0.0, 1.0]);
    }
    // The mouth is a dark plate on the back face, ten wide and four tall.
    f.block(
        p,
        [35.0, 7.6, 45.0, 8.0],
        [19.0, 23.0],
        0.0,
        0.0,
        flat(Ink::Lo),
    );
}

/// The T of troughs on the body top: the feed trough from the header and the cross trough between the outlets. It is
/// one plate, so the two do not overlap.
fn troughs(p: &mut Part, f: Frame) {
    let t = [
        [37.0, 20.0],
        [43.0, 20.0],
        [43.0, 37.0],
        [71.0, 37.0],
        [71.0, 43.0],
        [43.0, 43.0],
        [43.0, 72.0],
        [37.0, 72.0],
        [37.0, 43.0],
        [9.0, 43.0],
        [9.0, 37.0],
        [37.0, 37.0],
    ];
    f.solid(p, &f.poly(&t), [18.0, 18.2], 0.0, flat(Ink::Lo));
}

/// Three outlet flanges, on the left, right and front ends of the troughs, each with four bolts and a dark mouth.
fn flanges(p: &mut Part, f: Frame) {
    f.block(
        p,
        [4.0, 37.0, 12.0, 43.0],
        [12.0, 17.6],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.block(
        p,
        [68.0, 37.0, 76.0, 43.0],
        [12.0, 17.6],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    f.block(
        p,
        [36.0, 69.0, 44.0, 75.0],
        [12.0, 17.6],
        0.0,
        0.0,
        Finish::paper().near(),
    );
    for y in [38.2, 41.8] {
        for z in [13.2, 16.8] {
            f.bolt(p, [4.0, y], z, [-1.0, 0.0, 0.0]);
            f.bolt(p, [76.0, y], z, [1.0, 0.0, 0.0]);
        }
    }
    for x in [37.2, 42.8] {
        for z in [13.2, 16.8] {
            f.bolt(p, [x, 75.0], z, [0.0, 1.0, 0.0]);
        }
    }
    f.block(
        p,
        [3.6, 38.0, 4.0, 42.0],
        [13.5, 16.5],
        0.0,
        0.0,
        flat(Ink::Lo),
    );
    f.block(
        p,
        [76.0, 38.0, 76.4, 42.0],
        [13.5, 16.5],
        0.0,
        0.0,
        flat(Ink::Lo),
    );
    f.block(
        p,
        [38.0, 75.0, 42.0, 75.4],
        [13.5, 16.5],
        0.0,
        0.0,
        flat(Ink::Lo),
    );
}

/// The boss where the channels meet, its cap with six bolts, and the lit crown nut on top.
fn boss(p: &mut Part, f: Frame) {
    f.solid(
        p,
        &f.circle(CENTRE, 8.0, 24),
        [18.2, 26.0],
        0.0,
        Finish::paper().near(),
    );
    f.solid(
        p,
        &f.circle(CENTRE, 6.0, 24),
        [26.0, 28.0],
        0.4,
        Finish::paper().near(),
    );
    for k in 0..6 {
        f.bolt(
            p,
            polar(CENTRE, 4.8, 60.0 * k as f32),
            28.0,
            [0.0, 0.0, 1.0],
        );
    }
    f.solid(p, &f.circle(CENTRE, 2.0, 16), [28.0, 30.0], 0.0, lit());
    f.knurl(
        p,
        CENTRE,
        2.0,
        [28.0, 30.0],
        12,
        Stroke::new(Ink::Lo).close(),
    );
}
