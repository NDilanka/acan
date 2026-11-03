//! The view-independent 3D model a machine produces. The renderer projects it, shades it and outlines it.
//!
//! Units are plan units with z up. A model's origin is its plan centre on the ground, so its base sits on z = 0.
//! Each level of detail holds only the geometry that appears at that level. A view draws `Always`, and adds
//! `Near` and `Close` as zoom allows.

/// A machine model: one mesh per level of detail, painted lines and dots, and the box that holds them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    /// One mesh per level of detail, indexed by `Detail as usize`.
    pub meshes: [Mesh; 3],
    /// Painted detail on surfaces: ticks, seams, needles and rulers. Silhouettes and creases are not authored.
    pub lines: Vec<Line3>,
    /// Screws, lamps and LEDs: discs lying on a surface.
    pub dots: Vec<Dot3>,
    /// The box that contains every mesh vertex, line point and dot.
    pub bounds: Aabb,
}

/// Triangles with per-vertex data. Indices form a triangle list, counter-clockwise seen from outside.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    /// Vertex positions.
    pub positions: Vec<[f32; 3]>,
    /// Unit normals. Smooth on curved surfaces, split at hard edges such as box corners and rims.
    pub normals: Vec<[f32; 3]>,
    /// How each vertex's surface is shaded.
    pub surfaces: Vec<Surface>,
    /// Triangle list: every three indices make one triangle.
    pub indices: Vec<u32>,
}

/// A polyline on a surface, such as a tick, a seam, a needle or a ruler.
#[derive(Debug, Clone, PartialEq)]
pub struct Line3 {
    /// The points in order. The line closes back to the first point when `closed` is set.
    pub points: Vec<[f32; 3]>,
    /// Whether the last point joins the first.
    pub closed: bool,
    /// The ink the line is drawn with.
    pub tone: Ink,
    /// The line weight in plan units at zoom 1.
    pub width: f32,
    /// The level of detail the line needs.
    pub detail: Detail,
}

/// A small disc lying on a surface, such as a screw head, a lamp or an LED.
#[derive(Debug, Clone, PartialEq)]
pub struct Dot3 {
    /// The centre of the disc.
    pub center: [f32; 3],
    /// The unit normal of the surface the disc lies on.
    pub normal: [f32; 3],
    /// The radius in plan units.
    pub radius: f32,
    /// The ink the disc is drawn with.
    pub tone: Ink,
    /// The level of detail the disc needs.
    pub detail: Detail,
}

/// How a vertex's surface is shaded. `Paper` takes the four-tone light shading from its normal. The rest are flat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// A painted solid, shaded from its normal.
    Paper,
    /// Amber, drawn flat. Reserved for waiting on the user, so no model draws it.
    Lit,
    /// A window or lens, drawn flat.
    Glass,
    /// A flat fill in an ink colour.
    Ink(Ink),
}

/// Ink tones from the kit. `Agent(slot)` is the only saturated ink, reserved for agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ink {
    /// The subject: the parts the reader is meant to read, and the pointers on them.
    Hi,
    /// Structure: the bodies, rings and major ticks around the subject.
    Mid,
    /// The base and the finest marks: feet, minor ticks and knurls.
    Lo,
    /// Grooves and creases.
    Faint,
    /// A seam: a light painted line where two parts of a surface meet. Lighter than the contours
    /// and creases the renderer strokes from the mesh.
    Edge,
    /// Amber. Reserved for waiting on the user, so no model draws it.
    Lit,
    /// A status colour.
    Red,
    /// A status colour.
    Green,
    /// A status colour.
    Blue,
    /// An agent's slot. The models do not use it.
    Agent(u8),
}

/// The level of detail an item needs. `Always` is the silhouette and main structure. `Near` adds fine parts that
/// show at moderate zoom. `Close` adds the finest parts, which show only when zoomed in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Detail {
    /// The silhouette and the main structure.
    #[default]
    Always,
    /// Fine parts that show at moderate zoom: major ticks, seams, screws and bands.
    Near,
    /// The finest parts: minor ticks, knurls and dot grids.
    Close,
}

/// An axis-aligned box.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Aabb {
    /// The low corner.
    pub min: [f32; 3],
    /// The high corner.
    pub max: [f32; 3],
}

impl Aabb {
    /// The empty box, which grows to fit the first point added to it.
    pub const EMPTY: Self = Self {
        min: [f32::INFINITY; 3],
        max: [f32::NEG_INFINITY; 3],
    };

    /// Grows the box to contain `p`.
    pub fn include(&mut self, p: [f32; 3]) {
        for (axis, &v) in p.iter().enumerate() {
            self.min[axis] = self.min[axis].min(v);
            self.max[axis] = self.max[axis].max(v);
        }
    }

    /// Centre of the box.
    pub fn centre(&self) -> [f32; 3] {
        std::array::from_fn(|axis| (self.min[axis] + self.max[axis]) * 0.5)
    }

    /// The eight corners.
    pub fn corners(&self) -> [[f32; 3]; 8] {
        std::array::from_fn(|i| {
            std::array::from_fn(|axis| {
                if i >> axis & 1 == 1 {
                    self.max[axis]
                } else {
                    self.min[axis]
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_levels_index_the_meshes_in_order() {
        assert_eq!(Detail::Always as usize, 0);
        assert_eq!(Detail::Near as usize, 1);
        assert_eq!(Detail::Close as usize, 2);
    }

    #[test]
    fn an_empty_box_grows_to_hold_its_points() {
        let mut bounds = Aabb::EMPTY;
        bounds.include([1.0, -2.0, 0.0]);
        bounds.include([-3.0, 4.0, 5.0]);
        assert_eq!(bounds.min, [-3.0, -2.0, 0.0]);
        assert_eq!(bounds.max, [1.0, 4.0, 5.0]);
        assert_eq!(bounds.centre(), [-1.0, 1.0, 2.5]);
        assert_eq!(bounds.corners().len(), 8);
    }
}
