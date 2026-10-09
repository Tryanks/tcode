use crate::{
    icon::{Icon, IconName},
    overlay::{Notification, NotificationType, OverlayExt as _},
    scroll::ScrollableElement as _,
    sizing::Sizable as _,
    store::{TopicKind, WorkspaceStore, observe_store_topics},
    theme::ActiveTheme as _,
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
        self, ChecksState, Mergeability, PullRequestBadgeState, PullRequestKey, PullRequestSource,
        PullRequestState, PullRequestSyncError, ReviewDecision, ThreadPullRequestLink,
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
gpui::actions!(tcode_pull_requests, [OpenLinkDialog, OpenSourceControl]);

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
    link: Option<&ThreadPullRequestLink>,
    state: Option<PullRequestState>,
) -> PullRequestBadgeState {
    match link
        .and_then(|link| link.snapshot.as_ref())
        .map(|s| (s.state, s.is_draft))
        .or_else(|| state.map(|state| (state, false)))
    {
        Some((PullRequestState::Open, true)) => PullRequestBadgeState::Draft,
        Some((PullRequestState::Open, false)) => PullRequestBadgeState::Open,
        Some((PullRequestState::Merged, _)) => PullRequestBadgeState::Merged,
        Some((PullRequestState::Closed, _)) => PullRequestBadgeState::Closed,
        None => PullRequestBadgeState::Unknown,
    }
}
/// A single-link badge names that link.
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
pub fn badge(links: &[ThreadPullRequestLink], size: f32, cx: &App) -> Option<AnyElement> {
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
        h_flex()
            .id("pull-request-badge")
            .aria_label(label)
            .flex_none()
            .items_center()
            .gap(px(2.))
            .text_size(px(size - 1.))
            .font_features(gpui::FontFeatures(std::sync::Arc::new(vec![(
                "tnum".into(),
                1,
            )])))
            .text_color(color)
            .child(Icon::new(if stacked { IconName::Layers } else { glyph }).size(px(size)))
            .child(text)
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
    let trigger = crate::material::accessible_clickable(
        div(),
        SharedString::from(format!("pr-badge-{id}")),
        gpui::Role::Button,
        badge_label(links, cx).unwrap_or_default(),
        cx,
    )
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
                let mut rows = v_flex()
                    .id("pr-mini-list")
                    .w(px(320.))
                    .p_2()
                    .gap_0p5()
                    .rounded(crate::material::radius_overlay(cx))
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_xl()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
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
                                    link,
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
                for (index, (depth, link, caption)) in ordered.iter().take(8).enumerate() {
                    let (glyph, color, _) = appearance(row_state(Some(link), None), cx);
                    let (store, id, key) =
                        (hover_store.clone(), hover_id.clone(), link.key.clone());
                    rows = rows.child(
                        h_flex()
                            .id(index)
                            .h(px(24.))
                            .pl(px(depth.min(&3).to_owned() as f32 * 12.))
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().sidebar_accent))
                            .on_click(move |_, _, cx| {
                                store.update(cx, |store, cx| {
                                    store.select_session(id.clone());
                                    store.open_tab_for(&id, RightTab::PullRequests, cx);
                                    store.set_open_pull_request(&id, Some(key.clone()), cx);
                                });
                            })
                            .child(Icon::new(glyph).xsmall().text_color(color))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .child(format!("#{}", link.key.number)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .child(link.snapshot.as_ref().map_or_else(
                                        || link.key.repository.clone(),
                                        |s| s.title.clone(),
                                    )),
                            )
                            .children(caption.clone().map(|caption| {
                                div()
                                    .text_size(px(10.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(caption)
                            })),
                    );
                }
                if ordered.len() > 8 {
                    rows = rows.child(
                        div().text_size(px(11.)).child(
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
        WatchNotice::Unreadable => (
            NotificationType::Error,
            crate::tr!("pull_requests.watch_unreadable", number = number).into_owned(),
        ),
    }
}

fn source(link: Option<&ThreadPullRequestLink>) -> &'static str {
    match link.map(|link| link.source) {
        Some(PullRequestSource::Manual) => "pull_requests.source_manual",
        Some(PullRequestSource::Agent) => "pull_requests.source_agent",
        Some(PullRequestSource::Created) => "pull_requests.source_created",
        Some(PullRequestSource::Stack) => "pull_requests.source_stack",
        Some(PullRequestSource::Dismissed) => "pull_requests.source_dismissed",
        None => "pull_requests.source_not_linked",
    }
}
fn sync_error(link: &ThreadPullRequestLink) -> String {
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
            ago =
                crate::time::humanize_ago(retry_at.saturating_sub(tcode_core::project::now_secs()))
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

/// The row's menu: Open on GitHub, Copy link, the watch, what can become of the pull request,
/// and the link condition.
type RowMenu = std::rc::Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

fn row_menu(
    key: PullRequestKey,
    url: String,
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
            menu.menu(
                crate::tr!("pull_requests.open_on_github").into_owned(),
                Box::new(OpenUrl(url.clone())),
            )
            .menu(
                crate::tr!("pull_requests.copy_link").into_owned(),
                Box::new(CopyText(url.clone())),
            )
            .when(watched || can_watch, |menu| {
                menu.menu(
                    crate::tr!(if watched {
                        "pull_requests.stop_watching"
                    } else {
                        "pull_requests.watch"
                    })
                    .into_owned(),
                    Box::new(ChangeWatch {
                        key: key.clone(),
                        watching: !watched,
                    }),
                )
            })
            .when_some(offer(cx), |menu, offer| offer.menu(menu))
            .separator()
            .menu(
                crate::tr!(if action.linking {
                    "pull_requests.relink"
                } else if source_is_stack {
                    "pull_requests.dismiss"
                } else {
                    "pull_requests.unlink"
                })
                .into_owned(),
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
    fn row(&self, row: PullRequestRow<'_>, compact: bool, cx: &mut Context<Self>) -> AnyElement {
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
        let (glyph, color, state_label) = appearance(row_state(link, state), cx);
        let color = if visible {
            color
        } else {
            cx.theme().muted_foreground
        };
        let source = crate::tr!(source(link)).into_owned();
        let title = snapshot.map(|s| s.title.clone()).unwrap_or_else(|| {
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
        let mut detail = vec![source.clone()];
        let line_two = if let Some(snapshot) = snapshot {
            let author = snapshot.author.as_ref().map(|author| author.login.clone());
            let branches = format!("{} → {}", snapshot.head_branch, snapshot.base_branch);
            let stat = (
                format!("+{}", snapshot.additions),
                format!("−{}", snapshot.deletions),
            );
            detail.extend(author.clone());
            detail.push(branches.clone());
            detail.extend(layer.clone());
            detail.push(format!("{} {}", stat.0, stat.1));
            detail.retain(|part| !part.is_empty());
            if compact {
                // The phone sheet has no room for parts that give way, so the line truncates whole.
                div()
                    .min_w_0()
                    .truncate()
                    .child(detail.join(" · "))
                    .into_any_element()
            } else {
                // The author gives way first, so the source and the branches stay readable.
                h_flex()
                    .gap_1()
                    .items_center()
                    .min_w_0()
                    .overflow_hidden()
                    .child(div().flex_none().child(source.clone()))
                    .when_some(author, |line, author| {
                        line.child(dot())
                            .child(div().min_w_0().truncate().child(author))
                    })
                    .child(dot())
                    // gpui-base has no middle truncation; the row tooltip holds the full pair.
                    .child(
                        div()
                            .flex_shrink_0()
                            .max_w(gpui::relative(0.5))
                            .truncate()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(branches),
                    )
                    .when_some(layer.clone(), |line, layer| {
                        line.child(dot()).child(div().flex_none().child(layer))
                    })
                    .child(dot())
                    .child(
                        div()
                            .flex_none()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(cx.theme().success)
                            .child(stat.0),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(cx.theme().danger)
                            .child(stat.1),
                    )
                    .into_any_element()
            }
        } else {
            if let Some(link) = link.filter(|_| visible) {
                detail.insert(0, sync_error(link));
            } else {
                detail.push(crate::tr!(&format!("{state_label}_lower")).into_owned());
                detail.extend(layer.clone());
            }
            h_flex()
                .gap_1()
                .items_center()
                .min_w_0()
                .when(
                    link.is_some_and(|link| link.source == PullRequestSource::Dismissed),
                    |line| line.child(Icon::new(IconName::Unlink).size(px(12.))),
                )
                .child(div().min_w_0().truncate().child(detail.join(" · ")))
                .into_any_element()
        };
        let detail = detail.join(" · ");
        let menu = row_menu(key.clone(), url.clone(), link, watchable, self.offer(&key));
        let mut signals = h_flex().gap_1().flex_none().items_center();
        // The eye leads while the pull request is open or not yet read; the host ends the watch
        // once it merges or closes.
        if watched && snapshot.is_none_or(|s| s.state == PullRequestState::Open) {
            let tooltip = crate::tr!("pull_requests.watching_tooltip").into_owned();
            signals = signals.child(if compact {
                // A 14px target is below touch size: the more menu stops it on a phone.
                Icon::new(IconName::Eye)
                    .size(px(14.))
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
                    .size(px(14.))
                    .text_color(if hovered {
                        cx.theme().foreground
                    } else {
                        cx.theme().muted_foreground
                    }),
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
            });
        }
        if let Some(snapshot) = snapshot.filter(|s| s.state == PullRequestState::Open) {
            if let Some(check) = snapshot.checks_state {
                let (icon, color, label) = match check {
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
                    div()
                        .id(SharedString::from(format!("pr-signal-{label}")))
                        .child(Icon::new(icon).size(px(14.)).text_color(color))
                        .tooltip(move |window, cx| {
                            crate::widgets::tooltip::Tooltip::new(crate::tr!(label).into_owned())
                                .build(window, cx)
                        }),
                );
            }
            if let Some(review) = snapshot.review_decision {
                let (icon, color, label) = match review {
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
                    div()
                        .id(SharedString::from(format!("pr-signal-{label}")))
                        .child(Icon::new(icon).size(px(14.)).text_color(color))
                        .tooltip(move |window, cx| {
                            crate::widgets::tooltip::Tooltip::new(crate::tr!(label).into_owned())
                                .build(window, cx)
                        }),
                );
            }
            if snapshot.mergeability == Mergeability::Conflicting {
                signals = signals.child(
                    div()
                        .id("pr-conflict")
                        .child(
                            Icon::new(IconName::GitMergeConflict)
                                .size(px(14.))
                                .text_color(cx.theme().warning),
                        )
                        .tooltip({
                            let base = snapshot.base_branch.clone();
                            move |window, cx| {
                                crate::widgets::tooltip::Tooltip::new(
                                    crate::tr!("pull_requests.conflict", base = base.clone())
                                        .into_owned(),
                                )
                                .build(window, cx)
                            }
                        }),
                );
            }
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
        if let Some(blocker) = blocker {
            signals = signals.child(
                div()
                    .id("pr-signal-blocked")
                    .child(
                        Icon::new(IconName::Lock)
                            .size(px(14.))
                            .text_color(cx.theme().muted_foreground),
                    )
                    .tooltip(move |window, cx| {
                        crate::widgets::tooltip::Tooltip::new(blocker.clone()).build(window, cx)
                    }),
            );
        }
        let row_id = SharedString::from(format!(
            "pr-row-{}-{}-{}",
            key.host, key.repository, key.number
        ));
        let label = SharedString::from(format!("#{} {title}", key.number));
        let row = if compact {
            crate::material::list_row(row_id.clone(), label, cx)
        } else {
            crate::material::accessible_clickable(
                h_flex(),
                row_id.clone(),
                gpui::Role::Button,
                label,
                cx,
            )
            .hover(|s| s.bg(cx.theme().sidebar_accent))
        }
        .group(row_id.clone())
        .w_full()
        .min_w_0()
        .min_h(px(if compact { 64. } else { 52. }))
        .px_3()
        .py_2()
        .pl(px(12. + depth.min(3) as f32 * 16.))
        .gap_2()
        .items_start()
        .rounded_md()
        .cursor_pointer()
        .on_click({
            let store = self.store.clone();
            let window_state = self.window_state.clone();
            let key = key.clone();
            move |_, window, cx| open_pull_request(&store, &window_state, key.clone(), window, cx)
        })
        .when(depth > 0, |row| {
            row.child(div().w(px(1.)).h(px(24.)).bg(cx.theme().border))
        })
        .child(
            Icon::new(glyph)
                .size(px(if compact { 20. } else { 16. }))
                .text_color(color),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(px(if compact { 14. } else { 12. }))
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
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(if compact { 15. } else { 13. }))
                                .text_color(if visible {
                                    cx.theme().foreground
                                } else {
                                    cx.theme().muted_foreground
                                })
                                .when(snapshot.is_none(), |title| {
                                    title.font_family(cx.theme().mono_font_family.clone())
                                })
                                .child(title),
                        )
                        .when(
                            snapshot.is_some_and(|snapshot| {
                                snapshot.is_draft && snapshot.state == PullRequestState::Open
                            }),
                            |row| {
                                row.child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child(crate::tr!("pull_requests.draft")),
                                )
                            },
                        )
                        .child(signals),
                )
                .child(
                    div()
                        .id("pr-detail")
                        .min_w_0()
                        .text_size(px(if compact { 13. } else { 11. }))
                        .text_color(cx.theme().muted_foreground)
                        .tooltip(move |window, cx| {
                            crate::widgets::tooltip::Tooltip::new(detail.clone()).build(window, cx)
                        })
                        .child(line_two),
                ),
        )
        .child(
            div()
                .relative()
                .flex_none()
                .min_w(px(if compact { 44. } else { 36. }))
                .min_h(px(if compact { 44. } else { 24. }))
                .when_some(
                    running.clone().filter(|_| !compact),
                    |slot, (icon, color, _, tooltip)| {
                        slot.child(
                            div()
                                .id("pr-signal-operation")
                                .group_hover(row_id.clone(), |signal| signal.invisible())
                                .child(Icon::new(icon).size(px(14.)).text_color(color))
                                .tooltip(move |window, cx| {
                                    crate::widgets::tooltip::Tooltip::new(tooltip.clone())
                                        .build(window, cx)
                                }),
                        )
                    },
                )
                .when(!compact && running.is_none(), |slot| {
                    slot.child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .group_hover(row_id.clone(), |time| time.invisible())
                            .children(
                                snapshot
                                    .and_then(|snapshot| {
                                        chrono::DateTime::parse_from_rfc3339(&snapshot.updated_at)
                                            .ok()
                                    })
                                    .map(|updated| {
                                        crate::time::humanize_ago(
                                            tcode_core::project::now_secs()
                                                .saturating_sub(updated.timestamp().max(0) as u64),
                                        )
                                    }),
                            ),
                    )
                })
                .child(
                    Button::new(SharedString::from(format!(
                        "pr-menu-{}-{}-{}",
                        key.host, key.repository, key.number
                    )))
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Ellipsis)
                    .tooltip(
                        crate::tr!("pull_requests.actions_for", number = key.number.to_string())
                            .into_owned(),
                    )
                    .when(compact, |button| button.min_w(px(44.)).min_h(px(44.)))
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
                    }),
                ),
        );

        row.context_menu(move |state, window, cx| (menu)(state, window, cx))
            .into_any_element()
    }
}
impl PullRequestsPanel {
    /// What the host injects into this thread's provider for pull requests, before any call.
    fn agent_tools(
        &self,
        tools: &tcode_protocol::InjectedPullRequestTools,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.agent_tools_open;
        let muted = cx.theme().muted_foreground;
        let label = crate::tr!("pull_requests.agent_sees").into_owned();
        let caption = |key: &'static str| {
            div()
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!(key))
        };
        let header = crate::material::accessible_clickable(
            h_flex(),
            "pr-agent-tools",
            gpui::Role::Button,
            label.clone(),
            cx,
        )
        .aria_expanded(open)
        .gap_1()
        .items_center()
        .text_size(px(12.))
        .text_color(muted)
        .cursor_pointer()
        .child(Icon::new(IconName::Bot).size(px(12.)))
        .child(label)
        .on_click(cx.listener(|this, _, _, cx| {
            this.agent_tools_open = !this.agent_tools_open;
            cx.notify();
        }));
        let body = v_flex()
            .pt_1()
            .gap_2()
            .text_size(px(12.))
            .child(caption("pull_requests.agent_tools"))
            .children(tools.tools.iter().map(|(name, description)| {
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(name.clone()),
                    )
                    .child(div().text_color(muted).child(description.clone()))
            }))
            .child(caption("pull_requests.agent_instructions"))
            .child(crate::chat::components::disclosure::disclosure_body(
                "pr-agent-instructions",
                &tools.instructions,
                cx,
            ));
        v_flex()
            .flex_none()
            .px_3()
            .py_2()
            .child(header)
            .child(gpui_base::Collapsible::new().open(open).content(body))
            .into_any_element()
    }
}
impl Render for PullRequestsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let agent_tools = store
            .session_status()
            .and_then(|status| status.pull_request_tools.clone());
        let compact = self.window_state.read(cx).compact;
        // Level two: the pull request the thread opened, in the wide layout's tab.
        if !compact
            && let Some(session) = session.clone()
            && let Some(key) = store.open_pull_request(&session).cloned()
        {
            self.detail
                .update(cx, |detail, cx| detail.show(session, key, cx));
            return div()
                .size_full()
                .child(self.detail.clone())
                .into_any_element();
        }
        let mut rows = v_flex().id("pull-request-rows").w_full().min_w_0().gap_1();
        let mut notice_hosts = std::collections::HashSet::new();
        for link in links
            .iter()
            .filter(|link| link.visible() && link.snapshot.is_none())
        {
            let credential = matches!(
                link.sync_error,
                Some(PullRequestSyncError::NoCredential | PullRequestSyncError::HostDisabled)
            );
            let rate_limited = matches!(
                link.sync_error,
                Some(PullRequestSyncError::RateLimited { .. })
            );
            if !(credential || rate_limited) || !notice_hosts.insert(link.key.host.clone()) {
                continue;
            }
            rows = rows.child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .rounded_md()
                    .bg(cx.theme().muted)
                    .text_size(px(12.))
                    .child(div().flex_1().child(sync_error(link)))
                    .when(credential, |notice| {
                        notice.child(
                            Button::new(SharedString::from(format!(
                                "pr-settings-{}",
                                link.key.host
                            )))
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("pull_requests.open_settings"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenSourceControl), cx)
                            }),
                        )
                    }),
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
                        cx,
                    ));
                }
            }
        }
        let agent_tools = agent_tools.map(|tools| self.agent_tools(&tools, cx));
        let count = links.iter().filter(|link| link.visible()).count();
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
        if count == 0 {
            let empty = crate::material::empty_state(
                Icon::new(IconName::GitPullRequest),
                crate::tr!("pull_requests.empty_title").into_owned(),
                crate::tr!("pull_requests.empty_desc").into_owned(),
                cx,
            )
            .when(!read_only, |empty| {
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
        let summary = div()
            .flex_1()
            .text_size(px(if compact { 13. } else { 11. }))
            .text_color(cx.theme().muted_foreground)
            .child(summary);
        if compact {
            // The bottom sheet scrolls its own content, so the list keeps its natural height.
            return v_flex()
                .w_full()
                .gap_3()
                .on_action(cx.listener(Self::change_link))
                .on_action(cx.listener(Self::change_watch))
                .on_action(cx.listener(Self::run_lifecycle))
                .child(rows)
                .child(summary)
                .children(agent_tools)
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
                    .id("pr-scroll")
                    .flex_1()
                    .min_h_0()
                    .child(rows.p_2())
                    .overflow_y_scroll_area()
                    .track_scroll(&self.scroll),
            )
            .children(agent_tools)
            .child(
                h_flex()
                    .flex_none()
                    .h(px(32.))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(summary)
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(description.into_owned()),
            )
            .child(Input::new(&self.input).disabled(self.pending))
            .child(
                div()
                    .min_h(px(16.))
                    .text_size(px(12.))
                    .text_color(cx.theme().danger)
                    .child(self.error.clone().unwrap_or_default()),
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
            .w(px(460.))
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
