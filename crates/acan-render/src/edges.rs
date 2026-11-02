//! The edges of a mesh that the renderer strokes: creases, which draw wherever they face the
//! eye, and smooth edges, which draw only on the contour. They are found once per model set.
//! The vertex shader picks the edges that draw each frame, so no CPU work runs per frame.
//!
//! The mesh is welded by position, then triangles that lie flat together form patches. Two
//! patches that meet at more than `CREASE_DEGREES` make a crease, and so does an open boundary,
//! such as where a plinth with no bottom face stands on the ground. A bevel is a short run of
//! patches whose turns each stay under the threshold but add up past it. Such a run has one
//! crease, where half of its turn has happened, so a bevel never draws two lines or three.
//! Every other join between two patches is smooth, like the facets of a cylinder's wall.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::model::Mesh;

/// Two patches that meet at more than this angle, in degrees, make a crease.
const CREASE_DEGREES: f32 = 50.0;
/// Triangles whose normals differ by less than this, in degrees, are one flat patch.
const FLAT_DEGREES: f32 = 1.0;
/// Positions that round to the same multiple of this, in plan units, are one welded vertex.
const WELD_STEP: f32 = 1e-3;

/// How an edge decides whether it draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    /// Draws when either face turns toward the eye.
    Crease,
    /// Draws on the contour only, where one face turns toward the eye and the other away.
    Smooth,
}

/// One welded edge of a mesh, in model space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    pub ends: [[f32; 3]; 2],
    /// The outward normals of the two faces that meet here. An open boundary has one face, so
    /// both normals are that face's.
    pub normals: [[f32; 3]; 2],
    pub fold: Fold,
}

impl Edge {
    /// Whether the edge draws for an eye at `eye`, in model space, before the depth test hides
    /// what lies behind other surfaces. Mirrors `edge_shows` in `plant.wgsl`.
    pub fn shows_from(&self, eye: [f32; 3]) -> bool {
        let [a, b] = self.ends;
        let mid = [
            0.5 * (a[0] + b[0]),
            0.5 * (a[1] + b[1]),
            0.5 * (a[2] + b[2]),
        ];
        let to_eye = sub(eye, mid);
        let [front_a, front_b] = self.normals.map(|n| dot(n, to_eye) > 0.0);
        match self.fold {
            Fold::Crease => front_a || front_b,
            Fold::Smooth => front_a != front_b,
        }
    }
}

/// Every edge of `mesh` where two patches meet, and every open boundary, in a fixed order.
/// Each welded edge appears at most once. Edges inside a flat patch are left out, since they
/// can never be on the contour.
pub fn edges(mesh: &Mesh) -> Vec<Edge> {
    let welded = Welded::new(mesh);
    let faces = welded.faces(mesh);
    let mut edge_faces: BTreeMap<(u32, u32), Vec<usize>> = BTreeMap::new();
    for (f, face) in faces.iter().enumerate() {
        for k in 0..3 {
            let key = edge_key(face.verts[k], face.verts[(k + 1) % 3]);
            edge_faces.entry(key).or_default().push(f);
        }
    }

    let mut parent: Vec<usize> = (0..faces.len()).collect();
    let flat = FLAT_DEGREES.to_radians().cos();
    for list in edge_faces.values() {
        if let [f, g] = list[..]
            && dot(faces[f].normal, faces[g].normal) >= flat
        {
            let (rf, rg) = (find(&mut parent, f), find(&mut parent, g));
            parent[rf] = rg;
        }
    }
    let patch: Vec<usize> = (0..faces.len()).map(|f| find(&mut parent, f)).collect();

    let mut edges = Vec::new();
    let mut bends: BTreeMap<(usize, usize), Bend> = BTreeMap::new();
    for (&(a, b), list) in &edge_faces {
        let ends = [welded.positions[a as usize], welded.positions[b as usize]];
        let edge = |fold: Fold, f: usize, g: usize| Edge {
            ends,
            normals: [faces[f].normal, faces[g].normal],
            fold,
        };
        match list[..] {
            [f] => edges.push(edge(Fold::Crease, f, f)),
            [f, g] => {
                let (p, q) = (patch[f], patch[g]);
                if p == q {
                    continue;
                }
                let turn = angle_degrees(faces[p].normal, faces[q].normal);
                if turn > CREASE_DEGREES {
                    edges.push(edge(Fold::Crease, f, g));
                } else {
                    let ends = (p.min(q), p.max(q));
                    bends
                        .entry(ends)
                        .or_insert_with(|| Bend {
                            ends,
                            turn,
                            edges: Vec::new(),
                        })
                        .edges
                        .push(edge(Fold::Smooth, f, g));
                }
            }
            // Three or more faces on one edge: not a surface, so it always draws.
            _ => edges.push(edge(Fold::Crease, list[0], list[1])),
        }
    }
    let mut bends: Vec<Bend> = bends.into_values().collect();
    for index in bevel_midlines(&bends) {
        for edge in &mut bends[index].edges {
            edge.fold = Fold::Crease;
        }
    }
    edges.extend(bends.into_iter().flat_map(|bend| bend.edges));
    edges
}

/// A smooth join between two patches, with the edges that carry it.
struct Bend {
    ends: (usize, usize),
    turn: f32,
    edges: Vec<Edge>,
}

/// The bend that is the crease of each bevel run. A run is a path of smooth joins that starts
/// and ends at a crease. When its turns add up past `CREASE_DEGREES`, the crease goes where
/// half the turn has happened. A ring of smooth joins, such as a cylinder's wall, has no ends
/// and has no crease.
fn bevel_midlines(bends: &[Bend]) -> Vec<usize> {
    let mut adjacency: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (index, bend) in bends.iter().enumerate() {
        adjacency.entry(bend.ends.0).or_default().push(index);
        adjacency.entry(bend.ends.1).or_default().push(index);
    }

    let mut visited: BTreeSet<usize> = BTreeSet::new();
    let mut midlines = Vec::new();
    for &start in adjacency.keys() {
        if visited.contains(&start) {
            continue;
        }
        let (nodes, edges) = component(start, &adjacency, bends);
        visited.extend(nodes.iter().copied());
        let is_path = edges.len() + 1 == nodes.len()
            && nodes.iter().all(|n| adjacency[n].len() <= 2)
            && edges.len() >= 2;
        if !is_path {
            continue;
        }
        let order = path_order(&nodes, &adjacency, bends);
        let total: f32 = order.iter().map(|&e| bends[e].turn).sum();
        if total <= CREASE_DEGREES {
            continue;
        }
        let mut seen = 0.0;
        for &e in &order {
            seen += bends[e].turn;
            if seen >= 0.5 * total - 1e-3 {
                midlines.push(e);
                break;
            }
        }
    }
    midlines
}

/// The nodes and bends reachable from `start` through smooth joins.
fn component(
    start: usize,
    adjacency: &BTreeMap<usize, Vec<usize>>,
    bends: &[Bend],
) -> (Vec<usize>, Vec<usize>) {
    let mut nodes = vec![start];
    let mut edges = Vec::new();
    let mut seen_nodes = BTreeSet::from([start]);
    let mut seen_edges = BTreeSet::new();
    let mut stack = vec![start];
    while let Some(node) = stack.pop() {
        for &e in &adjacency[&node] {
            if seen_edges.insert(e) {
                edges.push(e);
            }
            let other = other_end(bends[e].ends, node);
            if seen_nodes.insert(other) {
                nodes.push(other);
                stack.push(other);
            }
        }
    }
    (nodes, edges)
}

/// The bends of a path, from one end to the other. Only called on a component that is a path.
fn path_order(
    nodes: &[usize],
    adjacency: &BTreeMap<usize, Vec<usize>>,
    bends: &[Bend],
) -> Vec<usize> {
    let Some(&start) = nodes.iter().find(|n| adjacency[n].len() == 1) else {
        return Vec::new();
    };
    let mut order = Vec::new();
    let mut node = start;
    let mut came_by = None;
    while let Some(&next) = adjacency[&node].iter().find(|&&e| Some(e) != came_by) {
        order.push(next);
        node = other_end(bends[next].ends, node);
        came_by = Some(next);
    }
    order
}

fn other_end(ends: (usize, usize), node: usize) -> usize {
    if ends.0 == node { ends.1 } else { ends.0 }
}

fn find(parent: &mut [usize], mut f: usize) -> usize {
    while parent[f] != f {
        parent[f] = parent[parent[f]];
        f = parent[f];
    }
    f
}

#[derive(Clone, Copy)]
struct Face {
    verts: [u32; 3],
    /// Outward, since the mesh winds counter-clockwise seen from outside.
    normal: [f32; 3],
}

/// The mesh's positions with coincident vertices merged, so faces that share a corner in space
/// share it in the index, whatever their normals.
struct Welded {
    positions: Vec<[f32; 3]>,
    index: Vec<u32>,
}

impl Welded {
    fn new(mesh: &Mesh) -> Self {
        let mut ids: HashMap<[i64; 3], u32> = HashMap::new();
        let mut positions = Vec::new();
        let mut index = Vec::with_capacity(mesh.positions.len());
        for &p in &mesh.positions {
            let key = p.map(|v| (v / WELD_STEP).round() as i64);
            let id = *ids.entry(key).or_insert_with(|| {
                positions.push(p);
                (positions.len() - 1) as u32
            });
            index.push(id);
        }
        Self { positions, index }
    }

    /// The triangles that have three distinct corners and a usable normal.
    fn faces(&self, mesh: &Mesh) -> Vec<Face> {
        mesh.indices
            .chunks_exact(3)
            .filter_map(|tri| {
                let verts = [tri[0], tri[1], tri[2]].map(|i| self.index[i as usize]);
                if verts[0] == verts[1] || verts[1] == verts[2] || verts[0] == verts[2] {
                    return None;
                }
                let [a, b, c] = verts.map(|v| self.positions[v as usize]);
                let normal = normalize(cross(sub(b, a), sub(c, a)))?;
                Some(Face { verts, normal })
            })
            .collect()
    }
}

fn edge_key(a: u32, b: u32) -> (u32, u32) {
    (a.min(b), a.max(b))
}

fn normalize(v: [f32; 3]) -> Option<[f32; 3]> {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    (len > 1e-12).then(|| v.map(|c| c / len))
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn angle_degrees(a: [f32; 3], b: [f32; 3]) -> f32 {
    dot(a, b).clamp(-1.0, 1.0).acos().to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Surface;

    /// Appends a quad with the given corners in order, as two triangles.
    fn quad(mesh: &mut Mesh, c: [[f32; 3]; 4]) {
        let first = mesh.positions.len() as u32;
        for p in c {
            mesh.positions.push(p);
            mesh.normals.push([0.0, 0.0, 1.0]);
            mesh.surfaces.push(Surface::Paper);
        }
        mesh.indices
            .extend([first, first + 1, first + 2, first, first + 2, first + 3]);
    }

    /// A box from -1 to 1 with every face split apart, as the stand-in models do. Each face
    /// winds counter-clockwise seen from outside.
    fn split_cube() -> Mesh {
        let mut mesh = Mesh::default();
        for axis in 0..3 {
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            for side in [-1.0_f32, 1.0] {
                let corner = |a: f32, b: f32| {
                    let mut p = [0.0; 3];
                    p[axis] = side;
                    p[u] = a;
                    p[v] = b;
                    p
                };
                let c = [
                    corner(-1.0, -1.0),
                    corner(1.0, -1.0),
                    corner(1.0, 1.0),
                    corner(-1.0, 1.0),
                ];
                quad(
                    &mut mesh,
                    if side > 0.0 {
                        c
                    } else {
                        [c[0], c[3], c[2], c[1]]
                    },
                );
            }
        }
        mesh
    }

    /// A prism along y from -1 to 1, whose cross-section is the polyline `profile` in (x, z).
    /// Each profile segment is one flat strip. The caps are left open, so the open boundary
    /// edges are the profile ends.
    fn extrude(profile: &[[f32; 2]]) -> Mesh {
        let mut mesh = Mesh::default();
        for pair in profile.windows(2) {
            let [a, b] = [pair[0], pair[1]];
            quad(
                &mut mesh,
                [
                    [a[0], -1.0, a[1]],
                    [b[0], -1.0, b[1]],
                    [b[0], 1.0, b[1]],
                    [a[0], 1.0, a[1]],
                ],
            );
        }
        mesh
    }

    /// Edges of `fold` whose two ends sit on the profile point `at` (in x and z) and run
    /// along y.
    fn along_y_at(edges: &[Edge], at: [f32; 2], fold: Fold) -> usize {
        edges
            .iter()
            .filter(|e| {
                e.fold == fold
                    && e.ends
                        .iter()
                        .all(|p| (p[0] - at[0]).abs() < 1e-4 && (p[2] - at[1]).abs() < 1e-4)
                    && (e.ends[0][1] - e.ends[1][1]).abs() > 1.0
            })
            .count()
    }

    fn count(edges: &[Edge], fold: Fold) -> usize {
        edges.iter().filter(|e| e.fold == fold).count()
    }

    #[test]
    fn a_cube_gives_twelve_creases_not_twenty_four_or_thirty_six() {
        let edges = edges(&split_cube());
        assert_eq!(edges.len(), 12, "one edge per cube edge: {edges:?}");
        assert_eq!(count(&edges, Fold::Crease), 12);
        let mut keys: Vec<[[i64; 3]; 2]> = edges
            .iter()
            .map(|e| {
                let mut ends = e.ends.map(|p| p.map(|v| (v * 1000.0).round() as i64));
                ends.sort();
                ends
            })
            .collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 12, "no edge appears twice");
    }

    #[test]
    fn face_normals_point_out_of_the_solid() {
        for edge in edges(&split_cube()) {
            for (n, end) in edge.normals.iter().zip(edge.ends) {
                assert!(dot(*n, end) > 0.0, "{edge:?} has an inward normal");
            }
        }
    }

    #[test]
    fn a_cube_seen_from_a_corner_hides_the_three_edges_between_back_faces() {
        let edges = edges(&split_cube());
        let shown = edges
            .iter()
            .filter(|e| e.shows_from([6.0, 5.0, 4.0]))
            .count();
        assert_eq!(shown, 9);
    }

    #[test]
    fn a_chamfered_edge_gives_one_bevel_crease_and_its_two_ends() {
        let c = 0.5;
        let profile = [
            [-1.0, -1.0],
            [1.0, -1.0],
            [1.0, 1.0 - c],
            [1.0 - c, 1.0],
            [-1.0, 1.0],
            [-1.0, -1.0],
        ];
        let mut mesh = extrude(&profile);
        // Caps at y = -1 and y = 1, each a fan from the first profile point.
        for y in [-1.0_f32, 1.0] {
            let p: Vec<[f32; 3]> = profile[..5].iter().map(|q| [q[0], y, q[1]]).collect();
            for k in 1..4 {
                let first = mesh.positions.len() as u32;
                for corner in [p[0], p[k], p[k + 1]] {
                    mesh.positions.push(corner);
                    mesh.normals.push([0.0, 1.0, 0.0]);
                    mesh.surfaces.push(Surface::Paper);
                }
                mesh.indices.extend([first, first + 1, first + 2]);
            }
        }
        let edges = edges(&mesh);
        assert_eq!(
            along_y_at(&edges, [1.0, 1.0 - c], Fold::Crease)
                + along_y_at(&edges, [1.0 - c, 1.0], Fold::Crease),
            1,
            "the chamfer has one crease, not one per side"
        );
        assert_eq!(
            along_y_at(&edges, [1.0, 1.0 - c], Fold::Smooth)
                + along_y_at(&edges, [1.0 - c, 1.0], Fold::Smooth),
            1,
            "its other side draws only on the contour"
        );
        assert_eq!(
            count(&edges, Fold::Crease),
            14,
            "three box edges, one bevel, and ten cap edges"
        );
    }

    #[test]
    fn a_three_step_bevel_has_one_crease_where_half_its_turn_is() {
        // A wall turning to a top in three 30 degree steps, a 90 degree corner in all.
        let step = |angle: f32| [angle.to_radians().cos(), angle.to_radians().sin()];
        let mut profile = vec![[1.0, 0.0], [1.0, 1.0]];
        let mut at = [1.0_f32, 1.0];
        for angle in [120.0_f32, 150.0, 180.0] {
            let d = step(angle);
            at = [at[0] + 0.5 * d[0], at[1] + 0.5 * d[1]];
            profile.push(at);
        }
        profile.push([at[0] - 2.0, at[1]]);
        let edges = edges(&extrude(&profile));
        for (point, what) in [(1, "wall meets the bevel"), (3, "bevel meets the top")] {
            assert_eq!(
                along_y_at(&edges, profile[point], Fold::Crease),
                0,
                "{what} smoothly"
            );
            assert_eq!(along_y_at(&edges, profile[point], Fold::Smooth), 1);
        }
        assert_eq!(
            along_y_at(&edges, profile[2], Fold::Crease),
            1,
            "one crease in the bevel"
        );
    }

    #[test]
    fn a_smooth_cylinder_wall_is_smooth_edges_between_two_rims() {
        let mut mesh = Mesh::default();
        let segments = 32;
        for k in 0..segments {
            let (s0, c0) = (std::f32::consts::TAU * k as f32 / segments as f32).sin_cos();
            let (s1, c1) = (std::f32::consts::TAU * (k + 1) as f32 / segments as f32).sin_cos();
            quad(
                &mut mesh,
                [[c0, s0, 1.0], [c1, s1, 1.0], [c1, s1, 5.0], [c0, s0, 5.0]],
            );
        }
        let edges = edges(&mesh);
        assert_eq!(
            count(&edges, Fold::Crease),
            2 * segments,
            "the two rims are creases"
        );
        assert_eq!(count(&edges, Fold::Smooth), segments);
        let contour = edges
            .iter()
            .filter(|e| e.fold == Fold::Smooth && e.shows_from([40.0, 3.0, 3.0]))
            .count();
        assert_eq!(
            contour, 2,
            "a wall seen from the side has two contour lines"
        );
    }

    #[test]
    fn an_open_boundary_on_the_ground_is_a_crease() {
        let mut mesh = Mesh::default();
        quad(
            &mut mesh,
            [
                [0.0, 0.0, 0.0],
                [4.0, 0.0, 0.0],
                [4.0, 3.0, 0.0],
                [0.0, 3.0, 0.0],
            ],
        );
        let edges = edges(&mesh);
        assert_eq!(edges.len(), 4, "the ground contact draws");
        assert!(
            edges
                .iter()
                .all(|e| e.fold == Fold::Crease && e.normals[0] == e.normals[1])
        );
        assert!(edges.iter().all(|e| e.shows_from([2.0, -9.0, 9.0])));
        assert!(!edges.iter().any(|e| e.shows_from([2.0, -9.0, -9.0])));
    }
}
