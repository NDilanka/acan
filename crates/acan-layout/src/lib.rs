//! Placement and routing geometry for a plant drawn as machines on a plan.
//!
//! The geometry sees each machine as a plan size, `[width, depth]`, and nothing else about it.
//! [`place`] puts the machines on a floor grid, grouped by district. [`layout`] places them and
//! routes each import as a belt. [`route`] finds the orthogonal belt paths round the footprints,
//! and [`bends`] rounds their corners. [`plant`] names a plant's machines and districts from
//! their file paths, and [`plan`] indexes the laid-out machines by name and by file path.

pub mod bends;
pub mod layout;
pub mod place;
pub mod plan;
pub mod plant;
pub mod route;

#[cfg(test)]
mod testdata;
