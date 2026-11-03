// Camera and theme, shared by every pass.
struct Camera {
    right: vec4<f32>,
    up: vec4<f32>,
    toward: vec4<f32>,
    // xyz: the eye. For the orthographic view it is the equivalent perspective eye, used for fog.
    // w: 1 when orthographic.
    eye: vec4<f32>,
    // xy: the ground pivot. z: eye distance, the length from the eye to the pivot. w: 1 when the
    // frame holds sRGB-encoded values, so blending does not happen in linear light.
    pivot: vec4<f32>,
    // x, y: viewport in device px. z: zoom in device px per plan unit at the pivot. w: thinnest
    // stroke in device px.
    view: vec4<f32>,
    // x: near plane. y: far plane (orthographic only). z: focal length in device px (perspective).
    // w: how far the view axis sits above the viewport's middle, in NDC (perspective).
    depth: vec4<f32>,
    // x, y: fog starts and ends, in plan units from the eye. z, w: the angle fade, as the absolute
    // sine of a ray's elevation, from and to.
    fog: vec4<f32>,
}

struct Theme {
    palette: array<vec4<f32>, @@PALETTE_LEN@@>,
    tints: array<vec4<f32>, @@TINT_LEN@@>,
}

@group(0) @binding(0) var<uniform> camera: Camera;
@group(0) @binding(1) var<uniform> theme: Theme;

// The line pass's ink, resolved to one sample per pixel. Read by the ink pass only.
@group(1) @binding(0) var ink_tex: texture_2d<f32>;

// Palette slots and tuning constants from `theme.rs`, written in by the renderer.
@@THEME@@
// The ground is depth-tested this far below z = 0, so surfaces that lie on it pass the test.
const GROUND_Z: f32 = -0.05;
// Faces whose normal is steeper than this are tops.
const TOP_NZ: f32 = 0.7;
// Dots sit this far toward the viewer, so they never z-fight the surface under them.
const LIFT: f32 = 0.05;

// Depth is reverse: 1 at the near plane and smaller farther away, cleared to 0. Perspective puts
// the far plane at infinity, so the depth is near / distance.
fn is_ortho() -> bool {
    return camera.eye.w > 0.5;
}

// Distance in front of the eye, along the view direction.
fn depth_of(p: vec3<f32>) -> f32 {
    return -dot(p - camera.eye.xyz, camera.toward.xyz);
}

fn ndc_depth(p: vec3<f32>) -> f32 {
    let d = depth_of(p);
    if (is_ortho()) {
        return 1.0 - (d - camera.depth.x) / (camera.depth.y - camera.depth.x);
    }
    return camera.depth.x / d;
}

// Clip space for a world point. Perspective divides by the depth, so the screen position is the
// focal length times the sideways offset over the depth.
fn to_clip(p: vec3<f32>) -> vec4<f32> {
    if (is_ortho()) {
        let rel = p - vec3<f32>(camera.pivot.xy, 0.0);
        let x = 2.0 * camera.view.z * dot(rel, camera.right.xyz) / camera.view.x;
        let y = 2.0 * camera.view.z * dot(rel, camera.up.xyz) / camera.view.y;
        return vec4<f32>(x, y, ndc_depth(p), 1.0);
    }
    let rel = p - camera.eye.xyz;
    let x = 2.0 * camera.depth.z * dot(rel, camera.right.xyz) / camera.view.x;
    let w = depth_of(p);
    let y = 2.0 * camera.depth.z * dot(rel, camera.up.xyz) / camera.view.y + camera.depth.w * w;
    return vec4<f32>(x, y, camera.depth.x, w);
}

// Device px per plan unit at a point: the zoom in orthographic, else the focal length over depth.
fn px_per_unit(origin: vec3<f32>) -> f32 {
    if (is_ortho()) {
        return camera.view.z;
    }
    return camera.depth.z / max(depth_of(origin), camera.depth.x);
}

// How far a mark grows about its origin to stay at least `grow.x` device px tall. `grow.y` is the
// mark's height in plan units. A mark with no floor (`grow.x` 0) keeps its plan size.
fn grown(origin: vec3<f32>, grow: vec2<f32>) -> f32 {
    if (grow.x <= 0.0) {
        return 1.0;
    }
    return max(1.0, grow.x / (grow.y * px_per_unit(origin)));
}

// A point turned `turn` radians anticlockwise about the z axis, through the origin. A model turns
// as one piece. Its positions and its normals both pass through here. At zero it is the identity.
fn turned(p: vec3<f32>, turn: f32) -> vec3<f32> {
    let c = cos(turn);
    let s = sin(turn);
    return vec3<f32>(c * p.x - s * p.y, s * p.x + c * p.y, p.z);
}

// Fog from the eye: nothing fades nearer than the fog start, and the far end is page white.
// Mirrors `Camera::fog_at`.
fn fog_at(p: vec3<f32>) -> f32 {
    return smoothstep(camera.fog.x, camera.fog.y, distance(p, camera.eye.xyz));
}

// The paper grey of a face, from its normal. The light is fixed to the camera's left, so a face
// keeps its tone at every azimuth: tops are nearly white, sides run from the lit tone to the dark
// one as they turn away from the light, and slopes blend toward the top.
fn paper_grey(n: vec3<f32>) -> f32 {
    let horizontal = n.xy;
    var side = PAPER_LIT;
    if (dot(horizontal, horizontal) > 1e-8) {
        let light = -normalize(camera.right.xy);
        let away = 0.5 - 0.5 * dot(normalize(horizontal), light);
        side = mix(PAPER_LIT, PAPER_DARK, away);
    }
    let up = smoothstep(0.3, TOP_NZ + 0.25, abs(n.z));
    return 1.0 - mix(side, PAPER_TOP, up);
}

// Mesh pass: one draw per kind per level, instanced.

struct MeshIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) code: u32,
    @location(3) offset: vec3<f32>,
    @location(4) tint: u32,
    @location(5) grow: vec2<f32>,
    @location(6) turn: f32,
}

struct MeshOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) @interpolate(flat) code: u32,
    @location(2) @interpolate(flat) tint: u32,
    @location(3) world: vec3<f32>,
}

@vertex
fn vs_mesh(v: MeshIn) -> MeshOut {
    var out: MeshOut;
    let p = turned(v.position, v.turn) * grown(v.offset, v.grow) + v.offset;
    out.clip = to_clip(p);
    out.normal = turned(v.normal, v.turn);
    out.code = v.code;
    out.tint = v.tint;
    out.world = p;
    return out;
}

@fragment
fn fs_mesh(v: MeshOut) -> @location(0) vec4<f32> {
    let n = normalize(v.normal);
    var ink = vec4<f32>(vec3<f32>(paper_grey(n)), 1.0);
    if (v.code != 0u) {
        ink = theme.palette[v.code - 1u];
    }
    ink = ink * theme.tints[v.tint];
    let fog = fog_at(v.world);
    let page = theme.palette[SLOT_PAGE].rgb;
    return vec4<f32>(mix(ink.rgb, page, fog), ink.a);
}

// An occluded silhouette is a bot that geometry in front hides. It draws as a faint outline in its
// tint. The pass runs the mesh's vertices with the depth test reversed, so only the hidden part draws,
// and only where the surface turns edge-on to the eye, so the fill stays clear.
const SILHOUETTE_ALPHA: f32 = 0.35;
// A face whose normal is within this of edge-on to the eye draws as outline.
const SILHOUETTE_EDGE: f32 = 0.3;

@fragment
fn fs_silhouette(v: MeshOut) -> @location(0) vec4<f32> {
    var to_eye = camera.toward.xyz;
    if (!is_ortho()) {
        to_eye = normalize(camera.eye.xyz - v.world);
    }
    let facing = abs(dot(normalize(v.normal), to_eye));
    let rim = 1.0 - smoothstep(0.0, SILHOUETTE_EDGE, facing);
    let fog = fog_at(v.world);
    let hue = theme.tints[v.tint].rgb;
    return vec4<f32>(hue, SILHOUETTE_ALPHA * rim * (1.0 - fog));
}

// Ground: one full-screen triangle. Each pixel's view ray meets the ground at a plan point, and
// the grid lines are analytic, so there is no geometry. A ray that rises above the horizon sees
// no ground: it is page white and writes no depth.

struct FullOut {
    @builtin(position) clip: vec4<f32>,
}

fn full_clip(i: u32) -> vec4<f32> {
    let x = f32((i << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(i & 2u) * 2.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> FullOut {
    return FullOut(full_clip(i));
}

struct Ground {
    point: vec2<f32>,
    // The unit direction of the view ray through the pixel.
    dir: vec3<f32>,
    hit: bool,
}

// The ground point under a device pixel, and whether the pixel sees the ground at all.
fn ground_ray(frag: vec2<f32>) -> Ground {
    let sx = frag.x - camera.view.x * 0.5;
    // The orthographic axis is always mid-screen, and its depth.w is 0.
    let sy = camera.view.y * 0.5 * (1.0 - camera.depth.w) - frag.y;
    if (is_ortho()) {
        let ox = sx / camera.view.z;
        let oy = sy / camera.view.z;
        let t = -oy * camera.up.z / camera.toward.z;
        let g = camera.pivot.xy + ox * camera.right.xy + oy * camera.up.xy + t * camera.toward.xy;
        return Ground(g, -camera.toward.xyz, true);
    }
    let dir = -camera.depth.z * camera.toward.xyz + sx * camera.right.xyz + sy * camera.up.xyz;
    let t = -camera.eye.z / min(dir.z, -1e-6);
    let g = camera.eye.xy + t * dir.xy;
    return Ground(g, normalize(dir), dir.z < 0.0);
}

// Coverage of a 1 px anti-aliased line every `spacing` plan units, and the spacing in device px.
fn grid_line(ground: vec2<f32>, spacing: f32) -> vec2<f32> {
    let c = ground / spacing;
    let d = max(fwidth(c), vec2<f32>(1e-6));
    let dist = abs(fract(c + 0.5) - 0.5) / d;
    let coverage = 1.0 - clamp(min(dist.x, dist.y), 0.0, 1.0);
    return vec2<f32>(coverage, 1.0 / max(d.x, d.y));
}

// Ink alpha of the grid at a ground point. A level fades in as its on-screen spacing grows,
// starting minor, and turns major as the next finer level fades in under it. The strongest
// level wins, so a line that two levels share is no darker than a major line.
fn grid_alpha(ground: vec2<f32>) -> f32 {
    var best = 0.0;
    var spacing = GRID_BASE;
    for (var level = 0; level < GRID_LEVELS; level = level + 1) {
        let line = grid_line(ground, spacing);
        let shown = smoothstep(GRID_SHOW_PX[0], GRID_SHOW_PX[1], line.y);
        let finer = smoothstep(GRID_SHOW_PX[0], GRID_SHOW_PX[1], line.y / GRID_STEP);
        let ink = mix(GRID_MINOR_ALPHA, GRID_MAJOR_ALPHA, finer);
        best = max(best, ink * shown * line.x);
        spacing = spacing * GRID_STEP;
    }
    return best;
}

struct GridOut {
    @location(0) colour: vec4<f32>,
    @builtin(frag_depth) depth: f32,
}

@fragment
fn fs_grid(@builtin(position) frag: vec4<f32>) -> GridOut {
    let ray = ground_ray(frag.xy);
    let g = ray.point;
    // Fog and the angle fade are computed for every pixel, then the sky is selected away, so the
    // derivatives in the grid lines stay in uniform control flow.
    let fog = fog_at(vec3<f32>(g, 0.0));
    let graze = smoothstep(camera.fog.z, camera.fog.w, abs(ray.dir.z));
    let line_alpha = grid_alpha(g) * graze * (1.0 - fog);
    let alpha = select(0.0, line_alpha, ray.hit);

    let page = theme.palette[SLOT_PAGE].rgb;
    let ink = theme.palette[SLOT_GRID].rgb;
    let depth = select(0.0, clamp(ndc_depth(vec3<f32>(g, GROUND_Z)), 0.0, 1.0), ray.hit);
    return GridOut(vec4<f32>(mix(page, ink, alpha), 1.0), depth);
}

// Belts: flat opaque ribbons on the ground. The side and end lines are drawn in the fill, from the
// distance to the ribbon's border in device pixels, so they never break up at a distance. They
// fade as the ribbon narrows on screen, measured across the belt and not along it, down to a
// floor so a belt never vanishes.

struct RibbonIn {
    @location(0) position: vec2<f32>,
    @location(1) side: f32,
    @location(2) along: f32,
    @location(3) length: f32,
    @location(4) half: f32,
}

struct RibbonOut {
    @builtin(position) clip: vec4<f32>,
    // Plan units from the centre line, signed.
    @location(0) across: f32,
    @location(1) along: f32,
    @location(2) @interpolate(flat) size: vec2<f32>,
    @location(3) world: vec3<f32>,
}

@vertex
fn vs_ribbon(v: RibbonIn) -> RibbonOut {
    let p = vec3<f32>(v.position, 0.0);
    var out: RibbonOut;
    out.clip = to_clip(p);
    out.across = v.side * v.half;
    out.along = v.along;
    out.size = vec2<f32>(v.length, v.half);
    out.world = p;
    return out;
}

@fragment
fn fs_ribbon(v: RibbonOut) -> @location(0) vec4<f32> {
    // Plan units per device pixel along each axis's screen gradient. The gradient length, unlike
    // fwidth, is the true screen distance across the belt, so a slanted belt reads its real width.
    let per_px_across = max(length(vec2<f32>(dpdx(v.across), dpdy(v.across))), 1e-6);
    let per_px_along = max(length(vec2<f32>(dpdx(v.along), dpdy(v.along))), 1e-6);
    let side_px = (v.size.y - abs(v.across)) / per_px_across;
    let end_px = min(v.along, v.size.x - v.along) / per_px_along;
    let line = clamp(BELT_EDGE_PX + 0.5 - min(side_px, end_px), 0.0, 1.0);
    let width_px = 2.0 * v.size.y / per_px_across;
    let shown = mix(BELT_EDGE_FLOOR, 1.0, smoothstep(BELT_EDGE_FADE_PX[0], BELT_EDGE_FADE_PX[1], width_px));
    let edge = theme.palette[SLOT_BELT_EDGE];
    let fill = theme.palette[SLOT_BELT].rgb;
    let colour = mix(fill, edge.rgb, edge.a * line * shown);
    let fog = fog_at(v.world);
    let page = theme.palette[SLOT_PAGE].rgb;
    return vec4<f32>(mix(colour, page, fog), 1.0);
}

// Contact shadow: a short soft falloff on the ground around each footprint, pushed a little
// away from the light, drawn before the machines.

struct HaloIn {
    @location(0) position: vec3<f32>,
    @location(1) rect: vec4<f32>,
    @location(2) offset: vec3<f32>,
}

struct HaloOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) rect: vec4<f32>,
    @location(2) world: vec3<f32>,
}

@vertex
fn vs_halo(v: HaloIn) -> HaloOut {
    var out: HaloOut;
    let p = v.position + v.offset;
    out.clip = to_clip(p);
    out.local = v.position.xy;
    out.rect = v.rect;
    out.world = p;
    return out;
}

@fragment
fn fs_halo(v: HaloOut) -> @location(0) vec4<f32> {
    let shifted = v.local - SHADOW_SHIFT * normalize(camera.right.xy);
    let q = max(max(v.rect.xy - shifted, shifted - v.rect.zw), vec2<f32>(0.0));
    let fade = 1.0 - smoothstep(0.0, SHADOW_REACH, length(q));
    let halo = theme.palette[SLOT_SHADOW];
    let page = theme.palette[SLOT_PAGE].rgb;
    let fog = fog_at(v.world);
    let alpha = halo.a * fade * fade * (1.0 - fog);
    return vec4<f32>(mix(halo.rgb, page, fog), alpha);
}

// Lines: every mesh edge that draws, then the painted lines. Each segment is a quad. The
// vertex shader lays a quad over the segment on screen, wider than the stroke by a pixel of
// anti-aliasing on every side, and the fragment shader cuts the stroke out of the quad by its
// distance to the segment, so every end is a round cap. Lines draw into the ink target, where
// they meet by max, so a joint or a crossing is never darker than one line. The target holds
// one channel, how much a line darkens the page, so a line is grey: a coloured ink draws as
// the grey of its luminance.

struct EdgeIn {
    @location(0) a: vec3<f32>,
    @location(1) b: vec3<f32>,
    @location(2) normal_a: vec3<f32>,
    @location(3) normal_b: vec3<f32>,
    @location(4) corner: vec2<f32>,
    @location(5) crease: u32,
    @location(6) offset: vec3<f32>,
    @location(7) grow: vec2<f32>,
    @location(8) turn: f32,
}

struct LineIn {
    @location(0) a: vec3<f32>,
    @location(1) b: vec3<f32>,
    @location(2) corner: vec2<f32>,
    @location(3) width: f32,
    @location(4) tone: u32,
    @location(5) offset: vec3<f32>,
    @location(6) grow: vec2<f32>,
    @location(7) turn: f32,
}

struct StrokeOut {
    @builtin(position) clip: vec4<f32>,
    // The segment's ends in device px from the viewport's middle, y up: one in xy, one in zw.
    @location(0) @interpolate(flat) ends: vec4<f32>,
    // The ink. Its alpha is the ink's opacity times the share of the stroke that shows, faded by
    // the fog at the segment's middle.
    @location(1) @interpolate(flat) ink: vec4<f32>,
    @location(2) @interpolate(flat) half_px: f32,
}

// A machine's size on screen, as the device px that `LOD_REFERENCE` plan units span at its origin.
fn screen_size(origin: vec3<f32>) -> f32 {
    var px_per_unit = camera.view.z;
    if (!is_ortho()) {
        px_per_unit = camera.depth.z / max(depth_of(origin), camera.depth.x);
    }
    return px_per_unit * LOD_REFERENCE;
}

// How much of a machine's painted detail shows.
fn detail_shown(origin: vec3<f32>) -> f32 {
    return smoothstep(LOD_FADE_PX[0], LOD_FADE_PX[1], screen_size(origin));
}

// How far a machine's line ink has eased in, from 0 when it is small on screen to 1 when large.
fn line_strength(origin: vec3<f32>) -> f32 {
    let size = max(screen_size(origin), 1e-3);
    return clamp(log2(size / LINE_FADE_PX[0]) / log2(LINE_FADE_PX[1] / LINE_FADE_PX[0]), 0.0, 1.0);
}

// The point moved `LINE_LIFT_PX` device px toward the eye at its own distance, along its ray, so
// it stays on the same pixel.
fn lifted(p: vec3<f32>) -> vec3<f32> {
    if (is_ortho()) {
        return p + camera.toward.xyz * (LINE_LIFT_PX / camera.view.z);
    }
    return p + normalize(camera.eye.xyz - p) * (LINE_LIFT_PX * depth_of(p) / camera.depth.z);
}

// Whether a mesh edge draws, before the depth test. A smooth edge draws on the contour, where
// one face turns toward the eye and the other away. A crease draws when either face turns
// toward the eye. Mirrors `Edge::shows_from` in `edges.rs`.
fn edge_shows(front_a: bool, front_b: bool, crease: bool) -> bool {
    return front_a != front_b || (crease && (front_a || front_b));
}

fn faces_eye(normal: vec3<f32>, at: vec3<f32>) -> bool {
    var to_eye = camera.toward.xyz;
    if (!is_ortho()) {
        to_eye = camera.eye.xyz - at;
    }
    return dot(normal, to_eye) > 0.0;
}

// Every corner lands on one point, so the segment rasterizes nothing.
fn no_stroke() -> StrokeOut {
    var gone: StrokeOut;
    gone.clip = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    return gone;
}

// Device px from the viewport's middle, y up, of a clip-space point.
fn to_screen(c: vec4<f32>) -> vec2<f32> {
    return c.xy / c.w * camera.view.xy * 0.5;
}

// `outside` is a screen direction, or zero. A stroke with one moves its own half width that way,
// so it lies beside the silhouette and not across it.
fn stroke(
    a: vec3<f32>,
    b: vec3<f32>,
    half_px: f32,
    ink: vec4<f32>,
    corner: vec2<f32>,
    outside: vec2<f32>,
) -> StrokeOut {
    let pa = lifted(a);
    let pb = lifted(b);
    let ca = to_clip(pa);
    let cb = to_clip(pb);
    let view = camera.view.xy;
    let delta = to_screen(cb) - to_screen(ca);
    let len = length(delta);
    var dir = vec2<f32>(1.0, 0.0);
    if (len > 1e-6) {
        dir = delta / len;
    }
    let normal = vec2<f32>(-dir.y, dir.x);
    let side = dot(normal, outside);
    let push = normal * select(0.0, sign(side) * half_px, abs(side) > 1e-3);
    let sa = to_screen(ca) + push;
    let sb = to_screen(cb) + push;
    // A pixel square reaches at most half its diagonal along any direction, so the coverage
    // is zero that far past the stroke's edge.
    let reach = half_px + 0.71;
    let shift = push + (normal * corner.y + dir * (corner.x * 2.0 - 1.0)) * reach;

    // Clip space is linear along a segment, so the interpolated point is exact. An offset of
    // `shift` px is 2 / view times that in NDC, and NDC is clip over w.
    let base = mix(ca, cb, corner.x);
    var out: StrokeOut;
    out.clip = vec4<f32>(base.xy + shift * 2.0 / view * base.w, base.z, base.w);
    out.ends = vec4<f32>(sa, sb);
    out.ink = vec4<f32>(ink.rgb, ink.a * (1.0 - fog_at(0.5 * (pa + pb))));
    out.half_px = half_px;
    return out;
}

@vertex
fn vs_edge(v: EdgeIn) -> StrokeOut {
    let k = grown(v.offset, v.grow);
    let a = turned(v.a, v.turn) * k + v.offset;
    let b = turned(v.b, v.turn) * k + v.offset;
    let normal_a = turned(v.normal_a, v.turn);
    let normal_b = turned(v.normal_b, v.turn);
    let mid = 0.5 * (a + b);
    let front_a = faces_eye(normal_a, mid);
    let front_b = faces_eye(normal_b, mid);
    if (!edge_shows(front_a, front_b, v.crease != 0u)) {
        return no_stroke();
    }
    // An open boundary has one face, so both its normals are the same.
    let outer = front_a != front_b || all(normal_a == normal_b);
    let far = select(CREASE_FAR_ALPHA, CONTOUR_FAR_ALPHA, outer);
    var ink = theme.palette[SLOT_LINE];
    ink.a = ink.a * mix(far, 1.0, line_strength(v.offset));
    if (ink.a <= 0.0) {
        return no_stroke();
    }
    // A contour drawn across the silhouette loses its inner half to the depth test wherever
    // the facets next to it graze the view, which bends and fades the line. Beside the
    // silhouette it lies over what is behind. The two normals add up to one that points out
    // of the surface across the edge.
    var outside = vec2<f32>(0.0);
    if (front_a != front_b) {
        let n = normal_a + normal_b;
        let s = to_screen(to_clip(mid + n)) - to_screen(to_clip(mid));
        if (length(s) > 1e-6) {
            outside = normalize(s);
        }
    }
    return stroke(a, b, 0.5 * LINE_PX, ink, v.corner, outside);
}

@vertex
fn vs_line(v: LineIn) -> StrokeOut {
    let shown = detail_shown(v.offset);
    if (shown <= 0.0) {
        return no_stroke();
    }
    // A stroke thinner than the floor draws at the floor with its ink thinned to match.
    let min_px = camera.view.w;
    var ink = theme.palette[v.tone];
    ink.a = ink.a * min(v.width / min_px, 1.0) * shown;
    let half_px = 0.5 * max(v.width, min_px);
    let k = grown(v.offset, v.grow);
    let a = turned(v.a, v.turn) * k + v.offset;
    let b = turned(v.b, v.turn) * k + v.offset;
    return stroke(a, b, half_px, ink, v.corner, vec2<f32>(0.0));
}

const LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);

// The share of a pixel square within `x` of its centre along the unit direction `n`. The
// square projects onto `n` as a trapezoid, `a + b` wide with a flat top `a - b` wide.
fn pixel_share(x: f32, n: vec2<f32>) -> f32 {
    let a = max(abs(n.x), abs(n.y));
    let b = min(abs(n.x), abs(n.y));
    let t = x + 0.5 * (a + b);
    if (t <= 0.0) {
        return 0.0;
    }
    if (t >= a + b) {
        return 1.0;
    }
    if (t < b) {
        return t * t / (2.0 * a * b);
    }
    if (t <= a) {
        return (t - 0.5 * b) / a;
    }
    let u = a + b - t;
    return 1.0 - u * u / (2.0 * a * b);
}

// The share of a pixel square that a stroke covers. A plain ramp on the distance shades a slanted
// thin line unevenly, alternately one dark pixel and two light ones, which reads as a beaded line.
fn stroke_coverage(v: StrokeOut) -> f32 {
    let p = vec2<f32>(v.clip.x - 0.5 * camera.view.x, 0.5 * camera.view.y - v.clip.y);
    let a = v.ends.xy;
    let ab = v.ends.zw - a;
    let t = clamp(dot(p - a, ab) / max(dot(ab, ab), 1e-6), 0.0, 1.0);
    let off = p - (a + t * ab);
    let d = length(off);
    var n = vec2<f32>(-ab.y, ab.x) / max(length(ab), 1e-6);
    if (d > 1e-4) {
        n = off / d;
    }
    return pixel_share(v.half_px - d, n) - pixel_share(-v.half_px - d, n);
}

// The ink a stroke lays on the page, as how much it darkens it. The coverage is the share of
// the pixel square inside the stroke.
@fragment
fn fs_stroke(v: StrokeOut) -> @location(0) vec4<f32> {
    let coverage = stroke_coverage(v);
    let lit = dot(v.ink.rgb, LUMA);
    var dark = (1.0 - lit) * coverage;
    if (camera.pivot.w > 0.5) {
        // An encoded frame darkens a pixel a stroke half covers by half its ink, which takes
        // more light than half. A slanted line, alternately on one pixel and split over two,
        // then beads. This darkens each pixel by what its share takes in light, on paper.
        let light = 1.0 - coverage * (1.0 - pow(lit, 2.2));
        dark = 1.0 - pow(light, 1.0 / 2.2);
    }
    // No discard: under the max blend a zero changes nothing, and a discard costs more.
    return vec4<f32>(dark * v.ink.a, 0.0, 0.0, coverage * v.ink.a);
}

// A hue stroke is one screen-space stroke per piece, in one tint: a belt's hue line, a drone's
// tether, a piece of its trail or a side of a ghost. Each piece carries its own width and opacity.

struct HueIn {
    @location(0) a: vec3<f32>,
    @location(1) b: vec3<f32>,
    @location(2) corner: vec2<f32>,
    @location(3) width: f32,
    @location(4) alpha: f32,
    @location(5) tint: u32,
    @location(6) origin: vec3<f32>,
    @location(7) grow: vec2<f32>,
    @location(8) turn: f32,
}

// A stroke's start is measured from its origin, turned and grown as a model is. A belt has no
// origin, so its start is the world point. A beam from a drone's eye starts on the eye at any size.
@vertex
fn vs_hue(v: HueIn) -> StrokeOut {
    let min_px = camera.view.w;
    var ink = theme.tints[v.tint];
    ink.a = v.alpha;
    let half_px = 0.5 * max(v.width, min_px);
    let start = v.origin + turned(v.a, v.turn) * grown(v.origin, v.grow);
    return stroke(start, v.b, half_px, ink, v.corner, vec2<f32>(0.0));
}

@fragment
fn fs_hue(v: StrokeOut) -> @location(0) vec4<f32> {
    return vec4<f32>(v.ink.rgb, v.ink.a * stroke_coverage(v));
}

// Dots: a disc is a square of corners in its own plane, cut to a circle in the fragment shader.

struct DotIn {
    @location(0) centre: vec3<f32>,
    @location(1) tangent: vec3<f32>,
    @location(2) bitangent: vec3<f32>,
    @location(3) corner: vec2<f32>,
    @location(4) tone: u32,
    @location(5) offset: vec3<f32>,
    @location(6) grow: vec2<f32>,
    @location(7) turn: f32,
}

struct DotOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) tone: u32,
    @location(2) world: vec3<f32>,
}

@vertex
fn vs_dot(v: DotIn) -> DotOut {
    let k = grown(v.offset, v.grow);
    let p = turned(v.centre, v.turn) * k + v.offset + camera.toward.xyz * LIFT
        + turned(v.tangent, v.turn) * (k * v.corner.x)
        + turned(v.bitangent, v.turn) * (k * v.corner.y);
    var out: DotOut;
    out.clip = to_clip(p);
    out.uv = v.corner;
    out.tone = v.tone;
    out.world = p;
    return out;
}

@fragment
fn fs_dot(v: DotOut) -> @location(0) vec4<f32> {
    let r = length(v.uv);
    let aa = max(fwidth(r), 1e-4);
    let coverage = clamp((1.0 - r) / aa + 0.5, 0.0, 1.0);
    if (coverage <= 0.0) {
        discard;
    }
    // A shadow fades to its rim, so it reads as light on the ground and not as a disc.
    let falloff = select(1.0, 1.0 - smoothstep(0.0, 1.0, r), v.tone == SLOT_SHADOW);
    let ink = theme.palette[v.tone];
    let page = theme.palette[SLOT_PAGE].rgb;
    let fog = fog_at(v.world);
    return vec4<f32>(mix(ink.rgb, page, fog), ink.a * coverage * falloff);
}

// Ink pass: a full-screen triangle that lays the resolved line ink on the frame. Its blend
// multiplies the frame by one minus the ink, so a line darkens paper, a shaded side and a belt
// alike.
@fragment
fn fs_ink(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let ink = textureLoad(ink_tex, vec2<i32>(frag.xy), 0).r;
    return vec4<f32>(vec3<f32>(ink), 0.0);
}
