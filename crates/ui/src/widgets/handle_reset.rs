//! A resize handle's double click, which puts what the handle resizes back
//! at its default size.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Axis, ClickEvent, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Styled as _, Window, div, px,
};
use gpui_base::{
    HandleEdge, InteractiveElementExt as _, ResizeHandleContext, ResizeHandleRenderer,
};

/// What a double click on a handle does.
pub(crate) type Reset = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// The appearance of a resizable group's handles that calls `on_reset` when
/// one is double clicked; see [`line`].
pub(crate) fn appearance(
    on_reset: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> ResizeHandleRenderer {
    let on_reset: Reset = Rc::new(on_reset);
    Rc::new(move |handle, _, cx| Some(line(handle, on_reset.clone(), cx)))
}

/// What `handle` paints: gpui-base's own line, in a layer over the handle's
/// whole band that hears a double click.
///
/// gpui-base's `resize_handle` takes listeners through the interactive
/// element traits, but a group (`h_resizable`, `v_resizable`, the dock's
/// splits) builds its handles itself and hands on only their appearance
/// (0.10). The listener comes in with the appearance, which replaces base's
/// line and so draws it again: one pixel on the seam, in the colours base
/// resolves for a handle at rest and for one the pointer holds.
pub(crate) fn line(handle: &ResizeHandleContext, on_reset: Reset, cx: &App) -> AnyElement {
    let theme = cx.global::<gpui_base::Theme>();
    let colour = if handle.is_active() {
        theme
            .resizable
            .active_handle
            .unwrap_or(theme.tokens.colors.ring)
    } else {
        theme.resizable.handle.unwrap_or(theme.tokens.colors.border)
    };
    let line = match handle.axis() {
        Axis::Horizontal => div().h_full().w(px(1.)),
        Axis::Vertical => div().w_full().h(px(1.)),
    };
    let band = match handle.axis() {
        Axis::Horizontal => div().flex(),
        Axis::Vertical => div().flex().flex_col(),
    };
    // The line is on the seam: in the middle of a band that straddles it, at
    // the outer edge of one that hugs its container's.
    match handle.edge() {
        None => band.justify_center(),
        Some(HandleEdge::Leading) => band.justify_start(),
        Some(HandleEdge::Trailing) => band.justify_end(),
    }
    .id("handle-reset")
    .absolute()
    .inset_0()
    .on_double_click(move |event, window, cx| on_reset(event, window, cx))
    .child(line.flex_none().bg(colour))
    .into_any_element()
}
