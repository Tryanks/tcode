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

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// Height reserved beneath chat messages for hover-revealed actions.
pub(crate) const CHAT_ACTION_ROW_HEIGHT: f32 = 24.;
/// Maximum width of the shared chat/composer content column.
pub(crate) const CHAT_CONTENT_MAX_WIDTH: f32 = 720.;
/// Minimum horizontal padding around the shared chat/composer content column.
pub(crate) const CHAT_CONTENT_MIN_PADDING: f32 = 24.;
/// The narrowest a side panel's content reads at: the right panel's (its
/// icon tabs and own controls fit in it).
pub(crate) const PANEL_CONTENT_MIN_WIDTH: f32 = 320.;
/// The chat's side rail at its narrowest: a card header still holds its
/// title, a few words of summary and its chevron. Kept below a panel's
/// narrowest so the rail sits beside the centred column of a maximized
/// 1512pt window with the sidebar open, whose chat is about 1256 wide.
pub(crate) const CHAT_RAIL_NARROWEST: f32 = 220.;
/// The chat's side rail at its widest: a wider gutter only widens the room
/// between the column and the rail, since card rows gain nothing past it.
pub(crate) const CHAT_RAIL_WIDEST: f32 = 300.;
/// Between the chat's content column and its side rail: the timeline
/// scrollbar's track, which stands just right of the column, is this wide,
/// so beside a column moved clear of the rail it fills this gap.
pub(crate) const CHAT_RAIL_GAP: f32 = 16.;
/// Between the side rail and the chat's right edge.
pub(crate) const CHAT_RAIL_EDGE: f32 = 16.;
/// What an agent's card in the rail's place keeps left of it beyond
/// [`CHAT_RAIL_GAP`]: its edge's resize band, 9 wide, which the timeline
/// scrollbar's track stays clear of.
pub(crate) const CHAT_SIDE_GRIP: f32 = 12.;
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

/// How long a list row's or a menu item's hover fill, or a list's hover
/// reveal, takes to come in or go.
pub(crate) const HOVER: std::time::Duration = std::time::Duration::from_millis(200);
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
/// Selectable list rows.
pub fn radius_row(cx: &App) -> Pixels {
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
pub fn overlay_contour<E: Styled>(el: E, cx: &App) -> Raised<E> {
    raised(
        el.bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_xl(),
    )
}

/// Height of the drag handle strip above a compact sheet's title row.
pub const SHEET_GRABBER_HEIGHT: f32 = 16.;
/// Height of a compact sheet's title row.
pub const SHEET_TITLE_HEIGHT: f32 = 48.;

/// The compact bottom sheet's surface: top corners at the sheet radius, opaque
/// fill, a hairline along its top edge and the large soft shadow.
pub fn sheet_contour<E: Styled>(el: E, cx: &App) -> Raised<E> {
    raised(
        el.rounded_t(radius_overlay_sheet(cx))
            .bg(cx.theme().popover)
            .border_t_1()
            .border_color(cx.theme().border)
            .shadow_xl(),
    )
}

/// The window edge a [`floating_panel`] is docked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// A side panel floating over the content: opaque fill, a medium shadow and a
/// hairline on the edge that faces the content.
pub fn floating_panel<E: Styled>(el: E, side: Side, cx: &App) -> Raised<E> {
    let el = raised(el).bg(cx.theme().popover).shadow_lg();
    match side {
        Side::Left => el.border_r_1(),
        Side::Right => el.border_l_1(),
    }
    .border_color(cx.theme().border)
}

/// A side panel `lift` of the way from docked to [floating](floating_panel). A
/// docked one is a peer of the content beside it: the same paper, opaque for
/// what runs on beneath it, the same hairline, and no shadow.
pub fn lifting_panel<E: Styled>(el: E, side: Side, lift: f32, cx: &App) -> Raised<E> {
    let mut el = floating_panel(el, side, cx);
    let style = el.style();
    style.box_shadow = style
        .box_shadow
        .take()
        .filter(|_| lift > 0.)
        .map(|shadows| {
            shadows
                .into_iter()
                .map(|mut shadow| {
                    shadow.color.a *= lift;
                    shadow
                })
                .collect()
        });
    let docked = opaque_canvas(cx).blend(content_surface(cx));
    el.bg(docked.blend(cx.theme().popover.opacity(lift)))
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
pub fn card_raised(cx: &App) -> Raised<Div> {
    raised(
        card(cx)
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_sm(),
    )
}

/// `surface`, raised off what it lies on by its drop shadows: while it is
/// drawn faint, by its own opacity or a fade it is painted under
/// ([`crate::motion::faded`]), its shadows are drawn only around it, as a
/// box shadow is drawn outside its border box; at rest, as GPUI draws them.
///
/// Gap: GPUI draws an element's drop shadow under it as well as around it,
/// and fades an element by fading each thing it paints rather than the
/// element as one. A raised surface fading in or out would show its own
/// shadow through its fill, darker than both what it lies on and the
/// surface. A rounded corner leaves part of its square uncovered, where the
/// shadow shows around the surface too, so the shadow is still drawn under
/// those squares. Remove this once GPUI draws a drop shadow only outside its
/// element, or fades an element as one.
pub fn raised<E: Styled>(surface: E) -> Raised<E> {
    Raised {
        inner: Some(surface),
    }
}

/// See [`raised`].
pub struct Raised<E> {
    inner: Option<E>,
}

impl<E> Raised<E> {
    fn inner(&mut self) -> &mut E {
        self.inner
            .as_mut()
            .expect("a raised surface is laid out once")
    }
}

/// What a [`Raised`] keeps from its layout to its paint.
pub struct RaisedLayout {
    surface: gpui::AnyElement,
    shadows: Vec<BoxShadow>,
    corners: gpui::CornersRefinement<gpui::AbsoluteLength>,
    /// The surface's own opacity.
    opacity: f32,
}

/// The regions `shadow` of a surface at `bounds` with `radii` corners is
/// drawn in while the surface is faint: all round it, and the parts of its
/// corners' squares the rounding leaves open. Their edges lie on device
/// pixels the surface covers whole, so neighbours meet without a gap or an
/// overlap.
///
/// GPUI culls a shadow whose content mask misses the rectangle it blurs,
/// although it draws the blur well past it (`Scene::insert_primitive`
/// against the shadow shader's margin). A shadow whose rectangle lies inside
/// the surface on a side, which a small or negative spread makes it, would
/// be culled from that side's region entirely: each region reaches into the
/// surface just past the rectangle's edge on its side, and the shadow is
/// drawn under that edge of the surface too.
fn around(
    bounds: gpui::Bounds<Pixels>,
    radii: gpui::Corners<Pixels>,
    shadow: &BoxShadow,
    scale: f32,
) -> Option<Vec<gpui::Bounds<Pixels>>> {
    let snap = |value: Pixels, up: bool| {
        let device = f32::from(value) * scale;
        px(if up { device.ceil() } else { device.floor() } / scale)
    };
    let (left, top) = (snap(bounds.left(), true), snap(bounds.top(), true));
    let (right, bottom) = (snap(bounds.right(), false), snap(bounds.bottom(), false));
    if right <= left || bottom <= top {
        return None;
    }
    // How far inside the surface the blurred rectangle's edge lies on each
    // side, and so how far that side's region reaches in: past the edge by a
    // device pixel, or not at all where the rectangle reaches out.
    let pixel = px(1. / scale);
    let inward = |inset: Pixels| {
        if inset < px(0.) {
            px(0.)
        } else {
            snap(inset, true) + pixel
        }
    };
    let (offset, spread) = (shadow.offset, shadow.spread_radius);
    let most = ((right - left) / 2.).min((bottom - top) / 2.);
    let band_top = inward(offset.y - spread).min(most);
    let band_bottom = inward(-offset.y - spread).min(most);
    let band_left = inward(offset.x - spread).min(most);
    let band_right = inward(-offset.x - spread).min(most);
    let reach =
        shadow.blur_radius * 3. + spread.max(px(0.)) + offset.x.abs() + offset.y.abs() + px(1.);
    let (outer_left, outer_top) = (left - reach, top - reach);
    let (outer_right, outer_bottom) = (right + reach, bottom + reach);
    let (inner_top, inner_bottom) = (top + band_top, bottom - band_bottom);
    let (inner_left, inner_right) = (left + band_left, right - band_right);
    let corner = |radius: Pixels| snap(radius.min(most), true);
    let rect = |left: Pixels, top: Pixels, right: Pixels, bottom: Pixels| {
        (right > left && bottom > top)
            .then(|| gpui::Bounds::from_corners(gpui::point(left, top), gpui::point(right, bottom)))
    };
    let (top_left, top_right) = (corner(radii.top_left), corner(radii.top_right));
    let (bottom_left, bottom_right) = (corner(radii.bottom_left), corner(radii.bottom_right));
    Some(
        [
            rect(outer_left, outer_top, outer_right, inner_top),
            rect(outer_left, inner_bottom, outer_right, outer_bottom),
            rect(outer_left, inner_top, inner_left, inner_bottom),
            rect(inner_right, inner_top, outer_right, inner_bottom),
            rect(inner_left, inner_top, left + top_left, top + top_left),
            rect(right - top_right, inner_top, inner_right, top + top_right),
            rect(
                inner_left,
                bottom - bottom_left,
                left + bottom_left,
                inner_bottom,
            ),
            rect(
                right - bottom_right,
                bottom - bottom_right,
                inner_right,
                inner_bottom,
            ),
        ]
        .into_iter()
        .flatten()
        .collect(),
    )
}

impl<E: IntoElement + Styled + 'static> IntoElement for Raised<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: IntoElement + Styled + 'static> gpui::Element for Raised<E> {
    type RequestLayoutState = RaisedLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut gpui::Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, RaisedLayout) {
        let mut inner = self
            .inner
            .take()
            .expect("a raised surface is laid out once");
        let style = inner.style();
        let shadows = style.box_shadow.take().unwrap_or_default();
        let corners = style.corner_radii.clone();
        let opacity = style.opacity.unwrap_or(1.);
        let mut surface = inner.into_any_element();
        let layout = surface.request_layout(window, cx);
        (
            layout,
            RaisedLayout {
                surface,
                shadows,
                corners,
                opacity,
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<Pixels>,
        layout: &mut RaisedLayout,
        window: &mut gpui::Window,
        cx: &mut App,
    ) {
        layout.surface.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        layout: &mut RaisedLayout,
        _: &mut (),
        window: &mut gpui::Window,
        cx: &mut App,
    ) {
        if !layout.shadows.is_empty() {
            let rem = window.rem_size();
            let most = bounds.size.width.min(bounds.size.height) / 2.;
            let radius = |corner: Option<gpui::AbsoluteLength>| {
                corner
                    .map_or(px(0.), |corner| corner.to_pixels(rem))
                    .min(most)
            };
            let corners = &layout.corners;
            let radii = gpui::Corners {
                top_left: radius(corners.top_left),
                top_right: radius(corners.top_right),
                bottom_right: radius(corners.bottom_right),
                bottom_left: radius(corners.bottom_left),
            };
            // The surface's own opacity is applied as it paints itself,
            // after these.
            let opacity = layout.opacity;
            let shadows: Vec<BoxShadow> = layout
                .shadows
                .iter()
                .cloned()
                .map(|mut shadow| {
                    shadow.color.a *= opacity;
                    shadow
                })
                .collect();
            let faint = opacity * crate::motion::faded::opacity() < 1.;
            let scale = window.scale_factor();
            for shadow in &shadows {
                let shadow = std::slice::from_ref(shadow);
                match around(bounds, radii, &shadow[0], scale).filter(|_| faint) {
                    Some(regions) => {
                        for region in regions {
                            window.with_content_mask(
                                Some(gpui::ContentMask { bounds: region }),
                                |window| window.paint_drop_shadows(bounds, radii, shadow),
                            );
                        }
                    }
                    None => window.paint_drop_shadows(bounds, radii, shadow),
                }
            }
        }
        // What the surface holds is drawn as faint as it is.
        crate::motion::faded::within(layout.opacity, || layout.surface.paint(window, cx));
    }
}

impl<E: Styled> Styled for Raised<E> {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        self.inner().style()
    }
}

impl<E: InteractiveElement> InteractiveElement for Raised<E> {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.inner().interactivity()
    }
}

impl<E: gpui::StatefulInteractiveElement> gpui::StatefulInteractiveElement for Raised<E> {}

impl<E: gpui::ParentElement> gpui::ParentElement for Raised<E> {
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.inner().extend(elements);
    }
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
pub fn group(cx: &App) -> Raised<Div> {
    raised(v_flex())
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
pub fn grouped(rows: Vec<gpui::AnyElement>, cx: &App) -> Raised<Div> {
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
pub fn list_row(id: impl Into<ElementId>, label: SharedString, cx: &App) -> Clickable<Div> {
    accessible_clickable(gpui_base::h_flex(), id, Role::Button, label, cx)
        .w_full()
        .min_h(px(LIST_ROW_MIN_HEIGHT))
        .px(px(COMPACT_PAGE_INSET))
        .py(px(8.))
        .gap_3()
        .items_center()
        .cursor_pointer()
        .relative()
        .child(row_hover(px(0.), cx))
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
) -> Clickable<Div> {
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
        .relative()
        .child(row_hover(radius_card(cx), cx))
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

/// A row's list hover fill, gliding as the pointer comes and goes, for the
/// row's first child; the row must be positioned and have an id.
pub(crate) fn row_hover(radius: Pixels, cx: &App) -> crate::motion::Sampled {
    let hover = cx.theme().list_hover;
    crate::motion::hover_layer(None, hover.opacity(0.), hover, radius, HOVER.into())
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
            ItemFill::Hover => item.child(crate::motion::hover_layer(
                None,
                hover.opacity(0.),
                hover,
                radius,
                HOVER.into(),
            )),
            ItemFill::Highlight(rest) => item.child(crate::motion::hover_layer(
                None,
                rest,
                hover,
                radius,
                HOVER.into(),
            )),
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
        .when(selected, |row| row.bg(cx.theme().list_active))
}

/// A [`selectable_row`] whose hover fill's clock is kept by `motion` under
/// `key`, so a fade under way carries on when the view is drawn under
/// another parent.
pub(crate) fn selectable_row_owned<E>(
    el: E,
    motion: &crate::motion::owned::Owned,
    key: impl Into<SharedString>,
    selected: bool,
    compact: bool,
    cx: &App,
) -> E
where
    E: Styled + InteractiveElement + gpui::ParentElement + gpui::prelude::FluentBuilder,
{
    selectable_surface_with(
        el,
        selectable_row_height(compact),
        selected,
        compact,
        Some((motion, key.into())),
        cx,
    )
    .when(selected, |row| row.bg(cx.theme().list_active))
}

/// A [`selectable_row`] whose selected fill is a mark its list draws under
/// it and glides between rows (a [`TabBar`](crate::widgets::tabs::TabBar)
/// rail).
pub(crate) fn selectable_row_under_mark<E>(el: E, selected: bool, compact: bool, cx: &App) -> E
where
    E: Styled + InteractiveElement + gpui::ParentElement + gpui::prelude::FluentBuilder,
{
    selectable_surface(el, selectable_row_height(compact), selected, compact, cx)
}

/// A [`selectable_row`] whose selected fill glides in and out as the
/// selection moves (a selection that follows the scroll), the row holding
/// it as a layer under its content. Its fills' clocks are kept by `motion`
/// under `key`, so they carry on when the view is drawn under another
/// parent.
pub(crate) fn selectable_row_gliding<E>(
    el: E,
    motion: &crate::motion::owned::Owned,
    key: impl Into<SharedString>,
    selected: bool,
    compact: bool,
    cx: &App,
) -> E
where
    E: Styled + InteractiveElement + gpui::ParentElement + gpui::prelude::FluentBuilder,
{
    let key = key.into();
    let (active, radius) = (cx.theme().list_active, cx.theme().tokens.radius.md);
    let fill_motion = motion.clone();
    let fill_key = format!("{key}/selected-fill");
    selectable_surface_with(
        el,
        selectable_row_height(compact),
        selected,
        compact,
        Some((motion, key)),
        cx,
    )
    .child(crate::motion::Sampled::new(move |window, cx| {
        let fill = fill_motion.transition(
            fill_key,
            if selected { active } else { active.opacity(0.) },
            GLIDE,
            window,
            cx,
        );
        div()
            .absolute()
            .inset_0()
            .rounded(radius)
            .bg(fill)
            .into_any_element()
    }))
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
    .when(selected, |row| row.bg(cx.theme().list_active))
}

/// The desktop rail's 10pt inset and 6pt gap sit between gpui-base's spacing
/// tokens; the denser rail is Tcode's.
fn selectable_surface<E>(el: E, height: f32, selected: bool, compact: bool, cx: &App) -> E
where
    E: Styled + InteractiveElement + gpui::ParentElement + gpui::prelude::FluentBuilder,
{
    selectable_surface_with(el, height, selected, compact, None, cx)
}

/// [`selectable_surface`] whose hover fill's clock is kept by the owner and
/// key `owned` names, when one does.
fn selectable_surface_with<E>(
    el: E,
    height: f32,
    selected: bool,
    compact: bool,
    owned: Option<(&crate::motion::owned::Owned, SharedString)>,
    cx: &App,
) -> E
where
    E: Styled + InteractiveElement + gpui::ParentElement + gpui::prelude::FluentBuilder,
{
    let theme = cx.theme();
    let hover = theme.list_hover;
    let gap = if compact {
        theme.tokens.spacing.sm
    } else {
        px(6.)
    };
    let inset = selectable_row_inset(compact);
    let radius = radius_row(cx);
    el.h(px(height))
        .relative()
        .flex()
        .items_center()
        .gap(gap)
        .px(inset)
        .rounded(radius)
        // A row the pointer selects lets its hover fill go as it is selected.
        .child({
            let (rest, hover) = (
                hover.opacity(0.),
                if selected { hover.opacity(0.) } else { hover },
            );
            crate::motion::hover_layer(owned, rest, hover, radius, HOVER.into())
        })
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
/// both themes without changing layout, fading in and out with the keyboard's
/// focus.
pub fn accessible_clickable<E: gpui::Element + gpui::InteractiveElement + Styled>(
    el: E,
    id: impl Into<ElementId>,
    role: Role,
    label: impl Into<SharedString>,
    cx: &App,
) -> Clickable<E> {
    let id = id.into();
    Clickable {
        inner: Some(el.id(id.clone()).role(role).aria_label(label)),
        id,
        focus: None,
        ring: focus_ring(cx),
    }
}

/// A surface [`accessible_clickable`] made clickable. It holds its own focus
/// handle, kept under its id, and paints its focus ring over itself fading in
/// and out ([`crate::motion::focus_ring`]): with GPUI's `tab_index` alone the
/// handle is GPUI's, and only its `focus_visible` style, which switches at
/// once, knows the keyboard focused it.
pub struct Clickable<E: gpui::Element + gpui::InteractiveElement> {
    inner: Option<Stateful<E>>,
    id: ElementId,
    /// The handle the caller tracks, when it keeps its own.
    focus: Option<gpui::FocusHandle>,
    ring: BoxShadow,
}

impl<E: gpui::Element + gpui::InteractiveElement> Clickable<E> {
    fn inner(&mut self) -> &mut Stateful<E> {
        self.inner.as_mut().expect("a clickable is laid out once")
    }
}

/// What a [`Clickable`] keeps from its layout to its paint: the surface, its
/// focus handle and its corners.
pub struct ClickableLayout {
    surface: gpui::AnyElement,
    focus: gpui::FocusHandle,
    corners: gpui::CornersRefinement<gpui::AbsoluteLength>,
}

impl<E: gpui::Element + gpui::InteractiveElement + Styled + 'static> IntoElement for Clickable<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: gpui::Element + gpui::InteractiveElement + Styled + 'static> gpui::Element
    for Clickable<E>
{
    type RequestLayoutState = ClickableLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut gpui::Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ClickableLayout) {
        let mut inner = self.inner.take().expect("a clickable is laid out once");
        let corners = inner.style().corner_radii.clone();
        let focus = match self.focus.clone() {
            Some(focus) => focus,
            None => crate::motion::focus_ring::handle(&self.id, window, cx),
        };
        let mut surface = inner.track_focus(&focus).into_any_element();
        let layout = surface.request_layout(window, cx);
        (
            layout,
            ClickableLayout {
                surface,
                focus,
                corners,
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<Pixels>,
        layout: &mut ClickableLayout,
        window: &mut gpui::Window,
        cx: &mut App,
    ) {
        layout.surface.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        layout: &mut ClickableLayout,
        _: &mut (),
        window: &mut gpui::Window,
        cx: &mut App,
    ) {
        layout.surface.paint(window, cx);
        crate::motion::focus_ring::paint(
            &layout.focus,
            self.ring.clone(),
            bounds,
            &layout.corners,
            window,
            cx,
        );
    }
}

impl<E: gpui::Element + gpui::InteractiveElement + Styled> Styled for Clickable<E> {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        self.inner().style()
    }
}

impl<E: gpui::Element + gpui::InteractiveElement> InteractiveElement for Clickable<E> {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.inner().interactivity()
    }

    /// A caller that keeps its own focus handle keeps it, and the ring
    /// follows that handle.
    fn track_focus(mut self, focus_handle: &gpui::FocusHandle) -> Self {
        self.focus = Some(focus_handle.clone());
        self
    }
}

impl<E: gpui::Element + gpui::InteractiveElement> gpui::StatefulInteractiveElement
    for Clickable<E>
{
}

impl<E: gpui::Element + gpui::InteractiveElement + gpui::ParentElement> gpui::ParentElement
    for Clickable<E>
{
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.inner().extend(elements);
    }
}

impl gpui::StyledImage for Clickable<gpui::Img> {
    fn image_style(&mut self) -> &mut gpui::ImageStyle {
        self.inner().image_style()
    }
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
    use super::*;
    use gpui::{IntoElement, Render, TestAppContext, Window};

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
