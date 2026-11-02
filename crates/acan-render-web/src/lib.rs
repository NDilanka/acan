//! Browser host for `acan-render`. The plant input, the kind table, the layout, the scene and the
//! plan build on every target, so `cargo test` covers them on the host. The product screen's
//! state (the origin's log folded into a view) is here too. `App` is the wasm surface that the
//! page drives.

pub mod api;
pub mod bends;
pub mod feed;
pub mod kind;
pub mod labels;
pub mod layout;
pub mod marks;
pub mod mirror;
pub mod plan;
pub mod plant;
pub mod plant_view;
pub mod pov;
pub mod scenario;
pub mod scene;
pub mod stand;

#[cfg(target_arch = "wasm32")]
mod app;

#[cfg(target_arch = "wasm32")]
pub use app::App;
