//! The switchbox: a wall cabinet on four standoffs, with an open door and three rails of matching toggle switches.
//! Config, settings and environment modules read as a narrow box with a flag-like door at right angles.

use crate::mesh::{Finish, Part, Stroke};
use crate::model::{Ink, Model};
use crate::models::Footprint;
use crate::parts::{self, Frame, Plane, flat, lit, rectangle};
use crate::plan::circle_ring;

pub(crate) const FOOTPRINT: Footprint = Footprint {
    w: 48.0,
    d: 32.0,
    top: 40.0,
};

pub(crate) fn build() -> Model {
    let f = Frame::of(FOOTPRINT);
    let mut p = Part::new();
    parts::plinth(&mut p, f);
    board(&mut p, f);
    for z in [16.0, 32.0] {
        f.solid(
            &mut p,
            &f.circle([21.3, 5.6], 1.0, 16),
            [z, z + 4.0],
            0.0,
            Finish::paper().near(),
        );
    }
    glands(&mut p, f);
    cabinet(&mut p, f);
    door(&mut p, f);
    switches(&mut p, f);
    p.finish()
}

/// A heavy board foot, two board fasteners and four short round standoffs behind the cabinet.
fn board(p: &mut Part, f: Frame) {
    f.block(
        p,
        [5.0, 9.0, 15.0, 23.0],
        [3.0, 5.0],
        0.0,
        0.6,
        Finish::paper(),
    );
    for y in [11.0, 21.0] {
        f.bolt(p, [13.0, y], 5.0, [0.0, 0.0, 1.0]);
    }
    f.block(
        p,
        [5.0, 4.0, 7.0, 28.0],
        [5.0, 40.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    for z in [8.0, 37.0] {
        f.bolt(p, [7.0, 27.0], z, [1.0, 0.0, 0.0]);
    }
    for [y, z] in [[9.0, 16.0], [23.0, 16.0], [9.0, 36.0], [23.0, 36.0]] {
        f.prism_x(p, [7.0, 9.0], &circle_ring(y, z, 1.0, 12), Finish::paper());
    }
}

/// Four knurled cable glands sit below their strip, hidden from the top view.
fn glands(p: &mut Part, f: Frame) {
    f.block(
        p,
        [15.0, 8.0, 23.0, 24.0],
        [10.0, 12.0],
        0.0,
        0.0,
        Finish::paper(),
    );
    for y in [10.0, 14.0, 18.0, 22.0] {
        let centre = [20.0, y];
        f.solid(
            p,
            &f.circle(centre, 1.1, 20),
            [7.5, 10.0],
            0.0,
            Finish::paper().near(),
        );
        f.knurl(
            p,
            centre,
            1.1,
            [7.5, 10.0],
            10,
            Stroke::new(Ink::Lo).close(),
        );
    }
}

/// The dark interior marks the open side; the cabinet top stays clear for its file bars.
fn cabinet(p: &mut Part, f: Frame) {
    f.block(
        p,
        [9.0, 6.0, 21.0, 26.0],
        [12.0, 40.0],
        0.0,
        0.6,
        Finish::paper(),
    );
    f.bolt(p, [15.0, 26.0], 15.0, [0.0, 1.0, 0.0]);
    let side = Plane::new([21.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]);
    side.recess(p, f, &rectangle([8.0, 14.0, 24.0, 38.0]));
    for x in [11.0, 19.0] {
        f.bolt(p, [x, 24.0], 40.0, [0.0, 0.0, 1.0]);
    }
}

/// The open door has a flat inner panel and document pocket; only its handle is lit.
fn door(p: &mut Part, f: Frame) {
    f.block(
        p,
        [21.0, 6.0, 41.0, 7.2],
        [12.5, 39.5],
        0.0,
        0.0,
        Finish::paper(),
    );
    let inner = Plane::new([0.0, 7.2, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);
    inner.face(
        p,
        f,
        &rectangle([14.0, 22.5, 38.0, 39.5]),
        flat(Ink::Faint).near(),
    );
    inner.lifted(crate::mesh::LIFT).face(
        p,
        f,
        &rectangle([17.0, 27.0, 25.0, 35.0]),
        Finish::paper().near(),
    );
    f.block(p, [41.0, 5.6, 43.0, 7.6], [22.0, 30.0], 0.0, 0.0, lit());
}

/// All twelve toggles lean the same way: no static position pretends to show a setting.
fn switches(p: &mut Part, f: Frame) {
    let mut toggles = Vec::new();
    for z in [19.0, 26.0, 33.0] {
        f.block(
            p,
            [21.0, 9.0, 22.0, 23.0],
            [z - 1.0, z + 1.0],
            0.0,
            0.0,
            Finish::paper().near(),
        );
        for y in [11.0, 14.67, 18.33, 22.0] {
            toggles.push([22.0, y, z]);
        }
    }
    toggles.sort_by(|a, b| (a[0] + a[1]).total_cmp(&(b[0] + b[1])));
    for a in toggles {
        f.bar(
            p,
            a,
            [23.8, a[1], a[2] + 1.8],
            [0.7, 0.7],
            Finish::paper().near(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::models::{Kind, assert_solids_near};

    #[test]
    fn solids_match_the_design() {
        assert_solids_near(Kind::Switchbox, 45, 15);
    }
}
