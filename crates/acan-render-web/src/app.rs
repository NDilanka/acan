//! The wasm surface the page drives. Pointer and key input arrive in CSS pixels, and the camera
//! works in device pixels, so this file scales input by the device pixel ratio. Positions that
//! cross back to JavaScript are CSS pixels.
//!
//! The plant is drawn in 3D from the scan JSON. The origin's log folds into a view on the next
//! frame after it changes, and the page reads that view back as JSON for its HTML panels.

use std::collections::HashMap;

use acan_render::camera::{DEFAULT_AZIMUTH, DEFAULT_ELEVATION, FitRoom};
use acan_render::{Aabb, Camera, Lod, Model, Renderer, Scene, wgpu};
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::api;
use crate::canvas;
use crate::feed;
use crate::kind::{self, ALL, PlanFootprint};
use crate::labels::{self, LabelSpot};
use crate::layout::{Layout, layout};
use crate::marks::{self, AgentSlots, Marks, Motion, Stand};
use crate::mirror::{Mirror, View};
use crate::plan::Plan;
use crate::plant::{Plant, PlantMachine, from_scan_json, rest_labels, stress};
use crate::plant_view;
use crate::pov::{self, Ride};
use crate::project::{self, ProjectMarks};
use crate::scene;
use crate::seal;
use crate::stand;
use crate::topdown;
use crate::tour;

/// Space around the plant at fit, in CSS pixels.
const FIT_MARGIN_CSS: f32 = 24.0;
/// Extra room the fit leaves under the plant, in CSS pixels. The machines' names are HTML hung
/// under the plates, so the framing boxes cannot size them. Larger than a name's height, so the
/// bottom margin matches the room the flag stack takes at the top; see `FIT_FLAG_TOP_CSS`.
const FIT_LABEL_CSS: f32 = 65.0;
/// Extra room the fit leaves at the right edge for the flag chips, in CSS pixels. A chip
/// extends right of its anchor above a drone, and the framing boxes cannot size it.
const FIT_FLAG_RIGHT_CSS: f32 = 60.0;
/// Extra room the fit leaves above the plant for the flag stack, in CSS pixels. A chip row is
/// about 30 pixels, and four rows stack above a machine in a clash. Measured against the shots:
/// three rows leave about 70 pixels of margin, four about 40.
const FIT_FLAG_TOP_CSS: f32 = 100.0;
/// Degrees of orbit per CSS pixel of drag.
const ORBIT_DEG_PER_PX: f32 = 0.4;
/// Degrees of tilt per CSS pixel of drag.
const TILT_DEG_PER_PX: f32 = 0.25;
/// Duration of a Q or E turn, in milliseconds.
const TURN_MS: f32 = 180.0;
/// The gap between frames, in ms, while only the idle bob runs: about 20 frames a second. The bob
/// moves a few hundredths of a pixel in that time, so the lower rate does not show.
const CALM_FRAME_MS: f32 = 50.0;
/// How far a drone reaches from its centre, its waiting ring included, in world units. A close-up
/// keeps this much of each drone clear of the page's side panels.
const FOCUS_REACH: f32 = 1.4 * acan_iso::BODY_RADIUS;

/// A close-up's hold: the CSS band its drones keep within, the world point it centres on, and
/// the camera as it left it, so a move by the reader ends the hold.
#[derive(Debug, Clone, Copy)]
struct FocusHold {
    band: [f32; 2],
    centre: [f32; 3],
    camera: Camera,
}

/// How many steps the top-down fit takes to bring the plant inside the screen edges it keeps clear.
const CLEAR_STEPS: usize = 12;

/// Fits `boxes` on `camera` at its angles, with `room`, as `Camera::fit_boxes_room` does. With
/// `clear`, the top-down fit, the corners must also stay inside the screen edges it names, as
/// `[top, right]` in device pixels. The kit's fit shifts the picture to clear the usage panel, which
/// can push the plant under the shell bar or past the left edge. A plant too big for the room the
/// edges leave zooms out to fit it. Otherwise a corner past an edge pans the picture back by its
/// reach.
fn fit_clear(
    camera: &mut Camera,
    boxes: &[Aabb],
    margin: f32,
    room: FitRoom,
    clear: Option<[f32; 2]>,
) {
    camera.fit_boxes_room(boxes, margin, room);
    let Some([top, right]) = clear else {
        return;
    };
    let corners: Vec<[f32; 3]> = boxes.iter().flat_map(|bounds| bounds.corners()).collect();
    let [width, height] = camera.viewport();
    let centre = [width * 0.5, height * 0.5];
    let room_x = right.max(1.0);
    let room_y = (height - top).max(1.0);
    for _ in 0..CLEAR_STEPS {
        let screen: Vec<[f32; 2]> = corners
            .iter()
            .map(|&corner| camera.world_to_screen(corner))
            .collect();
        let [above, beyond, left] = topdown::overlap(&screen, top, right);
        if above == 0.0 && beyond == 0.0 && left == 0.0 {
            break;
        }
        let [lo_x, hi_x, lo_y, hi_y] = screen.iter().fold(
            [
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
            ],
            |[lo_x, hi_x, lo_y, hi_y], &[x, y]| {
                [lo_x.min(x), hi_x.max(x), lo_y.min(y), hi_y.max(y)]
            },
        );
        // The plant's span on screen against the room. A span past the room means it cannot fit at
        // this zoom, so the zoom comes down a little and the next step measures again.
        let fit = (room_x / (hi_x - lo_x))
            .min(room_y / (hi_y - lo_y))
            .min(1.0);
        if fit < 1.0 {
            camera.set_zoom(camera.zoom() * fit * 0.98);
            continue;
        }
        // The ground under the centre moves down by `above`, left by `beyond` and right by `left`,
        // which moves the picture the same way.
        camera.drag_ground(centre, [centre[0] - beyond + left, centre[1] + above]);
    }
}

#[wasm_bindgen]
pub struct App {
    renderer: Renderer,
    camera: Camera,
    models: Vec<Model>,
    /// The free spots a bot can stand on, per model kind, relative to the model's origin.
    kind_spots: Vec<Vec<[f32; 2]>>,
    dpr: f32,
    turn: Option<Turn>,
    /// The camera's move to the tour step on screen, while one runs.
    tour_move: Option<TourMove>,
    /// Whether the view is top-down, the camera looking straight down. See [`topdown`].
    top_down: bool,
    /// The tilt the view had before it turned top-down, which turning back restores.
    tilt_before_top: f32,
    /// The camera's turn to top-down, or back to the default tilt, while one runs.
    top_move: Option<TopMove>,
    /// Whether the top-down view is set once the ride has eased back, as `V` asks while riding.
    top_after_leave: Option<bool>,
    /// The room the top-down fit keeps clear of the shell bar at the top and of the right column,
    /// as `[top, right]` in CSS pixels. See [`App::set_fit_inset`].
    fit_inset: [f32; 2],
    /// The machines the shown tour step stages, with the chip labels on each. Empty when no step
    /// is shown, so every machine keeps its normal tint.
    stage: Vec<tour::Spot>,
    plant: Option<Loaded>,
    plan: Option<Plan>,
    mirror: Option<Mirror>,
    /// The view as of the last rebuild. The marks and the clash card are read from it.
    view: Option<View>,
    marks: Marks,
    motion: Motion,
    /// Each agent's hue slot. It keeps a slot while the agent is in the view.
    slots: AgentSlots,
    /// Whether the marks' eases were moving on the last frame.
    moving: bool,
    /// Whether the last frame ran only for the idle bob, see [`App::frame_gap_ms`].
    calm: bool,
    /// Whether the marks hold still, as `?freeze=1` asks. Set once before a plant loads.
    still: bool,
    /// The index of the clash whose card is open, if any.
    card_clash: Option<usize>,
    /// An agent to close up on, the zoom as a share of the fit zoom, and the CSS band the page's
    /// panels leave free. It applies once the view places the agent's drone, then clears.
    focus: Option<(String, f32, Option<[f32; 2]>)>,
    /// A close-up that keeps its drones clear of the side panels until the reader moves it.
    focus_hold: Option<FocusHold>,
    /// The ride, while the page shows one agent's drone from inside.
    ride: Option<Ride>,
    /// Whether the fit runs once the ride has eased back, as the `0` key asks.
    fit_after_leave: bool,
    /// Whether a ride's camera bobs, as the page's reduced-motion setting allows.
    bob_enabled: bool,
    /// Where the bob is, in milliseconds through its period.
    bob_ms: f32,
    view_json: String,
    view_version: u32,
    dirty: bool,
    /// The page's clock for the marks: the time now on a live page, and when the reader last saw
    /// it. A still page keeps no time, so nothing on it reads stale.
    clock: marks::Clock,
    /// The agent whose drone the pointer is over, if any.
    hovered: Option<String>,
    /// The id of the project on screen when the canvas holds several, or empty for one plant.
    project: String,
    /// The other projects on the canvas. Each keeps its own marks state, so its agents, hue slots
    /// and eases never meet the ones on screen, and draws them at its place.
    parked: Vec<Parked>,
    /// Every project's id in the order it came onto the canvas. An entry of an every-project
    /// overlay names its project by its place here.
    order: Vec<String>,
    /// Moves when a parked project's marks or level of detail change, so the page redraws their
    /// flags.
    parked_version: u32,
    /// Each plant's layout from the origin, by `canvas::layout_key`.
    layouts: HashMap<u64, Layout>,
}

/// A project on the canvas that is not on screen: its plant at its place and its log folded, so
/// the page can list what waits there and switch to it without fetching again.
struct Parked {
    id: String,
    loaded: Loaded,
    plan: Plan,
    mirror: Option<Mirror>,
    /// The view as JSON, until the next event changes it.
    view_json: Option<String>,
    marks: ProjectMarks,
    /// Whether its marks draw at the far level of detail. See `canvas::far`.
    far: bool,
}

/// An ease from one azimuth to another, in degrees. `to` is not wrapped, so repeated turns add up.
struct Turn {
    from: f32,
    to: f32,
    elapsed_ms: f32,
}

/// An eased move of the camera's pivot and zoom to a tour step, from where it was when the step
/// began. See [`tour::progress`].
struct TourMove {
    from: tour::Pose,
    to: tour::Pose,
    elapsed_ms: f32,
}

/// The camera's turn to top-down, back, or a grid step: the pivot and zoom, the elevation and the
/// azimuth, each from where they were to where the fit puts them. See [`topdown`].
struct TopMove {
    from_pose: tour::Pose,
    to_pose: tour::Pose,
    from_elevation: f32,
    to_elevation: f32,
    from_azimuth: f32,
    to_azimuth: f32,
    elapsed_ms: f32,
}

/// The plant on screen.
struct Loaded {
    repo: String,
    machines: Vec<PlantMachine>,
    layout: Layout,
    /// The boxes the camera frames: the machines and their drone seats, without belts. See
    /// `scene::fit_boxes`.
    framing: Vec<Aabb>,
    /// The solid box of every machine, each with its machine index, for label occlusion.
    occluders: Vec<(usize, Aabb)>,
    /// Each machine's label half width in CSS pixels, estimated from its name.
    label_half_px: Vec<f32>,
    /// Whether each machine carries a name at rest once it is large enough on screen.
    rest: Vec<bool>,
    /// The machines and belts as the layout places them, before any mark is added.
    base: Scene,
    /// For each plan machine, its instance in `base`.
    plan_instance: Vec<usize>,
    /// For each plan machine, its model's top and the free spots on its plinth top.
    stands: Vec<Stand>,
    /// Each machine's model box, which a ride sizes its distance from, in the layout's machine
    /// order. See `scene::machine_model_boxes`.
    machine_bounds: Vec<Aabb>,
    /// The plant as it arrived and its layout from the origin, so a move rebuilds it in place.
    source: Plant,
    at_origin: Layout,
}

#[wasm_bindgen]
impl App {
    /// Builds the renderer on `canvas`. `backend` is `"webgpu"` or `"webgl"`. Set the canvas
    /// drawing size before calling, then call `resize`.
    pub async fn create(canvas: HtmlCanvasElement, backend: String) -> Result<App, JsError> {
        std::panic::set_hook(Box::new(|info| {
            web_sys::console::error_1(&info.to_string().into());
        }));
        let backends = match backend.as_str() {
            "webgpu" => wgpu::Backends::BROWSER_WEBGPU,
            "webgl" => wgpu::Backends::GL,
            other => return Err(JsError::new(&format!("unknown backend {other:?}"))),
        };
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let size = [canvas.width().max(1), canvas.height().max(1)];
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(js_err)?;
        let mut renderer = Renderer::new(&instance, surface, size)
            .await
            .map_err(js_err)?;
        let models = scene::models();
        renderer.set_models(&models);
        // A drone hovers beside each machine, behind it on either side of the default view, so its
        // eye turns toward the camera. See `stand::beside`.
        let drone = &models[usize::from(scene::mark_kinds().body)];
        let kind_spots = ALL
            .iter()
            .map(|&kind| {
                let (w, d) = kind.footprint();
                stand::beside(&models[kind::index(kind)], [w / 2.0, d / 2.0], drone)
            })
            .collect();
        Ok(App {
            renderer,
            camera: Camera::new([size[0] as f32, size[1] as f32]),
            models,
            kind_spots,
            dpr: 1.0,
            turn: None,
            tour_move: None,
            top_down: false,
            tilt_before_top: DEFAULT_ELEVATION,
            top_move: None,
            top_after_leave: None,
            fit_inset: [0.0; 2],
            stage: Vec::new(),
            plant: None,
            plan: None,
            mirror: None,
            view: None,
            marks: Marks::default(),
            motion: Motion::default(),
            slots: AgentSlots::default(),
            moving: false,
            calm: false,
            still: false,
            card_clash: None,
            focus: None,
            focus_hold: None,
            ride: None,
            fit_after_leave: false,
            bob_enabled: true,
            bob_ms: 0.0,
            clock: marks::Clock::default(),
            hovered: None,
            view_json: String::new(),
            view_version: 0,
            dirty: false,
            project: String::new(),
            parked: Vec::new(),
            order: Vec::new(),
            parked_version: 0,
            layouts: HashMap::new(),
        })
    }

    /// Backend and adapter in use, with the scene sample count.
    pub fn backend(&self) -> String {
        let info = self.renderer.adapter_info();
        format!(
            "{:?} / {} / {:?} / msaa {}",
            info.backend,
            info.name,
            self.renderer.surface_format(),
            self.renderer.samples()
        )
    }

    /// Sets the plant from `GET /api/plant` and fits the view to it. Returns the machine count.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON is not the plant's shape.
    pub fn set_plant(&mut self, json: &str) -> Result<u32, JsError> {
        let plant = from_scan_json(json).map_err(js_err)?;
        Ok(self.load(plant))
    }

    /// Checks one `plant-view/1` view, as `GET /api/orders/{order}/plant-view` answers it, and
    /// returns its summary as JSON. The page takes its numbers from the summary only after this
    /// passes.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON is not a `plant-view/1` view, or when its counts do not match
    /// its items.
    pub fn plant_view_summary(&self, json: &str) -> Result<String, JsError> {
        let view = plant_view::decode(json).map_err(js_err)?;
        Ok(plant_view::summarize(&view).to_string())
    }

    /// Checks one `plant-view/1` view and returns what the page plays over it, as JSON: the view's
    /// own counts and items, the mode, the chip's model, why the order plays, and each step's
    /// camera, staged marks and caption. Every caption is filled from bound evidence. The view's
    /// tour is admitted or the deterministic order is built and admitted, and the page says which.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON is not a `plant-view/1` view, its counts do not match its
    /// items, or no order can be admitted.
    pub fn tour_load(&self, json: &str) -> Result<String, JsError> {
        let play = tour::play(json).map_err(js_err)?;
        serde_json::to_string(&play).map_err(js_err)
    }

    /// Eases the camera to the pivot and zoom of a tour step: the machine `machine_id` (its name in
    /// the plant) at the zoom `zoom` (`fit`, `machine` or `close`). With `reduced`, or on a still
    /// page, the camera jumps. Returns false when the page cannot go there: no plant, no such
    /// machine, a ride on, or an unknown zoom. A step replaces the move in progress.
    pub fn tour_go(&mut self, machine_id: &str, zoom: &str, reduced: bool) -> bool {
        if self.ride.is_some() {
            return false;
        }
        let Some(to) = self.tour_pose(machine_id, zoom) else {
            return false;
        };
        self.ease_to(to, reduced);
        true
    }

    /// Stops the move to a tour step in progress, where the camera stands. Does nothing without one.
    pub fn tour_stop(&mut self) {
        self.tour_move = None;
    }

    /// Stages a tour step from its `spots`, as JSON `[{"machine_id", "labels"}]`: each machine
    /// they name keeps its tint, every other machine is dimmed, and each label shows as a chip on
    /// its machine. An empty text clears the stage, as leaving the tour does.
    ///
    /// # Errors
    ///
    /// Returns an error when the text is not a list of spots.
    pub fn tour_stage(&mut self, spots_json: &str) -> Result<(), JsError> {
        self.stage = if spots_json.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(spots_json).map_err(js_err)?
        };
        self.sync_scene();
        Ok(())
    }

    /// Which plan machines the shown tour step stages, in plan order, or `None` when no step is
    /// shown or none of its machines is on the plant.
    fn staged(&self) -> Option<Vec<bool>> {
        if self.stage.is_empty() {
            return None;
        }
        let plan = self.plan.as_ref()?;
        let names: Vec<String> = plan
            .machines
            .iter()
            .map(|machine| machine.name.clone())
            .collect();
        marks::staged_plan(&names, &self.stage)
    }

    /// The chips of the shown tour step, one per mark it stages.
    fn stage_chips(&self) -> Vec<marks::Flag> {
        match (&self.plan, &self.plant) {
            (Some(plan), Some(loaded)) => marks::stage_flags(&self.stage, plan, &loaded.stands),
            _ => Vec::new(),
        }
    }

    /// The flags the page labels: the marks' flags as the motion has them, then the chips of the
    /// shown tour step. `marks_json` and `flag_layout` both list them in this order.
    fn all_flags(&self) -> Vec<marks::Flag> {
        let mut flags = self.motion.flags_now(&self.marks.flags);
        flags.extend(self.stage_chips());
        flags
    }

    /// Checks the origin's `scope/confirmation` body for `order` (`o-N`) against the scope this
    /// page holds, and returns the check as JSON. The page shows a Seal only when the verdict is
    /// `match`.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON is not the confirmation body, or when the page holds no
    /// scope for the order.
    pub fn seal_check(&self, order: &str, json: &str) -> Result<String, JsError> {
        let route = api::confirmation(json).map_err(js_err)?;
        let lane = self
            .view
            .as_ref()
            .and_then(|view| {
                view.lanes
                    .iter()
                    .find(|lane| lane.order.to_string() == order)
            })
            .ok_or_else(|| JsError::new("the page holds no scope for this order"))?;
        serde_json::to_string(&seal::check(lane, &route)).map_err(js_err)
    }

    /// Holds the marks still, as `?freeze=1` asks. Call it before the plant loads: every ease then
    /// lands at once, a leaving bot is gone, no flag fades, and a landed dot keeps its word.
    pub fn set_still(&mut self, still: bool) {
        self.still = still;
        if still {
            self.clock = marks::Clock::default();
        }
        self.motion = self.new_motion();
        self.dirty = true;
        for index in 0..self.parked.len() {
            let motion = self.new_motion();
            let own = &mut self.parked[index].marks;
            own.motion = motion;
            own.dirty = true;
            self.parked[index].view_json = None;
        }
    }

    /// Tells the marks the time now, in epoch milliseconds, so a quiet agent's drone can read
    /// stale. Zero or less clears the clock, as a page that stops being live does. A still page
    /// ignores it.
    pub fn set_now(&mut self, ms: f64) {
        let now = (ms > 0.0 && !self.still).then_some(ms as u64);
        if now != self.clock.now {
            self.clock.now = now;
            self.dirty = true;
            self.parked_dirty();
        }
    }

    /// Tells the marks when the reader last saw the page and when they came back to it, in epoch
    /// milliseconds, once they were away a while. What landed between the two shows as ghosts.
    /// A time of zero or less clears it. A still page ignores it.
    pub fn set_seen(&mut self, seen_ms: f64, back_ms: f64) {
        let away = (seen_ms > 0.0 && back_ms >= seen_ms && !self.still).then_some(marks::Away {
            seen: seen_ms as u64,
            back: back_ms as u64,
        });
        if away != self.clock.away {
            self.clock.away = away;
            self.dirty = true;
            self.parked_dirty();
        }
    }

    /// Whether a flag listing changed while time passed, such as a landed dot going quiet or a
    /// fade ending. The page redraws its flags when it does.
    pub fn take_flags_changed(&mut self) -> bool {
        self.motion.take_flags_changed()
    }

    /// Loads a generated plant of `machines` machines for the benchmark. It has no log, so the
    /// view stays empty.
    pub fn set_stress(&mut self, machines: u32) -> u32 {
        self.load(stress(machines as usize))
    }

    /// Folds the log: a JSON array of stamped events from seq 0. An event this build cannot read
    /// is passed over and counted, see [`App::not_shown`].
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON does not parse or the log does not replay.
    pub fn load_log(&mut self, json: &str) -> Result<(), JsError> {
        let entries = feed::entries(json).map_err(js_err)?;
        self.mirror = Some(Mirror::from_entries(entries).map_err(js_err)?);
        // A replaced log is a seek: the drones land where the new log puts them.
        self.motion.cut();
        self.dirty = true;
        Ok(())
    }

    /// Applies one streamed event, given as its JSON. An event this build cannot read is passed
    /// over and counted.
    ///
    /// # Errors
    ///
    /// Returns an error when no log is loaded, the JSON does not parse, events were missed, or
    /// the event refers to something the log never created.
    pub fn ingest(&mut self, json: &str) -> Result<(), JsError> {
        let entry = feed::entry(json).map_err(js_err)?;
        let mirror = self
            .mirror
            .as_mut()
            .ok_or_else(|| js_err("no log is loaded"))?;
        mirror.ingest_entry(entry).map_err(js_err)?;
        self.dirty = true;
        Ok(())
    }

    /// How many events this page passed over because it cannot read them. Above zero, the page
    /// is older than the origin and says so.
    pub fn not_shown(&self) -> u32 {
        self.mirror.as_ref().map_or(0, Mirror::not_shown)
    }

    /// The seq the next event will carry. The stream resumes from here.
    pub fn next_seq(&self) -> f64 {
        self.mirror
            .as_ref()
            .map_or(0.0, |mirror| mirror.next_seq().get() as f64)
    }

    /// The view as of the last frame, as JSON. Compare [`App::view_version`] to see it change.
    pub fn view_json(&self) -> String {
        self.view_json.clone()
    }

    /// Counts the view rebuilds, so the page redraws its panels only when the view changed.
    pub fn view_version(&self) -> u32 {
        self.view_version
    }

    /// The repository's folder name, or an empty string before a plant loads.
    pub fn repo_name(&self) -> String {
        self.plant
            .as_ref()
            .map_or_else(String::new, |loaded| loaded.repo.clone())
    }

    /// Call on every canvas size or device pixel ratio change. The canvas drawing size must
    /// already be `css size * device pixel ratio`, rounded the same way.
    pub fn resize(&mut self, css_width: f32, css_height: f32, device_pixel_ratio: f32) {
        self.dpr = device_pixel_ratio.max(0.25);
        let size = [
            (css_width * self.dpr).round().max(1.0) as u32,
            (css_height * self.dpr).round().max(1.0) as u32,
        ];
        let viewport = [size[0] as f32, size[1] as f32];
        match self.ride.as_mut() {
            Some(ride) => ride.resize(viewport, &mut self.camera),
            None => self.camera.set_viewport(viewport),
        }
        self.renderer.resize(size);
        // The screen-size floors of the marks are in device px, so a new ratio moves them.
        self.sync_scene();
    }

    /// Moves the scene so the ground under the CSS point `(x0, y0)` lands under `(x1, y1)`.
    /// Called with the previous and the current pointer position, the ground point stays under
    /// the pointer in either projection.
    pub fn drag(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        if self.ride.is_some() {
            return;
        }
        self.tour_move = None;
        self.cancel_top_move();
        self.camera.drag_ground(
            [x0 * self.dpr, y0 * self.dpr],
            [x1 * self.dpr, y1 * self.dpr],
        );
    }

    /// Moves the scene by `(dx, dy)` CSS pixels as if dragged from the screen centre. The arrow
    /// keys and the benchmark use it.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        if self.ride.is_some() {
            return;
        }
        self.tour_move = None;
        self.cancel_top_move();
        let [vw, vh] = self.camera.viewport();
        let from = [vw * 0.5, vh * 0.5];
        self.camera
            .drag_ground(from, [from[0] + dx * self.dpr, from[1] + dy * self.dpr]);
    }

    /// Switches between the perspective and orthographic projections. The fit depends on the
    /// projection, so the plant is framed again at the same angles.
    pub fn toggle_projection(&mut self) {
        if self.ride.is_some() {
            return;
        }
        self.camera.toggle_projection();
        let (room, clear) = (self.fit_room(), self.clear_for(self.camera.elevation()));
        if let Some(loaded) = &self.plant {
            fit_clear(
                &mut self.camera,
                &loaded.framing,
                FIT_MARGIN_CSS * self.dpr,
                room,
                clear,
            );
        }
    }

    /// Turns the view by `(dx, dy)` CSS pixels of drag: horizontal orbits, vertical tilts. This
    /// cancels a turn in progress, so the drag always wins.
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        if self.ride.is_some() {
            return;
        }
        self.turn = None;
        self.tour_move = None;
        self.cancel_top_move();
        self.camera
            .orbit(dx * ORBIT_DEG_PER_PX, -dy * TILT_DEG_PER_PX);
        // A tilt away from the top ends the top-down view, so the page's button tells the truth.
        self.top_down = self.top_down && topdown::is_top_down(self.camera.elevation());
    }

    /// Starts an ease of `degrees` of azimuth. Repeated calls add up.
    pub fn turn(&mut self, degrees: f32) {
        if self.ride.is_some() {
            return;
        }
        let (from, to) = match self.turn.take() {
            Some(turn) => (self.camera.azimuth(), turn.to + degrees),
            None => (self.camera.azimuth(), self.camera.azimuth() + degrees),
        };
        self.turn = Some(Turn {
            from,
            to,
            elapsed_ms: 0.0,
        });
    }

    /// Moves an active turn and the eases of the marks on by `dt_ms`. Returns true while either is
    /// still moving, so the page asks for another frame only then.
    pub fn advance(&mut self, dt_ms: f32) -> bool {
        let turning = self.advance_turn(dt_ms);
        let touring = self.advance_tour_move(dt_ms);
        let topping = self.advance_top_move(dt_ms);
        let moving = self.motion.advance(dt_ms);
        let (parked_upload, parked_moving) = self.advance_parked(dt_ms);
        // The frame an ease ends on still uploads, so the marks land on their final places.
        if moving || self.moving || parked_upload {
            self.sync_scene();
        }
        self.moving = moving;
        let riding = self.advance_ride(dt_ms);
        self.calm = moving
            && self.motion.calm()
            && !(turning || touring || topping || riding || parked_moving);
        turning || touring || topping || moving || riding || parked_moving
    }

    /// Moves the top-down turn on by `dt_ms`: the pivot and zoom by the tour ease, the elevation by
    /// the same ease. It lands exactly on the fit at its end, then clears.
    fn advance_top_move(&mut self, dt_ms: f32) -> bool {
        let Some(movement) = self.top_move.as_mut() else {
            return false;
        };
        movement.elapsed_ms += dt_ms.max(0.0);
        let elapsed = movement.elapsed_ms;
        if elapsed >= topdown::MOVE_MS {
            let (pivot, zoom) = movement.to_pose;
            let (azimuth, elevation) = (movement.to_azimuth, movement.to_elevation);
            self.top_move = None;
            self.camera.set_angles(azimuth, elevation);
            self.camera.set_pivot(pivot);
            self.camera.set_zoom(zoom);
            return false;
        }
        let (pivot, zoom) = tour::progress(movement.from_pose, movement.to_pose, elapsed);
        let azimuth = topdown::ease(movement.from_azimuth, movement.to_azimuth, elapsed);
        let elevation = topdown::ease(movement.from_elevation, movement.to_elevation, elapsed);
        self.camera.set_angles(azimuth, elevation);
        self.camera.set_pivot(pivot);
        self.camera.set_zoom(zoom);
        true
    }

    /// Moves every parked project's eases on, far ones too, so their state is current when the
    /// camera comes back. Returns whether the scene needs their marks again and whether any moves.
    fn advance_parked(&mut self, dt_ms: f32) -> (bool, bool) {
        let (mut upload, mut moving, mut flags) = (false, false, false);
        for parked in &mut self.parked {
            let (up, still_moving) = parked.marks.advance(dt_ms);
            upload |= up;
            moving |= still_moving;
            flags |= parked.marks.motion.take_flags_changed();
        }
        if flags {
            self.parked_version += 1;
        }
        (upload, moving)
    }

    /// How long the page may wait before the next frame, in ms, after [`App::advance`] asked for
    /// one: zero for the next display frame, or [`CALM_FRAME_MS`] while only the idle bob runs.
    pub fn frame_gap_ms(&self) -> f32 {
        if self.calm { CALM_FRAME_MS } else { 0.0 }
    }

    /// Follows the ride's drone, moves the ride's ease on, and leaves the ride once its drone has
    /// faded out. Returns true while the ride or the bob needs another frame.
    fn advance_ride(&mut self, dt_ms: f32) -> bool {
        let Some(agent) = self.ride.as_ref().map(|ride| ride.agent().to_owned()) else {
            return false;
        };
        let riding_before = self.riding_agent().is_some();
        let gone = self.motion.body_at(&agent).is_none() && !self.motion.is_leaving(&agent);
        let snapshot = self.snapshot_of(&agent);
        let Some(ride) = self.ride.as_mut() else {
            return false;
        };
        if let Some(snapshot) = snapshot.filter(|_| !ride.is_leaving()) {
            ride.follow(snapshot, self.camera);
        }
        if gone {
            ride.leave(self.camera);
        }
        let moving = ride.advance(dt_ms, &mut self.camera);
        if self.ride.as_ref().is_some_and(Ride::finished) {
            self.ride = None;
            if self.fit_after_leave {
                self.fit_after_leave = false;
                self.fit_view();
            }
            if let Some(on) = self.top_after_leave.take() {
                self.set_top_down(on, false);
            }
        }
        if riding_before != self.riding_agent().is_some() {
            self.sync_scene();
        }
        let bobbing = self.bobbing();
        if bobbing {
            self.bob_ms = (self.bob_ms + dt_ms.max(0.0)) % pov::BOB_PERIOD_MS;
        }
        moving || bobbing
    }

    fn advance_turn(&mut self, dt_ms: f32) -> bool {
        let Some(turn) = self.turn.as_mut() else {
            return false;
        };
        turn.elapsed_ms += dt_ms.max(0.0);
        let t = (turn.elapsed_ms / TURN_MS).min(1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        let azimuth = turn.from + (turn.to - turn.from) * eased;
        let elevation = self.camera.elevation();
        self.camera.set_angles(azimuth, elevation);
        if t >= 1.0 {
            self.turn = None;
            return false;
        }
        true
    }

    /// Moves the camera's pivot and zoom toward a tour step's target, by the ease of the step. The
    /// move lands exactly on its target, then clears.
    fn advance_tour_move(&mut self, dt_ms: f32) -> bool {
        let Some(movement) = self.tour_move.as_mut() else {
            return false;
        };
        movement.elapsed_ms += dt_ms.max(0.0);
        let (pivot, zoom) = tour::progress(movement.from, movement.to, movement.elapsed_ms);
        self.camera.set_pivot(pivot);
        self.camera.set_zoom(zoom);
        if movement.elapsed_ms >= tour::MOVE_MS {
            self.tour_move = None;
            return false;
        }
        true
    }

    /// The pose a tour step goes to: the fit pose for `fit`, otherwise the pivot over the machine
    /// named `machine_id` at its zoom. `None` for an unknown zoom or a machine the plant lacks.
    fn tour_pose(&self, machine_id: &str, zoom: &str) -> Option<tour::Pose> {
        let zoom = tour::zoom_named(zoom)?;
        let Some(ratio) = tour::zoom_ratio(zoom) else {
            return self.framing_pose();
        };
        let loaded = self.plant.as_ref()?;
        let index = loaded
            .machines
            .iter()
            .position(|machine| machine.name == machine_id)?;
        let placed = loaded.layout.machines.get(index)?;
        let (w, d) = placed.kind.footprint();
        Some((
            [placed.x + w / 2.0, placed.y + d / 2.0],
            ratio * self.fit_zoom(),
        ))
    }

    /// Zooms by `factor` around the CSS point `(x, y)`, which stays under the pointer.
    pub fn zoom_at(&mut self, x: f32, y: f32, factor: f32) {
        if self.ride.is_some() {
            return;
        }
        self.tour_move = None;
        self.cancel_top_move();
        self.camera.zoom_at([x * self.dpr, y * self.dpr], factor);
    }

    /// Turns to the given angles in degrees and fits the plant there, centred on screen.
    pub fn view(&mut self, azimuth: f32, elevation: f32) {
        self.turn = None;
        self.tour_move = None;
        self.top_move = None;
        self.camera.set_angles(azimuth, elevation);
        self.top_down = topdown::is_top_down(self.camera.elevation());
        let (room, clear) = (self.fit_room(), self.clear_for(self.camera.elevation()));
        if let Some(loaded) = &self.plant {
            fit_clear(
                &mut self.camera,
                &loaded.framing,
                FIT_MARGIN_CSS * self.dpr,
                room,
                clear,
            );
        }
    }

    /// Returns to the default angles and fits the plant, which also leaves the top-down view. While
    /// riding, the ride eases back first and the fit runs once it has landed.
    pub fn fit(&mut self) {
        self.top_down = false;
        self.top_after_leave = None;
        if self.ride.is_some() {
            self.leave();
            self.fit_after_leave = true;
            return;
        }
        self.fit_view();
    }

    /// Whether the view is top-down: the camera looks straight down at the plant. The page shows
    /// it on the top-down button.
    pub fn top_down(&self) -> bool {
        self.top_down
    }

    /// Turns the view to top-down, or back to the tilt it had, and fits the plant there. The
    /// azimuth and the projection stay. With `reduced`, or on a still page, the camera jumps; else
    /// it eases over the tour's time. While riding, the ride eases back first and the turn runs
    /// once it has landed. A turn in progress is replaced.
    pub fn set_top_down(&mut self, on: bool, reduced: bool) {
        if self.ride.is_some() {
            self.leave();
            self.fit_after_leave = false;
            self.top_after_leave = Some(on);
            return;
        }
        self.top_after_leave = None;
        // Asking for the state the view is in, with no turn running, changes nothing.
        if self.top_down == on && self.top_move.is_none() {
            return;
        }
        self.tour_move = None;
        let mid_turn = self.top_move.take().is_some();
        let to_elevation = if on {
            // A turn back to the default tilt in progress keeps the tilt it is returning to.
            if !self.top_down && !mid_turn {
                self.tilt_before_top = self.camera.elevation();
            }
            topdown::ELEVATION
        } else if topdown::is_top_down(self.tilt_before_top) {
            DEFAULT_ELEVATION
        } else {
            self.tilt_before_top
        };
        self.top_down = on;
        // Turning to top-down snaps the azimuth to a grid step, so the grid lines up with the screen.
        // Turning back keeps the azimuth the reader chose.
        let azimuth = self.camera.azimuth();
        let to_azimuth = if on { topdown::snap(azimuth) } else { azimuth };
        self.move_top(to_azimuth, to_elevation, reduced);
    }

    /// Eases the camera to the fit at `to_azimuth` and `to_elevation`, from where it is now. With
    /// `reduced`, or on a still page, the camera jumps.
    fn move_top(&mut self, to_azimuth: f32, to_elevation: f32, reduced: bool) {
        self.top_move = None;
        let to_pose = self.fit_pose_at(to_azimuth, to_elevation);
        match to_pose {
            Some(to_pose) if !(reduced || self.still) => {
                self.top_move = Some(TopMove {
                    from_pose: (self.camera.pivot(), self.camera.zoom()),
                    to_pose,
                    from_elevation: self.camera.elevation(),
                    to_elevation,
                    from_azimuth: self.camera.azimuth(),
                    to_azimuth,
                    elapsed_ms: 0.0,
                });
            }
            _ => {
                self.camera.set_angles(to_azimuth, to_elevation);
                if let Some((pivot, zoom)) = to_pose {
                    self.camera.set_pivot(pivot);
                    self.camera.set_zoom(zoom);
                }
            }
        }
    }

    /// Flips the top-down view, as `V` does. Reads the state a pending turn will leave, so a press
    /// while riding flips it too.
    pub fn toggle_top_down(&mut self, reduced: bool) {
        let on = !self.top_after_leave.unwrap_or(self.top_down);
        self.set_top_down(on, reduced);
    }

    /// Turns the top-down view by `degrees` about the vertical axis, to the grid step that many
    /// degrees from the current one, and fits the plant there. Q and E turn one step. A turn in
    /// progress adds up. With `reduced`, or on a still page, the camera jumps. Does nothing off the
    /// top-down view.
    pub fn turn_top_down(&mut self, degrees: f32, reduced: bool) {
        if self.ride.is_some() || !self.top_down {
            return;
        }
        self.turn = None;
        self.tour_move = None;
        let from = match &self.top_move {
            Some(movement) => movement.to_azimuth,
            None => self.camera.azimuth(),
        };
        self.move_top(topdown::snap(from) + degrees, topdown::ELEVATION, reduced);
    }

    /// Ends a horizontal orbit drag in the top-down view: the view turns to the grid step nearest
    /// its azimuth, and fits the plant there. Does nothing off the top-down view, where a tilt has
    /// already left it.
    pub fn settle_top_down(&mut self, reduced: bool) {
        if self.ride.is_some() || !self.top_down {
            return;
        }
        self.move_top(
            topdown::snap(self.camera.azimuth()),
            topdown::ELEVATION,
            reduced,
        );
    }

    /// Sets the room the top-down fit keeps clear of the page's shell bar, which hangs at the top,
    /// and of the right column, in CSS pixels from the canvas edges. The next fit uses it; the
    /// view does not move until then.
    pub fn set_fit_inset(&mut self, top_css: f32, right_css: f32) {
        self.fit_inset = [top_css.max(0.0), right_css.max(0.0)];
    }

    /// Rides the drone of agent `agent` from inside. A ride already on switches to it, and a
    /// ride already on that agent does nothing. Returns false when the view has no drone of that
    /// name.
    pub fn ride(&mut self, agent: &str) -> bool {
        if self.riding_agent() == Some(agent) {
            return true;
        }
        let Some(snapshot) = self.snapshot_of(agent) else {
            return false;
        };
        self.turn = None;
        self.tour_move = None;
        self.cancel_top_move();
        self.fit_after_leave = false;
        if let Some(ride) = self.ride.as_mut().filter(|ride| ride.is_leaving()) {
            ride.resume(snapshot, self.camera);
        } else if let Some(ride) = self.ride.as_mut() {
            ride.switch(snapshot, self.camera);
        } else {
            self.ride = Some(Ride::enter(self.camera, snapshot));
        }
        self.sync_scene();
        true
    }

    /// Rides the drone whose disc centre is nearest the CSS point `(css_x, css_y)`. While riding,
    /// the rider is left out, so the nearest other drone is ridden. Returns false when no other
    /// drone is in the view.
    pub fn ride_nearest(&mut self, css_x: f32, css_y: f32) -> bool {
        let rider = self.riding_agent().map(str::to_owned);
        let nearest = self
            .marks
            .bodies
            .iter()
            .filter(|body| rider.as_deref() != Some(body.agent.as_str()))
            .map(|body| {
                let at = self.motion.body_at(&body.agent).unwrap_or(body.at);
                let ([x, y], _) = marks::drone_disc(at, |p| self.screen(p));
                (body.agent.as_str(), (x - css_x).hypot(y - css_y))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(agent, _)| agent.to_owned());
        nearest.is_some_and(|agent| self.ride(&agent))
    }

    /// Rides the next drone in the order the agents' names sort to, or the previous one when
    /// `direction` is -1. The order wraps at both ends. Returns false when no drone is in the view.
    pub fn ride_step(&mut self, direction: i32) -> bool {
        let current = self.riding_agent().map(str::to_owned);
        let order = pov::ride_order(self.marks.bodies.iter().map(|body| body.agent.as_str()));
        let next = pov::next_in_order(&order, current.as_deref(), direction).map(str::to_owned);
        next.is_some_and(|agent| self.ride(&agent))
    }

    /// Eases the camera back to the view the ride started from. Does nothing without a ride.
    pub fn leave(&mut self) {
        if let Some(ride) = self.ride.as_mut() {
            ride.leave(self.camera);
            self.sync_scene();
        }
    }

    /// The agent the page rides, or an empty string. It is empty once the ride is easing back.
    pub fn riding(&self) -> String {
        self.riding_agent().map_or_else(String::new, str::to_owned)
    }

    /// The rider's words and where its target is on screen, as JSON, or `null` when the page
    /// does not ride. See `pov::Info` for the fields.
    pub fn pov_json(&self) -> String {
        match self.ride.as_ref().filter(|ride| !ride.is_leaving()) {
            Some(ride) => ride.json(pov::target_on_screen(
                &self.drawn_camera(),
                ride.target(),
                self.dpr,
            )),
            None => "null".to_owned(),
        }
    }

    /// Scales the ride's distance by `factor` at once, within its range. Does nothing without a
    /// ride.
    pub fn pov_scale(&mut self, factor: f32) {
        if let Some(ride) = self.ride.as_mut() {
            ride.zoom(factor, &mut self.camera);
        }
    }

    /// Turns the hover bob on or off. The page turns it off under reduced motion, which also
    /// lands the drones' moves at once and holds their bob.
    pub fn set_bob(&mut self, enabled: bool) {
        self.bob_enabled = enabled;
        self.motion.set_reduced(!enabled);
        for parked in &mut self.parked {
            parked.marks.motion.set_reduced(!enabled);
        }
    }

    /// Shows the page's look: `paper`, `dark`, `blueprint` or `high_contrast`. Returns false, and
    /// keeps the look as it was, for a name it does not know.
    pub fn set_look(&mut self, name: &str) -> bool {
        let Some(look) = acan_render::theme::Look::from_name(name) else {
            return false;
        };
        self.renderer.set_look(look);
        true
    }

    /// A fresh motion for the page's freeze and reduced-motion settings.
    fn new_motion(&self) -> Motion {
        let mut motion = Motion::new(self.still);
        motion.set_reduced(!self.bob_enabled);
        motion
    }

    /// Fits the plant at the angles the framing uses: the top-down view's while it is on, else the
    /// default angles.
    fn fit_view(&mut self) {
        self.turn = None;
        self.tour_move = None;
        self.top_move = None;
        if let Some(loaded) = &self.plant {
            let (azimuth, elevation) = self.framing_angles();
            self.camera.set_angles(azimuth, elevation);
            let (room, clear) = (self.fit_room(), self.clear_for(elevation));
            fit_clear(
                &mut self.camera,
                &loaded.framing,
                FIT_MARGIN_CSS * self.dpr,
                room,
                clear,
            );
        }
    }

    /// The azimuth and elevation the page frames the plant at: the top-down view's while it is on,
    /// with the reader's azimuth, else the kit's own view.
    fn framing_angles(&self) -> (f32, f32) {
        if self.top_down {
            (self.camera.azimuth(), topdown::ELEVATION)
        } else {
            (DEFAULT_AZIMUTH, DEFAULT_ELEVATION)
        }
    }

    /// Stops a top-down turn in progress. The view stays where the turn had got to, and the
    /// top-down state follows where it is.
    fn cancel_top_move(&mut self) {
        if self.top_move.take().is_some() {
            self.top_down = topdown::is_top_down(self.camera.elevation());
        }
    }

    /// The agent the page rides, or none while there is no ride or it is easing back.
    fn riding_agent(&self) -> Option<&str> {
        self.ride
            .as_ref()
            .filter(|ride| !ride.is_leaving())
            .map(Ride::agent)
    }

    /// Whether the hover bob runs now: the page rides, the bob is on, and the page is not still.
    fn bobbing(&self) -> bool {
        self.riding_agent().is_some() && self.bob_enabled && !self.still
    }

    /// The camera a frame draws: the stored camera, lifted by the hover bob while riding.
    fn drawn_camera(&self) -> Camera {
        if self.riding_agent().is_none() {
            return self.camera;
        }
        let offset = pov::bob_offset(
            self.bob_enabled,
            self.still,
            self.bob_ms,
            self.camera.zoom(),
            self.dpr,
        );
        pov::bobbed(&self.camera, offset)
    }

    /// The drone of agent `agent` as the marks show it now, or None when the view has no drone
    /// of that name.
    fn snapshot_of(&self, agent: &str) -> Option<pov::Snapshot> {
        let loaded = self.plant.as_ref()?;
        let view = self.view.as_ref()?;
        let plan = self.plan.as_ref()?;
        let body = self.marks.bodies.iter().find(|body| body.agent == agent)?;
        let at = self.motion.body_at(agent).unwrap_or(body.at);
        let heading = self.motion.heading_of(agent).unwrap_or(0.0);
        let instance = *loaded.plan_instance.get(body.machine)?;
        let machine = *loaded.machine_bounds.get(instance)?;
        Some(pov::snapshot(view, plan, body, at, heading, machine))
    }

    /// Centres machine `index` and sets the zoom to `ratio` times the fit zoom.
    pub fn focus(&mut self, index: u32, ratio: f32) {
        let Some(loaded) = self.plant.as_ref() else {
            return;
        };
        let Some(placed) = loaded.layout.machines.get(index as usize) else {
            return;
        };
        let (w, d) = placed.kind.footprint();
        self.camera
            .set_pivot([placed.x + w / 2.0, placed.y + d / 2.0]);
        self.camera.set_zoom(ratio * self.fit_zoom());
    }

    /// Closes up on agent `name`'s drone: the drone sits at the screen centre, and the zoom is
    /// `ratio` times the fit zoom. `band` is the CSS x range `[left, right]` the page's panels
    /// leave free: when a drone on screen would sit under a panel past it, the view slides left
    /// just enough to clear it, as long as that does not push a drone off the band's left edge.
    /// The page asks for it with `?focus=`. It waits until the view places the drone, so it may
    /// be called before the log loads.
    pub fn focus_agent(&mut self, name: &str, ratio: f32, band: &[f32]) {
        let band = match band {
            [left, right] if left < right => Some([*left, *right]),
            _ => None,
        };
        self.focus = Some((name.to_owned(), ratio, band));
        self.apply_focus();
    }

    /// Applies a pending focus once the agent's drone is placed, then clears it.
    fn apply_focus(&mut self) {
        let Some((name, ratio, band)) = self.focus.as_ref() else {
            return;
        };
        let Some(body) = self.marks.bodies.iter().find(|body| &body.agent == name) else {
            return;
        };
        let at = self.motion.body_at(&body.agent).unwrap_or(body.at);
        let zoom = ratio * self.fit_zoom();
        let band = *band;
        self.camera.centre_on(at);
        self.camera.set_zoom(zoom);
        self.focus = None;
        self.focus_hold = band.map(|band| FocusHold {
            band,
            centre: at,
            camera: self.camera,
        });
        self.hold_clear();
    }

    /// Slides a close-up the reader has not moved so its drones keep clear of the side panels.
    /// The log may place a drone after the close-up, so each view rebuild checks again.
    fn hold_clear(&mut self) {
        let Some(hold) = self.focus_hold else {
            return;
        };
        if self.camera != hold.camera {
            self.focus_hold = None;
            return;
        }
        let slide = self.slide_clear(hold.band);
        if slide == 0.0 {
            return;
        }
        let centre = self.slid_centre(hold.centre, slide);
        self.camera.centre_on(centre);
        self.focus_hold = Some(FocusHold {
            centre,
            camera: self.camera,
            ..hold
        });
    }

    /// The point to centre on so the scene at `centre`'s depth moves `slide` CSS px across the
    /// screen. It is worked out at that depth, since a ground pan moves the drones, nearer the
    /// eye, further than the ground under a low perspective camera.
    fn slid_centre(&self, centre: [f32; 3], slide: f32) -> [f32; 3] {
        let at = self.camera.world_to_screen(centre);
        let step = |axis: usize| {
            let mut moved = centre;
            moved[axis] += 1.0;
            let [x, y] = self.camera.world_to_screen(moved);
            [x - at[0], y - at[1]]
        };
        let (sx, sy) = (step(0), step(1));
        let det = sx[0] * sy[1] - sy[0] * sx[1];
        if det.abs() < 1e-6 {
            return centre;
        }
        // The plan step d with J d = (-slide, 0) in device px, so the point now at the screen
        // centre less the slide takes the centre.
        let b = -slide * self.dpr;
        [
            centre[0] + b * sy[1] / det,
            centre[1] - b * sx[1] / det,
            centre[2],
        ]
    }

    /// How far, in CSS px, to slide the view sideways so every drone on screen keeps out past
    /// `band[1]` without one leaving past `band[0]`. Zero when none sits past the right edge.
    fn slide_clear(&self, band: [f32; 2]) -> f32 {
        // A drone's reach from its centre, ring and ears included, in CSS px.
        let reach = FOCUS_REACH * self.camera.zoom() / self.dpr;
        let xs: Vec<f32> = self
            .marks
            .bodies
            .iter()
            .filter_map(|body| self.motion.body_at(&body.agent))
            .filter_map(|at| pov::target_on_screen(&self.camera, at, self.dpr))
            .map(|[x, _]| x)
            .collect();
        let (lo, hi) = xs
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &x| {
                (lo.min(x), hi.max(x))
            });
        let over = hi + reach - band[1];
        if xs.is_empty() || over <= 0.0 {
            return 0.0;
        }
        let room = (lo - reach - band[0]).max(0.0);
        -over.min(room)
    }

    /// Level of detail now: 0 far, 1 near, 2 close.
    pub fn detail(&self) -> u32 {
        self.lod() as u32
    }

    /// Draws one frame, folding the log into the view first when it changed. Returns false when
    /// the browser skipped the frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the surface is lost or the view cannot be encoded.
    pub fn render(&mut self) -> Result<bool, JsError> {
        let parked = self.refresh_parked();
        if self.dirty {
            self.rebuild_view()?;
        } else if parked {
            self.sync_scene();
        }
        let camera = self.drawn_camera();
        self.renderer.render(&camera, self.lod()).map_err(js_err)
    }

    /// Label candidates as flat `[machine index, css x, css y, opacity]` quads, most important
    /// first: the machines largest on screen lead, so the page keeps the larger or nearer one
    /// when two labels collide. Each label hangs under its plate's front edge, or above its
    /// machine when a nearer one hides that spot, and fades with the plant's fog. `all` names
    /// every machine large enough on screen. Otherwise the machines named at rest are, plus the
    /// largest few on screen, so a far plant still shows some names.
    pub fn label_layout(&self, all: bool) -> Vec<f32> {
        match &self.plant {
            Some(loaded) => self.labels_of(loaded, all),
            None => Vec::new(),
        }
    }

    /// The label quads of one plant, as `label_layout` lists them.
    fn labels_of(&self, loaded: &Loaded, all: bool) -> Vec<f32> {
        let named: Vec<bool> = if all {
            vec![true; loaded.machines.len()]
        } else {
            loaded.rest.clone()
        };
        let dpr = self.dpr;
        let rules = if all {
            labels::Rules {
                min_px: labels::ALL_MIN_CSS_PX * dpr,
                floor_px: labels::ALL_MIN_CSS_PX * dpr,
                budget: 0,
                drop_px: labels::LABEL_DROP_CSS_PX * dpr,
                lift_px: labels::LABEL_LIFT_CSS_PX * dpr,
            }
        } else {
            labels::Rules {
                min_px: labels::REST_MIN_CSS_PX * dpr,
                floor_px: labels::BUDGET_FLOOR_CSS_PX * dpr,
                budget: labels::REST_BUDGET,
                drop_px: labels::LABEL_DROP_CSS_PX * dpr,
                lift_px: labels::LABEL_LIFT_CSS_PX * dpr,
            }
        };
        let footprints: Vec<[f32; 4]> = loaded
            .layout
            .machines
            .iter()
            .map(|m| m.footprint())
            .collect();
        let half_widths: Vec<f32> = loaded.label_half_px.iter().map(|w| w * dpr).collect();
        let machines = labels::Machines {
            footprints: &footprints,
            occluders: &loaded.occluders,
            half_widths: &half_widths,
        };
        let spots: Vec<LabelSpot> = labels::place(&self.drawn_camera(), &machines, &named, &rules);
        spots
            .iter()
            .flat_map(|spot| {
                [
                    spot.index as f32,
                    spot.screen[0] / self.dpr,
                    spot.screen[1] / self.dpr,
                    spot.opacity,
                ]
            })
            .collect()
    }

    /// The flags, the open clash card, the scope machine names the plant lacks and each clash's
    /// badges, as JSON: `{"flags": [...], "card": {...} | null, "unplaced": [...], "badges":
    /// [...]}`. The page reads it when the view changes or a card opens. Flag `i` anchors where
    /// `flag_layout` puts index `i`, and a clash's badges where `badge_layout` puts its clash.
    pub fn marks_json(&self) -> String {
        #[derive(serde::Serialize)]
        struct Out<'a> {
            flags: Vec<marks::Flag>,
            card: Option<marks::Card>,
            unplaced: &'a [String],
            badges: Vec<marks::Badges>,
        }
        let badges = self.view.as_ref().map_or_else(Vec::new, |view| {
            marks::clash_badges(view, &self.marks.pins, &self.slots)
        });
        let card = self.card_clash.and_then(|index| {
            let view = self.view.as_ref()?;
            let mirror = self.mirror.as_ref()?;
            marks::card(view, mirror.world(), index, &self.slots)
        });
        serde_json::to_string(&Out {
            flags: self.all_flags(),
            card,
            unplaced: &self.marks.unplaced,
            badges,
        })
        .expect("marks serialize as JSON")
    }

    /// Each flag's anchor on screen, as flat `[flag index, machine or -1, css x, css y]` quads, in
    /// the order `marks_json` lists the flags. The machine is the one an agent's flag stacks on,
    /// and -1 is a flag that stands alone. The anchors follow the bodies and diamonds while they
    /// move.
    pub fn flag_layout(&self) -> Vec<f32> {
        project::flag_quads(&self.all_flags(), &self.motion, |p| self.screen(p))
            .into_iter()
            .flatten()
            .collect()
    }

    /// Each clash diamond's centre on screen, as flat `[clash index, css x, css y]` triples. The
    /// page opens the card of the diamond a click lands on.
    pub fn pin_layout(&self) -> Vec<f32> {
        self.marks
            .pins
            .iter()
            .flat_map(|pin| {
                let foot = self.motion.pin_at(&pin.id).unwrap_or(pin.at);
                let [x, y] = marks::diamond_centre(foot, |p| self.screen(p));
                [pin.clash as f32, x, y]
            })
            .collect()
    }

    /// The agent whose drone the CSS point `(x, y)` is over, within its radius and a few px, or
    /// the empty string. The riding agent's drone has no body on screen, so it is never hovered.
    pub fn drone_at(&self, x: f32, y: f32) -> String {
        let riding = self.riding_agent();
        let bodies: Vec<&marks::Body> = self
            .marks
            .bodies
            .iter()
            .filter(|body| riding != Some(body.agent.as_str()))
            .collect();
        let discs: Vec<([f32; 2], f32)> = bodies
            .iter()
            .map(|body| {
                let at = self.motion.body_at(&body.agent).unwrap_or(body.at);
                marks::drone_disc(at, |p| self.screen(p))
            })
            .collect();
        marks::disc_under(&discs, [x, y]).map_or_else(String::new, |i| bodies[i].agent.clone())
    }

    /// The disc of `agent`'s drone on screen, as `[css x, css y, css radius]`, or empty when it
    /// has no drone. The page rings the hovered drone with it.
    pub fn drone_disc_of(&self, agent: &str) -> Vec<f32> {
        self.marks
            .bodies
            .iter()
            .find(|body| body.agent == agent)
            .map_or_else(Vec::new, |body| {
                let at = self.motion.body_at(&body.agent).unwrap_or(body.at);
                let ([x, y], radius) = marks::drone_disc(at, |p| self.screen(p));
                vec![x, y, radius]
            })
    }

    /// Marks the drone of `agent` as hovered, or none for the empty string, so its tether inks in
    /// fully. Returns whether that changed.
    pub fn set_hovered(&mut self, agent: &str) -> bool {
        let hovered = (!agent.is_empty()).then(|| agent.to_owned());
        if hovered == self.hovered {
            return false;
        }
        self.hovered = hovered;
        self.sync_scene();
        true
    }

    /// Each clash's badge centres on screen, as flat `[clash index, left css x, right css x, css
    /// y]` quads, following the diamonds as they rise and sink.
    pub fn badge_layout(&self) -> Vec<f32> {
        self.marks
            .pins
            .iter()
            .flat_map(|pin| {
                let foot = self.motion.pin_at(&pin.id).unwrap_or(pin.at);
                let [left, right] = marks::badge_centres(foot, |p| self.screen(p));
                [pin.clash as f32, left[0], right[0], left[1]]
            })
            .collect()
    }

    /// Each ghost on show, as flat `[css x, css y, landed count]` triples, at the corner of its
    /// outline lowest on screen. Alt labels each with how many tasks landed there.
    pub fn ghost_layout(&self) -> Vec<f32> {
        self.marks
            .ghosts
            .iter()
            .filter(|ghost| self.motion.ghost_alpha(ghost.machine).is_some())
            .flat_map(|ghost| {
                let [x, y] = ghost
                    .corners()
                    .map(|corner| self.screen(corner))
                    .into_iter()
                    .fold([0.0, f32::NEG_INFINITY], |low, p| {
                        if p[1] > low[1] { p } else { low }
                    });
                [x, y, ghost.count as f32]
            })
            .collect()
    }

    /// Clears the ghost whose outline holds the CSS point `(x, y)`, as the reader's pointer
    /// passing over it or a click on it does. Returns whether one began to fade.
    pub fn clear_ghost_at(&mut self, x: f32, y: f32) -> bool {
        let shown = self
            .marks
            .ghosts
            .iter()
            .filter(|ghost| self.motion.ghost_alpha(ghost.machine).is_some());
        let hit = shown.into_iter().find(|ghost| {
            let quad = ghost.corners().map(|corner| self.screen(corner));
            marks::inside_quad(&quad, [x, y])
        });
        let Some(machine) = hit.map(|ghost| ghost.machine) else {
            return false;
        };
        self.motion.clear_ghost(machine)
    }

    /// Clears every ghost, as the page does a while after the reader comes back. Returns whether
    /// any began to fade.
    pub fn clear_ghosts(&mut self) -> bool {
        let mut any = self.motion.clear_ghosts();
        for parked in &mut self.parked {
            any |= parked.marks.motion.clear_ghosts();
        }
        any
    }

    /// Each working drone's beacon on screen, as flat `[css x, css y, css radius]` triples, at its
    /// pulse now. The page rings each in paper.
    pub fn beacon_layout(&self) -> Vec<f32> {
        marks::beacon_discs(&self.motion, &self.marks, |p| self.screen(p))
            .into_iter()
            .flat_map(|([x, y], radius)| [x, y, radius])
            .collect()
    }
    /// Each drone's disc on screen, as flat `[css x, css y, css radius]` triples, in the order the
    /// marks list the drones. The page keeps flags and names clear of them.
    pub fn drone_layout(&self) -> Vec<f32> {
        project::drone_discs(&self.marks, &self.motion, None, |p| self.screen(p))
            .into_iter()
            .flat_map(|(_, disc)| disc)
            .collect()
    }

    /// Opens the card of the clash at `index` in the view. The camera does not move.
    pub fn open_card(&mut self, index: u32) {
        let index = index as usize;
        if self
            .view
            .as_ref()
            .is_some_and(|view| index < view.clashes.len())
        {
            self.card_clash = Some(index);
        }
    }

    /// Closes the card, if one is open.
    pub fn close_card(&mut self) {
        self.card_clash = None;
    }

    /// Closes the card when it is open. Otherwise opens the card of the first unsettled clash.
    pub fn toggle_card(&mut self) {
        if self.card_clash.is_some() {
            self.card_clash = None;
        } else {
            self.card_clash = self.marks.pins.first().map(|pin| pin.clash);
        }
    }

    /// CSS-pixel position of a plan point on the ground, relative to the canvas. The page pins
    /// its panels to the plant with it.
    pub fn world_to_screen(&self, x: f32, y: f32) -> Vec<f32> {
        let [px, py] = self.drawn_camera().world_to_screen([x, y, 0.0]);
        vec![px / self.dpr, py / self.dpr]
    }

    pub fn machine_name(&self, index: u32) -> String {
        self.plant
            .as_ref()
            .and_then(|loaded| loaded.machines.get(index as usize))
            .map_or_else(String::new, |machine| machine.name.clone())
    }

    pub fn machine_files(&self, index: u32) -> u32 {
        self.plant
            .as_ref()
            .and_then(|loaded| loaded.machines.get(index as usize))
            .map_or(0, |machine| machine.files as u32)
    }

    /// The height of machine `name`'s model top, in world units, or -1 when the plant has no such
    /// machine. The page picks the machine under the pointer by its true outline with it.
    pub fn machine_top(&self, name: &str) -> f32 {
        let (Some(plan), Some(loaded)) = (self.plan.as_ref(), self.plant.as_ref()) else {
            return -1.0;
        };
        plan.index_of(name)
            .and_then(|index| loaded.stands.get(index))
            .map_or(-1.0, |stand| stand.top)
    }

    pub fn machine_count(&self) -> u32 {
        self.plant
            .as_ref()
            .map_or(0, |loaded| loaded.machines.len() as u32)
    }
}

impl App {
    /// Puts `plant` on screen, with the plan placing its machines where the layout puts them.
    fn load(&mut self, plant: Plant) -> u32 {
        let (kinds, edges) = plant.layout_input();
        let laid = layout(&kinds, &edges);
        let count = plant.machines.len() as u32;
        self.show(plant, laid, [0.0; 2]);
        count
    }

    /// Puts `plant` on screen with its layout from the origin moved to `offset`, and fits it.
    fn show(&mut self, plant: Plant, at_origin: Layout, offset: [f32; 2]) {
        let (loaded, plan) = self.build(plant, at_origin, offset);
        self.plan = Some(plan);
        self.view = None;
        self.marks = Marks::default();
        self.motion = self.new_motion();
        self.card_clash = None;
        self.tour_move = None;
        self.top_move = None;
        self.ride = None;
        self.fit_after_leave = false;
        self.dirty = true;
        self.plant = Some(loaded);
        self.refresh_fog();
        // A new plant keeps the view it had, top-down or not.
        self.fit_view();
        self.sync_scene();
    }

    /// The plant's scene, framing and plan, with its layout from the origin moved to `offset`.
    fn build(&self, plant: Plant, at_origin: Layout, offset: [f32; 2]) -> (Loaded, Plan) {
        let source = plant.clone();
        let laid = if offset == [0.0; 2] {
            at_origin.clone()
        } else {
            at_origin.translated(offset)
        };
        let scene = scene::plant_scene(&laid, &self.models);
        let occluders = scene
            .instances
            .iter()
            .enumerate()
            .map(|(machine, inst)| {
                let model = self.models[usize::from(inst.kind)].bounds;
                let world = Aabb {
                    min: std::array::from_fn(|axis| inst.offset[axis] + model.min[axis]),
                    max: std::array::from_fn(|axis| inst.offset[axis] + model.max[axis]),
                };
                (machine, world)
            })
            .collect();
        let footprints: HashMap<&str, [f32; 4]> = plant
            .machines
            .iter()
            .zip(&laid.machines)
            .map(|(machine, placed)| (machine.name.as_str(), placed.footprint()))
            .collect();
        let plan = Plan::new(&plant, |name| {
            footprints.get(name).copied().unwrap_or([0.0; 4])
        });
        let instance_of: HashMap<&str, usize> = plant
            .machines
            .iter()
            .enumerate()
            .map(|(i, machine)| (machine.name.as_str(), i))
            .collect();
        let plan_instance: Vec<usize> = plan
            .machines
            .iter()
            .map(|machine| instance_of[machine.name.as_str()])
            .collect();
        let stands: Vec<Stand> = plan_instance
            .iter()
            .map(|&i| {
                let instance = &scene.instances[i];
                let kind = usize::from(instance.kind);
                Stand {
                    top: self.models[kind].bounds.max[2],
                    spots: self.kind_spots[kind]
                        .iter()
                        .map(|spot| [instance.offset[0] + spot[0], instance.offset[1] + spot[1]])
                        .collect(),
                }
            })
            .collect();
        let drone_reach = stand::reach(&self.models[usize::from(scene::mark_kinds().body)]);
        let framing = scene::fit_boxes(&laid, &self.models, &self.kind_spots, drone_reach);
        let machine_bounds = scene::machine_model_boxes(&laid, &self.models);
        let loaded = Loaded {
            machine_bounds,
            repo: plant.repo,
            rest: {
                let mut named = vec![false; plant.machines.len()];
                for index in rest_labels(&plant.machines) {
                    named[index] = true;
                }
                named
            },
            label_half_px: plant
                .machines
                .iter()
                .map(|m| m.name.chars().count() as f32 * labels::CHAR_CSS_PX * 0.5)
                .collect(),
            machines: plant.machines,
            layout: laid,
            framing,
            occluders,
            base: scene,
            plan_instance,
            stands,
            source,
            at_origin,
        };
        (loaded, plan)
    }

    /// Folds the log into the view, and places the marks it calls for on the plant.
    fn rebuild_view(&mut self) -> Result<(), JsError> {
        self.dirty = false;
        let (Some(mirror), Some(plan), Some(loaded)) = (&self.mirror, &self.plan, &self.plant)
        else {
            return Ok(());
        };
        let view = mirror.view_still(plan, self.still);
        self.slots.assign(&view);
        let marks = Marks::build_at(&view, plan, &loaded.stands, &self.slots, self.clock);
        self.view_json = serde_json::to_string(&view).map_err(js_err)?;
        self.view_version += 1;
        self.motion.sync(&marks);
        self.marks = marks;
        self.view = Some(view);
        self.apply_focus();
        self.hold_clear();
        self.sync_scene();
        Ok(())
    }

    /// Uploads the plant with the marks: each machine's footprint tint, then the body, ring and
    /// pin instances as the motion has them now.
    fn sync_scene(&mut self) {
        let Some(loaded) = &self.plant else {
            return;
        };
        let mut scene = loaded.base.clone();
        let staged = self.staged();
        for (plan_index, &instance) in loaded.plan_instance.iter().enumerate() {
            scene.instances[instance].tint =
                marks::machine_tint(&self.marks, plan_index, staged.as_deref());
        }
        self.motion.set_px_units(self.dpr / self.camera.zoom());
        let riding = self.riding_agent();
        let hovered = self.hovered.as_deref();
        scene.beams = marks::beams(&self.motion, &self.marks, self.dpr, riding, hovered);
        scene
            .beams
            .extend(marks::spots(&self.motion, &self.marks, self.dpr, riding));
        scene
            .beams
            .extend(marks::trail_beams(&self.motion, self.dpr, riding));
        scene
            .beams
            .extend(marks::ghost_beams(&self.motion, &self.marks, self.dpr));
        scene.instances.extend(marks::mark_instances(
            &self.motion,
            &self.marks,
            scene::mark_kinds(),
            self.dpr,
            riding,
        ));
        let px_units = self.dpr / self.camera.zoom();
        let dpr = self.dpr;
        for parked in &mut self.parked {
            let base = &parked.loaded.base;
            let start = scene.instances.len();
            scene.ribbons.extend(base.ribbons.iter().cloned());
            scene.instances.extend(base.instances.iter().copied());
            let own = &mut parked.marks;
            for (plan_index, &instance) in parked.loaded.plan_instance.iter().enumerate() {
                scene.instances[start + instance].tint =
                    marks::machine_tint(&own.marks, plan_index, None);
            }
            own.motion.set_px_units(px_units);
            if !parked.far {
                scene
                    .beams
                    .extend(marks::beams(&own.motion, &own.marks, dpr, None, None));
                scene
                    .beams
                    .extend(marks::spots(&own.motion, &own.marks, dpr, None));
                scene
                    .beams
                    .extend(marks::trail_beams(&own.motion, dpr, None));
                scene
                    .beams
                    .extend(marks::ghost_beams(&own.motion, &own.marks, dpr));
            }
            scene.instances.extend(marks::mark_instances(
                &own.motion,
                &own.marks,
                scene::mark_kinds(),
                dpr,
                None,
            ));
            scene.bounds = joined(scene.bounds, base.bounds);
        }
        self.renderer.set_scene(&scene);
        self.renderer
            .set_tints(&acan_render::theme::tints_of(self.renderer.look()));
    }

    /// A world point on screen, in CSS px.
    fn screen(&self, point: [f32; 3]) -> [f32; 2] {
        let [x, y] = self.drawn_camera().world_to_screen(point);
        [x / self.dpr, y / self.dpr]
    }

    /// The extra room the fit keeps for the page's overlays: flag chips above the plant and
    /// at its right edge, machine names at the bottom, and the usage panel's corner at the
    /// bottom-left. The panel's room is always kept, so the frame never moves when the panel
    /// first shows.
    fn fit_room(&self) -> FitRoom {
        let [_, vh] = self.camera.viewport();
        FitRoom {
            top: FIT_FLAG_TOP_CSS * self.dpr,
            right: FIT_FLAG_RIGHT_CSS * self.dpr,
            bottom: FIT_LABEL_CSS * self.dpr,
            avoid: labels::panel_avoid(vh, self.dpr),
        }
    }

    /// The zoom at which the plant fills the view at the default angles, in the current
    /// projection. Level of detail is measured against this, so orbiting never changes the
    /// detail.
    fn fit_zoom(&self) -> f32 {
        self.fit_pose().map_or(1.0, |(_, zoom)| zoom)
    }

    /// The pivot and zoom of the fit at the default angles, or `None` before a plant loads.
    fn fit_pose(&self) -> Option<tour::Pose> {
        self.fit_pose_at(DEFAULT_AZIMUTH, DEFAULT_ELEVATION)
    }

    /// The pivot and zoom of the fit at the framing angles, which are the top-down view's while it
    /// is on.
    fn framing_pose(&self) -> Option<tour::Pose> {
        let (azimuth, elevation) = self.framing_angles();
        self.fit_pose_at(azimuth, elevation)
    }

    /// The pivot and zoom that fit the plant at `azimuth` and `elevation` in the current
    /// projection, or `None` before a plant loads.
    fn fit_pose_at(&self, azimuth: f32, elevation: f32) -> Option<tour::Pose> {
        let loaded = self.plant.as_ref()?;
        let mut probe = self.camera;
        probe.set_angles(azimuth, elevation);
        let clear = self.clear_for(elevation);
        fit_clear(
            &mut probe,
            &loaded.framing,
            FIT_MARGIN_CSS * self.dpr,
            self.fit_room(),
            clear,
        );
        Some((probe.pivot(), probe.zoom()))
    }

    /// The screen edges the top-down fit keeps every corner inside, in device pixels as
    /// `[top, right]`: the bottom of the shell bar and the left edge of the right column. `None`
    /// off the top-down view, where the fit keeps only its own room.
    fn clear_for(&self, elevation: f32) -> Option<[f32; 2]> {
        if !topdown::is_top_down(elevation) {
            return None;
        }
        let [width, _] = self.camera.viewport();
        Some([
            self.fit_inset[0] * self.dpr,
            width - self.fit_inset[1] * self.dpr,
        ])
    }

    fn lod(&self) -> Lod {
        Lod::for_ratio(self.camera.zoom() / self.fit_zoom())
    }

    /// Eases the camera's pivot and zoom to `to`, as a tour step does. With `reduced`, or on a
    /// still page, the camera jumps.
    fn ease_to(&mut self, to: tour::Pose, reduced: bool) {
        if reduced || self.still {
            self.tour_move = None;
            self.camera.set_pivot(to.0);
            self.camera.set_zoom(to.1);
        } else {
            let from = (self.camera.pivot(), self.camera.zoom());
            self.tour_move = Some(TourMove {
                from,
                to,
                elapsed_ms: 0.0,
            });
        }
    }

    /// Keeps the fog clear of every plant on the canvas, so a neighbour is not fogged out.
    fn refresh_fog(&mut self) {
        let Some(loaded) = &self.plant else {
            return;
        };
        let bounds = self.parked.iter().fold(loaded.base.bounds, |acc, parked| {
            joined(acc, parked.loaded.base.bounds)
        });
        self.camera.set_fog_bounds(bounds);
    }

    /// The pivot and zoom that fit `framing` at the framing angles.
    fn pose_of(&self, framing: &[Aabb]) -> tour::Pose {
        let mut probe = self.camera;
        let (azimuth, elevation) = self.framing_angles();
        probe.set_angles(azimuth, elevation);
        let clear = self.clear_for(elevation);
        fit_clear(
            &mut probe,
            framing,
            FIT_MARGIN_CSS * self.dpr,
            self.fit_room(),
            clear,
        );
        (probe.pivot(), probe.zoom())
    }

    /// The plant's layout from the origin, from the cache when the same kinds and belts were laid
    /// out before.
    fn laid_out(&mut self, plant: &Plant) -> Layout {
        let key = canvas::layout_key(plant);
        if let Some(laid) = self.layouts.get(&key) {
            return laid.clone();
        }
        let (kinds, edges) = plant.layout_input();
        let laid = layout(&kinds, &edges);
        self.layouts.insert(key, laid.clone());
        laid
    }

    /// Every project's marks and eases with its overlay index and id: the one on screen, then the
    /// parked ones, leaving out the far ones unless `far`.
    fn every_project(&self, far: bool) -> Vec<(f32, &Marks, &Motion, &str)> {
        let mut out = Vec::new();
        if self.plant.is_some() {
            out.push((
                self.project_index(&self.project),
                &self.marks,
                &self.motion,
                self.project.as_str(),
            ));
        }
        for parked in self.parked.iter().filter(|parked| far || !parked.far) {
            out.push((
                self.project_index(&parked.id),
                &parked.marks.marks,
                &parked.marks.motion,
                parked.id.as_str(),
            ));
        }
        out
    }

    /// The overlay index of project `id`: its place in `order`, or 0 on a page with one plant.
    fn project_index(&self, id: &str) -> f32 {
        self.order.iter().position(|known| known == id).unwrap_or(0) as f32
    }

    /// Asks every parked project to fold its marks again, as the clock moving does.
    fn parked_dirty(&mut self) {
        for parked in &mut self.parked {
            parked.marks.dirty = true;
        }
    }

    /// Folds each parked project whose log moved, and sets each one's level of detail from where
    /// the camera stands. Returns whether either changed, so the scene uploads again.
    fn refresh_parked(&mut self) -> bool {
        if self.parked.is_empty() {
            return false;
        }
        let camera = self.drawn_camera();
        let dpr = self.dpr;
        let [vw, vh] = camera.viewport();
        let viewport = [vw / dpr, vh / dpr];
        let clock = self.clock;
        let still = self.still;
        let mut changed = false;
        for parked in &mut self.parked {
            if parked.marks.dirty {
                match &parked.mirror {
                    Some(mirror) => {
                        let stands = &parked.loaded.stands;
                        parked
                            .marks
                            .rebuild(mirror, &parked.plan, stands, clock, still);
                        parked.view_json = None;
                        changed = true;
                    }
                    None => parked.marks.dirty = false,
                }
            }
            let seen = screen_box(&camera, parked.loaded.layout.bounds, dpr);
            let far = canvas::far(seen, viewport);
            if far != parked.far {
                parked.far = far;
                changed = true;
            }
        }
        if changed {
            self.parked_version += 1;
        }
        changed
    }

    fn parked_mut(&mut self, id: &str) -> Result<&mut Parked, JsError> {
        self.parked
            .iter_mut()
            .find(|parked| parked.id == id)
            .ok_or_else(|| js_err("the canvas holds no such project"))
    }

    fn parked_index(&self, id: &str) -> Option<usize> {
        self.parked.iter().position(|parked| parked.id == id)
    }
}

/// The canvas of several projects. A page with one plant never calls these.
#[wasm_bindgen]
impl App {
    /// The bounds `[min x, min y, max x, max y]` of a plant from `GET /api/p/{id}/plant`, as its
    /// layout places it from the origin: everything it draws, models and belts included. The page
    /// packs the plants with them. They reach the page as a `Float32Array`.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON is not the plant's shape.
    pub fn plant_bounds(&mut self, json: &str) -> Result<Vec<f32>, JsError> {
        let plant = from_scan_json(json).map_err(js_err)?;
        let laid = self.laid_out(&plant);
        Ok(canvas::drawn_bounds(&laid, &self.models).to_vec())
    }

    /// Puts project `id`'s plant on the canvas with its layout's origin at `(x, y)`. The first
    /// project goes on screen and is fitted; a later one is parked beside it. A project the canvas
    /// holds already takes the new plant in place and keeps its log. Returns the machine count.
    ///
    /// # Errors
    ///
    /// Returns an error when the JSON is not the plant's shape.
    pub fn add_project(&mut self, id: &str, json: &str, x: f32, y: f32) -> Result<u32, JsError> {
        let plant = from_scan_json(json).map_err(js_err)?;
        let count = plant.machines.len() as u32;
        let at_origin = self.laid_out(&plant);
        if !self.order.iter().any(|known| known == id) {
            self.order.push(id.to_owned());
        }
        if self.plant.is_none() || self.project == id {
            self.project = id.to_owned();
            self.show(plant, at_origin, [x, y]);
            return Ok(count);
        }
        let (loaded, plan) = self.build(plant, at_origin, [x, y]);
        let motion = self.new_motion();
        match self.parked_index(id) {
            Some(index) => {
                let parked = &mut self.parked[index];
                parked.loaded = loaded;
                parked.plan = plan;
                parked.view_json = None;
                parked.marks.dirty = true;
                parked.marks.motion.cut();
            }
            None => self.parked.push(Parked {
                id: id.to_owned(),
                loaded,
                plan,
                mirror: None,
                view_json: None,
                marks: ProjectMarks::new(motion),
                far: false,
            }),
        }
        self.refresh_fog();
        self.sync_scene();
        Ok(count)
    }

    /// Moves project `id`'s plant so its layout's origin sits at `(x, y)`. The marks of the
    /// project on screen follow on the next frame. Returns false for a project the canvas lacks.
    pub fn move_project(&mut self, id: &str, x: f32, y: f32) -> bool {
        if id == self.project {
            let Some(old) = self.plant.take() else {
                return false;
            };
            let (loaded, plan) = self.build(old.source, old.at_origin, [x, y]);
            self.plant = Some(loaded);
            self.plan = Some(plan);
            self.dirty = true;
        } else if let Some(index) = self.parked_index(id) {
            let old = &self.parked[index].loaded;
            let (loaded, plan) = self.build(old.source.clone(), old.at_origin.clone(), [x, y]);
            let parked = &mut self.parked[index];
            parked.loaded = loaded;
            parked.plan = plan;
            parked.view_json = None;
            parked.marks.dirty = true;
            parked.marks.motion.cut();
        } else {
            return false;
        }
        self.refresh_fog();
        self.sync_scene();
        true
    }

    /// Puts project `id` on screen: its marks, names, flags and view take over, and the project
    /// that was on screen is parked as plain plant. The camera does not move. Returns false when
    /// the canvas lacks the project or its log has not loaded.
    pub fn activate(&mut self, id: &str) -> bool {
        if id == self.project {
            return self.plant.is_some();
        }
        let Some(index) = self.parked_index(id) else {
            return false;
        };
        if self.parked[index].mirror.is_none() {
            return false;
        }
        let next = self.parked.remove(index);
        if let (Some(loaded), Some(plan)) = (self.plant.take(), self.plan.take()) {
            let view_json = (self.view.is_some() && !self.dirty).then(|| self.view_json.clone());
            let marks = ProjectMarks {
                view: self.view.take(),
                marks: std::mem::take(&mut self.marks),
                motion: std::mem::take(&mut self.motion),
                slots: std::mem::take(&mut self.slots),
                dirty: self.dirty,
                moving: self.moving,
            };
            self.parked.insert(
                index,
                Parked {
                    id: std::mem::take(&mut self.project),
                    loaded,
                    plan,
                    mirror: self.mirror.take(),
                    view_json,
                    marks,
                    far: false,
                },
            );
        }
        self.project = next.id;
        self.plant = Some(next.loaded);
        self.plan = Some(next.plan);
        self.mirror = next.mirror;
        // The project keeps the marks it had while parked, so its drones do not drop in again.
        self.view = next.marks.view;
        self.marks = next.marks.marks;
        self.motion = next.marks.motion;
        self.slots = next.marks.slots;
        self.moving = next.marks.moving;
        self.hovered = None;
        self.parked_version += 1;
        self.card_clash = None;
        self.stage.clear();
        self.focus = None;
        self.ride = None;
        self.fit_after_leave = false;
        self.dirty = true;
        self.sync_scene();
        true
    }

    /// The id of the project on screen, or an empty string on a page with one plant.
    pub fn active_project(&self) -> String {
        self.project.clone()
    }

    /// Folds project `id`'s log, as `load_log` does for the project on screen.
    ///
    /// # Errors
    ///
    /// Returns an error for a project the canvas lacks, or a log that does not parse or replay.
    pub fn project_load_log(&mut self, id: &str, json: &str) -> Result<(), JsError> {
        if id == self.project {
            return self.load_log(json);
        }
        let entries = feed::entries(json).map_err(js_err)?;
        let mirror = Mirror::from_entries(entries).map_err(js_err)?;
        let parked = self.parked_mut(id)?;
        parked.mirror = Some(mirror);
        parked.view_json = None;
        parked.marks.dirty = true;
        parked.marks.motion.cut();
        Ok(())
    }

    /// Applies one streamed event of project `id`, as `ingest` does for the project on screen.
    ///
    /// # Errors
    ///
    /// Returns an error for a project the canvas lacks or whose log is not loaded, or an event
    /// that does not apply.
    pub fn project_ingest(&mut self, id: &str, json: &str) -> Result<(), JsError> {
        if id == self.project {
            return self.ingest(json);
        }
        let entry = feed::entry(json).map_err(js_err)?;
        let parked = self.parked_mut(id)?;
        let mirror = parked
            .mirror
            .as_mut()
            .ok_or_else(|| js_err("no log is loaded"))?;
        mirror.ingest_entry(entry).map_err(js_err)?;
        parked.view_json = None;
        parked.marks.dirty = true;
        Ok(())
    }

    /// The seq project `id`'s next event will carry, or 0 before its log loads.
    pub fn project_next_seq(&self, id: &str) -> f64 {
        if id == self.project {
            return self.next_seq();
        }
        self.parked
            .iter()
            .find(|parked| parked.id == id)
            .and_then(|parked| parked.mirror.as_ref())
            .map_or(0.0, |mirror| mirror.next_seq().get() as f64)
    }

    /// Project `id`'s view as JSON, or an empty string before its log loads. A parked project's
    /// view is folded when it is asked for after a change.
    pub fn project_view_json(&mut self, id: &str) -> String {
        if id == self.project {
            return self.view_json.clone();
        }
        let Some(index) = self.parked_index(id) else {
            return String::new();
        };
        let still = self.still;
        let parked = &mut self.parked[index];
        if parked.view_json.is_none() {
            parked.view_json = parked.mirror.as_ref().and_then(|mirror| {
                serde_json::to_string(&mirror.view_still(&parked.plan, still)).ok()
            });
        }
        parked.view_json.clone().unwrap_or_default()
    }

    /// Eases the camera to fit project `id`'s plant at the current angles, as a tour step moves.
    /// Returns false for a project the canvas lacks, or while riding, so the page leaves the
    /// ride first.
    pub fn fly_to(&mut self, id: &str, reduced: bool) -> bool {
        if self.ride.is_some() {
            return false;
        }
        let framing = if id == self.project {
            self.plant.as_ref().map(|loaded| loaded.framing.clone())
        } else {
            self.parked
                .iter()
                .find(|parked| parked.id == id)
                .map(|parked| parked.loaded.framing.clone())
        };
        let Some(framing) = framing else {
            return false;
        };
        self.turn = None;
        let to = self.pose_of(&framing);
        self.ease_to(to, reduced);
        true
    }

    /// The project whose plant holds the ground under the CSS point, or the nearest one. Empty
    /// when the canvas holds no project or the point is above the horizon.
    pub fn project_under(&self, css_x: f32, css_y: f32) -> String {
        let Some(point) = self.camera.ground_at([css_x * self.dpr, css_y * self.dpr]) else {
            return String::new();
        };
        let mut ids = Vec::new();
        let mut boxes = Vec::new();
        if let Some(loaded) = &self.plant {
            ids.push(self.project.as_str());
            boxes.push(loaded.layout.bounds);
        }
        for parked in &self.parked {
            ids.push(parked.id.as_str());
            boxes.push(parked.loaded.layout.bounds);
        }
        canvas::nearest(&boxes, point).map_or_else(String::new, |index| ids[index].to_owned())
    }

    /// The plan point on the ground under the CSS point, as `[x, y]`, or empty above the horizon.
    pub fn screen_to_ground(&self, css_x: f32, css_y: f32) -> Vec<f32> {
        self.camera
            .ground_at([css_x * self.dpr, css_y * self.dpr])
            .map_or_else(Vec::new, |point| point.to_vec())
    }
}

/// Every project's marks on the canvas. Each entry names its project by its place in
/// `canvas_order`. The one-project exports above keep their shapes and list the project on screen
/// only; these list it too, then every parked project. A parked project at the far level of
/// detail lists its drones and pins only.
#[wasm_bindgen]
impl App {
    /// The project ids as JSON, in the order the overlay entries index them.
    pub fn canvas_order(&self) -> String {
        serde_json::to_string(&self.order).expect("ids serialize as JSON")
    }

    /// Moves when a parked project's marks or level of detail change. The page reads
    /// `all_marks_json` again then.
    pub fn parked_version(&self) -> u32 {
        self.parked_version
    }

    /// Every project's flags and clash badges, as JSON `[{"project", "id", "far", "flags",
    /// "badges"}]`, in the shapes `marks_json` gives them. Flag `i` of project `p` anchors where
    /// `all_flag_layout` puts `[p, i]`.
    pub fn all_marks_json(&self) -> String {
        #[derive(serde::Serialize)]
        struct Entry<'a> {
            project: f32,
            id: &'a str,
            far: bool,
            flags: Vec<marks::Flag>,
            badges: Vec<marks::Badges>,
        }
        let mut out = Vec::new();
        if self.plant.is_some() {
            let badges = self.view.as_ref().map_or_else(Vec::new, |view| {
                marks::clash_badges(view, &self.marks.pins, &self.slots)
            });
            out.push(Entry {
                project: self.project_index(&self.project),
                id: &self.project,
                far: false,
                flags: self.all_flags(),
                badges,
            });
        }
        for parked in &self.parked {
            let own = &parked.marks;
            let badges = own.view.as_ref().map_or_else(Vec::new, |view| {
                marks::clash_badges(view, &own.marks.pins, &own.slots)
            });
            out.push(Entry {
                project: self.project_index(&parked.id),
                id: &parked.id,
                far: parked.far,
                flags: own.motion.flags_now(&own.marks.flags),
                badges,
            });
        }
        serde_json::to_string(&out).expect("marks serialize as JSON")
    }

    /// Every near project's flag anchors, as flat `[project, flag index, machine or -1, css x,
    /// css y]` quints. See `flag_layout`.
    pub fn all_flag_layout(&self) -> Vec<f32> {
        let screen = |p| self.screen(p);
        let mut out = Vec::new();
        if self.plant.is_some() {
            let front = self.project_index(&self.project);
            for quad in project::flag_quads(&self.all_flags(), &self.motion, screen) {
                out.push(front);
                out.extend(quad);
            }
        }
        for parked in self.parked.iter().filter(|parked| !parked.far) {
            let own = &parked.marks;
            let index = self.project_index(&parked.id);
            let flags = own.motion.flags_now(&own.marks.flags);
            for quad in project::flag_quads(&flags, &own.motion, screen) {
                out.push(index);
                out.extend(quad);
            }
        }
        out
    }

    /// Every project's clash diamonds, far ones too, as flat `[project, clash index, css x, css
    /// y]` quads. See `pin_layout`.
    pub fn all_pin_layout(&self) -> Vec<f32> {
        let screen = |p| self.screen(p);
        let mut out = Vec::new();
        for (index, marks, motion, _) in self.every_project(true) {
            for triple in project::pin_triples(marks, motion, screen) {
                out.push(index);
                out.extend(triple);
            }
        }
        out
    }

    /// Every near project's clash badges, as flat `[project, clash index, left css x, right css
    /// x, css y]` quints. See `badge_layout`.
    pub fn all_badge_layout(&self) -> Vec<f32> {
        let screen = |p| self.screen(p);
        let mut out = Vec::new();
        for (index, marks, motion, _) in self.every_project(false) {
            for quad in project::badge_quads(marks, motion, screen) {
                out.push(index);
                out.extend(quad);
            }
        }
        out
    }

    /// Every project's drone discs, far ones too, as flat `[project, css x, css y, css radius]`
    /// quads. See `drone_layout`.
    pub fn all_drone_layout(&self) -> Vec<f32> {
        let screen = |p| self.screen(p);
        let mut out = Vec::new();
        for (index, marks, motion, _) in self.every_project(true) {
            for (_, disc) in project::drone_discs(marks, motion, None, screen) {
                out.push(index);
                out.extend(disc);
            }
        }
        out
    }

    /// The drone under the CSS point `(x, y)` on any plant, as JSON `{"project", "id", "agent"}`,
    /// or the empty string. The riding agent's drone is left out, as `drone_at` leaves it.
    pub fn all_drone_at(&self, x: f32, y: f32) -> String {
        let screen = |p| self.screen(p);
        let riding = self.riding_agent();
        let mut hits: Vec<(f32, &str, &str)> = Vec::new();
        let mut discs: Vec<([f32; 2], f32)> = Vec::new();
        for (index, marks, motion, id) in self.every_project(true) {
            let skip = if id == self.project { riding } else { None };
            for (agent, [cx, cy, radius]) in project::drone_discs(marks, motion, skip, screen) {
                hits.push((index, id, agent));
                discs.push(([cx, cy], radius));
            }
        }
        marks::disc_under(&discs, [x, y]).map_or_else(String::new, |i| {
            let (project, id, agent) = hits[i];
            serde_json::json!({ "project": project, "id": id, "agent": agent }).to_string()
        })
    }

    /// Every near project's ghosts, as flat `[project, css x, css y, landed count]` quads. See
    /// `ghost_layout`.
    pub fn all_ghost_layout(&self) -> Vec<f32> {
        let screen = |p| self.screen(p);
        let mut out = Vec::new();
        for (index, marks, motion, _) in self.every_project(false) {
            for triple in project::ghost_triples(marks, motion, screen) {
                out.push(index);
                out.extend(triple);
            }
        }
        out
    }

    /// Every near project's working beacons, as flat `[project, css x, css y, css radius]` quads.
    /// See `beacon_layout`.
    pub fn all_beacon_layout(&self) -> Vec<f32> {
        let screen = |p| self.screen(p);
        let mut out = Vec::new();
        for (index, marks, motion, _) in self.every_project(false) {
            for ([x, y], radius) in marks::beacon_discs(motion, marks, screen) {
                out.extend([index, x, y, radius]);
            }
        }
        out
    }

    /// Every near project's machine names, as flat `[project, machine index, css x, css y,
    /// opacity]` quints. See `label_layout`.
    pub fn all_label_layout(&self, all: bool) -> Vec<f32> {
        let mut out = Vec::new();
        let mut add = |index: f32, quads: Vec<f32>| {
            for quad in quads.as_chunks::<4>().0 {
                out.push(index);
                out.extend_from_slice(quad);
            }
        };
        if let Some(loaded) = &self.plant {
            add(
                self.project_index(&self.project),
                self.labels_of(loaded, all),
            );
        }
        for parked in self.parked.iter().filter(|parked| !parked.far) {
            add(
                self.project_index(&parked.id),
                self.labels_of(&parked.loaded, all),
            );
        }
        out
    }

    /// The name of machine `index` on project `id`'s plant, or an empty string.
    pub fn project_machine_name(&self, id: &str, index: u32) -> String {
        let loaded = if id == self.project {
            self.plant.as_ref()
        } else {
            self.parked
                .iter()
                .find(|parked| parked.id == id)
                .map(|parked| &parked.loaded)
        };
        loaded
            .and_then(|loaded| loaded.machines.get(index as usize))
            .map_or_else(String::new, |machine| machine.name.clone())
    }
}

/// The box `[min x, min y, max x, max y]` in CSS px that a plan box covers on the ground.
fn screen_box(camera: &Camera, b: [f32; 4], dpr: f32) -> [f32; 4] {
    let corners = [[b[0], b[1]], [b[2], b[1]], [b[0], b[3]], [b[2], b[3]]]
        .map(|[x, y]| camera.world_to_screen([x, y, 0.0]).map(|v| v / dpr));
    corners.iter().fold(
        [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ],
        |acc, p| {
            [
                acc[0].min(p[0]),
                acc[1].min(p[1]),
                acc[2].max(p[0]),
                acc[3].max(p[1]),
            ]
        },
    )
}
/// The box round two boxes. An empty box adds nothing.
fn joined(a: Aabb, b: Aabb) -> Aabb {
    let mut out = a;
    if b.min[0] <= b.max[0] {
        out.include(b.min);
        out.include(b.max);
    }
    out
}

fn js_err(err: impl std::fmt::Display) -> JsError {
    JsError::new(&err.to_string())
}
