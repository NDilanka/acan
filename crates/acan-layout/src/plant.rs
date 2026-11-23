//! Plant names: the districts a plant's machines sit in, and the short names their signs and
//! labels show. A plant is one repository's machines, each with its plan size and the paths of
//! the files it groups, and the imports between them. Reading a plant from scan JSON is not
//! done here; this module needs only the plant itself.

use crate::layout::Edge;

/// One repository's machines and the imports between them, ready for layout.
#[derive(Clone, Debug, PartialEq)]
pub struct Plant {
    /// The repository's folder name, shown in the shell.
    pub repo: String,
    /// The machines. An [`Edge`] indexes into this list.
    pub machines: Vec<PlantMachine>,
    /// The imports between machines, by index into `machines`.
    pub edges: Vec<Edge>,
}

/// A machine on a plant: a group of files, drawn at its plan size.
#[derive(Clone, Debug, PartialEq)]
pub struct PlantMachine {
    /// The machine name, as the scan gives it.
    pub name: String,
    /// How many files the machine groups.
    pub files: usize,
    /// The plan size, `[width, depth]`.
    pub size: [f32; 2],
    /// The repo-relative paths of the files the machine groups, in scan order.
    pub paths: Vec<String>,
}

/// How many machines are named at rest: the largest by file count, so a big plant stays calm.
const REST_LABELS: usize = 20;

impl Plant {
    /// The plan sizes, edges and districts that [`crate::layout::layout`] takes, in machine order.
    #[must_use]
    pub fn layout_input(&self) -> (Vec<[f32; 2]>, Vec<Edge>, Vec<u32>) {
        (
            self.machines.iter().map(|m| m.size).collect(),
            self.edges.clone(),
            self.districts(),
        )
    }

    /// The district of each machine, as an index into [`Plant::district_paths`]. A machine's
    /// district is the first two folders of its first file, so `crates/acan-server` or
    /// `src/auth`. A machine with no file takes the folders of its name.
    #[must_use]
    pub fn districts(&self) -> Vec<u32> {
        let sorted = self.district_paths();
        self.machine_district_names()
            .iter()
            .map(|name| sorted.binary_search(name).unwrap_or(0) as u32)
            .collect()
    }

    /// The folder of each district, in the order [`Plant::districts`] numbers them.
    #[must_use]
    pub fn district_paths(&self) -> Vec<String> {
        let mut sorted = self.machine_district_names();
        sorted.sort_unstable();
        sorted.dedup();
        sorted
    }

    /// The name each district's sign shows, in the order [`Plant::districts`] numbers them: the
    /// last folder of its path, such as `acan-server` for `crates/acan-server`, with as many
    /// folders before it as tell it from the other districts.
    #[must_use]
    pub fn district_labels(&self) -> Vec<String> {
        short_names(&self.district_paths())
    }

    /// The short name each machine's label shows, in machine order. A machine is named within
    /// its district: `crates/acan-server/src` is `src`, and `crates/acan-server/src/vision` is
    /// `vision`, with as many folders before the last as tell it from its district mates. A
    /// machine that is its district's folder itself takes the district's sign name. A name
    /// outside its district's folder, as a demo plant's `auth`, is shortened the same way. The
    /// full name stays the machine's name, for its tooltip and its detail.
    #[must_use]
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

    /// The district name of each machine, before the districts are sorted and numbered.
    fn machine_district_names(&self) -> Vec<String> {
        self.machines.iter().map(district_name).collect()
    }
}

/// For each path, its last folder, or as many of its last folders as no other path ends with.
/// A path that is a whole tail of another keeps all of itself. Empty paths stay empty.
#[must_use]
pub fn short_names(paths: &[String]) -> Vec<String> {
    let parts: Vec<Vec<&str>> = paths
        .iter()
        .map(|path| path.split('/').filter(|part| !part.is_empty()).collect())
        .collect();
    parts
        .iter()
        .enumerate()
        .map(|(i, own)| {
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

/// The last `k` parts of `parts`, or all of them when there are fewer.
fn tail<'p, 'a>(parts: &'p [&'a str], k: usize) -> &'p [&'a str] {
    &parts[parts.len().saturating_sub(k)..]
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

/// The machines named at rest, as indexes: the most files first, at most [`REST_LABELS`] of
/// them. Machines with the same count keep their plant order.
#[must_use]
pub fn rest_labels(machines: &[PlantMachine]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..machines.len()).collect();
    order.sort_by(|&a, &b| machines[b].files.cmp(&machines[a].files).then(a.cmp(&b)));
    order.truncate(REST_LABELS);
    order
}

/// The folder name a repository root ends in. Both separators split it, so `work\acan-demo` and
/// `/home/me/repo/` both name their folder. An empty root gives an empty name.
#[must_use]
pub fn folder_name(root: &str) -> String {
    root.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdata::CRATE;

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
                    size: CRATE,
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
    fn the_layout_input_is_in_machine_order() {
        let mut plant = named(&["crates/acan-scan", "crates/acan-server", "tla/views"]);
        plant.edges = vec![Edge {
            from: 1,
            to: 0,
            imports: 2,
        }];
        let (sizes, edges, districts) = plant.layout_input();
        assert_eq!(sizes, vec![CRATE; 3]);
        assert_eq!(edges, plant.edges);
        // The districts sort as crates/acan-scan, crates/acan-server, tla/views.
        assert_eq!(districts, vec![0, 1, 2]);
    }

    #[test]
    fn at_rest_only_the_twenty_largest_machines_are_named() {
        let machine = |files: usize| PlantMachine {
            name: format!("m{files}"),
            files,
            size: CRATE,
            paths: Vec::new(),
        };
        let machines: Vec<PlantMachine> = (0..25).map(|i| machine(i % 7)).collect();
        let shown = rest_labels(&machines);
        assert_eq!(shown.len(), 20);
        let fewest_shown = shown.iter().map(|&i| machines[i].files).min();
        assert!(
            machines
                .iter()
                .enumerate()
                .all(|(i, m)| shown.contains(&i) || m.files <= fewest_shown.unwrap_or(0))
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
