//! The projects map in the corner of a canvas of several projects. The page draws each plant's box
//! north-up, with the ground the screen shows laid over it, and pans the camera where it is
//! clicked. This file holds the pure parts: the ground under the screen, the share of a plant that
//! is built, and the pose a pan eases to. `App` reads them out.

use acan_render::Camera;

use crate::tour;

/// How far from the pivot the view's footprint reaches, in eye distances. A low perspective view
/// sees ground almost to the horizon, so its top edge comes down to the row whose ground is this
/// close, and the map draws that edge as cut.
pub const FAR_EYES: f32 = 6.0;

/// Bisection steps on the screen row of a cut top edge. Twenty halve a viewport's height to well
/// under a pixel.
const ROW_STEPS: usize = 20;

/// The ground the screen shows: the plan points under the viewport's corners, in screen order top
/// left, top right, bottom right, bottom left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewQuad {
    pub corners: [[f32; 2]; 4],
    /// Whether the top edge came down from the top of the screen, because the top saw sky or
    /// ground past [`FAR_EYES`].
    pub cut: bool,
}

/// The ground under the screen's corners. The top edge drops to the highest screen row whose
/// ground at both ends lies within [`FAR_EYES`] eye distances of the pivot. `None` when the
/// bottom edge sees no ground that close, which no tilt the camera allows gives.
#[must_use]
pub fn view_quad(camera: &Camera) -> Option<ViewQuad> {
    let [width, height] = camera.viewport();
    let pivot = camera.pivot();
    let limit = FAR_EYES * camera.eye_distance();
    let near = |p: [f32; 2]| {
        p[0].is_finite() && p[1].is_finite() && (p[0] - pivot[0]).hypot(p[1] - pivot[1]) <= limit
    };
    let row = |y: f32| {
        let left = camera.ground_at([0.0, y]).filter(|&p| near(p))?;
        let right = camera.ground_at([width, y]).filter(|&p| near(p))?;
        Some([left, right])
    };
    let [bottom_left, bottom_right] = row(height)?;
    let ([top_left, top_right], cut) = match row(0.0) {
        Some(top) => (top, false),
        None => {
            // Rows nearer the bottom see nearer ground, so the rows that hold form one run down to
            // the bottom edge.
            let (mut sky, mut ground) = (0.0, height);
            for _ in 0..ROW_STEPS {
                let mid = (sky + ground) * 0.5;
                if row(mid).is_some() {
                    ground = mid;
                } else {
                    sky = mid;
                }
            }
            (row(ground)?, true)
        }
    };
    Some(ViewQuad {
        corners: [top_left, top_right, bottom_right, bottom_left],
        cut,
    })
}

/// The view as the page reads it: `[TL x, y, TR x, y, BR x, y, BL x, y, cut, pivot x, pivot y]`,
/// with `cut` 1 or 0, or empty when [`view_quad`] gives none.
#[must_use]
pub fn view_floats(camera: &Camera) -> Vec<f32> {
    let Some(quad) = view_quad(camera) else {
        return Vec::new();
    };
    let mut out: Vec<f32> = quad.corners.iter().flatten().copied().collect();
    out.push(if quad.cut { 1.0 } else { 0.0 });
    out.extend(camera.pivot());
    out
}

/// The share of a plant's `total` machines whose bodies are on the map, `hidden` naming the ones
/// that are not. A plant with no machine counts as built.
#[must_use]
pub fn built_share(total: usize, hidden: impl Fn(usize) -> bool) -> f32 {
    if total == 0 {
        return 1.0;
    }
    let shown = (0..total).filter(|&index| !hidden(index)).count();
    shown as f32 / total as f32
}

/// The pose a pan to the plan point `to` eases to: the camera's zoom with the pivot there. `None`
/// for a point that is not finite.
#[must_use]
pub fn pan_pose(camera: &Camera, to: [f32; 2]) -> Option<tour::Pose> {
    (to[0].is_finite() && to[1].is_finite()).then(|| (to, camera.zoom()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use acan_render::camera::{DEFAULT_AZIMUTH, DEFAULT_ELEVATION, Projection};

    use crate::topdown;

    const VIEWPORT: [f32; 2] = [1440.0, 900.0];

    fn camera(azimuth: f32, elevation: f32, projection: Projection) -> Camera {
        let mut camera = Camera::new(VIEWPORT);
        camera.set_projection(projection);
        camera.set_angles(azimuth, elevation);
        camera.set_pivot([40.0, -25.0]);
        camera.set_zoom(3.0);
        camera
    }

    fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
        [a[0] - b[0], a[1] - b[1]]
    }

    fn length(v: [f32; 2]) -> f32 {
        v[0].hypot(v[1])
    }

    fn cross(a: [f32; 2], b: [f32; 2]) -> f32 {
        a[0] * b[1] - a[1] * b[0]
    }

    fn screen_corners() -> [[f32; 2]; 4] {
        let [w, h] = VIEWPORT;
        [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]]
    }

    #[test]
    fn the_default_view_is_not_cut_and_each_corner_projects_back_to_its_screen_corner() {
        let camera = camera(DEFAULT_AZIMUTH, DEFAULT_ELEVATION, Projection::Perspective);
        let quad = view_quad(&camera).expect("the default view sees ground");
        assert!(!quad.cut);
        for (ground, screen) in quad.corners.iter().zip(screen_corners()) {
            let back = camera.world_to_screen([ground[0], ground[1], 0.0]);
            assert!(
                length(sub(back, screen)) < 0.5,
                "{ground:?} lands at {back:?}, not {screen:?}"
            );
        }
    }

    #[test]
    fn top_down_at_ninety_is_axis_aligned_centred_and_shaped_like_the_viewport() {
        let camera = camera(90.0, topdown::ELEVATION, Projection::Perspective);
        let quad = view_quad(&camera).expect("top-down sees ground");
        let [tl, tr, br, bl] = quad.corners;
        assert!(!quad.cut);
        let top = sub(tr, tl);
        let left = sub(bl, tl);
        assert!(top[1].abs() < 0.02 * top[0].abs(), "top edge {top:?}");
        assert!(left[0].abs() < 0.02 * left[1].abs(), "left edge {left:?}");
        let width = (length(top) + length(sub(br, bl))) * 0.5;
        let height = (length(left) + length(sub(br, tr))) * 0.5;
        let aspect = width / height;
        let want = VIEWPORT[0] / VIEWPORT[1];
        assert!(
            (aspect / want - 1.0).abs() < 0.03,
            "{aspect} against {want}"
        );
        let middle = [
            (tl[0] + tr[0] + br[0] + bl[0]) * 0.25,
            (tl[1] + tr[1] + br[1] + bl[1]) * 0.25,
        ];
        let off = length(sub(middle, camera.pivot()));
        assert!(
            off < 0.02 * width,
            "middle {middle:?} is {off} from the pivot"
        );
    }

    #[test]
    fn top_down_at_one_eighty_runs_the_top_edge_along_plus_y() {
        let camera = camera(180.0, topdown::ELEVATION, Projection::Perspective);
        let [tl, tr, _, _] = view_quad(&camera).expect("top-down sees ground").corners;
        let top = sub(tr, tl);
        assert!(
            top[1] > 0.0 && top[0].abs() < 0.02 * top[1],
            "top edge {top:?}"
        );
    }

    #[test]
    fn a_low_perspective_view_cuts_the_top_edge_within_the_far_limit() {
        let camera = camera(DEFAULT_AZIMUTH, 5.0, Projection::Perspective);
        let quad = view_quad(&camera).expect("the bottom edge sees ground");
        assert!(quad.cut);
        assert!(quad.corners.iter().flatten().all(|v| v.is_finite()));
        let limit = FAR_EYES * camera.eye_distance();
        for far in &quad.corners[..2] {
            let reach = length(sub(*far, camera.pivot()));
            assert!(
                reach <= limit * 1.0001,
                "{far:?} is {reach} out, past {limit}"
            );
        }
        let [tl, tr, br, bl] = quad.corners;
        assert!(
            length(sub(tr, tl)) > length(sub(br, bl)),
            "the far edge is wider"
        );
    }

    #[test]
    fn an_orthographic_view_is_a_parallelogram_and_not_cut_at_thirty_degrees() {
        let camera = camera(DEFAULT_AZIMUTH, 30.0, Projection::Orthographic);
        let quad = view_quad(&camera).expect("an orthographic view sees ground");
        assert!(!quad.cut);
        let [tl, tr, br, bl] = quad.corners;
        let (top, bottom) = (sub(tr, tl), sub(br, bl));
        let (left, right) = (sub(bl, tl), sub(br, tr));
        assert!(cross(top, bottom).abs() < 1e-3 * length(top) * length(bottom));
        assert!(cross(left, right).abs() < 1e-3 * length(left) * length(right));
    }

    #[test]
    fn doubling_the_zoom_halves_the_orthographic_width() {
        let mut camera = camera(DEFAULT_AZIMUTH, 30.0, Projection::Orthographic);
        let width = |camera: &Camera| {
            let [tl, tr, _, _] = view_quad(camera).expect("ground").corners;
            length(sub(tr, tl))
        };
        let before = width(&camera);
        camera.set_zoom(camera.zoom() * 2.0);
        let after = width(&camera);
        assert!((after / before - 0.5).abs() < 1e-3, "{before} then {after}");
    }

    #[test]
    fn the_floats_list_the_corners_the_cut_and_the_pivot() {
        let camera = camera(DEFAULT_AZIMUTH, 5.0, Projection::Perspective);
        let quad = view_quad(&camera).expect("ground");
        let floats = view_floats(&camera);
        assert_eq!(floats.len(), 11);
        assert_eq!(floats[..8], *quad.corners.as_flattened());
        assert_eq!(floats[8], 1.0);
        assert_eq!(floats[9..], camera.pivot());
    }

    #[test]
    fn the_built_share_counts_the_machines_on_the_map() {
        assert_eq!(built_share(0, |_| true), 1.0);
        assert_eq!(built_share(4, |_| true), 0.0);
        assert_eq!(built_share(4, |index| index == 2), 0.75);
        assert_eq!(built_share(4, |_| false), 1.0);
    }

    #[test]
    fn a_pan_keeps_the_zoom_and_moves_only_the_pivot() {
        let camera = camera(DEFAULT_AZIMUTH, DEFAULT_ELEVATION, Projection::Perspective);
        assert_eq!(
            pan_pose(&camera, [120.0, -8.5]),
            Some(([120.0, -8.5], camera.zoom()))
        );
        assert_eq!(pan_pose(&camera, [f32::NAN, 0.0]), None);
        assert_eq!(pan_pose(&camera, [0.0, f32::INFINITY]), None);
    }
}
