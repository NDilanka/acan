//! The shading rule the renderer and the review sheet share. Models supply normals and this rule turns them into tones.
//!
//! The light is fixed relative to the camera, so the look is the same at every azimuth. Faces that point up are
//! top faces. Side faces take one of four tones by how square they face the light, the lightest facing it.
//!
//! The camera at azimuth `az` looks from `(cos az, sin az)` in plan. The screen's right is `(-sin az, cos az)`, so
//! the light comes from the right. At 45 degrees the visible sides are +x (darkest) and +y (lightest).

/// The tone a face takes under the shading rule. The renderer maps each tone to a colour in its theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// A face pointing up.
    Top,
    /// The lightest side, facing the light.
    Shade3,
    /// A side turned partly from the light.
    Shade2,
    /// A side turned mostly from the light.
    Shade1,
    /// The darkest side, facing away from the light.
    Shade0,
}

/// Normals with a z above this are top faces.
const TOP_Z: f32 = 0.7;

/// The tone of a face with unit `normal`, for a camera at `azimuth` radians. Elevation does not change the tone.
#[must_use]
pub fn tone_for(normal: [f32; 3], azimuth: f32) -> Tone {
    let [x, y, z] = normal;
    if z > TOP_Z {
        return Tone::Top;
    }
    if z < -TOP_Z {
        return Tone::Shade0;
    }
    let horizontal = x.hypot(y);
    let (sin, cos) = azimuth.sin_cos();
    let facing = (x * -sin + y * cos) / horizontal;
    let lit = ((facing + 1.0) / 2.0 * 4.0).floor().clamp(0.0, 3.0);
    match lit as usize {
        0 => Tone::Shade0,
        1 => Tone::Shade1,
        2 => Tone::Shade2,
        _ => Tone::Shade3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::{Base, Finish, Part};
    use crate::plan::Plan;
    use std::f32::consts::FRAC_PI_4;

    #[test]
    fn a_box_at_the_default_view_shows_the_kit_tones() {
        let azimuth = 45.0_f32.to_radians();
        let elevation = 30.0_f32.to_radians();
        let square = Base::Plan(
            Plan {
                x: -1.0,
                y: -1.0,
                w: 2.0,
                d: 2.0,
                r: 0.0,
            },
            1,
        );
        let mut part = Part::new();
        part.slab(&square, 0.0, 1.0, 0.0, Finish::paper());
        let model = part.finish();
        let mesh = &model.meshes[0];
        let toward = [
            azimuth.cos() * elevation.cos(),
            azimuth.sin() * elevation.cos(),
            elevation.sin(),
        ];
        let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let mut tones = Vec::new();
        for tri in mesh.indices.chunks_exact(3) {
            let p: Vec<[f32; 3]> = tri.iter().map(|&i| mesh.positions[i as usize]).collect();
            let (u, v) = (sub(p[1], p[0]), sub(p[2], p[0]));
            let facing = [
                u[1] * v[2] - u[2] * v[1],
                u[2] * v[0] - u[0] * v[2],
                u[0] * v[1] - u[1] * v[0],
            ];
            if facing.iter().zip(toward).map(|(a, b)| a * b).sum::<f32>() > 0.0 {
                tones.push(tone_for(mesh.normals[tri[0] as usize], azimuth));
            }
        }
        // The top's four fan triangles, and two triangles on each of the two sides the camera sees.
        assert_eq!(tones.len(), 8, "visible tones {tones:?}");
        assert_eq!(tones.iter().filter(|&&t| t == Tone::Top).count(), 4);
        assert_eq!(tones.iter().filter(|&&t| t == Tone::Shade0).count(), 2);
        assert_eq!(tones.iter().filter(|&&t| t == Tone::Shade3).count(), 2);
    }

    #[test]
    fn the_side_facing_the_light_is_the_lightest_at_every_azimuth() {
        for quarter in 0..4 {
            let azimuth = FRAC_PI_4 + quarter as f32 * std::f32::consts::FRAC_PI_2;
            let (sin, cos) = azimuth.sin_cos();
            let light_face = [-sin, cos, 0.0];
            let dark_face = [sin, -cos, 0.0];
            assert_eq!(
                tone_for(light_face, azimuth),
                Tone::Shade3,
                "azimuth {azimuth}"
            );
            assert_eq!(
                tone_for(dark_face, azimuth),
                Tone::Shade0,
                "azimuth {azimuth}"
            );
        }
    }

    #[test]
    fn steep_faces_are_top_and_the_underside_is_darkest() {
        assert_eq!(tone_for([0.0, 0.0, 0.8], 0.0), Tone::Top);
        assert_eq!(tone_for([0.0, 0.0, -1.0], 0.0), Tone::Shade0);
    }
}
