//! Plant layout on the ground plan. Machines grow out of the middle of the plan as districts, on
//! the floor grid (see `place`). Each import is a belt routed square along the streets between
//! machines, clear of every footprint (see `route`). The result depends only on the input, so the
//! same plant always lays out the same way.

use crate::kind::{Kind, PlanFootprint};
use crate::place;
use crate::route::{self, Request};

/// Positions snap to this grid, in plan units.
pub const GRID: f32 = 8.0;
/// How far a machine's label band reaches past its plate, in plan units (see [`label_band`]).
/// About where the bottom of the label falls at the zoom that fits a plant of a few dozen
/// machines.
pub const LABEL_BAND: f32 = 32.0;
/// Room beyond the outermost machines that belts may use, in plan units. Belts can run round the
/// outside of the plant through it, past the label bands of the outermost machines.
const MARGIN: f32 = LABEL_BAND + 16.0;
/// How far a district's ground pad reaches past its outermost machines, in plan units: past the
/// label bands of its front machines, and well inside half a street, so two pads never meet.
pub const PAD_MARGIN: f32 = LABEL_BAND + GRID;
/// A district gets a pad only when at least this many districts hold two machines or more. A
/// plant of one folder of sources and a test or two reads as one place, not as districts.
const PADDED_DISTRICTS: usize = 2;

/// A directed import between two machines, by index. `from` imports `to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub imports: u32,
}

/// A machine on the plan. Its footprint is `kind.footprint()`, with the top left at `(x, y)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub x: f32,
    pub y: f32,
    pub kind: Kind,
}

impl Placed {
    /// `[min x, min y, max x, max y]` of the footprint.
    pub fn footprint(&self) -> [f32; 4] {
        let (w, d) = self.kind.footprint();
        [self.x, self.y, self.x + w, self.y + d]
    }
}

/// An orthogonal belt, from the edge of one plinth to the edge of another. `path` is the
/// centre line before its corners are rounded.
#[derive(Clone, Debug, PartialEq)]
pub struct Belt {
    pub from: usize,
    pub to: usize,
    pub imports: u32,
    pub path: Vec<[f32; 2]>,
}

/// The ground under one district's machines: a quiet pad its sign stands on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DistrictPad {
    /// The district, as `Plant::districts` numbers it.
    pub district: u32,
    /// `[min x, min y, max x, max y]` of the pad.
    pub rect: [f32; 4],
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// One entry per input machine, in input order.
    pub machines: Vec<Placed>,
    /// One entry per routed edge, in input order. An edge that no route fits inside the plan
    /// draws no belt.
    pub belts: Vec<Belt>,
    /// `[min x, min y, max x, max y]` of all footprints.
    pub bounds: [f32; 4],
    /// One pad per district, in district order, or none for a plant that reads as one place
    /// (see [`district_pads`]).
    pub districts: Vec<DistrictPad>,
}

impl Layout {
    /// The same layout moved by `by` on the plan: every footprint, belt point and the bounds. A
    /// project on a canvas of several is laid out from the origin, then moved to its place, so its
    /// scene, framing and marks are all in world coordinates.
    pub fn translated(&self, by: [f32; 2]) -> Layout {
        let [dx, dy] = by;
        Layout {
            machines: self
                .machines
                .iter()
                .map(|placed| Placed {
                    x: placed.x + dx,
                    y: placed.y + dy,
                    kind: placed.kind,
                })
                .collect(),
            belts: self
                .belts
                .iter()
                .map(|belt| Belt {
                    path: belt.path.iter().map(|&[x, y]| [x + dx, y + dy]).collect(),
                    ..belt.clone()
                })
                .collect(),
            bounds: [
                self.bounds[0] + dx,
                self.bounds[1] + dy,
                self.bounds[2] + dx,
                self.bounds[3] + dy,
            ],
            districts: self
                .districts
                .iter()
                .map(|pad| DistrictPad {
                    district: pad.district,
                    rect: [
                        pad.rect[0] + dx,
                        pad.rect[1] + dy,
                        pad.rect[2] + dx,
                        pad.rect[3] + dy,
                    ],
                })
                .collect(),
        }
    }
}

/// Lays out the machines and their imports. `districts` gives each machine's district (see
/// `Plant::districts`). An empty slice lays the whole plant out as one district. Every import is
/// routed except one that no route fits inside the plan.
pub fn layout(kinds: &[Kind], edges: &[Edge], districts: &[u32]) -> Layout {
    let tops = place::place(kinds, edges, districts);
    let machines: Vec<Placed> = kinds
        .iter()
        .zip(&tops)
        .map(|(&kind, &[x, y])| Placed { x, y, kind })
        .collect();
    let boxes: Vec<[f32; 4]> = machines.iter().map(Placed::footprint).collect();
    let bounds = bounds_of(&boxes);
    let frame = [
        bounds[0] - MARGIN,
        bounds[1] - MARGIN,
        bounds[2] + MARGIN,
        bounds[3] + MARGIN,
    ];

    // Short belts are routed first, so they take the streets nearest their machines. Ties keep
    // the input order, so the result depends only on the input.
    let mut order: Vec<usize> = (0..edges.len()).collect();
    order.sort_by(|&a, &b| {
        belt_length(&boxes, &edges[a])
            .total_cmp(&belt_length(&boxes, &edges[b]))
            .then(a.cmp(&b))
    });
    let requests: Vec<Request> = order
        .iter()
        .map(|&k| Request {
            from: edges[k].from,
            to: edges[k].to,
        })
        .collect();
    let mut paths: Vec<Option<Vec<[f32; 2]>>> = vec![None; edges.len()];
    let bands: Vec<[f32; 4]> = boxes.iter().map(|&b| label_band(b)).collect();
    for (&k, path) in order
        .iter()
        .zip(route::route(&boxes, &bands, frame, &requests))
    {
        paths[k] = path;
    }

    let belts = edges
        .iter()
        .zip(paths)
        .filter_map(|(edge, path)| {
            Some(Belt {
                from: edge.from,
                to: edge.to,
                imports: edge.imports,
                path: path?,
            })
        })
        .collect();
    Layout {
        districts: district_pads(&boxes, districts),
        machines,
        belts,
        bounds,
    }
}

/// One pad per district: the box round its machines, [`PAD_MARGIN`] wider on every side. Places
/// keep every district's whole block a street from the next (see `place`), so pads never
/// overlap. None when fewer than [`PADDED_DISTRICTS`] districts hold two machines or more.
pub fn district_pads(boxes: &[[f32; 4]], districts: &[u32]) -> Vec<DistrictPad> {
    if districts.len() != boxes.len() {
        return Vec::new();
    }
    let mut rects: std::collections::BTreeMap<u32, ([f32; 4], usize)> = Default::default();
    for (&district, &b) in districts.iter().zip(boxes) {
        rects
            .entry(district)
            .and_modify(|(r, count)| {
                *r = [
                    r[0].min(b[0]),
                    r[1].min(b[1]),
                    r[2].max(b[2]),
                    r[3].max(b[3]),
                ];
                *count += 1;
            })
            .or_insert((b, 1));
    }
    if rects.values().filter(|(_, count)| *count >= 2).count() < PADDED_DISTRICTS {
        return Vec::new();
    }
    rects
        .into_iter()
        .map(|(district, (r, _))| DistrictPad {
            district,
            rect: [
                r[0] - PAD_MARGIN,
                r[1] - PAD_MARGIN,
                r[2] + PAD_MARGIN,
                r[3] + PAD_MARGIN,
            ],
        })
        .collect()
}

/// How far inside its pad's front edge a district sign stands, in plan units.
const SIGN_INSET: f32 = GRID;

/// Where the sign of the district on the pad `rect` may stand, best first: just inside the two
/// pad edges nearest the viewer, level with each of the district's `machines` in turn. The
/// machine nearest the viewer comes first, and of its two spots the one on the edge nearer it.
/// So a sign stands by its district's front machine, not off at the pad's far corner, and the
/// page takes the first spot clear of the machine names. `depth` tells how near the viewer a
/// ground point is: the larger, the nearer. Spots keep [`PAD_MARGIN`] from the pad's corners.
pub fn sign_spots(
    rect: [f32; 4],
    machines: &[[f32; 4]],
    depth: impl Fn([f32; 2]) -> f32,
) -> Vec<[f32; 2]> {
    let [x0, y0, x1, y1] = rect;
    let corners = |r: [f32; 4]| [[r[0], r[1]], [r[2], r[1]], [r[0], r[3]], [r[2], r[3]]];
    let nearest = |r: [f32; 4]| {
        corners(r)
            .into_iter()
            .map(&depth)
            .fold(f32::NEG_INFINITY, f32::max)
    };
    let Some(front) = corners(rect)
        .into_iter()
        .max_by(|a, b| depth(*a).total_cmp(&depth(*b)))
    else {
        return Vec::new();
    };
    // The front corner's two edges: one at a fixed x, one at a fixed y.
    let ex = if front[0] == x1 {
        x1 - SIGN_INSET
    } else {
        x0 + SIGN_INSET
    };
    let ey = if front[1] == y1 {
        y1 - SIGN_INSET
    } else {
        y0 + SIGN_INSET
    };
    let along = |v: f32, lo: f32, hi: f32| v.min(hi - PAD_MARGIN).max(lo + PAD_MARGIN);
    let mut order: Vec<[f32; 4]> = machines.to_vec();
    order.sort_by(|a, b| nearest(*b).total_cmp(&nearest(*a)));
    let mut spots = Vec::with_capacity(order.len() * 2);
    for b in order {
        let mid = [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0];
        let on_y = [along(mid[0], x0, x1), ey];
        let on_x = [ex, along(mid[1], y0, y1)];
        let pair = if (mid[1] - ey).abs() <= (mid[0] - ex).abs() {
            [on_y, on_x]
        } else {
            [on_x, on_y]
        };
        for spot in pair {
            if !spots.contains(&spot) {
                spots.push(spot);
            }
        }
    }
    spots
}

/// The plan rectangle a machine's name hangs over, which belts keep out of: the quarter of the
/// plate nearest the viewer and the ground in front of it, `LABEL_BAND` deep. The default camera
/// looks from 45 degrees round, so the plate's front is its corner at the greatest x and y, and
/// the label hangs below that corner on screen, centred on it, about as wide as the plate (see
/// `labels`). Belts still leave the plate from the far half of those two sides.
pub fn label_band(footprint: [f32; 4]) -> [f32; 4] {
    let [x0, y0, x1, y1] = footprint;
    [
        (x0 + x1) / 2.0,
        (y0 + y1) / 2.0,
        x1 + LABEL_BAND,
        y1 + LABEL_BAND,
    ]
}

/// `[min x, min y, max x, max y]` over the footprints, or zeros for none.
fn bounds_of(boxes: &[[f32; 4]]) -> [f32; 4] {
    boxes
        .iter()
        .fold(None, |acc: Option<[f32; 4]>, f| match acc {
            None => Some(*f),
            Some(b) => Some([
                b[0].min(f[0]),
                b[1].min(f[1]),
                b[2].max(f[2]),
                b[3].max(f[3]),
            ]),
        })
        .unwrap_or([0.0; 4])
}

/// The distance between the centres of an edge's two machines, along the grid. Infinite when an
/// end is not a machine, so such an edge sorts last.
fn belt_length(boxes: &[[f32; 4]], edge: &Edge) -> f32 {
    let centre = |b: &[f32; 4]| [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0];
    match (boxes.get(edge.from), boxes.get(edge.to)) {
        (Some(a), Some(b)) => {
            let (a, b) = (centre(a), centre(b));
            (a[0] - b[0]).abs() + (a[1] - b[1]).abs()
        }
        _ => f32::INFINITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plant::{Plant, PlantMachine, from_scan_json, stress};

    const FIXTURE: &str = include_str!("../www/fixtures/demo-plant.json");

    /// A workspace of twelve crates of one to eleven folders each. No crate imports another, and
    /// only four have imports inside, the way a repository of mostly tests and examples scans.
    fn workspace() -> Plant {
        const FOLDERS: [usize; 12] = [2, 3, 3, 2, 3, 3, 4, 2, 11, 8, 4, 1];
        const IMPORTS: [(usize, usize, u32); 15] = [
            (11, 12, 1),
            (12, 11, 73),
            (18, 19, 44),
            (25, 26, 4),
            (26, 25, 1),
            (30, 32, 1),
            (31, 30, 1),
            (31, 32, 1),
            (32, 30, 1),
            (35, 36, 10),
            (35, 37, 6),
            (35, 38, 2),
            (37, 35, 24),
            (37, 36, 4),
            (38, 35, 1),
        ];
        let mut machines = Vec::new();
        for (c, &folders) in FOLDERS.iter().enumerate() {
            for f in 0..folders {
                let kind = match (c, f) {
                    (9, _) => Kind::Intake,
                    (_, 0) => Kind::Assembler,
                    _ => Kind::Crate,
                };
                let name = format!("crates/c{c}/f{f}");
                machines.push(PlantMachine {
                    paths: vec![format!("{name}/lib.rs")],
                    name,
                    files: 1,
                    kind,
                    guessed: false,
                    reason: String::new(),
                    set_by_user: false,
                });
            }
        }
        Plant {
            repo: "workspace".to_owned(),
            machines,
            edges: IMPORTS
                .iter()
                .map(|&(from, to, imports)| Edge { from, to, imports })
                .collect(),
        }
    }

    /// The workspace's 46 folders in twelve crates, densely linked the way a workspace of crates
    /// that use each other scans: each folder imports its own crate's first folder, and up to four
    /// other crates' first folders, the low-numbered crates far more often, as a core crate is.
    /// About two hundred imports.
    fn dense_workspace() -> Plant {
        let mut plant = workspace();
        let districts = plant.districts();
        let first: Vec<usize> = (0..12)
            .map(|d| districts.iter().position(|&x| x == d).expect("crate"))
            .collect();
        let mut edges = Vec::new();
        let mut seed = 0x9e37_79b9_u32;
        for (i, &own) in districts.iter().enumerate() {
            let own = own as usize;
            if first[own] != i {
                edges.push((i, first[own]));
            }
            for _ in 0..5 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Squaring a uniform pick leans it toward crate 0.
                let u = (seed >> 8) as f32 / (1 << 24) as f32;
                let crate_ = ((u * u) * 12.0) as usize;
                if crate_ != own {
                    edges.push((i, first[crate_]));
                }
            }
        }
        edges.sort_unstable();
        edges.dedup();
        plant.edges = edges
            .into_iter()
            .map(|(from, to)| Edge {
                from,
                to,
                imports: 1 + ((from * 7 + to) % 5) as u32,
            })
            .collect();
        plant
    }

    /// The clear distance between two footprints along the axis where they are furthest apart.
    fn gap(a: [f32; 4], b: [f32; 4]) -> f32 {
        let across_x = (b[0] - a[2]).max(a[0] - b[2]);
        let across_y = (b[1] - a[3]).max(a[1] - b[3]);
        across_x.max(across_y)
    }

    fn aspect(bounds: [f32; 4]) -> f32 {
        let (w, h) = (bounds[2] - bounds[0], bounds[3] - bounds[1]);
        w.max(h) / w.min(h)
    }

    fn chain_plant() -> (Vec<Kind>, Vec<Edge>) {
        let kinds = vec![
            Kind::Gate,
            Kind::Assembler,
            Kind::Tank,
            Kind::Crate,
            Kind::Assembler,
        ];
        let edges = vec![
            Edge {
                from: 0,
                to: 1,
                imports: 1,
            },
            Edge {
                from: 1,
                to: 2,
                imports: 2,
            },
            Edge {
                from: 0,
                to: 2,
                imports: 1,
            },
            Edge {
                from: 3,
                to: 4,
                imports: 1,
            },
            Edge {
                from: 2,
                to: 0,
                imports: 1,
            },
        ];
        (kinds, edges)
    }

    /// Whether the axis-aligned segment `a`-`b` runs inside the rectangle `r` anywhere, by
    /// more than half a plan unit.
    fn crosses(r: [f32; 4], a: [f32; 2], b: [f32; 2]) -> bool {
        let inset = 0.5;
        a[0].max(b[0]) > r[0] + inset
            && a[0].min(b[0]) < r[2] - inset
            && a[1].max(b[1]) > r[1] + inset
            && a[1].min(b[1]) < r[3] - inset
    }

    fn on_boundary(footprint: [f32; 4], p: [f32; 2]) -> bool {
        let eps = 1e-3;
        let within = p[0] >= footprint[0] - eps
            && p[0] <= footprint[2] + eps
            && p[1] >= footprint[1] - eps
            && p[1] <= footprint[3] + eps;
        within
            && ((p[0] - footprint[0]).abs() < eps
                || (p[0] - footprint[2]).abs() < eps
                || (p[1] - footprint[1]).abs() < eps
                || (p[1] - footprint[3]).abs() < eps)
    }

    /// Checks every invariant the brief names on one laid-out plant.
    fn check_invariants(layout: &Layout) {
        let boxes: Vec<[f32; 4]> = layout.machines.iter().map(Placed::footprint).collect();
        for (i, a) in boxes.iter().enumerate() {
            for (j, b) in boxes.iter().enumerate().skip(i + 1) {
                let apart = a[2] + GRID <= b[0]
                    || b[2] + GRID <= a[0]
                    || a[3] + GRID <= b[1]
                    || b[3] + GRID <= a[1];
                assert!(
                    apart,
                    "footprints {i} and {j} overlap or touch: {a:?} {b:?}"
                );
            }
        }
        for belt in &layout.belts {
            let path = &belt.path;
            assert!(
                path.len() >= 2,
                "belt {}->{} has no segment",
                belt.from,
                belt.to
            );
            for pair in path.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                let horizontal = (a[1] - b[1]).abs() < 1e-3;
                let vertical = (a[0] - b[0]).abs() < 1e-3;
                assert!(
                    horizontal ^ vertical,
                    "belt {}->{} has a diagonal or empty segment: {a:?} {b:?}",
                    belt.from,
                    belt.to
                );
                for (m, footprint) in boxes.iter().enumerate() {
                    assert!(
                        !crosses(*footprint, a, b),
                        "belt {}->{} runs under machine {m} from {a:?} to {b:?}",
                        belt.from,
                        belt.to
                    );
                    assert!(
                        !crosses(label_band(*footprint), a, b),
                        "belt {}->{} runs through the label of machine {m} from {a:?} to {b:?}",
                        belt.from,
                        belt.to
                    );
                }
            }
            let first = path[0];
            let last = path[path.len() - 1];
            assert!(
                on_boundary(boxes[belt.from], first),
                "belt {}->{} does not start on its plinth edge: {first:?}",
                belt.from,
                belt.to
            );
            assert!(
                on_boundary(boxes[belt.to], last),
                "belt {}->{} does not end on its plinth edge: {last:?}",
                belt.from,
                belt.to
            );
        }
    }

    fn check_plant(plant: &Plant) {
        let (kinds, edges, districts) = plant.layout_input();
        let layout = layout(&kinds, &edges, &districts);
        assert_eq!(layout.machines.len(), plant.machines.len());
        assert_eq!(layout.belts.len(), edges.len(), "every import is routed");
        check_invariants(&layout);
    }

    #[test]
    fn layout_is_deterministic() {
        let (kinds, edges) = chain_plant();
        assert_eq!(layout(&kinds, &edges, &[]), layout(&kinds, &edges, &[]));
        let plant = stress(300);
        let (kinds, edges, districts) = plant.layout_input();
        assert_eq!(
            layout(&kinds, &edges, &districts),
            layout(&kinds, &edges, &districts)
        );
    }

    #[test]
    fn a_closing_edge_is_routed_too() {
        let (kinds, edges) = chain_plant();
        let laid = layout(&kinds, &edges, &[]);
        assert_eq!(laid.belts.len(), 5);
        assert!(laid.belts.iter().any(|b| b.from == 2 && b.to == 0));
        check_invariants(&laid);
    }

    #[test]
    fn a_two_node_cycle_routes_two_belts() {
        let kinds = [Kind::Gate, Kind::Tank];
        let edges = [
            Edge {
                from: 0,
                to: 1,
                imports: 1,
            },
            Edge {
                from: 1,
                to: 0,
                imports: 1,
            },
        ];
        let laid = layout(&kinds, &edges, &[]);
        assert_eq!(laid.belts.len(), 2);
        check_invariants(&laid);
    }

    #[test]
    #[should_panic(expected = "runs under machine")]
    fn the_checker_rejects_a_belt_that_runs_under_a_machine() {
        let laid = Layout {
            machines: vec![
                Placed {
                    x: 0.0,
                    y: 0.0,
                    kind: Kind::Assembler,
                },
                Placed {
                    x: 200.0,
                    y: 0.0,
                    kind: Kind::Assembler,
                },
                Placed {
                    x: 400.0,
                    y: 0.0,
                    kind: Kind::Assembler,
                },
            ],
            belts: vec![Belt {
                from: 0,
                to: 2,
                imports: 1,
                path: vec![[96.0, 40.0], [400.0, 40.0]],
            }],
            bounds: [0.0, 0.0, 496.0, 86.0],
            districts: Vec::new(),
        };
        check_invariants(&laid);
    }

    #[test]
    fn an_empty_plant_has_an_empty_layout() {
        let laid = layout(&[], &[], &[]);
        assert!(laid.machines.is_empty());
        assert!(laid.belts.is_empty());
        assert_eq!(laid.bounds, [0.0; 4]);
    }

    #[test]
    fn a_hand_built_plant_meets_every_invariant() {
        let (kinds, edges) = chain_plant();
        check_invariants(&layout(&kinds, &edges, &[]));
    }

    #[test]
    fn the_demo_fixture_meets_every_invariant() {
        let plant = from_scan_json(FIXTURE).expect("fixture parses");
        check_plant(&plant);
    }

    #[test]
    fn a_large_generated_plant_meets_every_invariant() {
        check_plant(&stress(400));
    }

    #[test]
    fn a_large_plant_spreads_round_rather_than_along_a_line() {
        let plant = stress(200);
        let (kinds, edges, districts) = plant.layout_input();
        let bounds = layout(&kinds, &edges, &districts).bounds;
        let aspect = aspect(bounds);
        assert!(aspect < 2.5, "plant spans {bounds:?}, aspect {aspect}");
    }

    #[test]
    fn a_workspace_of_unlinked_crates_meets_every_invariant() {
        check_plant(&workspace());
    }

    #[test]
    fn a_workspace_of_unlinked_crates_spreads_round_in_blocks() {
        let (kinds, edges, districts) = workspace().layout_input();
        let laid = layout(&kinds, &edges, &districts);
        let aspect = aspect(laid.bounds);
        assert!(
            aspect < 2.0,
            "plant spans {:?}, aspect {aspect}",
            laid.bounds
        );
        let boxes: Vec<[f32; 4]> = laid.machines.iter().map(Placed::footprint).collect();
        for (i, own) in districts.iter().enumerate() {
            let nearest = |mate: bool| {
                (0..boxes.len())
                    .filter(|&j| j != i && (districts[j] == *own) == mate)
                    .map(|j| gap(boxes[i], boxes[j]))
                    .min_by(f32::total_cmp)
            };
            if let (Some(mate), Some(other)) = (nearest(true), nearest(false)) {
                assert!(
                    mate < other,
                    "machine {i} is {mate} from its district and {other} from another"
                );
            }
        }
    }

    #[test]
    fn belts_keep_clear_of_every_label_band() {
        let demo = from_scan_json(FIXTURE).expect("fixture parses");
        for plant in [demo, workspace(), stress(200)] {
            let (kinds, edges, districts) = plant.layout_input();
            let laid = layout(&kinds, &edges, &districts);
            assert_eq!(laid.belts.len(), edges.len(), "every import is routed");
            let bands: Vec<[f32; 4]> = laid
                .machines
                .iter()
                .map(|m| label_band(m.footprint()))
                .collect();
            for belt in &laid.belts {
                for s in belt.path.windows(2) {
                    for (m, band) in bands.iter().enumerate() {
                        assert!(
                            !crosses(*band, s[0], s[1]),
                            "belt {}->{} runs through the label of machine {m}: {:?}",
                            belt.from,
                            belt.to,
                            belt.path
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_densely_linked_workspace_routes_every_import() {
        let plant = dense_workspace();
        assert!(
            (150..=260).contains(&plant.edges.len()),
            "{} imports",
            plant.edges.len()
        );
        check_plant(&plant);
    }

    #[test]
    fn the_most_used_crate_of_a_dense_workspace_sits_near_the_middle() {
        let plant = dense_workspace();
        let (kinds, edges, districts) = plant.layout_input();
        let laid = layout(&kinds, &edges, &districts);
        let b = laid.bounds;
        let middle = [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0];
        let away: Vec<f32> = laid
            .districts
            .iter()
            .map(|pad| {
                let r = pad.rect;
                ((r[0] + r[2]) / 2.0 - middle[0]).hypot((r[1] + r[3]) / 2.0 - middle[1])
            })
            .collect();
        // Crate 0 is the one the others import most; it sorts first among the districts.
        let core = away[0];
        let nearer = away.iter().filter(|&&d| d < core).count();
        assert!(
            nearer <= 2,
            "{nearer} districts sit nearer the middle: {away:?}"
        );
    }

    #[test]
    fn each_district_has_a_pad_round_its_machines_and_no_two_pads_meet() {
        for plant in [workspace(), dense_workspace()] {
            check_pads(&plant);
        }
    }

    fn check_pads(plant: &Plant) {
        let (kinds, edges, districts) = plant.layout_input();
        let laid = layout(&kinds, &edges, &districts);
        assert_eq!(laid.districts.len(), 12, "one pad per crate");
        let inside =
            |r: [f32; 4], b: [f32; 4]| b[0] >= r[0] && b[1] >= r[1] && b[2] <= r[2] && b[3] <= r[3];
        for (m, placed) in laid.machines.iter().enumerate() {
            let pad = laid
                .districts
                .iter()
                .find(|pad| pad.district == districts[m])
                .expect("every machine's district has a pad");
            assert!(
                inside(pad.rect, placed.footprint()),
                "machine {m} off its pad"
            );
            assert!(
                inside(pad.rect, label_band(placed.footprint())),
                "machine {m}'s name off its pad"
            );
        }
        for (i, a) in laid.districts.iter().enumerate() {
            for b in &laid.districts[i + 1..] {
                assert!(
                    gap(a.rect, b.rect) >= GRID,
                    "pads {:?} and {:?} meet",
                    a.rect,
                    b.rect
                );
            }
        }
        let moved = laid.translated([100.0, -50.0]);
        assert_eq!(
            moved.districts[0].rect[0],
            laid.districts[0].rect[0] + 100.0
        );
        assert_eq!(moved.districts[0].rect[3], laid.districts[0].rect[3] - 50.0);
    }

    #[test]
    fn a_district_sign_stands_by_the_front_machine_of_its_pad() {
        let plant = dense_workspace();
        let (kinds, edges, districts) = plant.layout_input();
        let laid = layout(&kinds, &edges, &districts);
        // The default view's front is the greatest x and y; a view orbited a quarter turn has
        // its front at the least x and the greatest y.
        let views: [fn([f32; 2]) -> f32; 2] = [|p| p[0] + p[1], |p| p[1] - p[0]];
        for depth in views {
            for pad in &laid.districts {
                let boxes: Vec<[f32; 4]> = laid
                    .machines
                    .iter()
                    .zip(&districts)
                    .filter(|(_, d)| **d == pad.district)
                    .map(|(placed, _)| placed.footprint())
                    .collect();
                let spots = sign_spots(pad.rect, &boxes, depth);
                assert!(
                    spots.len() >= 2,
                    "pad {:?} has {} spots",
                    pad.rect,
                    spots.len()
                );
                let r = pad.rect;
                for s in &spots {
                    assert!(s[0] > r[0] && s[0] < r[2] && s[1] > r[1] && s[1] < r[3]);
                }
                let corners = [[r[0], r[1]], [r[2], r[1]], [r[0], r[3]], [r[2], r[3]]];
                let front = corners
                    .into_iter()
                    .max_by(|a, b| depth(*a).total_cmp(&depth(*b)))
                    .expect("four corners");
                let first = spots[0];
                let on_front_edge = (first[0] - front[0]).abs() <= GRID + 1e-3
                    || (first[1] - front[1]).abs() <= GRID + 1e-3;
                assert!(on_front_edge, "sign {first:?} off the front of {r:?}");
                // It stands nearer a front machine of the district than the bare corner does.
                let near = |f: &[f32; 4]| {
                    [[f[0], f[1]], [f[2], f[1]], [f[0], f[3]], [f[2], f[3]]]
                        .map(depth)
                        .into_iter()
                        .fold(f32::NEG_INFINITY, f32::max)
                };
                let most = boxes.iter().map(near).fold(f32::NEG_INFINITY, f32::max);
                let far = |p: [f32; 2], f: &[f32; 4]| {
                    let mid = [(f[0] + f[2]) / 2.0, (f[1] + f[3]) / 2.0];
                    (p[0] - mid[0]).abs() + (p[1] - mid[1]).abs()
                };
                assert!(
                    boxes
                        .iter()
                        .filter(|f| near(f) == most)
                        .any(|f| far(first, f) < far(front, f)),
                    "sign {first:?} no nearer a front machine than corner {front:?}: {boxes:?}"
                );
            }
        }
    }

    /// How far the belt `path` runs outside the town round `boxes`: where no footprint comes
    /// within `route::OUTSKIRT` of it along its row, or along its column, pushed out as far.
    fn outskirts_run(boxes: &[[f32; 4]], path: &[[f32; 2]]) -> f32 {
        let reach = route::OUTSKIRT;
        let outside = |x: f32, y: f32| {
            let span = |lo: usize, at: f32| {
                boxes
                    .iter()
                    .filter(|b| at >= b[lo] - reach && at <= b[lo + 2] + reach)
                    .fold([f32::INFINITY, f32::NEG_INFINITY], |s, b| {
                        let across = 1 - lo;
                        [s[0].min(b[across] - reach), s[1].max(b[across + 2] + reach)]
                    })
            };
            let (row, col) = (span(1, y), span(0, x));
            x < row[0] || x > row[1] || y < col[0] || y > col[1]
        };
        let mut run = 0.0;
        for s in path.windows(2) {
            let length = (s[0][0] - s[1][0]).abs() + (s[0][1] - s[1][1]).abs();
            let steps = (length / 4.0).ceil().max(1.0);
            for q in 0..steps as usize {
                let t = (q as f32 + 0.5) / steps;
                let x = s[0][0] + (s[1][0] - s[0][0]) * t;
                let y = s[0][1] + (s[1][1] - s[0][1]) * t;
                if outside(x, y) {
                    run += length / steps;
                }
            }
        }
        run
    }

    #[test]
    fn belts_of_a_dense_workspace_keep_to_the_town_and_take_no_long_way_round() {
        let plant = dense_workspace();
        let (kinds, edges, districts) = plant.layout_input();
        let laid = layout(&kinds, &edges, &districts);
        let boxes: Vec<[f32; 4]> = laid.machines.iter().map(Placed::footprint).collect();
        for belt in &laid.belts {
            let (a, b) = (boxes[belt.from], boxes[belt.to]);
            let apart =
                (b[0] - a[2]).max(a[0] - b[2]).max(0.0) + (b[1] - a[3]).max(a[1] - b[3]).max(0.0);
            let length: f32 = belt
                .path
                .windows(2)
                .map(|s| (s[0][0] - s[1][0]).abs() + (s[0][1] - s[1][1]).abs())
                .sum();
            assert!(
                length <= 2.0 * apart + 200.0,
                "belt {} -> {} runs {length} for machines {apart} apart",
                belt.from,
                belt.to
            );
            // A belt may cut a corner of the town, not run round its edge.
            let run = outskirts_run(&boxes, &belt.path);
            assert!(
                run <= route::OUTSKIRT,
                "belt {} -> {} runs {run} outside the town: {:?}",
                belt.from,
                belt.to,
                belt.path
            );
        }
    }

    #[test]
    fn a_plant_that_reads_as_one_place_has_no_pads() {
        // The demo's sources share one folder, and its one test folder holds a single machine.
        let demo = from_scan_json(FIXTURE).expect("fixture parses");
        let (kinds, edges, districts) = demo.layout_input();
        assert!(layout(&kinds, &edges, &districts).districts.is_empty());
        let (kinds, edges) = chain_plant();
        assert!(layout(&kinds, &edges, &[]).districts.is_empty());
        // A plant of one machine per district has nothing to group.
        let (kinds, edges, districts) = stress(60).layout_input();
        assert!(layout(&kinds, &edges, &districts).districts.is_empty());
    }

    #[test]
    fn bounds_cover_every_footprint() {
        let plant = from_scan_json(FIXTURE).expect("fixture parses");
        let (kinds, edges, districts) = plant.layout_input();
        let laid = layout(&kinds, &edges, &districts);
        for placed in &laid.machines {
            let f = placed.footprint();
            assert!(f[0] >= laid.bounds[0] && f[2] <= laid.bounds[2]);
            assert!(f[1] >= laid.bounds[1] && f[3] <= laid.bounds[3]);
        }
    }
}
