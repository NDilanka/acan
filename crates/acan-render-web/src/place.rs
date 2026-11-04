//! Plan placement. Machines grow out of the centre of the plan as districts, on the floor grid.
//!
//! A district is one group of machines, such as one crate (see `Plant::districts`). Each district
//! is laid out first as a block on its own: its machines in rows and columns about a machine
//! plate apart, each machine in the slot nearest the district mates it imports or is imported by.
//! Then the blocks go onto the floor one at a time. The most connected district goes first, at
//! the centre. Each later one is the one most connected to the districts already placed, and
//! takes the free spot nearest the machines it is tied to, drawn a little toward the centre; a
//! district with no ties takes the free spot nearest the centre. So the town fills a round plan
//! ring by ring, with the most connected districts in its middle and the loose ones round the
//! rim. Blocks of two districts keep a street apart.
//!
//! The plan is kept round on purpose. The default camera looks at it from 45 degrees round and
//! 30 degrees up, which already shows a round plan as a blob about twice as wide as it is tall on
//! screen. A plan stretched along one axis shows as a diagonal band instead. The result depends
//! only on the input, so the same plant always places the same way.

use crate::kind::{Kind, PlanFootprint};
use crate::layout::{Edge, GRID};

/// Clear cells between two machines of one district: about one machine plate, so a belt has room
/// to turn between them.
const DISTRICT_CELLS: i32 = 9;
/// Clear cells between machines of two districts: the street.
const STREET_CELLS: i32 = 13;
/// How much wider than deep a district's block of machines is laid out. Near square, so a big
/// district stays a block rather than a row.
const BLOCK_ASPECT: f32 = 1.2;
/// How hard the centre of the plan pulls a district, against the pull of the machines it is tied
/// to. Without it a chain of districts each placed beside the last walks off to one side.
const PULL: f32 = 0.5;

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

    let mut placer = Placer::new(sizes, district);
    for d in order {
        let block = Block::new(&members[d], &placer.sizes, &adj);
        let anchor = placer.anchor(&members[d], &adj);
        placer.put(&block, anchor);
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
/// each one with the most imports to the districts already placed. Ties go to the district with
/// more imports to districts not yet placed, then the bigger one, then the one that comes first,
/// so loose districts fill in biggest first and the smallest end up on the rim.
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
        let next = (0..count).filter(|&d| !placed[d]).max_by(|&a, &b| {
            linked[a]
                .total_cmp(&linked[b])
                .then(outside[a].total_cmp(&outside[b]))
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

/// The distance from `a` to `b`, squared.
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

/// One district laid out on its own: each machine's corner from the block's top-left, in cells.
struct Block {
    /// `(machine, x, y)` for every member.
    spots: Vec<(usize, i32, i32)>,
    /// Width and depth of the block in cells, from its top-left to its furthest machine edge.
    size: (i32, i32),
}

impl Block {
    /// Lays out `members` (core machines first) in rows and columns. The grid is as close to
    /// [`BLOCK_ASPECT`] as it can be without many empty slots. Each machine takes the free slot
    /// nearest the members placed before it that it imports or is imported by, else the one
    /// nearest the middle, so the core sits in the middle and imports stay short. Each column is
    /// as wide as its widest machine and each row as deep as its deepest, with about a machine
    /// plate between them.
    fn new(members: &[usize], sizes: &[(i32, i32)], adj: &[Vec<(usize, f32)>]) -> Self {
        let k = members.len();
        let wide = members.iter().map(|&i| sizes[i].0).max().unwrap_or(1) + DISTRICT_CELLS;
        let deep = members.iter().map(|&i| sizes[i].1).max().unwrap_or(1) + DISTRICT_CELLS;
        let (cols, rows) = shape(k, wide, deep);
        let mid = [cols as f32 / 2.0, rows as f32 / 2.0];
        let mut taken = vec![false; cols * rows];
        let mut at: Vec<Option<(usize, usize)>> = vec![None; sizes.len()];
        let mut chosen = Vec::with_capacity(k);
        for &i in members {
            let mut best: Option<(f32, f32, usize)> = None;
            for s in (0..cols * rows).filter(|&s| !taken[s]) {
                let (c, r) = (s % cols, s / cols);
                let p = [c as f32 + 0.5, r as f32 + 0.5];
                let ties: f32 = adj[i]
                    .iter()
                    .filter_map(|&(j, w)| {
                        let (jc, jr) = at[j]?;
                        Some(w * (c.abs_diff(jc) + r.abs_diff(jr)) as f32)
                    })
                    .sum();
                let key = (ties, distance2(p, mid), s);
                if best.is_none_or(|b| key < b) {
                    best = Some(key);
                }
            }
            let Some((.., s)) = best else { break };
            taken[s] = true;
            at[i] = Some((s % cols, s / cols));
            chosen.push((i, s % cols, s / cols));
        }

        let mut col_w = vec![0; cols];
        let mut row_d = vec![0; rows];
        for &(i, c, r) in &chosen {
            col_w[c] = col_w[c].max(sizes[i].0);
            row_d[r] = row_d[r].max(sizes[i].1);
        }
        // Empty columns and rows at the edge take no room; inner ones keep the block regular.
        let starts = |extent: &[i32]| -> Vec<i32> {
            let mut at = 0;
            extent
                .iter()
                .map(|&e| {
                    let here = at;
                    if e > 0 {
                        at += e + DISTRICT_CELLS;
                    }
                    here
                })
                .collect()
        };
        let (xs, ys) = (starts(&col_w), starts(&row_d));
        let spots: Vec<(usize, i32, i32)> = chosen
            .iter()
            .map(|&(i, c, r)| {
                let (w, d) = sizes[i];
                (i, xs[c] + (col_w[c] - w) / 2, ys[r] + (row_d[r] - d) / 2)
            })
            .collect();
        let size = spots.iter().fold((0, 0), |(bw, bd), &(i, x, y)| {
            (bw.max(x + sizes[i].0), bd.max(y + sizes[i].1))
        });
        Self { spots, size }
    }
}

/// Columns and rows for `k` slots of `wide` by `deep` cells: the grid whose shape is nearest
/// [`BLOCK_ASPECT`], with a cost for every slot left empty. Ties go to fewer columns.
fn shape(k: usize, wide: i32, deep: i32) -> (usize, usize) {
    let k = k.max(1);
    let mut best = (f32::INFINITY, 1, k);
    for cols in 1..=k {
        let rows = k.div_ceil(cols);
        if cols > 1 && (cols - 1) * rows >= k {
            continue;
        }
        let ratio = (cols as f32 * wide as f32) / (rows as f32 * deep as f32);
        let empty = (cols * rows - k) as f32 / k as f32;
        let cost = (ratio / BLOCK_ASPECT).ln().abs() + empty;
        if cost < best.0 {
            best = (cost, cols, rows);
        }
    }
    (best.1, best.2)
}

/// The state of a placement in progress: the floor, and for each machine its district, its
/// size, and its corner and middle once placed.
struct Placer {
    grid: Grid,
    district: Vec<u32>,
    sizes: Vec<(i32, i32)>,
    slot: Vec<(i32, i32)>,
    centre: Vec<[f32; 2]>,
    placed: Vec<bool>,
}

impl Placer {
    fn new(sizes: Vec<(i32, i32)>, district: Vec<u32>) -> Self {
        let n = sizes.len();
        Self {
            grid: Grid::new(&sizes),
            district,
            sizes,
            slot: vec![(0, 0); n],
            centre: vec![[0.0; 2]; n],
            placed: vec![false; n],
        }
    }

    /// The point a district grows from: the middle of the placed machines its members import or
    /// are imported by, weighted by the imports, or the centre of the plan when it has none.
    /// The centre is fixed, not the middle of what is placed so far, so a town of loose districts
    /// grows evenly round it instead of creeping off to one side.
    fn anchor(&self, members: &[usize], adj: &[Vec<(usize, f32)>]) -> [f32; 2] {
        let mut tied = Mean::default();
        for &i in members {
            for &(j, w) in &adj[i] {
                if self.placed[j] {
                    tied.add(self.centre[j], w);
                }
            }
        }
        tied.get().unwrap_or([0.0; 2])
    }

    /// Places `block` where its middle is nearest `anchor`, and records its machines.
    fn put(&mut self, block: &Block, anchor: [f32; 2]) {
        let Some(&(first, ..)) = block.spots.first() else {
            return;
        };
        let district = self.district[first];
        let (bx, by) = loop {
            if let Some(corner) = self.nearest(block, anchor, district) {
                break corner;
            }
            self.widen();
        };
        for &(i, x, y) in &block.spots {
            let (sw, sh) = self.sizes[i];
            let (cx, cy) = (bx + x, by + y);
            self.grid.mark(cx, cy, sw, sh, district);
            self.slot[i] = (cx, cy);
            self.centre[i] = middle(cx, cy, sw, sh);
            self.placed[i] = true;
        }
    }

    /// A floor twice as wide, with every placed machine marked on it again, for a block that
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

    /// The free top-left corner for `block` whose middle is nearest `target`, with the centre
    /// of the plan pulling too (see [`PULL`]), or `None` when the floor has no room for it. Of
    /// corners as near as each other, the one nearer the centre wins, then the one at the
    /// smaller angle round it, so ties never favour one side. The search goes outward in rings
    /// of tiles, so the result is the same every time.
    fn nearest(&self, block: &Block, target: [f32; 2], district: u32) -> Option<(i32, i32)> {
        // Distance to the target plus `PULL` times distance to the centre is, up to a constant,
        // the distance to this one point between them, so the search can aim straight at it.
        let aim = [target[0] / (1.0 + PULL), target[1] / (1.0 + PULL)];
        let (bw, bh) = block.size;
        let want_x = ((aim[0] - bw as f32 * GRID / 2.0) / GRID).round() as i32;
        let want_y = ((aim[1] - bh as f32 * GRID / 2.0) / GRID).round() as i32;
        let fits = |cx: i32, cy: i32| {
            block.spots.iter().all(|&(i, x, y)| {
                let (sw, sh) = self.sizes[i];
                self.grid.fits(cx + x, cy + y, sw, sh, district)
            })
        };
        // The search walks the cell under the top-left of the block's first machine, which must
        // be free, a tile at a time outward from where it would be at the aim, and skips tiles
        // with no free cell. A district is put once, so no cell is near its machines yet, and a
        // cell it may build on is a free one.
        let &(_, ox, oy) = block.spots.first()?;
        let tile0 = self.grid.tile_of(want_x + ox, want_y + oy);
        // A cell `r` tiles out on its ring puts the block's middle at least `r - 1` tiles from
        // the aim.
        let floor = (TILE as f32 * GRID).powi(2);
        let tiles = self.grid.tiles;
        let mut best: Option<(f32, f32, f32, i32, i32)> = None;
        for r in 0..=2 * tiles + tile0.0.abs().max(tile0.1.abs()) {
            if best.is_some_and(|(d2, ..)| (r as f32 - 1.0).max(0.0).powi(2) * floor > d2) {
                break;
            }
            for (tx, ty) in ring(tile0.0, tile0.1, r) {
                if !self.grid.has_free(tx, ty) {
                    continue;
                }
                let (x0, y0) = self.grid.tile_corner(tx, ty);
                // The nearest the block's middle can come to the aim from this tile.
                let lo = middle(x0 - ox, y0 - oy, bw, bh);
                let hi = middle(x0 - ox + TILE - 1, y0 - oy + TILE - 1, bw, bh);
                let near = [aim[0].clamp(lo[0], hi[0]), aim[1].clamp(lo[1], hi[1])];
                if best.is_some_and(|(d2, ..)| distance2(near, aim) > d2) {
                    continue;
                }
                for y in y0..y0 + TILE {
                    for x in x0..x0 + TILE {
                        if !self.grid.fits(x, y, 1, 1, district) {
                            continue;
                        }
                        let (cx, cy) = (x - ox, y - oy);
                        let centre = middle(cx, cy, bw, bh);
                        let key = (
                            distance2(centre, aim),
                            distance2(centre, [0.0; 2]),
                            centre[1].atan2(centre[0]),
                            cy,
                            cx,
                        );
                        if best.is_some_and(|b| key >= b) || !fits(cx, cy) {
                            continue;
                        }
                        best = Some(key);
                    }
                }
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

/// The side of a tile of the floor, in cells. The floor counts the cells of each tile that are
/// not free, so a search can skip a tile with none free at once.
const TILE: i32 = 16;

/// The floor, one [`Cell`] per grid cell, centred on the origin, in tiles of [`TILE`] cells.
struct Grid {
    side: i32,
    cells: Vec<Cell>,
    /// Tiles along each side.
    tiles: i32,
    /// For each tile, how many of its cells are not free.
    used: Vec<u32>,
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

    /// An empty floor about `side` cells wide, rounded up to a whole number of tiles.
    fn with_side(side: i32) -> Self {
        let tiles = (side.max(1) + TILE - 1) / TILE;
        let side = tiles * TILE;
        Self {
            side,
            cells: vec![Cell::Free; (side as usize) * (side as usize)],
            tiles,
            used: vec![0; (tiles as usize) * (tiles as usize)],
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

    /// The tile holding cell `(x, y)`, which may be off the floor.
    fn tile_of(&self, x: i32, y: i32) -> (i32, i32) {
        let half = self.side / 2;
        ((x + half).div_euclid(TILE), (y + half).div_euclid(TILE))
    }

    /// The top-left cell of tile `(tx, ty)`.
    fn tile_corner(&self, tx: i32, ty: i32) -> (i32, i32) {
        let half = self.side / 2;
        (tx * TILE - half, ty * TILE - half)
    }

    /// Whether tile `(tx, ty)` is on the floor and has a free cell.
    fn has_free(&self, tx: i32, ty: i32) -> bool {
        let on = (0..self.tiles).contains(&tx) && (0..self.tiles).contains(&ty);
        on && self.used[(ty * self.tiles + tx) as usize] < (TILE * TILE) as u32
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
                if self.cells[i] == Cell::Free {
                    let (tx, ty) = self.tile_of(x, y);
                    self.used[(ty * self.tiles + tx) as usize] += 1;
                }
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

    /// A plant of `count` machines in districts of one to six, with few imports: about one
    /// machine in five imports a district mate, and one district in four imports the one before
    /// it, the way a repository of crates with tests and examples scans.
    fn sparse_town(count: usize) -> (Vec<Kind>, Vec<Edge>, Vec<u32>) {
        const SIZES: [usize; 9] = [3, 1, 5, 2, 6, 4, 1, 2, 3];
        const KINDS: [Kind; 6] = [
            Kind::Assembler,
            Kind::Crate,
            Kind::Intake,
            Kind::Crate,
            Kind::Tank,
            Kind::Gate,
        ];
        let mut districts = Vec::with_capacity(count);
        let mut starts = Vec::new();
        let mut d = 0;
        while districts.len() < count {
            starts.push(districts.len());
            let size = SIZES[d % SIZES.len()].min(count - districts.len());
            districts.extend(std::iter::repeat_n(d as u32, size));
            d += 1;
        }
        let kinds = (0..count)
            .map(|i| KINDS[(i * 7 + i / 3) % KINDS.len()])
            .collect();
        let mut edges = Vec::new();
        for i in 1..count {
            if i % 5 == 0 && districts[i] == districts[i - 1] {
                edges.push(Edge {
                    from: i,
                    to: i - 1,
                    imports: 1 + (i % 4) as u32,
                });
            }
        }
        for (d, &start) in starts.iter().enumerate().skip(1) {
            if d % 4 == 0 {
                edges.push(Edge {
                    from: start,
                    to: starts[d - 1],
                    imports: 2,
                });
            }
        }
        (kinds, edges, districts)
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
        let sizes = vec![size; 6];
        let mut placer = Placer::new(sizes.clone(), (0..6).collect());
        placer.grid = Grid::with_side(16);
        for i in 0..6 {
            placer.put(&Block::new(&[i], &sizes, &adjacency(6, &[])), [0.0; 2]);
        }
        assert!(
            placer.grid.side > 16,
            "the floor stayed {}",
            placer.grid.side
        );
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
    fn loose_districts_spread_as_a_round_blob_at_any_size() {
        for count in [20, 33, 46, 100] {
            let (kinds, edges, districts) = sparse_town(count);
            let boxes = boxes(&kinds, &place(&kinds, &edges, &districts));
            let x0 = boxes.iter().map(|b| b[0]).fold(f32::MAX, f32::min);
            let y0 = boxes.iter().map(|b| b[1]).fold(f32::MAX, f32::min);
            let x1 = boxes.iter().map(|b| b[2]).fold(f32::MIN, f32::max);
            let y1 = boxes.iter().map(|b| b[3]).fold(f32::MIN, f32::max);
            let aspect = (x1 - x0) / (y1 - y0);
            assert!(
                (0.7..=1.45).contains(&aspect),
                "{count} machines span {} by {}, aspect {aspect}",
                x1 - x0,
                y1 - y0
            );

            // The spread of the machines along their longest and shortest axes: a line has one
            // far longer than the other, a blob two of a size.
            let centres: Vec<[f32; 2]> = boxes.iter().map(|&b| centre(b)).collect();
            let n = centres.len() as f32;
            let mx = centres.iter().map(|c| c[0]).sum::<f32>() / n;
            let my = centres.iter().map(|c| c[1]).sum::<f32>() / n;
            let (mut sxx, mut syy, mut sxy) = (0.0_f32, 0.0_f32, 0.0_f32);
            for c in &centres {
                let (dx, dy) = (c[0] - mx, c[1] - my);
                sxx += dx * dx / n;
                syy += dy * dy / n;
                sxy += dx * dy / n;
            }
            let half = (sxx + syy) / 2.0;
            let root = (((sxx - syy) / 2.0).powi(2) + sxy * sxy).sqrt();
            let elongation = ((half + root) / (half - root)).sqrt();
            assert!(
                elongation < 2.0,
                "{count} machines spread {elongation} times longer one way than the other"
            );

            // The middle of the town stays near the centre of the plan, rather than creeping off.
            let reach = (x1 - x0).max(y1 - y0) / 2.0;
            let off = ((x0 + x1) / 2.0).hypot((y0 + y1) / 2.0);
            assert!(
                off < reach * 0.25,
                "{count} machines: town middle {off} from the centre, reach {reach}"
            );
        }
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
