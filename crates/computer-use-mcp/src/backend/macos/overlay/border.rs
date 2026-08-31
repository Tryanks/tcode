use std::ptr;

use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use super::ffi::{
    Id, class, send_id, send_id_color, send_id_cstr, send_id_f32, send_id_id, send_id_objects,
    send_id_rect, send_id_rounded_rect, send_id_window_init, send_void, send_void_bool,
    send_void_f32, send_void_f64, send_void_id, send_void_isize, send_void_point, send_void_rect,
    send_void_rect_bool, send_void_size, send_void_two_ids, send_void_usize, status_window_level,
};
use super::geometry::{BORDER_PADDING, DisplayGeometry, border_frame};
use crate::outline::Frame;

const NS_WINDOW_STYLE_BORDERLESS: usize = 0;
const NS_BACKING_STORE_BUFFERED: usize = 2;
const NS_WINDOW_COLLECTION_BEHAVIOR: usize = (1 << 0) | (1 << 3) | (1 << 9);
const FADE_DURATION: f64 = 0.3;
const CORNER_RADIUS: f64 = 12.0;

pub(super) struct BorderUi {
    window: Id,
    container: Id,
    glow: Id,
    gradient: Id,
    mask: Id,
    visible: bool,
}

impl BorderUi {
    /// Must only be called from the process main queue.
    pub(super) fn new() -> Option<Self> {
        let window_class = class(c"NSWindow")?;
        let allocated = send_id(window_class, c"alloc")?;
        let window = send_id_window_init(
            allocated,
            c"initWithContentRect:styleMask:backing:defer:",
            rect(0.0, 0.0, 1.0, 1.0),
            NS_WINDOW_STYLE_BORDERLESS,
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
            rect(0.0, 0.0, 1.0, 1.0),
        )?;
        if !send_void_bool(view, c"setWantsLayer:", true) {
            return None;
        }
        let root = send_id(view, c"layer")?;
        let container = send_id(class(c"CALayer")?, c"layer")?;
        let glow = send_id(class(c"CAShapeLayer")?, c"layer")?;
        let gradient = send_id(class(c"CAGradientLayer")?, c"layer")?;
        let mask = send_id(class(c"CAShapeLayer")?, c"layer")?;

        let glow_color = cg_color(0.91, 0.25, 0.72, 0.34)?;
        let shadow_color = cg_color(0.30, 0.68, 1.0, 0.9)?;
        let mask_color = cg_color(1.0, 1.0, 1.0, 1.0)?;
        let colors = gradient_colors()?;

        let layers_configured = send_void_f32(container, c"setOpacity:", 0.0)
            && send_void_id(glow, c"setFillColor:", ptr::null_mut())
            && send_void_id(glow, c"setStrokeColor:", glow_color)
            && send_void_f64(glow, c"setLineWidth:", 13.0)
            && send_void_id(glow, c"setShadowColor:", shadow_color)
            && send_void_f32(glow, c"setShadowOpacity:", 0.75)
            && send_void_f64(glow, c"setShadowRadius:", 22.0)
            && send_void_size(glow, c"setShadowOffset:", CGSize::new(0.0, 0.0))
            && send_void_id(gradient, c"setColors:", colors)
            && send_void_point(gradient, c"setStartPoint:", CGPoint::new(0.0, 0.25))
            && send_void_point(gradient, c"setEndPoint:", CGPoint::new(1.0, 0.75))
            && send_void_id(mask, c"setFillColor:", ptr::null_mut())
            && send_void_id(mask, c"setStrokeColor:", mask_color)
            && send_void_f64(mask, c"setLineWidth:", 6.0)
            && send_void_id(gradient, c"setMask:", mask)
            && send_void_id(container, c"addSublayer:", glow)
            && send_void_id(container, c"addSublayer:", gradient)
            && send_void_id(root, c"addSublayer:", container)
            && send_void_id(window, c"setContentView:", view);
        if !layers_configured {
            return None;
        }

        Some(Self {
            window,
            container,
            glow,
            gradient,
            mask,
            visible: false,
        })
    }

    /// Must only be called from the process main queue.
    pub(super) fn show(&mut self, window_frame: Frame, display: DisplayGeometry) {
        let outer = border_frame(window_frame, display);
        let outer_rect = frame_rect(outer);
        let bounds = rect(0.0, 0.0, outer.w, outer.h);
        let inner = rect(
            BORDER_PADDING,
            BORDER_PADDING,
            window_frame.w,
            window_frame.h,
        );
        let Some(path) = send_id_rounded_rect(
            class(c"NSBezierPath").unwrap_or(ptr::null_mut()),
            c"bezierPathWithRoundedRect:xRadius:yRadius:",
            inner,
            CORNER_RADIUS,
            CORNER_RADIUS,
        )
        .and_then(|path| send_id(path, c"CGPath")) else {
            return;
        };

        let transaction = begin_without_implicit_animations();
        let updated = send_void_rect_bool(self.window, c"setFrame:display:", outer_rect, true)
            && send_void_rect(self.container, c"setFrame:", bounds)
            && send_void_rect(self.glow, c"setFrame:", bounds)
            && send_void_rect(self.gradient, c"setFrame:", bounds)
            && send_void_rect(self.mask, c"setFrame:", bounds)
            && send_void_id(self.glow, c"setPath:", path)
            && send_void_id(self.mask, c"setPath:", path);
        end_transaction(transaction);
        if !updated {
            return;
        }

        let _ = send_void(self.window, c"orderFrontRegardless");
        let from = if self.visible { 1.0 } else { 0.0 };
        animate_opacity(self.container, from, 1.0);
        self.visible = true;
    }

    /// Must only be called from the process main queue.
    pub(super) fn hide(&mut self) {
        if self.visible {
            animate_opacity(self.container, 1.0, 0.0);
            self.visible = false;
        }
    }
}

fn gradient_colors() -> Option<Id> {
    let colors = [
        cg_color(0.98, 0.66, 0.26, 0.95)?,
        cg_color(0.94, 0.29, 0.48, 0.96)?,
        cg_color(0.75, 0.42, 0.96, 0.94)?,
        cg_color(0.31, 0.72, 1.0, 0.95)?,
    ];
    send_id_objects(
        class(c"NSArray")?,
        c"arrayWithObjects:count:",
        colors.as_ptr(),
        colors.len(),
    )
}

fn animate_opacity(layer: Id, from: f32, to: f32) {
    let Some(key) = ns_string(c"tcode.agent-overlay.opacity") else {
        let _ = send_void_f32(layer, c"setOpacity:", to);
        return;
    };
    let Some(key_path) = ns_string(c"opacity") else {
        let _ = send_void_f32(layer, c"setOpacity:", to);
        return;
    };
    let Some(animation) = send_id_id(
        class(c"CABasicAnimation").unwrap_or(ptr::null_mut()),
        c"animationWithKeyPath:",
        key_path,
    ) else {
        let _ = send_void_f32(layer, c"setOpacity:", to);
        return;
    };
    let Some(from_value) = number(from) else {
        let _ = send_void_f32(layer, c"setOpacity:", to);
        return;
    };
    let Some(to_value) = number(to) else {
        let _ = send_void_f32(layer, c"setOpacity:", to);
        return;
    };

    let configured = send_void_id(animation, c"setFromValue:", from_value)
        && send_void_id(animation, c"setToValue:", to_value)
        && send_void_f64(animation, c"setDuration:", FADE_DURATION);
    if let Some(timing) = timing_function() {
        let _ = send_void_id(animation, c"setTimingFunction:", timing);
    }

    let transaction = begin_without_implicit_animations();
    let model_updated = send_void_f32(layer, c"setOpacity:", to);
    end_transaction(transaction);
    if configured && model_updated {
        let _ = send_void_two_ids(layer, c"addAnimation:forKey:", animation, key);
    }
}

fn begin_without_implicit_animations() -> Option<Id> {
    let transaction = class(c"CATransaction")?;
    if !send_void(transaction, c"begin") {
        return None;
    }
    let _ = send_void_bool(transaction, c"setDisableActions:", true);
    Some(transaction)
}

fn end_transaction(transaction: Option<Id>) {
    if let Some(transaction) = transaction {
        let _ = send_void(transaction, c"commit");
    }
}

fn timing_function() -> Option<Id> {
    let name = ns_string(c"easeInEaseOut")?;
    send_id_id(class(c"CAMediaTimingFunction")?, c"functionWithName:", name)
}

fn number(value: f32) -> Option<Id> {
    send_id_f32(class(c"NSNumber")?, c"numberWithFloat:", value)
}

fn ns_string(value: &std::ffi::CStr) -> Option<Id> {
    send_id_cstr(
        class(c"NSString")?,
        c"stringWithUTF8String:",
        value.as_ptr(),
    )
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

fn frame_rect(frame: Frame) -> CGRect {
    rect(frame.x, frame.y, frame.w, frame.h)
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
    CGRect::new(&CGPoint::new(x, y), &CGSize::new(width, height))
}
