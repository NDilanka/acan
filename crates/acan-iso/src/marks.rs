//! The marks drawn on the plant: the ring round a waiting agent, the clash pin's diamond, and the
//! stalk that holds the diamond above the machines. They are models like the machines, so the
//! renderer outlines them with the same line art. Each model's origin is on the ground under it.
//! The renderer picks the colour, from its tint slots and palette, so no model names a hue. The
//! agents' drones are in [`crate::drone`].

use std::f32::consts::TAU;

use crate::drone::{BODY_RADIUS, SHADOW_RADIUS};
use crate::model::{Aabb, Ink, Mesh, Model, Surface};
use crate::parts::PLINTH;

/// The height of the plinth top. A drone's shadow lies on it, and the ring lies on it.
pub const PLINTH_TOP: f32 = PLINTH;
/// The amber ring on the plinth top round a waiting agent's drone's shadow, with its inner and outer
/// radius in plan units. The renderer grows it with the drone, so the two keep their proportions.
const RING_INNER: f32 = 1.15 * BODY_RADIUS;
const RING_OUTER: f32 = 1.3 * BODY_RADIUS;
// The ring clears the shadow it surrounds.
const _: () = assert!(RING_INNER > SHADOW_RADIUS);
const RING_SEGMENTS: usize = 32;
/// The ring sits this far above the plinth top, so it never z-fights the plinth.
const RING_LIFT: f32 = 0.05;
/// The clash pin: its half width at the equator and its height, in plan units.
const PIN_RADIUS: f32 = 1.1;
pub const PIN_HEIGHT: f32 = 2.6;
/// The height of the stalk from the plinth top to the pin's foot, in plan units. It is above
/// every machine model, so the diamond clears them all.
pub const STALK_HEIGHT: f32 = 20.0;
/// Half the width of the stalk's square section.
const STALK_HALF: f32 = 0.3;

/// A flat amber ring lying on the plinth top round an agent's drone.
#[must_use]
pub fn waiting_ring() -> Model {
    let mut mesh = Mesh::default();
    for radius in [RING_OUTER, RING_INNER] {
        push_ring(
            &mut mesh,
            radius,
            RING_LIFT,
            |_| [0.0, 0.0, 1.0],
            RING_SEGMENTS,
        );
    }
    band(&mut mesh, 0, 2, RING_SEGMENTS);
    for surface in &mut mesh.surfaces {
        *surface = Surface::Lit;
    }
    model_of(mesh)
}

/// A red diamond standing on its foot: an octahedron, flat red, so its outline is the only line.
#[must_use]
pub fn clash_pin() -> Model {
    let mut mesh = Mesh::default();
    let mid = PIN_HEIGHT * 0.5;
    for (z, radius) in [(0.0, 0.0), (mid, PIN_RADIUS), (PIN_HEIGHT, 0.0)] {
        for j in 0..4 {
            let theta = TAU * j as f32 / 4.0;
            let position = [radius * theta.cos(), radius * theta.sin(), z];
            let normal = [
                position[0] / PIN_RADIUS,
                position[1] / PIN_RADIUS,
                (z - mid) / mid,
            ];
            mesh.positions.push(position);
            mesh.normals.push(normal);
            mesh.surfaces.push(Surface::Ink(Ink::Red));
        }
    }
    band(&mut mesh, 0, 3, 4);
    model_of(mesh)
}

/// The stalk that holds a clash pin: a thin red post from the plinth top to `STALK_HEIGHT`.
#[must_use]
pub fn clash_stalk() -> Model {
    let mut mesh = Mesh::default();
    cuboid(
        &mut mesh,
        [-STALK_HALF, -STALK_HALF, 0.0],
        [STALK_HALF, STALK_HALF, STALK_HEIGHT],
        Surface::Ink(Ink::Red),
    );
    model_of(mesh)
}

/// An axis-aligned box from `lo` to `hi`, with flat faces.
fn cuboid(mesh: &mut Mesh, lo: [f32; 3], hi: [f32; 3], surface: Surface) {
    for axis in 0..3 {
        let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
        for side in [lo[axis], hi[axis]] {
            let outward = if side == hi[axis] { 1.0 } else { -1.0 };
            let mut normal = [0.0; 3];
            normal[axis] = outward;
            let ring = [
                (lo[u], lo[v]),
                (hi[u], lo[v]),
                (hi[u], hi[v]),
                (lo[u], hi[v]),
            ];
            let corners = ring.map(|(a, b)| {
                let mut p = [0.0; 3];
                p[axis] = side;
                p[u] = a;
                p[v] = b;
                p
            });
            quad(mesh, corners, normal, surface);
        }
    }
}

/// One flat quad, its two triangles turned to face along `normal`.
fn quad(mesh: &mut Mesh, corners: [[f32; 3]; 4], normal: [f32; 3], surface: Surface) {
    let base = mesh.positions.len() as u32;
    for corner in corners {
        mesh.positions.push(corner);
        mesh.normals.push(normal);
        mesh.surfaces.push(surface);
    }
    let edge_a = sub(corners[1], corners[0]);
    let edge_b = sub(corners[2], corners[0]);
    let facing = dot(cross(edge_a, edge_b), normal);
    let order = if facing >= 0.0 {
        [0, 1, 2, 0, 2, 3]
    } else {
        [0, 2, 1, 0, 3, 2]
    };
    mesh.indices.extend(order.map(|i| base + i));
}

/// One ring of `segments` vertices at height `z`. `normal` gives the unit normal at each angle.
fn push_ring(
    mesh: &mut Mesh,
    radius: f32,
    z: f32,
    normal: impl Fn(f32) -> [f32; 3],
    segments: usize,
) {
    for j in 0..segments {
        let theta = TAU * j as f32 / segments as f32;
        mesh.positions
            .push([radius * theta.cos(), radius * theta.sin(), z]);
        mesh.normals.push(normal(theta));
        mesh.surfaces.push(Surface::Paper);
    }
}

/// Joins `rings` consecutive rings of `segments` vertices into triangles. The first ring starts at
/// vertex `start`. The rings run in order, each turning counter-clockwise seen from above, so the
/// triangles face outward for rings that climb.
fn band(mesh: &mut Mesh, start: usize, rings: usize, segments: usize) {
    for k in 0..rings - 1 {
        for j in 0..segments {
            let a = (start + k * segments + j) as u32;
            let b = (start + k * segments + (j + 1) % segments) as u32;
            let c = (start + (k + 1) * segments + j) as u32;
            let d = (start + (k + 1) * segments + (j + 1) % segments) as u32;
            mesh.indices.extend([a, b, c, b, d, c]);
        }
    }
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn model_of(mesh: Mesh) -> Model {
    let mut bounds = Aabb::EMPTY;
    for &p in &mesh.positions {
        bounds.include(p);
    }
    Model {
        meshes: [mesh, Mesh::default(), Mesh::default()],
        lines: Vec::new(),
        dots: Vec::new(),
        bounds,
    }
}

/// Whether the faces wind outward. The signed volume of a closed mesh is positive when its
/// normals point out, and it holds for a model whose parts overlap, where a per-face test
/// against the bounds centre would fail.
#[cfg(test)]
pub(crate) fn winds_outward(model: &Model) -> bool {
    let mesh = &model.meshes[0];
    let volume: f32 = mesh
        .indices
        .chunks(3)
        .map(|tri| {
            let [a, b, c] = [tri[0], tri[1], tri[2]].map(|i| mesh.positions[i as usize]);
            dot(a, cross(b, c)) / 6.0
        })
        .sum();
    volume > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_stands_on_its_foot_and_faces_outward() {
        let pin = clash_pin();
        assert!((pin.bounds.max[2] - PIN_HEIGHT).abs() < 1e-4);
        assert!(winds_outward(&pin), "the diamond faces outward");
    }

    #[test]
    fn the_stalk_stands_on_the_plinth_top_and_faces_outward() {
        let stalk = clash_stalk();
        assert!((stalk.bounds.max[2] - STALK_HEIGHT).abs() < 1e-4);
        assert!(winds_outward(&stalk), "the stalk faces outward");
    }

    #[test]
    fn the_ring_lies_flat_on_the_plinth_top_round_the_drone() {
        let ring = waiting_ring();
        assert!(ring.meshes[0].positions.iter().all(|p| p[2] == RING_LIFT));
        assert!(ring.bounds.max[0] <= RING_OUTER + 1e-4);
        assert!(ring.meshes[0].surfaces.iter().all(|s| *s == Surface::Lit));
    }
}
