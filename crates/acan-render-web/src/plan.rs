//! The plan view: each machine's rectangle as the renderer's layout puts it, and the lookups the
//! page's panels need to pin a place to a machine. The layout itself lives in `layout.rs`.

use std::collections::BTreeMap;

use acan_core::Locus;
use serde::Serialize;

use crate::plant::{Plant, PlantMachine};

/// One machine's footprint on the plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanMachine {
    /// The machine name.
    pub name: String,
    /// The top-left corner, in world units.
    pub min: [f32; 2],
    /// The width and height, in world units.
    pub size: [f32; 2],
}

impl PlanMachine {
    /// The middle of the footprint.
    #[must_use]
    pub fn center(&self) -> [f32; 2] {
        [
            self.min[0] + self.size[0] * 0.5,
            self.min[1] + self.size[1] * 0.5,
        ]
    }
}

/// The plan view of one plant: each machine's footprint, and the names that find it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    /// Machines, sorted by name.
    pub machines: Vec<PlanMachine>,
    by_name: BTreeMap<String, usize>,
    by_path: BTreeMap<String, usize>,
}

impl Plan {
    /// Indexes `plant`, placing each machine at the rectangle `footprint` gives its name as
    /// `[min x, min y, max x, max y]`. The same plant and footprints always give the same plan.
    #[must_use]
    pub fn new(plant: &Plant, footprint: impl Fn(&str) -> [f32; 4]) -> Self {
        let mut sorted: Vec<&PlantMachine> = plant.machines.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        let mut by_path = BTreeMap::new();
        for (i, machine) in sorted.iter().enumerate() {
            for path in &machine.paths {
                by_path.insert(path.clone(), i);
            }
        }

        let machines: Vec<PlanMachine> = sorted
            .iter()
            .map(|m| {
                let [x0, y0, x1, y1] = footprint(&m.name);
                PlanMachine {
                    name: m.name.clone(),
                    min: [x0, y0],
                    size: [x1 - x0, y1 - y0],
                }
            })
            .collect();
        let by_name = machines
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name.clone(), i))
            .collect();

        Self {
            machines,
            by_name,
            by_path,
        }
    }

    /// The index of the machine called `name`.
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }

    /// The index of the machine that groups the file at `path`.
    #[must_use]
    pub fn machine_of_path(&self, path: &str) -> Option<usize> {
        self.by_path.get(path).copied()
    }

    /// The machines a place on the plant touches. A belt touches both of its ends.
    #[must_use]
    pub fn machines_at(&self, locus: &Locus) -> Vec<usize> {
        match locus {
            Locus::Machine { machine } => self.index_of(machine.as_str()).into_iter().collect(),
            Locus::Belt { from, to } => [from.as_str(), to.as_str()]
                .into_iter()
                .filter_map(|name| self.index_of(name))
                .collect(),
            Locus::File { file } | Locus::Symbol { file, .. } => {
                self.machine_of_path(file.as_str()).into_iter().collect()
            }
        }
    }

    /// The point a place is drawn at: the middle of the machines it touches.
    #[must_use]
    pub fn point_at(&self, locus: &Locus) -> Option<[f32; 2]> {
        let centers: Vec<[f32; 2]> = self
            .machines_at(locus)
            .into_iter()
            .map(|i| self.machines[i].center())
            .collect();
        if centers.is_empty() {
            return None;
        }
        let n = centers.len() as f32;
        let sum = centers
            .iter()
            .fold([0.0, 0.0], |acc, c| [acc[0] + c[0], acc[1] + c[1]]);
        Some([sum[0] / n, sum[1] / n])
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use acan_core::{FilePath, MachineKey};

    use super::*;
    use crate::kind::Kind;

    fn machine(name: &str, paths: &[&str]) -> PlantMachine {
        PlantMachine {
            name: name.to_owned(),
            files: paths.len(),
            kind: Kind::Assembler,
            guessed: false,
            reason: String::new(),
            set_by_user: false,
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        }
    }

    fn sample() -> Plant {
        Plant {
            repo: "repo".to_owned(),
            machines: vec![
                machine("api", &["src/api/handler.ts"]),
                machine("ui", &["src/ui/greet.ts"]),
                machine("store", &["src/store.ts"]),
            ],
            edges: Vec::new(),
        }
    }

    /// Rectangles that differ per machine, so the centre tests tell machines apart.
    fn footprint(name: &str) -> [f32; 4] {
        match name {
            "api" => [0.0, 0.0, 96.0, 64.0],
            "store" => [200.0, 0.0, 296.0, 64.0],
            _ => [0.0, 200.0, 96.0, 264.0],
        }
    }

    fn plan() -> Plan {
        Plan::new(&sample(), footprint)
    }

    #[test]
    fn machines_are_sorted_and_indexed_by_name_and_path() {
        let plan = plan();
        let names: Vec<&str> = plan.machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["api", "store", "ui"]);
        assert_eq!(plan.index_of("ui"), Some(2));
        assert_eq!(plan.machine_of_path("src/api/handler.ts"), Some(0));
        assert_eq!(plan.machine_of_path("missing.ts"), None);
    }

    #[test]
    fn machines_take_their_rectangle_from_the_footprint() {
        let plan = plan();
        let store = &plan.machines[plan.index_of("store").expect("store is on the plan")];
        assert_eq!(store.min, [200.0, 0.0]);
        assert_eq!(store.size, [96.0, 64.0]);
        assert_eq!(store.center(), [248.0, 32.0]);
    }

    #[test]
    fn a_locus_maps_to_the_machines_it_names() {
        let plan = plan();
        let symbol = Locus::Symbol {
            file: FilePath::from_str("src/api/handler.ts").expect("valid path"),
            name: "handle".to_owned(),
        };
        assert_eq!(plan.machines_at(&symbol), [0]);
        let belt = Locus::Belt {
            from: MachineKey::from_str("ui").expect("valid key"),
            to: MachineKey::from_str("api").expect("valid key"),
        };
        assert_eq!(plan.machines_at(&belt), [2, 0]);
        let midpoint = plan.point_at(&belt).expect("both ends are on the plan");
        let (a, b) = (plan.machines[2].center(), plan.machines[0].center());
        assert_eq!(midpoint, [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]);
    }

    #[test]
    fn the_same_plant_gives_the_same_plan() {
        assert_eq!(plan(), plan());
    }

    #[test]
    fn footprints_serialize_as_name_min_and_size_in_plan_order() {
        let json = serde_json::to_value(&plan().machines).expect("the plan serializes");
        assert_eq!(
            json[1],
            serde_json::json!({ "name": "store", "min": [200.0, 0.0], "size": [96.0, 64.0] })
        );
        assert_eq!(json.as_array().map(Vec::len), Some(3));
    }
}
