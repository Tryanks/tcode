//! Shows a Wayland client's toplevels inside surfaces of another Wayland
//! client, so a toolkit that only draws through its own Wayland connection
//! (GTK, and the WebKitGTK view inside it) can appear in a GPUI window.
//!
//! [`Nest`] is a minimal compositor on its own thread. The embedded client
//! connects to it by socket name, and every toplevel it maps is matched to a
//! [`ViewId`] by its title ([`ViewId::title`]). The same thread is a client of
//! the session's compositor, on the host's own connection: it draws each view
//! (the toplevel and its popups) on the host surface the view is attached to
//! ([`Nest::attach`], [`Nest::place`]), draws the next frame when the
//! session's compositor asks for one, and passes the session's pointer on to
//! the view under it. Keys go in through [`Nest::key`], and text an input
//! method composes through [`Nest::compose_text`] and [`Nest::commit_text`].
//! [`Nest::start_gtk`] makes GTK in this process that client, and [`run_gtk`]
//! runs it.
#![cfg(target_os = "linux")]

mod compose;
mod host;
mod keys;
mod server;
mod toolkit;

pub use keys::{evdev_key, modifier};
pub use server::{Nest, Paste};
pub use toolkit::run_gtk;

const TITLE_PREFIX: &str = "tcode-nest:";

/// One embedded toplevel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ViewId(pub u64);

impl ViewId {
    /// The toplevel title that binds a window to this view. The embedded
    /// client must set it before the window's first commit.
    pub fn title(self) -> String {
        format!("{TITLE_PREFIX}{}", self.0)
    }

    fn from_title(title: &str) -> Option<Self> {
        title.strip_prefix(TITLE_PREFIX)?.parse().ok().map(Self)
    }
}

/// Where a view is shown, in the host's device pixels. The nest reports
/// scale 1 to its client, so these are the view's own pixels too.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// The size the view is laid out at.
    pub width: u32,
    pub height: u32,
    /// The part of the view its host surface shows; the surface's buffer is
    /// this size.
    pub crop: Crop,
    /// Device pixels per unit of the host surface's own coordinates, which
    /// is how the session's compositor reports pointer positions.
    pub scale: f32,
}

/// A rectangle of a view. Parts outside the view are transparent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub enum Event {
    /// The embedded client copied text to the clipboard.
    Copied(String),
    /// The embedded client is pasting: it asks for the clipboard's text.
    Paste(Paste),
    /// Where the text field of `view` that takes text draws its caret, for
    /// placing an input method's candidates; `None` once no text field of
    /// the view takes text or its caret is unknown.
    TextInput { view: ViewId, caret: Option<Caret> },
}

/// A rectangle in a view's window-geometry coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caret {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
