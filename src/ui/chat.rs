use std::collections::{HashMap, HashSet};
use std::time::Duration;

use std::path::Path;

use agent::{FileChange, ItemStatus};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, StyledExt as _,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    notification::Notification,
    text::{TextView, TextViewState},
    v_flex,
};

use crate::app::{AppEvent, AppState};
use crate::session::{EntryContent, TimelineEntry, TurnMeta};
use crate::store::now_millis;
use crate::ui::composer::{Composer, ComposerEvent};
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
            cx.subscribe_in(&app_state, window, |_, _, event, window, cx| {
                let AppEvent::Error(message) = event;
                window.push_notification(Notification::error(message.clone()), cx);
            }),
        ];

        Self {
            app_state,
            composer,
            scroll_handle: ScrollHandle::new(),
            md_states: HashMap::new(),
            expanded: HashSet::new(),
            session_key: None,
            follow: true,
            _tick: None,
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
                column = column.child(self.render_user(text, cx));
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

    fn render_user(&self, text: &str, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .justify_end()
            .child(
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
                        .child("Work Log"),
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
                        .child(format!("+{hidden} previous log entrys")),
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
                    .child(format!("Working for {}", format_duration(secs))),
            );
        } else {
            let label = match turn.duration_secs() {
                Some(secs) => format!("Worked for {}", format_duration(secs)),
                None => "Worked".to_string(),
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
            EntryContent::Command { command, status, .. } => {
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
                    .child(div().flex_none().child("Command run"))
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
                    .child(div().flex_none().child("Thinking"))
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
                    .child(format!("CHANGED FILES ({})", changes.len()))
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
                    .label(if collapsed { "Expand all" } else { "Collapse all" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_expanded(&card_key, cx);
                    })),
            )
            .child(
                Button::new(("view-diff", index))
                    .outline()
                    .xsmall()
                    .label("View diff")
                    .tooltip("Open this turn in the diff panel")
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let base = h_flex()
            .flex_shrink_0()
            .h(px(52.))
            .px_4()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border);

        let title_el = match &title {
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
                .child("No active thread"),
        };

        let diff_open = self.app_state.read(cx).diff_panel_open();
        window_drag_area("chat-header-drag", base, window, cx)
            .child(title_el)
            .when(title.is_some(), |this| {
                this.child(
                    h_flex()
                        .flex_none()
                        .gap_1()
                        .child(
                            Button::new("panel-layout")
                                .ghost()
                                .small()
                                .compact()
                                .icon(IconName::PanelBottom)
                                .tooltip("Toggle panel (soon)"),
                        )
                        .child(
                            Button::new("diff-panel")
                                .ghost()
                                .small()
                                .compact()
                                .icon(IconName::PanelRight)
                                .selected(diff_open)
                                .tooltip("Toggle diff panel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.app_state
                                        .update(cx, |state, cx| state.toggle_diff_panel(cx));
                                })),
                        ),
                )
            })
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
                    .child("Pick a thread to continue"),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(cx.theme().muted_foreground)
                    .child("Select an existing thread or create a new one to get started."),
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
                    .label("Scroll to end")
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
        let active = {
            let state = self.app_state.read(cx);
            state.active.as_ref().map(|active| {
                (
                    active.meta.title.clone(),
                    active.timeline.entries.clone(),
                    active.timeline.turns.clone(),
                    active.meta.cwd.clone(),
                )
            })
        };

        let root = v_flex().size_full().min_w_0().bg(cx.theme().background);

        let Some((title, entries, turns, cwd)) = active else {
            return root
                .child(self.render_header(None, window, cx))
                .child(self.render_empty_state(cx));
        };

        let header = self.render_header(Some(title), window, cx);

        // Group entries by turn and render each turn section into the centered
        // content column. The column fills the available width up to
        // `CONTENT_MAX_WIDTH`; horizontal padding lives on the centering wrapper
        // (below) so the column shrinks gracefully — never clipping — when the
        // diff panel narrows the chat region.
        let mut column = v_flex().w_full().max_w(px(CONTENT_MAX_WIDTH)).py_6().gap_8();
        for (index, turn) in turns.iter().enumerate() {
            let turn_entries: Vec<&TimelineEntry> =
                entries.iter().filter(|e| e.turn == index).collect();
            if turn_entries.is_empty() {
                continue;
            }
            column = column.child(self.render_turn(index, turn, &cwd, &turn_entries, cx));
        }

        root.child(header)
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
            .child(self.composer.clone())
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
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
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
#[cfg(unix)]
fn format_local_time(unix_ms: u64) -> String {
    #[repr(C)]
    struct CTm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const i8,
    }
    unsafe extern "C" {
        fn localtime_r(time: *const i64, result: *mut CTm) -> *mut CTm;
    }

    let secs = (unix_ms / 1000) as i64;
    let mut tm: CTm = unsafe { std::mem::zeroed() };
    if unsafe { localtime_r(&secs, &mut tm) }.is_null() {
        return String::new();
    }
    twelve_hour(tm.tm_hour, tm.tm_min)
}

#[cfg(not(unix))]
fn format_local_time(unix_ms: u64) -> String {
    // Fallback: UTC clock (no timezone database without extra deps).
    let total_min = (unix_ms / 60_000) % (24 * 60);
    twelve_hour((total_min / 60) as i32, (total_min % 60) as i32)
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
        assert_eq!(relativize("/private/tmp/proj/src/a.rs", cwd, canon), "src/a.rs");
        // Outside the cwd stays absolute.
        assert_eq!(relativize("/other/x.rs", cwd, canon), "/other/x.rs");
        // Already-relative paths are left as-is.
        assert_eq!(relativize("src/b.rs", cwd, canon), "src/b.rs");
    }
}
