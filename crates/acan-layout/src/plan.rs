//! The plan index: each machine's rectangle on the plan, and the lookups that pin a machine
//! name or a file path to its machine. The layout that places the rectangles lives in
//! [`crate::layout`].

use std::collections::BTreeMap;

use crate::layout::Layout;
use crate::plant::{Plant, PlantMachine};

/// One machine's footprint on the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanMachine {
    /// The machine name.
    pub name: String,
    /// The top-left corner, in plan units.
    pub min: [f32; 2],
    /// The width and depth, in plan units.
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

    /// Indexes `plant` at the rectangles `layout` placed it at: each machine's `min` is the
    /// corner its layout entry gives it, and its `size` is the plan size the plant gives it.
    ///
    /// # Panics
    ///
    /// Panics when `layout` holds fewer machines than `plant`: it is not the plant's own
    /// layout.
    #[must_use]
    pub fn from_layout(plant: &Plant, layout: &Layout) -> Self {
        let rects: BTreeMap<&str, [f32; 4]> = plant
            .machines
            .iter()
            .zip(&layout.machines)
            .map(|(machine, placed)| {
                (
                    machine.name.as_str(),
                    [
                        placed.x,
                        placed.y,
                        placed.x + machine.size[0],
                        placed.y + machine.size[1],
                    ],
                )
            })
            .collect();
        Self::new(plant, |name| {
            rects
                .get(name)
                .copied()
                .expect("the layout places every machine of its plant")
        })
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

    /// The middle of the centres of the machines `indices` names, or `None` when it is empty
    /// or holds an index off the plan.
    #[must_use]
    pub fn midpoint(&self, indices: &[usize]) -> Option<[f32; 2]> {
        if indices.is_empty() {
            return None;
        }
        let mut sum = [0.0, 0.0];
        for &i in indices {
            let center = self.machines.get(i)?.center();
            sum = [sum[0] + center[0], sum[1] + center[1]];
        }
        let n = indices.len() as f32;
        Some([sum[0] / n, sum[1] / n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::layout;
    use crate::testdata::CRATE;

    fn machine(name: &str, paths: &[&str]) -> PlantMachine {
        PlantMachine {
            name: name.to_owned(),
            files: paths.len(),
            size: CRATE,
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
    fn the_middle_of_a_belt_is_the_middle_of_its_ends() {
        let plan = plan();
        let (a, b) = (plan.machines[2].center(), plan.machines[0].center());
        assert_eq!(
            plan.midpoint(&[2, 0]),
            Some([(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5])
        );
        assert_eq!(plan.midpoint(&[0]), Some(plan.machines[0].center()));
        assert_eq!(plan.midpoint(&[]), None);
        assert_eq!(plan.midpoint(&[9]), None);
    }

    #[test]
    fn the_same_plant_gives_the_same_plan() {
        assert_eq!(plan(), plan());
    }

    #[test]
    fn from_layout_places_each_machine_where_the_layout_put_it() {
        let plant = sample();
        let (sizes, edges, districts) = plant.layout_input();
        let laid = layout(&sizes, &edges, &districts);
        let plan = Plan::from_layout(&plant, &laid);
        for (i, machine) in plant.machines.iter().enumerate() {
            let on_plan = &plan.machines[plan.index_of(&machine.name).expect("on the plan")];
            let placed = laid.machines[i];
            assert_eq!(on_plan.min, [placed.x, placed.y], "{}", machine.name);
            assert_eq!(on_plan.size, machine.size, "{}", machine.name);
        }
        assert_eq!(plan.machine_of_path("src/store.ts"), plan.index_of("store"));
        assert_eq!(plan, Plan::from_layout(&plant, &laid));
    }
}
