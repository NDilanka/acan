//! Rounding the corners of orthogonal belt paths. Belts that turn side by side turn as one
//! bundle: their corners share a centre, so the lanes stay parallel and in order through the
//! turn. A short jog between two opposite turns becomes one smooth S instead of two tight
//! corners.

use std::collections::HashMap;

/// Radius of a lone belt corner, and of the innermost lane of a bundle, in plan units.
pub const BELT_CORNER: f32 = 8.0;
/// Corners on one diagonal at most this far apart, along it, turn as one bundle.
const BUNDLE_GAP: f32 = 12.0;
/// A segment shorter than this between two opposite turns is a jog.
const JOG_MAX: f32 = 2.0 * BELT_CORNER;
/// The radius of the two arcs of a jog's S, when the belt has room for it.
const JOG_RADIUS: f32 = 2.0 * BELT_CORNER;
/// A bundle whose innermost corner would be tighter than this turns lane by lane instead.
const MIN_BUNDLE_CORNER: f32 = 2.0;
/// Segments of a quarter circle of `BELT_CORNER`. Wider turns get more, up to `MAX_ARC_STEPS`.
const CORNER_STEPS: usize = 4;
const MAX_ARC_STEPS: usize = 16;

/// How a path turns at one of its points.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Turn {
    /// An end or a point the path runs straight through.
    Point,
    /// A quarter circle of this radius.
    Arc(f32),
    /// The first corner of a jog, drawn as an S that reaches this far along the belt on
    /// either side.
    Jog(f32),
    /// The second corner of a jog. The S at the first corner covers it.
    JogEnd,
}

/// Each path with its corners rounded. The paths are orthogonal, and their ends stay put.
pub fn round_all(paths: &[Vec<[f32; 2]>]) -> Vec<Vec<[f32; 2]>> {
    let turns = plan(paths);
    paths
        .iter()
        .zip(&turns)
        .map(|(path, turns)| rounded(path, turns))
        .collect()
}

/// The directions into and out of a right-angle corner at point `i`.
fn corner(path: &[[f32; 2]], i: usize) -> Option<([f32; 2], [f32; 2])> {
    if i == 0 || i + 1 >= path.len() {
        return None;
    }
    let (a, p, b) = (path[i - 1], path[i], path[i + 1]);
    let (la, lb) = (distance(a, p), distance(p, b));
    if la < 1e-6 || lb < 1e-6 || cross(sub(p, a), sub(b, p)).abs() < 1e-6 {
        return None;
    }
    Some((scale(sub(p, a), 1.0 / la), scale(sub(b, p), 1.0 / lb)))
}

/// One of the four axis directions, as a small key.
fn heading(d: [f32; 2]) -> u8 {
    match (d[0].abs() >= d[1].abs(), d[0] + d[1] >= 0.0) {
        (true, true) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (false, false) => 3,
    }
}

/// One corner on a diagonal: how far out along it, and the path and point it belongs to.
#[derive(Clone, Copy)]
struct Lane {
    along: f32,
    path: usize,
    at: usize,
}

/// How each point of each path turns.
fn plan(paths: &[Vec<[f32; 2]>]) -> Vec<Vec<Turn>> {
    let mut turns: Vec<Vec<Turn>> = paths.iter().map(|p| vec![Turn::Point; p.len()]).collect();
    // How far along each adjacent segment a point's turn would like to reach.
    let mut reach: Vec<Vec<f32>> = paths.iter().map(|p| vec![0.0; p.len()]).collect();

    let mut jogs = Vec::new();
    for (k, path) in paths.iter().enumerate() {
        for i in 1..path.len().saturating_sub(2) {
            let (Some((into, _)), Some((_, out))) = (corner(path, i), corner(path, i + 1)) else {
                continue;
            };
            let short = distance(path[i], path[i + 1]) < JOG_MAX;
            if short && dot(into, out) > 0.99 && turns[k][i] == Turn::Point {
                turns[k][i] = Turn::Jog(0.0);
                turns[k][i + 1] = Turn::JogEnd;
                jogs.push((k, i));
            }
        }
    }

    // Corners that turn the same way on one diagonal are lanes of one bundle. Moving one lane
    // outward moves the corner along `into - out`, so `along` grows outward and `across` names
    // the diagonal.
    let mut diagonals: HashMap<(u8, u8, i64), Vec<Lane>> = HashMap::new();
    for (k, path) in paths.iter().enumerate() {
        for i in 0..path.len() {
            if turns[k][i] != Turn::Point {
                continue;
            }
            let Some((into, out)) = corner(path, i) else {
                continue;
            };
            let w = sub(into, out);
            let along = dot(path[i], w) * 0.5;
            let across = dot(path[i], [w[1], -w[0]]) * 0.5;
            let key = (heading(into), heading(out), (across * 2.0).round() as i64);
            diagonals.entry(key).or_default().push(Lane {
                along,
                path: k,
                at: i,
            });
        }
    }
    let mut bundles: Vec<Vec<Lane>> = Vec::new();
    for mut lanes in diagonals.into_values() {
        lanes.sort_by(|a, b| {
            a.along
                .total_cmp(&b.along)
                .then((a.path, a.at).cmp(&(b.path, b.at)))
        });
        let mut bundle: Vec<Lane> = Vec::new();
        for lane in lanes {
            if bundle
                .last()
                .is_some_and(|last| lane.along - last.along > BUNDLE_GAP)
            {
                bundles.push(std::mem::take(&mut bundle));
            }
            bundle.push(lane);
        }
        bundles.push(bundle);
    }
    for bundle in &bundles {
        let inner = bundle[0].along;
        for lane in bundle {
            reach[lane.path][lane.at] = BELT_CORNER + (lane.along - inner);
        }
    }

    for &(k, i) in &jogs {
        let path = &paths[k];
        let d = distance(path[i], path[i + 1]);
        let wanted = (JOG_RADIUS * d - d * d * 0.25).sqrt();
        let before = distance(path[i - 1], path[i]) - reach[k][i - 1];
        let after = distance(path[i + 1], path[i + 2]) - reach[k][i + 2];
        let h = wanted.min(before).min(after);
        if h > d * 0.5 + 0.5 {
            turns[k][i] = Turn::Jog(h);
            reach[k][i] = h;
            reach[k][i + 1] = h;
        } else {
            // No room for a smooth S, so the jog turns as two lone corners.
            for at in [i, i + 1] {
                turns[k][at] = Turn::Point;
                reach[k][at] = BELT_CORNER;
                bundles.push(vec![Lane {
                    along: 0.0,
                    path: k,
                    at,
                }]);
            }
        }
    }

    // A corner may use only its share of each segment it touches, so the turns at a
    // segment's two ends never overlap. A bundle keeps one centre while every lane fits, and
    // otherwise its lanes turn as lone corners.
    let fit = |lane: &Lane| {
        let (path, i) = (&paths[lane.path], lane.at);
        let wanted = reach[lane.path][i];
        let share = |other: usize| {
            let length = distance(path[other], path[i]);
            (length / (wanted + reach[lane.path][other])).min(1.0)
        };
        share(i - 1).min(share(i + 1)) * wanted
    };
    for bundle in &bundles {
        let inner = bundle[0].along;
        let base = bundle
            .iter()
            .map(|lane| fit(lane) - (lane.along - inner))
            .fold(f32::INFINITY, f32::min);
        for lane in bundle {
            let radius = if base >= MIN_BUNDLE_CORNER {
                base + (lane.along - inner)
            } else {
                fit(lane).min(BELT_CORNER)
            };
            turns[lane.path][lane.at] = Turn::Arc(radius);
        }
    }
    turns
}
/// The path with each corner replaced as `turns` says.
fn rounded(path: &[[f32; 2]], turns: &[Turn]) -> Vec<[f32; 2]> {
    let mut curve = Vec::with_capacity(path.len() * (CORNER_STEPS + 1));
    let mut push = |q: [f32; 2]| {
        if curve.last().is_none_or(|&last| distance(q, last) > 1e-4) {
            curve.push(q);
        }
    };
    for (i, &p) in path.iter().enumerate() {
        match (turns[i], corner(path, i)) {
            (Turn::Arc(r), Some((into, out))) => {
                let centre = add(p, add(scale(into, -r), scale(out, r)));
                let steps = ((r / BELT_CORNER * CORNER_STEPS as f32).ceil() as usize)
                    .clamp(CORNER_STEPS, MAX_ARC_STEPS);
                for s in 0..=steps {
                    let phi = std::f32::consts::FRAC_PI_2 * s as f32 / steps as f32;
                    push(add(
                        centre,
                        add(scale(out, -r * phi.cos()), scale(into, r * phi.sin())),
                    ));
                }
            }
            (Turn::Jog(h), Some((into, _))) => {
                jog(p, path[i + 1], into, h).into_iter().for_each(&mut push)
            }
            (Turn::JogEnd, _) => {}
            _ => push(p),
        }
    }
    curve
}

/// An S from `h` before `from` to `h` past `to`, along `into`: two arcs that meet halfway
/// along the jog, each tangent to the belt where it leaves it.
fn jog(from: [f32; 2], to: [f32; 2], into: [f32; 2], h: f32) -> Vec<[f32; 2]> {
    let d = distance(from, to);
    let side = scale(sub(to, from), 1.0 / d);
    let radius = (h * h + d * d * 0.25) / d;
    let sweep = (h / radius).asin();
    let start = add(from, scale(into, -h));
    let centre = add(start, scale(side, radius));
    let middle = scale(add(from, to), 0.5);
    let first: Vec<[f32; 2]> = (0..=CORNER_STEPS)
        .map(|s| {
            let phi = sweep * s as f32 / CORNER_STEPS as f32;
            add(
                centre,
                add(
                    scale(side, -radius * phi.cos()),
                    scale(into, radius * phi.sin()),
                ),
            )
        })
        .collect();
    // The second arc mirrors the first through the middle of the jog.
    let second = first
        .iter()
        .rev()
        .skip(1)
        .map(|&q| sub(scale(middle, 2.0), q));
    first.iter().copied().chain(second).collect()
}

fn add(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] + b[0], a[1] + b[1]]
}

fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

fn scale(a: [f32; 2], s: f32) -> [f32; 2] {
    [a[0] * s, a[1] * s]
}

fn dot(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

fn cross(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[1] - a[1] * b[0]
}

fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    let v = sub(b, a);
    v[0].hypot(v[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest change of heading between consecutive segments, in degrees.
    fn sharpest(path: &[[f32; 2]]) -> f32 {
        path.windows(3)
            .map(|w| {
                let (a, b) = (sub(w[1], w[0]), sub(w[2], w[1]));
                cross(a, b).atan2(dot(a, b)).abs().to_degrees()
            })
            .fold(0.0, f32::max)
    }

    fn nearest(p: [f32; 2], path: &[[f32; 2]]) -> f32 {
        path.windows(2)
            .map(|w| {
                let (a, b) = (w[0], w[1]);
                let ab = sub(b, a);
                let t = (dot(sub(p, a), ab) / dot(ab, ab).max(1e-9)).clamp(0.0, 1.0);
                distance(p, add(a, scale(ab, t)))
            })
            .fold(f32::INFINITY, f32::min)
    }

    #[test]
    fn rounding_keeps_the_endpoints_of_a_belt() {
        let path = vec![[0.0, 0.0], [50.0, 0.0], [50.0, 40.0]];
        let curve = &round_all(std::slice::from_ref(&path))[0];
        assert_eq!(curve.first(), Some(&[0.0, 0.0]));
        assert_eq!(curve.last(), Some(&[50.0, 40.0]));
        assert!(sharpest(curve) < 25.0, "the corner is rounded");
    }

    #[test]
    fn lanes_that_turn_side_by_side_stay_one_step_apart() {
        // Three belts run right, turn down and turn right again, each lane one step outside
        // the last through both turns.
        let lane = |k: f32| {
            vec![
                [0.0, 100.0 - 8.0 * k],
                [60.0 + 8.0 * k, 100.0 - 8.0 * k],
                [60.0 + 8.0 * k, 160.0 - 8.0 * k],
                [140.0, 160.0 - 8.0 * k],
            ]
        };
        let paths: Vec<Vec<[f32; 2]>> = (0..3).map(|k| lane(k as f32)).collect();
        let curves = round_all(&paths);
        for pair in curves.windows(2) {
            for &p in &pair[1] {
                let gap = nearest(p, &pair[0]);
                assert!((gap - 8.0).abs() < 0.3, "lanes {gap} apart at {p:?}");
            }
        }
    }

    #[test]
    fn a_short_jog_is_one_smooth_s() {
        let path = vec![[0.0, 0.0], [40.0, 0.0], [40.0, 3.0], [100.0, 3.0]];
        let curve = &round_all(std::slice::from_ref(&path))[0];
        assert!(sharpest(curve) < 15.0, "no kink: {curve:?}");
        assert_eq!(curve.first(), Some(&[0.0, 0.0]));
        assert_eq!(curve.last(), Some(&[100.0, 3.0]));
        let middle = nearest([40.0, 1.5], curve);
        assert!(middle < 0.05, "the S passes the middle of the jog");
    }

    #[test]
    fn corners_on_a_short_segment_share_it() {
        let path = vec![[0.0, 0.0], [20.0, 0.0], [20.0, 10.0], [0.0, 10.0]];
        let curve = &round_all(std::slice::from_ref(&path))[0];
        assert!(
            curve.iter().all(|p| p[0] <= 20.0 + 1e-3),
            "a U turn stays inside its corners"
        );
        assert_eq!(curve.last(), Some(&[0.0, 10.0]));
    }
}
