//! Test inputs for the layout: generated plants, a workspace of crates, and the demo plant. The generators copy the machines, sizes and imports of
//! acan-render-web's stress plant and of its workspace test, so the checks run on the same shapes
//! without the scan reader. The parity test in `tests/render_web_parity.rs` checks the copy.

use crate::layout::Edge;

/// Plan sizes `[width, depth]` of the machines the tests use, from acan-iso's footprint table.
pub const ANTENNA: [f32; 2] = [56.0, 56.0];
pub const ASSEMBLER: [f32; 2] = [64.0, 64.0];
pub const CONVEYOR: [f32; 2] = [96.0, 40.0];
pub const CRATE: [f32; 2] = [48.0, 48.0];
pub const GATE: [f32; 2] = [56.0, 72.0];
pub const INTAKE: [f32; 2] = [72.0, 88.0];
pub const SPLITTER: [f32; 2] = [80.0, 80.0];
pub const SWITCHBOARD: [f32; 2] = [64.0, 40.0];
pub const SWITCHBOX: [f32; 2] = [48.0, 32.0];
pub const TANK: [f32; 2] = [80.0, 80.0];
pub const TOOLRACK: [f32; 2] = [56.0, 32.0];
pub const VALVE: [f32; 2] = [40.0, 40.0];

/// Machine sizes, imports and districts: the three inputs of [`crate::layout::layout`].
pub type Input = (Vec<[f32; 2]>, Vec<Edge>, Vec<u32>);

/// The district number of each machine: the index of its district's name among the distinct
/// names in sorted order, the numbering that acan-render-web gives its plant districts.
fn district_numbers(names: &[String]) -> Vec<u32> {
    let mut sorted = names.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    names
        .iter()
        .map(|name| sorted.binary_search(name).unwrap_or(0) as u32)
        .collect()
}

/// A generated plant of `count` machines, the same plant as acan-render-web's `plant::stress`:
/// about `sqrt(count)` layers, each belt running from one layer to the next, and every machine
/// a district of its own.
pub fn generated(count: usize) -> Input {
    const NAMES: [(&str, [f32; 2]); 12] = [
        ("server", INTAKE),
        ("auth", GATE),
        ("store", TANK),
        ("middleware", SPLITTER),
        ("ratelimit", VALVE),
        ("util", TOOLRACK),
        ("errors", ASSEMBLER),
        ("links", ASSEMBLER),
        ("routes", SWITCHBOARD),
        ("config", SWITCHBOX),
        ("queue", CONVEYOR),
        ("client", ANTENNA),
    ];
    let mut rng = Lcg(0x5eed_0ac4_u64);
    let layers = ((count as f64).sqrt() as usize).max(2);
    let mut sizes = Vec::with_capacity(count);
    let mut names = Vec::with_capacity(count);
    let mut layer_of = Vec::with_capacity(count);
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); layers];
    for i in 0..count {
        let layer = i % layers;
        layer_of.push(layer);
        members[layer].push(i);
        let (base, size) = NAMES[i % NAMES.len()];
        // acan-render-web draws a file count here; the layout does not read it.
        rng.below(12);
        sizes.push(size);
        names.push(format!("src/{base}-{i}"));
    }
    let mut edges = Vec::new();
    for layer in 1..layers {
        for &to in &members[layer] {
            let pool = &members[layer - 1];
            if pool.is_empty() {
                continue;
            }
            let from = pool[rng.below(pool.len())];
            edges.push(Edge {
                from,
                to,
                imports: 1 + rng.below(3) as u32,
            });
        }
    }
    for (from, &layer) in layer_of.iter().enumerate() {
        if layer + 1 >= layers || rng.below(2) == 0 {
            continue;
        }
        let pool = &members[layer + 1];
        if pool.is_empty() {
            continue;
        }
        let to = pool[rng.below(pool.len())];
        edges.push(Edge {
            from,
            to,
            imports: 1 + rng.below(3) as u32,
        });
    }
    (sizes, edges, district_numbers(&names))
}

/// The generator of acan-render-web's stress plant, so the same count gives the same plant.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

/// A plant of `count` machines in districts of one to six, with few imports: about one machine in
/// five imports a district mate, and one district in four imports the one before it, the way a
/// repository of crates with tests and examples scans.
pub fn sparse_town(count: usize) -> Input {
    const SIZES: [usize; 9] = [3, 1, 5, 2, 6, 4, 1, 2, 3];
    const KINDS: [[f32; 2]; 6] = [ASSEMBLER, CRATE, INTAKE, CRATE, TANK, GATE];
    let mut districts = Vec::with_capacity(count);
    let mut starts = Vec::new();
    let mut d = 0;
    while districts.len() < count {
        starts.push(districts.len());
        let size = SIZES[d % SIZES.len()].min(count - districts.len());
        districts.extend(std::iter::repeat_n(d as u32, size));
        d += 1;
    }
    let sizes = (0..count)
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
    (sizes, edges, districts)
}

/// A workspace of twelve crates of one to eleven folders each. No crate imports another, and only
/// four have imports inside, the way a repository of mostly tests and examples scans.
pub fn workspace() -> Input {
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
    let mut sizes = Vec::new();
    let mut names = Vec::new();
    for (c, &folders) in FOLDERS.iter().enumerate() {
        for f in 0..folders {
            sizes.push(match (c, f) {
                (9, _) => INTAKE,
                (_, 0) => ASSEMBLER,
                _ => CRATE,
            });
            names.push(format!("crates/c{c}"));
        }
    }
    let edges = IMPORTS
        .iter()
        .map(|&(from, to, imports)| Edge { from, to, imports })
        .collect();
    (sizes, edges, district_numbers(&names))
}

/// The workspace's 46 folders in twelve crates, densely linked the way a workspace of crates that
/// use each other scans: each folder imports its own crate's first folder, and up to five other
/// crates' first folders, the low-numbered crates far more often, as a core crate is. About two
/// hundred imports.
pub fn dense_workspace() -> Input {
    let (sizes, _, districts) = workspace();
    let first: Vec<usize> = (0..12)
        .map(|d| districts.iter().position(|&x| x == d).expect("crate"))
        .collect();
    let mut links = Vec::new();
    let mut seed = 0x9e37_79b9_u32;
    for (i, &own) in districts.iter().enumerate() {
        let own = own as usize;
        if first[own] != i {
            links.push((i, first[own]));
        }
        for _ in 0..5 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // Squaring a uniform pick leans it toward crate 0.
            let u = (seed >> 8) as f32 / (1 << 24) as f32;
            let crate_ = ((u * u) * 12.0) as usize;
            if crate_ != own {
                links.push((i, first[crate_]));
            }
        }
    }
    links.sort_unstable();
    links.dedup();
    let edges = links
        .into_iter()
        .map(|(from, to)| Edge {
            from,
            to,
            imports: 1 + ((from * 7 + to) % 5) as u32,
        })
        .collect();
    (sizes, edges, districts)
}

/// The demo plant, from the committed fixture `tests/fixtures/demo-input.json`: nine machines,
/// eight of them in one district and one in another.
pub fn demo() -> Input {
    let value: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/demo-input.json"))
            .expect("the demo fixture is JSON");
    let sizes = value["sizes"]
        .as_array()
        .expect("sizes")
        .iter()
        .map(|size| {
            [
                size[0].as_f64().expect("width") as f32,
                size[1].as_f64().expect("depth") as f32,
            ]
        })
        .collect();
    let edges = value["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .map(|edge| Edge {
            from: edge["from"].as_u64().expect("from") as usize,
            to: edge["to"].as_u64().expect("to") as usize,
            imports: edge["imports"].as_u64().expect("imports") as u32,
        })
        .collect();
    let districts = value["districts"]
        .as_array()
        .expect("districts")
        .iter()
        .map(|d| d.as_u64().expect("district") as u32)
        .collect();
    (sizes, edges, districts)
}
