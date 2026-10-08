//! The Conversation view: the description, comments, reviews and review threads, oldest first.

use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    list, prelude::FluentBuilder as _, px,
};
use gpui_base::{Avatar, AvatarFallback, AvatarImage, h_flex, v_flex};
use tcode_core::{pull_request::PullRequestState, session::ReviewSide};
use tcode_protocol::{
    PullRequestComment, PullRequestRead, PullRequestReadResponse, PullRequestReviewState,
    PullRequestReviewThread,
};

use super::detail::{PullRequestView, Tab, ago_rfc3339, reason};
use crate::{
    icon::{Icon, IconName},
    markdown::{ImageResolver, MarkdownState, MarkdownView},
    material,
    sizing::Sizable as _,
    store::{MediaState, pull_request_media, pull_request_media_state},
    theme::ActiveTheme as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        menu::{CopyText, DropdownMenu as _, OpenUrl},
        tooltip::Tooltip,
    },
};

/// One row of the conversation list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Notice,
    Description,
    Comment(usize),
    Thread(usize),
    /// Merged or closed, from the thread's snapshot of the pull request.
    Event(PullRequestState, String),
}

/// Whether the client reads a URL through the host: GitHub's own hosts, where a credential
/// may decide the answer. Every other image keeps loading by its URL.
fn host_read(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some_and(|host| {
                let host = host.to_ascii_lowercase();
                host == "github.com"
                    || host == "www.github.com"
                    || host.ends_with(".githubusercontent.com")
            })
    })
}

/// The body without HTML comments, which GitHub templates leave behind; `None` when nothing
/// is left to show.
fn visible_body(body: &str) -> Option<String> {
    let mut visible = String::new();
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        visible.push_str(&rest[..start]);
        rest = rest[start..]
            .find("-->")
            .map_or("", |end| &rest[start + end + 3..]);
    }
    visible.push_str(rest);
    let visible = visible.trim().to_owned();
    (!visible.is_empty()).then_some(visible)
}

fn reaction_emoji(content: &str) -> &'static str {
    match content {
        "THUMBS_UP" => "👍",
        "THUMBS_DOWN" => "👎",
        "LAUGH" => "😄",
        "HOORAY" => "🎉",
        "CONFUSED" => "😕",
        "HEART" => "❤️",
        "ROCKET" => "🚀",
        "EYES" => "👀",
        _ => "·",
    }
}

impl PullRequestView {
    fn items(&self, cx: &App) -> Vec<Item> {
        let Some(conversation) = self.page().and_then(|page| page.conversation.data.as_ref())
        else {
            return Vec::new();
        };
        let mut timed: Vec<(String, Item)> = Vec::new();
        for (index, comment) in conversation.comments.iter().enumerate() {
            if visible_body(&comment.body).is_some() || comment.review_state.is_some() {
                timed.push((comment.created_at.clone(), Item::Comment(index)));
            }
        }
        for (index, thread) in conversation.threads.iter().enumerate() {
            let at = thread
                .comments
                .first()
                .map(|comment| comment.created_at.clone())
                .unwrap_or_default();
            timed.push((at, Item::Thread(index)));
        }
        if let Some(snapshot) = self.link(cx).and_then(|link| link.snapshot) {
            match snapshot.state {
                PullRequestState::Merged => {
                    if let Some(at) = snapshot.merged_at {
                        timed.push((at.clone(), Item::Event(PullRequestState::Merged, at)));
                    }
                }
                PullRequestState::Closed => {
                    if let Some(at) = snapshot.closed_at {
                        timed.push((at.clone(), Item::Event(PullRequestState::Closed, at)));
                    }
                }
                PullRequestState::Open => {}
            }
        }
        timed.sort_by(|left, right| left.0.cmp(&right.0));
        let mut items = Vec::with_capacity(timed.len() + 2);
        if !conversation.complete {
            items.push(Item::Notice);
        }
        items.push(Item::Description);
        items.extend(timed.into_iter().map(|(_, item)| item));
        items
    }

    /// Switches to the conversation at a thread.
    pub(super) fn show_thread(&mut self, id: &str, cx: &mut Context<Self>) {
        let items = self.items(cx);
        let Some(page) = self.page_mut() else { return };
        page.tab = Some(Tab::Conversation);
        let position = items.iter().position(|item| match item {
            Item::Thread(index) => page
                .conversation
                .data
                .as_ref()
                .and_then(|conversation| conversation.threads.get(*index))
                .is_some_and(|thread| thread.id == id),
            _ => false,
        });
        page.conversation_view.expanded.insert(id.to_owned());
        if let Some(position) = position {
            if page.conversation_view.list.item_count() != items.len() {
                page.conversation_view.list.reset(items.len());
            }
            page.conversation_view.list.scroll_to(gpui::ListOffset {
                item_ix: position,
                offset_in_item: px(0.),
            });
        }
        cx.notify();
    }

    fn show_in_files(&mut self, thread: &PullRequestReviewThread, cx: &mut Context<Self>) {
        let Some(anchor) = thread.anchor.clone() else {
            return;
        };
        let index = self
            .files_of()
            .and_then(|files| files.iter().position(|file| file.path == anchor.path));
        if let Some(page) = self.page_mut() {
            page.tab = Some(Tab::Files);
            page.files_view.collapsed.insert(anchor.path.clone(), false);
            if let (Some(index), Some(list)) = (index, page.files_view.list.as_ref()) {
                list.scroll_to_file(index);
            }
        }
        cx.notify();
    }

    fn resolver(&self) -> Option<ImageResolver> {
        let (session, key) = self.current.clone()?;
        let account = self.page()?.conversation.data.as_ref()?.account.clone();
        let source = {
            let (session, key, account) = (session.clone(), key.clone(), account.clone());
            Rc::new(move |url: &str| {
                host_read(url).then(|| {
                    pull_request_media(
                        session.clone(),
                        key.clone(),
                        account.clone(),
                        url.to_owned(),
                    )
                })
            })
        };
        let pending = Rc::new(
            move |url: &str,
                  title: &str,
                  window: &mut Window,
                  cx: &mut App|
                  -> Option<AnyElement> {
                if !host_read(url) {
                    return None;
                }
                let state = pull_request_media_state(&session, &key, &account, url, window, cx);
                media_stand_in(state, url, title, cx)
            },
        );
        Some(ImageResolver { source, pending })
    }

    /// The comment's rendered body, kept per comment so its selection and layout survive
    /// re-reads; a conversation read as another account gets fresh ones.
    fn markdown(&self, id: &str, body: &str, cx: &mut Context<Self>) -> Entity<MarkdownState> {
        let resolver = self.resolver();
        let account = self
            .page()
            .and_then(|page| page.conversation.data.as_ref())
            .map(|conversation| conversation.account.clone())
            .unwrap_or_default();
        let Some(page) = self.page() else {
            return cx.new(|cx| MarkdownState::new(body, cx));
        };
        let held = page
            .conversation_view
            .markdown
            .borrow()
            .get(id)
            .filter(|(held_account, _)| *held_account == account)
            .map(|(_, state)| state.clone());
        if let Some(state) = held {
            state.update(cx, |state, cx| state.set_text(body, cx));
            return state;
        }
        let state = cx.new(|cx| {
            let mut state = MarkdownState::new(body, cx);
            state.set_selectable(true, cx);
            state.set_compact_headings(true, cx);
            state.set_image_resolver(resolver, cx);
            state
        });
        page.conversation_view
            .markdown
            .borrow_mut()
            .insert(id.to_owned(), (account, state.clone()));
        state
    }

    pub(super) fn avatar(&self, login: &str, url: Option<&str>, size: f32, cx: &App) -> AnyElement {
        let initial = login
            .chars()
            .next()
            .map(|initial| initial.to_uppercase().to_string())
            .unwrap_or_default();
        let fallback = Avatar::new()
            .size(px(size))
            .rounded_full()
            .bg(cx.theme().muted)
            .fallback(
                AvatarFallback::new()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(size * 0.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(cx.theme().muted_foreground)
                    .child(initial),
            );
        let image = self
            .current
            .as_ref()
            .zip(url)
            .and_then(|((session, key), url)| {
                let account = self.page()?.conversation.data.as_ref()?.account.clone();
                Some(
                    Avatar::new()
                        .absolute()
                        .inset_0()
                        .rounded_full()
                        .overflow_hidden()
                        .image(
                            AvatarImage::new(pull_request_media(
                                session.clone(),
                                key.clone(),
                                account,
                                url.to_owned(),
                            ))
                            .size(px(size))
                            .rounded_full(),
                        ),
                )
            });
        // The initial stands behind the picture until it loads, and instead of it on failure.
        div()
            .relative()
            .flex_none()
            .size(px(size))
            .child(fallback)
            .children(image)
            .into_any_element()
    }

    fn comment_head(
        &self,
        comment: &PullRequestComment,
        action: Option<AnyElement>,
        size: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let login = comment
            .author
            .as_ref()
            .map(|author| author.login.clone())
            .unwrap_or_else(|| crate::tr!("pull_requests.conversation.ghost").into_owned());
        let avatar = self.avatar(
            &login,
            comment
                .author
                .as_ref()
                .and_then(|author| author.avatar_url.as_deref()),
            size,
            cx,
        );
        let created = comment.created_at.clone();
        let url = comment.url.clone();
        let body = comment.body.clone();
        h_flex()
            .gap_2()
            .items_center()
            .text_size(px(12.))
            .child(avatar)
            .child(div().font_weight(gpui::FontWeight::MEDIUM).child(login))
            .children(action)
            .child(
                div()
                    .id(SharedString::from(format!("pr-ago-{}", comment.id)))
                    .text_color(muted)
                    .children(ago_rfc3339(&comment.created_at))
                    .tooltip(move |window, cx| {
                        let local = chrono::DateTime::parse_from_rfc3339(&created)
                            .map(|time| {
                                time.with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d %H:%M")
                                    .to_string()
                            })
                            .unwrap_or_default();
                        Tooltip::new(local).build(window, cx)
                    }),
            )
            .when(comment.edited_at.is_some(), |head| {
                head.child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(crate::tr!("pull_requests.conversation.edited")),
                )
            })
            .child(div().flex_1())
            .child(
                Button::new(SharedString::from(format!(
                    "pr-comment-menu-{}",
                    comment.id
                )))
                .ghost()
                .xsmall()
                .compact()
                .icon(IconName::Ellipsis)
                .dropdown_menu(move |menu, _, _| {
                    let menu = match &url {
                        Some(url) => menu
                            .menu(
                                crate::tr!("pull_requests.conversation.copy_comment_link")
                                    .into_owned(),
                                Box::new(CopyText(url.clone())),
                            )
                            .menu(
                                crate::tr!("pull_requests.open_on_github").into_owned(),
                                Box::new(OpenUrl(url.clone())),
                            ),
                        None => menu,
                    };
                    menu.menu(
                        crate::tr!("pull_requests.conversation.copy_text").into_owned(),
                        Box::new(CopyText(body.clone())),
                    )
                }),
            )
            .into_any_element()
    }

    fn reactions(&self, comment: &PullRequestComment, cx: &App) -> Option<AnyElement> {
        if comment.reactions.is_empty() {
            return None;
        }
        Some(
            h_flex()
                .flex_wrap()
                .gap_1()
                .pt_1()
                .children(comment.reactions.iter().map(|reaction| {
                    let own = reaction.viewer_reacted;
                    h_flex()
                        .h(px(22.))
                        .px(px(6.))
                        .gap_1()
                        .items_center()
                        .rounded_full()
                        .border_1()
                        .border_color(if own {
                            cx.theme().primary
                        } else {
                            cx.theme().border
                        })
                        .bg(if own {
                            cx.theme().primary.opacity(0.1)
                        } else {
                            cx.theme().secondary
                        })
                        .text_size(px(12.))
                        .child(reaction_emoji(&reaction.content))
                        .child(reaction.count.to_string())
                }))
                .into_any_element(),
        )
    }

    fn body(
        &self,
        comment: &PullRequestComment,
        clamp: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let body = visible_body(&comment.body)?;
        let state = self.markdown(&comment.id, &body, cx);
        let expanded = self
            .page()
            .is_some_and(|page| page.conversation_view.expanded.contains(&comment.id));
        // A long body is clipped until the reader asks for all of it.
        let long = clamp && body.lines().count() > 16;
        let id = comment.id.clone();
        Some(
            v_flex()
                .min_w_0()
                .text_size(px(13.))
                .child(
                    div()
                        .min_w_0()
                        .when(long && !expanded, |body| {
                            body.max_h(px(320.)).overflow_hidden()
                        })
                        .child(
                            MarkdownView::new(&state)
                                .compact_headings(true)
                                .selectable(true),
                        ),
                )
                .when(long, |body| {
                    body.child(
                        div().pt_1().child(
                            Button::new(SharedString::from(format!("pr-comment-more-{id}")))
                                .ghost()
                                .xsmall()
                                .label(if expanded {
                                    crate::tr!("pull_requests.conversation.show_less")
                                } else {
                                    crate::tr!("pull_requests.conversation.show_full")
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_expanded(&id, cx);
                                })),
                        ),
                    )
                })
                .into_any_element(),
        )
    }

    fn toggle_expanded(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(page) = self.page_mut() {
            let view = &mut page.conversation_view;
            if !view.expanded.remove(id) {
                view.expanded.insert(id.to_owned());
            }
            view.list.remeasure();
        }
        cx.notify();
    }

    fn comment_card(
        &mut self,
        comment: &PullRequestComment,
        description: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.compact(cx);
        let muted = cx.theme().muted_foreground;
        let verdict = comment.review_state.map(|state| {
            let (icon, color, label) = match state {
                PullRequestReviewState::Approved => (
                    IconName::BadgeCheck,
                    cx.theme().success,
                    "pull_requests.conversation.approved",
                ),
                PullRequestReviewState::ChangesRequested => (
                    IconName::MessageSquareWarning,
                    cx.theme().danger,
                    "pull_requests.conversation.changes_requested",
                ),
                PullRequestReviewState::Dismissed => (
                    IconName::MessageSquare,
                    muted,
                    "pull_requests.conversation.dismissed",
                ),
                PullRequestReviewState::Commented | PullRequestReviewState::Pending => (
                    IconName::MessageSquare,
                    muted,
                    "pull_requests.conversation.reviewed",
                ),
            };
            h_flex()
                .gap_1()
                .items_center()
                .text_color(color)
                .child(Icon::new(icon).size(px(12.)))
                .child(crate::tr!(label))
                .into_any_element()
        });
        let action = verdict.or_else(|| {
            Some(
                div()
                    .text_color(muted)
                    .child(crate::tr!(if description {
                        "pull_requests.conversation.opened"
                    } else {
                        "pull_requests.conversation.commented"
                    }))
                    .into_any_element(),
            )
        });
        let head = self.comment_head(comment, action, if compact { 24. } else { 20. }, cx);
        let body = self.body(comment, true, cx);
        let empty_description = description && body.is_none();
        v_flex()
            .w_full()
            .min_w_0()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(head)
            .children(body)
            .when(empty_description, |card| {
                card.child(
                    div()
                        .text_size(px(13.))
                        .text_color(muted)
                        .child(crate::tr!("pull_requests.conversation.no_description")),
                )
            })
            .children(self.reactions(comment, cx))
            .into_any_element()
    }

    fn read_replies(&mut self, thread: &PullRequestReviewThread, cx: &mut Context<Self>) {
        let id = thread.id.clone();
        let after = self
            .page()
            .and_then(|page| page.conversation_view.replies.get(&id))
            .and_then(|replies| replies.after.clone())
            .or_else(|| thread.replies_after.clone());
        let Some(after) = after else { return };
        let Some(page) = self.page_mut() else { return };
        page.conversation_view
            .replies
            .entry(id.clone())
            .or_default()
            .loading = true;
        let key = id.clone();
        self.read(
            PullRequestRead::ThreadReplies {
                thread_id: id,
                after,
            },
            cx,
            move |page, result| {
                let replies = page.conversation_view.replies.entry(key).or_default();
                replies.loading = false;
                match result {
                    Ok((PullRequestReadResponse::ThreadReplies(more), _)) => {
                        replies.comments.extend(more.comments);
                        replies.after = more.after;
                        page.conversation_view.list.remeasure();
                    }
                    Ok(_) => {}
                    Err(error) => {
                        replies.after = None;
                        page.conversation_view.list.remeasure();
                        log::warn!("pull request replies: {}", reason(&error));
                    }
                }
            },
        );
    }

    /// A review thread: the head line and first comment inline on the diff, the whole thread
    /// in the conversation.
    pub(super) fn thread_card(
        &self,
        thread: &PullRequestReviewThread,
        inline: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let expanded = self
            .page()
            .is_some_and(|page| page.conversation_view.expanded.contains(&thread.id));
        let collapsed = thread.resolved && !expanded;
        let lines = thread.anchor.as_ref().map(|anchor| {
            let lines = if anchor.start_line == anchor.end_line {
                crate::tr!(
                    "pull_requests.conversation.line",
                    line = anchor.end_line.to_string()
                )
            } else {
                crate::tr!(
                    "pull_requests.conversation.lines",
                    start = anchor.start_line.to_string(),
                    end = anchor.end_line.to_string()
                )
            }
            .into_owned();
            if anchor.side == ReviewSide::Old {
                format!(
                    "{lines} {}",
                    crate::tr!("pull_requests.conversation.base_side")
                )
            } else {
                lines
            }
        });
        let rail = if thread.resolved || thread.outdated {
            muted
        } else {
            cx.theme().primary
        };
        let count = thread.total_comments.max(thread.comments.len() as u64);
        let id = thread.id.clone();
        let thread_for_files = thread.clone();
        let current = thread.anchor.is_some() && !thread.outdated;
        let chip = |label: &'static str, cx: &App| {
            material::semantic_chip(
                crate::tr!(label).into_owned(),
                cx.theme().secondary,
                muted,
                cx,
            )
        };
        let head = h_flex()
            .id(SharedString::from(format!(
                "pr-thread-head-{}-{inline}",
                thread.id
            )))
            .min_h(px(28.))
            .px_3()
            .gap_2()
            .items_center()
            .text_size(px(11.))
            .text_color(muted)
            .font_family(cx.theme().font_family.clone())
            .when(!inline, |head| {
                head.child(Icon::new(IconName::File).size(px(12.))).child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(px(12.))
                        .text_color(cx.theme().foreground)
                        .child(match thread.anchor.as_ref() {
                            Some(anchor) if anchor.start_line != anchor.end_line => {
                                format!("{}:{}–{}", thread.path, anchor.start_line, anchor.end_line)
                            }
                            Some(anchor) => format!("{}:{}", thread.path, anchor.end_line),
                            None => thread.path.clone(),
                        }),
                )
            })
            .when(inline, |head| head.children(lines.clone()))
            .when(thread.outdated, |head| {
                head.child(chip("pull_requests.conversation.outdated", cx))
            })
            .when(thread.resolved, |head| {
                head.child(chip("pull_requests.conversation.resolved", cx))
            })
            .child(
                crate::tr!(
                    if count == 1 {
                        "pull_requests.conversation.comment_count_one"
                    } else {
                        "pull_requests.conversation.comment_count"
                    },
                    count = count.to_string()
                )
                .into_owned(),
            )
            .child(div().flex_1())
            .when(inline, |head| {
                let id = id.clone();
                head.child(
                    Button::new(SharedString::from(format!("pr-thread-show-{}", thread.id)))
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.files.show_in_conversation"))
                        .on_click(cx.listener(move |this, _, _, cx| this.show_thread(&id, cx))),
                )
            })
            .when(!inline && current, |head| {
                head.child(
                    Button::new(SharedString::from(format!("pr-thread-files-{}", thread.id)))
                        .ghost()
                        .xsmall()
                        .label(crate::tr!("pull_requests.conversation.view_in_files"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.show_in_files(&thread_for_files, cx)
                        })),
                )
            })
            .when(thread.resolved, |head| {
                let id = id.clone();
                head.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(&id, cx)))
            });
        let mut card = v_flex()
            .min_w_full()
            .my_1()
            .pb(px(if collapsed { 0. } else { 6. }))
            .relative()
            .rounded(material::radius_card(cx))
            .bg(cx.theme().muted)
            .font_family(cx.theme().font_family.clone())
            .child(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(6.))
                    .bottom(px(6.))
                    .w(px(2.))
                    .rounded_full()
                    .bg(rail),
            )
            .child(head);
        if collapsed {
            return card.into_any_element();
        }
        if !inline && let Some(hunk) = &thread.diff_hunk {
            card = card.child(
                v_flex()
                    .mx_3()
                    .mb_1()
                    .text_size(px(12.))
                    .font_family(cx.theme().mono_font_family.clone())
                    .children(hunk.lines().map(|line| {
                        let bg = match line.chars().next() {
                            Some('+') => Some(cx.theme().success.opacity(0.13)),
                            Some('-') => Some(cx.theme().danger.opacity(0.12)),
                            _ => None,
                        };
                        div()
                            .px_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .when_some(bg, |row, bg| row.bg(bg))
                            .child(line.to_owned())
                    })),
            );
        }
        let replies = self
            .page()
            .and_then(|page| page.conversation_view.replies.get(&thread.id));
        let mut comments: Vec<PullRequestComment> = thread.comments.clone();
        if !inline && let Some(replies) = replies {
            comments.extend(replies.comments.iter().cloned());
        }
        let shown = if inline { 1 } else { comments.len() };
        for comment in comments.into_iter().take(shown) {
            let head = self.comment_head(&comment, None, 16., cx);
            let body = self.body(&comment, inline, cx);
            card = card.child(
                v_flex()
                    .px_3()
                    .py_1()
                    .gap_1()
                    .child(head)
                    .children(body)
                    .children(self.reactions(&comment, cx)),
            );
        }
        if inline {
            return card.into_any_element();
        }
        let loading = replies.is_some_and(|replies| replies.loading);
        let after = replies.map_or(thread.replies_after.clone(), |replies| {
            replies.after.clone()
        });
        let read = thread.comments.len() as u64
            + replies.map_or(0, |replies| replies.comments.len() as u64);
        if after.is_some() {
            let remaining = thread.total_comments.saturating_sub(read);
            let thread = thread.clone();
            card = card.child(
                div().px_3().child(
                    Button::new(SharedString::from(format!("pr-thread-more-{}", thread.id)))
                        .ghost()
                        .xsmall()
                        .disabled(loading)
                        .label(if loading {
                            crate::tr!("pull_requests.conversation.loading")
                        } else if remaining == 1 {
                            crate::tr!("pull_requests.conversation.more_replies_one")
                        } else if remaining > 1 {
                            crate::tr!(
                                "pull_requests.conversation.more_replies",
                                count = remaining.to_string()
                            )
                        } else {
                            crate::tr!("pull_requests.conversation.more_replies_unknown")
                        })
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.read_replies(&thread, cx)),
                        ),
                ),
            );
        } else if read < thread.total_comments {
            card = card.child(
                div()
                    .px_3()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(crate::tr!("pull_requests.conversation.replies_capped")),
            );
        }
        card.into_any_element()
    }

    fn render_item(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let items = self.items(cx);
        let Some(item) = items.get(index).cloned() else {
            return div().into_any_element();
        };
        let Some(conversation) = self.page().and_then(|page| page.conversation.data.clone()) else {
            return div().into_any_element();
        };
        let row = match item {
            Item::Notice => {
                let url = self.url(cx);
                h_flex()
                    .my_2()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(crate::diff::list::render_notice(
                        crate::tr!("pull_requests.conversation.capped_unknown").into_owned(),
                        cx,
                    )))
                    .children(url.map(|url| {
                        Button::new("pr-capped-open")
                            .ghost()
                            .xsmall()
                            .label(crate::tr!("pull_requests.open_on_github"))
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    }))
                    .into_any_element()
            }
            Item::Description => self.comment_card(&conversation.description, true, cx),
            Item::Comment(index) => match conversation.comments.get(index) {
                Some(comment) => self.comment_card(comment, false, cx),
                None => div().into_any_element(),
            },
            Item::Thread(index) => match conversation.threads.get(index) {
                Some(thread) => div()
                    .py_1()
                    .child(self.thread_card(thread, false, cx))
                    .into_any_element(),
                None => div().into_any_element(),
            },
            Item::Event(state, at) => {
                let ago = ago_rfc3339(&at).unwrap_or_default();
                let (icon, label) = match state {
                    PullRequestState::Merged => (
                        IconName::GitMerge,
                        crate::tr!("pull_requests.conversation.merged", ago = ago),
                    ),
                    _ => (
                        IconName::GitPullRequestClosed,
                        crate::tr!("pull_requests.conversation.closed", ago = ago),
                    ),
                };
                h_flex()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(icon).size(px(14.)))
                    .child(label)
                    .into_any_element()
            }
        };
        let compact = self.compact(cx);
        div()
            .px(px(if compact {
                material::COMPACT_PAGE_INSET
            } else {
                12.
            }))
            .child(row)
            .into_any_element()
    }

    pub(super) fn render_conversation(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(page) = self.page() else {
            return div().into_any_element();
        };
        let Some(conversation) = page.conversation.data.as_ref() else {
            return match &page.conversation.error {
                Some(error) => {
                    let error = error.clone();
                    self.failure(
                        &error,
                        crate::tr!("pull_requests.conversation.load_failed").into_owned(),
                        cx,
                    )
                }
                None => material::loading_skeleton(cx),
            };
        };
        let empty = visible_body(&conversation.description.body).is_none()
            && conversation.comments.is_empty()
            && conversation.threads.is_empty();
        if empty {
            return material::empty_state(
                Icon::new(IconName::MessageSquare),
                crate::tr!("pull_requests.conversation.empty_title").into_owned(),
                crate::tr!("pull_requests.conversation.empty_desc").into_owned(),
                cx,
            )
            .into_any_element();
        }
        let count = self.items(cx).len();
        let Some(page) = self.page() else {
            return div().into_any_element();
        };
        let state = page.conversation_view.list.clone();
        if state.item_count() != count {
            state.reset(count);
        }
        let view = cx.entity();
        div()
            .id("pr-conversation")
            .flex_1()
            .min_h_0()
            .size_full()
            .child(
                list(state, move |index, _, cx| {
                    view.update(cx, |view, cx| view.render_item(index, cx))
                })
                .size_full(),
            )
            .into_any_element()
    }
}

/// What stands in for host media until it can be drawn.
fn media_stand_in(state: MediaState, url: &str, title: &str, cx: &App) -> Option<AnyElement> {
    let muted = cx.theme().muted_foreground;
    let open = |label: &'static str, url: String| {
        Button::new(SharedString::from(format!("pr-media-open-{url}")))
            .ghost()
            .xsmall()
            .icon(IconName::ExternalLink)
            .label(crate::tr!(label))
            .on_click(move |_, _, cx| cx.open_url(&url))
    };
    let unavailable = |text: String, cx: &App| {
        h_flex()
            .w_full()
            .h(px(72.))
            .px_3()
            .gap_2()
            .items_center()
            .rounded(material::radius_card(cx))
            .bg(cx.theme().muted)
            .text_size(px(12.))
            .text_color(muted)
            .child(Icon::new(IconName::ImageOff).size(px(16.)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(if title.trim().is_empty() {
                        text
                    } else {
                        format!("{text} · {title}")
                    }),
            )
            .child(open("pull_requests.open_on_github", url.to_owned()))
            .into_any_element()
    };
    match state {
        MediaState::Ready => None,
        MediaState::Loading => Some(
            div()
                .w_full()
                .h(px(180.))
                .max_h(px(240.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(material::radius_card(cx))
                .bg(cx.theme().muted)
                .child(Icon::new(IconName::Image).size(px(20.)).text_color(muted))
                .into_any_element(),
        ),
        MediaState::Unavailable => Some(unavailable(
            crate::tr!("pull_requests.media.unavailable").into_owned(),
            cx,
        )),
        MediaState::TooLarge => Some(unavailable(
            crate::tr!(
                "pull_requests.media.too_large",
                size =
                    crate::thread_export::format_size(tcode_protocol::MAX_PULL_REQUEST_MEDIA_BYTES)
            )
            .into_owned(),
            cx,
        )),
        MediaState::External(mime) => {
            let audio = mime.starts_with("audio/");
            let name = if title.trim().is_empty() {
                url.rsplit('/')
                    .next()
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        crate::tr!(if audio {
                            "pull_requests.media.audio"
                        } else {
                            "pull_requests.media.video"
                        })
                        .into_owned()
                    })
            } else {
                title.to_owned()
            };
            Some(
                h_flex()
                    .w_full()
                    .h(px(56.))
                    .px_3()
                    .gap_2()
                    .items_center()
                    .rounded(material::radius_card(cx))
                    .bg(cx.theme().secondary)
                    .child(
                        Icon::new(if audio {
                            IconName::Music
                        } else {
                            IconName::Film
                        })
                        .size(px(18.))
                        .text_color(muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .child(name),
                    )
                    .child(
                        Button::new(SharedString::from(format!("pr-media-external-{url}")))
                            .outline()
                            .xsmall()
                            .icon(IconName::ExternalLink)
                            .label(crate::tr!("pull_requests.media.open_in_browser"))
                            .on_click({
                                let url = url.to_owned();
                                move |_, _, cx| cx.open_url(&url)
                            }),
                    )
                    .into_any_element(),
            )
        }
    }
}
