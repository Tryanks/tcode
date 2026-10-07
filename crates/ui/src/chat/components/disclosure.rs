use crate::scroll::ScrollableElement as _;
use std::borrow::Cow;

use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use gpui::{
    AnyElement, App, ClickEvent, Div, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};
use gpui_base::{h_flex, v_flex};

use tcode_core::session::OrchestrateCallback;

use super::super::model::one_line;

const CALLBACK_TITLE_MAX_CHARS: usize = 24;
const DISCLOSURE_LINE_HEIGHT: f32 = 20.;
const DISCLOSURE_CARD_MAX_HEIGHT: f32 = 320.;

pub(crate) fn callback_row(
    entry_id: &str,
    callback: &OrchestrateCallback,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let title = truncate_chars(&callback.title, CALLBACK_TITLE_MAX_CHARS);
    let label = SharedString::from(format!(
        "{title} {}",
        localized_callback_state(&callback.state)
    ));
    let body = if callback.body.trim().is_empty() {
        crate::tr!("chat.orchestrate_callback_empty").into_owned()
    } else {
        callback.body.clone()
    };
    disclosure(
        &format!("orchestrate-callback-{entry_id}"),
        label,
        &body,
        expanded,
        on_toggle,
        cx,
    )
}

/// A centered disclosure notification with a height-capped verbatim body: the
/// divider grammar's stub–label–stub row, with the label clickable to expand.
/// Orchestrate context and child-thread results read as ambient notifications
/// of the flow — the same standing as a relay or model-change divider.
pub(crate) fn disclosure(
    key: &str,
    label: SharedString,
    full_text: &str,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let toggle = crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("disclosure-{key}")),
        Role::Button,
        label.clone(),
        cx,
    )
    .aria_expanded(expanded)
    .flex_none()
    .h(px(24.))
    .px_1p5()
    .gap_1p5()
    .items_center()
    .rounded(crate::material::radius_button(cx))
    .text_size(px(11.))
    .text_color(muted)
    .cursor_pointer()
    .hover(|row| row.bg(cx.theme().accent))
    .on_click(on_toggle)
    .child(Icon::new(chevron(expanded)).size(px(12.)).text_color(muted))
    .child(label);

    let row = h_flex()
        .w_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(super::dividers::divider_stub(cx))
        .child(toggle)
        .child(super::dividers::divider_stub(cx));

    let mut block = v_flex().w_full().gap_1().child(row);
    if expanded {
        block = block.child(disclosure_body(key, full_text, cx));
    }
    block.into_any_element()
}

fn disclosure_body(key: &str, full_text: &str, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    div()
        .w_full()
        .rounded(crate::material::radius_card(cx))
        .bg(cx.theme().muted)
        // The card keeps clicks to itself, but a pan that its body cannot use
        // must still reach the timeline behind it.
        .block_mouse_except_scroll()
        .p_3()
        .child(
            div()
                .id(SharedString::from(format!("disclosure-body-{key}")))
                .w_full()
                .max_h(px(DISCLOSURE_CARD_MAX_HEIGHT))
                .overflow_y_scroll_area()
                .child(
                    div()
                        .w_full()
                        .text_size(px(13.))
                        .line_height(px(DISCLOSURE_LINE_HEIGHT))
                        .text_color(muted)
                        .child(full_text.to_string()),
                ),
        )
}

fn localized_callback_state(state: &str) -> Cow<'static, str> {
    match state {
        "completed" => crate::tr!("chat.orchestrate_state_completed"),
        "failed" => crate::tr!("chat.orchestrate_state_failed"),
        other => Cow::Owned(other.to_string()),
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    let text = one_line(text);
    if text.chars().count() <= max {
        return text;
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

fn chevron(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}
