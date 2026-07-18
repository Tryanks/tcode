//! Rushdown-backed Markdown rendering and window-level selection.
//!
//! The GPUI element architecture is adapted from gpui-component's Apache-2.0
//! `crates/ui/src/text` implementation. Parsing and highlighting use tcode's
//! rushdown IR and syntect bridge.

mod inline;
mod inline_flow;
pub(crate) mod nodes;
pub(crate) mod parse;
mod render;
mod selection;
mod state;
mod style;
mod utils;
mod view;
mod window_selection;

use gpui::{App, KeyBinding};
use gpui_component::input::{Copy, SelectAll};

pub(crate) use parse::parse;
pub use state::MarkdownState;
pub use style::TextViewStyle;
pub use view::MarkdownView;
pub(crate) use window_selection::TextSelectionController;

pub(super) const CONTEXT: &str = "MarkdownView";

/// Register Markdown copy/select-all bindings and selection globals.
pub fn init(cx: &mut App) {
    window_selection::init_global(cx);
    cx.bind_keys(vec![
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
    ]);
}
