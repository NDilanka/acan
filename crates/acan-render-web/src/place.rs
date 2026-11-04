//! Plan placement. Machines grow out of the centre of the plan as districts, on the floor grid.
//!
//! A district is one group of machines, such as one crate (see `Plant::districts`). The most
//! connected district is placed first, at the centre. Each later district is the one most
//! connected to the districts already placed. It starts beside the machines it imports or is
//! imported by, or beside the middle of the town when it has none, and grows there as one
//! block: each of its machines takes the free slot nearest both the machines it is tied to and
//! its district's machines placed before it. Imports stay close, a district forms a block, and
//! the plant spreads out round its core like a town. Machines of one district keep a belt's
//! width apart, and machines of two districts keep a street apart. The result depends only on
//! the input, so the same plant always places the same way.

use crate::kind::{Kind, PlanFootprint};
use crate::layout::{Edge, GRID};

/// Clear cells between two machines of one district: a belt runs through them.
const DISTRICT_CELLS: i32 = 3;
/// Clear cells between machines of two districts: the street.
const STREET_CELLS: i32 = 6;

/// The top-left corner of each machine on the floor grid, in plan units, in machine order. Every
/// corner is on the grid, and no two machines are closer than their gap requires. `districts`
/// names each machine's district; an empty slice puts every machine in one district.
pub fn place(kinds: &[Kind], edges: &[Edge], districts: &[u32]) -> Vec<[f32; 2]> {
    let n = kinds.len();
    if n == 0 {
        return Vec::new();
    }
    let (district, count) = district_index(n, districts);
    let adj = adjacency(n, edges);
    let sizes: Vec<(i32, i32)> = kinds.iter().map(|&kind| cells(kind)).collect();

    let mut members = vec![Vec::new(); count];
    for (i, &d) in district.iter().enumerate() {
        members[d as usize].push(i);
    }
    let total: Vec<f32> = adj
        .iter()
        .map(|list| list.iter().map(|&(_, w)| w).sum())
        .collect();
    let intra: Vec<f32> = (0..n)
        .map(|i| {
            adj[i]
                .iter()
                .filter(|&&(j, _)| district[j] == district[i])
                .map(|&(_, w)| w)
                .sum()
        })
        .collect();
    for list in &mut members {
        // Core machines first: the ones most tied to their own district, then the most connected.
        list.sort_by(|&a, &b| {
            intra[b]
                .total_cmp(&intra[a])
                .then(total[b].total_cmp(&total[a]))
                .then(a.cmp(&b))
        });
    }
    let order = district_order(&members, &district, &adj);

    let mut placer = Placer::new(sizes, district, count);
    for d in order {
        let anchor = placer.anchor(&members[d], &adj);
        for &i in &members[d] {
            let target = placer.target(i, &adj, anchor);
            placer.put(i, target);
        }
    }
    placer
        .slot
        .iter()
        .map(|&(cx, cy)| [cx as f32 * GRID, cy as f32 * GRID])
        .collect()
}

/// The district of each machine as a dense index, and how many districts there are. Machines
/// without a name in `districts` all go in district 0.
fn district_index(n: usize, districts: &[u32]) -> (Vec<u32>, usize) {
    if districts.len() != n {
        return (vec![0; n], 1);
    }
    let mut ids = districts.to_vec();
    ids.sort_unstable();
    ids.dedup();
    // There are no more distinct names than `u32` values, so a dense index fits in a `u32`.
    let index = districts
        .iter()
        .map(|id| ids.binary_search(id).unwrap_or(0) as u32)
        .collect();
    (index, ids.len())
}

/// Each machine's neighbours with the imports between them, in one list per machine, sorted by
/// neighbour. Imports in either direction count. Self imports and imports to a missing machine
/// are dropped.
fn adjacency(n: usize, edges: &[Edge]) -> Vec<Vec<(usize, f32)>> {
    let mut adj: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    for edge in edges {
        if edge.from < n && edge.to < n && edge.from != edge.to {
            let w = edge.imports.max(1) as f32;
            adj[edge.from].push((edge.to, w));
            adj[edge.to].push((edge.from, w));
        }
    }
    for list in &mut adj {
        list.sort_by_key(|&(j, _)| j);
        let mut merged: Vec<(usize, f32)> = Vec::with_capacity(list.len());
        for &(j, w) in list.iter() {
            match merged.last_mut() {
                Some(last) if last.0 == j => last.1 += w,
                _ => merged.push((j, w)),
            }
        }
        *list = merged;
    }
    adj
}

/// The order districts are placed in: first the one with the most imports to the others, then
/// each one with the most imports to the districts already placed. Ties go to the bigger
/// district, then the one that comes first.
fn district_order(
    members: &[Vec<usize>],
    district: &[u32],
    adj: &[Vec<(usize, f32)>],
) -> Vec<usize> {
    let count = members.len();
    let mut links: Vec<Vec<(usize, f32)>> = vec![Vec::new(); count];
    for (i, list) in adj.iter().enumerate() {
        for &(j, w) in list {
            if district[i] != district[j] {
                links[district[i] as usize].push((district[j] as usize, w));
            }
        }
    }
    let outside: Vec<f32> = links
        .iter()
        .map(|list| list.iter().map(|&(_, w)| w).sum())
        .collect();
    let mut linked = vec![0.0_f32; count];
    let mut placed = vec![false; count];
    let mut order = Vec::with_capacity(count);
    while order.len() < count {
        let first = order.is_empty();
        let next = (0..count).filter(|&d| !placed[d]).max_by(|&a, &b| {
            let (wa, wb) = if first {
                (outside[a], outside[b])
            } else {
                (linked[a], linked[b])
            };
            wa.total_cmp(&wb)
                .then(members[a].len().cmp(&members[b].len()))
                .then(b.cmp(&a))
        });
        let Some(d) = next else { break };
        placed[d] = true;
        order.push(d);
        for &(e, w) in &links[d] {
            linked[e] += w;
        }
    }
    order
}

/// The footprint of a kind in grid cells, rounded up.
fn cells(kind: Kind) -> (i32, i32) {
    let (w, d) = kind.footprint();
    (
        ((w / GRID).ceil() as i32).max(1),
        ((d / GRID).ceil() as i32).max(1),
    )
}

/// The middle of a `sw` by `sh` machine with its top-left corner at cell `(cx, cy)`, in plan units.
fn middle(cx: i32, cy: i32, sw: i32, sh: i32) -> [f32; 2] {
    [
        (cx as f32 + sw as f32 / 2.0) * GRID,
        (cy as f32 + sh as f32 / 2.0) * GRID,
    ]
}

fn distance2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    dx * dx + dy * dy
}

/// A weighted mean of points, built one point at a time.
#[derive(Clone, Copy, Default)]
struct Mean {
    sum: [f32; 2],
    weight: f32,
}

impl Mean {
    fn add(&mut self, p: [f32; 2], weight: f32) {
        self.sum[0] += weight * p[0];
        self.sum[1] += weight * p[1];
        self.weight += weight;
    }

    fn get(self) -> Option<[f32; 2]> {
        (self.weight > 0.0).then(|| [self.sum[0] / self.weight, self.sum[1] / self.weight])
    }
}

/// The state of a placement in progress: the floor, and for each machine its district, its
/// size, its corner and middle once placed, with the middle of each district and of the town.
struct Placer {
    grid: Grid,
    district: Vec<u32>,
    sizes: Vec<(i32, i32)>,
    slot: Vec<(i32, i32)>,
    centre: Vec<[f32; 2]>,
    placed: Vec<bool>,
    blocks: Vec<Mean>,
    town: Mean,
}

impl Placer {
    fn new(sizes: Vec<(i32, i32)>, district: Vec<u32>, districts: usize) -> Self {
        let n = sizes.len();
        Self {
            grid: Grid::new(&sizes),
            district,
            sizes,
            slot: vec![(0, 0); n],
            centre: vec![[0.0; 2]; n],
            placed: vec![false; n],
            blocks: vec![Mean::default(); districts],
            town: Mean::default(),
        }
    }

    /// Adds the placed machines that machine `i` imports or is imported by to `mean`, weighted by
    /// the imports.
    fn ties(&self, i: usize, adj: &[Vec<(usize, f32)>], mean: &mut Mean) {
        for &(j, w) in &adj[i] {
            if self.placed[j] {
                mean.add(self.centre[j], w);
            }
        }
    }

    /// The point a district grows from before any of its machines is placed: the middle of the
    /// placed machines it is tied to, or the middle of the town, or the origin at the start.
    fn anchor(&self, members: &[usize], adj: &[Vec<(usize, f32)>]) -> [f32; 2] {
        let mut tied = Mean::default();
        for &i in members {
            self.ties(i, adj, &mut tied);
        }
        tied.get().or(self.town.get()).unwrap_or([0.0; 2])
    }

    /// Where machine `i` would best stand: halfway between the placed machines it is tied to and
    /// the placed machines of its district, so it keeps both its belts short and its district
    /// one block. With only one of the two, that one; with neither, `anchor`.
    fn target(&self, i: usize, adj: &[Vec<(usize, f32)>], anchor: [f32; 2]) -> [f32; 2] {
        let mut tied = Mean::default();
        self.ties(i, adj, &mut tied);
        match (tied.get(), self.blocks[self.district[i] as usize].get()) {
            (Some(a), Some(b)) => [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0],
            (a, b) => a.or(b).unwrap_or(anchor),
        }
    }

    /// Places machine `i` in the free slot nearest `target`, and records it.
    fn put(&mut self, i: usize, target: [f32; 2]) {
        let (sw, sh) = self.sizes[i];
        let district = self.district[i];
        let (cx, cy) = loop {
            if let Some(slot) = self.nearest(target, sw, sh, district) {
                break slot;
            }
            self.widen();
        };
        self.grid.mark(cx, cy, sw, sh, district);
        let centre = middle(cx, cy, sw, sh);
        self.slot[i] = (cx, cy);
        self.centre[i] = centre;
        self.placed[i] = true;
        self.blocks[district as usize].add(centre, 1.0);
        self.town.add(centre, 1.0);
    }

    /// A floor twice as wide, with every placed machine marked on it again, for a machine that
    /// finds no room on the floor as it is.
    fn widen(&mut self) {
        let mut grid = Grid::with_side(self.grid.side * 2);
        for (i, &(cx, cy)) in self.slot.iter().enumerate() {
            if self.placed[i] {
                let (sw, sh) = self.sizes[i];
                grid.mark(cx, cy, sw, sh, self.district[i]);
            }
        }
        self.grid = grid;
    }

    /// The free corner for a `sw` by `sh` machine of `district` whose middle is nearest `target`,
    /// or `None` when the floor has no room for it. Of corners as near as each other, the one
    /// nearer the middle of the town wins, so the town grows round instead of towards one side;
    /// then the topmost, then the leftmost. The search goes outward in rings of cells, so the
    /// result is the same every time.
    fn nearest(&self, target: [f32; 2], sw: i32, sh: i32, district: u32) -> Option<(i32, i32)> {
        let want_x = ((target[0] - sw as f32 * GRID / 2.0) / GRID).round() as i32;
        let want_y = ((target[1] - sh as f32 * GRID / 2.0) / GRID).round() as i32;
        let town = self.town.get().unwrap_or(target);
        let mut best: Option<(f32, f32, i32, i32)> = None;
        for r in 0..=self.grid.side {
            // A corner on ring `r` has its middle at least `r - 1` cells from the target.
            if best.is_some_and(|(d2, ..)| (r as f32 - 1.0) * GRID > d2.sqrt()) {
                break;
            }
            for (cx, cy) in ring(want_x, want_y, r) {
                let centre = middle(cx, cy, sw, sh);
                let key = (distance2(centre, target), distance2(centre, town), cy, cx);
                if best.is_some_and(|b| key >= b) || !self.grid.fits(cx, cy, sw, sh, district) {
                    continue;
                }
                best = Some(key);
            }
        }
        best.map(|(.., cy, cx)| (cx, cy))
    }
}

/// The corners of the square ring of cells `r` steps from `(x, y)`. Ring 0 is the centre cell.
fn ring(x: i32, y: i32, r: i32) -> Vec<(i32, i32)> {
    if r == 0 {
        return vec![(x, y)];
    }
    let mut out = Vec::with_capacity(8 * r as usize);
    for dx in -r..=r {
        out.push((x + dx, y - r));
        out.push((x + dx, y + r));
    }
    for dy in -r + 1..r {
        out.push((x - r, y + dy));
        out.push((x + r, y + dy));
    }
    out
}

/// What a floor cell allows a machine still to be placed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cell {
    Free,
    /// Within a street of machines of this one district, so only it may build here.
    Near(u32),
    /// Within a street of machines of two districts or more, so no district may build here.
    Shared,
    /// On a machine or within a belt's width of one.
    Taken,
}

/// The floor, one [`Cell`] per grid cell, centred on the origin.
struct Grid {
    side: i32,
    cells: Vec<Cell>,
}

impl Grid {
    /// A floor twice as wide as the machines in `sizes` and their streets would cover if packed
    /// tight, so a town grown round its middle has room on it.
    fn new(sizes: &[(i32, i32)]) -> Self {
        let pad = STREET_CELLS;
        let area: i64 = sizes
            .iter()
            .map(|&(w, d)| i64::from(w + pad) * i64::from(d + pad))
            .sum();
        Self::with_side(((area as f64).sqrt() * 2.0) as i32 + 64)
    }

    /// An empty floor about `side` cells wide, rounded up to an even width.
    fn with_side(side: i32) -> Self {
        let side = side + side % 2;
        Self {
            side,
            cells: vec![Cell::Free; (side as usize) * (side as usize)],
        }
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        let half = self.side / 2;
        let (u, v) = (x + half, y + half);
        if u < 0 || v < 0 || u >= self.side || v >= self.side {
            return None;
        }
        Some((v * self.side + u) as usize)
    }

    /// Whether a `sw` by `sh` machine of `district` may stand at `(cx, cy)`: every cell under it
    /// is on the floor and free, or near machines of its own district only.
    fn fits(&self, cx: i32, cy: i32, sw: i32, sh: i32, district: u32) -> bool {
        (cy..cy + sh).all(|y| {
            (cx..cx + sw).all(|x| {
                self.index(x, y).is_some_and(|i| match self.cells[i] {
                    Cell::Free => true,
                    Cell::Near(d) => d == district,
                    Cell::Shared | Cell::Taken => false,
                })
            })
        })
    }

    /// Takes a machine's cells and a belt's width around them, and keeps a street's width
    /// around them for its district.
    fn mark(&mut self, cx: i32, cy: i32, sw: i32, sh: i32, district: u32) {
        let (pad, street) = (DISTRICT_CELLS, STREET_CELLS);
        for y in cy - street..cy + sh + street {
            for x in cx - street..cx + sw + street {
                let Some(i) = self.index(x, y) else {
                    continue;
                };
                let close = (cx - pad..cx + sw + pad).contains(&x)
                    && (cy - pad..cy + sh + pad).contains(&y);
                let cell = &mut self.cells[i];
                *cell = match *cell {
                    _ if close => Cell::Taken,
                    Cell::Free => Cell::Near(district),
                    Cell::Near(d) if d != district => Cell::Shared,
                    kept => kept,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::{Kind, PlanFootprint};

    fn stress_input(count: usize) -> (Vec<Kind>, Vec<Edge>, Vec<u32>) {
        crate::plant::stress(count).layout_input()
    }

    /// `[min x, min y, max x, max y]` of each machine placed at `tops`.
    fn boxes(kinds: &[Kind], tops: &[[f32; 2]]) -> Vec<[f32; 4]> {
        kinds
            .iter()
            .zip(tops)
            .map(|(&kind, &[x, y])| {
                let (w, d) = kind.footprint();
                [x, y, x + w, y + d]
            })
            .collect()
    }

    /// The gap between two footprints along the axis where they are furthest apart. Negative when
    /// they overlap on both axes.
    fn gap(a: [f32; 4], b: [f32; 4]) -> f32 {
        let across_x = (b[0] - a[2]).max(a[0] - b[2]);
        let across_y = (b[1] - a[3]).max(a[1] - b[3]);
        across_x.max(across_y)
    }

    fn centre(b: [f32; 4]) -> [f32; 2] {
        [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0]
    }

    fn manhattan(a: [f32; 2], b: [f32; 2]) -> f32 {
        (a[0] - b[0]).abs() + (a[1] - b[1]).abs()
    }

    #[test]
    fn every_corner_is_on_the_floor_grid() {
        let (kinds, edges, districts) = stress_input(300);
        for [x, y] in place(&kinds, &edges, &districts) {
            assert_eq!((x / GRID).round() * GRID, x);
            assert_eq!((y / GRID).round() * GRID, y);
        }
    }

    #[test]
    fn machines_keep_a_belt_apart_and_streets_between_districts() {
        let (kinds, edges, districts) = stress_input(300);
        let tops = place(&kinds, &edges, &districts);
        let boxes = boxes(&kinds, &tops);
        for (i, a) in boxes.iter().enumerate() {
            for (j, b) in boxes.iter().enumerate().skip(i + 1) {
                let cells = if districts[i] == districts[j] {
                    DISTRICT_CELLS
                } else {
                    STREET_CELLS
                };
                let need = cells as f32 * GRID;
                let apart = gap(*a, *b);
                assert!(
                    apart >= need,
                    "machines {i} and {j} are {apart} apart, need {need}"
                );
            }
        }
    }

    #[test]
    fn a_district_forms_a_block() {
        let kinds = vec![Kind::Assembler; 24];
        let edges: Vec<Edge> = (0..24)
            .filter(|i| i % 8 != 7)
            .map(|i| Edge {
                from: i,
                to: i + 1,
                imports: 1,
            })
            .collect();
        let districts: Vec<u32> = (0..24).map(|i| (i / 8) as u32).collect();
        let tops = place(&kinds, &edges, &districts);
        let centres: Vec<[f32; 2]> = boxes(&kinds, &tops).into_iter().map(centre).collect();
        let (mut within, mut within_n) = (0.0_f32, 0_u32);
        let (mut across, mut across_n) = (0.0_f32, 0_u32);
        for (i, a) in centres.iter().enumerate() {
            for (j, b) in centres.iter().enumerate().skip(i + 1) {
                if districts[i] == districts[j] {
                    within += manhattan(*a, *b);
                    within_n += 1;
                } else {
                    across += manhattan(*a, *b);
                    across_n += 1;
                }
            }
        }
        let (within, across) = (within / within_n as f32, across / across_n as f32);
        assert!(
            within < across,
            "a district's machines sit {within} apart on average, others {across}"
        );
    }

    #[test]
    fn imports_sit_closer_than_machines_in_general() {
        let (kinds, edges, districts) = stress_input(200);
        let tops = place(&kinds, &edges, &districts);
        let centres: Vec<[f32; 2]> = boxes(&kinds, &tops).into_iter().map(centre).collect();
        let imported: f32 = edges
            .iter()
            .map(|e| manhattan(centres[e.from], centres[e.to]))
            .sum::<f32>()
            / edges.len() as f32;
        let mut all = 0.0_f32;
        let mut pairs = 0_u32;
        for (i, a) in centres.iter().enumerate() {
            for b in centres.iter().skip(i + 1) {
                all += manhattan(*a, *b);
                pairs += 1;
            }
        }
        let all = all / pairs as f32;
        assert!(
            imported < all,
            "imports are {imported} apart on average, any two machines {all}"
        );
    }

    #[test]
    fn a_full_floor_widens_instead_of_stacking_machines() {
        let size = cells(Kind::Tank);
        let mut placer = Placer::new(vec![size; 6], (0..6).collect(), 6);
        placer.grid = Grid::with_side(16);
        for i in 0..6 {
            placer.put(i, [0.0; 2]);
        }
        assert!(placer.grid.side > 16, "the floor stayed {}", placer.grid.side);
        let (w, h) = size;
        for (i, &(ax, ay)) in placer.slot.iter().enumerate() {
            for (j, &(bx, by)) in placer.slot.iter().enumerate().skip(i + 1) {
                let apart = (bx - (ax + w))
                    .max(ax - (bx + w))
                    .max(by - (ay + h))
                    .max(ay - (by + h));
                assert!(
                    apart >= STREET_CELLS,
                    "machines {i} and {j} are {apart} cells apart"
                );
            }
        }
    }

    #[test]
    #[ignore]
    fn zz_diag() {
        let Ok(path) = std::env::var("LAYOUT_DIAG_PLANT") else {
            return;
        };
        let text = std::fs::read_to_string(path).expect("read");
        let plant = crate::plant::from_scan_json(&text).expect("parse");
        let (kinds, edges, districts) = plant.layout_input();
        let old_edges: Vec<crate::old_layout::Edge> = edges
            .iter()
            .map(|e| crate::old_layout::Edge {
                from: e.from,
                to: e.to,
                imports: e.imports,
            })
            .collect();
        let old = crate::old_layout::layout(&kinds, &old_edges);
        let [x0, y0, x1, y1] = old.bounds;
        eprintln!(
            "OLD layered layout on self: bounds {:?} w {} h {} aspect {}",
            old.bounds,
            x1 - x0,
            y1 - y0,
            (x1 - x0).max(y1 - y0) / (x1 - x0).min(y1 - y0)
        );
        summary("self", &kinds, &edges, &districts);
        let tops = place(&kinds, &edges, &districts);
        {
            let bx = boxes(&kinds, &tops);
            let (dd, _) = district_index(kinds.len(), &districts);
            let x0 = bx.iter().map(|b| b[0]).fold(f32::MAX, f32::min);
            let y0 = bx.iter().map(|b| b[1]).fold(f32::MAX, f32::min);
            let x1 = bx.iter().map(|b| b[2]).fold(f32::MIN, f32::max);
            let y1 = bx.iter().map(|b| b[3]).fold(f32::MIN, f32::max);
            let s = 16.0;
            let (cw, ch) = (((x1 - x0) / s).ceil() as usize, ((y1 - y0) / s).ceil() as usize);
            let mut rows = vec![vec!['.'; cw]; ch];
            for (i, b) in bx.iter().enumerate() {
                let ch_ = (b'a' + dd[i] as u8) as char;
                for y in ((b[1] - y0) / s) as usize..((b[3] - y0) / s).ceil() as usize {
                    for x in ((b[0] - x0) / s) as usize..((b[2] - x0) / s).ceil() as usize {
                        rows[y.min(ch - 1)][x.min(cw - 1)] = ch_;
                    }
                }
            }
            for row in rows {
                eprintln!("    {}", row.into_iter().collect::<String>());
            }
        }
        let (district, count) = district_index(kinds.len(), &districts);
        for d in 0..count as u32 {
            let own: Vec<usize> = (0..kinds.len()).filter(|&i| district[i] == d).collect();
            let mut c = [0.0_f32; 2];
            for &i in &own {
                let b = boxes(&kinds[i..=i], &tops[i..=i])[0];
                c[0] += centre(b)[0] / own.len() as f32;
                c[1] += centre(b)[1] / own.len() as f32;
            }
            eprintln!(
                "  district {d:2} {:<28} {:2} machines centroid ({:7.1},{:7.1})",
                plant.machines[own[0]].name,
                own.len(),
                c[0],
                c[1]
            );
        }
        let demo = crate::plant::from_scan_json(include_str!("../www/fixtures/demo-plant.json"))
            .expect("demo");
        let (k, e, d) = demo.layout_input();
        summary("demo", &k, &e, &d);
        for count in [200, 400] {
            let (k, e, d) = stress_input(count);
            summary(&format!("stress{count}"), &k, &e, &d);
        }
    }

    fn summary(name: &str, kinds: &[Kind], edges: &[Edge], districts: &[u32]) {
        let t0 = std::time::Instant::now();
        let tops = place(kinds, edges, districts);
        let t_place = t0.elapsed();
        let t1 = std::time::Instant::now();
        let laid = crate::layout::layout(kinds, edges, districts);
        let t_layout = t1.elapsed();
        let [x0, y0, x1, y1] = laid.bounds;
        let bx = boxes(kinds, &tops);
        let centres: Vec<[f32; 2]> = bx.iter().map(|&b| centre(b)).collect();
        let (district, count) = district_index(kinds.len(), districts);
        let mut intrusions = 0;
        let mut spread = 0.0_f32;
        let mut multi = 0;
        let mut strays = 0;
        for d in 0..count as u32 {
            let own: Vec<usize> = (0..kinds.len()).filter(|&i| district[i] == d).collect();
            if own.len() < 2 {
                continue;
            }
            multi += 1;
            let mut c = [0.0_f32; 2];
            let mut bb = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
            for &i in &own {
                c[0] += centres[i][0] / own.len() as f32;
                c[1] += centres[i][1] / own.len() as f32;
                bb = [
                    bb[0].min(bx[i][0]),
                    bb[1].min(bx[i][1]),
                    bb[2].max(bx[i][2]),
                    bb[3].max(bx[i][3]),
                ];
                let nearest = (0..kinds.len())
                    .filter(|&j| j != i)
                    .min_by(|&a, &b| gap(bx[i], bx[a]).total_cmp(&gap(bx[i], bx[b])));
                if nearest.is_some_and(|j| district[j] != d) {
                    strays += 1;
                }
            }
            spread += own.iter().map(|&i| manhattan(centres[i], c)).sum::<f32>() / own.len() as f32;
            intrusions += (0..kinds.len())
                .filter(|&j| district[j] != d)
                .filter(|&j| {
                    let p = centres[j];
                    p[0] > bb[0] && p[0] < bb[2] && p[1] > bb[1] && p[1] < bb[3]
                })
                .count();
        }
        let belt_len: f32 = laid
            .belts
            .iter()
            .map(|b| {
                b.path
                    .windows(2)
                    .map(|p| manhattan(p[0], p[1]))
                    .sum::<f32>()
            })
            .sum::<f32>()
            / laid.belts.len().max(1) as f32;
        let reach = tops
            .iter()
            .map(|t| (t[0] / GRID).abs().max((t[1] / GRID).abs()))
            .fold(0.0_f32, f32::max);
        let mid = [(x0 + x1) / 2.0, (y0 + y1) / 2.0];
        let offset = (mid[0].hypot(mid[1])) / ((x1 - x0).max(y1 - y0) / 2.0);
        let mut degree = vec![0_usize; kinds.len()];
        for e in edges {
            degree[e.from] += 1;
            degree[e.to] += 1;
        }
        let mut sorted = degree.clone();
        sorted.sort_unstable();
        let cut = sorted[sorted.len() * 9 / 10];
        let dist = |i: usize| (centres[i][0] - mid[0]).hypot(centres[i][1] - mid[1]);
        let core: Vec<usize> = (0..kinds.len()).filter(|&i| degree[i] >= cut.max(1)).collect();
        let core_ratio = if core.is_empty() {
            f32::NAN
        } else {
            (core.iter().map(|&i| dist(i)).sum::<f32>() / core.len() as f32)
                / ((0..kinds.len()).map(dist).sum::<f32>() / kinds.len() as f32)
        };
        eprintln!(
            "{name}: offset {offset:.2} core ratio {core_ratio:.2} ({} core) route {:?}",
            core.len(),
            t_layout.saturating_sub(t_place)
        );
        eprintln!(
            "{name}: n {} bounds {:?} w {} h {} aspect {:.3} belts {}/{} mean belt {:.0} district spread {:.0} intrusions {} strays {} reach {} of {} place {:?} layout {:?}",
            kinds.len(),
            laid.bounds,
            x1 - x0,
            y1 - y0,
            (x1 - x0).max(y1 - y0) / (x1 - x0).min(y1 - y0),
            laid.belts.len(),
            edges.len(),
            belt_len,
            spread / multi.max(1) as f32,
            intrusions,
            strays,
            reach,
            Grid::new(&kinds.iter().map(|&k| cells(k)).collect::<Vec<_>>()).side / 2,
            t_place,
            t_layout
        );
    }

    #[test]
    fn the_same_input_places_the_same_way() {
        let (kinds, edges, districts) = stress_input(150);
        assert_eq!(
            place(&kinds, &edges, &districts),
            place(&kinds, &edges, &districts)
        );
        assert_eq!(place(&kinds, &edges, &[]), place(&kinds, &edges, &[]));
    }
}
