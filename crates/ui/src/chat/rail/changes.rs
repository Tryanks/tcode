//! What the latest finished turn that changed files changed.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use agent::{ChangeCompleteness, FileChange};
use gpui::{AnyElement, App, Context, SharedString, Window};

use crate::chat::ChatView;
use crate::chat::components::changed_files::{ClickHandler, changed_file_rows};
use crate::chat::model::diff_stats;

/// How long the files of one turn take to cross-fade into a later turn's.
const CHANGE: Duration = Duration::from_millis(200);

/// The thread's latest finished turn that changed files, by its index among
/// the turns held.
fn latest(chat: &ChatView, cx: &App) -> Option<usize> {
    chat.workspace_store
        .read(cx)
        .with_timeline_for(
            chat.session_id.as_deref(),
            |timeline| {
                timeline.turns.iter().rposition(|turn| {
                    !turn.running
                        && turn
                            .changes
                            .as_ref()
                            .is_some_and(|changes| !changes.changes.is_empty())
                })
            },
            cx,
        )
        .flatten()
}

/// Whether the card has something to show for `chat`'s thread now.
pub(super) fn present(chat: &ChatView, cx: &App) -> bool {
    latest(chat, cx).is_some()
}

/// A turn's changes as the card last copied them, with what they were
/// copied from.
type HeldChanges = Rc<RefCell<Option<((usize, usize, usize), Rc<[FileChange]>)>>>;

/// A turn's changed-line totals, with what they were counted from.
type Totals = Rc<RefCell<Option<((usize, usize, usize), (u32, u32))>>>;

/// The line the card's header shows beside its title: which turn it is and
/// how long ago it finished, how many files it changed, whether that is all
/// it changed, and how many lines it added and removed.
pub(super) fn summary(chat: &ChatView, cx: &App) -> Option<SharedString> {
    let turn = latest(chat, cx)?;
    // Counting lines reads every diff, so it is done once per turn and set
    // of diffs rather than every frame the header is drawn.
    let totals = chat.motion.keep::<Totals>("rail-changes-totals");
    let store = chat.workspace_store.read(cx);
    let session = chat.session_id.as_deref();
    let (files, partial, ended, (added, deleted)) = store
        .with_timeline_for(
            session,
            |timeline| {
                let at = timeline.turns.get(turn)?;
                let changes = at.changes.as_ref()?;
                let counted = (
                    turn,
                    changes.changes.len(),
                    changes
                        .changes
                        .iter()
                        .map(|change| change.diff.as_deref().map_or(0, str::len))
                        .sum(),
                );
                let mut totals = totals.borrow_mut();
                let sums = match *totals {
                    Some((at, sums)) if at == counted => sums,
                    _ => {
                        let sums =
                            changes
                                .changes
                                .iter()
                                .fold((0, 0), |(added, deleted), change| {
                                    let (a, d) = diff_stats(change.diff.as_deref());
                                    (added + a, deleted + d)
                                });
                        *totals = Some((counted, sums));
                        sums
                    }
                };
                Some((
                    changes.changes.len(),
                    changes.completeness == ChangeCompleteness::Partial,
                    at.end_ts,
                    sums,
                ))
            },
            cx,
        )
        .flatten()?;
    let mut parts = vec![
        crate::tr!(
            "diff.turn",
            count = store.absolute_turn_for(session, turn) + 1
        )
        .into_owned(),
    ];
    parts
        .extend(ended.map(|ms| {
            crate::time::humanize_ago(crate::time::now_secs().saturating_sub(ms / 1000))
        }));
    let mut files = match files {
        1 => crate::tr!("diff.files_changed_one"),
        count => crate::tr!("diff.files_changed", count = count),
    }
    .into_owned();
    if partial {
        files = format!("{files} {}", crate::tr!("chat.changed_files_partial"));
    }
    parts.push(files);
    parts.push(format!("+{added} −{deleted}"));
    Some(parts.join(" · ").into())
}

/// What the card shows below its header, laid out at the rail's width: the
/// turn's changed files one row each, each opening its diff. A later turn's
/// files cross-fade over an earlier one's; a card that leaves shows the turn
/// it last showed.
pub(super) fn body(
    chat: &ChatView,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> Option<AnyElement> {
    let last = chat
        .motion
        .keep::<Rc<Cell<Option<usize>>>>("rail-changes-turn");
    if let Some(turn) = latest(chat, cx) {
        last.set(Some(turn));
    }
    let turn = last.get()?;
    let motion = chat.motion.scope("rail-changes");
    Some(
        chat.motion
            .cross_fade("rail-changes", turn, CHANGE, window, cx, |&turn, _, cx| {
                let (changes, cwd) = {
                    let store = chat.workspace_store.read(cx);
                    let session = chat.session_id.as_deref();
                    // The card is drawn every frame, so the turn's changes,
                    // diffs and all, are copied out only when they change.
                    let held = chat.motion.keep::<HeldChanges>("rail-changes-held");
                    let changes = store
                        .with_timeline_for(
                            session,
                            |timeline| {
                                let changes = &timeline.turns.get(turn)?.changes.as_ref()?.changes;
                                let key = (
                                    turn,
                                    changes.len(),
                                    changes
                                        .iter()
                                        .map(|change| change.diff.as_deref().map_or(0, str::len))
                                        .sum::<usize>(),
                                );
                                let mut held = held.borrow_mut();
                                match &*held {
                                    Some((at, kept)) if *at == key => Some(kept.clone()),
                                    _ => {
                                        let kept: Rc<[FileChange]> = changes.clone().into();
                                        *held = Some((key, kept.clone()));
                                        Some(kept)
                                    }
                                }
                            },
                            cx,
                        )
                        .flatten()
                        .unwrap_or_else(|| Rc::from([]));
                    let cwd = store
                        .session_status_for(session, cx)
                        .map(|status| status.cwd.clone())
                        .unwrap_or_default();
                    (changes, cwd)
                };
                let more_key = "rail:changes-more";
                let toggle_more: ClickHandler = Box::new(cx.listener(move |this, _, _, cx| {
                    if !this.expanded.remove(more_key) {
                        this.expanded.insert(more_key.to_owned());
                    }
                    cx.notify();
                }));
                let open_files = changes
                    .iter()
                    .map(|change| {
                        let path = change.path.clone();
                        Box::new(cx.listener(move |this, _, _, cx| {
                            this.open_diff(turn, Some(path.clone()), cx);
                        })) as ClickHandler
                    })
                    .collect();
                changed_file_rows(
                    turn,
                    &cwd,
                    &changes,
                    chat.expanded.contains(more_key),
                    open_files,
                    toggle_more,
                    &motion,
                    cx,
                )
            }),
    )
}
