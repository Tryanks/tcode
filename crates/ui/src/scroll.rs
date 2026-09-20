//! Tcode-owned scroll-area compositions over gpui-base's scrollbar, scroll
//! mask and edge-bounce behavior.
//!
//! GPUI's own scroll containers apply every wheel or pan delta that reaches
//! them, so two nested viewports would both move on one gesture, and a
//! horizontal-only container maps vertical input onto its axis. The areas here
//! pair a viewport with a [`ScrollableMask`] sibling: the mask consumes the
//! events its viewport can use in the capture phase and lets the rest bubble
//! to the ancestor scroller.

use std::{panic::Location, rc::Rc};

use gpui::{
    App, Axis, Div, Element, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    ScrollHandle, Stateful, StatefulInteractiveElement, StyleRefinement, Styled, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_base::{
    InteractiveElementExt as _, ScrollBounce, ScrollableMask, Scrollbar, ScrollbarHandle,
    StyledExt as _,
};

use crate::wheel_easing::{self, Handle};

pub(crate) trait ScrollableElement:
    InteractiveElement + Styled + ParentElement + Element
{
    /// A vertical viewport with Tcode's overlay scrollbar. Nested inside
    /// another scroller it owns vertical input while it can move and chains to
    /// the ancestor at its edges.
    #[track_caller]
    fn overflow_y_scrollbar(self) -> Scrollable<Self> {
        Scrollable::new(self)
    }

    /// A bounded vertical viewport without a scrollbar, for menus, option
    /// lists and detail panes that can sit inside a page or the timeline.
    /// It owns vertical input while it can move and chains at its edges.
    #[track_caller]
    fn overflow_y_scroll_area(self) -> ScrollArea<Self> {
        ScrollArea::new(self, Axis::Vertical)
    }

    /// A horizontal strip (tables, key bars, segmented tracks). It owns
    /// horizontal input even at its edges, so a horizontal gesture never moves
    /// the page, and leaves vertical input to the ancestor scroller.
    #[track_caller]
    fn overflow_x_scroll_area(self) -> ScrollArea<Self> {
        ScrollArea::new(self, Axis::Horizontal)
    }
}

/// A page-level vertical viewport: `element` scrolls by its own handler
/// (`overflow_y_scroll` or a `list`) through `handle`, mouse-wheel notches
/// over it ease, and on touch platforms its edges stretch and bounce back.
/// Scrollbars and toolbars stay outside; never nest one inside a masked area.
/// The bounce is also enabled in tests, where GPUI simulates touch phases.
#[track_caller]
pub(crate) fn page_viewport<E: IntoElement>(
    id: impl Into<ElementId>,
    handle: Handle,
    element: E,
) -> ScrollBounce {
    let enabled = cfg!(any(target_os = "ios", target_os = "android", test));
    let registered = wheel_easing::register(element, handle.clone());
    match handle {
        Handle::Scroll(handle) => ScrollBounce::new(id, &handle, registered),
        Handle::List(list) => ScrollBounce::new(id, &list, registered),
    }
    .enabled(enabled)
}

#[derive(IntoElement)]
pub(crate) struct Scrollable<E: InteractiveElement + Styled + ParentElement + Element> {
    id: ElementId,
    element: E,
}

impl<E> Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    #[track_caller]
    fn new(element: E) -> Self {
        Self {
            id: caller_id(),
            element,
        }
    }
}

impl<E> Styled for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl<E> ParentElement for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.element.extend(elements);
    }
}

impl<E> InteractiveElement for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.element.interactivity()
    }
}

impl<E> RenderOnce for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element + 'static,
{
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let scroll_handle = window
            .use_keyed_state(self.id.clone(), cx, |_, _| ScrollHandle::default())
            .read(cx)
            .clone();
        let root_style = root_style_from(&mut self.element);
        let content = self
            .element
            .id((self.id.clone(), "content"))
            .flex_none()
            .h_auto()
            .min_h_full();
        let scroll_area = div()
            .id((self.id.clone(), "area"))
            .size_full()
            .flex()
            .flex_col()
            .track_scroll(&scroll_handle)
            .overflow_y_scroll()
            .lock_scroll_axis()
            .child(content);

        div()
            .id(self.id.clone())
            .size_full()
            .refine_style(&root_style)
            .relative()
            .child(wheel_easing::register(
                scroll_area,
                Handle::Scroll(scroll_handle.clone()),
            ))
            .child(
                ScrollableMask::new(Axis::Vertical, &scroll_handle).id((self.id.clone(), "mask")),
            )
            .child(ScrollbarLayer {
                id: (self.id, "scrollbar").into(),
                scroll_handle: Rc::new(scroll_handle),
            })
    }
}

impl ScrollableElement for Div {}
impl<E> ScrollableElement for Stateful<E>
where
    E: ParentElement + Styled + Element,
    Self: InteractiveElement,
{
}

/// A masked viewport: the element keeps its id, styles and children and
/// becomes the scrolled viewport; a wrapper carries its sizing in the parent
/// layout and hosts the mask sibling.
#[derive(IntoElement)]
pub(crate) struct ScrollArea<E: InteractiveElement + Styled + ParentElement + Element> {
    id: ElementId,
    axis: Axis,
    element: E,
}

impl<E> ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    #[track_caller]
    fn new(element: E, axis: Axis) -> Self {
        let fallback = caller_id();
        Self {
            id: Element::id(&element).unwrap_or(fallback),
            axis,
            element,
        }
    }
}

impl<E> Styled for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl<E> ParentElement for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.element.extend(elements);
    }
}

impl<E> InteractiveElement for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.element.interactivity()
    }
}

impl<E> RenderOnce for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element + 'static,
{
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let scroll_handle = window
            .use_keyed_state((self.id.clone(), "scroll"), cx, |_, _| {
                ScrollHandle::default()
            })
            .read(cx)
            .clone();
        let root_style = root_style_from(&mut self.element);
        // The wrapper takes the element's place in its parent's layout; the
        // viewport fills it unless a maximum height of its own bounds it.
        let bounded = self.element.style().max_size.height.is_some();
        let viewport = self
            .element
            .id(self.id.clone())
            .track_scroll(&scroll_handle);
        let viewport = match self.axis {
            // The mask moves the offset; the viewport only clips. GPUI's own
            // horizontal container would map vertical input onto this axis.
            Axis::Horizontal => viewport.overflow_hidden(),
            Axis::Vertical => viewport
                .when(!bounded, |viewport| viewport.max_h_full())
                .overflow_y_scroll()
                .lock_scroll_axis(),
        };
        let viewport = match self.axis {
            Axis::Horizontal => viewport.into_any_element(),
            Axis::Vertical => {
                wheel_easing::register(viewport, Handle::Scroll(scroll_handle.clone()))
                    .into_any_element()
            }
        };
        div()
            .relative()
            .refine_style(&root_style)
            .child(viewport)
            .child(ScrollableMask::new(self.axis, &scroll_handle).id((self.id, "mask")))
    }
}

#[derive(IntoElement)]
struct ScrollbarLayer<H: ScrollbarHandle + Clone> {
    id: ElementId,
    scroll_handle: Rc<H>,
}

impl<H> RenderOnce for ScrollbarLayer<H>
where
    H: ScrollbarHandle + Clone + 'static,
{
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        if window.is_inspector_picking(cx) {
            return div();
        }
        div().absolute().inset_0().child(
            Scrollbar::vertical(self.scroll_handle.as_ref())
                .id(self.id)
                .viewport_from_layout(),
        )
    }
}

#[track_caller]
fn caller_id() -> ElementId {
    ElementId::CodeLocation(*Location::caller())
}

fn root_style_from<E: Styled>(element: &mut E) -> StyleRefinement {
    let style = element.style();
    StyleRefinement {
        size: style.size.clone(),
        min_size: style.min_size.clone(),
        max_size: style.max_size.clone(),
        flex_grow: style.flex_grow,
        flex_shrink: style.flex_shrink,
        flex_basis: style.flex_basis,
        align_self: style.align_self,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, ListAlignment, ListOffset, ListState, PlatformInput, Render, TestAppContext,
        TouchEvent, TouchId, TouchPhase, VisualTestContext, list, point, px,
    };

    /// A timeline-shaped nesting: a horizontal strip and a bounded vertical
    /// area inside `gpui::list` rows.
    struct NestedAreas(ListState);

    impl Render for NestedAreas {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(200.)).h(px(300.)).child(
                list(self.0.clone(), |ix, _, _| match ix {
                    0 => div()
                        .id("strip")
                        .debug_selector(|| "strip".into())
                        .w_full()
                        .h(px(60.))
                        .overflow_x_scroll_area()
                        .child(
                            div()
                                .debug_selector(|| "strip-content".into())
                                .w(px(800.))
                                .h(px(60.))
                                .flex_none(),
                        )
                        .into_any_element(),
                    1 => div()
                        .id("inner")
                        .debug_selector(|| "inner".into())
                        .w_full()
                        .max_h(px(100.))
                        .overflow_y_scroll_area()
                        .child(
                            div()
                                .debug_selector(|| "inner-content".into())
                                .w_full()
                                .h(px(400.))
                                .flex_none(),
                        )
                        .into_any_element(),
                    _ => div().w_full().h(px(80.)).into_any_element(),
                })
                .w_full()
                .h_full(),
            )
        }
    }

    fn touch(cx: &mut VisualTestContext, phase: TouchPhase, x: f32, y: f32) {
        cx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::Touch(TouchEvent {
                    id: TouchId(1),
                    phase,
                    position: point(px(x), px(y)),
                    predicted_position: None,
                    force: None,
                }),
                cx,
            );
            let _ = window.draw(cx);
        });
    }

    /// One finger pan without release momentum.
    fn pan(cx: &mut VisualTestContext, from: (f32, f32), to: (f32, f32)) {
        touch(cx, TouchPhase::Started, from.0, from.1);
        touch(cx, TouchPhase::Moved, to.0, to.1);
        touch(cx, TouchPhase::Cancelled, to.0, to.1);
    }

    fn top(state: &ListState) -> (usize, gpui::Pixels) {
        let top = state.logical_scroll_top();
        (top.item_ix, top.offset_in_item)
    }

    #[gpui::test]
    fn strips_and_bounded_areas_inside_a_list_own_only_their_axis(cx: &mut TestAppContext) {
        let state = ListState::new(20, ListAlignment::Top, px(0.)).measure_all();
        let (_, cx) = cx.add_window_view({
            let state = state.clone();
            move |_, _| NestedAreas(state)
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let strip = cx.debug_bounds("strip").expect("strip viewport");
        let content = cx.debug_bounds("strip-content").expect("strip content");
        assert_eq!(strip.size.width, px(200.));

        // A horizontal pan over the strip moves the strip alone, and keeps
        // moving it at its edge rather than handing the rest to the list.
        pan(cx, (150., 30.), (30., 34.));
        let moved = cx.debug_bounds("strip-content").expect("strip content");
        assert!(moved.left() < content.left(), "strip scrolls sideways");
        assert_eq!(top(&state), (0, px(0.)));
        assert_eq!(cx.debug_bounds("strip").unwrap(), strip);
        pan(cx, (190., 30.), (10., 30.));
        pan(cx, (190., 30.), (10., 30.));
        pan(cx, (190., 30.), (10., 30.));
        pan(cx, (190., 30.), (10., 30.));
        assert_eq!(
            cx.debug_bounds("strip-content").unwrap().right(),
            strip.right(),
            "the strip clamps at its end"
        );
        assert_eq!(top(&state), (0, px(0.)));

        // A vertical pan over the strip scrolls the list.
        pan(cx, (100., 30.), (104., -100.));
        assert_ne!(top(&state), (0, px(0.)));
        state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        // A vertical pan over the bounded area moves it alone while it can
        // scroll, then chains to the list at its edge.
        let inner = cx.debug_bounds("inner").expect("inner viewport");
        let content = cx.debug_bounds("inner-content").expect("inner content");
        assert_eq!(inner.size.height, px(100.));
        let y = f32::from(inner.center().y);
        pan(cx, (100., y), (100., y - 50.));
        assert!(cx.debug_bounds("inner-content").unwrap().top() < content.top());
        assert_eq!(top(&state), (0, px(0.)));
        assert_eq!(cx.debug_bounds("inner").unwrap(), inner);
        pan(cx, (100., y), (100., y - 1000.));
        assert_eq!(top(&state), (0, px(0.)), "one gesture stays with the area");
        assert_eq!(
            cx.debug_bounds("inner-content").unwrap().bottom(),
            inner.bottom(),
            "the area clamps at its end"
        );
        pan(cx, (100., y), (100., y - 50.));
        assert_ne!(
            top(&state),
            (0, px(0.)),
            "the next gesture reaches the list"
        );
    }
}
