//! The point of view a ride takes from inside one agent's drone. [`pov_camera`] places the camera
//! behind the drone's eye, looking along its beam at the machine it works on. [`Ride`] holds the
//! overview camera the ride came from, and eases the camera as the drone moves and as the ride
//! starts, switches and ends. Everything here takes plain values, so the host tests cover it.

use std::f32::consts::TAU;

use acan_iso::EYE_DISTANCE;
use acan_render::camera::{self, VERTICAL_FOV};
use acan_render::{Aabb, Camera};
use serde::Serialize;

use crate::marks::{self, Body};
use crate::mirror::View;
use crate::plan::Plan;

/// Milliseconds an ease takes to enter, leave or switch drones.
pub const POV_ENTER_MS: f32 = 500.0;
/// The distance scale a wheel can reach. One is the default distance.
pub const SCALE_MIN: f32 = 0.7;
pub const SCALE_MAX: f32 = 1.6;
/// The hover bob's amplitude on screen, in CSS px at the target, and its period.
pub const BOB_CSS_PX: f32 = 3.0;
pub const BOB_PERIOD_MS: f32 = 3600.0;

/// Where a ride's camera looks: the drone's centre, the point its beam ends on, the way it
/// faces, and the box of the machine it works on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Goal {
    pub at: [f32; 3],
    pub target: [f32; 3],
    pub heading: f32,
    pub machine: Aabb,
}

/// What the chip says about the drone it rides.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Info {
    pub agent: String,
    pub hue: String,
    pub waiting: bool,
    pub machine: String,
    /// The activity, with the drone's state word after it when the two differ.
    pub activity: String,
    /// The drone's state, as [`marks::DroneState::word`] gives it.
    pub state: &'static str,
    pub task: Option<String>,
    pub file: Option<String>,
}

/// The chip's activity words: the activity, then the state word when it says something more,
/// such as `building · working` or `building · stale`, but `idle` once.
#[must_use]
pub fn chip_activity(activity: &str, state: &str) -> String {
    if activity == state {
        activity.to_owned()
    } else {
        format!("{activity} · {state}")
    }
}

/// A drone's goal and chip words, as the marks give them now.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub goal: Goal,
    pub info: Info,
}

/// Below this gap the drone's eye sits on the target, so its side has no direction.
const EYE_ON_TARGET: f32 = 1e-3;
/// The share of the view height that the machine's model box spans at scale 1.
const SPAN_TARGET: f32 = 0.5;
/// The span is not exactly proportional to 1/d under perspective, so the solve repeats.
const SPAN_PASSES: usize = 2;

/// The camera at `scale` times the distance that spans the machine's model box half the view
/// height, on the side of its drone's eye. The viewport, projection and fog bounds come from
/// `base`.
pub fn pov_camera(base: &Camera, goal: &Goal, scale: f32) -> Camera {
    let eye_drone = [
        goal.at[0] + EYE_DISTANCE * goal.heading.cos(),
        goal.at[1] + EYE_DISTANCE * goal.heading.sin(),
        goal.at[2],
    ];
    let rel = sub(eye_drone, goal.target);
    let reach = norm(rel);
    let dir = if reach < EYE_ON_TARGET {
        let elevation = camera::POV_MIN_ELEVATION.to_radians();
        [
            -goal.heading.cos() * elevation.cos(),
            -goal.heading.sin() * elevation.cos(),
            elevation.sin(),
        ]
    } else {
        std::array::from_fn(|axis| rel[axis] / reach)
    };
    // A sphere of this radius fills half the view, but a flat box projects less than its sphere,
    // so the sphere distance only starts the solve.
    let radius = norm(sub(goal.machine.max, goal.machine.min)) * 0.5;
    let sphere = radius / (0.5 * (VERTICAL_FOV * 0.5).to_radians().tan());
    let distance = scale * solved_distance(base, goal, dir, sphere);
    aimed(base, goal, dir, distance)
}

/// The distance along `dir` at which the machine's model box spans `SPAN_TARGET` of the view
/// height. `first` starts the solve. A pass whose span is unusable, or whose corner falls behind
/// the eye, keeps the distance it had.
fn solved_distance(base: &Camera, goal: &Goal, dir: [f32; 3], first: f32) -> f32 {
    let mut distance = first;
    for _ in 0..SPAN_PASSES {
        let Some(span) =
            span_at(base, goal, dir, distance).filter(|span| span.is_finite() && *span > 0.0)
        else {
            break;
        };
        distance *= span / SPAN_TARGET;
    }
    distance
}

/// The model box's vertical span as a share of the view height, from a camera `distance` out
/// along `dir`. `None` when a corner is behind the eye.
fn span_at(base: &Camera, goal: &Goal, dir: [f32; 3], distance: f32) -> Option<f32> {
    let camera = aimed(base, goal, dir, distance);
    let corners = goal.machine.corners();
    if corners.iter().any(|&corner| !camera.in_front(corner)) {
        return None;
    }
    let (low, high) = corners.iter().fold(
        (f32::INFINITY, f32::NEG_INFINITY),
        |(low, high), &corner| {
            let y = camera.world_to_screen(corner)[1];
            (low.min(y), high.max(y))
        },
    );
    Some((high - low) / camera.viewport()[1])
}

/// The base camera looking at the target from `distance` out along `dir`.
fn aimed(base: &Camera, goal: &Goal, dir: [f32; 3], distance: f32) -> Camera {
    let mut camera = *base;
    camera.look_at(
        std::array::from_fn(|axis| goal.target[axis] + dir[axis] * distance),
        goal.target,
    );
    camera
}

/// The hover bob's offset now, in world units. It is zero when the bob is off or the page holds
/// still. The offset is in CSS px at the target, so it is `BOB_CSS_PX * dpr / zoom` world units.
pub fn bob_offset(enabled: bool, still: bool, ms: f32, zoom: f32, dpr: f32) -> f32 {
    if !enabled || still {
        return 0.0;
    }
    BOB_CSS_PX * dpr / zoom * (TAU * ms / BOB_PERIOD_MS).sin()
}

/// The camera with its centre and eye both lifted `offset` world units, for drawing only. The
/// camera it is copied from does not change.
pub fn bobbed(camera: &Camera, offset: f32) -> Camera {
    if offset == 0.0 {
        return *camera;
    }
    let eye = camera.eye();
    let toward = camera.basis().toward;
    let distance = camera.eye_distance();
    let centre: [f32; 3] = std::array::from_fn(|axis| eye[axis] - distance * toward[axis]);
    let mut out = *camera;
    out.centre_on([centre[0], centre[1], centre[2] + offset]);
    out
}

/// Where a point lands on screen in CSS px, or `None` when it is behind the camera or off the
/// viewport. The reticle is hidden for `None`.
pub fn target_on_screen(camera: &Camera, target: [f32; 3], dpr: f32) -> Option<[f32; 2]> {
    if !camera.in_front(target) {
        return None;
    }
    let [x, y] = camera.world_to_screen(target);
    let [width, height] = camera.viewport();
    if x < 0.0 || y < 0.0 || x > width || y > height {
        return None;
    }
    Some([x / dpr, y / dpr])
}

/// The agents in ride order: sorted by name, so the order holds across events.
pub fn ride_order<'a>(agents: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut order: Vec<&str> = agents.into_iter().collect();
    order.sort_unstable();
    order
}

/// The agent a Tab (`direction` 1) or Shift+Tab (`direction` -1) rides next, wrapping at both
/// ends. Without a current agent, Tab rides the first and Shift+Tab the last.
pub fn next_in_order<'a>(
    order: &[&'a str],
    current: Option<&str>,
    direction: i32,
) -> Option<&'a str> {
    let len = i32::try_from(order.len()).ok().filter(|&len| len > 0)?;
    let at = current.and_then(|name| order.iter().position(|other| *other == name));
    let index = match at {
        Some(index) => (i32::try_from(index).ok()? + direction.signum()).rem_euclid(len),
        None if direction > 0 => 0,
        None => len - 1,
    };
    order.get(usize::try_from(index).ok()?).copied()
}

/// The chip's words for the drone of `body`: its machine's name as the machine's label shows it,
/// its activity as its flag shows it, the text of the order its task belongs to, and the first
/// file its task names on that machine.
pub fn info(view: &View, plan: &Plan, body: &Body) -> Info {
    let task = view
        .agents
        .iter()
        .find(|agent| agent.id.to_string() == body.agent)
        .and_then(|agent| agent.task)
        .and_then(|id| view.tasks.iter().find(|task| task.id == id));
    let sentence = task.and_then(|task| {
        view.orders
            .iter()
            .find(|order| order.id == task.order)
            .map(|order| order.sentence.clone())
    });
    let file = task.and_then(|task| {
        task.paths
            .iter()
            .find(|path| plan.machine_of_path(path) == Some(body.machine))
            .cloned()
    });
    Info {
        agent: body.agent.clone(),
        hue: marks::hue_css(body.slot),
        waiting: body.waiting(),
        machine: plan.machines[body.machine].name.clone(),
        activity: chip_activity(body.activity, body.state.word()),
        state: body.state.word(),
        task: sentence,
        file,
    }
}

/// A drone's snapshot, at the spot and facing the marks show now. The eye is placed from the
/// same spot and facing as the beam, so the camera sits on the ray the beam draws.
pub fn snapshot(
    view: &View,
    plan: &Plan,
    body: &Body,
    at: [f32; 3],
    heading: f32,
    machine: Aabb,
) -> Snapshot {
    Snapshot {
        goal: Goal {
            at,
            target: body.target,
            heading,
            machine,
        },
        info: info(view, plan, body),
    }
}

/// A ride in progress: the overview camera it returns to, the fog range that camera had, and the
/// drone it follows. The camera moves along one ease at a time.
#[derive(Debug, Clone)]
pub struct Ride {
    info: Info,
    goal: Goal,
    scale: f32,
    saved: Camera,
    hold: (f32, f32),
    tween: Option<Tween>,
    leaving: bool,
}

#[derive(Debug, Clone)]
struct Tween {
    from: Camera,
    to: Camera,
    elapsed: f32,
    duration: f32,
}

impl Ride {
    /// Starts a ride on `snapshot`'s drone, easing from the overview camera `saved`.
    pub fn enter(saved: Camera, snapshot: Snapshot) -> Self {
        let hold = saved.fog_range();
        let mut ride = Self {
            info: snapshot.info,
            goal: snapshot.goal,
            scale: 1.0,
            saved,
            hold,
            tween: None,
            leaving: false,
        };
        ride.aim(saved, POV_ENTER_MS);
        ride
    }

    pub fn agent(&self) -> &str {
        &self.info.agent
    }

    pub fn target(&self) -> [f32; 3] {
        self.goal.target
    }

    /// Whether the ride is easing back to the overview camera.
    pub fn is_leaving(&self) -> bool {
        self.leaving
    }

    /// Whether the ride has left and its ease has ended, so the page can drop it.
    pub fn finished(&self) -> bool {
        self.leaving && self.tween.is_none()
    }

    pub fn info(&self) -> &Info {
        &self.info
    }

    /// The overview camera the ride returns to on leave.
    pub fn saved(&self) -> Camera {
        self.saved
    }

    /// Moves to another drone, easing from the camera as it is now.
    pub fn switch(&mut self, snapshot: Snapshot, current: Camera) {
        self.info = snapshot.info;
        self.goal = snapshot.goal;
        self.aim(current, POV_ENTER_MS);
    }

    /// Starts a ride again while the last one is still easing back, from the camera as it is now.
    /// The overview camera it returns to stays the one the first ride saved.
    pub fn resume(&mut self, snapshot: Snapshot, current: Camera) {
        self.leaving = false;
        self.info = snapshot.info;
        self.goal = snapshot.goal;
        self.aim(current, POV_ENTER_MS);
    }

    /// Takes the drone's words as they are now. When its spot or its target moves, the camera
    /// eases to the new point of view over the marks' move time.
    pub fn follow(&mut self, snapshot: Snapshot, current: Camera) {
        self.info = snapshot.info;
        if self.leaving || snapshot.goal == self.goal {
            return;
        }
        self.goal = snapshot.goal;
        self.aim(current, marks::MOVE_MS);
    }

    /// Eases back to the overview camera the ride started from.
    pub fn leave(&mut self, current: Camera) {
        if self.leaving {
            return;
        }
        self.leaving = true;
        self.tween = Some(Tween {
            from: current,
            to: self.overview(),
            elapsed: 0.0,
            duration: POV_ENTER_MS,
        });
    }

    /// Changes the distance scale by `factor`, within its range, at once and without an ease.
    pub fn zoom(&mut self, factor: f32, camera: &mut Camera) {
        if self.leaving {
            return;
        }
        self.scale = (self.scale * factor).clamp(SCALE_MIN, SCALE_MAX);
        self.tween = None;
        *camera = self.rest(camera);
    }

    /// Takes a new viewport. The overview camera and the camera follow it, so a leave still
    /// restores the same view.
    pub fn resize(&mut self, viewport: [f32; 2], camera: &mut Camera) {
        self.saved.set_viewport(viewport);
        camera.set_viewport(viewport);
        if self.leaving {
            let to = self.overview();
            if let Some(tween) = self.tween.as_mut() {
                tween.to = to;
            }
            return;
        }
        let to = self.rest(camera);
        match self.tween.as_mut() {
            Some(tween) => tween.to = to,
            None => *camera = to,
        }
    }

    /// Moves the camera along its ease by `dt_ms`. Returns true while an ease runs. When a leave
    /// ends, the camera is the overview camera exactly.
    pub fn advance(&mut self, dt_ms: f32, camera: &mut Camera) -> bool {
        let Some(tween) = self.tween.as_mut() else {
            return false;
        };
        tween.elapsed += dt_ms.max(0.0);
        let t = (tween.elapsed / tween.duration).min(1.0);
        if t < 1.0 {
            *camera = camera::lerp(&tween.from, &tween.to, ease_out(t));
            return true;
        }
        let to = tween.to;
        self.tween = None;
        *camera = if self.leaving { self.saved } else { to };
        false
    }

    /// The chip's JSON: the drone's words and where its target is on screen.
    pub fn json(&self, target_css: Option<[f32; 2]>) -> String {
        #[derive(Serialize)]
        struct Out<'a> {
            #[serde(flatten)]
            info: &'a Info,
            target_css: Option<[f32; 2]>,
        }
        serde_json::to_string(&Out {
            info: &self.info,
            target_css,
        })
        .expect("the chip serializes as JSON")
    }

    fn rest(&self, base: &Camera) -> Camera {
        let mut camera = pov_camera(base, &self.goal, self.scale);
        camera.hold_fog(Some(self.hold));
        camera
    }

    fn overview(&self) -> Camera {
        let mut camera = self.saved;
        camera.hold_fog(Some(self.hold));
        camera
    }

    fn aim(&mut self, from: Camera, duration: f32) {
        let to = self.rest(&from);
        self.tween = Some(Tween {
            from,
            to,
            elapsed: 0.0,
            duration,
        });
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn norm(a: [f32; 3]) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::{self, Kind};
    use crate::layout::Placed;
    use crate::scene;

    const VIEWPORT: [f32; 2] = [1440.0, 900.0];

    /// The model box of a machine of `kind` at the origin, moved so its centre sits on the origin.
    fn centred_model_box(kind: Kind) -> Aabb {
        let bounds = scene::machine_model_box(
            &Placed {
                x: 0.0,
                y: 0.0,
                kind,
            },
            &scene::models(),
        );
        let centre: [f32; 2] =
            std::array::from_fn(|axis| (bounds.min[axis] + bounds.max[axis]) * 0.5);
        Aabb {
            min: [
                bounds.min[0] - centre[0],
                bounds.min[1] - centre[1],
                bounds.min[2],
            ],
            max: [
                bounds.max[0] - centre[0],
                bounds.max[1] - centre[1],
                bounds.max[2],
            ],
        }
    }

    /// The model box of an auth machine, which is a gate, centred on the origin. The goals below
    /// sit on the origin, so they frame a real machine's size.
    fn machine() -> Aabb {
        centred_model_box(Kind::Gate)
    }

    fn goal() -> Goal {
        Goal {
            at: [20.0, 0.0, 26.0],
            target: [0.0, 0.0, 20.0],
            heading: std::f32::consts::PI,
            machine: machine(),
        }
    }

    fn info() -> Info {
        Info {
            agent: "agent-a".to_owned(),
            hue: "#1f9e8f".to_owned(),
            waiting: false,
            machine: "auth".to_owned(),
            activity: "reading_back".to_owned(),
            state: "idle",
            task: Some("sign in".to_owned()),
            file: None,
        }
    }

    fn snapshot_at(goal: Goal) -> Snapshot {
        Snapshot { goal, info: info() }
    }

    /// The overview camera of a plant the size of the block, fitted at the default angles.
    fn overview() -> Camera {
        let block = Aabb {
            min: [-200.0, -200.0, 0.0],
            max: [200.0, 200.0, 60.0],
        };
        let mut camera = Camera::new(VIEWPORT);
        camera.set_fog_bounds(block);
        camera.fit(&block, 24.0);
        camera
    }

    fn vertical_extent(camera: &Camera, bounds: &Aabb) -> f32 {
        let ys: Vec<f32> = bounds
            .corners()
            .iter()
            .map(|&corner| camera.world_to_screen(corner)[1])
            .collect();
        let high = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let low = ys.iter().copied().fold(f32::INFINITY, f32::min);
        (high - low) / VIEWPORT[1]
    }

    #[test]
    fn the_pov_eye_sits_on_the_ray_through_the_drone_eye_and_the_machine_spans_half_the_view() {
        let goal = goal();
        let camera = pov_camera(&overview(), &goal, 1.0);
        let eye_drone = [
            goal.at[0] + EYE_DISTANCE * goal.heading.cos(),
            goal.at[1] + EYE_DISTANCE * goal.heading.sin(),
            goal.at[2],
        ];
        let want = {
            let rel = sub(eye_drone, goal.target);
            let len = norm(rel);
            [rel[0] / len, rel[1] / len, rel[2] / len]
        };
        let eye = camera.eye();
        let got = {
            let rel = sub(eye, goal.target);
            let len = norm(rel);
            [rel[0] / len, rel[1] / len, rel[2] / len]
        };
        for axis in 0..3 {
            assert!(
                (got[axis] - want[axis]).abs() < 1e-3,
                "the eye leaves the ray: {got:?} vs {want:?}"
            );
        }
        let extent = vertical_extent(&camera, &machine());
        assert!(
            (0.4..=0.6).contains(&extent),
            "the machine spans {extent} of the view height"
        );
        let target = target_on_screen(&camera, goal.target, 1.0).expect("the target is in view");
        assert!((target[0] - 720.0).abs() < 1e-2, "target x {}", target[0]);
    }

    #[test]
    fn the_solved_span_is_half_the_view_at_the_default_angles_for_every_kind() {
        for kind in kind::ALL {
            let machine = centred_model_box(kind);
            let goal = Goal { machine, ..goal() };
            let camera = pov_camera(&overview(), &goal, 1.0);
            let span = vertical_extent(&camera, &machine);
            assert!(
                (span - 0.5).abs() <= 0.05,
                "{kind:?} spans {span} of the view height"
            );
            for corner in machine.corners() {
                assert!(
                    camera.in_front(corner),
                    "{kind:?} corner {corner:?} is behind the eye"
                );
            }
        }
    }

    #[test]
    fn the_span_stays_half_the_view_when_the_drone_is_far_from_its_target() {
        let far = Goal {
            at: [400.0, 0.0, 26.0],
            ..goal()
        };
        let machine = machine();
        for goal in [goal(), far] {
            let camera = pov_camera(&overview(), &goal, 1.0);
            let span = vertical_extent(&camera, &machine);
            assert!(
                (span - 0.5).abs() <= 0.05,
                "the machine spans {span} with the drone at {:?}",
                goal.at
            );
        }
    }

    #[test]
    fn a_drone_on_its_target_is_viewed_from_behind_at_the_minimum_elevation() {
        let target = goal().target;
        let on_target = Goal {
            at: [target[0] - EYE_DISTANCE, target[1], target[2]],
            heading: 0.0,
            ..goal()
        };
        let camera = pov_camera(&overview(), &on_target, 1.0);
        let eye = camera.eye();
        assert!(eye.iter().all(|value| value.is_finite()), "eye {eye:?}");
        assert!(
            (camera.elevation() - camera::POV_MIN_ELEVATION).abs() < 1e-3,
            "elevation {}",
            camera.elevation()
        );
        assert!(
            (camera.azimuth() - 180.0).abs() < 1e-3,
            "azimuth {}",
            camera.azimuth()
        );
        let on_screen = target_on_screen(&camera, target, 1.0).expect("the target is in view");
        assert!(
            (on_screen[0] - 720.0).abs() < 1e-2,
            "target x {}",
            on_screen[0]
        );
    }

    #[test]
    fn the_chip_adds_the_state_word_to_the_activity() {
        assert_eq!(chip_activity("building", "working"), "building · working");
        assert_eq!(chip_activity("building", "stale"), "building · stale");
        assert_eq!(chip_activity("gating", "waiting"), "gating · waiting");
        assert_eq!(chip_activity("idle", "idle"), "idle");
    }

    #[test]
    fn ride_order_is_sorted_and_wraps_both_ways() {
        let order = ride_order(["agent-c", "agent-a", "agent-b"]);
        assert_eq!(order, ["agent-a", "agent-b", "agent-c"]);
        assert_eq!(next_in_order(&order, None, 1), Some("agent-a"));
        assert_eq!(next_in_order(&order, None, -1), Some("agent-c"));
        assert_eq!(next_in_order(&order, Some("agent-a"), 1), Some("agent-b"));
        assert_eq!(next_in_order(&order, Some("agent-c"), 1), Some("agent-a"));
        assert_eq!(next_in_order(&order, Some("agent-a"), -1), Some("agent-c"));
        assert_eq!(next_in_order(&[], None, 1), None);
    }

    #[test]
    fn the_bob_is_zero_when_still_or_disabled_and_leaves_the_stored_camera_alone() {
        assert_eq!(bob_offset(true, true, 900.0, 2.0, 2.0), 0.0);
        assert_eq!(bob_offset(false, false, 900.0, 2.0, 2.0), 0.0);
        let offset = bob_offset(true, false, 900.0, 2.0, 2.0);
        assert!(offset.abs() > 1e-3, "the bob moves when on");

        let stored = pov_camera(&overview(), &goal(), 1.0);
        let before = stored;
        let drawn = bobbed(&stored, offset);
        assert_eq!(stored, before, "the stored camera never changes");
        let (eye, lifted) = (stored.eye(), drawn.eye());
        assert!((lifted[2] - eye[2] - offset).abs() < 1e-3);
        assert!((lifted[0] - eye[0]).abs() < 1e-3 && (lifted[1] - eye[1]).abs() < 1e-3);
        assert_eq!(bobbed(&stored, 0.0), stored);
    }

    #[test]
    fn leaving_restores_the_overview_camera_exactly_after_the_ease() {
        let saved = overview();
        let mut camera = saved;
        let mut ride = Ride::enter(saved, snapshot_at(goal()));
        assert!(
            !ride.advance(POV_ENTER_MS + 1.0, &mut camera),
            "the enter ease ends"
        );
        assert_ne!(camera, saved, "the ride moved the camera");

        ride.leave(camera);
        assert!(ride.advance(POV_ENTER_MS / 2.0, &mut camera));
        assert!(!ride.advance(POV_ENTER_MS, &mut camera));
        assert!(ride.finished());
        assert_eq!(camera, saved, "the saved camera comes back bit for bit");
    }

    #[test]
    fn a_target_change_eases_over_the_move_time_and_the_first_frame_is_not_the_end() {
        let mut camera = overview();
        let mut ride = Ride::enter(camera, snapshot_at(goal()));
        ride.advance(POV_ENTER_MS, &mut camera);
        let moved = Goal {
            target: [40.0, 0.0, 20.0],
            ..goal()
        };
        ride.follow(snapshot_at(moved), camera);
        let start = camera;
        let end = ride.rest(&camera);
        assert!(ride.advance(16.0, &mut camera), "the target change eases");
        assert_ne!(camera, start, "the first frame has moved");
        assert_ne!(camera, end, "the first frame is not the final camera");
        assert!(
            !ride.advance(marks::MOVE_MS, &mut camera),
            "the ease ends at the move time"
        );
        assert_eq!(camera, end, "the camera lands on the new point of view");
        let on_screen =
            target_on_screen(&camera, moved.target, 1.0).expect("the new target is in view");
        assert!((on_screen[0] - 720.0).abs() < 1e-2);
    }

    #[test]
    fn the_fog_at_the_target_is_zero_while_riding() {
        let mut camera = overview();
        let goal = goal();
        let mut ride = Ride::enter(camera, snapshot_at(goal));
        ride.advance(POV_ENTER_MS, &mut camera);
        assert_eq!(camera.fog_at(goal.target), 0.0, "the target is not fogged");
        let neighbour = [60.0, 0.0, 20.0];
        assert_eq!(camera.fog_at(neighbour), 0.0, "a neighbour is not fogged");
    }

    #[test]
    fn a_target_behind_the_camera_has_no_screen_point() {
        let camera = pov_camera(&overview(), &goal(), 1.0);
        let eye = camera.eye();
        let target = goal().target;
        let behind = std::array::from_fn(|axis| 2.0 * eye[axis] - target[axis]);
        assert_eq!(target_on_screen(&camera, behind, 2.0), None);
    }
}
