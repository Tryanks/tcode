use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use agent::{FileChange, ItemContent, ItemStatus, TurnStatus};
use tcode_core::notice::{HostNotice, host_notice};
use tcode_core::session::{EntryContent, SteeringStatus, TimelineEntry, TurnMeta, TurnTiming};

use super::components::notices::MessageTag;
use super::timeline::rows::TimelineRow;

/// The row, its turn, the entries it renders (a Work Log header's whole run),
/// the turn's entries for the trailer (empty unless the row is the turn's
/// last), how far the gap above the trailer is open, and the pinned message
/// ids.
pub(crate) type RowRenderArgs<'a> = (
    &'a TimelineRow,
    &'a TurnMeta,
    &'a Path,
    &'a [Arc<TimelineEntry>],
    &'a [Arc<TimelineEntry>],
    f32,
    (Option<&'a str>, Option<&'a str>),
);

/// A chronological block in a turn. File-change entries stay in activity runs
/// for summary counting, but are rendered by the turn-level CHANGED FILES card.
#[derive(Debug)]
pub(crate) enum Segment<'a> {
    ActivityRun(Vec<&'a TimelineEntry>),
    Images(Vec<&'a TimelineEntry>),
    Relay(&'a TimelineEntry),
    ModelChange(&'a TimelineEntry),
    ContextCompacted(&'a TimelineEntry),
    /// A context-window change no user message stands before to carry it.
    ContextWindowChanged(&'a TimelineEntry),
    /// Settings kept for a message not loaded here to carry them.
    SettingsKept(&'a TimelineEntry),
    SessionMoved(&'a TimelineEntry),
    Worktree(&'a TimelineEntry),
    /// Consecutive actions of one other thread on this thread's run.
    ControlAction(Vec<&'a TimelineEntry>),
    /// A rewind's divider.
    Rewind(&'a TimelineEntry),
    /// A user message, with the notes on its settings that follow it in its
    /// range ([`message_tags`]).
    User(&'a TimelineEntry),
    Assistant(&'a TimelineEntry),
    Plan(&'a TimelineEntry),
    Visualize(&'a TimelineEntry),
    /// A call that linked a pull request to the thread, as its record.
    PullRequest(&'a TimelineEntry),
    /// A subagent, or a thread a call started.
    ThreadCard(&'a TimelineEntry),
    Error(&'a TimelineEntry),
    /// A request the harness tries again, in its words for the latest
    /// attempt.
    RequestRetry(&'a TimelineEntry),
    /// A warning of the harness, in its words.
    Warning(&'a TimelineEntry),
    /// A record too large to send here, kept on the host.
    Withheld(&'a TimelineEntry),
}

#[derive(Debug)]
pub(crate) struct SegmentedEntries<'a> {
    pub(crate) flow: Vec<Segment<'a>>,
    /// The entry indices each segment of `flow` spans, in `entries` order.
    /// A run's range covers the pending steers it skipped over, so a row that
    /// re-segments its own range folds to the same segment.
    pub(crate) ranges: Vec<Range<usize>>,
    pub(crate) pending_steers: Vec<&'a TimelineEntry>,
}

pub(crate) fn displayed_error_text<'a>(content: &'a EntryContent, harness: &str) -> Cow<'a, str> {
    match content {
        EntryContent::Error { message, .. } => Cow::Borrowed(message),
        EntryContent::ProviderStartError { error } => {
            crate::tr!("errors.provider_start", harness = harness, error = error)
        }
        _ => unreachable!("displayed_error_text requires error timeline content"),
    }
}

/// What the error card of an exhausted usage window offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LimitResumeAction {
    /// A resume is queued for the reset: countdown plus cancel.
    Scheduled {
        queued_id: u64,
    },
    /// The error still ends the thread and nothing is queued after it: offer
    /// to resume at the reset. The host holds it until then by its own clock,
    /// and sends it at once when the reset has passed. A queued message (the
    /// resume itself while the provider starts, or the user's own) means
    /// something already follows the error.
    Schedule,
    None,
}

pub(crate) fn limit_resume_action(
    queued_id: Option<u64>,
    ends_thread: bool,
    queue_empty: bool,
) -> LimitResumeAction {
    match queued_id {
        Some(queued_id) => LimitResumeAction::Scheduled { queued_id },
        None if ends_thread && queue_empty => LimitResumeAction::Schedule,
        None => LimitResumeAction::None,
    }
}

pub(crate) type UserContent<'a> = (&'a str, Option<SteeringStatus>, &'a [String]);

pub(crate) fn user_content(content: &EntryContent) -> Option<UserContent<'_>> {
    match content {
        EntryContent::Item(ItemContent::UserMessage { text, attachments }) => {
            Some((text, None, attachments))
        }
        EntryContent::Steer {
            text,
            status,
            attachments,
        } => Some((text, Some(*status), attachments)),
        EntryContent::Feedback { text } => Some((text, None, &[])),
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
    let mut ranges = Vec::new();
    let mut activities = Vec::new();
    let mut run_start = None;
    let mut pending_steers = Vec::new();
    let flush_activities = |segments: &mut Vec<Segment<'a>>,
                            ranges: &mut Vec<Range<usize>>,
                            activities: &mut Vec<&'a TimelineEntry>,
                            run_start: &mut Option<usize>,
                            end: usize| {
        if !activities.is_empty() {
            segments.push(Segment::ActivityRun(std::mem::take(activities)));
            ranges.push(run_start.take().expect("an activity run has a start")..end);
        }
        *run_start = None;
    };

    for (index, entry) in entries.iter().enumerate() {
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
        let mut single = |segments: &mut Vec<Segment<'a>>,
                          ranges: &mut Vec<Range<usize>>,
                          segment: Segment<'a>| {
            flush_activities(segments, ranges, &mut activities, &mut run_start, index);
            segments.push(segment);
            ranges.push(index..index + 1);
        };
        match &entry.content {
            EntryContent::Item(ItemContent::ToolCall { .. })
                if super::tools::visualize::card(entry).is_some() =>
            {
                single(&mut segments, &mut ranges, Segment::Visualize(entry));
            }
            EntryContent::Item(ItemContent::ToolCall { .. })
                if super::tools::pull_request::card(entry).is_some() =>
            {
                single(&mut segments, &mut ranges, Segment::PullRequest(entry));
            }
            EntryContent::Item(ItemContent::Subagent { .. }) => {
                single(&mut segments, &mut ranges, Segment::ThreadCard(entry));
            }
            EntryContent::Item(ItemContent::ToolCall { .. })
                if super::tools::subagent::card(entry).is_some() =>
            {
                single(&mut segments, &mut ranges, Segment::ThreadCard(entry));
            }
            EntryContent::Item(item)
                if matches!(item, ItemContent::ImageRead { .. })
                    || matches!(item, ItemContent::ToolCall { image_reads, status: ItemStatus::Completed, .. } if !image_reads.is_empty())
                        // A computer run keeps its screenshots in its window.
                        && !super::tools::run::is_computer(entry) =>
            {
                flush_activities(
                    &mut segments,
                    &mut ranges,
                    &mut activities,
                    &mut run_start,
                    index,
                );
                if let Some(Segment::Images(images)) = segments.last_mut() {
                    images.push(entry);
                    ranges.last_mut().expect("an image run has a range").end = index + 1;
                } else {
                    segments.push(Segment::Images(vec![entry]));
                    ranges.push(index..index + 1);
                }
            }
            EntryContent::Item(ItemContent::ImageRead { .. })
            | EntryContent::Item(ItemContent::CommandExecution { .. })
            | EntryContent::Item(ItemContent::ToolCall { .. })
            | EntryContent::Item(ItemContent::WebSearch { .. })
            | EntryContent::Item(ItemContent::Other { .. })
            | EntryContent::Item(ItemContent::FileChange { .. }) => {
                run_start.get_or_insert(index);
                activities.push(entry);
            }
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
                run_start.get_or_insert(index);
                activities.push(entry);
            }
            EntryContent::Item(ItemContent::UserMessage { .. })
            | EntryContent::Steer { .. }
            | EntryContent::Feedback { .. } => {
                single(&mut segments, &mut ranges, Segment::User(entry));
            }
            EntryContent::Item(ItemContent::Plan { .. }) => {
                single(&mut segments, &mut ranges, Segment::Plan(entry));
            }
            EntryContent::ProviderRelay { .. } => {
                single(&mut segments, &mut ranges, Segment::Relay(entry));
            }
            EntryContent::ModelChanged { .. } => {
                single(&mut segments, &mut ranges, Segment::ModelChange(entry));
            }
            EntryContent::ContextCompacted(_) => {
                single(&mut segments, &mut ranges, Segment::ContextCompacted(entry));
            }
            // A note on the settings of the message just before it belongs
            // to that message's range.
            EntryContent::ContextWindowChanged { .. } | EntryContent::SettingsKept
                if matches!(segments.last(), Some(Segment::User(_)))
                    && ranges.last().is_some_and(|range| range.end == index) =>
            {
                ranges.last_mut().expect("a user segment has a range").end = index + 1;
            }
            EntryContent::ContextWindowChanged { .. } => {
                single(
                    &mut segments,
                    &mut ranges,
                    Segment::ContextWindowChanged(entry),
                );
            }
            EntryContent::SettingsKept => {
                single(&mut segments, &mut ranges, Segment::SettingsKept(entry));
            }
            EntryContent::SessionMoved { .. } => {
                single(&mut segments, &mut ranges, Segment::SessionMoved(entry));
            }
            // The checkout row under the composer shows the worktree a
            // thread works in; only what went wrong stands in the flow.
            EntryContent::Worktree {
                request: agent::WorktreeRequest::Create { .. },
                answer: agent::WorktreeAnswer::Ready { .. },
            } => {}
            EntryContent::Worktree { .. } => {
                single(&mut segments, &mut ranges, Segment::Worktree(entry));
            }
            // In no segment's range, so no row re-segments it without the
            // card that tells it.
            EntryContent::ControlAction(action)
                if super::tools::pull_request::told_by_card(action, entries) =>
            {
                flush_activities(
                    &mut segments,
                    &mut ranges,
                    &mut activities,
                    &mut run_start,
                    index,
                );
            }
            // The card of the call that started a thread tells it. The record
            // the starting thread keeps names the started one in its detail;
            // the started thread's own has no detail, and stays.
            EntryContent::ControlAction(action)
                if action.action == tcode_core::control::action::START_THREAD
                    && action.detail.is_some() => {}
            // What only changed the thread's own standing is the control
            // activity record's to tell, not the conversation's.
            EntryContent::ControlAction(action)
                if crate::control::changes_only_the_thread(&action.action) => {}
            EntryContent::ControlAction(action)
                if continues_control_run(&segments, &ranges, entries, index, action) =>
            {
                let Some(Segment::ControlAction(run)) = segments.last_mut() else {
                    unreachable!("a control run continues the last segment");
                };
                run.push(entry);
                ranges.last_mut().expect("a control run has a range").end = index + 1;
            }
            EntryContent::ControlAction(_) => {
                single(
                    &mut segments,
                    &mut ranges,
                    Segment::ControlAction(vec![entry]),
                );
            }
            EntryContent::Rewound(_) => {
                single(&mut segments, &mut ranges, Segment::Rewind(entry));
            }
            // Told in the action row of the message it was for, until the
            // next message is sent.
            EntryContent::RewindFailed { .. } => {}
            EntryContent::Item(ItemContent::AssistantMessage { .. }) => {
                single(&mut segments, &mut ranges, Segment::Assistant(entry));
            }
            EntryContent::Error { .. } | EntryContent::ProviderStartError { .. } => {
                single(&mut segments, &mut ranges, Segment::Error(entry));
            }
            EntryContent::Withheld { .. } => {
                single(&mut segments, &mut ranges, Segment::Withheld(entry));
            }
            EntryContent::RequestRetry { .. } => {
                single(&mut segments, &mut ranges, Segment::RequestRetry(entry));
            }
            EntryContent::Warning { .. } => {
                single(&mut segments, &mut ranges, Segment::Warning(entry));
            }
            // The thread's banner tells them, once.
            EntryContent::Unreadable { .. } => {}
        }
    }
    flush_activities(
        &mut segments,
        &mut ranges,
        &mut activities,
        &mut run_start,
        entries.len(),
    );
    SegmentedEntries {
        flow: segments,
        ranges,
        pending_steers,
    }
}

/// Whether the action at `index` goes on the last segment's run of one
/// actor's actions: nothing stands between them but actions no segment
/// tells, none of them told by a card in `entries`.
fn continues_control_run(
    segments: &[Segment<'_>],
    ranges: &[Range<usize>],
    entries: &[Arc<TimelineEntry>],
    index: usize,
    action: &agent::ControlAction,
) -> bool {
    let (Some(Segment::ControlAction(run)), Some(range)) = (segments.last(), ranges.last()) else {
        return false;
    };
    let EntryContent::ControlAction(first) = &run[0].content else {
        return false;
    };
    first.actor_thread_id == action.actor_thread_id
        && entries[range.end..index].iter().all(|between| {
            matches!(&between.content, EntryContent::ControlAction(skipped)
                if !super::tools::pull_request::told_by_card(skipped, entries))
        })
}

/// The notes on its settings a user message's range carries after it.
pub(crate) fn message_tags(range: &[Arc<TimelineEntry>]) -> Vec<MessageTag> {
    range
        .iter()
        .skip(1)
        .map_while(|entry| MessageTag::of(&entry.content))
        .collect()
}

/// The rewind to `checkpoint_id`, or with `undo` its undo, that the harness
/// last failed to make, while no message was sent since.
pub(crate) fn standing_rewind_failure<'a>(
    entries: &'a [Arc<TimelineEntry>],
    checkpoint_id: &str,
    undo: bool,
) -> Option<(Option<agent::RewindMode>, &'a agent::RewindFailure)> {
    for entry in entries.iter().rev() {
        match &entry.content {
            EntryContent::RewindFailed {
                checkpoint_id: failed,
                mode,
                failure,
            } if failed == checkpoint_id && mode.is_none() == undo => {
                return Some((*mode, failure));
            }
            content if user_content(content).is_some() && user_notice(content).is_none() => {
                return None;
            }
            _ => {}
        }
    }
    None
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
    pub(crate) reads: usize,
    pub(crate) searches: usize,
    pub(crate) computer: usize,
    pub(crate) tools: usize,
}

/// What a run of activities did, each counted by the window it shows as.
pub(crate) fn work_log_counts(entries: &[&TimelineEntry]) -> WorkLogCounts {
    use super::tools::Family;
    let mut counts = WorkLogCounts::default();
    let mut files = HashSet::new();
    for entry in entries {
        match super::tools::family(entry) {
            Some(Family::Command) => counts.commands += 1,
            Some(Family::Edit(paths)) => files.extend(paths),
            Some(Family::Read) => counts.reads += 1,
            Some(Family::Search) => counts.searches += 1,
            Some(Family::Computer) => counts.computer += 1,
            Some(Family::Tool) => counts.tools += 1,
            None => {}
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
    if counts.computer > 0 {
        clauses.push(if counts.computer == 1 {
            crate::tr!("chat.work_log_computer_one").into_owned()
        } else {
            crate::tr!("chat.work_log_computer", count = counts.computer).into_owned()
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
    if counts.reads > 0 {
        clauses.push(if counts.reads == 1 {
            crate::tr!("chat.work_log_read_one").into_owned()
        } else {
            crate::tr!("chat.work_log_reads", count = counts.reads).into_owned()
        });
    }
    if counts.searches > 0 {
        clauses.push(if counts.searches == 1 {
            crate::tr!("chat.work_log_search_one").into_owned()
        } else {
            crate::tr!("chat.work_log_searches", count = counts.searches).into_owned()
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

/// How long a run of activities took: from the first start seen to the
/// last end seen. `None` when its times do not span any: one activity whose
/// end was never seen took a time nobody knows.
pub(crate) fn activity_run_duration_ms(activities: &[&TimelineEntry]) -> Option<u64> {
    let start = activities.iter().filter_map(|entry| entry.ts).min()?;
    let end = activities
        .iter()
        .filter_map(|entry| entry.finished_ts.or(entry.ts))
        .max()?;
    (end > start).then(|| end - start)
}

// A failed command inside a run is normal agent probing (grep exit 1, a
// retried build); it never fails the run. Only the turn's own terminal
// status colors the snapshot, and only on the segment that carries it.
pub(crate) fn work_log_outcome(turn: &TurnMeta, is_last: bool) -> TurnStatus {
    if is_last {
        turn.status.unwrap_or(TurnStatus::Completed)
    } else {
        TurnStatus::Completed
    }
}

/// `text` collapsed to a single spaced line: every whitespace run (newlines
/// included) becomes one space, so a multi-line command shows its full content
/// in a one-line preview instead of just its first line. Clipped to more
/// characters than any row can render; the row fades out what does not fit.
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

const MINUTE: u64 = 60;

/// A span of time: tenths of a second under a minute, then minutes and
/// seconds, hours, minutes and seconds, and past a day, days, hours and
/// minutes, so a long span never grows an unreadable count of a small unit.
pub(crate) fn format_elapsed(elapsed_ms: u64) -> String {
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let secs = elapsed_ms / 1000;
    if secs < MINUTE {
        let tenths = elapsed_ms / 100;
        crate::tr!(
            "time.duration_seconds",
            seconds = format!("{}.{}", tenths / 10, tenths % 10)
        )
        .into_owned()
    } else if secs < HOUR {
        crate::tr!(
            "time.duration_minutes",
            minutes = secs / MINUTE,
            seconds = format!("{:02}", secs % MINUTE)
        )
        .into_owned()
    } else if secs < DAY {
        crate::tr!(
            "time.duration_hours",
            hours = secs / HOUR,
            minutes = format!("{:02}", (secs % HOUR) / MINUTE),
            seconds = format!("{:02}", secs % MINUTE)
        )
        .into_owned()
    } else {
        crate::tr!(
            "time.duration_days",
            days = secs / DAY,
            hours = format!("{:02}", (secs % DAY) / HOUR),
            minutes = format!("{:02}", (secs % HOUR) / MINUTE)
        )
        .into_owned()
    }
}

/// [`format_elapsed`] for a span counted in seconds, as a clock that ticks
/// each second or a harness's goal reports it, where tenths would say nothing:
/// whole seconds, floored, under a minute.
pub(crate) fn format_elapsed_seconds(elapsed_ms: u64) -> String {
    let secs = elapsed_ms / 1000;
    if secs < MINUTE {
        crate::tr!("time.duration_seconds", seconds = secs).into_owned()
    } else {
        format_elapsed(elapsed_ms)
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

/// The total's breakdown a finished turn's timing row shows on hover, so the
/// buckets do not compete with the clock, the total and the cost.
pub(crate) fn turn_time_breakdown(timing: TurnTiming) -> String {
    let ai = timing.total_ms - timing.tool_ms;
    [
        crate::tr!("chat.turn_ai", duration = format_elapsed(ai)).into_owned(),
        crate::tr!("chat.turn_tools", duration = format_elapsed(timing.tool_ms)).into_owned(),
    ]
    .join(" · ")
}

#[derive(Clone)]
pub(crate) struct TurnTimeClause {
    pub(crate) text: String,
    pub(crate) selector: &'static str,
    pub(crate) warning: bool,
    /// What the clause shows on hover: the total's breakdown.
    pub(crate) tooltip: Option<String>,
}

/// A finished turn's timing row: when it finished, how long it took, what it
/// cost, and the model that served it where that is not the one asked for
/// (`served_model`, by its shown name); each where the turn has it.
pub(crate) fn turn_time_clauses(
    clock: Option<String>,
    timing: Option<TurnTiming>,
    cost_usd: Option<f64>,
    served_model: Option<&str>,
) -> Vec<TurnTimeClause> {
    let mut clauses = Vec::new();
    if let Some(clock) = clock {
        clauses.push(TurnTimeClause {
            text: clock,
            selector: "turn-time-clock",
            warning: false,
            tooltip: None,
        });
    }
    if let Some(timing) = timing {
        clauses.push(TurnTimeClause {
            text: format_elapsed(timing.total_ms),
            selector: "turn-time-total",
            warning: false,
            tooltip: Some(turn_time_breakdown(timing)),
        });
    }
    if let Some(cost) = cost_usd {
        clauses.push(TurnTimeClause {
            text: format_cost_usd(cost),
            selector: "turn-time-cost",
            warning: false,
            tooltip: None,
        });
    }
    if let Some(served) = served_model {
        #[cfg(target_arch = "wasm32")]
        let warning_mark = "!";
        #[cfg(not(target_arch = "wasm32"))]
        let warning_mark = "⚠";
        clauses.push(TurnTimeClause {
            text: format!("{warning_mark} {served}"),
            selector: "turn-time-model",
            warning: true,
            tooltip: None,
        });
    }
    clauses
}

/// Count added / removed lines in a unified diff (ignoring the `+++`/`---`
/// file headers).
fn diff_stats(diff: &str) -> (u32, u32) {
    let mut added = 0;
    let mut removed = 0;
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
        }
        if !in_hunk && (line.starts_with("+++") || line.starts_with("---")) {
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
    pub(crate) kind: agent::FileChangeKind,
    pub(crate) counts: Option<(u32, u32)>,
    pub(crate) diff: Option<String>,
}

/// The lines a turn's changes add and remove, over the files whose lines
/// could be counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChangeTotals {
    pub(crate) added: u32,
    pub(crate) deleted: u32,
    /// How many files the totals leave out: the provider named them and gave
    /// no lines to count.
    pub(crate) uncounted: usize,
}

impl ChangeTotals {
    pub(crate) fn of(changes: &[agent::FileChange]) -> Self {
        let mut totals = Self {
            added: 0,
            deleted: 0,
            uncounted: 0,
        };
        for change in changes {
            match live_edit_counts(change.diff.as_deref()) {
                Some((added, deleted)) => {
                    totals.added += added;
                    totals.deleted += deleted;
                }
                None => totals.uncounted += 1,
            }
        }
        totals
    }

    /// What a header says beside the totals when they leave files out.
    pub(crate) fn uncounted_label(&self) -> Option<String> {
        (self.uncounted > 0).then(|| {
            crate::tr!("chat.changed_files_uncounted", count = self.uncounted).into_owned()
        })
    }
}

/// The `+N` / `-N` a file's row or chip should display, if any.
///
/// A diff that carries no added or removed lines — absent, empty,
/// whitespace-only, or nothing but `+++`/`---` headers — has nothing truthful
/// to show: "+0 -0" reads as "this edit changed nothing".
pub(crate) fn live_edit_counts(diff: Option<&str>) -> Option<(u32, u32)> {
    let (added, deleted) = diff_stats(diff?);
    (added != 0 || deleted != 0).then_some((added, deleted))
}

/// Expand a file-change snapshot into one live row per file, so a single
/// multi-file entry names every file instead of collapsing to an "N files"
/// label. Paths use the same workspace-relative display as CHANGED FILES.
pub(crate) fn live_edit_rows(changes: &[FileChange], cwd: &Path) -> Vec<LiveEditRow> {
    changes
        .iter()
        .map(|change| LiveEditRow {
            path: crate::workspace_walk::relativize_to_workspace(&change.path, cwd),
            kind: change.kind,
            counts: live_edit_counts(change.diff.as_deref()),
            diff: change.diff.clone(),
        })
        .collect()
}

/// Maximum number of windows kept directly visible at the tail of a live
/// activity run. Older ones move into a separate collapsed Work Log; once
/// prose ends the run, the full run becomes that settled Work Log instead.
pub(crate) const LIVE_ACTIVITY_WINDOW: usize = 5;

pub(crate) fn partition_activity_run<'a>(
    activities: &'a [&'a TimelineEntry],
    live: bool,
) -> (&'a [&'a TimelineEntry], &'a [&'a TimelineEntry]) {
    if !live {
        return activities.split_at(activities.len());
    }
    // The window counts windows, so a computer run is never split.
    let windows = super::tools::run::windows(activities);
    let first_visible = windows.len().saturating_sub(LIVE_ACTIVITY_WINDOW);
    activities.split_at(windows.get(first_visible).map_or(0, |window| window.start))
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
                // Encode a single newline as an explicit break so Markdown
                // cannot fold it into a space in display or selection.
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
            // Backslash-escaped brackets are LaTeX delimiters in Markdown.
            markdown.push_str(&format!("&#{};", ch as u32));
        } else {
            markdown.push(ch);
        }
    }

    markdown
}

/// The expansion key of the Work Log folding a run whose first activity is
/// `segment_id`, an id no other entry of the thread has.
pub(crate) fn work_log_key(segment_id: &str) -> String {
    format!("worklog-{segment_id}")
}

/// The notice the host wrote as a user message.
pub(crate) fn user_notice(content: &EntryContent) -> Option<HostNotice> {
    let (text, _, _) = user_content(content)?;
    host_notice(text)
}

/// The expansion key of each disclosure row a notice renders as, in order.
pub(crate) fn notice_keys(notice: &HostNotice, entry_id: &str) -> Vec<String> {
    match notice {
        HostNotice::Orchestrate(callbacks) => (0..callbacks.len())
            .map(|index| format!("orchestrate-callback-{entry_id}-{index}"))
            .collect(),
        HostNotice::PullRequest { .. } => vec![format!("pull-request-update-{entry_id}")],
        HostNotice::ReportReminder | HostNotice::Control(_) => Vec::new(),
    }
}

/// The per-entry expansion keys of a user message's disclosure rows, one per
/// row its notice renders as. Empty for an ordinary user message, which stays
/// a plain bubble.
pub(crate) fn disclosure_keys(content: &EntryContent, entry_id: &str) -> Vec<String> {
    user_notice(content).map_or_else(Vec::new, |notice| notice_keys(&notice, entry_id))
}

#[cfg(test)]
pub(in crate::chat) mod tests {
    use super::*;
    use agent::{FileChange, FileChangeKind, ItemContent, ItemStatus};
    use std::path::Path;
    use std::sync::Arc;
    use tcode_core::session::{EntryContent, SteeringStatus, TimelineEntry, TurnTiming};

    pub(in crate::chat) const REAL_DIFF: &str = "--- a/src/foo.rs\n\
                             +++ b/src/foo.rs\n\
                             @@ -1,3 +1,4 @@\n\
                             \x20context\n\
                             +added one\n\
                             +added two\n\
                             -removed one\n";

    pub(in crate::chat) fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
        Arc::new(TimelineEntry {
            id: id.to_string(),
            content,
            ts: None,
            finished_ts: None,
            background: false,
            turn: 0,
        })
    }

    pub(in crate::chat) fn user_item(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::UserMessage {
            text: text.into(),
            attachments: Vec::new(),
        })
    }

    pub(in crate::chat) fn assistant(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        })
    }

    pub(in crate::chat) fn reasoning(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::Reasoning { text: text.into() })
    }

    #[test]
    fn provider_start_error_is_localized_only_at_render_boundary() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let generic = EntryContent::Error {
            message: "generic\0原样".into(),
            limit_resets_at: None,
        };
        let provider_start = EntryContent::ProviderStartError {
            error: "spawn failed".into(),
        };

        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(
            displayed_error_text(&generic, "Codex").as_bytes(),
            b"generic\0\xe5\x8e\x9f\xe6\xa0\xb7"
        );
        assert_eq!(
            displayed_error_text(&provider_start, "Codex"),
            "Failed to start Codex: spawn failed"
        );

        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(
            displayed_error_text(&generic, "Codex").as_bytes(),
            b"generic\0\xe5\x8e\x9f\xe6\xa0\xb7"
        );
        assert_eq!(
            displayed_error_text(&provider_start, "Codex"),
            "无法启动 Codex：spawn failed"
        );
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }

    pub(in crate::chat) fn command(id: &str) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::Item(ItemContent::CommandExecution {
                command: id.to_string(),
                input: Vec::new(),
                output: String::new(),
                exit_code: Some(0),
                status: ItemStatus::Completed,
            }),
        )
    }

    /// A subagent, and a thread a call started, stand as cards between the
    /// runs; a call that started nothing stays in its run. The starting
    /// thread's record of the start, which names the started thread, is
    /// left out for the card that tells it; the started thread's own record
    /// stays, as do the records of what changes its runs, one actor's in a
    /// row on one line. A record of what only changed how the thread stands,
    /// as a rename, is left out, and does not part the line around it.
    #[test]
    fn started_agents_and_threads_stand_as_cards_told_once() {
        let start = |id: &str, status, output: Option<&str>| {
            entry(
                id,
                EntryContent::Item(ItemContent::ToolCall {
                    name: "mcp__tcode_control__start_thread".into(),
                    input: serde_json::json!({
                        "provider": "codex",
                        "title": "Migrate the keys",
                        "message": "Migrate the keys.",
                        "request_id": id,
                    }),
                    output: output.map(str::to_owned),
                    status,
                    image_reads: vec![],
                    family: None,
                }),
            )
        };
        let record = |id: &str, action: &str, detail: Option<&str>| {
            entry(
                id,
                EntryContent::ControlAction(agent::ControlAction {
                    actor_thread_id: "lead".into(),
                    actor_title: "Lead".into(),
                    action: action.into(),
                    detail: detail.map(str::to_owned),
                }),
            )
        };
        let entries = [
            command("cmd-1"),
            entry(
                "agent",
                EntryContent::Item(ItemContent::Subagent {
                    agent_type: "explore".into(),
                    description: "Survey the store".into(),
                    status: ItemStatus::InProgress,
                    summary: None,
                    model: None,
                    effort: None,
                }),
            ),
            start(
                "started",
                ItemStatus::Completed,
                Some(
                    r#"{"thread_id":"child","created":true,"title":"Migrate the keys","cwd":"/tmp"}"#,
                ),
            ),
            start("starting", ItemStatus::InProgress, None),
            start(
                "refused",
                ItemStatus::Failed,
                Some("unknown provider: gemini"),
            ),
            record("in-lead", "start_thread", Some("Migrate the keys")),
            command("cmd-2"),
            record("in-child", "start_thread", None),
            record("renamed", "rename", Some("Child — Keys")),
            record("interrupted", "interrupt", None),
        ];
        let segmented = segment_entries(&entries, false);
        assert!(matches!(
            segmented.flow.as_slice(),
            [
                Segment::ActivityRun(first),
                Segment::ThreadCard(agent),
                Segment::ThreadCard(started),
                Segment::ThreadCard(starting),
                Segment::ActivityRun(second),
                Segment::ControlAction(actions),
            ] if first.len() == 1
                && agent.id == "agent"
                && started.id == "started"
                && starting.id == "starting"
                && second.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["refused", "cmd-2"]
                && actions.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["in-child", "interrupted"]
        ));
        assert_eq!(segmented.ranges, vec![0..1, 1..2, 2..3, 3..4, 4..7, 7..10]);
    }

    /// A worktree Tcode made ready for the harness stays out of the flow, the
    /// checkout row showing where the thread works; one it could not make
    /// stands in it.
    #[test]
    fn only_a_worktree_tcode_could_not_make_stands_in_the_flow() {
        let worktree = |id: &str, answer| {
            entry(
                id,
                EntryContent::Worktree {
                    request: agent::WorktreeRequest::Create {
                        cwd: "/repo".into(),
                        name: "fix-upload".into(),
                    },
                    answer,
                },
            )
        };
        let entries = [
            command("cmd-1"),
            worktree(
                "made",
                agent::WorktreeAnswer::Ready {
                    path: "/repo/.worktrees/fix-upload".into(),
                    reused: false,
                },
            ),
            command("cmd-2"),
            worktree(
                "refused",
                agent::WorktreeAnswer::Refused {
                    error: "the branch is checked out elsewhere".into(),
                },
            ),
        ];
        let segmented = segment_entries(&entries, false);
        assert!(matches!(
            segmented.flow.as_slice(),
            [Segment::ActivityRun(run), Segment::Worktree(refused)]
                if run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>()
                    == ["cmd-1", "cmd-2"]
                && refused.id == "refused"
        ));
    }

    #[test]
    fn segmentation_preserves_message_order_and_groups_adjacent_activities() {
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
                    limit_resets_at: None,
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
        let entries = [
            command("cmd"),
            entry(
                "window",
                EntryContent::ContextWindowChanged { window: 500_000 },
            ),
        ];
        let segments = segment_entries(&entries, false).flow;

        assert!(matches!(
            segments.as_slice(),
            [Segment::ActivityRun(activities), Segment::ContextWindowChanged(entry)]
                if activities.len() == 1 && entry.id == "window"
        ));
        // The notes the runtime writes after the message they describe are
        // its row's, which carries them as its tags.
        let entries = [
            entry("user", user_item("go")),
            entry("kept", EntryContent::SettingsKept),
            entry(
                "window",
                EntryContent::ContextWindowChanged { window: 1_000_000 },
            ),
            command("cmd"),
        ];
        let segmented = segment_entries(&entries, false);
        assert!(matches!(
            segmented.flow.as_slice(),
            [Segment::User(user), Segment::ActivityRun(_)] if user.id == "user"
        ));
        assert_eq!(segmented.ranges, vec![0..3, 3..4]);
        assert_eq!(
            message_tags(&entries[0..3]),
            [
                MessageTag::SettingsKept,
                MessageTag::ContextWindow(1_000_000)
            ]
        );
        // What the harness says of the thread as it goes, a warning or a
        // request it tries again, is a line of its own where it was said,
        // while the turn runs and after: never joined to another's place,
        // never an activity of a Work Log.
        let warning = |id: &str| entry(id, EntryContent::Warning { message: id.into() });
        let entries = [
            warning("first"),
            entry(
                "retry",
                EntryContent::RequestRetry {
                    message: "retry 2/10".into(),
                },
            ),
            command("cmd"),
            warning("second"),
            entry("reply", assistant("done")),
        ];
        for running in [false, true] {
            let segmented = segment_entries(&entries, running);
            assert!(matches!(
                segmented.flow.as_slice(),
                [
                    Segment::Warning(first),
                    Segment::RequestRetry(retry),
                    Segment::ActivityRun(run),
                    Segment::Warning(second),
                    Segment::Assistant(_)
                ] if first.id == "first"
                    && retry.id == "retry"
                    && run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>() == ["cmd"]
                    && second.id == "second"
            ));
            assert_eq!(segmented.ranges, vec![0..1, 1..2, 2..3, 3..4, 4..5]);
        }
        let image = |id: &str| {
            entry(
                id,
                EntryContent::Item(ItemContent::ImageRead {
                    path: format!("/tmp/{id}.png"),
                    image: None,
                }),
            )
        };
        let entries = [
            image("one"),
            entry(
                "two",
                EntryContent::Item(ItemContent::ToolCall {
                    name: "Read".into(),
                    input: serde_json::json!({"file_path":"two.png"}),
                    output: None,
                    status: ItemStatus::Completed,
                    image_reads: vec![agent::Attachment {
                        media_type: "image/png".into(),
                        data_base64: String::new(),
                        source_path: None,
                    }],
                    family: None,
                }),
            ),
            command("cmd"),
            image("three"),
            entry("reply", assistant("done")),
        ];
        let segmented = segment_entries(&entries, false);
        assert!(
            matches!(segmented.flow.as_slice(), [Segment::Images(first), Segment::ActivityRun(_), Segment::Images(second), Segment::Assistant(_)]
            if first.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>() == ["one", "two"] && second[0].id == "three")
        );
        assert_eq!(segmented.ranges, vec![0..2, 2..3, 3..4, 4..5]);
        // A call to Tcode's own visualize tool stands in the thread as a
        // card while it runs and once it completed. One that failed drew
        // nothing, and another server's tool of the same name is not
        // Tcode's: both stay activities.
        let visualize = |id: &str, name: &str, status| {
            entry(
                id,
                EntryContent::Item(ItemContent::ToolCall {
                    name: name.into(),
                    input: serde_json::json!({
                        "title": "Latency",
                        "html": "<div><strong>42ms</strong></div>"
                    }),
                    output: None,
                    status,
                    image_reads: vec![],
                    family: None,
                }),
            )
        };
        let own = "mcp__tcode_visualize__visualize";
        let vis_entries = [
            command("cmd"),
            visualize("refused", own, ItemStatus::Failed),
            visualize("foreign", "mcp__charts__visualize", ItemStatus::Completed),
            visualize("drawn", own, ItemStatus::Completed),
            visualize("drawing", own, ItemStatus::InProgress),
            entry("reply", assistant("done")),
        ];
        let segmented = segment_entries(&vis_entries, false);
        assert!(matches!(
            segmented.flow.as_slice(),
            [Segment::ActivityRun(run), Segment::Visualize(drawn), Segment::Visualize(drawing), Segment::Assistant(_)]
            if run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>() == ["cmd", "refused", "foreign"]
                && drawn.id == "drawn" && drawing.id == "drawing"
        ));
        assert_eq!(segmented.ranges, vec![0..3, 3..4, 4..5, 5..6]);
        let segmented = segment_entries(&[], false);
        assert!(segmented.flow.is_empty());
        assert!(segmented.pending_steers.is_empty());
        let entries = [
            command("cmd-1"),
            entry(
                "edit",
                EntryContent::Item(ItemContent::FileChange {
                    changes: vec![],
                    status: ItemStatus::Completed,
                    output: None,
                    command: None,
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

    /// A rewind the harness did not make is told on the message it went to,
    /// and its undo's on the divider of the rewind, until a message is sent.
    #[test]
    fn a_rewind_failure_stands_until_the_next_message() {
        let failed = |id: &str, checkpoint: &str, mode: Option<agent::RewindMode>| {
            entry(
                id,
                EntryContent::RewindFailed {
                    checkpoint_id: checkpoint.into(),
                    mode,
                    failure: agent::RewindFailure::new(agent::RewindFailureReason::Conflicts, None),
                },
            )
        };
        let mode = Some(agent::RewindMode::Conversation);
        let mut entries = vec![
            entry("user", user_item("go")),
            failed("rewind", "c-1", mode),
            failed("undo", "c-0", None),
        ];
        let standing = |entries: &[Arc<TimelineEntry>], checkpoint, undo| {
            standing_rewind_failure(entries, checkpoint, undo).map(|(mode, _)| mode)
        };
        assert_eq!(standing(&entries, "c-1", false), Some(mode));
        assert_eq!(standing(&entries, "c-1", true), None);
        assert_eq!(standing(&entries, "c-0", true), Some(None));
        assert_eq!(standing(&entries, "c-2", false), None);
        entries.push(entry("next", user_item("again")));
        assert_eq!(standing(&entries, "c-1", false), None);
        assert_eq!(standing(&entries, "c-0", true), None);
    }

    #[test]
    fn pending_steers_float_after_live_flow_in_fifo_order_only_while_running() {
        let pending = |id: &str| {
            entry(
                id,
                EntryContent::Steer {
                    text: id.into(),
                    status: SteeringStatus::Pending,
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
    fn reasoning_preserves_nonempty_history_and_coalesces_only_adjacent_empty_placeholders() {
        for running in [true, false] {
            for (entries, expected) in [
                (
                    vec![
                        entry("first", reasoning("first")),
                        entry("latest", reasoning("latest")),
                    ],
                    vec!["first", "latest"],
                ),
                (
                    vec![
                        entry("empty-1", reasoning("")),
                        entry("empty-2", reasoning("  \n")),
                        entry("visible", reasoning("visible")),
                        command("command"),
                        entry("empty-3", reasoning("")),
                        entry("empty-4", reasoning("")),
                    ],
                    vec!["visible", "command", "empty-4"],
                ),
                (
                    vec![
                        entry("reason", reasoning("thinking")),
                        command("later-command"),
                    ],
                    vec!["reason", "later-command"],
                ),
            ] {
                let segments = segment_entries(&entries, running).flow;
                assert!(matches!(segments.as_slice(), [Segment::ActivityRun(run)]
                    if run.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>() == expected));
            }
            let entries = [
                entry("reason", reasoning("thinking")),
                entry("assistant", assistant("answer")),
            ];
            let segments = segment_entries(&entries, running).flow;
            assert!(
                matches!(segments.as_slice(), [Segment::ActivityRun(run), Segment::Assistant(entry)]
                if run.len() == 1 && run[0].id == "reason" && entry.id == "assistant")
            );
        }
    }

    pub(in crate::chat) fn file_change(id: &str, paths: &[&str]) -> Arc<TimelineEntry> {
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
                output: None,
                command: None,
            }),
        )
    }

    #[test]
    fn one_line_collapses_multiline_commands_into_a_single_spaced_line() {
        let cmd = "set pipe -e \"\n  cargo fmt --check\n  cargo clippy\n\"";
        assert_eq!(
            super::one_line(cmd),
            "set pipe -e \" cargo fmt --check cargo clippy \""
        );
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
    fn work_log_labels_localize_prioritized_counts_and_empty_fallbacks() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let all = WorkLogCounts {
            commands: 2,
            files: 3,
            reads: 1,
            searches: 2,
            computer: 4,
            tools: 1,
        };
        let tools = WorkLogCounts {
            tools: 2,
            ..Default::default()
        };
        for (locale, full_label, tools_label, fallback) in [
            (
                crate::LANGUAGE_ENGLISH,
                "1 tool call · 4 computer actions · 3 edits · 2 commands · 1 read · 2 searches",
                "2 tool calls",
                "1 activity",
            ),
            (
                crate::LANGUAGE_SIMPLIFIED_CHINESE,
                "1 次工具调用 · 4 次电脑操作 · 3 处编辑 · 2 条命令 · 1 次读取 · 2 次搜索",
                "2 次工具调用",
                "1 项活动",
            ),
        ] {
            crate::set_locale(locale);
            assert_eq!(work_log_capsule_label(&all, 9), full_label);
            assert_eq!(work_log_capsule_label(&tools, 2), tools_label);
            assert_eq!(work_log_capsule_label(&WorkLogCounts::default(), 0), "");
            assert_eq!(
                work_log_capsule_label(&WorkLogCounts::default(), 1),
                fallback
            );
        }
    }

    #[test]
    fn live_edit_rows_preserve_external_paths_and_report_only_observed_edits() {
        let cases = [
            (
                "/work/repo/src/foo.rs",
                Some(REAL_DIFF),
                "src/foo.rs",
                Some((2, 1)),
            ),
            (
                "/work/repo/crates/ui/src/chat.rs",
                None,
                "crates/ui/src/chat.rs",
                None,
            ),
            (
                "/elsewhere/vendor/bar.rs",
                Some("+only added\n"),
                "/elsewhere/vendor/bar.rs",
                Some((1, 0)),
            ),
            (
                "removed.rs",
                Some("-only removed\n"),
                "removed.rs",
                Some((0, 1)),
            ),
            (
                "literal-signs.txt",
                Some("@@ -1,1 +1,1 @@\n---literal\n+++literal\n"),
                "literal-signs.txt",
                Some((1, 1)),
            ),
            ("empty.rs", Some(""), "empty.rs", None),
            ("blank.rs", Some("   \n\t\n \n"), "blank.rs", None),
            ("headers.rs", Some("--- a/f\n+++ b/f\n"), "headers.rs", None),
            (
                "context.rs",
                Some("--- a/f\n+++ b/f\n@@ -1 +1 @@\n unchanged\n"),
                "context.rs",
                None,
            ),
        ];
        let changes = cases
            .iter()
            .map(|(path, diff, _, _)| FileChange {
                path: (*path).into(),
                kind: FileChangeKind::Modify,
                diff: diff.map(str::to_owned),
            })
            .collect::<Vec<_>>();
        let expected = cases
            .iter()
            .map(|(_, diff, path, counts)| LiveEditRow {
                path: (*path).into(),
                kind: FileChangeKind::Modify,
                counts: *counts,
                diff: diff.map(str::to_owned),
            })
            .collect::<Vec<_>>();
        assert_eq!(live_edit_rows(&changes, Path::new("/work/repo")), expected);
        assert_eq!(diff_stats("--- a/f\n+++ b/f\n"), (0, 0));
        assert_eq!(diff_stats(REAL_DIFF), (2, 1));
    }

    #[test]
    fn finished_activity_runs_use_segment_scoped_counts() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let entries = [
            command("command-1"),
            file_change("files-1", &["src/shared.rs", "src/shared.rs"]),
            file_change("files-2", &["src/shared.rs"]),
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
    fn finished_time_clauses_format_cost_and_name_the_served_model() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);

        let clauses = turn_time_clauses(
            Some("3:04 PM".into()),
            Some(TurnTiming::new(80_000, 35_000)),
            Some(0.12),
            Some("claude-opus-5"),
        );
        assert_eq!(
            clauses
                .iter()
                .map(|clause| (clause.selector, clause.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("turn-time-clock", "3:04 PM"),
                ("turn-time-total", "1m 20s"),
                ("turn-time-cost", "$0.12"),
                ("turn-time-model", "⚠ claude-opus-5"),
            ]
        );

        let sub_cent = turn_time_clauses(Some("3:04 PM".into()), None, Some(0.004), None);
        assert_eq!(
            sub_cent
                .iter()
                .map(|clause| (clause.selector, clause.text.as_str()))
                .collect::<Vec<_>>(),
            vec![("turn-time-clock", "3:04 PM"), ("turn-time-cost", "$0.004"),]
        );

        // A turn with no finish time still says what it cost and which model
        // served it.
        let unclocked = turn_time_clauses(None, None, Some(0.12), Some("claude-opus-5"));
        assert_eq!(
            unclocked
                .iter()
                .map(|clause| (clause.selector, clause.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("turn-time-cost", "$0.12"),
                ("turn-time-model", "⚠ claude-opus-5"),
            ]
        );
    }

    #[test]
    fn finished_time_rows_keep_totals_and_localized_breakdowns() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for (locale, totals, normal_tooltip, ai_only_tooltip) in [
            (
                crate::LANGUAGE_ENGLISH,
                ["1m 20s", "8.0s", "10.5s", "1d 00h 00m"],
                "Model thinking & response 45.0s · Tool calls 35.0s",
                "Model thinking & response 8.0s · Tool calls 0.0s",
            ),
            (
                crate::LANGUAGE_SIMPLIFIED_CHINESE,
                ["1 分 20 秒", "8.0 秒", "10.5 秒", "1 天 00 小时 00 分"],
                "模型思考与回答 45.0 秒 · 工具调用 35.0 秒",
                "模型思考与回答 8.0 秒 · 工具调用 0.0 秒",
            ),
        ] {
            crate::set_locale(locale);
            for ((timing, tooltip), total) in [
                (TurnTiming::new(80_000, 35_000), Some(normal_tooltip)),
                (TurnTiming::new(8_000, 0), Some(ai_only_tooltip)),
                (TurnTiming::new(10_500, 3_600), None),
                (TurnTiming::new(86_459_000, 84_600_000), None),
            ]
            .into_iter()
            .zip(totals)
            {
                let clauses = turn_time_clauses(None, Some(timing), None, None);
                assert_eq!(
                    clauses
                        .iter()
                        .map(|clause| clause.text.as_str())
                        .collect::<Vec<_>>(),
                    [total]
                );
                if let Some(tooltip) = tooltip {
                    assert_eq!(clauses[0].tooltip.as_deref(), Some(tooltip));
                }
            }
        }
    }

    /// Every timing reads in one format, and a long span rolls up into a
    /// larger unit instead of a five-digit count of minutes.
    #[test]
    fn a_span_reads_in_one_format_from_tenths_to_days() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        for (ms, text) in [
            (0, "0.0s"),
            (12_399, "12.3s"),
            (65_299, "1m 05s"),
            (3_600_000, "1h 00m 00s"),
            (90_061_000, "1d 01h 01m"),
            // A work log's span in a thread recorded months apart.
            (7_323_276_000, "84d 18h 14m"),
        ] {
            assert_eq!(format_elapsed(ms), text, "{ms}");
        }
        for (ms, text) in [(15_900, "15s"), (59_999, "59s"), (65_299, "1m 05s")] {
            assert_eq!(format_elapsed_seconds(ms), text, "{ms}");
        }
    }
}
