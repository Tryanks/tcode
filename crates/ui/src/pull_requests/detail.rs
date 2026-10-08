//! One pull request read through the host: the header, the Files / Conversation switch and
//! the reads behind them. The desktop draws it inside the Pull requests tab; the phone draws it
//! as a page.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement as _, IntoElement, ListAlignment,
    ListState, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::pull_request::{
    ChecksState, Mergeability, PullRequestKey, PullRequestSource, PullRequestStackState,
    PullRequestState, ReviewDecision, ThreadPullRequestLink,
};
use tcode_protocol::{
    Command, ProtocolError, PullRequestConversation, PullRequestFile, PullRequestFiles,
    PullRequestRead, PullRequestReadResponse, PullRequestViewedFiles,
};

use super::{ChangeLink, ChangeWatch, appearance, row_menu, row_state, source};
use crate::{
    icon::{Icon, IconName},
    material,
    overlay::{Notification, OverlayExt as _},
    sizing::Sizable as _,
    store::{TopicKind, WorkspaceStore, observe_store_topics},
    theme::ActiveTheme as _,
    widgets::{
        Popover,
        button::{Button, ButtonVariants as _},
        menu::DropdownMenu as _,
        tooltip::Tooltip,
    },
    window_state::WindowState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    Files,
    Conversation,
}

/// One host read: what it last answered and when to ask again. A failure is kept until the
/// reader retries, so a failing read is not asked again on every frame.
pub(super) struct Read<T> {
    pub(super) data: Option<T>,
    pub(super) expires_at: u64,
    pub(super) loading: bool,
    pub(super) error: Option<ProtocolError>,
    /// When the data on hand was answered.
    pub(super) loaded_at: u64,
}

impl<T> Default for Read<T> {
    fn default() -> Self {
        Self {
            data: None,
            expires_at: 0,
            loading: false,
            error: None,
            loaded_at: 0,
        }
    }
}

impl<T> Read<T> {
    pub(super) fn due(&self, now: u64) -> bool {
        !self.loading && self.error.is_none() && (self.data.is_none() || now >= self.expires_at)
    }
}

pub(super) enum TextState {
    Loading,
    /// The file's text on both sides, the base side reconstructed when it had to be.
    Loaded {
        old: String,
        new: String,
    },
    Missing,
    Oversized,
    Unreadable,
}

/// What a page of the Files view holds beyond the host's answer.
#[derive(Default)]
pub(super) struct FilesView {
    pub(super) list: Option<crate::diff::list::DiffList>,
    /// The head, file count and options the list was rendered for.
    pub(super) rendered: Option<(String, usize, bool, bool, bool)>,
    pub(super) rendering: bool,
    pub(super) page_loading: bool,
    pub(super) page_error: Option<ProtocolError>,
    pub(super) texts: HashMap<String, TextState>,
    /// The reader's own collapse choices, by path; others follow the viewed mark.
    pub(super) collapsed: HashMap<String, bool>,
    /// A gap the reader expanded while its file's text was being read.
    pub(super) pending_expand: Option<(String, u32, crate::diff::model::ExpandDir)>,
    pub(super) off_diff_open: bool,
}

#[derive(Default)]
pub(super) struct Replies {
    pub(super) comments: Vec<tcode_protocol::PullRequestComment>,
    pub(super) after: Option<String>,
    pub(super) loading: bool,
}

pub(super) struct ConversationView {
    pub(super) list: ListState,
    /// By comment id, with the account its conversation was read as.
    pub(super) markdown:
        std::cell::RefCell<HashMap<String, (String, Entity<crate::markdown::MarkdownState>)>>,
    pub(super) replies: HashMap<String, Replies>,
    /// Resolved threads and long comments the reader opened.
    pub(super) expanded: HashSet<String>,
}

impl Default for ConversationView {
    fn default() -> Self {
        Self {
            list: ListState::new(0, ListAlignment::Top, px(200.)),
            markdown: Default::default(),
            replies: HashMap::new(),
            expanded: HashSet::new(),
        }
    }
}

#[derive(Default)]
pub(super) struct PullRequestPage {
    pub(super) tab: Option<Tab>,
    pub(super) files: Read<PullRequestFiles>,
    pub(super) files_view: FilesView,
    pub(super) conversation: Read<PullRequestConversation>,
    pub(super) conversation_view: ConversationView,
    pub(super) viewed: Read<PullRequestViewedFiles>,
    /// Marks shown before the host confirms them, by path.
    pub(super) viewed_marks: HashMap<String, bool>,
    /// Marks waiting to be sent together.
    pub(super) viewed_batch: HashMap<String, bool>,
    pub(super) viewed_flush: Option<Task<()>>,
    pub(super) refreshing: bool,
}

pub struct PullRequestView {
    pub(super) store: Entity<WorkspaceStore>,
    pub(super) window_state: Entity<WindowState>,
    /// The thread the pull request is read through, and the pull request.
    pub(super) current: Option<(String, PullRequestKey)>,
    pub(super) pages: HashMap<(String, PullRequestKey), PullRequestPage>,
    pub(super) ignore_ws: bool,
    pub(super) show_invisibles: bool,
    pub(super) file_column: bool,
    pub(super) file_filter: Option<(Entity<crate::widgets::input::InputState>, Subscription)>,
    wake: Option<(u64, Task<()>)>,
    _subscriptions: [Subscription; 2],
}

pub(super) fn now() -> u64 {
    tcode_core::project::now_secs()
}

pub(super) fn ago(seconds: u64) -> String {
    crate::time::humanize_ago(now().saturating_sub(seconds))
}

pub(super) fn ago_rfc3339(stamp: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .map(|time| ago(time.timestamp().max(0) as u64))
}

/// The host's reason, at most 320 characters on screen.
pub(super) fn reason(error: &ProtocolError) -> String {
    let message = error.message.trim();
    if message.chars().count() > 320 {
        format!("{}…", message.chars().take(320).collect::<String>())
    } else {
        message.to_owned()
    }
}

impl PullRequestView {
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
        ];
        Self {
            store,
            window_state,
            current: None,
            pages: HashMap::new(),
            ignore_ws: false,
            show_invisibles: false,
            file_column: true,
            file_filter: None,
            wake: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn show(&mut self, session: String, key: PullRequestKey, cx: &mut Context<Self>) {
        let current = Some((session, key));
        if self.current != current {
            self.current = current;
            cx.notify();
        }
    }

    pub(super) fn compact(&self, cx: &App) -> bool {
        self.window_state.read(cx).compact
    }

    pub(super) fn page(&self) -> Option<&PullRequestPage> {
        self.pages.get(self.current.as_ref()?)
    }

    pub(super) fn page_mut(&mut self) -> Option<&mut PullRequestPage> {
        let current = self.current.clone()?;
        Some(self.pages.entry(current).or_default())
    }

    /// The thread's link, or the stack layer the pull request was opened from.
    pub(super) fn link(&self, cx: &App) -> Option<ThreadPullRequestLink> {
        let (session, key) = self.current.as_ref()?;
        self.store
            .read(cx)
            .pull_requests(session)
            .iter()
            .find(|link| link.key == *key)
            .cloned()
    }

    fn stack(&self, cx: &App) -> Option<tcode_core::pull_request::PullRequestStack> {
        let (session, key) = self.current.as_ref()?;
        self.store
            .read(cx)
            .pull_requests(session)
            .iter()
            .filter(|link| link.visible())
            .find_map(|link| match &link.stack {
                PullRequestStackState::Native(stack)
                    if link.key.host == key.host
                        && link.key.repository == key.repository
                        && stack.layers.iter().any(|layer| layer.number == key.number) =>
                {
                    Some(stack.clone())
                }
                _ => None,
            })
    }

    pub(super) fn url(&self, cx: &App) -> Option<String> {
        let (_, key) = self.current.as_ref()?;
        self.link(cx)
            .map(|link| link.url)
            .filter(|url| !url.is_empty())
            .or_else(|| {
                self.stack(cx)?
                    .layers
                    .iter()
                    .find(|layer| layer.number == key.number)
                    .map(|layer| layer.url.clone())
            })
    }

    pub(super) fn tab(&self, cx: &App) -> Tab {
        if let Some(tab) = self.page().and_then(|page| page.tab) {
            return tab;
        }
        let state = self
            .link(cx)
            .and_then(|link| link.snapshot.map(|snapshot| snapshot.state));
        match state {
            Some(PullRequestState::Merged | PullRequestState::Closed) => Tab::Conversation,
            _ => Tab::Files,
        }
    }

    fn select_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if let Some(page) = self.page_mut() {
            page.tab = Some(tab);
        }
        cx.notify();
    }

    /// Asks the host one read of the pull request on view and hands the answer to `land`.
    pub(super) fn read(
        &mut self,
        read: PullRequestRead,
        cx: &mut Context<Self>,
        land: impl FnOnce(&mut PullRequestPage, Result<(PullRequestReadResponse, u64), ProtocolError>)
        + 'static,
    ) {
        let Some((session, key)) = self.current.clone() else {
            return;
        };
        let task = self.store.update(cx, |store, cx| {
            store.read_pull_request(session.clone(), key.clone(), read, cx)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(page) = this.pages.get_mut(&(session, key)) {
                    land(page, result);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn read_files(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.page_mut() else { return };
        page.files.loading = true;
        self.read(PullRequestRead::Files { page: None }, cx, |page, result| {
            page.files.loading = false;
            page.refreshing = false;
            match result {
                Ok((PullRequestReadResponse::Files(files), expires_at)) => {
                    page.files.expires_at = expires_at;
                    page.files.error = None;
                    page.files.loaded_at = now();
                    if page.files.data.as_ref() != Some(&files) {
                        let head_moved = page
                            .files
                            .data
                            .as_ref()
                            .is_some_and(|held| held.head != files.head);
                        if head_moved {
                            page.files_view = FilesView::default();
                        } else {
                            page.files_view.list = None;
                            page.files_view.rendered = None;
                            page.files_view.page_error = None;
                        }
                        page.files.data = Some(files);
                    }
                }
                Ok(_) => {}
                Err(error) => page.files.error = Some(error),
            }
        });
    }

    pub(super) fn read_next_page(&mut self, next: u32, cx: &mut Context<Self>) {
        let Some(page) = self.page_mut() else { return };
        page.files_view.page_loading = true;
        page.files_view.page_error = None;
        self.read(
            PullRequestRead::Files { page: Some(next) },
            cx,
            move |page, result| {
                page.files_view.page_loading = false;
                match result {
                    Ok((PullRequestReadResponse::Files(more), _)) => {
                        let Some(files) = page.files.data.as_mut() else {
                            return;
                        };
                        // A page of another head is no continuation of this one.
                        if more.head != files.head {
                            page.files.expires_at = 0;
                            return;
                        }
                        files.files.extend(more.files);
                        files.next_page = more.next_page;
                        files.complete = more.complete;
                    }
                    Ok(_) => {}
                    Err(error) => page.files_view.page_error = Some(error),
                }
            },
        );
    }

    pub(super) fn read_conversation(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.page_mut() else { return };
        page.conversation.loading = true;
        self.read(PullRequestRead::Conversation, cx, |page, result| {
            page.conversation.loading = false;
            page.refreshing = false;
            match result {
                Ok((PullRequestReadResponse::Conversation(conversation), expires_at)) => {
                    page.conversation.expires_at = expires_at;
                    page.conversation.error = None;
                    page.conversation.loaded_at = now();
                    page.conversation.data = Some(conversation);
                }
                Ok(_) => {}
                Err(error) => page.conversation.error = Some(error),
            }
        });
    }

    pub(super) fn read_viewed(&mut self, cx: &mut Context<Self>) {
        let Some(page) = self.page_mut() else { return };
        page.viewed.loading = true;
        self.read(PullRequestRead::ViewedFiles, cx, |page, result| {
            page.viewed.loading = false;
            match result {
                Ok((PullRequestReadResponse::ViewedFiles(viewed), expires_at)) => {
                    page.viewed.expires_at = expires_at;
                    page.viewed.error = None;
                    page.viewed.loaded_at = now();
                    page.viewed.data = Some(viewed);
                    // The host's answer now carries every mark already sent.
                    page.viewed_marks
                        .retain(|path, _| page.viewed_batch.contains_key(path));
                }
                Ok(_) => {}
                Err(error) => page.viewed.error = Some(error),
            }
        });
    }

    /// Starts the reads the visible view needs, and wakes when the soonest of them expires.
    fn ensure_reads(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab(cx);
        let Some(page) = self.page_mut() else { return };
        let now = now();
        let (files_due, conversation_due, viewed_due) = (
            page.files.due(now),
            page.conversation.due(now),
            page.viewed.due(now),
        );
        let mut expiries = vec![page.conversation.expires_at];
        if tab == Tab::Files {
            expiries.extend([page.files.expires_at, page.viewed.expires_at]);
        }
        match tab {
            Tab::Files => {
                if files_due {
                    self.read_files(cx);
                }
                // The review threads drawn on the diff come with the conversation.
                if conversation_due {
                    self.read_conversation(cx);
                }
                if viewed_due {
                    self.read_viewed(cx);
                }
            }
            Tab::Conversation => {
                if conversation_due {
                    self.read_conversation(cx);
                }
            }
        }
        let soonest = expiries.into_iter().filter(|at| *at > now).min();
        if let Some(at) = soonest
            && self.wake.as_ref().is_none_or(|(wake, _)| *wake != at)
        {
            let delay = std::time::Duration::from_secs(at.saturating_sub(now));
            let task = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update(cx, |this, cx| {
                    this.wake = None;
                    cx.notify();
                });
            });
            self.wake = Some((at, task));
        }
    }

    /// A manual refresh: the host drops what it holds about the pull request and the visible
    /// view reads again from the start.
    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session, key)) = self.current.clone() else {
            return;
        };
        let tab = self.tab(cx);
        let Some(page) = self.page_mut() else { return };
        page.refreshing = true;
        page.files.error = None;
        page.conversation.error = None;
        page.viewed.error = None;
        page.conversation_view.replies.clear();
        let task = self.store.update(cx, |store, cx| {
            store.command(
                Command::RefreshPullRequest {
                    session_id: session,
                    key,
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(_) => {
                    match tab {
                        Tab::Files => {
                            this.read_files(cx);
                            this.read_viewed(cx);
                            this.read_conversation(cx);
                        }
                        Tab::Conversation => this.read_conversation(cx),
                    }
                    cx.notify();
                }
                Err(error) => {
                    if let Some(page) = this.page_mut() {
                        page.refreshing = false;
                    }
                    window.push_notification(
                        Notification::warning(
                            crate::tr!("pull_requests.detail.refresh_failed", ago = ago(now()))
                                .into_owned()
                                + " "
                                + &reason(&error),
                        ),
                        cx,
                    );
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn retry(&mut self, cx: &mut Context<Self>) {
        if let Some(page) = self.page_mut() {
            page.files.error = None;
            page.conversation.error = None;
            page.viewed.error = None;
        }
        cx.notify();
    }

    fn change_link(&mut self, action: &ChangeLink, window: &mut Window, cx: &mut Context<Self>) {
        super::change_link(&self.store, action, window, cx);
    }

    fn change_watch(&mut self, action: &ChangeWatch, window: &mut Window, cx: &mut Context<Self>) {
        super::change_watch(&self.store, action, window, cx);
    }

    fn header(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let Some((session, key)) = self.current.clone() else {
            return div().into_any_element();
        };
        let link = self.link(cx);
        let stack = self.stack(cx);
        let layer = stack
            .as_ref()
            .and_then(|stack| stack.layers.iter().find(|layer| layer.number == key.number));
        let visible = link.as_ref().is_some_and(|link| link.visible());
        let snapshot = link
            .as_ref()
            .filter(|link| link.visible())
            .and_then(|link| link.snapshot.clone());
        let badge = row_state(link.as_ref(), layer.map(|layer| layer.state));
        let (glyph, color, state_label) = appearance(badge, cx);
        let title = snapshot
            .as_ref()
            .map(|s| s.title.clone())
            .unwrap_or_else(|| {
                layer
                    .map(|layer| layer.head_branch.clone())
                    .unwrap_or_else(|| key.repository.clone())
            });
        let muted = cx.theme().muted_foreground;
        let text = if compact { 13. } else { 12. };
        let mut line_two = h_flex()
            .flex_wrap()
            .gap_x(px(6.))
            .gap_y(px(2.))
            .items_center()
            .text_size(px(text))
            .text_color(muted)
            .child(
                div()
                    .id("pr-detail-number")
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(format!("#{}", key.number))
                    .tooltip({
                        let label = crate::tr!(
                            "pull_requests.number_tooltip",
                            source = crate::tr!(source(link.as_ref())).into_owned(),
                            ago = ago(link
                                .as_ref()
                                .and_then(|link| link.linked_at)
                                .unwrap_or_default())
                        )
                        .into_owned();
                        move |window, cx| Tooltip::new(label.clone()).build(window, cx)
                    }),
            )
            .child(material::semantic_chip(
                crate::tr!(state_label).into_owned(),
                color.opacity(0.1),
                color,
                cx,
            ));
        if let Some(snapshot) = &snapshot {
            if let Some(author) = &snapshot.author {
                line_two = line_two.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        // The conversation names the avatar the host may read for this author.
                        .child(self.avatar(
                            &author.login,
                            self.author_avatar(&author.login).as_deref(),
                            16.,
                            cx,
                        ))
                        .child(author.login.clone()),
                );
            }
            let (base, head) = (snapshot.base_branch.clone(), snapshot.head_branch.clone());
            line_two =
                line_two
                    .child(
                        div()
                            .id("pr-detail-branches")
                            .min_w_0()
                            .max_w(px(320.))
                            .truncate()
                            .font_family(cx.theme().mono_font_family.clone())
                            // gpui-base has no middle truncation; the tooltip holds the full pair.
                            .child(format!("{base} ← {head}"))
                            .tooltip(move |window, cx| {
                                Tooltip::new(
                                    crate::tr!(
                                        "pull_requests.detail.merging_into",
                                        head = head.clone(),
                                        base = base.clone()
                                    )
                                    .into_owned(),
                                )
                                .build(window, cx)
                            }),
                    )
                    .children(ago_rfc3339(&snapshot.updated_at).map(|ago| {
                        crate::tr!("pull_requests.detail.updated", ago = ago).into_owned()
                    }));
        }
        let mut chips = h_flex().gap(px(6.)).items_center().text_size(px(text));
        let mut has_chip = false;
        if let Some(snapshot) = snapshot
            .as_ref()
            .filter(|s| s.state == PullRequestState::Open)
        {
            let chip = |icon: IconName, color, label: String| {
                h_flex()
                    .flex_none()
                    .gap_1()
                    .items_center()
                    .child(Icon::new(icon).size(px(14.)).text_color(color))
                    .child(label)
            };
            if let Some(checks) = snapshot.checks_state {
                has_chip = true;
                let (icon, color, label) = match checks {
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
                chips = chips.child(chip(icon, color, crate::tr!(label).into_owned()));
            }
            if let Some(review) = snapshot.review_decision {
                has_chip = true;
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
                chips = chips.child(chip(icon, color, crate::tr!(label).into_owned()));
            }
            if snapshot.mergeability == Mergeability::Conflicting {
                has_chip = true;
                chips = chips.child(chip(
                    IconName::GitMergeConflict,
                    cx.theme().warning,
                    crate::tr!(
                        "pull_requests.conflict",
                        base = snapshot.base_branch.clone()
                    )
                    .into_owned(),
                ));
            }
        }
        if let Some(stack) = &stack {
            has_chip = true;
            chips = chips.child(self.layer_selector(stack, &key, cx));
        }
        if visible
            && link.as_ref().is_some_and(|link| link.watch.is_some())
            && snapshot
                .as_ref()
                .is_none_or(|s| s.state == PullRequestState::Open)
        {
            has_chip = true;
            chips = chips.child(
                div()
                    .id("pr-detail-watch")
                    .child(Icon::new(IconName::Eye).size(px(14.)).text_color(muted))
                    .tooltip(|window, cx| {
                        Tooltip::new(crate::tr!("pull_requests.watching_tooltip").into_owned())
                            .build(window, cx)
                    }),
            );
        }
        let condition = match link.as_ref().map(|link| link.source) {
            Some(PullRequestSource::Dismissed) => Some("pull_requests.source_dismissed"),
            None => Some("pull_requests.source_not_linked"),
            _ => None,
        };
        let _ = (&session, window);
        v_flex()
            .flex_none()
            .px(px(if compact {
                material::COMPACT_PAGE_INSET
            } else {
                12.
            }))
            .pt_2()
            .pb_2()
            .gap(px(6.))
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .pt(px(2.))
                            .child(Icon::new(glyph).size(px(16.)).text_color(color)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(if compact { 17. } else { 15. }))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .line_clamp(2)
                            .when(snapshot.is_none(), |title| {
                                title.font_family(cx.theme().mono_font_family.clone())
                            })
                            .child(title),
                    ),
            )
            .child(line_two)
            .when(has_chip, |header| {
                header.child(if compact {
                    div()
                        .id("pr-detail-chips")
                        .overflow_x_scroll()
                        .child(chips)
                        .into_any_element()
                } else {
                    chips.flex_wrap().into_any_element()
                })
            })
            .children(condition.map(|condition| {
                h_flex()
                    .gap_1()
                    .items_center()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(Icon::new(IconName::Unlink).size(px(12.)))
                    .child(crate::tr!(condition))
            }))
            .into_any_element()
    }

    fn layer_selector(
        &self,
        stack: &tcode_core::pull_request::PullRequestStack,
        key: &PullRequestKey,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.compact(cx);
        let index = stack
            .layers
            .iter()
            .position(|layer| layer.number == key.number)
            .map_or(0, |index| index + 1);
        let count = stack.layers.len();
        let muted = cx.theme().muted_foreground;
        let trigger = Button::new("pr-layer-select")
            .ghost()
            .outline()
            .compact()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .child(Icon::new(IconName::Layers).size(px(14.)))
                    .child(
                        crate::tr!(
                            "pull_requests.layer_position",
                            index = index.to_string(),
                            count = count.to_string()
                        )
                        .into_owned(),
                    )
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
            );
        let links = self
            .current
            .as_ref()
            .map(|(session, _)| self.store.read(cx).pull_requests(session).to_vec())
            .unwrap_or_default();
        let stack = stack.clone();
        let current = key.clone();
        let view = cx.entity();
        let popover = Popover::new("pr-layer-popover").trigger(trigger);
        let popover = if compact {
            popover.bottom_sheet(crate::tr!("pull_requests.detail.stack_title").into_owned())
        } else {
            popover
        };
        popover
            .content(move |_, _, cx| {
                let popover = cx.entity();
                let rows = stack.layers.iter().rev().map(|layer| {
                    let layer_key =
                        PullRequestKey::new(&current.host, &current.repository, layer.number);
                    let link = links.iter().find(|link| link.key == layer_key);
                    let linked = link.is_some_and(|link| link.visible());
                    let (glyph, color, _) = appearance(row_state(link, Some(layer.state)), cx);
                    let selected = layer_key == current;
                    let title = link
                        .and_then(|link| link.snapshot.as_ref())
                        .filter(|_| linked)
                        .map(|snapshot| snapshot.title.clone());
                    let condition = match link.map(|link| link.source) {
                        Some(PullRequestSource::Dismissed) => {
                            Some(crate::tr!("pull_requests.source_dismissed"))
                        }
                        None => Some(crate::tr!("pull_requests.detail.layer_not_linked")),
                        _ => None,
                    };
                    let view = view.clone();
                    let popover = popover.clone();
                    let label = format!("#{} {}", layer.number, title.clone().unwrap_or_default());
                    material::accessible_clickable(
                        h_flex(),
                        ("pr-layer", layer.number as usize),
                        gpui::Role::MenuItem,
                        label,
                        cx,
                    )
                    .aria_selected(selected)
                    .w_full()
                    .h(px(if compact { 44. } else { 32. }))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .rounded(cx.theme().tokens.radius.sm)
                    .cursor_pointer()
                    .hover(|row| row.bg(cx.theme().list_hover))
                    .when(selected, |row| row.bg(cx.theme().list_active))
                    .child(Icon::new(glyph).size(px(14.)).text_color(if linked {
                        color
                    } else {
                        cx.theme().muted_foreground
                    }))
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(px(12.))
                            .child(format!("#{}", layer.number)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .when(title.is_none(), |text| {
                                text.font_family(cx.theme().mono_font_family.clone())
                                    .text_color(cx.theme().muted_foreground)
                            })
                            .child(title.unwrap_or_else(|| layer.head_branch.clone())),
                    )
                    .children(condition.map(|condition| {
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(condition)
                    }))
                    .when(selected, |row| {
                        row.child(Icon::new(IconName::Check).size(px(12.)))
                    })
                    .on_click(move |_, window, cx| {
                        view.update(cx, |view, cx| {
                            if let Some((session, _)) = view.current.clone() {
                                view.store.update(cx, |store, cx| {
                                    store.set_open_pull_request(
                                        &session,
                                        Some(layer_key.clone()),
                                        cx,
                                    )
                                });
                                view.show(session, layer_key.clone(), cx);
                            }
                        });
                        popover.update(cx, |state, cx| state.dismiss(window, cx));
                    })
                });
                v_flex()
                    .id("pr-layer-list")
                    .role(gpui::Role::Menu)
                    .w(px(340.))
                    .max_h(px(360.))
                    .p_1()
                    .gap_0p5()
                    .overflow_y_scroll()
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .gap_1()
                            .items_center()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(Icon::new(IconName::Layers).size(px(12.)))
                            .child(
                                crate::tr!(
                                    "pull_requests.stack_caption",
                                    count = stack.layers.len().to_string(),
                                    base = stack.base.clone()
                                )
                                .into_owned(),
                            ),
                    )
                    .children(rows)
            })
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_xl()
            .rounded(material::radius_overlay(cx))
            .into_any_element()
    }

    /// The menu of the row this pull request was opened from.
    fn menu(&self, cx: &App) -> Option<super::RowMenu> {
        let (session, key) = self.current.as_ref()?;
        let link = self.link(cx);
        let watchable = self
            .store
            .read(cx)
            .thread_meta(session)
            .is_some_and(|meta| {
                !meta.is_settled() && meta.archived_at.is_none() && meta.parent_session_id.is_none()
            });
        Some(row_menu(
            key.clone(),
            self.url(cx).unwrap_or_default(),
            link.as_ref(),
            watchable,
        ))
    }

    fn sub_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let key = self.current.as_ref().map(|(_, key)| key.clone());
        let refreshing = self.page().is_some_and(|page| page.refreshing);
        let url = self.url(cx);
        let store = self.store.clone();
        let session = self.current.as_ref().map(|(session, _)| session.clone());
        h_flex()
            .flex_none()
            .h(px(40.))
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("pr-detail-back")
                    .ghost()
                    .small()
                    .icon(IconName::ChevronLeft)
                    .label(crate::tr!("pull_requests.detail.back"))
                    .tooltip(crate::tr!("pull_requests.detail.back_tooltip"))
                    .on_click(move |_, _, cx| {
                        if let Some(session) = &session {
                            store.update(cx, |store, cx| {
                                store.set_open_pull_request(session, None, cx)
                            });
                        }
                    }),
            )
            .child(div().flex_1())
            .child(self.refresh_button(refreshing, false, cx))
            .when_some(url, |bar, url| {
                bar.child(
                    Button::new("pr-detail-open")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::ExternalLink)
                        .tooltip(crate::tr!("pull_requests.open_on_github"))
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            })
            .when_some(self.menu(cx).zip(key), |bar, (menu, key)| {
                bar.child(
                    Button::new("pr-detail-menu")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::Ellipsis)
                        .tooltip(
                            crate::tr!(
                                "pull_requests.actions_for",
                                number = key.number.to_string()
                            )
                            .into_owned(),
                        )
                        .dropdown_menu(move |menu_state, window, cx| {
                            (menu)(menu_state, window, cx)
                        }),
                )
            })
            .into_any_element()
    }

    pub(super) fn refresh_button(
        &self,
        refreshing: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let loaded = self
            .page()
            .map(|page| {
                [page.files.loaded_at, page.conversation.loaded_at]
                    .into_iter()
                    .filter(|at| *at > 0)
                    .min()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let tooltip = if refreshing {
            crate::tr!("pull_requests.detail.refreshing").into_owned()
        } else {
            crate::tr!("pull_requests.detail.refresh", ago = ago(loaded)).into_owned()
        };
        if refreshing {
            return div()
                .flex_none()
                .size(px(if compact { material::TOUCH_TARGET } else { 24. }))
                .flex()
                .items_center()
                .justify_center()
                .child(crate::widgets::spinner::Spinner::new().small())
                .into_any_element();
        }
        material::toolbar_icon_button("pr-detail-refresh", IconName::RefreshCw, tooltip, compact)
            .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx)))
            .into_any_element()
    }

    /// The phone nav bar's trailing actions: refresh and the row menu.
    pub fn nav_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let refreshing = self.page().is_some_and(|page| page.refreshing);
        let mut actions = vec![self.refresh_button(refreshing, true, cx)];
        if let Some((menu, key)) = self
            .menu(cx)
            .zip(self.current.as_ref().map(|(_, key)| key.clone()))
        {
            actions.push(
                material::toolbar_icon_button(
                    "pr-detail-menu",
                    IconName::Ellipsis,
                    crate::tr!("pull_requests.actions_for", number = key.number.to_string())
                        .into_owned(),
                    true,
                )
                .dropdown_menu(move |menu_state, window, cx| (menu)(menu_state, window, cx))
                .into_any_element(),
            );
        }
        actions
    }

    /// The nav bar title on a phone: the number only, the title is in the header.
    pub fn nav_title(&self) -> SharedString {
        self.current
            .as_ref()
            .map(|(_, key)| format!("#{}", key.number))
            .unwrap_or_default()
            .into()
    }

    fn switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let tab = self.tab(cx);
        let page = self.page();
        let files = page.and_then(|page| page.files.data.as_ref());
        let link = self.link(cx);
        let file_count = files.map(|files| files.changed_files).or_else(|| {
            link.as_ref()
                .and_then(|link| link.snapshot.as_ref().map(|s| s.changed_files))
        });
        let conversation_count = page
            .and_then(|page| page.conversation.data.as_ref())
            .map(|conversation| conversation.comments.len());
        let label = |plain: &str, counted: &str, count: Option<u64>| match count {
            Some(count) => crate::tr!(counted, count = count.to_string()).into_owned(),
            None => crate::tr!(plain).into_owned(),
        };
        let segments = [
            (
                "files",
                label(
                    "pull_requests.detail.files",
                    "pull_requests.detail.files_count",
                    file_count,
                ),
                Tab::Files,
            ),
            (
                "conversation",
                label(
                    "pull_requests.detail.conversation",
                    "pull_requests.detail.conversation_count",
                    conversation_count.map(|count| count as u64),
                ),
                Tab::Conversation,
            ),
        ]
        .into_iter()
        .map(|(id, label, segment)| {
            material::segment(
                SharedString::from(format!("pr-view-{id}")),
                label,
                tab == segment,
                cx,
            )
            .on_change({
                let view = cx.entity();
                move |_, _, _, cx| view.update(cx, |view, cx| view.select_tab(segment, cx))
            })
        })
        .collect::<Vec<_>>();
        let stats = (tab == Tab::Files)
            .then(|| link.as_ref().and_then(|link| link.snapshot.clone()))
            .flatten()
            .map(|snapshot| {
                h_flex()
                    .flex_none()
                    .gap_1()
                    .text_size(px(11.))
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(
                        div()
                            .text_color(cx.theme().success)
                            .child(format!("+{}", snapshot.additions)),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().danger)
                            .child(format!("−{}", snapshot.deletions)),
                    )
            });
        h_flex()
            .flex_none()
            .h(px(if compact { material::TOUCH_TARGET } else { 36. }))
            .px(px(if compact {
                material::COMPACT_PAGE_INSET
            } else {
                12.
            }))
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(material::segmented_track("pr-view", segments, cx)),
            )
            .children(stats)
            .into_any_element()
    }

    /// Host conditions above the switch: a credential to connect, a rate limit, an unlink.
    fn notices(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        let errors = self.page().map(|page| {
            [
                &page.files.error,
                &page.conversation.error,
                &page.viewed.error,
            ]
            .into_iter()
            .flatten()
            .map(|error| error.code.clone())
            .collect::<Vec<_>>()
        });
        let errors = errors.unwrap_or_default();
        let notice = |text: String, action: Option<AnyElement>, cx: &App| {
            h_flex()
                .mx(px(12.))
                .px_3()
                .py_2()
                .gap_2()
                .items_center()
                .rounded_md()
                .bg(cx.theme().muted)
                .text_size(px(12.))
                .child(div().flex_1().min_w_0().child(text))
                .children(action)
                .into_any_element()
        };
        if errors.iter().any(|code| {
            code == "pull_request_no_credential" || code == "pull_request_host_disabled"
        }) {
            notices.push(notice(
                crate::tr!(
                    "pull_requests.notice_no_credential",
                    host = self
                        .current
                        .as_ref()
                        .map(|(_, key)| key.host.clone())
                        .unwrap_or_default()
                )
                .into_owned(),
                Some(
                    Button::new("pr-detail-settings")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.open_settings"))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(super::OpenSourceControl), cx)
                        })
                        .into_any_element(),
                ),
                cx,
            ));
        }
        if errors
            .iter()
            .any(|code| code == "pull_request_rate_limited")
        {
            notices.push(notice(
                crate::tr!(
                    "pull_requests.notice_rate_limited",
                    host = self
                        .current
                        .as_ref()
                        .map(|(_, key)| key.host.clone())
                        .unwrap_or_default(),
                    time = String::new()
                )
                .into_owned(),
                None,
                cx,
            ));
        }
        let unlinked = self
            .link(cx)
            .is_some_and(|link| !link.visible() && link.source != PullRequestSource::Stack)
            || (self.link(cx).is_none() && self.stack(cx).is_none());
        if unlinked
            && let Some((url, key)) = self
                .url(cx)
                .zip(self.current.as_ref().map(|(_, key)| key.clone()))
        {
            notices.push(notice(
                crate::tr!("pull_requests.detail.no_longer_linked").into_owned(),
                Some(
                    Button::new("pr-detail-relink")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.relink"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(ChangeLink {
                                    key: key.clone(),
                                    url: url.clone(),
                                    linking: true,
                                }),
                                cx,
                            )
                        })
                        .into_any_element(),
                ),
                cx,
            ));
        }
        // A refresh that failed while content stays on screen.
        let stale = self.page().and_then(|page| {
            let files = page
                .files
                .data
                .as_ref()
                .and(page.files.error.as_ref())
                .map(|error| (error.code.clone(), page.files.loaded_at));
            let conversation = page
                .conversation
                .data
                .as_ref()
                .and(page.conversation.error.as_ref())
                .map(|error| (error.code.clone(), page.conversation.loaded_at));
            files
                .or(conversation)
                .filter(|(code, _)| code != "pull_request_rate_limited")
        });
        if let Some((_, loaded_at)) = stale {
            notices.push(notice(
                crate::tr!("pull_requests.detail.refresh_failed", ago = ago(loaded_at))
                    .into_owned(),
                Some(
                    Button::new("pr-detail-retry")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.detail.retry"))
                        .on_click(cx.listener(|this, _, _, cx| this.retry(cx)))
                        .into_any_element(),
                ),
                cx,
            ));
        }
        notices
    }

    /// The body when nothing could be read at all.
    pub(super) fn failure(
        &self,
        error: &ProtocolError,
        title: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some((_, key)) = self.current.clone() else {
            return div().into_any_element();
        };
        match error.code.as_str() {
            "pull_request_no_credential" | "pull_request_host_disabled" => material::empty_state(
                Icon::new(IconName::Lock),
                crate::tr!("pull_requests.detail.no_credential_title").into_owned(),
                String::new(),
                cx,
            )
            .into_any_element(),
            "pull_request_not_found" => {
                let url = self.url(cx);
                material::empty_state(
                    Icon::new(IconName::GitPullRequest),
                    crate::tr!(
                        "pull_requests.detail.unavailable_title",
                        number = key.number.to_string()
                    )
                    .into_owned(),
                    crate::tr!("pull_requests.detail.unavailable_desc").into_owned(),
                    cx,
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("pr-unavailable-retry")
                                .outline()
                                .small()
                                .label(crate::tr!("pull_requests.detail.retry"))
                                .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                        )
                        .children(url.map(|url| {
                            Button::new("pr-unavailable-open")
                                .ghost()
                                .small()
                                .label(crate::tr!("pull_requests.open_on_github"))
                                .on_click(move |_, _, cx| cx.open_url(&url))
                        })),
                )
                .into_any_element()
            }
            _ => v_flex()
                .m_3()
                .p_3()
                .gap_2()
                .rounded(material::radius_card(cx))
                .bg(cx.theme().danger.opacity(0.08))
                .border_1()
                .border_color(cx.theme().danger.opacity(0.3))
                .text_size(px(13.))
                .child(div().font_weight(gpui::FontWeight::MEDIUM).child(title))
                .child(
                    div()
                        .id("pr-failure-reason")
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(reason(error))
                        .tooltip({
                            let full = error.message.clone();
                            move |window, cx| Tooltip::new(full.clone()).build(window, cx)
                        }),
                )
                .child(
                    div().child(
                        Button::new("pr-failure-retry")
                            .outline()
                            .xsmall()
                            .label(crate::tr!("pull_requests.detail.retry"))
                            .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                    ),
                )
                .into_any_element(),
        }
    }

    fn author_avatar(&self, login: &str) -> Option<String> {
        let conversation = self.page()?.conversation.data.as_ref()?;
        std::iter::once(&conversation.description)
            .chain(&conversation.comments)
            .filter_map(|comment| comment.author.as_ref())
            .find(|author| author.login == login)?
            .avatar_url
            .clone()
    }

    pub(super) fn files_of(&self) -> Option<&[PullRequestFile]> {
        self.page()
            .and_then(|page| page.files.data.as_ref())
            .map(|files| files.files.as_slice())
    }
}

impl Render for PullRequestView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.current.is_none() {
            return div().into_any_element();
        }
        self.ensure_reads(cx);
        let compact = self.compact(cx);
        let tab = self.tab(cx);
        let body = match tab {
            Tab::Files => self.render_files(window, cx),
            Tab::Conversation => self.render_conversation(window, cx),
        };
        v_flex()
            .size_full()
            .min_w_0()
            .on_action(cx.listener(Self::change_link))
            .on_action(cx.listener(Self::change_watch))
            .on_action(cx.listener(Self::copy_selected_lines))
            .when(!compact, |view| view.child(self.sub_bar(cx)))
            .child(self.header(window, cx))
            .children(self.notices(cx))
            .child(self.switch(cx))
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .into_any_element()
    }
}
