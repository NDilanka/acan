//! The light theme: a white page, neutral greys and fine grey line art. Colours live here and
//! nowhere in a model. The shader reads the slot numbers and the tuning constants from the
//! WGSL that `wgsl_consts` writes, so the two never disagree.

use crate::model::{Ink, Surface};

/// Palette slots on the GPU.
pub const PALETTE_LEN: usize = 23;
/// Tint slots. Slot 0 is the identity; instances pick a slot to tint what they draw. The slots from
/// [`TINT_DIM0`] up are dimmed copies of the plain ones below it.
pub const TINT_LEN: usize = 40;
/// The first dimmed tint slot: `TINT_DIM0 + slot` is the plain slot `slot` darkened by
/// [`DIM_SHARE`]. A machine a tour step does not stage takes its dimmed slot.
pub const TINT_DIM0: u32 = 20;
/// The share of its colour each dimmed slot keeps.
pub const DIM_SHARE: f32 = 0.4;
const _: () = assert!(
    TINT_DIM0 as usize * 2 == TINT_LEN,
    "every plain tint slot has a dimmed copy"
);
/// Agent hues, as `#rrggbb`. Only agents are saturated on the page. Each hue holds at least 4.5:1
/// against white, so white text reads on it, and none is red, amber or green, which mean a clash,
/// waiting and a pass.
pub const AGENT_HUES: [u32; AGENT_SLOTS] =
    [0x0f7a6e, 0x6a45c8, 0x2763c4, 0x9c3c86, 0x0b6f94, 0x5d6b12];
pub const AGENT_SLOTS: usize = 6;
/// The share of the clash red a machine with an open clash takes, mixed into white. It is the only
/// colour a machine takes from the live work: a machine stays grey until it is abnormal.
pub const CLASH_MIX: f32 = 0.14;
/// The clash red, the diamond's colour.
pub const CLASH_RED: u32 = 0xd0453a;
/// The grey of a stale drone's eye, which has gone quiet while it holds a task.
pub const STALE_GREY: u32 = 0xc4c4c8;
/// The grey of a ghost: the outline a landed machine keeps for a returning reader.
pub const GHOST_GREY: u32 = 0x6e6e73;
/// Tint slot of each agent's accent at full hue: `TINT_BODY0 + slot`. A working drone's eye, its
/// beam and its beacon draw with it, and an idle drone's eye with its dimmed copy.
pub const TINT_BODY0: u32 = 1;
/// Tint slot of a drone's shadow on the plinth top: black, which the spot's own opacity thins.
pub const TINT_SHADOW: u32 = 7;
/// Tint slot of a stale drone's eye. The drone's body is white in every state.
pub const TINT_STALE: u32 = 13;
/// Tint slot of a ghost's outline.
pub const TINT_GHOST: u32 = 14;
/// Tint slot of a machine with an open clash.
pub const TINT_CLASH: u32 = 15;
/// Tint slot of a scope outline for a plot: amber at full strength. A draft and a confirmed plot
/// both outline in it.
pub const TINT_OUTLINE_AMBER: u32 = 8;
/// Tint slot of a scope outline for a keep: brass at full strength. A draft and a confirmed keep
/// both outline in it.
pub const TINT_OUTLINE_BRASS: u32 = 9;
/// Tint slots of a confirmed scope's body, in [`SCOPE_TINTS`] order: a plot, then a keep.
/// `TINT_SCOPE0 + i` is the i-th.
pub const TINT_SCOPE0: u32 = 16;
/// The share of each confirmed lane's hue mixed into white for its machine's body: a plot takes
/// 28 percent amber and a keep 36 percent brass. A draft takes no body tint; it shows its outline
/// only, so it reads as not yet in force.
pub const SCOPE_TINTS: [f32; 2] = [0.28, 0.36];

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
/// A district's ground pad, at its alpha, and the fine line round it. Both are a faint step off
/// the desk in the look's own neutral, never a signal hue: a district is a place, not a state.
pub const SLOT_PAD: u32 = 21;
pub const SLOT_PAD_EDGE: u32 = 22;
/// The pad's outline width in device pixels, and its corner radius in plan units.
pub const PAD_EDGE_PX: f32 = 1.0;
pub const PAD_RADIUS: f32 = 16.0;
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
/// Opacity of a grid line. The look's grid colour is its `SLOT_GRID` palette entry, and that
/// entry's alpha scales these. A level is minor while the finer level under it is still hidden and
/// turns major as that level appears. A pixel takes the strongest level, never the sum, so the grid
/// stays under the machine line art.
pub const GRID_MINOR_ALPHA: f32 = 0.10;
pub const GRID_MAJOR_ALPHA: f32 = 0.16;

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
    rgb(CLASH_RED, 1.0), // red: the clash pin
    rgb(0x30a46c, 1.0),  // green
    rgb(0x3e63dd, 1.0),  // blue
    rgb(AGENT_HUES[0], 1.0),
    rgb(AGENT_HUES[1], 1.0),
    rgb(AGENT_HUES[2], 1.0),
    rgb(AGENT_HUES[3], 1.0),
    rgb(0xe9e9e9, 1.0),  // belt
    rgb(0xb8b8b8, 1.0),  // belt edge
    rgb(0x767676, 1.0),  // contour and crease
    rgb(0xa8a8a8, 1.0),  // seam
    rgb(0x000000, 0.03), // district pad, at its alpha
    rgb(0x000000, 0.13), // district pad edge, at its alpha
];

/// A look the page shows. Only the desk changes: the grid, the shadow and the page colour. The
/// plant's paper, line art and the agent hues stay the same, so an agent keeps its colour when the
/// look changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// Light paper: the page's own look.
    Paper,
    Dark,
    Blueprint,
    HighContrast,
}

impl Look {
    /// The look a settings value names: `paper`, `dark`, `blueprint` or `high_contrast`.
    pub fn from_name(name: &str) -> Option<Look> {
        match name {
            "paper" => Some(Look::Paper),
            "dark" => Some(Look::Dark),
            "blueprint" => Some(Look::Blueprint),
            "high_contrast" => Some(Look::HighContrast),
            _ => None,
        }
    }

    /// The look's palette, sRGB with straight alpha.
    pub fn colours(self) -> [[f32; 4]; PALETTE_LEN] {
        match self {
            Look::Paper => PALETTE,
            Look::Dark => DARK,
            Look::Blueprint => BLUEPRINT,
            Look::HighContrast => HIGH_CONTRAST,
        }
    }

    /// The ghost grey: a ghost drone's outline on the desk.
    fn ghost_grey(self) -> u32 {
        match self {
            Look::Paper => GHOST_GREY,
            Look::Dark => 0xa0a0a5,
            Look::Blueprint => 0xb4b4b8,
            Look::HighContrast => 0x4a4a4f,
        }
    }

    /// The scope amber: a plot's outline, and the hue of a confirmed plot's body.
    fn scope_amber(self) -> u32 {
        match self {
            Look::Paper => 0xd99a1e,
            Look::Dark => 0xf0ad28,
            Look::Blueprint => 0xffbe3b,
            Look::HighContrast => 0xb87400,
        }
    }

    /// The scope brass: a keep's outline, and the hue of a confirmed keep's body.
    fn scope_brass(self) -> u32 {
        match self {
            Look::Paper => 0x9c7f2e,
            Look::Dark => 0xc9a64a,
            Look::Blueprint => 0xd9c07a,
            Look::HighContrast => 0x6b5418,
        }
    }
}

/// Dark: a dark desk with a light grid. The paper models stay white. The belts are a grey well
/// under the machines' faces, so the white machines stand out on the desk.
const DARK: [[f32; 4]; PALETTE_LEN] = [
    rgb(0x262624, 1.0),  // page
    rgb(0xd99a1e, 1.0),  // lit
    rgb(0xf2f2f2, 1.0),  // glass
    rgb(0x000000, 0.70), // contact shadow
    rgb(0xffffff, 0.8),  // grid, at its alpha
    rgb(0x26262a, 1.0),  // hi
    rgb(0x8a8a8f, 1.0),  // mid
    rgb(0xb4b4b8, 1.0),  // lo
    rgb(0xd4d4d6, 1.0),  // faint
    rgb(0x3a3a3e, 1.0),  // dot
    rgb(CLASH_RED, 1.0), // red: the clash pin
    rgb(0x30a46c, 1.0),  // green
    rgb(0x3e63dd, 1.0),  // blue
    rgb(AGENT_HUES[0], 1.0),
    rgb(AGENT_HUES[1], 1.0),
    rgb(AGENT_HUES[2], 1.0),
    rgb(AGENT_HUES[3], 1.0),
    rgb(0x6b6b70, 1.0),  // belt
    rgb(0x4e4e53, 1.0),  // belt edge
    rgb(0x767676, 1.0),  // contour and crease
    rgb(0xa8a8a8, 1.0),  // seam
    rgb(0xffffff, 0.04), // district pad, at its alpha
    rgb(0xffffff, 0.16), // district pad edge, at its alpha
];

/// Blueprint: a blue desk with a light grid.
const BLUEPRINT: [[f32; 4]; PALETTE_LEN] = [
    rgb(0x1f3a5c, 1.0),  // page
    rgb(0xd99a1e, 1.0),  // lit
    rgb(0xf2f2f2, 1.0),  // glass
    rgb(0x0a1626, 0.35), // contact shadow
    rgb(0x8cbef0, 0.7),  // grid, at its alpha: a pale blue line on the blue desk
    rgb(0x26324f, 1.0),  // hi
    rgb(0x8a94b4, 1.0),  // mid
    rgb(0xb4bcd8, 1.0),  // lo
    rgb(0xd4d8e8, 1.0),  // faint
    rgb(0x3a3a3e, 1.0),  // dot
    rgb(CLASH_RED, 1.0), // red: the clash pin
    rgb(0x30a46c, 1.0),  // green
    rgb(0x3e63dd, 1.0),  // blue
    rgb(AGENT_HUES[0], 1.0),
    rgb(AGENT_HUES[1], 1.0),
    rgb(AGENT_HUES[2], 1.0),
    rgb(AGENT_HUES[3], 1.0),
    rgb(0xe9e9e9, 1.0),  // belt
    rgb(0xb8b8b8, 1.0),  // belt edge
    rgb(0x76829c, 1.0),  // contour and crease
    rgb(0xa8b0c8, 1.0),  // seam
    rgb(0x8cbef0, 0.07), // district pad, at its alpha
    rgb(0x8cbef0, 0.3),  // district pad edge, at its alpha
];

/// High contrast: a white desk with a black grid and darker line art.
const HIGH_CONTRAST: [[f32; 4]; PALETTE_LEN] = [
    rgb(0xffffff, 1.0),  // page
    rgb(0xd99a1e, 1.0),  // lit
    rgb(0xf2f2f2, 1.0),  // glass
    rgb(0x000000, 0.14), // contact shadow
    rgb(0x000000, 1.0),  // grid, at its alpha
    rgb(0x000000, 1.0),  // hi
    rgb(0x555555, 1.0),  // mid
    rgb(0x808080, 1.0),  // lo
    rgb(0xa6a6a6, 1.0),  // faint
    rgb(0x000000, 1.0),  // dot
    rgb(CLASH_RED, 1.0), // red: the clash pin
    rgb(0x30a46c, 1.0),  // green
    rgb(0x3e63dd, 1.0),  // blue
    rgb(AGENT_HUES[0], 1.0),
    rgb(AGENT_HUES[1], 1.0),
    rgb(AGENT_HUES[2], 1.0),
    rgb(AGENT_HUES[3], 1.0),
    rgb(0xe6e6e6, 1.0),  // belt
    rgb(0x6b6b6b, 1.0),  // belt edge
    rgb(0x505050, 1.0),  // contour and crease
    rgb(0x707070, 1.0),  // seam
    rgb(0x000000, 0.04), // district pad, at its alpha
    rgb(0x000000, 0.35), // district pad edge, at its alpha
];

/// Palette slot of an ink used by a line.
pub fn ink_slot(ink: Ink) -> u32 {
    match ink {
        Ink::Hi => SLOT_HI,
        Ink::Mid => SLOT_MID,
        Ink::Lo => SLOT_LO,
        Ink::Faint => SLOT_FAINT,
        Ink::Edge => SLOT_EDGE,
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
    palette_of(Look::Paper, srgb_surface)
}

/// The palette of a look, as the surface wants it.
pub fn palette_of(look: Look, srgb_surface: bool) -> [[f32; 4]; PALETTE_LEN] {
    look.colours()
        .map(|c| if srgb_surface { to_linear(c) } else { c })
}

/// The tint table in sRGB. Slot 0 is the identity, each agent has a body slot at full hue, and the
/// shadow, stale, ghost, clash, outline and scope slots follow.
pub fn tints() -> [[f32; 4]; TINT_LEN] {
    tints_of(Look::Paper)
}

/// The tint table of a look, in sRGB. The ghost grey and the scope colours differ between looks.
pub fn tints_of(look: Look) -> [[f32; 4]; TINT_LEN] {
    let mut table = [[1.0; 4]; TINT_LEN];
    for slot in 0..AGENT_SLOTS {
        let hue = agent_colour(slot);
        table[TINT_BODY0 as usize + slot] = hue;
    }
    table[TINT_SHADOW as usize] = rgb(0x000000, 1.0);
    table[TINT_OUTLINE_AMBER as usize] = rgb(look.scope_amber(), 1.0);
    table[TINT_OUTLINE_BRASS as usize] = rgb(look.scope_brass(), 1.0);
    table[TINT_STALE as usize] = rgb(STALE_GREY, 1.0);
    table[TINT_GHOST as usize] = rgb(look.ghost_grey(), 1.0);
    table[TINT_CLASH as usize] = mixed_into_white(rgb(CLASH_RED, 1.0), CLASH_MIX);
    let hues = [look.scope_amber(), look.scope_brass()];
    for (i, share) in SCOPE_TINTS.into_iter().enumerate() {
        table[TINT_SCOPE0 as usize + i] = mixed_into_white(rgb(hues[i], 1.0), share);
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
    page_of(Look::Paper, srgb_surface)
}

/// The page colour of a look, for the clear.
pub fn page_of(look: Look, srgb_surface: bool) -> [f32; 4] {
    palette_of(look, srgb_surface)[SLOT_PAGE as usize]
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
        u("SLOT_PAD", SLOT_PAD),
        u("SLOT_PAD_EDGE", SLOT_PAD_EDGE),
        f("PAD_EDGE_PX", PAD_EDGE_PX),
        f("PAD_RADIUS", PAD_RADIUS),
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
    TINT_BODY0 as usize + AGENT_SLOTS <= TINT_SHADOW as usize
        && TINT_SHADOW < TINT_OUTLINE_AMBER
        && TINT_OUTLINE_AMBER < TINT_OUTLINE_BRASS
        && TINT_OUTLINE_BRASS < TINT_STALE
        && TINT_STALE < TINT_GHOST
        && TINT_GHOST < TINT_CLASH
        && TINT_CLASH < TINT_SCOPE0
        && TINT_SCOPE0 as usize + SCOPE_TINTS.len() <= TINT_DIM0 as usize,
    "body, shadow, outline, stale, ghost, clash and scope tints do not overlap"
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
            Ink::Lit,
            Ink::Red,
            Ink::Green,
            Ink::Blue,
            Ink::Agent(0),
            Ink::Agent(3),
        ];
        let slots: Vec<u32> = inks.iter().map(|&ink| ink_slot(ink)).collect();
        assert!(slots.iter().all(|&s| (s as usize) < PALETTE_LEN));
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

    const LOOKS: [Look; 4] = [Look::Paper, Look::Dark, Look::Blueprint, Look::HighContrast];

    #[test]
    fn the_grid_stays_under_the_line_art() {
        for look in LOOKS {
            let palette = look.colours();
            // The grid reads against its desk, and its opacity scales the line ink.
            let grid = palette[SLOT_GRID as usize];
            assert!(
                contrast(grid, palette[SLOT_PAGE as usize]) >= 2.0,
                "{look:?} grid reads on the desk"
            );
            let major = GRID_MAJOR_ALPHA * grid[3];
            let faintest_line = [SLOT_EDGE, SLOT_LINE, SLOT_BELT_EDGE]
                .map(|slot| ink(palette[slot as usize]))
                .into_iter()
                .chain([CONTOUR_FAR_ALPHA * ink(palette[SLOT_LINE as usize])])
                .fold(f32::INFINITY, f32::min);
            assert!(
                major < faintest_line,
                "{look:?}: major grid ink {major} must stay under every line, {faintest_line}"
            );
        }
    }

    #[test]
    fn the_inks_lines_are_drawn_in_are_near_grey() {
        // Lines draw as the grey of their ink's luminance, so a slight tint is all they lose.
        for look in LOOKS {
            let palette = look.colours();
            let slots = [Ink::Hi, Ink::Mid, Ink::Lo, Ink::Faint, Ink::Edge]
                .map(ink_slot)
                .into_iter()
                .chain([SLOT_LINE]);
            // A blueprint's lines are blue-grey: the red channel, which the ink reads, is the paper's.
            let limit = if look == Look::Blueprint {
                48.0 / 255.0
            } else {
                6.0 / 255.0
            };
            for slot in slots {
                let [r, g, b, _] = palette[slot as usize];
                let spread = r.max(g).max(b) - r.min(g).min(b);
                assert!(
                    spread <= limit,
                    "{look:?} slot {slot} is tinted by {spread}"
                );
                if look == Look::Blueprint {
                    assert_eq!(
                        r, PALETTE[slot as usize][0],
                        "blueprint slot {slot} keeps its ink"
                    );
                }
            }
        }
    }

    #[test]
    fn a_crease_is_darker_than_a_seam() {
        for look in LOOKS {
            let palette = look.colours();
            let line = palette[SLOT_LINE as usize];
            let seam = palette[SLOT_EDGE as usize];
            assert!(ink(line) > ink(seam), "{look:?}");
            assert!(ink(line) < 0.7, "{look:?}: the line is grey, not black");
        }
    }

    #[test]
    fn a_belt_sits_under_the_plate_tops_with_a_darker_edge() {
        for look in LOOKS {
            let palette = look.colours();
            let belt = ink(palette[SLOT_BELT as usize]);
            // Every belt is below a plate top and stays a grey, never black.
            assert!(belt > PAPER_TOP && belt < 0.7, "{look:?} belt ink {belt}");
            assert!(ink(palette[SLOT_BELT_EDGE as usize]) > belt, "{look:?}");
            if look == Look::Dark {
                // On the dark desk the belt is softer than the machines: darker than the darkest
                // plate face, and still clear of the desk.
                assert!(belt > PAPER_DARK, "{look:?} belt ink {belt}");
                assert!(
                    contrast(palette[SLOT_BELT as usize], palette[SLOT_PAGE as usize]) >= 2.0,
                    "{look:?} belt reads on the desk"
                );
            } else {
                // On a light desk a belt is one step below a plate top.
                assert!(belt < PAPER_LIT * 2.0, "{look:?} belt ink {belt}");
            }
        }
    }

    #[test]
    fn a_district_pad_is_a_faint_neutral_step_off_the_desk() {
        for look in LOOKS {
            let palette = look.colours();
            let page = palette[SLOT_PAGE as usize];
            let pad = palette[SLOT_PAD as usize];
            let edge = palette[SLOT_PAD_EDGE as usize];
            // The pad tints the desk only faintly, and its outline is stronger than its fill but
            // still a quiet line.
            assert!(
                pad[3] > 0.0 && pad[3] <= 0.08,
                "{look:?} pad alpha {}",
                pad[3]
            );
            assert!(
                edge[3] > pad[3] && edge[3] <= 0.4,
                "{look:?} pad edge alpha"
            );
            for colour in [pad, edge] {
                let [r, g, b, _] = colour;
                let spread = r.max(g).max(b) - r.min(g).min(b);
                // Grey on the paper looks and the dark desk; the blueprint's own pale blue.
                let limit = if look == Look::Blueprint {
                    0.5
                } else {
                    1.0 / 255.0
                };
                assert!(spread <= limit, "{look:?} pad is tinted by {spread}");
                assert!(!(r > g + 0.2 && r > b + 0.2), "{look:?} a pad is never red");
            }
            // On a dark desk the pad lightens it; on a light desk it darkens it.
            let lighter = pad[0] > page[0];
            assert_eq!(
                lighter,
                page[0] < 0.5,
                "{look:?} pad steps away from the desk"
            );
        }
    }

    #[test]
    fn every_look_keeps_the_agent_hues_and_the_clash_red() {
        for look in LOOKS {
            let palette = look.colours();
            for slot in [
                SLOT_RED,
                SLOT_AGENT0,
                SLOT_AGENT0 + 1,
                SLOT_AGENT0 + 2,
                SLOT_AGENT0 + 3,
            ] {
                assert_eq!(
                    palette[slot as usize], PALETTE[slot as usize],
                    "{look:?} slot {slot}"
                );
            }
        }
    }

    #[test]
    fn a_look_name_maps_to_its_look() {
        assert_eq!(Look::from_name("paper"), Some(Look::Paper));
        assert_eq!(Look::from_name("dark"), Some(Look::Dark));
        assert_eq!(Look::from_name("blueprint"), Some(Look::Blueprint));
        assert_eq!(Look::from_name("high_contrast"), Some(Look::HighContrast));
        assert_eq!(Look::from_name("sepia"), None);
    }

    #[test]
    fn the_paper_look_is_the_palette_the_page_has_always_had() {
        assert_eq!(palette_of(Look::Paper, false), palette(false));
        assert_eq!(tints_of(Look::Paper), tints());
        assert_eq!(page_of(Look::Paper, false), page(false));
    }

    /// The WCAG contrast ratio of two sRGB colours.
    fn contrast(a: [f32; 4], b: [f32; 4]) -> f32 {
        let luminance = |c: [f32; 4]| {
            let l = to_linear(c);
            0.2126 * l[0] + 0.7152 * l[1] + 0.0722 * l[2]
        };
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn every_agent_hue_holds_white_text() {
        let white = rgb(0xffffff, 1.0);
        for slot in 0..AGENT_SLOTS {
            let ratio = contrast(agent_colour(slot), white);
            assert!(ratio >= 4.5, "slot {slot} is {ratio}:1 against white");
        }
    }

    #[test]
    fn no_agent_hue_reads_as_the_clash_waiting_or_pass_colour() {
        let hue_of = |c: [f32; 4]| {
            let (max, min) = (c[0].max(c[1]).max(c[2]), c[0].min(c[1]).min(c[2]));
            let d = max - min;
            let h = if max == c[0] {
                ((c[1] - c[2]) / d).rem_euclid(6.0)
            } else if max == c[1] {
                (c[2] - c[0]) / d + 2.0
            } else {
                (c[0] - c[1]) / d + 4.0
            };
            60.0 * h
        };
        for slot in 0..AGENT_SLOTS {
            let h = hue_of(agent_colour(slot));
            for (name, signal) in [("red", 0.0_f32), ("amber", 40.0), ("green", 120.0)] {
                let apart = (h - signal).abs().min(360.0 - (h - signal).abs());
                assert!(apart >= 20.0, "slot {slot} at {h} reads as {name}");
            }
        }
    }

    #[test]
    fn a_machine_takes_only_a_faint_clash_red_and_the_marks_greys_are_neutral() {
        let table = tints();
        let clash = table[TINT_CLASH as usize];
        assert!(
            clash[0] > clash[1] && clash[0] > clash[2],
            "the clash tint is red"
        );
        assert!(ink(clash) < 0.05 && clash[1] > 0.85, "and faint: {clash:?}");
        for look in LOOKS {
            let table = tints_of(look);
            for slot in [TINT_STALE, TINT_GHOST] {
                let [r, g, b, _] = table[slot as usize];
                let spread = r.max(g).max(b) - r.min(g).min(b);
                assert!(spread <= 6.0 / 255.0, "{look:?} slot {slot} is grey");
            }
        }
    }

    #[test]
    fn scope_outlines_are_full_strength_and_a_confirmed_body_is_its_hue_mixed_into_white() {
        for look in LOOKS {
            let table = tints_of(look);
            let amber = table[TINT_OUTLINE_AMBER as usize];
            let brass = table[TINT_OUTLINE_BRASS as usize];
            assert_eq!(amber, rgb(look.scope_amber(), 1.0), "{look:?} amber");
            assert_eq!(brass, rgb(look.scope_brass(), 1.0), "{look:?} brass");
            let plot = table[TINT_SCOPE0 as usize];
            let keep = table[TINT_SCOPE0 as usize + 1];
            assert_eq!(
                plot,
                mixed_into_white(rgb(look.scope_amber(), 1.0), SCOPE_TINTS[0])
            );
            assert_eq!(
                keep,
                mixed_into_white(rgb(look.scope_brass(), 1.0), SCOPE_TINTS[1])
            );
        }
        let table = tints();
        let amber = table[TINT_OUTLINE_AMBER as usize];
        let plot = table[TINT_SCOPE0 as usize];
        assert!(
            plot[1] > amber[1] && plot[2] > amber[2],
            "a body tint is the outline's hue mixed toward white"
        );
    }

    #[test]
    fn an_idle_eye_is_its_agents_hue_dimmed_and_a_stale_eye_grey() {
        let table = tints();
        for slot in 0..AGENT_SLOTS {
            let bright = table[TINT_BODY0 as usize + slot];
            let dim = table[(TINT_DIM0 + TINT_BODY0) as usize + slot];
            assert_eq!(bright, agent_colour(slot), "a working eye is the full hue");
            for channel in 0..3 {
                assert!((dim[channel] - DIM_SHARE * bright[channel]).abs() < 1e-6);
            }
        }
        let stale = table[TINT_STALE as usize];
        assert!(
            (stale[0] - stale[2]).abs() <= 6.0 / 255.0,
            "a stale eye is grey"
        );
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
            "SLOT_PAD",
            "SLOT_PAD_EDGE",
        ] {
            assert!(consts.contains(&format!("const {name}: u32")), "{name}");
        }
        assert!(consts.contains(&format!("const SLOT_LINE: u32 = {SLOT_LINE}u;")));
    }
}
