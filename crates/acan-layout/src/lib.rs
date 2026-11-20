//! Placement and routing geometry for a plant drawn as machines on a plan.
//!
//! Each machine is a plan size, `[width, depth]`, and nothing else about it. [`place`] puts the
//! machines on a floor grid, grouped by district. [`layout`] places them and routes each import
//! as a belt. [`route`] finds the orthogonal belt paths round the footprints, and [`bends`]
//! rounds their corners.

pub mod bends;
pub mod layout;
pub mod place;
pub mod route;

#[cfg(test)]
mod testdata;
