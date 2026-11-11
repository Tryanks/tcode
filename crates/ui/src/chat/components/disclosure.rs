use crate::scroll::ScrollableElement as _;
use std::borrow::Cow;

use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use gpui::{
    AnyElement, App, ClickEvent, Div, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use tcode_core::notice::{CallbackEvent, HostNotice, OrchestrateCallback};

use super::super::model::one_line;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

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

fn callback_label(callback: &OrchestrateCallback) -> String {
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
/// `event` follows the child's title and does not fade with it. The link to
/// the child underlines under the pointer as `hover`, the timeline's,
/// follows it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn callback_row(
    key: &str,
    event: SharedString,
    callback: &OrchestrateCallback,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_open: Option<super::changed_files::ClickHandler>,
    hover: crate::motion::ListHover,
    motion: &crate::motion::owned::Owned,
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
        Some((on_open, hover)),
        motion,
        cx,
    )
}

/// The link to a callback's child, its underline fading in at the list's
/// hover `reveal`. Once the child has gone the link is leaving, and takes no
/// activation. Not gpui-base's `Link`: it keeps its focus handle to itself,
/// so its keyboard focus ring could only switch at once; the clickable's
/// fades.
fn open_child_link(
    key: &str,
    on_open: Option<super::changed_files::ClickHandler>,
    hover: &crate::motion::ListHover,
    reveal: f32,
    cx: &App,
) -> AnyElement {
    let id = SharedString::from(format!("callback-open-{key}"));
    let primary = cx.theme().primary;
    crate::material::accessible_clickable(
        div(),
        id.clone(),
        Role::Link,
        SharedString::from(crate::tr!("agents.open").into_owned()),
        cx,
    )
    .relative()
    .child(hover.hitbox(id))
    .when(reveal > 0., |link| {
        link.underline()
            .text_decoration_color(primary.opacity(reveal))
    })
    .when_some(on_open, |link, on_open| {
        link.cursor_pointer()
            .on_click(move |event, window, cx| on_open(event, window, cx))
    })
    .flex_none()
    .text_token(&cx.theme().tokens.typography.xs)
    .text_color(primary)
    .child(crate::widgets::fade_text(crate::tr!("agents.open_child")))
    .into_any_element()
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
    motion: &crate::motion::owned::Owned,
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
        motion,
        cx,
    )
}

/// `open` is a callback's link to its child, `None` in it once the child
/// has gone, and the hover it follows.
#[allow(clippy::too_many_arguments)]
fn disclosure_with(
    key: &str,
    label: SharedString,
    suffix: Option<SharedString>,
    full_text: String,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    open: Option<(
        Option<super::changed_files::ClickHandler>,
        crate::motion::ListHover,
    )>,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let key = key.to_owned();
    let motion = motion.clone();
    crate::motion::Sampled::new(move |window, cx| {
        // The label is a child rather than `.label()`: the divider grammar keeps it
        // muted, and `Button` paints its label in the variant's foreground.
        let toggle = Button::new(SharedString::from(format!("disclosure-{key}")))
            .motion(&motion)
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
                crate::widgets::Chevron::owned_icon(
                    &motion,
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
        let open = open.and_then(|(on_open, hover)| {
            let shown = motion.shown(
                format!("callback-open-shown-{key}"),
                on_open.is_some().then_some(()),
                SHOW,
                window,
                cx,
            )?;
            let reveal = hover.reveal(&format!("callback-open-{key}"), window, cx);
            Some(shown.fade(open_child_link(&key, on_open, &hover, reveal, cx)))
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

        let body = motion
            .shown(
                format!("disclosure-fold-{key}"),
                expanded.then_some(()),
                SHOW,
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
