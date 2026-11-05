//! Where pages show on Windows, so the surfaces beneath them leave those
//! bounds unpainted ([`crate::material::surface_fill`]).
//!
//! Gap: gpui-webview has no window composition on Windows; it hosts WebView2
//! as a child window of the GPUI window, and gpui-fast-windows binds GPUI's
//! DirectComposition target topmost, above the window's children. Everything
//! GPUI paints in a page's bounds is composited over the page: the window's
//! translucent canvas dims it, an opaque panel fill hides it. Removed once
//! gpui-webview composes the page on Windows as it does on macOS and Linux.

use std::cell::RefCell;
use std::collections::HashMap;

use gpui::{
    App, AppContext as _, Bounds, Entity, EntityId, Global, Pixels, Subscription, Window, WindowId,
};
use gpui_webview::WebView;

/// A page on screen: the window it is in, its bounds there, and the
/// subscription that forgets it once its view is released.
struct Hole {
    window: WindowId,
    bounds: Bounds<Pixels>,
    _released: Subscription,
}

/// The pages on screen, by view. Read and written while frames are drawn,
/// so it is not an entity; `changed` is notified, after the frame, to draw
/// again the surfaces whose last paint left another hole.
struct PageHoles {
    pages: RefCell<HashMap<EntityId, Hole>>,
    changed: Entity<()>,
}

impl Global for PageHoles {}

fn holes(cx: &mut App) -> &PageHoles {
    if !cx.has_global::<PageHoles>() {
        let changed = cx.new(|_| ());
        cx.set_global(PageHoles {
            pages: RefCell::default(),
            changed,
        });
    }
    cx.global::<PageHoles>()
}

/// After the frame: what was painted around the old holes is drawn again.
fn notify_changed(cx: &mut App) {
    let changed = holes(cx).changed.clone();
    cx.defer(move |cx| changed.update(cx, |_, cx| cx.notify()));
}

/// Record where `view` shows this frame, from its prepaint: `bounds` within
/// the content mask, or nowhere once it is not shown or clipped away.
pub(super) fn record(
    view: &Entity<WebView>,
    bounds: Bounds<Pixels>,
    window: &Window,
    cx: &mut App,
) {
    let shown = view.read(cx).visible();
    let bounds = bounds.intersect(&window.content_mask().bounds);
    let id = view.entity_id();
    if !shown || bounds.is_empty() {
        forget(id, cx);
        return;
    }
    let window = window.window_handle().window_id();
    let registry = holes(cx);
    if let Some(hole) = registry.pages.borrow_mut().get_mut(&id) {
        if hole.window == window && hole.bounds == bounds {
            return;
        }
        hole.window = window;
        hole.bounds = bounds;
    } else {
        let released = cx.observe_release(view, move |_, cx| forget(id, cx));
        holes(cx).pages.borrow_mut().insert(
            id,
            Hole {
                window,
                bounds,
                _released: released,
            },
        );
    }
    notify_changed(cx);
}

/// `view` no longer shows: it was hidden or released.
pub(super) fn forget(view: EntityId, cx: &mut App) {
    if !cx.has_global::<PageHoles>() {
        return;
    }
    let removed = holes(cx).pages.borrow_mut().remove(&view);
    if removed.is_some() {
        notify_changed(cx);
    }
}

/// The bounds pages show at in `window`. Read while painting, so a surface
/// painted around them is drawn again when they change.
pub(crate) fn in_window(window: &Window, cx: &mut App) -> Vec<Bounds<Pixels>> {
    if !cx.has_global::<PageHoles>() {
        return Vec::new();
    }
    let id = window.window_handle().window_id();
    let holes = cx.global::<PageHoles>();
    let changed = holes.changed.clone();
    let bounds = holes
        .pages
        .borrow()
        .values()
        .filter(|hole| hole.window == id)
        .map(|hole| hole.bounds)
        .collect();
    changed.read(cx);
    bounds
}
