//! Parts that recur across the figures: the plinth, the frame each figure is drawn in, and the bolts, knurls, lines and
//! flat faces they are built from.
//!
//! A figure is laid out in design units: its plinth's low corner is the origin, x runs right, y runs toward the viewer
//! and z runs up. [`Frame`] moves those positions onto the kit's centred plan, so a figure reads the way its parts list
//! reads.

use crate::mesh::{Base, Finish, LIFT, Part, Stroke};
use crate::model::{Ink, Surface};
use crate::models::Footprint;
use crate::plan::{Plan, Point};

/// The plinth's height. The groove and the drone's shadow lie on its top.
pub(crate) const PLINTH: f32 = 3.0;
/// Radius of a bolt head, in plan units.
const BOLT: f32 = 0.5;
/// How far a knurl ridge stops short of the rings above and below, so the ridges do not touch them.
const KNURL_INSET: f32 = 0.4;

/// A figure's frame: its footprint, for moving design positions onto the centred plan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Frame {
    pub(crate) w: f32,
    pub(crate) d: f32,
}

impl Frame {
    /// The frame of a figure with `footprint`.
    pub(crate) const fn of(footprint: Footprint) -> Self {
        Self {
            w: footprint.w,
            d: footprint.d,
        }
    }

    /// The centred plan point of the design point `(x, y)`.
    pub(crate) fn at(self, x: f32, y: f32) -> Point {
        [x - self.w / 2.0, y - self.d / 2.0]
    }

    /// The centred point of the design point `(x, y)` at height `z`.
    pub(crate) fn at3(self, x: f32, y: f32, z: f32) -> [f32; 3] {
        up(self.at(x, y), z)
    }

    /// The plan rectangle `[x0, y0, x1, y1]`, with its corners rounded by `r`.
    fn plan(self, [x0, y0, x1, y1]: [f32; 4], r: f32) -> Plan {
        let [x, y] = self.at(x0, y0);
        Plan {
            x,
            y,
            w: x1 - x0,
            d: y1 - y0,
            r,
        }
    }

    /// The base of the plan rectangle `[x0, y0, x1, y1]`, with rounded corners when `r` is positive.
    pub(crate) fn rect(self, at: [f32; 4], r: f32) -> Base {
        let steps = if r > 0.0 { 2 } else { 1 };
        Base::Plan(self.plan(at, r), steps)
    }

    /// The base of a circle at the design point `centre`.
    pub(crate) fn circle(self, centre: Point, radius: f32, steps: usize) -> Base {
        Base::Circle {
            center: self.at(centre[0], centre[1]),
            radius,
            steps,
        }
    }

    /// The base of the polygon with the design points `points`, whichever way they wind.
    pub(crate) fn poly(self, points: &[Point]) -> Base {
        let mapped: Vec<Point> = points.iter().map(|p| self.at(p[0], p[1])).collect();
        Base::polygon(&mapped)
    }

    /// A block over the design rectangle `at`, from `z[0]` to `z[1]`, with rounded corners `r` and edges bevelled `bevel`.
    pub(crate) fn block(
        self,
        part: &mut Part,
        at: [f32; 4],
        z: [f32; 2],
        r: f32,
        bevel: f32,
        finish: Finish,
    ) {
        part.slab(&self.rect(at, r), z[0], z[1], bevel, finish);
    }

    /// A solid over `base` from `z[0]` to `z[1]`, with its edges bevelled by `bevel`.
    pub(crate) fn solid(
        self,
        part: &mut Part,
        base: &Base,
        z: [f32; 2],
        bevel: f32,
        finish: Finish,
    ) {
        part.slab(base, z[0], z[1], bevel, finish);
    }

    /// A profile in the y-z plane, swept from `x[0]` to `x[1]`.
    pub(crate) fn prism_x(self, part: &mut Part, x: [f32; 2], profile: &[Point], finish: Finish) {
        let mut local = Part::new();
        local.slab(&Base::polygon(profile), x[0], x[1], 0.0, finish);
        part.place(
            local,
            self.at3(0.0, 0.0, 0.0),
            [[0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]],
        );
    }

    /// A rectangular bar between two design points, with a square-ended section `size`.
    pub(crate) fn bar(
        self,
        part: &mut Part,
        a: [f32; 3],
        b: [f32; 3],
        size: Point,
        finish: Finish,
    ) {
        let delta = std::array::from_fn::<_, 3, _>(|i| b[i] - a[i]);
        let length = delta.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(length > 0.0, "a bar needs distinct ends");
        let axis = delta.map(|v| v / length);
        let horizontal = axis[0].hypot(axis[1]);
        let across = if horizontal > 1e-6 {
            [-axis[1] / horizontal, axis[0] / horizontal, 0.0]
        } else {
            [1.0, 0.0, 0.0]
        };
        let up = cross(axis, across);
        let mut local = Part::new();
        local.slab(
            &Base::polygon(&rectangle([
                -size[0] / 2.0,
                -size[1] / 2.0,
                size[0] / 2.0,
                size[1] / 2.0,
            ])),
            0.0,
            length,
            0.0,
            finish,
        );
        part.place(local, self.at3(a[0], a[1], a[2]), [across, up, axis]);
    }

    /// A polyline on the surface at height `z`, through the design points `points`.
    pub(crate) fn line(
        self,
        part: &mut Part,
        points: &[Point],
        z: f32,
        closed: bool,
        stroke: Stroke,
    ) {
        let lifted: Vec<[f32; 3]> = points
            .iter()
            .map(|p| self.at3(p[0], p[1], z + LIFT))
            .collect();
        part.line(&lifted, closed, stroke);
    }

    /// A polyline through design points that each carry their own height.
    pub(crate) fn line3(self, part: &mut Part, points: &[[f32; 3]], stroke: Stroke) {
        let mapped: Vec<[f32; 3]> = points.iter().map(|p| self.at3(p[0], p[1], p[2])).collect();
        part.line(&mapped, false, stroke);
    }

    /// A disc of radius `radius` on the surface at the design point `p` and height `z`, facing `normal`.
    pub(crate) fn dot(
        self,
        part: &mut Part,
        p: Point,
        z: f32,
        normal: [f32; 3],
        radius: f32,
        stroke: Stroke,
    ) {
        let at = self.at3(p[0], p[1], z);
        let proud = std::array::from_fn(|axis| at[axis] + normal[axis] * LIFT);
        part.dot(proud, normal, radius, stroke);
    }

    /// A bolt head at the design point `p` on the surface at height `z`, facing `normal`.
    pub(crate) fn bolt(self, part: &mut Part, p: Point, z: f32, normal: [f32; 3]) {
        bolt(part, self.at3(p[0], p[1], z), normal);
    }

    /// A flat quad in design coordinates, facing the side `facing` points to.
    pub(crate) fn quad(
        self,
        part: &mut Part,
        corners: [[f32; 3]; 4],
        facing: [f32; 3],
        finish: Finish,
    ) {
        let mapped = corners.map(|[x, y, z]| self.at3(x, y, z));
        part.quad(mapped, facing, finish);
    }

    /// A closed ring round the design point `centre`, of radius `radius`, on the surface at height `z`.
    pub(crate) fn circle_line(
        self,
        part: &mut Part,
        centre: Point,
        radius: f32,
        z: f32,
        steps: usize,
        stroke: Stroke,
    ) {
        topline(part, &self.circle(centre, radius, steps), z, stroke);
    }

    /// Ridges up a cylinder centred on the design point `centre`, of radius `radius`, between `z[0]` and `z[1]`.
    pub(crate) fn knurl(
        self,
        part: &mut Part,
        centre: Point,
        radius: f32,
        z: [f32; 2],
        count: usize,
        stroke: Stroke,
    ) {
        knurl(
            part,
            self.at(centre[0], centre[1]),
            radius,
            z,
            count,
            stroke,
        );
    }
}

/// A surface in design coordinates, with two perpendicular unit axes and their outward normal.
#[derive(Clone, Copy)]
pub(crate) struct Plane {
    origin: [f32; 3],
    axes: [[f32; 3]; 3],
}

impl Plane {
    pub(crate) fn new(origin: [f32; 3], along: [f32; 3], across: [f32; 3]) -> Self {
        Self {
            origin,
            axes: [along, across, cross(along, across)],
        }
    }

    pub(crate) fn top(z: f32) -> Self {
        Self::new([0.0, 0.0, z], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0])
    }

    /// The same plane lifted along its normal.
    pub(crate) fn lifted(self, by: f32) -> Self {
        Self {
            origin: std::array::from_fn(|i| self.origin[i] + self.axes[2][i] * by),
            ..self
        }
    }

    /// A local surface position, lifted out along the normal by `height`.
    fn at(self, point: Point, height: f32) -> [f32; 3] {
        std::array::from_fn(|i| {
            self.origin[i]
                + self.axes[0][i] * point[0]
                + self.axes[1][i] * point[1]
                + self.axes[2][i] * height
        })
    }

    /// A closed plate, including concave shapes, at the given heights above the plane.
    pub(crate) fn slab(
        self,
        part: &mut Part,
        frame: Frame,
        points: &[Point],
        z: [f32; 2],
        finish: Finish,
    ) {
        let mut local = Part::new();
        local.plate(points, z, finish);
        part.place(
            local,
            frame.at3(self.origin[0], self.origin[1], self.origin[2]),
            self.axes,
        );
    }

    /// A flat painted face. Its very thin closed backing keeps the figure's meshes watertight.
    pub(crate) fn face(self, part: &mut Part, frame: Frame, points: &[Point], finish: Finish) {
        self.slab(part, frame, points, [LIFT / 2.0, LIFT], finish);
    }

    /// A recess floor and its faint rim, drawn flat rather than cutting a sub-unit pocket.
    pub(crate) fn recess(self, part: &mut Part, frame: Frame, points: &[Point]) {
        self.face(part, frame, points, flat(Ink::Lo).near());
        self.line(part, frame, points, true, Stroke::new(Ink::Faint).near());
    }

    pub(crate) fn line(
        self,
        part: &mut Part,
        frame: Frame,
        points: &[Point],
        closed: bool,
        stroke: Stroke,
    ) {
        let mapped: Vec<_> = points
            .iter()
            .map(|&p| {
                let at = self.at(p, LIFT);
                frame.at3(at[0], at[1], at[2])
            })
            .collect();
        part.line(&mapped, closed, stroke);
    }

    /// A bolt on this plane: one counted head and a ring, both at close detail.
    pub(crate) fn bolt(
        self,
        part: &mut Part,
        frame: Frame,
        at: Point,
        radii: Point,
        inks: [Ink; 2],
    ) {
        part.solids += 1;
        let centre = self.at(at, LIFT);
        part.dot(
            frame.at3(centre[0], centre[1], centre[2]),
            self.axes[2],
            radii[0],
            Stroke::new(inks[0]).close(),
        );
        let ring = crate::plan::circle_ring(at[0], at[1], radii[1], 12);
        self.line(part, frame, &ring, true, Stroke::new(inks[1]).close());
    }
}

/// Corners of the rectangle `[x0, y0, x1, y1]` in a surface's local coordinates.
pub(crate) fn rectangle([x0, y0, x1, y1]: [f32; 4]) -> [Point; 4] {
    [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// The bevelled base every figure stands on. Its groove is an overlay, so the plinth itself is plain.
pub(crate) fn plinth(part: &mut Part, frame: Frame) {
    frame.block(
        part,
        [0.0, 0.0, frame.w, frame.d],
        [0.0, PLINTH],
        2.0,
        0.8,
        Finish::paper(),
    );
}

/// A bolt head: a small disc set just proud of the face whose outward normal is `normal`.
pub(crate) fn bolt(part: &mut Part, at: [f32; 3], normal: [f32; 3]) {
    part.solids += 1;
    let proud = std::array::from_fn(|axis| at[axis] + normal[axis] * LIFT);
    part.dot(proud, normal, BOLT, Stroke::new(Ink::Lo).close());
}

/// The point `p` lifted to height `z`.
pub(crate) fn up(p: Point, z: f32) -> [f32; 3] {
    [p[0], p[1], z]
}

/// The point `radius` from `centre` at `degrees`, counter-clockwise from +x.
pub(crate) fn polar(centre: Point, radius: f32, degrees: f32) -> Point {
    let (sin, cos) = degrees.to_radians().sin_cos();
    [centre[0] + radius * cos, centre[1] + radius * sin]
}

/// The corners of a rectangle centred on `centre`, `half[0]` out along the unit vector `along` and `half[1]` out
/// along `across`.
pub(crate) fn rect_on(centre: Point, along: Point, across: Point, half: [f32; 2]) -> [Point; 4] {
    let corner = |s: f32, t: f32| -> Point {
        [
            centre[0] + along[0] * s * half[0] + across[0] * t * half[1],
            centre[1] + along[1] * s * half[0] + across[1] * t * half[1],
        ]
    };
    [
        corner(-1.0, -1.0),
        corner(1.0, -1.0),
        corner(1.0, 1.0),
        corner(-1.0, 1.0),
    ]
}

/// The unit vector at `degrees` counter-clockwise from +x.
pub(crate) fn unit(degrees: f32) -> Point {
    polar([0.0, 0.0], 1.0, degrees)
}

/// Ridges up a cylinder's side, all the way round, between `z[0]` and `z[1]`. `center` is in the centred plan.
fn knurl(part: &mut Part, center: Point, radius: f32, z: [f32; 2], count: usize, stroke: Stroke) {
    for index in 0..count {
        let angle = polar(center, radius + LIFT, index as f32 / count as f32 * 360.0);
        part.line(
            &[up(angle, z[0] + KNURL_INSET), up(angle, z[1] - KNURL_INSET)],
            false,
            stroke,
        );
    }
}

/// A flat face in one ink: a floor, a mouth, or a plate. It takes no shading from its normal.
pub(crate) fn flat(ink: Ink) -> Finish {
    Finish::new(Surface::Ink(ink))
}

/// The model's one lit part: flat black, the ink that names the kind from above.
pub(crate) fn lit() -> Finish {
    flat(Ink::Hi)
}

/// A window of glass, drawn flat and light.
pub(crate) fn glass() -> Finish {
    Finish::new(Surface::Glass)
}

/// A closed outline on the plane at `z`, following `base`.
pub(crate) fn topline(part: &mut Part, base: &Base, z: f32, stroke: Stroke) {
    let points: Vec<[f32; 3]> = base
        .ring(0.0)
        .into_iter()
        .map(|[x, y]| [x, y, z + LIFT])
        .collect();
    part.line(&points, true, stroke);
}

/// A closed outline running round the side of `base` at height `z`, just outside its wall.
pub(crate) fn sideline(part: &mut Part, base: &Base, z: f32, stroke: Stroke) {
    let points: Vec<[f32; 3]> = base
        .ring(LIFT)
        .into_iter()
        .map(|[x, y]| [x, y, z])
        .collect();
    part.line(&points, true, stroke);
}
