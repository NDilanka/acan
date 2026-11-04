//! Plan shapes on the floor: rounded rectangles and circles, built the way the kit builds them.

use std::f32::consts::TAU;

pub(crate) type Point = [f32; 2];

/// A rectangle on the floor with rounded corners. `r` is clamped to half the shorter side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Plan {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) w: f32,
    pub(crate) d: f32,
    pub(crate) r: f32,
}

impl Plan {
    /// The outline, corner by corner: each corner is an arc of `steps` segments, and a square corner is one point.
    pub(crate) fn ring(&self, steps: usize) -> Vec<Point> {
        let Plan { x, y, w, d, r } = *self;
        let radius = r.min(w / 2.0).min(d / 2.0).max(0.0);
        let count = if radius > 0.0 { steps } else { 0 };
        let corners = [
            (x + w - radius, y + radius, -90.0_f32),
            (x + w - radius, y + d - radius, 0.0),
            (x + radius, y + d - radius, 90.0),
            (x + radius, y + radius, 180.0),
        ];
        let mut out = Vec::with_capacity(4 * (count + 1));
        for (cx, cy, start) in corners {
            if count == 0 {
                out.push([cx, cy]);
                continue;
            }
            for step in 0..=count {
                let angle = (start + 90.0 * step as f32 / count as f32).to_radians();
                out.push([cx + radius * angle.cos(), cy + radius * angle.sin()]);
            }
        }
        out
    }

    /// The plan moved in by `by` on every side, with its radius reduced to match.
    pub(crate) fn inset(&self, by: f32) -> Self {
        Self {
            x: self.x + by,
            y: self.y + by,
            w: self.w - 2.0 * by,
            d: self.d - 2.0 * by,
            r: (self.r - by).max(0.0),
        }
    }
}

/// A circle as `steps` points, starting at angle zero and going round once.
pub(crate) fn circle_ring(cx: f32, cy: f32, radius: f32, steps: usize) -> Vec<Point> {
    (0..steps)
        .map(|step| {
            let angle = step as f32 / steps as f32 * TAU;
            [cx + radius * angle.cos(), cy + radius * angle.sin()]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_points(actual: &[Point], expected: &[Point]) {
        assert_eq!(actual.len(), expected.len(), "point count: {actual:?}");
        for (a, e) in actual.iter().zip(expected) {
            assert!(
                (a[0] - e[0]).abs() < 1e-4 && (a[1] - e[1]).abs() < 1e-4,
                "{a:?} != {e:?}"
            );
        }
    }

    #[test]
    fn rounded_plan_has_four_arcs_of_steps_plus_one_points() {
        let ring = Plan {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            d: 6.0,
            r: 2.0,
        }
        .ring(2);
        assert_eq!(ring.len(), 12);
        let expected = [
            [8.0, 0.0],
            [9.414_213, 0.585_786],
            [10.0, 2.0],
            [10.0, 4.0],
            [9.414_213, 5.414_213],
            [8.0, 6.0],
            [2.0, 6.0],
            [0.585_786, 5.414_213],
            [0.0, 4.0],
            [0.0, 2.0],
            [0.585_786, 0.585_786],
            [2.0, 0.0],
        ];
        assert_points(&ring, &expected);
    }

    #[test]
    fn rounded_plan_starts_and_ends_on_its_low_edge() {
        let plan = Plan {
            x: 3.0,
            y: -2.0,
            w: 10.0,
            d: 6.0,
            r: 2.0,
        };
        let ring = plan.ring(8);
        assert_eq!(ring.len(), 4 * 9);
        assert_eq!(ring.first().map(|p| p[1]), Some(plan.y));
        assert_eq!(ring.last().map(|p| p[1]), Some(plan.y));
    }

    #[test]
    fn square_corners_are_four_points() {
        let ring = Plan {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            d: 6.0,
            r: 0.0,
        }
        .ring(4);
        assert_points(&ring, &[[10.0, 0.0], [10.0, 6.0], [0.0, 6.0], [0.0, 0.0]]);
    }

    #[test]
    fn radius_is_clamped_to_half_the_short_side() {
        let ring = Plan {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            d: 6.0,
            r: 50.0,
        }
        .ring(2);
        assert!(
            ring.iter()
                .all(|p| (0.0..=10.0).contains(&p[0]) && (0.0..=6.0).contains(&p[1]))
        );
    }

    #[test]
    fn inset_shrinks_size_and_radius() {
        let plan = Plan {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            d: 6.0,
            r: 2.0,
        }
        .inset(1.5);
        assert_eq!(
            plan,
            Plan {
                x: 1.5,
                y: 1.5,
                w: 7.0,
                d: 3.0,
                r: 0.5
            }
        );
    }

    #[test]
    fn circle_has_its_steps_on_the_circle() {
        assert_points(
            &circle_ring(0.0, 0.0, 2.0, 4),
            &[[2.0, 0.0], [0.0, 2.0], [-2.0, 0.0], [0.0, -2.0]],
        );
        let ring = circle_ring(1.0, -1.0, 3.0, 40);
        assert_eq!(ring.len(), 40);
        for p in ring {
            assert!(((p[0] - 1.0).hypot(p[1] + 1.0) - 3.0).abs() < 1e-4);
        }
    }
}
