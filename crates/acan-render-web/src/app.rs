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
use crate::scene;
use crate::stand;

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

#[wasm_bindgen]
pub struct App {
    renderer: Renderer,
    camera: Camera,
    models: Vec<Model>,
    /// The free spots a bot can stand on, per model kind, relative to the model's origin.
    kind_spots: Vec<Vec<[f32; 2]>>,
    dpr: f32,
    turn: Option<Turn>,
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
    /// Whether the marks hold still, as `?freeze=1` asks. Set once before a plant loads.
    still: bool,
    /// The index of the clash whose card is open, if any.
    card_clash: Option<usize>,
    /// An agent to close up on, and the zoom as a share of the fit zoom. It applies once the view
    /// places the agent's drone, then clears.
    focus: Option<(String, f32)>,
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
}

/// An ease from one azimuth to another, in degrees. `to` is not wrapped, so repeated turns add up.
struct Turn {
    from: f32,
    to: f32,
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
    /// The belts that draw, with the instance index of the machines they join.
    belts: Vec<scene::BeltLine>,
    /// For each plan machine, its instance in `base`.
    plan_instance: Vec<usize>,
    /// For each plan machine, its model's top and the free spots on its plinth top.
    stands: Vec<Stand>,
    /// Each machine's model box, which a ride sizes its distance from, in the layout's machine
    /// order. See `scene::machine_model_boxes`.
    machine_bounds: Vec<Aabb>,
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
            plant: None,
            plan: None,
            mirror: None,
            view: None,
            marks: Marks::default(),
            motion: Motion::default(),
            slots: AgentSlots::default(),
            moving: false,
            still: false,
            card_clash: None,
            focus: None,
            ride: None,
            fit_after_leave: false,
            bob_enabled: true,
            bob_ms: 0.0,
            view_json: String::new(),
            view_version: 0,
            dirty: false,
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

    /// Holds the marks still, as `?freeze=1` asks. Call it before the plant loads: every ease then
    /// lands at once, a leaving bot is gone, no flag fades, and a landed dot keeps its word.
    pub fn set_still(&mut self, still: bool) {
        self.still = still;
        self.motion = Motion::new(still);
        self.dirty = true;
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
        if let Some(loaded) = &self.plant {
            self.camera
                .fit_boxes_room(&loaded.framing, FIT_MARGIN_CSS * self.dpr, self.fit_room());
        }
    }

    /// Turns the view by `(dx, dy)` CSS pixels of drag: horizontal orbits, vertical tilts. This
    /// cancels a turn in progress, so the drag always wins.
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        if self.ride.is_some() {
            return;
        }
        self.turn = None;
        self.camera
            .orbit(dx * ORBIT_DEG_PER_PX, -dy * TILT_DEG_PER_PX);
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
        let moving = self.motion.advance(dt_ms);
        // The frame an ease ends on still uploads, so the marks land on their final places.
        if moving || self.moving {
            self.sync_scene();
        }
        self.moving = moving;
        let riding = self.advance_ride(dt_ms);
        turning || moving || riding
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

    /// Zooms by `factor` around the CSS point `(x, y)`, which stays under the pointer.
    pub fn zoom_at(&mut self, x: f32, y: f32, factor: f32) {
        if self.ride.is_some() {
            return;
        }
        self.camera.zoom_at([x * self.dpr, y * self.dpr], factor);
    }

    /// Turns to the given angles in degrees and fits the plant there, centred on screen.
    pub fn view(&mut self, azimuth: f32, elevation: f32) {
        self.turn = None;
        self.camera.set_angles(azimuth, elevation);
        if let Some(loaded) = &self.plant {
            self.camera
                .fit_boxes_room(&loaded.framing, FIT_MARGIN_CSS * self.dpr, self.fit_room());
        }
    }

    /// Returns to the default angles and fits the plant. While riding, the ride eases back first
    /// and the fit runs once it has landed.
    pub fn fit(&mut self) {
        if self.ride.is_some() {
            self.leave();
            self.fit_after_leave = true;
            return;
        }
        self.fit_view();
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

    /// Turns the hover bob on or off. The page turns it off under reduced motion.
    pub fn set_bob(&mut self, enabled: bool) {
        self.bob_enabled = enabled;
    }

    /// Returns to the default angles and fits the plant.
    fn fit_view(&mut self) {
        self.turn = None;
        if let Some(loaded) = &self.plant {
            self.camera.set_angles(DEFAULT_AZIMUTH, DEFAULT_ELEVATION);
            self.camera
                .fit_boxes_room(&loaded.framing, FIT_MARGIN_CSS * self.dpr, self.fit_room());
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
    /// `ratio` times the fit zoom. The page asks for it with `?focus=`. It waits until the view
    /// places the drone, so it may be called before the log loads.
    pub fn focus_agent(&mut self, name: &str, ratio: f32) {
        self.focus = Some((name.to_owned(), ratio));
        self.apply_focus();
    }

    /// Applies a pending focus once the agent's drone is placed, then clears it.
    fn apply_focus(&mut self) {
        let Some((name, ratio)) = self.focus.as_ref() else {
            return;
        };
        let Some(body) = self.marks.bodies.iter().find(|body| &body.agent == name) else {
            return;
        };
        let at = self.motion.body_at(&body.agent).unwrap_or(body.at);
        let zoom = ratio * self.fit_zoom();
        self.camera.centre_on(at);
        self.camera.set_zoom(zoom);
        self.focus = None;
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
        if self.dirty {
            self.rebuild_view()?;
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
        let Some(loaded) = &self.plant else {
            return Vec::new();
        };
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

    /// The flags and the open clash card, as JSON: `{"flags": [...], "card": {...} | null}`. The
    /// page reads it when the view changes or a card opens. Flag `i` anchors where `flag_layout`
    /// puts index `i`.
    pub fn marks_json(&self) -> String {
        #[derive(serde::Serialize)]
        struct Out {
            flags: Vec<marks::Flag>,
            card: Option<marks::Card>,
        }
        let card = self.card_clash.and_then(|index| {
            let view = self.view.as_ref()?;
            let mirror = self.mirror.as_ref()?;
            marks::card(view, mirror.world(), index, &self.slots)
        });
        serde_json::to_string(&Out {
            flags: self.motion.flags_now(&self.marks.flags),
            card,
        })
        .expect("marks serialize as JSON")
    }

    /// Each flag's anchor on screen, as flat `[flag index, machine or -1, css x, css y]` quads, in
    /// the order `marks_json` lists the flags. The machine is the one an agent's flag stacks on,
    /// and -1 is a flag that stands alone. The anchors follow the bodies and diamonds while they
    /// move.
    pub fn flag_layout(&self) -> Vec<f32> {
        self.motion
            .flags_now(&self.marks.flags)
            .iter()
            .enumerate()
            .flat_map(|(index, flag)| {
                let at = self.motion.flag_at(flag);
                let [x, y] = marks::flag_anchor(flag, at, |p| self.screen(p));
                let machine = flag.machine.map_or(-1.0, |machine| machine as f32);
                [index as f32, machine, x, y]
            })
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

    /// Each drone's disc on screen, as flat `[css x, css y, css radius]` triples, in the order the
    /// marks list the drones. The page keeps flags and names clear of them.
    pub fn drone_layout(&self) -> Vec<f32> {
        self.marks
            .bodies
            .iter()
            .flat_map(|body| {
                let at = self.motion.body_at(&body.agent).unwrap_or(body.at);
                let ([x, y], radius) = marks::drone_disc(at, |p| self.screen(p));
                [x, y, radius]
            })
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
        let scene = scene::plant_scene(&laid, &self.models);
        self.camera.set_fog_bounds(scene.bounds);
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
        let count = plant.machines.len() as u32;
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
        self.plan = Some(plan);
        self.view = None;
        self.marks = Marks::default();
        self.motion = Motion::new(self.still);
        self.card_clash = None;
        self.ride = None;
        self.fit_after_leave = false;
        self.dirty = true;
        let belts = scene::belt_lines(&laid.belts);
        let drone_reach = stand::reach(&self.models[usize::from(scene::mark_kinds().body)]);
        let framing = scene::fit_boxes(&laid, &self.models, &self.kind_spots, drone_reach);
        let machine_bounds = scene::machine_model_boxes(&laid, &self.models);
        self.plant = Some(Loaded {
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
            belts,
            plan_instance,
            stands,
        });
        self.fit();
        self.sync_scene();
        count
    }

    /// Folds the log into the view, and places the marks it calls for on the plant.
    fn rebuild_view(&mut self) -> Result<(), JsError> {
        self.dirty = false;
        let (Some(mirror), Some(plan), Some(loaded)) = (&self.mirror, &self.plan, &self.plant)
        else {
            return Ok(());
        };
        let view = mirror.view(plan);
        self.slots.assign(&view);
        let marks = Marks::build(&view, plan, &loaded.stands, &self.slots);
        self.view_json = serde_json::to_string(&view).map_err(js_err)?;
        self.view_version += 1;
        self.motion.sync(&marks);
        self.marks = marks;
        self.view = Some(view);
        self.apply_focus();
        self.sync_scene();
        Ok(())
    }

    /// Uploads the plant with the marks: each machine's footprint tint, then the body, ring and
    /// pin instances as the motion has them now. The tint table follows the footprints.
    fn sync_scene(&mut self) {
        let Some(loaded) = &self.plant else {
            return;
        };
        let mut scene = loaded.base.clone();
        for (plan_index, &instance) in loaded.plan_instance.iter().enumerate() {
            scene.instances[instance].tint = self.marks.footprint_tint(plan_index);
        }
        let hues: Vec<(usize, u8)> = self
            .marks
            .belt_hues
            .iter()
            .map(|&(plan_index, tint)| (loaded.plan_instance[plan_index], tint))
            .collect();
        scene.hue_lines = scene::hue_lines(&loaded.belts, &hues);
        let riding = self.riding_agent();
        scene.beams = marks::beams(&self.motion, &self.marks, self.dpr, riding);
        scene.instances.extend(marks::mark_instances(
            &self.motion,
            &self.marks,
            scene::mark_kinds(),
            self.dpr,
            riding,
        ));
        self.renderer.set_scene(&scene);
        self.renderer.set_tints(&self.marks.tint_table());
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
        match &self.plant {
            Some(loaded) => {
                let mut probe = self.camera;
                probe.set_angles(DEFAULT_AZIMUTH, DEFAULT_ELEVATION);
                probe.fit_boxes_room(&loaded.framing, FIT_MARGIN_CSS * self.dpr, self.fit_room());
                probe.zoom()
            }
            None => 1.0,
        }
    }

    fn lod(&self) -> Lod {
        Lod::for_ratio(self.camera.zoom() / self.fit_zoom())
    }
}

fn js_err(err: impl std::fmt::Display) -> JsError {
    JsError::new(&err.to_string())
}
