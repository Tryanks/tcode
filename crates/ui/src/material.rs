//! Shared material surfaces, radii and layout helpers.
//! Reading surfaces remain near-opaque over the translucent window canvas;
//! semantic colors come from the active theme.

use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::Popover;
use crate::widgets::button::{Button, ButtonVariants as _};
use gpui::{
    App, BoxShadow, Div, ElementId, Hsla, InteractiveElement, IntoElement, ParentElement as _,
    Pixels, Rgba, Role, SharedString, Stateful, StatefulInteractiveElement as _, Styled, div,
    linear_color_stop, linear_gradient, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, Toolbar, h_flex, v_flex};

/// Height reserved beneath chat messages for hover-revealed actions.
pub(crate) const CHAT_ACTION_ROW_HEIGHT: f32 = 24.;
/// Maximum width of the shared chat/composer content column.
pub(crate) const CHAT_CONTENT_MAX_WIDTH: f32 = 720.;
/// Minimum horizontal padding around the shared chat/composer content column.
pub(crate) const CHAT_CONTENT_MIN_PADDING: f32 = 24.;
/// The compact layout's page inset: content is held this far clear of both
/// window edges. Cards inside that content inset a further [`CARD_INSET`].
pub(crate) const COMPACT_PAGE_INSET: f32 = 16.;
/// The touch-target height of a [`list_row`]. gpui-base carries no
/// control-height token, so row heights are Tcode's.
pub(crate) const LIST_ROW_MIN_HEIGHT: f32 = 56.;
/// Padding inside a card, chip or notice that already sits within a page inset.
pub(crate) const CARD_INSET: f32 = 12.;
/// The smallest square a finger can reliably hit. gpui-base carries no
/// control-height token (its `TouchHandle::HIT_SIZE` is text selection's own).
pub(crate) const TOUCH_TARGET: f32 = 44.;
/// Width of numeric form fields, including their step controls.
pub(crate) const NUMBER_INPUT_WIDTH: f32 = 96.;

/// The height of a text field. gpui-base carries no control-height token.
pub(crate) fn input_height(compact: bool) -> f32 {
    if compact { TOUCH_TARGET } else { 32. }
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Hsla {
    Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: a as f32 / 255.,
    }
    .into()
}

/// T0 canvas: the theme's translucent tint over the native window material.
/// Transparent when the macOS system material already tints the backdrop
/// (`macos_backdrop`), so the two tints do not stack.
pub fn canvas(cx: &App) -> Hsla {
    #[cfg(target_os = "macos")]
    if crate::macos_backdrop::installed(cx) {
        return gpui::transparent_black();
    }
    cx.theme().background
}

/// Flatten the canvas over fullscreen vibrancy, where its translucent color
/// would otherwise composite against black.
pub fn opaque_canvas(cx: &App) -> Hsla {
    Hsla {
        a: 1.,
        ..cx.theme().background
    }
}

/// T1 paper: the near-opaque reading plane the chat workspace, right panel and
/// full-page routes paint over the vibrancy canvas. Warm paper in light mode,
/// blue-carbon in dark.
pub fn content_surface(cx: &App) -> Hsla {
    paper(cx.theme())
}

fn paper(theme: &crate::theme::Theme) -> Hsla {
    theme.by_mode(rgba(0xFD, 0xFD, 0xFB, 0xF2), rgba(0x1B, 0x1E, 0x24, 0xF0))
}

/// Popovers, menus, dialogs, toasts: the theme's large-element radius.
pub fn radius_overlay(cx: &App) -> Pixels {
    cx.theme().tokens.radius.lg
}
/// Cards, event cards, diff blocks.
pub fn radius_card(cx: &App) -> Pixels {
    cx.theme().tokens.radius.md
}
/// Plain inputs and button-group containers.
pub fn radius_input(cx: &App) -> Pixels {
    cx.theme().tokens.radius.md
}
/// Buttons.
pub fn radius_button(cx: &App) -> Pixels {
    cx.theme().tokens.radius.md
}
/// Chips and compact status badges.
pub fn radius_chip(cx: &App) -> Pixels {
    cx.theme().tokens.radius.sm
}
/// Composer field corners.
pub fn radius_composer(cx: &App) -> Pixels {
    cx.theme().tokens.radius.lg
}
/// The phone's bottom sheet, top corners only.
pub fn radius_overlay_sheet(cx: &App) -> Pixels {
    cx.theme().tokens.radius.xl
}

/// A T3 overlay popover: one panel surface at the overlay radius with the
/// component library's large soft shadow.
pub fn overlay_popover(id: impl Into<ElementId>, cx: &App) -> Popover {
    Popover::new(id).rounded(radius_overlay(cx)).shadow_xl()
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

/// Height of the drag handle strip above a compact sheet's title row.
pub const SHEET_GRABBER_HEIGHT: f32 = 16.;
/// Height of a compact sheet's title row.
pub const SHEET_TITLE_HEIGHT: f32 = 48.;

/// The compact bottom sheet's surface: top corners at the sheet radius, opaque
/// fill, a hairline along its top edge and the large soft shadow.
pub fn sheet_contour<E: Styled>(el: E, cx: &App) -> E {
    el.rounded_t(radius_overlay_sheet(cx))
        .bg(cx.theme().popover)
        .border_t_1()
        .border_color(cx.theme().border)
        .shadow_xl()
}

/// The window edge a [`floating_panel`] is docked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// A side panel floating over the content: opaque fill, a medium shadow and a
/// hairline on the edge that faces the content.
pub fn floating_panel<E: Styled>(el: E, side: Side, cx: &App) -> E {
    let el = el.bg(cx.theme().popover).shadow_lg();
    match side {
        Side::Left => el.border_r_1(),
        Side::Right => el.border_l_1(),
    }
    .border_color(cx.theme().border)
}

/// A card inside a page or a chat column: its radius, inset, gap and the muted
/// wash. [`card_secondary`] and [`card_raised`] change only the fill.
pub fn card(cx: &App) -> Div {
    let spacing = &cx.theme().tokens.spacing;
    v_flex()
        .rounded(radius_card(cx))
        .px(px(CARD_INSET))
        .py(spacing.sm)
        .gap(spacing.sm)
        .bg(cx.theme().muted)
}

/// A [`card`] on the secondary wash.
pub fn card_secondary(cx: &App) -> Div {
    card(cx).bg(cx.theme().secondary)
}

/// A [`card`] lifted off the page: opaque fill, hairline border, small shadow.
pub fn card_raised(cx: &App) -> Div {
    card(cx)
        .bg(cx.theme().popover)
        .border_1()
        .border_color(cx.theme().border)
        .shadow_sm()
}

/// A dense header strip across the top of a [`card`]: a desktop section bar's
/// height at the card inset. Callers add the content.
pub fn card_header_row(cx: &App) -> Div {
    h_flex()
        .w_full()
        .h(px(bar_height(Bar::Section, false)))
        .px(px(CARD_INSET))
        .gap(cx.theme().tokens.spacing.sm)
        .items_center()
}

/// A banner washed in one semantic `tone` (danger, warning, info). The 10pt
/// vertical inset has no gpui-base spacing token (it has 8 and 12).
pub fn notice(tone: Hsla, cx: &App) -> Div {
    h_flex()
        .w_full()
        .rounded(radius_card(cx))
        .px(px(CARD_INSET))
        .py(px(10.))
        .gap(cx.theme().tokens.spacing.sm)
        .bg(tone.opacity(0.12))
        .border_1()
        .border_color(tone.opacity(0.22))
}

/// The one-line "nothing here" text inside a list or card; a whole empty page
/// uses [`empty_state`].
pub fn empty_row(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .w_full()
        .px(px(CARD_INSET))
        .py(cx.theme().tokens.spacing.lg)
        .text_secondary(false, cx)
        .child(text.into())
}

/// The vertical space between two sections of a page or a dialog.
pub fn section_gap(cx: &App) -> Pixels {
    cx.theme().tokens.spacing.xl
}

/// One section of a form: an optional caption over its content.
pub fn section(label: Option<SharedString>, cx: &App) -> Div {
    v_flex()
        .w_full()
        .gap(cx.theme().tokens.spacing.md)
        .when_some(label, |section, label| {
            section.child(div().text_caption(cx).child(label))
        })
}

/// The hairline between two rows of a list or group, `indent` in from its
/// leading edge so it starts under the row's text.
pub fn row_hairline(indent: Pixels, cx: &App) -> Div {
    div()
        .w_full()
        .pl(indent)
        .child(div().w_full().h(px(1.)).bg(cx.theme().border.opacity(0.6)))
}

/// One grouped-list container shared by settings surfaces.
pub fn group(cx: &App) -> Div {
    v_flex()
        .w_full()
        .rounded(radius_card(cx))
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().popover)
        .shadow_md()
        .overflow_hidden()
}

/// Assemble rows into a floating group with inset hairlines between them.
/// Rows carry their own inset ([`group_row`] for the common one).
pub fn grouped(rows: Vec<gpui::AnyElement>, cx: &App) -> Div {
    let mut group = group(cx);
    let last = rows.len().saturating_sub(1);
    for (index, row) in rows.into_iter().enumerate() {
        group = group.child(row);
        if index != last {
            group = group.child(row_hairline(px(CARD_INSET), cx));
        }
    }
    group
}

/// One row inside a [`group`]: label leading, control trailing, at the card
/// inset. The 10pt vertical inset has no gpui-base spacing token (it has 8
/// and 12).
pub fn group_row(cx: &App) -> Div {
    h_flex()
        .w_full()
        .min_h(px(TOUCH_TARGET))
        .px(px(CARD_INSET))
        .py(px(10.))
        .gap(cx.theme().tokens.spacing.md)
        .items_center()
}

/// A [`group_row`] for a compact window, where a label column beside a control
/// has no room: the label sits above a full-width control.
pub fn group_row_stacked(cx: &App) -> Div {
    v_flex()
        .w_full()
        .min_h(px(TOUCH_TARGET))
        .px(px(CARD_INSET))
        .py(px(10.))
        .gap(cx.theme().tokens.spacing.sm)
        .items_start()
}

/// The caption above a [`group`], aligned with its rows' text.
pub fn group_caption(label: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .pl(px(CARD_INSET))
        .pb(cx.theme().tokens.spacing.sm)
        .text_caption(cx)
        .child(label.into())
}

/// One row of a navigable content list — a thread, a machine, a project. Plain
/// rows on the page, not a card: 56pt of touch target at the page inset, with
/// a hover fill on a pointer and a pressed tint everywhere.
///
/// Settings-like forms use [`grouped`] instead: plain rows for navigable
/// content, grouped cards for settings-like forms.
pub fn list_row(id: impl Into<ElementId>, label: SharedString, cx: &App) -> Stateful<Div> {
    accessible_clickable(gpui_base::h_flex(), id, Role::Button, label, cx)
        .w_full()
        .min_h(px(LIST_ROW_MIN_HEIGHT))
        .px(px(COMPACT_PAGE_INSET))
        .py(px(8.))
        .gap_3()
        .items_center()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().list_hover))
        .active(|style| style.bg(cx.theme().list_active))
}

/// One row of a side panel's list: on a compact layout a [`list_row`]; wide, a
/// two-line 52pt row at the card inset with a rounded hover fill. Callers add
/// the content and the click.
pub fn panel_row(
    id: impl Into<ElementId>,
    label: SharedString,
    compact: bool,
    cx: &App,
) -> Stateful<Div> {
    if compact {
        return list_row(id, label, cx);
    }
    let spacing = &cx.theme().tokens.spacing;
    accessible_clickable(h_flex(), id, Role::Button, label, cx)
        .w_full()
        .min_h(px(52.))
        .px(px(CARD_INSET))
        .py(spacing.sm)
        .gap(spacing.sm)
        .items_center()
        .rounded(radius_card(cx))
        .hover(|style| style.bg(cx.theme().list_hover))
}

/// What fills a [`menu_item_frame`] beneath its content.
#[derive(Clone, Copy, PartialEq)]
pub enum ItemFill {
    /// Nothing: an item that cannot be chosen.
    Inert,
    /// `muted` under the pointer.
    Hover,
    /// The fill of the item the keyboard is on, and `muted` under the
    /// pointer.
    Highlight(Hsla),
}

impl ItemFill {
    /// [`ItemFill::Highlight`] with `fill` while `on`, [`ItemFill::Hover`]
    /// otherwise.
    pub fn highlight(on: bool, fill: Hsla) -> Self {
        if on {
            Self::Highlight(fill)
        } else {
            Self::Hover
        }
    }
}

/// One item of a menu or a menu-like picker: its inset, gap, radius and, on a
/// touch layout, a [`TOUCH_TARGET`] height, and the `fill` beneath its
/// content, which glides as the pointer and the highlight come and go. The
/// item must be given an id.
pub fn menu_item_frame(touch: bool, fill: ItemFill, cx: &App) -> Div {
    let spacing = &cx.theme().tokens.spacing;
    let radius = radius_button(cx);
    let hover = cx.theme().muted;
    h_flex()
        .relative()
        .gap(spacing.sm)
        .px(spacing.sm)
        .py(spacing.xs)
        .when(touch, |item| item.min_h(px(TOUCH_TARGET)))
        .rounded(radius)
        .map(|item| match fill {
            ItemFill::Inert => item,
            ItemFill::Hover => {
                item.child(crate::motion::hover_layer(hover.opacity(0.), hover, radius))
            }
            ItemFill::Highlight(rest) => {
                item.child(crate::motion::hover_layer(rest, hover, radius))
            }
        })
}

/// The three heights a horizontal bar of controls takes. gpui-base's `Toolbar`
/// is a behaviour primitive (role, roving arrow focus) with no geometry, and
/// its tokens carry no bar or control height, so the heights live here, once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    /// A window's top strip: the height of the caption cluster
    /// ([`CAPTION_STRIP_HEIGHT`](crate::window_caption::CAPTION_STRIP_HEIGHT)).
    Strip,
    /// A panel or page toolbar: 40 on the desktop, the `TabBar` underline
    /// height; a [`TOUCH_TARGET`] on a touch layout.
    Toolbar,
    /// A section header inside a rail or list: a small control's height on the
    /// desktop; a [`TOUCH_TARGET`] on a touch layout.
    Section,
}

pub fn bar_height(kind: Bar, compact: bool) -> f32 {
    match (kind, compact) {
        (Bar::Strip, _) => crate::window_caption::CAPTION_STRIP_HEIGHT,
        (Bar::Toolbar | Bar::Section, true) => TOUCH_TARGET,
        (Bar::Toolbar, false) => 40.,
        (Bar::Section, false) => 28.,
    }
}

/// One horizontal bar of controls — a page header, a panel toolbar, a section
/// header — on gpui-base's [`Toolbar`], which gives it the toolbar role and
/// roving arrow-key focus. The bar owns its height, inset (`spacing.sm`, the
/// page inset on a touch layout) and gap; callers add content.
pub fn bar(id: impl Into<ElementId>, kind: Bar, compact: bool, cx: &App) -> Toolbar {
    let spacing = &cx.theme().tokens.spacing;
    let inset = if compact {
        px(COMPACT_PAGE_INSET)
    } else {
        spacing.sm
    };
    Toolbar::new(id)
        .h_flex()
        .flex_none()
        .w_full()
        .h(px(bar_height(kind, compact)))
        .px(inset)
        .gap(spacing.xs)
}

/// A [`bar`] separated from the content below it by a hairline.
pub fn bar_divider_below(bar: Toolbar, cx: &App) -> Toolbar {
    bar.border_b_1().border_color(cx.theme().border)
}

/// The title inside a [`bar`]: takes the remaining width and truncates.
pub fn bar_title(title: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .text_title(cx)
        .child(crate::widgets::fade_text(title.into()))
}

/// The height of a [`selectable_row`]. gpui-base carries no control-height
/// token.
pub(crate) fn selectable_row_height(compact: bool) -> f32 {
    if compact { TOUCH_TARGET } else { 32. }
}

/// The surface of a selectable navigation row, the rail form of
/// [`TabBar`](crate::widgets::tabs::TabBar): the selected row takes the active
/// list fill, the rest a hover fill. Callers add the row's content, click and
/// accessibility.
pub(crate) fn selectable_row<E>(el: E, selected: bool, compact: bool, cx: &App) -> E
where
    E: Styled + InteractiveElement + gpui::ParentElement + gpui::prelude::FluentBuilder,
{
    selectable_surface(el, selectable_row_height(compact), selected, compact, cx)
}

/// The height of a [`selectable_row_two_line`].
pub const fn selectable_row_two_line_height(compact: bool) -> f32 {
    if compact { 56. } else { 48. }
}

/// The desktop inset of a [`selectable_row`].
pub fn selectable_row_inset(compact: bool) -> Pixels {
    if compact { px(12.) } else { px(10.) }
}

/// A [`selectable_row`] holding two lines of text.
pub fn selectable_row_two_line<E>(el: E, selected: bool, compact: bool, cx: &App) -> E
where
    E: Styled + InteractiveElement + gpui::ParentElement + gpui::prelude::FluentBuilder,
{
    selectable_surface(
        el,
        selectable_row_two_line_height(compact),
        selected,
        compact,
        cx,
    )
}

/// The desktop rail's 10pt inset and 6pt gap sit between gpui-base's spacing
/// tokens; the denser rail is Tcode's.
fn selectable_surface<E>(el: E, height: f32, selected: bool, compact: bool, cx: &App) -> E
where
    E: Styled + InteractiveElement + gpui::ParentElement + gpui::prelude::FluentBuilder,
{
    let theme = cx.theme();
    let (active, hover) = (theme.list_active, theme.list_hover);
    let gap = if compact {
        theme.tokens.spacing.sm
    } else {
        px(6.)
    };
    let inset = selectable_row_inset(compact);
    let radius = theme.tokens.radius.md;
    el.h(px(height))
        .relative()
        .flex()
        .items_center()
        .gap(gap)
        .px(inset)
        .rounded(radius)
        .when(selected, |row| row.bg(active))
        // A row the pointer selects lets its hover fill go as it is selected.
        .child(crate::motion::hover_layer(
            hover.opacity(0.),
            if selected { hover.opacity(0.) } else { hover },
            radius,
        ))
}

/// The caption above one section of a navigable content list: at the page
/// inset on a compact layout, a desktop section bar's height otherwise.
pub fn list_caption(label: SharedString, compact: bool, cx: &App) -> Div {
    let spacing = &cx.theme().tokens.spacing;
    let caption = div().w_full().text_caption(cx);
    if compact {
        caption
            .px(px(COMPACT_PAGE_INSET))
            .pt(spacing.md)
            .pb(spacing.xs)
    } else {
        caption
            .h(px(bar_height(Bar::Section, false)))
            .px(spacing.md)
            .flex()
            .items_center()
    }
    .child(label)
}

/// Shared wordmark.
pub fn brand_wordmark(cx: &App) -> impl IntoElement {
    gpui_base::h_flex().items_center().gap_2().child(
        div()
            .text_size(px(14.))
            .font_bold()
            .text_color(cx.theme().sidebar_foreground)
            .child(crate::tr!("app.name")),
    )
}

/// The scrim behind modal overlays, at the `overlay` token's values
/// (`themes/tcode.json`: `#1F232852` light, `#00000080` dark). Passing the ink
/// `foreground` through in dark mode would *lighten* the page behind the overlay
/// instead of pushing it back, so the dark scrim is black.
pub fn scrim(progress: f32, cx: &App) -> Hsla {
    cx.theme().by_mode(
        cx.theme().foreground.opacity(0.32 * progress),
        gpui::black().opacity(0.5 * progress),
    )
}

/// Drag handle above a compact sheet's title row.
pub fn sheet_grabber(cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .w_full()
        .h(px(SHEET_GRABBER_HEIGHT))
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
    let theme = cx.theme();
    let (spacing, typography) = (&theme.tokens.spacing, &theme.tokens.typography);
    v_flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .gap(spacing.sm)
        .p(spacing.xl)
        .child(icon.size(px(24.)).text_color(theme.muted_foreground))
        .child(
            div()
                .max_w(px(280.))
                .text_center()
                .text_token(&typography.lg)
                .font_semibold()
                .child(title.into()),
        )
        .map(|state| {
            let body = body.into();
            if body.is_empty() {
                return state;
            }
            state.child(
                div()
                    .max_w(px(280.))
                    .text_center()
                    .text_token(&typography.sm)
                    .text_color(theme.muted_foreground)
                    .child(body),
            )
        })
}

/// A panel toolbar's icon control: a dense ghost button on the desktop, a
/// [`TOUCH_TARGET`] square on a touch layout.
pub fn toolbar_icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<crate::icon::Icon>,
    tooltip: impl Into<SharedString>,
    compact: bool,
) -> Button {
    let tooltip = tooltip.into();
    let button = Button::new(id)
        .ghost()
        .aria_label(tooltip.clone())
        .tooltip(tooltip)
        .icon(icon);
    if compact {
        button.with_size(px(TOUCH_TARGET)).touch(true)
    } else {
        button.small().compact()
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

/// A dot in a status color, beside the text it qualifies. A glyph, not text,
/// so no type token sizes it.
pub fn status_dot(color: Hsla) -> Div {
    div().flex_none().size(px(8.)).rounded_full().bg(color)
}

/// A status badge in one semantic fill.
pub fn semantic_chip(label: impl Into<SharedString>, bg: Hsla, fg: Hsla, cx: &App) -> Div {
    chip_surface(bg, fg, cx).child(label.into())
}

/// A [`semantic_chip`] led by a glyph, sized by the chip's text.
pub fn semantic_chip_with_icon(
    icon: crate::icon::Icon,
    label: impl Into<SharedString>,
    bg: Hsla,
    fg: Hsla,
    cx: &App,
) -> Div {
    chip_surface(bg, fg, cx).child(icon).child(label.into())
}

/// A [`semantic_chip`]'s surface, for a label that is not plain text.
pub fn chip_surface(bg: Hsla, fg: Hsla, cx: &App) -> Div {
    let theme = cx.theme();
    gpui_base::h_flex()
        .flex_none()
        .gap(theme.tokens.spacing.xs)
        .px(theme.tokens.spacing.sm)
        .py(px(1.))
        .rounded(radius_chip(cx))
        .bg(bg)
        .text_token(&theme.tokens.typography.xs)
        .font_medium()
        .text_color(fg)
}

/// A [`semantic_chip`] with no semantic tone.
pub fn chip(label: impl Into<SharedString>, cx: &App) -> Div {
    semantic_chip(label, cx.theme().secondary, cx.theme().foreground, cx)
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
/// `on_click` for a focused clickable element; this helper supplies the tab stop,
/// semantic role/name, and a keyboard-only outline that remains legible in
/// both themes without changing layout.
pub fn accessible_clickable<E: gpui::Element + gpui::InteractiveElement>(
    el: E,
    id: impl Into<ElementId>,
    role: Role,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<E> {
    let ring = focus_ring(cx);
    el.tab_index(0)
        .focus_visible(move |style| style.shadow(vec![ring]))
        .id(id)
        .role(role)
        .aria_label(label)
}

/// The keyboard focus ring of a clickable that is not a `Button`.
///
/// Inset: GPUI paints a drop shadow's whole rectangle beneath its element
/// rather than only outside it, so an outset ring fills a transparent control.
pub(crate) fn focus_ring(cx: &App) -> BoxShadow {
    let theme = cx.theme();
    let ring = theme.by_mode(theme.ring.opacity(0.58), theme.ring.opacity(0.72));
    BoxShadow::new(px(0.), px(0.), ring)
        .spread_radius(px(2.))
        .inset()
}

/// The outline colour of palette slot `slot`, worn by a pane beside others
/// and by its thread's row in the threads list.
///
/// As drawn, at half alpha over [`content_surface`], an outline contrasts
/// about 2:1 with it in light (1.95–2.23:1) and 2.7:1 in dark (2.71–2.77:1):
/// deliberately under the 3:1 asked of non-text, because it is a hint of
/// which pane is which thread, not a control's boundary. Before alpha every
/// hue has the same contrast against that surface, 4.5:1 in light and 7:1
/// in dark, so every outline is as distinct, and as quiet, as the others.
///
/// gpui-base's tokens and Tcode's theme have no categorical hues: the
/// theme's own hues mean the focus ring, danger, warning or success, and
/// provider colours mean a provider. So the six hues go in the gaps the
/// active theme's ring, danger, warning and success hues (each background
/// and foreground) leave on the wheel: the gaps are cut into six cells in
/// all, handed out so the narrowest cell is as wide as it can be, and each
/// hue sits at a cell's centre, a cell from the next hue and at least half
/// a cell from the theme's. Slot 0 takes the hue farthest from the theme's,
/// each later slot the one farthest from the slots before it, so neighbours
/// differ. With themes/tcode.json the slots are, in light, 322° 158° 60°
/// 258° 110° 203° (at least 45° apart and 22° from the theme's hues), and in
/// dark 292° 110° 182° 61° 340° 245° (47° apart, 23° from the theme's).
///
/// The slots are found for each mode's palette at rest, so while the theme
/// glides an outline goes from its light colour to its dark one with
/// everything else.
pub(crate) fn pane_colour(slot: usize, cx: &App) -> Hsla {
    use crate::theme::{ThemeMode, palette};
    let theme = cx.theme();
    if theme.darkness <= 0. || theme.darkness >= 1. {
        return slot_colour(slot, palette(theme.mode, cx));
    }
    theme.by_mode(
        slot_colour(slot, palette(ThemeMode::Light, cx)),
        slot_colour(slot, palette(ThemeMode::Dark, cx)),
    )
}

fn slot_colour(slot: usize, theme: &crate::theme::Theme) -> Hsla {
    const SATURATION: f32 = 0.75;
    fn luminance(colour: Hsla) -> f32 {
        let Rgba { r, g, b, .. } = colour.to_rgb();
        let linear = |channel: f32| {
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }
    fn distance(a: f32, b: f32) -> f32 {
        let turn = (a - b).rem_euclid(1.);
        turn.min(1. - turn)
    }
    let dark = theme.mode.is_dark();
    let mut taken = [
        theme.ring,
        theme.danger,
        theme.danger_foreground,
        theme.warning,
        theme.warning_foreground,
        theme.success,
        theme.success_foreground,
    ]
    .map(|colour| colour.h.rem_euclid(1.));
    taken.sort_by(f32::total_cmp);
    let gaps: [(f32, f32); 7] = std::array::from_fn(|index| {
        let next = taken.get(index + 1).copied().unwrap_or(taken[0] + 1.);
        (taken[index], next - taken[index])
    });
    let mut cells = [0_usize; 7];
    for _ in 0..crate::panes::MAX_PANES {
        let cell = |index: usize| gaps[index].1 / (cells[index] + 1) as f32;
        let widest = (0..gaps.len())
            .max_by(|&a, &b| cell(a).total_cmp(&cell(b)))
            .unwrap_or_default();
        cells[widest] += 1;
    }
    let mut hues = [0_f32; crate::panes::MAX_PANES];
    let centres = gaps
        .iter()
        .zip(cells)
        .flat_map(|(&(start, length), count)| {
            (0..count)
                .map(move |cell| (start + length * (cell as f32 + 0.5) / count as f32).fract())
        });
    for (hue, centre) in hues.iter_mut().zip(centres) {
        *hue = centre;
    }
    let slot = slot % hues.len();
    for next in 0..=slot {
        let from = if next == 0 { &taken[..] } else { &hues[..next] };
        let nearest = |hue: f32| {
            from.iter()
                .map(|&other| distance(hue, other))
                .fold(f32::MAX, f32::min)
        };
        let farthest = (next..hues.len())
            .max_by(|&a, &b| nearest(hues[a]).total_cmp(&nearest(hues[b])))
            .unwrap_or(next);
        hues.swap(next, farthest);
    }
    let hue = hues[slot];
    let surface = luminance(paper(theme));
    let target = if dark {
        7. * (surface + 0.05) - 0.05
    } else {
        (surface + 0.05) / 4.5 - 0.05
    };
    // Luminance rises with lightness at a fixed hue and saturation.
    let (mut low, mut high) = (0., 1.);
    for _ in 0..20 {
        let mid = (low + high) / 2.;
        if luminance(gpui::hsla(hue, SATURATION, mid, 1.)) < target {
            low = mid;
        } else {
            high = mid;
        }
    }
    gpui::hsla(hue, SATURATION, (low + high) / 2., 0.5)
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
                        .rounded(cx.theme().tokens.radius.sm)
                        .bg(cx.theme().secondary),
                )
                .child(
                    div()
                        .w(gpui::relative(0.35))
                        .h(px(11.))
                        .rounded(cx.theme().tokens.radius.sm)
                        .bg(cx.theme().secondary),
                )
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui::{IntoElement, Render, TestAppContext, Window};
    use tcode_core::settings::ThemeMode as Setting;

    use super::*;
    use crate::settings_page::apply_theme;
    use crate::theme::{ThemeMode, palette};

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            gpui::Empty
        }
    }

    /// A pane's outline goes from its light colour straight to its dark one
    /// while the theme glides, rather than through colours found for a
    /// palette half way between.
    #[gpui::test]
    fn a_pane_colour_glides_between_its_two_modes(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| Blank);
        let at_rest = |mode, cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| slot_colour(0, palette(mode, cx)).to_rgb())
        };
        let (light, dark) = (at_rest(ThemeMode::Light, cx), at_rest(ThemeMode::Dark, cx));
        assert_eq!(cx.update(|_, cx| pane_colour(0, cx).to_rgb()), light);

        cx.update(|window, cx| apply_theme(Setting::Dark, window, cx));
        let mut previous = light;
        for _ in 0..4 {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(60));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            let now = cx.update(|_, cx| pane_colour(0, cx).to_rgb());
            assert_ne!(now, dark, "the glide has not ended yet");
            for (from, to, previous, now) in [
                (light.r, dark.r, previous.r, now.r),
                (light.g, dark.g, previous.g, now.g),
                (light.b, dark.b, previous.b, now.b),
            ] {
                let toward = |x: f32| (x - from) * (to - from).signum();
                assert!(
                    toward(previous) <= toward(now) + 1e-4 && toward(now) <= toward(to) + 1e-4,
                    "{previous:?} -> {now:?} on the way from {light:?} to {dark:?}"
                );
            }
            previous = now;
        }
        assert_ne!(previous, light, "the outline has left its light colour");

        crate::test_frames::draw_settled(cx);
        assert_eq!(cx.update(|_, cx| pane_colour(0, cx).to_rgb()), dark);
    }

    /// A menu item and the rows of a virtual list, each built with `&App`
    /// only, as a popover's content and a list's rows are.
    struct Rows;

    fn menu_item(cx: &App) -> impl IntoElement {
        menu_item_frame(false, ItemFill::Hover, cx)
            .id("menu-item")
            .debug_selector(|| "menu-item".into())
            .w(px(200.))
            .h(px(32.))
            .child("Item")
    }

    impl Render for Rows {
        fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
            div().w(px(200.)).child(menu_item(cx)).child(
                crate::scroll::VirtualList::uniform("rows", 20, |range, _, cx| {
                    range
                        .map(|index| {
                            selectable_row(div().id(("row", index)), false, false, cx)
                                .debug_selector(move || format!("row-{index}"))
                                .w_full()
                                .child("Row")
                        })
                        .collect()
                })
                .w_full()
                .h(px(200.)),
            )
        }
    }

    /// The alpha of the fill of `hue` painted over the whole of `selector`.
    fn fill(selector: &'static str, hue: Hsla, cx: &mut gpui::VisualTestContext) -> f32 {
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.update(|window, _| {
            let bounds = bounds.scale(window.scale_factor());
            window
                .painted_quads()
                .iter()
                .filter(|quad| quad.bounds == bounds)
                .filter_map(|quad| quad.background.as_solid())
                .filter(|fill| (fill.h, fill.s, fill.l) == (hue.h, hue.s, hue.l))
                .map(|fill| fill.a)
                .fold(0., f32::max)
        })
    }

    /// Under the pointer, a menu item and a list row each fill in over
    /// several frames, delivered as a display delivers them, and fade out
    /// when the pointer leaves; at rest each holds its full hover fill, or
    /// none.
    #[gpui::test]
    fn menu_items_and_list_rows_fade_their_hover_fill(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| Rows);
        let away = gpui::point(px(400.), px(400.));
        cx.simulate_mouse_move(away, None, gpui::Modifiers::none());
        crate::test_frames::draw_settled(cx);
        let (muted, list_hover) = cx.update(|_, cx| (cx.theme().muted, cx.theme().list_hover));
        let frames = |selector: &'static str, hue: Hsla, cx: &mut gpui::VisualTestContext| {
            (0..20)
                .map(|_| {
                    cx.executor()
                        .advance_clock(std::time::Duration::from_millis(16));
                    cx.update(|window, cx| {
                        window.simulate_next_frame(cx);
                        let _ = window.draw(cx);
                    });
                    fill(selector, hue, cx)
                })
                .collect::<Vec<_>>()
        };
        let fades = |alphas: &[f32], full: f32, rising: bool| {
            alphas.iter().filter(|a| **a > 0. && **a < full).count() >= 4
                && alphas.windows(2).all(|pair| {
                    if rising {
                        pair[1] >= pair[0]
                    } else {
                        pair[1] <= pair[0]
                    }
                })
        };
        for (selector, hue) in [("menu-item", muted), ("row-2", list_hover)] {
            let at = cx.debug_bounds(selector).unwrap().center();
            assert_eq!(fill(selector, hue, cx), 0.);
            cx.simulate_mouse_move(at, None, gpui::Modifiers::none());
            let alphas = frames(selector, hue, cx);
            assert!(
                fades(&alphas, hue.a, true),
                "{selector} popped in: {alphas:?}"
            );
            assert_eq!(alphas.last(), Some(&hue.a));
            cx.simulate_mouse_move(away, None, gpui::Modifiers::none());
            let alphas = frames(selector, hue, cx);
            assert!(
                fades(&alphas, hue.a, false),
                "{selector} popped out: {alphas:?}"
            );
            assert_eq!(alphas.last(), Some(&0.));
        }
    }
}
