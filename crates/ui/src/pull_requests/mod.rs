use crate::{
    icon::{Icon, IconName, IconNamed as _},
    overlay::{Notification, NotificationType, OverlayExt as _},
    scroll::ScrollableElement as _,
    sizing::Sizable as _,
    store::{TopicKind, WorkspaceStore, observe_store_topics},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
        menu::{ContextMenuExt as _, CopyText, DropdownMenu as _, OpenUrl, PopupMenu},
    },
    window_state::WindowState,
};
use gpui::{
    Action, Anchor, AnyElement, App, AppContext as _, Context, Entity, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;
use tcode_core::{
    pull_request::{
        self, ChecksState, HostTerms, Mergeability, PullRequestBadgeState, PullRequestKey,
        PullRequestSnapshot, PullRequestSource, PullRequestState, PullRequestSyncError,
        ReviewDecision, ThreadPullRequestLink,
    },
    pull_request_watch::{WatchChangeKind, WatchNotice},
    ui::RightTab,
};
use tcode_protocol::Command;

mod compose;
mod conversation;
mod detail;
mod files;
mod lifecycle;
mod meta;
mod review;
mod stack;

pub use detail::PullRequestView;
pub use stack::present_result as present_stack_result;

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace=tcode_pull_requests,no_json)]
pub struct LinkUrl(pub String);
gpui::actions!(
    tcode_pull_requests,
    [OpenLinkDialog, OpenSourceControl, ShowAgentTools]
);

fn appearance(state: PullRequestBadgeState, cx: &App) -> (IconName, Hsla, &'static str) {
    match state {
        PullRequestBadgeState::Open => (
            IconName::GitPullRequest,
            cx.theme().success,
            "pull_requests.state_open",
        ),
        PullRequestBadgeState::Draft => (
            IconName::GitPullRequestDraft,
            cx.theme().muted_foreground,
            "pull_requests.state_draft",
        ),
        PullRequestBadgeState::Unknown => (
            IconName::GitPullRequest,
            cx.theme().muted_foreground,
            "pull_requests.state_unknown",
        ),
        PullRequestBadgeState::Merged => (
            IconName::GitMerge,
            cx.theme().info,
            "pull_requests.state_merged",
        ),
        PullRequestBadgeState::Closed => (
            IconName::GitPullRequestClosed,
            cx.theme().danger,
            "pull_requests.state_closed",
        ),
    }
}
fn row_state(
    snapshot: Option<&PullRequestSnapshot>,
    state: Option<PullRequestState>,
) -> PullRequestBadgeState {
    match (snapshot, state) {
        (None, Some(PullRequestState::Open)) => PullRequestBadgeState::Open,
        (None, Some(PullRequestState::Merged)) => PullRequestBadgeState::Merged,
        (None, Some(PullRequestState::Closed)) => PullRequestBadgeState::Closed,
        (snapshot, _) => PullRequestBadgeState::of(snapshot),
    }
}
fn single_number(links: &[ThreadPullRequestLink]) -> Option<u64> {
    links
        .iter()
        .find(|link| link.visible())
        .map(|link| link.key.number)
}
pub fn badge_label(links: &[ThreadPullRequestLink], cx: &App) -> Option<String> {
    let (state, count, stacked) = pull_request::badge(links)?;
    let (_, _, label) = appearance(state, cx);
    let number = single_number(links)?;
    let label = format!("{label}_lower");
    let label = if stacked {
        crate::tr!(
            "pull_requests.badge_stack",
            count = count.to_string(),
            state = crate::tr!(&label)
        )
        .into_owned()
    } else if count > 1 {
        crate::tr!(
            "pull_requests.badge_several",
            count = count.to_string(),
            state = crate::tr!(&label)
        )
        .into_owned()
    } else {
        crate::tr!(
            "pull_requests.badge_single",
            number = number.to_string(),
            state = crate::tr!(&label)
        )
        .into_owned()
    };
    Some(label)
}
pub fn badge(links: &[ThreadPullRequestLink], _size: f32, cx: &App) -> Option<AnyElement> {
    let (state, count, stacked) = pull_request::badge(links)?;
    let (glyph, color, _) = appearance(state, cx);
    let label = badge_label(links, cx)?;
    let number = single_number(links)?;
    let text = if stacked {
        count.to_string()
    } else if count > 1 {
        format!("+{count}")
    } else {
        format!("#{number}")
    };
    Some(
        crate::motion::Sampled::new(move |window, cx| {
            // A merge or a close recolours the badge where it stands.
            let color = gpui_base::motion::transition(
                "pull-request-badge-color",
                color,
                crate::motion::settle(),
                window,
                cx,
            );
            h_flex()
                .id("pull-request-badge")
                .aria_label(label)
                .flex_none()
                .items_center()
                .gap(cx.theme().tokens.spacing.xxs)
                .text_token(&cx.theme().tokens.typography.xs)
                .font_features(gpui::FontFeatures(std::sync::Arc::new(vec![(
                    "tnum".into(),
                    1,
                )])))
                .text_color(color)
                .child(Icon::new(if stacked { IconName::Layers } else { glyph }))
                .child(text)
                .into_any_element()
        })
        .into_any_element(),
    )
}
/// Every visible linked pull request's number in link order, each in its own state's glyph and
/// color: the thread's header and the phone's row show them all, where the sidebar's rows can
/// afford only [`badge`].
pub fn numbers(links: &[ThreadPullRequestLink], cx: &App) -> Option<AnyElement> {
    let label = badge_label(links, cx)?;
    let spacing = &cx.theme().tokens.spacing;
    Some(
        h_flex()
            .id("pull-request-numbers")
            .aria_label(label)
            .flex_none()
            .items_center()
            .gap(spacing.xs)
            .text_token(&cx.theme().tokens.typography.xs)
            .font_features(gpui::FontFeatures(std::sync::Arc::new(vec![(
                "tnum".into(),
                1,
            )])))
            .children(links.iter().filter(|link| link.visible()).map(|link| {
                let (glyph, color, _) =
                    appearance(PullRequestBadgeState::of(link.snapshot.as_ref()), cx);
                let (number, gap) = (link.key.number, spacing.xxs);
                crate::motion::Sampled::new(move |window, cx| {
                    let color = gpui_base::motion::transition(
                        SharedString::from(format!("pull-request-number-{number}")),
                        color,
                        crate::motion::settle(),
                        window,
                        cx,
                    );
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap(gap)
                        .text_color(color)
                        .child(Icon::new(glyph))
                        .child(format!("#{number}"))
                        .into_any_element()
                })
            }))
            .into_any_element(),
    )
}
pub fn sidebar_badge(
    links: &[ThreadPullRequestLink],
    id: &str,
    store: Entity<WorkspaceStore>,
    cx: &App,
) -> Option<AnyElement> {
    let badge = badge(links, 12., cx)?;
    let id = id.to_owned();
    let hover_links = links.to_vec();
    let hover_store = store.clone();
    let hover_id = id.clone();
    let trigger = Button::new(SharedString::from(format!("pr-badge-{id}")))
        .text()
        .aria_label(badge_label(links, cx).unwrap_or_default())
        .cursor_pointer()
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            store.update(cx, |store, cx| {
                store.select_session(id.clone());
                store.open_tab_for(&id, RightTab::PullRequests, cx);
            });
        })
        .child(badge);
    Some(
        gpui_base::HoverCard::new("pr-hover-card")
            .anchor(Anchor::TopLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                // gpui-base's HoverCard is an unstyled popup; the surface is the popover's.
                let spacing = &cx.theme().tokens.spacing;
                let mut rows = crate::material::overlay_contour(v_flex(), cx)
                    .rounded(crate::material::radius_overlay(cx))
                    .id("pr-mini-list")
                    .w(px(320.))
                    .p(spacing.sm)
                    .gap(spacing.xxs)
                    .text_token(&cx.theme().tokens.typography.xs)
                    .child(
                        div()
                            .text_caption(cx)
                            .child(crate::tr!("pull_requests.title")),
                    );
                let ordered: Vec<_> = pull_request::groups(&hover_links)
                    .into_iter()
                    .flat_map(|group| {
                        let caption = match group.kind {
                            pull_request::PullRequestGroupKind::Native => {
                                Some("pull_requests.stack_short")
                            }
                            pull_request::PullRequestGroupKind::Derived => {
                                Some("pull_requests.chain_short")
                            }
                            _ => None,
                        };
                        let count = group.links.len();
                        group
                            .links
                            .into_iter()
                            .enumerate()
                            .map(move |(depth, link)| {
                                (
                                    depth,
                                    &link.key,
                                    link.snapshot.as_ref(),
                                    if depth == 0 {
                                        caption.map(|caption| {
                                            crate::tr!(caption, count = count.to_string())
                                                .into_owned()
                                        })
                                    } else {
                                        None
                                    },
                                )
                            })
                    })
                    .collect();
                for (index, (depth, key, snapshot, caption)) in ordered.iter().take(8).enumerate() {
                    let (glyph, color, _) = appearance(row_state(*snapshot, None), cx);
                    let (store, id, open) = (hover_store.clone(), hover_id.clone(), (*key).clone());
                    // A hover-card miniature is denser than any list owner's row, and
                    // no row owner carries a tree indent.
                    rows =
                        rows.child(
                            h_flex()
                                .id(index)
                                .h(px(24.))
                                .pl(spacing.md * depth.min(&3).to_owned() as f32)
                                .gap(spacing.sm)
                                .items_center()
                                .cursor_pointer()
                                .hover(|s| s.bg(cx.theme().list_hover))
                                .on_click(move |_, _, cx| {
                                    store.update(cx, |store, cx| {
                                        store.select_session(id.clone());
                                        store.open_tab_for(&id, RightTab::PullRequests, cx);
                                        store.set_open_pull_request(&id, Some(open.clone()), cx);
                                    });
                                })
                                .child(Icon::new(glyph).text_color(color))
                                .child(div().child(format!("#{}", key.number)))
                                .child(div().flex_1().min_w_0().overflow_hidden().child(
                                    crate::widgets::fade_text(snapshot.map_or_else(
                                        || key.repository.clone(),
                                        |s| s.title.clone(),
                                    )),
                                ))
                                .children(
                                    caption.clone().map(|caption| {
                                        div().text_secondary(false, cx).child(caption)
                                    }),
                                ),
                        );
                }
                if ordered.len() > 8 {
                    rows = rows.child(
                        div().text_secondary(false, cx).child(
                            crate::tr!(
                                "pull_requests.more",
                                count = (ordered.len() - 8).to_string()
                            )
                            .into_owned(),
                        ),
                    );
                }
                rows
            })
            .into_any_element(),
    )
}

/// What a watch message reports, localized and joined for its notice and transcript row.
pub fn watch_notice_kinds(notice: &WatchNotice) -> String {
    let parts: Vec<_> = match notice {
        WatchNotice::Update { kinds, stopped } => kinds
            .iter()
            .map(|kind| match kind {
                WatchChangeKind::ChecksFailed => "pull_requests.kind_checks_failed",
                WatchChangeKind::ChecksPassed => "pull_requests.kind_checks_passed",
                WatchChangeKind::NewComments => "pull_requests.kind_new_comments",
                WatchChangeKind::MergeConflict => "pull_requests.kind_merge_conflict",
            })
            .chain(stopped.then_some("pull_requests.kind_stopped"))
            .collect(),
        WatchNotice::Closed => vec!["pull_requests.kind_closed"],
        WatchNotice::Merged => vec!["pull_requests.kind_merged"],
        WatchNotice::Unreadable => vec!["pull_requests.kind_unreadable"],
    };
    parts
        .into_iter()
        .map(|key| crate::tr!(key).into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The toast a watch's wake or end raises: its kind and message.
pub fn watch_notice_toast(number: u64, notice: &WatchNotice) -> (NotificationType, String) {
    let number = number.to_string();
    match notice {
        WatchNotice::Update {
            stopped: false,
            kinds,
        } => (
            if kinds.iter().any(|kind| {
                matches!(
                    kind,
                    WatchChangeKind::ChecksFailed | WatchChangeKind::MergeConflict
                )
            }) {
                NotificationType::Warning
            } else if kinds
                .iter()
                .all(|kind| *kind == WatchChangeKind::ChecksPassed)
            {
                NotificationType::Success
            } else {
                NotificationType::Info
            },
            crate::tr!(
                "pull_requests.watch_update",
                number = number,
                kinds = watch_notice_kinds(notice)
            )
            .into_owned(),
        ),
        WatchNotice::Update { stopped: true, .. } => (
            NotificationType::Info,
            crate::tr!("pull_requests.watch_comment_limit", number = number).into_owned(),
        ),
        WatchNotice::Closed => (
            NotificationType::Info,
            crate::tr!("pull_requests.watch_closed", number = number).into_owned(),
        ),
        WatchNotice::Merged => (
            NotificationType::Success,
            crate::tr!("pull_requests.watch_merged", number = number).into_owned(),
        ),
        WatchNotice::Unreadable => (
            NotificationType::Error,
            crate::tr!("pull_requests.watch_unreadable", number = number).into_owned(),
        ),
    }
}

/// What made the thread show the row's pull request; a row without a link is a layer of a
/// stack.
fn source(link: Option<&ThreadPullRequestLink>) -> &'static str {
    match link.map(|link| link.source) {
        Some(PullRequestSource::Manual) => "pull_requests.source_manual",
        Some(PullRequestSource::Agent) => "pull_requests.source_agent",
        Some(PullRequestSource::Opened) => "pull_requests.source_opened",
        Some(PullRequestSource::Stack) => "pull_requests.source_stack",
        Some(PullRequestSource::Dismissed) => "pull_requests.source_dismissed",
        None => "pull_requests.source_not_linked",
    }
}
/// When a rate limit lifts, as the notices say it: ahead, or soon once its time has passed.
pub(super) fn resumes(retry_at: u64) -> String {
    let now = tcode_core::project::now_secs();
    if retry_at <= now {
        crate::tr!("pull_requests.notice_soon").into_owned()
    } else {
        crate::time::humanize_in(retry_at - now)
    }
}

fn sync_error(link: &ThreadPullRequestLink, host_name: &str) -> String {
    match link.sync_error.as_ref() {
        Some(PullRequestSyncError::NoCredential | PullRequestSyncError::HostDisabled) => {
            crate::tr!(
                "pull_requests.notice_no_credential",
                host = link.key.host.clone()
            )
            .into_owned()
        }
        Some(PullRequestSyncError::RateLimited { retry_at }) => crate::tr!(
            "pull_requests.notice_rate_limited",
            host_name = host_name,
            when = resumes(*retry_at)
        )
        .into_owned(),
        Some(PullRequestSyncError::NotFound) => crate::tr!(
            "pull_requests.error_not_found",
            reference = format!("#{}", link.key.number)
        )
        .into_owned(),
        Some(PullRequestSyncError::Failed) => {
            crate::tr!("pull_requests.notice_failed").into_owned()
        }
        None => crate::tr!("pull_requests.waiting_for_host").into_owned(),
    }
}
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace=tcode_pull_requests,no_json)]
struct ChangeLink {
    key: PullRequestKey,
    url: String,
    linking: bool,
}

#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace=tcode_pull_requests,no_json)]
struct ChangeWatch {
    key: PullRequestKey,
    watching: bool,
}

fn change_watch<T: 'static>(
    store: &Entity<WorkspaceStore>,
    action: &ChangeWatch,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let Some(session_id) = store.read(cx).active_session_id() else {
        return;
    };
    let number = action.key.number;
    let task = store.update(cx, |store, cx| {
        store.command(
            Command::WatchPullRequest {
                session_id,
                key: action.key.clone(),
                watching: action.watching,
            },
            cx,
        )
    });
    cx.spawn_in(window, async move |_, cx| {
        if let Err(error) = task.await {
            _ = cx.update(|window, cx| {
                window.push_notification(
                    Notification::warning(
                        crate::tr!(
                            "pull_requests.watch_failed",
                            number = number.to_string(),
                            reason = error.message
                        )
                        .into_owned(),
                    ),
                    cx,
                )
            });
        }
    })
    .detach();
}
/// Pauses or resumes every watch of the active thread.
fn set_watches_paused(
    store: &Entity<WorkspaceStore>,
    paused: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(session_id) = store.read(cx).active_session_id() else {
        return;
    };
    let task = store.update(cx, |store, cx| {
        store.command(
            Command::SetPullRequestWatchesPaused { session_id, paused },
            cx,
        )
    });
    window
        .spawn(cx, async move |cx| {
            if let Err(error) = task.await {
                _ = cx.update(|window, cx| {
                    window.push_notification(
                        Notification::warning(
                            crate::tr!("pull_requests.monitoring_failed", reason = error.message)
                                .into_owned(),
                        ),
                        cx,
                    )
                });
            }
        })
        .detach();
}
fn change_link<T: 'static>(
    store: &Entity<WorkspaceStore>,
    action: &ChangeLink,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let Some(session_id) = store.read(cx).active_session_id() else {
        return;
    };
    let command = if action.linking {
        Command::LinkPullRequest {
            session_id,
            reference: action.url.clone(),
        }
    } else {
        Command::UnlinkPullRequest {
            session_id,
            key: action.key.clone(),
        }
    };
    let task = store.update(cx, |store, cx| store.command(command, cx));
    cx.spawn_in(window, async move |_, cx| {
        if let Err(error) = task.await {
            _ = cx.update(|window, cx| {
                window.push_notification(
                    Notification::error(
                        crate::tr!("pull_requests.update_failed", reason = error.message)
                            .into_owned(),
                    ),
                    cx,
                )
            });
        }
    })
    .detach();
}

/// What Tcode says about the pull request host at `host`, by the kind settings give it.
pub(crate) fn host_terms(store: &WorkspaceStore, host: &str) -> &'static HostTerms {
    store.settings().source_control.kind(host).terms()
}

/// The pull request host at `host` as a person reads it.
pub(crate) fn host_name(store: &WorkspaceStore, host: &str) -> String {
    host_terms(store, host).display_name(host)
}

/// The row's menu: Open on the host, Copy link, the watch, what can become of the pull request,
/// and the link condition.
type RowMenu = std::rc::Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

fn row_menu(
    key: PullRequestKey,
    url: String,
    host_name: String,
    link: Option<&ThreadPullRequestLink>,
    watchable: bool,
    offer: impl Fn(&App) -> Option<lifecycle::Offer> + 'static,
) -> RowMenu {
    let visible = link.is_some_and(|link| link.visible());
    let watched = link.is_some_and(|link| link.visible() && link.watch.is_some());
    // A merged pull request cannot reopen; a saved closed one may have.
    let can_watch = visible
        && !watched
        && watchable
        && link
            .and_then(|link| link.snapshot.as_ref())
            .is_none_or(|snapshot| snapshot.state != PullRequestState::Merged);
    let action = ChangeLink {
        key: key.clone(),
        url: url.clone(),
        linking: !visible,
    };
    let source_is_stack = link.is_some_and(|link| link.source == PullRequestSource::Stack);
    std::rc::Rc::new(
        move |menu: PopupMenu, _: &mut Window, cx: &mut Context<PopupMenu>| {
            menu.menu_with_icon(
                crate::tr!("pull_requests.open_on_host", host_name = &host_name).into_owned(),
                IconName::ExternalLink,
                Box::new(OpenUrl(url.clone())),
            )
            .menu_with_icon(
                crate::tr!("pull_requests.copy_link").into_owned(),
                IconName::Copy,
                Box::new(CopyText(url.clone())),
            )
            .when(watched || can_watch, |menu| {
                let (label, icon) = if watched {
                    ("pull_requests.stop_watching", IconName::EyeOff)
                } else {
                    ("pull_requests.watch", IconName::Eye)
                };
                menu.menu_with_icon(
                    crate::tr!(label).into_owned(),
                    icon,
                    Box::new(ChangeWatch {
                        key: key.clone(),
                        watching: !watched,
                    }),
                )
            })
            .when_some(offer(cx), |menu, offer| offer.menu(menu))
            .separator()
            .menu_with_icon(
                crate::tr!(if action.linking {
                    "pull_requests.relink"
                } else if source_is_stack {
                    "pull_requests.dismiss"
                } else {
                    "pull_requests.unlink"
                })
                .into_owned(),
                if action.linking {
                    IconName::Link
                } else {
                    IconName::Unlink
                },
                Box::new(action.clone()),
            )
        },
    )
}

/// Opens a pull request of the active thread here: inside the tab in a wide window, as a page
/// over the thread on a phone.
fn open_pull_request(
    store: &Entity<WorkspaceStore>,
    window_state: &Entity<WindowState>,
    key: PullRequestKey,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(session) = store.read(cx).active_session_id() else {
        return;
    };
    store.update(cx, |store, cx| {
        store.set_open_pull_request(&session, Some(key), cx)
    });
    if window_state.read(cx).compact {
        window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
        window_state.update(cx, |state, cx| {
            state.go(crate::window_state::Destination::PullRequest, cx)
        });
    }
}

struct PullRequestRow<'a> {
    key: PullRequestKey,
    url: String,
    link: Option<&'a ThreadPullRequestLink>,
    branch: Option<&'a str>,
    state: Option<PullRequestState>,
    position: Option<(usize, usize)>,
    depth: usize,
    /// The thread may start a watch: not settled, archived or a subagent.
    watchable: bool,
}
pub struct PullRequestsPanel {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    scroll: ScrollHandle,
    /// The watched row whose eye the pointer is over, which then shows what a click does.
    eye_hovered: Option<PullRequestKey>,
    agent_tools_open: bool,
    detail: Entity<PullRequestView>,
    /// The pull request the active thread had open when last drawn.
    shown: Option<PullRequestKey>,
    _subscriptions: [Subscription; 3],
}
impl PullRequestsPanel {
    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = [
            observe_store_topics(
                &store,
                &[
                    TopicKind::Index,
                    TopicKind::SessionStatus,
                    TopicKind::ActiveSession,
                ],
                cx,
            ),
            cx.observe(&window_state, |_, _, cx| cx.notify()),
            // Opening or leaving a pull request is client state, not a host topic.
            cx.observe(&store, |this: &mut Self, store, cx| {
                let store = store.read(cx);
                let open = store
                    .active_session_id()
                    .and_then(|id| store.open_pull_request(&id).cloned());
                if this.shown != open {
                    this.shown = open;
                    cx.notify();
                }
            }),
        ];
        let detail = cx.new(|cx| PullRequestView::new(store.clone(), window_state.clone(), cx));
        Self {
            store,
            window_state,
            scroll: ScrollHandle::new(),
            eye_hovered: None,
            agent_tools_open: false,
            detail,
            shown: None,
            _subscriptions: subscriptions,
        }
    }

    /// The pull request view this list opens into.
    pub fn detail(&self) -> Entity<PullRequestView> {
        self.detail.clone()
    }
    fn change_watch(&mut self, action: &ChangeWatch, window: &mut Window, cx: &mut Context<Self>) {
        change_watch(&self.store, action, window, cx);
    }
    fn change_link(&mut self, action: &ChangeLink, window: &mut Window, cx: &mut Context<Self>) {
        change_link(&self.store, action, window, cx);
    }
    /// A list row offers what its link shows, as its menu opens; the host reads the rest
    /// before it writes.
    fn offer(&self, key: &PullRequestKey) -> impl Fn(&App) -> Option<lifecycle::Offer> + 'static {
        let (store, detail, key) = (self.store.clone(), self.detail.clone(), key.clone());
        move |cx| {
            let store = store.read(cx);
            if store
                .session_status()
                .is_none_or(|status| status.conversation_read_only)
            {
                return None;
            }
            let session = store.active_session_id()?;
            let busy = detail.read(cx).lifecycle_busy(&session, &key);
            let operations = store
                .thread_meta(&session)
                .map_or(&[][..], |meta| meta.pull_request_operations.as_slice());
            lifecycle::Offer::new(
                &key,
                store.pull_requests(&session),
                operations,
                None,
                None,
                None,
                busy,
            )
        }
    }
    fn run_lifecycle(
        &mut self,
        action: &lifecycle::RunLifecycle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.store.read(cx).active_session_id() else {
            return;
        };
        let target = self.detail.update(cx, |detail, cx| {
            detail.lifecycle_target(&session, &action.key, false, cx)
        });
        if let Some(target) = target {
            target.run(action.kind, window, cx);
        }
    }
    fn row(
        &self,
        row: PullRequestRow<'_>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let PullRequestRow {
            key,
            url,
            link,
            branch,
            state,
            position,
            depth,
            watchable,
        } = row;
        let visible = link.is_some_and(|link| link.visible());
        let watched = link.is_some_and(|link| link.visible() && link.watch.is_some());
        let snapshot = link
            .filter(|link| link.visible())
            .and_then(|link| link.snapshot.as_ref());
        let (glyph, color, state_label) = appearance(
            row_state(link.and_then(|link| link.snapshot.as_ref()), state),
            cx,
        );
        let row_id = SharedString::from(format!(
            "pr-row-{}-{}-{}",
            key.host, key.repository, key.number
        ));
        let row_key = gpui::ElementId::from(row_id.clone());
        let color = if visible {
            color
        } else {
            cx.theme().muted_foreground
        };
        let source = crate::tr!(source(link)).into_owned();
        let title: String = snapshot.map(|s| s.title.clone()).unwrap_or_else(|| {
            branch
                .map(str::to_owned)
                .unwrap_or_else(|| key.repository.clone())
        });
        let layer = position.map(|(index, count)| {
            crate::tr!(
                "pull_requests.layer_position",
                index = index.to_string(),
                count = count.to_string()
            )
            .into_owned()
        });
        let dot = || div().flex_none().child("·");
        // The layer and the diff stat never give way; the parts before them do.
        let tail_layer = |line: gpui::Div, layer: Option<String>| {
            line.when_some(layer, |line, layer| {
                line.child(dot()).child(div().flex_none().child(layer))
            })
        };
        let mut detail = vec![source.clone()];
        // The line a row shows before its first sync, which the synced line cross-fades over.
        let mut placeholder = None;
        let line_two = if let Some(snapshot) = snapshot {
            let author = snapshot.author.as_ref().map(|author| author.login.clone());
            let branches = format!("{} → {}", snapshot.head_branch, snapshot.base_branch);
            // A host that counts no lines shows no stat rather than zeros.
            let stat = snapshot
                .additions
                .zip(snapshot.deletions)
                .map(|(additions, deletions)| (format!("+{additions}"), format!("−{deletions}")));
            detail.extend(author.clone());
            detail.push(branches.clone());
            detail.retain(|part| !part.is_empty());
            detail.extend(layer.clone());
            detail.extend(stat.as_ref().map(|stat| format!("{} {}", stat.0, stat.1)));
            detail.retain(|part| !part.is_empty());
            // Spacing is each part's leading margin, so a part that gives way takes its
            // separator and its spacing with it.
            let sep = || div().flex_none().ml_1().child("·");
            let mono = cx.theme().mono_font_family.clone();
            let typography = &cx.theme().tokens.typography;
            let line_height = if compact {
                typography.sm.line_height
            } else {
                typography.xs.line_height
            };
            h_flex()
                .items_center()
                .min_w_0()
                .overflow_hidden()
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(source.clone())),
                )
                // The author gives way first, whole: a line too narrow for it wraps it onto
                // a second line this one-line box clips. The box is exactly one line of
                // line 2's text tall; stretching
                // it to the row would not do, as a stretched box takes its wrapped content's
                // two lines. gpui has no setter for a shrink value, hence the reach into the
                // style.
                .when_some(author, |line, author| {
                    line.child(
                        h_flex()
                            .flex_wrap()
                            .h(line_height)
                            .min_w_0()
                            .overflow_hidden()
                            .map(|mut group| {
                                group.style().flex_shrink = Some(10000.);
                                group
                            })
                            // An empty first item, so the author wraps rather than overflows.
                            .child(div().h_full())
                            .child(
                                h_flex()
                                    .flex_none()
                                    .child(sep())
                                    .child(div().ml_1().child(author)),
                            ),
                    )
                })
                // gpui-base has no middle truncation; the row tooltip holds the full pair.
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .map(|mut branches| {
                            branches.style().flex_shrink = Some(100.);
                            branches
                        })
                        .font_family(mono.clone())
                        .child(crate::widgets::fade_text(format!(" · {branches}"))),
                )
                .when_some(layer.clone(), |line, layer| {
                    line.child(sep())
                        .child(div().flex_none().ml_1().child(layer))
                })
                .when_some(stat, |line, (additions, deletions)| {
                    line.child(sep())
                        .child(
                            div()
                                .flex_none()
                                .ml_1()
                                .font_family(mono.clone())
                                .text_color(cx.theme().success)
                                .child(additions),
                        )
                        .child(
                            div()
                                .flex_none()
                                .ml_1()
                                .font_family(mono)
                                .text_color(cx.theme().danger)
                                .child(deletions),
                        )
                })
                .into_any_element()
        } else {
            let layer = if let Some(link) = link.filter(|_| visible) {
                detail.insert(
                    0,
                    sync_error(link, &host_name(self.store.read(cx), &key.host)),
                );
                None
            } else {
                detail.push(crate::tr!(&format!("{state_label}_lower")).into_owned());
                layer.clone()
            };
            let lead = detail.join(" · ");
            placeholder = Some(lead.clone());
            detail.extend(layer.clone());
            let line = h_flex()
                .gap_1()
                .items_center()
                .min_w_0()
                .overflow_hidden()
                .when(
                    link.is_some_and(|link| link.source == PullRequestSource::Dismissed),
                    |line| line.child(Icon::new(IconName::Unlink)),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(lead)),
                );
            tail_layer(line, layer).into_any_element()
        };
        let detail = detail.join(" · ");
        let title_text = title.clone();
        // The first sync cross-fades the synced line over the placeholder line before it.
        let line_two = std::cell::RefCell::new(Some(line_two));
        let line_two = crate::motion::cross_fade(
            (row_key.clone(), "line"),
            placeholder.clone(),
            window,
            cx,
            |shown, _, _| match shown {
                _ if *shown == placeholder => line_two
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(|| div().into_any_element()),
                Some(lead) => div()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(lead.clone()))
                    .into_any_element(),
                None => div().into_any_element(),
            },
        );
        let title = crate::motion::cross_fade(
            (row_key.clone(), "title"),
            (title.clone(), snapshot.is_none()),
            window,
            cx,
            |(title, mono), _, cx| {
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .when(*mono, |title| {
                        title.font_family(cx.theme().mono_font_family.clone())
                    })
                    .child(crate::widgets::fade_text(title.clone()))
                    .into_any_element()
            },
        );
        let title_color = gpui_base::motion::transition(
            (row_key.clone(), "title-color"),
            if visible {
                cx.theme().foreground
            } else {
                cx.theme().muted_foreground
            },
            crate::motion::settle(),
            window,
            cx,
        );
        let menu = row_menu(
            key.clone(),
            url.clone(),
            host_name(self.store.read(cx), &key.host),
            link,
            watchable,
            self.offer(&key),
        );
        let mut signals = h_flex().gap_1().flex_none().items_center();
        // The eye leads while the pull request is open or not yet read; the host ends the watch
        // once it merges or closes.
        let eye = crate::motion::shown(
            (row_key.clone(), "eye"),
            (watched && snapshot.is_none_or(|s| s.state == PullRequestState::Open)).then_some(()),
            window,
            cx,
        );
        if let Some(eye) = eye {
            let store = self.store.read(cx);
            let paused = store
                .active_session_id()
                .as_deref()
                .and_then(|id| store.thread_meta(id))
                .is_some_and(|meta| meta.pull_request_watches_paused);
            let tooltip = crate::tr!(if paused {
                "pull_requests.watching_paused_tooltip"
            } else {
                "pull_requests.watching_tooltip"
            })
            .into_owned();
            let eye_color = gpui_base::motion::transition(
                (row_key.clone(), "eye-color"),
                if self.eye_hovered.as_ref() == Some(&key) {
                    cx.theme().foreground
                } else {
                    cx.theme().muted_foreground
                },
                crate::motion::settle(),
                window,
                cx,
            );
            signals = signals.child(eye.fade(if compact {
                // A glyph is below touch size: the more menu stops the watch on a phone.
                Icon::new(IconName::Eye)
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element()
            } else {
                let hovered = self.eye_hovered.as_ref() == Some(&key);
                let hover_key = key.clone();
                let stop = ChangeWatch {
                    key: key.clone(),
                    watching: false,
                };
                Button::new(SharedString::from(format!(
                    "pr-eye-{}-{}-{}",
                    key.host, key.repository, key.number
                )))
                .ghost()
                .xsmall()
                .icon(
                    Icon::new(if hovered {
                        IconName::EyeOff
                    } else {
                        IconName::Eye
                    })
                    .text_color(eye_color),
                )
                .aria_label(
                    crate::tr!(
                        "pull_requests.stop_watching_number",
                        number = key.number.to_string()
                    )
                    .into_owned(),
                )
                .tooltip(tooltip)
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    let key = hovered.then(|| hover_key.clone());
                    if this.eye_hovered != key {
                        this.eye_hovered = key;
                        cx.notify();
                    }
                }))
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    window.dispatch_action(Box::new(stop.clone()), cx);
                })
                .into_any_element()
            }));
        }
        let open_snapshot = snapshot.filter(|s| s.state == PullRequestState::Open);
        if let Some(check) = crate::motion::shown(
            (row_key.clone(), "checks"),
            open_snapshot.and_then(|s| s.checks_state),
            window,
            cx,
        ) {
            let (icon, color, label) = match check.value {
                ChecksState::Passing => (
                    IconName::CircleCheck,
                    cx.theme().success,
                    "pull_requests.checks_passing",
                ),
                ChecksState::Failing => (
                    IconName::CircleX,
                    cx.theme().danger,
                    "pull_requests.checks_failing",
                ),
                ChecksState::Pending => (
                    IconName::CircleDashed,
                    cx.theme().warning,
                    "pull_requests.checks_pending",
                ),
            };
            signals = signals.child(
                check.fade(
                    div()
                        .id("pr-signal-checks")
                        .child(crate::motion::status_icon(
                            (row_key.clone(), "checks-icon"),
                            icon,
                            color,
                            window,
                            cx,
                        ))
                        .tooltip(move |window, cx| {
                            crate::widgets::tooltip::Tooltip::new(crate::tr!(label).into_owned())
                                .build(window, cx)
                        }),
                ),
            );
        }
        if let Some(review) = crate::motion::shown(
            (row_key.clone(), "review"),
            open_snapshot.and_then(|s| s.review_decision),
            window,
            cx,
        ) {
            let (icon, color, label) = match review.value {
                ReviewDecision::Approved => (
                    IconName::BadgeCheck,
                    cx.theme().success,
                    "pull_requests.review_approved",
                ),
                ReviewDecision::ChangesRequested => (
                    IconName::MessageSquareWarning,
                    cx.theme().danger,
                    "pull_requests.review_changes_requested",
                ),
                ReviewDecision::Required => (
                    IconName::MessageSquareMore,
                    cx.theme().muted_foreground,
                    "pull_requests.review_required",
                ),
            };
            signals = signals.child(
                review.fade(
                    div()
                        .id("pr-signal-review")
                        .child(crate::motion::status_icon(
                            (row_key.clone(), "review-icon"),
                            icon,
                            color,
                            window,
                            cx,
                        ))
                        .tooltip(move |window, cx| {
                            crate::widgets::tooltip::Tooltip::new(crate::tr!(label).into_owned())
                                .build(window, cx)
                        }),
                ),
            );
        }
        if let Some(conflict) = crate::motion::shown(
            (row_key.clone(), "conflict"),
            open_snapshot
                .filter(|s| s.mergeability == Mergeability::Conflicting)
                .map(|s| s.base_branch.clone()),
            window,
            cx,
        ) {
            let base = conflict.value.clone();
            signals = signals.child(
                conflict.fade(
                    div()
                        .id("pr-conflict")
                        .child(Icon::new(IconName::GitMergeConflict).text_color(cx.theme().warning))
                        .tooltip(move |window, cx| {
                            crate::widgets::tooltip::Tooltip::new(
                                crate::tr!("pull_requests.conflict", base = base.clone())
                                    .into_owned(),
                            )
                            .build(window, cx)
                        }),
                ),
            );
        }
        // The stack's view of the row: a layer below that blocks merging it, and the stack's
        // write running over it.
        let (blocker, running) = {
            let store = self.store.read(cx);
            let session = store.active_session_id();
            let links = session
                .as_deref()
                .map_or(&[][..], |id| store.pull_requests(id));
            let operations = session
                .as_deref()
                .and_then(|id| store.thread_meta(id))
                .map_or(&[][..], |meta| meta.pull_request_operations.as_slice());
            let running = pull_request::stack_operation(operations, links, &key)
                .filter(|operation| operation.covers(key.number))
                .map(|operation| stack::chip_words(operation, cx));
            (stack::row_blocker(links, &key), running)
        };
        if let Some(kept) = crate::motion::shown((row_key.clone(), "blocker"), blocker, window, cx)
        {
            let blocker = kept.value.clone();
            signals = signals.child(
                kept.fade(
                    div()
                        .id("pr-signal-blocked")
                        .child(Icon::new(IconName::Lock).text_color(cx.theme().muted_foreground))
                        .tooltip(move |window, cx| {
                            crate::widgets::tooltip::Tooltip::new(blocker.clone()).build(window, cx)
                        }),
                ),
            );
        }
        // A merge, a close or an unlink recolours the row where it stands.
        let glyph =
            crate::motion::status_icon((row_key.clone(), "state"), glyph, color, window, cx);
        let draft = crate::motion::shown(
            (row_key.clone(), "draft"),
            snapshot
                .is_some_and(|snapshot| {
                    snapshot.is_draft && snapshot.state == PullRequestState::Open
                })
                .then_some(()),
            window,
            cx,
        );
        // The running write takes the relative time's place, and gives it back.
        let trailing = (!compact).then(|| match running {
            Some((icon, color, _, tooltip)) => (Some((icon.path(), color, tooltip)), None),
            None => (
                None,
                snapshot
                    .and_then(|snapshot| {
                        chrono::DateTime::parse_from_rfc3339(&snapshot.updated_at).ok()
                    })
                    .map(|updated| {
                        crate::time::humanize_ago(
                            tcode_core::project::now_secs()
                                .saturating_sub(updated.timestamp().max(0) as u64),
                        )
                    }),
            ),
        });
        let trailing = trailing.map(|trailing| {
            let group = row_id.clone();
            crate::motion::cross_fade(
                (row_key.clone(), "trailing"),
                trailing,
                window,
                cx,
                move |(running, updated), _, cx| match running {
                    Some((icon, color, tooltip)) => {
                        let tooltip = tooltip.clone();
                        div()
                            .id("pr-signal-operation")
                            .group_hover(group.clone(), |signal| signal.invisible())
                            .child(Icon::empty().path(icon.clone()).text_color(*color))
                            .tooltip(move |window, cx| {
                                crate::widgets::tooltip::Tooltip::new(tooltip.clone())
                                    .build(window, cx)
                            })
                            .into_any_element()
                    }
                    None => div()
                        .text_secondary(false, cx)
                        .group_hover(group.clone(), |time| time.invisible())
                        .children(updated.clone())
                        .into_any_element(),
                },
            )
        });
        let label = SharedString::from(format!("#{} {}", key.number, title_text));
        let spacing = &cx.theme().tokens.spacing;
        let inset = if compact {
            px(crate::material::COMPACT_PAGE_INSET)
        } else {
            px(crate::material::CARD_INSET)
        };
        let row = crate::material::panel_row(row_id.clone(), label, compact, cx)
            .group(row_id.clone())
            .min_w_0()
            // `panel_row` has no tree indent: a stacked layer steps in one level per depth.
            .pl(inset + spacing.lg * depth.min(3) as f32)
            .items_start()
            .cursor_pointer()
            .on_click({
                let store = self.store.clone();
                let window_state = self.window_state.clone();
                let key = key.clone();
                move |_, window, cx| {
                    open_pull_request(&store, &window_state, key.clone(), window, cx)
                }
            })
            .when(depth > 0, |row| {
                row.child(div().w(px(1.)).h(spacing.xl).bg(cx.theme().border))
            })
            .child(div().text_body(compact, cx).child(glyph))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(spacing.xxs)
                    .child(
                        h_flex()
                            .gap(spacing.sm)
                            .items_center()
                            .child(
                                div()
                                    .text_meta_mono(compact, cx)
                                    .text_color(cx.theme().foreground)
                                    .id("pr-number")
                                    .tooltip({
                                        let label = crate::tr!(
                                            "pull_requests.number_tooltip",
                                            source = source.clone(),
                                            ago = crate::time::humanize_ago(
                                                tcode_core::project::now_secs().saturating_sub(
                                                    link.and_then(|link| link.linked_at)
                                                        .unwrap_or_default()
                                                )
                                            )
                                        )
                                        .into_owned();
                                        move |window, cx| {
                                            crate::widgets::tooltip::Tooltip::new(label.clone())
                                                .build(window, cx)
                                        }
                                    })
                                    .child(format!("#{}", key.number)),
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_body(compact, cx)
                                    .text_color(title_color)
                                    .child(title),
                            )
                            .when_some(draft, |row, draft| {
                                row.child(
                                    draft.fade(
                                        div()
                                            .text_secondary(false, cx)
                                            .child(crate::tr!("pull_requests.draft")),
                                    ),
                                )
                            })
                            .child(signals),
                    )
                    .child(
                        div()
                            .id("pr-detail")
                            .relative()
                            .min_w_0()
                            .text_secondary(compact, cx)
                            .tooltip(move |window, cx| {
                                crate::widgets::tooltip::Tooltip::new(detail.clone())
                                    .build(window, cx)
                            })
                            .child(line_two),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_none()
                    // The desktop slot holds the relative time under the hover-revealed menu
                    // button; no owner sizes that overlay.
                    .when(!compact, |slot| slot.min_w(px(36.)).min_h(spacing.xl))
                    .children(trailing)
                    .child(
                        Button::new(SharedString::from(format!(
                            "pr-menu-{}-{}-{}",
                            key.host, key.repository, key.number
                        )))
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::Ellipsis)
                        .aria_label(
                            crate::tr!(
                                "pull_requests.actions_for",
                                number = key.number.to_string()
                            )
                            .into_owned(),
                        )
                        .tooltip(
                            crate::tr!(
                                "pull_requests.actions_for",
                                number = key.number.to_string()
                            )
                            .into_owned(),
                        )
                        .when(!compact, |button| {
                            button
                                .absolute()
                                .right_0()
                                .top_0()
                                .opacity(0.)
                                .group_hover(row_id.clone(), |button| button.opacity(1.))
                                .focus(|button| button.opacity(1.))
                        })
                        .dropdown_menu({
                            let menu = menu.clone();
                            move |state, window, cx| (menu)(state, window, cx)
                        })
                        .touch(compact),
                    ),
            );

        row.context_menu(move |state, window, cx| (menu)(state, window, cx))
            .touch(compact)
            .into_any_element()
    }
}
/// Every tool and text Tcode injects into a thread's provider, grouped by the
/// server that offers it.
pub(crate) fn agent_sees_body(
    tools: &tcode_protocol::InjectedTools,
    id: &str,
    cx: &App,
) -> gpui::Div {
    let muted = cx.theme().muted_foreground;
    let caption = |text: String| div().text_caption(cx).child(text);
    let tool = |(name, description): &(String, String)| {
        v_flex()
            .gap_0p5()
            .child(
                div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(name.clone()),
            )
            .child(div().text_color(muted).child(description.clone()))
    };
    let mut body = v_flex()
        .gap(cx.theme().tokens.spacing.sm)
        .text_token(&cx.theme().tokens.typography.xs)
        .child(caption(
            crate::tr!("pull_requests.agent_tools").into_owned(),
        ));
    let listed: usize = tools.servers.iter().map(|server| server.tools).sum();
    if listed == tools.tools.len() {
        let mut rest = tools.tools.as_slice();
        for server in &tools.servers {
            let (own, next) = rest.split_at(server.tools);
            rest = next;
            body = body
                .child(
                    div()
                        .pt(cx.theme().tokens.spacing.xs)
                        .text_caption(cx)
                        .child(crate::tr!(
                            "pull_requests.agent_tools_server",
                            name = server.name.as_str()
                        )),
                )
                .children(own.iter().map(tool));
        }
    } else {
        body = body.children(tools.tools.iter().map(tool));
    }
    body.when(!tools.attached, |body| {
        body.child(caption(
            crate::tr!("pull_requests.agent_not_attached").into_owned(),
        ))
    })
    .when(!tools.instructions.is_empty(), |body| {
        body.child(caption(
            crate::tr!("pull_requests.agent_instructions").into_owned(),
        ))
    })
    .children(
        tools
            .instructions
            .iter()
            .enumerate()
            .map(|(index, injected)| {
                use agent::InstructionChannel;
                use tcode_protocol::InjectedChannel;
                let channel = match &injected.channel {
                    InjectedChannel::Linking(InstructionChannel::SessionAppend) => {
                        crate::tr!("pull_requests.channel_system").into_owned()
                    }
                    InjectedChannel::Linking(InstructionChannel::PerPromptSystem) => {
                        crate::tr!("pull_requests.channel_system_per_prompt").into_owned()
                    }
                    InjectedChannel::Linking(InstructionChannel::DeveloperContext) => {
                        crate::tr!("pull_requests.channel_developer_context").into_owned()
                    }
                    InjectedChannel::Linking(
                        InstructionChannel::PerPromptUserBlock | InstructionChannel::None,
                    ) => crate::tr!("pull_requests.channel_per_turn_block").into_owned(),
                    InjectedChannel::McpServer(name) => {
                        crate::tr!("pull_requests.channel_mcp_server", name = name).into_owned()
                    }
                    InjectedChannel::DeveloperInstructions => {
                        crate::tr!("pull_requests.channel_developer_instructions").into_owned()
                    }
                };
                v_flex()
                    .gap_0p5()
                    .child(div().text_caption(cx).child(channel))
                    .child(crate::chat::components::disclosure::disclosure_body(
                        &format!("{id}-instructions-{index}"),
                        &injected.text,
                        cx,
                    ))
            }),
    )
}

/// The active thread's [`agent_sees_body`] in a dialog, for a thread with no
/// pull request panel to hold it.
pub(crate) fn open_agent_sees(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| AgentSees::new(store, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        let view = view.clone();
        dialog
            .title(crate::tr!("pull_requests.agent_sees").into_owned())
            .content(move |content, _, _| content.child(view.clone()))
    });
}

struct AgentSees {
    store: Entity<WorkspaceStore>,
    _subscription: gpui::Subscription,
}

impl AgentSees {
    fn new(store: Entity<WorkspaceStore>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            _subscription: subscription,
        }
    }
}

impl Render for AgentSees {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tools = self
            .store
            .read(cx)
            .session_status()
            .and_then(|status| status.injected_tools.clone());
        div()
            .id("agent-sees-scroll")
            .w_full()
            .max_h(px(crate::overlay::DIALOG_LIST_HEIGHT))
            .overflow_y_scroll_area()
            .children(tools.map(|tools| agent_sees_body(&tools, "agent-sees", cx)))
    }
}

impl PullRequestsPanel {
    /// What the host injects into this thread's provider, before any call.
    fn agent_tools(
        &self,
        tools: &tcode_protocol::InjectedTools,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.agent_tools_open;
        let label = crate::tr!("pull_requests.agent_sees").into_owned();
        let header = crate::material::accessible_clickable(
            h_flex(),
            "pr-agent-tools",
            gpui::Role::Button,
            label.clone(),
            cx,
        )
        .aria_expanded(open)
        .gap(cx.theme().tokens.spacing.xs)
        .items_center()
        .text_secondary(false, cx)
        .cursor_pointer()
        .child(Icon::new(IconName::Bot))
        .child(label)
        .on_click(cx.listener(|this, _, _, cx| {
            this.agent_tools_open = !this.agent_tools_open;
            cx.notify();
        }));
        let body = agent_sees_body(tools, "pr-agent", cx).pt(cx.theme().tokens.spacing.xs);
        // Open in the side panel, the list takes a flex share (basis 0) and scrolls in it: at
        // its content height it would push the panel past its bounds. The bottom sheet
        // scrolls its own content, so there the list keeps its natural height.
        let fill = open && !self.window_state.read(cx).compact;
        v_flex()
            .map(|section| {
                if fill {
                    section.flex_1().min_h_0()
                } else {
                    section.flex_none()
                }
            })
            .px(px(crate::material::CARD_INSET))
            .py(cx.theme().tokens.spacing.sm)
            .child(header)
            .child(
                div()
                    .id("pr-agent-tools-scroll")
                    .when(fill, |list| list.flex_1().min_h_0())
                    .child(
                        crate::widgets::Collapsible::new("pr-agent-tools-body", open).content(body),
                    )
                    .overflow_y_scroll_area(),
            )
            .into_any_element()
    }
}
impl Render for PullRequestsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Level two: the pull request the thread opened, in the wide layout's tab, pushed over
        // the list and popped back off it. The phone's page is the shell's.
        let open = (!self.window_state.read(cx).compact)
            .then(|| {
                let store = self.store.read(cx);
                let session = store.active_session_id()?;
                let key = store.open_pull_request(&session).cloned()?;
                Some((session, key))
            })
            .flatten();
        let opened = open.is_some();
        if let Some((session, key)) = open {
            self.detail
                .update(cx, |detail, cx| detail.show(session, key, cx));
        }
        let page = crate::motion::shown("pr-detail-page", opened.then_some(()), window, cx);
        let Some(page) = page else {
            return self.list(window, cx);
        };
        if page.progress() >= 1. {
            return div()
                .size_full()
                .child(self.detail.clone())
                .into_any_element();
        }
        let progress = page.progress();
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(gpui::relative(-0.25 * progress))
                    .right(gpui::relative(0.25 * progress))
                    .child(self.list(window, cx)),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(gpui::relative(1. - progress))
                    .right(gpui::relative(progress - 1.))
                    .opacity(if page.leaving() { 1. } else { progress })
                    .bg(crate::material::content_surface(cx))
                    .child(self.detail.clone()),
            )
            // Neither page takes input while one slides over the other.
            .child(div().absolute().inset_0().occlude())
            .into_any_element()
    }
}

impl PullRequestsPanel {
    fn list(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let session = store.active_session_id();
        let links = session
            .as_deref()
            .map(|id| store.pull_requests(id).to_vec())
            .unwrap_or_default();
        let operations = session
            .as_deref()
            .and_then(|id| store.thread_meta(id))
            .map(|meta| meta.pull_request_operations.clone())
            .unwrap_or_default();
        let read_only = store
            .session_status()
            .is_none_or(|status| status.conversation_read_only);
        let watchable = session
            .as_deref()
            .and_then(|id| store.thread_meta(id))
            .is_some_and(|meta| {
                !meta.is_settled() && meta.archived_at.is_none() && meta.parent_session_id.is_none()
            });
        let watches_paused = session
            .as_deref()
            .and_then(|id| store.thread_meta(id))
            .is_some_and(|meta| meta.pull_request_watches_paused);
        let agent_tools = store
            .session_status()
            .and_then(|status| status.injected_tools.clone());
        let compact = self.window_state.read(cx).compact;
        let mut rows = v_flex().id("pull-request-rows").w_full().min_w_0().gap_1();
        // One notice per host whose pull requests cannot sync, in the order the links name them.
        let mut hosts = Vec::new();
        for link in &links {
            if !hosts.contains(&link.key.host) {
                hosts.push(link.key.host.clone());
            }
        }
        for host in hosts {
            let notice = links
                .iter()
                .filter(|link| link.visible() && link.snapshot.is_none() && link.key.host == host)
                .find_map(|link| {
                    let credential = matches!(
                        link.sync_error,
                        Some(
                            PullRequestSyncError::NoCredential | PullRequestSyncError::HostDisabled
                        )
                    );
                    let rate_limited = matches!(
                        link.sync_error,
                        Some(PullRequestSyncError::RateLimited { .. })
                    );
                    (credential || rate_limited).then(|| {
                        (
                            sync_error(link, &host_name(self.store.read(cx), &host)),
                            credential,
                        )
                    })
                });
            let Some(notice) = crate::motion::shown(
                SharedString::from(format!("pr-host-notice-{host}")),
                notice,
                window,
                cx,
            ) else {
                continue;
            };
            let (text, credential) = notice.value.clone();
            rows = rows.child(
                notice.fold(
                    crate::material::card(cx)
                        .flex_row()
                        .items_center()
                        .text_token(&cx.theme().tokens.typography.xs)
                        .child(div().flex_1().child(text))
                        .when(credential, |notice| {
                            notice.child(
                                Button::new(SharedString::from(format!("pr-settings-{host}")))
                                    .ghost()
                                    .xsmall()
                                    .label(crate::tr!("pull_requests.open_settings"))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(OpenSourceControl), cx)
                                    }),
                            )
                        }),
                ),
            );
        }
        for group in pull_request::groups(&links) {
            if let Some(stack) = group.stack {
                let anchor = group.links[0];
                let operation =
                    tcode_core::pull_request::stack_operation(&operations, &links, &anchor.key);
                rows = rows.child(stack::caption(stack, operation, &links, compact, cx));
                for (index, layer) in stack.layers.iter().enumerate() {
                    let key =
                        PullRequestKey::new(&anchor.key.host, &anchor.key.repository, layer.number);
                    let link = links.iter().find(|link| link.key == key);
                    rows = rows.child(self.row(
                        PullRequestRow {
                            key,
                            url: layer.url.clone(),
                            link,
                            branch: Some(&layer.head_branch),
                            state: Some(layer.state),
                            position: Some((index + 1, stack.layers.len())),
                            depth: index,
                            watchable,
                        },
                        compact,
                        window,
                        cx,
                    ));
                }
            } else {
                for (index, link) in group.links.iter().enumerate() {
                    rows = rows.child(self.row(
                        PullRequestRow {
                            key: link.key.clone(),
                            url: link.url.clone(),
                            link: Some(link),
                            branch: None,
                            state: None,
                            position: None,
                            depth: index,
                            watchable,
                        },
                        compact,
                        window,
                        cx,
                    ));
                }
            }
        }
        let agent_tools = agent_tools.map(|tools| self.agent_tools(&tools, cx));
        let count = links.iter().filter(|link| link.visible()).count();
        // Monitoring pauses and resumes for the whole thread, beside the watches it governs.
        let monitoring = crate::motion::shown(
            "pr-monitoring-shown",
            (!read_only && watchable && tcode_core::pull_request::watched(&links).next().is_some())
                .then_some(watches_paused),
            window,
            cx,
        )
        .map(|kept| {
            let paused = kept.value;
            let store = self.store.clone();
            let button = Button::new("pr-monitoring")
                .debug_selector(|| "pr-monitoring".into())
                .ghost()
                .icon(if paused {
                    IconName::Play
                } else {
                    IconName::Pause
                })
                .label(crate::tr!(if paused {
                    "pull_requests.resume_monitoring"
                } else {
                    "pull_requests.pause_monitoring"
                }))
                .on_click(move |_, window, cx| set_watches_paused(&store, !paused, window, cx));
            (kept, button)
        });
        let link_button = |id: &'static str| {
            let store = self.store.clone();
            let session = session.clone();
            Button::new(id)
                .outline()
                .small()
                .icon(IconName::Plus)
                .label(crate::tr!("pull_requests.link_menu"))
                .on_click(move |_, window, cx| {
                    if let Some(id) = &session {
                        open_link_dialog(store.clone(), id.clone(), window, cx);
                    }
                })
        };
        // The first link cross-fades the list over the empty state.
        let emptied = gpui_base::motion::transition(
            "pr-empty",
            if count == 0 { 1. } else { 0. },
            crate::motion::settle(),
            window,
            cx,
        );
        let empty_state = |cx: &App| {
            crate::material::empty_state(
                Icon::new(IconName::GitPullRequest),
                crate::tr!("pull_requests.empty_title").into_owned(),
                crate::tr!("pull_requests.empty_desc").into_owned(),
                cx,
            )
        };
        let was_empty = (count > 0 && emptied > 0.).then(|| {
            div()
                .absolute()
                .inset_0()
                .opacity(emptied)
                .child(empty_state(cx))
        });
        if count == 0 {
            let empty = empty_state(cx).opacity(emptied).when(!read_only, |empty| {
                empty.child(div().pt_1().child(link_button("link-pr-empty")))
            });
            return v_flex()
                .size_full()
                .on_action(cx.listener(Self::change_link))
                .on_action(cx.listener(Self::change_watch))
                .on_action(cx.listener(Self::run_lifecycle))
                .child(rows.p_2())
                .child(empty)
                .children(agent_tools)
                .into_any_element();
        }
        let open = links
            .iter()
            .filter(|link| {
                link.visible()
                    && link
                        .snapshot
                        .as_ref()
                        .is_some_and(|s| s.state == PullRequestState::Open)
            })
            .count();
        let mut summary = format!(
            "{} · {}",
            crate::tr!("pull_requests.summary_open", count = open.to_string()),
            crate::tr!("pull_requests.summary_linked", count = count.to_string())
        );
        if let Some(synced) = links
            .iter()
            .filter_map(|link| link.snapshot.as_ref().map(|snapshot| snapshot.synced_at))
            .max()
        {
            summary.push_str(&format!(
                " · {}",
                crate::tr!(
                    "pull_requests.summary_synced",
                    ago = crate::time::humanize_ago(
                        tcode_core::project::now_secs().saturating_sub(synced)
                    )
                )
            ));
        }
        let summary = div().flex_1().text_secondary(compact, cx).child(summary);
        if compact {
            // The bottom sheet scrolls its own content, so the list keeps its natural height.
            return v_flex()
                .relative()
                .w_full()
                .gap_3()
                .children(was_empty)
                .on_action(cx.listener(Self::change_link))
                .on_action(cx.listener(Self::change_watch))
                .on_action(cx.listener(Self::run_lifecycle))
                .child(rows)
                .child(summary)
                .children(agent_tools)
                .children(monitoring.map(|(kept, button)| kept.fold(button.outline().w_full())))
                .when(!read_only, |sheet| {
                    sheet.child(link_button("link-pr").w_full())
                })
                .into_any_element();
        }
        v_flex()
            .size_full()
            .min_w_0()
            .on_action(cx.listener(Self::change_link))
            .on_action(cx.listener(Self::change_watch))
            .on_action(cx.listener(Self::run_lifecycle))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("pr-scroll")
                            .size_full()
                            .opacity(1. - emptied)
                            .child(rows.p_2())
                            .overflow_y_scroll_area()
                            .track_scroll(&self.scroll),
                    )
                    .children(was_empty),
            )
            .children(agent_tools)
            .child(
                crate::material::bar("pr-footer", crate::material::Bar::Toolbar, false, cx)
                    .items_center()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(summary)
                    .children(monitoring.map(|(kept, button)| kept.fade(button.xsmall())))
                    .when(!read_only, |footer| {
                        footer.child(
                            link_button("link-pr")
                                .ghost()
                                .xsmall()
                                .label(crate::tr!("pull_requests.link_short")),
                        )
                    }),
            )
            .into_any_element()
    }
}

struct LinkDialog {
    store: Entity<WorkspaceStore>,
    id: String,
    input: Entity<InputState>,
    pending: bool,
    error: Option<String>,
    _subscription: Subscription,
}
impl LinkDialog {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending {
            return;
        }
        let reference = self.input.read(cx).value().to_string();
        if reference.trim().is_empty() {
            self.error = Some(crate::tr!("pull_requests.error_blank").into_owned());
            cx.notify();
            return;
        }
        self.pending = true;
        self.error = None;
        cx.notify();
        let task = self.store.update(cx, |store, cx| {
            store.command(
                Command::LinkPullRequest {
                    session_id: self.id.clone(),
                    reference,
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, |this, window, cx| {
                this.pending = false;
                match result {
                    Ok(_) => window.close_dialog(cx),
                    Err(error) => {
                        this.error = Some(match error.code.as_str() {
                            "pull_request_invalid_reference" => {
                                crate::tr!("pull_requests.error_unrecognized").into_owned()
                            }
                            "pull_request_no_repository" => {
                                crate::tr!("pull_requests.error_no_repository").into_owned()
                            }
                            _ => crate::tr!("pull_requests.link_failed", reason = error.message)
                                .into_owned(),
                        });
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}
impl Render for LinkDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let error = crate::motion::shown("pr-link-error", self.error.clone(), window, cx);
        let description = match self.store.read(cx).github_repository(&self.id) {
            Some(repository) => {
                crate::tr!("pull_requests.dialog_desc_repo", repository = repository)
            }
            None => crate::tr!("pull_requests.dialog_desc_url"),
        };
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_body(false, cx)
                    .text_color(cx.theme().muted_foreground)
                    .child(description.into_owned()),
            )
            .child(Input::new(&self.input).disabled(self.pending))
            .child(
                div()
                    .min_h(cx.theme().tokens.typography.xs.line_height)
                    .text_token(&cx.theme().tokens.typography.xs)
                    .text_color(cx.theme().danger)
                    .when_some(error, |slot, error| {
                        slot.child(error.fade(error.value.clone()))
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("pr-cancel")
                            .outline()
                            .small()
                            .disabled(self.pending)
                            .label(crate::tr!("sidebar.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("pr-link")
                            .primary()
                            .small()
                            .disabled(self.pending || self.input.read(cx).value().trim().is_empty())
                            .label(crate::tr!(if self.pending {
                                "pull_requests.dialog_linking"
                            } else {
                                "pull_requests.dialog_link"
                            }))
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
pub fn open_link_dialog(
    store: Entity<WorkspaceStore>,
    id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let input = cx.new(|cx| {
        InputState::new(window, cx).placeholder(crate::tr!("pull_requests.dialog_placeholder"))
    });
    let dialog = cx.new(|cx| {
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this: &mut LinkDialog, _, event, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            },
        );
        LinkDialog {
            store,
            id,
            input: input.clone(),
            pending: false,
            error: None,
            _subscription: subscription,
        }
    });
    window.open_dialog(cx, move |base, _, cx| {
        base.title(crate::tr!("pull_requests.dialog_title").into_owned())
            .keyboard(!dialog.read(cx).pending)
            .close_button(!dialog.read(cx).pending)
            .overlay_closable(!dialog.read(cx).pending)
            .footer(crate::overlay::DialogActions::new())
            .content({
                let dialog = dialog.clone();
                move |content, _, _| content.child(dialog.clone())
            })
    });
    input.update(cx, |input, cx| input.focus(window, cx));
}
