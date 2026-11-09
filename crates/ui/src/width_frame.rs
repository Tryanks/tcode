//! A view laid out at the width it is given this frame, handed that width
//! before it is laid out.
//!
//! gpui-fast lays a container query's content out as a layout root of its
//! own, and draws no view around such a root again from last frame
//! (`fast/splice.rs`, "layout does not reach"): a query inside a large view
//! builds all of it again for any change nested in it. Here the query is the
//! whole of a view of its own, around the view it sizes, which reads the
//! width it was handed rather than querying it, so a change nested in that
//! view is built again alone and the view is drawn from last frame around it.

use gpui::{
    AppContext as _, Context, Entity, IntoElement, Pixels, Render, Styled as _, WeakEntity, Window,
    div,
};

pub(crate) struct WidthFrame<V> {
    view: WeakEntity<V>,
    /// The width last handed to the view.
    width: Option<Pixels>,
    hand: fn(&mut V, Pixels),
}

impl<V: Render> WidthFrame<V> {
    /// A frame for `view`, which `hand` tells the width it is laid out at.
    pub(crate) fn new(
        view: WeakEntity<V>,
        hand: fn(&mut V, Pixels),
        cx: &mut gpui::App,
    ) -> Entity<Self> {
        cx.new(|_| Self {
            view,
            width: None,
            hand,
        })
    }
}

impl<V: Render> Render for WidthFrame<V> {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (frame, view, hand) = (cx.weak_entity(), self.view.clone(), self.hand);
        gpui::container_query(move |size, _, cx| {
            let Some(view) = view.upgrade() else {
                return div().size_full().into_any_element();
            };
            let handed = frame
                .update(cx, |frame, _| frame.width.replace(size.width))
                .ok()
                .flatten();
            // A write while the window draws: the view is built again for
            // it, this frame, before it is laid out.
            if handed != Some(size.width) {
                view.update(cx, |view, _| hand(view, size.width));
            }
            view.into_any_element()
        })
    }
}
