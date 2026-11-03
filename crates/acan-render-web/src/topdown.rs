//! The top-down view: the camera looks straight down at the plant, at the azimuth and projection
//! the reader already has. The page turns to it with `V` or its button, and the Map setting's
//! "Top-down by default" turns to it at startup. This file holds the pure parts. `App` runs the
//! ease.

use acan_render::camera::MAX_ELEVATION;

use crate::tour;

/// The elevation of the top-down view, in degrees. The camera allows no more than this, which is
/// one degree short of straight down, so a building's sides shrink to nothing in plan.
pub const ELEVATION: f32 = MAX_ELEVATION;

/// How long a turn to top-down, or back, takes, in milliseconds. The same as a tour move.
pub const MOVE_MS: f32 = tour::MOVE_MS;

/// Whether an elevation counts as top-down. A half degree of slack keeps a view that the clamp
/// has already brought to the limit counted.
#[must_use]
pub fn is_top_down(elevation: f32) -> bool {
    elevation >= ELEVATION - 0.5
}

/// The elevation `elapsed_ms` into a turn from `from` to `to`. It eases out like a tour move and
/// lands exactly on `to` once the move has run its time.
#[must_use]
pub fn elevation(from: f32, to: f32, elapsed_ms: f32) -> f32 {
    let t = (elapsed_ms.max(0.0) / MOVE_MS).min(1.0);
    if t >= 1.0 {
        return to;
    }
    let eased = 1.0 - (1.0 - t).powi(3);
    from + (to - from) * eased
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
        assert_eq!(elevation(from, to, 0.0), from);
        let mut last = from;
        for step in 1..=12 {
            let now = elevation(from, to, MOVE_MS * step as f32 / 12.0);
            assert!(now >= last, "the turn goes one way: {last} then {now}");
            last = now;
        }
        assert_eq!(elevation(from, to, MOVE_MS), to);
        assert_eq!(elevation(from, to, MOVE_MS * 3.0), to);
    }

    #[test]
    fn a_turn_back_to_the_default_tilt_eases_down() {
        let mid = elevation(ELEVATION, 30.0, MOVE_MS / 2.0);
        assert!(mid < ELEVATION && mid > 30.0, "{mid}");
        assert_eq!(elevation(ELEVATION, 30.0, MOVE_MS), 30.0);
    }
}
