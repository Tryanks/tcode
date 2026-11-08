//! A run of computer use: consecutive calls that drive a computer — a
//! computer-use script, a desktop observation or action, a preview browser
//! step — and the thoughts between them, shown as one window, "Used
//! computer · N actions", as Codex groups them. Its header tells the latest
//! step; the latest screenshot stays in view under it; opened, it holds each
//! call's own window, keyed as it would be on its own.

use std::ops::Range;
use std::path::Path;

use agent::{ItemContent, ItemStatus};
use gpui::{
    AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _, Window,
    prelude::FluentBuilder as _,
};
use gpui_base::v_flex;
use tcode_core::session::{EntryContent, TimelineEntry};

use super::window::{self, Argument, Chip, Header, Verb};
use super::{Call, drives_computer};
use crate::chat::timeline::TimelineView;
use crate::chat::timeline::{ActivityRow, AutoActivityRecency};
use crate::icon::IconName;

/// Whether `entry` is a call that drives a computer.
pub(in crate::chat) fn is_computer(entry: &TimelineEntry) -> bool {
    Call::of(entry).is_some_and(|call| drives_computer(&call))
}

fn is_thought(entry: &TimelineEntry) -> bool {
    matches!(
        entry.content,
        EntryContent::Item(ItemContent::Reasoning { .. })
    )
}

/// The windows a run of activities shows as, as ranges of it: one per
/// activity, except that consecutive computer calls — with the thoughts
/// between them, not after the last — share one.
pub(in crate::chat) fn windows(activities: &[&TimelineEntry]) -> Vec<Range<usize>> {
    let mut windows = Vec::new();
    let mut index = 0;
    while index < activities.len() {
        let start = index;
        index += 1;
        if is_computer(activities[start]) {
            let mut probe = index;
            while probe < activities.len() {
                if is_computer(activities[probe]) {
                    probe += 1;
                    index = probe;
                } else if is_thought(activities[probe]) {
                    probe += 1;
                } else {
                    break;
                }
            }
        }
        windows.push(start..index);
    }
    windows
}

/// Whether the window `range` spans is a computer run.
pub(in crate::chat) fn is_run(activities: &[&TimelineEntry], range: &Range<usize>) -> bool {
    is_computer(activities[range.start])
}

/// The run's key in element ids and its open state.
fn key(first: &TimelineEntry) -> String {
    format!("run-{}", first.id)
}

impl TimelineView {
    /// A computer run's window: its header, the latest screenshot, and,
    /// open, each call's own window.
    pub(in crate::chat) fn compose_computer_run(
        &mut self,
        run: &[&TimelineEntry],
        cwd: &Path,
        live_reasoning_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = run[0];
        let key = key(first);
        let open_key = format!("tool-{key}");
        let open = self.expanded.contains(&open_key);
        let calls: Vec<Call<'_>> = run.iter().filter_map(|entry| Call::of(entry)).collect();
        let latest = calls.last().expect("a run holds a call");
        let running = calls
            .iter()
            .any(|call| call.status == ItemStatus::InProgress);
        let failed = calls
            .iter()
            .filter(|call| call.status == ItemStatus::Failed)
            .count();
        let status = if running {
            ItemStatus::InProgress
        } else {
            ItemStatus::Completed
        };
        let latest_activity = super::activity(latest.entry, false, cwd);
        let step = latest_activity.header.label(latest.status);
        let header = Header::new(IconName::Monitor, Verb("use_computer"))
            .argument(Argument::plain(step))
            .chip(Some(Chip::Count(super::counted(
                "tools.actions",
                calls.len(),
            ))))
            .chip((failed > 0).then(|| {
                Chip::Warning(crate::tr!("tools.actions_failed", count = failed).into_owned())
            }));
        let elapsed_ms = match (first.ts, run.last().and_then(|entry| entry.finished_ts)) {
            (Some(start), Some(end)) if !running => Some(end.saturating_sub(start)),
            _ => None,
        };
        // The latest screenshot, the run's evidence, open or shut.
        let shot = calls
            .iter()
            .rev()
            .find(|call| !call.images.is_empty())
            .map(|call| {
                let title = super::activity(call.entry, false, cwd)
                    .header
                    .label(call.status);
                super::super::components::images::item_tile(
                    self.workspace_store.read(cx).image_host(),
                    super::super::components::images::Tile::of(
                        call.entry,
                        call.images.len() - 1,
                        title,
                    ),
                    self.session_key.as_deref().unwrap_or_default(),
                    &self.motion,
                    cx,
                )
            });
        let copy = calls
            .iter()
            .map(|call| {
                super::activity(call.entry, false, cwd)
                    .header
                    .label(call.status)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut copy_items = Vec::new();
        super::copy(&mut copy_items, "chat.copy_text", &copy);
        let detail = match window::fold(&self.motion, &key, open, window, cx) {
            Some(shown) => {
                let rows: Vec<AnyElement> = run
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        self.compose_activity_row(
                            (entry, cwd),
                            ActivityRow {
                                compact: false,
                                live_reasoning: live_reasoning_id == Some(entry.id.as_str()),
                                auto_expand: false,
                                recency: AutoActivityRecency::from_newer_activity_count(
                                    run.len() - index - 1,
                                ),
                                images_inline: true,
                            },
                            window,
                            cx,
                        )
                    })
                    .collect();
                window::Detail::Fold(
                    shown,
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .children(rows)
                        .into_any_element(),
                    run.len() as u64,
                )
            }
            None => window::Detail::Shut,
        };
        let (turn, first_id) = (first.turn, first.id.clone());
        let toggle = window::tool_window(
            window::WindowSpec {
                key: key.clone(),
                header,
                status,
                error: None,
                elapsed_ms,
                compact: false,
                copy: copy_items,
                whole_output: None,
                path: None,
                hover: self.hover.clone(),
                motion: self.motion.clone(),
            },
            detail,
            cx.listener(move |this, _, _, cx| {
                if !this.expanded.remove(&open_key) {
                    this.expanded.insert(open_key.clone());
                }
                this.remeasure_activity(turn, &first_id, cx);
                cx.notify();
            }),
            cx,
        );
        v_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .child(toggle)
            .when_some(shot, |run, shot| {
                // In line with the header's words, past its glyph.
                run.child(gpui_base::h_flex().pl(gpui::px(30.)).child(shot))
            })
            .into_any_element()
    }
}
