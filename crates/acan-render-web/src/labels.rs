//! Where machine names go on screen. A label hangs under its plate's front edge, or above the
//! machine when a nearer one covers that spot, fades with the plant's fog, and is left out when
//! its machine is too small on screen to name or when other machines cover both spots. The
//! page then drops any label that collides with one placed before it, so the order here is
//! the priority: larger on screen first.

use acan_render::camera::{Projection, Projector};
use acan_render::{Aabb, Camera};

/// The screen rectangle the usage panel covers on a viewport `viewport_height` device pixels
/// tall, anchored to its bottom-left corner, as `(min x, min y, max x, max y)` in device pixels.
/// The fit keeps the machines' names clear of it; see `Camera::fit_boxes_room`.
pub fn panel_avoid(viewport_height: f32, dpr: f32) -> [f32; 4] {
    let [width, height] = USAGE_PANEL_CSS;
    [
        0.0,
        viewport_height - height * dpr,
        width * dpr,
        viewport_height,
    ]
}

/// A machine named at rest needs at least this many CSS pixels of plate width on screen.
pub const REST_MIN_CSS_PX: f32 = 18.0;
/// With Alt held every machine may be named, but only once its plate is this wide on screen.
pub const ALL_MIN_CSS_PX: f32 = 34.0;
/// The average advance of one character of a label, in CSS pixels, for its width before the
/// page measures it.
pub const CHAR_CSS_PX: f32 = 6.5;
/// From the plate's front edge down to the middle of the label, in CSS pixels. Matches the
/// label's offset and line height in `index.html`.
pub const LABEL_DROP_CSS_PX: f32 = 13.0;
/// The size of the screen rectangle the fixed usage panel covers at rest, in CSS pixels: width
/// and height. The panel now holds the meta attribution table, its legend and its note, so these
/// are estimates from that content, not measurements in a browser. The panel grows with the
/// rows, so the pad is generous.
pub const USAGE_PANEL_CSS: [f32; 2] = [480.0, 210.0];
/// The label's bottom edge sits this many `LABEL_DROP_CSS_PX` below the plate's front edge.
const LABEL_BOTTOM_DROPS: f32 = 1.6;
/// Below the size to name, this many of the largest machines are still named, down to
/// `BUDGET_FLOOR_CSS_PX` of plate width, at most one in each of as many equal regions of the
/// screen. Largest first alone would name only the nearest edge of a perspective view.
pub const REST_BUDGET: usize = 12;
pub const BUDGET_FLOOR_CSS_PX: f32 = 6.0;
/// The budget tries at most this many machines per label, so a dense plant where most
/// labels are covered does not test every machine on every frame.
const BUDGET_TRIES: usize = 16;
/// Side of a cell of the screen grid that finds the occluders near a label, in device pixels.
const GRID_CELL_PX: f32 = 64.0;
/// From a machine's highest point on screen up to the anchor of a label above it, in CSS
/// pixels: a small gap plus the label's offset and height.
pub const LABEL_LIFT_CSS_PX: f32 = 24.0;
/// Labels fainter than this are left out.
const MIN_OPACITY: f32 = 0.05;
/// A label may sit this far outside the viewport, in device pixels, and still be placed.
const VIEW_MARGIN_PX: f32 = 40.0;

/// A label that may be shown, in device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LabelSpot {
    pub index: usize,
    /// The point the label hangs from: the plate's front edge under the plate's centre, or a
    /// spot above the machine.
    pub screen: [f32; 2],
    pub opacity: f32,
    /// The plate's width on screen.
    pub size_px: f32,
}

/// The machines labels are placed among. `footprints` holds each machine's plan rectangle
/// `[min x, min y, max x, max y]`, `occluders` the world boxes of each machine's solid parts
/// with the machine they belong to, in machine order, and `half_widths` each label's half width in device px.
pub struct Machines<'a> {
    pub footprints: &'a [[f32; 4]],
    pub occluders: &'a [(usize, Aabb)],
    pub half_widths: &'a [f32],
}

/// How many labels to place, in device pixels. A machine whose plate is at least `min_px`
/// wide on screen is named. Below that, the largest machines down to `floor_px` are named
/// until `budget` labels are placed, at most one in each of about `budget` regions of the
/// screen, so even a far plant shows a few names spread over it. `drop_px` is the distance
/// from an anchor to the label's middle, and `lift_px` the distance from a machine's highest
/// point up to the anchor of a label above it.
pub struct Rules {
    pub min_px: f32,
    pub floor_px: f32,
    pub budget: usize,
    pub drop_px: f32,
    pub lift_px: f32,
}

/// The labels that may show, most important first. Every machine whose plate is at least
/// `rules.floor_px` wide on screen is considered, largest first. One marked in `named` is
/// placed once it reaches `rules.min_px`, and any other only while fewer than `rules.budget`
/// labels are placed and its screen region holds none yet. A label hangs under its plate's
/// front edge. When a nearer machine covers that spot it moves above its machine's highest
/// point instead, and it is left out when another machine covers that spot too.
pub fn place(
    camera: &Camera,
    machines: &Machines,
    named: &[bool],
    rules: &Rules,
) -> Vec<LabelSpot> {
    let view = camera.projector();
    let rects: Vec<Option<[f32; 4]>> = machines
        .occluders
        .iter()
        .map(|(_, b)| screen_rect(&view, b))
        .collect();
    let placer = Placer {
        camera,
        view,
        occluders: machines.occluders,
        grid: Grid::new(camera.viewport(), &rects),
        rects,
        rules,
    };
    let mut plates: Vec<Plate> = machines
        .footprints
        .iter()
        .enumerate()
        .filter_map(|(index, &f)| placer.plate(index, f))
        .collect();
    plates.sort_by(|a, b| b.size_px.total_cmp(&a.size_px).then(a.index.cmp(&b.index)));
    let mut spots = Vec::new();
    let mut regions = Regions::new(camera.viewport(), rules.budget);
    let mut tries = rules.budget * BUDGET_TRIES;
    for plate in &plates {
        let big = plate.size_px >= rules.min_px;
        let is_named = big && named.get(plate.index).copied().unwrap_or(false);
        if !is_named {
            if spots.len() >= rules.budget || tries == 0 {
                // Plates come largest first, so past the size to name nothing more qualifies.
                if !big {
                    break;
                }
                continue;
            }
            if regions.taken(plate.at) {
                continue;
            }
            tries -= 1;
        }
        let half = machines
            .half_widths
            .get(plate.index)
            .copied()
            .unwrap_or(0.0);
        if let Some(anchor) = placer.anchor(plate, half, !is_named) {
            // A label that moved above its machine may have moved into a taken region.
            if !is_named && regions.taken(anchor) {
                continue;
            }
            regions.take(anchor);
            spots.push(LabelSpot {
                index: plate.index,
                screen: anchor,
                opacity: plate.opacity,
                size_px: plate.size_px,
            });
        }
    }
    spots
}

/// Equal regions of the viewport, about as many as the budget and close to square, each
/// marked once it holds a label. A point off screen counts in the nearest region.
struct Regions {
    cols: usize,
    rows: usize,
    cell: [f32; 2],
    taken: Vec<bool>,
}

impl Regions {
    fn new([vw, vh]: [f32; 2], budget: usize) -> Self {
        let n = budget.max(1) as f32;
        let cols = (n * vw / vh.max(1.0)).sqrt().round().clamp(1.0, n);
        let rows = (n / cols).ceil();
        Regions {
            cols: cols as usize,
            rows: rows as usize,
            cell: [vw.max(1.0) / cols, vh.max(1.0) / rows],
            taken: vec![false; cols as usize * rows as usize],
        }
    }

    fn of(&self, p: [f32; 2]) -> usize {
        let at = |v: f32, size: f32, count: usize| ((v / size).max(0.0) as usize).min(count - 1);
        at(p[1], self.cell[1], self.rows) * self.cols + at(p[0], self.cell[0], self.cols)
    }

    fn taken(&self, p: [f32; 2]) -> bool {
        self.taken[self.of(p)]
    }

    fn take(&mut self, p: [f32; 2]) {
        let i = self.of(p);
        self.taken[i] = true;
    }
}

/// A machine's plate as seen on screen.
struct Plate {
    index: usize,
    centre: [f32; 3],
    front: [f32; 3],
    /// Where a label under the plate hangs from: below the plate's centre, on the front
    /// corner's screen row.
    at: [f32; 2],
    size_px: f32,
    opacity: f32,
}

struct Placer<'a> {
    camera: &'a Camera,
    view: Projector,
    occluders: &'a [(usize, Aabb)],
    rects: Vec<Option<[f32; 4]>>,
    grid: Grid,
    rules: &'a Rules,
}

/// The occluders whose screen rectangles reach each cell of a grid over the viewport and its
/// margin. A rectangle past the grid's edge is kept in the edge cells, and one partly behind
/// the eye in every cell, so a lookup never misses an occluder.
struct Grid {
    origin: f32,
    cols: usize,
    rows: usize,
    cells: Vec<Vec<usize>>,
    everywhere: Vec<usize>,
}

impl Grid {
    fn new([vw, vh]: [f32; 2], rects: &[Option<[f32; 4]>]) -> Self {
        let origin = -VIEW_MARGIN_PX;
        let span = |v: f32| ((v + 2.0 * VIEW_MARGIN_PX) / GRID_CELL_PX).ceil().max(1.0) as usize;
        let (cols, rows) = (span(vw), span(vh));
        let mut grid = Grid {
            origin,
            cols,
            rows,
            cells: vec![Vec::new(); cols * rows],
            everywhere: Vec::new(),
        };
        for (o, rect) in rects.iter().enumerate() {
            match rect {
                Some(r) => {
                    let ([c0, r0], [c1, r1]) = grid.cells_of(*r);
                    for row in r0..=r1 {
                        for col in c0..=c1 {
                            grid.cells[row * cols + col].push(o);
                        }
                    }
                }
                None => grid.everywhere.push(o),
            }
        }
        grid
    }

    /// The first and last cell, as `[col, row]`, that the screen box `r` touches.
    fn cells_of(&self, r: [f32; 4]) -> ([usize; 2], [usize; 2]) {
        let cell = |v: f32, count: usize| {
            (((v - self.origin) / GRID_CELL_PX).floor().max(0.0) as usize).min(count - 1)
        };
        (
            [cell(r[0], self.cols), cell(r[1], self.rows)],
            [cell(r[2], self.cols), cell(r[3], self.rows)],
        )
    }

    /// The occluders that may reach the screen box `r`, each once.
    fn near(&self, r: [f32; 4]) -> Vec<usize> {
        let ([c0, r0], [c1, r1]) = self.cells_of(r);
        let mut found = self.everywhere.clone();
        for row in r0..=r1 {
            for col in c0..=c1 {
                found.extend_from_slice(&self.cells[row * self.cols + col]);
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }
}

impl Placer<'_> {
    /// The plate of the machine at `index`, or `None` when it is behind the eye, too small to
    /// name or lost in the fog.
    fn plate(&self, index: usize, f: [f32; 4]) -> Option<Plate> {
        let camera = self.camera;
        let corners =
            [[f[0], f[1]], [f[2], f[1]], [f[2], f[3]], [f[0], f[3]]].map(|[x, y]| [x, y, 0.0]);
        if !corners.iter().all(|&c| self.view.in_front(c)) {
            return None;
        }
        let screen = corners.map(|c| self.view.world_to_screen(c));
        let xs = screen.map(|s| s[0]);
        let size_px = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max)
            - xs.iter().copied().fold(f32::INFINITY, f32::min);
        if size_px < self.rules.floor_px {
            return None;
        }
        let front = (0..4).max_by(|&a, &b| screen[a][1].total_cmp(&screen[b][1]))?;
        let opacity = 1.0 - camera.fog_at(corners[front]);
        if opacity < MIN_OPACITY {
            return None;
        }
        let centre = [(f[0] + f[2]) * 0.5, (f[1] + f[3]) * 0.5, 0.0];
        Some(Plate {
            index,
            centre,
            front: corners[front],
            at: [self.view.world_to_screen(centre)[0], screen[front][1]],
            size_px,
            opacity,
        })
    }

    /// Where the label of `plate` hangs from, or `None` when no spot for it is clear.
    /// `half` is the label's half width. With `over_farther` the spot above the machine may
    /// lie over machines behind it, so a far name can sit inside a dense field, where no spot
    /// is ever clear of every other machine.
    fn anchor(&self, plate: &Plate, half: f32, over_farther: bool) -> Option<[f32; 2]> {
        let (camera, rules) = (self.camera, self.rules);
        let across = |anchor: [f32; 2], rows: [f32; 2]| {
            rows.into_iter().flat_map(move |drop| {
                [-1.0, -0.5, 0.0, 0.5, 1.0].map(|t| [anchor[0] + t * half, anchor[1] + drop])
            })
        };

        // Under the plate: hidden when a nearer machine stands on the ground under any part
        // of the text, or in front of the plate's front corner.
        let below = plate.at;
        let (centre, front) = (plate.centre, plate.front);
        let out = unit2([front[0] - centre[0], front[1] - centre[1]]);
        let edge = [front[0] + out[0], front[1] + out[1], 0.5];
        let rows = [rules.drop_px, rules.drop_px * LABEL_BOTTOM_DROPS];
        let below_clear = self.inside(below) && {
            let spots = across(below, rows).chain([self.view.world_to_screen(edge)]);
            let near = self.near(spots, plate.index);
            let mut ground = across(below, rows)
                .filter_map(|px| camera.ground_at(px))
                .map(|[x, y]| [x, y, 0.2]);
            !std::iter::once(edge)
                .chain(&mut ground)
                .any(|p| self.nearer_box_hides(p, &near))
        };
        if below_clear {
            return Some(below);
        }

        // Above the machine: hidden when any other machine lies on the ray through any part of
        // the text, nearer or, unless `over_farther`, farther, so a label never sits on one.
        let first = self
            .occluders
            .partition_point(|(owner, _)| *owner < plate.index);
        let end = self
            .occluders
            .partition_point(|(owner, _)| *owner <= plate.index);
        let top = self.occluders[first..end]
            .iter()
            .flat_map(|(_, b)| b.corners())
            .map(|c| self.view.world_to_screen(c)[1])
            .fold(f32::INFINITY, f32::min);
        if !top.is_finite() {
            return None;
        }
        let above = [below[0], top - rules.lift_px];
        let rows = [rules.drop_px * 0.75, rules.drop_px * 1.25];
        if !self.inside(above) {
            return None;
        }
        let far = if over_farther {
            let (eye, toward) = (camera.eye(), camera.basis().toward);
            -(0..3)
                .map(|i| (plate.centre[i] - eye[i]) * toward[i])
                .sum::<f32>()
        } else {
            camera.fog_range().1
        };
        let near = self.near(across(above, rows), plate.index);
        let covered = across(above, rows).any(|px| {
            let (from, to) = (camera.ray_point(px, 0.0), camera.ray_point(px, far));
            near.iter().any(|&o| {
                rect_holds(self.rects[o], px) && segment_hits(from, to, &self.occluders[o].1)
            })
        });
        (!covered).then_some(above)
    }

    /// The occluders of machines other than `own` whose screen rectangles may reach the box
    /// around `spots`.
    fn near(&self, spots: impl Iterator<Item = [f32; 2]>, own: usize) -> Vec<usize> {
        let mut b = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        for s in spots {
            b = [
                b[0].min(s[0]),
                b[1].min(s[1]),
                b[2].max(s[0]),
                b[3].max(s[1]),
            ];
        }
        let mut near = self.grid.near(b);
        near.retain(|&o| {
            self.occluders[o].0 != own
                && self.rects[o]
                    .is_none_or(|r| r[0] <= b[2] && r[2] >= b[0] && r[1] <= b[3] && r[3] >= b[1])
        });
        near
    }

    /// Whether `p` is on screen, or close enough to it for a label.
    fn inside(&self, p: [f32; 2]) -> bool {
        let [vw, vh] = self.camera.viewport();
        p[0] >= -VIEW_MARGIN_PX
            && p[0] <= vw + VIEW_MARGIN_PX
            && p[1] >= -VIEW_MARGIN_PX
            && p[1] <= vh + VIEW_MARGIN_PX
    }

    /// Whether one of the occluders `near` lies between the eye and `p`.
    fn nearer_box_hides(&self, p: [f32; 3], near: &[usize]) -> bool {
        let camera = self.camera;
        let s = self.view.world_to_screen(p);
        let origin = match camera.projection() {
            Projection::Perspective => camera.eye(),
            Projection::Orthographic => {
                let t = camera.basis().toward;
                let reach = 4.0 * camera.eye_distance();
                std::array::from_fn(|axis| p[axis] + t[axis] * reach)
            }
        };
        near.iter()
            .any(|&o| rect_holds(self.rects[o], s) && segment_hits(origin, p, &self.occluders[o].1))
    }
}

/// Whether a box's screen rectangle may hold `s`. A box partly behind the eye always may.
fn rect_holds(rect: Option<[f32; 4]>, s: [f32; 2]) -> bool {
    rect.is_none_or(|r| s[0] >= r[0] && s[0] <= r[2] && s[1] >= r[1] && s[1] <= r[3])
}

/// The screen rectangle around a box's corners, or `None` when part of it is behind the eye.
fn screen_rect(view: &Projector, b: &Aabb) -> Option<[f32; 4]> {
    let mut r: [f32; 4] = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for c in b.corners() {
        if !view.in_front(c) {
            return None;
        }
        let s = view.world_to_screen(c);
        r = [
            r[0].min(s[0]),
            r[1].min(s[1]),
            r[2].max(s[0]),
            r[3].max(s[1]),
        ];
    }
    Some(r)
}

/// Whether the segment from `a` to `b` passes through the box, short of `b` itself.
fn segment_hits(a: [f32; 3], b: [f32; 3], bx: &Aabb) -> bool {
    let (mut t0, mut t1) = (0.0_f32, 1.0_f32 - 1e-4);
    for axis in 0..3 {
        let d = b[axis] - a[axis];
        if d.abs() < 1e-9 {
            if a[axis] < bx.min[axis] || a[axis] > bx.max[axis] {
                return false;
            }
            continue;
        }
        let (mut near, mut far) = ((bx.min[axis] - a[axis]) / d, (bx.max[axis] - a[axis]) / d);
        if near > far {
            std::mem::swap(&mut near, &mut far);
        }
        t0 = t0.max(near);
        t1 = t1.min(far);
        if t0 > t1 {
            return false;
        }
    }
    true
}

fn unit2(v: [f32; 2]) -> [f32; 2] {
    let len = v[0].hypot(v[1]).max(1e-6);
    [v[0] / len, v[1] / len]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usage_panel_room_anchors_to_the_bottom_left_corner() {
        for (height, dpr) in [(700.0, 1.0), (1200.0, 2.0)] {
            let avoid = panel_avoid(height, dpr);
            assert_eq!(avoid[0], 0.0, "left edge is the screen's left edge");
            assert_eq!(avoid[3], height, "bottom is the viewport height");
            assert_eq!(
                avoid[1],
                height - USAGE_PANEL_CSS[1] * dpr,
                "top is the panel's height up from the bottom"
            );
        }
    }

    const W: f32 = 96.0;
    const D: f32 = 86.0;
    const H: f32 = 40.0;

    fn tall(x: f32, y: f32, h: f32) -> ([f32; 4], Aabb) {
        (
            [x, y, x + W, y + D],
            Aabb {
                min: [x, y, 0.0],
                max: [x + W, y + D, h],
            },
        )
    }

    fn machine(x: f32, y: f32) -> ([f32; 4], Aabb) {
        tall(x, y, H)
    }

    fn camera_on(machines: &[([f32; 4], Aabb)], azimuth: f32, elevation: f32) -> Camera {
        let mut bounds = Aabb::EMPTY;
        for (_, b) in machines {
            bounds.include(b.min);
            bounds.include(b.max);
        }
        let mut camera = Camera::new([1440.0, 900.0]);
        camera.set_angles(azimuth, elevation);
        camera.fit(&bounds, 24.0);
        camera.set_fog_bounds(bounds);
        camera
    }

    fn rules(min_px: f32) -> Rules {
        Rules {
            min_px,
            floor_px: 0.0,
            budget: 0,
            drop_px: LABEL_DROP_CSS_PX,
            lift_px: LABEL_LIFT_CSS_PX,
        }
    }

    /// The spots of every machine, each labelled `half` device pixels to either side.
    fn spots(
        machines: &[([f32; 4], Aabb)],
        camera: &Camera,
        half: f32,
        rules: &Rules,
    ) -> Vec<LabelSpot> {
        let footprints: Vec<[f32; 4]> = machines.iter().map(|(f, _)| *f).collect();
        let occluders: Vec<(usize, Aabb)> = machines.iter().map(|(_, b)| *b).enumerate().collect();
        let half_widths = vec![half; machines.len()];
        let named = vec![true; machines.len()];
        let all = Machines {
            footprints: &footprints,
            occluders: &occluders,
            half_widths: &half_widths,
        };
        place(camera, &all, &named, rules)
    }

    fn placed(machines: &[([f32; 4], Aabb)], camera: &Camera) -> Vec<usize> {
        spots(machines, camera, 0.0, &rules(10.0))
            .into_iter()
            .map(|s| s.index)
            .collect()
    }

    /// The highest screen row of a machine's box.
    fn top_of(camera: &Camera, b: &Aabb) -> f32 {
        b.corners()
            .iter()
            .map(|&c| camera.world_to_screen(c)[1])
            .fold(f32::INFINITY, f32::min)
    }

    #[test]
    fn a_label_behind_a_nearer_machine_at_low_tilt_moves_above_its_machine() {
        // The camera looks along -x at azimuth 0, so the machine at larger x is in front.
        let machines = [machine(0.0, 0.0), machine(130.0, 0.0)];
        let camera = camera_on(&machines, 0.0, 8.0);
        let shown = spots(&machines, &camera, 0.0, &rules(10.0));
        let back = shown
            .iter()
            .find(|s| s.index == 0)
            .expect("the far machine is named");
        assert!(
            back.screen[1] < top_of(&camera, &machines[0].1),
            "its label sits above it"
        );
        let front = shown
            .iter()
            .find(|s| s.index == 1)
            .expect("the near machine is named");
        assert!(front.screen[1] > top_of(&camera, &machines[1].1));
    }

    #[test]
    fn a_label_covered_below_and_above_is_hidden() {
        // A tower in front hides both the far machine's front edge and the sky above it.
        let machines = [machine(0.0, 0.0), tall(130.0, 0.0, 400.0)];
        let camera = camera_on(&machines, 0.0, 8.0);
        assert_eq!(placed(&machines, &camera), vec![1]);
    }

    #[test]
    fn a_label_whose_end_runs_onto_a_nearer_machine_moves_above() {
        // Machine 1 stands in front of machine 0 and off to one side, so only a wide label
        // reaches it.
        let machines = [machine(0.0, 0.0), machine(130.0, 110.0)];
        let camera = camera_on(&machines, 0.0, 8.0);
        let centre_x = |f: [f32; 4]| {
            let c = [(f[0] + f[2]) * 0.5, (f[1] + f[3]) * 0.5, 0.0];
            camera.world_to_screen(c)[0]
        };
        let reach = (centre_x(machines[1].0) - centre_x(machines[0].0)).abs();
        let top = top_of(&camera, &machines[0].1);
        let row = |half: f32| {
            spots(&machines, &camera, half, &rules(10.0))
                .iter()
                .find(|s| s.index == 0)
                .map(|s| s.screen[1])
        };
        assert!(
            row(0.0).is_some_and(|y| y > top),
            "a point label stays under its plate"
        );
        assert!(
            row(reach).is_some_and(|y| y < top),
            "a wide one moves above"
        );
    }

    #[test]
    fn side_by_side_machines_both_keep_their_labels() {
        let machines = [machine(0.0, 0.0), machine(0.0, 200.0)];
        let camera = camera_on(&machines, 0.0, 30.0);
        let mut shown = placed(&machines, &camera);
        shown.sort();
        assert_eq!(shown, vec![0, 1]);
    }

    #[test]
    fn the_larger_machine_on_screen_comes_first() {
        let machines = [machine(0.0, 0.0), machine(400.0, 0.0)];
        let camera = camera_on(&machines, 0.0, 30.0);
        let shown = placed(&machines, &camera);
        assert_eq!(
            shown.first(),
            Some(&1),
            "the nearer machine leads: {shown:?}"
        );
    }

    #[test]
    fn a_machine_too_small_on_screen_is_not_named() {
        let machines = [machine(0.0, 0.0)];
        let camera = camera_on(&machines, 45.0, 30.0);
        assert!(spots(&machines, &camera, 0.0, &rules(1e6)).is_empty());
    }

    #[test]
    fn the_budget_names_the_largest_machines_below_the_size_to_name() {
        let machines = [machine(0.0, 0.0), machine(0.0, 200.0), machine(400.0, 0.0)];
        let camera = camera_on(&machines, 0.0, 30.0);
        let budget = Rules {
            budget: 1,
            ..rules(1e6)
        };
        let shown: Vec<usize> = spots(&machines, &camera, 0.0, &budget)
            .iter()
            .map(|s| s.index)
            .collect();
        assert_eq!(shown, vec![2], "only the nearest is named");
    }

    #[test]
    fn the_budget_spreads_its_names_over_the_screen() {
        // Spread out, and packed so tight that no spot inside the field is clear of every
        // other machine. Packed tight, the field's right edge covers every spot in the lower
        // right region, so that region stays empty.
        for (pitch, count) in [(200.0, 6), (W + 8.0, 5)] {
            let machines: Vec<_> = (0..64)
                .map(|k| machine(pitch * (k % 8) as f32, pitch * (k / 8) as f32))
                .collect();
            let camera = camera_on(&machines, 45.0, 30.0);
            let budget = Rules {
                budget: 6,
                ..rules(1e6)
            };
            let shown = spots(&machines, &camera, 0.0, &budget);
            assert_eq!(shown.len(), count, "pitch {pitch}: {shown:?}");
            let regions = Regions::new(camera.viewport(), budget.budget);
            let mut cells: Vec<usize> = shown.iter().map(|s| regions.of(s.screen)).collect();
            cells.sort_unstable();
            cells.dedup();
            assert_eq!(cells.len(), count, "one name to a region: {shown:?}");
            let rows = shown.iter().map(|s| s.screen[1]);
            let spread = rows.clone().fold(f32::MIN, f32::max) - rows.fold(f32::MAX, f32::min);
            assert!(
                spread > 0.25 * camera.viewport()[1],
                "pitch {pitch}: names reach into the field, {spread}"
            );
        }
    }

    #[test]
    fn the_anchor_is_the_front_edge_of_the_plate() {
        let machines = [machine(0.0, 0.0)];
        let camera = camera_on(&machines, 45.0, 30.0);
        let spot = spots(&machines, &camera, 0.0, &rules(1.0))[0];
        let f = machines[0].0;
        let lowest = [[f[0], f[1]], [f[2], f[1]], [f[2], f[3]], [f[0], f[3]]]
            .map(|[x, y]| camera.world_to_screen([x, y, 0.0])[1])
            .into_iter()
            .fold(f32::NEG_INFINITY, f32::max);
        assert!((spot.screen[1] - lowest).abs() < 1e-3);
        assert!(spot.opacity > 0.9, "a fitted plant is not fogged");
    }

    #[test]
    fn a_segment_through_a_box_hits_and_one_beside_it_misses() {
        let b = Aabb {
            min: [0.0; 3],
            max: [1.0; 3],
        };
        assert!(segment_hits([-1.0, 0.5, 0.5], [2.0, 0.5, 0.5], &b));
        assert!(!segment_hits([-1.0, 2.0, 0.5], [2.0, 2.0, 0.5], &b));
        assert!(
            !segment_hits([-3.0, 0.5, 0.5], [-1.0, 0.5, 0.5], &b),
            "stops short"
        );
    }
}
