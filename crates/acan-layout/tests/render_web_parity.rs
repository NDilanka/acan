//! Parity with acan-render-web's layout, on the demo plant and on two generated plants. This
//! file and the acan-render-web dev-dependency go away with acan-render-web, once the 2D layout
//! has moved over and nothing else needs the web host.

use acan_layout::layout::{self as ours, Edge, Layout};
use acan_render_web::kind::PlanFootprint;
use acan_render_web::layout as theirs;
use acan_render_web::plant::{Plant, from_scan_json, stress};

/// The demo plant, read from acan-render-web's own fixture.
fn demo() -> Plant {
    from_scan_json(include_str!(
        "../../acan-render-web/www/fixtures/demo-plant.json"
    ))
    .expect("the demo fixture reads as a plant")
}

/// Lays the plant out with both crates and checks that the machines, belts, bounds and district
/// pads come out the same.
fn check_parity(name: &str, plant: &Plant) {
    let (kinds, edges, districts) = plant.layout_input();
    let sizes: Vec<[f32; 2]> = kinds
        .iter()
        .map(|&kind| {
            let (w, d) = kind.footprint();
            [w, d]
        })
        .collect();
    let our_edges: Vec<Edge> = edges
        .iter()
        .map(|e| Edge {
            from: e.from,
            to: e.to,
            imports: e.imports,
        })
        .collect();
    let before = theirs::layout(&kinds, &edges, &districts);
    let after = ours::layout(&sizes, &our_edges, &districts);
    assert!(!after.belts.is_empty(), "{name}: the plant has belts");
    assert_same(name, &before, &after, edges.len());
}

fn assert_same(name: &str, before: &theirs::Layout, after: &Layout, edges: usize) {
    assert_eq!(
        before.machines.len(),
        after.machines.len(),
        "{name}: machine count"
    );
    for (i, (a, b)) in after.machines.iter().zip(&before.machines).enumerate() {
        let (w, d) = b.kind.footprint();
        assert_eq!(
            (a.x, a.y, a.w, a.d),
            (b.x, b.y, w, d),
            "{name}: machine {i}"
        );
    }
    assert_eq!(before.belts.len(), after.belts.len(), "{name}: belt count");
    for (i, (a, b)) in after.belts.iter().zip(&before.belts).enumerate() {
        assert_eq!(
            (a.from, a.to, a.imports),
            (b.from, b.to, b.imports),
            "{name}: belt {i}"
        );
        assert_eq!(a.path, b.path, "{name}: belt {i} path");
    }
    assert_eq!(before.bounds, after.bounds, "{name}: bounds");
    assert_eq!(
        before.districts.len(),
        after.districts.len(),
        "{name}: pad count"
    );
    for (a, b) in after.districts.iter().zip(&before.districts) {
        assert_eq!((a.district, a.rect), (b.district, b.rect), "{name}: pad");
    }
    // acan-render-web drops an edge that no route fits without a trace; here it is listed.
    assert_eq!(
        after.belts.len() + after.unrouted.len(),
        edges,
        "{name}: every edge is routed or listed as unrouted"
    );
}

#[test]
fn the_demo_plant_lays_out_as_render_web_does() {
    check_parity("demo", &demo());
}

#[test]
fn a_fifty_machine_plant_lays_out_as_render_web_does() {
    check_parity("stress(50)", &stress(50));
}

#[test]
fn a_three_hundred_machine_plant_lays_out_as_render_web_does() {
    check_parity("stress(300)", &stress(300));
}
