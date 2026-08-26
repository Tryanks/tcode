use std::borrow::Cow;
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use agent::{FileChange, ItemContent, ItemStatus, TurnStatus};
use tcode_core::project::{Project, SessionMeta};
use tcode_core::session::{
    EntryContent, SteeringStatus, TimelineEntry, TurnMeta, TurnTiming, parse_orchestrate_callback,
};

pub(crate) type TurnRenderArgs<'a> = (
    usize,
    &'a TurnMeta,
    &'a Path,
    &'a [Arc<TimelineEntry>],
    (Option<&'a str>, Option<&'a str>),
);

/// A chronological block in a turn. File-change entries stay in activity runs
/// for summary counting, but are rendered by the turn-level CHANGED FILES card.
#[derive(Debug)]
pub(crate) enum Segment<'a> {
    ActivityRun(Vec<&'a TimelineEntry>),
    Relay(&'a TimelineEntry),
    ModelChange(&'a TimelineEntry),
    ContextCompacted(&'a TimelineEntry),
    User(&'a TimelineEntry),
    Assistant(&'a TimelineEntry),
    Error(&'a TimelineEntry),
}

#[derive(Debug)]
pub(crate) struct SegmentedEntries<'a> {
    pub(crate) flow: Vec<Segment<'a>>,
    pub(crate) pending_steers: Vec<&'a TimelineEntry>,
}

pub(crate) fn displayed_error_text(content: &EntryContent) -> Cow<'_, str> {
    match content {
        EntryContent::Error { message } => Cow::Borrowed(message),
        EntryContent::ProviderStartError { error } => {
            crate::tr!("errors.provider_start", error = error)
        }
        _ => unreachable!("displayed_error_text requires error timeline content"),
    }
}

pub(crate) type UserContent<'a> = (&'a str, Option<SteeringStatus>, Option<usize>, &'a [String]);

pub(crate) fn user_content(content: &EntryContent) -> Option<UserContent<'_>> {
    match content {
        EntryContent::Item(ItemContent::UserMessage {
            text,
            context_len,
            attachments,
        }) => Some((text, None, *context_len, attachments)),
        EntryContent::Steer {
            text,
            status,
            context_len,
            attachments,
        } => Some((text, Some(*status), *context_len, attachments)),
        _ => None,
    }
}

/// IDs whose message action rows stay visible without hover.
pub(crate) fn latest_message_ids(
    entries: &[Arc<TimelineEntry>],
) -> (Option<String>, Option<String>) {
    let mut last_user_id = None;
    let mut last_assistant_id = None;
    for entry in entries.iter().rev() {
        if last_user_id.is_none()
            && matches!(
                entry.content,
                EntryContent::Item(ItemContent::UserMessage { .. }) | EntryContent::Steer { .. }
            )
        {
            last_user_id = Some(entry.id.clone());
        }
        if last_assistant_id.is_none()
            && matches!(
                entry.content,
                EntryContent::Item(ItemContent::AssistantMessage { .. })
            )
        {
            last_assistant_id = Some(entry.id.clone());
        }
        if last_user_id.is_some() && last_assistant_id.is_some() {
            break;
        }
    }
    (last_user_id, last_assistant_id)
}

/// Coalesce only adjacent activity entries, leaving messages and errors at
/// their exact positions in the timeline.
pub(crate) fn segment_entries<'a>(
    entries: &'a [Arc<TimelineEntry>],
    turn_running: bool,
) -> SegmentedEntries<'a> {
    let mut segments = Vec::new();
    let mut activities = Vec::new();
    let mut pending_steers = Vec::new();
    let flush_activities = |segments: &mut Vec<Segment<'a>>,
                            activities: &mut Vec<&'a TimelineEntry>| {
        if !activities.is_empty() {
            segments.push(Segment::ActivityRun(std::mem::take(activities)));
        }
    };

    for entry in entries {
        let entry = entry.as_ref();
        if turn_running
            && matches!(
                entry.content,
                EntryContent::Steer {
                    status: SteeringStatus::Pending,
                    ..
                }
            )
        {
            pending_steers.push(entry);
            continue;
        }
        match &entry.content {
            EntryContent::Item(ItemContent::CommandExecution { .. })
            | EntryContent::Item(ItemContent::ToolCall { .. })
            | EntryContent::Item(ItemContent::Subagent { .. })
            | EntryContent::Item(ItemContent::WebSearch { .. })
            | EntryContent::Item(ItemContent::Other { .. })
            | EntryContent::Item(ItemContent::FileChange { .. }) => activities.push(entry),
            EntryContent::Item(ItemContent::Reasoning { .. }) => {
                if activities.last().is_some_and(|previous| {
                    matches!(
                        &previous.content,
                        EntryContent::Item(ItemContent::Reasoning { text })
                            if text.trim().is_empty()
                    )
                }) {
                    activities.pop();
                }
                activities.push(entry);
            }
            EntryContent::Item(ItemContent::UserMessage { .. }) | EntryContent::Steer { .. } => {
                flush_activities(&mut segments, &mut activities);
                segments.push(Segment::User(entry));
            }
            EntryContent::ProviderRelay { .. } => {
                flush_activities(&mut segments, &mut activities);
                segments.push(Segment::Relay(entry));
            }
            EntryContent::ModelChanged { .. } => {
                flush_activities(&mut segments, &mut activities);
                segments.push(Segment::ModelChange(entry));
            }
            EntryContent::ContextCompacted => {
                flush_activities(&mut segments, &mut activities);
                segments.push(Segment::ContextCompacted(entry));
            }
            EntryContent::Item(ItemContent::AssistantMessage { .. }) => {
                flush_activities(&mut segments, &mut activities);
                segments.push(Segment::Assistant(entry));
            }
            EntryContent::Error { .. } | EntryContent::ProviderStartError { .. } => {
                flush_activities(&mut segments, &mut activities);
                segments.push(Segment::Error(entry));
            }
        }
    }
    flush_activities(&mut segments, &mut activities);
    SegmentedEntries {
        flow: segments,
        pending_steers,
    }
}

/// Which segment, if any, is the turn's *live* work log — the one that opens on
/// its own while the turn is running.
///
/// A running turn whose last segment is prose has none: liveness is carried by
/// the turn-level working indicator standing bare after every segment, so every
/// run is already settled. Nothing is appended to host the indicator.
pub(crate) fn live_activity_segment(segments: &[Segment<'_>], turn_running: bool) -> Option<usize> {
    let last = segments
        .iter()
        .rposition(|segment| matches!(segment, Segment::ActivityRun(_)))?;
    (!turn_running || last + 1 == segments.len()).then_some(last)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkLogCounts {
    pub(crate) commands: usize,
    pub(crate) files: usize,
    pub(crate) tools: usize,
    pub(crate) subagents: usize,
}

pub(crate) fn work_log_counts(entries: &[&TimelineEntry]) -> WorkLogCounts {
    let mut counts = WorkLogCounts::default();
    let mut files = HashSet::new();

    for entry in entries {
        match &entry.content {
            EntryContent::Item(ItemContent::CommandExecution { .. }) => counts.commands += 1,
            EntryContent::Item(ItemContent::FileChange { changes, .. }) => {
                files.extend(changes.iter().map(|change| change.path.as_str()));
            }
            EntryContent::Item(ItemContent::ToolCall { .. })
            | EntryContent::Item(ItemContent::WebSearch { .. })
            | EntryContent::Item(ItemContent::Other { .. }) => counts.tools += 1,
            EntryContent::Item(ItemContent::Subagent { .. }) => counts.subagents += 1,
            EntryContent::ContextCompacted
            | EntryContent::Steer { .. }
            | EntryContent::Item(ItemContent::UserMessage { .. })
            | EntryContent::Item(ItemContent::AssistantMessage { .. })
            | EntryContent::Item(ItemContent::Reasoning { .. })
            | EntryContent::Error { .. }
            | EntryContent::ProviderStartError { .. }
            | EntryContent::ProviderRelay { .. }
            | EntryContent::ModelChanged { .. } => {}
        }
    }
    counts.files = files.len();
    counts
}

pub(crate) fn work_log_capsule_label(counts: &WorkLogCounts, activity_count: usize) -> String {
    let mut clauses = Vec::new();
    if counts.tools > 0 {
        clauses.push(if counts.tools == 1 {
            crate::tr!("chat.work_log_tool_one").into_owned()
        } else {
            crate::tr!("chat.work_log_tools", count = counts.tools).into_owned()
        });
    }
    if counts.files > 0 {
        clauses.push(if counts.files == 1 {
            crate::tr!("chat.work_log_edit_one").into_owned()
        } else {
            crate::tr!("chat.work_log_edits", count = counts.files).into_owned()
        });
    }
    if counts.commands > 0 {
        clauses.push(if counts.commands == 1 {
            crate::tr!("chat.work_log_command_one").into_owned()
        } else {
            crate::tr!("chat.work_log_commands", count = counts.commands).into_owned()
        });
    }
    if clauses.is_empty() && activity_count > 0 {
        clauses.push(if activity_count == 1 {
            crate::tr!("chat.work_log_activity_one").into_owned()
        } else {
            crate::tr!("chat.work_log_activities", count = activity_count).into_owned()
        });
    }
    clauses.join(" · ")
}

pub(crate) fn activity_run_duration_ms(
    activities: &[&TimelineEntry],
    turn: &TurnMeta,
    is_last: bool,
) -> u64 {
    let first = activities.iter().find_map(|entry| entry.ts);
    let last = activities.iter().rev().find_map(|entry| entry.ts);
    match (first, last) {
        (Some(first), Some(last)) => last.saturating_sub(first),
        _ if is_last => turn.timing.map_or(0, |timing| timing.total_ms),
        _ => 0,
    }
}

// A failed command inside a run is normal agent probing (grep exit 1, a
// retried build); it never fails the run. Only the turn's own terminal
// status colors the snapshot, and only on the segment that carries it.
pub(crate) fn work_log_outcome(
    turn: &TurnMeta,
    _activities: &[&TimelineEntry],
    is_last: bool,
) -> TurnStatus {
    if is_last {
        turn.status.unwrap_or(TurnStatus::Completed)
    } else {
        TurnStatus::Completed
    }
}

/// `text` collapsed to a single spaced line: every whitespace run (newlines
/// included) becomes one space, so a multi-line command shows its full content
/// in a one-line preview instead of just its first line. Clipped to more
/// characters than any row can render; the visual ellipsis comes from the
/// row's `text_ellipsis`.
pub(crate) fn one_line(text: &str) -> String {
    const MAX_CHARS: usize = 600;
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_CHARS)
        .collect()
}

/// Like [`one_line`], but line breaks stay visible: each break between
/// non-empty lines becomes a literal `\n` marker in the output, and the
/// returned byte ranges let the row paint those markers fainter than the
/// text around them so they read as break symbols, not command content.
pub(crate) fn one_line_with_break_markers(text: &str) -> (String, Vec<Range<usize>>) {
    const MAX_CHARS: usize = 600;
    let mut out = String::new();
    let mut markers = Vec::new();
    let mut chars = 0usize;
    for line in text.lines() {
        let mut on_new_line = !out.is_empty();
        for word in line.split_whitespace() {
            if on_new_line {
                let start = out.len();
                out.push_str("\\n");
                markers.push(start..out.len());
                chars += 2;
                on_new_line = false;
            } else if !out.is_empty() {
                out.push(' ');
                chars += 1;
            }
            out.push_str(word);
            chars += word.chars().count();
            if chars >= MAX_CHARS {
                return (out, markers);
            }
        }
    }
    (out, markers)
}

/// A short one-line summary of a tool call's input for the Work Log.
pub(crate) fn tool_brief(input: &serde_json::Value) -> String {
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
pub(crate) fn format_duration(secs: u64) -> String {
    if secs >= 60 {
        crate::tr!(
            "time.duration_minutes",
            minutes = secs / 60,
            seconds = format!("{:02}", secs % 60)
        )
        .into_owned()
    } else {
        crate::tr!("time.duration_seconds", seconds = secs).into_owned()
    }
}

pub(crate) fn format_elapsed_deciseconds(elapsed_ms: u64) -> String {
    let deciseconds = elapsed_ms / 100;
    let seconds = deciseconds / 10;
    let tenth = deciseconds % 10;
    if seconds < 60 {
        format!("{seconds}.{tenth}s")
    } else {
        format!("{}m {}.{tenth}s", seconds / 60, seconds % 60)
    }
}

pub(crate) fn format_compact_span(secs: u64) -> String {
    if secs >= 3600 {
        format!(
            "{}h{:02}m{:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// A finished turn's duration. Long turns roll up into hours rather than
/// growing an unreadable minute count (a full day reads `24h 00m 59s`, not
/// `1440m 59s`), keeping the seconds because this row reports actual elapsed
/// time. The live "Working for" indicator keeps [`format_duration`].
pub(crate) fn format_span(secs: u64) -> String {
    if secs >= 3600 {
        crate::tr!(
            "time.duration_hours",
            hours = secs / 3600,
            minutes = format!("{:02}", (secs % 3600) / 60),
            seconds = format!("{:02}", secs % 60)
        )
        .into_owned()
    } else {
        format_duration(secs)
    }
}

pub(crate) fn divergent_served_model<'a>(
    served_model: Option<&'a str>,
    requested_model: Option<&str>,
) -> Option<&'a str> {
    match (served_model, requested_model) {
        (Some(served), Some(requested)) if served != requested => Some(served),
        _ => None,
    }
}

pub(crate) fn format_cost_usd(cost: f64) -> String {
    if cost.abs() >= 0.01 {
        return format!("${cost:.2}");
    }

    let decimals = if (cost * 1_000.).round() != 0. { 3 } else { 4 };
    let formatted = format!("{cost:.decimals$}");
    let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
    if trimmed == "0" || trimmed == "-0" {
        "$0.0000".to_string()
    } else {
        format!("${trimmed}")
    }
}

/// The quiet, visible portion of a finished turn's timing row. Bucket details
/// intentionally live in [`turn_time_breakdown`] instead of competing with the
/// completion clock, total duration, and cost.
pub(crate) fn turn_time_parts(clock: String, timing: Option<TurnTiming>) -> Vec<String> {
    let mut parts = vec![clock];
    if let Some(timing) = timing {
        parts.push(format_compact_span(timing.total_ms / 1000));
    }
    parts
}

pub(crate) fn turn_time_breakdown(timing: Option<TurnTiming>) -> Option<String> {
    timing.map(|timing| {
        let total = timing.total_ms / 1000;
        let tools = (timing.tool_ms / 1000).min(total);
        let ai = total - tools;
        [
            crate::tr!("chat.turn_ai", duration = format_span(ai)).into_owned(),
            crate::tr!("chat.turn_tools", duration = format_span(tools)).into_owned(),
        ]
        .join(" · ")
    })
}

#[derive(Clone)]
pub(crate) struct TurnTimeClause {
    pub(crate) text: String,
    pub(crate) selector: &'static str,
    pub(crate) warning: bool,
}

pub(crate) fn turn_time_clauses(
    clock: String,
    timing: Option<TurnTiming>,
    cost_usd: Option<f64>,
    served_model: Option<&str>,
    requested_model: Option<&str>,
) -> Vec<TurnTimeClause> {
    const TIMING_SELECTORS: [&str; 2] = ["turn-time-clock", "turn-time-total"];

    let mut clauses = turn_time_parts(clock, timing)
        .into_iter()
        .zip(TIMING_SELECTORS)
        .map(|(text, selector)| TurnTimeClause {
            text,
            selector,
            warning: false,
        })
        .collect::<Vec<_>>();
    if let Some(cost) = cost_usd {
        clauses.push(TurnTimeClause {
            text: format_cost_usd(cost),
            selector: "turn-time-cost",
            warning: false,
        });
    }
    if let Some(served) = divergent_served_model(served_model, requested_model) {
        clauses.push(TurnTimeClause {
            text: format!("⚠ {served}"),
            selector: "turn-time-model",
            warning: true,
        });
    }
    clauses
}

/// Count added / removed lines in a unified diff (ignoring the `+++`/`---`
/// file headers).
pub(crate) fn diff_stats(diff: Option<&str>) -> (u32, u32) {
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

/// One live Work Log row for an edited file: the workspace-relative display
/// path plus `+added` / `-deleted` counts. The counts are `None` when the entry
/// carries no diff — "+0 -0" would read as "this edit changed nothing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveEditRow {
    pub(crate) path: String,
    pub(crate) counts: Option<(u32, u32)>,
}

/// The `+N` / `-N` a live edit row should display, if any.
///
/// A diff that carries no added or removed lines — absent, empty,
/// whitespace-only, or nothing but `+++`/`---` headers — has nothing truthful
/// to show: "+0 -0" reads as "this edit changed nothing". The finished-turn
/// CHANGED FILES card keeps its own `diff_stats` totals unchanged.
pub(crate) fn live_edit_counts(diff: Option<&str>) -> Option<(u32, u32)> {
    let (added, deleted) = diff_stats(Some(diff?));
    (added != 0 || deleted != 0).then_some((added, deleted))
}

/// Expand a file-change snapshot into one live row per file, so a single
/// multi-file entry names every file instead of collapsing to an "N files"
/// label. Paths use the same workspace-relative display as CHANGED FILES.
pub(crate) fn live_edit_rows(changes: &[FileChange], cwd: &Path) -> Vec<LiveEditRow> {
    changes
        .iter()
        .map(|change| LiveEditRow {
            path: tcode_services::user_files::relativize_to_workspace(&change.path, cwd),
            counts: live_edit_counts(change.diff.as_deref()),
        })
        .collect()
}

/// Maximum number of entries kept directly visible at the tail of a live
/// activity run. Older entries move into a separate collapsed Work Log; once
/// prose ends the run, the full run becomes that settled Work Log instead.
pub(crate) const LIVE_ACTIVITY_WINDOW: usize = 5;

pub(crate) fn partition_activity_run<'a>(
    activities: &'a [&'a TimelineEntry],
    live: bool,
) -> (&'a [&'a TimelineEntry], &'a [&'a TimelineEntry]) {
    let visible = if live {
        activities.len().min(LIVE_ACTIVITY_WINDOW)
    } else {
        0
    };
    activities.split_at(activities.len() - visible)
}

/// Format a unix-ms timestamp as a local 12-hour clock, e.g. "2:39 AM".
pub(crate) fn format_local_time(unix_ms: u64) -> String {
    use chrono::{Local, TimeZone as _};

    Local
        .timestamp_millis_opt(unix_ms as i64)
        .single()
        .map(|time| time.format("%-I:%M %p").to_string())
        .unwrap_or_default()
}

const START_HUB_PROJECT_LIMIT: usize = 6;

/// Projects shown by the empty-chat start hub, ordered by latest unarchived
/// thread activity. Projects without threads follow alphabetically.
pub(crate) fn start_hub_projects(
    projects: &[Project],
    sessions: &[SessionMeta],
) -> Vec<(Project, Option<u64>)> {
    let mut projects: Vec<(Project, Option<u64>)> = projects
        .iter()
        .cloned()
        .map(|project| {
            let last_activity = sessions
                .iter()
                .filter(|session| session.archived_at.is_none())
                .filter(|session| session.project_id.as_deref() == Some(project.id.as_str()))
                .map(|session| session.updated_at)
                .max();
            (project, last_activity)
        })
        .collect();
    projects.sort_by(|(project_a, activity_a), (project_b, activity_b)| {
        match (activity_a, activity_b) {
            (Some(activity_a), Some(activity_b)) => activity_b.cmp(activity_a).then_with(|| {
                project_a
                    .name
                    .to_lowercase()
                    .cmp(&project_b.name.to_lowercase())
            }),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => project_a
                .name
                .to_lowercase()
                .cmp(&project_b.name.to_lowercase()),
        }
    });
    projects.truncate(START_HUB_PROJECT_LIMIT);
    projects
}

/// Pre-measure this many full-window heights on each side of the chat viewport.
///
/// GPUI's list performs the expensive first layout for items in this band, so a
/// generous buffer keeps ordinary trackpad/wheel scrolling from discovering and
/// laying out a turn on the same frame in which it becomes visible. The chat
/// viewport is shorter than the full window, making this a conservative lower
/// bound in practice while the list itself remains bounded for huge histories.
const TIMELINE_OVERDRAW_VIEWPORTS: f32 = 4.;
const TIMELINE_MIN_OVERDRAW: f32 = 3072.;

pub(crate) fn timeline_overdraw(viewport_height: f32) -> f32 {
    (viewport_height.max(0.) * TIMELINE_OVERDRAW_VIEWPORTS).max(TIMELINE_MIN_OVERDRAW)
}

/// How to bring a mirrored [`MdState`] in line with the timeline's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MdSync {
    /// Already in sync.
    Noop,
    /// The text grew by an append.
    Push(String),
    /// The text changed in a way an append cannot express.
    Reset,
}

/// The pure delta/reset decision behind [`MdState::sync`].
pub(crate) fn md_sync(synced: &str, text: &str) -> MdSync {
    if synced == text {
        return MdSync::Noop;
    }
    match text.strip_prefix(synced) {
        Some(delta) if !delta.is_empty() => MdSync::Push(delta.to_string()),
        _ => MdSync::Reset,
    }
}

/// Return the part of a user entry that belongs in its message bubble.
pub(crate) fn user_visible_text(text: &str, context_len: Option<usize>) -> &str {
    context_len
        .filter(|len| *len <= text.len() && text.is_char_boundary(*len))
        .map_or(text, |len| &text[len..])
}

/// Encode plain text as markdown whose rendered text is still literal input.
pub(crate) fn plain_text_as_markdown(text: &str) -> String {
    let mut markdown = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut at_line_start = true;
    let mut line_is_empty = true;

    while let Some(ch) = chars.next() {
        if ch == '\n' {
            let mut newline_count = 1;
            while chars.next_if_eq(&'\n').is_some() {
                newline_count += 1;
            }

            if newline_count == 1 && !line_is_empty && chars.peek().is_some() {
                // gpui-component currently drops markdown Break nodes while
                // building paragraph text. An inline HTML break is converted
                // to an InlineNode containing "\n", so both display and mouse
                // selection preserve the original newline.
                markdown.push_str("<br>");
            } else {
                markdown.extend(std::iter::repeat_n('\n', newline_count));
            }
            at_line_start = true;
            line_is_empty = true;
            continue;
        }

        line_is_empty = false;
        if at_line_start {
            match ch {
                ' ' => {
                    markdown.push_str("&#32;");
                    continue;
                }
                '\t' => {
                    markdown.push_str("&#9;");
                    continue;
                }
                _ => at_line_start = false,
            }
        }

        if ch.is_ascii_punctuation() {
            markdown.push('\\');
        }
        markdown.push(ch);
    }

    markdown
}

/// Cached indexing and cheap height-affecting identity for one virtualized turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnListItem {
    pub(crate) entry_range: Range<usize>,
    pub(crate) entry_count: usize,
    pub(crate) identity: u64,
    pub(crate) content: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnIndexMeta {
    start_ts: Option<u64>,
    end_ts: Option<u64>,
    running: bool,
    timing: Option<tcode_core::session::TurnTiming>,
    served_model: Option<String>,
    cost_usd: Option<u64>,
    status: Option<TurnStatus>,
}

impl From<&TurnMeta> for TurnIndexMeta {
    fn from(turn: &TurnMeta) -> Self {
        Self {
            start_ts: turn.start_ts,
            end_ts: turn.end_ts,
            running: turn.running,
            timing: turn.timing,
            served_model: turn.served_model.clone(),
            cost_usd: turn.cost_usd.map(f64::to_bits),
            status: turn.status,
        }
    }
}

impl TurnIndexMeta {
    fn matches(&self, turn: &TurnMeta) -> bool {
        self.start_ts == turn.start_ts
            && self.end_ts == turn.end_ts
            && self.running == turn.running
            && self.timing == turn.timing
            && self.served_model.as_deref() == turn.served_model.as_deref()
            && self.cost_usd == turn.cost_usd.map(f64::to_bits)
            && self.status == turn.status
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProposedPlanIndex {
    turn: usize,
    item_id: String,
    markdown: String,
}

/// Stateful input snapshot for reusing indexed turns across store notifications.
#[derive(Debug, Default)]
pub(crate) struct TurnIndexCache {
    entries: Vec<Arc<TimelineEntry>>,
    turns: Vec<TurnIndexMeta>,
    proposed_plan: Option<ProposedPlanIndex>,
    expanded: HashSet<String>,
    #[cfg(test)]
    reindexed_turns: usize,
}

impl TurnIndexCache {
    pub(crate) fn sync(
        &mut self,
        items: &mut Vec<TurnListItem>,
        turns: &[TurnMeta],
        entries: &[Arc<TimelineEntry>],
        proposed_plan: Option<(usize, &str, &str)>,
        expanded: &HashSet<String>,
        reset: bool,
    ) -> ListSync {
        let item_count = turns
            .len()
            .max(entries.last().map_or(0, |entry| entry.turn + 1));
        let entry_divergence = self
            .entries
            .iter()
            .zip(entries)
            .position(|(old, new)| !Arc::ptr_eq(old, new))
            .unwrap_or(self.entries.len().min(entries.len()));
        let tail_replace = entries.len() == self.entries.len()
            && entry_divergence.checked_add(1) == Some(entries.len());
        let append = entry_divergence == self.entries.len() && entries.len() >= self.entries.len();
        let proposed_plan_changed = match (&self.proposed_plan, proposed_plan) {
            (None, None) => false,
            (Some(old), Some((turn, item_id, markdown))) => {
                old.turn != turn || old.item_id != item_id || old.markdown != markdown
            }
            _ => true,
        };
        let settings_changed = proposed_plan_changed || self.expanded != *expanded;
        let must_reset = reset
            || settings_changed
            || entries.len() < self.entries.len()
            || (!append && !tail_replace)
            || turns.len() < self.turns.len();

        let turn_divergence = self
            .turns
            .iter()
            .zip(turns)
            .position(|(old, new)| !old.matches(new))
            .unwrap_or(self.turns.len().min(turns.len()));
        let mut reindex_from = if must_reset { 0 } else { item_count };
        if !must_reset {
            if entry_divergence < entries.len() {
                reindex_from = reindex_from.min(entries[entry_divergence].turn);
            }
            if entry_divergence < self.entries.len() {
                reindex_from = reindex_from.min(self.entries[entry_divergence].turn);
            }
            if turn_divergence < turns.len() {
                reindex_from = reindex_from.min(turn_divergence);
            }
        }

        let suffix = if reindex_from == 0 {
            index_turns(turns, entries, proposed_plan, expanded)
        } else {
            if reindex_from < item_count {
                index_turns_from(turns, entries, proposed_plan, expanded, reindex_from)
            } else {
                Vec::new()
            }
        };
        let sync = list_sync_with(items, item_count, reset, |index| {
            if index < reindex_from {
                &items[index]
            } else {
                &suffix[index - reindex_from]
            }
        });
        items.truncate(reindex_from);
        items.extend(suffix);
        items.truncate(item_count);

        #[cfg(test)]
        {
            self.reindexed_turns = item_count.saturating_sub(reindex_from);
        }
        self.entries.truncate(entry_divergence);
        self.entries
            .extend(entries[entry_divergence..].iter().cloned());
        self.turns.truncate(turn_divergence);
        self.turns
            .extend(turns[turn_divergence..].iter().map(TurnIndexMeta::from));
        if proposed_plan_changed {
            self.proposed_plan = proposed_plan.map(|(turn, item_id, markdown)| ProposedPlanIndex {
                turn,
                item_id: item_id.to_owned(),
                markdown: markdown.to_owned(),
            });
        }
        if self.expanded != *expanded {
            self.expanded.clone_from(expanded);
        }
        sync
    }

    #[cfg(test)]
    fn reindexed_turns(&self) -> usize {
        self.reindexed_turns
    }
}

/// Mutation to apply to the persistent [`ListState`] after a timeline sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListSync {
    None,
    Reset {
        count: usize,
    },
    Incremental {
        append: Option<Range<usize>>,
        remeasure: Vec<usize>,
    },
}

/// Build contiguous entry ranges and fingerprints for turn-level list items.
///
/// Timeline entries are chronological, so all entries for a turn are adjacent.
/// The max entry turn keeps a temporary orphan bucket renderable if a provider
/// ever exposes an entry before its corresponding `TurnMeta`.
pub(crate) fn index_turns(
    turns: &[TurnMeta],
    entries: &[Arc<TimelineEntry>],
    proposed_plan: Option<(usize, &str, &str)>,
    expanded: &HashSet<String>,
) -> Vec<TurnListItem> {
    index_turns_from(turns, entries, proposed_plan, expanded, 0)
}

fn index_turns_from(
    turns: &[TurnMeta],
    entries: &[Arc<TimelineEntry>],
    proposed_plan: Option<(usize, &str, &str)>,
    expanded: &HashSet<String>,
    first_turn: usize,
) -> Vec<TurnListItem> {
    debug_assert!(entries.windows(2).all(|pair| pair[0].turn <= pair[1].turn));

    let item_count = turns
        .len()
        .max(entries.last().map_or(0, |entry| entry.turn + 1));
    let first_entry = entries.partition_point(|entry| entry.turn < first_turn);
    let mut ranges = vec![entries.len()..entries.len(); item_count.saturating_sub(first_turn)];
    for (index, entry) in entries.iter().enumerate().skip(first_entry) {
        let range = &mut ranges[entry.turn - first_turn];
        if range.start == entries.len() {
            range.start = index;
        }
        range.end = index + 1;
    }

    ranges
        .into_iter()
        .enumerate()
        .map(|(offset, entry_range)| {
            let index = first_turn + offset;
            let mut identity = DefaultHasher::new();
            let mut content = DefaultHasher::new();
            for entry in &entries[entry_range.clone()] {
                entry.id.hash(&mut identity);
                std::mem::discriminant(&entry.content).hash(&mut content);
                entry.ts.hash(&mut content);
                hash_entry_shape(&entry.content, &mut content);
                // A disclosure row (orchestrate context / callback) grows a tall
                // scroll card when expanded, so its toggle state must change the
                // turn fingerprint or the list keeps the collapsed measurement.
                if let Some(key) = disclosure_key(&entry.content, &entry.id) {
                    expanded.contains(&key).hash(&mut content);
                }
            }
            if let Some(turn) = turns.get(index) {
                turn.start_ts.hash(&mut content);
                turn.end_ts.hash(&mut content);
                turn.running.hash(&mut content);
                // The finished bottom row renders the turn's breakdown.
                turn.timing.hash(&mut content);
                turn.served_model.hash(&mut content);
                turn.cost_usd.map(f64::to_bits).hash(&mut content);
                turn.status
                    .as_ref()
                    .map(std::mem::discriminant)
                    .hash(&mut content);
            }
            if let Some((turn, item_id, markdown)) = proposed_plan
                && turn == index
            {
                item_id.hash(&mut identity);
                markdown.len().hash(&mut content);
            }
            TurnListItem {
                entry_count: entry_range.len(),
                entry_range,
                identity: identity.finish(),
                content: content.finish(),
            }
        })
        .collect()
}

/// The per-entry expansion key for a user message that renders as a disclosure
/// row rather than a bubble: an orchestrate context split (annotated with a
/// `context_len`) or a child-thread callback (whose text parses as one). `None`
/// for an ordinary user message, which stays a plain bubble.
fn disclosure_key(content: &EntryContent, entry_id: &str) -> Option<String> {
    let (text, _, context_len, _) = user_content(content)?;
    if context_len.is_some() {
        Some(format!("orchestrate-context-{entry_id}"))
    } else if parse_orchestrate_callback(text).is_some() {
        Some(format!("orchestrate-callback-{entry_id}"))
    } else {
        None
    }
}

/// Hash only data that can alter a turn's layout. Text lengths make streaming
/// updates O(number of entries) without repeatedly hashing growing markdown.
fn hash_entry_shape(content: &EntryContent, hash: &mut DefaultHasher) {
    if let EntryContent::Item(item) = content {
        std::mem::discriminant(item).hash(hash);
    }
    match content {
        EntryContent::Item(ItemContent::UserMessage {
            text,
            context_len,
            attachments,
        }) => {
            attachments.len().hash(hash);
            text.len().hash(hash);
            Option::<SteeringStatus>::None.hash(hash);
            context_len.hash(hash);
        }
        EntryContent::Steer {
            text,
            status,
            context_len,
            attachments,
        } => {
            attachments.len().hash(hash);
            text.len().hash(hash);
            status.hash(hash);
            context_len.hash(hash);
        }
        EntryContent::Item(ItemContent::AssistantMessage { text })
        | EntryContent::Item(ItemContent::Reasoning { text }) => {
            text.len().hash(hash);
        }
        EntryContent::Item(ItemContent::CommandExecution {
            command,
            output,
            exit_code,
            status,
        }) => {
            command.len().hash(hash);
            output.len().hash(hash);
            exit_code.hash(hash);
            std::mem::discriminant(status).hash(hash);
        }
        EntryContent::Item(ItemContent::FileChange { changes, .. }) => {
            changes.len().hash(hash);
            for change in changes {
                change.path.len().hash(hash);
                change.diff.as_ref().map(String::len).hash(hash);
            }
        }
        EntryContent::Item(ItemContent::ToolCall {
            name,
            input,
            output,
            status,
        }) => {
            name.len().hash(hash);
            input.to_string().len().hash(hash);
            output.as_ref().map(String::len).hash(hash);
            std::mem::discriminant(status).hash(hash);
        }
        EntryContent::Item(ItemContent::Subagent {
            agent_type,
            description,
            status,
            summary,
        }) => {
            agent_type.len().hash(hash);
            description.len().hash(hash);
            std::mem::discriminant(status).hash(hash);
            summary.as_ref().map(String::len).hash(hash);
        }
        EntryContent::Error { message } => message.len().hash(hash),
        EntryContent::ProviderStartError { error } => error.len().hash(hash),
        EntryContent::ProviderRelay {
            from_provider,
            to_provider,
            ..
        } => {
            from_provider.hash(hash);
            to_provider.hash(hash);
        }
        EntryContent::ModelChanged { from, to, reason } => {
            from.hash(hash);
            to.hash(hash);
            reason.hash(hash);
        }
        EntryContent::ContextCompacted => {}
        EntryContent::Item(ItemContent::WebSearch { query }) => {
            "web_search".len().hash(hash);
            serde_json::json!({ "query": query })
                .to_string()
                .len()
                .hash(hash);
            Option::<usize>::None.hash(hash);
            std::mem::discriminant(&ItemStatus::Completed).hash(hash);
        }
        EntryContent::Item(ItemContent::Other {
            provider_kind,
            summary,
        }) => {
            provider_kind.len().hash(hash);
            serde_json::json!({ "summary": summary })
                .to_string()
                .len()
                .hash(hash);
            Option::<usize>::None.hash(hash);
            std::mem::discriminant(&ItemStatus::Completed).hash(hash);
        }
    }
}

#[cfg(test)]
pub(crate) fn list_sync(
    old: &[TurnListItem],
    new: &[TurnListItem],
    session_changed: bool,
) -> ListSync {
    list_sync_with(old, new.len(), session_changed, |index| &new[index])
}

fn list_sync_with<'a>(
    old: &[TurnListItem],
    new_len: usize,
    session_changed: bool,
    new_at: impl Fn(usize) -> &'a TurnListItem,
) -> ListSync {
    let common = old.len().min(new_len);
    let replaced = (0..common).any(|index| {
        let old = &old[index];
        let new = new_at(index);
        new.entry_count < old.entry_count
            || (new.entry_count == old.entry_count && new.identity != old.identity)
    });
    if session_changed || new_len < old.len() || replaced {
        return ListSync::Reset { count: new_len };
    }

    let append = (new_len > old.len()).then_some(old.len()..new_len);
    let mut remeasure = (0..common)
        .filter(|&index| {
            old[index].entry_count != new_at(index).entry_count
                || old[index].content != new_at(index).content
        })
        .collect::<Vec<_>>();
    // The former last item gains an inter-turn gap when a new turn appears.
    if append.is_some() && !old.is_empty() && !remeasure.contains(&(old.len() - 1)) {
        remeasure.push(old.len() - 1);
    }

    if append.is_none() && remeasure.is_empty() {
        ListSync::None
    } else {
        ListSync::Incremental { append, remeasure }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{FileChange, FileChangeKind, ItemContent, ItemStatus, ProviderKind};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tcode_core::project::{Project, SessionMeta};
    use tcode_core::session::{EntryContent, SteeringStatus, TimelineEntry, TurnMeta, TurnTiming};

    const REAL_DIFF: &str = "--- a/src/foo.rs\n\
                             +++ b/src/foo.rs\n\
                             @@ -1,3 +1,4 @@\n\
                             \x20context\n\
                             +added one\n\
                             +added two\n\
                             -removed one\n";

    #[test]
    fn start_hub_orders_active_projects_then_alphabetical_empty_projects_and_caps_at_six() {
        let project = |id: &str, name: &str| Project {
            id: id.into(),
            name: name.into(),
            root: PathBuf::from(format!("/{id}")),
            created_at: 0,
        };
        let projects = vec![
            project("archived", "Archived only"),
            project("alpha", "Alpha"),
            project("recent", "Recent"),
            project("older", "Older"),
            project("middle", "Middle"),
            project("extra", "Extra"),
            project("zulu", "Zulu"),
        ];
        let session = |id: &str, project_id: &str, updated_at: u64, archived: bool| {
            let mut session =
                SessionMeta::new(ProviderKind::Codex, PathBuf::from("/project"), None);
            session.id = id.into();
            session.project_id = Some(project_id.into());
            session.updated_at = updated_at;
            session.archived_at = archived.then_some(updated_at);
            session
        };
        let sessions = vec![
            session("recent-thread", "recent", 50, false),
            session("middle-thread", "middle", 30, false),
            session("older-thread", "older", 20, false),
            session("extra-thread", "extra", 10, false),
            session("archived-thread", "archived", 100, true),
        ];

        let ordered = start_hub_projects(&projects, &sessions);
        let ids: Vec<&str> = ordered
            .iter()
            .map(|(project, _)| project.id.as_str())
            .collect();

        assert_eq!(
            ids,
            vec!["recent", "middle", "older", "extra", "alpha", "archived"]
        );
        assert_eq!(ordered[0].1, Some(50));
        assert_eq!(ordered[5].1, None);
    }

    fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
        Arc::new(TimelineEntry {
            id: id.to_string(),
            content,
            ts: None,
            turn: 0,
        })
    }

    fn user_item(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::UserMessage {
            text: text.into(),
            context_len: None,
            attachments: Vec::new(),
        })
    }

    fn assistant(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::AssistantMessage { text: text.into() })
    }

    fn reasoning(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::Reasoning { text: text.into() })
    }

    #[test]
    fn latest_message_ids_find_the_newest_user_and_assistant_entries() {
        let entries = vec![
            entry("user-old", user_item("old")),
            entry("assistant-old", assistant("old")),
            command("command"),
            entry(
                "steer-new",
                EntryContent::Steer {
                    text: "new".into(),
                    status: SteeringStatus::Pending,
                    context_len: None,
                    attachments: Vec::new(),
                },
            ),
            entry("assistant-new", assistant("new")),
            entry("reasoning", reasoning("later but not a message")),
        ];

        assert_eq!(
            latest_message_ids(&entries),
            (Some("steer-new".into()), Some("assistant-new".into()))
        );
    }

    #[test]
    fn provider_start_error_is_localized_only_at_render_boundary() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let generic = EntryContent::Error {
            message: "generic\0原样".into(),
        };
        let provider_start = EntryContent::ProviderStartError {
            error: "spawn failed".into(),
        };

        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(
            displayed_error_text(&generic).as_bytes(),
            b"generic\0\xe5\x8e\x9f\xe6\xa0\xb7"
        );
        assert_eq!(
            displayed_error_text(&provider_start),
            "Failed to start provider: spawn failed"
        );

        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            displayed_error_text(&generic).as_bytes(),
            b"generic\0\xe5\x8e\x9f\xe6\xa0\xb7"
        );
        assert_eq!(
            displayed_error_text(&provider_start),
            "启动提供商失败：spawn failed"
        );
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }

    #[test]
    fn timeline_overdraw_keeps_multiple_viewports_warm() {
        // Headless/early construction still gets a useful buffer.
        assert_eq!(timeline_overdraw(0.), 3072.);
        // Normal windows retain four full window heights on both sides.
        assert_eq!(timeline_overdraw(900.), 3600.);
        assert_eq!(timeline_overdraw(1440.), 5760.);
    }

    fn command(id: &str) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::Item(ItemContent::CommandExecution {
                command: id.to_string(),
                output: String::new(),
                exit_code: Some(0),
                status: ItemStatus::Completed,
            }),
        )
    }

    fn at_turn(mut entry: Arc<TimelineEntry>, turn: usize) -> Arc<TimelineEntry> {
        Arc::make_mut(&mut entry).turn = turn;
        entry
    }

    #[test]
    fn turn_list_index_and_sync_cover_stream_append_truncate_and_session_switch() {
        let turns = vec![TurnMeta::default()];
        let expanded = HashSet::new();
        let mut entries = vec![
            entry("user-0", user_item("go")),
            entry("assistant-0", assistant("working")),
        ];
        let initial = index_turns(&turns, &entries, None, &expanded);
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].entry_range, 0..2);

        // Another entry joins the current turn: identity stays at item index 0,
        // but its variable height must be measured again.
        entries.push(command("command-0"));
        let current_turn_append = index_turns(&turns, &entries, None, &expanded);
        assert_eq!(current_turn_append[0].entry_range, 0..3);
        assert_eq!(
            list_sync(&initial, &current_turn_append, false),
            ListSync::Incremental {
                append: None,
                remeasure: vec![0],
            }
        );

        // A new turn adds exactly one list item. The former tail is also
        // remeasured because it gains the visual inter-turn gap.
        let turns = vec![TurnMeta::default(), TurnMeta::default()];
        entries.push(at_turn(entry("user-1", user_item("next")), 1));
        let new_turn = index_turns(&turns, &entries, None, &expanded);
        assert_eq!(new_turn[0].entry_range, 0..3);
        assert_eq!(new_turn[1].entry_range, 3..4);
        assert_eq!(
            list_sync(&current_turn_append, &new_turn, false),
            ListSync::Incremental {
                append: Some(1..2),
                remeasure: vec![0],
            }
        );

        // Conversation truncation cannot leave ListState with stale item indices.
        assert_eq!(
            list_sync(&new_turn, &initial, false),
            ListSync::Reset { count: 1 }
        );
        // Even an equal-shaped replacement must reset when the session changes.
        assert_eq!(
            list_sync(&initial, &initial, true),
            ListSync::Reset { count: 1 }
        );
    }

    #[test]
    fn incremental_turn_index_matches_full_index_across_tail_and_reset_scenarios() {
        let mut cache = TurnIndexCache::default();
        let mut turns = vec![TurnMeta::default()];
        let mut entries = vec![entry("user-0", user_item("go"))];
        let mut expanded = HashSet::new();
        let mut indexed = Vec::new();

        cache.sync(&mut indexed, &turns, &entries, None, &expanded, false);
        assert_eq!(indexed, index_turns(&turns, &entries, None, &expanded));

        // (a) Append an entry to the last turn.
        entries.push(entry("assistant-0", assistant("working")));
        cache.sync(&mut indexed, &turns, &entries, None, &expanded, false);
        assert_eq!(indexed, index_turns(&turns, &entries, None, &expanded));

        // (b) Append a new turn.
        turns.push(TurnMeta::default());
        entries.push(at_turn(entry("user-1", user_item("next")), 1));
        cache.sync(&mut indexed, &turns, &entries, None, &expanded, false);
        assert_eq!(indexed, index_turns(&turns, &entries, None, &expanded));

        // (c) Replace the streaming tail Arc with updated content.
        entries[2] = at_turn(entry("user-1", user_item("next, updated")), 1);
        cache.sync(&mut indexed, &turns, &entries, None, &expanded, false);
        assert_eq!(indexed, index_turns(&turns, &entries, None, &expanded));

        // (d) Toggle a disclosure expansion key.
        entries[2] = at_turn(
            entry(
                "user-1",
                EntryContent::Item(ItemContent::UserMessage {
                    text: "context\nquestion".into(),
                    context_len: Some(8),
                    attachments: Vec::new(),
                }),
            ),
            1,
        );
        cache.sync(&mut indexed, &turns, &entries, None, &expanded, false);
        expanded.insert("orchestrate-context-user-1".into());
        cache.sync(&mut indexed, &turns, &entries, None, &expanded, false);
        assert_eq!(indexed, index_turns(&turns, &entries, None, &expanded));

        // (e) A session switch resets unrelated cached inputs.
        let switched_turns = vec![TurnMeta::default()];
        let switched_entries = vec![entry("new-session", assistant("fresh"))];
        let no_expanded = HashSet::new();
        cache.sync(
            &mut indexed,
            &switched_turns,
            &switched_entries,
            None,
            &no_expanded,
            true,
        );
        assert_eq!(
            indexed,
            index_turns(&switched_turns, &switched_entries, None, &no_expanded)
        );

        // (f) Rewind/removal takes the full path and remains equivalent.
        let empty_entries = Vec::new();
        cache.sync(
            &mut indexed,
            &switched_turns,
            &empty_entries,
            None,
            &no_expanded,
            false,
        );
        assert_eq!(
            indexed,
            index_turns(&switched_turns, &empty_entries, None, &no_expanded)
        );
    }

    #[test]
    fn replacing_the_tail_of_a_200_turn_timeline_reindexes_one_turn() {
        let turns = vec![TurnMeta::default(); 200];
        let mut entries = (0..200)
            .map(|turn| at_turn(entry(&format!("assistant-{turn}"), assistant("x")), turn))
            .collect::<Vec<_>>();
        let expanded = HashSet::new();
        let mut cache = TurnIndexCache::default();
        let mut incremental = Vec::new();
        cache.sync(&mut incremental, &turns, &entries, None, &expanded, false);

        entries[199] = at_turn(entry("assistant-199", assistant("streamed")), 199);
        cache.sync(&mut incremental, &turns, &entries, None, &expanded, false);

        assert_eq!(incremental, index_turns(&turns, &entries, None, &expanded));
        assert!(
            cache.reindexed_turns() <= 1,
            "tail replacement reindexed {} turns",
            cache.reindexed_turns()
        );
    }

    #[test]
    fn subagent_status_change_remeasures_the_turn() {
        let turns = vec![TurnMeta::default()];
        let entries = vec![entry(
            "spawn",
            EntryContent::Item(ItemContent::Subagent {
                agent_type: "researcher".into(),
                description: "Inspect the protocol".into(),
                status: ItemStatus::InProgress,
                summary: None,
            }),
        )];
        let running = index_turns(&turns, &entries, None, &HashSet::new());
        let mut completed_entries = entries;
        if let EntryContent::Item(ItemContent::Subagent {
            status, summary, ..
        }) = &mut Arc::make_mut(&mut completed_entries[0]).content
        {
            *status = ItemStatus::Completed;
            *summary = Some("Found the event envelope".into());
        }
        let completed = index_turns(&turns, &completed_entries, None, &HashSet::new());
        assert_eq!(
            list_sync(&running, &completed, false),
            ListSync::Incremental {
                append: None,
                remeasure: vec![0],
            }
        );
    }

    #[test]
    fn segment_entries_preserves_interleaved_timeline_order() {
        let entries = [
            entry("user", user_item("go")),
            command("cmd-1"),
            command("cmd-2"),
            entry("assistant-1", assistant("first")),
            command("cmd-3"),
            entry("assistant-2", assistant("second")),
            entry(
                "error",
                EntryContent::Error {
                    message: "boom".into(),
                },
            ),
        ];
        let segments = segment_entries(&entries, false).flow;

        assert_eq!(segments.len(), 6);
        assert!(matches!(segments[0], Segment::User(entry) if entry.id == "user"));
        assert!(matches!(
            &segments[1],
            Segment::ActivityRun(entries)
                if entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["cmd-1", "cmd-2"]
        ));
        assert!(matches!(segments[2], Segment::Assistant(entry) if entry.id == "assistant-1"));
        assert!(matches!(
            &segments[3],
            Segment::ActivityRun(entries)
                if entries.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>() == ["cmd-3"]
        ));
        assert!(matches!(segments[4], Segment::Assistant(entry) if entry.id == "assistant-2"));
        assert!(matches!(segments[5], Segment::Error(entry) if entry.id == "error"));
    }

    #[test]
    fn segment_entries_coalesces_an_all_activity_turn() {
        let entries = [command("cmd-1"), command("cmd-2")];
        let segments = segment_entries(&entries, false).flow;

        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(entries)] if entries.len() == 2
        ));
    }

    #[test]
    fn segment_entries_handles_an_empty_turn() {
        let segmented = segment_entries(&[], false);
        assert!(segmented.flow.is_empty());
        assert!(segmented.pending_steers.is_empty());
    }

    #[test]
    fn pending_steers_float_after_live_flow_in_fifo_order_only_while_running() {
        let pending = |id: &str| {
            entry(
                id,
                EntryContent::Steer {
                    text: id.into(),
                    status: SteeringStatus::Pending,
                    context_len: None,
                    attachments: Vec::new(),
                },
            )
        };
        let entries = [
            entry("assistant-a", assistant("a")),
            pending("steer-a"),
            command("command"),
            pending("steer-b"),
            entry("assistant-b", assistant("b")),
        ];

        let live = segment_entries(&entries, true);
        assert_eq!(live.flow.len(), 3);
        assert!(matches!(live.flow[0], Segment::Assistant(entry) if entry.id == "assistant-a"));
        assert!(matches!(
            &live.flow[1],
            Segment::ActivityRun(run) if run.len() == 1 && run[0].id == "command"
        ));
        assert!(matches!(live.flow[2], Segment::Assistant(entry) if entry.id == "assistant-b"));
        assert_eq!(
            live.pending_steers
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["steer-a", "steer-b"]
        );

        let idle = segment_entries(&entries, false);
        assert!(idle.pending_steers.is_empty());
        assert_eq!(idle.flow.len(), 5);
        assert!(matches!(idle.flow[1], Segment::User(entry) if entry.id == "steer-a"));
        assert!(matches!(idle.flow[3], Segment::User(entry) if entry.id == "steer-b"));
    }

    #[test]
    fn steer_status_and_reordering_invalidate_the_virtualized_turn_row() {
        let turns = vec![TurnMeta {
            running: true,
            ..Default::default()
        }];
        let expanded = HashSet::new();
        let pending = entry(
            "steer",
            EntryContent::Steer {
                text: "redirect".into(),
                status: SteeringStatus::Pending,
                context_len: None,
                attachments: Vec::new(),
            },
        );
        let assistant = entry("assistant", assistant("working"));
        let before = index_turns(
            &turns,
            &[pending.clone(), assistant.clone()],
            None,
            &expanded,
        );

        let mut accepted = pending;
        if let EntryContent::Steer { status, .. } = &mut Arc::make_mut(&mut accepted).content {
            *status = SteeringStatus::Accepted;
        }
        let status_changed = index_turns(
            &turns,
            &[accepted.clone(), assistant.clone()],
            None,
            &expanded,
        );
        assert_eq!(
            list_sync(&before, &status_changed, false),
            ListSync::Incremental {
                append: None,
                remeasure: vec![0],
            }
        );

        let reordered = index_turns(&turns, &[assistant, accepted], None, &expanded);
        assert_eq!(
            list_sync(&status_changed, &reordered, false),
            ListSync::Reset { count: 1 }
        );
    }

    #[test]
    fn segment_entries_keeps_activity_runs_continuous_across_file_changes() {
        let entries = [
            command("cmd-1"),
            entry(
                "edit",
                EntryContent::Item(ItemContent::FileChange {
                    changes: vec![],
                    status: ItemStatus::Completed,
                }),
            ),
            command("cmd-2"),
        ];
        let segments = segment_entries(&entries, false).flow;

        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(run)]
                if run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["cmd-1", "edit", "cmd-2"]
        ));
    }

    #[test]
    fn all_reasoning_remains_reachable_while_the_latest_is_live() {
        let entries = [
            entry("reason-1", reasoning("first")),
            entry("reason-2", reasoning("latest")),
        ];

        let segments = segment_entries(&entries, true).flow;
        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(run)]
                if run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["reason-1", "reason-2"]
        ));
    }

    #[test]
    fn consecutive_empty_reasoning_collapses_into_the_latest_reasoning() {
        let entries = [
            entry("empty-1", reasoning("")),
            entry("empty-2", reasoning("  \n")),
            entry("reason", reasoning("visible")),
            command("command"),
            entry("empty-3", reasoning("")),
            entry("empty-4", reasoning("")),
        ];

        let segments = segment_entries(&entries, true).flow;
        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(run)]
                if run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["reason", "command", "empty-4"]
        ));
    }

    #[test]
    fn later_activity_settles_reasoning_without_removing_it() {
        let entries = [
            entry("reason", reasoning("thinking")),
            command("later-command"),
        ];

        let segments = segment_entries(&entries, true).flow;
        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(run)]
                if run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["reason", "later-command"]
        ));

        let entries = [
            entry("reason", reasoning("thinking")),
            entry("assistant", assistant("answer")),
        ];
        let segments = segment_entries(&entries, true).flow;
        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(run), Segment::Assistant(entry)]
                if run.len() == 1 && run[0].id == "reason" && entry.id == "assistant"
        ));
    }

    #[test]
    fn only_a_trailing_activity_run_is_live_in_a_running_turn() {
        let prose_tail = [command("cmd"), entry("assistant", assistant("answer"))];
        let segments = segment_entries(&prose_tail, true).flow;
        // Prose, then the bare turn-level indicator: no empty run is invented
        // to host it, and the earlier run has already settled.
        assert_eq!(segments.len(), 2);
        assert_eq!(live_activity_segment(&segments, true), None);
        // Once the turn ends, that same run is the final settled work log.
        assert_eq!(live_activity_segment(&segments, false), Some(0));

        let activity_tail = [entry("assistant", assistant("answer")), command("cmd")];
        let segments = segment_entries(&activity_tail, true).flow;
        assert_eq!(live_activity_segment(&segments, true), Some(1));

        let prose_only = [entry("assistant", assistant("answer"))];
        let segments = segment_entries(&prose_only, true).flow;
        assert_eq!(live_activity_segment(&segments, true), None);
    }

    #[test]
    fn completion_keeps_reasoning_reachable_in_history() {
        let entries = [entry("reason", reasoning("finished thinking"))];

        assert!(matches!(
            segment_entries(&entries, false).flow.as_slice(),
            [Segment::ActivityRun(run)] if run.len() == 1 && run[0].id == "reason"
        ));
    }

    fn file_change(id: &str, paths: &[&str]) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::Item(ItemContent::FileChange {
                changes: paths
                    .iter()
                    .map(|path| FileChange {
                        path: (*path).to_string(),
                        kind: FileChangeKind::Modify,
                        diff: None,
                    })
                    .collect(),
                status: ItemStatus::Completed,
            }),
        )
    }

    fn refs(entries: &[Arc<TimelineEntry>]) -> Vec<&TimelineEntry> {
        entries.iter().map(AsRef::as_ref).collect()
    }

    #[test]
    fn one_line_collapses_multiline_commands_into_a_single_spaced_line() {
        let cmd = "set pipe -e \"\n  cargo fmt --check\n  cargo clippy\n\"";
        assert_eq!(
            super::one_line(cmd),
            "set pipe -e \" cargo fmt --check cargo clippy \""
        );

        let long = "word ".repeat(500);
        assert_eq!(super::one_line(&long).chars().count(), 600);
    }

    #[test]
    fn break_marker_preview_marks_every_line_break_with_its_range() {
        let cmd = "set pipe -e \"\n  cargo fmt --check\n\n  cargo clippy\n\"";
        let (preview, markers) = super::one_line_with_break_markers(cmd);
        assert_eq!(
            preview,
            "set pipe -e \"\\ncargo fmt --check\\ncargo clippy\\n\""
        );
        assert_eq!(markers.len(), 3);
        for range in markers {
            assert_eq!(&preview[range], "\\n");
        }

        let (single, markers) = super::one_line_with_break_markers("cargo test");
        assert_eq!(single, "cargo test");
        assert!(markers.is_empty());
    }

    #[test]
    fn work_log_capsule_localizes_nonzero_counts_in_priority_order() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let counts = WorkLogCounts {
            commands: 2,
            files: 3,
            tools: 1,
            subagents: 2,
        };

        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(
            work_log_capsule_label(&counts, 9),
            "1 tool call · 3 edits · 2 commands"
        );
        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            work_log_capsule_label(&counts, 9),
            "1 次工具调用 · 3 处编辑 · 2 条命令"
        );
    }

    #[test]
    fn work_log_capsule_omits_zero_components_and_uses_activity_fallback() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let tools_only = WorkLogCounts {
            tools: 2,
            ..WorkLogCounts::default()
        };

        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(work_log_capsule_label(&tools_only, 2), "2 tool calls");
        assert_eq!(work_log_capsule_label(&WorkLogCounts::default(), 0), "");
        assert_eq!(
            work_log_capsule_label(&WorkLogCounts::default(), 1),
            "1 activity"
        );
        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(work_log_capsule_label(&tools_only, 2), "2 次工具调用");
    }

    #[test]
    fn work_log_counts_unique_file_paths_across_snapshots() {
        let entries = [
            file_change("files-1", &["src/a.rs", "src/b.rs"]),
            file_change("files-2", &["src/a.rs", "src/a.rs"]),
        ];

        assert_eq!(work_log_counts(&refs(&entries)).files, 2);
    }

    #[test]
    fn live_activity_run_keeps_five_entries_outside_the_folded_prefix() {
        let entries = [
            command("cargo check"),
            file_change("edit", &["src/foo.rs"]),
            command("cargo test"),
            command("cargo clippy"),
            command("cargo fmt"),
            command("cargo nextest"),
        ];
        let activities = refs(&entries);
        let ids = |rows: &[&TimelineEntry]| {
            rows.iter()
                .map(|entry| entry.id.clone())
                .collect::<Vec<String>>()
        };

        let (folded, visible) = partition_activity_run(&activities, true);
        assert_eq!(ids(folded), ["cargo check"]);
        assert_eq!(
            ids(visible),
            [
                "edit",
                "cargo test",
                "cargo clippy",
                "cargo fmt",
                "cargo nextest"
            ]
        );

        let (folded, visible) = partition_activity_run(&activities[..5], true);
        assert!(folded.is_empty());
        assert_eq!(ids(visible), ids(&activities[..5]));

        // Assistant prose settles the run: the same six entries are now all
        // represented by one collapsed Work Log and none remain loose.
        let (folded, visible) = partition_activity_run(&activities, false);
        assert_eq!(ids(folded), ids(&activities));
        assert!(visible.is_empty());
    }

    #[test]
    fn live_edit_rows_expand_every_file_and_relativize_to_the_workspace() {
        let cwd = Path::new("/work/repo");
        let changes = vec![
            FileChange {
                path: "/work/repo/src/foo.rs".into(),
                kind: FileChangeKind::Modify,
                diff: None,
            },
            FileChange {
                path: "/work/repo/crates/ui/src/chat.rs".into(),
                kind: FileChangeKind::Modify,
                diff: None,
            },
            FileChange {
                path: "/elsewhere/vendor/bar.rs".into(),
                kind: FileChangeKind::Create,
                diff: None,
            },
        ];

        let rows = live_edit_rows(&changes, cwd);
        assert_eq!(
            rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
            [
                "src/foo.rs",
                "crates/ui/src/chat.rs",
                "/elsewhere/vendor/bar.rs"
            ]
        );
        // No diff means no counts: "+0 -0" would claim the edit changed nothing.
        assert!(rows.iter().all(|row| row.counts.is_none()));
    }

    #[test]
    fn live_edit_counts_only_survive_when_a_diff_has_real_edits() {
        // A real diff counts accurately, ignoring the `+++`/`---` headers.
        assert_eq!(live_edit_counts(Some(REAL_DIFF)), Some((2, 1)));
        assert_eq!(live_edit_counts(Some("+only added\n")), Some((1, 0)));
        assert_eq!(live_edit_counts(Some("-only removed\n")), Some((0, 1)));

        // Nothing displayable: "+0 -0" would claim the edit changed nothing.
        assert_eq!(live_edit_counts(None), None);
        assert_eq!(live_edit_counts(Some("")), None);
        assert_eq!(live_edit_counts(Some("   \n\t\n \n")), None);
        assert_eq!(
            live_edit_counts(Some("--- a/src/foo.rs\n+++ b/src/foo.rs\n")),
            None
        );
        assert_eq!(
            live_edit_counts(Some("--- a/f\n+++ b/f\n@@ -1 +1 @@\n unchanged\n")),
            None
        );

        // The finished CHANGED FILES card keeps its own totals semantics, so a
        // header-only diff still contributes (0, 0) there rather than vanishing.
        assert_eq!(diff_stats(Some("--- a/f\n+++ b/f\n")), (0, 0));
        assert_eq!(diff_stats(Some(REAL_DIFF)), (2, 1));
    }

    #[test]
    fn live_edit_rows_carry_counts_only_for_files_with_real_edits() {
        let cwd = Path::new("/work/repo");
        let changes = vec![
            FileChange {
                path: "/work/repo/src/foo.rs".into(),
                kind: FileChangeKind::Modify,
                diff: Some(REAL_DIFF.into()),
            },
            FileChange {
                path: "/work/repo/src/bar.rs".into(),
                kind: FileChangeKind::Create,
                diff: Some(String::new()),
            },
        ];

        assert_eq!(
            live_edit_rows(&changes, cwd),
            vec![
                LiveEditRow {
                    path: "src/foo.rs".into(),
                    counts: Some((2, 1)),
                },
                LiveEditRow {
                    path: "src/bar.rs".into(),
                    counts: None,
                },
            ]
        );
    }

    #[test]
    fn finished_activity_runs_use_segment_scoped_counts() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let entries = [
            command("command-1"),
            file_change("files-1", &["src/shared.rs"]),
            entry("assistant", assistant("intermediate output")),
            command("command-2"),
            command("command-3"),
        ];
        let segments = segment_entries(&entries, false).flow;
        let activity_indexes: Vec<usize> = segments
            .iter()
            .enumerate()
            .filter_map(|(index, segment)| {
                matches!(segment, Segment::ActivityRun(_)).then_some(index)
            })
            .collect();
        assert_eq!(activity_indexes.len(), 2);

        let counts = work_log_counts(&refs(&entries));
        assert_eq!(counts.commands, 3);
        assert_eq!(counts.files, 1);

        let labels = || {
            activity_indexes
                .iter()
                .map(|index| {
                    let Segment::ActivityRun(activities) = &segments[*index] else {
                        unreachable!();
                    };
                    let segment_counts = work_log_counts(activities);
                    work_log_capsule_label(&segment_counts, activities.len())
                })
                .collect::<Vec<_>>()
        };
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(labels(), ["1 edit · 1 command", "2 commands"]);
        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(labels(), ["1 处编辑 · 1 条命令", "2 条命令"]);
    }

    #[test]
    fn relativize_strips_cwd_prefix() {
        let cwd = Path::new("/tmp/proj");
        assert_eq!(
            tcode_services::user_files::relativize_to_workspace("/tmp/proj/src/a.rs", cwd),
            "src/a.rs"
        );
        assert_eq!(
            tcode_services::user_files::relativize_to_workspace("/tmp/proj/a.rs", cwd),
            "a.rs"
        );
        // Outside the cwd stays absolute.
        assert_eq!(
            tcode_services::user_files::relativize_to_workspace("/other/x.rs", cwd),
            "/other/x.rs"
        );
        // Already-relative paths are left as-is.
        assert_eq!(
            tcode_services::user_files::relativize_to_workspace("src/b.rs", cwd),
            "src/b.rs"
        );
    }

    #[test]
    fn md_sync_decides_push_reset_and_noop() {
        // Unchanged text does nothing (the streaming hot path: most notifies
        // carry no new text for a given entry).
        assert_eq!(md_sync("abc", "abc"), MdSync::Noop);
        assert_eq!(md_sync("", ""), MdSync::Noop);
        // An append is a push of just the delta.
        assert_eq!(md_sync("", "I"), MdSync::Push("I".into()));
        assert_eq!(md_sync("I", "I'll go"), MdSync::Push("'ll go".into()));
        // Anything that is not an append is a reset: a rewrite, a shrink, or a
        // snapshot that replaces the accumulated text.
        assert_eq!(md_sync("abc", "xbc"), MdSync::Reset);
        assert_eq!(md_sync("abcd", "abc"), MdSync::Reset);
        assert_eq!(md_sync("abc", ""), MdSync::Reset);
    }

    fn turn_time_row(clock: String, timing: Option<TurnTiming>) -> String {
        turn_time_parts(clock, timing).join(" · ")
    }

    #[test]
    fn finished_time_clauses_format_cost_and_only_show_a_divergent_served_model() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);

        let clauses = turn_time_clauses(
            "3:04 PM".into(),
            Some(TurnTiming::new(80_000, 35_000)),
            Some(0.12),
            Some("claude-opus-5"),
            Some("claude-fable-5"),
        );
        assert_eq!(
            clauses
                .iter()
                .map(|clause| (clause.selector, clause.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("turn-time-clock", "3:04 PM"),
                ("turn-time-total", "1m20s"),
                ("turn-time-cost", "$0.12"),
                ("turn-time-model", "⚠ claude-opus-5"),
            ]
        );

        let sub_cent = turn_time_clauses(
            "3:04 PM".into(),
            None,
            Some(0.004),
            Some("claude-fable-5"),
            Some("claude-fable-5"),
        );
        assert_eq!(
            sub_cent
                .iter()
                .map(|clause| (clause.selector, clause.text.as_str()))
                .collect::<Vec<_>>(),
            vec![("turn-time-clock", "3:04 PM"), ("turn-time-cost", "$0.004"),]
        );

        let missing_requested =
            turn_time_clauses("3:04 PM".into(), None, None, Some("served"), None);
        assert_eq!(missing_requested.len(), 1);
    }

    #[test]
    fn the_time_row_keeps_one_compact_unit_per_visible_clause() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        // The footer wraps at clause boundaries, so each clause has to be its
        // own unit — never one string the narrow column would have to clip.
        assert_eq!(
            turn_time_parts("3:04 PM".into(), Some(TurnTiming::new(80_000, 35_000))),
            vec!["3:04 PM", "1m20s"]
        );
        assert_eq!(
            turn_time_parts("3:04 PM".into(), Some(TurnTiming::new(10_500, 3_600))),
            vec!["3:04 PM", "10s"]
        );
        // No clause carries a separator of its own: the row's dots belong to the
        // layout, so a wrapped line can never open with an orphaned one.
        for clause in turn_time_parts("3:04 PM".into(), Some(TurnTiming::new(80_000, 35_000))) {
            assert!(
                !clause.contains('·'),
                "clause {clause:?} embeds a separator"
            );
        }
        // The legacy fallback stays a single unit, so it renders dot-free.
        assert_eq!(
            turn_time_parts("9:00 AM".into(), None),
            vec!["9:00 AM".to_string()]
        );
    }

    #[test]
    fn finished_time_row_is_compact_and_breakdown_is_localized_in_tooltip() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // 1m 20s total, 35s of it inside tool calls.
        let timing = TurnTiming::new(80_000, 35_000);

        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(
            turn_time_row("3:04 PM".into(), Some(timing)),
            "3:04 PM · 1m20s"
        );
        assert_eq!(
            turn_time_breakdown(Some(timing)).as_deref(),
            Some("AI thinking & response 45s · Tool calls 35s")
        );

        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            turn_time_row("3:04 PM".into(), Some(timing)),
            "3:04 PM · 1m20s"
        );
        assert_eq!(
            turn_time_breakdown(Some(timing)).as_deref(),
            Some("AI 思考与回答 45 秒 · 工具调用 35 秒")
        );
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }

    #[test]
    fn an_ai_only_turn_keeps_total_visible_and_buckets_in_tooltip() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(
            turn_time_row("9:00 AM".into(), Some(TurnTiming::new(8_000, 0))),
            "9:00 AM · 8s"
        );
        assert_eq!(
            turn_time_breakdown(Some(TurnTiming::new(8_000, 0))).as_deref(),
            Some("AI thinking & response 8s · Tool calls 0s")
        );
    }

    #[test]
    fn a_turn_without_a_derivable_breakdown_keeps_the_bare_clock() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(turn_time_row("9:00 AM".into(), None), "9:00 AM");
    }

    #[test]
    fn day_long_turns_read_in_hours_in_both_locales() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // 24h 00m 59s total, 23h 30m 00s of it waiting on tools. The seconds
        // survive the hour rollup — this row reports real elapsed time.
        let timing = TurnTiming::new(86_459_000, 84_600_000);

        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(
            turn_time_row("1:00 AM".into(), Some(timing)),
            "1:00 AM · 24h00m59s"
        );

        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            turn_time_row("1:00 AM".into(), Some(timing)),
            "1:00 AM · 24h00m59s"
        );
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }

    #[test]
    fn the_live_working_indicator_keeps_its_own_format() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        // The running row is untouched by the breakdown's hour rollup.
        assert_eq!(format_duration(3_600), "60m 00s");
        assert_eq!(format_duration(90_061), "1501m 01s");
        // The finished row rolls up, and keeps every second it claims.
        assert_eq!(format_span(3_600), "1h 00m 00s");
        assert_eq!(format_span(90_061), "25h 01m 01s");
        assert_eq!(format_span(59), "59s");
        assert_eq!(format_span(90), "1m 30s");
        assert_eq!(format_elapsed_deciseconds(0), "0.0s");
        assert_eq!(format_elapsed_deciseconds(12_399), "12.3s");
        assert_eq!(format_elapsed_deciseconds(65_299), "1m 5.2s");
    }
}
