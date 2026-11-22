use crate::scroll::ScrollableElement as _;
use std::borrow::Cow;

use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use gpui::{
    App, Div, InteractiveElement as _, ParentElement as _, SharedString, Styled as _, div, px,
};

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

fn callback_label(callback: &OrchestrateCallback) -> String {
    format!(
        "{} {}",
        one_line(&callback.title),
        callback_event_label(callback)
    )
}

/// What became of the callback's child, worded to follow its title.
fn callback_event_label(callback: &OrchestrateCallback) -> Cow<'static, str> {
    match &callback.event {
        CallbackEvent::Ended(state) => localized_callback_state(state),
        CallbackEvent::Approval => crate::tr!("chat.orchestrate_state_approval"),
        CallbackEvent::Question => crate::tr!("chat.orchestrate_state_question"),
    }
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
