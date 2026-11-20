//! Placement and routing geometry for a plant drawn as machines on a plan.
//!
//! Each machine is a plan size, `[width, depth]`, and nothing else; the crate knows no machine
//! kinds. [`route`] finds orthogonal belt paths round the footprints, and [`bends`] rounds their
//! corners.

pub mod bends;
pub mod route;
