//! The commands the thread's agent ran in its latest turn, in the
//! foreground: one that went on in the background is the Background card's.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use agent::ItemStatus;
use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::session::Timeline;

use super::RailView;
use super::stack::{Fit, Fitted};
use crate::chat::model::one_line_with_break_markers;
use crate::chat::tools::shell;
use crate::icon::IconName;
use crate::material::CARD_INSET;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::spinner::Spinner;

/// How long a row takes to fold in or out, or to move to a new place.
const ROWS: Duration = Duration::from_millis(200);

/// How long a row's mark takes to cross-fade to another state's.
const CHANGE: Duration = Duration::from_millis(200);

/// How a command stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Running,
    Passed,
    /// With the code it exited with, when its harness reported one.
    Failed(Option<i32>),
    Interrupted,
    Declined,
}

impl Mark {
    fn of(status: ItemStatus, exit_code: Option<i32>) -> Self {
        match status {
            ItemStatus::InProgress => Self::Running,
            ItemStatus::Completed if exit_code.is_none_or(|code| code == 0) => Self::Passed,
            ItemStatus::Completed | ItemStatus::Failed => Self::Failed(exit_code),
            ItemStatus::Interrupted => Self::Interrupted,
            ItemStatus::Declined => Self::Declined,
        }
    }
}

/// One command as its row shows it.
#[derive(Clone, Debug, PartialEq)]
struct Line {
    entry_id: String,
    turn: usize,
    /// The command line on one line.
    command: String,
    mark: Mark,
    /// When it started (unix ms), when that was seen.
    started_ms: Option<u64>,
}

/// The commands the card lists, worked out once each time the timeline
/// changes rather than every frame the rail is drawn.
#[derive(Default)]
pub(in crate::chat) struct Commands {
    /// Running first, newest first within each group.
    lines: Rc<[Line]>,
    total: usize,
    running: usize,
    failed: usize,
    /// The latest turn's commands that went on in the background.
    backgrounded: usize,
}

impl Commands {
    /// Lists again the commands of `timeline`'s latest turn that ran in the
    /// foreground, and counts those that went on in the background.
    pub(in crate::chat) fn sync(&mut self, timeline: &Timeline) {
        let mut running = Vec::new();
        let mut settled = Vec::new();
        let mut backgrounded = 0;
        let Some(latest) = timeline.turns.len().checked_sub(1) else {
            *self = Self::default();
            return;
        };
        // Entries are in turn order, so the latest turn's are the last.
        let start = timeline
            .entries
            .partition_point(|entry| entry.turn < latest);
        for entry in &timeline.entries[start..] {
            let Some(ran) = shell::ran(entry) else {
                continue;
            };
            if entry.background {
                backgrounded += 1;
                continue;
            }
            let line = Line {
                entry_id: entry.id.clone(),
                turn: entry.turn,
                command: one_line_with_break_markers(&ran.command).0,
                mark: Mark::of(ran.status, ran.exit_code),
                started_ms: entry.ts,
            };
            if line.mark == Mark::Running {
                running.push(line);
            } else {
                settled.push(line);
            }
        }
        self.total = running.len() + settled.len();
        self.backgrounded = backgrounded;
        self.running = running.len();
        self.failed = settled
            .iter()
            .filter(|line| matches!(line.mark, Mark::Failed(_)))
            .count();
        self.lines = running
            .into_iter()
            .rev()
            .chain(settled.into_iter().rev())
            .collect();
    }
}

/// Whether the card has something to show for `rail`'s thread now.
pub(super) fn present(rail: &RailView, _cx: &App) -> bool {
    rail.commands.total > 0
}

/// The line the card's header shows beside its title: how
/// many commands run, or how many ran, and how many of those failed.
pub(super) fn summary(rail: &RailView, _cx: &App) -> Option<SharedString> {
    let commands = &rail.commands;
    let mut parts = vec![
        match (commands.running, commands.total, commands.backgrounded) {
            (0, 0, _) => return None,
            // With some gone to the background, the count is the turn's.
            (0, total, backgrounded @ 1..) => {
                crate::tr!("rail.commands_turn", count = total + backgrounded).into_owned()
            }
            (0, 1, _) => crate::tr!("rail.commands_one").into_owned(),
            (0, total, _) => crate::tr!("rail.commands", count = total).into_owned(),
            (running, ..) => crate::tr!("rail.commands_running", count = running).into_owned(),
        },
    ];
    if commands.failed > 0 {
        parts.push(crate::tr!("rail.commands_failed", count = commands.failed).into_owned());
    }
    if commands.backgrounded > 0 {
        parts.push(
            crate::tr!("rail.commands_backgrounded", count = commands.backgrounded).into_owned(),
        );
    }
    Some(parts.join(" · ").into())
}

/// The commands the card lists: the thread's, or while the card leaves,
/// those it listed last.
fn listed(rail: &RailView) -> Rc<[Line]> {
    let last = rail
        .motion
        .keep::<Rc<RefCell<Rc<[Line]>>>>("rail-commands-lines");
    if rail.commands.total > 0 {
        last.replace(rail.commands.lines.clone());
    }
    last.borrow().clone()
}

fn fitted(rail: &RailView) -> Rc<Cell<Fitted>> {
    rail.motion.keep("rail-commands-fitted")
}

/// How tall the note under the rows was laid out, which the rows leave room
/// for.
fn note_height(rail: &RailView) -> Rc<Cell<f32>> {
    rail.motion.keep("rail-commands-note")
}

/// The card's rows never scroll: in a rail too short for them all it
/// shows as many as fit and counts the rest.
pub(super) fn fit(rail: &RailView, cx: &App) -> Fit {
    let note = if rail.commands.backgrounded > 0 {
        note_height(rail).get()
    } else {
        0.
    };
    Fit {
        rows: listed(rail).len(),
        edge: f32::from(cx.theme().tokens.spacing.sm) + note,
        fitted: fitted(rail),
    }
}

/// What the card shows below its header, laid out at the rail's width: a
/// row for each command that fits the card's height, running ones first,
/// each with how it stands at its right, bringing its place in the timeline
/// into view on a click; then how many more; then, where some of the turn's
/// commands went on in the background, that the Background card lists
/// them. Commands that leave the list, or no longer fit, fold away as they
/// stood.
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
    let rows = lines[..shown]
        .iter()
        .map(|line| (SharedString::from(line.entry_id.clone()), line.clone()));
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
                .child(row(key, line, window, cx))
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
    let note = (rail.commands.backgrounded > 0).then(|| {
        let height = note_height(rail);
        div()
            .w_full()
            .on_children_prepainted(move |bounds, _, _| {
                if let Some(note) = bounds.first() {
                    height.set(f32::from(note.size.height));
                }
            })
            .child(
                div()
                    .debug_selector(|| "rail-commands-note".into())
                    .w_full()
                    .px_1()
                    .pt_1()
                    .text_secondary(false, cx)
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("rail.commands_backgrounded_note")),
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
            .children(note)
            .into_any_element(),
    )
}

fn row(
    key: &SharedString,
    line: &Line,
    window: &mut Window,
    cx: &mut Context<RailView>,
) -> AnyElement {
    let theme = cx.theme();
    let (muted, danger, warning) = (theme.muted_foreground, theme.danger, theme.warning);
    let mark = mark(key, line.mark, window, cx);
    let trailing = match line.mark {
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
        Mark::Passed | Mark::Failed(None) => None,
    };
    let radius = crate::material::radius_card(cx);
    let (turn, entry_id) = (line.turn, line.entry_id.clone());
    let selector = key.clone();
    crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("rail-command-{key}")),
        Role::Button,
        line.command.clone(),
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
    .cursor_pointer()
    .child(crate::material::row_hover(radius, cx))
    .child(
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .text_meta_mono(false, cx)
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
    .on_click(cx.listener(move |rail, _, _, cx| {
        rail.timeline.update(cx, |timeline, cx| {
            timeline.scroll_to_activity(turn, &entry_id, cx)
        });
    }))
    .into_any_element()
}

/// A spinner while the command runs, then a check, or a cross when it
/// failed; one cross-fades into the next.
fn mark(key: &SharedString, mark: Mark, window: &mut Window, cx: &mut App) -> AnyElement {
    let id = SharedString::from(format!("rail-command-mark-{key}"));
    crate::motion::cross_fade_part(
        id.clone(),
        mark == Mark::Running,
        CHANGE,
        window,
        cx,
        move |running, window, cx| {
            if *running {
                return Spinner::new().color(cx.theme().primary).into_any_element();
            }
            let theme = cx.theme();
            let (icon, color) = match mark {
                Mark::Running | Mark::Passed => (IconName::CircleCheck, theme.success),
                Mark::Failed(_) => (IconName::CircleX, theme.danger),
                Mark::Interrupted => (IconName::CircleAlert, theme.warning),
                Mark::Declined => (IconName::Ban, theme.muted_foreground),
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
