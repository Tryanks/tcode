//! Narrow C ABI shared with the UIKit host.
//!
//! UIKit objects remain owned by Swift. Rust stores only a non-owning pointer
//! to the attached `UIView` and consumes value-type snapshots from callbacks.

use super::IosWindow;
use anyhow::{Result, anyhow};
use gpui::{
    AppLifecyclePhase, Autocapitalize, Edges, Modifiers, PlatformInput, TextInputAction,
    TextInputConfiguration, TouchEvent, TouchId, TouchPhase, WindowAppearance, WindowInsets, point,
    px,
};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr::NonNull,
    slice,
};

/// One UIKit touch serialized for the Rust input pipeline.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct GpuiIosTouch {
    pub identifier: u64,
    pub x: f64,
    pub y: f64,
    pub predicted_x: f64,
    pub predicted_y: f64,
    pub force: f32,
    pub has_prediction: u8,
    pub has_force: u8,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct HostMetrics {
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) scale: f32,
    pub(crate) safe_top: f32,
    pub(crate) safe_right: f32,
    pub(crate) safe_bottom: f32,
    pub(crate) safe_left: f32,
    pub(crate) keyboard_height: f32,
    pub(crate) appearance: WindowAppearance,
    pub(crate) active: bool,
}

impl Default for HostMetrics {
    fn default() -> Self {
        Self {
            width: 0.0,
            height: 0.0,
            scale: 1.0,
            safe_top: 0.0,
            safe_right: 0.0,
            safe_bottom: 0.0,
            safe_left: 0.0,
            keyboard_height: 0.0,
            appearance: WindowAppearance::Light,
            active: true,
        }
    }
}

impl HostMetrics {
    pub(crate) fn insets(self) -> WindowInsets {
        WindowInsets {
            safe_area: Edges {
                top: px(self.safe_top),
                right: px(self.safe_right),
                bottom: px(self.safe_bottom),
                left: px(self.safe_left),
            },
            ime: Edges {
                top: px(0.0),
                right: px(0.0),
                bottom: px(self.keyboard_height),
                left: px(0.0),
            },
        }
    }
}

#[derive(Default)]
struct HostState {
    view: Option<NonNull<c_void>>,
    window: Option<NonNull<IosWindow>>,
    metrics: HostMetrics,
}

thread_local! {
    static HOST: RefCell<HostState> = RefCell::new(HostState::default());
    static INITIALIZED: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "C" {
    fn gpui_ios_host_log(level: u32, bytes: *const u8, length: usize);
    fn gpui_ios_host_schedule_frame();
    fn gpui_ios_host_show_keyboard();
    fn gpui_ios_host_hide_keyboard();
    fn gpui_ios_host_configure_text_input(
        autocorrect: u8,
        autocapitalize: u32,
        suggestions: u8,
        input_action: u32,
    );
    fn gpui_ios_host_open_url(bytes: *const u8, length: usize);
    fn gpui_ios_host_clipboard_text_length() -> usize;
    fn gpui_ios_host_read_clipboard(bytes: *mut u8, capacity: usize) -> usize;
    fn gpui_ios_host_write_clipboard(bytes: *const u8, length: usize);
}

struct IosLogger;

static IOS_LOGGER: IosLogger = IosLogger;

impl log::Log for IosLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = format!(
            "[{}][{}] {}",
            record.level(),
            record.target(),
            record.args()
        );
        // SAFETY: the Swift executable copies this UTF-8 buffer synchronously.
        unsafe { gpui_ios_host_log(record.level() as u32, message.as_ptr(), message.len()) }
    }

    fn flush(&self) {}
}

fn install_logger() {
    if log::set_logger(&IOS_LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

pub(crate) fn host_metrics() -> HostMetrics {
    HOST.with(|host| host.borrow().metrics)
}

pub(crate) fn host_view() -> Result<NonNull<c_void>> {
    HOST.with(|host| {
        host.borrow()
            .view
            .ok_or_else(|| anyhow!("gpui_ios_attach_view must run before opening a GPUI window"))
    })
}

pub(crate) fn register_window(window: &IosWindow) {
    HOST.with(|host| {
        host.borrow_mut().window = Some(NonNull::from(window));
    });
}

pub(crate) fn unregister_window(window: &IosWindow) {
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        if host.window == Some(NonNull::from(window)) {
            host.window = None;
        }
    });
}

fn with_window(f: impl FnOnce(&IosWindow)) {
    let window = HOST.with(|host| host.borrow().window);
    if let Some(window) = window {
        // SAFETY: `register_window` stores the address of the boxed platform
        // window, and `Drop` unregisters it before that allocation is freed.
        f(unsafe { window.as_ref() });
    }
}

pub(crate) fn host_schedule_frame() {
    // SAFETY: the Swift executable provides this symbol and accepts no state.
    unsafe { gpui_ios_host_schedule_frame() }
}

pub(crate) fn host_show_keyboard() {
    // SAFETY: UIKit work is performed by Swift on this same main-thread call.
    unsafe { gpui_ios_host_show_keyboard() }
}

pub(crate) fn host_hide_keyboard() {
    // SAFETY: UIKit work is performed by Swift on this same main-thread call.
    unsafe { gpui_ios_host_hide_keyboard() }
}

pub(crate) fn host_configure_text_input(configuration: &TextInputConfiguration) {
    let autocapitalize = match configuration.autocapitalize {
        Autocapitalize::None => 0,
        Autocapitalize::Words => 1,
        Autocapitalize::Sentences => 2,
        Autocapitalize::Characters => 3,
    };
    let action = match configuration.input_action {
        TextInputAction::Unspecified => 0,
        TextInputAction::Enter => 1,
        TextInputAction::Done => 2,
        TextInputAction::Go => 3,
        TextInputAction::Next => 4,
        TextInputAction::Previous => 5,
        TextInputAction::Search => 6,
        TextInputAction::Send => 7,
    };
    // SAFETY: all arguments are plain C values documented by the bridging header.
    unsafe {
        gpui_ios_host_configure_text_input(
            u8::from(configuration.autocorrect),
            autocapitalize,
            u8::from(configuration.suggestions),
            action,
        );
    }
}

pub(crate) fn host_open_url(url: &str) {
    // SAFETY: the buffer remains alive for the duration of the Swift call.
    unsafe { gpui_ios_host_open_url(url.as_ptr(), url.len()) }
}

pub(crate) fn host_read_clipboard() -> Option<String> {
    // SAFETY: the first host call returns the byte capacity needed by the second.
    let length = unsafe { gpui_ios_host_clipboard_text_length() };
    if length == 0 {
        return None;
    }
    let mut bytes = vec![0_u8; length];
    // SAFETY: `bytes` is writable for exactly `length` bytes.
    let written = unsafe { gpui_ios_host_read_clipboard(bytes.as_mut_ptr(), bytes.len()) };
    bytes.truncate(written.min(bytes.len()));
    String::from_utf8(bytes).ok()
}

pub(crate) fn host_write_clipboard(text: &str) {
    // SAFETY: the buffer remains alive for the duration of the Swift call.
    unsafe { gpui_ios_host_write_clipboard(text.as_ptr(), text.len()) }
}

/// Current UIKit safe-area and keyboard insets in logical points.
pub fn insets() -> WindowInsets {
    host_metrics().insets()
}

/// Current UIKit safe-area insets in logical points.
pub fn safe_area() -> Edges<gpui::Pixels> {
    insets().safe_area
}

/// Shows or hides the UIKit software-keyboard proxy.
pub fn set_keyboard_visible(visible: bool) {
    if visible {
        host_show_keyboard();
    } else {
        host_hide_keyboard();
    }
}

fn utf8(bytes: *const u8, length: usize) -> Option<String> {
    if length == 0 {
        return Some(String::new());
    }
    if bytes.is_null() {
        return None;
    }
    // SAFETY: the ABI requires `bytes` to address `length` readable bytes for
    // the duration of this call. The returned value never escapes the call.
    let bytes = unsafe { slice::from_raw_parts(bytes, length) };
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

fn dispatch_touches(touches: *const GpuiIosTouch, count: usize, phase: TouchPhase) {
    if count == 0 || touches.is_null() {
        return;
    }
    // SAFETY: Swift passes a contiguous temporary array valid until this
    // synchronous function returns.
    let touches = unsafe { slice::from_raw_parts(touches, count) };
    with_window(|window| {
        for touch in touches {
            let position = point(px(touch.x as f32), px(touch.y as f32));
            window.set_mouse_position(position);
            let predicted_position = (touch.has_prediction != 0)
                .then(|| point(px(touch.predicted_x as f32), px(touch.predicted_y as f32)));
            let force = (touch.has_force != 0).then(|| touch.force.clamp(0.0, 1.0));
            window.dispatch_input(PlatformInput::Touch(TouchEvent {
                id: TouchId(touch.identifier),
                phase,
                position,
                predicted_position,
                force,
            }));
        }
    });
}

fn modifiers(bits: u32) -> Modifiers {
    Modifiers {
        shift: bits & 1 != 0,
        control: bits & 2 != 0,
        alt: bits & 4 != 0,
        platform: bits & 8 != 0,
        function: bits & 16 != 0,
    }
}

/// Initializes the process-wide platform on the UIKit main thread.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_init() {
    super::assert_main_thread();
    install_logger();
    INITIALIZED.with(|initialized| {
        if !initialized.replace(true) {
            let _ = super::platform();
        }
    });
}

/// Attaches the process-wide platform to a `UIView` backed by `CAMetalLayer`.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_attach_view(
    view: *mut c_void,
    width: f32,
    height: f32,
    scale: f32,
    dark: u8,
) -> u8 {
    super::assert_main_thread();
    let Some(view) = NonNull::new(view) else {
        return 0;
    };
    let (previous_view, previous_metrics) = HOST.with(|host| {
        let mut host = host.borrow_mut();
        let previous = (host.view, host.metrics);
        host.view = Some(view);
        host.metrics.width = width.max(0.0);
        host.metrics.height = height.max(0.0);
        host.metrics.scale = scale.max(1.0);
        host.metrics.appearance = if dark != 0 {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        };
        previous
    });

    let mut replacement_error = None;
    with_window(|window| {
        if window.needs_surface_replacement(view) {
            replacement_error = window
                .replace_surface_from_host(view, width, height, scale)
                .err();
        } else {
            window.resize_from_host(width, height, scale);
        }
    });
    if let Some(error) = replacement_error {
        HOST.with(|host| {
            let mut host = host.borrow_mut();
            host.view = previous_view;
            host.metrics = previous_metrics;
        });
        log::error!("failed to replace the GPUI iOS Metal surface: {error:#}");
        return 0;
    }
    1
}

/// Removes the current non-owning UIView reference.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_detach_view(view: *mut c_void) {
    let view = NonNull::new(view);
    let detached = HOST.with(|host| {
        let mut host = host.borrow_mut();
        if host.view == view {
            host.view = None;
            true
        } else {
            false
        }
    });
    if detached && let Some(view) = view {
        with_window(|window| window.detach_surface_from_host(view));
    }
}

/// Drives one GPUI frame from `CADisplayLink`.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_request_frame() {
    with_window(|window| window.request_frame(false));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_touches_began(touches: *const GpuiIosTouch, count: usize) {
    dispatch_touches(touches, count, TouchPhase::Started);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_touches_moved(touches: *const GpuiIosTouch, count: usize) {
    dispatch_touches(touches, count, TouchPhase::Moved);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_touches_ended(touches: *const GpuiIosTouch, count: usize) {
    dispatch_touches(touches, count, TouchPhase::Ended);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_touches_cancelled(touches: *const GpuiIosTouch, count: usize) {
    dispatch_touches(touches, count, TouchPhase::Cancelled);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_resize(width: f32, height: f32, scale: f32) {
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        host.metrics.width = width.max(0.0);
        host.metrics.height = height.max(0.0);
        host.metrics.scale = scale.max(1.0);
    });
    with_window(|window| window.resize_from_host(width, height, scale));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_scale_factor_changed(scale: f32) {
    let metrics = HOST.with(|host| {
        let mut host = host.borrow_mut();
        host.metrics.scale = scale.max(1.0);
        host.metrics
    });
    with_window(|window| window.resize_from_host(metrics.width, metrics.height, metrics.scale));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_safe_area_changed(top: f32, right: f32, bottom: f32, left: f32) {
    let insets = HOST.with(|host| {
        let mut host = host.borrow_mut();
        host.metrics.safe_top = top.max(0.0);
        host.metrics.safe_right = right.max(0.0);
        host.metrics.safe_bottom = bottom.max(0.0);
        host.metrics.safe_left = left.max(0.0);
        host.metrics.insets()
    });
    with_window(|window| window.update_insets(insets));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_keyboard_frame_changed(height: f32) {
    let insets = HOST.with(|host| {
        let mut host = host.borrow_mut();
        host.metrics.keyboard_height = height.max(0.0);
        host.metrics.insets()
    });
    with_window(|window| window.update_insets(insets));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_appearance_changed(dark: u8) {
    let appearance = if dark != 0 {
        WindowAppearance::Dark
    } else {
        WindowAppearance::Light
    };
    HOST.with(|host| host.borrow_mut().metrics.appearance = appearance);
    with_window(|window| window.update_appearance(appearance));
}

fn lifecycle(phase: AppLifecyclePhase, active: Option<bool>) {
    if let Some(active) = active {
        HOST.with(|host| host.borrow_mut().metrics.active = active);
        with_window(|window| window.update_active(active));
    }
    super::with_platform(|platform| platform.notify_lifecycle(phase));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_lifecycle_active() {
    lifecycle(AppLifecyclePhase::Active, Some(true));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_lifecycle_inactive() {
    lifecycle(AppLifecyclePhase::Inactive, Some(false));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_lifecycle_background() {
    lifecycle(AppLifecyclePhase::Background, Some(false));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_lifecycle_foreground() {
    lifecycle(AppLifecyclePhase::Foreground, None);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_memory_warning() {
    super::with_platform(super::IosPlatform::notify_memory_warning);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_insert_text(bytes: *const u8, length: usize) {
    if let Some(text) = utf8(bytes, length) {
        with_window(|window| window.insert_text(&text));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_set_marked_text(
    bytes: *const u8,
    length: usize,
    selection_start: usize,
    selection_length: usize,
) {
    if let Some(text) = utf8(bytes, length) {
        with_window(|window| {
            window.set_marked_text(&text, selection_start, selection_length);
        });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_unmark_text() {
    with_window(IosWindow::unmark_text);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_delete_backward() {
    with_window(IosWindow::delete_backward);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_key_event(
    key_bytes: *const u8,
    key_length: usize,
    character_bytes: *const u8,
    character_length: usize,
    modifier_bits: u32,
    down: u8,
    repeat: u8,
) {
    let Some(key) = utf8(key_bytes, key_length) else {
        return;
    };
    let character = (character_length > 0)
        .then(|| utf8(character_bytes, character_length))
        .flatten();
    with_window(|window| {
        window.key_event(
            key,
            character,
            modifiers(modifier_bits),
            down != 0,
            repeat != 0,
        );
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_open_url_received(bytes: *const u8, length: usize) {
    let Some(url) = utf8(bytes, length) else {
        return;
    };
    super::with_platform(|platform| platform.notify_open_urls(vec![url]));
}
