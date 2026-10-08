//! The Files view: a pull request's changed files drawn by the diff list, from the host's
//! files read and never from the local checkout.

use std::ops::Range;
use std::rc::Rc;

use agent::FileChangeKind;
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, HighlightStyle, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;
use tcode_core::session::ReviewSide;
use tcode_protocol::{
    PullRequestFile, PullRequestFileText, PullRequestPatch, PullRequestRead,
    PullRequestReadResponse, PullRequestReviewThread, PullRequestViewedState,
};

use super::detail::{PullRequestView, TextState, reason};
use crate::{
    diff::{
        list::{
            DiffList, DiffListHost, DiffSelectionMenu, FileLayout, FileMenu, GapState, kind_rail,
            render_list, render_notice,
        },
        model::{
            DiffColors, ExpandDir, FileDiffInput, RenderedFile, build_file, reconstruct_from_text,
        },
    },
    icon::{Icon, IconName},
    material,
    overlay::{Notification, OverlayExt as _},
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    widgets::{
        Popover,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        input::{Input, InputState},
        menu::{CopyText, DropdownMenu as _, OpenUrl},
        tooltip::Tooltip,
    },
};

/// A Files view toggle; the phone's overflow menu addresses them by action.
#[derive(Action, Clone, Copy, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_pull_requests, no_json)]
enum FilesOption {
    Split,
    Wrap,
    Whitespace,
    Invisibles,
    CollapseAll,
    ExpandAll,
}

/// How a file's viewed mark reads to the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Viewed {
    Marked,
    Unmarked,
    Changed,
    Unknown,
}

/// A column of files beside the diff once it has this much room.
const FILE_COLUMN_MIN_WIDTH: f32 = 720.;

impl PullRequestView {
    fn head(&self) -> Option<String> {
        self.page()
            .and_then(|page| page.files.data.as_ref())
            .map(|files| files.head.clone())
    }

    /// The file's text at the revision Files is read for, on GitHub.
    pub(super) fn file_url(&self, path: &str, cx: &App) -> Option<String> {
        let url = self.url(cx)?;
        let head = self.head()?;
        let repository = url.rsplit_once("/pull/")?.0.to_owned();
        Some(format!("{repository}/blob/{head}/{path}"))
    }

    fn viewed_state(&self, path: &str) -> Viewed {
        let Some(page) = self.page() else {
            return Viewed::Unknown;
        };
        if let Some(mark) = page.viewed_marks.get(path) {
            return if *mark {
                Viewed::Marked
            } else {
                Viewed::Unmarked
            };
        }
        let Some(viewed) = page.viewed.data.as_ref() else {
            return Viewed::Unknown;
        };
        match viewed.files.iter().find(|(file, _)| file == path) {
            Some((_, PullRequestViewedState::Viewed)) => Viewed::Marked,
            Some((_, PullRequestViewedState::Dismissed)) => Viewed::Changed,
            Some((_, PullRequestViewedState::Unviewed)) => Viewed::Unmarked,
            None if viewed.complete => Viewed::Unmarked,
            None => Viewed::Unknown,
        }
    }

    fn collapsed(&self, path: &str) -> bool {
        self.page()
            .and_then(|page| page.files_view.collapsed.get(path).copied())
            .unwrap_or_else(|| self.viewed_state(path) == Viewed::Marked)
    }

    fn layouts(&self) -> Vec<FileLayout> {
        self.files_of()
            .unwrap_or_default()
            .iter()
            .map(|file| {
                if !matches!(file.patch, PullRequestPatch::Hunks(_)) {
                    FileLayout::Placeholder
                } else if self.collapsed(&file.path) {
                    FileLayout::Header
                } else {
                    FileLayout::Rows
                }
            })
            .collect()
    }

    /// Renders the files the host answered with, once per head, page and option change.
    fn ensure_list(&mut self, cx: &mut Context<Self>) {
        let dark = cx.theme().mode.is_dark();
        let (ignore_ws, show_invisibles) = (self.ignore_ws, self.show_invisibles);
        let layouts = self.layouts();
        let Some(page) = self.page_mut() else { return };
        let Some(files) = page.files.data.as_ref() else {
            return;
        };
        let key = (
            files.head.clone(),
            files.files.len(),
            dark,
            ignore_ws,
            show_invisibles,
        );
        if page.files_view.rendered.as_ref() == Some(&key) {
            if let Some(list) = page.files_view.list.as_mut() {
                list.set_layouts(layouts);
            }
            return;
        }
        if page.files_view.rendering {
            return;
        }
        // A new page only appends: the files already drawn keep their rows and scroll.
        let kept = page
            .files_view
            .rendered
            .as_ref()
            .filter(|rendered| {
                (&rendered.0, rendered.2, rendered.3, rendered.4) == (&key.0, key.2, key.3, key.4)
            })
            .map_or(0, |rendered| rendered.1);
        let pending: Vec<_> = files.files[kept..]
            .iter()
            .map(|file| {
                let text = match page.files_view.texts.get(&file.path) {
                    Some(TextState::Loaded { old, new }) => Some((old.clone(), new.clone())),
                    _ => None,
                };
                (file.clone(), text)
            })
            .collect();
        page.files_view.rendering = true;
        let theme = cx.theme().highlight_theme.clone();
        let colors = DiffColors {
            added_word_bg: cx.theme().success.opacity(0.30),
            removed_word_bg: cx.theme().danger.opacity(0.28),
        };
        let whitespace = HighlightStyle {
            color: Some(cx.theme().muted_foreground),
            ..Default::default()
        };
        let current = self.current.clone();
        cx.spawn(async move |this, cx| {
            let rendered = cx
                .background_executor()
                .spawn(async move {
                    pending
                        .iter()
                        .map(|(file, text)| {
                            render(
                                file,
                                text.as_ref(),
                                ignore_ws,
                                show_invisibles,
                                &theme,
                                &colors,
                                &whitespace,
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let layouts = this.layouts();
                let Some(page) = current.and_then(|current| this.pages.get_mut(&current)) else {
                    return;
                };
                page.files_view.rendering = false;
                if page.files.data.as_ref().map(|files| files.head.as_str()) != Some(key.0.as_str())
                {
                    return;
                }
                match page.files_view.list.as_mut().filter(|_| kept > 0) {
                    Some(list) => {
                        list.files.extend(rendered);
                        list.set_layouts(layouts);
                        list.relayout();
                    }
                    None => page.files_view.list = Some(DiffList::new(rendered, layouts)),
                }
                page.files_view.rendered = Some(key);
                cx.notify();
            });
        })
        .detach();
    }

    fn file_index(&self, path: &str) -> Option<usize> {
        self.files_of()?.iter().position(|file| file.path == path)
    }

    /// Reads the file's text at the head, and reconstructs its base side from the patch,
    /// which is the side the hunks were cut against; the base revision's own text is read
    /// only when that fails.
    fn read_text(&mut self, file: usize, cx: &mut Context<Self>) {
        let Some(head) = self.head() else { return };
        let Some(entry) = self.files_of().and_then(|files| files.get(file)).cloned() else {
            return;
        };
        let base = self
            .page()
            .and_then(|page| page.files.data.as_ref())
            .map(|files| files.base.clone())
            .unwrap_or_default();
        let Some(page) = self.page_mut() else { return };
        page.files_view
            .texts
            .insert(entry.path.clone(), TextState::Loading);
        let path = entry.path.clone();
        let patch = match &entry.patch {
            PullRequestPatch::Hunks(hunks) => hunks.clone(),
            _ => String::new(),
        };
        let deleted = entry.kind == FileChangeKind::Delete;
        let (revision, side_path) = if deleted {
            (
                base.clone(),
                entry.previous_path.clone().unwrap_or_else(|| path.clone()),
            )
        } else {
            (head.clone(), path.clone())
        };
        let read = PullRequestRead::FileText {
            revision: revision.clone(),
            path: side_path,
        };
        let created = entry.kind == FileChangeKind::Create;
        self.read(read, cx, move |page, result| {
            let state = match result {
                Ok((PullRequestReadResponse::FileText(PullRequestFileText::Text(text)), _)) => {
                    if deleted {
                        TextState::Loaded {
                            old: text,
                            new: String::new(),
                        }
                    } else if created {
                        TextState::Loaded {
                            old: String::new(),
                            new: text,
                        }
                    } else {
                        match reconstruct_from_text(text, &patch) {
                            Some((old, new)) => TextState::Loaded { old, new },
                            None => TextState::Unreadable,
                        }
                    }
                }
                Ok((PullRequestReadResponse::FileText(PullRequestFileText::Missing), _)) => {
                    TextState::Missing
                }
                Ok((PullRequestReadResponse::FileText(PullRequestFileText::Oversized), _)) => {
                    TextState::Oversized
                }
                Ok(_) => TextState::Unreadable,
                Err(_) => {
                    page.files_view.texts.remove(&path);
                    page.files_view.pending_expand = None;
                    return;
                }
            };
            page.files_view.texts.insert(path, state);
        });
        let _ = revision;
    }

    /// Redraws a file once its text arrived, then applies the expansion that asked for it.
    fn apply_text(&mut self, cx: &mut Context<Self>) {
        let dark_theme = cx.theme().highlight_theme.clone();
        let colors = DiffColors {
            added_word_bg: cx.theme().success.opacity(0.30),
            removed_word_bg: cx.theme().danger.opacity(0.28),
        };
        let whitespace = HighlightStyle {
            color: Some(cx.theme().muted_foreground),
            ..Default::default()
        };
        let (ignore_ws, show_invisibles) = (self.ignore_ws, self.show_invisibles);
        let Some(page) = self.page_mut() else { return };
        let Some((path, start, direction)) = page.files_view.pending_expand.clone() else {
            return;
        };
        let Some(TextState::Loaded { old, new }) = page.files_view.texts.get(&path) else {
            if !matches!(page.files_view.texts.get(&path), Some(TextState::Loading)) {
                page.files_view.pending_expand = None;
            }
            return;
        };
        let Some(files) = page.files.data.as_ref() else {
            return;
        };
        let Some(index) = files.files.iter().position(|file| file.path == path) else {
            return;
        };
        let rendered = render(
            &files.files[index],
            Some(&(old.clone(), new.clone())),
            ignore_ws,
            show_invisibles,
            &dark_theme,
            &colors,
            &whitespace,
        );
        page.files_view.pending_expand = None;
        let Some(list) = page.files_view.list.as_mut() else {
            return;
        };
        if let Some(slot) = list.files.get_mut(index) {
            *slot = rendered;
        }
        let gap = list.files[index]
            .collapsed
            .iter()
            .find(|gap| gap.contains(&start) || gap.start == start)
            .cloned();
        match gap {
            Some(gap) => list.expand(index, gap, direction),
            None => list.relayout(),
        }
        cx.notify();
    }

    fn mark_viewed(
        &mut self,
        path: String,
        viewed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(page) = self.page_mut() else { return };
        page.viewed_marks.insert(path.clone(), viewed);
        page.viewed_batch.insert(path.clone(), viewed);
        // Marking collapses a file and unmarking opens it; the reader's own choice yields.
        page.files_view.collapsed.remove(&path);
        if page.viewed_flush.is_none() {
            page.viewed_flush = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(400))
                    .await;
                let _ = this.update_in(cx, |this, window, cx| this.flush_viewed(window, cx));
            }));
        }
        cx.notify();
    }

    /// Sends the marks pressed within one beat, one command per direction.
    fn flush_viewed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session, key)) = self.current.clone() else {
            return;
        };
        let Some(page) = self.page_mut() else { return };
        page.viewed_flush = None;
        let batch = std::mem::take(&mut page.viewed_batch);
        for viewed in [true, false] {
            let paths: Vec<_> = batch
                .iter()
                .filter(|(_, mark)| **mark == viewed)
                .map(|(path, _)| path.clone())
                .collect();
            if paths.is_empty() {
                continue;
            }
            let task = self.store.update(cx, |store, cx| {
                store.command(
                    tcode_protocol::Command::SetPullRequestFilesViewed {
                        session_id: session.clone(),
                        key: key.clone(),
                        paths: paths.clone(),
                        viewed,
                    },
                    cx,
                )
            });
            let current = (session.clone(), key.clone());
            cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    let Some(page) = this.pages.get_mut(&current) else {
                        return;
                    };
                    if let Err(error) = result {
                        for path in &paths {
                            page.viewed_marks.remove(path);
                        }
                        window.push_notification(
                            Notification::error(
                                crate::tr!(
                                    "pull_requests.files.viewed_failed",
                                    reason = reason(&error)
                                )
                                .into_owned(),
                            ),
                            cx,
                        );
                    }
                    // The host dropped its viewed read; read the marks it now holds.
                    page.viewed.expires_at = 0;
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn set_all_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        let paths: Vec<_> = self
            .files_of()
            .unwrap_or_default()
            .iter()
            .map(|file| file.path.clone())
            .collect();
        if let Some(page) = self.page_mut() {
            for path in paths {
                page.files_view.collapsed.insert(path, collapsed);
            }
        }
        cx.notify();
    }

    fn apply_option(&mut self, option: FilesOption, cx: &mut Context<Self>) {
        match option {
            FilesOption::Split => {
                let split = self.store.read(cx).diff_split();
                self.store
                    .update(cx, |store, cx| store.set_diff_split(!split, cx));
            }
            FilesOption::Wrap => self
                .store
                .update(cx, |store, cx| store.toggle_diff_wrap(cx)),
            FilesOption::Whitespace => self.ignore_ws = !self.ignore_ws,
            FilesOption::Invisibles => self.show_invisibles = !self.show_invisibles,
            FilesOption::CollapseAll => self.set_all_collapsed(true, cx),
            FilesOption::ExpandAll => self.set_all_collapsed(false, cx),
        }
        if let Some(list) = self.page().and_then(|page| page.files_view.list.as_ref()) {
            list.remeasure();
        }
        cx.notify();
    }

    fn on_option(&mut self, option: &FilesOption, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_option(*option, cx);
    }

    pub(super) fn copy_selected_lines(
        &mut self,
        action: &DiffSelectionMenu,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *action != DiffSelectionMenu::CopyLines {
            return;
        }
        let text = self
            .page()
            .and_then(|page| page.files_view.list.as_ref())
            .map(DiffList::selected_lines)
            .unwrap_or_default();
        if !text.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    fn scroll_to(&mut self, file: usize, cx: &mut Context<Self>) {
        if let Some(list) = self.page().and_then(|page| page.files_view.list.as_ref()) {
            list.scroll_to_file(file);
        }
        cx.notify();
    }

    /// Threads drawn on the diff: current anchors on a file whose lines are drawn.
    fn inline_threads(&self) -> Vec<&PullRequestReviewThread> {
        let (Some(page), Some(head)) = (self.page(), self.head()) else {
            return Vec::new();
        };
        let Some(conversation) = page.conversation.data.as_ref() else {
            return Vec::new();
        };
        let list = page.files_view.list.as_ref();
        conversation
            .threads
            .iter()
            .filter(|thread| {
                let Some(anchor) = thread.anchor.as_ref().filter(|_| !thread.outdated) else {
                    return false;
                };
                if anchor.revision != head {
                    return false;
                }
                let Some(index) = self.file_index(&anchor.path) else {
                    return false;
                };
                list.and_then(|list| list.files.get(index))
                    .filter(|_| list.is_some_and(|list| list.layout(index) == FileLayout::Rows))
                    .is_some_and(|file| {
                        file.all_rows.iter().any(|row| match anchor.side {
                            ReviewSide::Old => row.old == Some(anchor.end_line),
                            ReviewSide::New => row.new == Some(anchor.end_line),
                        })
                    })
            })
            .collect()
    }

    fn off_diff(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let page = self.page()?;
        let conversation = page.conversation.data.as_ref()?;
        let inline: Vec<_> = self
            .inline_threads()
            .into_iter()
            .map(|thread| thread.id.clone())
            .collect();
        let threads: Vec<_> = conversation
            .threads
            .iter()
            .filter(|thread| !inline.contains(&thread.id))
            .cloned()
            .collect();
        if threads.is_empty() {
            return None;
        }
        let more = page
            .files
            .data
            .as_ref()
            .is_some_and(|files| files.next_page.is_some());
        let count = threads.len();
        let label = if more {
            crate::tr!(
                "pull_requests.files.off_diff_loaded",
                count = count.to_string()
            )
        } else if count == 1 {
            crate::tr!("pull_requests.files.off_diff_one")
        } else {
            crate::tr!("pull_requests.files.off_diff", count = count.to_string())
        }
        .into_owned();
        let open = page.files_view.off_diff_open;
        let muted = cx.theme().muted_foreground;
        Some(
            v_flex()
                .flex_none()
                .mx_2()
                .mb_1()
                .rounded(material::radius_card(cx))
                .bg(cx.theme().muted)
                .child(
                    h_flex()
                        .id("pr-off-diff")
                        .h(px(28.))
                        .px_3()
                        .gap_2()
                        .items_center()
                        .text_size(px(12.))
                        .cursor_pointer()
                        .child(
                            Icon::new(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(px(12.))
                            .text_color(muted),
                        )
                        .child(label)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(page) = this.page_mut() {
                                page.files_view.off_diff_open = !page.files_view.off_diff_open;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    gpui_base::Collapsible::new().open(open).content(
                        v_flex()
                            .id("pr-off-diff-rows")
                            .max_h(px(240.))
                            .overflow_y_scroll()
                            .pb_1()
                            .children(threads.into_iter().map(|thread| {
                                let line = thread
                                    .anchor
                                    .as_ref()
                                    .map(|anchor| format!("{}:{}", thread.path, anchor.end_line))
                                    .unwrap_or_else(|| thread.path.clone());
                                let excerpt = thread
                                    .comments
                                    .first()
                                    .map(|comment| {
                                        comment.body.lines().next().unwrap_or_default().to_owned()
                                    })
                                    .unwrap_or_default();
                                let id = thread.id.clone();
                                h_flex()
                                    .id(SharedString::from(format!("pr-off-diff-{}", thread.id)))
                                    .h(px(28.))
                                    .px_3()
                                    .gap_2()
                                    .items_center()
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .hover(|row| row.bg(cx.theme().list_hover))
                                    .child(
                                        div()
                                            .flex_none()
                                            .max_w(px(220.))
                                            .truncate()
                                            .font_family(cx.theme().mono_font_family.clone())
                                            .child(line),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(muted)
                                            .child(excerpt),
                                    )
                                    .when(thread.outdated, |row| {
                                        row.child(material::semantic_chip(
                                            crate::tr!("pull_requests.conversation.outdated")
                                                .into_owned(),
                                            cx.theme().secondary,
                                            muted,
                                            cx,
                                        ))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.show_thread(&id, cx);
                                    }))
                            })),
                    ),
                )
                .into_any_element(),
        )
    }

    fn viewed_box(
        &self,
        file: usize,
        labelled: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let entry = self.files_of()?.get(file)?.clone();
        let compact = self.compact(cx);
        let state = self.viewed_state(&entry.path);
        let read_only = self
            .store
            .read(cx)
            .session_status()
            .is_none_or(|status| status.conversation_read_only);
        let path = entry.path.clone();
        if state == Viewed::Unknown {
            let unreadable = self.page().is_some_and(|page| page.viewed.error.is_some());
            return Some(
                div()
                    .id(SharedString::from(format!("pr-viewed-unknown-{file}")))
                    .flex_none()
                    .child(
                        Icon::new(IconName::CircleQuestionMark)
                            .size(px(14.))
                            .text_color(cx.theme().muted_foreground),
                    )
                    .tooltip(move |window, cx| {
                        Tooltip::new(
                            crate::tr!(if unreadable {
                                "pull_requests.files.viewed_unreadable"
                            } else {
                                "pull_requests.files.viewed_unknown"
                            })
                            .into_owned(),
                        )
                        .build(window, cx)
                    })
                    .into_any_element(),
            );
        }
        let checked = state == Viewed::Marked;
        let tooltip = if read_only {
            crate::tr!("pull_requests.files.viewed_read_only")
        } else if state == Viewed::Changed {
            crate::tr!("pull_requests.files.changed_since_viewed_tooltip")
        } else if checked {
            crate::tr!("pull_requests.files.mark_unviewed")
        } else {
            crate::tr!("pull_requests.files.mark_viewed")
        }
        .into_owned();
        let view = cx.entity();
        let mut checkbox = Checkbox::new(SharedString::from(format!("pr-viewed-{file}")))
            .checked(checked)
            .disabled(read_only)
            .on_click(move |checked, window, cx| {
                let path = path.clone();
                let checked = *checked;
                view.update(cx, |view, cx| view.mark_viewed(path, checked, window, cx));
            });
        if labelled && !compact {
            checkbox = checkbox.label(crate::tr!("pull_requests.files.viewed"));
        }
        Some(
            div()
                .id(SharedString::from(format!("pr-viewed-box-{file}")))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.))
                .when(compact, |cell| cell.size(px(material::TOUCH_TARGET)))
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(checkbox)
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .into_any_element(),
        )
    }

    fn toolbar(&self, wide_column: bool, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let page = self.page();
        let files = page.and_then(|page| page.files.data.as_ref());
        let count = files.map_or(0, |files| files.files.len());
        let more = files.is_some_and(|files| files.next_page.is_some());
        let count_label = if more {
            crate::tr!("pull_requests.files.count_more", count = count.to_string())
        } else if count == 1 {
            crate::tr!("pull_requests.files.count_one")
        } else {
            crate::tr!("pull_requests.files.count", count = count.to_string())
        }
        .into_owned();
        let viewed_counter = files.map(|files| {
            let viewed = files
                .files
                .iter()
                .filter(|file| self.viewed_state(&file.path) == Viewed::Marked)
                .count();
            let unknown = files
                .files
                .iter()
                .any(|file| self.viewed_state(&file.path) == Viewed::Unknown);
            let tooltip = if unknown {
                crate::tr!("pull_requests.files.viewed_unknown")
            } else {
                crate::tr!("pull_requests.files.viewed_owner_anonymous")
            }
            .into_owned();
            div()
                .id("pr-viewed-counter")
                .flex_none()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(
                    crate::tr!(
                        "pull_requests.files.viewed_count",
                        viewed = viewed.to_string(),
                        total = files.files.len().to_string()
                    )
                    .into_owned(),
                )
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        });
        let split = self.store.read(cx).diff_split();
        let wrap = self.store.read(cx).diff_word_wrap();
        let all_collapsed = files.is_some_and(|files| {
            !files.files.is_empty() && files.files.iter().all(|file| self.collapsed(&file.path))
        });
        let options = [
            (
                FilesOption::Split,
                "pr-files-split",
                IconName::PanelLeft,
                split,
                if split {
                    crate::tr!("diff.unified_view")
                } else {
                    crate::tr!("diff.split_view")
                }
                .into_owned(),
            ),
            (
                FilesOption::Wrap,
                "pr-files-wrap",
                IconName::Menu,
                wrap,
                crate::tr!("diff.toggle_wrap").into_owned(),
            ),
            (
                FilesOption::Whitespace,
                "pr-files-whitespace",
                IconName::Eye,
                self.ignore_ws,
                crate::tr!("diff.toggle_whitespace").into_owned(),
            ),
            (
                FilesOption::Invisibles,
                "pr-files-invisibles",
                IconName::CaseSensitive,
                self.show_invisibles,
                crate::tr!("diff.toggle_invisibles").into_owned(),
            ),
        ];
        let mut toolbar = h_flex()
            .flex_none()
            .h(px(if compact { material::TOUCH_TARGET } else { 40. }))
            .w_full()
            .px(px(if compact {
                material::COMPACT_PAGE_INSET
            } else {
                8.
            }))
            .gap_1()
            .items_center()
            .when(!wide_column, |toolbar| {
                toolbar.child(self.file_jump(count_label, cx))
            })
            .children(viewed_counter)
            .child(div().flex_1());
        if compact {
            toolbar = toolbar.child(
                material::toolbar_icon_button(
                    "pr-files-options",
                    IconName::Ellipsis,
                    crate::tr!("mobile.more_actions"),
                    true,
                )
                .dropdown_menu(move |mut menu, _, _| {
                    for (option, _, _, on, label) in &options {
                        menu = menu.menu_with_check(label.clone(), *on, Box::new(*option));
                    }
                    menu.separator().menu(
                        crate::tr!(if all_collapsed {
                            "pull_requests.files.expand_all"
                        } else {
                            "pull_requests.files.collapse_all"
                        })
                        .into_owned(),
                        Box::new(if all_collapsed {
                            FilesOption::ExpandAll
                        } else {
                            FilesOption::CollapseAll
                        }),
                    )
                }),
            );
        } else {
            for (option, id, icon, on, label) in options {
                toolbar = toolbar.child(
                    Button::new(id)
                        .ghost()
                        .small()
                        .compact()
                        .icon(icon)
                        .selected(on)
                        .tooltip(label)
                        .on_click(cx.listener(move |this, _, _, cx| this.apply_option(option, cx))),
                );
            }
            toolbar = toolbar
                .child(
                    Button::new("pr-files-collapse")
                        .ghost()
                        .small()
                        .compact()
                        .icon(if all_collapsed {
                            IconName::ChevronsUpDown
                        } else {
                            IconName::ChevronsDownUp
                        })
                        .tooltip(if all_collapsed {
                            crate::tr!("pull_requests.files.expand_all")
                        } else {
                            crate::tr!("pull_requests.files.collapse_all")
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_all_collapsed(!all_collapsed, cx)
                        })),
                )
                .child(
                    Button::new("pr-files-column")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::PanelLeft)
                        .selected(self.file_column)
                        .tooltip(if self.file_column {
                            crate::tr!("pull_requests.files.hide_list")
                        } else {
                            crate::tr!("pull_requests.files.show_list")
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.file_column = !this.file_column;
                            cx.notify();
                        })),
                );
        }
        toolbar.into_any_element()
    }

    /// The files matching the filter, in the host's order.
    fn filtered(&self, filter: &str) -> Vec<usize> {
        let filter = filter.to_lowercase();
        self.files_of()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .filter(|(_, file)| filter.is_empty() || file.path.to_lowercase().contains(&filter))
            .map(|(index, _)| index)
            .collect()
    }

    /// The files as a virtual list, so a three-thousand-file pull request lays out one screen.
    fn file_list(
        &self,
        id: &'static str,
        filter: &str,
        in_popover: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let indexes = self.filtered(filter);
        let view = cx.entity();
        crate::scroll::VirtualList::uniform(id, indexes.len(), move |range, _, cx| {
            view.update(cx, |view, cx| {
                range
                    .map(|position| view.file_row(indexes[position], in_popover, cx))
                    .collect()
            })
        })
        .w_full()
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn file_row(&self, index: usize, in_popover: bool, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let Some(file) = self.files_of().and_then(|files| files.get(index)).cloned() else {
            return div().into_any_element();
        };
        let top = self
            .page()
            .and_then(|page| page.files_view.list.as_ref())
            .and_then(|list| list.top_file(self.store.read(cx).diff_split()));
        let muted = cx.theme().muted_foreground;
        let (directory, name) = match file.path.rsplit_once('/') {
            Some((directory, name)) => (format!("{directory}/"), name.to_owned()),
            None => (String::new(), file.path.clone()),
        };
        let availability = match file.patch {
            PullRequestPatch::Binary => Some((IconName::Binary, muted)),
            PullRequestPatch::Oversized => Some((IconName::FileX, muted)),
            PullRequestPatch::Hunks(_) => None,
        };
        let tooltip = file
            .previous_path
            .as_ref()
            .map(|previous| format!("{previous} → {}", file.path))
            .unwrap_or_else(|| file.path.clone());
        h_flex()
            .id(SharedString::from(format!(
                "pr-file-row-{in_popover}-{index}"
            )))
            .w_full()
            .h(px(if compact { 44. } else { 28. }))
            .px_2()
            .gap_2()
            .items_center()
            .rounded(cx.theme().tokens.radius.sm)
            .cursor_pointer()
            .hover(|row| row.bg(cx.theme().list_hover))
            .when(top == Some(index) && !in_popover, |row| {
                row.bg(cx.theme().list_active)
            })
            .child(
                Icon::new(IconName::File)
                    .size(px(12.))
                    .text_color(kind_rail(file.kind, cx).unwrap_or(muted)),
            )
            // gpui-base has no start truncation: the directory gives way first, then the name.
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex_shrink(8.)
                            .min_w_0()
                            .truncate()
                            .text_color(muted)
                            .child(directory),
                    )
                    .child(div().flex_shrink(1.).min_w_0().truncate().child(name)),
            )
            .children(
                availability.map(|(icon, color)| Icon::new(icon).size(px(12.)).text_color(color)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .text_size(px(11.))
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(
                        div()
                            .text_color(cx.theme().success)
                            .child(format!("+{}", file.additions)),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().danger)
                            .child(format!("−{}", file.deletions)),
                    ),
            )
            .children(
                self.viewed_box(index, false, cx)
                    .filter(|_| !in_popover || compact),
            )
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.scroll_to(index, cx);
                if in_popover {
                    window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
                }
            }))
            .into_any_element()
    }

    fn filter_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        if let Some((input, _)) = &self.file_filter {
            return input.clone();
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!("pull_requests.files.filter"))
        });
        let subscription = cx.observe(&input, |_, _, cx| cx.notify());
        self.file_filter = Some((input.clone(), subscription));
        input
    }

    fn file_jump(&self, label: String, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact(cx);
        let muted = cx.theme().muted_foreground;
        let view = cx.entity();
        let trigger = Button::new("pr-file-jump")
            .ghost()
            .outline()
            .compact()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .child(Icon::new(IconName::Files).size(px(14.)))
                    .child(label)
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
            );
        let popover = Popover::new("pr-file-jump-popover").trigger(trigger);
        let popover = if compact {
            popover.bottom_sheet(crate::tr!("pull_requests.detail.files").into_owned())
        } else {
            popover
        };
        popover
            .content(move |_, window, cx| {
                view.update(cx, |view, cx| {
                    let input = view.filter_input(window, cx);
                    let filter = input.read(cx).value().to_string();
                    v_flex()
                        .w(px(360.))
                        .max_h(px(420.))
                        .p_1()
                        .gap_1()
                        .child(Input::new(&input).small())
                        .child(view.file_list("pr-file-jump-rows", &filter, true, cx))
                })
            })
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_xl()
            .rounded(material::radius_overlay(cx))
            .into_any_element()
    }

    fn file_column(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let input = self.filter_input(window, cx);
        let filter = input.read(cx).value().to_string();
        let count = self.files_of().map_or(0, <[PullRequestFile]>::len);
        v_flex()
            .flex_none()
            .w(px(280.))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .p_1()
            .gap_1()
            .child(
                div()
                    .px_2()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        crate::tr!("pull_requests.files.count", count = count.to_string())
                            .into_owned(),
                    ),
            )
            .child(Input::new(&input).small())
            .child(self.file_list("pr-file-column-rows", &filter, false, cx))
            .into_any_element()
    }

    pub(super) fn render_files(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_list(cx);
        self.apply_text(cx);
        let Some(page) = self.page() else {
            return div().into_any_element();
        };
        let files = page.files.data.as_ref();
        if files.is_none() {
            return match &page.files.error {
                Some(error) => {
                    let error = error.clone();
                    self.failure(
                        &error,
                        crate::tr!("pull_requests.files.load_failed").into_owned(),
                        cx,
                    )
                }
                None => material::loading_skeleton(cx),
            };
        }
        let files = files.cloned().unwrap_or_else(|| unreachable!());
        if files.files.is_empty() && files.next_page.is_none() {
            return material::empty_state(
                Icon::new(IconName::File),
                crate::tr!("pull_requests.files.empty_title").into_owned(),
                crate::tr!("pull_requests.files.empty_desc").into_owned(),
                cx,
            )
            .into_any_element();
        }
        // The next page is read as the reader nears the end of what is drawn.
        let split = self.store.read(cx).diff_split();
        let wrap = self.store.read(cx).diff_word_wrap();
        let near_end = page
            .files_view
            .list
            .as_ref()
            .and_then(|list| list.top_file(split))
            .is_some_and(|top| top + 20 >= files.files.len());
        if let Some(next) = files.next_page
            && near_end
            && !page.files_view.page_loading
            && page.files_view.page_error.is_none()
        {
            self.read_next_page(next, cx);
        }
        let Some(page) = self.page() else {
            return div().into_any_element();
        };
        let compact = self.compact(cx);
        let wide = !compact
            && f32::from(window.viewport_size().width) >= FILE_COLUMN_MIN_WIDTH
            && self.file_column;
        let partial = (files.next_page.is_none() && !files.complete).then(|| {
            let listed = files.files.len();
            let text = if files.changed_files > listed as u64 {
                crate::tr!(
                    "pull_requests.files.partial",
                    shown = listed.to_string(),
                    total = files.changed_files.to_string()
                )
            } else {
                crate::tr!("pull_requests.files.partial_unknown")
            }
            .into_owned();
            div().mx_2().mb_1().child(render_notice(text, cx))
        });
        let footer = files
            .next_page
            .filter(|_| page.files_view.page_loading || page.files_view.page_error.is_some())
            .map(|_| {
                let failed = page.files_view.page_error.is_some();
                h_flex()
                    .flex_none()
                    .h(px(40.))
                    .px_3()
                    .gap_2()
                    .items_center()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .when(!failed, |row| {
                        row.child(crate::widgets::spinner::Spinner::new().small())
                            .child(crate::tr!("pull_requests.files.loading_more"))
                    })
                    .when(failed, |row| {
                        row.child(crate::tr!("pull_requests.files.rest_failed"))
                            .child(
                                Button::new("pr-files-rest-retry")
                                    .outline()
                                    .xsmall()
                                    .label(crate::tr!("pull_requests.detail.retry"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(page) = this.page_mut() {
                                            page.files_view.page_error = None;
                                        }
                                        cx.notify();
                                    })),
                            )
                    })
            });
        let body = if page.files_view.list.is_some() {
            render_list(self, "pr-files-body", split, wrap, cx)
        } else {
            material::loading_skeleton(cx)
        };
        let off_diff = self.off_diff(cx);
        let content = v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .children(partial)
            .children(off_diff)
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .when(compact, |body| body.px(px(material::COMPACT_PAGE_INSET)))
                    .child(body),
            )
            .children(footer);
        v_flex()
            .size_full()
            .min_h_0()
            .on_action(cx.listener(Self::on_option))
            .child(self.toolbar(wide, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .when(wide, |row| row.child(self.file_column(window, cx)))
                    .child(content),
            )
            .into_any_element()
    }

    fn placeholder_row(&self, file: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.files_of().and_then(|files| files.get(file)).cloned() else {
            return div().into_any_element();
        };
        let (icon, text) = match entry.patch {
            PullRequestPatch::Binary => (
                IconName::Binary,
                crate::tr!("pull_requests.files.binary").into_owned(),
            ),
            PullRequestPatch::Oversized => (
                IconName::FileX,
                crate::tr!(
                    "pull_requests.files.oversized",
                    size = crate::tr!(
                        "pull_requests.files.line_changes",
                        count = (entry.additions + entry.deletions).to_string()
                    )
                    .into_owned()
                )
                .into_owned(),
            ),
            PullRequestPatch::Hunks(_) => return div().into_any_element(),
        };
        let url = self.file_url(&entry.path, cx);
        h_flex()
            .min_w_full()
            .min_h(px(72.))
            .my_1()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .rounded(material::radius_card(cx))
            .bg(cx.theme().muted)
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .font_family(cx.theme().font_family.clone())
            .child(Icon::new(icon).size(px(14.)))
            .child(div().flex_1().min_w_0().child(text))
            .children(url.map(|url| {
                Button::new(SharedString::from(format!("pr-file-open-{file}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ExternalLink)
                    .label(crate::tr!("pull_requests.open_on_github"))
                    .on_click(move |_, _, cx| cx.open_url(&url))
            }))
            .into_any_element()
    }
}

impl DiffListHost for PullRequestView {
    fn diff_list(&self) -> Option<&DiffList> {
        self.page()?.files_view.list.as_ref()
    }

    fn diff_list_mut(&mut self) -> Option<&mut DiffList> {
        let current = self.current.clone()?;
        self.pages.get_mut(&current)?.files_view.list.as_mut()
    }

    fn expand_gap(
        &mut self,
        file: usize,
        lines: Range<u32>,
        direction: ExpandDir,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self
            .files_of()
            .and_then(|files| files.get(file))
            .map(|file| file.path.clone())
        else {
            return;
        };
        let loaded = matches!(
            self.page()
                .and_then(|page| page.files_view.texts.get(&path)),
            Some(TextState::Loaded { .. })
        );
        if loaded {
            if let Some(list) = self.diff_list_mut() {
                list.expand(file, lines, direction);
            }
        } else if let Some(page) = self.page_mut()
            && !page.files_view.texts.contains_key(&path)
        {
            page.files_view.pending_expand = Some((path, lines.start, direction));
            self.read_text(file, cx);
        }
        cx.notify();
    }

    fn gap_state(&self, file: usize, expandable: bool) -> GapState {
        let Some(entry) = self.files_of().and_then(|files| files.get(file)) else {
            return GapState::Fixed(None);
        };
        let head = self.head().unwrap_or_default();
        let short = head.get(..7).unwrap_or(&head).to_owned();
        match self
            .page()
            .and_then(|page| page.files_view.texts.get(&entry.path))
        {
            Some(TextState::Loading) => GapState::Loading,
            Some(TextState::Loaded { .. }) if expandable => GapState::Expandable,
            Some(TextState::Loaded { .. }) | Some(TextState::Unreadable) => GapState::Fixed(None),
            Some(TextState::Missing) => GapState::Fixed(Some(
                crate::tr!("pull_requests.files.side_missing", revision = short).into_owned(),
            )),
            Some(TextState::Oversized) => GapState::Fixed(Some(
                crate::tr!("pull_requests.files.too_large_to_expand").into_owned(),
            )),
            None => GapState::Expandable,
        }
    }

    fn header_title(&self, file: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.files_of().and_then(|files| files.get(file)) else {
            return div().into_any_element();
        };
        h_flex()
            .min_w_0()
            .gap_1()
            .text_size(px(13.))
            .line_height(px(18.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .children(entry.previous_path.as_ref().map(|previous| {
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{previous} →"))
            }))
            .child(
                div()
                    .text_color(cx.theme().foreground)
                    .child(entry.path.clone()),
            )
            .into_any_element()
    }

    fn header_leading(&self, file: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.files_of()?.get(file)?.path.clone();
        let collapsed = self.collapsed(&path);
        Some(
            div()
                .id(SharedString::from(format!("pr-file-chevron-{file}")))
                .flex_none()
                .child(
                    Icon::new(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .size(px(12.))
                    .text_color(cx.theme().muted_foreground),
                )
                .tooltip(move |window, cx| {
                    Tooltip::new(
                        crate::tr!(if collapsed {
                            "pull_requests.files.expand_file"
                        } else {
                            "pull_requests.files.collapse_file"
                        })
                        .into_owned(),
                    )
                    .build(window, cx)
                })
                .into_any_element(),
        )
    }

    fn header_note(&self, file: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = &self.files_of()?.get(file)?.path;
        (self.viewed_state(path) == Viewed::Changed).then(|| {
            div()
                .text_size(px(11.))
                .line_height(px(18.))
                .text_color(cx.theme().warning)
                .child(crate::tr!("pull_requests.files.changed_since_viewed"))
                .into_any_element()
        })
    }

    fn header_trailing(&self, file: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.viewed_box(file, true, cx)
    }

    fn header_clicked(&mut self, file: usize, cx: &mut Context<Self>) {
        let Some(path) = self
            .files_of()
            .and_then(|files| files.get(file))
            .map(|file| file.path.clone())
        else {
            return;
        };
        let collapsed = self.collapsed(&path);
        if let Some(page) = self.page_mut() {
            page.files_view.collapsed.insert(path, !collapsed);
        }
        cx.notify();
    }

    fn file_menu(&self, file: usize, cx: &App) -> FileMenu {
        let path = self
            .files_of()
            .and_then(|files| files.get(file))
            .map(|file| file.path.clone())
            .unwrap_or_default();
        let url = self.file_url(&path, cx);
        Rc::new(move |menu, _, _| {
            menu.menu(
                crate::tr!("pull_requests.files.copy_path").into_owned(),
                Box::new(CopyText(path.clone())),
            )
            .when_some(url.clone(), |menu, url| {
                menu.menu(
                    crate::tr!("pull_requests.open_on_github").into_owned(),
                    Box::new(OpenUrl(url)),
                )
            })
        })
    }

    fn row_extras(
        &self,
        file: usize,
        old: Option<u32>,
        new: Option<u32>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(path) = self
            .files_of()
            .and_then(|files| files.get(file))
            .map(|file| file.path.clone())
        else {
            return Vec::new();
        };
        let threads: Vec<_> = self
            .inline_threads()
            .into_iter()
            .filter(|thread| {
                thread.anchor.as_ref().is_some_and(|anchor| {
                    anchor.path == path
                        && match anchor.side {
                            ReviewSide::Old => old == Some(anchor.end_line),
                            ReviewSide::New => new == Some(anchor.end_line),
                        }
                })
            })
            .cloned()
            .collect();
        threads
            .iter()
            .map(|thread| self.thread_card(thread, true, cx))
            .collect()
    }

    fn placeholder(&self, file: usize, cx: &mut Context<Self>) -> AnyElement {
        self.placeholder_row(file, cx)
    }
}

/// One file of the pull request through the diff renderer: the hunks alone until its text at
/// the head has been read, then the whole file.
fn render(
    file: &PullRequestFile,
    text: Option<&(String, String)>,
    ignore_whitespace: bool,
    show_invisibles: bool,
    theme: &crate::highlight::HighlightTheme,
    colors: &DiffColors,
    whitespace: &HighlightStyle,
) -> RenderedFile {
    let patch = match &file.patch {
        PullRequestPatch::Hunks(hunks) => Some(hunks.as_str()),
        _ => None,
    };
    build_file(
        &FileDiffInput {
            path: &file.path,
            kind: file.kind,
            old_text: text.map(|(old, _)| old.as_str()),
            new_text: text.map(|(_, new)| new.as_str()),
            patch,
            ignore_whitespace,
            show_invisibles,
        },
        file.path.clone(),
        crate::highlight::language_name_for_path(&file.path),
        theme,
        colors,
        whitespace,
    )
}
