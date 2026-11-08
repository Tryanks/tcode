//! Shows a Wayland client's toplevels inside subsurfaces of another Wayland
//! client, so a toolkit that only draws through its own Wayland connection
//! (GTK, and the WebKitGTK view inside it) can appear in a GPUI window.
//!
//! [`Nest`] is a minimal compositor on its own thread. The embedded client
//! connects to it by socket name; every toplevel it maps is matched to a
//! [`ViewId`] by its title ([`ViewId::title`]), and each composited frame of a
//! view (the toplevel and its popups, cropped to the window geometry) comes
//! back as an [`Event::Frame`]. [`Presenter`] puts those frames on a
//! `wl_surface` of the host's own connection. Input goes the other way through
//! [`Nest::pointer`] and [`Nest::key`], in the view's window-geometry
//! coordinates.
#![cfg(target_os = "linux")]

mod compose;
mod keys;
mod present;
mod server;

pub use keys::{evdev_key, modifier};
pub use present::{Crop, Presenter, Target};
pub use server::Nest;

/// The `BTN_*` codes of `linux/input-event-codes.h` that [`PointerEvent::Button`] takes.
pub mod button {
    pub const LEFT: u32 = 0x110;
    pub const RIGHT: u32 = 0x111;
    pub const MIDDLE: u32 = 0x112;
    /// `BTN_SIDE`, which GTK reports as button 8 (back).
    pub const BACK: u32 = 0x113;
    /// `BTN_EXTRA`, which GTK reports as button 9 (forward).
    pub const FORWARD: u32 = 0x114;
}

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

/// A composited view: premultiplied ARGB8888 in native byte order (the
/// `wl_shm` `argb8888` layout), `width * 4` bytes per row.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub enum Event {
    /// A new frame of `view`. The view draws its next frame only after
    /// [`Nest::presented`] answers this one.
    Frame { view: ViewId, frame: Frame },
    /// The view's toplevel was destroyed.
    Closed { view: ViewId },
    /// The embedded client copied text to the clipboard.
    Clipboard(String),
}

#[derive(Clone, Copy, Debug)]
pub enum ScrollSource {
    /// Discrete steps, with `discrete` counting them in 120ths.
    Wheel,
    /// Continuous scrolling (touchpads); `stop` ends a gesture for kinetic
    /// scrolling.
    Finger,
}

/// Pointer input in the view's window-geometry coordinates (physical pixels:
/// the nest reports scale 1, so its logical pixels are the host's device
/// pixels).
#[derive(Clone, Copy, Debug)]
pub enum PointerEvent {
    Motion {
        x: f64,
        y: f64,
    },
    Leave,
    Button {
        button: u32,
        pressed: bool,
    },
    /// Positive values scroll down and right, as in `wl_pointer.axis`.
    Scroll {
        dx: f64,
        dy: f64,
        source: ScrollSource,
        discrete: Option<(i32, i32)>,
        stop: bool,
    },
}
