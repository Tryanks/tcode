//! The card an event takes in the chat flow: one surface, a header of an
//! icon tile, a title over a subtitle and a status at the end, and sections
//! under it. Each card composes these parts the way [`super::card`] and
//! [`super::card_header_row`] are composed.

use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use gpui::{
    AnyElement, App, Div, Hsla, ParentElement as _, SharedString, Styled as _, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use super::CARD_INSET;

/// The card's corner radius, which whatever lies over its whole surface
/// (a hover fill, a ring) follows.
pub(crate) const RADIUS: f32 = 12.;
const TILE: f32 = 30.;
const TILE_RADIUS: f32 = 8.;
const TILE_ICON: f32 = 16.;
const BADGE: f32 = 12.;

/// What a card asks of whoever reads it, which tints its surface: neutral
/// when it only records something, warning when something went wrong and the
/// flow carried on, danger when the turn or operation stopped, waiting when
/// it waits on the user, which it does only with the action that answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CardTone {
    Neutral,
    Warning,
    Danger,
    Waiting,
}

impl CardTone {
    /// The tone's colour, `None` for a neutral card.
    fn color(self, cx: &App) -> Option<Hsla> {
        let theme = cx.theme();
        match self {
            Self::Neutral => None,
            Self::Warning => Some(theme.warning),
            Self::Danger => Some(theme.danger),
            Self::Waiting => Some(theme.primary),
        }
    }
}

/// The card's surface: a hairline border over a faint wash, both in its
/// tone. Children are its header and the sections under it, each drawing
/// its own inset and the hairline above it.
pub(crate) fn event_card(tone: CardTone, cx: &App) -> Div {
    let (border, fill) = surface_colors(tone, cx);
    v_flex()
        .w_full()
        .rounded(px(RADIUS))
        .border_1()
        .border_color(border)
        .bg(fill)
        .overflow_hidden()
}

/// An [`event_card`] floating over what it lies on: its wash over an opaque
/// fill, so nothing under it shows through, lifted by a small shadow.
pub(crate) fn raised_event_card(tone: CardTone, cx: &App) -> super::Raised<Div> {
    let (_, fill) = surface_colors(tone, cx);
    super::raised(
        event_card(tone, cx)
            .bg(cx.theme().popover.blend(fill))
            .shadow_sm(),
    )
}

/// The border and the fill of a card's surface in `tone`.
fn surface_colors(tone: CardTone, cx: &App) -> (Hsla, Hsla) {
    let theme = cx.theme();
    match tone.color(cx) {
        None => (theme.border, theme.foreground.opacity(0.025)),
        Some(tone) => (tone.opacity(0.4), tone.opacity(0.06)),
    }
}

/// The header row: a [`tile`], then [`header_text`], then a status. What
/// follows the text, a status or a control, reads at the caption size.
/// `slim` is the one-line card's, for an event that needs no subtitle.
pub(crate) fn header(slim: bool, cx: &App) -> Div {
    h_flex()
        .w_full()
        .min_w_0()
        .items_center()
        .px(px(CARD_INSET))
        .py(px(if slim { 8. } else { 10. }))
        .gap(px(10.))
        .text_token(&cx.theme().tokens.typography.xs)
        .text_color(cx.theme().foreground)
}

/// The square the card's icon sits in. `badge` hangs a smaller icon off its
/// bottom-right corner, on a disc of the chat's paper so it stays legible.
pub(crate) fn tile(icon: Icon, badge: Option<Icon>, cx: &App) -> Div {
    tile_in(icon, badge, cx.theme().muted_foreground, cx)
}

/// A [`tile`] for a plain icon name, the icon drawn in `color`: an event's
/// own colour, such as an error's.
pub(crate) fn tinted_tile(icon: IconName, color: Hsla, cx: &App) -> Div {
    tile_in(Icon::new(icon), None, color, cx)
}

/// A [`tile`] whose icon keeps the colour it was given, as a vendor's glyph
/// keeps its profile's.
pub(crate) fn glyph_tile(icon: Icon, badge: Option<Icon>, cx: &App) -> Div {
    let color = icon
        .explicit_text_color()
        .unwrap_or(cx.theme().muted_foreground);
    tile_in(icon, badge, color, cx)
}

fn tile_in(icon: Icon, badge: Option<Icon>, color: Hsla, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .relative()
        .flex_none()
        .size(px(TILE))
        .rounded(px(TILE_RADIUS))
        .bg(theme.foreground.opacity(0.07))
        .flex()
        .items_center()
        .justify_center()
        .child(icon.size(px(TILE_ICON)).text_color(color))
        .when_some(badge, |tile, badge| {
            tile.child(
                div()
                    .absolute()
                    .right(px(-3.))
                    .bottom(px(-3.))
                    .size(px(BADGE + 2.))
                    .rounded_full()
                    .bg(super::content_surface(cx))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(badge.size(px(BADGE)).text_color(theme.foreground)),
            )
        })
}

/// A [`tile`] for a plain icon name with no badge.
pub(crate) fn icon_tile(icon: IconName, cx: &App) -> Div {
    tile(Icon::new(icon), None, cx)
}

/// The title over its subtitle, each held to one line. It takes the room
/// the header leaves, so a status after it stays whole.
pub(crate) fn header_text(
    title: impl Into<SharedString>,
    subtitle: Option<SharedString>,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    v_flex()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .child(
            div()
                .min_w_0()
                .text_token(&theme.tokens.typography.sm)
                .font_medium()
                .child(crate::widgets::fade_text(title.into())),
        )
        .when_some(subtitle, |text, subtitle| {
            text.child(
                div()
                    .min_w_0()
                    .text_token(&theme.tokens.typography.xs)
                    .text_color(theme.muted_foreground)
                    .child(crate::widgets::fade_text(subtitle)),
            )
        })
}

/// What became of the event, at the header's end in `color`: an optional
/// glyph and a word or two.
pub(crate) fn status(
    glyph: Option<AnyElement>,
    label: impl Into<SharedString>,
    color: Hsla,
    cx: &App,
) -> Div {
    h_flex()
        .flex_none()
        .items_center()
        .gap(cx.theme().tokens.spacing.xs)
        .text_token(&cx.theme().tokens.typography.xs)
        .text_color(color)
        .children(glyph)
        .child(label.into())
}

/// A section under the header, ruled off from what is above it.
pub(crate) fn section(cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .w_full()
        .min_w_0()
        .px(px(CARD_INSET))
        .py(px(9.))
        .gap(theme.tokens.spacing.sm)
        .border_t_1()
        .border_color(theme.border)
        .text_token(&theme.tokens.typography.sm)
}

/// The card's last section: its actions, at the end of the row.
pub(crate) fn footer(cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .w_full()
        .items_center()
        .justify_end()
        .px(px(CARD_INSET))
        .py(theme.tokens.spacing.sm)
        .gap(theme.tokens.spacing.sm)
        .border_t_1()
        .border_color(theme.border)
        .text_token(&theme.tokens.typography.xs)
}
