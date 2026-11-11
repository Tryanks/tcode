//! Rushdown-backed Markdown rendering and document text selection.
//!
//! The GPUI element architecture is adapted from gpui-component's Apache-2.0
//! `crates/ui/src/text` implementation. Parsing and highlighting use tcode's
//! rushdown IR and syntect bridge.
//!
//! gpui-base's `TextView` (0.9) is not used because of what a timeline
//! message needs and it cannot give:
//! - Selection that spans many messages in a virtual list and copies the
//!   ones scrolled out of it. gpui-base's window selection forgets a
//!   participant that stops painting, and a selection loses its end with it
//!   (`text_selection.rs`, `stale_participants`); [`SelectionDocument`] keeps
//!   its ends by entry and reads off-screen entries from their nodes.
//! - A context menu on the text ([`MenuExtension`]); `TextView` has none.
//! - Blocks that keep state while a message streams: a Mermaid fence keeps
//!   its last diagram and cross-fades to the next (`mermaid.rs`), a picture
//!   holds a stand-in while it loads and glides to its size
//!   (`fitted_image.rs`). `TextView`'s block renderers are stateless
//!   functions, and its images draw nothing of their own until they decode.
//! - A stream fade that knows where the tail is (`stream_fade.rs`).
//! - Rendered math: `TextView` parses it but draws it as code.

mod fitted_image;
mod image_link;
mod inline;
mod inline_flow;
mod link_target;
mod math;
mod math_layout;
mod math_parse;
mod mermaid;
pub(crate) mod nodes;
pub(crate) mod parse;
mod render;
mod selection;
mod state;
mod stream_fade;
mod utils;
mod view;

use gpui::{App, KeyBinding};
use gpui_base::input::SelectAll;

pub use link_target::OpenThreadAnchor;
pub use selection::{SelectionArea, SelectionDocument, SelectionGesture};
pub use state::{ImageResolver, MarkdownState};
pub use view::{MarkdownView, MenuExtension};

pub(super) const CONTEXT: &str = "MarkdownView";

/// Register Markdown select-all bindings. Copying is the selection area's.
pub fn init(cx: &mut App) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(
            latex_rust::STIX_TWO_MATH_OTF,
        )])
        .expect("embedded math font must load");
    cx.bind_keys(vec![
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
    ]);
}
