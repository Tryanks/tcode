//! The cards a pull request takes in the flow: the record of a call that
//! linked one to the thread, and a watch's wake.

use crate::icon::{Icon, IconName};
use crate::material::event_card::{self, CardTone};
use crate::scroll::ScrollableElement as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use tcode_core::pull_request::PullRequestBadgeState;
use tcode_core::pull_request_watch::{WatchChangeKind, WatchNotice};

use super::changed_files::ClickHandler;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

const BODY_MAX_HEIGHT: f32 = 320.;

/// What a linked pull request's record shows.
pub(crate) struct PullRequestRecord {
    /// Its title, or `repository#number` while the thread has read none.
    pub(crate) title: SharedString,
    pub(crate) subtitle: SharedString,
    /// What the call did and when.
    pub(crate) said: SharedString,
    /// Opens it in the thread's Pull requests panel, `None` once the thread
    /// no longer shows it.
    pub(crate) on_open: Option<ClickHandler>,
}

/// The record of a call that linked a pull request, as it stood then: no
/// state, checks or review, which only the panel it opens keeps current.
pub(crate) fn record_card(
    id: &str,
    record: PullRequestRecord,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let header = event_card::header(false, cx)
        .child(event_card::icon_tile(IconName::GitPullRequest, cx))
        .child(event_card::header_text(
            record.title,
            Some(record.subtitle),
            cx,
        ))
        .child(event_card::status(
            None,
            record.said,
            cx.theme().muted_foreground,
            cx,
        ));
    let footer = record.on_open.map(|on_open| {
        let selector = format!("pull-request-record-open-{id}");
        event_card::footer(cx).child(
            super::changed_files::quiet_control(
                SharedString::from(selector.clone()),
                crate::tr!("pull_requests.record_open").into_owned().into(),
                on_open,
                cx,
            )
            .motion(motion)
            .debug_selector(move || selector),
        )
    });
    let selector = format!("pull-request-record-{id}");
    event_card::event_card(CardTone::Neutral, cx)
        .debug_selector(move || selector)
        .child(header)
        .children(footer)
        .into_any_element()
}

/// The tone a wake asks for: something to fix while the watch goes on, or
/// only news.
fn tone(notice: &WatchNotice) -> CardTone {
    match notice {
        WatchNotice::Update { kinds, .. }
            if kinds.iter().any(|kind| {
                matches!(
                    kind,
                    WatchChangeKind::ChecksFailed | WatchChangeKind::MergeConflict
                )
            }) =>
        {
            CardTone::Warning
        }
        WatchNotice::Unreadable => CardTone::Warning,
        WatchNotice::Update { .. } | WatchNotice::Closed | WatchNotice::Merged => CardTone::Neutral,
    }
}

/// The glyph and colour of what weighs most in a wake.
fn glyph(notice: &WatchNotice, cx: &App) -> (IconName, gpui::Hsla) {
    let theme = cx.theme();
    match notice {
        WatchNotice::Update { kinds, .. } => {
            let has = |kind| kinds.contains(&kind);
            if has(WatchChangeKind::ChecksFailed) {
                (IconName::CircleX, theme.warning)
            } else if has(WatchChangeKind::MergeConflict) {
                (IconName::GitMergeConflict, theme.warning)
            } else if has(WatchChangeKind::ChecksPassed) {
                (IconName::CircleCheck, theme.success)
            } else if has(WatchChangeKind::NewComments) {
                (IconName::MessageSquare, theme.muted_foreground)
            } else {
                (IconName::EyeOff, theme.muted_foreground)
            }
        }
        WatchNotice::Merged => {
            let (glyph, color, _) =
                crate::pull_requests::appearance(PullRequestBadgeState::Merged, cx);
            (glyph, color)
        }
        WatchNotice::Closed => (IconName::GitPullRequestClosed, theme.muted_foreground),
        WatchNotice::Unreadable => (IconName::TriangleAlert, theme.warning),
    }
}

/// A watch's wake of the thread, one line naming the pull request and what
/// changed. The message it sent the model is the card's to open, whole.
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_card(
    key: &str,
    title: SharedString,
    notice: &WatchNotice,
    message: &str,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let key = key.to_owned();
    let tone = tone(notice);
    let (glyph, color) = glyph(notice, cx);
    let kinds = SharedString::from(crate::pull_requests::watch_notice_kinds(notice));
    let message = message.to_owned();
    let motion = motion.clone();
    crate::motion::Sampled::new(move |window, cx| {
        let muted = cx.theme().muted_foreground;
        let header = crate::material::accessible_clickable(
            event_card::header(true, cx),
            SharedString::from(format!("pull-request-update-toggle-{key}")),
            Role::Button,
            SharedString::from(format!("{title} · {kinds}")),
            cx,
        )
        .aria_expanded(expanded)
        .cursor_pointer()
        .on_click(on_toggle)
        .child(event_card::icon_tile(IconName::GitPullRequest, cx))
        .child(event_card::header_text(title.clone(), None, cx))
        .child(event_card::status(
            Some(Icon::new(glyph).text_color(color).into_any_element()),
            kinds.clone(),
            color,
            cx,
        ))
        .child(
            crate::widgets::Chevron::owned_icon(
                &motion,
                SharedString::from(format!("pull-request-update-chevron-{key}")),
                expanded,
                window,
                cx,
            )
            .text_color(muted),
        );
        let body = motion
            .shown(
                format!("pull-request-update-fold-{key}"),
                expanded.then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|fold| {
                fold.fold(
                    event_card::section(cx).child(
                        div()
                            .id(SharedString::from(format!(
                                "pull-request-update-body-{key}"
                            )))
                            .w_full()
                            .max_h(px(BODY_MAX_HEIGHT))
                            .overflow_y_scroll_area()
                            .child(
                                div()
                                    .w_full()
                                    .text_body(false, cx)
                                    .text_color(muted)
                                    .child(message.clone()),
                            ),
                    ),
                )
            });
        let selector = key.clone();
        event_card::event_card(tone, cx)
            .debug_selector(move || selector)
            .child(header)
            .children(body)
            .into_any_element()
    })
    .into_any_element()
}
