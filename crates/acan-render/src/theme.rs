//! The light theme: a white page, neutral greys and fine grey line art. Colours live here and
//! nowhere in a model. The shader reads the slot numbers and the tuning constants from the
//! WGSL that `wgsl_consts` writes, so the two never disagree.

use crate::model::{Ink, Surface};

/// Palette slots on the GPU.
pub const PALETTE_LEN: usize = 21;
/// Tint slots. Slot 0 is the identity; instances pick a slot to tint what they draw. The slots from
/// [`TINT_DIM0`] up are dimmed copies of the plain ones below it.
pub const TINT_LEN: usize = 48;
/// The first dimmed tint slot: `TINT_DIM0 + slot` is the plain slot `slot` darkened by
/// [`DIM_SHARE`]. A machine a tour step does not stage takes its dimmed slot.
pub const TINT_DIM0: u32 = 24;
/// The share of its colour each dimmed slot keeps.
pub const DIM_SHARE: f32 = 0.4;
const _: () = assert!(
    TINT_DIM0 as usize * 2 == TINT_LEN,
    "every plain tint slot has a dimmed copy"
);
/// Agent hues, as `#rrggbb`. An agent's hue comes from its id, so it is the same on every reload.
pub const AGENT_HUES: [u32; AGENT_SLOTS] =
    [0x1f9e8f, 0x7a5cd6, 0x3d7fd1, 0xc0548c, 0x6a9a2e, 0x9b4d9b];
pub const AGENT_SLOTS: usize = 6;
/// The share of an agent's hue a machine's footprint takes, mixed into white.
pub const FOOTPRINT_MIX: f32 = 0.22;
/// The share of an agent's hue a drone's shell takes, mixed into white. A shell reads as a pale
/// tint of the hue, and its line art stays dark.
pub const SHELL_MIX: f32 = 0.28;
// The shell stays a pale tint, so its strokes and shading read over it.
const _: () = assert!(SHELL_MIX < 0.3, "the shell stays a pale tint");
/// Tint slot of each agent's accent at full hue: `TINT_BODY0 + slot`. The drone's eye and lamp, its
/// beam and its beacon draw with it.
pub const TINT_BODY0: u32 = 1;
/// Tint slot of each agent's footprint mix: `TINT_FOOTPRINT0 + slot`.
pub const TINT_FOOTPRINT0: u32 = 7;
/// Tint slot for a machine two or more agents touch. The page sets it per frame.
pub const TINT_SHARED: u32 = 13;
/// Tint slot of each agent's drone shell mix: `TINT_SHELL0 + slot`.
pub const TINT_SHELL0: u32 = 14;
/// Tint slots of the scope lanes, in [`SCOPE_TINTS`] order: a draft plot, a confirmed plot, a
/// draft keep and a confirmed keep. `TINT_SCOPE0 + i` is the i-th.
pub const TINT_SCOPE0: u32 = 20;
/// The scope tints as a hue and the share of it mixed into white. A plot is amber and a keep is
/// brass; a draft takes less of its hue than a confirmed lane, so it reads as not yet in force.
pub const SCOPE_TINTS: [(u32, f32); 4] = [
    (0xd99a1e, 0.12),
    (0xd99a1e, 0.28),
    (0x9c7f2e, 0.16),
    (0x9c7f2e, 0.36),
];

pub const SLOT_PAGE: u32 = 0;
pub const SLOT_LIT: u32 = 1;
pub const SLOT_GLASS: u32 = 2;
/// The soft contact shadow at the foot of a machine.
pub const SLOT_SHADOW: u32 = 3;
pub const SLOT_GRID: u32 = 4;
pub const SLOT_HI: u32 = 5;
pub const SLOT_MID: u32 = 6;
pub const SLOT_LO: u32 = 7;
pub const SLOT_FAINT: u32 = 8;
pub const SLOT_DOT: u32 = 9;
pub const SLOT_RED: u32 = 10;
pub const SLOT_GREEN: u32 = 11;
pub const SLOT_BLUE: u32 = 12;
/// Four agent slots. Placeholders until agents get a palette.
pub const SLOT_AGENT0: u32 = 13;
/// The opaque fill of a belt and the fine line along its sides and ends.
pub const SLOT_BELT: u32 = 17;
pub const SLOT_BELT_EDGE: u32 = 18;
/// The contours and creases the renderer strokes from a model's mesh.
pub const SLOT_LINE: u32 = 19;
/// A seam: the light painted line of `Ink::Edge`.
pub const SLOT_EDGE: u32 = 20;
/// Width of a contour or crease in device pixels, at every zoom.
pub const LINE_PX: f32 = 0.9;
/// The line ink eases in as a machine grows on screen, from its size at `LINE_FADE_PX.0` to
/// `LINE_FADE_PX.1` device pixels, on a log scale. The size is measured as for the seams'
/// level of detail. Zoomed in, every edge has the full ink. Zoomed out, a machine keeps a light
/// contour and ground contact, which hold it apart from the ground, while its inner creases
/// fade, so a small machine reads as a soft block and not as a knot of lines.
pub const LINE_FADE_PX: (f32, f32) = (40.0, 480.0);
/// The ink's opacity on a machine at `LINE_FADE_PX.0` or smaller: on the contour and at an
/// open boundary, and on an inner crease, where both faces turn toward the eye.
pub const CONTOUR_FAR_ALPHA: f32 = 0.5;
pub const CREASE_FAR_ALPHA: f32 = 0.0;
/// Lines are moved this many device pixels toward the eye before the depth test, at their own
/// distance, so a line wins against the surface it lies on.
pub const LINE_LIFT_PX: f32 = 1.0;

/// Paper shading, as darkness below white. The light is fixed to the camera, from its left,
/// so a face keeps its tone at every azimuth. A top is nearly white, a side facing the light
/// is `PAPER_LIT` darker and a side facing away is `PAPER_DARK` darker. The two sides a box
/// shows at the default view sit at 7% and 14%.
pub const PAPER_TOP: f32 = 0.015;
pub const PAPER_LIT: f32 = 0.056;
pub const PAPER_DARK: f32 = 0.154;

/// Distance fog from the eye. It starts past the plant's farthest corner, and never nearer
/// than `FOG_NEAR` times the pivot's eye distance, so the plant and the ground under it stay
/// clear. It ends `FOG_SPAN` times as far as it starts, which leaves the grid room to
/// converge toward a soft horizon. Fog mixes toward the page colour.
pub const FOG_NEAR: f32 = 1.5;
pub const FOG_SPAN: f32 = 8.0;
/// Lines fade as the view grazes the ground, from `ANGLE_FADE.0` to `ANGLE_FADE.1` of the
/// absolute sine of the ray's elevation. This kills moire near the horizon, as Blender does.
pub const ANGLE_FADE: (f32, f32) = (0.004, 0.05);
/// Grid levels, each `GRID_STEP` times the spacing of the one before, starting at `GRID_BASE`
/// plan units.
pub const GRID_LEVELS: u32 = 4;
pub const GRID_BASE: f32 = 8.0;
pub const GRID_STEP: f32 = 5.0;
/// A level shows while its on-screen spacing is between these device pixels, so levels cross
/// fade as the zoom changes.
pub const GRID_SHOW_PX: (f32, f32) = (6.0, 30.0);
/// Ink of a grid line, as black alpha on the white page. A level is minor while the finer
/// level under it is still hidden and turns major as that level appears. A pixel takes the
/// strongest level, never the sum, so the grid stays under the machine line art.
pub const GRID_MINOR_ALPHA: f32 = 0.05;
pub const GRID_MAJOR_ALPHA: f32 = 0.11;

/// The contact shadow: how far it reaches past the footprint, in plan units, and how far the
/// light pushes it toward the camera's right.
pub const SHADOW_REACH: f32 = 7.0;
pub const SHADOW_SHIFT: f32 = 2.0;

/// The belt's side and end lines are this wide, in device pixels. They fade as the belt's
/// width across it on screen narrows from `BELT_EDGE_FADE_PX.1` to `BELT_EDGE_FADE_PX.0`
/// device pixels, but never below `BELT_EDGE_FLOOR`, so a far belt stays a quiet strip and
/// never vanishes or dots.
pub const BELT_EDGE_PX: f32 = 1.0;
pub const BELT_EDGE_FADE_PX: (f32, f32) = (3.0, 9.0);
pub const BELT_EDGE_FLOOR: f32 = 0.35;

/// Inner lines (feature edges and painted detail) fade out as a machine shrinks on screen.
/// The size is the screen length of `LOD_REFERENCE` plan units at the machine. Below
/// `LOD_FADE_PX.0` device pixels a machine is only its contours and creases.
pub const LOD_REFERENCE: f32 = 96.0;
pub const LOD_FADE_PX: (f32, f32) = (30.0, 60.0);

const fn rgb(hex: u32, alpha: f32) -> [f32; 4] {
    [
        ((hex >> 16) & 255) as f32 / 255.0,
        ((hex >> 8) & 255) as f32 / 255.0,
        (hex & 255) as f32 / 255.0,
        alpha,
    ]
}

/// sRGB with straight alpha.
const PALETTE: [[f32; 4]; PALETTE_LEN] = [
    rgb(0xffffff, 1.0),  // page
    rgb(0xd99a1e, 1.0),  // lit: amber, waiting on the user
    rgb(0xf2f2f2, 1.0),  // glass
    rgb(0x000000, 0.08), // contact shadow
    rgb(0x000000, 1.0),  // grid, at its alpha
    rgb(0x26262a, 1.0),  // hi
    rgb(0x8a8a8f, 1.0),  // mid
    rgb(0xb4b4b8, 1.0),  // lo
    rgb(0xd4d4d6, 1.0),  // faint
    rgb(0x3a3a3e, 1.0),  // dot
    rgb(0xd0453a, 1.0),  // red: the clash pin
    rgb(0x30a46c, 1.0),  // green
    rgb(0x3e63dd, 1.0),  // blue
    rgb(AGENT_HUES[0], 1.0),
    rgb(AGENT_HUES[1], 1.0),
    rgb(AGENT_HUES[2], 1.0),
    rgb(AGENT_HUES[3], 1.0),
    rgb(0xe9e9e9, 1.0), // belt
    rgb(0xb8b8b8, 1.0), // belt edge
    rgb(0x767676, 1.0), // contour and crease
    rgb(0xa8a8a8, 1.0), // seam
];

/// Palette slot of an ink used by a line.
pub fn ink_slot(ink: Ink) -> u32 {
    match ink {
        Ink::Hi => SLOT_HI,
        Ink::Mid => SLOT_MID,
        Ink::Lo => SLOT_LO,
        Ink::Faint => SLOT_FAINT,
        Ink::Edge => SLOT_EDGE,
        Ink::Shadow => SLOT_SHADOW,
        Ink::Lit => SLOT_LIT,
        Ink::Red => SLOT_RED,
        Ink::Green => SLOT_GREEN,
        Ink::Blue => SLOT_BLUE,
        Ink::Agent(slot) => SLOT_AGENT0 + u32::from(slot % 4),
    }
}

/// Palette slot of a dot. A hi dot is the kit's dot ink, a little lighter than a hi line.
pub fn dot_slot(ink: Ink) -> u32 {
    match ink {
        Ink::Hi => SLOT_DOT,
        other => ink_slot(other),
    }
}

/// The per-vertex code of a surface: 0 for paper (shaded in the shader), otherwise the palette
/// slot plus one.
pub fn flat_code(surface: Surface) -> u32 {
    match surface {
        Surface::Paper => 0,
        Surface::Lit => SLOT_LIT + 1,
        Surface::Glass => SLOT_GLASS + 1,
        Surface::Ink(ink) => ink_slot(ink) + 1,
    }
}

/// The palette as the surface wants it: linear when the surface is an sRGB format.
pub fn palette(srgb_surface: bool) -> [[f32; 4]; PALETTE_LEN] {
    PALETTE.map(|c| if srgb_surface { to_linear(c) } else { c })
}

/// The tint table in sRGB. Slot 0 is the identity, each agent has a body slot at full hue and a
/// footprint slot mixed into white, and the shared slot starts as the identity.
pub fn tints() -> [[f32; 4]; TINT_LEN] {
    let mut table = [[1.0; 4]; TINT_LEN];
    for slot in 0..AGENT_SLOTS {
        let hue = agent_colour(slot);
        table[TINT_BODY0 as usize + slot] = hue;
        table[TINT_FOOTPRINT0 as usize + slot] = footprint_mix(hue);
        table[TINT_SHELL0 as usize + slot] = shell_mix(hue);
    }
    for (i, (hue, share)) in SCOPE_TINTS.into_iter().enumerate() {
        table[TINT_SCOPE0 as usize + i] = mixed_into_white(rgb(hue, 1.0), share);
    }
    add_dim_slots(&mut table);
    table
}

/// Fills the dimmed slots from the plain ones: each keeps [`DIM_SHARE`] of its colour and its
/// alpha. Call it once the plain slots hold their final values.
pub fn add_dim_slots(table: &mut [[f32; 4]; TINT_LEN]) {
    for slot in 0..TINT_DIM0 as usize {
        let plain = table[slot];
        table[TINT_DIM0 as usize + slot] = [
            plain[0] * DIM_SHARE,
            plain[1] * DIM_SHARE,
            plain[2] * DIM_SHARE,
            plain[3],
        ];
    }
}

/// An agent's hue in sRGB, for the slot it holds.
pub fn agent_colour(slot: usize) -> [f32; 4] {
    rgb(AGENT_HUES[slot], 1.0)
}

/// A hue mixed into white by [`FOOTPRINT_MIX`], in sRGB.
pub fn footprint_mix(hue: [f32; 4]) -> [f32; 4] {
    mixed_into_white(hue, FOOTPRINT_MIX)
}

/// A hue mixed into white by [`SHELL_MIX`], in sRGB.
pub fn shell_mix(hue: [f32; 4]) -> [f32; 4] {
    mixed_into_white(hue, SHELL_MIX)
}

fn mixed_into_white(hue: [f32; 4], share: f32) -> [f32; 4] {
    let mix = |channel: f32| 1.0 + (channel - 1.0) * share;
    [mix(hue[0]), mix(hue[1]), mix(hue[2]), 1.0]
}

/// A colour given in sRGB, converted to the surface's encoding.
pub fn surface_colour(colour: [f32; 4], srgb_surface: bool) -> [f32; 4] {
    if srgb_surface {
        to_linear(colour)
    } else {
        colour
    }
}

/// The page colour for the clear, linear when the surface is an sRGB format.
pub fn page(srgb_surface: bool) -> [f32; 4] {
    palette(srgb_surface)[SLOT_PAGE as usize]
}

/// The slots and tuning constants the shader reads, as WGSL constant declarations.
pub fn wgsl_consts() -> String {
    let f = |name: &str, value: f32| format!("const {name}: f32 = {value:?};\n");
    let u = |name: &str, value: u32| format!("const {name}: u32 = {value}u;\n");
    let pair = |name: &str, (lo, hi): (f32, f32)| {
        format!("const {name}: vec2<f32> = vec2<f32>({lo:?}, {hi:?});\n")
    };
    [
        u("SLOT_PAGE", SLOT_PAGE),
        u("SLOT_SHADOW", SLOT_SHADOW),
        u("SLOT_GRID", SLOT_GRID),
        u("SLOT_BELT", SLOT_BELT),
        u("SLOT_BELT_EDGE", SLOT_BELT_EDGE),
        u("SLOT_LINE", SLOT_LINE),
        f("LINE_PX", LINE_PX),
        pair("LINE_FADE_PX", LINE_FADE_PX),
        f("CONTOUR_FAR_ALPHA", CONTOUR_FAR_ALPHA),
        f("CREASE_FAR_ALPHA", CREASE_FAR_ALPHA),
        f("LINE_LIFT_PX", LINE_LIFT_PX),
        f("PAPER_TOP", PAPER_TOP),
        f("PAPER_LIT", PAPER_LIT),
        f("PAPER_DARK", PAPER_DARK),
        format!("const GRID_LEVELS: i32 = {};\n", GRID_LEVELS),
        f("GRID_BASE", GRID_BASE),
        f("GRID_STEP", GRID_STEP),
        f("GRID_MINOR_ALPHA", GRID_MINOR_ALPHA),
        f("GRID_MAJOR_ALPHA", GRID_MAJOR_ALPHA),
        pair("GRID_SHOW_PX", GRID_SHOW_PX),
        f("SHADOW_REACH", SHADOW_REACH),
        f("SHADOW_SHIFT", SHADOW_SHIFT),
        f("BELT_EDGE_PX", BELT_EDGE_PX),
        pair("BELT_EDGE_FADE_PX", BELT_EDGE_FADE_PX),
        f("BELT_EDGE_FLOOR", BELT_EDGE_FLOOR),
        f("LOD_REFERENCE", LOD_REFERENCE),
        pair("LOD_FADE_PX", LOD_FADE_PX),
    ]
    .concat()
}

fn to_linear(c: [f32; 4]) -> [f32; 4] {
    let f = |x: f32| {
        if x <= 0.040_45 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };
    [f(c[0]), f(c[1]), f(c[2]), c[3]]
}

const _: () = assert!(
    SLOT_AGENT0 + 3 < SLOT_BELT,
    "agents do not run into the belt slots"
);
const _: () = assert!(
    TINT_BODY0 as usize + AGENT_SLOTS <= TINT_FOOTPRINT0 as usize
        && TINT_FOOTPRINT0 as usize + AGENT_SLOTS <= TINT_SHARED as usize
        && TINT_SHARED < TINT_SHELL0
        && TINT_SHELL0 as usize + AGENT_SLOTS <= TINT_SCOPE0 as usize
        && TINT_SCOPE0 as usize + SCOPE_TINTS.len() <= TINT_DIM0 as usize,
    "body, footprint, shared, shell and scope tints do not overlap"
);
const _: () = assert!(GRID_MINOR_ALPHA < GRID_MAJOR_ALPHA);
const _: () = assert!(PAPER_TOP > 0.0, "a top never matches the page");
const _: () = assert!(PAPER_TOP < PAPER_LIT && PAPER_LIT < PAPER_DARK);
const _: () = assert!(PAPER_DARK < 0.2, "no face turns dark");
const _: () = assert!(LINE_FADE_PX.0 > 0.0 && LINE_FADE_PX.0 < LINE_FADE_PX.1);
const _: () = assert!(
    0.0 <= CREASE_FAR_ALPHA && CREASE_FAR_ALPHA < CONTOUR_FAR_ALPHA && CONTOUR_FAR_ALPHA < 1.0,
    "a small machine keeps its contour longer than its creases"
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Ink coverage of a colour on the white page: 0 is the page, 1 is black.
    fn ink(c: [f32; 4]) -> f32 {
        (1.0 - c[0]) * c[3]
    }

    #[test]
    fn every_ink_maps_to_its_own_slot_in_range() {
        let inks = [
            Ink::Hi,
            Ink::Mid,
            Ink::Lo,
            Ink::Faint,
            Ink::Edge,
            Ink::Shadow,
            Ink::Lit,
            Ink::Red,
            Ink::Green,
            Ink::Blue,
            Ink::Agent(0),
            Ink::Agent(3),
        ];
        let slots: Vec<u32> = inks.iter().map(|&ink| ink_slot(ink)).collect();
        assert!(slots.iter().all(|&s| (s as usize) < PALETTE_LEN));
        assert_eq!(ink_slot(Ink::Shadow), SLOT_SHADOW);
        assert_eq!(ink_slot(Ink::Agent(0)), ink_slot(Ink::Agent(4)));
        assert_eq!(ink_slot(Ink::Agent(3)), SLOT_AGENT0 + 3);
        assert_eq!(dot_slot(Ink::Hi), SLOT_DOT);
        assert_eq!(dot_slot(Ink::Mid), SLOT_MID);
    }

    #[test]
    fn flat_codes_leave_zero_for_paper() {
        assert_eq!(flat_code(Surface::Paper), 0);
        assert_eq!(flat_code(Surface::Lit), SLOT_LIT + 1);
        assert_eq!(flat_code(Surface::Ink(Ink::Hi)), SLOT_HI + 1);
    }

    #[test]
    fn the_grid_stays_under_the_line_art() {
        let grid = PALETTE[SLOT_GRID as usize];
        assert_eq!(&grid[..3], &[0.0, 0.0, 0.0]);
        let faintest_line = [SLOT_EDGE, SLOT_LINE, SLOT_BELT_EDGE]
            .map(|slot| ink(PALETTE[slot as usize]))
            .into_iter()
            .chain([CONTOUR_FAR_ALPHA * ink(PALETTE[SLOT_LINE as usize])])
            .fold(f32::INFINITY, f32::min);
        assert!(
            GRID_MAJOR_ALPHA < faintest_line,
            "major grid ink {GRID_MAJOR_ALPHA} must stay under every line, {faintest_line}"
        );
    }

    #[test]
    fn the_inks_lines_are_drawn_in_are_near_grey() {
        // Lines draw as the grey of their ink's luminance, so a slight tint is all they lose.
        let slots = [Ink::Hi, Ink::Mid, Ink::Lo, Ink::Faint, Ink::Edge]
            .map(ink_slot)
            .into_iter()
            .chain([SLOT_LINE]);
        for slot in slots {
            let [r, g, b, _] = PALETTE[slot as usize];
            let spread = r.max(g).max(b) - r.min(g).min(b);
            assert!(spread <= 6.0 / 255.0, "slot {slot} is tinted by {spread}");
        }
    }

    #[test]
    fn a_crease_is_darker_than_a_seam() {
        let line = PALETTE[SLOT_LINE as usize];
        let seam = PALETTE[SLOT_EDGE as usize];
        assert!(ink(line) > ink(seam));
        assert!(ink(line) < 0.7, "the line is grey, not black");
    }

    #[test]
    fn a_belt_is_a_step_darker_than_a_plate_top() {
        let belt = ink(PALETTE[SLOT_BELT as usize]);
        assert!(
            belt > PAPER_TOP && belt < PAPER_LIT * 2.0,
            "belt ink {belt}"
        );
        assert!(ink(PALETTE[SLOT_BELT_EDGE as usize]) > belt);
    }

    #[test]
    fn a_shell_is_a_light_tint_of_its_agents_hue() {
        for slot in 0..AGENT_SLOTS {
            let hue = agent_colour(slot);
            let shell = tints()[TINT_SHELL0 as usize + slot];
            for channel in 0..3 {
                let share = (1.0 - shell[channel]) / (1.0 - hue[channel]);
                assert!(
                    (share - SHELL_MIX).abs() < 1e-5,
                    "slot {slot} channel {channel} mixes {share}, not {SHELL_MIX}"
                );
            }
        }
    }

    #[test]
    fn the_shader_constants_name_every_slot_it_reads() {
        let consts = wgsl_consts();
        for name in [
            "SLOT_PAGE",
            "SLOT_SHADOW",
            "SLOT_GRID",
            "SLOT_BELT",
            "SLOT_BELT_EDGE",
            "SLOT_LINE",
        ] {
            assert!(consts.contains(&format!("const {name}: u32")), "{name}");
        }
        assert!(consts.contains(&format!("const SLOT_LINE: u32 = {SLOT_LINE}u;")));
    }
}
