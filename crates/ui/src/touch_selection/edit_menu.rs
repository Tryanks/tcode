//! The edit menu of a touch selection, adapted from gpui-component's
//! Apache-2.0 `touch_selection/edit_menu.rs`.

use std::rc::Rc;

#[cfg(target_os = "ios")]
use gpui::canvas;
use gpui::{
    App, Bounds, ClickEvent, Corners, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, RenderOnce, SharedString, Styled as _, Window, deferred, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{Placement, Positioner, h_flex};

use crate::{
    material,
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    widgets::button::{Button, ButtonVariants as _},
};

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a surface drawn here takes to appear, and to leave.
const APPEAR: std::time::Duration = std::time::Duration::from_millis(200);

/// What a command does when its row is pressed.
type ItemHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// One command in the edit menu.
#[derive(Clone)]
pub(crate) struct EditMenuItem {
    label: SharedString,
    /// Names the row for a test, independent of the locale.
    selector: &'static str,
    on_click: ItemHandler,
    #[cfg(target_os = "ios")]
    native_paste: bool,
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
            #[cfg(target_os = "ios")]
            native_paste: false,
        }
    }
    pub(crate) fn native_paste(self) -> Self {
        #[cfg(target_os = "ios")]
        {
            Self {
                native_paste: true,
                ..self
            }
        }
        #[cfg(not(target_os = "ios"))]
        {
            self
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
    open: bool,
}

impl EditMenu {
    pub(crate) fn new(id: impl Into<ElementId>, anchor: Bounds<Pixels>) -> Self {
        Self {
            id: id.into(),
            anchor,
            items: Vec::new(),
            open: true,
        }
    }

    /// Whether the menu shows: it fades and rises in as it opens, and fades
    /// out, taking no presses, as it closes.
    pub(crate) fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    pub(crate) fn items(mut self, items: impl IntoIterator<Item = EditMenuItem>) -> Self {
        self.items.extend(items);
        self
    }
}

/// The gap between the selection's handles and the menu.
const GAP: Pixels = px(8.);

/// The menu's height: one finger-sized row inside the contour's hairline.
fn menu_height() -> Pixels {
    px(material::TOUCH_TARGET) + px(2.)
}

/// The side of the selection the menu takes: above it when it fits between
/// the selection and the top of what the window shows, below it when it fits
/// there instead, otherwise whichever side has more room.
///
/// [`Positioner`] makes the same choice against the whole viewport, which on
/// a phone runs under the status bar and the software keyboard; gpui-base has
/// no way to give it the window's visible bounds instead. Choosing the side
/// here, against [`Window::fully_visible_bounds`], keeps the menu out from
/// under both; the positioner still aligns and clamps it.
fn side(anchor: Bounds<Pixels>, visible: Bounds<Pixels>) -> Placement {
    let above = anchor.top() - GAP - visible.top();
    let below = visible.bottom() - (anchor.bottom() + GAP);
    if menu_height() <= above {
        Placement::Top
    } else if menu_height() <= below || below > above {
        Placement::Bottom
    } else {
        Placement::Top
    }
}

impl RenderOnce for EditMenu {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id = self.id;
        let presence =
            crate::motion::appear((id.clone(), "presence"), self.open, APPEAR, window, cx);
        if !presence.should_render() {
            return gpui::Empty.into_any_element();
        }
        let progress = presence.progress;
        // A T3 pill, as iOS draws its edit menu: the overlay contour at the
        // overlay radius, its items filling it edge to edge.
        let radius = material::radius_overlay(cx);
        // Inside the hairline border, so a pressed end item's fill follows
        // the pill's corner.
        let inner_radius = radius - px(1.);
        let hairline = cx.theme().border;
        #[cfg(target_os = "ios")]
        let (foreground, background) = (cx.theme().foreground, cx.theme().popover);
        let last = self.items.len().saturating_sub(1);
        // An item that comes or goes (Select All, once used) narrows away and
        // its neighbours close up, the pill with them.
        let items = self
            .items
            .into_iter()
            .enumerate()
            .map(|(ix, item)| (SharedString::from(item.selector), (ix, item)))
            .collect::<Vec<_>>();
        let items = crate::motion::keyed_across(
            (id.clone(), "items"),
            items,
            ROWS,
            window,
            cx,
            move |_, (ix, item), _, _| {
                let (ix, item) = (*ix, item.clone());
                let on_click = item.on_click;
                #[cfg(target_os = "ios")]
                let native_paste = item.native_paste;
                #[cfg(not(target_os = "ios"))]
                let native_paste = false;
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
                // This is a finger's menu wherever it shows: a touch-target row.
                // Its text is 17pt because UIKit draws its own Paste item at that
                // size and takes no font; no typography token is 17. Pressing an
                // item leaves focus on the text it acts on: an input keeps its
                // keyboard, and a message keeps answering the menu's actions.
                let button = Button::new(selector)
                    .ghost()
                    .with_size(px(material::TOUCH_TARGET))
                    .touch(true)
                    .text_size(px(17.))
                    .rounded_corners(corners)
                    .tab_stop(false)
                    .focusable(false)
                    .debug_selector(move || format!("edit-menu-{selector}"))
                    .on_click(move |_: &ClickEvent, window, cx| on_click(window, cx));
                let button = if native_paste {
                    // UIKit's control draws this item's label; the cell keeps its
                    // own unseen, for its width.
                    button.aria_label(item.label.clone()).child(
                        div()
                            .text_color(gpui::transparent_black())
                            .child(item.label),
                    )
                } else {
                    button.label(item.label)
                };
                let button = button.into_any_element();
                #[cfg(target_os = "ios")]
                let button = if native_paste {
                    // UIKit only trusts a paste its own UIPasteControl starts, and
                    // the control draws its own label. It is laid over this cell
                    // in the cell's colours, with the bar's inner corners — the
                    // cell shows the same fill under a middle item's rounding —
                    // so it reads as one more item. Its bounds are read from a
                    // wrapper the size of the cell: Button's content area
                    // excludes its padding.
                    div()
                        .relative()
                        .flex()
                        .child(button)
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, _, _, _| {
                                    gpui_ios::set_paste_control(Some(gpui_ios::PasteControl {
                                        bounds,
                                        foreground,
                                        background,
                                        corner_radius: inner_radius,
                                    }))
                                },
                            )
                            .absolute()
                            .inset_0(),
                        )
                        .into_any_element()
                } else {
                    button
                };
                // A rule between neighbours, none before the first, the full
                // height of the bar; each item carries the one before it.
                h_flex()
                    .h_full()
                    .items_stretch()
                    .when(ix > 0, |item| {
                        item.child(div().flex_shrink_0().w(px(1.)).bg(hairline))
                    })
                    .child(button)
                    .into_any_element()
            },
        );
        deferred(
            Positioner::side(self.anchor)
                .placement(side(self.anchor, window.fully_visible_bounds()))
                .offset(GAP)
                .occlude()
                .child(
                    div()
                        .relative()
                        .opacity(progress)
                        .top(crate::motion::lift(progress))
                        .child(
                            items.render(
                                material::overlay_contour(
                                    h_flex().relative().items_stretch().overflow_hidden(),
                                    cx,
                                )
                                .rounded(radius)
                                .debug_selector(|| "edit-menu".into()),
                            ),
                        )
                        .when(!self.open, |menu| menu.child(crate::motion::shield())),
                ),
        )
        .with_priority(gpui_base::POPUP_PRIORITY)
        .into_any_element()
    }
}
