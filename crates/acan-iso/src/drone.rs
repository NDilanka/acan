//! The agents' drone is a small white sphere that hovers over the machine it works on, with no
//! rotors. One big round lens on its nose looks at the machine, with a ring and an iris in the
//! agent's hue. Two small disc ears stand off its sides, and a short antenna rises from its crown.
//! A soft glow in the agent's hue lies under the body, and the renderer draws it, so the drone
//! reads as floating.
//!
//! A far drone is a circle with two discs and its eye: the seams and the lens glint draw at `Near`
//! and closer, and the rivets at `Close` only.
//!
//! The drone is two models. The body is white paper and takes no tint, so it stays white in every
//! state. The accent is the iris, the lens ring, the ear hubs and the antenna tip, and it takes the
//! agent's hue, which the renderer brightens or dims for the drone's state. Both share one centre at
//! the origin, so the renderer places the centre over the machine, turns the drone to face its
//! target, and grows it about the centre. Every model is built at heading zero, nose toward +x.

use std::f32::consts::TAU;

use crate::model::{Aabb, Detail, Dot3, Ink, Line3, Mesh, Model, Surface};

/// The drone's radius in plan units. Its body is 0.4 of a footprint 60 units wide, near the middle of
/// the kit's footprints, which run from 18 to 96 units across.
pub const BODY_RADIUS: f32 = 12.0;
/// The drone's full height, which is its body's diameter. The renderer's screen floor measures it.
pub const BOT_HEIGHT: f32 = 2.0 * BODY_RADIUS;
/// The eye sits this far from the centre, on the nose. A beam starts here, so it turns with the drone.
pub const EYE_DISTANCE: f32 = BODY_RADIUS;
/// The shadow's radius on the plinth top when the drone is low. It is a soft halo, so it reads as
/// light on the ground.
pub const SHADOW_RADIUS: f32 = 0.9 * BODY_RADIUS;
/// The glow under the body: a soft level disc in the agent's hue, this far below the centre and
/// this wide, which the renderer draws. The body hides its middle, so it shows round the belly.
pub const GLOW_DROP: f32 = 0.8 * BODY_RADIUS;
pub const GLOW_RADIUS: f32 = 1.6 * BODY_RADIUS;
/// The beacon's radius at its target. The renderer sets its on-screen size each frame.
pub const BEACON_RADIUS: f32 = 0.07;
/// The lens, as angles from the nose: the hue iris, the dark glass round it, and the hue ring that
/// edges the glass.
const IRIS_DEG: f32 = 14.0;
const LENS_DEG: f32 = 30.0;
const RING_DEG: f32 = 38.0;
/// The pupil at the middle of the iris, and the glint on the glass, as radii in plan units.
const PUPIL: f32 = 0.09 * BODY_RADIUS;
const GLINT: f32 = 0.06 * BODY_RADIUS;
/// An ear is a short drum on the side axis: its inner face is sunk in the body, its outer face stands
/// off the side, and a hue hub sits on the outer face.
const EAR_INNER: f32 = 0.78 * BODY_RADIUS;
const EAR_OUTER: f32 = 1.1 * BODY_RADIUS;
const EAR_RADIUS: f32 = 0.36 * BODY_RADIUS;
const EAR_HUB: f32 = 0.2 * BODY_RADIUS;
/// The hub is a short drum that stands this far off the ear's face. A disc laid on the face
/// z-fights it at a distance, since the mesh pass biases both by their slope.
const HUB_RISE: f32 = 0.03 * BODY_RADIUS;
const EAR_SEGMENTS: usize = 20;
/// The antenna: a thin stem from inside the crown, set back from the nose, and a hue tip.
const ANTENNA_BACK: f32 = -0.2 * BODY_RADIUS;
const ANTENNA_FOOT: f32 = 0.9 * BODY_RADIUS;
const ANTENNA_TOP: f32 = 1.42 * BODY_RADIUS;
const ANTENNA_STEM: f32 = 0.05 * BODY_RADIUS;
const ANTENNA_TIP: f32 = 0.11 * BODY_RADIUS;
const ANTENNA_SEGMENTS: usize = 6;
/// The rivets along the lower seam, and the arc of it they leave clear round the nose, in degrees.
const RIVETS: usize = 9;
const RIVET: f32 = 0.035 * BODY_RADIUS;
const RIVET_CLEAR_DEG: f32 = 60.0;
/// A seam that would cross the lens is cut where it passes this cosine from the nose, a little
/// outside the ring.
const NOSE_CLEAR: f32 = 0.74;
const BODY_SEGMENTS: usize = 24;
/// The body's bands from the lens to the tail, as (from, to, surface, steps) in degrees from the nose.
/// The iris and the ring are the accent's, so the body leaves them open. A latitude falls at 90
/// degrees, so the belly and the crown reach the full radius.
const BODY_BANDS: [(f32, f32, Surface, usize); 3] = [
    (IRIS_DEG, LENS_DEG, Surface::Ink(Ink::Hi), 2),
    (RING_DEG, 90.0, Surface::Paper, 6),
    (90.0, 180.0, Surface::Paper, 9),
];
/// The accent's bands on the body sphere. Their edges fall on the body's, so the two models meet
/// without a gap.
const ACCENT_BANDS: [(f32, f32, Surface, usize); 2] = [
    (0.0, IRIS_DEG, Surface::Paper, 2),
    (LENS_DEG, RING_DEG, Surface::Paper, 1),
];
const TIP_BANDS: [(f32, f32, Surface, usize); 1] = [(0.0, 180.0, Surface::Paper, 4)];
/// A dot or a disc sits this far proud of the surface under it, so it never z-fights the surface.
const PROUD: f32 = 0.004;
/// The stroke weight of a painted line, in the kit's weight.
const WEIGHT: f32 = 0.6;
const NOSE: V = [1.0, 0.0, 0.0];
const SIDE: V = [0.0, 1.0, 0.0];
const UP: V = [0.0, 0.0, 1.0];
const ORIGIN: V = [0.0, 0.0, 0.0];

type V = [f32; 3];

/// What the drone is made of, counted as it is built. The tests check these counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DroneParts {
    pub ears: usize,
    pub antennas: usize,
    pub eyes: usize,
    pub glints: usize,
    pub seams: usize,
    pub rivets: usize,
}

/// A drone's body: its white sphere, the dark glass of its lens, its ears, its antenna stem and its
/// painted seams and rivets. It takes no tint, so it is white in every state.
#[must_use]
pub fn agent_body() -> Model {
    body().0
}

/// A drone's accent: the iris and the ring of its lens, the hubs of its ears and the tip of its
/// antenna. It takes the agent's hue at the strength of the drone's state.
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

/// The drone's body parts, counted as its model is built.
#[must_use]
pub fn drone_parts() -> DroneParts {
    body().1
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

fn body() -> (Model, DroneParts) {
    let mut build = Build::default();
    sphere(&mut build, ORIGIN, BODY_RADIUS, &BODY_BANDS);
    for side in [1.0, -1.0] {
        ear(&mut build, side);
    }
    antenna(&mut build);
    eye(&mut build);
    seams(&mut build);
    rivets(&mut build);
    finish(build)
}

fn accent() -> Model {
    let mut build = Build::default();
    sphere(&mut build, ORIGIN, BODY_RADIUS, &ACCENT_BANDS);
    for side in [1.0_f32, -1.0] {
        drum(
            &mut build,
            scale3(SIDE, side * (EAR_OUTER - HUB_RISE)),
            scale3(SIDE, side * (EAR_OUTER + HUB_RISE)),
            EAR_HUB,
            EAR_SEGMENTS,
        );
    }
    sphere(
        &mut build,
        [ANTENNA_BACK, 0.0, ANTENNA_TOP],
        ANTENNA_TIP,
        &TIP_BANDS,
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

/// Two unit directions square to `axis` and to each other.
fn across(axis: V) -> (V, V) {
    let helper = if axis[2].abs() < 0.9 { UP } else { NOSE };
    let u = unit3(cross3(helper, axis));
    (u, cross3(axis, u))
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

/// A flat round disc facing `normal`, centred on `centre`.
fn disc(build: &mut Build, centre: V, normal: V, radius: f32, count: usize) {
    let (u, v) = across(normal);
    let inside = sub3(centre, normal);
    let middle = build.vertex(centre, normal, Surface::Paper);
    let rim: Vec<u32> = circle(centre, radius, u, v, count)
        .into_iter()
        .map(|p| build.vertex(p, normal, Surface::Paper))
        .collect();
    for j in 0..count {
        build.triangle(middle, rim[j], rim[(j + 1) % count], inside);
    }
}

/// A round drum from `a` to `b`, with a flat face at each end.
fn drum(build: &mut Build, a: V, b: V, radius: f32, count: usize) {
    let axis = unit3(sub3(b, a));
    let (u, v) = across(axis);
    let inside = scale3(add3(a, b), 0.5);
    let dirs = circle(ORIGIN, 1.0, u, v, count);
    let wall = |build: &mut Build, end: V| -> Vec<u32> {
        dirs.iter()
            .map(|&d| build.vertex(add3(end, scale3(d, radius)), d, Surface::Paper))
            .collect()
    };
    let low = wall(build, a);
    let high = wall(build, b);
    for j in 0..count {
        let k = (j + 1) % count;
        build.triangle(low[j], low[k], high[k], inside);
        build.triangle(low[j], high[k], high[j], inside);
    }
    disc(build, a, scale3(axis, -1.0), radius, count);
    disc(build, b, axis, radius, count);
}

/// An ear on the side `side` (1 or -1): a short drum whose inner face is sunk in the body.
fn ear(build: &mut Build, side: f32) {
    drum(
        build,
        scale3(SIDE, side * EAR_INNER),
        scale3(SIDE, side * EAR_OUTER),
        EAR_RADIUS,
        EAR_SEGMENTS,
    );
    build.parts.ears += 1;
}

/// The antenna's stem, from inside the crown to its tip, which is the accent's.
fn antenna(build: &mut Build) {
    drum(
        build,
        [ANTENNA_BACK, 0.0, ANTENNA_FOOT],
        [ANTENNA_BACK, 0.0, ANTENNA_TOP],
        ANTENNA_STEM,
        ANTENNA_SEGMENTS,
    );
    build.parts.antennas += 1;
}

/// The pupil at the middle of the iris, and the glint on the glass above and left of it.
fn eye(build: &mut Build) {
    build.dot(
        scale3(NOSE, BODY_RADIUS + PROUD),
        NOSE,
        PUPIL,
        Ink::Hi,
        Detail::Always,
    );
    build.parts.eyes += 1;
    let glint = sphere_dir(0.5 * (IRIS_DEG + LENS_DEG), 0.75 * TAU / 2.0);
    build.dot(
        scale3(glint, BODY_RADIUS + PROUD),
        glint,
        GLINT,
        Ink::Faint,
        Detail::Near,
    );
    build.parts.glints += 1;
}

/// The seam heights over the body, as the height of a unit direction. Rivets follow the lower one.
const UPPER_SEAM: f32 = 0.42;
const LOWER_SEAM: f32 = -0.5;

/// Two latitude seams, cut where they cross the lens, and one band over the crown behind the
/// antenna, from ear to ear.
fn seams(build: &mut Build) {
    const SAMPLES: usize = 56;
    for height in [UPPER_SEAM, LOWER_SEAM] {
        let r = (1.0 - height * height).sqrt();
        let dirs: Vec<V> = (0..SAMPLES)
            .map(|j| {
                let a = TAU * j as f32 / SAMPLES as f32;
                [r * a.cos(), r * a.sin(), height]
            })
            .collect();
        seam(build, &dirs);
    }
    let back = -0.35_f32;
    let r = (1.0 - back * back).sqrt();
    let crown: Vec<V> = (0..=SAMPLES / 2)
        .map(|j| {
            let a = (0.2 + 0.6 * j as f32 / (SAMPLES / 2) as f32) * TAU / 2.0;
            scale3([back, r * a.cos(), r * a.sin()], BODY_RADIUS)
        })
        .collect();
    build.line(crown, false, Ink::Mid, Detail::Near);
    build.parts.seams += 1;
}

/// One seam over the body, as unit directions from its centre. Where a sample lies within
/// `NOSE_CLEAR` of the nose it is dropped, and the rest draw as open runs or one closed loop.
fn seam(build: &mut Build, dirs: &[V]) {
    let kept: Vec<bool> = dirs.iter().map(|&d| dot3(d, NOSE) <= NOSE_CLEAR).collect();
    let point = |d: V| scale3(d, BODY_RADIUS);
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

/// Rivets along the lower seam, round the sides and the back, clear of the lens.
fn rivets(build: &mut Build) {
    let r = (1.0 - LOWER_SEAM * LOWER_SEAM).sqrt();
    for i in 0..RIVETS {
        let span = 360.0 - 2.0 * RIVET_CLEAR_DEG;
        let degrees = RIVET_CLEAR_DEG + span * (i as f32 + 0.5) / RIVETS as f32;
        let a = degrees.to_radians();
        let dir = [r * a.cos(), r * a.sin(), LOWER_SEAM];
        build.dot(
            scale3(dir, BODY_RADIUS + PROUD),
            dir,
            RIVET,
            Ink::Mid,
            Detail::Close,
        );
        build.parts.rivets += 1;
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
    fn the_body_is_centred_on_the_origin_with_its_radius_below_and_its_antenna_above() {
        let body = agent_body();
        let (lowest, highest) = mesh_heights(&body);
        assert!(
            (lowest + BODY_RADIUS).abs() < 1e-4,
            "the belly is one radius below the centre: {lowest}"
        );
        assert!(
            (highest - ANTENNA_TOP).abs() < 1e-4,
            "only the antenna rises past the crown: {highest}"
        );
        assert!(winds_outward(&body), "the solids face outward");
        assert!((BOT_HEIGHT - 2.0 * BODY_RADIUS).abs() < 1e-6);
    }

    #[test]
    fn the_ears_stand_off_the_sides_and_nothing_hangs_below_the_body() {
        let body = agent_body();
        assert!(
            (body.bounds.max[1] - EAR_OUTER).abs() < 1e-4
                && (body.bounds.min[1] + EAR_OUTER).abs() < 1e-4,
            "an ear's face stands off each side: {:?}",
            body.bounds
        );
        assert!(
            (body.bounds.max[0] - BODY_RADIUS - PROUD).abs() < 1e-4,
            "nothing stands out past the lens: {:?}",
            body.bounds
        );
        let (lowest, _) = mesh_heights(&body);
        assert!(lowest >= -BODY_RADIUS - 1e-4, "no rotor or claw hangs below");
    }

    #[test]
    fn the_drone_has_two_ears_an_antenna_one_eye_and_a_few_seams() {
        let parts = drone_parts();
        assert_eq!(parts.ears, 2);
        assert_eq!(parts.antennas, 1);
        assert_eq!(parts.eyes, 1);
        assert_eq!(parts.glints, 1);
        assert_eq!(parts.rivets, RIVETS);
        assert!(
            (3..=6).contains(&parts.seams),
            "a few seams, cut round the lens: {parts:?}"
        );
    }

    #[test]
    fn a_far_drone_draws_only_its_pupil_and_the_fine_art_waits_for_near_and_close() {
        let body = agent_body();
        let strokes = |model: &Model, level: Detail| {
            model.lines.iter().filter(|l| l.detail == level).count()
                + model.dots.iter().filter(|d| d.detail == level).count()
        };
        let parts = drone_parts();
        assert_eq!(strokes(&body, Detail::Always), parts.eyes, "the pupil only");
        assert_eq!(
            strokes(&body, Detail::Near),
            parts.seams + parts.glints,
            "the seams and the glint draw at Near"
        );
        assert_eq!(
            strokes(&body, Detail::Close),
            parts.rivets,
            "the rivets draw at Close"
        );
        let accent = agent_accent();
        assert!(
            accent.lines.is_empty() && accent.dots.is_empty(),
            "the accent is solid"
        );
    }

    #[test]
    fn the_body_takes_no_hue_and_its_lens_glass_is_dark() {
        let body = agent_body();
        let surfaces = &body.meshes[0].surfaces;
        assert!(
            surfaces
                .iter()
                .all(|s| matches!(s, Surface::Paper | Surface::Ink(Ink::Hi))),
            "the body is white paper and dark glass"
        );
        assert!(surfaces.contains(&Surface::Ink(Ink::Hi)), "the lens glass");
    }

    #[test]
    fn the_accent_is_the_iris_the_ring_the_ear_hubs_and_the_antenna_tip() {
        let accent = agent_accent();
        let positions = &accent.meshes[0].positions;
        assert!(
            (accent.bounds.max[0] - BODY_RADIUS).abs() < 1e-4,
            "the iris reaches the nose radius: {:?}",
            accent.bounds
        );
        assert!(
            (accent.bounds.max[2] - (ANTENNA_TOP + ANTENNA_TIP)).abs() < 1e-4,
            "the tip stands on the antenna: {:?}",
            accent.bounds
        );
        for side in [1.0_f32, -1.0] {
            assert!(
                positions
                    .iter()
                    .any(|p| (p[1] - side * (EAR_OUTER + HUB_RISE)).abs() < 1e-4),
                "a hub on the ear at side {side}"
            );
        }
        let ring_edge = (RING_DEG.to_radians().cos() * BODY_RADIUS, 1e-3);
        assert!(
            positions.iter().any(|p| (p[0] - ring_edge.0).abs() < ring_edge.1),
            "the ring edges the lens"
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
