use crate::model::{Aabb, Detail};

/// One machine on the plan: a model kind placed at an offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Instance {
    /// Where the model's origin sits. The model's plan centre is on the ground at `z = 0`.
    pub offset: [f32; 3],
    /// Index into the models given to `Renderer::set_models`.
    pub kind: u16,
    /// Index into the theme's tint slots. Slot 0 is the identity.
    pub tint: u8,
    /// A screen-size floor for a small mark: the model grows about its origin until it is at
    /// least `grow[0]` device px tall. `grow[1]` is its height in plan units. `[0.0; 2]` draws
    /// the model at plan size.
    pub grow: [f32; 2],
    /// The angle, in radians, the model turns about its origin, anticlockwise from +x. Its
    /// normals turn with it. `0.0` draws the model as it was built.
    pub turn: f32,
}

/// A straight thin line in space, or a soft spot, in one tint, drawn like a hue line. It is for a mark that links
/// two points, such as a drone's beam to its target, a piece of a drone's trail, or a side of a
/// ghost's outline. Each carries its own width and opacity, so a trail can fade piece by piece. Its start is measured from `origin` and turns
/// and grows about it as an instance does, so a beam that starts on a drone stays on the drone's
/// eye while the drone grows. A beam between two world points has zero `origin`, zero `grow` and
/// zero `turn`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Beam {
    /// The start, in the frame of `origin` before `turn` and `grow` apply.
    pub from: [f32; 3],
    /// The end, in world units. It is not turned or grown.
    pub to: [f32; 3],
    /// The point the start is measured from, in world units.
    pub origin: [f32; 3],
    /// How the start grows about `origin`, as an instance's `grow` does.
    pub grow: [f32; 2],
    /// The angle the start turns about `origin`, as an instance's `turn` does.
    pub turn: f32,
    /// Index into the theme's tint slots, as an instance's tint is.
    pub tint: u8,
    /// The stroke's opacity, from 0 to 1. The tint's own alpha is not read.
    pub alpha: f32,
    /// The stroke's width in device px, before the shader's thinnest-stroke floor.
    pub width_px: f32,
    /// When above zero, the beam is a soft spot instead of a stroke: a flat round disc of this
    /// radius in plan units, lying level and centred on the start, that holds its middle and
    /// fades toward its rim. It grows with the start, and `to` and `width_px` are not read. A
    /// drone's shadow is a spot.
    pub spot: f32,
}

/// A belt: a flat opaque ribbon lying on the ground along a centre line, with a fine line
/// along its sides and across its ends. The edge pass reads it as ground.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ribbon {
    /// Plan points of the centre line, with any rounded corners already in it.
    pub centre: Vec<[f32; 2]>,
    /// Full width in plan units.
    pub width: f32,
}

/// A district's ground: a faint rounded rectangle lying on the ground with a fine outline, under
/// the belts and the machines.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pad {
    /// `[min x, min y, max x, max y]` on the plan.
    pub rect: [f32; 4],
}

/// A thin line along a belt's centre, in one tint. It lies on the ground and fades toward both
/// ends, so it reads as the belt's colour without covering it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HueLine {
    /// Plan points of the centre line.
    pub centre: Vec<[f32; 2]>,
    /// Index into the theme's tint slots, as an instance's tint is.
    pub tint: u8,
}

/// What the renderer draws, set once per plant change.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    /// District pads, drawn on the ground under everything else.
    pub pads: Vec<Pad>,
    /// Belts, drawn in order. A later ribbon covers an earlier one where they cross.
    pub ribbons: Vec<Ribbon>,
    pub instances: Vec<Instance>,
    /// Thin lines along belt centres, drawn over the ground and under nothing that stands in front.
    pub hue_lines: Vec<HueLine>,
    /// Beams in space, drawn with the hue lines.
    pub beams: Vec<Beam>,
    /// Model kinds whose instances also draw in their tint where geometry in front hides them, as
    /// a faint outline. Nothing else is drawn that way.
    pub silhouettes: Vec<u16>,
    /// Box around everything in the scene, used to size the depth range.
    pub bounds: Aabb,
}

/// Level of detail, picked from zoom relative to the fit zoom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Lod {
    Far = 0,
    Near = 1,
    Close = 2,
}

impl Lod {
    /// Zoom divided by fit zoom at which `Near` detail appears.
    pub const NEAR_RATIO: f32 = 1.0;
    /// Zoom divided by fit zoom at which `Close` detail appears.
    pub const CLOSE_RATIO: f32 = 2.5;

    pub fn for_ratio(ratio: f32) -> Self {
        if ratio >= Self::CLOSE_RATIO {
            Self::Close
        } else if ratio >= Self::NEAR_RATIO {
            Self::Near
        } else {
            Self::Far
        }
    }

    /// Whether detail authored at `detail` is drawn at this level.
    pub fn shows(self, detail: Detail) -> bool {
        detail as u8 <= self as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_switch_at_the_documented_ratios() {
        assert_eq!(Lod::for_ratio(0.99), Lod::Far);
        assert_eq!(Lod::for_ratio(1.0), Lod::Near);
        assert_eq!(Lod::for_ratio(2.49), Lod::Near);
        assert_eq!(Lod::for_ratio(2.5), Lod::Close);
    }

    #[test]
    fn each_level_shows_its_own_detail_and_the_ones_below() {
        assert!(Lod::Far.shows(Detail::Always));
        assert!(!Lod::Far.shows(Detail::Near));
        assert!(Lod::Near.shows(Detail::Near));
        assert!(!Lod::Near.shows(Detail::Close));
        assert!(Lod::Close.shows(Detail::Close));
    }
}
