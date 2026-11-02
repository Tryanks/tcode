use crate::scroll::ScrollableElement as _;
use std::borrow::Cow;

use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use gpui::{
    AnyElement, App, ClickEvent, Div, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px,
};
use gpui_base::{Link, h_flex, v_flex};

use tcode_core::notice::{CallbackEvent, HostNotice, OrchestrateCallback};

use super::super::model::one_line;

const DISCLOSURE_CARD_MAX_HEIGHT: f32 = 320.;

/// One line naming a notice. A row per callback names only its callback.
pub(crate) fn notice_label(notice: &HostNotice) -> String {
    match notice {
        HostNotice::Orchestrate(callbacks) => callbacks
            .iter()
            .map(callback_label)
            .collect::<Vec<_>>()
            .join(" · "),
        HostNotice::PullRequest { number, notice } => crate::tr!(
            "pull_requests.update_label",
            number = number.to_string(),
            kinds = crate::pull_requests::watch_notice_kinds(notice)
        )
        .into_owned(),
        HostNotice::ReportReminder => crate::tr!("chat.report_reminder").into_owned(),
        HostNotice::Control(messages) => messages.join(" · "),
    }
}

pub(crate) fn callback_label(callback: &OrchestrateCallback) -> String {
    format!(
        "{} {}",
        one_line(&callback.title),
        callback_event_label(callback)
    )
}

/// What became of the callback's child, which its row keeps in view however
/// long the child's title is.
pub(crate) fn callback_event_label(callback: &OrchestrateCallback) -> Cow<'static, str> {
    match &callback.event {
        CallbackEvent::Ended(state) => localized_callback_state(state),
        CallbackEvent::Approval => crate::tr!("chat.orchestrate_state_approval"),
        CallbackEvent::Question => crate::tr!("chat.orchestrate_state_question"),
    }
}

/// `on_open` opens the child the callback reports on, while it still exists.
/// `event` follows the child's title and does not fade with it.
pub(crate) fn callback_row(
    key: &str,
    event: SharedString,
    callback: &OrchestrateCallback,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_open: Option<super::subagent::ClickHandler>,
    cx: &App,
) -> AnyElement {
    let body = if callback.body.trim().is_empty() {
        crate::tr!("chat.orchestrate_callback_empty").into_owned()
    } else {
        callback.body.clone()
    };
    let title = one_line(&callback.title).into();
    disclosure_with(
        key,
        title,
        Some(event),
        body,
        expanded,
        on_toggle,
        Some(on_open),
        cx,
    )
}

/// The link to a callback's child. Once the child has gone the link is
/// leaving, and takes no activation.
fn open_child_link(key: &str, on_open: Option<super::subagent::ClickHandler>, cx: &App) -> Link {
    let ring = crate::material::focus_ring(cx);
    let link = Link::new(SharedString::from(format!("callback-open-{key}")))
        .accessibility_label(SharedString::from(crate::tr!("agents.open").into_owned()));
    match on_open {
        Some(on_open) => link.on_activate(on_open),
        None => link,
    }
    .flex_none()
    .text_token(&cx.theme().tokens.typography.xs)
    .text_color(cx.theme().primary)
    .cursor_pointer()
    .hover(|link| link.underline())
    .focus_visible(move |style| style.shadow(vec![ring]))
    .child(crate::tr!("agents.open_child"))
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
    disclosure_with(
        key,
        label,
        None,
        full_text.to_string(),
        expanded,
        on_toggle,
        None,
        cx,
    )
}

/// `open` is a callback's link to its child: `Some(None)` once the child
/// has gone.
#[allow(clippy::too_many_arguments)]
fn disclosure_with(
    key: &str,
    label: SharedString,
    suffix: Option<SharedString>,
    full_text: String,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    open: Option<Option<super::subagent::ClickHandler>>,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let key = key.to_owned();
    crate::motion::Sampled::new(move |window, cx| {
        // The label is a child rather than `.label()`: the divider grammar keeps it
        // muted, and `Button` paints its label in the variant's foreground.
        let toggle = Button::new(SharedString::from(format!("disclosure-{key}")))
            .ghost()
            .small()
            .aria_label(match &suffix {
                Some(suffix) => format!("{label} {suffix}").into(),
                None => label.clone(),
            })
            .aria_expanded(expanded)
            // A long notice (a `[control]` message is one) truncates inside the row instead of
            // spilling past both edges of the centered divider.
            .max_w_full()
            .overflow_hidden()
            .text_token(&cx.theme().tokens.typography.xs)
            .icon(
                crate::widgets::Chevron::icon(
                    SharedString::from(format!("disclosure-chevron-{key}")),
                    expanded,
                    window,
                    cx,
                )
                .text_color(muted),
            )
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1()
                    .text_color(muted)
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(label)),
                    )
                    .children(suffix.map(|suffix| div().flex_none().child(suffix))),
            )
            .on_click(on_toggle)
            .debug_selector({
                let selector = format!("disclosure-{key}");
                move || selector
            });
        let open = open.and_then(|on_open| {
            crate::motion::shown(
                SharedString::from(format!("callback-open-shown-{key}")),
                on_open.is_some().then_some(()),
                window,
                cx,
            )
            .map(|shown| shown.fade(open_child_link(&key, on_open, cx)))
        });

        let row = h_flex()
            .w_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(super::dividers::divider_stub(cx))
            .child(toggle)
            .children(open)
            .child(super::dividers::divider_stub(cx));

        let body = crate::motion::shown(
            SharedString::from(format!("disclosure-fold-{key}")),
            expanded.then_some(()),
            window,
            cx,
        )
        .map(|fold| fold.fold(disclosure_body(&key, &full_text, cx)));
        v_flex()
            .w_full()
            .gap_1()
            .child(row)
            .children(body)
            .into_any_element()
    })
    .into_any_element()
}

pub(crate) fn disclosure_body(key: &str, full_text: &str, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    crate::material::card(cx)
        .w_full()
        // The card keeps clicks to itself, but a pan that its body cannot use
        // must still reach the timeline behind it.
        .block_mouse_except_scroll()
        .child(
            div()
                .id(SharedString::from(format!("disclosure-body-{key}")))
                .w_full()
                .max_h(px(DISCLOSURE_CARD_MAX_HEIGHT))
                .overflow_y_scroll_area()
                .child(
                    div()
                        .w_full()
                        .text_body(false, cx)
                        .text_color(muted)
                        .child(full_text.to_string()),
                ),
        )
}

fn localized_callback_state(state: &str) -> Cow<'static, str> {
    match state {
        "completed" => crate::tr!("chat.orchestrate_state_completed"),
        "reported" => crate::tr!("chat.orchestrate_state_reported"),
        "updated" => crate::tr!("chat.orchestrate_state_updated"),
        "unreported" => crate::tr!("chat.orchestrate_state_unreported"),
        "failed" => crate::tr!("chat.orchestrate_state_failed"),
        other => Cow::Owned(other.to_string()),
    }
}
