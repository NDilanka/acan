//! Orthogonal belt routes around machine footprints, for a plant laid out in rows.
//!
//! [`route`] finds, for each request, a path from the plinth of one machine to the plinth of
//! another. The path keeps [`CLEARANCE`] plan units from every machine it does not start or end
//! at, stays out of every keep-out area (where machine names hang), runs only on axis-aligned
//! segments, and stays inside the frame it is given.
//!
//! The search runs over a lattice of the lines that matter: every footprint edge pushed out by
//! the clearance, the frame, and a midline in each gap wider than two belt steps (footprints no
//! wider than that add their centre line instead). A lattice node strictly inside a footprint
//! grown by the clearance, or strictly inside a keep-out area, is blocked. Lines are never more
//! than two belt steps apart, so a keep-out area wider and deeper than that has a line strictly
//! inside it each way and no step jumps across it. The grown edges are lines and at least one line runs
//! strictly between them, so a step between two adjacent free nodes is free along its whole
//! length.

use std::collections::BinaryHeap;

/// The clear space a route keeps from every other machine, in plan units. A belt corner is
/// rounded by this radius, so a route turns no tighter than the drawing can bend it.
pub const CLEARANCE: f32 = 8.0;

/// A belt to route: from the plinth of machine `from` to the plinth of machine `to`. Both are
/// indices into the footprints given to [`route`], and they differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// The machine the belt leaves.
    pub from: usize,
    /// The machine the belt enters.
    pub to: usize,
}

/// The spacing kept free for belts running side by side; a gap wider than two of these gets a
/// midline so parallel belts have a track between the outer ones.
const BELT_STEP: f32 = 8.0;
/// The shortest run between two turns that leaves room for both rounded corners.
const MIN_RUN: f32 = 2.0 * CLEARANCE;
/// Lines closer than this are one line.
const MERGE: f32 = 2e-4;
/// Slack for "strictly inside" tests and for finding a line by coordinate. It sits above the
/// rounding of plan coordinates in f32 and well below anything a reader can see.
const EPS: f32 = 5e-4;
/// Cost units per plan unit, so costs stay integer and ties are exact.
const SCALE: f32 = 64.0;
const TURN: u64 = 24 * 64;
const SHORT_RUN: u64 = 40 * 64;
const CROSSING: u64 = 60 * 64;
/// How far past the box round its two machines a route is first searched for, in plan units:
/// a street and a bit.
const REACH: f32 = 120.0;
/// Factor on a lattice edge that a belt with no end in common already uses.
const OVERLAP: u64 = 400;
/// Divisor on a lattice edge that only belts sharing an end with this one use, so the belts out
/// of one machine, or into one machine, gather into one trunk. A dense plant then draws a few
/// trunks into its most used machines instead of a mesh of belts side by side.
const TRUNK: u64 = 4;
/// How far the town reaches past the outermost machines along each lattice line, in plan
/// units: a street and the pad round it.
pub const OUTSKIRT: f32 = 96.0;
/// Factor on a lattice edge outside the town. Every monotone route between two machines is
/// equally long, so without it a long belt takes the one that crosses nothing: round the
/// empty edge of the plan, where it reads as a detour.
const OUTSKIRT_FACTOR: u64 = 2;
const NONE: u32 = u32::MAX;
const FREE: usize = usize::MAX;

const EAST: usize = 0;
const WEST: usize = 1;
const NORTH: usize = 2;
const SOUTH: usize = 3;
const DIRS: [usize; 4] = [EAST, WEST, NORTH, SOUTH];
const HORIZONTAL: usize = 0;
const VERTICAL: usize = 1;

/// Routes each request in order over the plan. `boxes` are machine footprints as
/// `[min x, min y, max x, max y]` in plan units. No route runs strictly inside a `keep_out`
/// rectangle, given the same way; each is wider and deeper than two belt steps. `frame` bounds
/// every route, in the same units.
///
/// Returns one centre line per request, in request order: at least two points, axis-aligned
/// segments, starting on an edge of `boxes[from]` and ending on an edge of `boxes[to]`. `None`
/// where no route fits inside the frame. Each route sees the routes before it: belts that leave
/// the same machine or enter the same machine share a trunk, and other belts keep off each
/// other's track.
pub fn route(
    boxes: &[[f32; 4]],
    keep_out: &[[f32; 4]],
    frame: [f32; 4],
    requests: &[Request],
) -> Vec<Option<Vec<[f32; 2]>>> {
    let usable = frame.iter().all(|v| v.is_finite()) && frame[0] < frame[2] && frame[1] < frame[3];
    if !usable {
        return vec![None; requests.len()];
    }
    let plan = Plan { boxes, keep_out };
    let lattice = Lattice::new(plan, frame, &[]);
    let Some(mut search) = Search::new(&lattice) else {
        return vec![None; requests.len()];
    };
    let mut occupancy = Occupancy::new(&lattice);
    requests
        .iter()
        .map(|&request| {
            let valid = request.from < boxes.len()
                && request.to < boxes.len()
                && request.from != request.to;
            if !valid {
                return None;
            }
            let near = Query {
                plan,
                lattice: &lattice,
                occupancy: &occupancy,
                request,
                mode: Mode::Clear,
                reach: REACH,
            };
            // Most routes stay near the box round their two machines. Searching only there
            // saves exploring the whole plan for a route that pays to cross other belts; a
            // route that needs to go further still finds its way on the second search.
            let points = search
                .run(&near)
                .or_else(|| {
                    search.run(&Query {
                        reach: f32::INFINITY,
                        ..near
                    })
                })
                .or_else(|| tight(plan, frame, request))?;
            occupancy.mark(&lattice, &points, request);
            Some(points)
        })
        .collect()
}

/// Routes one request in [`Mode::Tight`], on a lattice that also has the edges of its end
/// footprints. It is the rare fallback, so it builds its own buffers and does not see the
/// routes placed before it.
fn tight(plan: Plan, frame: [f32; 4], request: Request) -> Option<Vec<[f32; 2]>> {
    let ends = [plan.boxes[request.from], plan.boxes[request.to]];
    let lattice = Lattice::new(plan, frame, &ends);
    let occupancy = Occupancy::new(&lattice);
    let query = Query {
        plan,
        lattice: &lattice,
        occupancy: &occupancy,
        request,
        mode: Mode::Tight,
        reach: f32::INFINITY,
    };
    Search::new(&lattice)?.run(&query)
}

/// What routes go round: the machine footprints and the keep-out areas.
#[derive(Clone, Copy)]
struct Plan<'a> {
    boxes: &'a [[f32; 4]],
    keep_out: &'a [[f32; 4]],
}

/// How a request treats the machines it starts and ends at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Both end machines keep their clearance ring; the route leaves and enters through a
    /// straight stub across the ring, square to the edge and clear of its corners.
    Clear,
    /// Fallback when the rings leave no way through: only the end footprints themselves block,
    /// and the route may start and end anywhere on their edges.
    Tight,
}

/// The routing lattice: sorted line coordinates per axis, the integer cost of the distance from
/// the first line to each line, and, per node, how many footprints grown by the clearance hold
/// it strictly inside.
struct Lattice {
    xs: Vec<f32>,
    ys: Vec<f32>,
    px: Vec<u64>,
    py: Vec<u64>,
    cover: Vec<u16>,
    /// Per horizontal line, the span along x the town covers; per vertical line, along y.
    rows: Vec<[f32; 2]>,
    cols: Vec<[f32; 2]>,
}

impl Lattice {
    /// The lattice over `plan` inside `frame`. The edges of the footprints in `ends` become
    /// lines too, for routes that start and end right on them.
    fn new(plan: Plan, frame: [f32; 4], ends: &[[f32; 4]]) -> Self {
        let xs = lines(plan, ends, frame[0], frame[2], 0);
        let ys = lines(plan, ends, frame[1], frame[3], 1);
        let (px, py) = (prefix(&xs), prefix(&ys));
        let nx = xs.len();
        let mut cover = vec![0u16; nx * ys.len()];
        let grown = plan.boxes.iter().map(|b| inflate(*b, CLEARANCE));
        for grown in grown.chain(plan.keep_out.iter().copied()) {
            let (i0, i1) = inner(&xs, grown[0], grown[2]);
            let (j0, j1) = inner(&ys, grown[1], grown[3]);
            for j in j0..j1 {
                for c in &mut cover[j * nx + i0..j * nx + i1] {
                    *c = c.saturating_add(1);
                }
            }
        }
        let rows = town(plan.boxes, &ys, 1);
        let cols = town(plan.boxes, &xs, 0);
        Self {
            xs,
            ys,
            px,
            py,
            cover,
            rows,
            cols,
        }
    }

    /// Whether node `(i, j)` lies outside the town along its row or its column.
    fn outskirts(&self, i: usize, j: usize) -> bool {
        let (x, y) = (self.xs[i], self.ys[j]);
        let (row, col) = (self.rows[j], self.cols[i]);
        x < row[0] || x > row[1] || y < col[0] || y > col[1]
    }

    /// The step from node `(i, j)` towards `dir`, if the lattice goes on that way.
    fn go(&self, i: usize, j: usize, dir: usize) -> Option<Move> {
        let (nx, ny) = (self.xs.len(), self.ys.len());
        let (ni, nj, edge) = match dir {
            EAST if i + 1 < nx => (i + 1, j, j * (nx - 1) + i),
            WEST if i > 0 => (i - 1, j, j * (nx - 1) + i - 1),
            NORTH if j + 1 < ny => (i, j + 1, j * nx + i),
            SOUTH if j > 0 => (i, j - 1, (j - 1) * nx + i),
            _ => return None,
        };
        let (cost, len) = if horizontal(dir) {
            let (a, b) = (i.min(ni), i.max(ni));
            (self.px[b] - self.px[a], self.xs[b] - self.xs[a])
        } else {
            let (a, b) = (j.min(nj), j.max(nj));
            (self.py[b] - self.py[a], self.ys[b] - self.ys[a])
        };
        Some(Move {
            i: ni,
            j: nj,
            node: nj * nx + ni,
            edge,
            cost,
            len,
        })
    }

    /// A lower bound on the cost from node `(i, j)`, travelling along `axis`, to the lines in
    /// `target`, given as index ranges per axis: the integer length plus the turns the route
    /// must still make. The length matches the step costs exactly, so the bound stays flat
    /// along a straight run towards the target.
    fn estimate(&self, i: usize, j: usize, axis: usize, target: [usize; 4]) -> u64 {
        let dx = axis_gap(&self.px, i, target[0], target[2]);
        let dy = axis_gap(&self.py, j, target[1], target[3]);
        let turn = match (dx > 0, dy > 0) {
            (true, true) => true,
            (true, false) => axis != HORIZONTAL,
            (false, true) => axis != VERTICAL,
            (false, false) => false,
        };
        dx + dy + if turn { TURN } else { 0 }
    }

    /// The index ranges of the lines inside the rectangle `r`, as `[i0, j0, i1, j1]`, inclusive.
    fn span(&self, r: [f32; 4]) -> [usize; 4] {
        let (i0, i1) = around(&self.xs, r[0], r[2]);
        let (j0, j1) = around(&self.ys, r[1], r[3]);
        [i0, j0, i1.saturating_sub(1), j1.saturating_sub(1)]
    }

    fn nodes(&self) -> usize {
        self.xs.len() * self.ys.len()
    }

    fn point(&self, node: usize) -> [f32; 2] {
        let nx = self.xs.len();
        [self.xs[node % nx], self.ys[node / nx]]
    }
}

/// One lattice step: the node reached, as indices and as a node number, the edge walked
/// (horizontal and vertical edges are numbered apart), its integer cost and its length.
struct Move {
    i: usize,
    j: usize,
    node: usize,
    edge: usize,
    cost: u64,
    len: f32,
}

/// The lattice lines along one axis, sorted, merged and clipped to `[lo, hi]`. Each footprint
/// gives its grown edges and its centre line; the centre keeps a step from spanning the whole
/// grown footprint between two free nodes.
fn lines(plan: Plan, ends: &[[f32; 4]], lo: f32, hi: f32, axis: usize) -> Vec<f32> {
    let boxes = plan.boxes;
    let mut raw = Vec::with_capacity(boxes.len() * 3 + ends.len() * 2 + 2);
    raw.extend([lo, hi]);
    let mut keep = |v: f32| {
        if v.is_finite() && v >= lo && v <= hi {
            raw.push(v);
        }
    };
    for b in boxes {
        let (a, c) = (b[axis], b[axis + 2]);
        keep(a - CLEARANCE);
        if c - a <= 2.0 * BELT_STEP {
            keep(0.5 * (a + c));
        }
        keep(c + CLEARANCE);
    }
    for b in ends {
        keep(b[axis]);
        keep(b[axis + 2]);
    }
    raw.sort_by(f32::total_cmp);
    raw.dedup_by(|later, kept| *later - *kept <= MERGE);
    let mut out = Vec::with_capacity(raw.len() * 2);
    for pair in raw.windows(2) {
        out.push(pair[0]);
        // Split a gap into equal pieces no wider than two belt steps, so belts running side by
        // side each get a track between the outer ones.
        let gap = pair[1] - pair[0];
        let pieces = (gap / (2.0 * BELT_STEP)).ceil() as usize;
        for i in 1..pieces {
            out.push(pair[0] + gap * i as f32 / pieces as f32);
        }
    }
    out.extend(raw.last().copied());
    out
}

/// For each line across `axis` (0 for vertical lines at x, 1 for horizontal lines at y), the
/// span the town covers along it: from the first to the last footprint within [`OUTSKIRT`] of
/// the line, pushed out by as much. A line no footprint comes near has an empty span.
fn town(boxes: &[[f32; 4]], lines: &[f32], axis: usize) -> Vec<[f32; 2]> {
    let along = 1 - axis;
    lines
        .iter()
        .map(|&v| {
            boxes
                .iter()
                .filter(|b| v >= b[axis] - OUTSKIRT && v <= b[axis + 2] + OUTSKIRT)
                .fold([f32::INFINITY, f32::NEG_INFINITY], |s, b| {
                    [
                        s[0].min(b[along] - OUTSKIRT),
                        s[1].max(b[along + 2] + OUTSKIRT),
                    ]
                })
        })
        .collect()
}

/// The integer cost of the distance from the first line to each line.
fn prefix(lines: &[f32]) -> Vec<u64> {
    let mut total = 0;
    let mut out = Vec::with_capacity(lines.len());
    out.push(0);
    for pair in lines.windows(2) {
        // A gap between lines is positive and bounded by the frame, so the rounded value fits.
        total += (((pair[1] - pair[0]) * SCALE).round() as u64).max(1);
        out.push(total);
    }
    out
}

/// The cost from line `k` to the nearest line in `lo..=hi`; zero when the range is empty.
fn axis_gap(prefix: &[u64], k: usize, lo: usize, hi: usize) -> u64 {
    if lo > hi {
        0
    } else if k < lo {
        prefix[lo] - prefix[k]
    } else if k > hi {
        prefix[k] - prefix[hi]
    } else {
        0
    }
}

/// The index range of lines strictly between `lo` and `hi`.
fn inner(lines: &[f32], lo: f32, hi: f32) -> (usize, usize) {
    let start = lines.partition_point(|&v| v <= lo + EPS);
    let end = lines.partition_point(|&v| v < hi - EPS);
    (start, end.max(start))
}

/// The index range of lines from `lo` to `hi`, both included.
fn around(lines: &[f32], lo: f32, hi: f32) -> (usize, usize) {
    let start = lines.partition_point(|&v| v < lo - EPS);
    let end = lines.partition_point(|&v| v <= hi + EPS);
    (start, end.max(start))
}

/// The index of the line at `v`, if there is one.
fn find(lines: &[f32], v: f32) -> Option<usize> {
    let i = lines.partition_point(|&x| x < v);
    [i.checked_sub(1), Some(i)]
        .into_iter()
        .flatten()
        .filter(|&k| k < lines.len() && (lines[k] - v).abs() <= EPS)
        .min_by(|&a, &b| (lines[a] - v).abs().total_cmp(&(lines[b] - v).abs()))
}

fn inflate(b: [f32; 4], by: f32) -> [f32; 4] {
    [b[0] - by, b[1] - by, b[2] + by, b[3] + by]
}

fn strictly_inside(r: [f32; 4], p: [f32; 2]) -> bool {
    p[0] > r[0] + EPS && p[0] < r[2] - EPS && p[1] > r[1] + EPS && p[1] < r[3] - EPS
}

/// Whether the axis-aligned segment `a`-`b` runs strictly inside `r` anywhere.
fn segment_hits(r: [f32; 4], a: [f32; 2], b: [f32; 2]) -> bool {
    a[0].max(b[0]) > r[0] + EPS
        && a[0].min(b[0]) < r[2] - EPS
        && a[1].max(b[1]) > r[1] + EPS
        && a[1].min(b[1]) < r[3] - EPS
}

fn horizontal(dir: usize) -> bool {
    axis(dir) == HORIZONTAL
}

/// The axis a direction runs along: [`HORIZONTAL`] or [`VERTICAL`].
fn axis(dir: usize) -> usize {
    dir >> 1
}

/// The belts that run on one lattice edge: the machine they all leave, and the one they all
/// enter, where they share one. Belts that share an end are kin: they may run on one track, as
/// a trunk out of the machine they leave or into the machine they enter.
#[derive(Clone, Copy)]
struct Owner {
    used: bool,
    from: usize,
    to: usize,
}

impl Owner {
    const EMPTY: Self = Self {
        used: false,
        from: FREE,
        to: FREE,
    };

    fn add(&mut self, request: Request) {
        if !self.used {
            *self = Self {
                used: true,
                from: request.from,
                to: request.to,
            };
            return;
        }
        if self.from != request.from {
            self.from = FREE;
        }
        if self.to != request.to {
            self.to = FREE;
        }
    }

    /// Whether a belt that is not kin to `request` runs here.
    fn other(self, request: Request) -> bool {
        self.used && self.from != request.from && self.to != request.to
    }

    /// Whether every belt here is kin to `request`, by the same end.
    fn kin(self, request: Request) -> bool {
        self.used && (self.from == request.from || self.to == request.to)
    }
}

/// Which lattice edges the routes placed so far run on.
struct Occupancy {
    horizontal: Vec<Owner>,
    vertical: Vec<Owner>,
}

impl Occupancy {
    fn new(lattice: &Lattice) -> Self {
        let (nx, ny) = (lattice.xs.len(), lattice.ys.len());
        Self {
            horizontal: vec![Owner::EMPTY; ny * (nx - 1)],
            vertical: vec![Owner::EMPTY; (ny - 1) * nx],
        }
    }

    /// The cost of the step `m` towards `dir` for the belt `request`: its length, scaled by who
    /// already runs there and by [`OUTSKIRT_FACTOR`] when `outskirts`, plus a crossing charge
    /// when it lands across a belt that is not kin.
    fn cost(
        &self,
        lattice: &Lattice,
        m: &Move,
        dir: usize,
        request: Request,
        outskirts: bool,
    ) -> u64 {
        let (nx, ny) = (lattice.xs.len(), lattice.ys.len());
        let (owner, crosses) = if horizontal(dir) {
            let north = m.j + 1 < ny && self.vertical[m.node].other(request);
            let south = m.j > 0 && self.vertical[m.node - nx].other(request);
            (self.horizontal[m.edge], north || south)
        } else {
            let row = m.j * (nx - 1);
            let east = m.i + 1 < nx && self.horizontal[row + m.i].other(request);
            let west = m.i > 0 && self.horizontal[row + m.i - 1].other(request);
            (self.vertical[m.edge], east || west)
        };
        let base = if outskirts {
            m.cost * OUTSKIRT_FACTOR
        } else {
            m.cost
        };
        let step = if owner.other(request) {
            base * OVERLAP
        } else if owner.kin(request) {
            (base / TRUNK).max(1)
        } else {
            base
        };
        if crosses { step + CROSSING } else { step }
    }

    /// Records the lattice edges under the route `points` as used by the belt `request`. Works
    /// on coordinates, so a route found on another lattice over the same plan is recorded too.
    fn mark(&mut self, lattice: &Lattice, points: &[[f32; 2]], request: Request) {
        let nx = lattice.xs.len();
        for pair in points.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if a[1] == b[1] {
                let Some(j) = find(&lattice.ys, a[1]) else {
                    continue;
                };
                let (i0, i1) = around(&lattice.xs, a[0].min(b[0]), a[0].max(b[0]));
                for i in i0..i1.saturating_sub(1) {
                    self.horizontal[j * (nx - 1) + i].add(request);
                }
            } else {
                let Some(i) = find(&lattice.xs, a[0]) else {
                    continue;
                };
                let (j0, j1) = around(&lattice.ys, a[1].min(b[1]), a[1].max(b[1]));
                for j in j0..j1.saturating_sub(1) {
                    self.vertical[j * nx + i].add(request);
                }
            }
        }
    }
}

/// Where a route may leave or enter a machine: a lattice node, the direction of travel there,
/// and the point on the machine's edge.
struct Port {
    node: usize,
    dir: usize,
    point: [f32; 2],
}

/// One request in one mode, with everything the search reads.
struct Query<'a> {
    plan: Plan<'a>,
    lattice: &'a Lattice,
    occupancy: &'a Occupancy,
    request: Request,
    mode: Mode,
    /// How far past the box round both end machines the search may go, in plan units.
    reach: f32,
}

impl Query<'_> {
    /// Whether the route may not pass through `node`. In [`Mode::Tight`] the end footprints
    /// block only their own interior; their edges are lines of that lattice and a line runs
    /// strictly between each pair of them, so no step crosses them either.
    fn blocked(&self, node: usize) -> bool {
        let cover = self.lattice.cover[node];
        match self.mode {
            Mode::Clear => cover > 0,
            Mode::Tight => {
                let p = self.lattice.point(node);
                let boxes = self.plan.boxes;
                let ends = [boxes[self.request.from], boxes[self.request.to]];
                let own = ends
                    .iter()
                    .filter(|b| strictly_inside(inflate(**b, CLEARANCE), p))
                    .count();
                usize::from(cover) > own || ends.iter().any(|b| strictly_inside(*b, p))
            }
        }
    }

    fn stub_clear(&self, own: usize, a: [f32; 2], b: [f32; 2]) -> bool {
        let plan = self.plan;
        plan.boxes
            .iter()
            .enumerate()
            .all(|(m, r)| m == own || !segment_hits(inflate(*r, CLEARANCE), a, b))
            && plan.keep_out.iter().all(|r| !segment_hits(*r, a, b))
    }

    /// The ports of machine `own`; `leaving` gives the travel direction out of it, otherwise
    /// into it.
    fn ports(&self, own: usize, leaving: bool) -> Vec<Port> {
        let lattice = self.lattice;
        let nx = lattice.xs.len();
        let b = self.plan.boxes[own];
        let off = match self.mode {
            Mode::Clear => CLEARANCE,
            Mode::Tight => 0.0,
        };
        let sides = [
            (WEST, b[0] - off, b[0]),
            (EAST, b[2] + off, b[2]),
            (SOUTH, b[1] - off, b[1]),
            (NORTH, b[3] + off, b[3]),
        ];
        let mut ports = Vec::new();
        for (outward, at, edge) in sides {
            let upright = horizontal(outward);
            let (fixed, along, lo, hi) = if upright {
                (&lattice.xs, &lattice.ys, b[1], b[3])
            } else {
                (&lattice.ys, &lattice.xs, b[0], b[2])
            };
            let Some(f) = find(fixed, at) else {
                continue;
            };
            let (k0, k1) = match self.mode {
                Mode::Clear => inner(along, lo, hi),
                Mode::Tight => around(along, lo, hi),
            };
            for k in k0..k1 {
                let node = if upright { k * nx + f } else { f * nx + k };
                let p = lattice.point(node);
                let (point, free) = match self.mode {
                    Mode::Clear => {
                        let start = if upright { [edge, p[1]] } else { [p[0], edge] };
                        let free = lattice.cover[node] == 0 && self.stub_clear(own, start, p);
                        (start, free)
                    }
                    Mode::Tight => (p, !self.blocked(node)),
                };
                if free {
                    let dir = if leaving { outward } else { outward ^ 1 };
                    ports.push(Port { node, dir, point });
                }
            }
        }
        ports
    }
}

/// The best label found so far for one search state: a lattice node and the axis the route
/// travels along there. Going back the way it came never pays, so the axis is enough.
#[derive(Clone, Copy)]
struct Slot {
    round: u32,
    parent: u32,
    cost: u64,
    /// The length of the straight run that ends here.
    run: f32,
    /// Whether that run is the first one, starting at the machine edge.
    leaving: bool,
}

/// Search buffers reused across requests. A slot belongs to the current request only when its
/// round matches, so nothing is cleared between requests.
struct Search {
    round: u32,
    slots: Vec<Slot>,
    goal: Vec<u32>,
    heap: BinaryHeap<u128>,
}

/// The largest estimate or cost a heap key holds; larger values saturate, which only blunts
/// the tie-break.
const KEY_MAX: u64 = (1 << 40) - 1;

/// Packs a heap entry into one integer so the max-heap pops the lowest estimate first, then
/// the highest cost so far (the deepest state), then the lowest state index.
fn key(f: u64, g: u64, id: u32) -> u128 {
    let depth = KEY_MAX - g.min(KEY_MAX);
    !((u128::from(f.min(KEY_MAX)) << 72) | (u128::from(depth) << 32) | u128::from(id))
}

/// The estimate, cost so far and state index packed by [`key`].
fn unkey(key: u128) -> (u64, u64, u32) {
    let k = !key;
    // Each field is shifted down and masked to its width, so the casts keep every bit.
    let f = (k >> 72) as u64 & KEY_MAX;
    let depth = (k >> 32) as u64 & KEY_MAX;
    (f, KEY_MAX - depth, k as u32)
}

impl Search {
    /// Buffers for `lattice`; `None` when its states do not fit the `u32` indices.
    fn new(lattice: &Lattice) -> Option<Self> {
        let nodes = lattice.nodes();
        let states = nodes.checked_mul(2).filter(|&s| s < NONE as usize)?;
        let empty = Slot {
            round: 0,
            parent: NONE,
            cost: u64::MAX,
            run: 0.0,
            leaving: false,
        };
        Some(Self {
            round: 0,
            slots: vec![empty; states],
            goal: vec![0; nodes],
            heap: BinaryHeap::new(),
        })
    }

    fn begin(&mut self) {
        if self.round == u32::MAX {
            for slot in &mut self.slots {
                slot.round = 0;
            }
            self.goal.fill(0);
            self.round = 0;
        }
        self.round += 1;
        self.heap.clear();
    }

    fn relax(&mut self, state: usize, slot: Slot, h: u64) {
        let old = &mut self.slots[state];
        if old.round == self.round && old.cost <= slot.cost {
            return;
        }
        *old = slot;
        // `Search::new` checks that every state index fits below `NONE`.
        let id = state as u32;
        self.heap.push(key(slot.cost + h, slot.cost, id));
    }

    /// A* from every source port to the cheapest goal port. Returns the route's points.
    fn run(&mut self, query: &Query) -> Option<Vec<[f32; 2]>> {
        let sources = query.ports(query.request.from, true);
        let goals = query.ports(query.request.to, false);
        if sources.is_empty() || goals.is_empty() {
            return None;
        }
        self.begin();
        let round = self.round;
        let lattice = query.lattice;
        let occupancy = query.occupancy;
        let from = query.request.from;
        let target = lattice.span(inflate(query.plan.boxes[query.request.to], CLEARANCE));
        let (a, b) = (query.plan.boxes[from], query.plan.boxes[query.request.to]);
        let around_both = [
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ];
        // An infinite reach would make an empty span, so it is capped well past any frame.
        let window = lattice.span(inflate(around_both, query.reach.min(1e9)));
        let nx = lattice.xs.len();
        for goal in &goals {
            self.goal[goal.node] = round;
        }
        for source in &sources {
            let slot = Slot {
                round,
                parent: NONE,
                cost: 0,
                run: CLEARANCE,
                leaving: true,
            };
            let (i, j) = (source.node % nx, source.node / nx);
            let along = axis(source.dir);
            let h = lattice.estimate(i, j, along, target);
            self.relax(source.node * 2 + along, slot, h);
        }
        let mut best: Option<(u64, usize, usize)> = None;
        while let Some(entry) = self.heap.pop() {
            let (f, g, id) = unkey(entry);
            if best.is_some_and(|(c, _, _)| f >= c) {
                break;
            }
            let state = id as usize;
            let slot = self.slots[state];
            if slot.cost.min(KEY_MAX) != g {
                continue;
            }
            let (node, along) = (state / 2, state % 2);
            if self.goal[node] == round {
                for (k, goal) in goals.iter().enumerate() {
                    if goal.node == node {
                        let c = g + bend(along, axis(goal.dir), slot.run, slot.leaving);
                        if best.is_none_or(|(b, _, _)| c < b) {
                            best = Some((c, state, k));
                        }
                    }
                }
            }
            let (i, j) = (node % nx, node / nx);
            let back = (slot.parent != NONE).then_some(slot.parent as usize / 2);
            for dir in DIRS {
                let Some(m) = lattice.go(i, j, dir) else {
                    continue;
                };
                let outside =
                    m.i < window[0] || m.i > window[2] || m.j < window[1] || m.j > window[3];
                if outside || back == Some(m.node) || query.blocked(m.node) {
                    continue;
                }
                let next = axis(dir);
                let straight = next == along;
                let outskirts = lattice.outskirts(i, j) || lattice.outskirts(m.i, m.j);
                let cost = g
                    + occupancy.cost(lattice, &m, dir, query.request, outskirts)
                    + bend(along, next, slot.run, slot.leaving);
                let label = Slot {
                    round,
                    parent: id,
                    cost,
                    run: if straight { slot.run + m.len } else { m.len },
                    leaving: slot.leaving && straight,
                };
                let h = lattice.estimate(m.i, m.j, next, target);
                self.relax(m.node * 2 + next, label, h);
            }
        }
        let (_, last, goal) = best?;
        let mut nodes = Vec::new();
        let mut state = last;
        loop {
            nodes.push(state / 2);
            let parent = self.slots[state].parent;
            if parent == NONE {
                break;
            }
            state = parent as usize;
        }
        nodes.reverse();
        let source = sources.iter().find(|s| s.node * 2 + axis(s.dir) == state)?;
        let points = tidy(
            std::iter::once(source.point)
                .chain(nodes.iter().map(|&n| lattice.point(n)))
                .chain(std::iter::once(goals[goal].point)),
        );
        (points.len() >= 2).then_some(points)
    }
}

/// The cost of going on along axis `next` after travelling along `along` on a run of length
/// `run`.
fn bend(along: usize, next: usize, run: f32, leaving: bool) -> u64 {
    if along == next {
        0
    } else if !leaving && run < MIN_RUN - EPS {
        TURN + SHORT_RUN
    } else {
        TURN
    }
}

/// Drops repeated points and points between two collinear neighbours. Coordinates come from
/// the same lattice lines, so exact comparison is the intended test.
fn tidy(points: impl Iterator<Item = [f32; 2]>) -> Vec<[f32; 2]> {
    let mut out: Vec<[f32; 2]> = Vec::new();
    for p in points {
        if out.last() == Some(&p) {
            continue;
        }
        if let [.., a, b] = *out.as_slice()
            && ((a[0] == b[0] && b[0] == p[0]) || (a[1] == b[1] && b[1] == p[1]))
        {
            out.pop();
            if out.last() == Some(&p) {
                continue;
            }
        }
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const TOL: f32 = 1e-3;

    fn frame_around(boxes: &[[f32; 4]], margin: f32) -> [f32; 4] {
        let bounds = boxes
            .iter()
            .fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |f, b| {
                [
                    f[0].min(b[0]),
                    f[1].min(b[1]),
                    f[2].max(b[2]),
                    f[3].max(b[3]),
                ]
            });
        inflate(bounds, margin)
    }

    fn on_boundary(footprint: [f32; 4], p: [f32; 2]) -> bool {
        let within = p[0] >= footprint[0] - TOL
            && p[0] <= footprint[2] + TOL
            && p[1] >= footprint[1] - TOL
            && p[1] <= footprint[3] + TOL;
        within
            && ((p[0] - footprint[0]).abs() < TOL
                || (p[0] - footprint[2]).abs() < TOL
                || (p[1] - footprint[1]).abs() < TOL
                || (p[1] - footprint[3]).abs() < TOL)
    }

    /// The smallest Chebyshev distance from the segment `a`-`b` to the footprint `r`.
    fn chebyshev(a: [f32; 2], b: [f32; 2], r: [f32; 4]) -> f32 {
        let gx = (r[0] - a[0].max(b[0])).max(a[0].min(b[0]) - r[2]).max(0.0);
        let gy = (r[1] - a[1].max(b[1])).max(a[1].min(b[1]) - r[3]).max(0.0);
        gx.max(gy)
    }

    /// Whether the segment `a`-`b` runs under the footprint `r`, inset as the layout checks it.
    fn enters(a: [f32; 2], b: [f32; 2], r: [f32; 4]) -> bool {
        let inset = 0.5;
        a[0].max(b[0]) > r[0] + inset
            && a[0].min(b[0]) < r[2] - inset
            && a[1].max(b[1]) > r[1] + inset
            && a[1].min(b[1]) < r[3] - inset
    }

    /// Checks every invariant on the routes that exist; returns how many are `None`.
    fn check(
        boxes: &[[f32; 4]],
        frame: [f32; 4],
        requests: &[Request],
        routes: &[Option<Vec<[f32; 2]>>],
    ) -> usize {
        assert_eq!(routes.len(), requests.len());
        let mut missing = 0;
        for (k, (request, path)) in requests.iter().zip(routes).enumerate() {
            let Some(path) = path else {
                missing += 1;
                continue;
            };
            let (from, to) = (request.from, request.to);
            assert!(path.len() >= 2, "route {k} ({from}->{to}) has no segment");
            for pair in path.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                assert!(
                    (a[0] == b[0]) != (a[1] == b[1]),
                    "route {k} ({from}->{to}) has a diagonal or empty segment: {a:?} {b:?}"
                );
                for (m, footprint) in boxes.iter().enumerate() {
                    assert!(
                        !enters(a, b, *footprint),
                        "route {k} ({from}->{to}) runs under machine {m}: {a:?} {b:?}"
                    );
                    if m != from && m != to {
                        let d = chebyshev(a, b, *footprint);
                        assert!(
                            d >= CLEARANCE - TOL,
                            "route {k} ({from}->{to}) passes {d} from machine {m}: {a:?} {b:?}"
                        );
                    }
                }
            }
            for triple in path.windows(3) {
                let (a, b, c) = (triple[0], triple[1], triple[2]);
                let collinear = (a[0] == b[0] && b[0] == c[0]) || (a[1] == b[1] && b[1] == c[1]);
                assert!(
                    !collinear,
                    "route {k} ({from}->{to}) has an unmerged point {b:?}"
                );
            }
            for p in path {
                let within = p[0] >= frame[0] - TOL
                    && p[0] <= frame[2] + TOL
                    && p[1] >= frame[1] - TOL
                    && p[1] <= frame[3] + TOL;
                assert!(within, "route {k} ({from}->{to}) leaves the frame at {p:?}");
            }
            let (first, last) = (path[0], path[path.len() - 1]);
            assert!(
                on_boundary(boxes[from], first),
                "route {k} ({from}->{to}) does not start on its machine: {first:?}"
            );
            assert!(
                on_boundary(boxes[to], last),
                "route {k} ({from}->{to}) does not end on its machine: {last:?}"
            );
        }
        missing
    }

    /// Routes and checks, and requires every request to get a route.
    fn routed(boxes: &[[f32; 4]], frame: [f32; 4], requests: &[Request]) -> Vec<Vec<[f32; 2]>> {
        let routes = route(boxes, &[], frame, requests);
        assert_eq!(
            check(boxes, frame, requests, &routes),
            0,
            "a request has no route"
        );
        routes.into_iter().flatten().collect()
    }

    fn grid(cols: usize, rows: usize, size: [f32; 2], gap: f32) -> Vec<[f32; 4]> {
        let mut boxes = Vec::with_capacity(cols * rows);
        for r in 0..rows {
            for c in 0..cols {
                let x = c as f32 * (size[0] + gap);
                let y = r as f32 * (size[1] + gap);
                boxes.push([x, y, x + size[0], y + size[1]]);
            }
        }
        boxes
    }

    fn requests(pairs: &[(usize, usize)]) -> Vec<Request> {
        pairs
            .iter()
            .map(|&(from, to)| Request { from, to })
            .collect()
    }

    fn turns(routes: &[Vec<[f32; 2]>]) -> usize {
        routes.iter().map(|p| p.len() - 2).sum()
    }

    fn overlaps(a: [f32; 2], b: [f32; 2], c: [f32; 2], d: [f32; 2]) -> bool {
        let span = |p: f32, q: f32, r: f32, s: f32| p.max(q).min(r.max(s)) - p.min(q).max(r.min(s));
        let flat = |u: [f32; 2], v: [f32; 2]| u[1] == v[1];
        if flat(a, b) && flat(c, d) {
            (a[1] - c[1]).abs() < TOL && span(a[0], b[0], c[0], d[0]) > TOL
        } else if !flat(a, b) && !flat(c, d) {
            (a[0] - c[0]).abs() < TOL && span(a[1], b[1], c[1], d[1]) > TOL
        } else {
            false
        }
    }

    /// How many routes run along a stretch of a route from another machine.
    fn shared(requests: &[Request], routes: &[Option<Vec<[f32; 2]>>]) -> usize {
        let placed: Vec<(usize, &Vec<[f32; 2]>)> = requests
            .iter()
            .zip(routes)
            .filter_map(|(r, p)| p.as_ref().map(|p| (r.from, p)))
            .collect();
        placed
            .iter()
            .filter(|(from, path)| {
                placed.iter().any(|(other, rival)| {
                    other != from
                        && path
                            .windows(2)
                            .any(|s| rival.windows(2).any(|t| overlaps(s[0], s[1], t[0], t[1])))
                })
            })
            .count()
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.next() % (hi - lo + 1)
        }
    }

    /// A plant in four bands, boxes in serpentine order, with chain belts and random belts.
    fn serpentine() -> (Vec<[f32; 4]>, [f32; 4], Vec<Request>) {
        let mut lcg = Lcg(0x5eed);
        let depth = 86.0;
        let mut boxes = Vec::new();
        for (band, count) in [12, 12, 11, 11].into_iter().enumerate() {
            let top = band as f32 * (depth + 120.0);
            let mut row = Vec::with_capacity(count);
            let mut x = 0.0;
            for _ in 0..count {
                let w = lcg.range(400, 960) as f32 / 10.0;
                let d = lcg.range(400, 860) as f32 / 10.0;
                let y = top + (depth - d) * 0.5;
                row.push([x, y, x + w, y + d]);
                x += w + 72.0;
            }
            if band % 2 == 1 {
                row.reverse();
            }
            boxes.extend(row);
        }
        let n = boxes.len();
        let mut pairs: Vec<(usize, usize)> = (0..n - 1).map(|k| (k, k + 1)).collect();
        while pairs.len() < 80 {
            // The values are below the box count, which fits in usize.
            let from = lcg.range(0, n as u64 - 1) as usize;
            let to = lcg.range(0, n as u64 - 1) as usize;
            if from != to {
                pairs.push((from, to));
            }
        }
        let frame = frame_around(&boxes, 24.0);
        (boxes, frame, requests(&pairs))
    }

    #[test]
    fn route_side_by_side_both_ways() {
        let boxes = [[0.0, 0.0, 50.0, 96.0], [122.0, 11.0, 166.0, 85.0]];
        let frame = frame_around(&boxes, 24.0);
        let routes = routed(&boxes, frame, &requests(&[(0, 1), (1, 0)]));
        // Facing edges with a clear gap give one straight belt each way.
        assert_eq!(routes[0].len(), 2, "{:?}", routes[0]);
        assert_eq!(routes[1].len(), 2, "{:?}", routes[1]);
        assert_ne!(
            routes[0][0][1], routes[1][0][1],
            "the two belts share a track"
        );
    }

    #[test]
    fn route_grid_goes_around_the_middle() {
        let boxes = grid(3, 3, [60.0, 60.0], 72.0);
        let frame = frame_around(&boxes, 24.0);
        let reqs = requests(&[
            (3, 5),
            (5, 3),
            (1, 7),
            (0, 8),
            (8, 0),
            (2, 6),
            (0, 2),
            (6, 4),
        ]);
        let routes = routed(&boxes, frame, &reqs);
        for (request, path) in reqs.iter().zip(&routes) {
            if request.from == 4 || request.to == 4 {
                continue;
            }
            for s in path.windows(2) {
                assert!(
                    chebyshev(s[0], s[1], boxes[4]) >= CLEARANCE - TOL,
                    "{request:?} enters the middle box: {path:?}"
                );
            }
        }
    }

    #[test]
    fn route_tight_columns() {
        // Two columns of five, 40 apart both ways.
        let mut boxes = Vec::new();
        for c in 0..2 {
            for r in 0..5 {
                let x = c as f32 * 90.0;
                let y = r as f32 * 80.0;
                boxes.push([x, y, x + 50.0, y + 40.0]);
            }
        }
        let frame = frame_around(&boxes, 24.0);
        routed(
            &boxes,
            frame,
            &requests(&[(0, 4), (4, 0), (0, 9), (5, 4), (2, 7), (1, 8)]),
        );
    }

    #[test]
    fn route_reaches_boxes_at_the_frame_edge() {
        let boxes = [
            [0.0, 0.0, 44.0, 74.0],
            [116.0, 30.0, 134.0, 48.0],
            [206.0, 0.0, 256.0, 96.0],
            [100.0, 120.0, 150.0, 160.0],
        ];
        let frame = frame_around(&boxes, 24.0);
        routed(
            &boxes,
            frame,
            &requests(&[(0, 2), (2, 0), (0, 3), (3, 2), (1, 3)]),
        );
    }

    #[test]
    fn route_rejects_bad_requests() {
        let boxes = [[0.0, 0.0, 40.0, 40.0], [100.0, 0.0, 140.0, 40.0]];
        let frame = frame_around(&boxes, 24.0);
        let routes = route(&boxes, &[], frame, &requests(&[(0, 0), (0, 5), (0, 1)]));
        assert!(routes[0].is_none() && routes[1].is_none());
        assert!(routes[2].is_some());
    }

    #[test]
    fn route_falls_back_when_rings_leave_no_stub() {
        // The frame hugs the two machines and their rings overlap, so no stub fits; the route
        // runs edge to edge instead, still clear of the third machine.
        let boxes = [
            [0.0, 0.0, 40.0, 40.0],
            [50.0, 0.0, 90.0, 40.0],
            [0.0, 50.0, 90.0, 90.0],
        ];
        let frame = frame_around(&boxes, 4.0);
        let routes = routed(&boxes, frame, &requests(&[(0, 1), (1, 0)]));
        assert!(routes.iter().all(|p| p.len() == 2), "{routes:?}");
    }

    #[test]
    fn route_is_none_when_walled_off() {
        // The middle machine spans the frame from bottom to top.
        let boxes = [
            [0.0, 0.0, 40.0, 40.0],
            [100.0, -30.0, 140.0, 70.0],
            [200.0, 0.0, 240.0, 40.0],
        ];
        let frame = [-24.0, -30.0, 264.0, 70.0];
        let reqs = requests(&[(0, 2), (0, 1)]);
        let routes = route(&boxes, &[], frame, &reqs);
        assert_eq!(check(&boxes, frame, &reqs, &routes), 1);
        assert!(routes[0].is_none());
    }

    #[test]
    fn route_serpentine_plant() {
        let (boxes, frame, reqs) = serpentine();
        assert_eq!(boxes.len(), 46);
        let start = Instant::now();
        let routes = route(&boxes, &[], frame, &reqs);
        let elapsed = start.elapsed();
        let missing = check(&boxes, frame, &reqs, &routes);
        let shared = shared(&reqs, &routes);
        let placed: Vec<Vec<[f32; 2]>> = routes.into_iter().flatten().collect();
        eprintln!(
            "serpentine: {elapsed:?}, {} requests, {} routes, {missing} none, {} turns, {shared} sharing a track with another machine's belt",
            reqs.len(),
            placed.len(),
            turns(&placed),
        );
        assert_eq!(missing, 0);
        assert!(elapsed.as_secs() < 20);
    }

    #[test]
    fn belts_into_one_machine_gather_into_one_trunk() {
        // Three machines in a row above a fourth; each imports it. The belts share a track into
        // it instead of running side by side.
        let mut boxes = grid(3, 1, [60.0, 60.0], 72.0);
        boxes.push([66.0, 240.0, 126.0, 300.0]);
        let frame = frame_around(&boxes, 48.0);
        let reqs = requests(&[(0, 3), (1, 3), (2, 3)]);
        let routes = route(&boxes, &[], frame, &reqs);
        assert_eq!(check(&boxes, frame, &reqs, &routes), 0);
        assert!(
            shared(&reqs, &routes) >= 2,
            "the belts into one machine run apart: {routes:?}"
        );
    }

    #[test]
    fn route_keeps_out_of_keep_out_areas() {
        // The straight belt between the two machines would cross the band; it goes round.
        let boxes = [[0.0, 0.0, 40.0, 40.0], [200.0, 0.0, 240.0, 40.0]];
        let band = [80.0, -40.0, 160.0, 80.0];
        let frame = [-24.0, -120.0, 264.0, 160.0];
        let reqs = requests(&[(0, 1), (1, 0)]);
        let routes = route(&boxes, &[band], frame, &reqs);
        assert_eq!(check(&boxes, frame, &reqs, &routes), 0);
        for path in routes.iter().flatten() {
            for s in path.windows(2) {
                assert!(!segment_hits(band, s[0], s[1]), "{path:?} crosses the band");
            }
        }
    }

    #[test]
    fn route_is_deterministic() {
        let (boxes, frame, reqs) = serpentine();
        assert_eq!(
            route(&boxes, &[], frame, &reqs),
            route(&boxes, &[], frame, &reqs)
        );
    }
}
