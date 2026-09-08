//! Shared material surfaces, radii and layout helpers.
//! Reading surfaces remain near-opaque over the translucent window canvas;
//! semantic colors come from the active theme.

use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::touch_scroll::TouchScrollExt as _;
use crate::widgets::Popover;
use crate::widgets::button::{Button, ButtonVariants as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, BoxShadow, Div, ElementId, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Rgba, Role, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, div,
    linear_color_stop, linear_gradient, px,
};
use gpui_base::{StyledExt as _, v_flex};

/// Height reserved beneath chat messages for hover-revealed actions.
pub(crate) const CHAT_ACTION_ROW_HEIGHT: f32 = 24.;
/// Maximum width of the shared chat/composer content column.
pub(crate) const CHAT_CONTENT_MAX_WIDTH: f32 = 720.;
/// Minimum horizontal padding around the shared chat/composer content column.
pub(crate) const CHAT_CONTENT_MIN_PADDING: f32 = 24.;
/// The compact layout's page inset: content is held this far clear of both
/// window edges. Cards inside that content inset a further [`CARD_INSET`].
pub(crate) const COMPACT_PAGE_INSET: f32 = 16.;
/// Padding inside a card, chip or notice that already sits within a page inset.
pub(crate) const CARD_INSET: f32 = 12.;
/// The smallest square a finger can reliably hit.
pub(crate) const TOUCH_TARGET: f32 = 44.;

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Hsla {
    Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: a as f32 / 255.,
    }
    .into()
}

/// Flatten the canvas over fullscreen vibrancy, where its translucent color
/// would otherwise composite against black.
pub fn opaque_canvas(cx: &App) -> Hsla {
    cx.theme().background.opacity(1.)
}

/// T1 paper: the near-opaque reading plane the chat workspace, right panel and
/// full-page routes paint over the vibrancy canvas. Warm paper in light mode,
/// blue-carbon in dark.
pub fn content_surface(cx: &App) -> Hsla {
    if cx.theme().mode.is_dark() {
        rgba(0x1B, 0x1E, 0x24, 0xF0)
    } else {
        rgba(0xFD, 0xFD, 0xFB, 0xF2)
    }
}

/// Popovers, menus, dialogs, toasts.
pub fn radius_overlay() -> Pixels {
    px(10.)
}
/// Cards, event cards, diff blocks.
pub fn radius_card() -> Pixels {
    px(10.)
}
/// Plain inputs and button-group containers.
pub fn radius_input() -> Pixels {
    px(8.)
}
/// Buttons.
pub fn radius_button() -> Pixels {
    px(8.)
}
/// Chips and compact status badges.
pub fn radius_chip() -> Pixels {
    px(6.)
}
/// Composer field corners.
pub fn radius_composer() -> Pixels {
    px(14.)
}
/// The phone's bottom sheet, top corners only.
pub fn radius_overlay_sheet() -> Pixels {
    px(14.)
}

/// A T3 overlay popover: one panel surface at the overlay radius with the
/// component library's large soft shadow.
pub fn overlay_popover(id: impl Into<ElementId>) -> Popover {
    Popover::new(id).rounded(radius_overlay()).shadow_xl()
}

/// A 1px separator that fades out toward both ends, replacing full-bleed
/// hairlines inside the paper plane.
pub fn faded_hairline(cx: &App) -> impl IntoElement {
    let color = cx.theme().border;
    let clear = color.opacity(0.);
    div()
        .w_full()
        .h(px(1.))
        .flex()
        .child(div().flex_1().h_full().bg(linear_gradient(
            90.,
            linear_color_stop(color, 1.),
            linear_color_stop(clear, 0.),
        )))
        .child(div().flex_1().h_full().bg(linear_gradient(
            90.,
            linear_color_stop(clear, 1.),
            linear_color_stop(color, 0.),
        )))
}

/// Applies the T3 overlay contour: fully opaque fill, hairline border and a
/// large soft shadow. Radius stays the caller's choice (`radius_overlay`).
pub fn overlay_contour(el: Div, cx: &App) -> Div {
    el.bg(cx.theme().popover)
        .border_1()
        .border_color(cx.theme().border)
        .shadow_xl()
}

/// One grouped-list container shared by settings surfaces.
pub fn group(cx: &App) -> Div {
    v_flex()
        .w_full()
        .rounded(radius_card())
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().popover)
        .shadow_md()
        .overflow_hidden()
}

/// Assemble rows into a floating group with inset hairlines between them.
pub fn grouped(rows: Vec<gpui::AnyElement>, cx: &App) -> Div {
    let mut group = group(cx);
    let last = rows.len().saturating_sub(1);
    for (index, row) in rows.into_iter().enumerate() {
        group = group.child(row);
        if index != last {
            group = group.child(
                div()
                    .w_full()
                    .pl_3()
                    .child(div().w_full().h(px(1.)).bg(cx.theme().border.opacity(0.6))),
            );
        }
    }
    group
}

/// One row of a navigable content list — a thread, a machine, a project. Plain
/// rows on the page, not a card: 56pt of touch target at the page inset, with
/// a hover fill on a pointer and a pressed tint everywhere.
///
/// Settings-like forms use [`grouped`] instead; see the list-style rule in
/// `docs/DESIGN.md`.
pub fn list_row(id: impl Into<ElementId>, label: SharedString, cx: &App) -> Stateful<Div> {
    accessible_clickable(gpui_base::h_flex(), id, Role::Button, label, cx)
        .w_full()
        .min_h(px(56.))
        .px(px(COMPACT_PAGE_INSET))
        .py(px(8.))
        .gap_3()
        .items_center()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().list_hover))
        .active(|style| style.bg(cx.theme().list_active))
}

/// The caption above one section of a navigable content list.
pub fn list_caption(label: SharedString, cx: &App) -> Div {
    div()
        .w_full()
        .px(px(COMPACT_PAGE_INSET))
        .pt(px(12.))
        .pb(px(4.))
        .text_size(px(13.))
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

/// Stack [`list_row`]s with a hairline between them, indented to the text.
pub fn plain_list(rows: Vec<gpui::AnyElement>, cx: &App) -> Div {
    let mut list = v_flex().w_full();
    let last = rows.len().saturating_sub(1);
    for (index, row) in rows.into_iter().enumerate() {
        list = list.child(row);
        if index != last {
            list = list.child(
                div()
                    .w_full()
                    .pl(px(COMPACT_PAGE_INSET))
                    .child(div().w_full().h(px(1.)).bg(cx.theme().border.opacity(0.6))),
            );
        }
    }
    list
}

/// Shared wordmark and DEV channel badge.
pub fn brand_wordmark(cx: &App) -> impl IntoElement {
    gpui_base::h_flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .text_size(px(14.))
                .font_bold()
                .text_color(cx.theme().sidebar_foreground)
                .child("tcode"),
        )
        .child(
            div()
                .px_1()
                .py(px(1.))
                .rounded_sm()
                .bg(cx.theme().muted)
                .text_color(cx.theme().muted_foreground)
                .text_size(px(9.))
                .font_semibold()
                .child("DEV"),
        )
}

/// The scrim behind a bottom sheet, at the `overlay` token's values
/// (`themes/tcode.json`: `#1F232852` light, `#00000080` dark). Passing the ink
/// `foreground` through in dark mode would *lighten* the page behind the sheet
/// instead of pushing it back, so the dark scrim is black.
pub fn scrim(progress: f32, cx: &App) -> Hsla {
    if cx.theme().mode.is_dark() {
        gpui::black().opacity(0.5 * progress)
    } else {
        cx.theme().foreground.opacity(0.32 * progress)
    }
}

/// Drag handle above a compact sheet's title row.
pub fn sheet_grabber(cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .w_full()
        .h(px(16.))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .w(px(36.))
                .h(px(5.))
                .rounded_full()
                .bg(cx.theme().muted_foreground.opacity(0.3)),
        )
}

/// Compact empty state; callers append their primary action.
pub fn empty_state(
    icon: crate::icon::Icon,
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    cx: &App,
) -> Div {
    v_flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .gap(px(10.))
        .p(px(24.))
        .child(icon.size(px(24.)).text_color(cx.theme().muted_foreground))
        .child(
            div()
                .max_w(px(280.))
                .text_center()
                .text_size(px(17.))
                .line_height(px(22.))
                .font_semibold()
                .child(title.into()),
        )
        .child(
            div()
                .max_w(px(280.))
                .text_center()
                .text_size(px(15.))
                .line_height(px(20.))
                .text_color(cx.theme().muted_foreground)
                .child(body.into()),
        )
}

/// Track for [`segment`] controls. Long labels scroll horizontally instead
/// of clipping; shorter groups divide the available width.
pub(crate) fn segmented_track(
    id: impl Into<ElementId>,
    cx: &App,
) -> crate::touch_scroll::Registered<Stateful<Div>> {
    gpui_base::h_flex()
        .id(id)
        .h(px(40.))
        .flex_none()
        .gap(px(2.))
        .p(px(3.))
        .rounded(px(10.))
        .bg(cx.theme().secondary)
        .touch_overflow_x_scroll()
}

/// One segment of a [`segmented_track`]. The selected segment is a T3 solid
/// with a hairline; the rest are bare. The caller attaches `on_click`.
pub fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    let label = label.into();
    // Grows into spare width, never shrinks below its label: the track scrolls
    // instead of ellipsizing (see `segmented_track`).
    accessible_clickable(div(), id, Role::Button, label.clone(), cx)
        .flex_grow(1.)
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .px(px(6.))
        .rounded(px(8.))
        .cursor_pointer()
        .text_size(px(13.))
        .when(selected, |el| {
            el.bg(cx.theme().popover)
                .border_1()
                .border_color(cx.theme().border)
                .font_medium()
        })
        .when(!selected, |el| el.text_color(cx.theme().muted_foreground))
        .child(div().flex_none().child(label))
}

/// A panel toolbar's icon control. Compact toolbars owe a finger a
/// [`TOUCH_TARGET`] square, so the icon is passed as a *child*: `Button` scales
/// whatever `icon()` receives from its own size, which would blow a 44pt button
/// up to a 33pt glyph. The desktop keeps its dense control.
pub fn toolbar_icon_button(
    id: impl Into<ElementId>,
    icon: crate::icon::IconName,
    tooltip: impl Into<SharedString>,
    compact: bool,
) -> Button {
    let tooltip = tooltip.into();
    if compact {
        Button::new(id)
            .ghost()
            .aria_label(tooltip.clone())
            .tooltip(tooltip)
            .with_size(px(TOUCH_TARGET))
            .size(px(TOUCH_TARGET))
            .child(crate::icon::Icon::new(icon).size(px(20.)))
    } else {
        Button::new(id)
            .ghost()
            .small()
            .compact()
            .icon(icon)
            .tooltip(tooltip)
    }
}

/// The indented body under a work-log row, a plan or a file edit: a hairline
/// rail 8pt in from the column, with its content another 14pt clear of it.
///
/// The 8pt is padding on a full-width wrapper, never a margin on the rail
/// itself: a `w_full` box with `ml_2` is 8pt *wider* than the column it sits in,
/// and on a compact page that is 8pt straight off the edge.
pub fn rail_detail(content: impl IntoElement, cx: &App) -> Div {
    div().w_full().min_w_0().pl_2().child(
        div()
            .w_full()
            .min_w_0()
            .pl(px(14.))
            .py_0p5()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(content),
    )
}

pub fn semantic_chip(label: impl Into<SharedString>, bg: Hsla, fg: Hsla) -> Div {
    div()
        .flex_none()
        .px_2()
        .py(px(1.))
        .rounded(radius_chip())
        .bg(bg)
        .text_size(px(11.))
        .font_medium()
        .text_color(fg)
        .child(label.into())
}

/// Uppercase section label with the design system's 0.08em tracking.
/// GPUI has no letter-spacing primitive, so the 10.5px label is split into
/// glyph elements with an exact 0.84px inter-glyph gap.
pub(crate) fn tracked_uppercase(text: &str) -> Div {
    gpui_base::h_flex().gap(px(0.84)).children(
        text.to_uppercase()
            .chars()
            .map(|character| div().child(character.to_string())),
    )
}

/// Gives a raw clickable surface the same keyboard and accessibility treatment
/// as the component-library controls. GPUI automatically maps Enter/Space to
/// `on_click` for a focused clickable div; this helper supplies the tab stop,
/// semantic role/name, and a keyboard-only outline that remains legible in
/// both themes without changing layout.
pub fn accessible_clickable(
    el: Div,
    id: impl Into<ElementId>,
    role: Role,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let ring = cx.theme().ring.opacity(if cx.theme().mode.is_dark() {
        0.72
    } else {
        0.58
    });

    el.tab_index(0)
        .focus_visible(|style| {
            style.shadow(vec![
                BoxShadow::new(px(0.), px(0.), ring).spread_radius(px(2.)),
            ])
        })
        .id(id)
        .role(role)
        .aria_label(label)
}

/// Neutral rows shared by unhydrated workspace and conversation views.
pub fn loading_skeleton(cx: &gpui::App) -> gpui::AnyElement {
    v_flex()
        .id("baseline-loading")
        .debug_selector(|| "baseline-loading".into())
        .flex_1()
        .px(px(COMPACT_PAGE_INSET))
        .pt(px(8.))
        .gap(px(4.))
        .children((0..3).map(|_| {
            v_flex()
                .h(px(56.))
                .justify_center()
                .gap(px(8.))
                .opacity(0.3)
                .child(
                    div()
                        .w(gpui::relative(0.7))
                        .h(px(14.))
                        .rounded(px(4.))
                        .bg(cx.theme().secondary),
                )
                .child(
                    div()
                        .w(gpui::relative(0.35))
                        .h(px(11.))
                        .rounded(px(4.))
                        .bg(cx.theme().secondary),
                )
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, KeyBinding, KeyUpEvent, Keystroke, Render, TestAppContext, VisualTestContext,
        Window,
    };
    use std::{cell::Cell, rc::Rc};

    gpui::actions!(accessible_controls_probe, [FocusNext, FocusPrevious]);

    struct AccessibleControlsProbe {
        activations: Rc<Cell<[usize; 2]>>,
    }

    impl AccessibleControlsProbe {
        fn new(activations: Rc<Cell<[usize; 2]>>, _cx: &mut Context<Self>) -> Self {
            Self { activations }
        }
    }

    impl Render for AccessibleControlsProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let first_activations = self.activations.clone();
            let second_activations = self.activations.clone();

            div()
                .key_context("AccessibleControlsProbe")
                .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
                .on_action(|_: &FocusPrevious, window, cx| window.focus_prev(cx))
                .child(
                    accessible_clickable(div().size(px(24.)), "first", Role::Button, "First", cx)
                        .on_click(move |_, _, _| {
                            let mut counts = first_activations.get();
                            counts[0] += 1;
                            first_activations.set(counts);
                        }),
                )
                .child(
                    accessible_clickable(div().size(px(24.)), "second", Role::Switch, "Second", cx)
                        .on_click(move |_, _, _| {
                            let mut counts = second_activations.get();
                            counts[1] += 1;
                            second_activations.set(counts);
                        }),
                )
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    fn activate(cx: &mut VisualTestContext, key: &str) {
        cx.simulate_keystrokes(key);
        cx.simulate_event(KeyUpEvent {
            keystroke: Keystroke::parse(key).expect("valid activation key"),
        });
    }

    #[gpui::test]
    fn raw_controls_follow_root_tab_order_and_activate_from_the_keyboard(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            cx.bind_keys([
                KeyBinding::new("tab", FocusNext, Some("AccessibleControlsProbe")),
                KeyBinding::new("shift-tab", FocusPrevious, Some("AccessibleControlsProbe")),
            ]);
        });
        let activations = Rc::new(Cell::new([0, 0]));
        let probe_activations = activations.clone();
        let (_probe, cx) = cx.add_window_view(move |_, cx| {
            AccessibleControlsProbe::new(probe_activations.clone(), cx)
        });
        let cx: &mut VisualTestContext = cx;
        draw(cx);

        // A dependency's `Root` cannot be instantiated with GPUI's macOS mock
        // window, so bootstrap the first focus exactly as Root's Tab action
        // does, then exercise real Tab/Shift-Tab key dispatch from there.
        cx.update(|window, cx| window.focus_next(cx));
        draw(cx);
        activate(cx, "enter");
        assert_eq!(activations.get(), [1, 0]);

        cx.simulate_keystrokes("tab");
        draw(cx);
        activate(cx, "space");
        assert_eq!(activations.get(), [1, 1]);

        cx.simulate_keystrokes("shift-tab");
        draw(cx);
        activate(cx, "enter");
        assert_eq!(activations.get(), [2, 1]);
    }
}
