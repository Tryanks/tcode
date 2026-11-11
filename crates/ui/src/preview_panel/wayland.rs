//! The preview browser on a Wayland session.
//!
//! WebKitGTK only draws through GDK's own Wayland connection, and a surface of
//! one connection cannot be a child of another's, so GTK runs against a nested
//! compositor ([`wayland_nest::Nest`]) instead of the session's. Each webview
//! lives in its own undecorated GTK window there; the nest composites that
//! window and its popups (`<select>` menus, context menus) and the frames are
//! shown on a GPUI Fast native composition surface of the preview's window.
//! Pointer input reaches the page through the surface's input consumer, keys
//! and input method text through the element returned by [`element`].
//!
//! GTK can be initialized once per process, on one display, so a process that
//! opened its first preview here keeps all of GTK on the nest.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gdk::glib;
use gdk::glib::translate::{IntoGlib as _, ToGlibPtr as _};
use gpui::{
    AnyElement, AnyWindowHandle, App, AsyncApp, Bounds, ClipboardItem, CursorStyle,
    DispatchEventResult, Entity, FocusHandle, Focusable as _, InputHandler,
    InteractiveElement as _, IntoElement as _, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers,
    ModifiersChangedEvent, MouseButton, NavigationDirection, ParentElement as _, Pixels,
    PlatformInput, ScrollDelta, Styled as _, TouchPhase, UTF16Selection, Window,
    WindowCompositionSurface, canvas, div, point, px, size,
};
use gpui_webview::WebView;
use gtk::prelude::*;
use raw_window_handle::{HasDisplayHandle as _, RawDisplayHandle, RawWindowHandle};
use wayland_nest::{
    Caret, Crop, Event, Frame, Nest, PointerEvent, Presenter, ScrollSource, Target, ViewId,
};
use webkit2gtk::{HitTestResultExt as _, WebViewExt as _};
use wry::{WebViewBuilderExtUnix as _, WebViewExtUnix as _};

/// How often GTK's pending events are dispatched while a preview exists, as
/// gpui-webview pumps GTK on X11.
const GTK_POLL_INTERVAL: Duration = Duration::from_millis(16);
/// How long GTK is still pumped after the last preview is released, so the
/// webview's teardown (its web process, its windows on the nest) completes.
const GTK_WIND_DOWN: Duration = Duration::from_secs(5);
/// The shortest time between two frames of one view. The nest's client draws
/// its next frame when the last one is answered, and answering at once would
/// let an animating page redraw as fast as it can. The host compositor's
/// frame callbacks would pace this better, but gpui-fast 0.1.13's
/// `PlatformSurfaceAttachment` exposes none for a native surface, and a
/// callback the presenter asked for on its own event queue would only be
/// seen when something next dispatched that queue.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// GPUI's Wayland client turns axis values into deltas with these factors;
/// they are undone to give GTK the values a compositor would have sent.
const GPUI_LINES_PER_STEP: f32 = 3.0;
const GPUI_PIXELS_PER_AXIS_UNIT: f32 = 3.0;
const AXIS_UNITS_PER_STEP: f64 = 10.0;

/// Whether this process can host the preview on Wayland.
pub(super) fn supported() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| !dir.is_empty())
}

/// Whether `window` is on Wayland, where previews are built by [`build`].
pub(super) fn active(window: &Window) -> bool {
    matches!(
        window.display_handle().map(|handle| handle.as_raw()),
        Ok(RawDisplayHandle::Wayland(_))
    )
}

struct Host {
    nest: Nest,
    presenter: Presenter,
    slots: RefCell<HashMap<ViewId, Rc<Slot>>>,
    next_view: Cell<u64>,
    /// Set once the app quits, from when the host is leaked (see [`start`]):
    /// a webview destroyed after that releases nothing.
    quitting: Cell<bool>,
    /// Whether GTK's events are being dispatched (see [`pump_gtk`]).
    pumping: Cell<bool>,
    /// GDK's cursors for the CSS names in [`CSS_CURSORS`].
    cursors: Vec<(gdk::Cursor, CursorStyle)>,
}

thread_local! {
    static HOST: OnceCell<Result<Rc<Host>, String>> = const { OnceCell::new() };
}

fn host(window: &Window, cx: &mut App) -> Result<Rc<Host>, String> {
    HOST.with(|host| host.get_or_init(|| start(window, cx)).clone())
}

fn current_host() -> Option<Rc<Host>> {
    HOST.with(|host| host.get().and_then(|host| host.as_ref().ok().cloned()))
}

fn start(window: &Window, cx: &mut App) -> Result<Rc<Host>, String> {
    let display = match window.display_handle().map_err(|e| e.to_string())?.as_raw() {
        RawDisplayHandle::Wayland(handle) => handle.display,
        _ => return Err("the window is not on Wayland".into()),
    };
    let nest = Nest::start().map_err(|error| format!("nested compositor: {error}"))?;
    nest.start_gtk()?;
    // SAFETY: GPUI's Wayland connection lives as long as the process.
    let presenter = unsafe { Presenter::new(display) }?;
    let host = Rc::new(Host {
        nest,
        presenter,
        slots: RefCell::new(HashMap::new()),
        next_view: Cell::new(0),
        quitting: Cell::new(false),
        pumping: Cell::new(false),
        cursors: css_cursors(),
    });

    let quitting = Rc::downgrade(&host);
    cx.on_app_quit(move |_| {
        if let Some(host) = quitting.upgrade() {
            host.quitting.set(true);
            // GPUI drops its Wayland client and disconnects the display
            // before the app's webviews and this thread's locals are
            // destroyed. The presenter and the slots' targets live on that
            // display, and a slot's native surface cannot be dropped after
            // the client: gpui-fast-linux 0.1.13's `WaylandNativeSurface::drop`
            // (fast/composition/wayland.rs:362-367) calls
            // `WaylandClientStatePtr::get_client`, which panics once the
            // client is gone (linux/wayland/client.rs:584-588). So the
            // process exits with the host and its slots leaked.
            std::mem::forget(host);
        }
        async {}
    })
    .detach();

    let events = host.nest.events();
    let weak = Rc::downgrade(&host);
    cx.spawn(async move |cx| {
        while let Ok(event) = events.recv().await {
            let Some(host) = weak.upgrade() else {
                break;
            };
            match event {
                Event::Frame { view, frame } => {
                    let slot = host.slots.borrow().get(&view).cloned();
                    match slot {
                        Some(slot) => slot.show_frame(&host, frame),
                        None => host.nest.presented(view),
                    }
                }
                Event::Closed { view } => {
                    log::debug!("preview: nested window of {view:?} closed");
                }
                Event::Clipboard(text) => {
                    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
                }
                Event::TextInput { view, caret } => {
                    if let Some(slot) = host.slots.borrow().get(&view) {
                        slot.caret.set(caret);
                    }
                }
            }
        }
    })
    .detach();
    Ok(host)
}

/// The CSS cursors WebKitGTK sets by name that GPUI can show.
///
/// GTK 3 sets a cursor on Wayland as a surface it draws, never by name, so
/// the nest cannot tell which one the page asked for. GDK 3's Wayland
/// backend makes one cursor per name and hands it out again
/// (`_gdk_wayland_display_get_cursor_for_name_with_scale`,
/// gdk/wayland/gdkcursor-wayland.c), so the cursor WebKitGTK sets on its
/// window is the very object made here from the same name. Any other cursor
/// (an image the page supplies, or a name GPUI has no equivalent for) falls
/// back to what WebKit's hit test reports.
const CSS_CURSORS: &[(&str, CursorStyle)] = &[
    ("default", CursorStyle::Arrow),
    ("pointer", CursorStyle::PointingHand),
    ("text", CursorStyle::IBeam),
    ("vertical-text", CursorStyle::IBeamCursorForVerticalLayout),
    ("crosshair", CursorStyle::Crosshair),
    ("grab", CursorStyle::OpenHand),
    ("grabbing", CursorStyle::ClosedHand),
    ("not-allowed", CursorStyle::OperationNotAllowed),
    ("no-drop", CursorStyle::OperationNotAllowed),
    ("alias", CursorStyle::DragLink),
    ("copy", CursorStyle::DragCopy),
    ("context-menu", CursorStyle::ContextualMenu),
    ("col-resize", CursorStyle::ResizeColumn),
    ("row-resize", CursorStyle::ResizeRow),
    ("ew-resize", CursorStyle::ResizeLeftRight),
    ("e-resize", CursorStyle::ResizeRight),
    ("w-resize", CursorStyle::ResizeLeft),
    ("ns-resize", CursorStyle::ResizeUpDown),
    ("n-resize", CursorStyle::ResizeUp),
    ("s-resize", CursorStyle::ResizeDown),
    ("nwse-resize", CursorStyle::ResizeUpLeftDownRight),
    ("nw-resize", CursorStyle::ResizeUpLeftDownRight),
    ("se-resize", CursorStyle::ResizeUpLeftDownRight),
    ("nesw-resize", CursorStyle::ResizeUpRightDownLeft),
    ("ne-resize", CursorStyle::ResizeUpRightDownLeft),
    ("sw-resize", CursorStyle::ResizeUpRightDownLeft),
];

/// GDK's cursor for each of [`CSS_CURSORS`] its cursor theme has.
fn css_cursors() -> Vec<(gdk::Cursor, CursorStyle)> {
    let Some(display) = gdk::Display::default() else {
        return Vec::new();
    };
    CSS_CURSORS
        .iter()
        .filter_map(|&(name, style)| Some((gdk::Cursor::from_name(&display, name)?, style)))
        .collect()
}

/// Dispatches GTK's events while a preview exists, and for
/// [`GTK_WIND_DOWN`] after the last is released.
fn pump_gtk(host: &Rc<Host>, cx: &App) {
    if host.pumping.replace(true) {
        return;
    }
    log::debug!("preview: pumping GTK");
    let weak = Rc::downgrade(host);
    cx.spawn(async move |cx| {
        let mut idle_since = None;
        loop {
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
            let Some(host) = weak.upgrade() else {
                return;
            };
            let now = cx.background_executor().now();
            if !host.slots.borrow().is_empty() {
                idle_since = None;
            } else if now.duration_since(*idle_since.get_or_insert(now)) >= GTK_WIND_DOWN {
                host.pumping.set(false);
                log::debug!("preview: no preview left; GTK is no longer pumped");
                return;
            }
            drop(host);
            cx.background_executor().timer(GTK_POLL_INTERVAL).await;
        }
    })
    .detach();
}

/// Builds a preview webview hosted through the nest.
pub(super) fn build(
    builder: wry::WebViewBuilder,
    window: &mut Window,
    cx: &mut App,
) -> wry::Result<WebView> {
    let error = |message: String| wry::Error::Io(std::io::Error::other(message));
    let host = host(window, cx).map_err(error)?;
    let id = ViewId(host.next_view.get());
    host.next_view.set(id.0 + 1);

    let gtk_window = gtk::Window::new(gtk::WindowType::Toplevel);
    gtk_window.set_title(&id.title());
    gtk_window.set_decorated(false);
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    gtk_window.add(&container);
    let raw = builder.build_gtk(&container)?;
    gtk_window.show_all();

    let (surface, target) = {
        let composition = window
            .enable_window_composition()
            .map_err(|e| error(e.to_string()))?;
        let surface = composition
            .create_native_surface()
            .map_err(|e| error(e.to_string()))?;
        let target = native_target(&host, &surface).inspect_err(|_| {
            let _ = composition.remove_surface(surface.id());
        });
        (surface, target.map_err(error)?)
    };
    if let Ok(platform) = surface.platform_surface() {
        let _ = platform.set_visible(false);
    }

    let webkit = raw.webview();
    let before = signal_marker(&webkit);
    let view = WebView::new(raw, window, cx);
    disconnect_press_forwarding(&webkit, before);

    let slot = Rc::new(Slot {
        id,
        webkit: webkit.as_ptr() as usize,
        gtk_window,
        surface,
        target: RefCell::new(Some(target)),
        window: window.window_handle(),
        cx: cx.to_async(),
        geometry: Cell::new(None),
        frame: RefCell::new(None),
        gtk_visible: Cell::new(webkit.is_visible()),
        shown: Cell::new(false),
        size: Cell::new(None),
        unanswered: Cell::new(false),
        answer_at: Cell::new(cx.background_executor().now()),
        cursor: Cell::new(CursorStyle::Arrow),
        css_cursor: Cell::new(None),
        hovered: Cell::new(false),
        focus: view.focus_handle(cx),
        focused: Cell::new(false),
        modifiers: Cell::new(SentModifiers::default()),
        held: RefCell::new(HashMap::new()),
        composing: RefCell::new(None),
        caret: Cell::new(None),
    });

    let input = Rc::downgrade(&slot);
    window
        .enable_window_composition()
        .map_err(|e| error(e.to_string()))?
        .on_input(slot.surface.id(), move |event| {
            if let Some(slot) = input.upgrade() {
                slot.pointer(event);
            }
            DispatchEventResult {
                propagate: false,
                default_prevented: true,
            }
        })
        .map_err(|e| error(e.to_string()))?;
    connect_webkit(&webkit, &slot);
    host.slots.borrow_mut().insert(id, slot);
    pump_gtk(&host, cx);
    Ok(view)
}

fn native_target(host: &Host, surface: &WindowCompositionSurface) -> Result<Target, String> {
    let handle = surface
        .platform_surface()
        .and_then(|platform| platform.platform_handle())
        .map_err(|error| error.to_string())?;
    match handle.downcast::<RawWindowHandle>().map(|handle| *handle) {
        // SAFETY: GPUI destroys this wl_surface only when the composition
        // surface is removed, which `Slot::release` does after dropping the
        // target.
        Ok(RawWindowHandle::Wayland(handle)) => unsafe { host.presenter.target(handle.surface) },
        _ => Err("the composition surface is not a Wayland surface".into()),
    }
}

/// The id the next signal handler connected anywhere in the process gets
/// after the ones already connected. GLib numbers handlers from one
/// process-wide counter.
fn signal_marker(object: &impl IsA<glib::Object>) -> std::ffi::c_ulong {
    let id = object.connect_notify_local(None, |_, _| {});
    // SAFETY: only reads the id.
    let raw = unsafe { id.as_raw() };
    object.disconnect(id);
    raw
}

/// gpui-webview's `WebView::new` (0.9.0, `linux::forward_mouse_down`)
/// connects a `button-press-event` handler that forwards each GTK press on
/// the page back to GPUI as a synthetic click, so overlays close when the
/// page is clicked on X11. Here GPUI already sees every real press before
/// forwarding it to the nest, and the synthetic click would land on this
/// preview's own input consumer and be forwarded again, without end. That
/// handler is the `button-press-event` handler numbered after `before`.
fn disconnect_press_forwarding(webkit: &webkit2gtk::WebView, before: std::ffi::c_ulong) {
    use glib::gobject_ffi::{
        G_SIGNAL_MATCH_ID, G_SIGNAL_MATCH_UNBLOCKED, g_signal_handler_block,
        g_signal_handler_disconnect, g_signal_handler_find, g_signal_handler_unblock,
        g_signal_lookup,
    };
    let after = signal_marker(webkit);
    let instance: *mut glib::gobject_ffi::GObject =
        webkit.upcast_ref::<glib::Object>().to_glib_none().0;
    // SAFETY: `instance` is alive for these calls; the lookups only read,
    // and every handler blocked here is unblocked before returning.
    unsafe {
        let signal = g_signal_lookup(
            c"button-press-event".as_ptr(),
            gtk::Widget::static_type().into_glib(),
        );
        // GLib finds one handler at a time; blocking each one found lists
        // the next.
        let mut handlers = Vec::new();
        loop {
            let id = g_signal_handler_find(
                instance,
                G_SIGNAL_MATCH_ID | G_SIGNAL_MATCH_UNBLOCKED,
                signal,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if id == 0 {
                break;
            }
            g_signal_handler_block(instance, id);
            handlers.push(id);
        }
        for id in handlers {
            g_signal_handler_unblock(instance, id);
            if before < id && id < after {
                g_signal_handler_disconnect(instance, id);
            }
        }
    }
}

fn connect_webkit(webkit: &webkit2gtk::WebView, slot: &Rc<Slot>) {
    let shown = Rc::downgrade(slot);
    webkit.connect_show(move |_| {
        if let Some(slot) = shown.upgrade() {
            slot.gtk_visible.set(true);
            slot.refresh_visibility();
        }
    });
    let hidden = Rc::downgrade(slot);
    webkit.connect_hide(move |_| {
        if let Some(slot) = hidden.upgrade() {
            slot.gtk_visible.set(false);
            slot.refresh_visibility();
        }
    });
    let pointed = Rc::downgrade(slot);
    webkit.connect_mouse_target_changed(move |_, hit, _| {
        let Some(slot) = pointed.upgrade() else {
            return;
        };
        let cursor = if hit.context_is_link() {
            CursorStyle::PointingHand
        } else if hit.context_is_editable() {
            CursorStyle::IBeam
        } else {
            CursorStyle::Arrow
        };
        if slot.cursor.replace(cursor) != cursor {
            slot.refresh_window();
        }
    });
    if let Some(window) = webkit.window() {
        watch_cursor(&window, slot);
    }
    let realized = Rc::downgrade(slot);
    webkit.connect_realize(move |webkit| {
        if let (Some(slot), Some(window)) = (realized.upgrade(), webkit.window()) {
            watch_cursor(&window, &slot);
        }
    });
    let destroyed: Weak<Slot> = Rc::downgrade(slot);
    webkit.connect_destroy(move |_| {
        if let Some(slot) = destroyed.upgrade() {
            slot.release();
        }
    });
}

/// Follows the cursor WebKit sets on its GDK window.
fn watch_cursor(window: &gdk::Window, slot: &Rc<Slot>) {
    let slot = Rc::downgrade(slot);
    window.connect_notify_local(Some("cursor"), move |window, _| {
        let (Some(slot), Some(host)) = (slot.upgrade(), current_host()) else {
            return;
        };
        let style = window.cursor().and_then(|cursor| {
            host.cursors
                .iter()
                .find(|(known, _)| *known == cursor)
                .map(|&(_, style)| style)
        });
        if slot.css_cursor.replace(style) != style {
            slot.refresh_window();
        }
    });
}

#[derive(Clone, Copy, Default, PartialEq)]
struct SentModifiers {
    shift: bool,
    control: bool,
    alt: bool,
    super_key: bool,
}

#[derive(Clone, Copy)]
struct Geometry {
    document: Bounds<Pixels>,
    visible: Bounds<Pixels>,
    scale: f32,
}

impl Geometry {
    fn crop(&self) -> Crop {
        let document = self.document.to_device_pixels(self.scale);
        let visible = self.visible.to_device_pixels(self.scale);
        Crop {
            x: visible.origin.x.0 - document.origin.x.0,
            y: visible.origin.y.0 - document.origin.y.0,
            width: visible.size.width.0,
            height: visible.size.height.0,
        }
    }

    /// A rectangle of the view's coordinates in the window.
    fn window_bounds(&self, caret: Caret) -> Bounds<Pixels> {
        let logical = |value: i32| px(value as f32 / self.scale);
        Bounds::new(
            self.document.origin + point(logical(caret.x), logical(caret.y)),
            size(logical(caret.width), logical(caret.height)),
        )
    }

    /// A window position in the view's coordinates (its device pixels).
    fn view_position(&self, position: gpui::Point<Pixels>) -> (f64, f64) {
        let offset = position - self.document.origin;
        (
            f64::from(f32::from(offset.x) * self.scale),
            f64::from(f32::from(offset.y) * self.scale),
        )
    }
}

/// One webview's GTK window, composition surface and the state between them.
struct Slot {
    id: ViewId,
    webkit: usize,
    gtk_window: gtk::Window,
    surface: WindowCompositionSurface,
    target: RefCell<Option<Target>>,
    window: AnyWindowHandle,
    cx: AsyncApp,
    geometry: Cell<Option<Geometry>>,
    frame: RefCell<Option<Frame>>,
    gtk_visible: Cell<bool>,
    shown: Cell<bool>,
    size: Cell<Option<(u32, u32)>>,
    /// A frame arrived while hidden; it is answered once the view shows.
    unanswered: Cell<bool>,
    answer_at: Cell<Instant>,
    /// The cursor WebKit's hit test suggests.
    cursor: Cell<CursorStyle>,
    /// The CSS cursor the page set, when GPUI has it (see [`CSS_CURSORS`]).
    css_cursor: Cell<Option<CursorStyle>>,
    /// Whether the pointer is over the view's surface.
    hovered: Cell<bool>,
    focus: FocusHandle,
    focused: Cell<bool>,
    modifiers: Cell<SentModifiers>,
    /// The nest key each held GPUI key pressed, by GPUI key name.
    held: RefCell<HashMap<String, u32>>,
    /// The input method's composition, while there is one.
    composing: RefCell<Option<String>>,
    /// Where the page's text field draws its caret, in the view.
    caret: Cell<Option<Caret>>,
}

impl Slot {
    fn nest(&self) -> Option<Rc<Host>> {
        current_host()
    }

    /// Follows the element's bounds and clip for this frame.
    fn layout(&self, document: Bounds<Pixels>, clip: Bounds<Pixels>, scale: f32) {
        let geometry = Geometry {
            document,
            visible: document.intersect(&clip),
            scale,
        };
        let previous = self.geometry.replace(Some(geometry));
        let size = document.to_device_pixels(scale).size;
        let size = (size.width.0.max(1) as u32, size.height.0.max(1) as u32);
        if self.size.replace(Some(size)) != Some(size)
            && let Some(host) = self.nest()
        {
            host.nest.configure(self.id, size.0, size.1);
        }
        let moved = previous.is_none_or(|previous| {
            previous.document != geometry.document
                || previous.visible != geometry.visible
                || previous.scale != geometry.scale
        });
        if moved {
            if !geometry.visible.is_empty()
                && let Ok(platform) = self.surface.platform_surface()
                && let Err(error) = platform.set_bounds(geometry.visible.to_device_pixels(scale))
            {
                log::debug!("preview: failed to place nested surface: {error:#}");
            }
            self.present();
        }
        self.refresh_visibility();
    }

    fn show_frame(&self, host: &Host, frame: Frame) {
        self.frame.replace(Some(frame));
        self.present_with(host);
        self.unanswered.set(true);
        self.refresh_visibility();
        if self.shown.get() {
            self.answer();
        }
    }

    /// Lets the view draw its next frame, no sooner than [`FRAME_INTERVAL`]
    /// after the last.
    fn answer(&self) {
        if !self.unanswered.replace(false) {
            return;
        }
        let now = self.cx.background_executor().now();
        let at = self.answer_at.get().max(now);
        self.answer_at.set(at + FRAME_INTERVAL);
        let view = self.id;
        self.cx
            .spawn(async move |cx| {
                cx.background_executor().timer(at - now).await;
                if let Some(host) = current_host() {
                    host.nest.presented(view);
                }
            })
            .detach();
    }

    fn present(&self) {
        if let Some(host) = self.nest() {
            self.present_with(&host);
        }
    }

    fn present_with(&self, host: &Host) {
        let Some(geometry) = self.geometry.get() else {
            return;
        };
        if geometry.visible.is_empty() {
            return;
        }
        let frame = self.frame.borrow();
        let Some(frame) = frame.as_ref() else {
            return;
        };
        let mut target = self.target.borrow_mut();
        let Some(target) = target.as_mut() else {
            return;
        };
        if let Err(error) = target.present(&host.presenter, frame, geometry.crop()) {
            log::debug!("preview: failed to present nested frame: {error}");
        }
    }

    fn refresh_visibility(&self) {
        let laid_out = self
            .geometry
            .get()
            .is_some_and(|geometry| !geometry.visible.is_empty());
        let show = self.gtk_visible.get()
            && laid_out
            && self.frame.borrow().is_some()
            && self.target.borrow().is_some();
        if self.shown.replace(show) == show {
            return;
        }
        if let Ok(platform) = self.surface.platform_surface()
            && let Err(error) = platform.set_visible(show)
        {
            log::debug!("preview: failed to show nested surface: {error:#}");
        }
        if show {
            self.answer();
        }
    }

    fn set_focused(&self, focused: bool) {
        if self.focused.replace(focused) == focused {
            return;
        }
        if let Some(host) = self.nest() {
            if !focused {
                for (_, code) in self.held.borrow_mut().drain() {
                    host.nest.key(code, false);
                }
                self.sync_modifiers(&host, Modifiers::default(), false);
                self.composing.take();
            }
            host.nest.focus(focused.then_some(self.id));
        }
    }

    fn pointer(&self, input: &PlatformInput) {
        let (Some(host), Some(geometry)) = (self.nest(), self.geometry.get()) else {
            return;
        };
        let motion = |position| {
            let (x, y) = geometry.view_position(position);
            PointerEvent::Motion { x, y }
        };
        let send = |event| host.nest.pointer(self.id, event);
        match input {
            PlatformInput::MouseMove(event) => {
                self.set_hovered(true);
                send(motion(event.position));
            }
            PlatformInput::MouseDown(event) => {
                self.pressed(event.button);
                send(motion(event.position));
                if let Some(button) = button(event.button) {
                    send(PointerEvent::Button {
                        button,
                        pressed: true,
                    });
                }
            }
            PlatformInput::MouseUp(event) => {
                send(motion(event.position));
                if let Some(button) = button(event.button) {
                    send(PointerEvent::Button {
                        button,
                        pressed: false,
                    });
                }
            }
            PlatformInput::MouseExited(_) => {
                self.set_hovered(false);
                send(PointerEvent::Leave);
            }
            PlatformInput::Scroll(event) => {
                send(motion(event.position));
                let (dx, dy, source, discrete) = match event.delta {
                    ScrollDelta::Lines(lines) => {
                        let steps = (
                            -lines.x / GPUI_LINES_PER_STEP,
                            -lines.y / GPUI_LINES_PER_STEP,
                        );
                        (
                            f64::from(steps.0) * AXIS_UNITS_PER_STEP,
                            f64::from(steps.1) * AXIS_UNITS_PER_STEP,
                            ScrollSource::Wheel,
                            Some(((steps.0 * 120.0) as i32, (steps.1 * 120.0) as i32)),
                        )
                    }
                    ScrollDelta::Pixels(pixels) => {
                        let unit = |value: Pixels| {
                            f64::from(
                                -f32::from(value) / GPUI_PIXELS_PER_AXIS_UNIT * geometry.scale,
                            )
                        };
                        (unit(pixels.x), unit(pixels.y), ScrollSource::Finger, None)
                    }
                };
                send(PointerEvent::Scroll {
                    dx,
                    dy,
                    source,
                    discrete,
                    stop: matches!(event.touch_phase, TouchPhase::Ended),
                });
            }
            _ => {}
        }
    }

    fn set_hovered(&self, hovered: bool) {
        if self.hovered.replace(hovered) != hovered {
            self.refresh_window();
        }
    }

    /// Gives the page the keyboard when it is pressed, and the clipboard
    /// before its context menu opens. GPUI hit-tests no hitbox under a
    /// native surface, so only the surface's input handler sees the press,
    /// while the window is being updated.
    fn pressed(&self, button: MouseButton) {
        let (window, focus) = (self.window, self.focus.clone());
        self.cx
            .spawn(async move |cx| {
                let _ = window.update(cx, |_, window, cx| {
                    window.focus(&focus, cx);
                    if button == MouseButton::Right {
                        offer_clipboard(cx);
                    }
                });
            })
            .detach();
    }

    /// Forwards a key, or the character it types; returns whether it did.
    /// Text an input method commits reaches [`PageInput`] instead.
    fn key(&self, keystroke: &Keystroke, pressed: bool, repeat: bool, cx: &App) -> bool {
        let Some(host) = self.nest() else {
            return false;
        };
        let modifiers = keystroke.modifiers;
        if !pressed {
            let Some(code) = self.held.borrow_mut().remove(&keystroke.key) else {
                return false;
            };
            host.nest.key(code, false);
            self.sync_modifiers(&host, modifiers, false);
            return true;
        }
        if !repeat && pastes(keystroke) {
            offer_clipboard(cx);
        }
        // A shortcut is the key under the US layout GPUI guesses for it, and
        // anything else is the character the user's layout typed.
        let shortcut = modifiers.control || modifiers.alt || modifiers.platform;
        let typed = match &keystroke.key_char {
            Some(text) if !shortcut => text,
            _ => &keystroke.key,
        };
        let Some((code, shift)) = wayland_nest::evdev_key(typed) else {
            // A character the US layout lacks is typed through a key the
            // nest binds to it, so it reaches the page in order with the
            // keys around it.
            let Some(text) = keystroke.key_char.as_deref().filter(|_| !shortcut) else {
                return false;
            };
            if self.composing.take().is_some() {
                host.nest.compose_text(String::new(), None);
            }
            self.sync_modifiers(&host, modifiers, false);
            for c in text.chars() {
                host.nest.type_char(c);
            }
            return true;
        };
        self.sync_modifiers(&host, modifiers, shift);
        // A repeat, or a key GPUI made from an input method's commit (which
        // has no release), presses a key that is still down.
        if let Some(down) = self.held.borrow_mut().insert(keystroke.key.clone(), code) {
            host.nest.key(down, false);
        }
        host.nest.key(code, true);
        true
    }

    fn sync_modifiers(&self, host: &Host, modifiers: Modifiers, shift: bool) {
        use wayland_nest::modifier;
        let wanted = SentModifiers {
            shift: modifiers.shift || shift,
            control: modifiers.control,
            alt: modifiers.alt,
            super_key: modifiers.platform,
        };
        let sent = self.modifiers.replace(wanted);
        for (was, is, code) in [
            (sent.shift, wanted.shift, modifier::SHIFT),
            (sent.control, wanted.control, modifier::CONTROL),
            (sent.alt, wanted.alt, modifier::ALT),
            (sent.super_key, wanted.super_key, modifier::SUPER),
        ] {
            if was != is {
                host.nest.key(code, is);
            }
        }
    }

    /// Repaints the window, so a changed cursor is applied. GTK signals can
    /// fire while GPUI is updating, so this waits for the next turn.
    fn refresh_window(&self) {
        let window = self.window;
        self.cx
            .spawn(async move |cx| {
                let _ = window.update(cx, |_, window, _| window.refresh());
            })
            .detach();
    }

    /// Tears down everything but the webview, which is being destroyed.
    fn release(self: &Rc<Self>) {
        let Some(host) = current_host() else {
            return;
        };
        if host.quitting.get() {
            return;
        }
        host.slots.borrow_mut().remove(&self.id);
        self.target.borrow_mut().take();
        let window = self.window;
        let surface = self.surface.id();
        let gtk_window = self.gtk_window.clone();
        self.cx
            .spawn(async move |cx| {
                // SAFETY: the window holds only the destroyed webview.
                unsafe { gtk_window.destroy() };
                let result = window.update(cx, |_, window, _| {
                    window.enable_window_composition()?.remove_surface(surface)
                });
                if let Ok(Err(error)) = result {
                    log::warn!("preview: cannot release the nested surface: {error:#}");
                }
            })
            .detach();
    }
}

/// Whether `keystroke` pastes in GTK: Ctrl+V, Ctrl+Shift+V or Shift+Insert.
fn pastes(keystroke: &Keystroke) -> bool {
    let modifiers = keystroke.modifiers;
    if modifiers.alt || modifiers.platform {
        return false;
    }
    match keystroke.key.as_str() {
        "v" => modifiers.control,
        "insert" => modifiers.shift && !modifiers.control,
        _ => false,
    }
}

/// Offers the system clipboard to the page before something that may paste
/// it. The nest cannot watch the system clipboard, so it is synced on the
/// inputs that paste: the paste keys and the context menu. A read blocks
/// this thread until the clipboard's owner answers (up to 4 s in
/// gpui-fast-linux 0.1.13's `PIPE_READ_TIMEOUT`), so nothing else reads it.
fn offer_clipboard(cx: &App) {
    if let Some(host) = current_host()
        && let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
    {
        host.nest.set_clipboard(text);
    }
}

/// The page's text input as GPUI's platform input sees it: what the input
/// method composes and commits goes to the page's focused text field through
/// the nest. The page's text is not mirrored here, so the document is the
/// composition alone.
struct PageInput(Rc<Slot>);

impl InputHandler for PageInput {
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut App,
    ) -> Option<UTF16Selection> {
        let end = self.0.composing.borrow().as_deref().map_or(0, utf16_len);
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _: &mut Window, _: &mut App) -> Option<Range<usize>> {
        Some(0..utf16_len(self.0.composing.borrow().as_deref()?))
    }

    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        _: &mut App,
    ) {
        let composing = self.0.composing.take();
        let Some(host) = self.0.nest() else {
            return;
        };
        if !text.is_empty() {
            host.nest.commit_text(text.into());
        } else if composing.is_some() {
            host.nest.compose_text(String::new(), None);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) {
        let Some(host) = self.0.nest() else {
            return;
        };
        let end = utf16_len(text);
        let selected = selected.unwrap_or(end..end);
        let cursor = byte_offset(text, selected.start)..byte_offset(text, selected.end);
        self.0
            .composing
            .replace((!text.is_empty()).then(|| text.to_owned()));
        host.nest.compose_text(text.into(), Some(cursor));
    }

    /// Keeps the composition as typed text.
    fn unmark_text(&mut self, _: &mut Window, _: &mut App) {
        if let Some(text) = self.0.composing.take()
            && let Some(host) = self.0.nest()
        {
            host.nest.commit_text(text);
        }
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<Bounds<Pixels>> {
        Some(self.0.geometry.get()?.window_bounds(self.0.caret.get()?))
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<usize> {
        None
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The byte offset in `text` of the UTF-16 offset `utf16`.
fn byte_offset(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (offset, c) in text.char_indices() {
        if units >= utf16 {
            return offset;
        }
        units += c.len_utf16();
    }
    text.len()
}

fn button(button: MouseButton) -> Option<u32> {
    use wayland_nest::button;
    Some(match button {
        MouseButton::Left => button::LEFT,
        MouseButton::Right => button::RIGHT,
        MouseButton::Middle => button::MIDDLE,
        MouseButton::Navigate(NavigationDirection::Back) => button::BACK,
        MouseButton::Navigate(NavigationDirection::Forward) => button::FORWARD,
    })
}

/// Wraps a preview built by [`build`] with what the nest needs from GPUI:
/// layout, keyboard focus, keys and input method text, and the page's cursor.
pub(super) fn element(view: Entity<WebView>, cx: &App) -> AnyElement {
    let webkit = view.read(cx).raw().webview().as_ptr() as usize;
    let slot = current_host().and_then(|host| {
        host.slots
            .borrow()
            .values()
            .find(|slot| slot.webkit == webkit)
            .cloned()
    });
    let Some(slot) = slot else {
        return view.into_any_element();
    };
    let (down, up, changed, laid_out, painted) =
        (slot.clone(), slot.clone(), slot.clone(), slot.clone(), slot);
    div()
        .size_full()
        .relative()
        .on_key_down(move |event: &KeyDownEvent, _, cx| {
            if down.key(&event.keystroke, true, event.is_held, cx) {
                cx.stop_propagation();
            }
        })
        .on_key_up(move |event: &KeyUpEvent, _, cx| {
            if up.key(&event.keystroke, false, false, cx) {
                cx.stop_propagation();
            }
        })
        .on_modifiers_changed(move |event: &ModifiersChangedEvent, _, _| {
            if let Some(host) = changed.nest()
                && changed.focused.get()
            {
                changed.sync_modifiers(&host, event.modifiers, false);
            }
        })
        .child(view)
        .child(
            canvas(
                move |bounds, window, _| {
                    let clip = window.content_mask().bounds;
                    laid_out.layout(bounds, clip, window.scale_factor());
                    laid_out.set_focused(laid_out.focus.is_focused(window));
                },
                move |_, _, window, cx| {
                    if painted.hovered.get() {
                        let cursor = painted.css_cursor.get().unwrap_or(painted.cursor.get());
                        window.set_window_cursor_style(cursor);
                    }
                    window.handle_input(&painted.focus, PageInput(painted.clone()), cx);
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
        .into_any_element()
}
