//! Plant layout on the ground plan. Machines are layered by import direction (longest path),
//! ordered inside each layer by the barycentre of their neighbours, and stacked on a grid.
//! A belt that spans several layers crosses each one in the channel between the two machines
//! its place in the row order falls between, so every belt runs through a gap between columns
//! or a channel between machines, never through a footprint. The
//! result depends only on the input, so the same plant always lays out the same way.

use std::collections::VecDeque;

use crate::kind::{Kind, PlanFootprint};

/// Positions snap to this grid, in plan units.
pub const GRID: f32 = 8.0;
/// Clear width between columns. Belts run through it.
pub const COLUMN_GAP: f32 = 72.0;
/// Clear height between items stacked in one column.
pub const ROW_GAP: f32 = 40.0;
/// Step between belts that share a gap or a channel: the widest belt plus a clear gap.
const BELT_STEP: f32 = 8.0;
/// Belts keep this far from the machines on either side of the gap or channel they run in.
const LANE_MARGIN: f32 = 8.0;
/// A column at least this full of the tallest column's height is spread to its full height,
/// so the plant's edges line up. A shorter column is centred.
const JUSTIFY_FILL: f32 = 0.8;
/// Barycentre sweeps. Each sweep moves down or up one layer at a time.
const SWEEPS: usize = 8;
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

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// One entry per input machine, in input order.
    pub machines: Vec<Placed>,
    /// One entry per routed edge, in input order. Edges that closed a cycle are not routed.
    pub belts: Vec<Belt>,
    /// `[min x, min y, max x, max y]` of all footprints.
    pub bounds: [f32; 4],
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
        }
    }
}

pub fn layout(kinds: &[Kind], edges: &[Edge]) -> Layout {
    let machines = kinds.len();
    let accepted = acyclic_edges(machines, edges);
    let layer = longest_path_layers(machines, edges, &accepted);
    let graph = Graph::build(&layer, edges, &accepted);
    let rows = order_rows(&graph);

    let heights: Vec<f32> = kinds.iter().map(|k| snap_up(k.footprint().1)).collect();
    let widths: Vec<f32> = rows
        .iter()
        .map(|row| {
            row.iter()
                .filter(|&&node| node < machines)
                .map(|&node| snap_up(kinds[node].footprint().0))
                .fold(0.0, f32::max)
        })
        .collect();
    let mut column_x = Vec::with_capacity(rows.len());
    let mut x = 0.0;
    for width in &widths {
        column_x.push(x);
        x += width + COLUMN_GAP;
    }

    let stacks: Vec<f32> = rows
        .iter()
        .map(|row| {
            let (count, items) = row
                .iter()
                .filter(|&&node| node < machines)
                .fold((0_usize, 0.0_f32), |(c, sum), &node| {
                    (c + 1, sum + heights[node])
                });
            items + ROW_GAP * count.saturating_sub(1) as f32
        })
        .collect();
    let tallest = stacks.iter().copied().fold(0.0, f32::max);

    let mut source_of = vec![usize::MAX; graph.layer.len()];
    let mut target_of = vec![usize::MAX; graph.layer.len()];
    for chain in &graph.chains {
        for &node in &chain[1..chain.len() - 1] {
            source_of[node] = chain[0];
            target_of[node] = chain[chain.len() - 1];
        }
    }

    // Machines first: lanes take no room in a column, so the machines settle on their own.
    let mut node_y = vec![0.0_f32; graph.layer.len()];
    let mut spans: Vec<Vec<(f32, f32)>> = Vec::with_capacity(rows.len());
    for (l, row) in rows.iter().enumerate() {
        let count = row.iter().filter(|&&node| node < machines).count();
        let spare = tallest - stacks[l];
        let justify = count > 1 && stacks[l] >= JUSTIFY_FILL * tallest;
        let (mut y, extra) = if justify {
            (0.0, spare / (count - 1) as f32)
        } else {
            (spare / 2.0, 0.0)
        };
        let mut column = Vec::with_capacity(count);
        for &node in row.iter().filter(|&&node| node < machines) {
            let top = snap(y);
            node_y[node] = top;
            column.push((top, top + heights[node]));
            y += heights[node] + ROW_GAP + extra;
        }
        spans.push(column);
    }
    let centre_y = |m: usize, node_y: &[f32]| node_y[m] + kinds[m].footprint().1 / 2.0;

    // A lane crosses its column in the channel, between two machines or past either end,
    // that lies nearest the straight run from where it comes from to its target. Lanes of one
    // source in one channel merge, and the rest keep a belt's step apart.
    for (l, row) in rows.iter().enumerate() {
        let column = &spans[l];
        let channels: Vec<(f32, f32)> = (0..=column.len())
            .map(|c| {
                let lo = c
                    .checked_sub(1)
                    .map_or(f32::NEG_INFINITY, |p| column[p].1 + LANE_MARGIN);
                let hi = column.get(c).map_or(f32::INFINITY, |s| s.0 - LANE_MARGIN);
                (lo, hi)
            })
            .collect();
        let mut wanted: Vec<LaneWish> = Vec::new();
        for &node in row.iter().filter(|&&node| node >= machines) {
            let pred = graph.up[node][0];
            let from = if pred < machines {
                centre_y(pred, &node_y)
            } else {
                node_y[pred]
            };
            let goal = centre_y(target_of[node], &node_y);
            let (channel, y) = channels
                .iter()
                .map(|&(lo, hi)| from.clamp(lo, hi.max(lo)))
                .enumerate()
                .min_by(|a, b| {
                    let cost = |y: f32| ((y - from).abs() + (y - goal).abs(), (y - from).abs());
                    let (ca, cb) = (cost(a.1), cost(b.1));
                    ca.0.total_cmp(&cb.0).then(ca.1.total_cmp(&cb.1))
                })
                .unwrap_or((0, from));
            wanted.push(LaneWish {
                channel,
                source: source_of[node],
                y,
                node,
            });
        }
        wanted.sort_by_key(|w| (w.channel, w.source, w.node));
        for channel in wanted.chunk_by(|a, b| a.channel == b.channel) {
            let mut groups: Vec<(f32, &[LaneWish])> = channel
                .chunk_by(|a, b| a.source == b.source)
                .map(|g| (g.iter().map(|w| w.y).sum::<f32>() / g.len() as f32, g))
                .collect();
            groups.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1[0].source.cmp(&b.1[0].source)));
            let (lo, hi) = channels[channel[0].channel];
            let desired: Vec<f32> = groups.iter().map(|g| g.0).collect();
            for (y, (_, members)) in pack(&desired, lo, hi, BELT_STEP).into_iter().zip(&groups) {
                for member in *members {
                    node_y[member.node] = y;
                }
            }
        }
    }
    let mut placed = Vec::with_capacity(machines);
    for (node, &kind) in kinds.iter().enumerate() {
        let l = graph.layer[node];
        let (w, _) = kind.footprint();
        placed.push(Placed {
            x: column_x[l] + snap((widths[l] - w) / 2.0),
            y: node_y[node],
            kind,
        });
    }
    let exit_y = |m: usize| placed[m].y + placed[m].kind.footprint().1 / 2.0;

    // Belts enter a target in the order they arrive from top to bottom, so they never cross
    // in the last gap, and as level with where they arrive as the plinth allows.
    let arrival = |chain: &Vec<usize>| match chain.len() {
        0..=2 => exit_y(chain[0]),
        n => node_y[chain[n - 2]],
    };
    let mut incoming: Vec<Vec<usize>> = vec![Vec::new(); machines];
    for (k, chain) in graph.chains.iter().enumerate() {
        incoming[chain[chain.len() - 1]].push(k);
    }
    let mut entry_y = vec![0.0_f32; graph.chains.len()];
    for (to, ks) in incoming.iter_mut().enumerate() {
        ks.sort_by(|&a, &b| {
            arrival(&graph.chains[a])
                .total_cmp(&arrival(&graph.chains[b]))
                .then(a.cmp(&b))
        });
        let top = placed[to].y + LANE_MARGIN;
        let bottom = placed[to].y + placed[to].kind.footprint().1 - LANE_MARGIN;
        let desired: Vec<f32> = ks
            .iter()
            .map(|&k| arrival(&graph.chains[k]).clamp(top, bottom))
            .collect();
        for (&k, y) in ks.iter().zip(pack(&desired, top, bottom, BELT_STEP)) {
            entry_y[k] = y;
        }
    }

    // Every belt from one source turns at the same x in a gap, so a source's belts leave as one
    // trunk and part only where they turn toward their own targets. In a gap, trunks that run
    // down sit left of those that run up. Among those that run down, the one that starts lower
    // sits further left, and among those that run up, the one that starts higher does, which
    // keeps two staggered trunks from crossing twice.
    let y_of = |node: usize| {
        if node < machines {
            exit_y(node)
        } else {
            node_y[node]
        }
    };
    let mut gap_x: Vec<Vec<(usize, f32)>> = vec![Vec::new(); rows.len()];
    for (l, slots) in gap_x.iter_mut().enumerate() {
        // Per source: the sum of start ys, the sum of end ys and the count.
        let mut runs: Vec<(usize, f32, f32, f32)> = Vec::new();
        for (k, chain) in graph.chains.iter().enumerate() {
            let Some(i) = (1..chain.len()).find(|&i| graph.layer[chain[i]] == l) else {
                continue;
            };
            let start = y_of(chain[i - 1]);
            let end = if chain[i] < machines {
                entry_y[k]
            } else {
                node_y[chain[i]]
            };
            match runs.iter_mut().find(|r| r.0 == chain[0]) {
                Some(r) => {
                    r.1 += start;
                    r.2 += end;
                    r.3 += 1.0;
                }
                None => runs.push((chain[0], start, end, 1.0)),
            }
        }
        let mut keyed: Vec<(bool, f32, usize)> = runs
            .iter()
            .map(|&(source, starts, ends, n)| {
                let (start, end) = (starts / n, ends / n);
                let up = end < start;
                (up, if up { start } else { -start }, source)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2)));
        let centre = column_x[l] - COLUMN_GAP / 2.0;
        let n = keyed.len();
        *slots = keyed
            .iter()
            .enumerate()
            .map(|(j, &(_, _, s))| {
                (
                    s,
                    centre + lane_offset(j, n, COLUMN_GAP - 2.0 * LANE_MARGIN),
                )
            })
            .collect();
    }
    let mut belts = Vec::with_capacity(graph.chains.len());
    for (k, chain) in graph.chains.iter().enumerate() {
        let source = chain[0];
        let from = placed[source];
        let to = placed[chain[chain.len() - 1]];
        let mut y = exit_y(source);
        let mut path = vec![[from.x + from.kind.footprint().0, y]];
        for &node in &chain[1..] {
            let l = graph.layer[node];
            let jog_x = gap_x[l]
                .iter()
                .find(|&&(s, _)| s == source)
                .map_or(column_x[l] - COLUMN_GAP / 2.0, |&(_, x)| x);
            path.push([jog_x, y]);
            y = if node < machines {
                entry_y[k]
            } else {
                node_y[node]
            };
            path.push([jog_x, y]);
        }
        path.push([to.x, y]);
        path.dedup();
        let edge = edges[accepted[k]];
        belts.push(Belt {
            from: edge.from,
            to: edge.to,
            imports: edge.imports,
            path,
        });
    }
    let bounds = placed
        .iter()
        .map(Placed::footprint)
        .fold(None, |acc: Option<[f32; 4]>, f| match acc {
            None => Some(f),
            Some(b) => Some([
                b[0].min(f[0]),
                b[1].min(f[1]),
                b[2].max(f[2]),
                b[3].max(f[3]),
            ]),
        });
    Layout {
        machines: placed,
        belts,
        bounds: bounds.unwrap_or([0.0; 4]),
    }
}

/// Where one lane would like to cross its column: the channel, the machine its belt leaves,
/// the y nearest its straight run, and its node.
struct LaneWish {
    channel: usize,
    source: usize,
    y: f32,
    node: usize,
}

/// Indices of the edges that keep the import graph acyclic, in input order. A self import, an
/// edge to a machine that does not exist, and an edge that closes a cycle are dropped.
fn acyclic_edges(machines: usize, edges: &[Edge]) -> Vec<usize> {
    let mut out = vec![Vec::new(); machines];
    let mut accepted = Vec::new();
    for (k, edge) in edges.iter().enumerate() {
        if edge.from >= machines
            || edge.to >= machines
            || edge.from == edge.to
            || reaches(&out, edge.to, edge.from)
        {
            continue;
        }
        out[edge.from].push(edge.to);
        accepted.push(k);
    }
    accepted
}

fn reaches(out: &[Vec<usize>], from: usize, to: usize) -> bool {
    let mut seen = vec![false; out.len()];
    let mut stack = vec![from];
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if std::mem::replace(&mut seen[node], true) {
            continue;
        }
        stack.extend(out[node].iter().copied());
    }
    false
}

/// Layer of each machine: the longest path from a source, so every edge points down a layer.
fn longest_path_layers(machines: usize, edges: &[Edge], accepted: &[usize]) -> Vec<usize> {
    let mut out = vec![Vec::new(); machines];
    let mut indegree = vec![0_usize; machines];
    for &k in accepted {
        let edge = edges[k];
        out[edge.from].push(edge.to);
        indegree[edge.to] += 1;
    }
    let mut layer = vec![0_usize; machines];
    let mut queue: VecDeque<usize> = (0..machines).filter(|&m| indegree[m] == 0).collect();
    while let Some(u) = queue.pop_front() {
        for &v in &out[u] {
            layer[v] = layer[v].max(layer[u] + 1);
            indegree[v] -= 1;
            if indegree[v] == 0 {
                queue.push_back(v);
            }
        }
    }
    layer
}

/// The layered graph: machines are nodes `0..machines`, and every layer a long edge crosses
/// adds one dummy node after them.
struct Graph {
    layer: Vec<usize>,
    up: Vec<Vec<usize>>,
    down: Vec<Vec<usize>>,
    /// Per accepted edge, its nodes from source to target.
    chains: Vec<Vec<usize>>,
}

impl Graph {
    fn build(machine_layer: &[usize], edges: &[Edge], accepted: &[usize]) -> Self {
        let mut layer = machine_layer.to_vec();
        let mut chains = Vec::with_capacity(accepted.len());
        for &k in accepted {
            let edge = edges[k];
            let mut chain = vec![edge.from];
            for l in machine_layer[edge.from] + 1..machine_layer[edge.to] {
                chain.push(layer.len());
                layer.push(l);
            }
            chain.push(edge.to);
            chains.push(chain);
        }
        let mut up = vec![Vec::new(); layer.len()];
        let mut down = vec![Vec::new(); layer.len()];
        for chain in &chains {
            for pair in chain.windows(2) {
                down[pair[0]].push(pair[1]);
                up[pair[1]].push(pair[0]);
            }
        }
        Self {
            layer,
            up,
            down,
            chains,
        }
    }
}

/// Nodes of each layer in drawing order, from the barycentre sweeps.
fn order_rows(graph: &Graph) -> Vec<Vec<usize>> {
    let layers = graph.layer.iter().copied().max().map_or(0, |max| max + 1);
    let mut rows = vec![Vec::new(); layers];
    for (node, &l) in graph.layer.iter().enumerate() {
        rows[l].push(node);
    }
    let mut pos = vec![0_usize; graph.layer.len()];
    for row in &rows {
        set_positions(row, &mut pos);
    }
    for sweep in 0..SWEEPS {
        let downward = sweep % 2 == 0;
        let order: Vec<usize> = if downward {
            (1..layers).collect()
        } else {
            (0..layers.saturating_sub(1)).rev().collect()
        };
        let neighbours = if downward { &graph.up } else { &graph.down };
        for l in order {
            let mut keyed: Vec<(f64, usize, usize)> = rows[l]
                .iter()
                .enumerate()
                .map(|(i, &node)| {
                    let adjacent = &neighbours[node];
                    let key = if adjacent.is_empty() {
                        i as f64
                    } else {
                        let sum: f64 = adjacent.iter().map(|&a| pos[a] as f64).sum();
                        sum / adjacent.len() as f64
                    };
                    (key, i, node)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            rows[l] = keyed.into_iter().map(|(_, _, node)| node).collect();
            set_positions(&rows[l], &mut pos);
        }
    }
    rows
}

fn set_positions(row: &[usize], pos: &mut [usize]) {
    for (i, &node) in row.iter().enumerate() {
        pos[node] = i;
    }
}

/// Positions near `desired`, which is sorted, kept at least `step` apart inside `[lo, hi]`.
/// The step shrinks when the range is too short for all of them.
fn pack(desired: &[f32], lo: f32, hi: f32, step: f32) -> Vec<f32> {
    let n = desired.len();
    let span = (hi - lo).max(0.0);
    let step = if n > 1 && span.is_finite() {
        step.min(span / (n - 1) as f32)
    } else {
        step
    };
    let mut out: Vec<f32> = Vec::with_capacity(n);
    for &d in desired {
        let floor = out.last().map_or(lo, |&p| p + step);
        out.push(d.max(floor));
    }
    let mut ceiling = hi;
    for p in out.iter_mut().rev() {
        *p = p.min(ceiling);
        ceiling = *p - step;
    }
    out
}

/// Offset from a gap's centre line of belt `index` of `count`. Belts sit `BELT_STEP` apart,
/// closer when `room` is too narrow for that.
fn lane_offset(index: usize, count: usize, room: f32) -> f32 {
    if count <= 1 {
        return 0.0;
    }
    let step = BELT_STEP.min(room.max(0.0) / (count - 1) as f32);
    (index as f32 - (count - 1) as f32 / 2.0) * step
}

fn snap(value: f32) -> f32 {
    (value / GRID).round() * GRID
}

fn snap_up(value: f32) -> f32 {
    (value / GRID).ceil() * GRID
}

