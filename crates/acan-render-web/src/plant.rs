//! Plant input. A live or fixture plant arrives as the scan JSON that `GET /api/plant`
//! returns (the shape of acan-scan's `Plant`). A stress plant is generated here for the
//! benchmark. Both reduce to a kind per machine and the import edges between them.

use std::collections::HashMap;

use serde::Deserialize;

use crate::kind::{Kind, classify};
use crate::layout::Edge;

#[derive(Clone, Debug, PartialEq)]
pub struct Plant {
    /// The repository's folder name, shown in the shell.
    pub repo: String,
    pub machines: Vec<PlantMachine>,
    pub edges: Vec<Edge>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlantMachine {
    pub name: String,
    pub files: usize,
    pub kind: Kind,
    /// The repo-relative paths of the files the machine groups, in scan order.
    pub paths: Vec<String>,
}

impl Plant {
    /// The kinds and edges the layout takes, in machine order.
    pub fn layout_input(&self) -> (Vec<Kind>, Vec<Edge>) {
        (
            self.machines.iter().map(|m| m.kind).collect(),
            self.edges.clone(),
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlantError {
    #[error("plant JSON does not parse: {0}")]
    Json(#[from] serde_json::Error),
    #[error("belt refers to machine {0}, which the plant does not list")]
    UnknownMachine(u32),
}

#[derive(Deserialize)]
struct ScanPlant {
    root: String,
    machines: Vec<ScanMachine>,
    belts: Vec<ScanBelt>,
    files: Vec<ScanFile>,
}

#[derive(Deserialize)]
struct ScanMachine {
    id: u32,
    name: String,
    dir: String,
    files: Vec<u32>,
}

#[derive(Deserialize)]
struct ScanBelt {
    from: u32,
    to: u32,
    imports: u32,
}

#[derive(Deserialize)]
struct ScanFile {
    id: u32,
    path: String,
}

/// Parses the scan JSON of `GET /api/plant`. Fields the plant does not use are ignored.
pub fn from_scan_json(text: &str) -> Result<Plant, PlantError> {
    let scan: ScanPlant = serde_json::from_str(text)?;
    let paths: HashMap<u32, &str> = scan
        .files
        .iter()
        .map(|file| (file.id, file.path.as_str()))
        .collect();
    let index: HashMap<u32, usize> = scan
        .machines
        .iter()
        .enumerate()
        .map(|(i, machine)| (machine.id, i))
        .collect();
    let machines = scan
        .machines
        .iter()
        .map(|machine| {
            let paths: Vec<&str> = machine
                .files
                .iter()
                .filter_map(|id| paths.get(id).copied())
                .collect();
            PlantMachine {
                name: machine.name.clone(),
                files: machine.files.len(),
                kind: classify(&machine.name, &machine.dir, &paths),
                paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            }
        })
        .collect();
    let edges = scan
        .belts
        .iter()
        .map(|belt| {
            let lookup = |id: u32| {
                index
                    .get(&id)
                    .copied()
                    .ok_or(PlantError::UnknownMachine(id))
            };
            Ok(Edge {
                from: lookup(belt.from)?,
                to: lookup(belt.to)?,
                imports: belt.imports,
            })
        })
        .collect::<Result<Vec<_>, PlantError>>()?;
    Ok(Plant {
        repo: folder_name(&scan.root),
        machines,
        edges,
    })
}

/// Machines named at rest: the most files first, capped so the plant stays calm. Names of the
/// rest appear only while Alt is held.
pub fn rest_labels(machines: &[PlantMachine]) -> Vec<usize> {
    const REST_LABELS: usize = 20;
    let mut order: Vec<usize> = (0..machines.len()).collect();
    order.sort_by(|&a, &b| machines[b].files.cmp(&machines[a].files).then(a.cmp(&b)));
    order.truncate(REST_LABELS);
    order
}

fn folder_name(root: &str) -> String {
    root.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// A generated plant of `count` machines for the benchmark. Machines sit in about `sqrt(count)`
/// layers, and every belt runs from one layer to the next, so belts stay short.
pub fn stress(count: usize) -> Plant {
    const NAMES: [&str; 12] = [
        "server",
        "auth",
        "store",
        "middleware",
        "ratelimit",
        "util",
        "errors",
        "links",
        "dashboard",
        "config",
        "queue",
        "mailer",
    ];
    let mut rng = Lcg(0x5eed_0ac4_u64);
    let layers = ((count as f64).sqrt() as usize).max(2);
    let mut machines = Vec::with_capacity(count);
    let mut layer_of = Vec::with_capacity(count);
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); layers];
    for i in 0..count {
        // Layers take turns, so every column holds the same number of machines and the
        // footprint comes out a clean rectangle.
        let layer = i % layers;
        layer_of.push(layer);
        members[layer].push(i);
        let base = NAMES[i % NAMES.len()];
        let files = 1 + rng.below(12);
        let paths: Vec<String> = (0..files)
            .map(|f| format!("src/{base}-{i}/file{f}.ts"))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let name = format!("{base}-{i}");
        machines.push(PlantMachine {
            kind: classify(&name, "src", &refs),
            name,
            files,
            paths,
        });
    }
    // Every machine below the first layer gets one edge from the layer above it, and about half
    // of the machines start one edge into the next layer down. Each belt then spans exactly one
    // layer, so the layout adds no lane nodes and the footprint stays near square.
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
    Plant {
        repo: "stress".to_string(),
        machines,
        edges,
    }
}

/// A small deterministic generator. The benchmark plant is the same on every run.
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

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../www/fixtures/demo-plant.json");

    const LIVE: &str = r#"{
        "root": "acan-demo",
        "revision": "0123456789abcdef",
        "machines": [
            {"id": 0, "name": "src", "dir": "src", "files": [0, 1], "lang_mix": {"typescript": 2}},
            {"id": 1, "name": "test", "dir": "test", "files": [2], "lang_mix": {"typescript": 1}}
        ],
        "belts": [{"from": 1, "to": 0, "imports": 3}],
        "files": [
            {"id": 0, "path": "src/auth.ts", "lang": "typescript"},
            {"id": 1, "path": "src/server.ts", "lang": "typescript"},
            {"id": 2, "path": "test/login.test.ts", "lang": "typescript"}
        ]
    }"#;

    fn kinds_by_name(plant: &Plant) -> Vec<(&str, Kind)> {
        plant
            .machines
            .iter()
            .map(|m| (m.name.as_str(), m.kind))
            .collect()
    }

    #[test]
    fn the_demo_fixture_maps_each_machine_to_its_kind() {
        let plant = from_scan_json(FIXTURE).expect("fixture parses");
        assert_eq!(plant.repo, "acan-demo");
        assert_eq!(plant.machines.len(), 9);
        assert_eq!(plant.edges.len(), 19);
        assert_eq!(
            kinds_by_name(&plant),
            vec![
                ("auth", Kind::Gate),
                ("dashboard", Kind::Assembler),
                ("errors", Kind::Assembler),
                ("links", Kind::Assembler),
                ("middleware", Kind::Splitter),
                ("ratelimit", Kind::Valve),
                ("server", Kind::Intake),
                ("store", Kind::Tank),
                ("test", Kind::Crate),
            ]
        );
    }

    #[test]
    fn the_live_two_machine_plant_maps_to_assembler_and_crate() {
        let plant = from_scan_json(LIVE).expect("live JSON parses");
        assert_eq!(plant.repo, "acan-demo");
        assert_eq!(
            kinds_by_name(&plant),
            vec![("src", Kind::Assembler), ("test", Kind::Crate)]
        );
        assert_eq!(
            plant.edges,
            vec![Edge {
                from: 1,
                to: 0,
                imports: 3
            }]
        );
    }

    #[test]
    fn a_belt_to_an_unknown_machine_is_an_error() {
        let text = LIVE.replace("\"to\": 0", "\"to\": 9");
        assert!(matches!(
            from_scan_json(&text),
            Err(PlantError::UnknownMachine(9))
        ));
    }

    #[test]
    fn the_stress_plant_is_deterministic_and_uses_several_kinds() {
        let a = stress(500);
        assert_eq!(a, stress(500));
        assert_eq!(a.machines.len(), 500);
        assert!(!a.edges.is_empty());
        let kinds: std::collections::BTreeSet<Kind> = a.machines.iter().map(|m| m.kind).collect();
        assert!(kinds.len() >= 4, "stress plant covers only {kinds:?}");
    }

    #[test]
    fn the_stress_footprint_stays_within_two_and_a_half_to_one() {
        for count in [100_usize, 500, 2000] {
            let plant = stress(count);
            let (kinds, edges) = plant.layout_input();
            let laid = crate::layout::layout(&kinds, &edges);
            let [x0, y0, x1, y1] = laid.bounds;
            let (w, d) = (x1 - x0, y1 - y0);
            let aspect = w.max(d) / w.min(d);
            assert!(
                aspect <= 2.5,
                "{count} machines: footprint {w} by {d}, aspect {aspect}"
            );
        }
    }

    #[test]
    fn at_rest_only_the_twenty_largest_machines_are_named() {
        let machine = |files: usize| PlantMachine {
            name: format!("m{files}"),
            files,
            kind: Kind::Assembler,
            paths: Vec::new(),
        };
        let machines: Vec<PlantMachine> = (0..25).map(|i| machine(i % 7)).collect();
        let named = rest_labels(&machines);
        assert_eq!(named.len(), 20);
        let fewest_named = named.iter().map(|&i| machines[i].files).min();
        assert!(
            machines
                .iter()
                .enumerate()
                .all(|(i, m)| { named.contains(&i) || m.files <= fewest_named.unwrap_or(0) })
        );
        assert_eq!(rest_labels(&machines[..5]).len(), 5);
    }

    #[test]
    fn folder_names_split_on_both_separators() {
        assert_eq!(folder_name("work\\acan-demo"), "acan-demo");
        assert_eq!(folder_name("/home/me/repo/"), "repo");
        assert_eq!(folder_name(""), "");
    }
}
