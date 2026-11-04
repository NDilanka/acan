//! The builder every model uses. Solids are swept along a plan outline through a profile of stations, so one
//! primitive covers bevelled slabs, cylinders, boxes, prisms and stepped lathe bodies. Tubes run along any axis.
//!
//! Every triangle winds counter-clockwise seen from outside, and every vertex carries the normal of the surface
//! it lies on. A joint between two plan edges is smooth when the surface turns by less than 60 degrees there, so
//! a rounded corner shades as one curve. Joints in a profile are always creases, so bevels and steps stay crisp.

use crate::model::{Aabb, Detail, Dot3, Ink, Line3, Mesh, Model, Surface};
use crate::plan::{Plan, Point, circle_ring};

/// How far a painted mark stands off the surface under it, so the two never z-fight.
pub(crate) const LIFT: f32 = 0.02;

/// The kit's line weight at zoom 1, in plan units.
const WEIGHT: f32 = 0.6;

/// Joints whose surface turns by less than 60 degrees have smooth normals.
const SMOOTH_COS: f32 = 0.5;

/// The plan outline a solid is swept from. Growing it moves every side out by the same distance.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Base {
    /// A rounded plan rectangle, with `steps` segments per corner.
    Plan(Plan, usize),
    /// A circle with `steps` sides.
    Circle {
        center: Point,
        radius: f32,
        steps: usize,
    },
    /// A convex polygon, counter-clockwise in plan. Growing it offsets each side, with mitred corners.
    Polygon(Vec<Point>),
}

impl Base {
    /// A polygon, reversed when needed so that it winds counter-clockwise in plan.
    pub(crate) fn polygon(points: &[Point]) -> Self {
        let mut ring = points.to_vec();
        if signed_area(&ring) < 0.0 {
            ring.reverse();
        }
        Self::Polygon(ring)
    }

    /// The outline grown by `grow`. Its vertex count does not depend on `grow`.
    pub(crate) fn ring(&self, grow: f32) -> Vec<Point> {
        match self {
            Self::Plan(plan, steps) => plan.inset(-grow).ring(*steps),
            Self::Circle {
                center,
                radius,
                steps,
            } => circle_ring(center[0], center[1], radius + grow, *steps),
            Self::Polygon(points) => offset(points, grow),
        }
    }
}

/// A point on the profile of a solid: how far its wall is grown from the base, and its height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Station {
    grow: f32,
    z: f32,
}

impl Station {
    pub(crate) const fn at(grow: f32, z: f32) -> Self {
        Self { grow, z }
    }
}

/// Which level a solid goes to, and how its surface is shaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Finish {
    surface: Surface,
    level: Detail,
}

impl Finish {
    pub(crate) const fn new(surface: Surface) -> Self {
        Self {
            surface,
            level: Detail::Always,
        }
    }

    pub(crate) const fn paper() -> Self {
        Self::new(Surface::Paper)
    }

    /// The same finish, sent to the `Near` mesh.
    pub(crate) const fn near(self) -> Self {
        Self {
            level: Detail::Near,
            ..self
        }
    }

    /// The same finish, sent to the `Close` mesh.
    pub(crate) const fn close(self) -> Self {
        Self {
            level: Detail::Close,
            ..self
        }
    }
}

/// How a painted line is drawn: its ink, its weight and the level it needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Stroke {
    tone: Ink,
    width: f32,
    detail: Detail,
}

impl Stroke {
    pub(crate) const fn new(tone: Ink) -> Self {
        Self {
            tone,
            width: WEIGHT,
            detail: Detail::Always,
        }
    }

    pub(crate) const fn near(self) -> Self {
        Self {
            detail: Detail::Near,
            ..self
        }
    }

    pub(crate) const fn close(self) -> Self {
        Self {
            detail: Detail::Close,
            ..self
        }
    }

    pub(crate) fn tone(self) -> Ink {
        self.tone
    }

    pub(crate) fn detail(self) -> Detail {
        self.detail
    }
}

/// The geometry of one part of a model, before it is combined into the model.
#[derive(Debug, Default)]
pub(crate) struct Part {
    meshes: [Mesh; 3],
    lines: Vec<Line3>,
    dots: Vec<Dot3>,
    pub(crate) solids: u32,
}

impl Part {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn finish(self) -> Model {
        let bounds = bounds_of(&self);
        Model {
            meshes: self.meshes,
            lines: self.lines,
            dots: self.dots,
            bounds,
            solids: self.solids,
        }
    }

    /// Adds a part in a right-handed orthonormal frame. Positions move with the origin; normals only rotate.
    pub(crate) fn place(&mut self, part: Part, origin: [f32; 3], axes: [[f32; 3]; 3]) {
        let rotate = |v: [f32; 3]| {
            std::array::from_fn(|i| axes[0][i] * v[0] + axes[1][i] * v[1] + axes[2][i] * v[2])
        };
        let position = |v: [f32; 3]| {
            let rotated: [f32; 3] = rotate(v);
            std::array::from_fn(|i| origin[i] + rotated[i])
        };
        self.solids += part.solids;
        for (target, source) in self.meshes.iter_mut().zip(part.meshes) {
            let start =
                u32::try_from(target.positions.len()).expect("mesh has fewer than 2^32 vertices");
            target
                .positions
                .extend(source.positions.into_iter().map(position));
            target
                .normals
                .extend(source.normals.into_iter().map(rotate));
            target.surfaces.extend(source.surfaces);
            target
                .indices
                .extend(source.indices.into_iter().map(|i| start + i));
        }
        self.lines.extend(part.lines.into_iter().map(|mut line| {
            line.points = line.points.into_iter().map(position).collect();
            line
        }));
        self.dots.extend(part.dots.into_iter().map(|mut dot| {
            dot.center = position(dot.center);
            dot.normal = rotate(dot.normal);
            dot
        }));
    }

    /// A closed thin plate over a simple polygon, including concave outlines such as a mould cavity.
    pub(crate) fn plate(&mut self, points: &[Point], z: [f32; 2], finish: Finish) {
        let base = Base::polygon(points);
        let ring = base.ring(0.0);
        self.slab(&base, z[0], z[1], 0.0, finish);
        let mesh = &mut self.meshes[finish.level as usize];
        // Replace the sweep's convex fan caps with ears; the side walls and the solid tally stay unchanged.
        mesh.indices.truncate(mesh.indices.len() - ring.len() * 6);
        let triangles = polygon_triangles(&ring);
        for (height, up) in [(z[0], false), (z[1], true)] {
            let normal = [0.0, 0.0, if up { 1.0 } else { -1.0 }];
            let vertices: Vec<u32> = ring
                .iter()
                .map(|p| mesh.vertex([p[0], p[1], height], normal, finish.surface))
                .collect();
            for &[a, b, c] in &triangles {
                if up {
                    mesh.triangle(vertices[a], vertices[b], vertices[c]);
                } else {
                    mesh.triangle(vertices[c], vertices[b], vertices[a]);
                }
            }
        }
    }

    /// Sweeps `base` through `stations`, bottom first, and closes both ends with flat caps.
    pub(crate) fn sweep(&mut self, base: &Base, stations: &[Station], finish: Finish) {
        self.solids += 1;
        assert!(
            stations.len() >= 2,
            "a sweep needs two stations, got {}",
            stations.len()
        );
        let rings: Vec<Vec<Point>> = stations.iter().map(|s| base.ring(s.grow)).collect();
        let count = rings[0].len();
        let plan = PlanNormals::new(&base.ring(0.0));
        let mesh = &mut self.meshes[finish.level as usize];

        for (segment, pair) in stations.windows(2).enumerate() {
            let (low, high) = (pair[0], pair[1]);
            let profile = profile_normal(low, high);
            for edge in 0..count {
                let next = (edge + 1) % count;
                let (start, end) = plan.ends(edge);
                let [a_low, b_low] = [rings[segment][edge], rings[segment][next]];
                let [a_high, b_high] = [rings[segment + 1][edge], rings[segment + 1][next]];
                let va_low = mesh.vertex(
                    [a_low[0], a_low[1], low.z],
                    blend(start, profile),
                    finish.surface,
                );
                let vb_low = mesh.vertex(
                    [b_low[0], b_low[1], low.z],
                    blend(end, profile),
                    finish.surface,
                );
                let vb_high = mesh.vertex(
                    [b_high[0], b_high[1], high.z],
                    blend(end, profile),
                    finish.surface,
                );
                let va_high = mesh.vertex(
                    [a_high[0], a_high[1], high.z],
                    blend(start, profile),
                    finish.surface,
                );
                mesh.triangle(va_low, vb_low, vb_high);
                mesh.triangle(va_low, vb_high, va_high);
            }
        }

        let first = stations[0].z;
        let last = stations[stations.len() - 1].z;
        cap(
            mesh,
            &rings[0],
            first,
            [0.0, 0.0, -1.0],
            finish.surface,
            false,
        );
        cap(
            mesh,
            &rings[rings.len() - 1],
            last,
            [0.0, 0.0, 1.0],
            finish.surface,
            true,
        );
    }

    /// A solid from `z0` to `z1` whose top and bottom edges are bevelled inward by `bevel`.
    pub(crate) fn slab(&mut self, base: &Base, z0: f32, z1: f32, bevel: f32, finish: Finish) {
        assert!(z1 > z0, "a slab needs z1 > z0, got {z0} to {z1}");
        let stations = if bevel > 0.0 {
            assert!(
                z1 - z0 > 2.0 * bevel,
                "a bevel of {bevel} needs a slab taller than twice that"
            );
            vec![
                Station::at(-bevel, z0),
                Station::at(0.0, z0 + bevel),
                Station::at(0.0, z1 - bevel),
                Station::at(-bevel, z1),
            ]
        } else {
            vec![Station::at(0.0, z0), Station::at(0.0, z1)]
        };
        self.sweep(base, &stations, finish);
    }

    /// A flat quad, flat-shaded, with its corners in order around its edge. Its normal points to the side `facing`
    /// points to, so the corners may wind either way.
    pub(crate) fn quad(&mut self, corners: [[f32; 3]; 4], facing: [f32; 3], finish: Finish) {
        self.solids += 1;
        let mut normal = unit3(cross3(
            sub3(corners[1], corners[0]),
            sub3(corners[2], corners[0]),
        ));
        let order = if dot3(normal, facing) < 0.0 {
            normal = scale3(normal, -1.0);
            [0, 3, 2, 1]
        } else {
            [0, 1, 2, 3]
        };
        let mesh = &mut self.meshes[finish.level as usize];
        let v: Vec<u32> = order
            .iter()
            .map(|&i| mesh.vertex(corners[i], normal, finish.surface))
            .collect();
        mesh.triangle(v[0], v[1], v[2]);
        mesh.triangle(v[0], v[2], v[3]);
    }

    /// A polyline. It is painted on top of whatever it lies on, so the caller lifts it by [`LIFT`].
    pub(crate) fn line(&mut self, points: &[[f32; 3]], closed: bool, stroke: Stroke) {
        self.lines.push(Line3 {
            points: points.to_vec(),
            closed,
            tone: stroke.tone(),
            width: stroke.width,
            detail: stroke.detail(),
        });
    }

    /// A disc lying on a surface whose normal is `normal`.
    pub(crate) fn dot(&mut self, center: [f32; 3], normal: [f32; 3], radius: f32, stroke: Stroke) {
        self.dots.push(Dot3 {
            center,
            normal,
            radius,
            tone: stroke.tone(),
            detail: stroke.detail(),
        });
    }
}

impl Mesh {
    fn vertex(&mut self, position: [f32; 3], normal: [f32; 3], surface: Surface) -> u32 {
        let index = u32::try_from(self.positions.len()).expect("mesh has fewer than 2^32 vertices");
        self.positions.push(position);
        self.normals.push(normal);
        self.surfaces.push(surface);
        index
    }

    fn triangle(&mut self, a: u32, b: u32, c: u32) {
        self.indices.extend([a, b, c]);
    }
}

/// Plan normals of the base outline: one per edge, and whether each corner is smooth.
struct PlanNormals {
    edges: Vec<Point>,
    smooth: Vec<bool>,
}

impl PlanNormals {
    fn new(ring: &[Point]) -> Self {
        let count = ring.len();
        let edges: Vec<Point> = (0..count)
            .map(|i| edge_normal(ring[i], ring[(i + 1) % count]))
            .collect();
        let smooth = (0..count)
            .map(|i| dot2(edges[(i + count - 1) % count], edges[i]) > SMOOTH_COS)
            .collect();
        Self { edges, smooth }
    }

    /// The plan normal at the start and at the end of edge `i`.
    fn ends(&self, i: usize) -> (Point, Point) {
        let count = self.edges.len();
        let here = self.edges[i];
        let next = (i + 1) % count;
        let start = if self.smooth[i] {
            unit2(add2(self.edges[(i + count - 1) % count], here))
        } else {
            here
        };
        let end = if self.smooth[next] {
            unit2(add2(here, self.edges[next]))
        } else {
            here
        };
        (start, end)
    }
}

/// The outward (radial, vertical) normal of the profile segment from `a` to `b`.
fn profile_normal(a: Station, b: Station) -> [f32; 2] {
    let (dg, dz) = (b.grow - a.grow, b.z - a.z);
    let length = dg.hypot(dz);
    assert!(length > 0.0, "stations must not repeat");
    [dz / length, -dg / length]
}

/// The unit normal at a vertex: the plan normal tilted by the profile normal.
fn blend(plan: Point, profile: [f32; 2]) -> [f32; 3] {
    unit3([plan[0] * profile[0], plan[1] * profile[0], profile[1]])
}

/// A flat cap over `ring` at height `z`. `up` says whether the cap faces +z (the ring winds counter-clockwise from above).
fn cap(mesh: &mut Mesh, ring: &[Point], z: f32, normal: [f32; 3], surface: Surface, up: bool) {
    let count = ring.len();
    let centre = ring
        .iter()
        .fold([0.0_f32, 0.0], |acc, p| [acc[0] + p[0], acc[1] + p[1]]);
    let centre = [centre[0] / count as f32, centre[1] / count as f32];
    let middle = mesh.vertex([centre[0], centre[1], z], normal, surface);
    let rim: Vec<u32> = ring
        .iter()
        .map(|p| mesh.vertex([p[0], p[1], z], normal, surface))
        .collect();
    for i in 0..count {
        let j = (i + 1) % count;
        if up {
            mesh.triangle(middle, rim[i], rim[j]);
        } else {
            mesh.triangle(middle, rim[j], rim[i]);
        }
    }
}

/// Ear clipping for a simple counter-clockwise polygon. A point on an ear's edge keeps that ear from being cut.
fn polygon_triangles(ring: &[Point]) -> Vec<[usize; 3]> {
    let cross = |a: Point, b: Point, c: Point| {
        (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
    };
    let mut remaining: Vec<usize> = (0..ring.len()).collect();
    let mut triangles = Vec::new();
    while remaining.len() > 3 {
        let count = remaining.len();
        let ear = (0..count)
            .find(|&i| {
                let [a, b, c] = [
                    remaining[(i + count - 1) % count],
                    remaining[i],
                    remaining[(i + 1) % count],
                ];
                cross(ring[a], ring[b], ring[c]) > 1e-6
                    && !remaining.iter().any(|&j| {
                        j != a
                            && j != b
                            && j != c
                            && cross(ring[a], ring[b], ring[j]) >= -1e-6
                            && cross(ring[b], ring[c], ring[j]) >= -1e-6
                            && cross(ring[c], ring[a], ring[j]) >= -1e-6
                    })
            })
            .expect("a simple polygon has an ear");
        triangles.push([
            remaining[(ear + count - 1) % count],
            remaining[ear],
            remaining[(ear + 1) % count],
        ]);
        remaining.remove(ear);
    }
    triangles.push([remaining[0], remaining[1], remaining[2]]);
    triangles
}

/// The outline offset by `grow`, with each corner mitred along the average of its two edge normals.
fn offset(points: &[Point], grow: f32) -> Vec<Point> {
    let count = points.len();
    let normals: Vec<Point> = (0..count)
        .map(|i| edge_normal(points[i], points[(i + 1) % count]))
        .collect();
    (0..count)
        .map(|i| {
            let before = normals[(i + count - 1) % count];
            let after = normals[i];
            let scale = grow / (1.0 + dot2(before, after));
            let sum = add2(before, after);
            [points[i][0] + sum[0] * scale, points[i][1] + sum[1] * scale]
        })
        .collect()
}

/// Outward unit normal of the edge from `a` to `b`, for a ring that winds counter-clockwise in plan.
fn edge_normal(a: Point, b: Point) -> Point {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let length = dx.hypot(dy).max(f32::MIN_POSITIVE);
    [dy / length, -dx / length]
}

/// Shoelace area of a ring, positive when it winds counter-clockwise in plan.
fn signed_area(ring: &[Point]) -> f32 {
    ring.iter()
        .zip(ring.iter().cycle().skip(1))
        .map(|(a, b)| a[0] * b[1] - b[0] * a[1])
        .sum::<f32>()
        / 2.0
}

fn bounds_of(part: &Part) -> Aabb {
    let mut bounds = Aabb {
        min: [f32::INFINITY; 3],
        max: [f32::NEG_INFINITY; 3],
    };
    for mesh in &part.meshes {
        for &position in &mesh.positions {
            include(&mut bounds, position, [0.0; 3]);
        }
    }
    for line in &part.lines {
        for &point in &line.points {
            include(&mut bounds, point, [0.0; 3]);
        }
    }
    for dot in &part.dots {
        // A disc reaches its radius across its own plane and nothing along its normal.
        let reach = dot
            .normal
            .map(|n| dot.radius * (1.0 - n * n).max(0.0).sqrt());
        include(&mut bounds, dot.center, reach);
    }
    if bounds.min[0] > bounds.max[0] {
        return Aabb {
            min: [0.0; 3],
            max: [0.0; 3],
        };
    }
    bounds
}

/// Grows `bounds` to contain `point` extended by `reach` along each axis.
fn include(bounds: &mut Aabb, point: [f32; 3], reach: [f32; 3]) {
    for (axis, &by) in reach.iter().enumerate() {
        bounds.min[axis] = bounds.min[axis].min(point[axis] - by);
        bounds.max[axis] = bounds.max[axis].max(point[axis] + by);
    }
}

fn dot2(a: Point, b: Point) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

fn add2(a: Point, b: Point) -> Point {
    [a[0] + b[0], a[1] + b[1]]
}

fn unit2(v: Point) -> Point {
    let length = v[0].hypot(v[1]);
    if length > 0.0 {
        [v[0] / length, v[1] / length]
    } else {
        v
    }
}

fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale3(v: [f32; 3], by: f32) -> [f32; 3] {
    [v[0] * by, v[1] * by, v[2] * by]
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn unit3(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if length > 0.0 {
        scale3(v, 1.0 / length)
    } else {
        v
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;

    const EPS: f32 = 1e-4;

    fn square(size: f32) -> Base {
        Base::Plan(
            Plan {
                x: -size / 2.0,
                y: -size / 2.0,
                w: size,
                d: size,
                r: 0.0,
            },
            1,
        )
    }

    fn mesh_of(build: impl FnOnce(&mut Part)) -> Mesh {
        let mut part = Part::new();
        build(&mut part);
        part.finish().meshes[0].clone()
    }

    fn position(mesh: &Mesh, index: u32) -> [f32; 3] {
        mesh.positions[index as usize]
    }

    /// Every edge, keyed by quantised endpoints, with how many triangles use it.
    fn edge_counts(mesh: &Mesh) -> HashMap<([i64; 3], [i64; 3]), usize> {
        let key = |p: [f32; 3]| p.map(|c| (c * 1000.0).round() as i64);
        let mut counts = HashMap::new();
        for tri in mesh.indices.chunks(3) {
            for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
                let (ka, kb) = (key(position(mesh, a)), key(position(mesh, b)));
                let edge = if ka < kb { (ka, kb) } else { (kb, ka) };
                *counts.entry(edge).or_insert(0) += 1;
            }
        }
        counts
    }

    /// Checks a union of closed solids: every edge is used by an even number of triangles, so no edge is left open.
    /// Solids that stand on one another share a face, and their shared edges count twice for each solid.
    pub(crate) fn check_closed(mesh: &Mesh) -> Result<(), String> {
        for (edge, count) in edge_counts(mesh) {
            if count % 2 != 0 {
                return Err(format!("edge {edge:?} is used by {count} triangles"));
            }
        }
        Ok(())
    }

    /// Checks that every triangle's winding agrees with the unit normals at its three vertices.
    pub(crate) fn check_outward(mesh: &Mesh) -> Result<(), String> {
        for tri in mesh.indices.chunks(3) {
            let [a, b, c] = [
                position(mesh, tri[0]),
                position(mesh, tri[1]),
                position(mesh, tri[2]),
            ];
            let facing = cross3(sub3(b, a), sub3(c, a));
            for &index in tri {
                let normal = mesh.normals[index as usize];
                let length =
                    (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
                if (length - 1.0).abs() >= EPS {
                    return Err(format!("normal {normal:?} has length {length}"));
                }
                if dot3(facing, normal) <= 0.0 {
                    return Err(format!(
                        "triangle {tri:?} faces {facing:?} but its normal is {normal:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn assert_closed(mesh: &Mesh) {
        check_closed(mesh).unwrap_or_else(|error| panic!("{error}"));
    }

    pub(crate) fn assert_outward(mesh: &Mesh) {
        check_outward(mesh).unwrap_or_else(|error| panic!("{error}"));
    }

    fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }

    #[test]
    fn a_box_is_a_closed_solid_with_flat_outward_faces() {
        let mesh = mesh_of(|part| part.slab(&square(2.0), 0.0, 1.0, 0.0, Finish::paper()));
        assert_eq!(
            mesh.indices.len() / 3,
            16,
            "two walls of quads and two fan caps"
        );
        assert_closed(&mesh);
        assert_outward(&mesh);
    }

    #[test]
    fn a_bevelled_slab_is_closed_and_its_bevels_face_outward() {
        let base = Base::Plan(
            Plan {
                x: -5.0,
                y: -4.0,
                w: 10.0,
                d: 8.0,
                r: 2.0,
            },
            4,
        );
        let mesh = mesh_of(|part| part.slab(&base, 0.0, 6.0, 0.8, Finish::paper()));
        assert_closed(&mesh);
        assert_outward(&mesh);
    }

    #[test]
    fn a_cylinder_has_smooth_sides_and_split_rims() {
        let base = Base::Circle {
            center: [0.0, 0.0],
            radius: 3.0,
            steps: 24,
        };
        let mesh = mesh_of(|part| part.slab(&base, 0.0, 4.0, 0.0, Finish::paper()));
        assert_closed(&mesh);
        assert_outward(&mesh);
        let side_normals = mesh.normals.iter().filter(|n| n[2].abs() < EPS).count();
        assert_eq!(side_normals, 2 * 24 * 2, "each side vertex is horizontal");
        let flat = mesh
            .normals
            .iter()
            .filter(|n| n[2].abs() > 1.0 - EPS)
            .count();
        assert_eq!(flat, 2 * (24 + 1), "each cap has its own rim and centre");
    }

    #[test]
    fn a_stepped_lathe_with_beads_is_closed_and_outward() {
        let base = Base::Circle {
            center: [0.0, 0.0],
            radius: 5.0,
            steps: 16,
        };
        let stations = [
            Station::at(-1.0, 0.0),
            Station::at(-1.0, 1.0),
            Station::at(0.0, 1.0),
            Station::at(0.0, 5.0),
            Station::at(0.8, 5.0),
            Station::at(0.8, 6.0),
            Station::at(0.0, 6.0),
            Station::at(0.0, 8.0),
        ];
        let mesh = mesh_of(|part| part.sweep(&base, &stations, Finish::paper()));
        assert_closed(&mesh);
        assert_outward(&mesh);
    }

    #[test]
    fn a_prism_on_a_polygon_is_closed_and_outward_whichever_way_it_winds() {
        let clockwise = Base::polygon(&[[0.0, 0.0], [0.0, 2.0], [3.0, 2.0], [3.0, 0.0]]);
        let mesh = mesh_of(|part| part.slab(&clockwise, 0.0, 2.0, 0.3, Finish::paper()));
        assert_closed(&mesh);
        assert_outward(&mesh);
    }

    #[test]
    fn a_concave_plate_has_closed_outward_caps_whichever_way_it_winds() {
        let mut ring = vec![
            [0.0, 0.0],
            [6.0, 0.0],
            [6.0, 1.0],
            [4.0, 1.0],
            [4.0, 3.0],
            [6.0, 3.0],
            [6.0, 4.0],
            [0.0, 4.0],
            [0.0, 3.0],
            [2.0, 3.0],
            [2.0, 1.0],
            [0.0, 1.0],
        ];
        for _ in 0..2 {
            let mut p = Part::new();
            p.plate(&ring, [0.01, 0.02], Finish::paper().near());
            let model = p.finish();
            assert_eq!(model.solids, 1);
            assert_eq!(model.meshes[1].indices.len() / 3, 44);
            assert_closed(&model.meshes[1]);
            assert_outward(&model.meshes[1]);
            ring.reverse();
        }
    }

    #[test]
    fn placing_a_part_rotates_its_mesh_lines_and_dots_without_changing_the_tally() {
        let mut source = Part::new();
        source.slab(&square(2.0), 0.0, 3.0, 0.0, Finish::paper().near());
        source.line(
            &[[0.0, 0.0, 1.0], [0.0, 0.0, 2.0]],
            false,
            Stroke::new(Ink::Lo).close(),
        );
        source.dot(
            [0.0, 0.0, 3.0],
            [0.0, 0.0, 1.0],
            0.5,
            Stroke::new(Ink::Lo).close(),
        );
        let mut target = Part::new();
        target.place(
            source,
            [10.0, 20.0, 30.0],
            [[0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]],
        );
        let model = target.finish();
        assert_eq!(model.solids, 1);
        assert_eq!(
            model.lines[0].points,
            [[11.0, 20.0, 30.0], [12.0, 20.0, 30.0]]
        );
        assert_eq!(model.dots[0].center, [13.0, 20.0, 30.0]);
        assert_eq!(model.dots[0].normal, [1.0, 0.0, 0.0]);
        assert_closed(&model.meshes[1]);
        assert_outward(&model.meshes[1]);
        assert!(model.meshes[0].indices.is_empty());
        assert!(model.meshes[2].indices.is_empty());
    }

    #[test]
    fn detail_levels_go_to_their_own_meshes() {
        let model = {
            let mut part = Part::new();
            part.slab(&square(2.0), 0.0, 1.0, 0.0, Finish::paper());
            part.slab(&square(1.0), 1.0, 2.0, 0.0, Finish::paper().near());
            part.finish()
        };
        assert_eq!(model.meshes[0].indices.len(), 48);
        assert_eq!(model.meshes[1].indices.len(), 48);
        assert!(model.meshes[2].indices.is_empty());
    }

    #[test]
    fn bounds_include_lines_and_dots_with_their_radius() {
        let mut part = Part::new();
        part.line(
            &[[1.0, 2.0, 3.0], [-4.0, 0.0, 0.5]],
            false,
            Stroke::new(Ink::Hi),
        );
        part.dot([0.0, 0.0, 1.0], [0.0, 0.0, 1.0], 0.5, Stroke::new(Ink::Hi));
        let model = part.finish();
        assert_eq!(model.bounds.min, [-4.0, -0.5, 0.5]);
        assert_eq!(model.bounds.max, [1.0, 2.0, 3.0]);
    }
}
