//! Tcode-owned scroll-area styling over gpui-base's scrollbar behavior.

use std::{panic::Location, rc::Rc};

use gpui::{
    App, Div, Element, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    ScrollHandle, Stateful, StatefulInteractiveElement, StyleRefinement, Styled, Window, div,
};
use gpui_base::{
    InteractiveElementExt as _, Scrollbar, ScrollbarAxis, ScrollbarHandle, StyledExt as _,
};

pub(crate) trait ScrollableElement:
    InteractiveElement + Styled + ParentElement + Element
{
    #[track_caller]
    fn overflow_y_scrollbar(self) -> Scrollable<Self> {
        Scrollable::new(self, ScrollbarAxis::Vertical)
    }
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
    fn new(element: E, _axis: ScrollbarAxis) -> Self {
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
            .child(scroll_area)
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
