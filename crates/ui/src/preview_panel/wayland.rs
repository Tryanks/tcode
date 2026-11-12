//! The preview browser on a Wayland session.
//!
//! WebKitGTK only draws through GDK's own Wayland connection, and a surface of
//! one connection cannot be a child of another's, so GTK runs against a nested
//! compositor ([`wayland_nest::Nest`]) instead of the session's. Each webview
//! lives in its own undecorated GTK window there, and the nest draws that
//! window and its popups (`<select>` menus, context menus) on a GPUI Fast
//! native composition surface of the preview's window, at the session's pace,
//! and passes the session's pointer on that surface to the page. This module
//! tells the nest where each view is shown and gives it what only GPUI has:
//! keyboard focus, keys and input method text (through the element returned
//! by [`element`]), and the clipboard.
//!
//! GTK can be initialized once per process, on one display, so a process that
//! opened its first preview here keeps all of GTK on the nest.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ops::Range;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};

use gpui::{
    AnyElement, AnyWindowHandle, App, AsyncApp, Bounds, ClipboardItem, CursorStyle,
    DispatchEventResult, Entity, FocusHandle, Focusable as _, InputHandler,
    InteractiveElement as _, IntoElement as _, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers,
    ModifiersChangedEvent, ParentElement as _, Pixels, PlatformInput, Styled as _, UTF16Selection,
    Window, WindowCompositionSurface, canvas, div, point, px, size,
};
use gpui_webview::WebView;
use gtk::prelude::*;
use raw_window_handle::{HasDisplayHandle as _, RawDisplayHandle, RawWindowHandle};
use wayland_nest::{Caret, Crop, Event, Nest, Placement, ViewId};
use webkit2gtk::{HitTestResultExt as _, WebViewExt as _};
use wry::{WebViewBuilderExtUnix as _, WebViewExtUnix as _};

/// Whether `window` is on Wayland, where previews are built by [`build`].
pub(super) fn active(window: &Window) -> bool {
    matches!(
        window.display_handle().map(|handle| handle.as_raw()),
        Ok(RawDisplayHandle::Wayland(_))
    )
}

struct Host {
    nest: Nest,
    slots: RefCell<HashMap<ViewId, Rc<Slot>>>,
    next_view: Cell<u64>,
    /// Set once the app quits, from when the host is leaked (see [`start`]):
    /// a webview destroyed after that releases nothing.
    quitting: Cell<bool>,
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
    // SAFETY: GPUI keeps its Wayland connection until the app quits, and the
    // nest leaves it then, below.
    let nest =
        unsafe { Nest::start(display) }.map_err(|error| format!("nested compositor: {error}"))?;
    nest.start_gtk()?;
    cx.spawn(async |_| {
        wayland_nest::run_gtk().await;
    })
    .detach();
    let host = Rc::new(Host {
        nest,
        slots: RefCell::new(HashMap::new()),
        next_view: Cell::new(0),
        quitting: Cell::new(false),
        cursors: css_cursors(),
    });

    let quitting = Rc::downgrade(&host);
    cx.on_app_quit(move |_| {
        if let Some(host) = quitting.upgrade() {
            host.quitting.set(true);
            // GPUI drops its Wayland client and disconnects the display
            // before the app's webviews and this thread's locals are
            // destroyed, so the nest stops using that display now. A slot's
            // native surface cannot be dropped after the client either:
            // gpui-fast-linux 0.1.13's `WaylandNativeSurface::drop`
            // (fast/composition/wayland.rs:362-367) calls
            // `WaylandClientStatePtr::get_client`, which panics once the
            // client is gone (linux/wayland/client.rs:584-588). So the
            // process exits with the host and its slots leaked.
            host.nest.leave_host();
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
                Event::Copied(text) => {
                    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
                }
                // A read blocks this thread until the clipboard's owner
                // answers (up to 4 s in gpui-fast-linux 0.1.13's
                // `PIPE_READ_TIMEOUT`); it happens when the page pastes and
                // at no other time.
                Event::Paste(paste) => {
                    paste.answer(
                        cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text())),
                    );
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

    let (surface, native) = {
        let composition = window
            .enable_window_composition()
            .map_err(|e| error(e.to_string()))?;
        let surface = composition
            .create_native_surface()
            .map_err(|e| error(e.to_string()))?;
        let native = native_surface(&surface).inspect_err(|_| {
            let _ = composition.remove_surface(surface.id());
        });
        (surface, native.map_err(error)?)
    };
    if let Ok(platform) = surface.platform_surface() {
        let _ = platform.set_visible(false);
    }
    // SAFETY: GPUI destroys this wl_surface only when the composition
    // surface is removed, which `Slot::release` does after the nest has
    // detached from it.
    unsafe { host.nest.attach(id, native) };

    let webkit = raw.webview();
    let view = WebView::new(raw, window, cx);
    let slot = Rc::new(Slot {
        id,
        webkit: webkit.as_ptr() as usize,
        gtk_window,
        surface,
        window: window.window_handle(),
        cx: cx.to_async(),
        geometry: Cell::new(None),
        placement: Cell::new(None),
        gtk_visible: Cell::new(webkit.is_visible()),
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
    Ok(view)
}

/// The `wl_surface` of a native composition surface.
fn native_surface(surface: &WindowCompositionSurface) -> Result<NonNull<c_void>, String> {
    let handle = surface
        .platform_surface()
        .and_then(|platform| platform.platform_handle())
        .map_err(|error| error.to_string())?;
    match handle.downcast::<RawWindowHandle>().map(|handle| *handle) {
        Ok(RawWindowHandle::Wayland(handle)) => Ok(handle.surface),
        _ => Err("the composition surface is not a Wayland surface".into()),
    }
}

fn connect_webkit(webkit: &webkit2gtk::WebView, slot: &Rc<Slot>) {
    let shown = Rc::downgrade(slot);
    webkit.connect_show(move |_| {
        if let Some(slot) = shown.upgrade() {
            slot.gtk_visible.set(true);
            slot.show();
        }
    });
    let hidden = Rc::downgrade(slot);
    webkit.connect_hide(move |_| {
        if let Some(slot) = hidden.upgrade() {
            slot.gtk_visible.set(false);
            slot.show();
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

#[derive(Clone, Copy, PartialEq)]
struct Geometry {
    document: Bounds<Pixels>,
    visible: Bounds<Pixels>,
    scale: f32,
}

impl Geometry {
    /// Where the nest shows the view: the visible part of the document, in
    /// device pixels.
    fn placement(&self) -> Placement {
        let document = self.document.to_device_pixels(self.scale);
        let visible = self.visible.to_device_pixels(self.scale);
        Placement {
            width: document.size.width.0.max(1) as u32,
            height: document.size.height.0.max(1) as u32,
            crop: Crop {
                x: visible.origin.x.0 - document.origin.x.0,
                y: visible.origin.y.0 - document.origin.y.0,
                width: visible.size.width.0,
                height: visible.size.height.0,
            },
            scale: self.scale,
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
}

/// One webview's GTK window, composition surface and the state between them.
struct Slot {
    id: ViewId,
    webkit: usize,
    gtk_window: gtk::Window,
    surface: WindowCompositionSurface,
    window: AnyWindowHandle,
    cx: AsyncApp,
    geometry: Cell<Option<Geometry>>,
    /// Where the nest was last told to show the view; `None` while its
    /// surface is hidden.
    placement: Cell<Option<Placement>>,
    gtk_visible: Cell<bool>,
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
        if self.geometry.replace(Some(geometry)) != Some(geometry)
            && !geometry.visible.is_empty()
            && let Ok(platform) = self.surface.platform_surface()
            && let Err(error) = platform.set_bounds(geometry.visible.to_device_pixels(scale))
        {
            log::error!("preview: failed to place nested surface: {error:#}");
        }
        self.show();
    }

    /// Shows the surface where the view is laid out, or hides it, and tells
    /// the nest, which draws the view there. The surface is shown before the
    /// nest is told: a hidden surface has no place in the window to draw on.
    fn show(&self) {
        let placement = self
            .geometry
            .get()
            .filter(|geometry| self.gtk_visible.get() && !geometry.visible.is_empty())
            .map(|geometry| geometry.placement());
        let previous = self.placement.replace(placement);
        if previous == placement {
            return;
        }
        if previous.is_some() != placement.is_some()
            && let Ok(platform) = self.surface.platform_surface()
            && let Err(error) = platform.set_visible(placement.is_some())
        {
            log::error!("preview: failed to show nested surface: {error:#}");
        }
        if let Some(host) = self.nest() {
            host.nest.place(self.id, placement);
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

    /// What GPUI needs from the pointer on the view's surface; the nest
    /// hears the session's pointer itself and passes it to the page. GPUI
    /// hit-tests no hitbox under a native surface, so only the surface's
    /// input handler sees these, while the window is being updated.
    fn pointer(&self, input: &PlatformInput) {
        match input {
            PlatformInput::MouseMove(_) => self.set_hovered(true),
            PlatformInput::MouseExited(_) => self.set_hovered(false),
            // The page takes the keyboard when it is pressed.
            PlatformInput::MouseDown(_) => {
                let (window, focus) = (self.window, self.focus.clone());
                self.cx
                    .spawn(async move |cx| {
                        let _ = window.update(cx, |_, window, cx| window.focus(&focus, cx));
                    })
                    .detach();
            }
            _ => {}
        }
    }

    fn set_hovered(&self, hovered: bool) {
        if self.hovered.replace(hovered) != hovered {
            self.refresh_window();
        }
    }

    /// Forwards a key, or the character it types; returns whether it did.
    /// Text an input method commits reaches [`PageInput`] instead.
    ///
    /// GPUI reports a key as its name and the character it types, not as
    /// the key code the session sent, and decides first whether the key is
    /// one of the app's own shortcuts. So the page gets the key on the US
    /// layout that types the same thing, not the user's own layout.
    fn key(&self, keystroke: &Keystroke, pressed: bool) -> bool {
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
        host.nest.detach(self.id);
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
                    log::error!("preview: cannot release the nested surface: {error:#}");
                }
            })
            .detach();
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
            if down.key(&event.keystroke, true) {
                cx.stop_propagation();
            }
        })
        .on_key_up(move |event: &KeyUpEvent, _, cx| {
            if up.key(&event.keystroke, false) {
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
