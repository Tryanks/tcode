//! What a pull request takes in the flow: the record of a call that linked
//! one to the thread, and a watch's wake, which is a card.

use crate::icon::{Icon, IconName};
use crate::material::event_card::{self, CardTone};
use crate::scroll::ScrollableElement as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_core::pull_request::PullRequestBadgeState;
use tcode_core::pull_request_watch::{WatchChangeKind, WatchNotice};

use super::changed_files::ClickHandler;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

const BODY_MAX_HEIGHT: f32 = 320.;

/// The record's event line and link, a half step over the caption size.
const RECORD_LINE: f32 = 12.5;
/// The time at the event line's end, a step under the line it ends.
const RECORD_TIME: f32 = 11.5;
const RECORD_GLYPH: f32 = 15.;
/// The text's inset from the rule down the record's left edge.
const RECORD_INSET: f32 = 12.;

/// What a linked pull request's record shows.
pub(crate) struct PullRequestRecord {
    /// What the call did.
    pub(crate) said: SharedString,
    /// When, in the reader's local time.
    pub(crate) at: Option<SharedString>,
    /// Its title, or `repository#number` while the thread has read none.
    pub(crate) title: SharedString,
    /// `repository #number`, or its host while the thread has read none.
    pub(crate) place: SharedString,
    /// Its head and base branches, once the thread has read them.
    pub(crate) branches: Option<(SharedString, SharedString)>,
    /// Opens it in the thread's Pull requests panel, `None` once the thread
    /// no longer shows it.
    pub(crate) on_open: Option<ClickHandler>,
}

/// The record of a call that linked a pull request, as it stood then: no
/// surface and no state, checks or review, which only the panel it opens
/// keeps current. A rule down its left edge sets it off from the flow.
pub(crate) fn record_card(id: &str, record: PullRequestRecord, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let typography = &theme.tokens.typography;
    let event = h_flex()
        .w_full()
        .min_w_0()
        .items_center()
        .gap(px(7.))
        .child(
            Icon::new(IconName::GitPullRequest)
                .size(px(RECORD_GLYPH))
                .text_color(theme.muted_foreground),
        )
        .child(
            div()
                .min_w_0()
                .child(crate::widgets::fade_text(record.said)),
        )
        .children(record.at.map(|at| {
            div()
                .flex_none()
                .ml_auto()
                .font_family(typography.mono.clone())
                .text_size(px(RECORD_TIME))
                .child(at)
        }));
    let title = div()
        .w_full()
        .min_w_0()
        .mt(px(4.))
        .mb(px(1.))
        .text_token(&typography.sm)
        .font_medium()
        .text_color(theme.foreground)
        .child(record.title);
    let meta = h_flex()
        .w_full()
        .min_w_0()
        .flex_wrap()
        .items_center()
        .gap(px(4.))
        .text_token(&typography.xs)
        .child(record.place)
        .when_some(record.branches, |meta, (head, base)| {
            meta.child("·").child(
                div()
                    .min_w_0()
                    .font_family(typography.mono.clone())
                    .child(SharedString::from(format!("{head} → {base}"))),
            )
        });
    let link = record.on_open.map(|on_open| {
        let selector = format!("pull-request-record-open-{id}");
        let label = SharedString::from(crate::tr!("pull_requests.record_open").into_owned());
        crate::material::accessible_clickable(
            div(),
            SharedString::from(selector.clone()),
            Role::Link,
            label.clone(),
            cx,
        )
        .mt(px(7.))
        .cursor_pointer()
        .text_color(theme.primary)
        .on_click(move |event, window, cx| on_open(event, window, cx))
        .debug_selector(move || selector)
        .child(SharedString::from(format!("{label} ›")))
    });
    let selector = format!("pull-request-record-{id}");
    v_flex()
        .w_full()
        .min_w_0()
        .py(px(2.))
        .pl(px(RECORD_INSET))
        .border_l_2()
        .border_color(theme.border)
        .text_token(&typography.xs)
        .text_size(px(RECORD_LINE))
        .text_color(theme.muted_foreground)
        .debug_selector(move || selector)
        .child(event)
        .child(title)
        .child(meta)
        .children(link)
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
