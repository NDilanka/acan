//! Several projects on one canvas. Each project's plant is laid out from the origin and moved to
//! its place, so the camera, the scene and the marks work in one world. The page packs the places
//! and keeps them; this module only answers questions about the placed bounds.

use serde::Deserialize;

/// The scan's revision, which a laid-out plant is cached by, or `None` when the plant has none.
pub fn revision(json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Head {
        revision: Option<String>,
    }
    serde_json::from_str::<Head>(json).ok()?.revision
}

/// The box round every `[min x, min y, max x, max y]` box, or `None` for no box.
pub fn union(boxes: &[[f32; 4]]) -> Option<[f32; 4]> {
    boxes.iter().copied().reduce(|a, b| {
        [
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ]
    })
}

/// The index of the box nearest the plan point `p`: one that holds it, or else the one whose edge
/// is closest. `None` for no box.
pub fn nearest(boxes: &[[f32; 4]], p: [f32; 2]) -> Option<usize> {
    boxes
        .iter()
        .map(|b| {
            let dx = (b[0] - p[0]).max(p[0] - b[2]).max(0.0);
            let dy = (b[1] - p[1]).max(p[1] - b[3]).max(0.0);
            dx.hypot(dy)
        })
        .enumerate()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::layout;
    use crate::plant::{from_scan_json, stress};

    const FIXTURE: &str = include_str!("../www/fixtures/demo-plant.json");

    /// The page's gap between two packed plants, in plan units.
    const GAP: f32 = 240.0;

    #[test]
    fn a_baked_offset_moves_every_footprint_belt_and_the_bounds() {
        let plant = from_scan_json(FIXTURE).expect("fixture parses");
        let (kinds, edges) = plant.layout_input();
        let laid = layout(&kinds, &edges);
        let by = [1000.0, -40.0];
        let moved = laid.translated(by);
        assert_eq!(moved.machines.len(), laid.machines.len());
        for (a, b) in laid.machines.iter().zip(&moved.machines) {
            let (fa, fb) = (a.footprint(), b.footprint());
            assert_eq!(
                [fb[0] - fa[0], fb[1] - fa[1], fb[2] - fa[2], fb[3] - fa[3]],
                [by[0], by[1], by[0], by[1]]
            );
            assert_eq!(a.kind, b.kind);
        }
        for (a, b) in laid.belts.iter().zip(&moved.belts) {
            assert_eq!((a.from, a.to, a.imports), (b.from, b.to, b.imports));
            for (pa, pb) in a.path.iter().zip(&b.path) {
                assert_eq!([pb[0] - pa[0], pb[1] - pa[1]], by);
            }
        }
        let [x0, y0, x1, y1] = laid.bounds;
        assert_eq!(
            moved.bounds,
            [x0 + by[0], y0 + by[1], x1 + by[0], y1 + by[1]]
        );
        assert_eq!(laid.translated([0.0, 0.0]), laid, "no offset, no change");
    }

    #[test]
    fn two_plants_packed_with_a_gap_do_not_meet_and_their_union_spans_both() {
        let demo = from_scan_json(FIXTURE).expect("fixture parses");
        let big = stress(60);
        let lay = |plant: &crate::plant::Plant| {
            let (kinds, edges) = plant.layout_input();
            layout(&kinds, &edges)
        };
        let (a, b) = (lay(&demo), lay(&big));
        // Left to right: the second starts one gap past the first's right edge, tops aligned.
        let a_at = a.translated([-a.bounds[0], -a.bounds[1]]);
        let b_at = b.translated([a_at.bounds[2] + GAP - b.bounds[0], -b.bounds[1]]);
        assert_eq!(b_at.bounds[0] - a_at.bounds[2], GAP);
        for placed in &b_at.machines {
            let f = placed.footprint();
            assert!(f[0] > a_at.bounds[2], "{f:?} reaches into the first plant");
        }
        let all = union(&[a_at.bounds, b_at.bounds]).expect("two boxes");
        let a_w = a.bounds[2] - a.bounds[0];
        let b_w = b.bounds[2] - b.bounds[0];
        assert_eq!(all[0], 0.0);
        assert_eq!(all[1], 0.0);
        assert!((all[2] - (a_w + GAP + b_w)).abs() < 1e-3);
        assert_eq!(
            all[3],
            (a.bounds[3] - a.bounds[1]).max(b.bounds[3] - b.bounds[1])
        );
        assert_eq!(union(&[]), None);
    }

    #[test]
    fn the_nearest_box_holds_the_point_or_is_closest_to_it() {
        let boxes = [[0.0, 0.0, 100.0, 50.0], [340.0, 0.0, 500.0, 80.0]];
        assert_eq!(nearest(&boxes, [50.0, 25.0]), Some(0));
        assert_eq!(nearest(&boxes, [400.0, 10.0]), Some(1));
        assert_eq!(
            nearest(&boxes, [200.0, 10.0]),
            Some(0),
            "nearer the first's edge"
        );
        assert_eq!(nearest(&boxes, [260.0, 10.0]), Some(1));
        assert_eq!(nearest(&[], [0.0, 0.0]), None);
    }

    #[test]
    fn the_revision_is_read_when_the_plant_has_one() {
        assert_eq!(
            revision(r#"{"revision": "abc", "root": "x"}"#).as_deref(),
            Some("abc")
        );
        assert_eq!(revision(r#"{"root": "x"}"#), None);
        assert_eq!(revision("not json"), None);
    }
}
