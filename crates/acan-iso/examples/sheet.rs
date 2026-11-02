//! Writes a standalone HTML review sheet of the 3D models: each model at four azimuths, elevation 30 degrees,
//! orthographic. Faces are culled to the camera and painter-sorted, shaded by the shading rule. Lines and dots are
//! drawn where no nearer face hides them. Prints the triangle and item counts of each model per detail level.
//!
//! Usage: `cargo run -p acan-iso --example sheet -- <out.html>`

use std::fmt::Write as _;
use std::process::ExitCode;

use acan_iso::{Dot3, Ink, Kind, Line3, Model, Surface, Tone, model, tone_for};

/// Pixels per plan unit in the sheet.
const SCALE: f32 = 3.0;
/// Margin around each model, in plan units.
const PAD: f32 = 4.0;
/// Elevation of the camera above the plan, in degrees.
const ELEVATION: f32 = 30.0;
/// Camera azimuths drawn, in degrees.
const AZIMUTHS: [f32; 4] = [45.0, 135.0, 225.0, 315.0];
/// Longest piece of a line checked for occlusion, in plan units.
const PIECE: f32 = 0.5;
/// How much nearer a face must be than a line or dot to hide it, in plan units.
const DEPTH_EPS: f32 = 0.05;
/// Slack on barycentric and box tests, so that points on shared edges are not missed.
const EDGE: f32 = 1e-4;

const KINDS: [Kind; 7] = [
    Kind::Assembler,
    Kind::Crate,
    Kind::Intake,
    Kind::Gate,
    Kind::Splitter,
    Kind::Tank,
    Kind::Valve,
];

type Vec2 = [f32; 2];
type Vec3 = [f32; 3];

fn main() -> ExitCode {
    let Some(out) = std::env::args().nth(1) else {
        eprintln!("usage: sheet <out.html>");
        return ExitCode::FAILURE;
    };
    let mut cards = String::new();
    println!(
        "{:<10} {:>8} {:>8} {:>8} {:>8} {:>7} {:>6}",
        "kind", "always", "near", "close", "total", "lines", "dots"
    );
    for kind in KINDS {
        let built = model(kind);
        let triangles: Vec<usize> = built
            .meshes
            .iter()
            .map(|mesh| mesh.indices.len() / 3)
            .collect();
        println!(
            "{:<10} {:>8} {:>8} {:>8} {:>8} {:>7} {:>6}",
            format!("{kind:?}"),
            triangles[0],
            triangles[1],
            triangles[2],
            triangles.iter().sum::<usize>(),
            built.lines.len(),
            built.dots.len(),
        );
        for azimuth in AZIMUTHS {
            let view = View::new(azimuth);
            let (attrs, body) = render(&built, &view);
            let _ = writeln!(
                cards,
                "<figure><svg xmlns=\"http://www.w3.org/2000/svg\" {attrs}>{body}</svg><figcaption>{kind:?} &middot; az {azimuth:.0} &middot; el {ELEVATION:.0}</figcaption></figure>",
            );
        }
    }
    if let Err(error) = std::fs::write(&out, page(&cards)) {
        eprintln!("cannot write {out}: {error}");
        return ExitCode::FAILURE;
    }
    println!("wrote {out}");
    ExitCode::SUCCESS
}

/// The camera: orthographic, looking at the origin from `ELEVATION` degrees up at `azimuth`.
#[derive(Debug, Clone, Copy)]
struct View {
    azimuth: f32,
    /// Unit vector from the model towards the viewer.
    toward: Vec3,
    right: Vec3,
    up: Vec3,
}

impl View {
    fn new(azimuth_degrees: f32) -> Self {
        let azimuth = azimuth_degrees.to_radians();
        let elevation = ELEVATION.to_radians();
        let (sin_a, cos_a) = azimuth.sin_cos();
        let (sin_e, cos_e) = elevation.sin_cos();
        Self {
            azimuth,
            toward: [cos_a * cos_e, sin_a * cos_e, sin_e],
            right: [-sin_a, cos_a, 0.0],
            up: [-sin_e * cos_a, -sin_e * sin_a, cos_e],
        }
    }

    /// Screen position in plan units, with y pointing down the page.
    fn screen(&self, p: Vec3) -> Vec2 {
        [inner(p, self.right), -inner(p, self.up)]
    }

    /// Distance along the view direction. Larger is nearer the viewer.
    fn depth(&self, p: Vec3) -> f32 {
        inner(p, self.toward)
    }
}

/// A front-facing triangle, projected for painting and for occlusion tests.
struct Face {
    screen: [Vec2; 3],
    depth: [f32; 3],
    /// Depth of the centroid, which orders the painting.
    centre: f32,
    lo: Vec2,
    hi: Vec2,
    fill: &'static str,
}

impl Face {
    fn new(p: [Vec3; 3], fill: &'static str, view: &View) -> Self {
        let screen = p.map(|q| view.screen(q));
        let depth = p.map(|q| view.depth(q));
        let xs = screen.map(|s| s[0]);
        let ys = screen.map(|s| s[1]);
        Self {
            screen,
            depth,
            centre: depth.iter().sum::<f32>() / 3.0,
            lo: [
                xs.iter().copied().fold(f32::INFINITY, f32::min),
                ys.iter().copied().fold(f32::INFINITY, f32::min),
            ],
            hi: [
                xs.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                ys.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            ],
            fill,
        }
    }

    /// The depth of this triangle above screen point `s`, when `s` lies on it.
    fn depth_at(&self, s: Vec2) -> Option<f32> {
        if s[0] < self.lo[0] - EDGE
            || s[0] > self.hi[0] + EDGE
            || s[1] < self.lo[1] - EDGE
            || s[1] > self.hi[1] + EDGE
        {
            return None;
        }
        let [a, b, c] = self.screen;
        let det = (b[1] - c[1]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[1] - c[1]);
        if det.abs() < 1e-12 {
            return None;
        }
        let w0 = ((b[1] - c[1]) * (s[0] - c[0]) + (c[0] - b[0]) * (s[1] - c[1])) / det;
        let w1 = ((c[1] - a[1]) * (s[0] - c[0]) + (a[0] - c[0]) * (s[1] - c[1])) / det;
        let w2 = 1.0 - w0 - w1;
        if [w0, w1, w2].iter().any(|&w| w < -EDGE) {
            return None;
        }
        Some(w0 * self.depth[0] + w1 * self.depth[1] + w2 * self.depth[2])
    }
}

/// Returns the SVG root attributes and the body of one view of `built`.
fn render(built: &Model, view: &View) -> (String, String) {
    let faces = front_faces(built, view);
    let (lo, hi) = screen_box(built, view);
    let (w, h) = (hi[0] - lo[0] + 2.0 * PAD, hi[1] - lo[1] + 2.0 * PAD);
    let attrs = format!(
        "viewBox=\"{:.3} {:.3} {:.3} {:.3}\" width=\"{:.0}\" height=\"{:.0}\"",
        lo[0] - PAD,
        lo[1] - PAD,
        w,
        h,
        w * SCALE,
        h * SCALE,
    );
    let mut body = String::new();
    for face in &faces {
        let _ = writeln!(
            body,
            "<path d=\"{}Z\" fill=\"{f}\" stroke=\"{f}\" stroke-width=\"0.25\" stroke-linejoin=\"round\"/>",
            polygon(&face.screen),
            f = face.fill,
        );
    }
    for line in &built.lines {
        paint_line(line, view, &faces, &mut body);
    }
    for dot in &built.dots {
        paint_dot(dot, view, &faces, &mut body);
    }
    (attrs, body)
}

/// The triangles of every mesh that face the camera, painter-sorted from far to near.
fn front_faces(built: &Model, view: &View) -> Vec<Face> {
    let mut faces = Vec::new();
    for mesh in &built.meshes {
        for tri in mesh.indices.chunks_exact(3) {
            let idx = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
            let p = idx.map(|i| mesh.positions[i]);
            let facing = cross(sub(p[1], p[0]), sub(p[2], p[0]));
            if inner(facing, view.toward) <= 0.0 {
                continue;
            }
            let summed = idx
                .iter()
                .fold([0.0; 3], |acc, &i| add(acc, mesh.normals[i]));
            let normal = unit(summed);
            let fill = surface_fill(mesh.surfaces[idx[0]], normal, view.azimuth);
            faces.push(Face::new(p, fill, view));
        }
    }
    faces.sort_by(|a, b| a.centre.total_cmp(&b.centre));
    faces
}

/// Each piece of a line that no nearer face hides, as a stroke.
fn paint_line(line: &Line3, view: &View, faces: &[Face], out: &mut String) {
    let count = line.points.len();
    if count < 2 {
        return;
    }
    let segments = if line.closed { count } else { count - 1 };
    for (i, &a) in line.points.iter().enumerate().take(segments) {
        let b = line.points[(i + 1) % count];
        let pieces = (distance(a, b) / PIECE).ceil().max(1.0) as usize;
        for k in 0..pieces {
            let t0 = k as f32 / pieces as f32;
            let t1 = (k + 1) as f32 / pieces as f32;
            let mid = lerp(a, b, (t0 + t1) / 2.0);
            if hidden(faces, view.screen(mid), view.depth(mid)) {
                continue;
            }
            let (s0, s1) = (view.screen(lerp(a, b, t0)), view.screen(lerp(a, b, t1)));
            let _ = writeln!(
                out,
                "<path d=\"M{:.2} {:.2}L{:.2} {:.2}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\" stroke-linecap=\"round\"/>",
                s0[0],
                s0[1],
                s1[0],
                s1[1],
                ink_colour(line.tone),
                line.width,
            );
        }
    }
}

/// A disc, drawn when it faces the camera and no nearer face hides its centre.
fn paint_dot(dot: &Dot3, view: &View, faces: &[Face], out: &mut String) {
    if dot_facing(dot, view) <= 0.0 {
        return;
    }
    let s = view.screen(dot.center);
    if hidden(faces, s, view.depth(dot.center)) {
        return;
    }
    let _ = writeln!(
        out,
        "<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"{}\" fill=\"{}\"/>",
        s[0],
        s[1],
        dot.radius,
        ink_colour(dot.tone),
    );
}

fn dot_facing(dot: &Dot3, view: &View) -> f32 {
    inner(dot.normal, view.toward)
}

/// Whether some face nearer than `depth` covers screen point `s`.
fn hidden(faces: &[Face], s: Vec2, depth: f32) -> bool {
    faces
        .iter()
        .any(|face| face.depth_at(s).is_some_and(|d| d > depth + DEPTH_EPS))
}

/// The screen box of the model's bounds, from its eight corners.
fn screen_box(built: &Model, view: &View) -> (Vec2, Vec2) {
    let b = built.bounds;
    let mut lo = [f32::INFINITY; 2];
    let mut hi = [f32::NEG_INFINITY; 2];
    for corner in 0..8 {
        let pick = |bit: usize, axis: usize| {
            if corner & bit == 0 {
                b.min[axis]
            } else {
                b.max[axis]
            }
        };
        let s = view.screen([pick(1, 0), pick(2, 1), pick(4, 2)]);
        lo = [lo[0].min(s[0]), lo[1].min(s[1])];
        hi = [hi[0].max(s[0]), hi[1].max(s[1])];
    }
    (lo, hi)
}

/// The fill of a surface. Paper is shaded by the rule. The accents are dev-sheet colours, not the renderer's.
fn surface_fill(surface: Surface, normal: Vec3, azimuth: f32) -> &'static str {
    match surface {
        Surface::Paper => match tone_for(normal, azimuth) {
            Tone::Top => "#ffffff",
            Tone::Shade3 => "#f4f4f1",
            Tone::Shade2 => "#ededea",
            Tone::Shade1 => "#e5e5e2",
            Tone::Shade0 => "#dcdcd9",
        },
        Surface::Lit => "#ffe08a",
        Surface::Glass => "#c4dde8",
        Surface::Ink(ink) => ink_colour(ink),
    }
}

/// Lines and dots take the colour of their ink. The models use no agent slot, so `Agent` is drawn grey here.
fn ink_colour(tone: Ink) -> &'static str {
    match tone {
        Ink::Hi => "#26262a",
        Ink::Mid | Ink::Agent(_) => "#8a8a8f",
        Ink::Lo => "#b4b4b8",
        Ink::Faint => "#d4d4d6",
        Ink::Edge => "#ebebe9",
        Ink::Shadow => "#000000",
        Ink::Lit => "#e8a33d",
        Ink::Red => "#e5484d",
        Ink::Green => "#30a46c",
        Ink::Blue => "#3e63dd",
    }
}

fn polygon(points: &[Vec2]) -> String {
    let mut out = String::new();
    for (index, [x, y]) in points.iter().enumerate() {
        let command = if index == 0 { 'M' } else { 'L' };
        let _ = write!(out, "{command}{x:.2} {y:.2}");
    }
    out
}

fn inner(a: Vec3, b: Vec3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn unit(v: Vec3) -> Vec3 {
    let length = inner(v, v).sqrt();
    if length > 0.0 {
        [v[0] / length, v[1] / length, v[2] / length]
    } else {
        v
    }
}

fn lerp(a: Vec3, b: Vec3, t: f32) -> Vec3 {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn distance(a: Vec3, b: Vec3) -> f32 {
    inner(sub(b, a), sub(b, a)).sqrt()
}

fn page(cards: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>acan-iso models</title>\n\
         <style>\n\
         html, body {{ background: #ffffff; color: #26262a; margin: 0; }}\n\
         body {{ font-family: system-ui, sans-serif; padding: 24px; }}\n\
         h1 {{ font-size: 18px; font-weight: 600; margin: 0 0 16px; }}\n\
         .grid {{ display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 16px; }}\n\
         figure {{ margin: 0; border: 1px solid #ededea; border-radius: 6px; padding: 12px; background: #ffffff; }}\n\
         svg {{ width: 100%; height: auto; display: block; }}\n\
         figcaption {{ font-size: 13px; color: #8a8a8f; margin-top: 8px; }}\n\
         </style>\n</head>\n<body>\n<h1>acan-iso models, 3D, elevation 30</h1>\n\
         <div class=\"grid\">\n{cards}</div>\n</body>\n</html>\n"
    )
}
