//! Plant input. A live or fixture plant arrives as the scan JSON that `GET /api/plant`
//! returns (the shape of acan-scan's `Plant`). A stress plant is generated here for the
//! benchmark. Both reduce to a kind per machine and the import edges between them.

use std::collections::HashMap;

use serde::Deserialize;

use crate::kind::Kind;
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
    /// True when a rule guessed the kind, so the plinth groove is broken.
    pub guessed: bool,
    /// The rule that picked the kind, for the tooltip.
    pub reason: String,
    /// True when the user set the kind in `.acan/kinds.json`.
    pub set_by_user: bool,
    /// The repo-relative paths of the files the machine groups, in scan order.
    pub paths: Vec<String>,
}

impl Plant {
    /// The kinds, edges and districts the layout takes, in machine order.
    pub fn layout_input(&self) -> (Vec<Kind>, Vec<Edge>, Vec<u32>) {
        (
            self.machines.iter().map(|m| m.kind).collect(),
            self.edges.clone(),
            self.districts(),
        )
    }

    /// The district of each machine, as an index into the sorted district names. A machine's
    /// district is the first two folders of its first file, so `crates/acan-server` or `src/auth`.
    /// A machine with no file takes the folders of its name.
    pub fn districts(&self) -> Vec<u32> {
        let names: Vec<String> = self.machines.iter().map(district_name).collect();
        let sorted = self.district_paths();
        names
            .iter()
            .map(|name| sorted.binary_search(name).unwrap_or(0) as u32)
            .collect()
    }

    /// The folder of each district, in the order [`Plant::districts`] numbers them.
    pub fn district_paths(&self) -> Vec<String> {
        let mut sorted: Vec<String> = self.machines.iter().map(district_name).collect();
        sorted.sort_unstable();
        sorted.dedup();
        sorted
    }

    /// The name each district's sign shows, in the order [`Plant::districts`] numbers them: the
    /// last folder of its path, such as `acan-server` for `crates/acan-server`, with as many
    /// folders before it as tell it from the other districts.
    pub fn district_labels(&self) -> Vec<String> {
        short_names(&self.district_paths())
    }

    /// The short name each machine's label shows, in machine order. A machine is named within
    /// its district: `crates/acan-server/src` is `src`, and `crates/acan-server/src/vision` is
    /// `vision`, with as many folders before the last as tell it from its district mates. A
    /// machine that is its district's folder itself takes the district's sign name. A name
    /// outside its district's folder, as a demo plant's `auth`, is shortened the same way. The
    /// full name stays the machine's name, for its tooltip and its detail.
    pub fn machine_labels(&self) -> Vec<String> {
        let paths = self.district_paths();
        let signs = short_names(&paths);
        let districts = self.districts();
        let relative: Vec<&str> = self
            .machines
            .iter()
            .zip(&districts)
            .map(|(machine, &d)| {
                let folder = paths[d as usize].as_str();
                let name = machine.name.trim_matches('/');
                if name == folder {
                    ""
                } else {
                    name.strip_prefix(folder)
                        .and_then(|rest| rest.strip_prefix('/'))
                        .filter(|_| !folder.is_empty())
                        .unwrap_or(name)
                }
            })
            .collect();
        let mut labels = vec![String::new(); self.machines.len()];
        for (d, sign) in signs.iter().enumerate() {
            let mates: Vec<usize> = (0..self.machines.len())
                .filter(|&i| districts[i] as usize == d)
                .collect();
            let names: Vec<String> = mates.iter().map(|&i| relative[i].to_owned()).collect();
            for (&i, short) in mates.iter().zip(short_names(&names)) {
                labels[i] = if short.is_empty() {
                    sign.clone()
                } else {
                    short
                };
            }
        }
        labels
    }
}

/// For each path, its last folder, or as many of its last folders as no other path ends with.
/// A path that is a whole tail of another keeps all of itself. Empty paths stay empty.
pub fn short_names(paths: &[String]) -> Vec<String> {
    let parts: Vec<Vec<&str>> = paths
        .iter()
        .map(|path| path.split('/').filter(|part| !part.is_empty()).collect())
        .collect();
    parts
        .iter()
        .enumerate()
        .map(|(i, own)| {
            fn tail<'p, 'a>(p: &'p [&'a str], k: usize) -> &'p [&'a str] {
                &p[p.len().saturating_sub(k)..]
            }
            let k = (1..=own.len())
                .find(|&k| {
                    let mine = tail(own, k);
                    parts
                        .iter()
                        .enumerate()
                        .all(|(j, other)| j == i || other.len() < k || tail(other, k) != mine)
                })
                .unwrap_or(own.len());
            tail(own, k).join("/")
        })
        .collect()
}

/// The first two folders of a machine's first file, or of its name when it has no file.
fn district_name(machine: &PlantMachine) -> String {
    let source = machine
        .paths
        .first()
        .map_or(machine.name.as_str(), String::as_str);
    let folder = source.rsplit_once('/').map_or("", |(folder, _)| folder);
    folder
        .split('/')
        .filter(|part| !part.is_empty())
        .take(2)
        .collect::<Vec<_>>()
        .join("/")
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
    /// The directory the scan grouped the machine under. The rules read it when no kind is given.
    #[serde(default)]
    dir: String,
    files: Vec<u32>,
    /// Missing in older plant JSON. Then the kind comes from the name, the directory and the file
    /// paths, with the scan's own rules (acan-iso's `classify`), so the drawing matches a fresh scan.
    /// A kind this build does not know reads as an assembler (see acan-iso's `Kind`).
    #[serde(default)]
    kind: Option<Kind>,
    #[serde(default)]
    kind_guessed: bool,
    #[serde(default)]
    kind_reason: String,
    #[serde(default)]
    kind_set_by_user: bool,
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
            let (kind, guessed, reason, set_by_user) = match machine.kind {
                Some(kind) => (
                    kind,
                    machine.kind_guessed,
                    machine.kind_reason.clone(),
                    machine.kind_set_by_user,
                ),
                None => {
                    let decision = acan_iso::classify(&machine.name, &machine.dir, &paths, &[]);
                    (decision.kind, decision.guessed, decision.reason, false)
                }
            };
            PlantMachine {
                name: machine.name.clone(),
                files: machine.files.len(),
                kind,
                guessed,
                reason,
                set_by_user,
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
    // Each name takes the kind the scan's token table gives it, so the stress plant shows the rules.
    const NAMES: [(&str, Kind); 12] = [
        ("server", Kind::Intake),
        ("auth", Kind::Gate),
        ("store", Kind::Tank),
        ("middleware", Kind::Splitter),
        ("ratelimit", Kind::Valve),
        ("util", Kind::Toolrack),
        ("errors", Kind::Assembler),
        ("links", Kind::Assembler),
        ("routes", Kind::Switchboard),
        ("config", Kind::Switchbox),
        ("queue", Kind::Conveyor),
        ("client", Kind::Antenna),
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
        let (base, kind) = NAMES[i % NAMES.len()];
        let files = 1 + rng.below(12);
        let paths: Vec<String> = (0..files)
            .map(|f| format!("src/{base}-{i}/file{f}.ts"))
            .collect();
        let guessed = kind != Kind::Assembler;
        let reason = if guessed {
            format!("name word \"{base}\"")
        } else {
            "no rule matched; default".to_owned()
        };
        machines.push(PlantMachine {
            name: format!("{base}-{i}"),
            files,
            kind,
            guessed,
            reason,
            set_by_user: false,
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
            {"id": 1, "name": "test", "dir": "test", "files": [2], "lang_mix": {"typescript": 1}, "kind": "crate", "kind_guessed": false, "kind_reason": "only tests"}
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
    fn a_kind_from_a_newer_scan_reads_as_an_assembler() {
        // An older renderer has no kind named "switchboard", so the same rule applies in reverse:
        // a name no kind has falls back to the assembler instead of failing the whole plant.
        let text = LIVE.replace("\"crate\"", "\"a_kind_from_the_future\"");
        let plant = from_scan_json(&text).expect("an unknown kind still parses");
        assert_eq!(
            kinds_by_name(&plant),
            vec![("src", Kind::Assembler), ("test", Kind::Assembler)]
        );
    }

    #[test]
    fn the_scan_gives_the_kind_its_guess_and_reason() {
        let plant = from_scan_json(FIXTURE).expect("fixture parses");
        let server = plant
            .machines
            .iter()
            .find(|m| m.name == "server")
            .expect("the server machine");
        assert_eq!(server.kind, Kind::Intake);
        assert!(server.guessed);
        assert_eq!(server.reason, "name word \"server\"");
        assert!(!server.set_by_user);
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
        // The footprint is the machines' placement alone, so the belts are not routed here.
        for count in [100_usize, 500, 2000] {
            let plant = stress(count);
            let (kinds, edges, districts) = plant.layout_input();
            let tops = crate::place::place(&kinds, &edges, &districts);
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for (kind, [x, y]) in kinds.iter().zip(tops) {
                let (w, d) = crate::kind::PlanFootprint::footprint(*kind);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + w), y1.max(y + d));
            }
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
            guessed: false,
            reason: String::new(),
            set_by_user: false,
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
    fn a_plant_without_kinds_takes_the_kinds_its_names_give() {
        // Older plant JSON has no kind keys. The kinds then come from the scan's rules, as a fresh scan gives them.
        let text = LIVE.replace(
            ", \"kind\": \"crate\", \"kind_guessed\": false, \"kind_reason\": \"only tests\"",
            "",
        );
        let text = text.replace("\"name\": \"src\"", "\"name\": \"auth\"");
        let plant = from_scan_json(&text).expect("a plant without kinds parses");
        assert_eq!(
            kinds_by_name(&plant),
            vec![("auth", Kind::Gate), ("test", Kind::Crate)]
        );
        assert!(plant.machines[0].guessed);
        assert_eq!(plant.machines[0].reason, "name word \"auth\"");
        assert_eq!(plant.machines[1].reason, "only tests");
        assert!(!plant.machines[1].set_by_user);
    }

    #[test]
    fn the_shipped_replay_plant_parses_into_nine_machines_with_their_kinds() {
        const REPLAY: &str = include_str!("../../../replay/plant.json");
        let plant = from_scan_json(REPLAY).expect("the shipped replay plant parses");
        assert_eq!(plant.machines.len(), 9);
        assert_eq!(plant.machines[0].name, "auth");
        assert_eq!(plant.machines[0].kind, Kind::Gate);
    }

    #[test]
    fn the_shipped_replay_plant_carries_the_kinds_the_rules_give() {
        // The recorded replay has a kind for every machine, and each is the scan's rule for its name,
        // directory and file paths. A changed rule or a new scan needs the bundle's kinds written again.
        const REPLAY: &str = include_str!("../../../replay/plant.json");
        let value: serde_json::Value =
            serde_json::from_str(REPLAY).expect("the replay plant parses");
        let files: HashMap<u64, &str> = value["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|file| {
                (
                    file["id"].as_u64().expect("file id"),
                    file["path"].as_str().expect("path"),
                )
            })
            .collect();
        let machines = value["machines"].as_array().expect("machines");
        assert_eq!(machines.len(), 9);
        for machine in machines {
            let name = machine["name"].as_str().expect("machine name");
            let dir = machine["dir"].as_str().expect("machine dir");
            let paths: Vec<&str> = machine["files"]
                .as_array()
                .expect("machine files")
                .iter()
                .filter_map(|id| files.get(&id.as_u64()?).copied())
                .collect();
            let want = serde_json::to_value(acan_iso::classify(name, dir, &paths, &[]).kind)
                .expect("a kind name");
            assert_eq!(machine["kind"], want, "{name}");
        }
    }

    /// A plant of machines named by their folders, each with one file in it, as a workspace of
    /// crates scans.
    fn named(names: &[&str]) -> Plant {
        Plant {
            repo: "workspace".to_owned(),
            machines: names
                .iter()
                .map(|name| PlantMachine {
                    name: (*name).to_owned(),
                    files: 1,
                    kind: Kind::Crate,
                    guessed: false,
                    reason: String::new(),
                    set_by_user: false,
                    paths: vec![format!("{name}/lib.rs")],
                })
                .collect(),
            edges: Vec::new(),
        }
    }

    #[test]
    fn short_names_keep_the_last_folder_and_as_many_before_it_as_tell_them_apart() {
        let paths = |list: &[&str]| list.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            short_names(&paths(&[
                "crates/acan-server",
                "crates/acan-scan",
                "tla/views"
            ])),
            ["acan-server", "acan-scan", "views"]
        );
        assert_eq!(
            short_names(&paths(&[
                "src",
                "tests/fixtures/rust-crate/src",
                "src/vision"
            ])),
            ["src", "rust-crate/src", "vision"]
        );
        assert_eq!(short_names(&paths(&["a/x", "b/x", ""])), ["a/x", "b/x", ""]);
    }

    #[test]
    fn a_machine_is_labelled_within_its_district() {
        let plant = named(&[
            "crates/acan-server",
            "crates/acan-server/src",
            "crates/acan-server/src/vision",
            "crates/acan-server/tests",
            "crates/acan-server/tests/support",
            "crates/acan-scan/src",
            "crates/acan-scan/tests",
            "crates/acan-scan/tests/fixtures/rust-crate/src",
            "crates/acan-scan/tests/fixtures/rust-crate/src/a",
            "crates/acan-witness/tests/support",
            "tla/views",
        ]);
        assert_eq!(
            plant.district_labels(),
            ["acan-scan", "acan-server", "acan-witness", "views"]
        );
        assert_eq!(
            plant.machine_labels(),
            [
                "acan-server",
                "src",
                "vision",
                "tests",
                "support",
                "src",
                "tests",
                "rust-crate/src",
                "a",
                "support",
                "views",
            ]
        );
        for (label, machine) in plant.machine_labels().iter().zip(&plant.machines) {
            assert!(
                !label.contains("crates/"),
                "{} keeps its whole path",
                machine.name
            );
        }
    }

    #[test]
    fn the_demo_and_live_plants_keep_their_machine_names_as_labels() {
        for text in [FIXTURE, LIVE] {
            let plant = from_scan_json(text).expect("plant parses");
            let names: Vec<String> = plant.machines.iter().map(|m| m.name.clone()).collect();
            assert_eq!(plant.machine_labels(), names);
        }
    }

    #[test]
    fn folder_names_split_on_both_separators() {
        assert_eq!(folder_name("work\\acan-demo"), "acan-demo");
        assert_eq!(folder_name("/home/me/repo/"), "repo");
        assert_eq!(folder_name(""), "");
    }
}
