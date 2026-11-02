//! Turns models and scenes into the vertex data the GPU draws. It runs once per model set and
//! once per scene, never per frame.

use std::ops::Range;

use bytemuck::{Pod, Zeroable};

use crate::edges::{self, Fold};
use crate::model::{Aabb, Detail, Dot3, Line3, Mesh};
use crate::scene::{Beam, HueLine, Instance, Ribbon};
use crate::theme::{SHADOW_REACH, SHADOW_SHIFT, dot_slot, flat_code, ink_slot};

/// A ribbon's miter never stretches its width by more than this at a sharp turn.
const MAX_MITER: f32 = 2.0;

/// Strokes and dots are quads of four vertices, drawn by `quad_indices`. Instanced over every
/// machine, a shared corner is fetched and shaded once instead of twice.
pub const QUAD_CORNERS: u32 = 4;

/// The corners of a stroke segment. `x` is 0 at the first end and 1 at the second. `y` is -1
/// or 1 across the stroke.
const SEGMENT_CORNERS: [[f32; 2]; QUAD_CORNERS as usize] =
    [[0.0, -1.0], [1.0, -1.0], [1.0, 1.0], [0.0, 1.0]];

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct MeshVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub code: u32,
}

/// One corner of a painted stroke segment. Each segment is four vertices, one per corner in
/// `SEGMENT_CORNERS`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct LineVertex {
    pub a: [f32; 3],
    pub b: [f32; 3],
    pub corner: [f32; 2],
    pub width: f32,
    pub tone: u32,
}

/// One corner of a mesh edge's stroke. Each edge is four vertices, like a painted segment.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct EdgeVertex {
    pub ends: [[f32; 3]; 2],
    /// The outward normals of the two faces that meet at the edge.
    pub normals: [[f32; 3]; 2],
    pub corner: [f32; 2],
    /// 1 for a crease, 0 for a smooth edge that draws only on the contour.
    pub crease: u32,
}

/// One corner of a disc. `tangent` and `bitangent` span the disc, scaled by its radius.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct DotVertex {
    pub center: [f32; 3],
    pub tangent: [f32; 3],
    pub bitangent: [f32; 3],
    pub corner: [f32; 2],
    pub tone: u32,
}

/// One corner of a ribbon segment.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct RibbonVertex {
    pub position: [f32; 2],
    /// -1 on one side of the ribbon and 1 on the other.
    pub side: f32,
    /// Plan units along the centre line from the ribbon's start.
    pub along: f32,
    /// The ribbon's full length and half width, in plan units.
    pub length: f32,
    pub half: f32,
}

/// One corner of a hue line's stroke. Each piece is four vertices, one per corner in
/// `SEGMENT_CORNERS`, like a painted segment.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct HueVertex {
    /// Where the stroke starts. A belt's start is a world point. A beam's start is in the frame of
    /// `origin`, as [`Beam::from`] is.
    pub a: [f32; 3],
    pub b: [f32; 3],
    pub corner: [f32; 2],
    /// The stroke's width in device px.
    pub width: f32,
    /// The stroke's opacity, which fades toward a belt's ends.
    pub alpha: f32,
    pub tint: u32,
    /// The point `a` is measured from. A belt has zero here, so its start is the world point.
    pub origin: [f32; 3],
    /// How `a` grows about `origin`. Zero keeps it at plan size.
    pub grow: [f32; 2],
    /// The angle `a` turns about `origin`.
    pub turn: f32,
}

/// A contact shadow quad: one corner, with the footprint it falls around.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct HaloVertex {
    pub position: [f32; 3],
    /// The model's footprint in model space: min x, min y, max x, max y.
    pub rect: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct InstanceRaw {
    pub offset: [f32; 3],
    pub tint: u32,
    pub grow: [f32; 2],
    pub turn: f32,
}

/// Vertex data split by detail. `counts[level]` is the number of vertices drawn at that level
/// of detail, cumulative, so a level draws the prefix `0..counts[level]`.
#[derive(Debug, PartialEq)]
pub struct Strokes<V> {
    pub vertices: Vec<V>,
    pub counts: [u32; 3],
}

pub fn mesh_vertices(mesh: &Mesh) -> Vec<MeshVertex> {
    assert_eq!(
        mesh.positions.len(),
        mesh.normals.len(),
        "mesh needs one normal per position"
    );
    assert_eq!(
        mesh.positions.len(),
        mesh.surfaces.len(),
        "mesh needs one surface per position"
    );
    mesh.positions
        .iter()
        .zip(&mesh.normals)
        .zip(&mesh.surfaces)
        .map(|((&position, &normal), &surface)| MeshVertex {
            position,
            normal,
            code: flat_code(surface),
        })
        .collect()
}

pub fn line_vertices(lines: &[Line3]) -> Strokes<LineVertex> {
    let mut buckets: [Vec<LineVertex>; 3] = Default::default();
    for line in lines {
        let segments =
            line.points.len().saturating_sub(1) + usize::from(line.closed && line.points.len() > 2);
        let bucket = &mut buckets[level_of(line.detail)];
        for s in 0..segments {
            let a = line.points[s];
            let b = line.points[(s + 1) % line.points.len()];
            for corner in SEGMENT_CORNERS {
                bucket.push(LineVertex {
                    a,
                    b,
                    corner,
                    width: line.width,
                    tone: ink_slot(line.tone),
                });
            }
        }
    }
    cumulative(buckets)
}

/// The stroke of every edge of each level's mesh, drawn at that level of detail.
pub fn edge_vertices(meshes: &[Mesh; 3]) -> Strokes<EdgeVertex> {
    cumulative(meshes.each_ref().map(|mesh| {
        edges::edges(mesh)
            .iter()
            .flat_map(|edge| {
                SEGMENT_CORNERS.map(|corner| EdgeVertex {
                    ends: edge.ends,
                    normals: edge.normals,
                    corner,
                    crease: u32::from(edge.fold == Fold::Crease),
                })
            })
            .collect()
    }))
}

pub fn dot_vertices(dots: &[Dot3]) -> Strokes<DotVertex> {
    const CORNERS: [[f32; 2]; QUAD_CORNERS as usize] =
        [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];
    let mut buckets: [Vec<DotVertex>; 3] = Default::default();
    for dot in dots {
        let (tangent, bitangent) = disc_axes(dot.normal, dot.radius);
        let bucket = &mut buckets[level_of(dot.detail)];
        for corner in CORNERS {
            bucket.push(DotVertex {
                center: dot.center,
                tangent,
                bitangent,
                corner,
                tone: dot_slot(dot.tone),
            });
        }
    }
    cumulative(buckets)
}

/// The indices of `quads` quads of `QUAD_CORNERS` vertices each, two triangles a quad.
pub fn quad_indices(quads: u32) -> Vec<u32> {
    (0..quads)
        .flat_map(|q| [0, 1, 2, 0, 2, 3].map(|i| q * QUAD_CORNERS + i))
        .collect()
}

/// The width of a hue line in device px, before the shader's thinnest-stroke floor.
pub const HUE_PX: f32 = 1.0;
/// A hue line's opacity along the middle of its belt.
pub const HUE_ALPHA: f32 = 0.8;
/// The longest piece of a hue line, in plan units. Each piece has one opacity, so the fade steps
/// by this much.
const HUE_PIECE: f32 = 1.0;
/// The share of a belt's length over which its hue line fades in from each end.
const HUE_FADE_SHARE: f32 = 0.3;

/// Four vertices per piece of every hue line, pieces at most `HUE_PIECE` plan units long. A
/// piece's opacity follows how far its middle lies from the nearer end of its belt.
pub fn hue_vertices(lines: &[HueLine]) -> Vec<HueVertex> {
    let mut out = Vec::new();
    for line in lines {
        let total = polyline_length(&line.centre);
        if total <= 0.0 {
            continue;
        }
        let pieces = (total / HUE_PIECE).ceil().max(1.0) as usize;
        let step = total / pieces as f32;
        let fade = HUE_FADE_SHARE * total;
        for piece in 0..pieces {
            let from = piece as f32 * step;
            let to = from + step;
            let middle = 0.5 * (from + to);
            let nearer_end = middle.min(total - middle);
            let alpha = HUE_ALPHA * (nearer_end / fade).min(1.0);
            let [ax, ay] = point_at(&line.centre, from);
            let [bx, by] = point_at(&line.centre, to);
            for corner in SEGMENT_CORNERS {
                out.push(HueVertex {
                    a: [ax, ay, 0.0],
                    b: [bx, by, 0.0],
                    corner,
                    width: HUE_PX,
                    alpha,
                    tint: u32::from(line.tint),
                    origin: [0.0; 3],
                    grow: [0.0; 2],
                    turn: 0.0,
                });
            }
        }
    }
    out
}

/// The device px width of a beam, and its opacity. A beam is as fine as a hue line.
pub const BEAM_PX: f32 = HUE_PX;
pub const BEAM_ALPHA: f32 = HUE_ALPHA;

/// Four vertices per beam, one stroke from its `from` to its `to`.
pub fn beam_vertices(beams: &[Beam]) -> Vec<HueVertex> {
    beams
        .iter()
        .flat_map(|beam| {
            SEGMENT_CORNERS.map(|corner| HueVertex {
                a: beam.from,
                b: beam.to,
                corner,
                width: BEAM_PX,
                alpha: BEAM_ALPHA,
                tint: u32::from(beam.tint),
                origin: beam.origin,
                grow: beam.grow,
                turn: beam.turn,
            })
        })
        .collect()
}

fn segment_length(a: [f32; 2], b: [f32; 2]) -> f32 {
    (b[0] - a[0]).hypot(b[1] - a[1])
}

/// The length of a polyline, in plan units.
fn polyline_length(points: &[[f32; 2]]) -> f32 {
    points
        .windows(2)
        .map(|pair| segment_length(pair[0], pair[1]))
        .sum()
}

/// The point `along` plan units from the first point of a polyline, held at its ends.
fn point_at(points: &[[f32; 2]], along: f32) -> [f32; 2] {
    let mut left = along.max(0.0);
    for pair in points.windows(2) {
        let len = segment_length(pair[0], pair[1]);
        if left <= len {
            let t = if len > 0.0 { left / len } else { 0.0 };
            return [
                pair[0][0] + (pair[1][0] - pair[0][0]) * t,
                pair[0][1] + (pair[1][1] - pair[0][1]) * t,
            ];
        }
        left -= len;
    }
    points.last().copied().unwrap_or([0.0; 2])
}

/// Six vertices covering `bounds` plus the contact shadow's reach and its shift.
pub fn halo_vertices(bounds: &Aabb) -> [HaloVertex; 6] {
    let reach = SHADOW_REACH + SHADOW_SHIFT;
    let [x0, y0, x1, y1] = [
        bounds.min[0] - reach,
        bounds.min[1] - reach,
        bounds.max[0] + reach,
        bounds.max[1] + reach,
    ];
    let rect = [bounds.min[0], bounds.min[1], bounds.max[0], bounds.max[1]];
    let corner = |x: f32, y: f32| HaloVertex {
        position: [x, y, 0.0],
        rect,
    };
    let (a, b, c, d) = (
        corner(x0, y0),
        corner(x1, y0),
        corner(x1, y1),
        corner(x0, y1),
    );
    [a, b, c, a, c, d]
}

/// Two triangles per centre-line segment of every ribbon. Corners are mitred, so a ribbon
/// keeps its width through a turn.
pub fn ribbon_vertices(ribbons: &[Ribbon]) -> Vec<RibbonVertex> {
    let mut out = Vec::new();
    for ribbon in ribbons {
        let centre = &ribbon.centre;
        if centre.len() < 2 {
            continue;
        }
        let mut along = vec![0.0_f32; centre.len()];
        for i in 1..centre.len() {
            let d = sub2(centre[i], centre[i - 1]);
            along[i] = along[i - 1] + d[0].hypot(d[1]);
        }
        let length = along[centre.len() - 1];
        let half = ribbon.width * 0.5;
        let offsets = miter_offsets(centre);
        let corner = |i: usize, side: f32| RibbonVertex {
            position: [
                centre[i][0] + side * half * offsets[i][0],
                centre[i][1] + side * half * offsets[i][1],
            ],
            side,
            along: along[i],
            length,
            half,
        };
        for i in 0..centre.len() - 1 {
            let (a, b, c, d) = (
                corner(i, 1.0),
                corner(i, -1.0),
                corner(i + 1, -1.0),
                corner(i + 1, 1.0),
            );
            out.extend([a, b, c, a, c, d]);
        }
    }
    out
}

/// The offset of each centre point's side corner for a unit half width: the mitred normal.
fn miter_offsets(points: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let unit = |v: [f32; 2]| {
        let len = v[0].hypot(v[1]);
        (len > 1e-6).then(|| [v[0] / len, v[1] / len])
    };
    let normal = |d: [f32; 2]| [-d[1], d[0]];
    (0..points.len())
        .map(|i| {
            let incoming = (i > 0)
                .then(|| unit(sub2(points[i], points[i - 1])))
                .flatten();
            let outgoing = (i + 1 < points.len())
                .then(|| unit(sub2(points[i + 1], points[i])))
                .flatten();
            match (incoming, outgoing) {
                (Some(a), Some(b)) => {
                    let (na, nb) = (normal(a), normal(b));
                    let Some(m) = unit([na[0] + nb[0], na[1] + nb[1]]) else {
                        return na;
                    };
                    let stretch = (1.0 / (m[0] * na[0] + m[1] * na[1]).max(1e-3)).min(MAX_MITER);
                    [m[0] * stretch, m[1] * stretch]
                }
                (Some(d), None) | (None, Some(d)) => normal(d),
                (None, None) => [0.0, 1.0],
            }
        })
        .collect()
}

fn sub2(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

/// Instance data grouped kind by kind, and the instance range of each kind.
pub fn instance_data(instances: &[Instance], kinds: usize) -> (Vec<InstanceRaw>, Vec<Range<u32>>) {
    let mut raw = Vec::with_capacity(instances.len());
    let mut ranges = Vec::with_capacity(kinds);
    for kind in 0..kinds {
        let start = raw.len() as u32;
        raw.extend(
            instances
                .iter()
                .filter(|inst| usize::from(inst.kind) == kind)
                .map(|inst| InstanceRaw {
                    offset: inst.offset,
                    tint: u32::from(inst.tint),
                    grow: inst.grow,
                    turn: inst.turn,
                }),
        );
        ranges.push(start..raw.len() as u32);
    }
    assert_eq!(
        raw.len(),
        instances.len(),
        "every instance must name a model kind below {kinds}"
    );
    (raw, ranges)
}

/// Two unit-free axes spanning the disc with normal `normal`, each as long as `radius`.
fn disc_axes(normal: [f32; 3], radius: f32) -> ([f32; 3], [f32; 3]) {
    let n = normalize(normal);
    let helper = if n[2].abs() < 0.9 {
        [0.0, 0.0, 1.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let t1 = normalize(cross(n, helper));
    let t2 = cross(n, t1);
    (scale(t1, radius), scale(t2, radius))
}

fn level_of(detail: Detail) -> usize {
    detail as usize
}

fn cumulative<V>(buckets: [Vec<V>; 3]) -> Strokes<V> {
    let counts = [
        buckets[0].len() as u32,
        (buckets[0].len() + buckets[1].len()) as u32,
        (buckets[0].len() + buckets[1].len() + buckets[2].len()) as u32,
    ];
    let vertices: Vec<V> = buckets.into_iter().flatten().collect();
    Strokes { vertices, counts }
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-12);
    scale(v, 1.0 / len)
}

fn scale(v: [f32; 3], k: f32) -> [f32; 3] {
    [v[0] * k, v[1] * k, v[2] * k]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Ink, Surface};

    fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }

    fn stroke(points: Vec<[f32; 3]>, closed: bool, detail: Detail) -> Line3 {
        Line3 {
            points,
            closed,
            tone: Ink::Mid,
            width: 0.6,
            detail,
        }
    }

    #[test]
    fn open_and_closed_polylines_make_their_segments() {
        let open = stroke(
            vec![[0.0; 3], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0]],
            false,
            Detail::Always,
        );
        let closed = stroke(
            vec![[0.0; 3], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0]],
            true,
            Detail::Always,
        );
        assert_eq!(line_vertices(&[open]).vertices.len(), 2 * 4);
        assert_eq!(line_vertices(&[closed]).vertices.len(), 3 * 4);
    }

    #[test]
    fn lines_are_grouped_so_each_level_draws_a_prefix() {
        let lines = [
            stroke(vec![[0.0; 3], [1.0, 0.0, 0.0]], false, Detail::Close),
            stroke(vec![[0.0; 3], [2.0, 0.0, 0.0]], false, Detail::Always),
            stroke(vec![[0.0; 3], [3.0, 0.0, 0.0]], false, Detail::Near),
        ];
        let strokes = line_vertices(&lines);
        assert_eq!(strokes.counts, [4, 8, 12]);
        assert_eq!(strokes.vertices[0].b, [2.0, 0.0, 0.0]);
        assert_eq!(strokes.vertices[4].b, [3.0, 0.0, 0.0]);
        assert_eq!(strokes.vertices[8].b, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn a_hue_line_fades_toward_both_ends_and_is_steady_between() {
        let line = HueLine {
            centre: vec![[0.0, 0.0], [10.0, 0.0]],
            tint: 3,
        };
        let vertices = hue_vertices(&[line]);
        let alphas: Vec<f32> = vertices.chunks_exact(4).map(|quad| quad[0].alpha).collect();
        assert_eq!(alphas.len(), 10, "one piece per plan unit");
        assert!(
            alphas[0] < alphas[1] && alphas[1] < alphas[2],
            "it fades in"
        );
        assert!(
            alphas[9] < alphas[8] && alphas[8] < alphas[7],
            "it fades out"
        );
        assert_eq!(alphas[5], HUE_ALPHA, "the middle is fully opaque");
        assert!(vertices.iter().all(|vertex| vertex.tint == 3));
    }

    #[test]
    fn a_quad_is_two_triangles_over_its_own_four_corners() {
        assert_eq!(quad_indices(2), [0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7]);
        // Both triangles turn the same way, so together they cover the quad without crossing.
        let turn = |[a, b, c]: [usize; 3]| {
            let [p, q, r] = [a, b, c].map(|i| SEGMENT_CORNERS[i]);
            (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0])
        };
        assert!(turn([0, 1, 2]) > 0.0 && turn([0, 2, 3]) > 0.0);
    }

    #[test]
    fn dot_axes_lie_in_the_surface_and_have_the_radius() {
        let (t1, t2) = disc_axes([0.0, 0.0, 1.0], 0.9);
        assert!((dot(t1, t1).sqrt() - 0.9).abs() < 1e-5);
        assert!((dot(t2, t2).sqrt() - 0.9).abs() < 1e-5);
        assert!(dot(t1, t2).abs() < 1e-5);
        assert!(dot(t1, [0.0, 0.0, 1.0]).abs() < 1e-5);
        let (u1, u2) = disc_axes([1.0, 0.0, 0.0], 2.0);
        assert!(dot(u1, [1.0, 0.0, 0.0]).abs() < 1e-5);
        assert!(dot(u2, [1.0, 0.0, 0.0]).abs() < 1e-5);
    }

    #[test]
    fn halo_covers_the_footprint_and_its_falloff() {
        let bounds = Aabb {
            min: [-48.0, -43.0, 0.0],
            max: [48.0, 43.0, 6.0],
        };
        let corners = halo_vertices(&bounds);
        let xs: Vec<f32> = corners.iter().map(|v| v.position[0]).collect();
        let lowest = xs.iter().copied().fold(f32::INFINITY, f32::min);
        assert!((lowest - (-48.0 - SHADOW_REACH - SHADOW_SHIFT)).abs() < 1e-4);
        assert!(corners.iter().all(|v| v.rect == [-48.0, -43.0, 48.0, 43.0]));
    }

    #[test]
    fn a_ribbon_keeps_its_width_through_a_right_angle_turn() {
        let ribbon = Ribbon {
            centre: vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]],
            width: 4.0,
        };
        let vertices = ribbon_vertices(&[ribbon]);
        assert_eq!(vertices.len(), 2 * 6, "two segments of two triangles");
        let at_turn: Vec<[f32; 2]> = vertices
            .iter()
            .filter(|v| (v.along - 10.0).abs() < 1e-4)
            .map(|v| v.position)
            .collect();
        assert!(!at_turn.is_empty());
        for p in at_turn {
            let off = [(p[0] - 10.0).abs(), p[1].abs()];
            assert!(
                (off[0] - 2.0).abs() < 1e-3 && (off[1] - 2.0).abs() < 1e-3,
                "the mitred corner sits a half width from both runs: {p:?}"
            );
        }
        assert!(vertices.iter().all(|v| v.length == 20.0 && v.half == 2.0));
    }

    #[test]
    fn a_ribbon_with_one_point_draws_nothing() {
        let ribbon = Ribbon {
            centre: vec![[0.0, 0.0]],
            width: 4.0,
        };
        assert!(ribbon_vertices(&[ribbon]).is_empty());
    }

    #[test]
    fn instances_group_by_kind() {
        let inst = |kind: u16, x: f32| Instance {
            offset: [x, 0.0, 0.0],
            kind,
            tint: 0,
            grow: [0.0; 2],
            turn: 0.25 * x,
        };
        let (raw, ranges) = instance_data(&[inst(1, 1.0), inst(0, 2.0), inst(1, 3.0)], 2);
        assert_eq!(ranges, vec![0..1, 1..3]);
        let xs: Vec<f32> = raw.iter().map(|r| r.offset[0]).collect();
        assert_eq!(xs, [2.0, 1.0, 3.0]);
        let turns: Vec<f32> = raw.iter().map(|r| r.turn).collect();
        assert_eq!(turns, [0.5, 0.25, 0.75], "each instance keeps its own turn");
    }

    #[test]
    fn a_beam_strokes_from_its_start_to_its_end_in_its_tint() {
        let beam = Beam {
            from: [1.0, 2.0, 3.0],
            to: [4.0, 5.0, 6.0],
            origin: [7.0, 8.0, 9.0],
            grow: [20.0, 24.0],
            turn: 0.5,
            tint: 2,
        };
        let vertices = beam_vertices(&[beam]);
        assert_eq!(vertices.len(), QUAD_CORNERS as usize);
        assert!(vertices.iter().all(|v| v.a == beam.from && v.b == beam.to));
        assert!(vertices.iter().all(|v| v.tint == 2 && v.width == BEAM_PX));
        assert!(
            vertices
                .iter()
                .all(|v| v.origin == beam.origin && v.grow == beam.grow && v.turn == beam.turn),
            "the start follows the drone it is measured from"
        );
    }

    #[test]
    fn a_belt_stroke_starts_at_its_world_point_with_no_turn_or_grow() {
        let line = HueLine {
            centre: vec![[1.0, 2.0], [3.0, 4.0]],
            tint: 1,
        };
        let vertices = hue_vertices(&[line]);
        assert!(!vertices.is_empty());
        assert!(
            vertices
                .iter()
                .all(|v| v.origin == [0.0; 3] && v.grow == [0.0; 2] && v.turn == 0.0)
        );
    }

    #[test]
    #[should_panic(expected = "every instance must name a model kind")]
    fn an_instance_of_an_unknown_kind_panics() {
        let stray = Instance {
            offset: [0.0; 3],
            kind: 5,
            tint: 0,
            grow: [0.0; 2],
            turn: 0.0,
        };
        instance_data(&[stray], 2);
    }

    #[test]
    fn each_level_draws_its_own_mesh_edges_after_the_levels_before() {
        let tri = |z: f32| Mesh {
            positions: vec![[0.0, 0.0, z], [1.0, 0.0, z], [0.0, 1.0, z]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            surfaces: vec![Surface::Paper; 3],
            indices: vec![0, 1, 2],
        };
        let strokes = edge_vertices(&[tri(0.0), Mesh::default(), tri(2.0)]);
        assert_eq!(
            strokes.counts,
            [12, 12, 24],
            "three boundary creases a level"
        );
        assert!(strokes.vertices.iter().all(|v| v.crease == 1));
        assert!(strokes.vertices[12..].iter().all(|v| v.ends[0][2] == 2.0));
    }

    #[test]
    fn mesh_vertices_carry_the_flat_code_of_each_surface() {
        let mesh = Mesh {
            positions: vec![[0.0; 3]; 2],
            normals: vec![[0.0, 0.0, 1.0]; 2],
            surfaces: vec![Surface::Paper, Surface::Glass],
            indices: vec![0, 1, 1],
        };
        let vertices = mesh_vertices(&mesh);
        assert_eq!(vertices[0].code, 0);
        assert_ne!(vertices[1].code, 0);
    }
}
