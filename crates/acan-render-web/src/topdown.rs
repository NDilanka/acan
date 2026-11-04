//! The top-down view: the camera looks straight down at the plant. The page turns to it with `V` or
//! its button, and the Map setting's "Top-down by default" turns to it at startup. On the turn the
//! azimuth snaps to a grid step, so the grid lines up with the screen edges; Q and E and a
//! horizontal orbit drag turn it a step at a time. This file holds the pure parts. `App` runs the
//! eases.

use acan_render::camera::MAX_ELEVATION;

use crate::tour;

/// The elevation of the top-down view, in degrees. The camera allows no more than this, which is
/// one degree short of straight down, so a building's sides shrink to nothing in plan.
pub const ELEVATION: f32 = MAX_ELEVATION;

/// How long a turn to top-down, or back, or a grid step, takes, in milliseconds. The same as a tour
/// move.
pub const MOVE_MS: f32 = tour::MOVE_MS;

/// One grid step of the azimuth, in degrees. At every multiple of it the grid lines run along the
/// screen edges.
pub const STEP_DEG: f32 = 90.0;

/// Whether an elevation counts as top-down. A half degree of slack keeps a view that the clamp
/// has already brought to the limit counted.
#[must_use]
pub fn is_top_down(elevation: f32) -> bool {
    elevation >= ELEVATION - 0.5
}

/// The azimuth of the grid step nearest `azimuth`, in degrees. A tie, halfway between two steps,
/// goes to the later one, so the answer is the same whichever way the view was turned.
#[must_use]
pub fn snap(azimuth: f32) -> f32 {
    (azimuth / STEP_DEG + 0.5).floor() * STEP_DEG
}

/// The value `elapsed_ms` into an eased move from `from` to `to`. It eases out like a tour move and
/// lands exactly on `to` once the move has run its time.
#[must_use]
pub fn ease(from: f32, to: f32, elapsed_ms: f32) -> f32 {
    let t = (elapsed_ms.max(0.0) / MOVE_MS).min(1.0);
    if t >= 1.0 {
        return to;
    }
    let eased = 1.0 - (1.0 - t).powi(3);
    from + (to - from) * eased
}

/// How far the screen points reach past the edges: `top` is the least y a point may have, and
/// `right` the greatest x. The left edge is the screen's own, at x = 0. Returns `[above, beyond,
/// left]`, the reach past each, in the same units, zero where every point is inside.
#[must_use]
pub fn overlap(points: &[[f32; 2]], top: f32, right: f32) -> [f32; 3] {
    let mut above = 0.0_f32;
    let mut beyond = 0.0_f32;
    let mut left = 0.0_f32;
    for &[x, y] in points {
        above = above.max(top - y);
        beyond = beyond.max(x - right);
        left = left.max(-x);
    }
    [above, beyond, left]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_counts_as_top_down_and_a_tilt_short_of_it_does_not() {
        assert!(is_top_down(ELEVATION));
        assert!(is_top_down(ELEVATION - 0.4));
        assert!(!is_top_down(ELEVATION - 1.0));
        assert!(!is_top_down(30.0));
    }

    #[test]
    fn the_turn_starts_at_from_eases_up_and_lands_on_to() {
        let (from, to) = (30.0, ELEVATION);
        assert_eq!(ease(from, to, 0.0), from);
        let mut last = from;
        for step in 1..=12 {
            let now = ease(from, to, MOVE_MS * step as f32 / 12.0);
            assert!(now >= last, "the turn goes one way: {last} then {now}");
            last = now;
        }
        assert_eq!(ease(from, to, MOVE_MS), to);
        assert_eq!(ease(from, to, MOVE_MS * 3.0), to);
    }

    #[test]
    fn a_turn_back_to_the_default_tilt_eases_down() {
        let mid = ease(ELEVATION, 30.0, MOVE_MS / 2.0);
        assert!(mid < ELEVATION && mid > 30.0, "{mid}");
        assert_eq!(ease(ELEVATION, 30.0, MOVE_MS), 30.0);
    }

    #[test]
    fn the_snap_takes_the_nearest_grid_step_and_a_tie_goes_to_the_later_one() {
        assert_eq!(snap(45.0), 90.0, "a tie goes to the later step");
        assert_eq!(snap(44.9), 0.0);
        assert_eq!(snap(90.0), 90.0);
        assert_eq!(snap(134.0), 90.0);
        assert_eq!(snap(136.0), 180.0);
        assert_eq!(snap(-50.0), -90.0);
        assert_eq!(snap(-10.0), 0.0);
    }

    #[test]
    fn the_overlap_is_how_far_points_reach_past_the_edges() {
        assert_eq!(overlap(&[[10.0, 100.0]], 60.0, 250.0), [0.0, 0.0, 0.0]);
        assert_eq!(
            overlap(&[[10.0, 40.0], [300.0, 200.0]], 60.0, 250.0),
            [20.0, 50.0, 0.0]
        );
        assert_eq!(
            overlap(&[[-30.0, 100.0], [20.0, 90.0]], 60.0, 250.0),
            [0.0, 0.0, 30.0]
        );
    }
}
