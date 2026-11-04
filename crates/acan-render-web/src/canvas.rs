//! Several projects on one canvas. Each project's plant is laid out from the origin and moved to
//! its place, so the camera, the scene and the marks work in one world. The page packs the places
//! and keeps them; this module only answers questions about the placed bounds.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use acan_render::Model;

use crate::layout::Layout;
use crate::plant::Plant;
use crate::scene;

/// The key a plant's layout is cached by: a digest of exactly what the layout reads, so two
/// projects share a layout only when their kinds and belts are the same.
pub fn layout_key(plant: &Plant) -> u64 {
    let mut hasher = DefaultHasher::new();
    format!("{:?}", plant.layout_input()).hash(&mut hasher);
    hasher.finish()
}

/// The plan box `[min x, min y, max x, max y]` the plant draws in: every footprint, every model
/// past its footprint and every belt. A model can reach past its footprint, so the gap between
/// two packed plants is measured from what is drawn, not from the footprints alone.
pub fn drawn_bounds(laid: &Layout, models: &[Model]) -> [f32; 4] {
    let drawn = scene::plant_scene(laid, models).bounds;
    let b = laid.bounds;
    [
        b[0].min(drawn.min[0]),
        b[1].min(drawn.min[1]),
        b[2].max(drawn.max[0]),
        b[3].max(drawn.max[1]),
    ]
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

/// A plant narrower than this on screen, in CSS px, draws its marks at the far level of detail.
pub const FAR_WIDTH_CSS: f32 = 160.0;

/// Whether a project's marks draw at the far level of detail: its plant's box on screen, `[min
/// x, min y, max x, max y]` in CSS px, is narrower than [`FAR_WIDTH_CSS`] or lies wholly outside
/// the `viewport` (CSS width and height). Far marks keep their drones, pins, tints and state, and
/// drop their flags, names, trails, tethers, beams and rings.
pub fn far(screen_box: [f32; 4], viewport: [f32; 2]) -> bool {
    let [x0, y0, x1, y1] = screen_box;
    if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) {
        return true;
    }
    let outside = x1 < 0.0 || y1 < 0.0 || x0 > viewport[0] || y0 > viewport[1];
    outside || x1 - x0 < FAR_WIDTH_CSS
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
        let (kinds, edges, districts) = plant.layout_input();
        let laid = layout(&kinds, &edges, &districts);
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
            let (kinds, edges, districts) = plant.layout_input();
            layout(&kinds, &edges, &districts)
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

    /// The live shop project's plant: two TypeScript folders, the tests importing the source.
    const SHOP: &str = r#"{
        "root": "shop",
        "revision": "9eb2346761e5d754",
        "machines": [
            {"id": 0, "name": "src", "dir": "src", "files": [0, 1, 2, 3]},
            {"id": 1, "name": "test", "dir": "test", "files": [4]}
        ],
        "belts": [{"from": 1, "to": 0, "imports": 1}],
        "files": [
            {"id": 0, "path": "src/api.ts"},
            {"id": 1, "path": "src/cart.ts"},
            {"id": 2, "path": "src/checkout.ts"},
            {"id": 3, "path": "src/pricing.ts"},
            {"id": 4, "path": "test/checkout.test.ts"}
        ]
    }"#;

    /// The page's `pack` in projects.js: each layout's origin, left to right, one gap apart.
    fn pack(boxes: &[[f32; 4]], gap: f32) -> Vec<[f32; 2]> {
        let mut cursor = 0.0;
        boxes
            .iter()
            .map(|b| {
                let place = [cursor - b[0], -b[1]];
                cursor += b[2] - b[0] + gap;
                place
            })
            .collect()
    }

    /// Every drawn box of a placed plant on the plan: each machine's model box and each belt.
    fn drawn_boxes(laid: &Layout, models: &[Model]) -> Vec<[f32; 4]> {
        let mut out: Vec<[f32; 4]> = scene::machine_model_boxes(laid, models)
            .iter()
            .map(|b| [b.min[0], b.min[1], b.max[0], b.max[1]])
            .collect();
        for belt in &laid.belts {
            for pair in belt.path.windows(2) {
                let [a, b] = [pair[0], pair[1]];
                out.push([
                    a[0].min(b[0]),
                    a[1].min(b[1]),
                    a[0].max(b[0]),
                    a[1].max(b[1]),
                ]);
            }
        }
        out
    }

    #[test]
    fn the_demo_and_shop_plants_packed_by_their_drawn_bounds_never_overlap() {
        let models = scene::models();
        let lay = |json: &str| {
            let plant = from_scan_json(json).expect("fixture parses");
            let (kinds, edges, districts) = plant.layout_input();
            layout(&kinds, &edges, &districts)
        };
        let (demo, shop) = (lay(FIXTURE), lay(SHOP));
        let boxes = [drawn_bounds(&demo, &models), drawn_bounds(&shop, &models)];
        for (laid, drawn) in [(&demo, boxes[0]), (&shop, boxes[1])] {
            let b = laid.bounds;
            assert!(drawn[0] <= b[0] && drawn[1] <= b[1] && drawn[2] >= b[2] && drawn[3] >= b[3]);
        }
        let places = pack(&boxes, GAP);
        let demo_at = demo.translated(places[0]);
        let shop_at = shop.translated(places[1]);
        let demo_drawn = drawn_bounds(&demo_at, &models);
        let shop_drawn = drawn_bounds(&shop_at, &models);
        assert!(
            (shop_drawn[0] - (demo_drawn[2] + GAP)).abs() < 1e-3,
            "shop starts at {} but demo ends at {}",
            shop_drawn[0],
            demo_drawn[2]
        );
        for a in drawn_boxes(&demo_at, &models) {
            for b in drawn_boxes(&shop_at, &models) {
                let apart = a[2] < b[0] || b[2] < a[0] || a[3] < b[1] || b[3] < a[1];
                assert!(apart, "demo's {a:?} meets shop's {b:?}");
                assert!(b[0] - a[2] >= GAP - 1e-3, "{b:?} is within a gap of {a:?}");
            }
        }
    }

    #[test]
    fn plants_share_a_cached_layout_only_when_what_the_layout_reads_is_the_same() {
        let demo = from_scan_json(FIXTURE).expect("fixture parses");
        let shop = from_scan_json(SHOP).expect("shop parses");
        let renamed = from_scan_json(&SHOP.replace("\"root\": \"shop\"", "\"root\": \"notes\""))
            .expect("renamed parses");
        assert_eq!(layout_key(&shop), layout_key(&renamed));
        assert_ne!(layout_key(&shop), layout_key(&demo));
        let lone =
            from_scan_json(&SHOP.replace("\"imports\": 1", "\"imports\": 2")).expect("parses");
        assert_ne!(
            layout_key(&shop),
            layout_key(&lone),
            "a belt's weight is read too"
        );
    }
    #[test]
    fn marks_go_far_when_the_plant_is_narrow_on_screen_or_out_of_view() {
        let view = [1366.0, 768.0];
        assert!(
            !far([100.0, 100.0, 700.0, 400.0], view),
            "a plant across the view is near"
        );
        assert!(
            !far([1200.0, 700.0, 1400.0, 900.0], view),
            "a plant half in view is near"
        );
        assert!(
            far([600.0, 300.0, 600.0 + FAR_WIDTH_CSS - 1.0, 360.0], view),
            "too narrow"
        );
        assert!(
            !far([600.0, 300.0, 600.0 + FAR_WIDTH_CSS, 360.0], view),
            "at the width"
        );
        assert!(
            far([1400.0, 100.0, 2000.0, 400.0], view),
            "right of the view"
        );
        assert!(far([-900.0, 100.0, -10.0, 400.0], view), "left of the view");
        assert!(far([100.0, 800.0, 700.0, 900.0], view), "below the view");
        assert!(far([100.0, -500.0, 700.0, -1.0], view), "above the view");
        assert!(
            far([f32::NAN, 0.0, 10.0, 10.0], view),
            "a plant behind the camera"
        );
    }
}
