//! Rushdown-backed Markdown rendering and window-level selection.
//!
//! The GPUI element architecture is adapted from gpui-component's Apache-2.0
//! `crates/ui/src/text` implementation. Parsing and highlighting use tcode's
//! rushdown IR and syntect bridge.

mod inline;
mod inline_flow;
mod link_target;
pub(crate) mod nodes;
pub(crate) mod parse;
mod render;
mod selection_adapter;
mod state;
mod utils;
mod view;

use gpui::{App, KeyBinding};
use gpui_base::input::{Copy, SelectAll};

pub(crate) use parse::parse;
pub use state::MarkdownState;
pub use view::MarkdownView;

pub(super) const CONTEXT: &str = "MarkdownView";

/// Register Markdown copy/select-all bindings.
pub fn init(cx: &mut App) {
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
