//! Presence for a view the platform draws itself, outside GPUI's scene: an
//! embedded web page.
//!
//! Gap: gpui-base's presence fades GPUI content, and GPUI cannot fade over a
//! native view, which composites above GPUI's base content; gpui-webview
//! 0.8.1 offers `set_visible` and bounds, no opacity. So the view's own alpha
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
const ENTER: Duration = crate::edge_back::SETTLE;
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
/// lists it here as present; it fades in from transparent. Listed as absent
/// while still shown, it fades out, covered so it takes no input, and is
/// hidden once the fade lands. Every view is listed every frame, so its
/// presence is kept between frames.
pub(crate) struct NativeFade<V> {
    id: ElementId,
    views: Vec<(SharedString, V, bool)>,
}

impl<V: NativeView> NativeFade<V> {
    pub(crate) fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            views: Vec::new(),
        }
    }

    pub(crate) fn view(mut self, key: impl Into<SharedString>, view: V, present: bool) -> Self {
        self.views.push((key.into(), view, present));
        self
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
                    for (key, view, present) in views {
                        let shown = view.shown(cx);
                        let present = present && shown;
                        let sample = Presence::new(
                            ElementId::NamedChild(std::sync::Arc::new(id.clone()), key),
                            present,
                        )
                        .transition(Transition::new(if present { ENTER } else { LEAVE }))
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

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui::{
        App, Bounds, Context, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
        Render, StatefulInteractiveElement as _, Styled as _, TestAppContext, Window, div, point,
        px, size,
    };

    use super::{ENTER, LEAVE, NativeFade, NativeView};
    use crate::test_frames::draw;

    /// A stand-in for a platform view: what the platform would be told.
    #[derive(Clone, Default)]
    struct Page {
        shown: Rc<Cell<bool>>,
        alpha: Rc<Cell<f32>>,
    }

    const PAGE: Bounds<Pixels> = Bounds {
        origin: point(px(20.), px(20.)),
        size: size(px(100.), px(100.)),
    };

    impl NativeView for Page {
        fn shown(&self, _: &App) -> bool {
            self.shown.get()
        }
        fn hide(&self, _: &mut App) {
            self.shown.set(false);
        }
        fn set_alpha(&self, alpha: f32, _: &App) {
            self.alpha.set(alpha);
        }
        fn bounds(&self, _: &App) -> Bounds<Pixels> {
            PAGE
        }
    }

    /// GPUI content under the page, and the page wanted or not.
    struct Owner {
        page: Page,
        wanted: bool,
        clicks: Rc<Cell<usize>>,
    }

    impl Render for Owner {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();
            div()
                .size_full()
                .child(
                    div()
                        .id("under")
                        .debug_selector(|| "under".into())
                        .size(px(140.))
                        .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                )
                .child(NativeFade::new("pages").view("page", self.page.clone(), self.wanted))
        }
    }

    /// The owner shows a page and it fades in. Unwanted, it stays on screen
    /// while it fades out, and what is under it takes no click until it has
    /// gone; then it is hidden and the click lands.
    #[gpui::test]
    fn a_native_view_fades_in_and_is_hidden_only_once_it_has_faded_out(cx: &mut TestAppContext) {
        let page = Page::default();
        let clicks = Rc::new(Cell::new(0));
        let (owner, cx) = cx.add_window_view(|_, _| Owner {
            page: page.clone(),
            wanted: false,
            clicks: clicks.clone(),
        });
        draw(cx);
        let over_page = PAGE.center();

        page.shown.set(true);
        owner.update(cx, |owner, cx| {
            owner.wanted = true;
            cx.notify();
        });
        draw(cx);
        assert_eq!(page.alpha.get(), 0., "a page shown starts transparent");
        cx.executor().advance_clock(ENTER / 2);
        draw(cx);
        let half = page.alpha.get();
        assert!(half > 0. && half < 1., "and fades in: {half}");
        cx.executor().advance_clock(ENTER);
        draw(cx);
        assert_eq!(page.alpha.get(), 1.);

        owner.update(cx, |owner, cx| {
            owner.wanted = false;
            cx.notify();
        });
        draw(cx);
        cx.executor().advance_clock(LEAVE / 2);
        draw(cx);
        let half = page.alpha.get();
        assert!(page.shown.get(), "a leaving page stays on screen");
        assert!(half > 0. && half < 1., "fading out: {half}");
        cx.simulate_click(over_page, Default::default());
        assert_eq!(clicks.get(), 0, "and is covered while it leaves");

        cx.executor().advance_clock(LEAVE);
        draw(cx);
        assert!(!page.shown.get(), "hidden once it has faded out");
        cx.simulate_click(over_page, Default::default());
        assert_eq!(clicks.get(), 1, "uncovered once hidden");
    }

    /// With motion reduced, an unwanted page hides on the frame it is let go.
    #[gpui::test]
    fn with_motion_reduced_a_native_view_hides_at_once(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let page = Page::default();
        page.shown.set(true);
        let (owner, cx) = cx.add_window_view(|_, _| Owner {
            page: page.clone(),
            wanted: true,
            clicks: Rc::default(),
        });
        draw(cx);
        assert_eq!(page.alpha.get(), 1.);
        owner.update(cx, |owner, cx| {
            owner.wanted = false;
            cx.notify();
        });
        draw(cx);
        assert!(!page.shown.get());
    }
}
