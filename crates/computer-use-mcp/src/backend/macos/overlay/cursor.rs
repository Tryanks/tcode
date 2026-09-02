use std::ptr;

use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use super::OverlayActionKind;
use super::ffi::{
    Id, class, send_id, send_id_color, send_id_cstr, send_id_id, send_id_rect, send_id_window_init,
    send_void, send_void_bool, send_void_f32, send_void_f64, send_void_id, send_void_isize,
    send_void_point, send_void_rect, send_void_size, send_void_usize, status_window_level,
};
use super::geometry::{DisplayGeometry, ax_screen_to_appkit};

const CURSOR_SIZE: f64 = 40.0;
const HOTSPOT_X: f64 = 4.0;
const HOTSPOT_Y: f64 = 35.0;
const ANIMATION_DURATION: f64 = 0.25;

const NS_WINDOW_STYLE_NONACTIVATING_PANEL: usize = 1 << 7;
const NS_BACKING_STORE_BUFFERED: usize = 2;
const NS_WINDOW_COLLECTION_BEHAVIOR: usize = (1 << 0) | (1 << 3) | (1 << 9);

pub(super) struct CursorUi {
    window: Id,
    shape: Id,
    visible: bool,
}

impl CursorUi {
    /// Must only be called from the process main queue.
    pub(super) fn new() -> Option<Self> {
        let panel_class = class(c"NSPanel")?;
        let panel = send_id(panel_class, c"alloc")?;
        let window = send_id_window_init(
            panel,
            c"initWithContentRect:styleMask:backing:defer:",
            rect(-CURSOR_SIZE, -CURSOR_SIZE, CURSOR_SIZE, CURSOR_SIZE),
            NS_WINDOW_STYLE_NONACTIVATING_PANEL,
            NS_BACKING_STORE_BUFFERED,
            false,
        )?;

        let clear = send_id(class(c"NSColor")?, c"clearColor")?;
        let configured = send_void_bool(window, c"setReleasedWhenClosed:", false)
            && send_void_bool(window, c"setIgnoresMouseEvents:", true)
            && send_void_bool(window, c"setOpaque:", false)
            && send_void_bool(window, c"setHasShadow:", false)
            && send_void_bool(window, c"setHidesOnDeactivate:", false)
            && send_void_id(window, c"setBackgroundColor:", clear)
            && send_void_usize(
                window,
                c"setCollectionBehavior:",
                NS_WINDOW_COLLECTION_BEHAVIOR,
            )
            && send_void_isize(window, c"setLevel:", status_window_level());
        if !configured {
            return None;
        }

        let view = send_id_rect(
            send_id(class(c"NSView")?, c"alloc")?,
            c"initWithFrame:",
            rect(0.0, 0.0, CURSOR_SIZE, CURSOR_SIZE),
        )?;
        if !send_void_bool(view, c"setWantsLayer:", true) {
            return None;
        }
        let root_layer = send_id(view, c"layer")?;
        let shape = send_id(class(c"CAShapeLayer")?, c"layer")?;
        let path = cursor_path()?;
        let fill = cg_color(0.08, 0.09, 0.12, 0.98)?;
        let stroke = cg_color(0.93, 0.35, 0.72, 1.0)?;
        let shadow = cg_color(0.0, 0.0, 0.0, 0.8)?;

        let shape_configured = send_void_id(shape, c"setPath:", path)
            && send_void_rect(
                shape,
                c"setFrame:",
                rect(0.0, 0.0, CURSOR_SIZE, CURSOR_SIZE),
            )
            && send_void_id(shape, c"setFillColor:", fill)
            && send_void_id(shape, c"setStrokeColor:", stroke)
            && send_void_f64(shape, c"setLineWidth:", 2.0)
            && send_void_id(shape, c"setShadowColor:", shadow)
            && send_void_f32(shape, c"setShadowOpacity:", 0.55)
            && send_void_f64(shape, c"setShadowRadius:", 2.5)
            && send_void_size(shape, c"setShadowOffset:", CGSize::new(0.0, -1.0))
            && send_void_id(root_layer, c"addSublayer:", shape)
            && send_void_id(window, c"setContentView:", view);
        if !shape_configured {
            return None;
        }

        Some(Self {
            window,
            shape,
            visible: false,
        })
    }

    /// Must only be called from the process main queue.
    pub(super) fn show(
        &mut self,
        kind: OverlayActionKind,
        ax_point: (f64, f64),
        display: DisplayGeometry,
        visible: bool,
    ) {
        self.set_kind(kind);
        let appkit_point = ax_screen_to_appkit(ax_point, display);
        if self.visible {
            animate_window_origin(self.window, window_origin(appkit_point));
        } else {
            // A hidden panel has no on-screen position to slide from: land on
            // the point, then reveal.
            let _ = send_void_point(self.window, c"setFrameOrigin:", window_origin(appkit_point));
        }
        self.set_visible(visible);
    }

    /// Must only be called from the process main queue.
    pub(super) fn show_drag(
        &mut self,
        from_ax: (f64, f64),
        to_ax: (f64, f64),
        from_display: DisplayGeometry,
        to_display: DisplayGeometry,
        visible: bool,
    ) {
        self.set_kind(OverlayActionKind::Drag);
        let from = ax_screen_to_appkit(from_ax, from_display);
        let to = ax_screen_to_appkit(to_ax, to_display);
        let _ = send_void_point(self.window, c"setFrameOrigin:", window_origin(from));
        self.set_visible(visible);
        animate_window_origin(self.window, window_origin(to));
    }

    /// Must only be called from the process main queue.
    pub(super) fn set_visible(&mut self, visible: bool) {
        if visible == self.visible {
            return;
        }
        if visible {
            let _ = send_void(self.window, c"orderFrontRegardless");
            let _ = send_void(self.window, c"displayIfNeeded");
        } else {
            let _ = send_void_id(self.window, c"orderOut:", ptr::null_mut());
        }
        self.visible = visible;
    }

    /// Must only be called from the process main queue.
    pub(super) fn hide(&mut self) {
        self.set_visible(false);
    }

    fn set_kind(&self, kind: OverlayActionKind) {
        let (red, green, blue) = match kind {
            OverlayActionKind::Click => (0.93, 0.35, 0.72),
            OverlayActionKind::Scroll => (0.30, 0.78, 0.96),
            OverlayActionKind::Drag => (0.66, 0.43, 0.96),
            OverlayActionKind::Keyboard => (0.98, 0.66, 0.28),
            OverlayActionKind::Move => (0.32, 0.66, 1.0),
        };
        if let Some(color) = cg_color(red, green, blue, 1.0) {
            let _ = send_void_id(self.shape, c"setStrokeColor:", color);
        }
    }
}

fn cursor_path() -> Option<Id> {
    let path = send_id(class(c"NSBezierPath")?, c"bezierPath")?;
    for (index, point) in [
        (4.0, 35.0),
        (4.0, 7.0),
        (11.5, 14.0),
        (17.5, 3.5),
        (22.0, 6.0),
        (16.0, 16.5),
        (26.0, 16.5),
    ]
    .into_iter()
    .enumerate()
    {
        let selector = if index == 0 {
            c"moveToPoint:"
        } else {
            c"lineToPoint:"
        };
        if !send_void_point(path, selector, CGPoint::new(point.0, point.1)) {
            return None;
        }
    }
    if !send_void(path, c"closePath") {
        return None;
    }
    send_id(path, c"CGPath")
}

fn animate_window_origin(window: Id, origin: CGPoint) {
    let Some(context_class) = class(c"NSAnimationContext") else {
        let _ = send_void_point(window, c"setFrameOrigin:", origin);
        return;
    };
    if !send_void(context_class, c"beginGrouping") {
        let _ = send_void_point(window, c"setFrameOrigin:", origin);
        return;
    }

    let animated = send_id(context_class, c"currentContext").is_some_and(|context| {
        let duration_set = send_void_f64(context, c"setDuration:", ANIMATION_DURATION);
        if let Some(timing) = timing_function() {
            let _ = send_void_id(context, c"setTimingFunction:", timing);
        }
        let moved = send_id(window, c"animator")
            .is_some_and(|animator| send_void_point(animator, c"setFrameOrigin:", origin));
        duration_set && moved
    });
    let _ = send_void(context_class, c"endGrouping");
    if !animated {
        let _ = send_void_point(window, c"setFrameOrigin:", origin);
    }
}

fn timing_function() -> Option<Id> {
    let name = send_id_cstr(
        class(c"NSString")?,
        c"stringWithUTF8String:",
        c"easeOut".as_ptr(),
    )?;
    send_id_id(class(c"CAMediaTimingFunction")?, c"functionWithName:", name)
}

fn cg_color(red: f64, green: f64, blue: f64, alpha: f64) -> Option<Id> {
    let color = send_id_color(
        class(c"NSColor")?,
        c"colorWithSRGBRed:green:blue:alpha:",
        red,
        green,
        blue,
        alpha,
    )?;
    send_id(color, c"CGColor")
}

fn window_origin(point: (f64, f64)) -> CGPoint {
    CGPoint::new(point.0 - HOTSPOT_X, point.1 - HOTSPOT_Y)
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
    CGRect::new(&CGPoint::new(x, y), &CGSize::new(width, height))
}
