pub mod algorithm;
pub mod model;
pub mod parse;
// The panel owns desktop AppState; portable clients consume the diff model.
#[cfg(feature = "desktop")]
mod view;

#[cfg(feature = "desktop")]
pub use view::DiffPanel;
