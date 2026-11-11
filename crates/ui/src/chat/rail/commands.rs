//! The commands of the thread's agent: those running in the background,
//! whichever turn started them, then those its latest turn ran.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use agent::{BackgroundKind, ItemContent, ItemStatus};
use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::session::{EntryContent, Timeline};
use tcode_protocol::BackgroundTaskStatus;

use super::RailView;
use super::stack::{Fit, Fitted};
use crate::chat::background::{Background, Place, Standing};
use crate::chat::model::{one_line, one_line_with_break_markers};
use crate::chat::tools::shell;
use crate::icon::IconName;
use crate::material::CARD_INSET;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::spinner::Spinner;
use crate::widgets::tooltip::{Tooltip, TooltipTrigger as _};

/// How long a row takes to fold in or out, or to move to a new place.
const ROWS: Duration = Duration::from_millis(200);

/// How long a row's mark takes to cross-fade to another state's.
const CHANGE: Duration = Duration::from_millis(200);

/// How a command stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Running,
    /// With the code it exited with when that was not 0: a harness may
    /// count a command that exited non-zero as done.
    Passed(Option<i32>),
    /// With the code it exited with, when its harness reported one.
    Failed(Option<i32>),
    Interrupted,
    Declined,
    /// Running in the background, as the piece of its harness's background
    /// work its row shows.
    Background,
}

impl Mark {
    fn of(status: ItemStatus, exit_code: Option<i32>) -> Self {
        match status {
            ItemStatus::InProgress => Self::Running,
            ItemStatus::Completed => Self::Passed(exit_code.filter(|code| *code != 0)),
            ItemStatus::Failed => Self::Failed(exit_code),
            ItemStatus::Interrupted => Self::Interrupted,
            ItemStatus::Declined => Self::Declined,
        }
    }
}

/// One command as its row shows it.
#[derive(Clone, Debug, PartialEq)]
struct Line {
    /// Its call's id, or the piece's when the harness names no call.
    key: String,
    /// Where its call is in the timeline, by turn.
    call: Option<(usize, String)>,
    /// The command line on one line, or what the harness names the piece.
    command: String,
    /// Whether that is a command line, set in the monospace face.
    mono: bool,
    mark: Mark,
    /// When it started (unix ms), when that was seen.
    started_ms: Option<u64>,
    /// The piece of background work it is.
    background: Option<BackgroundTaskStatus>,
}

/// The commands the card lists, worked out once each time the thread
/// changes rather than every frame the rail is drawn.
#[derive(Default)]
pub(in crate::chat) struct Commands {
    /// Those in the background first, then the running ones, newest first
    /// within each group.
    lines: Rc<[Line]>,
    /// The latest turn's commands that ran in the foreground.
    total: usize,
    running: usize,
    failed: usize,
    /// The pieces still running in the background.
    background: usize,
}

impl Commands {
    /// Lists again the thread's background work that is not an agent, from
    /// `tasks`, and the commands of `timeline`'s latest turn that are not
    /// part of it. A piece is an agent when its harness says so, whether or
    /// not it has named the call yet, or when its call is a subagent's: the
    /// Agents card lists the thread's own, and one a subagent started stands
    /// in that subagent's thread.
    pub(in crate::chat) fn sync(&mut self, timeline: &Timeline, tasks: &[BackgroundTaskStatus]) {
        let mut background = Vec::new();
        for row in tasks.iter().rev() {
            let task = &row.task;
            if task.kind == BackgroundKind::Agent {
                continue;
            }
            let entry = task
                .item_id
                .as_deref()
                .and_then(|item| timeline.entries.iter().find(|entry| entry.id == item));
            if entry.is_some_and(|entry| {
                matches!(
                    entry.content,
                    EntryContent::Item(ItemContent::Subagent { .. })
                )
            }) {
                continue;
            }
            let ran = entry.and_then(|entry| shell::ran(entry));
            background.push(Line {
                key: task.item_id.clone().unwrap_or_else(|| task.id.clone()),
                call: entry.map(|entry| (entry.turn, entry.id.clone())),
                mono: ran.is_some() || task.kind == BackgroundKind::Command,
                command: match ran {
                    Some(ran) => one_line_with_break_markers(&ran.command).0,
                    None => one_line(&task.name),
                },
                mark: Mark::Background,
                started_ms: Some(task.started_at),
                background: Some(row.clone()),
            });
        }
        let mut running = Vec::new();
        let mut settled = Vec::new();
        // Entries are in turn order, so the latest turn's are the last.
        let start = timeline
            .turns
            .len()
            .checked_sub(1)
            .map_or(timeline.entries.len(), |latest| {
                timeline
                    .entries
                    .partition_point(|entry| entry.turn < latest)
            });
        for entry in &timeline.entries[start..] {
            let Some(ran) = shell::ran(entry) else {
                continue;
            };
            if background.iter().any(|line| line.key == entry.id) {
                continue;
            }
            let line = Line {
                key: entry.id.clone(),
                call: Some((entry.turn, entry.id.clone())),
                command: one_line_with_break_markers(&ran.command).0,
                mono: true,
                mark: Mark::of(ran.status, ran.exit_code),
                started_ms: entry.ts,
                background: None,
            };
            if line.mark == Mark::Running {
                running.push(line);
            } else {
                settled.push(line);
            }
        }
        self.total = running.len() + settled.len();
        self.running = running.len();
        self.failed = settled
            .iter()
            .filter(|line| matches!(line.mark, Mark::Failed(_)))
            .count();
        self.background = background
            .iter()
            .filter(|line| line.background.as_ref().is_some_and(|row| !row.ended))
            .count();
        self.lines = background
            .into_iter()
            .chain(running.into_iter().rev())
            .chain(settled.into_iter().rev())
            .collect();
    }
}

/// Whether the card has something to show for `rail`'s thread now.
pub(super) fn present(rail: &RailView, _cx: &App) -> bool {
    !rail.commands.lines.is_empty()
}

/// The line the card's header shows beside its title: how many commands
/// run, or else how many ran; how many run in the background; and how many
/// failed.
pub(super) fn summary(rail: &RailView, _cx: &App) -> Option<SharedString> {
    let commands = &rail.commands;
    let mut parts = Vec::new();
    match (commands.running, commands.total) {
        (0, 0) => {}
        (0, 1) => parts.push(crate::tr!("rail.commands_one").into_owned()),
        (0, total) => parts.push(crate::tr!("rail.commands", count = total).into_owned()),
        (running, _) => {
            parts.push(crate::tr!("rail.commands_running", count = running).into_owned())
        }
    }
    if commands.background > 0 {
        parts
            .push(crate::tr!("rail.commands_background", count = commands.background).into_owned());
    }
    if commands.failed > 0 {
        parts.push(crate::tr!("rail.commands_failed", count = commands.failed).into_owned());
    }
    (!parts.is_empty()).then(|| parts.join(" · ").into())
}

/// The commands the card lists: the thread's, or while the card leaves,
/// those it listed last.
fn listed(rail: &RailView) -> Rc<[Line]> {
    let last = rail
        .motion
        .keep::<Rc<RefCell<Rc<[Line]>>>>("rail-commands-lines");
    if !rail.commands.lines.is_empty() {
        last.replace(rail.commands.lines.clone());
    }
    last.borrow().clone()
}

fn fitted(rail: &RailView) -> Rc<Cell<Fitted>> {
    rail.motion.keep("rail-commands-fitted")
}

/// The card's rows never scroll: in a rail too short for them all it
/// shows as many as fit and counts the rest.
pub(super) fn fit(rail: &RailView, cx: &App) -> Fit {
    Fit {
        rows: listed(rail).len(),
        edge: f32::from(cx.theme().tokens.spacing.sm),
        fitted: fitted(rail),
    }
}

/// What the card shows below its header, laid out at the rail's width: a
/// row for each command that fits the card's height, those in the
/// background first, then the running ones, each with how it stands at its
/// right, bringing its place in the timeline into view on a click; then how
/// many more. Commands that leave the list, or no longer fit, fold away as
/// they stood.
pub(super) fn body(
    rail: &RailView,
    window: &mut Window,
    cx: &mut Context<RailView>,
) -> Option<AnyElement> {
    let lines = listed(rail);
    if lines.is_empty() {
        return None;
    }
    let fitted = fitted(rail);
    let shown = fitted.get().shown.unwrap_or(lines.len()).min(lines.len());
    let more = lines.len() - shown;
    // The tallest row laid out this frame, noted once they all are.
    let tallest = Rc::new(Cell::new(0f32));
    let background = rail.background(cx).zip(rail.background_place(cx));
    let highlight = rail.background_highlight.clone();
    let rows = lines[..shown]
        .iter()
        .map(|line| (SharedString::from(line.key.clone()), line.clone()));
    let rows = crate::motion::keyed(
        "rail-commands",
        rows,
        ROWS,
        window,
        cx,
        |key, line, window, cx| {
            let tallest = tallest.clone();
            // Folding, a row is laid out whole and clipped: this is its
            // height at rest.
            div()
                .w_full()
                .on_children_prepainted(move |bounds, _, _| {
                    for row in bounds {
                        tallest.set(tallest.get().max(f32::from(row.size.height)));
                    }
                })
                .child(row(
                    key,
                    line,
                    background
                        .as_ref()
                        .map(|(background, place)| (background, place)),
                    highlight.as_deref() == Some(line.key.as_str()),
                    window,
                    cx,
                ))
                .into_any_element()
        },
    )
    .render(v_flex().w_full().min_w_0().on_children_prepainted({
        let fitted = fitted.clone();
        move |_, _, _| {
            let row = tallest.replace(0.);
            if row > 0. {
                fitted.set(Fitted {
                    row: Some(row),
                    ..fitted.get()
                });
            }
        }
    }));
    let more = crate::motion::shown(
        "rail-commands-more",
        (more > 0).then_some(more),
        CHANGE,
        window,
        cx,
    )
    .map(|shown| {
        let count = shown.value;
        let fitted = fitted.clone();
        shown.fold(
            div()
                .w_full()
                .on_children_prepainted(move |bounds, _, _| {
                    if let Some(more) = bounds.first() {
                        fitted.set(Fitted {
                            more: Some(f32::from(more.size.height)),
                            ..fitted.get()
                        });
                    }
                })
                .child(
                    div()
                        .debug_selector(|| "rail-commands-more".into())
                        .w_full()
                        .px_1()
                        .pt_1()
                        .text_secondary(false, cx)
                        .child(crate::tr!("rail.commands_more", count = count)),
                ),
        )
    });
    Some(
        v_flex()
            .w_full()
            .min_w_0()
            .px(px(CARD_INSET))
            .pb(cx.theme().tokens.spacing.sm)
            .child(rows)
            .children(more)
            .into_any_element(),
    )
}

fn row(
    key: &SharedString,
    line: &Line,
    background: Option<(&Background, &Place)>,
    highlighted: bool,
    window: &mut Window,
    cx: &mut Context<RailView>,
) -> AnyElement {
    let theme = cx.theme();
    let (muted, danger, warning) = (theme.muted_foreground, theme.danger, theme.warning);
    let standing = line.background.as_ref().map(|row| {
        let waited = background.and_then(|(background, _)| background.waited(row, cx));
        Standing::of(row, waited)
    });
    let ended = standing == Some(Standing::Ended);
    let mark = mark(key, line.mark, ended, window, cx);
    let trailing = match line.mark {
        // A piece of background work says how it stands, with its stop.
        Mark::Background => {
            line.background
                .as_ref()
                .zip(background)
                .map(|(row, (background, place))| {
                    crate::chat::background::controls(
                        background,
                        row,
                        place,
                        &format!("rail-command-bg-{key}"),
                        true,
                        window,
                        cx,
                    )
                })
        }
        Mark::Running => line.started_ms.map(|since| {
            crate::widgets::clock::Clock::since(
                SharedString::from(format!("rail-command-elapsed-{key}")),
                since,
                |elapsed| crate::chat::model::format_elapsed_seconds(elapsed).into(),
            )
            .into_any_element()
        }),
        Mark::Failed(Some(code)) => Some(
            div()
                .text_color(danger)
                .child(crate::tr!("tools.exit_code", code = code))
                .into_any_element(),
        ),
        Mark::Passed(Some(code)) => Some(
            div()
                .text_color(muted)
                .child(crate::tr!("tools.exit_code", code = code))
                .into_any_element(),
        ),
        Mark::Interrupted => Some(
            div()
                .text_color(warning)
                .child(crate::tr!("tools.status.interrupted"))
                .into_any_element(),
        ),
        Mark::Declined => Some(
            div()
                .text_color(muted)
                .child(crate::tr!("tools.status.declined"))
                .into_any_element(),
        ),
        Mark::Passed(None) | Mark::Failed(None) => None,
    };
    let radius = crate::material::radius_card(cx);
    let wash =
        crate::chat::background::wash(&format!("rail-command-{key}"), highlighted, window, cx);
    let label = if line.mark == Mark::Background {
        format!(
            "{} · {}",
            line.command,
            crate::tr!("tools.status.background")
        )
    } else {
        line.command.clone()
    };
    let call = line.call.clone();
    let selector = key.clone();
    crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("rail-command-{key}")),
        Role::Button,
        label,
        cx,
    )
    .debug_selector(move || format!("rail-command-{selector}"))
    .relative()
    .w_full()
    .min_w_0()
    .px_1()
    .py_1()
    .gap_2()
    .items_center()
    .rounded(radius)
    .when_some(wash, |row, wash| row.bg(wash))
    .when(ended, |row| row.opacity(0.6))
    .child(crate::material::row_hover(radius, cx))
    .child(
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .map(|command| {
                if line.mono {
                    command.text_meta_mono(false, cx)
                } else {
                    command.text_body(false, cx)
                }
            })
            .when(ended, |command| command.text_color(muted))
            .child(crate::widgets::fade_text(line.command.clone())),
    )
    .child(
        h_flex()
            .flex_none()
            .gap_1()
            .items_center()
            .whitespace_nowrap()
            .text_secondary(false, cx)
            .children(trailing)
            .child(mark),
    )
    .map(|row| match call {
        Some((turn, entry_id)) => {
            let background = line.mark == Mark::Background;
            row.cursor_pointer()
                .on_click(cx.listener(move |rail, _, _, cx| {
                    rail.timeline.update(cx, |timeline, cx| {
                        // One in the background may be an earlier turn's,
                        // folded into its work log: that opens to show it.
                        if background {
                            timeline.locate(&entry_id, cx)
                        } else {
                            timeline.scroll_to_activity(turn, &entry_id, cx)
                        }
                    });
                }))
        }
        None => row,
    })
    .into_any_element()
}

/// What the mark shows.
#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    Spinner,
    Breathing,
    Settled,
}

/// A spinner while the command runs, a breathing dot while it runs in the
/// background, as its call's window has, then a check, or a cross when it
/// failed; one cross-fades into the next.
fn mark(
    key: &SharedString,
    mark: Mark,
    ended: bool,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let id = SharedString::from(format!("rail-command-mark-{key}"));
    let glyph = match mark {
        Mark::Running => Glyph::Spinner,
        Mark::Background if !ended => Glyph::Breathing,
        _ => Glyph::Settled,
    };
    crate::motion::cross_fade_part(
        id.clone(),
        glyph,
        CHANGE,
        window,
        cx,
        move |glyph, window, cx| {
            let theme = cx.theme();
            match glyph {
                Glyph::Spinner => return Spinner::new().color(theme.primary).into_any_element(),
                Glyph::Breathing => {
                    return div()
                        .id(SharedString::from(format!("{id}-background")))
                        .flex_none()
                        .child(crate::widgets::breathing::BreathingDot::new(theme.primary))
                        .with_tooltip(|_, _| Tooltip::new(crate::tr!("tools.status.background")))
                        .into_any_element();
                }
                Glyph::Settled => {}
            }
            let (icon, color) = match mark {
                Mark::Running | Mark::Passed(_) => (IconName::CircleCheck, theme.success),
                Mark::Failed(_) => (IconName::CircleX, theme.danger),
                Mark::Interrupted => (IconName::CircleAlert, theme.warning),
                Mark::Declined => (IconName::Ban, theme.muted_foreground),
                Mark::Background => (IconName::CircleCheck, theme.muted_foreground),
            };
            crate::motion::status_icon(
                SharedString::from(format!("{id}-icon")),
                icon,
                color,
                CHANGE,
                window,
                cx,
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use agent::{AgentEvent, ThreadItem};

    use super::*;

    /// A command stands as its harness reported it: one its harness counted
    /// as done is not a failure for exiting non-zero, and the code it exited
    /// with stays on its row; one its harness counted as failed is.
    #[test]
    fn a_command_stands_as_its_harness_reported_it() {
        let command = |id: &str, exit_code, status| {
            AgentEvent::ItemCompleted(ThreadItem {
                id: id.into(),
                parent_item_id: None,
                content: ItemContent::CommandExecution {
                    command: "grep -q missing notes.txt".into(),
                    output: String::new(),
                    exit_code,
                    status,
                },
            })
        };
        let timeline = Timeline::fold_events([
            AgentEvent::TurnStarted {
                turn_id: "turn".into(),
            },
            command("done", Some(1), ItemStatus::Completed),
            command("failed", Some(2), ItemStatus::Failed),
        ]);
        let mut commands = Commands::default();
        commands.sync(&timeline, &[]);
        let marks: Vec<_> = commands
            .lines
            .iter()
            .map(|line| (line.key.as_str(), line.mark))
            .collect();
        assert!(
            marks.contains(&("done", Mark::Passed(Some(1)))),
            "{marks:?}"
        );
        assert!(
            marks.contains(&("failed", Mark::Failed(Some(2)))),
            "{marks:?}"
        );
        assert_eq!(commands.failed, 1);
    }

    /// The card lists commands only: background work its harness reports as
    /// an agent is not among them, whether the harness has not named its
    /// call yet or its call is not in this thread, as for an agent one of
    /// the thread's subagents started.
    #[test]
    fn an_agent_in_the_background_is_not_a_command() {
        let task = |id: &str, kind, item_id: Option<&str>| BackgroundTaskStatus {
            task: agent::BackgroundTask {
                id: id.into(),
                name: id.into(),
                kind,
                item_id: item_id.map(str::to_owned),
                started_at: 1,
                started_reported: false,
            },
            listed: true,
            stopping_since: None,
            ended: false,
        };
        let mut commands = Commands::default();
        commands.sync(
            &Timeline::default(),
            &[
                task("a-unnamed", BackgroundKind::Agent, None),
                task("a-nested", BackgroundKind::Agent, Some("toolu_nested")),
                task("b-dev", BackgroundKind::Command, None),
            ],
        );
        let keys: Vec<_> = commands
            .lines
            .iter()
            .map(|line| line.key.as_str())
            .collect();
        assert_eq!(keys, ["b-dev"]);
        assert_eq!(commands.background, 1);
    }
}
