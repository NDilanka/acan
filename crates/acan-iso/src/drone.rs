//! The agents' drone is a small construction drone that hovers over the machine it works on, drawn as
//! line art. Its shell and its two ducted-fan nacelles carry a light tint of the agent's hue under dark
//! painted lines. Its glass eye and the lamp on its crown carry the full hue. Two struts hold the
//! nacelles to the body, a swept fin sits behind, and a grille sits on the back.
//!
//! The fine line art is held back until the drone is large on screen, so a far drone reads as a
//! dot with ears: the blades, the grille and the vents draw at `Close` only, and the seams, the
//! struts and the hubs at `Near` and closer.
//!
//! The drone is three models. The shell and the accent share one centre at the origin, so the renderer
//! places the centre over the machine, turns the drone to face its target, and grows it about the
//! centre. The shadow is a third model, one dot the renderer sets on the plinth top under the centre.
//! Every model is built at heading zero, nose toward +x.

use std::f32::consts::{PI, TAU};

use crate::model::{Aabb, Detail, Dot3, Ink, Line3, Mesh, Model, Surface};

/// The drone's radius in plan units. Its body is 0.4 of a footprint 60 units wide, near the middle of
/// the kit's footprints, which run from 18 to 96 units across.
pub const BODY_RADIUS: f32 = 12.0;
/// The design draws the body at radius 9, and every other measure is its design value times this.
const SCALE: f32 = BODY_RADIUS / 9.0;
/// The drone's full height, which is its body's diameter. The renderer's screen floor measures it.
pub const BOT_HEIGHT: f32 = 2.0 * BODY_RADIUS;
/// The eye sits this far from the centre, on the nose. A beam starts here, so it turns with the drone.
pub const EYE_DISTANCE: f32 = BODY_RADIUS;
/// The shadow's radius on the plinth top. It is a soft halo, so it reads as light on the ground.
pub const SHADOW_RADIUS: f32 = 0.9 * BODY_RADIUS;
/// The beacon's radius at its target. The renderer sets its on-screen size each frame.
pub const BEACON_RADIUS: f32 = 0.07;
const NACELLE_SIDE: f32 = (9.0 + 3.4) * SCALE;
const NACELLE_DROP: f32 = -1.5 * SCALE;
const NACELLE_RADIUS: f32 = 4.4 * SCALE;
const NACELLE_HALF: f32 = 3.2 * SCALE;
const NACELLE_SEGMENTS: usize = 32;
const BLADES: usize = 14;
/// The glass cap and the bezel ring of the eye, and the inner ring, as angles from the nose.
const GLASS_DEG: f32 = 31.0;
const BEZEL_DEG: f32 = 40.0;
const INNER_DEG: f32 = 20.0;
/// The eye dot's radius, which the pupil draws at.
const EYE_DOT: f32 = 1.6 * SCALE;
/// The lamp on the crown is a small sphere whose centre sits on the body's top. It is half the
/// eye dot's size, so the saturated lamp reads as a dot rather than a blob.
const LAMP_RADIUS: f32 = 0.5 * EYE_DOT;
/// A grille dot's radius: a third of the grille's first size, so the grille reads as fine dots.
const GRILLE_DOT: f32 = 0.13 * SCALE;
/// A seam or a band that would cross the eye is cut where it passes this cosine from the nose.
const NOSE_CLEAR: f32 = 0.72;
/// Half the thickness of the fin, each side of its plane.
const FIN_HALF: f32 = 0.03;
/// The fin's outline, counter-clockwise in its plane, as (nose, up) in units of the body radius.
const FIN: [(f32, f32); 4] = [(-0.7, 0.2), (-1.4, 0.5), (-1.5, 0.2), (-0.5, -0.2)];
const BODY_SEGMENTS: usize = 24;
/// The shell's bands from the bezel to the tail, as (from, to, surface, steps) in degrees from the nose.
const SHELL_BANDS: [(f32, f32, Surface, usize); 2] = [
    (GLASS_DEG, BEZEL_DEG, Surface::Ink(Ink::Mid), 1),
    (BEZEL_DEG, 180.0, Surface::Paper, 14),
];
/// The glass cap. Its edge falls on the bezel's edge, so the two models meet without a gap.
const GLASS_BANDS: [(f32, f32, Surface, usize); 1] = [(0.0, GLASS_DEG, Surface::Paper, 3)];
const LAMP_BANDS: [(f32, f32, Surface, usize); 1] = [(0.0, 180.0, Surface::Paper, 4)];
/// A dot or a line sits this far proud of the surface under it, so it never z-fights the surface.
const PROUD: f32 = 0.004;
/// The stroke weight of a painted line, in the kit's weight.
const WEIGHT: f32 = 0.6;
const NOSE: V = [1.0, 0.0, 0.0];
const SIDE: V = [0.0, 1.0, 0.0];
const UP: V = [0.0, 0.0, 1.0];
const ORIGIN: V = [0.0, 0.0, 0.0];

type V = [f32; 3];

/// What the drone's shell is made of, counted as it is built. The tests check these counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DroneParts {
    pub nacelles: usize,
    pub blades: usize,
    pub hubs: usize,
    pub fins: usize,
    pub eyes: usize,
    pub vents: usize,
    pub grille: usize,
    pub seams: usize,
}

/// A drone's shell: its body, nacelles, fin, grille and painted lines. It takes the agent's light
/// tint, and its line art stays dark.
#[must_use]
pub fn agent_body() -> Model {
    shell().0
}

/// A drone's accent: the glass of its eye and the lamp on its crown. It takes the agent's full hue.
#[must_use]
pub fn agent_accent() -> Model {
    accent()
}

/// A drone's shadow: one soft dot on the plinth top, centred on the origin, which the renderer sets
/// under the drone's centre.
#[must_use]
pub fn agent_shadow() -> Model {
    let mut build = Build::default();
    build.dot(ORIGIN, UP, SHADOW_RADIUS, Ink::Shadow, Detail::Always);
    finish(build).0
}

/// The drone's shell parts, counted as its model is built.
#[must_use]
pub fn drone_parts() -> DroneParts {
    shell().1
}

/// The marker at a drone's target on a machine is a small sphere, centred on its origin, which the
/// renderer grows to its screen size and tints with the agent's hue.
#[must_use]
pub fn beacon() -> Model {
    let mut build = Build::default();
    sphere(
        &mut build,
        ORIGIN,
        BEACON_RADIUS,
        &[(0.0, 180.0, Surface::Paper, 4)],
    );
    finish(build).0
}

fn shell() -> (Model, DroneParts) {
    let mut build = Build::default();
    sphere(&mut build, ORIGIN, BODY_RADIUS, &SHELL_BANDS);
    nacelle(&mut build, 1.0);
    nacelle(&mut build, -1.0);
    fin(&mut build);
    eye(&mut build);
    grille(&mut build);
    seams(&mut build);
    finish(build)
}

fn accent() -> Model {
    let mut build = Build::default();
    sphere(&mut build, ORIGIN, BODY_RADIUS, &GLASS_BANDS);
    sphere(
        &mut build,
        [0.0, 0.0, BODY_RADIUS],
        LAMP_RADIUS,
        &LAMP_BANDS,
    );
    finish(build).0
}

/// The painted and solid parts of a drone or beacon as they are built.
#[derive(Default)]
struct Build {
    mesh: Mesh,
    lines: Vec<Line3>,
    dots: Vec<Dot3>,
    parts: DroneParts,
}

impl Build {
    fn vertex(&mut self, position: V, normal: V, surface: Surface) -> u32 {
        let index = u32::try_from(self.mesh.positions.len())
            .expect("the drone has fewer than 2^32 vertices");
        self.mesh.positions.push(position);
        self.mesh.normals.push(normal);
        self.mesh.surfaces.push(surface);
        index
    }

    /// A triangle that faces away from `inside`, whichever order its corners come in.
    fn triangle(&mut self, a: u32, b: u32, c: u32, inside: V) {
        let [pa, pb, pc] = [a, b, c].map(|i| self.mesh.positions[i as usize]);
        let centroid = scale3(add3(add3(pa, pb), pc), 1.0 / 3.0);
        let facing = dot3(cross3(sub3(pb, pa), sub3(pc, pa)), sub3(centroid, inside));
        if facing >= 0.0 {
            self.mesh.indices.extend([a, b, c]);
        } else {
            self.mesh.indices.extend([a, c, b]);
        }
    }

    fn line(&mut self, points: Vec<V>, closed: bool, tone: Ink, detail: Detail) {
        self.lines.push(Line3 {
            points,
            closed,
            tone,
            width: WEIGHT,
            detail,
        });
    }

    fn dot(&mut self, center: V, normal: V, radius: f32, tone: Ink, detail: Detail) {
        self.dots.push(Dot3 {
            center,
            normal,
            radius,
            tone,
            detail,
        });
    }
}

fn finish(build: Build) -> (Model, DroneParts) {
    let mut bounds = Aabb::EMPTY;
    for &p in &build.mesh.positions {
        bounds.include(p);
    }
    for line in &build.lines {
        for &p in &line.points {
            bounds.include(p);
        }
    }
    for dot in &build.dots {
        bounds.include(dot.center);
    }
    let model = Model {
        meshes: [build.mesh, Mesh::default(), Mesh::default()],
        lines: build.lines,
        dots: build.dots,
        bounds,
    };
    (model, build.parts)
}

/// A point `nose`, `side` and `up` from the drone's centre, in the frame of heading zero.
fn at(nose: f32, side: f32, up: f32) -> V {
    [nose, side, up]
}

/// A unit direction from the body's centre, `degrees` from the nose and `phi` round the nose axis.
fn sphere_dir(degrees: f32, phi: f32) -> V {
    let theta = degrees.to_radians();
    let ring = add3(scale3(SIDE, phi.cos()), scale3(UP, phi.sin()));
    add3(scale3(NOSE, theta.cos()), scale3(ring, theta.sin()))
}

/// The ring of points about `centre`, `count` of them, in the plane of `u` and `v`.
fn circle(centre: V, radius: f32, u: V, v: V, count: usize) -> Vec<V> {
    (0..count)
        .map(|j| {
            let a = TAU * j as f32 / count as f32;
            let offset = add3(scale3(u, a.cos()), scale3(v, a.sin()));
            add3(centre, scale3(offset, radius))
        })
        .collect()
}

/// A latitude of a sphere, or one of its poles.
enum Rim {
    Pole(u32),
    Ring(Vec<u32>),
}

fn rim(build: &mut Build, centre: V, radius: f32, degrees: f32, surface: Surface) -> Rim {
    if degrees <= 0.0 || degrees >= 180.0 {
        let dir = if degrees <= 0.0 {
            NOSE
        } else {
            scale3(NOSE, -1.0)
        };
        let index = build.vertex(add3(centre, scale3(dir, radius)), dir, surface);
        return Rim::Pole(index);
    }
    let ring = (0..BODY_SEGMENTS)
        .map(|j| {
            let dir = sphere_dir(degrees, TAU * j as f32 / BODY_SEGMENTS as f32);
            build.vertex(add3(centre, scale3(dir, radius)), dir, surface)
        })
        .collect();
    Rim::Ring(ring)
}

fn join(build: &mut Build, lower: &Rim, upper: &Rim, inside: V) {
    let steps = (0..BODY_SEGMENTS).map(|j| (j, (j + 1) % BODY_SEGMENTS));
    match (lower, upper) {
        (Rim::Pole(p), Rim::Ring(r)) => {
            for (j, k) in steps {
                build.triangle(*p, r[j], r[k], inside);
            }
        }
        (Rim::Ring(r), Rim::Pole(p)) => {
            for (j, k) in steps {
                build.triangle(r[j], *p, r[k], inside);
            }
        }
        (Rim::Ring(a), Rim::Ring(b)) => {
            for (j, k) in steps {
                build.triangle(a[j], a[k], b[k], inside);
                build.triangle(a[j], b[k], b[j], inside);
            }
        }
        (Rim::Pole(_), Rim::Pole(_)) => {}
    }
}

/// A sphere cut into bands of latitude, each `(from, to, surface, steps)` in degrees from the nose.
fn sphere(build: &mut Build, centre: V, radius: f32, bands: &[(f32, f32, Surface, usize)]) {
    for &(from, to, surface, steps) in bands {
        let mut lower = rim(build, centre, radius, from, surface);
        for step in 1..=steps {
            let degrees = from + (to - from) * step as f32 / steps as f32;
            let upper = rim(build, centre, radius, degrees, surface);
            join(build, &lower, &upper, centre);
            lower = upper;
        }
    }
}

/// A round tube along the nose axis, from `a` to `b`, with flat ends.
fn tube(build: &mut Build, a: V, b: V, radius: f32) {
    let inside = scale3(add3(a, b), 0.5);
    let dirs = circle([0.0; 3], 1.0, SIDE, UP, NACELLE_SEGMENTS);
    let wall = |build: &mut Build, end: V| -> Vec<u32> {
        dirs.iter()
            .map(|&d| build.vertex(add3(end, scale3(d, radius)), d, Surface::Paper))
            .collect()
    };
    let cap = |build: &mut Build, end: V, normal: V| -> Vec<u32> {
        dirs.iter()
            .map(|&d| build.vertex(add3(end, scale3(d, radius)), normal, Surface::Paper))
            .collect()
    };
    let low = wall(build, a);
    let high = wall(build, b);
    let steps = (0..NACELLE_SEGMENTS).map(|j| (j, (j + 1) % NACELLE_SEGMENTS));
    for (j, k) in steps.clone() {
        build.triangle(low[j], low[k], high[k], inside);
        build.triangle(low[j], high[k], high[j], inside);
    }
    let rim_low = cap(build, a, scale3(NOSE, -1.0));
    let rim_high = cap(build, b, NOSE);
    let centre_low = build.vertex(a, scale3(NOSE, -1.0), Surface::Paper);
    let centre_high = build.vertex(b, NOSE, Surface::Paper);
    for (j, k) in steps {
        build.triangle(centre_low, rim_low[j], rim_low[k], inside);
        build.triangle(centre_high, rim_high[k], rim_high[j], inside);
    }
}

/// A ducted fan at the side, built from its tube, the duct rings on both faces, the blades and hub on the
/// front face, and the strut to the body.
fn nacelle(build: &mut Build, side: f32) {
    let centre = at(0.0, side * NACELLE_SIDE, NACELLE_DROP);
    let front = add3(centre, scale3(NOSE, NACELLE_HALF));
    let back = add3(centre, scale3(NOSE, -NACELLE_HALF));
    tube(build, back, front, NACELLE_RADIUS);
    build.line(
        circle(front, NACELLE_RADIUS, SIDE, UP, 24),
        true,
        Ink::Hi,
        Detail::Always,
    );
    build.line(
        circle(back, 3.0 * SCALE, SIDE, UP, 24),
        true,
        Ink::Lo,
        Detail::Always,
    );
    build.line(
        circle(back, 1.6 * SCALE, SIDE, UP, 20),
        true,
        Ink::Lo,
        Detail::Always,
    );
    for i in 0..BLADES {
        let a = TAU * i as f32 / BLADES as f32;
        let inner = add3(front, scale3(sweep(a), 1.3 * SCALE));
        let outer = add3(front, scale3(sweep(a + 0.5), 3.9 * SCALE));
        build.line(vec![inner, outer], false, Ink::Mid, Detail::Close);
    }
    build.parts.blades += BLADES;
    build.dot(
        add3(front, scale3(NOSE, 0.4 * SCALE)),
        NOSE,
        1.3 * SCALE,
        Ink::Mid,
        Detail::Near,
    );
    build.parts.hubs += 1;
    let strut_root = at(0.0, side * 0.8 * BODY_RADIUS, 0.0);
    let strut_end = add3(centre, scale3(SIDE, -side * NACELLE_RADIUS * 0.6));
    build.line(vec![strut_root, strut_end], false, Ink::Mid, Detail::Near);
    build.parts.nacelles += 1;
}

/// A unit direction in the plane of the front face of a nacelle, `a` radians round from the side.
fn sweep(a: f32) -> V {
    add3(scale3(SIDE, a.cos()), scale3(UP, a.sin()))
}

/// The swept fin behind the body is a thin prism whose outline lies in the plane of the nose and up.
fn fin(build: &mut Build) {
    let lo: Vec<V> = FIN
        .iter()
        .map(|&(f, u)| at(f * BODY_RADIUS, -FIN_HALF, u * BODY_RADIUS))
        .collect();
    let hi: Vec<V> = FIN
        .iter()
        .map(|&(f, u)| at(f * BODY_RADIUS, FIN_HALF, u * BODY_RADIUS))
        .collect();
    let inside = scale3(
        lo.iter().chain(&hi).fold([0.0; 3], |sum, &p| add3(sum, p)),
        1.0 / (2 * FIN.len()) as f32,
    );
    let lo_ids: Vec<u32> = lo
        .iter()
        .map(|&p| build.vertex(p, scale3(SIDE, -1.0), Surface::Paper))
        .collect();
    let hi_ids: Vec<u32> = hi
        .iter()
        .map(|&p| build.vertex(p, SIDE, Surface::Paper))
        .collect();
    for pair in lo_ids[1..].windows(2) {
        build.triangle(lo_ids[0], pair[0], pair[1], inside);
    }
    for pair in hi_ids[1..].windows(2) {
        build.triangle(hi_ids[0], pair[0], pair[1], inside);
    }
    let sides = (0..FIN.len()).map(|i| (i, (i + 1) % FIN.len()));
    for (i, j) in sides {
        let edge = sub3(lo[j], lo[i]);
        let normal = unit3(add3(scale3(NOSE, edge[2]), scale3(UP, -edge[0])));
        let corners = [lo[i], lo[j], hi[j], hi[i]].map(|p| build.vertex(p, normal, Surface::Paper));
        build.triangle(corners[0], corners[1], corners[2], inside);
        build.triangle(corners[0], corners[2], corners[3], inside);
    }
    build.parts.fins += 1;
}

/// The eye on the nose. The glass is the accent model's cap, so here are the rings round it, the two
/// vents above and below, and the eye dot at the nose's pole.
fn eye(build: &mut Build) {
    let centre = at(0.0, 0.0, 0.0);
    for (degrees, tone) in [(INNER_DEG, Ink::Lo), (BEZEL_DEG, Ink::Hi)] {
        let ring = (0..BODY_SEGMENTS)
            .map(|j| {
                let dir = sphere_dir(degrees, TAU * j as f32 / BODY_SEGMENTS as f32);
                add3(centre, scale3(dir, BODY_RADIUS))
            })
            .collect();
        build.line(ring, true, tone, Detail::Always);
    }
    for lift in [0.52_f32, -0.52] {
        let vent: Vec<V> = (0..5)
            .map(|i| {
                let u = -0.22 + 0.44 * i as f32 / 4.0;
                let dir = unit3(add3(
                    add3(scale3(NOSE, 0.8), scale3(SIDE, u)),
                    scale3(UP, lift),
                ));
                add3(centre, scale3(dir, BODY_RADIUS))
            })
            .collect();
        build.line(vent, false, Ink::Mid, Detail::Close);
        build.parts.vents += 1;
    }
    build.dot(
        add3(centre, scale3(NOSE, BODY_RADIUS + PROUD)),
        NOSE,
        EYE_DOT,
        Ink::Hi,
        Detail::Always,
    );
    build.parts.eyes += 1;
}

/// A small dotted grille on the back of the upper shell.
fn grille(build: &mut Build) {
    for i in -3..=3 {
        for lat in [0.62_f32, 0.7] {
            let ring = (1.0 - lat * lat).sqrt();
            let az = PI + i as f32 * 0.13;
            let dir = add3(
                add3(scale3(NOSE, az.cos() * ring), scale3(SIDE, az.sin() * ring)),
                scale3(UP, lat),
            );
            let centre = add3(at(0.0, 0.0, 0.0), scale3(dir, BODY_RADIUS + PROUD));
            build.dot(centre, dir, GRILLE_DOT, Ink::Lo, Detail::Close);
            build.parts.grille += 1;
        }
    }
}

/// Two latitude seams and two meridians, cut where they cross the eye.
fn seams(build: &mut Build) {
    const SAMPLES: usize = 56;
    let centre = at(0.0, 0.0, 0.0);
    for height in [0.38_f32, -0.42] {
        let r = (1.0 - height * height).sqrt();
        let dirs: Vec<V> = (0..SAMPLES)
            .map(|j| {
                let a = TAU * j as f32 / SAMPLES as f32;
                [r * a.cos(), r * a.sin(), height]
            })
            .collect();
        seam(build, centre, &dirs);
    }
    for sign in [1.0_f32, -1.0] {
        let normal = unit3(add3(NOSE, scale3(SIDE, 0.9 * sign)));
        let u1 = unit3(cross3(UP, normal));
        let u2 = cross3(normal, u1);
        let dirs: Vec<V> = (0..SAMPLES)
            .map(|j| {
                let a = TAU * j as f32 / SAMPLES as f32;
                add3(scale3(u1, a.cos()), scale3(u2, a.sin()))
            })
            .collect();
        seam(build, centre, &dirs);
    }
}

/// One seam over the body, as unit directions from its centre. Where a sample lies within
/// `NOSE_CLEAR` of the nose it is dropped, and the rest draw as open runs or one closed loop.
fn seam(build: &mut Build, centre: V, dirs: &[V]) {
    let kept: Vec<bool> = dirs.iter().map(|&d| dot3(d, NOSE) <= NOSE_CLEAR).collect();
    let point = |d: V| add3(centre, scale3(d, BODY_RADIUS));
    if kept.iter().all(|&k| k) {
        build.line(
            dirs.iter().map(|&d| point(d)).collect(),
            true,
            Ink::Mid,
            Detail::Near,
        );
        build.parts.seams += 1;
        return;
    }
    let Some(start) = kept.iter().position(|&k| !k) else {
        return;
    };
    let mut run: Vec<V> = Vec::new();
    for step in 1..=dirs.len() {
        let index = (start + step) % dirs.len();
        if kept[index] {
            run.push(point(dirs[index]));
            continue;
        }
        let finished = std::mem::take(&mut run);
        if finished.len() > 1 {
            build.line(finished, false, Ink::Mid, Detail::Near);
            build.parts.seams += 1;
        }
    }
}

fn add3(a: V, b: V) -> V {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub3(a: V, b: V) -> V {
    add3(a, scale3(b, -1.0))
}

fn scale3(a: V, k: f32) -> V {
    a.map(|v| v * k)
}

fn dot3(a: V, b: V) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross3(a: V, b: V) -> V {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn unit3(a: V) -> V {
    scale3(a, 1.0 / dot3(a, a).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marks::winds_outward;

    fn mesh_heights(model: &Model) -> (f32, f32) {
        model.meshes[0]
            .positions
            .iter()
            .map(|p| p[2])
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), z| {
                (lo.min(z), hi.max(z))
            })
    }

    #[test]
    fn the_shell_is_centred_on_the_origin_with_its_radius_above_and_below() {
        let shell = agent_body();
        let (lowest, highest) = mesh_heights(&shell);
        assert!(
            (highest - BODY_RADIUS).abs() < 1e-4,
            "the crown is one radius above the centre: {highest}"
        );
        assert!(
            (lowest + BODY_RADIUS).abs() < 1e-4,
            "the belly is one radius below the centre: {lowest}"
        );
        assert!(winds_outward(&shell), "the solids face outward");
    }

    #[test]
    fn the_nacelles_hang_inside_the_body_height() {
        let (lowest, _) = mesh_heights(&agent_body());
        assert!(
            lowest >= -BODY_RADIUS - 1e-4,
            "nothing hangs below the body"
        );
        assert!((BOT_HEIGHT - 2.0 * BODY_RADIUS).abs() < 1e-6);
    }

    #[test]
    fn the_drone_has_two_nacelles_fourteen_blades_each_and_an_eye_on_the_nose() {
        let parts = drone_parts();
        assert_eq!(parts.nacelles, 2);
        assert_eq!(parts.blades, 2 * BLADES);
        assert_eq!(parts.hubs, 2);
        assert_eq!(parts.fins, 1);
        assert_eq!(parts.eyes, 1);
        assert_eq!(parts.vents, 2);
        assert_eq!(parts.grille, 14);
        assert!(parts.seams >= 2, "the latitude seams draw: {parts:?}");
    }

    #[test]
    fn a_far_drone_draws_only_its_rings_and_eye_and_the_fine_art_waits_for_close() {
        let shell = agent_body();
        let strokes = |level: Detail| {
            shell.lines.iter().filter(|l| l.detail == level).count()
                + shell.dots.iter().filter(|d| d.detail == level).count()
        };
        assert!(
            strokes(Detail::Always) <= 10,
            "a far drone keeps about eight strokes: {}",
            strokes(Detail::Always)
        );
        let parts = drone_parts();
        assert_eq!(
            strokes(Detail::Close),
            parts.blades + parts.vents + parts.grille,
            "the blades, vents and grille draw at Close"
        );
        assert_eq!(
            strokes(Detail::Near),
            parts.seams + parts.hubs + parts.nacelles,
            "the seams, hubs and struts draw at Near"
        );
    }

    #[test]
    fn the_accent_is_the_glass_on_the_nose_and_the_lamp_on_the_crown() {
        let accent = agent_accent();
        assert!(
            (accent.bounds.max[0] - BODY_RADIUS).abs() < 1e-4,
            "the glass reaches the nose radius: {:?}",
            accent.bounds
        );
        assert!(
            (accent.bounds.max[2] - (BODY_RADIUS + LAMP_RADIUS)).abs() < 1e-4,
            "the lamp stands on the crown: {:?}",
            accent.bounds
        );
        assert!(winds_outward(&accent), "the accent faces outward");
    }

    #[test]
    fn the_shadow_is_one_soft_dot_on_the_origin() {
        let shadow = agent_shadow();
        assert_eq!(shadow.dots.len(), 1, "one shadow dot");
        let dot = &shadow.dots[0];
        assert_eq!(dot.tone, Ink::Shadow);
        assert_eq!(dot.center, ORIGIN, "the shadow is centred on the origin");
        assert!((dot.normal[2] - 1.0).abs() < 1e-6, "the disc lies flat");
        assert!((dot.radius - SHADOW_RADIUS).abs() < 1e-6);
    }

    #[test]
    fn the_beacon_is_centred_on_its_origin() {
        let beacon = beacon();
        let centre = beacon.bounds.centre();
        assert!(centre.iter().all(|v| v.abs() < 1e-4));
        assert!((beacon.bounds.max[2] - BEACON_RADIUS).abs() < 1e-4);
        assert!(winds_outward(&beacon), "the beacon faces outward");
    }
}
