//! View-independent 3D models of the plant's machines.
//!
//! A model is a [`Model`]: one triangle mesh per level of detail, painted lines and dots, and a bounding box.
//! It is in plan units with z up and its base on the ground, so the renderer can orbit it through 360 degrees
//! and shade it with [`tone_for`]. Each item carries a [`Detail`] so the renderer decides what to show at a
//! given zoom. The kit's proportions and palette are the reference (MIT, see `LICENSE-anatomy`).

mod drone;
mod marks;
mod mesh;
mod model;
mod models;
mod overlay;
mod parts;
mod plan;
mod shade;

pub use drone::{
    BEACON_RADIUS, BODY_RADIUS, BOT_HEIGHT, DroneParts, EYE_DISTANCE, EYE_TILT_DEG, SHADOW_RADIUS,
    SHADOW_RISE, agent_accent, agent_body, beacon, drone_parts, eye_point, shadow_at,
};
pub use marks::{PIN_HEIGHT, PLINTH_TOP, STALK_HEIGHT, clash_pin, clash_stalk, waiting_ring};
pub use model::{Aabb, Detail, Dot3, Ink, Line3, Mesh, Model, Surface};
pub use models::{Footprint, Kind, footprint, model};
pub use overlay::{Overlay, bar_count, overlay_model};
pub use shade::{Tone, tone_for};
