//! The plant's scene: a flat ribbon for each belt and one instance for each machine, built from
//! the layout and the acan-iso models.

use acan_iso::{BODY_RADIUS, Overlay};
use acan_render::{Aabb, HueLine, Instance, Model, Ribbon, Scene};

use crate::bends;
use crate::kind::{self, ALL, Kind, PlanFootprint};
use crate::layout::{Belt, Layout, Placed};
use crate::marks::{FLAG_ROOM, MarkKinds, hover_height};
use crate::plant::PlantMachine;

/// The gap between a belt's end and the plinth it meets, in plan units.
const BELT_PORT_GAP: f32 = 1.5;

/// The mark models [`models`] lists after the machine kinds: the six [`mark_kinds`].
const MARK_MODELS: usize = 6;

/// One acan-iso model per kind, in [`kind::ALL`] order. The marks follow: an agent's drone body,
/// its accent, the ring round a waiting agent, the clash pin, its stalk, and the beam's end.
/// [`mark_kinds`] names them. Then each kind's overlays, six per kind, in [`kind::ALL`] order;
/// [`overlay_kind`] names them.
pub fn models() -> Vec<Model> {
    let mut out: Vec<Model> = ALL.iter().map(|&kind| acan_iso::model(kind)).collect();
    out.extend([
        acan_iso::agent_body(),
        acan_iso::agent_accent(),
        acan_iso::waiting_ring(),
        acan_iso::clash_pin(),
        acan_iso::clash_stalk(),
        acan_iso::beacon(),
    ]);
    out.extend(
        ALL.iter()
            .flat_map(|&kind| Overlay::ALL.map(|overlay| acan_iso::overlay_model(kind, overlay))),
    );
    out
}

/// The model index of `overlay` on a machine of `kind`, within [`models`].
pub fn overlay_kind(kind: Kind, overlay: Overlay) -> u16 {
    let first = ALL.len() + MARK_MODELS;
    let index = first + kind::index(kind) * Overlay::ALL.len() + overlay.index();
    u16::try_from(index).expect("model indexes fit in u16")
}

/// The overlays a machine carries from its data, placed on its base instance at `at`: its file
/// bars and its groove, which take `tint` so they dim with the machine, and its failed tick, which
/// stays red while a check fails.
#[must_use]
pub fn overlay_instances(
    machine: &PlantMachine,
    at: [f32; 3],
    failed: bool,
    tint: u8,
) -> Vec<Instance> {
    let mut marks = Vec::with_capacity(3);
    if let Some(count) = acan_iso::bar_count(machine.files) {
        marks.push((Overlay::Bars(count), tint));
    }
    marks.push((
        Overlay::Groove {
            broken: machine.guessed,
        },
        tint,
    ));
    if failed {
        marks.push((Overlay::FailedTick, 0));
    }
    marks
        .into_iter()
        .map(|(overlay, tint)| Instance {
            offset: at,
            kind: overlay_kind(machine.kind, overlay),
            tint,
            grow: [0.0; 2],
            turn: 0.0,
        })
        .collect()
}

/// The model kinds of the marks, which follow the machine kinds in [`models`].
pub fn mark_kinds() -> MarkKinds {
    let base = ALL.len() as u16;
    MarkKinds {
        body: base,
        accent: base + 1,
        ring: base + 2,
        pin: base + 3,
        stalk: base + 4,
        beacon: base + 5,
    }
}

/// The plant: a ribbon for each belt and an instance for each machine. `models` is indexed the
/// way [`kind::index`] indexes kinds.
pub fn plant_scene(layout: &Layout, models: &[Model]) -> Scene {
    let ribbons = belt_ribbons(&layout.belts);
    let mut bounds = Aabb::EMPTY;
    for ribbon in &ribbons {
        for &[x, y] in &ribbon.centre {
            bounds.include([x, y, 0.0]);
        }
    }

    let mut instances = Vec::with_capacity(layout.machines.len());
    for placed in &layout.machines {
        let (w, d) = placed.kind.footprint();
        let offset = [placed.x + w / 2.0, placed.y + d / 2.0, 0.0];
        let index = kind::index(placed.kind);
        for corner in models[index].bounds.corners() {
            bounds.include(std::array::from_fn(|axis| offset[axis] + corner[axis]));
        }
        // A drone hovers over the plinth of its machine, and its flag stands above it, so the plant
        // frames the drone's height and the flag's room.
        let high = hover_height(models[index].bounds.max[2]) + BODY_RADIUS + FLAG_ROOM;
        for [x, y] in [
            [placed.x, placed.y],
            [placed.x + w, placed.y],
            [placed.x, placed.y + d],
            [placed.x + w, placed.y + d],
        ] {
            bounds.include([x, y, high]);
        }
        instances.push(Instance {
            offset,
            kind: index as u16,
            tint: 0,
            grow: [0.0; 2],
            turn: 0.0,
        });
    }
    Scene {
        ribbons,
        instances,
        hue_lines: Vec::new(),
        beams: Vec::new(),
        // The body is white, so a hidden drone shows as its hue accent's outline.
        silhouettes: vec![mark_kinds().accent],
        bounds: nonempty(bounds),
    }
}

/// The drone seats a machine's drones take first. They sit on either side of the default view.
const SEATS: usize = 2;

/// The boxes the camera frames: each machine's model box up to the top of its flag room, and a
/// box round each of its first two drone seats, from the drone's underside to its crown. Belts
/// are left out, so a belt's path cannot pull the plant off centre. The seats are framed whether
/// or not a drone is there, so the frame does not move when an agent arrives. The machines' names
/// and the flag chips are HTML, so the camera leaves them room in pixels instead; see
/// `Camera::fit_boxes_room`. `spots` is indexed the way [`kind::index`] indexes kinds, and
/// `reach` is a drone's reach in plan units.
pub fn fit_boxes(
    layout: &Layout,
    models: &[Model],
    spots: &[Vec<[f32; 2]>],
    reach: f32,
) -> Vec<Aabb> {
    let mut boxes = Vec::with_capacity(layout.machines.len());
    for placed in &layout.machines {
        let (w, d) = placed.kind.footprint();
        let index = kind::index(placed.kind);
        let model = &models[index].bounds;
        let offset = [placed.x + w / 2.0, placed.y + d / 2.0];
        let hover = hover_height(model.max[2]);
        boxes.push(machine_box(placed, models));
        for spot in spots[index].iter().take(SEATS) {
            let at = [offset[0] + spot[0], offset[1] + spot[1]];
            boxes.push(Aabb {
                min: [at[0] - reach, at[1] - reach, hover - BODY_RADIUS],
                max: [at[0] + reach, at[1] + reach, hover + BODY_RADIUS],
            });
        }
    }
    if boxes.is_empty() {
        boxes.push(nonempty(Aabb::EMPTY));
    }
    boxes
}

/// The box of one machine that the camera frames: its model's footprint from the plinth up to the
/// top of its flag room.
pub fn machine_box(placed: &Placed, models: &[Model]) -> Aabb {
    let model = &models[kind::index(placed.kind)].bounds;
    let spot = on_spot(placed, model);
    let hover = hover_height(model.max[2]);
    Aabb {
        min: [spot.min[0], spot.min[1], 0.0],
        max: [spot.max[0], spot.max[1], hover + BODY_RADIUS + FLAG_ROOM],
    }
}

/// The box of one machine's model alone, on its plinth: no drone hover, flag room or seats. A ride
/// solves its distance so this box spans half the view height (`pov::pov_camera`).
pub fn machine_model_box(placed: &Placed, models: &[Model]) -> Aabb {
    on_spot(placed, &models[kind::index(placed.kind)].bounds)
}

/// A model's bounds moved to the machine's place: its heights stay, and its footprint offset is
/// the centre of the machine's footprint.
fn on_spot(placed: &Placed, model: &Aabb) -> Aabb {
    let (w, d) = placed.kind.footprint();
    let offset = [placed.x + w / 2.0, placed.y + d / 2.0];
    Aabb {
        min: [
            offset[0] + model.min[0],
            offset[1] + model.min[1],
            model.min[2],
        ],
        max: [
            offset[0] + model.max[0],
            offset[1] + model.max[1],
            model.max[2],
        ],
    }
}

/// The model boxes of a layout, in the layout's machine order.
pub fn machine_model_boxes(layout: &Layout, models: &[Model]) -> Vec<Aabb> {
    layout
        .machines
        .iter()
        .map(|placed| machine_model_box(placed, models))
        .collect()
}

/// The box itself when it holds anything, or a unit box for a plant with nothing to draw.
fn nonempty(bounds: Aabb) -> Aabb {
    if bounds.min[0] > bounds.max[0] {
        return Aabb {
            min: [0.0; 3],
            max: [1.0; 3],
        };
    }
    bounds
}

/// A belt as it draws: its rounded centre line on the plan, its width, and the machines it joins.
#[derive(Clone, Debug, PartialEq)]
pub struct BeltLine {
    /// The index of the machine its path starts at.
    pub from: usize,
    /// The index of the machine its path ends at.
    pub to: usize,
    /// Plan points of the rounded centre line, stopped just short of both plinths.
    pub centre: Vec<[f32; 2]>,
    /// Full width in plan units.
    pub width: f32,
}

/// Each belt that draws, in layout order, as its rounded centre line. A belt too short to draw is
/// left out.
pub fn belt_lines(belts: &[Belt]) -> Vec<BeltLine> {
    let (kept, paths): (Vec<&Belt>, Vec<Vec<[f32; 2]>>) = belts
        .iter()
        .filter_map(|belt| Some((belt, trimmed(&belt.path, BELT_PORT_GAP)?)))
        .unzip();
    kept.into_iter()
        .zip(bends::round_all(&paths))
        .map(|(belt, centre)| BeltLine {
            from: belt.from,
            to: belt.to,
            centre,
            width: belt_width(belt.imports),
        })
        .collect()
}

/// Each belt is a flat ribbon on the ground along its rounded path, stopped just short of both
/// plinths. A belt too short to draw is left out.
fn belt_ribbons(belts: &[Belt]) -> Vec<Ribbon> {
    belt_lines(belts)
        .into_iter()
        .map(|line| Ribbon {
            centre: line.centre,
            width: line.width,
        })
        .collect()
}

/// The hue lines to draw. Each belt that joins a machine a live task's files sit on gets one line,
/// once for each tint. `hues` pairs an instance index with a tint.
pub fn hue_lines(belts: &[BeltLine], hues: &[(usize, u8)]) -> Vec<HueLine> {
    let mut out = Vec::new();
    for belt in belts {
        let mut tints: Vec<u8> = hues
            .iter()
            .filter(|(machine, _)| *machine == belt.from || *machine == belt.to)
            .map(|&(_, tint)| tint)
            .collect();
        tints.sort_unstable();
        tints.dedup();
        out.extend(tints.into_iter().map(|tint| HueLine {
            centre: belt.centre.clone(),
            tint,
        }));
    }
    out
}

/// A belt's width in plan units, a little wider for more imports. The widest belt still leaves
/// a gap to its neighbour at the layout's belt step.
fn belt_width(imports: u32) -> f32 {
    4.0 + imports.min(8) as f32 * 0.3
}

/// The path with `gap` cut off each end, or `None` when nothing is left.
fn trimmed(path: &[[f32; 2]], gap: f32) -> Option<Vec<[f32; 2]>> {
    let [first, second] = [*path.first()?, *path.get(1)?];
    let n = path.len();
    let [before_last, last] = [path[n - 2], path[n - 1]];
    let (head, tail) = (distance(first, second), distance(before_last, last));
    if head <= gap || tail <= gap || (n == 2 && head <= 2.0 * gap) {
        return None;
    }
    let mut out = path.to_vec();
    out[0] = toward(first, second, gap / head);
    out[n - 1] = toward(last, before_last, gap / tail);
    Some(out)
}

/// The point a fraction `t` of the way from `from` to `to`.
fn toward(from: [f32; 2], to: [f32; 2], t: f32) -> [f32; 2] {
    [
        from[0] + (to[0] - from[0]) * t,
        from[1] + (to[1] - from[1]) * t,
    ]
}

fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    let v = [b[0] - a[0], b[1] - a[1]];
    v[0].hypot(v[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::Kind;
    use crate::layout::layout;
    use crate::plant::{from_scan_json, stress};

    const FIXTURE: &str = include_str!("../www/fixtures/demo-plant.json");

    #[test]
    fn a_hue_line_runs_along_each_belt_that_joins_a_hued_machine_once_per_tint() {
        let belt = |from: usize, to: usize| BeltLine {
            from,
            to,
            centre: vec![[0.0, 0.0], [10.0, 0.0]],
            width: 4.0,
        };
        let belts = [belt(0, 1), belt(1, 2), belt(2, 3)];
        let lines = hue_lines(&belts, &[(1, 7), (1, 7), (2, 9)]);
        let tints: Vec<u8> = lines.iter().map(|line| line.tint).collect();
        // Belt 0 touches machine 1 and belt 2 touches machine 2. Belt 1 joins both, so it carries
        // both tints.
        assert_eq!(tints, vec![7, 7, 9, 9]);
        assert!(hue_lines(&belts, &[]).is_empty(), "no hue, no line");
    }

    #[test]
    fn models_stand_on_the_ground_inside_their_footprint() {
        for (kind, model) in ALL.into_iter().zip(models()) {
            let (w, d) = kind.footprint();
            let b = model.bounds;
            assert!(b.min[2].abs() < 1e-4, "{kind:?} floats above z = 0");
            assert!(b.max[2] > 6.0, "{kind:?} has no body");
            assert!(
                b.min[0] >= -w / 2.0 - 1e-3 && b.max[0] <= w / 2.0 + 1e-3,
                "{kind:?} width"
            );
            assert!(
                b.min[1] >= -d / 2.0 - 1e-3 && b.max[1] <= d / 2.0 + 1e-3,
                "{kind:?} depth"
            );
        }
    }

    #[test]
    fn a_machines_overlays_follow_its_files_its_guess_and_its_failed_check() {
        let machine = |files: usize, guessed: bool| PlantMachine {
            name: "api".to_owned(),
            files,
            kind: Kind::Gate,
            guessed,
            reason: String::new(),
            set_by_user: false,
            paths: Vec::new(),
        };
        let at = [10.0, 20.0, 0.0];
        let kinds = |instances: &[Instance]| -> Vec<u16> {
            instances.iter().map(|inst| inst.kind).collect()
        };
        let bars = overlay_kind(Kind::Gate, Overlay::Bars(2));
        let broken = overlay_kind(Kind::Gate, Overlay::Groove { broken: true });
        let closed = overlay_kind(Kind::Gate, Overlay::Groove { broken: false });
        let tick = overlay_kind(Kind::Gate, Overlay::FailedTick);

        let guessed = overlay_instances(&machine(5, true), at, false, 0);
        assert_eq!(kinds(&guessed), [bars, broken]);
        let exact = overlay_instances(&machine(0, false), at, false, 0);
        assert_eq!(kinds(&exact), [closed], "no files, no bars");
        let failing = overlay_instances(&machine(5, false), at, true, 9);
        assert_eq!(kinds(&failing), [bars, closed, tick]);
        assert_eq!(failing[2].tint, 0, "the failed tick stays red when dimmed");
        assert_eq!(failing[0].tint, 9, "the bars dim with their machine");
        assert!(failing.iter().all(|inst| inst.offset == at));

        let models = models();
        for overlay in Overlay::ALL {
            let index = usize::from(overlay_kind(Kind::Gate, overlay));
            assert!(index < models.len(), "{overlay:?} has a model");
        }
    }

    #[test]
    fn a_ride_measures_each_machine_from_its_model_alone() {
        let plant = from_scan_json(FIXTURE).expect("fixture parses");
        let (kinds, edges) = plant.layout_input();
        let laid = layout(&kinds, &edges);
        let models = models();
        let boxes = machine_model_boxes(&laid, &models);
        assert_eq!(boxes.len(), laid.machines.len());
        for (placed, bounds) in laid.machines.iter().zip(&boxes) {
            let model = models[kind::index(placed.kind)].bounds;
            assert_eq!(
                bounds.max[2], model.max[2],
                "{:?} stops at its model top",
                placed.kind
            );
            let framed = machine_box(placed, &models);
            assert!(
                bounds.max[2] < framed.max[2],
                "{:?} leaves its hover and flag room out",
                placed.kind
            );
        }
    }

    #[test]
    fn the_scene_places_each_machine_at_its_footprint_centre() {
        let plant = from_scan_json(FIXTURE).expect("fixture parses");
        let (kinds, edges) = plant.layout_input();
        let laid = layout(&kinds, &edges);
        let scene = plant_scene(&laid, &models());
        assert_eq!(scene.instances.len(), plant.machines.len());
        for (placed, inst) in laid.machines.iter().zip(&scene.instances) {
            let (w, d) = placed.kind.footprint();
            assert_eq!(inst.offset, [placed.x + w / 2.0, placed.y + d / 2.0, 0.0]);
            assert_eq!(usize::from(inst.kind), kind::index(placed.kind));
        }
        assert_eq!(scene.ribbons.len(), laid.belts.len());
    }

    #[test]
    fn the_scene_bounds_hold_every_machine_and_belt() {
        let plant = stress(200);
        let (kinds, edges) = plant.layout_input();
        let laid = layout(&kinds, &edges);
        let scene = plant_scene(&laid, &models());
        for placed in &laid.machines {
            let f = placed.footprint();
            assert!(f[0] >= scene.bounds.min[0] && f[2] <= scene.bounds.max[0]);
            assert!(f[1] >= scene.bounds.min[1] && f[3] <= scene.bounds.max[1]);
        }
        for ribbon in &scene.ribbons {
            for p in &ribbon.centre {
                for (axis, value) in p.iter().enumerate() {
                    assert!(*value >= scene.bounds.min[axis] - 1e-3);
                    assert!(*value <= scene.bounds.max[axis] + 1e-3);
                }
            }
        }
    }

    #[test]
    fn no_belt_reaches_under_a_plinth_across_its_whole_width() {
        for plant in [
            from_scan_json(FIXTURE).expect("fixture parses"),
            stress(200),
        ] {
            let (kinds, edges) = plant.layout_input();
            let laid = layout(&kinds, &edges);
            for ribbon in &plant_scene(&laid, &models()).ribbons {
                let c = &ribbon.centre;
                for i in 0..c.len() {
                    let next = c[(i + 1).min(c.len() - 1)];
                    let prev = c[i.saturating_sub(1)];
                    let d = [next[0] - prev[0], next[1] - prev[1]];
                    let len = d[0].hypot(d[1]).max(1e-6);
                    let across = [-d[1] / len, d[0] / len];
                    for s in [-1.0, -0.5, 0.0, 0.5, 1.0] {
                        let k = 0.5 * ribbon.width * s;
                        let p = [c[i][0] + across[0] * k, c[i][1] + across[1] * k];
                        for m in &laid.machines {
                            let f = m.footprint();
                            assert!(
                                !(p[0] > f[0] && p[0] < f[2] && p[1] > f[1] && p[1] < f[3]),
                                "{p:?} of a belt lies under the plinth at {f:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_belt_stops_just_short_of_both_plates() {
        let belt = Belt {
            from: 0,
            to: 1,
            imports: 1,
            path: vec![[0.0, 0.0], [100.0, 0.0]],
        };
        let ribbons = belt_ribbons(std::slice::from_ref(&belt));
        let ribbon = ribbons.first().expect("a long belt draws");
        assert_eq!(
            ribbon.centre,
            vec![[BELT_PORT_GAP, 0.0], [100.0 - BELT_PORT_GAP, 0.0]]
        );
        assert_eq!(ribbon.width, belt_width(1));
        let stub = Belt {
            path: vec![[0.0, 0.0], [2.0, 0.0]],
            ..belt
        };
        assert!(
            belt_ribbons(&[stub]).is_empty(),
            "a belt shorter than its gaps draws nothing"
        );
    }

    #[test]
    fn belts_are_narrow_next_to_the_machines() {
        let (w, d) = Kind::Assembler.footprint();
        assert!(
            belt_width(8) * 10.0 <= w.min(d),
            "the widest belt stays within a tenth of a plate"
        );
        assert!(belt_width(1) < belt_width(8));
    }
}
