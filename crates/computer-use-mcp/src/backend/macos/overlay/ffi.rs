use std::ffi::{CStr, c_char, c_void};
use std::mem;

use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use super::geometry::DisplayGeometry;
use crate::outline::Frame;

pub(super) type Id = *mut c_void;
type Sel = *mut c_void;
// SAFETY: this callback ABI is the dispatch_function_t signature from libdispatch.
pub(super) type DispatchFn = unsafe extern "C" fn(*mut c_void);

// SAFETY: these declarations match the Objective-C runtime's public C ABI.
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
}

// SAFETY: AppKit is linked for the Objective-C window and view classes used below.
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

// SAFETY: QuartzCore is linked for the Objective-C layer classes used below.
#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {}

// SAFETY: these declarations match CoreGraphics' public display C ABI.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGGetDisplaysWithPoint(
        point: CGPoint,
        max_displays: u32,
        displays: *mut u32,
        display_count: *mut u32,
    ) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGWindowLevelForKey(key: i32) -> i32;
}

// SAFETY: libdispatch is a libSystem component and these declarations match its C ABI.
#[link(name = "System")]
unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: Id, context: *mut c_void, work: DispatchFn);
}

macro_rules! invoke {
    ($return_type:ty, $receiver:expr, $selector:expr $(, $argument_type:ty => $argument:expr)* $(,)?) => {{
        // SAFETY: each wrapper below fixes the function signature to the documented
        // Objective-C ABI of the selectors for which that wrapper is used.
        let function: unsafe extern "C" fn(Id, Sel $(, $argument_type)*) -> $return_type =
            unsafe { mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        // SAFETY: receiver and selector were checked for null, and each caller uses
        // the wrapper whose fixed signature matches the selector's documented ABI.
        unsafe { function($receiver, $selector $(, $argument)*) }
    }};
}

pub(super) fn class(name: &CStr) -> Option<Id> {
    // SAFETY: name is a live, nul-terminated C string for this call.
    let value = unsafe { objc_getClass(name.as_ptr()) };
    (!value.is_null()).then_some(value)
}

fn selector(name: &CStr) -> Option<Sel> {
    // SAFETY: name is a live, nul-terminated C string for this call.
    let value = unsafe { sel_registerName(name.as_ptr()) };
    (!value.is_null()).then_some(value)
}

fn can_send(receiver: Id, target: Sel) -> bool {
    if receiver.is_null() || target.is_null() {
        return false;
    }
    let Some(check) = selector(c"respondsToSelector:") else {
        return false;
    };
    invoke!(i8, receiver, check, Sel => target) != 0
}

pub(super) fn send_id(receiver: Id, name: &CStr) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let value = invoke!(Id, receiver, selector);
    (!value.is_null()).then_some(value)
}

pub(super) fn send_id_cstr(receiver: Id, name: &CStr, value: *const c_char) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) || value.is_null() {
        return None;
    }
    let result = invoke!(Id, receiver, selector, *const c_char => value);
    (!result.is_null()).then_some(result)
}

pub(super) fn send_id_id(receiver: Id, name: &CStr, value: Id) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let result = invoke!(Id, receiver, selector, Id => value);
    (!result.is_null()).then_some(result)
}

pub(super) fn send_id_rect(receiver: Id, name: &CStr, rect: CGRect) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let value = invoke!(Id, receiver, selector, CGRect => rect);
    (!value.is_null()).then_some(value)
}

pub(super) fn send_id_window_init(
    receiver: Id,
    name: &CStr,
    rect: CGRect,
    style: usize,
    backing: usize,
    defer: bool,
) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let value = invoke!(
        Id,
        receiver,
        selector,
        CGRect => rect,
        usize => style,
        usize => backing,
        i8 => i8::from(defer),
    );
    (!value.is_null()).then_some(value)
}

pub(super) fn send_id_rounded_rect(
    receiver: Id,
    name: &CStr,
    rect: CGRect,
    x_radius: f64,
    y_radius: f64,
) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let value = invoke!(
        Id,
        receiver,
        selector,
        CGRect => rect,
        f64 => x_radius,
        f64 => y_radius,
    );
    (!value.is_null()).then_some(value)
}

pub(super) fn send_id_color(
    receiver: Id,
    name: &CStr,
    red: f64,
    green: f64,
    blue: f64,
    alpha: f64,
) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let value = invoke!(
        Id,
        receiver,
        selector,
        f64 => red,
        f64 => green,
        f64 => blue,
        f64 => alpha,
    );
    (!value.is_null()).then_some(value)
}

pub(super) fn send_id_f32(receiver: Id, name: &CStr, value: f32) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let result = invoke!(Id, receiver, selector, f32 => value);
    (!result.is_null()).then_some(result)
}

pub(super) fn send_id_objects(
    receiver: Id,
    name: &CStr,
    objects: *const Id,
    count: usize,
) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) || (objects.is_null() && count != 0) {
        return None;
    }
    let value = invoke!(
        Id,
        receiver,
        selector,
        *const Id => objects,
        usize => count,
    );
    (!value.is_null()).then_some(value)
}

pub(super) fn send_void(receiver: Id, name: &CStr) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector);
    true
}

pub(super) fn send_void_id(receiver: Id, name: &CStr, value: Id) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, Id => value);
    true
}

pub(super) fn send_void_two_ids(receiver: Id, name: &CStr, first: Id, second: Id) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, Id => first, Id => second);
    true
}

pub(super) fn send_void_bool(receiver: Id, name: &CStr, value: bool) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, i8 => i8::from(value));
    true
}

pub(super) fn send_void_f64(receiver: Id, name: &CStr, value: f64) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, f64 => value);
    true
}

pub(super) fn send_void_f32(receiver: Id, name: &CStr, value: f32) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, f32 => value);
    true
}

pub(super) fn send_void_isize(receiver: Id, name: &CStr, value: isize) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, isize => value);
    true
}

pub(super) fn send_void_usize(receiver: Id, name: &CStr, value: usize) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, usize => value);
    true
}

pub(super) fn send_void_point(receiver: Id, name: &CStr, value: CGPoint) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, CGPoint => value);
    true
}

pub(super) fn send_void_size(receiver: Id, name: &CStr, value: CGSize) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, CGSize => value);
    true
}

pub(super) fn send_void_rect(receiver: Id, name: &CStr, value: CGRect) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, CGRect => value);
    true
}

pub(super) fn send_void_rect_bool(receiver: Id, name: &CStr, rect: CGRect, value: bool) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, CGRect => rect, i8 => i8::from(value));
    true
}

pub(super) fn dispatch_main(context: *mut c_void, work: DispatchFn) -> bool {
    let queue = dispatch_get_main_queue();
    if queue.is_null() {
        return false;
    }
    // SAFETY: the caller owns context until work runs; libdispatch invokes work
    // exactly once with that unchanged context on the main queue.
    unsafe { dispatch_async_f(queue, context, work) };
    true
}

// The C SDK defines dispatch_get_main_queue as this inline address operation.
fn dispatch_get_main_queue() -> Id {
    (&raw const _dispatch_main_q).cast_mut().cast::<c_void>()
}

pub(super) fn display_frame_for_ax_point(point: (f64, f64)) -> Option<DisplayGeometry> {
    if !point.0.is_finite() || !point.1.is_finite() {
        return None;
    }
    let mut display = 0_u32;
    let mut count = 0_u32;
    // SAFETY: display and count are valid writable outputs for one display ID.
    let error = unsafe {
        CGGetDisplaysWithPoint(CGPoint::new(point.0, point.1), 1, &mut display, &mut count)
    };
    // SAFETY: CGMainDisplayID takes no arguments and returns a display ID.
    let main_display = unsafe { CGMainDisplayID() };
    if error != 0 || count == 0 {
        display = main_display;
    }
    if display == 0 {
        return None;
    }
    // SAFETY: display came from CoreGraphics and is valid for this bounds query.
    let bounds = unsafe { CGDisplayBounds(display) };
    let ax = Frame {
        x: bounds.origin.x,
        y: bounds.origin.y,
        w: bounds.size.width,
        h: bounds.size.height,
    };
    if main_display == 0 {
        return None;
    }
    // SAFETY: main_display came from CoreGraphics and is valid for this bounds query.
    let main_bounds = unsafe { CGDisplayBounds(main_display) };
    let appkit = Frame {
        x: ax.x,
        y: main_bounds.origin.y + main_bounds.size.height - (ax.y + ax.h),
        w: ax.w,
        h: ax.h,
    };
    (ax.x.is_finite()
        && ax.y.is_finite()
        && ax.w.is_finite()
        && ax.h.is_finite()
        && appkit.x.is_finite()
        && appkit.y.is_finite()
        && appkit.w.is_finite()
        && appkit.h.is_finite()
        && ax.w > 0.0
        && ax.h > 0.0)
        .then_some(DisplayGeometry { ax, appkit })
}

pub(super) fn status_window_level() -> isize {
    const STATUS_WINDOW_LEVEL_KEY: i32 = 9;
    // SAFETY: STATUS_WINDOW_LEVEL_KEY is a documented CGWindowLevelKey value.
    unsafe { CGWindowLevelForKey(STATUS_WINDOW_LEVEL_KEY) as isize }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libdispatch_main_queue_symbol_is_available() {
        assert!(!dispatch_get_main_queue().is_null());
    }
}
