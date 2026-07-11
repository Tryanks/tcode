use std::collections::{HashMap, HashSet};
use std::time::Duration;

use std::path::{Path, PathBuf};

use agent::{FileChange, ItemStatus};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClipboardItem, Context, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, StyledExt as _,
    WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    notification::Notification,
    popover::Popover,
    text::{TextView, TextViewState},
    tooltip::Tooltip,
    v_flex,
};

use crate::app::{AppEvent, AppState};
use crate::git::GitAction;
use crate::session::{EntryContent, TimelineEntry, TurnMeta};
use crate::store::now_millis;
use crate::ui::commit_dialog::CommitDialog;
use crate::ui::composer::{Composer, ComposerEvent};
use crate::ui::terminal_drawer::TerminalDrawer;
use crate::ui::window_drag_area;

/// Content-column max width (T3 centers the timeline at ~760px).
const CONTENT_MAX_WIDTH: f32 = 768.;
/// Minimum horizontal padding around the content column so bubbles/cards never
/// clip when the chat region is narrowed (e.g. the diff panel is open).
const CONTENT_MIN_PADDING: f32 = 24.;
/// How many activity rows to show before the "+N previous log entrys" expander.
const WORKLOG_VISIBLE_ROWS: usize = 2;

/// Markdown state that grows with streaming deltas (stream_markdown pattern).
struct MdState {
    state: Entity<TextViewState>,
    synced: String,
}

pub struct ChatView {
    app_state: Entity<AppState>,
    composer: Entity<Composer>,
    terminal_drawer: Entity<TerminalDrawer>,
    scroll_handle: ScrollHandle,
    md_states: HashMap<String, MdState>,
    /// Open/closed keys for collapsibles (work logs, activity rows, cards, files).
    expanded: HashSet<String>,
    session_key: Option<String>,
    /// Whether the timeline follows streaming output to the bottom. Engaged on
    /// submit and whenever the user scrolls back near the bottom; disengaged
    /// when the user scrolls up to read earlier content.
    follow: bool,
    /// 1s ticker kept alive while a turn is running (drives live "Working for Ns").
    _tick: Option<Task<()>>,
    /// Whether the proposed-plan card's "Copied!" confirmation is showing (2s).
    plan_copied: bool,
    _plan_copied_task: Option<Task<()>>,
    /// The live commit dialog entity while it is open (kept alive across frames).
    commit_dialog: Option<Entity<CommitDialog>>,
    _subscriptions: Vec<Subscription>,
}

impl ChatView {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| Composer::new(app_state.clone(), window, cx));

        let subscriptions = vec![
            cx.subscribe(&composer, |this, _, event, cx| {
                let ComposerEvent::Submitted = event;
                // Force the timeline to the bottom and (re)engage follow-mode so
                // the streaming reply stays visible even if the user had
                // scrolled up before sending.
                this.follow = true;
                this.scroll_handle.scroll_to_bottom();
                cx.notify();
            }),
            cx.observe(&app_state, |this, _, cx| {
                this.sync_markdown_states(cx);
                cx.notify();
            }),
            cx.subscribe_in(&app_state, window, |_, _, event, window, cx| match event {
                AppEvent::Error(message) => {
                    window.push_notification(Notification::error(message.clone()), cx);
                }
                AppEvent::Notice(message) => {
                    window.push_notification(Notification::success(message.clone()), cx);
                }
            }),
        ];
        let terminal_drawer = cx.new(|cx| TerminalDrawer::new(app_state.clone(), cx));

        Self {
            app_state,
            composer,
            terminal_drawer,
            scroll_handle: ScrollHandle::new(),
            md_states: HashMap::new(),
            expanded: HashSet::new(),
            session_key: None,
            follow: true,
            _tick: None,
            plan_copied: false,
            _plan_copied_task: None,
            commit_dialog: None,
            _subscriptions: subscriptions,
        }
    }

    /// Mirror timeline markdown text into `TextViewState` entities, growing
    /// them with `push_str` when possible so streaming reparses incrementally.
    fn sync_markdown_states(&mut self, cx: &mut Context<Self>) {
        let (session_key, texts, running) = {
            let state = self.app_state.read(cx);
            let session_key = state.active_session_id().map(str::to_string);
            let mut texts: Vec<(String, String)> = Vec::new();
            let mut running = false;
            if let Some(active) = &state.active {
                running = active.timeline.turn_running;
                for entry in &active.timeline.entries {
                    match &entry.content {
                        EntryContent::Assistant { text } | EntryContent::Reasoning { text } => {
                            texts.push((entry.id.clone(), text.clone()));
                        }
                        _ => {}
                    }
                }
                // The proposed-plan card renders its markdown too.
                if let Some(plan) = &active.timeline.proposed_plan {
                    texts.push((format!("plan:{}", plan.item_id), plan.markdown.clone()));
                }
            }
            (session_key, texts, running)
        };

        let session_changed = session_key != self.session_key;
        if session_changed {
            self.md_states.clear();
            self.expanded.clear();
            self.session_key = session_key;
            // A freshly opened session starts pinned to the latest content.
            self.follow = true;
        }

        for (id, text) in texts {
            if let Some(md) = self.md_states.get_mut(&id) {
                if md.synced != text {
                    if let Some(delta) = text.strip_prefix(md.synced.as_str()) {
                        let delta = delta.to_string();
                        md.state.update(cx, |state, cx| state.push_str(&delta, cx));
                    } else {
                        md.state.update(cx, |state, cx| state.set_text(&text, cx));
                    }
                    md.synced = text;
                }
            } else {
                let state = cx.new(|cx| TextViewState::markdown(&text, cx));
                self.md_states.insert(
                    id,
                    MdState {
                        state,
                        synced: text,
                    },
                );
            }
        }

        // Keep a 1s ticker alive while a turn runs so the live "Working for Ns"
        // counter advances; drop it (cancelling) when nothing is running.
        if running && self._tick.is_none() {
            self._tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    smol::Timer::after(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        } else if !running {
            self._tick = None;
        }

        if session_changed || (running && self.follow) {
            self.scroll_handle.scroll_to_bottom();
        }
    }

    fn is_near_bottom(&self) -> bool {
        const BOTTOM_FOLLOW_THRESHOLD: f32 = 32.0;
        let remaining = self.scroll_handle.max_offset().y + self.scroll_handle.offset().y;
        remaining <= px(BOTTOM_FOLLOW_THRESHOLD)
    }

    /// Whether there is scrolled-away content below the viewport.
    fn has_content_below(&self) -> bool {
        self.scroll_handle.max_offset().y > px(1.) && !self.is_near_bottom()
    }

    fn toggle_expanded(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
        }
        cx.notify();
    }

    // -- turn rendering -----------------------------------------------------

    /// Render one turn ("Work Log" section) and its surrounding blocks.
    fn render_turn(
        &self,
        index: usize,
        turn: &TurnMeta,
        cwd: &Path,
        entries: &[&TimelineEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut column = v_flex().w_full().gap_3();

        // 1. User messages (right-aligned bubbles).
        for entry in entries {
            if let EntryContent::User { text } = &entry.content {
                column = column.child(self.render_user(index, text, cx));
            }
        }

        // 2. Work Log: activity rows (commands / tools / reasoning / errors).
        let activities: Vec<&TimelineEntry> = entries
            .iter()
            .copied()
            .filter(|e| {
                matches!(
                    e.content,
                    EntryContent::Command { .. }
                        | EntryContent::Tool { .. }
                        | EntryContent::Reasoning { .. }
                        | EntryContent::Error { .. }
                        | EntryContent::ContextCompacted
                )
            })
            .collect();
        if !activities.is_empty() || turn.duration_secs().is_some() || turn.running {
            column = column.child(self.render_work_log(index, turn, &activities, cx));
        }

        // 3. Assistant markdown.
        for entry in entries {
            if let EntryContent::Assistant { text } = &entry.content {
                column = column.child(self.render_assistant(&entry.id, text, cx));
            }
        }

        // 3b. Proposed-plan card (the captured plan for this turn).
        if let Some((item_id, markdown)) = {
            let state = self.app_state.read(cx);
            state
                .active
                .as_ref()
                .and_then(|a| a.timeline.proposed_plan.as_ref())
                .filter(|plan| plan.turn == index)
                .map(|plan| (plan.item_id.clone(), plan.markdown.clone()))
        } {
            column = column.child(self.render_proposed_plan_card(index, &item_id, &markdown, cx));
        }

        // 4. CHANGED FILES card (aggregated across the turn's file changes).
        // The card only appears once the turn has finished: while a turn runs,
        // file edits are visible as Work Log activity rows and accumulate
        // silently. Replay marks turns idle (mark_idle), so finished turns from
        // stored sessions still render the card.
        if !turn.running {
            let mut changes: Vec<&FileChange> = Vec::new();
            for entry in entries {
                if let EntryContent::FileChange {
                    changes: file_changes,
                    ..
                } = &entry.content
                {
                    changes.extend(file_changes.iter());
                }
            }
            if !changes.is_empty() {
                column = column.child(self.render_changed_files(index, cwd, &changes, cx));
            }
        }

        // 5. Turn timestamp row (finished turns with a known end time).
        if !turn.running {
            if let Some(ts) = turn.end_ts.or(entries.last().and_then(|e| e.ts)) {
                column = column.child(self.render_timestamp(ts, cx));
            }
        }

        column.into_any_element()
    }

    fn render_user(&self, turn: usize, text: &str, cx: &mut Context<Self>) -> AnyElement {
        // A user bubble whose turn has a checkpoint gets a hover "revert"
        // affordance (Group B): rewind the thread to just before this message.
        let has_checkpoint = self.app_state.read(cx).turn_has_checkpoint(turn);
        let turn_running = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .is_some_and(|a| a.timeline.turn_running);
        let group_key = format!("user-turn-{turn}");
        let mut row = h_flex()
            .group(group_key.clone())
            .w_full()
            .justify_end()
            .items_center()
            .gap_2();

        if has_checkpoint && !turn_running {
            let app_state = self.app_state.clone();
            row = row.child(
                div()
                    .invisible()
                    .group_hover(group_key.clone(), |s| s.visible())
                    .child(
                        Button::new(("revert", turn))
                            .ghost()
                            .xsmall()
                            .compact()
                            .icon(
                                Icon::empty()
                                    .path("icons/rotate-ccw.svg")
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .tooltip(rust_i18n::t!("checkpoint.revert_tooltip"))
                            .on_click(move |_, window, cx| {
                                let app_state = app_state.clone();
                                window.open_alert_dialog(cx, move |alert, _, _| {
                                    let app_state = app_state.clone();
                                    alert
                                        .title(rust_i18n::t!(
                                            "checkpoint.revert_title",
                                            turn = turn
                                        ))
                                        .description(rust_i18n::t!("checkpoint.revert_description"))
                                        .button_props(
                                            DialogButtonProps::default()
                                                .ok_variant(ButtonVariant::Danger)
                                                .ok_text(rust_i18n::t!("checkpoint.revert_action"))
                                                .cancel_text(rust_i18n::t!("settings.cancel"))
                                                .show_cancel(true),
                                        )
                                        .on_ok(move |_, _, cx| {
                                            app_state.update(cx, |state, cx| {
                                                state.revert_to_turn(turn, cx);
                                            });
                                            true
                                        })
                                });
                            }),
                    ),
            );
        }

        row.child(
            div()
                .max_w_3_4()
                .px_4()
                .py_3()
                .rounded_xl()
                .bg(cx.theme().muted)
                .text_color(cx.theme().foreground)
                .text_size(px(15.))
                .child(text.to_string()),
        )
        .into_any_element()
    }

    fn render_assistant(&self, id: &str, text: &str, _cx: &mut Context<Self>) -> AnyElement {
        let content: AnyElement = if let Some(md) = self.md_states.get(id) {
            TextView::new(&md.state).selectable(true).into_any_element()
        } else {
            div().child(text.to_string()).into_any_element()
        };
        div()
            .w_full()
            .text_size(px(15.))
            .line_height(px(26.))
            .child(content)
            .into_any_element()
    }

    /// The Work Log section: a top divider, activity rows (collapsible), and a
    /// "Worked for XmYYs" footer (or a live "Working for Ns" indicator).
    fn render_work_log(
        &self,
        index: usize,
        turn: &TurnMeta,
        activities: &[&TimelineEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section_key = format!("worklog-{index}");
        let rows_key = format!("worklog-rows-{index}");
        // Finished turns collapse by default; running turns are always open.
        let expanded = turn.running || self.expanded.contains(&section_key);
        let muted = cx.theme().muted_foreground;

        let mut section = v_flex()
            .w_full()
            .pt_4()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().border);

        if expanded {
            if !turn.running {
                section = section.child(
                    div()
                        .text_size(px(11.))
                        .font_medium()
                        .text_color(muted)
                        .child(rust_i18n::t!("chat.work_log")),
                );
            }

            let total = activities.len();
            let rows_expanded = self.expanded.contains(&rows_key);
            let hidden = total.saturating_sub(WORKLOG_VISIBLE_ROWS);
            let visible: Vec<&TimelineEntry> = if rows_expanded || hidden == 0 {
                activities.to_vec()
            } else {
                activities[total - WORKLOG_VISIBLE_ROWS..].to_vec()
            };

            for entry in &visible {
                section = section.child(self.render_activity_row(entry, cx));
            }

            if !rows_expanded && hidden > 0 {
                section = section.child(
                    h_flex()
                        .id(("worklog-more", index))
                        .gap_1()
                        .items_center()
                        .py_0p5()
                        .text_size(px(13.))
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(|s| s.text_color(cx.theme().foreground))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_expanded(&rows_key, cx);
                        }))
                        .child(Icon::new(IconName::ChevronDown).xsmall())
                        .child(rust_i18n::t!("chat.previous_logs", count = hidden)),
                );
            }
        }

        // Footer: live "Working" indicator, or the toggleable "Worked for" row.
        if turn.running {
            let secs = turn
                .start_ts
                .map(|start| now_millis().saturating_sub(start) / 1000)
                .unwrap_or(0);
            section = section.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(div().text_color(cx.theme().primary).child("•••"))
                    .child(rust_i18n::t!(
                        "chat.working_for",
                        duration = format_duration(secs)
                    )),
            );
        } else {
            let label = match turn.duration_secs() {
                Some(secs) => {
                    rust_i18n::t!("chat.worked_for", duration = format_duration(secs)).into_owned()
                }
                None => rust_i18n::t!("chat.worked").into_owned(),
            };
            section = section.child(
                h_flex()
                    .id(("worklog-footer", index))
                    .gap_1()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .cursor_pointer()
                    .hover(|s| s.text_color(cx.theme().foreground))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_expanded(&section_key, cx);
                    }))
                    .child(label)
                    .child(Icon::new(chevron(expanded)).xsmall()),
            );
        }

        section.into_any_element()
    }

    /// One Work Log activity row: a muted status icon + a one-line summary.
    fn render_activity_row(&self, entry: &TimelineEntry, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let (icon, summary): (IconName, AnyElement) = match &entry.content {
            EntryContent::Command {
                command, status, ..
            } => {
                let icon = if matches!(status, ItemStatus::InProgress) {
                    IconName::SquareTerminal
                } else {
                    IconName::Check
                };
                let summary = h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .overflow_hidden()
                    .child(div().flex_none().child(rust_i18n::t!("chat.command_run")))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(muted)
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(one_line(command)),
                    )
                    .into_any_element();
                (icon, summary)
            }
            EntryContent::Tool {
                name,
                input,
                output,
                status,
            } => {
                // Prefer an input summary; fall back to a snippet of the output.
                let mut brief = tool_brief(input);
                if brief.is_empty() {
                    if let Some(output) = output {
                        brief = one_line(output);
                    }
                }
                let summary = h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .overflow_hidden()
                    .child(div().flex_none().child(name.clone()))
                    .when(!brief.is_empty(), |this| {
                        this.child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_color(muted)
                                .child(brief),
                        )
                    })
                    .into_any_element();
                (activity_icon(*status), summary)
            }
            EntryContent::Reasoning { text } => {
                let summary = h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .overflow_hidden()
                    .child(div().flex_none().child(rust_i18n::t!("chat.thinking")))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(muted)
                            .child(one_line(text)),
                    )
                    .into_any_element();
                (IconName::Check, summary)
            }
            EntryContent::Error { message } => {
                let summary = div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(cx.theme().danger)
                    .child(one_line(message))
                    .into_any_element();
                (IconName::TriangleAlert, summary)
            }
            EntryContent::ContextCompacted => {
                let summary = div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(muted)
                    .child(rust_i18n::t!("chat.context_compacted"))
                    .into_any_element();
                (IconName::Minimize, summary)
            }
            // Non-activity content never reaches this row.
            _ => (IconName::Check, div().into_any_element()),
        };

        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .py_0p5()
            .text_size(px(13.))
            .child(Icon::new(icon).xsmall().text_color(muted))
            .child(summary)
            .into_any_element()
    }

    /// The CHANGED FILES card: header with totals + a directory-grouped tree.
    fn render_changed_files(
        &self,
        index: usize,
        cwd: &Path,
        changes: &[&FileChange],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let card_key = format!("card-{index}");
        let collapsed = self.expanded.contains(&card_key);

        let (total_add, total_del): (u32, u32) = changes
            .iter()
            .map(|c| diff_stats(c.diff.as_deref()))
            .fold((0, 0), |(a, d), (ca, cd)| (a + ca, d + cd));

        let header = h_flex()
            .w_full()
            .px_4()
            .py_2()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(muted)
                    .child(rust_i18n::t!("chat.changed_files", count = changes.len()))
                    .child("·")
                    .child(
                        div()
                            .text_color(cx.theme().success)
                            .child(format!("+{total_add}")),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().danger)
                            .child(format!("-{total_del}")),
                    ),
            )
            .child(
                Button::new(("collapse-all", index))
                    .ghost()
                    .xsmall()
                    .label(if collapsed {
                        rust_i18n::t!("chat.expand_all")
                    } else {
                        rust_i18n::t!("chat.collapse_all")
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_expanded(&card_key, cx);
                    })),
            )
            .child(
                Button::new(("view-diff", index))
                    .outline()
                    .xsmall()
                    .label(rust_i18n::t!("chat.view_diff"))
                    .tooltip(rust_i18n::t!("chat.view_diff_tooltip"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.open_diff_for_turn(index, cx));
                    })),
            );

        let mut card = v_flex()
            .w_full()
            .rounded(px(10.))
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .child(header);

        if !collapsed {
            let mut body = v_flex().w_full().px_2().pb_2().gap(px(1.));
            for (dir, files) in group_by_dir(changes, cwd) {
                let dir_add: u32 = files.iter().map(|f| f.added).sum();
                let dir_del: u32 = files.iter().map(|f| f.deleted).sum();
                if !dir.is_empty() {
                    body = body.child(
                        h_flex()
                            .w_full()
                            .px_2()
                            .py_1()
                            .gap_1p5()
                            .items_center()
                            .text_size(px(13.))
                            .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted))
                            .child(Icon::new(IconName::Folder).xsmall().text_color(muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(dir.clone()),
                            )
                            .child(diff_counts(dir_add, dir_del, cx)),
                    );
                }
                for file in files {
                    body = body.child(
                        h_flex()
                            .w_full()
                            .pl(px(if dir.is_empty() { 8. } else { 28. }))
                            .pr_2()
                            .py_1()
                            .gap_1p5()
                            .items_center()
                            .text_size(px(13.))
                            .child(Icon::new(IconName::File).xsmall().text_color(muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(file.name),
                            )
                            .child(diff_counts(file.added, file.deleted, cx)),
                    );
                }
            }
            card = card.child(body);
        }

        card.into_any_element()
    }

    /// The inline proposed-plan timeline card (S1 §5): a "Plan" badge, title,
    /// markdown body (collapsible when long), and Copy / Download / Save actions.
    fn render_proposed_plan_card(
        &self,
        turn: usize,
        item_id: &str,
        markdown: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = crate::session::plan_title(markdown)
            .unwrap_or_else(|| rust_i18n::t!("plan.proposed_plan").into_owned());
        let long = markdown.chars().count() > 900 || markdown.lines().count() > 20;
        let collapse_key = format!("plan-card-{turn}");
        let collapsed = long && self.expanded.contains(&collapse_key);

        let body: AnyElement = if collapsed {
            div().into_any_element()
        } else if let Some(md) = self.md_states.get(&format!("plan:{item_id}")) {
            div()
                .w_full()
                .text_size(px(14.))
                .line_height(px(22.))
                .child(TextView::new(&md.state).selectable(true))
                .into_any_element()
        } else {
            div()
                .w_full()
                .child(markdown.to_string())
                .into_any_element()
        };

        let md_copy = markdown.to_string();
        let md_download = markdown.to_string();
        let md_save = markdown.to_string();
        let copied = self.plan_copied;

        v_flex()
            .w_full()
            .gap_2()
            .p_4()
            .rounded(px(12.))
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .px_2()
                            .py(px(1.))
                            .rounded(px(4.))
                            .bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground)
                            .text_size(px(11.))
                            .font_medium()
                            .child(rust_i18n::t!("plan.badge")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(15.))
                            .font_semibold()
                            .child(title),
                    )
                    .when(long, |this| {
                        let key = collapse_key.clone();
                        this.child(
                            Button::new(("plan-collapse", turn))
                                .ghost()
                                .xsmall()
                                .label(if collapsed {
                                    rust_i18n::t!("plan.expand")
                                } else {
                                    rust_i18n::t!("plan.collapse")
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.toggle_expanded(&key, cx);
                                })),
                        )
                    }),
            )
            .child(body)
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        Button::new(("plan-copy", turn))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .label(if copied {
                                rust_i18n::t!("plan.copied")
                            } else {
                                rust_i18n::t!("plan.copy")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_copy.clone();
                                this.app_state.update(cx, |s, cx| s.copy_plan(md, cx));
                                this.mark_plan_copied(cx);
                            })),
                    )
                    .child(
                        Button::new(("plan-download", turn))
                            .ghost()
                            .xsmall()
                            .icon(Icon::empty().path("icons/download.svg"))
                            .label(rust_i18n::t!("plan.download"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_download.clone();
                                this.app_state.update(cx, |s, cx| s.download_plan(md, cx));
                            })),
                    )
                    .child(
                        Button::new(("plan-save", turn))
                            .ghost()
                            .xsmall()
                            .icon(IconName::HardDrive)
                            .label(rust_i18n::t!("plan.save_workspace"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let md = md_save.clone();
                                this.app_state
                                    .update(cx, |s, cx| s.save_plan_to_workspace(md, cx));
                            })),
                    ),
            )
            .into_any_element()
    }

    fn mark_plan_copied(&mut self, cx: &mut Context<Self>) {
        self.plan_copied = true;
        self._plan_copied_task = Some(cx.spawn(async move |this, cx| {
            smol::Timer::after(Duration::from_secs(2)).await;
            let _ = this.update(cx, |this, cx| {
                this.plan_copied = false;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_timestamp(&self, ts: u64, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .gap_1p5()
            .items_center()
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .child(Icon::new(IconName::Info).xsmall())
            .child(format_local_time(ts))
            .into_any_element()
    }

    // -- top-level surfaces -------------------------------------------------

    fn render_header(
        &self,
        title: Option<String>,
        is_draft: bool,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // With the sidebar collapsed to its 48px strip, the native traffic
        // lights (which sit at the window's top-left) overhang into the chat
        // header — inset the header content so the title clears them.
        let collapsed = self.app_state.read(cx).sidebar_collapsed;
        let base = h_flex()
            .flex_shrink_0()
            .h(px(52.))
            .px_4()
            .when(collapsed, |this| this.pl(px(36.)))
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border);

        // A draft shows a muted "New thread" label; an open thread its title;
        // nothing active shows "No active thread".
        let title_el = if is_draft {
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(16.))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("chat.new_thread"))
        } else {
            match &title {
                Some(title) => div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(16.))
                    .font_medium()
                    .child(title.clone()),
                None => div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(16.))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("chat.no_active_thread")),
            }
        };

        // The right-side cluster (Open split-button + panel toggles) shows for
        // any active thread, including a draft.
        let show_actions = is_draft || title.is_some();
        let diff_showing = {
            let state = self.app_state.read(cx);
            state.diff_panel_open() && state.right_tab() == crate::app::RightTab::Diff
        };
        let plan_showing = self.app_state.read(cx).plan_panel_showing();
        let preview_showing = self.app_state.read(cx).preview_panel_showing();
        let terminal_open = self.app_state.read(cx).terminal_panel_open();
        window_drag_area("chat-header-drag", base, window, cx)
            .child(title_el)
            .when(show_actions, |this| {
                this.children(self.render_git_button(cx))
                    .children(cwd.clone().map(|cwd| self.render_open_button(cwd, cx)))
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .child(
                                Button::new("panel-layout")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::PanelBottom)
                                    .selected(terminal_open)
                                    .tooltip(rust_i18n::t!("chat.toggle_terminal"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.app_state.update(cx, |state, cx| {
                                            state.toggle_terminal_panel(cx)
                                        });
                                    })),
                            )
                            .child(
                                Button::new("plan-panel")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::Map)
                                    .selected(plan_showing)
                                    .tooltip(rust_i18n::t!("chat.toggle_plan"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.app_state
                                            .update(cx, |state, cx| state.toggle_plan_panel(cx));
                                    })),
                            )
                            .child(
                                Button::new("preview-panel")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::Globe)
                                    .selected(preview_showing)
                                    .tooltip(rust_i18n::t!("chat.toggle_preview"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.app_state
                                            .update(cx, |state, cx| state.toggle_preview_panel(cx));
                                    })),
                            )
                            .child(
                                Button::new("diff-panel")
                                    .ghost()
                                    .small()
                                    .compact()
                                    .icon(IconName::PanelRight)
                                    .selected(diff_showing)
                                    .tooltip(rust_i18n::t!("chat.toggle_diff"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.app_state
                                            .update(cx, |state, cx| state.toggle_diff_panel(cx));
                                    })),
                            ),
                    )
            })
            .into_any_element()
    }

    /// The adaptive Git quick-action split-button (left of Open): the primary
    /// action follows the background git status (Commit / Commit & push / Push /
    /// Pull / Publish branch / Initialize Git, or a disabled status hint); the
    /// chevron lists the applicable subset. Ported from T3's `GitActionsControl`.
    fn render_git_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let quick = self.app_state.read(cx).git_quick_action()?;
        let border = cx.theme().border;
        let items = self.app_state.read(cx).git_menu_items();

        // Main action segment.
        let label: SharedString = rust_i18n::t!(quick.label_key).into_owned().into();
        let main_icon = quick
            .action
            .map(git_action_icon)
            .unwrap_or_else(|| Icon::empty().path("icons/git-branch.svg"));
        let mut main = h_flex()
            .id("git-main")
            .h_full()
            .px_2()
            .gap_1p5()
            .items_center()
            .text_size(px(13.))
            .child(main_icon.xsmall().text_color(if quick.disabled {
                cx.theme().muted_foreground
            } else {
                cx.theme().foreground
            }))
            .child(label);
        if quick.disabled {
            main = main.text_color(cx.theme().muted_foreground);
            if let Some(hint) = quick.hint_key {
                let text: SharedString = rust_i18n::t!(hint).into_owned().into();
                main = main.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx));
            }
        } else if let Some(action) = quick.action {
            main = main
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().accent))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.trigger_git_action(action, window, cx);
                }));
        }

        // Dropdown listing the applicable subset. Menu rows dispatch through the
        // ChatView entity (the popover content runs at App level, not in a view
        // context, so `cx.listener` is unavailable here).
        let chat = cx.entity();
        let chevron = Popover::new("git-menu")
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("git-menu-trigger")
                    .ghost()
                    .compact()
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let muted = cx.theme().muted_foreground;
                let accent = cx.theme().accent;
                let popover = cx.entity();
                let mut menu = v_flex().w(px(210.)).p_1().gap_0p5();
                for (index, item) in items.clone().into_iter().enumerate() {
                    let label: SharedString = rust_i18n::t!(item.label_key).into_owned().into();
                    let action = item.action;
                    let disabled = item.disabled;
                    let popover = popover.clone();
                    let chat = chat.clone();
                    let mut row = h_flex()
                        .id(("git-menu-item", index))
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .text_size(px(13.))
                        .child(git_action_icon(action).xsmall().text_color(muted))
                        .child(div().flex_1().child(label));
                    if disabled {
                        row = row.text_color(muted);
                        if let Some(hint) = item.hint_key {
                            let text: SharedString = rust_i18n::t!(hint).into_owned().into();
                            row = row.tooltip(move |window, cx| {
                                Tooltip::new(text.clone()).build(window, cx)
                            });
                        }
                    } else {
                        row = row.cursor_pointer().hover(move |s| s.bg(accent)).on_click(
                            move |_, window, cx| {
                                popover.update(cx, |st, cx| st.dismiss(window, cx));
                                chat.update(cx, |this, cx| {
                                    this.trigger_git_action(action, window, cx)
                                });
                            },
                        );
                    }
                    menu = menu.child(row);
                }
                menu.into_any_element()
            });

        Some(
            h_flex()
                .flex_none()
                .h(px(28.))
                .items_center()
                .rounded(px(8.))
                .border_1()
                .border_color(border)
                .overflow_hidden()
                .child(main)
                .child(div().w_px().h(px(16.)).bg(border))
                .child(chevron)
                .into_any_element(),
        )
    }

    /// Dispatch a git quick-action: commit-style actions open the commit dialog;
    /// everything else runs in the background with a progress toast.
    fn trigger_git_action(
        &mut self,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.opens_commit_dialog() {
            self.open_commit_dialog(action, window, cx);
        } else {
            self.app_state.update(cx, |state, cx| {
                state.run_git_action(action, None, None, None, cx)
            });
        }
    }

    /// Open the commit dialog for `action` (Commit or Commit & push).
    fn open_commit_dialog(
        &mut self,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| CommitDialog::new(self.app_state.clone(), action, window, cx));
        self.commit_dialog = Some(dialog.clone());
        window.open_dialog(cx, move |dlg, window, cx| {
            let content = dialog.clone();
            let footer_dialog = dialog.clone();
            dlg.title(rust_i18n::t!("git.commit.title").into_owned())
                .w(px(600.))
                .content(move |content_el, _window, _cx| content_el.child(content.clone()))
                .footer(render_commit_footer(&footer_dialog, window, cx))
        });
    }

    /// The bordered "Open" split-button: main click opens the session cwd in
    /// Zed; the chevron opens a menu (Zed / Finder / Copy path). Matches T3's
    /// header control.
    fn render_open_button(&self, cwd: PathBuf, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().border;
        let main_cwd = cwd.clone();
        let menu_cwd = cwd;

        let chevron = Popover::new("open-menu")
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("open-menu-trigger")
                    .ghost()
                    .compact()
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let zed_cwd = menu_cwd.clone();
                let reveal_cwd = menu_cwd.clone();
                let copy_cwd = menu_cwd.clone();
                let popover = cx.entity();
                let p1 = popover.clone();
                let p2 = popover.clone();
                let p3 = popover.clone();
                let muted = cx.theme().muted_foreground;
                let accent = cx.theme().accent;
                let menu_item = move |id: &'static str, icon: IconName, label: SharedString| {
                    h_flex()
                        .id(id)
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_size(px(13.))
                        .hover(move |s| s.bg(accent))
                        .child(Icon::new(icon).xsmall().text_color(muted))
                        .child(label)
                };
                v_flex()
                    .w(px(180.))
                    .p_1()
                    .gap_0p5()
                    .child(
                        menu_item(
                            "open-zed",
                            IconName::ExternalLink,
                            rust_i18n::t!("chat.open_zed").into_owned().into(),
                        )
                        .on_click(move |_, window, cx| {
                            open_in_zed(&zed_cwd, window, cx);
                            p1.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        menu_item(
                            "reveal-in-file-manager",
                            IconName::FolderOpen,
                            rust_i18n::t!("chat.reveal_in_file_manager")
                                .into_owned()
                                .into(),
                        )
                        .on_click(move |_, window, cx| {
                            reveal_in_file_manager(&reveal_cwd, cx);
                            p2.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        menu_item(
                            "copy-path",
                            IconName::Copy,
                            rust_i18n::t!("chat.copy_path").into_owned().into(),
                        )
                        .on_click(move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                copy_cwd.display().to_string(),
                            ));
                            p3.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .into_any_element()
            });

        h_flex()
            .flex_none()
            .h(px(28.))
            .items_center()
            .rounded(px(8.))
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .child(
                h_flex()
                    .id("open-main")
                    .h_full()
                    .px_2()
                    .gap_1p5()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .hover(|s| s.bg(cx.theme().accent))
                    .child(
                        Icon::new(IconName::ExternalLink)
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(rust_i18n::t!("chat.open"))
                    .on_click(cx.listener(move |_, _, window, cx| {
                        open_in_zed(&main_cwd, window, cx);
                    })),
            )
            .child(div().w_px().h(px(16.)).bg(border))
            .child(chevron)
            .into_any_element()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                div()
                    .text_size(px(20.))
                    .font_semibold()
                    .child(rust_i18n::t!("chat.empty_title")),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("chat.empty_description")),
            )
            .into_any_element()
    }

    fn render_scroll_pill(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .flex_shrink_0()
            .w_full()
            .justify_center()
            .pb_1()
            .child(
                Button::new("scroll-to-end")
                    .outline()
                    .small()
                    .icon(IconName::ChevronDown)
                    .label(rust_i18n::t!("chat.scroll_end"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.follow = true;
                        this.scroll_handle.scroll_to_bottom();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Screenshot-only: `--debug-git-dialog` opens the commit dialog once the
        // background git status has landed (a header click is not drivable
        // headlessly). Consumed once.
        let open_commit_dialog = self.app_state.update(cx, |state, _| {
            let armed = state.debug_open_commit_dialog && state.git_status.is_some();
            if armed {
                state.debug_open_commit_dialog = false;
            }
            armed
        });
        if open_commit_dialog {
            self.open_commit_dialog(GitAction::Commit, window, cx);
        }

        let active = {
            let state = self.app_state.read(cx);
            state.active.as_ref().map(|active| {
                (
                    active.meta.title.clone(),
                    active.timeline.entries.clone(),
                    active.timeline.turns.clone(),
                    active.meta.cwd.clone(),
                    active.draft,
                )
            })
        };

        let root = v_flex().size_full().min_w_0().bg(cx.theme().background);

        let Some((title, entries, turns, cwd, is_draft)) = active else {
            return root
                .child(self.render_header(None, false, None, window, cx))
                .child(self.render_empty_state(cx));
        };

        let title = if is_draft { None } else { Some(title) };
        let header = self.render_header(title, is_draft, Some(cwd.clone()), window, cx);
        let terminal_open = self.app_state.read(cx).terminal_panel_open();
        let terminal_height = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .map(|a| a.terminal_workspace.height)
            .unwrap_or(240.);

        // Group entries by turn and render each turn section into the centered
        // content column. The column fills the available width up to
        // `CONTENT_MAX_WIDTH`; horizontal padding lives on the centering wrapper
        // (below) so the column shrinks gracefully — never clipping — when the
        // diff panel narrows the chat region.
        let mut column = v_flex()
            .w_full()
            .max_w(px(CONTENT_MAX_WIDTH))
            .py_6()
            .gap_8();
        for (index, turn) in turns.iter().enumerate() {
            let turn_entries: Vec<&TimelineEntry> =
                entries.iter().filter(|e| e.turn == index).collect();
            if turn_entries.is_empty() {
                continue;
            }
            column = column.child(self.render_turn(index, turn, &cwd, &turn_entries, cx));
        }

        let main = v_flex()
            .size_full()
            .min_h_0()
            .child(
                div()
                    .id("timeline")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                        // Following disengages when the user scrolls up to read,
                        // and re-engages once they return near the bottom.
                        this.follow = this.is_near_bottom();
                        cx.notify();
                    }))
                    .child(
                        h_flex()
                            .w_full()
                            .justify_center()
                            .px(px(CONTENT_MIN_PADDING))
                            .child(column),
                    ),
            )
            .when(self.has_content_below(), |this| {
                this.child(self.render_scroll_pill(cx))
            })
            .child(self.composer.clone());

        let body: AnyElement = if terminal_open {
            let drawer = self.terminal_drawer.clone();
            let drawer_resize = self.terminal_drawer.clone();
            let width = f32::from(window.bounds().size.width);
            drawer.update(cx, |drawer, cx| drawer.resize(width, terminal_height, cx));
            gpui_component::resizable::v_resizable("chat-terminal-panels")
                .on_resize(move |state, _, cx| {
                    let height = state.read(cx).sizes().get(1).copied();
                    if let Some(height) = height {
                        drawer_resize
                            .update(cx, |drawer, cx| drawer.resize(width, f32::from(height), cx));
                    }
                })
                .child(gpui_component::resizable::resizable_panel().child(main))
                .child(
                    gpui_component::resizable::resizable_panel()
                        .flex_none()
                        .size(px(terminal_height))
                        .size_range(px(120.)..px(600.))
                        .child(self.terminal_drawer.clone()),
                )
                .into_any_element()
        } else {
            main.into_any_element()
        };
        root.child(header).child(body)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn chevron(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}

/// Launch `zed <cwd>` detached; surface a notification if the CLI is missing.
/// The leading icon for a git quick-action.
fn git_action_icon(action: GitAction) -> Icon {
    match action {
        GitAction::Push => Icon::new(IconName::ArrowUp),
        GitAction::Pull => Icon::empty().path("icons/download.svg"),
        _ => Icon::empty().path("icons/git-branch.svg"),
    }
}

/// The commit dialog's footer action row (Cancel / Commit[& push]). Built inside
/// the `open_dialog` builder so the buttons can close the dialog on click.
fn render_commit_footer(
    dialog: &Entity<CommitDialog>,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let confirm_label = dialog.update(cx, |d, cx| d.confirm_label(cx));
    let cancel_dialog = dialog.clone();
    let confirm_dialog = dialog.clone();
    h_flex()
        .w_full()
        .gap_2()
        .justify_end()
        .child(
            Button::new("commit-cancel")
                .ghost()
                .label(rust_i18n::t!("git.commit.cancel"))
                .on_click(move |_, window, cx| {
                    let _ = &cancel_dialog;
                    window.close_dialog(cx);
                }),
        )
        .child(
            Button::new("commit-confirm")
                .primary()
                .label(confirm_label)
                .on_click(move |_, window, cx| {
                    let should_close = confirm_dialog.update(cx, |d, cx| d.confirm(window, cx));
                    if should_close {
                        window.close_dialog(cx);
                    }
                }),
        )
        .into_any_element()
}

fn open_in_zed(cwd: &Path, window: &mut Window, cx: &mut App) {
    if crate::process::command("zed").arg(cwd).spawn().is_err() {
        window.push_notification(
            Notification::error(rust_i18n::t!("errors.zed_cli_missing")),
            cx,
        );
    }
}

/// Reveal `cwd` in the platform's file manager (Finder / Explorer / the XDG
/// file manager). gpui does the platform dispatch, so no shell-out is needed.
fn reveal_in_file_manager(cwd: &Path, cx: &mut App) {
    cx.reveal_path(cwd);
}

/// Leading icon for a Work Log activity row, keyed on the item's status.
fn activity_icon(status: ItemStatus) -> IconName {
    match status {
        ItemStatus::InProgress => IconName::LoaderCircle,
        ItemStatus::Completed => IconName::Check,
        ItemStatus::Failed | ItemStatus::Declined => IconName::CircleX,
    }
}

/// First non-empty line of `text`, collapsed to a single spaced line.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// A short one-line summary of a tool call's input for the Work Log.
fn tool_brief(input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Object(map) => map
            .get("query")
            .or_else(|| map.get("path"))
            .or_else(|| map.get("command"))
            .or_else(|| map.get("summary"))
            .and_then(|v| v.as_str())
            .map(one_line)
            .unwrap_or_default(),
        serde_json::Value::String(s) => one_line(s),
        _ => String::new(),
    }
}

/// Wall-clock duration formatted as "XmYYs" / "YYs".
fn format_duration(secs: u64) -> String {
    if secs >= 60 {
        rust_i18n::t!(
            "time.duration_minutes",
            minutes = secs / 60,
            seconds = format!("{:02}", secs % 60)
        )
        .into_owned()
    } else {
        rust_i18n::t!("time.duration_seconds", seconds = secs).into_owned()
    }
}

/// Count added / removed lines in a unified diff (ignoring the `+++`/`---`
/// file headers).
fn diff_stats(diff: Option<&str>) -> (u32, u32) {
    let Some(diff) = diff else {
        return (0, 0);
    };
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') => added += 1,
            Some(b'-') => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

struct FileRow {
    name: String,
    added: u32,
    deleted: u32,
}

/// Make `path` relative to the session `cwd` when it lives under it; otherwise
/// return it unchanged (absolute paths outside the repo stay absolute).
///
/// `canonical_cwd` is the symlink-resolved form of `cwd` (e.g. `/tmp` →
/// `/private/tmp` on macOS): providers often report canonical paths while the
/// stored cwd is the symlinked one, so we try both prefixes.
fn relativize(path: &str, cwd: &Path, canonical_cwd: &Path) -> String {
    let p = Path::new(path);
    p.strip_prefix(cwd)
        .or_else(|_| p.strip_prefix(canonical_cwd))
        .map(|rel| rel.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// Group file changes by their parent directory (preserving first-seen order),
/// so the CHANGED FILES card can render a folder → files tree. Paths are shown
/// relative to the session `cwd` when they live under it.
fn group_by_dir(changes: &[&FileChange], cwd: &Path) -> Vec<(String, Vec<FileRow>)> {
    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut groups: Vec<(String, Vec<FileRow>)> = Vec::new();
    for change in changes {
        let display = relativize(&change.path, cwd, &canonical_cwd);
        let (dir, name) = match display.rsplit_once('/') {
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => (String::new(), display.clone()),
        };
        let (added, deleted) = diff_stats(change.diff.as_deref());
        let row = FileRow {
            name,
            added,
            deleted,
        };
        if let Some(group) = groups.iter_mut().find(|(d, _)| *d == dir) {
            group.1.push(row);
        } else {
            groups.push((dir, vec![row]));
        }
    }
    groups
}

fn diff_counts(added: u32, deleted: u32, cx: &Context<ChatView>) -> AnyElement {
    h_flex()
        .flex_none()
        .gap_2()
        .text_size(px(12.))
        .child(
            div()
                .text_color(cx.theme().success)
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(cx.theme().danger)
                .child(format!("-{deleted}")),
        )
        .into_any_element()
}

/// Format a unix-ms timestamp as a local 12-hour clock, e.g. "2:39 AM".
///
/// `chrono::Local` reads the platform's timezone (Unix: the tz database /
/// `localtime_r`; Windows: the OS timezone API), so this is correct on all three
/// targets — unlike the hand-rolled `localtime_r` FFI it replaces, whose `tm`
/// layout was UB on 32-bit and which fell back to a UTC clock on Windows.
fn format_local_time(unix_ms: u64) -> String {
    use chrono::{Local, TimeZone as _, Timelike as _};

    let Some(local) = Local.timestamp_millis_opt(unix_ms as i64).single() else {
        return String::new();
    };
    twelve_hour(local.hour() as i32, local.minute() as i32)
}

fn twelve_hour(hour24: i32, minute: i32) -> String {
    let (hour12, meridiem) = match hour24 {
        0 => (12, "AM"),
        1..=11 => (hour24, "AM"),
        12 => (12, "PM"),
        _ => (hour24 - 12, "PM"),
    };
    format!("{hour12}:{minute:02} {meridiem}")
}

#[cfg(test)]
mod tests {
    use super::relativize;
    use std::path::Path;

    #[test]
    fn relativize_strips_cwd_prefix() {
        let cwd = Path::new("/tmp/proj");
        let canon = Path::new("/private/tmp/proj");
        assert_eq!(relativize("/tmp/proj/src/a.rs", cwd, canon), "src/a.rs");
        assert_eq!(relativize("/tmp/proj/a.rs", cwd, canon), "a.rs");
        // Provider reports the canonical (symlink-resolved) path.
        assert_eq!(
            relativize("/private/tmp/proj/src/a.rs", cwd, canon),
            "src/a.rs"
        );
        // Outside the cwd stays absolute.
        assert_eq!(relativize("/other/x.rs", cwd, canon), "/other/x.rs");
        // Already-relative paths are left as-is.
        assert_eq!(relativize("src/b.rs", cwd, canon), "src/b.rs");
    }
}
