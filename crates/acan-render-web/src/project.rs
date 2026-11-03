//! The marks of one project on a canvas of several. The project on screen keeps its marks in the
//! app's own fields; every other project keeps them here, so its drones, pins and flags move and
//! fold on their own. Each project's agents, hue slots and eases live in its own state, so the
//! same agent name in two projects never meets. The overlay helpers below are what the
//! one-project exports and their every-project variants share, so both place a mark the same way.

use crate::marks::{self, AgentSlots, Clock, Flag, Marks, Motion, Stand};
use crate::mirror::{Mirror, View};
use crate::plan::Plan;

/// One project's view, marks and eases, as the project on screen holds them in the app.
pub struct ProjectMarks {
    pub view: Option<View>,
    pub marks: Marks,
    pub motion: Motion,
    pub slots: AgentSlots,
    /// Whether the log moved since the marks were built.
    pub dirty: bool,
    /// Whether the eases were moving on the last frame.
    pub moving: bool,
}

impl ProjectMarks {
    pub fn new(motion: Motion) -> Self {
        Self {
            view: None,
            marks: Marks::default(),
            motion,
            slots: AgentSlots::default(),
            dirty: true,
            moving: false,
        }
    }

    /// Folds the log into the view and places its marks, as the app does for the project on
    /// screen. A `still` page shows no drift.
    pub fn rebuild(
        &mut self,
        mirror: &Mirror,
        plan: &Plan,
        stands: &[Stand],
        clock: Clock,
        still: bool,
    ) {
        self.dirty = false;
        let view = mirror.view_still(plan, still);
        self.slots.assign(&view);
        let marks = Marks::build_at(&view, plan, stands, &self.slots, clock);
        self.motion.sync(&marks);
        self.marks = marks;
        self.view = Some(view);
    }

    /// Moves the eases on by `dt_ms`. Returns whether the scene needs the marks again (the frame
    /// an ease ends on still uploads) and whether they still move.
    pub fn advance(&mut self, dt_ms: f32) -> (bool, bool) {
        let moving = self.motion.advance(dt_ms);
        let upload = moving || self.moving;
        self.moving = moving;
        (upload, moving)
    }
}

/// Each flag's anchor on screen as `[flag index, machine or -1, css x, css y]`.
pub fn flag_quads(
    flags: &[Flag],
    motion: &Motion,
    screen: impl Fn([f32; 3]) -> [f32; 2],
) -> Vec<[f32; 4]> {
    flags
        .iter()
        .enumerate()
        .map(|(index, flag)| {
            let at = motion.flag_at(flag);
            let [x, y] = marks::flag_anchor(flag, at, &screen);
            let machine = flag.machine.map_or(-1.0, |machine| machine as f32);
            [index as f32, machine, x, y]
        })
        .collect()
}

/// Each drone's disc on screen as `(agent, [css x, css y, css radius])`, in the order the marks
/// list the drones, leaving out `skip`'s.
pub fn drone_discs<'a>(
    marks: &'a Marks,
    motion: &Motion,
    skip: Option<&str>,
    screen: impl Fn([f32; 3]) -> [f32; 2],
) -> Vec<(&'a str, [f32; 3])> {
    marks
        .bodies
        .iter()
        .filter(|body| skip != Some(body.agent.as_str()))
        .map(|body| {
            let at = motion.body_at(&body.agent).unwrap_or(body.at);
            let ([x, y], radius) = marks::drone_disc(marks::bobbed(motion, body, at), &screen);
            (body.agent.as_str(), [x, y, radius])
        })
        .collect()
}

/// Each clash diamond's centre on screen as `[clash index, css x, css y]`.
pub fn pin_triples(
    marks: &Marks,
    motion: &Motion,
    screen: impl Fn([f32; 3]) -> [f32; 2],
) -> Vec<[f32; 3]> {
    marks
        .pins
        .iter()
        .map(|pin| {
            let foot = motion.pin_at(&pin.id).unwrap_or(pin.at);
            let [x, y] = marks::diamond_centre(foot, &screen);
            [pin.clash as f32, x, y]
        })
        .collect()
}

/// Each clash's badge centres on screen as `[clash index, left css x, right css x, css y]`.
pub fn badge_quads(
    marks: &Marks,
    motion: &Motion,
    screen: impl Fn([f32; 3]) -> [f32; 2],
) -> Vec<[f32; 4]> {
    marks
        .pins
        .iter()
        .map(|pin| {
            let foot = motion.pin_at(&pin.id).unwrap_or(pin.at);
            let [left, right] = marks::badge_centres(foot, &screen);
            [pin.clash as f32, left[0], right[0], left[1]]
        })
        .collect()
}

/// Each ghost on show as `[css x, css y, landed count]`, at its outline's corner lowest on screen.
pub fn ghost_triples(
    marks: &Marks,
    motion: &Motion,
    screen: impl Fn([f32; 3]) -> [f32; 2],
) -> Vec<[f32; 3]> {
    marks
        .ghosts
        .iter()
        .filter(|ghost| motion.ghost_alpha(ghost.machine).is_some())
        .map(|ghost| {
            let [x, y] = ghost.corners().map(&screen).into_iter().fold(
                [0.0, f32::NEG_INFINITY],
                |low, p| {
                    if p[1] > low[1] { p } else { low }
                },
            );
            [x, y, ghost.count as f32]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::str::FromStr;

    use acan_core::{
        Belt, CodeIndex, CodeRev, FilePath, IndexedFile, IndexedSymbol, MachineKey, MemberId,
        Origin, Stamped, Timestamp, WorldId, WorldRef,
    };

    use super::*;
    use crate::plant::from_scan_json;
    use crate::scenario::{self, Target};

    const PLANT: &str = r#"{
        "root": "/repo",
        "machines": [
            {"id": 1, "name": "src/api", "dir": "src/api", "files": [1]},
            {"id": 2, "name": "src/ui", "dir": "src/ui", "files": [2]}
        ],
        "belts": [{"from": 2, "to": 1, "imports": 1}],
        "files": [
            {"id": 1, "path": "src/api/auth.ts"},
            {"id": 2, "path": "src/ui/dashboard.ts"}
        ]
    }"#;

    /// The plant with its machines `dx` along the plan, as a project packed beside another.
    fn plan_at(dx: f32) -> Plan {
        let plant = from_scan_json(PLANT).expect("the plant parses");
        Plan::new(&plant, |name| {
            if name == "src/api" {
                [dx, 0.0, dx + 96.0, 86.0]
            } else {
                [dx + 200.0, 0.0, dx + 296.0, 86.0]
            }
        })
    }

    fn stands(plan: &Plan) -> Vec<Stand> {
        plan.machines
            .iter()
            .map(|machine| {
                let c = machine.center();
                Stand {
                    top: 10.0,
                    spots: vec![
                        [c[0] - 20.0, c[1]],
                        [c[0] + 20.0, c[1]],
                        [c[0], c[1] + 20.0],
                    ],
                }
            })
            .collect()
    }

    fn founder() -> MemberId {
        MemberId::from_str("owner").expect("a member id")
    }

    fn index() -> CodeIndex {
        let api = MachineKey::from_str("src/api").expect("a machine key");
        let ui = MachineKey::from_str("src/ui").expect("a machine key");
        let symbol = |name: &str| IndexedSymbol {
            name: name.to_owned(),
            bytes: 0..40,
            exported: true,
        };
        let files = BTreeMap::from([
            (
                FilePath::from_str("src/api/auth.ts").expect("a file path"),
                IndexedFile {
                    machine: api.clone(),
                    symbols: vec![symbol("login")],
                },
            ),
            (
                FilePath::from_str("src/ui/dashboard.ts").expect("a file path"),
                IndexedFile {
                    machine: ui.clone(),
                    symbols: vec![symbol("render")],
                },
            ),
        ]);
        CodeIndex {
            rev: CodeRev::from_str("demo-rev").expect("a code rev"),
            files,
            belts: BTreeSet::from([Belt { from: ui, to: api }]),
        }
    }

    /// The demo script: one order, two agents on src/api, one clash. Then, with `land`, both tasks
    /// build, pass and land.
    fn log(land: bool) -> Vec<Stamped> {
        let index = index();
        let world = WorldRef {
            id: WorldId::from_str("demo").expect("a world id"),
            repo: "demo".to_owned(),
        };
        let (mut origin, opened) = Origin::open(world, founder(), Timestamp::from_millis(1_000));
        let target = Target::first_symbol(&index, "Make login return a session")
            .expect("the index holds a symbol");
        let mut log: Vec<Stamped> = std::iter::once(opened)
            .chain(
                scenario::clash_script(
                    &mut origin,
                    &index,
                    &founder(),
                    &target,
                    Timestamp::from_millis(2_000),
                )
                .expect("the script runs"),
            )
            .collect();
        if land {
            log.extend(
                scenario::land_script(
                    &mut origin,
                    &index,
                    &founder(),
                    Timestamp::from_millis(4_000),
                )
                .expect("the land script runs"),
            );
        }
        log
    }

    fn built(dx: f32, land: bool) -> (ProjectMarks, Plan) {
        let plan = plan_at(dx);
        let mirror = Mirror::from_log(log(land)).expect("the log replays");
        let mut project = ProjectMarks::new(Motion::new(true));
        project.rebuild(&mirror, &plan, &stands(&plan), Clock::default(), false);
        (project, plan)
    }

    #[test]
    fn two_projects_with_the_same_agents_keep_their_own_slots_and_bodies() {
        let (a, _) = built(0.0, false);
        let (b, _) = built(1000.0, false);
        assert!(a.marks.bodies.len() >= 2, "the script has two agents");
        let names = |p: &ProjectMarks| -> Vec<String> {
            p.marks
                .bodies
                .iter()
                .map(|body| body.agent.clone())
                .collect()
        };
        assert_eq!(
            names(&a),
            names(&b),
            "both projects have agents of the same names"
        );
        for (body_a, body_b) in a.marks.bodies.iter().zip(&b.marks.bodies) {
            assert_eq!(a.slots.slot(&body_a.agent), b.slots.slot(&body_b.agent));
            let at_a = a
                .motion
                .body_at(&body_a.agent)
                .expect("a's drone is placed");
            let at_b = b
                .motion
                .body_at(&body_b.agent)
                .expect("b's drone is placed");
            assert!(
                (at_b[0] - at_a[0] - 1000.0).abs() < 1e-3,
                "each drone stands on its own plant: {at_a:?} and {at_b:?}"
            );
        }
    }

    #[test]
    fn a_fold_in_one_project_leaves_the_other_s_marks_as_they_were() {
        let (mut a, plan_a) = built(0.0, false);
        let (b, _) = built(1000.0, false);
        let before = b.marks.clone();
        let flags_before = b.motion.flags_now(&b.marks.flags);
        let a_before = a.marks.clone();
        let landed = Mirror::from_log(log(true)).expect("the log replays");
        a.dirty = true;
        a.rebuild(&landed, &plan_a, &stands(&plan_a), Clock::default(), false);
        assert_ne!(a.marks, a_before, "the fold changed a's marks");
        assert_eq!(b.marks, before, "b's marks did not move");
        assert_eq!(b.motion.flags_now(&b.marks.flags), flags_before);
        assert!(!a.dirty);
    }

    #[test]
    fn the_shared_helpers_place_flags_and_drones_as_the_one_project_exports_did() {
        let (p, _) = built(0.0, false);
        let screen = |q: [f32; 3]| [q[0] * 0.5 + q[2], q[1] * 0.25 - q[2]];
        let flags = p.motion.flags_now(&p.marks.flags);
        // The one-project `flag_layout` and `drone_layout` bodies before they shared the helpers.
        let flag_before: Vec<f32> = flags
            .iter()
            .enumerate()
            .flat_map(|(index, flag)| {
                let at = p.motion.flag_at(flag);
                let [x, y] = marks::flag_anchor(flag, at, screen);
                let machine = flag.machine.map_or(-1.0, |machine| machine as f32);
                [index as f32, machine, x, y]
            })
            .collect();
        let drone_before: Vec<f32> = p
            .marks
            .bodies
            .iter()
            .flat_map(|body| {
                let at = p.motion.body_at(&body.agent).unwrap_or(body.at);
                let ([x, y], radius) = marks::drone_disc(at, screen);
                [x, y, radius]
            })
            .collect();
        assert!(!flag_before.is_empty() && !drone_before.is_empty());
        let flag_now: Vec<f32> = flag_quads(&flags, &p.motion, screen)
            .into_iter()
            .flatten()
            .collect();
        let drone_now: Vec<f32> = drone_discs(&p.marks, &p.motion, None, screen)
            .into_iter()
            .flat_map(|(_, disc)| disc)
            .collect();
        assert_eq!(flag_now, flag_before);
        assert_eq!(drone_now, drone_before);
        let first = p.marks.bodies[0].agent.clone();
        assert_eq!(
            drone_discs(&p.marks, &p.motion, Some(&first), screen).len(),
            p.marks.bodies.len() - 1,
            "the skipped drone is left out"
        );
    }
}
