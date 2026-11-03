//! The marks drawn on the plant, read from the origin's view.
//!
//! [`Marks::build`] says where each mark stands. An agent's drone hovers beside its machine, with
//! its shadow on the surface straight under its centre. The amber ring round an agent that waits on the user,
//! the clash pin on its stalk, the faint red of a machine with an open clash, and the flags the page
//! labels. [`card`]
//! builds the clash card from the view and the world, so its rows follow the origin. [`Motion`] only
//! eases marks between the positions the view gives them, and turns each drone toward its machine.

use std::collections::{BTreeMap, BTreeSet};
use std::f32::consts::{PI, TAU};

use acan_core::{OrderId, TaskId, World};
use acan_iso::{
    BEACON_RADIUS, BODY_RADIUS, BOT_HEIGHT, PIN_HEIGHT, PLINTH_TOP, STALK_HEIGHT, eye_point,
    shadow_at,
};
use acan_render::theme::{
    AGENT_SLOTS, TINT_BODY0, TINT_CLASH, TINT_DIM0, TINT_GHOST, TINT_SCOPE0, TINT_SHADOW,
    TINT_STALE,
};
use acan_render::{Beam, Instance};
use serde::Serialize;

use crate::mirror::{AgentView, CheckView, ClashView, Drift, View};
use crate::plan::Plan;
use crate::slices::{ScopeTint, Slice, slice_layout};
use crate::tour::Spot;

/// The height of the ground, in world units. A drone that hovers beside its machine has this
/// under it, so its shadow lies here.
pub const GROUND_Z: f32 = 0.0;

/// The gap between a seated drone's underside and the surface straight under it, in world units:
/// a quarter of its body radius, so its shadow touches it.
pub const HOVER_GAP: f32 = 0.25 * BODY_RADIUS;

/// The height of a drone's centre over the surface straight under it, in world units: its body
/// clears that surface by [`HOVER_GAP`].
#[must_use]
pub fn hover_height(surface: f32) -> f32 {
    surface + BODY_RADIUS + HOVER_GAP
}

/// The height of the first surface straight under `at` in plan: the top of the machine whose
/// footprint holds that point, the highest where several do, or else the ground. A footprint
/// stands to its stand's top, so a drone over a machine finds that top below it, and a drone
/// beside every machine finds the ground.
#[must_use]
pub fn surface_below(at: [f32; 2], plan: &Plan, stands: &[Stand]) -> f32 {
    plan.machines
        .iter()
        .enumerate()
        .filter(|(_, machine)| {
            (machine.min[0]..=machine.min[0] + machine.size[0]).contains(&at[0])
                && (machine.min[1]..=machine.min[1] + machine.size[1]).contains(&at[1])
        })
        .map(|(i, _)| stands.get(i).map_or(PLINTH_TOP, |stand| stand.top))
        .fold(GROUND_Z, f32::max)
}

/// The room above a drone's crown that its flag takes, in world units. The plant's fit keeps it
/// on screen, since a flag that leaves the canvas is hidden.
pub const FLAG_ROOM: f32 = 24.0;

/// Distance between the centres of two drones on one machine, in plan units. Two drones are a
/// body and a gap apart, so they never overlap.
const SEAT_STEP: f32 = 2.0 * BODY_RADIUS + 2.0;
/// Height above a machine's top where its order flag anchors, in plan units.
const ORDER_LIFT: f32 = 4.0;
/// The shortest a drone and a clash diamond are drawn on screen, in CSS px. The renderer grows
/// either one that would be shorter. The drone's body is sized to be about 40 px at fit zoom, so this
/// floor only applies when the view is zoomed out past fit.
pub const BOT_MIN_PX: f32 = 24.0;
pub const PIN_MIN_PX: f32 = 22.0;
/// The shortest a beacon is drawn on screen, in CSS px, when it is at rest. Only a working
/// agent's drone has a beacon.
const BEACON_PX: f32 = 7.0;
/// How much a working agent's beacon swells at the top of its pulse, as a share of its rest size.
const BEACON_SWING: f32 = 0.5;
/// The width of the tether from a drone's eye to its target, in device px.
const TETHER_PX: f32 = 1.5;
/// The opacity of a drone's shadow at full strength, which [`shadow_at`] thins as the drone rises.
const SHADOW_ALPHA: f32 = 0.32;
/// The least opacity a drone's shadow keeps at any height, so a drone at hover height still casts
/// a readable contact shadow on the ground under it.
const SHADOW_MIN_ALPHA: f32 = 0.22;
/// How far a stale drone sits below its seat, in plan units, so it reads as sagging.
const STALE_DROP: f32 = 0.25 * BODY_RADIUS;
/// Milliseconds for one pulse of a working agent's beacon.
pub const PULSE_MS: f32 = 1_200.0;
/// Milliseconds a drone takes to turn to face a new machine.
pub const TURN_MS: f32 = 400.0;
/// The activities in which an agent is working on its task, as the mirror names a claimed task's
/// stage. Its drone bobs and its beacon pulses while it works. Every other activity (`open`,
/// `readback`, `queued`, `landed` and `idle`) is idle.
const WORKING: [&str; 4] = ["claimed", "declared", "building", "gating"];
/// Plan units below which a drone and its target share a spot, so the drone has no direction.
const FACE_EPSILON: f32 = 1e-3;
/// Milliseconds a landed machine's dot shows its change word before it goes quiet.
pub const LANDED_HOLD_MS: f32 = 4_000.0;
/// Milliseconds a bot takes to rise off its machine, and an order flag to fade out.
pub const LEAVE_MS: f32 = 400.0;
/// How far a landed dot sits inside its plinth's corner, in plan units, so it stands on the plinth
/// and off its edge.
const LANDED_INSET: f32 = 2.0;
/// The diamond's half width as a share of its height. It sets where a clash label starts.
const DIAMOND_HALF_WIDTH: f32 = 0.42;
/// A clash badge's diameter, and its gap from the diamond's side, in CSS px.
pub const BADGE_PX: f32 = 16.0;
const BADGE_GAP_PX: f32 = 3.0;
/// The most characters a badge's initials take.
const INITIALS_MAX: usize = 3;
/// Gap between a bot's crown or a diamond and its flag, in CSS px.
const FLAG_GAP_PX: f32 = 8.0;
/// How far above its place a body starts its drop in, how far a leaving body rises, and how far
/// below its place a pin starts its rise, in plan units.
const RISE: f32 = 6.0;
/// Milliseconds a body takes to drop in on arrival.
pub const ARRIVE_MS: f32 = 240.0;
/// Milliseconds the ride's camera takes to follow its drone to a new spot.
pub const MOVE_MS: f32 = 400.0;
/// A move between machines takes `MOVE_BASE_MS` plus `MOVE_PER_UNIT_MS` for each plan unit it
/// covers, held between the two bounds.
const MOVE_BASE_MS: f32 = 350.0;
const MOVE_PER_UNIT_MS: f32 = 0.6;
const MOVE_BOUNDS_MS: (f32, f32) = (450.0, 900.0);
/// How far a moving drone's centre clears the taller of its two machines at the top of its arc.
const ARC_CLEAR: f32 = 1.5 * BODY_RADIUS;
/// Milliseconds a moving drone takes to turn to its way of travel at the start of a move, and to
/// its target at the end.
const FACE_MS: f32 = 150.0;
/// A move's trail: the pieces it is cut into, its opacity as the drone passes, and the
/// milliseconds each piece takes to fade out after that.
const TRAIL_PIECES: usize = 24;
const TRAIL_ALPHA: f32 = 0.45;
const TRAIL_FADE_MS: f32 = 2_000.0;
/// The width of a trail in CSS px.
const TRAIL_PX: f32 = 1.0;
/// A working drone's bob: its height on screen in CSS px, and its period.
const BOB_PX: f32 = 1.0;
const BOB_PERIOD_MS: f32 = 2_400.0;
/// An idle drone's bob, lower and slower than a working one's, so the two states still read apart.
const IDLE_BOB_PX: f32 = 0.6;
const IDLE_BOB_PERIOD_MS: f32 = 4_000.0;
const _: () = assert!(IDLE_BOB_PX < BOB_PX && IDLE_BOB_PERIOD_MS > BOB_PERIOD_MS);
/// Milliseconds a pin takes to rise in, or to sink out.
pub const PIN_MS: f32 = 250.0;
/// Milliseconds an agent that holds a task may be quiet before its drone reads stale.
pub const STALE_MS: u64 = 2 * 60 * 1_000;
/// Milliseconds an agent that holds a task may be quiet before its drone goes and an away dot
/// takes its place.
pub const AWAY_MS: u64 = 10 * 60 * 1_000;
/// Milliseconds a claimed task may go without a declaration, with its agent still heard, before
/// its drone drifts.
pub const STUCK_DECLARE_MS: u64 = 5 * 60 * 1_000;
/// Milliseconds a declared and cleared task may go without a build report, with its agent still
/// heard, before its drone drifts.
pub const STUCK_BUILD_MS: u64 = 15 * 60 * 1_000;
/// The most ghosts the plant shows at once. The latest landings keep theirs.
pub const GHOST_MAX: usize = 12;
/// How far a ghost's outline stands out from its machine's footprint, in plan units.
const GHOST_OFFSET: f32 = 3.0;
/// A ghost's opacity, and its width in CSS px.
const GHOST_ALPHA: f32 = 0.8;
const GHOST_PX: f32 = 1.75;
/// Milliseconds a ghost takes to fade out once it clears.
pub const GHOST_FADE_MS: f32 = 400.0;
/// The dot of a declaration whose task has no agent.
const UNCLAIMED: &str = "#7a7a72";

/// The palette slot of each agent. The first agent in the view takes slot 0, the next slot 1, and
/// so on. A slot stays with its agent while the agent is in the view, so a hue does not move
/// when another agent arrives.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentSlots(BTreeMap<String, usize>);

impl AgentSlots {
    /// Gives every agent in `view` a slot, keeping the ones already given. A new agent takes the
    /// lowest slot that no agent holds.
    pub fn assign(&mut self, view: &View) {
        let present: BTreeSet<String> = view
            .agents
            .iter()
            .map(|agent| agent.id.to_string())
            .collect();
        self.0.retain(|agent, _| present.contains(agent));
        for agent in &view.agents {
            let id = agent.id.to_string();
            if self.0.contains_key(&id) {
                continue;
            }
            let taken: BTreeSet<usize> = self.0.values().copied().collect();
            let slot = (0..AGENT_SLOTS)
                .find(|slot| !taken.contains(slot))
                .unwrap_or(taken.len() % AGENT_SLOTS);
            self.0.insert(id, slot);
        }
    }

    /// The slot of `agent`. An agent the view has not given a slot reads slot 0.
    #[must_use]
    pub fn slot(&self, agent: &str) -> usize {
        self.0.get(agent).copied().unwrap_or(0)
    }
}

/// The hue of a palette slot, as `#rrggbb`. Every mark that shows an agent's hue reads it here,
/// and the renderer's tints for the drone come from the same palette.
#[must_use]
pub fn hue_css(slot: usize) -> String {
    let [r, g, b, _] = acan_render::theme::agent_colour(slot).map(|c| (c * 255.0).round() as u32);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The tint slot of an agent's hue at full strength. A working drone's eye, its beam and its
/// beacon draw with it.
#[must_use]
pub fn body_tint(slot: usize) -> u8 {
    u8::try_from(TINT_BODY0 as usize + slot).expect("tint slots fit in a byte")
}

/// The tint slot of a drone's accent, its iris, lens ring, ear hubs and antenna tip, in `state`:
/// the full hue while it works, the hue dimmed while it idles or waits, and grey once stale.
#[must_use]
pub fn eye_tint(state: DroneState, slot: usize) -> u8 {
    let tint = match state {
        DroneState::Working | DroneState::Drifting => u32::from(body_tint(slot)),
        DroneState::Idle | DroneState::Waiting => u32::from(body_tint(slot)) + TINT_DIM0,
        DroneState::Stale => TINT_STALE,
    };
    u8::try_from(tint).expect("tint slots fit in a byte")
}

/// A drone over a machine. It hovers on a spot beside the machine, [`HOVER_GAP`] above the
/// surface straight under it, so its shadow touches it. Its shadow lies on that surface under its
/// centre.
#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    pub agent: String,
    pub slot: usize,
    /// The plan index of the machine it works on.
    pub machine: usize,
    /// Where the drone's centre hovers, in world units: [`hover_height`] over the surface straight
    /// under its seat.
    pub at: [f32; 3],
    /// The height of the surface straight under the drone, in world units: the ground beside its
    /// machine, or the machine's top when the drone hovers over it. Its shadow lies there.
    pub ground: f32,
    /// The point the drone faces and its beam ends at, in world units. It is the machine's centre on
    /// its top, lifted by the beacon's radius so the beacon stands on the top.
    pub target: [f32; 3],
    pub activity: &'static str,
    /// What the drone shows: working, idle, waiting on the user, stale, or drifting.
    pub state: DroneState,
    /// The check the agent's task answers to, as the chip beside its flag shows it.
    pub check: Option<CheckView>,
}

impl Body {
    /// Whether the agent is working on its task now, drifting or not, so its beacon pulses.
    #[must_use]
    pub fn working(&self) -> bool {
        matches!(self.state, DroneState::Working | DroneState::Drifting)
    }

    /// Whether the agent waits on the user: its task is in an open clash, or its order asks a
    /// question, is stopped, or has an open contest.
    #[must_use]
    pub fn waiting(&self) -> bool {
        self.state == DroneState::Waiting
    }
}

/// What a drone shows of its agent. One state shows at a time, by precedence: waiting, then
/// stale, then drifting, then working, then idle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DroneState {
    /// On a claimed task: its eye is bright, it bobs and its beacon pulses.
    Working,
    /// On an open, read-back, queued or landed task, or none: its eye dim, and it bobs lower and
    /// slower than a working drone.
    #[default]
    Idle,
    /// Its task is in an open clash, or its order asks a question, is stopped or has an open
    /// contest: as idle, with an amber ring.
    Waiting,
    /// It holds a task but has been quiet for a while: still and lower, and its eye grey.
    Stale,
    /// It holds a task it has drifted from: it looks as it does while it works, and its flag
    /// carries a grey chevron.
    Drifting,
}

impl DroneState {
    /// The state of an agent whose activity is `activity`. `waiting` is whether it waits on the
    /// user, `stale` whether it holds a task and has gone quiet, and `drifting` whether its task
    /// has drifted from its plan.
    #[must_use]
    pub fn of(activity: &str, waiting: bool, stale: bool, drifting: bool) -> Self {
        if waiting {
            Self::Waiting
        } else if stale {
            Self::Stale
        } else if drifting {
            Self::Drifting
        } else if WORKING.contains(&activity) {
            Self::Working
        } else {
            Self::Idle
        }
    }

    /// The tether's opacity: strong while the agent works, waits or drifts, faint while it idles,
    /// and fainter still once it has gone stale.
    #[must_use]
    pub fn tether_alpha(self) -> f32 {
        match self {
            Self::Working | Self::Waiting | Self::Drifting => 0.9,
            Self::Idle => 0.3,
            Self::Stale => 0.2,
        }
    }

    /// The bob of a drone in this state: its height on screen in CSS px and its period in ms.
    /// `None` for a drone that holds still: a waiting drone, so its ring stays put, and a stale one.
    #[must_use]
    pub fn bob(self) -> Option<(f32, f32)> {
        match self {
            Self::Working | Self::Drifting => Some((BOB_PX, BOB_PERIOD_MS)),
            Self::Idle => Some((IDLE_BOB_PX, IDLE_BOB_PERIOD_MS)),
            Self::Waiting | Self::Stale => None,
        }
    }

    /// The state in one word, as the ride chip and the flag's hover line name it.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Waiting => "waiting",
            Self::Stale => "stale",
            Self::Drifting => "drift",
        }
    }
}

/// The angle, in radians from +x, of the ground direction from `from` to `to`. `None` when the two
/// points share a spot, since then there is no direction to face.
#[must_use]
pub fn heading(from: [f32; 3], to: [f32; 3]) -> Option<f32> {
    let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
    (dx.hypot(dy) > FACE_EPSILON).then(|| dy.atan2(dx))
}

/// A clash the pin marks: an unsettled clash with a machine to stand on.
#[derive(Debug, Clone, PartialEq)]
pub struct Pin {
    /// The clash id, as the view writes it. It keys the pin's motion.
    pub id: String,
    /// The index of the clash in the view.
    pub clash: usize,
    /// The plan index of the machine it stands on.
    pub machine: usize,
    /// Where the stalk stands, in world units: the machine's centre on its top. The diamond sits
    /// on the stalk's top, [`STALK_HEIGHT`] above this point.
    pub at: [f32; 3],
}

/// The round badge on one side of a clash diamond: the initials of the agent that holds one of
/// the two tasks, on its hue, or a grey `?` for a task no agent holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Badge {
    pub text: String,
    pub hue: String,
}

/// A clash's two badges: task `a`'s on the diamond's left, task `b`'s on its right.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Badges {
    /// The index of the clash in the view, so a click opens its card.
    pub clash: usize,
    pub a: Badge,
    pub b: Badge,
}

/// An agent's initials, from the last segment of its id (split on `-`, `_`, space and `.`): that
/// segment's first letter in upper case, then the id's trailing digits, at most [`INITIALS_MAX`]
/// characters. A last segment with no letter falls back to the id's first letter. `w2` gives
/// `W2`, `agent-a` gives `A`, `agent-b` gives `B` and `worker-12` gives `W12`.
#[must_use]
pub fn initials(id: &str) -> String {
    initials_with(id, 0)
}

/// The initials of the two agents on one clash, never the same text. When they would read the
/// same, each takes more letters of its last segment, so `agent` and `anna` give `AG` and `AN`.
/// Ids that still match, such as two whose last segments are equal, get a digit on the second.
#[must_use]
pub fn pair_initials(a: &str, b: &str) -> (String, String) {
    for more in 0..INITIALS_MAX {
        let (first, second) = (initials_with(a, more), initials_with(b, more));
        if first != second {
            return (first, second);
        }
    }
    let first = initials(a);
    let stem: String = first.chars().take(INITIALS_MAX - 1).collect();
    let second = ('2'..='9')
        .map(|digit| format!("{stem}{digit}"))
        .find(|text| *text != first)
        .expect("one of eight digits differs");
    (first, second)
}

/// The last segment of `id`, split on `-`, `_`, space and `.`.
fn last_segment(id: &str) -> &str {
    id.rsplit(['-', '_', ' ', '.']).next().unwrap_or(id)
}

/// The first letter of `id`'s last segment (or of `id`, when that segment has no letter), then
/// `more` of the letters after it, then the id's trailing digits, in upper case and cut to
/// [`INITIALS_MAX`] characters.
fn initials_with(id: &str, more: usize) -> String {
    let digits: String = {
        let tail: Vec<char> = id.chars().rev().take_while(char::is_ascii_digit).collect();
        tail.into_iter().rev().collect()
    };
    let segment = last_segment(id);
    let source = if segment.chars().any(char::is_alphabetic) {
        segment
    } else {
        id
    };
    let letters = source
        .chars()
        .filter(|c| c.is_alphabetic())
        .take(1 + more)
        .flat_map(char::to_uppercase);
    let mut out: String = letters.chain(digits.chars()).take(INITIALS_MAX).collect();
    if out.is_empty() {
        out.push('?');
    }
    out
}

/// The badges of each pin's clash, in pin order.
#[must_use]
pub fn clash_badges(view: &View, pins: &[Pin], slots: &AgentSlots) -> Vec<Badges> {
    let holder = |task: TaskId| {
        view.tasks
            .iter()
            .find(|candidate| candidate.id == task)
            .and_then(|candidate| candidate.agent.as_ref())
            .map(ToString::to_string)
    };
    pins.iter()
        .filter_map(|pin| {
            let clash = view.clashes.get(pin.clash)?;
            let (a, b) = (holder(clash.a), holder(clash.b));
            let (a_text, b_text) = match (&a, &b) {
                (Some(a), Some(b)) => pair_initials(a, b),
                _ => (
                    a.as_deref().map_or_else(|| "?".to_owned(), initials),
                    b.as_deref().map_or_else(|| "?".to_owned(), initials),
                ),
            };
            let badge = |agent: &Option<String>, text: String| Badge {
                text,
                hue: agent
                    .as_deref()
                    .map_or_else(|| UNCLAIMED.to_owned(), |id| hue_css(slots.slot(id))),
            };
            Some(Badges {
                clash: pin.clash,
                a: badge(&a, a_text),
                b: badge(&b, b_text),
            })
        })
        .collect()
}

/// The centres of a diamond's two badges on screen, in CSS px, left then right, given the
/// diamond's foot in world units.
#[must_use]
pub fn badge_centres(foot: [f32; 3], project: impl Fn([f32; 3]) -> [f32; 2]) -> [[f32; 2]; 2] {
    let (base, height) = visual_extent(foot, PIN_HEIGHT, PIN_MIN_PX, &project);
    let y = base[1] - height * 0.5;
    let reach = height * DIAMOND_HALF_WIDTH + BADGE_GAP_PX + BADGE_PX * 0.5;
    [[base[0] - reach, y], [base[0] + reach, y]]
}

/// What a flag's anchor follows while the marks move.
#[derive(Debug, Clone, PartialEq)]
pub enum Follow {
    /// Stays at the flag's own point.
    Still,
    /// Rides the body of the named agent, bobbing with it at its slot and state.
    Body {
        agent: String,
        slot: usize,
        state: DroneState,
    },
    /// Rides the diamond of the named clash.
    Pin(String),
    /// The landed dot of a machine. `count` is how many of its tasks have landed, so a new
    /// landing restarts the dot's hold.
    Landed { machine: usize, count: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagKind {
    Agent,
    Order,
    Clash,
    Question,
    Landed,
    /// An agent quiet for [`AWAY_MS`] or more: a hollow ring in its hue and how long it has been
    /// quiet, on the back corner of its machine's plinth, where its drone was.
    Away,
    /// One scope lane's plots and keeps on one machine.
    Plot,
    /// A mark a tour step stages, as a chip on the machine the mark sits on.
    Stage,
}

/// What an agent's flag says of its drone, beyond its short text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DroneFlag {
    /// The agent id in full, as `acan:drone-hover` and the ride name it.
    pub agent: String,
    /// The drone's state, as [`DroneState::word`] gives it.
    pub state: &'static str,
    /// How long a stale agent has been quiet, such as `3m`. A drifting agent's words, such as
    /// `drift · quiet 6m` or `drift · beyond scope`.
    pub quiet: Option<String>,
}

/// The most characters of an agent id its flag shows.
const FLAG_ID_CHARS: usize = 12;

/// A label the page draws over the plant, with its anchor in world units.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Flag {
    pub kind: FlagKind,
    pub text: String,
    /// The agent's hue as `#rrggbb`, for the dot.
    pub hue: Option<String>,
    /// Whether the agent waits on the user. The flag then carries an amber dot.
    pub waiting: bool,
    /// The clash index a clash flag belongs to, so a click opens its card.
    pub clash: Option<usize>,
    /// The check an agent's task answers to, shown as a chip beside the agent's flag.
    pub check: Option<CheckView>,
    /// Whether the flag is fading out. A flag that has left the view keeps its place for
    /// [`LEAVE_MS`] and then goes.
    pub leaving: bool,
    /// The lines Alt adds under the flag. A plot flag lists its plots and keeps here.
    pub detail: Vec<String>,
    /// On a clash flag, the confirmed scope the clash sits inside, in words.
    pub scope: Option<String>,
    /// On an agent's flag, the drone it names.
    pub drone: Option<DroneFlag>,
    /// The machine an order, clash or plot flag marks, as the plan names it. The page shows such
    /// a flag's words only while that machine, or the flag's own mark, is hovered.
    pub on: Option<String>,
    /// The plan index of the machine an agent's flag stacks on, so the page groups them.
    #[serde(skip)]
    pub machine: Option<usize>,
    #[serde(skip)]
    pub at: [f32; 3],
    #[serde(skip)]
    pub follow: Follow,
}

/// One row of the clash card.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardRow {
    /// A declaration: the agent that holds the task, the task, and its hue.
    Declaration {
        agent: String,
        hue: String,
        text: String,
    },
    /// The witness the clash would break, in red.
    Witness { text: String },
    /// The confirmed scope the clash sits inside. The overlap is on ground a human allowed, so
    /// it is a boundary dispute between two agents, not a scope violation.
    Scope { text: String },
    /// The settlement the card suggests, always the last row. The key settles with it.
    Settle {
        key: usize,
        text: String,
        suggested: bool,
    },
}

/// The clash card: its title, its rows, and what each key does.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Card {
    /// The index of the clash in the view.
    pub clash: usize,
    pub title: String,
    pub rows: Vec<CardRow>,
    /// What each key does, such as `1 narrow t-0`. Empty once the clash is resolved.
    pub keys: Vec<String>,
    /// How the clash was resolved, once it has been.
    pub resolved: Option<String>,
}

/// The page's clock, as the marks read it. Every time is in log milliseconds. A frozen or
/// replayed page gives no `now`, so nothing there reads stale or has a ghost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Clock {
    /// The time now, on a live page.
    pub now: Option<u64>,
    /// The reader's last time away, once they come back to the page after a while.
    pub away: Option<Away>,
}

/// A time the reader was away from the page: when they last saw it, and when they came back.
/// Only what landed between the two has a ghost, so work that lands while they watch has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Away {
    pub seen: u64,
    pub back: u64,
}

impl Clock {
    /// How long `agent` has been quiet, or `None` with no clock or when it was never heard.
    fn quiet(self, agent: &AgentView) -> Option<u64> {
        Some(self.now?.saturating_sub(agent.heard_at?))
    }
}

/// What the marks need from a plan machine's model: the height of its top, and the free spots on
/// its plinth top, in world XY, front first.
#[derive(Debug, Clone, PartialEq)]
pub struct Stand {
    /// The height of the model's top, in world units. The stalk rises from it.
    pub top: f32,
    pub spots: Vec<[f32; 2]>,
}

/// Every mark the view calls for, placed on the plan.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Marks {
    pub bodies: Vec<Body>,
    pub pins: Vec<Pin>,
    pub flags: Vec<Flag>,
    /// For each plan machine, whether it is abnormal: an unsettled clash sits on it. Only an
    /// abnormal machine takes a colour from the live work. A working, waiting or stale agent
    /// leaves its machine grey, and the drone carries the state.
    pub abnormal: Vec<bool>,
    /// For each plan machine, the scope tint its footprint takes when it is not abnormal.
    pub scope_tints: Vec<Option<ScopeTint>>,
    /// Scope machine names the plan does not have, so the page can say they are not drawn.
    pub unplaced: Vec<String>,
    /// The machines where work landed while the reader was away, latest first.
    pub ghosts: Vec<Ghost>,
}

/// A machine where work landed while the reader was away, drawn as a grey outline round its
/// footprint on the ground until the reader looks at it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ghost {
    /// The plan machine.
    pub machine: usize,
    /// How many of its tasks landed since the reader last saw the page.
    pub count: usize,
    /// When the latest of them landed, in log milliseconds.
    pub at: u64,
    /// The outline, as `[x, y, w, h]` in world units on the ground.
    pub rect: [f32; 4],
}

impl Ghost {
    /// The outline's corners on the ground, in turn round the rectangle.
    #[must_use]
    pub fn corners(&self) -> [[f32; 3]; 4] {
        let [x, y, w, h] = self.rect;
        [
            [x, y, 0.0],
            [x + w, y, 0.0],
            [x + w, y + h, 0.0],
            [x, y + h, 0.0],
        ]
    }
}

impl Marks {
    /// Places the marks for `view` on `plan`, with no clock. `stands` gives each plan machine's
    /// model, in plan order. `slots` gives each agent its hue, so it must already cover the
    /// view's agents.
    #[must_use]
    pub fn build(view: &View, plan: &Plan, stands: &[Stand], slots: &AgentSlots) -> Self {
        Self::build_at(view, plan, stands, slots, Clock::default())
    }

    /// Places the marks for `view` on `plan` at the time `clock` gives, so a quiet agent's
    /// drone can read stale.
    #[must_use]
    pub fn build_at(
        view: &View,
        plan: &Plan,
        stands: &[Stand],
        slots: &AgentSlots,
        clock: Clock,
    ) -> Self {
        let waiting = waiting_agents(view);
        let (bodies, pins) = place(view, plan, stands, slots, &waiting, clock);
        let abnormal = (0..plan.machines.len())
            .map(|i| view.machines.get(i).is_some_and(|machine| machine.abnormal))
            .collect();
        let scope = slice_layout(&view.lanes, plan);
        let mut flags = flags(view, plan, stands, &bodies, &pins, clock);
        flags.extend(away_flags(view, plan, slots, &waiting, clock));
        flags.extend(
            scope
                .slices
                .iter()
                .map(|slice| plot_flag(slice, plan, stands)),
        );
        Self {
            bodies,
            pins,
            flags,
            abnormal,
            scope_tints: scope.tints,
            unplaced: scope.unplaced,
            ghosts: ghosts(view, plan, clock),
        }
    }

    /// The tint slot of a plan machine's footprint: the faint clash red when it is abnormal,
    /// otherwise its scope tint or none. An open clash outranks the scope underneath it.
    #[must_use]
    pub fn footprint_tint(&self, machine: usize) -> u8 {
        let tint = if self.abnormal.get(machine).copied().unwrap_or(false) {
            TINT_CLASH
        } else {
            match self.scope_tints.get(machine).copied().flatten() {
                Some(scope) => TINT_SCOPE0 + scope as u32,
                None => 0,
            }
        };
        u8::try_from(tint).expect("tint slots fit in a byte")
    }
}

/// The tint slot an instance takes while a tour step is staged. A machine the step stages keeps its
/// slot, and any other takes its dimmed slot, so the step's marks stand out. With no stage, every
/// machine keeps its slot.
#[must_use]
pub fn emphasised(tint: u8, staged: bool) -> u8 {
    if staged {
        tint
    } else {
        let dimmed = u32::from(tint) + TINT_DIM0;
        u8::try_from(dimmed).expect("dimmed tint slots fit in a byte")
    }
}

/// Which plan machines a tour step stages, in plan order, from the spots it names. `None` when no
/// named machine is on the plant, so nothing is dimmed.
#[must_use]
pub fn staged_plan(machine_names: &[String], spots: &[Spot]) -> Option<Vec<bool>> {
    let staged: Vec<bool> = machine_names
        .iter()
        .map(|name| spots.iter().any(|spot| spot.machine_id.as_str() == name))
        .collect();
    staged.iter().any(|&on| on).then_some(staged)
}

/// The chips a tour step stages: one per mark, stacked over the plan machine the mark sits on. A
/// spot whose machine is not on the plant has no chip.
#[must_use]
pub fn stage_flags(spots: &[Spot], plan: &Plan, stands: &[Stand]) -> Vec<Flag> {
    let mut flags = Vec::new();
    for spot in spots {
        let Some(machine) = plan
            .machines
            .iter()
            .position(|plan_machine| plan_machine.name == spot.machine_id.as_str())
        else {
            continue;
        };
        let centre = plan.machines[machine].center();
        let top = stands.get(machine).map_or(PLINTH_TOP, |stand| stand.top);
        for label in &spot.labels {
            flags.push(Flag {
                kind: FlagKind::Stage,
                text: label.clone(),
                hue: None,
                waiting: false,
                clash: None,
                check: None,
                leaving: false,
                detail: Vec::new(),
                scope: None,
                machine: Some(machine),
                at: [centre[0], centre[1], top + ORDER_LIFT],
                drone: None,
                on: None,
                follow: Follow::Still,
            });
        }
    }
    flags
}

/// The tint slot a plan machine's instance takes now: its footprint slot, dimmed while a step is
/// staged and the machine is not one of its marks. `staged` is by plan index, and `None` when no
/// step is staged.
#[must_use]
pub fn machine_tint(marks: &Marks, plan_index: usize, staged: Option<&[bool]>) -> u8 {
    let tint = marks.footprint_tint(plan_index);
    match staged {
        None => tint,
        Some(staged) => emphasised(tint, staged.get(plan_index).copied().unwrap_or(true)),
    }
}

#[cfg(test)]
mod stage_tests {
    use super::*;

    fn spot(machine: &str) -> Spot {
        Spot {
            machine_id: serde_json::from_str(&format!("\"{machine}\"")).expect("a machine id"),
            labels: vec![format!("bounced: {machine}")],
        }
    }

    #[test]
    fn a_staged_machine_keeps_its_slot_and_any_other_takes_the_dimmed_one() {
        let dim0 = u8::try_from(TINT_DIM0).expect("fits");
        assert_eq!(emphasised(7, true), 7);
        assert_eq!(emphasised(7, false), 7 + dim0);
        assert_eq!(emphasised(0, false), dim0);
    }

    #[test]
    fn a_step_stages_only_the_machines_it_names_and_none_when_none_is_on_the_plant() {
        let names = [
            "auth".to_owned(),
            "links".to_owned(),
            "ratelimit".to_owned(),
        ];
        assert_eq!(
            staged_plan(&names, &[spot("ratelimit")]),
            Some(vec![false, false, true])
        );
        assert_eq!(staged_plan(&names, &[spot("elsewhere")]), None);
        assert_eq!(staged_plan(&names, &[]), None);
    }

    #[test]
    fn a_step_dims_the_machines_it_does_not_stage_and_clearing_restores_them() {
        let marks = Marks {
            abnormal: vec![true, false],
            ..Marks::default()
        };
        let staged = [true, false];
        let clash = u8::try_from(TINT_CLASH).expect("fits");
        let dim0 = u8::try_from(TINT_DIM0).expect("fits");
        assert_eq!(machine_tint(&marks, 0, Some(&staged)), clash);
        assert_eq!(machine_tint(&marks, 1, Some(&staged)), dim0);
        // With no step staged, each machine keeps the slot its footprint gives it.
        assert_eq!(machine_tint(&marks, 0, None), marks.footprint_tint(0));
        assert_eq!(machine_tint(&marks, 1, None), marks.footprint_tint(1));
    }

    #[test]
    fn each_dimmed_slot_is_its_plain_colour_at_the_dim_share() {
        let table = acan_render::theme::tints();
        for slot in 0..TINT_DIM0 as usize {
            let plain = table[slot];
            let dimmed = table[TINT_DIM0 as usize + slot];
            for channel in 0..3 {
                assert!(
                    (dimmed[channel] - plain[channel] * acan_render::theme::DIM_SHARE).abs() < 1e-6,
                    "slot {slot} channel {channel}"
                );
            }
            assert_eq!(dimmed[3], plain[3], "slot {slot} keeps its alpha");
        }
    }
}

/// The clash card for the clash at `index` in the view, or `None` when there is no such clash.
#[must_use]
pub fn card(view: &View, world: &World, index: usize, slots: &AgentSlots) -> Option<Card> {
    let clash = view.clashes.get(index)?;
    let mut rows: Vec<CardRow> = [clash.a, clash.b]
        .into_iter()
        .map(|task| declaration(view, task, slots))
        .collect();
    if let Some(witness) = &clash.breaks {
        rows.push(CardRow::Witness {
            text: format!("breaks {witness}"),
        });
    }
    if let Some(scope) = &clash.scope {
        rows.push(CardRow::Scope {
            text: scope.clone(),
        });
    }
    let mut keys = Vec::new();
    if is_open(clash) {
        keys = clash
            .options
            .iter()
            .map(|option| format!("{} {}", option.key, option.label))
            .collect();
        let suggested = world
            .clashes()
            .find(|candidate| candidate.id == clash.id)
            .and_then(|candidate| candidate.explanation.as_ref())
            .and_then(|explanation| explanation.suggested.clone());
        let chosen = clash
            .options
            .iter()
            .position(|option| suggested.as_ref() == Some(&option.choice));
        let (option, from_explanation) = match chosen {
            Some(i) => (clash.options.get(i), true),
            None => (clash.options.first(), false),
        };
        if let Some(option) = option {
            rows.push(CardRow::Settle {
                key: option.key,
                text: option.label.clone(),
                suggested: from_explanation,
            });
        }
    }
    Some(Card {
        clash: index,
        title: clash.label.clone(),
        rows,
        keys,
        resolved: clash.resolved.clone(),
    })
}

/// How a body's glide eases.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Path {
    /// A drop straight down onto its place, easing out.
    Arrive,
    /// An arc between machines whose top is at `apex`, easing in and out. The drone faces
    /// `travel` on the way, turning from `start` at the outset and to its target at the end.
    Move { apex: f32, travel: f32, start: f32 },
}

/// A body's glide, from where it was to where the view puts it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Glide {
    from: [f32; 3],
    to: [f32; 3],
    elapsed: f32,
    duration: f32,
    path: Path,
    /// The agent's palette slot, kept so the body can still draw once it has left.
    slot: usize,
    /// The top of the machine the body glides to, in world units. The next move arcs over it.
    top: f32,
}

impl Glide {
    /// A drop onto `to` from [`RISE`] above it.
    fn arrive(to: [f32; 3], slot: usize, top: f32) -> Self {
        Self {
            from: [to[0], to[1], to[2] + RISE],
            to,
            elapsed: 0.0,
            duration: ARRIVE_MS,
            path: Path::Arrive,
            slot,
            top,
        }
    }

    /// An arc from `from` to `to` over the taller of the two machine tops, facing the way it
    /// travels. `start` is the heading the drone faces as it sets off.
    fn moving(from: [f32; 3], to: [f32; 3], tops: (f32, f32), start: f32, slot: usize) -> Self {
        let travel = heading(from, to).unwrap_or(start);
        Self {
            from,
            to,
            elapsed: 0.0,
            duration: move_ms(distance([from[0], from[1]], [to[0], to[1]])),
            path: Path::Move {
                apex: tops.0.max(tops.1) + ARC_CLEAR,
                travel,
                start,
            },
            slot,
            top: tops.1,
        }
    }

    /// The glide already at its end, for a mark that must not move.
    fn settled(to: [f32; 3], slot: usize, top: f32) -> Self {
        Self {
            from: to,
            to,
            elapsed: 0.0,
            duration: 0.0,
            path: Path::Arrive,
            slot,
            top,
        }
    }

    fn at(&self) -> [f32; 3] {
        if !self.running() {
            return self.to;
        }
        let share = self.elapsed / self.duration;
        let lerp = |axis: usize, t: f32| self.from[axis] + (self.to[axis] - self.from[axis]) * t;
        match self.path {
            Path::Arrive => {
                let t = cubic_out(share);
                std::array::from_fn(|axis| lerp(axis, t))
            }
            Path::Move { apex, .. } => {
                let t = cubic_in_out(share);
                let middle = 0.5 * (self.from[2] + self.to[2]);
                let lift = (apex - middle).max(0.0) * 4.0 * t * (1.0 - t);
                [lerp(0, t), lerp(1, t), lerp(2, t) + lift]
            }
        }
    }

    /// The heading a moving drone faces now, toward `face` at the end, or `None` when the glide
    /// is not a move under way.
    fn heading(&self, face: f32) -> Option<f32> {
        let Path::Move { travel, start, .. } = self.path else {
            return None;
        };
        if !self.running() {
            return None;
        }
        let left = self.duration - self.elapsed;
        Some(if self.elapsed < FACE_MS {
            start + wrap(travel - start) * ease(self.elapsed / FACE_MS)
        } else if left > FACE_MS {
            travel
        } else {
            travel + wrap(face - travel) * ease(1.0 - left / FACE_MS)
        })
    }

    fn running(&self) -> bool {
        self.elapsed < self.duration
    }
}

/// The pen line a move leaves on the plinth plane, under the path the drone flew. Each piece
/// inks in as the drone passes over it and fades out over [`TRAIL_FADE_MS`].
#[derive(Debug, Clone, PartialEq)]
struct Trail {
    agent: String,
    slot: usize,
    from: [f32; 2],
    to: [f32; 2],
    duration: f32,
    elapsed: f32,
}

impl Trail {
    /// The pieces drawn now, as their ends and their opacity.
    fn pieces(&self) -> Vec<([f32; 2], [f32; 2], f32)> {
        let point = |share: f32| {
            [
                self.from[0] + (self.to[0] - self.from[0]) * share,
                self.from[1] + (self.to[1] - self.from[1]) * share,
            ]
        };
        (0..TRAIL_PIECES)
            .filter_map(|i| {
                let (start, end) = (
                    i as f32 / TRAIL_PIECES as f32,
                    (i + 1) as f32 / TRAIL_PIECES as f32,
                );
                let passed = self.duration * cubic_in_out_inverse(end);
                let age = self.elapsed - passed;
                let alpha = TRAIL_ALPHA * (1.0 - age / TRAIL_FADE_MS);
                (age >= 0.0 && alpha > 0.0).then(|| (point(start), point(end), alpha))
            })
            .collect()
    }

    fn running(&self) -> bool {
        self.elapsed < self.duration + TRAIL_FADE_MS
    }
}

/// Milliseconds a move of `units` plan units takes.
fn move_ms(units: f32) -> f32 {
    (MOVE_BASE_MS + MOVE_PER_UNIT_MS * units).clamp(MOVE_BOUNDS_MS.0, MOVE_BOUNDS_MS.1)
}

/// A drone's turn toward its machine, from the heading it starts from along the shorter arc to the
/// face it wants.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Turn {
    /// The heading the drone faces when the turn ends, as [`heading`] gives it.
    face: f32,
    from: f32,
    sweep: f32,
    elapsed: f32,
    duration: f32,
}

impl Turn {
    /// A drone already facing `face`.
    fn settled(face: f32) -> Self {
        Self {
            face,
            from: face,
            sweep: 0.0,
            elapsed: 0.0,
            duration: 0.0,
        }
    }

    /// A turn from `from` to `face` by the shorter arc, over [`TURN_MS`].
    fn between(from: f32, face: f32) -> Self {
        Self {
            face,
            from,
            sweep: wrap(face - from),
            elapsed: 0.0,
            duration: TURN_MS,
        }
    }

    fn at(&self) -> f32 {
        let t = if self.running() {
            ease(self.elapsed / self.duration)
        } else {
            1.0
        };
        self.from + self.sweep * t
    }

    fn running(&self) -> bool {
        self.elapsed < self.duration
    }
}

/// The signed angle equal to `a` in (-PI, PI].
fn wrap(a: f32) -> f32 {
    (a + PI).rem_euclid(TAU) - PI
}

/// A body that has left its machine: it rises [`RISE`] off it over [`LEAVE_MS`] and is gone.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Leave {
    at: [f32; 3],
    slot: usize,
    heading: f32,
    elapsed: f32,
}

impl Leave {
    /// Where the body stands now: it has risen part way off its machine.
    fn now(&self) -> Leaving {
        let t = cubic_out(self.elapsed / LEAVE_MS);
        Leaving {
            at: [self.at[0], self.at[1], self.at[2] + RISE * t],
            slot: self.slot,
            heading: self.heading,
        }
    }
}

/// A machine's landed dot: how many of its tasks have landed, and for how long the dot has held
/// its word.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Landing {
    count: usize,
    elapsed: f32,
}

/// An order flag that has left the view and is fading out.
#[derive(Debug, Clone, PartialEq)]
struct Fade {
    flag: Flag,
    elapsed: f32,
}

/// A body on its way out, as the instances draw it now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Leaving {
    pub at: [f32; 3],
    pub slot: usize,
    pub heading: f32,
}

/// A pin's rise in, or its sink out once the view no longer shows it.
#[derive(Debug, Clone)]
struct Rise {
    pin: Pin,
    elapsed: f32,
    leaving: bool,
}

impl Rise {
    /// Where the diamond's foot stands: the top of the stalk, eased in or out.
    fn at(&self) -> [f32; 3] {
        let t = ease(self.elapsed / PIN_MS);
        let lift = if self.leaving { t } else { 1.0 - t };
        [
            self.pin.at[0],
            self.pin.at[1],
            self.pin.at[2] + STALK_HEIGHT - RISE * lift,
        ]
    }

    fn running(&self) -> bool {
        self.elapsed < PIN_MS
    }
}

/// The eases the marks run between the view's positions. Nothing here is read back into the view.
///
/// A still motion, from `?freeze=1`, lands every ease at once: a bot does not glide, a bot that
/// left is gone, no flag fades, and a landed dot keeps its word because its hold never runs. Under
/// reduced motion the drones jump to their places and do not bob, while flags still fade.
/// Working and idle drones bob, each state at its own height and pace.
#[derive(Debug, Default)]
pub struct Motion {
    still: bool,
    reduced: bool,
    bodies: BTreeMap<String, Glide>,
    turns: BTreeMap<String, Turn>,
    leaving: BTreeMap<String, Leave>,
    pins: BTreeMap<String, Rise>,
    landed: BTreeMap<usize, Landing>,
    fades: Vec<Fade>,
    order_flag: Option<Flag>,
    flags_changed: bool,
    /// Whether an agent is working now, so a beacon pulses. Set by [`Motion::sync`].
    pulsing: bool,
    /// Where the pulse is, in milliseconds through its cycle. It stays at zero while still.
    pulse_ms: f32,
    /// Where the working drones' bob is, in milliseconds through its period.
    bob_ms: f32,
    /// Whether an idle drone is shown, so the idle bob runs. Set by [`Motion::sync`].
    idling: bool,
    /// Where the idle drones' bob is, in milliseconds through its period.
    idle_ms: f32,
    /// Whether the last [`Motion::advance`] ran only for the idle bob.
    calm: bool,
    /// World units per CSS px at the drones, so the bob is [`BOB_PX`] on screen. Zero until the
    /// page sets it, which keeps every drone level.
    px_units: f32,
    /// The trails of recent moves, oldest first.
    trails: Vec<Trail>,
    /// Whether the next sync lands every drone at once, as after a replay seek.
    cut: bool,
    /// The ghosts shown, by plan machine.
    ghosts: BTreeMap<usize, Haunt>,
    /// The ghosts the reader cleared, by plan machine, with the landing each showed. A cleared
    /// ghost stays gone until a newer task lands there.
    cleared: BTreeMap<usize, u64>,
}

/// A ghost on show: the landing it shows, and how far its fade out has run once it clears.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Haunt {
    at: u64,
    fade: Option<f32>,
}

impl Motion {
    #[must_use]
    pub fn new(still: bool) -> Self {
        Self {
            still,
            ..Self::default()
        }
    }

    /// Holds the drones' glides and bob still under reduced motion. Flags still fade.
    pub fn set_reduced(&mut self, reduced: bool) {
        self.reduced = reduced;
    }

    /// Lands every drone at its place at the next sync, with no glide and no trail, and clears the
    /// trails there are. The page calls it when the log is replaced, as a replay seek does.
    pub fn cut(&mut self) {
        self.cut = true;
        self.trails.clear();
    }

    /// Sets how many world units a CSS px covers at the drones, which sizes the bob.
    pub fn set_px_units(&mut self, units: f32) {
        self.px_units = if units.is_finite() {
            units.max(0.0)
        } else {
            0.0
        };
    }

    /// Points each ease at the marks the view now gives. A body that arrives drops in, a body that
    /// moves arcs from where it is, a body that leaves rises off its machine, and a pin that leaves
    /// sinks out.
    pub fn sync(&mut self, marks: &Marks) {
        let jump = self.still || self.reduced || std::mem::take(&mut self.cut);
        for body in &marks.bodies {
            let top = body.target[2] - BEACON_RADIUS;
            let glide = match self.bodies.get(&body.agent) {
                Some(glide) if glide.to == body.at => {
                    self.sync_turn(body);
                    continue;
                }
                _ if jump => Glide::settled(body.at, body.slot, top),
                Some(glide) => {
                    let start = self.heading_of(&body.agent).unwrap_or(0.0);
                    Glide::moving(glide.at(), body.at, (glide.top, top), start, body.slot)
                }
                None => Glide::arrive(body.at, body.slot, top),
            };
            let moves = matches!(glide.path, Path::Move { .. });
            if moves {
                self.trails.push(Trail {
                    agent: body.agent.clone(),
                    slot: body.slot,
                    from: [glide.from[0], glide.from[1]],
                    to: [glide.to[0], glide.to[1]],
                    duration: glide.duration,
                    elapsed: 0.0,
                });
            }
            self.bodies.insert(body.agent.clone(), glide);
            if moves {
                // The move's own heading carries the turn, so the drone ends facing its target.
                let face = heading(body.at, body.target).unwrap_or(0.0);
                self.turns.insert(body.agent.clone(), Turn::settled(face));
            } else {
                self.sync_turn(body);
            }
            self.leaving.remove(&body.agent);
        }
        let live: BTreeSet<&str> = marks
            .bodies
            .iter()
            .map(|body| body.agent.as_str())
            .collect();
        let gone: Vec<(String, Glide, f32)> = self
            .bodies
            .iter()
            .filter(|(agent, _)| !live.contains(agent.as_str()))
            .map(|(agent, glide)| {
                let heading = self.turns.get(agent).map_or(0.0, Turn::at);
                (agent.clone(), *glide, heading)
            })
            .collect();
        self.bodies.retain(|agent, _| live.contains(agent.as_str()));
        self.turns.retain(|agent, _| live.contains(agent.as_str()));
        self.trails
            .retain(|trail| live.contains(trail.agent.as_str()));
        for (agent, glide, heading) in gone {
            if !self.still && !self.reduced {
                self.leaving.insert(
                    agent,
                    Leave {
                        at: glide.at(),
                        slot: glide.slot,
                        heading,
                        elapsed: 0.0,
                    },
                );
            }
        }
        self.pulsing = marks.bodies.iter().any(Body::working);
        self.idling = marks
            .bodies
            .iter()
            .any(|body| body.state == DroneState::Idle);

        for pin in &marks.pins {
            self.pins
                .entry(pin.id.clone())
                .and_modify(|rise| {
                    rise.leaving = false;
                    rise.pin = pin.clone();
                })
                .or_insert_with(|| Rise {
                    pin: pin.clone(),
                    elapsed: 0.0,
                    leaving: false,
                });
        }
        for (id, rise) in &mut self.pins {
            let shown = marks.pins.iter().any(|pin| &pin.id == id);
            if !shown && !rise.leaving {
                rise.leaving = true;
                rise.elapsed = 0.0;
            }
        }
        let still = self.still;
        self.pins
            .retain(|_, rise| !rise.leaving || (!still && rise.running()));

        self.sync_landed(marks);
        self.sync_order(marks);
        self.sync_ghosts(marks);
    }

    /// Shows each ghost the marks give that the reader has not cleared. A ghost whose landing
    /// changes shows again.
    fn sync_ghosts(&mut self, marks: &Marks) {
        let shown: BTreeMap<usize, u64> = marks
            .ghosts
            .iter()
            .map(|ghost| (ghost.machine, ghost.at))
            .collect();
        self.cleared
            .retain(|machine, at| shown.get(machine) == Some(at));
        self.ghosts
            .retain(|machine, haunt| shown.get(machine) == Some(&haunt.at));
        for (&machine, &at) in &shown {
            if !self.cleared.contains_key(&machine) {
                self.ghosts
                    .entry(machine)
                    .or_insert(Haunt { at, fade: None });
            }
        }
    }

    /// Starts the fade out of the ghost on plan machine `machine`. Returns whether one was there
    /// and had not begun to fade.
    pub fn clear_ghost(&mut self, machine: usize) -> bool {
        let Some(haunt) = self.ghosts.get_mut(&machine) else {
            return false;
        };
        if haunt.fade.is_some() {
            return false;
        }
        haunt.fade = Some(0.0);
        true
    }

    /// Starts the fade out of every ghost. Returns whether any began to fade.
    pub fn clear_ghosts(&mut self) -> bool {
        let machines: Vec<usize> = self.ghosts.keys().copied().collect();
        machines
            .into_iter()
            .fold(false, |any, machine| self.clear_ghost(machine) | any)
    }

    /// The opacity of the ghost on plan machine `machine` now, or `None` when it shows none.
    #[must_use]
    pub fn ghost_alpha(&self, machine: usize) -> Option<f32> {
        let haunt = self.ghosts.get(&machine)?;
        let left = haunt.fade.map_or(1.0, |fade| 1.0 - fade / GHOST_FADE_MS);
        (left > 0.0).then_some(GHOST_ALPHA * left)
    }

    /// Points a drone at its machine. A drone turns when the spot it faces changes, and a still
    /// motion turns it at once. A drone with no direction keeps the heading it has.
    fn sync_turn(&mut self, body: &Body) {
        let Some(face) = heading(body.at, body.target) else {
            self.turns
                .entry(body.agent.clone())
                .or_insert_with(|| Turn::settled(0.0));
            return;
        };
        let turn = match self.turns.get(&body.agent) {
            Some(turn) if turn.face == face => return,
            Some(turn) if !self.still && !self.reduced => Turn::between(turn.at(), face),
            _ => Turn::settled(face),
        };
        self.turns.insert(body.agent.clone(), turn);
    }

    /// Starts a machine's hold again when one more of its tasks lands. Machines that no longer
    /// show a landed dot lose their hold.
    fn sync_landed(&mut self, marks: &Marks) {
        let mut shown = BTreeSet::new();
        for flag in &marks.flags {
            let Follow::Landed { machine, count } = flag.follow else {
                continue;
            };
            shown.insert(machine);
            if !matches!(self.landed.get(&machine), Some(landing) if landing.count == count) {
                self.landed.insert(
                    machine,
                    Landing {
                        count,
                        elapsed: 0.0,
                    },
                );
            }
        }
        self.landed.retain(|machine, _| shown.contains(machine));
    }

    /// An order flag that the view drops fades out over [`LEAVE_MS`], so the flag stays on the
    /// plant a moment after its order has landed.
    fn sync_order(&mut self, marks: &Marks) {
        let order = marks
            .flags
            .iter()
            .find(|flag| flag.kind == FlagKind::Order)
            .cloned();
        if order.is_none()
            && let Some(previous) = self.order_flag.take()
            && !self.still
        {
            self.fades.push(Fade {
                flag: Flag {
                    leaving: true,
                    ..previous
                },
                elapsed: 0.0,
            });
        }
        self.order_flag = order;
    }

    /// Moves every ease on by `dt_ms`. Returns true while any of them is still running. A landed
    /// dot going quiet and a fade ending both change what the page lists, so they set
    /// [`Motion::take_flags_changed`].
    pub fn advance(&mut self, dt_ms: f32) -> bool {
        if self.still {
            return false;
        }
        let dt = dt_ms.max(0.0);
        for glide in self.bodies.values_mut() {
            glide.elapsed += dt;
        }
        for leave in self.leaving.values_mut() {
            leave.elapsed += dt;
        }
        for trail in &mut self.trails {
            trail.elapsed += dt;
        }
        self.trails.retain(Trail::running);
        self.leaving.retain(|_, leave| leave.elapsed < LEAVE_MS);
        for rise in self.pins.values_mut() {
            rise.elapsed += dt;
        }
        for landing in self.landed.values_mut() {
            let holding = landing.elapsed < LANDED_HOLD_MS;
            landing.elapsed += dt;
            if holding && landing.elapsed >= LANDED_HOLD_MS {
                self.flags_changed = true;
            }
        }
        for fade in &mut self.fades {
            fade.elapsed += dt;
        }
        let fades_before = self.fades.len();
        self.fades.retain(|fade| fade.elapsed < LEAVE_MS);
        self.flags_changed |= self.fades.len() != fades_before;
        for turn in self.turns.values_mut().filter(|turn| turn.running()) {
            turn.elapsed += dt;
        }
        if self.pulsing {
            self.pulse_ms = (self.pulse_ms + dt) % PULSE_MS;
            if !self.reduced {
                self.bob_ms = (self.bob_ms + dt) % BOB_PERIOD_MS;
            }
        }
        let idle_bob = self.idling && !self.reduced;
        if idle_bob {
            self.idle_ms = (self.idle_ms + dt) % IDLE_BOB_PERIOD_MS;
        }
        let bodies = self.bodies.values().any(Glide::running);
        let turning = self.turns.values().any(Turn::running);
        let pins = self.pins.values().any(Rise::running);
        let leaving = !self.leaving.is_empty();
        let holding = self
            .landed
            .values()
            .any(|landing| landing.elapsed < LANDED_HOLD_MS);
        let fading = !self.fades.is_empty();
        let trailing = !self.trails.is_empty();
        let ghosting = self.advance_ghosts(dt);
        let busy = bodies
            || turning
            || pins
            || leaving
            || holding
            || fading
            || trailing
            || ghosting
            || self.pulsing;
        self.calm = idle_bob && !busy;
        busy || idle_bob
    }

    /// Whether the last [`Motion::advance`] ran only for the idle bob. The bob then moves a small
    /// fraction of a pixel a frame, so the page may draw it at a lower frame rate.
    #[must_use]
    pub fn calm(&self) -> bool {
        self.calm
    }

    /// Moves each ghost's fade on, and drops the ghosts that have faded. Returns whether any is
    /// still fading.
    fn advance_ghosts(&mut self, dt: f32) -> bool {
        let mut gone = Vec::new();
        for (&machine, haunt) in &mut self.ghosts {
            if let Some(fade) = haunt.fade.as_mut() {
                *fade += dt;
                if *fade >= GHOST_FADE_MS {
                    gone.push((machine, haunt.at));
                }
            }
        }
        for (machine, at) in gone {
            self.ghosts.remove(&machine);
            self.cleared.insert(machine, at);
        }
        self.ghosts.values().any(|haunt| haunt.fade.is_some())
    }

    /// How far a working agent's beacon has swollen now, from 0 at rest to 1 at the top of a pulse.
    #[must_use]
    pub fn pulse(&self) -> f32 {
        0.5 - 0.5 * (TAU * self.pulse_ms / PULSE_MS).cos()
    }

    /// How far a drone in palette slot `slot` and state `state` has bobbed now, in world units.
    /// Each slot starts at its own phase, so the drones on one machine do not bob together.
    #[must_use]
    pub fn bob(&self, slot: usize, state: DroneState) -> f32 {
        let Some((px, period)) = state.bob() else {
            return 0.0;
        };
        if self.still || self.reduced {
            return 0.0;
        }
        let ms = if matches!(state, DroneState::Working | DroneState::Drifting) {
            self.bob_ms
        } else {
            self.idle_ms
        };
        let phase = ms / period + slot as f32 / AGENT_SLOTS as f32;
        0.5 * px * self.px_units * (TAU * phase).sin()
    }

    /// The heading a drone faces now, in radians from +x. A drone on the move faces the way it
    /// travels and turns to its target in the move's last [`FACE_MS`].
    #[must_use]
    pub fn heading_of(&self, agent: &str) -> Option<f32> {
        let turn = self.turns.get(agent).map(Turn::at);
        self.bodies
            .get(agent)
            .and_then(|glide| glide.heading(turn.unwrap_or(0.0)))
            .or(turn)
    }

    /// Whether a flag listing changed while time passed, such as a landed dot going quiet. Reading
    /// it clears it.
    pub fn take_flags_changed(&mut self) -> bool {
        std::mem::take(&mut self.flags_changed)
    }

    /// Whether a machine's landed dot has held its word for [`LANDED_HOLD_MS`]. A still motion
    /// never lets the hold run out, so its dots keep their words.
    #[must_use]
    pub fn landed_quiet(&self, machine: usize) -> bool {
        !self.still
            && self
                .landed
                .get(&machine)
                .is_some_and(|landing| landing.elapsed >= LANDED_HOLD_MS)
    }

    /// The flags as the page lists them now: the view's flags, with each landed dot quiet once
    /// its hold has run out, and the order flags still fading out at the end.
    #[must_use]
    pub fn flags_now(&self, flags: &[Flag]) -> Vec<Flag> {
        let mut out: Vec<Flag> = flags
            .iter()
            .map(|flag| match flag.follow {
                Follow::Landed { machine, .. } if self.landed_quiet(machine) => Flag {
                    text: String::new(),
                    ..flag.clone()
                },
                _ => flag.clone(),
            })
            .collect();
        out.extend(self.fades.iter().map(|fade| fade.flag.clone()));
        out
    }

    /// The bodies that are leaving now. Each sinks into its plinth over [`LEAVE_MS`].
    #[must_use]
    pub fn leaving_now(&self) -> Vec<Leaving> {
        self.leaving.values().map(Leave::now).collect()
    }

    /// Whether an agent's drone is still easing off its machine after it left the view.
    #[must_use]
    pub fn is_leaving(&self, agent: &str) -> bool {
        self.leaving.contains_key(agent)
    }

    /// Where an agent's body stands now.
    #[must_use]
    pub fn body_at(&self, agent: &str) -> Option<[f32; 3]> {
        self.bodies.get(agent).map(Glide::at)
    }

    /// Where a clash's diamond stands now: its foot, on the top of the stalk. Pins that are
    /// sinking out are included until they are gone.
    #[must_use]
    pub fn pin_at(&self, clash: &str) -> Option<[f32; 3]> {
        self.pins.get(clash).map(Rise::at)
    }

    /// Where a flag's anchor stands now. A flag that follows a body or a diamond rides it.
    #[must_use]
    pub fn flag_at(&self, flag: &Flag) -> [f32; 3] {
        match &flag.follow {
            Follow::Still | Follow::Landed { .. } => flag.at,
            Follow::Body { agent, slot, state } => self.body_at(agent).map_or(flag.at, |at| {
                [at[0], at[1], at[2] + self.bob(*slot, *state)]
            }),
            Follow::Pin(clash) => self.pin_at(clash).unwrap_or(flag.at),
        }
    }

    /// Every pin on the plant now, as its stalk's foot and its diamond's foot. A pin that has
    /// sunk out is gone.
    #[must_use]
    pub fn pins_now(&self) -> Vec<([f32; 3], [f32; 3])> {
        self.pins
            .values()
            .filter(|rise| !rise.leaving || rise.running())
            .map(|rise| (rise.pin.at, rise.at()))
            .collect()
    }
}

/// The model kinds of the marks, after the machine kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkKinds {
    /// A drone's white body.
    pub body: u16,
    /// A drone's iris, lens ring, ear hubs and antenna tip, in its state's tint.
    pub accent: u16,
    pub ring: u16,
    pub pin: u16,
    pub stalk: u16,
    pub beacon: u16,
}

/// A drone's instances: its white body and its accent at its centre, the accent in `accent`,
/// which [`eye_tint`] gives for the drone's state. Its shadow is [`spots`].
fn drone(kinds: MarkKinds, at: [f32; 3], accent: u8, turn: f32, bot: [f32; 2]) -> [Instance; 2] {
    [
        Instance {
            offset: at,
            kind: kinds.body,
            tint: 0,
            grow: bot,
            turn,
        },
        Instance {
            offset: at,
            kind: kinds.accent,
            tint: accent,
            grow: bot,
            turn,
        },
    ]
}

/// The screen floor a drone grows to, in device px, as the grow factor of its model: its height in
/// plan units, and the floor `dpr` turns from CSS px into device px.
fn drone_grow(dpr: f32) -> [f32; 2] {
    [BOT_MIN_PX * dpr, BOT_HEIGHT]
}

/// The instances of the marks now. They are every drone, the ring round each waiting drone at its
/// body's height, each drone that is leaving, each working drone's beacon at its target, and each
/// pin's stalk and diamond. A drone, its ring, a beacon and a diamond grow to their screen floor,
/// which `dpr` turns from CSS px into device px. A stalk keeps its plan size. A working agent's
/// beacon swells with its pulse. A drone's accent takes its state's tint. The riding agent's drone
/// has no body or accent, but its ring and beacon still draw.
#[must_use]
pub fn mark_instances(
    motion: &Motion,
    marks: &Marks,
    kinds: MarkKinds,
    dpr: f32,
    riding: Option<&str>,
) -> Vec<Instance> {
    let bot = drone_grow(dpr);
    let diamond = [PIN_MIN_PX * dpr, PIN_HEIGHT];
    let mut out = Vec::new();
    for body in &marks.bodies {
        let Some(at) = motion.body_at(&body.agent) else {
            continue;
        };
        let turn = motion.heading_of(&body.agent).unwrap_or(0.0);
        let at = bobbed(motion, body, at);
        if riding != Some(body.agent.as_str()) {
            out.extend(drone(kinds, at, eye_tint(body.state, body.slot), turn, bot));
        }
        if body.waiting() {
            out.push(Instance {
                offset: at,
                kind: kinds.ring,
                tint: 0,
                grow: bot,
                turn: 0.0,
            });
        }
        if !body.working() {
            continue;
        }
        let swell = 1.0 + BEACON_SWING * motion.pulse();
        out.push(Instance {
            offset: body.target,
            kind: kinds.beacon,
            tint: body_tint(body.slot),
            grow: [BEACON_PX * dpr * swell, 2.0 * BEACON_RADIUS],
            turn: 0.0,
        });
    }
    for (agent, leave) in &motion.leaving {
        if riding == Some(agent.as_str()) {
            continue;
        }
        let leaving = leave.now();
        let accent = eye_tint(DroneState::Idle, leaving.slot);
        out.extend(drone(kinds, leaving.at, accent, leaving.heading, bot));
    }
    for (foot, head) in motion.pins_now() {
        out.push(Instance {
            offset: foot,
            kind: kinds.stalk,
            tint: 0,
            grow: [0.0; 2],
            turn: 0.0,
        });
        out.push(Instance {
            offset: head,
            kind: kinds.pin,
            tint: 0,
            grow: diamond,
            turn: 0.0,
        });
    }
    out
}

/// A drone's centre lifted by its bob, which is zero for a drone that holds still.
pub fn bobbed(motion: &Motion, body: &Body, at: [f32; 3]) -> [f32; 3] {
    [at[0], at[1], at[2] + motion.bob(body.slot, body.state)]
}

/// The beams now run from each drone's eye to its target. The eye is on the nose, looking a little
/// down, in the drone's own frame, so the beam turns and grows with the drone and starts on the eye
/// at any size. A drone
/// that is leaving has none, and neither has the riding agent's drone. The hovered drone's tether
/// inks in fully.
#[must_use]
pub fn beams(
    motion: &Motion,
    marks: &Marks,
    dpr: f32,
    riding: Option<&str>,
    hovered: Option<&str>,
) -> Vec<Beam> {
    marks
        .bodies
        .iter()
        .filter(|body| riding != Some(body.agent.as_str()))
        .filter_map(|body| {
            let at = bobbed(motion, body, motion.body_at(&body.agent)?);
            let facing = motion.heading_of(&body.agent)?;
            Some(Beam {
                from: eye_point(),
                to: body.target,
                origin: at,
                grow: drone_grow(dpr),
                turn: facing,
                tint: body_tint(body.slot),
                alpha: if hovered == Some(body.agent.as_str()) {
                    1.0
                } else {
                    body.state.tether_alpha()
                },
                width_px: TETHER_PX * dpr,
                spot: 0.0,
            })
        })
        .collect()
}

/// The shadows now, each a soft level disc on the surface straight under its drone, so it is the
/// drone's vertical projection from any camera angle. It lies on the ground, or the machine's top,
/// beneath the drone's centre, shrinks and thins as the drone rises, and grows with the drone. A
/// leaving drone casts one as an idle one does, and the riding agent's drone casts none.
#[must_use]
pub fn spots(motion: &Motion, marks: &Marks, dpr: f32, riding: Option<&str>) -> Vec<Beam> {
    let shadow = |at: [f32; 3], ground: f32| {
        let (radius, strength) = shadow_at(at[2] - ground);
        Beam {
            from: [0.0; 3],
            to: [at[0], at[1], ground],
            origin: [at[0], at[1], ground],
            grow: drone_grow(dpr),
            turn: 0.0,
            tint: u8::try_from(TINT_SHADOW).expect("tint slots fit in a byte"),
            alpha: SHADOW_MIN_ALPHA.max(SHADOW_ALPHA * strength),
            width_px: 0.0,
            spot: radius,
        }
    };
    let bodies = marks
        .bodies
        .iter()
        .filter(|body| riding != Some(body.agent.as_str()))
        .filter_map(|body| {
            let at = bobbed(motion, body, motion.body_at(&body.agent)?);
            Some((at, body.ground))
        });
    let leaving = motion
        .leaving
        .iter()
        .filter(|(agent, _)| riding != Some(agent.as_str()))
        .map(|(_, leave)| (leave.now().at, GROUND_Z));
    bodies
        .chain(leaving)
        .map(|(at, ground)| shadow(at, ground))
        .collect()
}

/// The trails now, as fine pen lines in each agent's hue on the plinth plane under the paths the
/// drones flew. The riding agent leaves none, since the ride's camera flies the path itself.
#[must_use]
pub fn trail_beams(motion: &Motion, dpr: f32, riding: Option<&str>) -> Vec<Beam> {
    motion
        .trails
        .iter()
        .filter(|trail| riding != Some(trail.agent.as_str()))
        .flat_map(|trail| {
            let tint = body_tint(trail.slot);
            trail.pieces().into_iter().map(move |(a, b, alpha)| Beam {
                from: [a[0], a[1], PLINTH_TOP],
                to: [b[0], b[1], PLINTH_TOP],
                origin: [0.0; 3],
                grow: [0.0; 2],
                turn: 0.0,
                tint,
                alpha,
                width_px: TRAIL_PX * dpr,
                spot: 0.0,
            })
        })
        .collect()
}

/// The ghosts now, each a grey loop of four strokes round its machine's footprint on the ground.
#[must_use]
pub fn ghost_beams(motion: &Motion, marks: &Marks, dpr: f32) -> Vec<Beam> {
    let tint = u8::try_from(TINT_GHOST).expect("tint slots fit in a byte");
    marks
        .ghosts
        .iter()
        .filter_map(|ghost| Some((ghost.corners(), motion.ghost_alpha(ghost.machine)?)))
        .flat_map(|(corners, alpha)| {
            (0..4).map(move |i| Beam {
                from: corners[i],
                to: corners[(i + 1) % 4],
                origin: [0.0; 3],
                grow: [0.0; 2],
                turn: 0.0,
                tint,
                alpha,
                width_px: GHOST_PX * dpr,
                spot: 0.0,
            })
        })
        .collect()
}

/// Where a flag anchors on screen, in CSS px, given its point `at` in world units and how the
/// camera projects a world point to the screen. A drone's flag anchors above its crown, and the
/// page stacks the flags of one machine above it with a leader. A clash flag anchors to the right
/// of its diamond, and a landed dot to the right of its machine's point, clear of the order flag
/// above the same machine. Other flags anchor at their own point.
#[must_use]
pub fn flag_anchor(flag: &Flag, at: [f32; 3], project: impl Fn([f32; 3]) -> [f32; 2]) -> [f32; 2] {
    match flag.kind {
        FlagKind::Agent => {
            let (centre, height) = visual_extent(at, BOT_HEIGHT, BOT_MIN_PX, &project);
            [centre[0], centre[1] - height * 0.5 - FLAG_GAP_PX]
        }
        FlagKind::Clash => {
            let [_, right] = badge_centres(at, &project);
            [right[0] + BADGE_PX * 0.5 + FLAG_GAP_PX, right[1]]
        }
        FlagKind::Landed => {
            let [x, y] = project(at);
            [x + FLAG_GAP_PX, y]
        }
        FlagKind::Away => project(at),
        FlagKind::Order | FlagKind::Question | FlagKind::Plot | FlagKind::Stage => project(at),
    }
}

/// How far past a drone's disc the pointer still hovers it, in CSS px.
pub const HOVER_PAD_PX: f32 = 6.0;

/// The index of the disc the CSS point `p` hovers: within its radius and [`HOVER_PAD_PX`], and
/// the nearest centre when two reach. Each disc is its centre and radius, as [`drone_disc`] gives.
#[must_use]
pub fn disc_under(discs: &[([f32; 2], f32)], p: [f32; 2]) -> Option<usize> {
    discs
        .iter()
        .enumerate()
        .map(|(i, (centre, radius))| (i, distance(*centre, p), *radius))
        .filter(|&(_, gap, radius)| gap <= radius + HOVER_PAD_PX)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _, _)| i)
}

/// Each working drone's beacon on screen, as its centre in CSS px and its radius there, at the
/// size the renderer draws it now, pulse included. The page rings each in paper, so the hue dot
/// still reads on a grey machine top.
#[must_use]
pub fn beacon_discs(
    motion: &Motion,
    marks: &Marks,
    project: impl Fn([f32; 3]) -> [f32; 2],
) -> Vec<([f32; 2], f32)> {
    let swell = 1.0 + BEACON_SWING * motion.pulse();
    marks
        .bodies
        .iter()
        .filter(|body| body.working() && motion.body_at(&body.agent).is_some())
        .map(|body| {
            let (centre, height) = visual_extent(
                body.target,
                2.0 * BEACON_RADIUS,
                BEACON_PX * swell,
                &project,
            );
            (centre, height * 0.5)
        })
        .collect()
}
/// A drone's centre on screen, in CSS px, and its radius there. The radius is half the drone's
/// screen height, which the screen floor sets when the drone is small.
#[must_use]
pub fn drone_disc(at: [f32; 3], project: impl Fn([f32; 3]) -> [f32; 2]) -> ([f32; 2], f32) {
    let (centre, height) = visual_extent(at, BOT_HEIGHT, BOT_MIN_PX, &project);
    (centre, height * 0.5)
}

/// Where a diamond's centre is on screen, in CSS px, given its foot in world units.
#[must_use]
pub fn diamond_centre(foot: [f32; 3], project: impl Fn([f32; 3]) -> [f32; 2]) -> [f32; 2] {
    let (base, height) = visual_extent(foot, PIN_HEIGHT, PIN_MIN_PX, &project);
    [base[0], base[1] - height * 0.5]
}

/// A model's anchor on screen, and its height on screen in CSS px: its plan height at the anchor's
/// scale, or the floor when that is taller, as the renderer draws it. A drone's anchor is its centre.
fn visual_extent(
    at: [f32; 3],
    plan_height: f32,
    floor_px: f32,
    project: &impl Fn([f32; 3]) -> [f32; 2],
) -> ([f32; 2], f32) {
    let base = project(at);
    let up = project([at[0], at[1], at[2] + 1.0]);
    let per_unit = (base[1] - up[1]).abs();
    (base, (plan_height * per_unit).max(floor_px))
}

fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn cubic_out(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

/// The share of a move's time at which [`cubic_in_out`] reaches `y`.
fn cubic_in_out_inverse(y: f32) -> f32 {
    let y = y.clamp(0.0, 1.0);
    if y < 0.5 {
        (y / 4.0).cbrt()
    } else {
        1.0 - (2.0 * (1.0 - y)).cbrt() / 2.0
    }
}

fn cubic_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let u = -2.0 * t + 2.0;
        1.0 - u * u * u / 2.0
    }
}

fn is_open(clash: &ClashView) -> bool {
    matches!(clash.state, "open" | "proposed")
}

/// The agents that wait on the user: their task is in an open clash, or their order asks a
/// question, has its repair budget spent, or has an open contest.
fn waiting_agents(view: &View) -> BTreeSet<String> {
    let open_tasks: Vec<TaskId> = view
        .clashes
        .iter()
        .filter(|clash| is_open(clash))
        .flat_map(|clash| [clash.a, clash.b])
        .collect();
    let asked: Vec<OrderId> = view
        .questions
        .iter()
        .map(|question| question.order)
        .collect();
    let held: Vec<OrderId> = view
        .lanes
        .iter()
        .filter(|lane| lane.stopped || lane.open_contests > 0)
        .map(|lane| lane.order)
        .collect();
    view.agents
        .iter()
        .filter(|agent| {
            agent.task.is_some_and(|task| {
                open_tasks.contains(&task)
                    || view
                        .tasks
                        .iter()
                        .find(|candidate| candidate.id == task)
                        .is_some_and(|candidate| {
                            asked.contains(&candidate.order) || held.contains(&candidate.order)
                        })
            })
        })
        .map(|agent| agent.id.to_string())
        .collect()
}

/// Places every drone over a plinth spot of its machine, and every diamond on its stalk.
fn place(
    view: &View,
    plan: &Plan,
    stands: &[Stand],
    slots: &AgentSlots,
    waiting: &BTreeSet<String>,
    clock: Clock,
) -> (Vec<Body>, Vec<Pin>) {
    let homes: Vec<Option<usize>> = view
        .agents
        .iter()
        .map(|agent| {
            agent
                .machine
                .as_deref()
                .and_then(|name| plan.index_of(name))
        })
        .collect();
    let mut seated: BTreeMap<usize, Vec<[f32; 2]>> = BTreeMap::new();
    let mut bodies = Vec::new();
    for (agent, home) in view.agents.iter().zip(homes) {
        let Some(machine) = home else {
            continue;
        };
        if away_for(agent, waiting, clock).is_some() {
            continue;
        }
        let centre = plan.machines[machine].center();
        let top = stands.get(machine).map_or(PLINTH_TOP, |stand| stand.top);
        let spots: &[[f32; 2]] = stands
            .get(machine)
            .map_or(&[][..], |stand| stand.spots.as_slice());
        let seats = seated.entry(machine).or_default();
        let spot = seat(spots, centre, seats);
        seats.push(spot);
        // The drone hovers a gap above the first surface straight under its seat: the ground
        // beside its machine, or the machine's top when the seat lies over it.
        let ground = surface_below(spot, plan, stands);
        let z = hover_height(ground);
        let id = agent.id.to_string();
        let state = DroneState::of(
            agent.activity,
            waiting.contains(&id),
            stale(agent, clock),
            agent.drift != Drift::None,
        );
        let z = seat_height(z, state);
        let check = agent
            .task
            .and_then(|task| view.tasks.iter().find(|candidate| candidate.id == task))
            .and_then(|candidate| candidate.check.clone());
        bodies.push(Body {
            slot: slots.slot(&id),
            state,
            agent: id,
            machine,
            at: [spot[0], spot[1], z],
            ground,
            target: [centre[0], centre[1], top + BEACON_RADIUS],
            activity: agent.activity,
            check,
        });
    }
    (bodies, pins(view, plan, stands))
}

/// The machines where a task landed while the reader was away, latest first and at most
/// [`GHOST_MAX`]. Only a live page that knows when it was last seen has any.
fn ghosts(view: &View, plan: &Plan, clock: Clock) -> Vec<Ghost> {
    let (Some(_), Some(away)) = (clock.now, clock.away) else {
        return Vec::new();
    };
    let while_away = |at: &&u64| **at > away.seen && **at <= away.back;
    let mut ghosts: Vec<Ghost> = plan
        .machines
        .iter()
        .zip(&view.machines)
        .enumerate()
        .filter_map(|(machine, (plan_machine, seen_machine))| {
            let count = seen_machine.landings.iter().filter(while_away).count();
            let at = *seen_machine.landings.iter().rfind(while_away)?;
            let [x, y] = plan_machine.min;
            let [w, h] = plan_machine.size;
            (count > 0).then_some(Ghost {
                machine,
                count,
                at,
                rect: [
                    x - GHOST_OFFSET,
                    y - GHOST_OFFSET,
                    w + 2.0 * GHOST_OFFSET,
                    h + 2.0 * GHOST_OFFSET,
                ],
            })
        })
        .collect();
    ghosts.sort_by(|a, b| b.at.cmp(&a.at).then(a.machine.cmp(&b.machine)));
    ghosts.truncate(GHOST_MAX);
    ghosts
}

/// Whether the screen point `p` lies inside the polygon `quad`, by the even-odd rule.
#[must_use]
pub fn inside_quad(quad: &[[f32; 2]], p: [f32; 2]) -> bool {
    let mut inside = false;
    let mut j = quad.len().wrapping_sub(1);
    for (i, &[xi, yi]) in quad.iter().enumerate() {
        let [xj, yj] = quad[j];
        if (yi > p[1]) != (yj > p[1]) && p[0] < (xj - xi) * (p[1] - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Whether an agent's drone reads stale: it holds a task and has been quiet for [`STALE_MS`],
/// but not yet for [`AWAY_MS`].
fn stale(agent: &AgentView, clock: Clock) -> bool {
    holds_task(agent)
        && clock
            .quiet(agent)
            .is_some_and(|quiet| (STALE_MS..AWAY_MS).contains(&quiet))
}

/// How long an agent that holds a task has been quiet, once that is [`AWAY_MS`] or more. An
/// agent that waits on the user is quiet because of the user, so it keeps its drone and ring.
fn away_for(agent: &AgentView, waiting: &BTreeSet<String>, clock: Clock) -> Option<u64> {
    let quiet = clock.quiet(agent)?;
    let away = holds_task(agent) && quiet >= AWAY_MS && !waiting.contains(&agent.id.to_string());
    away.then_some(quiet)
}

fn holds_task(agent: &AgentView) -> bool {
    agent.task.is_some() && agent.activity != "readback"
}

/// How long an away dot says its agent has been quiet: whole minutes, then whole hours from an
/// hour on, such as `12m` or `2h`.
#[must_use]
pub fn away_stamp(quiet_ms: u64) -> String {
    let minutes = quiet_ms / 60_000;
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h", minutes / 60)
    }
}

/// One away dot for each agent on a machine that has been quiet for [`AWAY_MS`] or more. It
/// stands on the plinth's back corner, opposite the landed dot.
fn away_flags(
    view: &View,
    plan: &Plan,
    slots: &AgentSlots,
    waiting: &BTreeSet<String>,
    clock: Clock,
) -> Vec<Flag> {
    view.agents
        .iter()
        .filter_map(|agent| {
            let quiet = away_for(agent, waiting, clock)?;
            let machine = plan.index_of(agent.machine.as_deref()?)?;
            let footprint = &plan.machines[machine];
            let id = agent.id.to_string();
            Some(Flag {
                kind: FlagKind::Away,
                text: away_stamp(quiet),
                hue: Some(hue_css(slots.slot(&id))),
                waiting: false,
                clash: None,
                check: None,
                leaving: false,
                detail: Vec::new(),
                scope: None,
                machine: None,
                at: [
                    footprint.min[0] + footprint.size[0] - LANDED_INSET,
                    footprint.min[1] + footprint.size[1] - LANDED_INSET,
                    PLINTH_TOP,
                ],
                drone: None,
                on: None,
                follow: Follow::Still,
            })
        })
        .collect()
}

/// The height a drone's centre takes over its seat: a stale drone sags below it.
fn seat_height(z: f32, state: DroneState) -> f32 {
    if state == DroneState::Stale {
        z - STALE_DROP
    } else {
        z
    }
}

/// The spot an agent takes on a machine: the first free spot a seat's step from the agents
/// already seated there, or else the spot farthest from them. A machine with no free spot seats
/// its agent at the machine's centre.
fn seat(spots: &[[f32; 2]], centre: [f32; 2], taken: &[[f32; 2]]) -> [f32; 2] {
    let apart = |spot: &[f32; 2]| {
        taken
            .iter()
            .map(|other| distance(*other, *spot))
            .fold(f32::INFINITY, f32::min)
    };
    spots
        .iter()
        .copied()
        .find(|spot| apart(spot) >= SEAT_STEP)
        .or_else(|| {
            spots
                .iter()
                .copied()
                .max_by(|a, b| apart(a).total_cmp(&apart(b)))
        })
        .unwrap_or(centre)
}

fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn pins(view: &View, plan: &Plan, stands: &[Stand]) -> Vec<Pin> {
    view.clashes
        .iter()
        .enumerate()
        .filter(|(_, clash)| is_open(clash))
        .filter_map(|(index, clash)| {
            let machine = nearest(plan, clash.at?)?;
            let centre = plan.machines[machine].center();
            let top = stands.get(machine).map_or(PLINTH_TOP, |stand| stand.top);
            Some(Pin {
                id: clash.id.to_string(),
                clash: index,
                machine,
                at: [centre[0], centre[1], top],
            })
        })
        .collect()
}

fn flags(
    view: &View,
    plan: &Plan,
    stands: &[Stand],
    bodies: &[Body],
    pins: &[Pin],
    clock: Clock,
) -> Vec<Flag> {
    let mut out = Vec::new();
    for body in bodies {
        out.push(agent_flag(view, body, clock));
    }
    out.extend(landed_flags(view, plan));
    // One flag for the order in progress. The page shows it only while its machine is hovered.
    let active = view
        .orders
        .iter()
        .find(|order| !matches!(order.phase, "landed" | "cancelled"));
    if let Some(order) = active
        && let Some(machine) = plan.index_of(&order.machine)
    {
        let centre = plan.machines[machine].center();
        let top = stands.get(machine).map_or(PLINTH_TOP, |stand| stand.top);
        out.push(Flag {
            kind: FlagKind::Order,
            text: format!(
                "{} · {} · {}",
                order.id,
                short_name(&order.machine),
                order.phase.replace('_', " ")
            ),
            hue: None,
            waiting: false,
            clash: None,
            check: None,
            leaving: false,
            detail: Vec::new(),
            scope: None,
            machine: None,
            at: [centre[0], centre[1], top + ORDER_LIFT],
            drone: None,
            on: Some(plan.machines[machine].name.clone()),
            follow: Follow::Still,
        });
    }
    for pin in pins {
        out.push(Flag {
            kind: FlagKind::Clash,
            text: view.clashes[pin.clash].label.clone(),
            hue: None,
            waiting: false,
            clash: Some(pin.clash),
            check: None,
            leaving: false,
            detail: clash_agents(view, &view.clashes[pin.clash]),
            scope: view.clashes[pin.clash].scope.clone(),
            machine: None,
            at: [pin.at[0], pin.at[1], pin.at[2] + STALK_HEIGHT],
            drone: None,
            on: Some(plan.machines[pin.machine].name.clone()),
            follow: Follow::Pin(pin.id.clone()),
        });
    }
    for question in &view.questions {
        let Some(at) = question.at else {
            continue;
        };
        out.push(Flag {
            kind: FlagKind::Question,
            text: question.text.clone(),
            hue: None,
            waiting: true,
            clash: None,
            check: None,
            leaving: false,
            detail: Vec::new(),
            scope: None,
            machine: None,
            at: [at[0], at[1], PLINTH_TOP + ORDER_LIFT],
            drone: None,
            on: None,
            follow: Follow::Still,
        });
    }
    out
}

/// The two agents of a clash, task `a`'s first, each as `agent-a · t-0`. A task no agent holds
/// reads `unclaimed · t-1`.
fn clash_agents(view: &View, clash: &ClashView) -> Vec<String> {
    [clash.a, clash.b]
        .into_iter()
        .map(|task| {
            let holder = view
                .tasks
                .iter()
                .find(|candidate| candidate.id == task)
                .and_then(|candidate| candidate.agent.as_ref())
                .map_or_else(|| "unclaimed".to_owned(), ToString::to_string);
            format!("{holder} · {task}")
        })
        .collect()
}

/// The flag of one lane's slice, above its machine. Alt lists its plots and keeps.
fn plot_flag(slice: &Slice, plan: &Plan, stands: &[Stand]) -> Flag {
    let centre = plan.machines[slice.machine].center();
    let top = stands
        .get(slice.machine)
        .map_or(PLINTH_TOP, |stand| stand.top);
    Flag {
        kind: FlagKind::Plot,
        text: slice.text(),
        hue: None,
        waiting: false,
        clash: None,
        check: None,
        leaving: false,
        detail: slice.detail(),
        scope: None,
        machine: None,
        at: [centre[0], centre[1], top + ORDER_LIFT],
        drone: None,
        on: Some(plan.machines[slice.machine].name.clone()),
        follow: Follow::Still,
    }
}

/// An agent's flag: its id cut to [`FLAG_ID_CHARS`], with its activity and task title as the
/// detail that hover or Alt adds. A stale agent's flag says how long it has been quiet.
fn agent_flag(view: &View, body: &Body, clock: Clock) -> Flag {
    let agent = view
        .agents
        .iter()
        .find(|agent| agent.id.to_string() == body.agent);
    let title = agent
        .and_then(|agent| agent.task)
        .and_then(|task| view.tasks.iter().find(|candidate| candidate.id == task))
        .map(|task| task.title.clone());
    let quiet = match (body.state, agent.map(|agent| agent.drift)) {
        (DroneState::Stale, _) => agent.and_then(|agent| clock.quiet(agent)).map(away_stamp),
        (DroneState::Drifting, Some(Drift::Stuck { quiet_ms })) => {
            Some(format!("drift · quiet {}", away_stamp(quiet_ms)))
        }
        (DroneState::Drifting, _) => Some("drift · beyond scope".to_owned()),
        _ => None,
    };
    Flag {
        kind: FlagKind::Agent,
        text: body.agent.chars().take(FLAG_ID_CHARS).collect(),
        hue: Some(hue_css(body.slot)),
        waiting: body.waiting(),
        clash: None,
        check: body.check.clone(),
        leaving: false,
        detail: std::iter::once(body.activity.to_owned())
            .chain(title)
            .collect(),
        scope: None,
        drone: Some(DroneFlag {
            agent: body.agent.clone(),
            state: body.state.word(),
            quiet,
        }),
        machine: Some(body.machine),
        at: body.at,
        on: None,
        follow: Follow::Body {
            agent: body.agent.clone(),
            slot: body.slot,
            state: body.state,
        },
    }
}

/// One landed dot per machine that has a landed task. Its text is `landed` and the change word of
/// the last task that landed there. The page keeps the word for [`LANDED_HOLD_MS`], then the dot
/// goes quiet with no text. Every dot stands at the same corner of its machine's plinth.
fn landed_flags(view: &View, plan: &Plan) -> Vec<Flag> {
    let mut per_machine: BTreeMap<usize, (usize, String)> = BTreeMap::new();
    for task in view.tasks.iter().filter(|task| task.state == "landed") {
        let Some(machine) = task.machine.as_deref().and_then(|name| plan.index_of(name)) else {
            continue;
        };
        let word = task.change.clone().unwrap_or_else(|| task.title.clone());
        let entry = per_machine.entry(machine).or_insert((0, String::new()));
        entry.0 += 1;
        entry.1 = word;
    }
    per_machine
        .into_iter()
        .map(|(machine, (count, word))| {
            let [x, y] = plan.machines[machine].min;
            Flag {
                kind: FlagKind::Landed,
                text: format!("landed · {word}"),
                hue: None,
                waiting: false,
                clash: None,
                check: None,
                leaving: false,
                detail: Vec::new(),
                scope: None,
                machine: None,
                at: [x + LANDED_INSET, y + LANDED_INSET, PLINTH_TOP],
                drone: None,
                on: None,
                follow: Follow::Landed { machine, count },
            }
        })
        .collect()
}

fn declaration(view: &View, task: TaskId, slots: &AgentSlots) -> CardRow {
    let found = view.tasks.iter().find(|candidate| candidate.id == task);
    let title = found.map_or_else(|| task.to_string(), |candidate| candidate.title.clone());
    match found.and_then(|candidate| candidate.agent.clone()) {
        Some(agent) => {
            let id = agent.to_string();
            CardRow::Declaration {
                hue: hue_css(slots.slot(&id)),
                agent: id,
                text: format!("{task} · {title}"),
            }
        }
        None => CardRow::Declaration {
            agent: "unclaimed".to_owned(),
            hue: UNCLAIMED.to_owned(),
            text: format!("{task} · {title}"),
        },
    }
}

fn nearest(plan: &Plan, at: [f32; 2]) -> Option<usize> {
    let distance = |i: usize| {
        let c = plan.machines[i].center();
        (c[0] - at[0]).powi(2) + (c[1] - at[1]).powi(2)
    };
    (0..plan.machines.len()).min_by(|&a, &b| distance(a).total_cmp(&distance(b)))
}

pub fn short_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::str::FromStr;

    use acan_core::{
        Belt, CodeIndex, CodeRev, Command, Ctx, Event, FilePath, HumanCmd, IndexedFile,
        IndexedSymbol, MachineKey, MemberId, Origin, Stamped, Timestamp, WorldId, WorldRef,
    };
    use acan_intent::Scope;
    use serde_json::json;

    use super::*;
    use crate::lanes::LaneView;
    use crate::mirror::Mirror;
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

    fn plan() -> Plan {
        let plant = from_scan_json(PLANT).expect("the plant parses");
        Plan::new(&plant, |name| {
            if name == "src/api" {
                [0.0, 0.0, 96.0, 86.0]
            } else {
                [200.0, 0.0, 296.0, 86.0]
            }
        })
    }

    /// Whether `flag` rides the body of agent `agent`.
    fn follows_body(flag: &Flag, agent: &str) -> bool {
        matches!(&flag.follow, Follow::Body { agent: named, .. } if named == agent)
    }

    /// Every machine's stand: its top at the plinth top, with three spots round its centre. Two
    /// spots on the x axis are a seat step apart, so two drones can take them.
    fn stands(plan: &Plan) -> Vec<Stand> {
        plan.machines
            .iter()
            .map(|machine| {
                let c = machine.center();
                Stand {
                    top: PLINTH_TOP,
                    spots: vec![
                        [c[0] - SEAT_STEP * 0.5 - 1.0, c[1]],
                        [c[0] + SEAT_STEP * 0.5 + 1.0, c[1]],
                        [c[0], c[1] + SEAT_STEP * 0.5 + 1.0],
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

    /// Runs the demo script on a fresh origin: one order, two agents on src/api, one clash.
    fn scripted() -> (Origin, Vec<Stamped>, CodeIndex) {
        let index = index();
        let world = WorldRef {
            id: WorldId::from_str("demo").expect("a world id"),
            repo: "demo".to_owned(),
        };
        let (mut origin, opened) = Origin::open(world, founder(), Timestamp::from_millis(1_000));
        let target = Target::first_symbol(&index, "Make login return a session")
            .expect("the index holds a symbol");
        let events = scenario::clash_script(
            &mut origin,
            &index,
            &founder(),
            &target,
            Timestamp::from_millis(2_000),
        )
        .expect("the script runs");
        let log = std::iter::once(opened).chain(events).collect();
        (origin, log, index)
    }

    fn machine(plan: &Plan, name: &str) -> usize {
        plan.index_of(name).expect("the plan has the machine")
    }

    /// The slots of the scripted view's agents, in view order.
    fn slots_for(view: &View) -> AgentSlots {
        let mut slots = AgentSlots::default();
        slots.assign(view);
        slots
    }

    #[test]
    fn an_open_clash_outranks_the_scope_tint_underneath() {
        let marks = Marks {
            abnormal: vec![false, true, false],
            scope_tints: vec![Some(ScopeTint::Keep), Some(ScopeTint::Plot), None],
            ..Marks::default()
        };
        assert_eq!(
            u32::from(marks.footprint_tint(0)),
            TINT_SCOPE0 + ScopeTint::Keep as u32
        );
        assert_eq!(u32::from(marks.footprint_tint(1)), TINT_CLASH);
        assert_eq!(marks.footprint_tint(2), 0);
    }

    fn flag(kind: FlagKind) -> Flag {
        Flag {
            kind,
            text: String::new(),
            hue: None,
            waiting: false,
            clash: None,
            check: None,
            leaving: false,
            detail: Vec::new(),
            scope: None,
            machine: None,
            at: [0.0; 3],
            drone: None,
            on: None,
            follow: Follow::Still,
        }
    }

    /// The scripted clash followed by the rest of the demo: both tasks build, pass and land.
    fn landed() -> (Mirror, CodeIndex) {
        let (mut origin, mut log, index) = scripted();
        let now = Timestamp::from_millis(4_000);
        let rest = scenario::land_script(&mut origin, &index, &founder(), now)
            .expect("the land script runs");
        log.extend(rest);
        (Mirror::from_log(log).expect("the log replays"), index)
    }

    fn landed_text(motion: &Motion, marks: &Marks) -> Vec<String> {
        motion
            .flags_now(&marks.flags)
            .into_iter()
            .filter(|flag| flag.kind == FlagKind::Landed)
            .map(|flag| flag.text)
            .collect()
    }

    #[test]
    fn a_landed_dot_keeps_its_word_for_the_hold_then_goes_quiet() {
        let plan = plan();
        let (mirror, _index) = landed();
        let view = mirror.view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        let mut motion = Motion::default();
        motion.sync(&marks);

        assert_eq!(
            landed_text(&motion, &marks),
            vec!["landed · login".to_owned()],
            "the machine's dot names the change while it holds"
        );
        assert!(motion.advance(LANDED_HOLD_MS - 1.0), "the hold still runs");
        assert_eq!(
            landed_text(&motion, &marks),
            vec!["landed · login".to_owned()]
        );
        motion.advance(1.0);
        assert_eq!(
            landed_text(&motion, &marks),
            vec![String::new()],
            "after the hold the dot is quiet"
        );
        assert!(
            motion.take_flags_changed(),
            "the page is told the dot went quiet"
        );
        assert!(!motion.advance(LEAVE_MS), "nothing else is moving");
    }

    #[test]
    fn a_still_bot_stands_at_its_place_and_its_flag_has_a_finite_anchor() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        let mut motion = Motion::new(true);
        motion.sync(&marks);
        assert!(!marks.bodies.is_empty(), "the scripted view has bots");
        for body in &marks.bodies {
            assert_eq!(motion.body_at(&body.agent), Some(body.at), "{}", body.agent);
        }
        for flag in &marks.flags {
            assert!(
                motion.flag_at(flag).iter().all(|v| v.is_finite()),
                "a flag anchor is finite: {}",
                flag.text
            );
        }
    }

    #[test]
    fn a_still_motion_keeps_the_landed_word_and_never_moves() {
        let plan = plan();
        let (mirror, _index) = landed();
        let view = mirror.view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        let mut motion = Motion::new(true);
        motion.sync(&marks);
        assert!(
            !motion.advance(LANDED_HOLD_MS * 2.0),
            "a still page does not ease"
        );
        assert_eq!(
            landed_text(&motion, &marks),
            vec!["landed · login".to_owned()]
        );
    }

    #[test]
    fn a_landed_bot_rises_off_its_machine_then_leaves_and_the_order_flag_fades() {
        let plan = plan();
        let stands = stands(&plan);
        let before = mirror_before_land().view(&plan);
        let before_marks = Marks::build(&before, &plan, &stands, &slots_for(&before));
        let mut motion = Motion::default();
        motion.sync(&before_marks);
        motion.advance(ARRIVE_MS);
        assert!(
            motion.leaving_now().is_empty(),
            "the bots stand on their machines"
        );

        let (after, _index) = landed();
        let after_view = after.view(&plan);
        let after_marks = Marks::build(&after_view, &plan, &stands, &slots_for(&after_view));
        motion.sync(&after_marks);
        assert_eq!(
            motion.leaving_now().len(),
            2,
            "both bots leave once their tasks land"
        );

        motion.advance(LEAVE_MS / 2.0);
        let hover = before_marks
            .bodies
            .iter()
            .map(|body| body.at[2])
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            motion.leaving_now().iter().all(|bot| bot.at[2] > hover),
            "a leaving bot rises from its hover"
        );
        let fading = |flags: &[Flag]| {
            flags
                .iter()
                .any(|flag| flag.kind == FlagKind::Order && flag.leaving)
        };
        assert!(
            fading(&motion.flags_now(&after_marks.flags)),
            "the order flag fades out"
        );

        motion.advance(LEAVE_MS);
        assert!(
            motion.leaving_now().is_empty(),
            "the bots are gone after the ease"
        );
        assert!(
            !fading(&motion.flags_now(&after_marks.flags)),
            "the order flag is gone"
        );
    }

    /// The demo after the clash settles and the first task has built and passed its gate, but
    /// before either task lands.
    fn mirror_before_land() -> Mirror {
        let (mut origin, mut log, index) = scripted();
        let rest = scenario::land_script(
            &mut origin,
            &index,
            &founder(),
            Timestamp::from_millis(4_000),
        )
        .expect("the land script runs");
        let cut = rest
            .iter()
            .position(|stamped| matches!(stamped.event, Event::Landed { .. }))
            .expect("the script lands a task");
        log.extend(rest.into_iter().take(cut));
        Mirror::from_log(log).expect("the log replays")
    }

    #[test]
    fn a_bot_is_drawn_at_least_its_screen_floor_tall() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        let mut motion = Motion::default();
        motion.sync(&marks);
        motion.advance(ARRIVE_MS);
        let kinds = MarkKinds {
            body: 40,
            accent: 41,
            ring: 42,
            pin: 43,
            stalk: 44,
            beacon: 45,
        };
        for dpr in [1.0_f32, 2.0] {
            let bot = mark_instances(&motion, &marks, kinds, dpr, None)
                .into_iter()
                .find(|instance| instance.kind == kinds.body)
                .expect("a bot is drawn");
            assert_eq!(bot.grow[0], BOT_MIN_PX * dpr, "the floor is in device px");
        }
        assert_eq!(
            BOT_MIN_PX, 24.0,
            "a bot reads as a dot with ears from 24 px"
        );
    }

    #[test]
    fn the_first_two_agents_take_teal_and_violet_and_keep_them() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let mut slots = AgentSlots::default();
        slots.assign(&view);
        let ids: Vec<String> = view.agents.iter().map(|a| a.id.to_string()).collect();
        assert_eq!(slots.slot(&ids[0]), 0, "the first agent takes slot 0");
        assert_eq!(slots.slot(&ids[1]), 1, "the second agent takes slot 1");
        assert_eq!(hue_css(0), "#0f7a6e", "teal");
        assert_eq!(hue_css(1), "#6a45c8", "violet");
        let before = slots.clone();
        slots.assign(&view);
        assert_eq!(slots, before, "a slot stays with its agent");
    }

    #[test]
    fn two_agents_on_one_machine_stand_apart_on_its_top() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let slots = slots_for(&view);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots);

        let api = machine(&plan, "src/api");
        let on_api: Vec<&Body> = marks
            .bodies
            .iter()
            .filter(|body| body.machine == api)
            .collect();
        let [first, second] = on_api.as_slice() else {
            panic!("both agents stand on src/api");
        };
        let gap = distance([first.at[0], first.at[1]], [second.at[0], second.at[1]]);
        assert!(gap >= SEAT_STEP, "the bodies are a seat apart, not {gap}");
        for body in [first, second] {
            assert_eq!(
                body.ground,
                surface_below([body.at[0], body.at[1]], &plan, &stands(&plan)),
                "a drone's shadow lies on the surface straight under it"
            );
            assert_eq!(
                body.at[2] - BODY_RADIUS - body.ground,
                HOVER_GAP,
                "a drone's underside clears the surface under it by a quarter of its radius"
            );
        }
        assert_ne!(first.slot, second.slot, "the two agents have distinct hues");
    }

    #[test]
    fn a_drone_sees_its_machine_top_when_over_it_and_the_ground_beside_every_machine() {
        let plan = plan();
        let stands = stands(&plan);
        let api = machine(&plan, "src/api");
        let centre = plan.machines[api].center();
        assert_eq!(
            surface_below(centre, &plan, &stands),
            stands[api].top,
            "over the machine, its top"
        );
        assert_eq!(
            surface_below([-1.0e6, -1.0e6], &plan, &stands),
            GROUND_Z,
            "far from every machine, the ground"
        );
    }

    #[test]
    fn a_bot_falls_back_to_the_centre_when_no_spot_is_free() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let mut bare = stands(&plan);
        for stand in &mut bare {
            stand.spots.clear();
        }
        let marks = Marks::build(&view, &plan, &bare, &slots_for(&view));
        let api = machine(&plan, "src/api");
        let centre = plan.machines[api].center();
        let top = bare[api].top;
        for body in marks.bodies.iter().filter(|body| body.machine == api) {
            assert_eq!([body.at[0], body.at[1]], centre);
            assert!(
                body.at[2] - BODY_RADIUS >= top + HOVER_GAP - 1e-4,
                "over the machine's centre the body's bottom clears its top"
            );
        }
    }

    #[test]
    fn the_scripted_clash_gets_a_pin_on_its_machine_with_its_label() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));

        let api = machine(&plan, "src/api");
        let [pin] = marks.pins.as_slice() else {
            panic!("the open clash has one pin");
        };
        assert_eq!(pin.machine, api);
        assert_eq!(pin.clash, 0);
        assert_eq!(pin.id, view.clashes[0].id.to_string());
        assert_eq!(
            pin.at[2], PLINTH_TOP,
            "the stalk rises from the machine top"
        );

        let clash_flag = marks
            .flags
            .iter()
            .find(|flag| flag.kind == FlagKind::Clash)
            .expect("the pin has a flag");
        assert_eq!(clash_flag.text, view.clashes[0].label);
        assert_eq!(clash_flag.clash, Some(0));
        assert_eq!(
            clash_flag.at[2],
            PLINTH_TOP + STALK_HEIGHT,
            "the flag rides the diamond"
        );

        assert_eq!(
            marks.bodies.iter().filter(|body| body.waiting()).count(),
            2,
            "both agents wait on the open clash"
        );
        assert_eq!(
            u32::from(marks.footprint_tint(api)),
            TINT_CLASH,
            "the clashed machine takes the faint red"
        );
        let ui = machine(&plan, "src/ui");
        assert_eq!(marks.footprint_tint(ui), 0, "a quiet machine stays grey");
    }

    #[test]
    fn a_clash_flag_holds_its_id_overlap_and_two_agents_for_the_hover() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        let clash = &view.clashes[0];
        let flag = marks
            .flags
            .iter()
            .find(|flag| flag.kind == FlagKind::Clash)
            .expect("the pin has a flag");
        assert!(
            flag.text.starts_with(&clash.id.to_string()),
            "{}",
            flag.text
        );
        assert_eq!(
            flag.on.as_deref(),
            Some("src/api"),
            "it marks the clashed machine"
        );
        let agents: Vec<String> = view
            .agents
            .iter()
            .map(|agent| agent.id.to_string())
            .collect();
        let [a, b] = flag.detail.as_slice() else {
            panic!("the detail names two agents: {:?}", flag.detail);
        };
        assert!(a.ends_with(&format!(" · {}", clash.a)), "{a}");
        assert!(b.ends_with(&format!(" · {}", clash.b)), "{b}");
        let holder = |line: &str| line.split(" · ").next().unwrap_or_default().to_owned();
        assert!(
            agents.contains(&holder(a)) && agents.contains(&holder(b)),
            "{agents:?}"
        );
        assert_ne!(holder(a), holder(b), "the two agents differ");
    }

    #[test]
    fn an_order_flag_names_the_machine_whose_hover_shows_it() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        let order = marks
            .flags
            .iter()
            .find(|flag| flag.kind == FlagKind::Order)
            .expect("the live order has a flag");
        assert_eq!(order.on.as_deref(), Some(view.orders[0].machine.as_str()));
        for flag in marks
            .flags
            .iter()
            .filter(|flag| flag.kind == FlagKind::Agent)
        {
            assert_eq!(
                flag.on, None,
                "an agent's pill is not a machine's hover text"
            );
        }
    }

    #[test]
    fn a_machine_with_working_agents_and_no_clash_stays_grey() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let mut view = Mirror::from_log(log).expect("the log replays").view(&plan);
        view.clashes.clear();
        for machine in &mut view.machines {
            machine.abnormal = false;
        }
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        assert!(!marks.bodies.is_empty(), "the agents still have drones");
        for body in &marks.bodies {
            assert_eq!(marks.footprint_tint(body.machine), 0);
        }
    }

    #[test]
    fn the_card_rows_come_from_the_view() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let mirror = Mirror::from_log(log).expect("the log replays");
        let view = mirror.view(&plan);

        let card = card(&view, mirror.world(), 0, &slots_for(&view)).expect("clash 0 has a card");
        assert_eq!(card.title, view.clashes[0].label);
        let [first, second, settle] = card.rows.as_slice() else {
            panic!("two declarations and the settle row, with no witness");
        };
        assert!(matches!(first, CardRow::Declaration { hue, .. } if hue.starts_with('#')));
        assert!(matches!(second, CardRow::Declaration { .. }));
        assert!(matches!(
            settle,
            CardRow::Settle {
                key: 1,
                suggested: false,
                ..
            }
        ));
        assert_eq!(card.keys.len(), view.clashes[0].options.len());
        assert!(card.keys[0].starts_with("1 "));
        assert_eq!(card.resolved, None);
    }

    #[test]
    fn a_broken_witness_is_a_row_before_the_settle() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let mirror = Mirror::from_log(log).expect("the log replays");
        let mut view = mirror.view(&plan);
        view.clashes[0].breaks = Some("w-2".to_owned());

        let card = card(&view, mirror.world(), 0, &slots_for(&view)).expect("clash 0 has a card");
        let [_, _, witness, settle] = card.rows.as_slice() else {
            panic!("two declarations, the witness and the settle row");
        };
        assert_eq!(
            witness,
            &CardRow::Witness {
                text: "breaks w-2".to_owned()
            }
        );
        assert!(matches!(settle, CardRow::Settle { .. }));
    }

    #[test]
    fn a_settled_clash_loses_its_pin_once_the_pin_has_sunk() {
        let plan = plan();
        let stands = stands(&plan);
        let (mut origin, log, index) = scripted();
        let mut mirror = Mirror::from_log(log).expect("the log replays");

        let before = mirror.view(&plan);
        let mut motion = Motion::default();
        motion.sync(&Marks::build(
            &before,
            &plan,
            &stands,
            &AgentSlots::default(),
        ));
        assert_eq!(motion.pins_now().len(), 1, "the open clash shows its pin");

        let clash = before.clashes[0].id;
        let option = before.clashes[0]
            .options
            .first()
            .expect("the clash offers a settlement");
        let payload = json!({"cmd": "settle_clash", "clash": clash, "choice": option.choice});
        let cmd: HumanCmd = serde_json::from_value(payload).expect("a human command");
        let ctx = Ctx {
            now: Timestamp::from_millis(3_000),
            index: &index,
        };
        let events = origin
            .execute(
                &ctx,
                Command::Human {
                    member: founder(),
                    cmd,
                },
            )
            .expect("the settle is accepted");
        for stamped in &events {
            mirror
                .ingest(stamped)
                .expect("a live event applies in order");
        }

        let after = mirror.view(&plan);
        let marks = Marks::build(&after, &plan, &stands, &AgentSlots::default());
        assert!(marks.pins.is_empty(), "a settled clash has no pin");
        motion.sync(&marks);
        assert_eq!(
            motion.pins_now().len(),
            1,
            "the pin is still there while it sinks out"
        );
        motion.advance(PIN_MS);
        assert!(
            motion.pins_now().is_empty(),
            "the pin is gone after the ease"
        );
    }

    #[test]
    fn a_waiting_body_gets_a_ring_and_each_pin_a_stalk_and_a_diamond() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        let mut motion = Motion::default();
        motion.sync(&marks);
        motion.advance(ARRIVE_MS);

        let kinds = MarkKinds {
            body: 40,
            accent: 41,
            ring: 42,
            pin: 43,
            stalk: 44,
            beacon: 45,
        };
        let instances = mark_instances(&motion, &marks, kinds, 1.0, None);
        let count = |kind: u16| instances.iter().filter(|i| i.kind == kind).count();
        assert_eq!(count(kinds.body), 2, "each drone has a body");
        assert_eq!(count(kinds.accent), 2, "each drone has an accent");
        let shadow = u8::try_from(TINT_SHADOW).expect("fits");
        let shadows: Vec<Beam> = spots(&motion, &marks, 1.0, None)
            .into_iter()
            .filter(|spot| spot.tint == shadow)
            .collect();
        assert_eq!(shadows.len(), 2, "each drone has a shadow");
        for body in &marks.bodies {
            let shadow = shadows
                .iter()
                .find(|s| s.origin[0] == body.at[0] && s.origin[1] == body.at[1])
                .expect("the drone has a shadow straight under it");
            assert_eq!(
                shadow.origin[2], body.ground,
                "the shadow lies on the ground under the drone"
            );
        }
        assert_eq!(count(kinds.ring), 2, "each waiting agent has a ring");
        assert_eq!(count(kinds.pin), 1);
        assert_eq!(count(kinds.stalk), 1, "each diamond has its stalk");
    }

    #[test]
    fn a_drone_flag_sits_above_the_crown_and_a_clash_label_to_the_right() {
        // Screen y falls as z rises, and one plan unit is one px, so the floors decide the sizes.
        let project = |p: [f32; 3]| [p[0], -p[2]];
        let agent = Flag {
            kind: FlagKind::Agent,
            ..flag(FlagKind::Agent)
        };
        let centre = [10.0, 5.0, 50.0];
        let anchor = flag_anchor(&agent, centre, project);
        assert_eq!(anchor[0], 10.0);
        assert!(
            (anchor[1] - (-50.0 - BODY_RADIUS - FLAG_GAP_PX)).abs() < 1e-3,
            "the flag sits a gap above the crown, got {}",
            anchor[1]
        );

        let clash = flag(FlagKind::Clash);
        let label = flag_anchor(&clash, centre, project);
        let [left, right] = badge_centres(centre, project);
        assert!(left[0] < centre[0] && right[0] > centre[0]);
        assert_eq!(left[1], right[1], "the badges sit level");
        let half = PIN_MIN_PX * DIAMOND_HALF_WIDTH;
        assert!(
            right[0] - BADGE_PX * 0.5 > centre[0] + half,
            "the badge is clear of the diamond"
        );
        assert_eq!(
            label,
            [right[0] + BADGE_PX * 0.5 + FLAG_GAP_PX, right[1]],
            "the label starts right of the right badge"
        );
    }

    #[test]
    fn a_long_quiet_agent_is_away_its_drone_gone_and_a_dot_on_its_plinth() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let mut view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let waiting_view = view.clone();
        view.clashes.clear();
        let heard = view.agents[0].heard_at.expect("the agent was heard");
        let agent = view.agents[0].id.to_string();
        let slots = slots_for(&view);
        let stands = stands(&plan);
        let at = |now: Option<u64>| {
            let clock = Clock { now, away: None };
            Marks::build_at(&view, &plan, &stands, &slots, clock)
        };
        let aways = |marks: &Marks| -> Vec<Flag> {
            marks
                .flags
                .iter()
                .filter(|flag| flag.kind == FlagKind::Away)
                .cloned()
                .collect()
        };
        let has_drone = |marks: &Marks| marks.bodies.iter().any(|body| body.agent == agent);

        let frozen = at(None);
        assert!(
            has_drone(&frozen) && aways(&frozen).is_empty(),
            "no clock, no away"
        );
        let stale = at(Some(heard + AWAY_MS - 1));
        assert!(has_drone(&stale) && aways(&stale).is_empty());
        let body = stale.bodies.iter().find(|body| body.agent == agent);
        assert_eq!(body.map(|body| body.state), Some(DroneState::Stale));

        let away = at(Some(heard + 12 * 60_000));
        assert!(!has_drone(&away), "the drone is gone");
        assert!(
            away.flags.iter().all(|flag| !follows_body(flag, &agent)),
            "and its flag with it"
        );
        let dots = aways(&away);
        let hue = Some(hue_css(slots.slot(&agent)));
        let dot = dots
            .iter()
            .find(|dot| dot.hue == hue)
            .expect("the agent has an away dot in its hue");
        assert_eq!(dots.len(), away.bodies.len().abs_diff(view.agents.len()));
        assert_eq!(dot.text, "12m");
        let name = view.agents[0]
            .machine
            .as_deref()
            .expect("the agent has a machine");
        let footprint = &plan.machines[machine(&plan, name)];
        assert_eq!(
            dot.at,
            [
                footprint.min[0] + footprint.size[0] - LANDED_INSET,
                footprint.min[1] + footprint.size[1] - LANDED_INSET,
                PLINTH_TOP,
            ],
            "the back corner, opposite the landed dot"
        );
        let project = |p: [f32; 3]| [p[0], -p[2]];
        assert_eq!(flag_anchor(dot, dot.at, project), project(dot.at));

        let waiting = Marks::build_at(
            &waiting_view,
            &plan,
            &stands,
            &slots,
            Clock {
                now: Some(heard + 12 * 60_000),
                away: None,
            },
        );
        let body = waiting.bodies.iter().find(|body| body.agent == agent);
        assert_eq!(
            body.map(|body| body.state),
            Some(DroneState::Waiting),
            "an agent that waits on the user is never away"
        );
    }

    #[test]
    fn an_agent_flag_names_its_drone_short_and_says_how_long_a_stale_one_is_quiet() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let mut view = Mirror::from_log(log).expect("the log replays").view(&plan);
        view.clashes.clear();
        let heard = view.agents[0].heard_at.expect("the agent was heard");
        let agent = view.agents[0].id.to_string();
        let slots = slots_for(&view);
        let stands = stands(&plan);
        let flag_at = |now: Option<u64>| {
            let clock = Clock { now, away: None };
            Marks::build_at(&view, &plan, &stands, &slots, clock)
                .flags
                .into_iter()
                .find(|flag| follows_body(flag, &agent))
                .expect("the agent has a flag")
        };
        let fresh = flag_at(None);
        assert_eq!(
            fresh.text,
            agent.chars().take(FLAG_ID_CHARS).collect::<String>()
        );
        let drone = fresh.drone.clone().expect("an agent flag names its drone");
        assert_eq!(drone.agent, agent);
        assert_eq!(drone.state, "working");
        assert_eq!(drone.quiet, None);
        let title = view.agents[0]
            .task
            .and_then(|task| view.tasks.iter().find(|candidate| candidate.id == task))
            .map(|task| task.title.clone())
            .expect("the agent holds a task");
        assert_eq!(
            fresh.detail,
            vec![view.agents[0].activity.to_owned(), title],
            "hover or Alt adds the activity and the task title"
        );
        let stale = flag_at(Some(heard + 3 * 60_000));
        let drone = stale.drone.expect("an agent flag names its drone");
        assert_eq!(drone.state, "stale");
        assert_eq!(drone.quiet.as_deref(), Some("3m"));

        let long = Body {
            agent: "a-very-long-agent-name".to_owned(),
            ..Marks::build(&view, &plan, &stands, &slots).bodies[0].clone()
        };
        let flag = agent_flag(&view, &long, Clock::default());
        assert_eq!(flag.text, "a-very-long-", "cut to twelve characters");
        assert_eq!(
            flag.drone.map(|drone| drone.agent).as_deref(),
            Some("a-very-long-agent-name")
        );
    }

    #[test]
    fn the_pointer_hovers_the_nearest_drone_within_its_radius_and_six_px() {
        let discs = [([100.0, 100.0], 12.0), ([130.0, 100.0], 12.0)];
        assert_eq!(disc_under(&discs, [100.0, 100.0]), Some(0));
        assert_eq!(
            disc_under(&discs, [100.0, 100.0 + 12.0 + HOVER_PAD_PX]),
            Some(0)
        );
        assert_eq!(
            disc_under(&discs, [100.0, 100.0 + 12.0 + HOVER_PAD_PX + 0.5]),
            None
        );
        assert_eq!(
            disc_under(&discs, [114.0, 100.0]),
            Some(0),
            "nearer the first"
        );
        assert_eq!(
            disc_under(&discs, [116.0, 100.0]),
            Some(1),
            "nearer the second"
        );
        assert_eq!(disc_under(&[], [0.0, 0.0]), None);
    }

    #[test]
    fn a_hovered_drone_is_named_as_the_page_names_its_agent_in_the_view() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        let json = serde_json::to_value(&view).expect("the view serializes");
        let ids: Vec<&str> = json["agents"]
            .as_array()
            .expect("the view lists its agents")
            .iter()
            .map(|agent| agent["id"].as_str().expect("an agent id is a string"))
            .collect();
        assert!(!marks.bodies.is_empty());
        for body in &marks.bodies {
            assert!(
                ids.contains(&body.agent.as_str()),
                "acan:drone-hover names {} as view.agents[].id does",
                body.agent
            );
        }
    }

    #[test]
    fn a_hovered_drone_inks_its_tether_in_fully() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        let mut motion = Motion::new(true);
        motion.sync(&marks);
        let agent = marks.bodies[0].agent.clone();
        let rest = beams(&motion, &marks, 1.0, None, None);
        let hovered = beams(&motion, &marks, 1.0, None, Some(&agent));
        assert_eq!(rest.len(), hovered.len());
        assert_eq!(hovered[0].alpha, 1.0);
        assert_eq!(rest[0].alpha, marks.bodies[0].state.tether_alpha());
        assert_eq!(&hovered[1..], &rest[1..], "the other tethers keep theirs");
    }

    #[test]
    fn an_away_stamp_counts_minutes_then_hours() {
        assert_eq!(away_stamp(AWAY_MS), "10m");
        assert_eq!(away_stamp(59 * 60_000 + 59_999), "59m");
        assert_eq!(away_stamp(60 * 60_000), "1h");
        assert_eq!(away_stamp(150 * 60_000), "2h");
    }

    #[test]
    fn initials_are_the_first_letter_of_the_last_segment_and_the_trailing_digits() {
        assert_eq!(initials("w2"), "W2");
        assert_eq!(initials("agent-a"), "A");
        assert_eq!(initials("agent-b"), "B");
        assert_eq!(initials("worker-12"), "W12");
        assert_eq!(initials("worker-1234"), "W12", "at most three characters");
        assert_eq!(initials("team_blue"), "B");
        assert_eq!(
            initials("ci.runner 7"),
            "C7",
            "a last segment of digits only"
        );
        assert_eq!(initials("42"), "42");
        assert_eq!(initials(""), "?");
        assert_eq!(
            pair_initials("agent-a", "agent-b"),
            ("A".to_owned(), "B".to_owned())
        );
        assert_eq!(
            pair_initials("agent", "anna"),
            ("AG".to_owned(), "AN".to_owned())
        );
        assert_eq!(
            pair_initials("w2", "w3"),
            ("W2".to_owned(), "W3".to_owned())
        );
    }

    #[test]
    fn two_agents_on_one_clash_never_share_initials() {
        let ids = [
            "agent-a",
            "agent-b",
            "agent",
            "anna",
            "w2",
            "w3",
            "worker-12",
            "worker-1234",
            "worker-1299",
            "team1-a",
            "team2-a",
            "x-ab",
            "y-ab",
            "42",
            "43",
            "",
            "-",
            "a-12",
            "b-12",
            "alpha",
            "alps",
        ];
        for a in ids {
            for b in ids {
                if a == b {
                    continue;
                }
                let (first, second) = pair_initials(a, b);
                assert_ne!(first, second, "{a} and {b}");
                assert!(first.chars().count() <= INITIALS_MAX, "{a} and {b}");
                assert!(second.chars().count() <= INITIALS_MAX, "{a} and {b}");
            }
        }
    }
    #[test]
    fn a_clash_has_a_badge_in_each_agents_hue_and_a_grey_query_for_no_agent() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let mut view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let slots = slots_for(&view);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots);
        assert!(!marks.pins.is_empty(), "the script has a clash");
        let badges = clash_badges(&view, &marks.pins, &slots);
        assert_eq!(badges.len(), marks.pins.len());
        let clash = &view.clashes[badges[0].clash];
        let holder = |task: TaskId| {
            view.tasks
                .iter()
                .find(|candidate| candidate.id == task)
                .and_then(|candidate| candidate.agent.as_ref())
                .map(ToString::to_string)
                .expect("the script claims both tasks")
        };
        let (a, b) = (holder(clash.a), holder(clash.b));
        let (a_text, b_text) = pair_initials(&a, &b);
        assert_eq!(
            badges[0].a,
            Badge {
                text: a_text,
                hue: hue_css(slots.slot(&a))
            }
        );
        assert_eq!(
            badges[0].b,
            Badge {
                text: b_text,
                hue: hue_css(slots.slot(&b))
            }
        );
        let task_b = clash.b;
        for task in &mut view.tasks {
            if task.id == task_b {
                task.agent = None;
            }
        }
        let badges = clash_badges(&view, &marks.pins, &slots);
        assert_eq!(
            badges[0].b,
            Badge {
                text: "?".to_owned(),
                hue: UNCLAIMED.to_owned()
            }
        );
    }

    #[test]
    fn a_landed_dot_stands_on_the_plinth_at_the_same_corner_of_every_machine() {
        let plan = plan();
        let (mirror, _index) = landed();
        let view = mirror.view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        let dots: Vec<&Flag> = marks
            .flags
            .iter()
            .filter(|flag| flag.kind == FlagKind::Landed)
            .collect();
        assert!(!dots.is_empty(), "the landed story has a dot");
        for dot in dots {
            let machine = match dot.follow {
                Follow::Landed { machine, .. } => machine,
                _ => panic!("a landed dot follows its machine"),
            };
            let [x, y] = plan.machines[machine].min;
            assert_eq!(
                dot.at,
                [x + LANDED_INSET, y + LANDED_INSET, PLINTH_TOP],
                "the dot sits on the plinth top at the machine's corner"
            );
        }
    }

    #[test]
    fn a_landed_dot_sits_beside_its_machine_point_clear_of_the_order_flag() {
        let project = |p: [f32; 3]| [p[0], -p[2]];
        let at = [10.0, 5.0, PLINTH_TOP];
        let landed = flag_anchor(&flag(FlagKind::Landed), at, project);
        let order = flag_anchor(&flag(FlagKind::Order), at, project);
        assert_eq!(landed, [10.0 + FLAG_GAP_PX, -PLINTH_TOP]);
        assert_eq!(
            order,
            [10.0, -PLINTH_TOP],
            "the order flag stays over the point"
        );
    }

    #[test]
    fn a_seat_stays_put_and_the_farthest_free_spot_takes_the_rest() {
        let spots = [[0.0, 0.0], [4.0, 0.0]];
        let first = seat(&spots, [9.0, 9.0], &[]);
        assert_eq!(first, [0.0, 0.0]);
        let second = seat(&spots, [9.0, 9.0], &[first]);
        assert_eq!(
            second,
            [4.0, 0.0],
            "a spot nearer than a seat step is not free"
        );
        assert_eq!(seat(&[], [9.0, 9.0], &[]), [9.0, 9.0]);
    }

    /// A drone at `at`, facing `target`, with no marks but its own.
    fn drone(agent: &str, at: [f32; 3], target: [f32; 3], activity: &'static str) -> Body {
        Body {
            agent: agent.to_string(),
            slot: 0,
            machine: 0,
            at,
            ground: GROUND_Z,
            target,
            activity,
            state: DroneState::of(activity, false, false, false),
            check: None,
        }
    }

    #[test]
    fn a_quiet_agent_holding_a_task_reads_stale_only_on_a_live_clock() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let mut view = Mirror::from_log(log).expect("the log replays").view(&plan);
        view.clashes.clear();
        for machine in &mut view.machines {
            machine.abnormal = false;
        }
        let heard = view.agents[0].heard_at.expect("the agent was heard");
        let slots = slots_for(&view);
        let stands = stands(&plan);
        let at = |now: Option<u64>| {
            let clock = Clock { now, away: None };
            Marks::build_at(&view, &plan, &stands, &slots, clock).bodies[0].state
        };
        assert_eq!(
            at(None),
            DroneState::Working,
            "no clock, as frozen or replayed"
        );
        assert_eq!(at(Some(heard + STALE_MS - 1)), DroneState::Working);
        assert_eq!(at(Some(heard + STALE_MS)), DroneState::Stale);
        let stale = Marks::build_at(
            &view,
            &plan,
            &stands,
            &slots,
            Clock {
                now: Some(heard + STALE_MS),
                away: None,
            },
        );
        let fresh = Marks::build(&view, &plan, &stands, &slots);
        assert_eq!(
            stale.bodies[0].at[2],
            fresh.bodies[0].at[2] - STALE_DROP,
            "a stale drone sags"
        );
        assert_eq!(
            stale.footprint_tint(stale.bodies[0].machine),
            0,
            "and its machine stays grey"
        );
    }

    #[test]
    fn a_claimed_stage_works_and_every_other_activity_is_idle() {
        for activity in ["claimed", "declared", "building", "gating"] {
            assert_eq!(
                DroneState::of(activity, false, false, false),
                DroneState::Working
            );
        }
        for activity in ["open", "readback", "queued", "landed", "idle"] {
            assert_eq!(
                DroneState::of(activity, false, false, false),
                DroneState::Idle
            );
        }
    }

    #[test]
    fn waiting_outranks_stale_which_outranks_drifting_which_outranks_working() {
        assert_eq!(
            DroneState::of("building", true, true, true),
            DroneState::Waiting
        );
        assert_eq!(
            DroneState::of("building", false, true, true),
            DroneState::Stale
        );
        assert_eq!(
            DroneState::of("open", false, true, false),
            DroneState::Stale
        );
        assert_eq!(
            DroneState::of("building", false, false, true),
            DroneState::Drifting
        );
        assert_eq!(
            DroneState::of("building", false, false, false),
            DroneState::Working
        );
    }

    /// The scripted view with its clashes and questions cleared, agent-a building on its task, and
    /// that task's order given one scope lane, stopped or not, with `open_contests` contests.
    fn view_with_lane(stopped: bool, open_contests: u32) -> View {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let mut view = Mirror::from_log(log).expect("the log replays").view(&plan);
        view.clashes.clear();
        view.questions.clear();
        view.agents[0].activity = "building";
        let order = view.agents[0]
            .task
            .and_then(|task| view.tasks.iter().find(|candidate| candidate.id == task))
            .map(|task| task.order)
            .expect("the agent holds a task");
        let scope: Scope = serde_json::from_value(json!({
            "version": "acan-scope/1",
            "order": order.to_string(),
            "rev": 1,
            "base_commit": "0123456789abcdef0123456789abcdef01234567",
            "body": {"may": [], "must_keep": [], "open": []}
        }))
        .expect("the scope fixture decodes");
        let scope_hash = scope.scope_hash().expect("the scope hashes");
        view.lanes = vec![LaneView {
            order,
            rev: scope.rev,
            scope_hash,
            scope,
            state: "unconfirmed",
            may: Vec::new(),
            must_keep: Vec::new(),
            open: Vec::new(),
            confirmation: None,
            live_leases: 0,
            stopped,
            open_contests,
            episodes_opened: 0,
            max_episodes: None,
            rounds_used: 0,
            repair_rounds: None,
            hold: None,
        }];
        view
    }

    /// The state of the first agent's drone in `view`. There is no clock, so it is never stale.
    fn first_drone_state(view: &View) -> DroneState {
        let plan = plan();
        let agent = view.agents[0].id.to_string();
        Marks::build(view, &plan, &stands(&plan), &slots_for(view))
            .bodies
            .iter()
            .find(|body| body.agent == agent)
            .map(|body| body.state)
            .expect("the agent has a drone")
    }

    #[test]
    fn an_agent_on_a_stopped_order_waits() {
        assert_eq!(
            first_drone_state(&view_with_lane(true, 0)),
            DroneState::Waiting,
            "a spent repair budget holds the order's agents"
        );
    }

    #[test]
    fn an_agent_under_an_open_contest_waits() {
        assert_eq!(
            first_drone_state(&view_with_lane(false, 1)),
            DroneState::Waiting,
            "an open contest holds the order's agents"
        );
    }

    #[test]
    fn an_agent_on_a_live_order_still_works() {
        assert_eq!(
            first_drone_state(&view_with_lane(false, 0)),
            DroneState::Working
        );
    }

    #[test]
    fn every_activity_the_mirror_names_has_a_state() {
        let plan = plan();
        let (_origin, log, _index) = scripted();
        let view = Mirror::from_log(log).expect("the log replays").view(&plan);
        let marks = Marks::build(&view, &plan, &stands(&plan), &slots_for(&view));
        for body in &marks.bodies {
            assert_eq!(
                body.waiting(),
                body.state == DroneState::Waiting,
                "{}",
                body.agent
            );
        }
        let words: BTreeSet<&str> = [
            DroneState::Working,
            DroneState::Idle,
            DroneState::Waiting,
            DroneState::Stale,
        ]
        .map(DroneState::word)
        .into_iter()
        .collect();
        assert_eq!(words.len(), 4, "each state has its own word");
    }

    fn marks_of(bodies: Vec<Body>) -> Marks {
        Marks {
            bodies,
            ..Marks::default()
        }
    }

    const KINDS: MarkKinds = MarkKinds {
        body: 40,
        accent: 41,
        ring: 42,
        pin: 43,
        stalk: 44,
        beacon: 45,
    };

    #[test]
    fn a_drone_faces_its_machine_centre_and_the_angle_is_the_plan_direction() {
        let a = [0.0, 0.0, PLINTH_TOP];
        assert!(
            heading(a, [1.0, 0.0, 9.0]).is_some_and(|h| h.abs() < 1e-6),
            "east is zero"
        );
        assert!(heading(a, [0.0, 2.0, 9.0]).is_some_and(|h| (h - PI / 2.0).abs() < 1e-6));
        assert_eq!(
            heading(a, [0.0, 0.0, 5.0]),
            None,
            "a drone on the centre has no way to face"
        );

        let drone = drone(
            "a",
            [0.0, 0.0, PLINTH_TOP],
            [3.0, 4.0, PLINTH_TOP],
            "building",
        );
        let marks = marks_of(vec![drone.clone()]);
        let mut motion = Motion::new(true);
        motion.sync(&marks);
        let facing = motion.heading_of("a").expect("the drone has a heading");
        let want = (4.0_f32).atan2(3.0);
        assert!(
            (facing - want).abs() < 1e-5,
            "faces the centre: {facing} vs {want}"
        );
    }

    #[test]
    fn a_drone_turns_the_short_way_and_eases_over_the_turn_time() {
        let at = [0.0, 0.0, PLINTH_TOP];
        let east = drone("a", at, [1.0, 0.0, PLINTH_TOP], "idle");
        let north = drone("a", at, [0.0, 1.0, PLINTH_TOP], "idle");
        let mut motion = Motion::default();
        motion.sync(&marks_of(vec![east]));
        assert!(motion.heading_of("a").is_some_and(|h| h.abs() < 1e-6));

        motion.sync(&marks_of(vec![north]));
        assert!(
            motion.advance(TURN_MS / 2.0),
            "a turn keeps the frames coming"
        );
        let half = motion.heading_of("a").expect("the drone turns");
        assert!(
            (half - PI / 4.0).abs() < 1e-4,
            "halfway through an eased turn: {half}"
        );
        motion.advance(TURN_MS);
        let done = motion.heading_of("a").expect("the drone turned");
        assert!((done - PI / 2.0).abs() < 1e-4, "ends facing north: {done}");
        assert!(
            motion.advance(0.0) && motion.calm(),
            "a settled idle drone keeps only its bob"
        );

        // From just short of pi to just past -pi is a turn of a few degrees, not a full circle.
        let near_pi = drone("b", at, [-1.0, 0.0001, PLINTH_TOP], "idle");
        let past = drone("b", at, [-1.0, -0.0001, PLINTH_TOP], "idle");
        motion.sync(&marks_of(vec![near_pi]));
        motion.sync(&marks_of(vec![past]));
        motion.advance(TURN_MS / 2.0);
        let mid = motion.heading_of("b").expect("the drone turns");
        assert!(mid.cos() < -0.999, "the short arc crosses pi: {mid}");
    }

    #[test]
    fn an_arriving_drone_drops_onto_its_place_easing_out() {
        let at = [0.0, 0.0, 40.0];
        let mut motion = Motion::default();
        motion.sync(&marks_of(vec![drone("a", at, [9.0, 0.0, 9.0], "idle")]));
        let start = motion.body_at("a").expect("the drone arrives");
        assert_eq!(start[2], at[2] + RISE, "it starts above its place");
        motion.advance(ARRIVE_MS / 2.0);
        let half = motion.body_at("a").expect("it drops")[2];
        assert!(
            half - at[2] < RISE * 0.5,
            "an ease out covers most of the drop by half time: {half}"
        );
        motion.advance(ARRIVE_MS / 2.0);
        assert_eq!(motion.body_at("a"), Some(at));
    }

    #[test]
    fn a_move_arcs_over_the_taller_machine_and_faces_the_way_it_travels() {
        let low = |x: f32, top: f32| {
            drone(
                "a",
                [x, 0.0, hover_height(top)],
                [x, 30.0, top + BEACON_RADIUS],
                "idle",
            )
        };
        let mut motion = Motion::default();
        motion.sync(&marks_of(vec![low(0.0, 10.0)]));
        motion.advance(ARRIVE_MS + TURN_MS);
        motion.sync(&marks_of(vec![low(200.0, 30.0)]));
        let duration = move_ms(200.0);
        assert_eq!(duration, 470.0, "350 ms and 0.6 ms a unit");
        motion.advance(duration / 2.0);
        let mid = motion.body_at("a").expect("it moves");
        assert!(
            (mid[2] - (30.0 + ARC_CLEAR)).abs() < 1e-3,
            "the top of the arc clears the taller machine: {mid:?}"
        );
        let facing = motion.heading_of("a").expect("it faces");
        assert!(
            facing.abs() < 1e-4,
            "it faces east, the way it goes: {facing}"
        );
        motion.advance(duration / 2.0);
        let face = motion.heading_of("a").expect("it faces");
        assert!(
            (face - PI / 2.0).abs() < 1e-4,
            "it ends facing its target: {face}"
        );
        assert_eq!(motion.body_at("a").map(|at| at[0]), Some(200.0));
        assert_eq!(move_ms(0.0), 450.0, "a short move takes at least 450 ms");
        assert_eq!(move_ms(5_000.0), 900.0, "a long one at most 900 ms");
    }

    #[test]
    fn a_still_or_reduced_motion_lands_a_move_at_once_and_holds_the_bob() {
        for (still, reduced) in [(true, false), (false, true)] {
            let mut motion = Motion::new(still);
            motion.set_reduced(reduced);
            motion.set_px_units(2.0);
            let first = drone("a", [0.0, 0.0, 40.0], [0.0, 9.0, 9.0], "building");
            let second = drone("a", [90.0, 0.0, 40.0], [90.0, 9.0, 9.0], "building");
            motion.sync(&marks_of(vec![first]));
            assert_eq!(motion.body_at("a"), Some([0.0, 0.0, 40.0]));
            motion.sync(&marks_of(vec![second]));
            assert_eq!(motion.body_at("a"), Some([90.0, 0.0, 40.0]), "it jumps");
            motion.advance(BOB_PERIOD_MS / 4.0);
            assert_eq!(motion.bob(0, DroneState::Working), 0.0, "and does not bob");
            assert_eq!(motion.bob(0, DroneState::Idle), 0.0, "idle or working");
        }
    }

    /// A drone that has settled on one machine and then moves to another, 200 units east.
    fn moved(motion: &mut Motion) {
        let at = |x: f32| drone("a", [x, 0.0, 40.0], [x, 30.0, 9.0], "idle");
        motion.sync(&marks_of(vec![at(0.0)]));
        motion.advance(ARRIVE_MS + TURN_MS);
        motion.sync(&marks_of(vec![at(200.0)]));
    }

    #[test]
    fn a_move_leaves_a_trail_that_inks_in_behind_the_drone_and_fades_over_two_seconds() {
        let mut motion = Motion::default();
        motion.sync(&marks_of(vec![drone(
            "a",
            [0.0, 0.0, 40.0],
            [0.0, 30.0, 9.0],
            "idle",
        )]));
        assert!(
            trail_beams(&motion, 1.0, None).is_empty(),
            "an arrival leaves no trail"
        );
        motion.advance(ARRIVE_MS + TURN_MS);
        motion.sync(&marks_of(vec![drone(
            "a",
            [200.0, 0.0, 40.0],
            [200.0, 30.0, 9.0],
            "idle",
        )]));
        let duration = move_ms(200.0);
        motion.advance(duration / 2.0);
        let half = trail_beams(&motion, 2.0, None);
        assert_eq!(half.len(), TRAIL_PIECES / 2, "half the way is inked");
        assert!(
            half.iter()
                .all(|beam| beam.from[2] == PLINTH_TOP && beam.width_px == 2.0)
        );
        assert!(
            half.iter()
                .all(|beam| beam.alpha <= TRAIL_ALPHA && beam.alpha > 0.0)
        );
        assert_eq!(half[0].tint, body_tint(0), "the trail is the agent's hue");
        motion.advance(duration / 2.0);
        let full = trail_beams(&motion, 1.0, None);
        assert_eq!(full.len(), TRAIL_PIECES, "at most 24 pieces");
        assert!(
            full[0].alpha < full[TRAIL_PIECES - 1].alpha,
            "the oldest piece is the faintest"
        );
        assert!(
            motion.advance(TRAIL_FADE_MS - 1.0),
            "the fade keeps frames coming"
        );
        motion.advance(1.0);
        assert!(trail_beams(&motion, 1.0, None).is_empty(), "gone after 2 s");
        assert!(
            motion.advance(16.0) && motion.calm(),
            "and only the idle bob runs"
        );
    }

    #[test]
    fn no_trail_under_freeze_reduced_motion_a_seek_or_a_ride() {
        let mut still = Motion::new(true);
        moved(&mut still);
        assert!(trail_beams(&still, 1.0, None).is_empty(), "freeze");
        let mut reduced = Motion::default();
        reduced.set_reduced(true);
        moved(&mut reduced);
        assert!(
            trail_beams(&reduced, 1.0, None).is_empty(),
            "reduced motion"
        );
        let mut seek = Motion::default();
        seek.sync(&marks_of(vec![drone(
            "a",
            [0.0, 0.0, 40.0],
            [0.0, 30.0, 9.0],
            "idle",
        )]));
        seek.advance(ARRIVE_MS);
        seek.cut();
        seek.sync(&marks_of(vec![drone(
            "a",
            [200.0, 0.0, 40.0],
            [200.0, 30.0, 9.0],
            "idle",
        )]));
        assert_eq!(seek.body_at("a"), Some([200.0, 0.0, 40.0]), "a seek jumps");
        seek.advance(100.0);
        assert!(
            trail_beams(&seek, 1.0, None).is_empty(),
            "a seek leaves no trail"
        );
        let mut ride = Motion::default();
        moved(&mut ride);
        ride.advance(move_ms(200.0));
        assert!(!trail_beams(&ride, 1.0, None).is_empty());
        assert!(
            trail_beams(&ride, 1.0, Some("a")).is_empty(),
            "the ride's drone leaves none"
        );
    }

    #[test]
    fn a_working_drone_bobs_one_px_and_each_slot_has_its_own_phase() {
        let mut motion = Motion::default();
        motion.set_px_units(2.0);
        let marks = marks_of(vec![drone(
            "a",
            [0.0, 0.0, 40.0],
            [9.0, 0.0, 9.0],
            "building",
        )]);
        motion.sync(&marks);
        motion.advance(ARRIVE_MS);
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for _ in 0..48 {
            motion.advance(BOB_PERIOD_MS / 48.0);
            lo = lo.min(motion.bob(0, DroneState::Working));
            hi = hi.max(motion.bob(0, DroneState::Working));
        }
        assert!(
            ((hi - lo) - BOB_PX * 2.0).abs() < 0.05,
            "the bob spans 1 CSS px: {lo} to {hi}"
        );
        assert_ne!(
            motion.bob(0, DroneState::Working),
            motion.bob(1, DroneState::Working),
            "slots bob apart"
        );
        for state in [DroneState::Waiting, DroneState::Stale] {
            let mut still = drone("a", [0.0, 0.0, 40.0], [9.0, 0.0, 9.0], "idle");
            still.state = state;
            let shell = mark_instances(&motion, &marks_of(vec![still]), KINDS, 1.0, None)
                .into_iter()
                .find(|i| i.kind == KINDS.body)
                .expect("the drone draws");
            assert_eq!(shell.offset[2], 40.0, "a {state:?} drone does not bob");
        }
    }

    /// The span of a drone's bob over `period` ms, sampled 48 times, in world units.
    fn bob_span(motion: &mut Motion, state: DroneState, period: f32) -> f32 {
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for _ in 0..48 {
            motion.advance(period / 48.0);
            lo = lo.min(motion.bob(0, state));
            hi = hi.max(motion.bob(0, state));
        }
        hi - lo
    }

    #[test]
    fn an_idle_drone_bobs_lower_and_slower_than_a_working_one() {
        let (working, idle) = (
            DroneState::Working.bob().expect("a working drone bobs"),
            DroneState::Idle.bob().expect("an idle drone bobs"),
        );
        assert!(idle.0 < working.0, "lower: {idle:?} against {working:?}");
        assert!(idle.1 > working.1, "slower: {idle:?} against {working:?}");
        assert_eq!(
            DroneState::Waiting.bob(),
            None,
            "a waiting drone holds still"
        );
        assert_eq!(DroneState::Stale.bob(), None, "a stale drone holds still");

        let mut motion = Motion::default();
        motion.set_px_units(2.0);
        let marks = marks_of(vec![drone("a", [0.0, 0.0, 40.0], [9.0, 0.0, 9.0], "idle")]);
        motion.sync(&marks);
        motion.advance(ARRIVE_MS + TURN_MS);
        let span = bob_span(&mut motion, DroneState::Idle, IDLE_BOB_PERIOD_MS);
        assert!(
            (span - IDLE_BOB_PX * 2.0).abs() < 0.05,
            "the idle bob spans {IDLE_BOB_PX} CSS px: {span}"
        );
        let before = motion.bob(0, DroneState::Idle);
        motion.advance(IDLE_BOB_PERIOD_MS);
        assert!(
            (motion.bob(0, DroneState::Idle) - before).abs() < 1e-3,
            "and repeats every {IDLE_BOB_PERIOD_MS} ms"
        );
        let shell = mark_instances(&motion, &marks, KINDS, 1.0, None)
            .into_iter()
            .find(|i| i.kind == KINDS.body)
            .expect("the drone draws");
        assert_eq!(shell.offset[2], 40.0 + motion.bob(0, DroneState::Idle));
    }

    #[test]
    fn an_agents_name_bobs_with_its_drone() {
        let mut motion = Motion::default();
        motion.set_px_units(2.0);
        let marks = marks_of(vec![drone("a", [0.0, 0.0, 40.0], [9.0, 0.0, 9.0], "idle")]);
        motion.sync(&marks);
        motion.advance(ARRIVE_MS + TURN_MS);
        motion.advance(IDLE_BOB_PERIOD_MS / 4.0);
        let name = Flag {
            kind: FlagKind::Agent,
            text: "a".to_string(),
            hue: None,
            waiting: false,
            clash: None,
            check: None,
            leaving: false,
            detail: Vec::new(),
            scope: None,
            drone: None,
            on: None,
            machine: Some(0),
            at: [0.0, 0.0, 40.0],
            follow: Follow::Body {
                agent: "a".to_string(),
                slot: 0,
                state: DroneState::Idle,
            },
        };
        let bob = motion.bob(0, DroneState::Idle);
        assert_ne!(
            bob, 0.0,
            "the drone is off its rest at this point of its bob"
        );
        assert_eq!(
            motion.flag_at(&name),
            [0.0, 0.0, 40.0 + bob],
            "the name sits over the drone as it is now"
        );
    }

    #[test]
    fn reduced_motion_or_freeze_holds_an_idle_drone_and_asks_for_no_frames() {
        for (still, reduced) in [(false, true), (true, false)] {
            let mut motion = Motion::new(still);
            motion.set_reduced(reduced);
            motion.set_px_units(2.0);
            motion.sync(&marks_of(vec![drone(
                "a",
                [0.0, 0.0, 40.0],
                [9.0, 0.0, 9.0],
                "idle",
            )]));
            motion.advance(ARRIVE_MS + TURN_MS);
            assert!(!motion.advance(IDLE_BOB_PERIOD_MS / 4.0), "no frames");
            assert_eq!(motion.bob(0, DroneState::Idle), 0.0, "and no bob");
        }
    }

    #[test]
    fn the_loop_runs_only_while_a_drone_bobs_and_runs_calm_for_idle_drones_alone() {
        let settled = |states: &[DroneState]| {
            let mut motion = Motion::default();
            motion.set_px_units(2.0);
            let bodies = states
                .iter()
                .enumerate()
                .map(|(i, &state)| {
                    let x = 100.0 * i as f32;
                    let mut body = drone(&format!("d{i}"), [x, 0.0, 40.0], [x, 9.0, 9.0], "idle");
                    body.state = state;
                    body
                })
                .collect();
            motion.sync(&marks_of(bodies));
            motion.advance(ARRIVE_MS + TURN_MS);
            let more = motion.advance(16.0);
            (more, motion.calm())
        };
        assert_eq!(
            settled(&[DroneState::Waiting, DroneState::Stale]),
            (false, false),
            "no drone bobs, so no frames"
        );
        assert_eq!(settled(&[]), (false, false), "no drones, no frames");
        assert_eq!(
            settled(&[DroneState::Idle, DroneState::Waiting]),
            (true, true),
            "an idle drone keeps a calm loop"
        );
        assert_eq!(
            settled(&[DroneState::Idle, DroneState::Working]),
            (true, false),
            "a working drone keeps the full rate"
        );
    }

    #[test]
    fn a_drone_on_its_machines_centre_keeps_the_heading_it_had() {
        let at = [0.0, 0.0, PLINTH_TOP];
        let mut motion = Motion::new(true);
        motion.sync(&marks_of(vec![drone(
            "a",
            at,
            [0.0, 1.0, PLINTH_TOP],
            "idle",
        )]));
        motion.sync(&marks_of(vec![drone("a", at, at, "idle")]));
        let kept = motion.heading_of("a").expect("the drone keeps its heading");
        assert!((kept - PI / 2.0).abs() < 1e-6, "still faces north: {kept}");
    }

    #[test]
    fn a_beam_starts_on_the_eye_in_the_drones_frame_and_ends_on_its_target() {
        let at = [2.0, 3.0, 40.0];
        let target = [5.0, 3.0, 9.0];
        let marks = marks_of(vec![drone("a", at, target, "building")]);
        let mut motion = Motion::new(true);
        motion.sync(&marks);
        let beams = beams(&motion, &marks, 2.0, None, None);
        assert_eq!(beams.len(), 1, "one beam per drone");
        let beam = &beams[0];
        assert_eq!(beam.to, target, "the beam ends on the target");
        assert_eq!(
            beam.origin, at,
            "the beam is measured from the drone's centre"
        );
        assert_eq!(
            beam.grow,
            drone_grow(2.0),
            "the beam grows as the drone does"
        );
        let facing = motion.heading_of("a").expect("the drone faces east");
        assert_eq!(beam.turn, facing, "the beam turns as the drone does");
        assert_eq!(beam.from, eye_point(), "the beam starts on the lens");
    }

    #[test]
    fn the_eye_aims_at_the_machine_the_drone_works_on() {
        let at = [0.0, 0.0, 40.0];
        let target = [30.0, 40.0, 9.0];
        let marks = marks_of(vec![drone("a", at, target, "building")]);
        let mut motion = Motion::new(true);
        motion.sync(&marks);
        let beam = beams(&motion, &marks, 1.0, None, None)[0];
        let body = mark_instances(&motion, &marks, KINDS, 1.0, None)
            .into_iter()
            .find(|i| i.kind == KINDS.body)
            .expect("the drone draws");
        assert_eq!(body.turn, beam.turn, "the tether turns with the body");
        let (s, c) = beam.turn.sin_cos();
        let [x, y, z] = beam.from;
        let eye = [c * x - s * y, s * x + c * y, z];
        let ahead = [target[0] - at[0], target[1] - at[1]];
        let reach = ahead[0].hypot(ahead[1]);
        let flat = eye[0].hypot(eye[1]);
        let along = (eye[0] * ahead[0] + eye[1] * ahead[1]) / (flat * reach);
        assert!(
            (along - 1.0).abs() < 1e-5,
            "the eye faces the target: {along}"
        );
        assert!(eye[2] < 0.0, "and looks down at it from above");
        let gap = |p: [f32; 3]| {
            (0..3)
                .map(|k| (p[k] - target[k]).powi(2))
                .sum::<f32>()
                .sqrt()
        };
        let eye_at = [at[0] + eye[0], at[1] + eye[1], at[2] + eye[2]];
        assert!(
            gap(eye_at) < gap(at),
            "the eye is the near side of the body"
        );
    }

    #[test]
    fn a_drone_at_map_zoom_keeps_to_three_instances_and_eight_strokes() {
        let at = [0.0, 0.0, 40.0];
        let target = [9.0, 0.0, 9.0];
        let always = |model: &acan_render::Model| {
            model
                .lines
                .iter()
                .filter(|l| l.detail == acan_render::Detail::Always)
                .count()
                + model
                    .dots
                    .iter()
                    .filter(|d| d.detail == acan_render::Detail::Always)
                    .count()
        };
        let model_strokes = always(&acan_iso::agent_body()) + always(&acan_iso::agent_accent());
        for activity in ["building", "idle"] {
            let marks = marks_of(vec![drone("a", at, target, activity)]);
            let mut motion = Motion::new(true);
            motion.sync(&marks);
            let instances = mark_instances(&motion, &marks, KINDS, 1.0, None).len();
            let strokes = model_strokes
                + beams(&motion, &marks, 1.0, None, None).len()
                + spots(&motion, &marks, 1.0, None).len();
            assert!(instances <= 3, "{activity}: {instances} instances");
            assert!(strokes <= 8, "{activity}: {strokes} strokes");
        }
    }

    #[test]
    fn only_a_working_agents_beacon_pulses() {
        let at = [0.0, 0.0, PLINTH_TOP];
        let target = [4.0, 0.0, 9.0];
        let marks = marks_of(vec![
            drone("idle", at, target, "idle"),
            drone("busy", at, target, "building"),
        ]);
        let mut motion = Motion::default();
        motion.sync(&marks);
        motion.advance(PULSE_MS / 4.0);
        let beacons: Vec<f32> = mark_instances(&motion, &marks, KINDS, 1.0, None)
            .into_iter()
            .filter(|instance| instance.kind == KINDS.beacon)
            .map(|instance| instance.grow[0])
            .collect();
        assert_eq!(beacons.len(), 1, "only the working drone has a beacon");
        assert!(
            beacons[0] > BEACON_PX,
            "a working beacon swells: {}",
            beacons[0]
        );
    }

    #[test]
    fn a_working_beacon_has_a_disc_for_its_paper_ring_and_an_idle_drone_none() {
        let at = [0.0, 0.0, PLINTH_TOP];
        let target = [4.0, 0.0, 9.0];
        let marks = marks_of(vec![
            drone("idle", at, target, "idle"),
            drone("busy", at, target, "building"),
        ]);
        let mut motion = Motion::default();
        motion.sync(&marks);
        let flat = |p: [f32; 3]| [p[0], -p[2]];
        let rest = beacon_discs(&motion, &marks, flat);
        assert_eq!(rest.len(), 1, "only the working drone has a beacon");
        assert_eq!(rest[0].0, [4.0, -9.0], "the disc is centred on the beacon");
        assert!(
            (rest[0].1 - BEACON_PX / 2.0).abs() < 1e-4,
            "at rest it is the floor"
        );
        motion.advance(PULSE_MS / 2.0);
        let swollen = beacon_discs(&motion, &marks, flat);
        assert!(swollen[0].1 > rest[0].1, "the ring follows the pulse");
    }

    #[test]
    fn each_state_sets_its_tether_eye_and_a_stale_drone_sits_lower() {
        let at = [0.0, 0.0, 40.0];
        let target = [4.0, 0.0, 9.0];
        let with = |agent: &str, state: DroneState| Body {
            state,
            ..drone(agent, at, target, "building")
        };
        let marks = marks_of(vec![
            with("work", DroneState::Working),
            with("idle", DroneState::Idle),
            with("wait", DroneState::Waiting),
            with("stale", DroneState::Stale),
        ]);
        let mut motion = Motion::new(true);
        motion.sync(&marks);
        let alphas: Vec<f32> = beams(&motion, &marks, 2.0, None, None)
            .iter()
            .map(|beam| beam.alpha)
            .collect();
        assert_eq!(alphas, [0.9, 0.3, 0.9, 0.2]);
        assert!(
            beams(&motion, &marks, 2.0, None, None)
                .iter()
                .all(|beam| beam.width_px == TETHER_PX * 2.0),
            "the tether is 1.5 CSS px"
        );
        let instances = mark_instances(&motion, &marks, KINDS, 1.0, None);
        let tints = |kind: u16| -> Vec<u8> {
            instances
                .iter()
                .filter(|i| i.kind == kind)
                .map(|i| i.tint)
                .collect()
        };
        assert_eq!(
            tints(KINDS.body),
            [0; 4],
            "the body is white in every state"
        );
        let dim = u8::try_from(TINT_DIM0 + TINT_BODY0).expect("fits");
        assert_eq!(
            tints(KINDS.accent),
            [
                body_tint(0),
                dim,
                dim,
                u8::try_from(TINT_STALE).expect("fits")
            ],
            "the eye is bright at work, dim at idle and while waiting, and grey once stale"
        );
        let shadow = u8::try_from(TINT_SHADOW).expect("fits");
        let all = spots(&motion, &marks, 1.0, None);
        assert!(
            all.iter().all(|spot| spot.tint == shadow),
            "a drone's only spot is its shadow"
        );
        assert_eq!(all.len(), 4, "every drone has a shadow, a stale one too");
        let ring = instances
            .iter()
            .find(|i| i.kind == KINDS.ring)
            .expect("the waiting drone has a ring");
        assert_eq!(ring.offset, at, "the ring sits at the body's height");
        assert_eq!(
            instances.iter().filter(|i| i.kind == KINDS.beacon).count(),
            1,
            "only the working drone has a beacon"
        );
        assert_eq!(seat_height(40.0, DroneState::Stale), 40.0 - STALE_DROP);
        assert_eq!(seat_height(40.0, DroneState::Working), 40.0);
    }

    #[test]
    fn a_still_motion_keeps_every_drone_beam_and_beacon_where_they_are() {
        let at = [0.0, 0.0, PLINTH_TOP];
        let target = [4.0, 0.0, 9.0];
        let marks = marks_of(vec![drone("busy", at, target, "building")]);
        let mut motion = Motion::new(true);
        motion.sync(&marks);
        let before = mark_instances(&motion, &marks, KINDS, 1.0, None);
        let beams_before = beams(&motion, &marks, 1.0, None, None);
        assert!(!motion.advance(PULSE_MS / 3.0), "nothing moves while still");
        assert_eq!(mark_instances(&motion, &marks, KINDS, 1.0, None), before);
        assert_eq!(beams(&motion, &marks, 1.0, None, None), beams_before);
    }

    #[test]
    fn a_riding_drone_keeps_its_beacon_and_loses_its_body_shadow_and_beam() {
        let at = [2.0, 3.0, 40.0];
        let target = [5.0, 3.0, 9.0];
        let marks = marks_of(vec![
            drone("a", at, target, "building"),
            drone("b", [9.0, 3.0, 40.0], target, "idle"),
        ]);
        let mut motion = Motion::new(true);
        motion.sync(&marks);
        let instances = mark_instances(&motion, &marks, KINDS, 1.0, Some("a"));
        let count = |kind: u16| instances.iter().filter(|i| i.kind == kind).count();
        assert_eq!(count(KINDS.body), 1, "only the other drone has a body");
        assert_eq!(count(KINDS.accent), 1, "only the other drone has an accent");
        assert_eq!(
            count(KINDS.beacon),
            1,
            "the riding drone keeps its beacon, and the idle one has none"
        );
        let ridden = beams(&motion, &marks, 1.0, Some("a"), None);
        assert_eq!(ridden.len(), 1, "the riding drone has no beam");
        assert_eq!(ridden[0].origin, [9.0, 3.0, 40.0]);
        let spots = spots(&motion, &marks, 1.0, Some("a"));
        assert_eq!(spots.len(), 1, "the riding drone has no shadow");
        assert!(spots.iter().all(|s| s.origin[..2] == [9.0, 3.0]));
    }

    #[test]
    fn a_shadow_keeps_a_readable_minimum_however_high_its_drone_hovers() {
        assert!(
            (0.18..=0.25).contains(&SHADOW_MIN_ALPHA),
            "the minimum is a readable share: {SHADOW_MIN_ALPHA}"
        );
        let target = [5.0, 3.0, 9.0];
        let shadow = u8::try_from(TINT_SHADOW).expect("fits");
        for height in [4.0 * BODY_RADIUS, 20.0 * BODY_RADIUS] {
            let at = [2.0, 3.0, PLINTH_TOP + height];
            let body = Body {
                ground: PLINTH_TOP,
                ..drone("hover", at, target, "idle")
            };
            let marks = marks_of(vec![body]);
            let mut motion = Motion::new(true);
            motion.sync(&marks);
            let spot = spots(&motion, &marks, 1.0, None)
                .into_iter()
                .find(|s| s.tint == shadow)
                .expect("the drone casts a shadow");
            assert!(
                spot.alpha >= SHADOW_MIN_ALPHA,
                "a shadow at {height} keeps its minimum: {}",
                spot.alpha
            );
        }
    }

    #[test]
    fn a_higher_drone_casts_a_smaller_fainter_shadow_under_its_centre() {
        let target = [5.0, 3.0, 9.0];
        let low = [2.0, 3.0, PLINTH_TOP + 1.5 * BODY_RADIUS];
        let high = [20.0, 3.0, PLINTH_TOP + 4.0 * BODY_RADIUS];
        // Both hover over the plinth, so the surface under each is its top.
        let over_plinth = |agent: &str, at| Body {
            ground: PLINTH_TOP,
            ..drone(agent, at, target, "idle")
        };
        let marks = marks_of(vec![over_plinth("low", low), over_plinth("high", high)]);
        let mut motion = Motion::new(true);
        motion.sync(&marks);
        let shadow = u8::try_from(TINT_SHADOW).expect("fits");
        let shadows: Vec<Beam> = spots(&motion, &marks, 1.0, None)
            .into_iter()
            .filter(|spot| spot.tint == shadow)
            .collect();
        assert_eq!(shadows.len(), 2);
        let (under_low, under_high) = (&shadows[0], &shadows[1]);
        assert_eq!(under_low.origin, [low[0], low[1], PLINTH_TOP]);
        assert_eq!(under_high.origin, [high[0], high[1], PLINTH_TOP]);
        assert!(
            under_high.spot < under_low.spot,
            "the higher shadow is smaller"
        );
        assert!(under_high.alpha < under_low.alpha, "and fainter");
        assert!(under_low.spot <= acan_iso::SHADOW_RADIUS);
        assert_eq!(
            under_low.grow,
            drone_grow(1.0),
            "a shadow grows with its drone at the screen floor"
        );
    }

    /// The scripted view with tasks landed on src/api at `api` and on src/ui at `ui`.
    fn landed_view(plan: &Plan, api: &[u64], ui: &[u64]) -> View {
        let (_origin, log, _index) = scripted();
        let mut view = Mirror::from_log(log).expect("the log replays").view(plan);
        view.machines[machine(plan, "src/api")].landings = api.to_vec();
        view.machines[machine(plan, "src/ui")].landings = ui.to_vec();
        view
    }

    fn ghosts_at(view: &View, plan: &Plan, now: Option<u64>, seen: Option<u64>) -> Vec<Ghost> {
        let slots = slots_for(view);
        let away = seen.map(|seen| Away { seen, back: 20_000 });
        Marks::build_at(view, plan, &stands(plan), &slots, Clock { now, away }).ghosts
    }

    #[test]
    fn a_machine_where_work_landed_while_the_reader_was_away_has_a_ghost() {
        let plan = plan();
        let view = landed_view(&plan, &[5_000, 9_000, 12_000], &[7_000]);
        let ghosts = ghosts_at(&view, &plan, Some(30_000), Some(8_000));
        assert_eq!(ghosts.len(), 1, "only src/api landed after last seen");
        let ghost = ghosts[0];
        assert_eq!(ghost.machine, machine(&plan, "src/api"));
        assert_eq!(ghost.count, 2, "two of its tasks landed since");
        assert_eq!(ghost.at, 12_000);
        let footprint = &plan.machines[ghost.machine];
        assert_eq!(
            ghost.rect,
            [
                footprint.min[0] - GHOST_OFFSET,
                footprint.min[1] - GHOST_OFFSET,
                footprint.size[0] + 2.0 * GHOST_OFFSET,
                footprint.size[1] + 2.0 * GHOST_OFFSET,
            ],
            "the outline stands out from the footprint"
        );
        assert!(ghost.corners().iter().all(|corner| corner[2] == 0.0));
        let later = landed_view(&plan, &[5_000, 9_000, 12_000], &[7_000, 25_000]);
        let ghosts = ghosts_at(&later, &plan, Some(30_000), Some(8_000));
        assert_eq!(
            ghosts.len(),
            1,
            "work that lands after the reader is back has no ghost"
        );
        let both = ghosts_at(&view, &plan, Some(20_000), Some(6_000));
        assert_eq!(
            both.iter().map(|ghost| ghost.at).collect::<Vec<_>>(),
            [12_000, 7_000],
            "latest first"
        );
    }

    #[test]
    fn a_frozen_or_replayed_page_or_one_never_left_has_no_ghosts() {
        let plan = plan();
        let view = landed_view(&plan, &[9_000], &[9_000]);
        assert!(
            ghosts_at(&view, &plan, None, Some(1_000)).is_empty(),
            "no clock"
        );
        assert!(
            ghosts_at(&view, &plan, Some(20_000), None).is_empty(),
            "never away"
        );
        assert!(
            ghosts_at(&view, &plan, Some(20_000), Some(9_000)).is_empty(),
            "seen since"
        );
    }

    #[test]
    fn ghosts_keep_the_latest_dozen() {
        let small = plan();
        let template = landed_view(&small, &[], &[]).machines[0].clone();
        let mut plan = Plan::default();
        let mut view = landed_view(&small, &[], &[]);
        view.machines.clear();
        for i in 0..20_u64 {
            let name = format!("m{i}");
            plan.machines.push(crate::plan::PlanMachine {
                name: name.clone(),
                min: [i as f32 * 100.0, 0.0],
                size: [80.0, 80.0],
            });
            view.machines.push(crate::mirror::MachineView {
                name,
                landings: vec![2_000 + i],
                ..template.clone()
            });
        }
        let clock = Clock {
            now: Some(20_000),
            away: Some(Away {
                seen: 1_000,
                back: 20_000,
            }),
        };
        let ghosts = ghosts(&view, &plan, clock);
        assert_eq!(ghosts.len(), GHOST_MAX);
        assert_eq!(ghosts[0].machine, 19, "the latest landing first");
        assert_eq!(ghosts[GHOST_MAX - 1].machine, 20 - GHOST_MAX);
    }

    #[test]
    fn a_ghost_is_a_grey_loop_that_fades_once_cleared_and_stays_gone() {
        let plan = plan();
        let view = landed_view(&plan, &[9_000], &[]);
        let clock = Clock {
            now: Some(20_000),
            away: Some(Away {
                seen: 1_000,
                back: 20_000,
            }),
        };
        let marks = Marks::build_at(&view, &plan, &stands(&plan), &slots_for(&view), clock);
        let api = machine(&plan, "src/api");
        let mut motion = Motion::new(false);
        motion.sync(&marks);
        let loop_ = ghost_beams(&motion, &marks, 2.0);
        assert_eq!(loop_.len(), 4, "one stroke loop");
        assert!(loop_.iter().all(|beam| {
            u32::from(beam.tint) == TINT_GHOST
                && beam.alpha == GHOST_ALPHA
                && beam.width_px == GHOST_PX * 2.0
                && beam.from[2] == 0.0
        }));
        for (i, beam) in loop_.iter().enumerate() {
            assert_eq!(beam.to, loop_[(i + 1) % 4].from, "the sides close");
        }
        assert!(!motion.advance(1_000.0), "a ghost at rest needs no frames");
        assert!(motion.clear_ghost(api));
        assert!(!motion.clear_ghost(api), "it is already fading");
        assert!(motion.advance(GHOST_FADE_MS / 2.0), "the fade runs");
        let half = motion.ghost_alpha(api).expect("still fading");
        assert!((half - GHOST_ALPHA / 2.0).abs() < 1e-4);
        assert!(!motion.advance(GHOST_FADE_MS));
        assert_eq!(motion.ghost_alpha(api), None);
        assert!(ghost_beams(&motion, &marks, 2.0).is_empty());
        motion.sync(&marks);
        assert_eq!(motion.ghost_alpha(api), None, "a cleared ghost stays gone");
        let newer = landed_view(&plan, &[9_000, 15_000], &[]);
        let marks = Marks::build_at(&newer, &plan, &stands(&plan), &slots_for(&newer), clock);
        motion.sync(&marks);
        assert_eq!(
            motion.ghost_alpha(api),
            Some(GHOST_ALPHA),
            "a newer landing brings it back"
        );
        assert!(motion.clear_ghosts());
        assert!(!motion.advance(GHOST_FADE_MS));
        assert_eq!(motion.ghost_alpha(api), None);
    }

    #[test]
    fn a_point_is_inside_a_quad_by_the_even_odd_rule() {
        let quad = [[0.0, 0.0], [10.0, 2.0], [12.0, 12.0], [-2.0, 10.0]];
        assert!(inside_quad(&quad, [5.0, 5.0]));
        assert!(!inside_quad(&quad, [11.0, 1.0]));
        assert!(!inside_quad(&quad, [-5.0, 5.0]));
        assert!(!inside_quad(&[], [0.0, 0.0]));
    }
}
