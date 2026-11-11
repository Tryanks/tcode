//! Presence for a view the platform draws itself, outside GPUI's scene: an
//! embedded web page.
//!
//! Gap: gpui-base's presence fades GPUI content, and GPUI cannot fade over a
//! native view, which composites above GPUI's base content; gpui-webview
//! offers `set_visible` and bounds, no opacity. So the view's own alpha
//! (`NSView.alphaValue`, `UIView.alpha`) follows a gpui-base [`Presence`],
//! and the view is hidden only once its exit has played. An AppKit view is
//! hit-tested at any alpha, so a leaving view is covered by a hitbox on
//! GPUI's overlay surface, which takes the pointer before the native view
//! does; a view that cannot be covered that way stops taking input through
//! [`NativeView::set_inert`].

use std::time::Duration;

use gpui::{
    App, Bounds, ElementId, HitboxBehavior, IntoElement, Pixels, SharedString, Styled as _, canvas,
    deferred,
};
use gpui_base::motion::{Presence, Transition};

/// How long a native view takes to fade in where it now belongs.
const ENTER: Duration = Duration::from_millis(200);
/// How long it takes to fade out: quicker, since what replaces it, or the
/// drag it steps aside for, is already under way.
const LEAVE: Duration = Duration::from_millis(120);

/// A native view [`NativeFade`] drives.
pub(crate) trait NativeView: 'static {
    /// Whether the platform draws the view at all.
    fn shown(&self, cx: &App) -> bool;
    /// Stop drawing it: its exit has played.
    fn hide(&self, cx: &mut App);
    fn set_alpha(&self, alpha: f32, cx: &App);
    /// Whether the view itself declines input. Only where a GPUI hitbox
    /// cannot cover it: by default, it can.
    fn set_inert(&self, _inert: bool, _cx: &App) {}
    /// Where the view is, in window coordinates.
    fn bounds(&self, cx: &App) -> Bounds<Pixels>;
}

/// The native views of one owner, each present or not; drawn once per
/// frame, anywhere in the window, for as long as the owner has views.
///
/// The owner shows a view (`shown()` true) when it wants it on screen and
/// lists it as present; it fades in from transparent. Listed as absent while
/// still shown, it fades out, covered so it takes no input, and is hidden
/// once the fade lands. An owner may also hide a view itself, with no fade;
/// shown again, it fades in from transparent. Every view is listed every
/// frame, so its presence is kept between frames.
///
/// The list is read when the frame is prepainted, not when this is built:
/// the owner may show a view while its own element renders later in the
/// same frame, and a list read earlier would call that view absent.
pub(crate) struct NativeFade<V> {
    id: ElementId,
    views: ListViews<V>,
}

/// One view of a [`NativeFade`]: its key, the view, and whether it is
/// present.
pub(crate) type NativeEntry<V> = (SharedString, V, bool);

type ListViews<V> = Box<dyn FnOnce(&App) -> Vec<NativeEntry<V>>>;

impl<V: NativeView> NativeFade<V> {
    pub(crate) fn new(
        id: impl Into<ElementId>,
        views: impl FnOnce(&App) -> Vec<NativeEntry<V>> + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            views: Box::new(views),
        }
    }
}

impl<V: NativeView> IntoElement for NativeFade<V> {
    type Element = gpui::Deferred;

    fn into_element(self) -> Self::Element {
        let Self { id, views } = self;
        // Deferred, so the cover is on the overlay surface, above the native
        // views, and is prepainted after the views' own elements this frame.
        deferred(
            canvas(
                move |_, window, cx| {
                    let mut covers = Vec::new();
                    for (key, view, present) in views(cx) {
                        let shown = view.shown(cx);
                        let present = present && shown;
                        let sample = Presence::new(
                            ElementId::NamedChild(std::sync::Arc::new(id.clone()), key),
                            present,
                        )
                        // An owner that hid its view has already ended the
                        // exit: none is left to play when it shows again.
                        .transition(Transition::new(if !shown {
                            Duration::ZERO
                        } else if present {
                            ENTER
                        } else {
                            LEAVE
                        }))
                        .sample(window, cx);
                        if !shown {
                            continue;
                        }
                        if !sample.should_render() {
                            view.hide(cx);
                            continue;
                        }
                        view.set_alpha(sample.progress, cx);
                        view.set_inert(!present, cx);
                        if !present {
                            covers.push(view.bounds(cx));
                        }
                    }
                    for bounds in covers {
                        window.insert_hitbox(bounds, HitboxBehavior::BlockMouse);
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_0(),
        )
    }
}
