//! What the latest finished turn that changed files changed.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gpui::{AnyElement, App, Context, SharedString, Window};

use crate::chat::ChatView;
use crate::chat::components::changed_files::{ChangedFilesHandlers, ClickHandler, changed_files};
use crate::chat::model::diff_stats;

/// How long the files of one turn take to cross-fade into a later turn's.
const CHANGE: Duration = Duration::from_millis(200);

/// The thread's latest finished turn that changed files, by its index among
/// the turns held.
fn latest(chat: &ChatView, cx: &App) -> Option<usize> {
    chat.workspace_store
        .read(cx)
        .with_timeline_for(chat.session_id.as_deref(), |timeline| {
            timeline.turns.iter().rposition(|turn| {
                !turn.running
                    && turn
                        .changes
                        .as_ref()
                        .is_some_and(|changes| !changes.changes.is_empty())
            })
        })
        .flatten()
}

/// Whether the card has something to show for `chat`'s thread now.
pub(super) fn present(chat: &ChatView, cx: &App) -> bool {
    latest(chat, cx).is_some()
}

/// A turn's changed-line totals, with what they were counted from.
type Totals = Rc<RefCell<Option<((usize, usize, usize), (u32, u32))>>>;

/// The line the card's header shows beside its title, open or shut: how
/// many files the turn changed, and how many lines it added and removed.
pub(super) fn summary(chat: &ChatView, cx: &App) -> Option<SharedString> {
    let turn = latest(chat, cx)?;
    // Counting lines reads every diff, so it is done once per turn and set
    // of diffs rather than every frame the header is drawn.
    let totals = chat.motion.keep::<Totals>("rail-changes-totals");
    let (files, (added, deleted)) = chat
        .workspace_store
        .read(cx)
        .with_timeline_for(chat.session_id.as_deref(), |timeline| {
            let changes = &timeline.turns.get(turn)?.changes.as_ref()?.changes;
            let counted = (
                turn,
                changes.len(),
                changes
                    .iter()
                    .map(|change| change.diff.as_deref().map_or(0, str::len))
                    .sum(),
            );
            let mut totals = totals.borrow_mut();
            let sums = match *totals {
                Some((at, sums)) if at == counted => sums,
                _ => {
                    let sums = changes.iter().fold((0, 0), |(added, deleted), change| {
                        let (a, d) = diff_stats(change.diff.as_deref());
                        (added + a, deleted + d)
                    });
                    *totals = Some((counted, sums));
                    sums
                }
            };
            Some((changes.len(), sums))
        })
        .flatten()?;
    let files = match files {
        1 => crate::tr!("diff.files_changed_one"),
        count => crate::tr!("diff.files_changed", count = count),
    };
    Some(format!("{files} · +{added} −{deleted}").into())
}

/// What the card shows open, laid out at the rail's width: the turn's
/// changed files, as the turn's own summary in the timeline shows them, each
/// opening its diff. A later turn's files cross-fade over an earlier one's;
/// a card that leaves shows the turn it last showed.
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
                let (changes, completeness, cwd) = {
                    let store = chat.workspace_store.read(cx);
                    let (changes, completeness) =
                        store.chat_turn_changes_for(chat.session_id.as_deref(), turn);
                    let cwd = store
                        .session_status_for(chat.session_id.as_deref())
                        .map(|status| status.cwd.clone())
                        .unwrap_or_default();
                    (changes, completeness, cwd)
                };
                let more_key = "rail:changes-more";
                let handlers = ChangedFilesHandlers {
                    view_diff: Box::new(cx.listener(move |this, _, _, cx| {
                        this.open_diff(turn, None, cx);
                    })),
                    toggle_more: Box::new(cx.listener(move |this, _, _, cx| {
                        if !this.expanded.remove(more_key) {
                            this.expanded.insert(more_key.to_owned());
                        }
                        cx.notify();
                    })),
                    open_files: changes
                        .iter()
                        .map(|change| {
                            let path = change.path.clone();
                            Box::new(cx.listener(move |this, _, _, cx| {
                                this.open_diff(turn, Some(path.clone()), cx);
                            })) as ClickHandler
                        })
                        .collect(),
                };
                changed_files(
                    turn,
                    &cwd,
                    &changes,
                    completeness,
                    chat.expanded.contains(more_key),
                    handlers,
                    &motion,
                    cx,
                )
            }),
    )
}
