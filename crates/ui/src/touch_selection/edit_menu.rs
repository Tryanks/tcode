//! The edit menu of a touch selection, adapted from gpui-component's
//! Apache-2.0 `touch_selection/edit_menu.rs`.

use std::rc::Rc;

use gpui::{
    App, Bounds, ClickEvent, Corners, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, RenderOnce, SharedString, Styled as _, Window, canvas, deferred,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{Placement, Positioner, h_flex};

use super::handle::SurfaceHandler;
use crate::{
    material,
    theme::ActiveTheme as _,
    widgets::button::{Button, ButtonVariants as _},
};

/// What a command does when its row is pressed.
type ItemHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One command in the edit menu.
pub(crate) struct EditMenuItem {
    label: SharedString,
    /// Names the row for a test, independent of the locale.
    selector: &'static str,
    on_click: ItemHandler,
}

impl EditMenuItem {
    pub(crate) fn new(
        selector: &'static str,
        label: impl Into<SharedString>,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            selector,
            on_click: Rc::new(on_click),
        }
    }
}

/// The row of commands a touch selection offers: Cut, Copy, Paste, Select All
/// — whichever apply. It floats above the selection, or below it when there
/// is no room above, and stays out of the way of the handles' knobs.
///
/// Every item is a [`Button`] sized for a finger, so the row keeps the button
/// family's press feedback. It carries no arrow: it belongs to the selection
/// it sits on, not to a trigger.
#[derive(IntoElement)]
pub(crate) struct EditMenu {
    id: ElementId,
    /// The selection, including the room its handles take.
    anchor: Bounds<Pixels>,
    items: Vec<EditMenuItem>,
    on_paint: Option<SurfaceHandler>,
}

impl EditMenu {
    pub(crate) fn new(id: impl Into<ElementId>, anchor: Bounds<Pixels>) -> Self {
        Self {
            id: id.into(),
            anchor,
            items: Vec::new(),
            on_paint: None,
        }
    }

    pub(crate) fn items(mut self, items: impl IntoIterator<Item = EditMenuItem>) -> Self {
        self.items.extend(items);
        self
    }

    pub(crate) fn on_paint(mut self, on_paint: SurfaceHandler) -> Self {
        self.on_paint = Some(on_paint);
        self
    }
}

impl RenderOnce for EditMenu {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id;
        let on_paint = self.on_paint;
        // A T3 pill, as iOS draws its edit menu: the overlay contour at the
        // overlay radius, its items filling it edge to edge.
        let radius = material::radius_overlay();
        // Inside the hairline border, so a pressed end item's fill follows
        // the pill's corner.
        let inner_radius = radius - px(1.);
        let hairline = cx.theme().border;
        let last = self.items.len().saturating_sub(1);
        let items = self.items.into_iter().enumerate().flat_map(|(ix, item)| {
            let on_click = item.on_click;
            // Each item's press surface takes the bar's own corners: the
            // first the left pair, the last the right pair, the ones between
            // none.
            let corner = |on: bool| if on { inner_radius } else { px(0.) };
            let corners = Corners {
                top_left: corner(ix == 0),
                bottom_left: corner(ix == 0),
                top_right: corner(ix == last),
                bottom_right: corner(ix == last),
            };
            let selector = item.selector;
            // This is a finger's menu wherever it shows: the compact pill's
            // 15px text in the button family's 32px medium row. Pressing an
            // item leaves focus on the text it acts on: an input keeps its
            // keyboard, and a message keeps answering the menu's actions.
            let button = Button::new(ix)
                .ghost()
                .text_size(px(15.))
                .rounded_corners(corners)
                .tab_stop(false)
                .focusable(false)
                .debug_selector(move || format!("edit-menu-{selector}"))
                .label(item.label)
                .on_click(move |_: &ClickEvent, window, cx| on_click(window, cx))
                .into_any_element();
            // A rule between neighbours, none before the first, the full
            // height of the bar.
            (ix > 0)
                .then(|| {
                    div()
                        .flex_shrink_0()
                        .w(px(1.))
                        .bg(hairline)
                        .into_any_element()
                })
                .into_iter()
                .chain([button])
        });
        deferred(
            Positioner::side(self.anchor)
                .placement(Placement::Top)
                .offset(px(8.))
                .occlude()
                .child(
                    material::overlay_contour(
                        h_flex().relative().items_stretch().overflow_hidden(),
                        cx,
                    )
                    .rounded(radius)
                    .id(id)
                    .debug_selector(|| "edit-menu".into())
                    .children(items)
                    .when_some(on_paint, |this, on_paint| {
                        this.child(
                            canvas(
                                move |bounds, window, cx| on_paint(bounds, window, cx),
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .inset_0(),
                        )
                    }),
                ),
        )
        .with_priority(gpui_base::POPUP_PRIORITY)
    }
}
