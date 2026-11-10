//! The rewind panel a user message opens, and the divider a rewind leaves.
use std::rc::Rc;
use std::sync::Arc;

use agent::{
    ItemContent, LaterHistory, RewindConflictKind, RewindFailure, RewindFailureReason, RewindMode,
    RewindPreview,
};
use gpui::{
    AnyElement, App, Axis, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{Radio, RadioGroup, h_flex, v_flex};
use tcode_core::session::{EntryContent, Rewound, TimelineEntry};

use crate::scroll::ScrollableElement as _;
use crate::sizing::Sizable as _;
use crate::store::RewindTarget;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

const PANEL_WIDTH: f32 = 340.;
const LIST_MAX_HEIGHT: f32 = 160.;
const REMOVED_MAX_HEIGHT: f32 = 320.;

pub(crate) type Handler = Rc<dyn Fn(&mut Window, &mut App)>;
pub(crate) type SelectHandler = Rc<dyn Fn(RewindMode, &mut Window, &mut App)>;

/// What the Harness answered when asked which files a rewind would restore.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PreviewState {
    Checking,
    Ready(RewindPreview),
    Refused(RewindFailure),
}

impl PreviewState {
    /// The Harness found nothing a files restore would change.
    pub(crate) fn finds_no_files(&self) -> bool {
        match self {
            Self::Ready(preview) => preview.files.files.is_empty() && preview.conflicts.is_empty(),
            Self::Refused(failure) => failure.reason == RewindFailureReason::NothingToRestore,
            Self::Checking => false,
        }
    }
}

/// Everything the open panel shows.
pub(crate) struct Panel {
    pub(crate) target: RewindTarget,
    pub(crate) steer: bool,
    /// The scopes offered, the file ones gone once a preview found no files.
    pub(crate) choices: Vec<RewindMode>,
    pub(crate) selected: RewindMode,
    /// The preview of the selected scope, where the Harness has one for it.
    pub(crate) preview: Option<PreviewState>,
    /// File scopes were withdrawn because the preview found no files.
    pub(crate) no_files: bool,
    pub(crate) files_expanded: bool,
}

pub(crate) struct PanelActions {
    pub(crate) select: SelectHandler,
    pub(crate) toggle_files: Handler,
    pub(crate) confirm: Handler,
    pub(crate) cancel: Handler,
}

pub(crate) fn mode_label(mode: RewindMode) -> String {
    match mode {
        RewindMode::FilesAndConversation => crate::tr!("rewind.mode.files_and_conversation"),
        RewindMode::Conversation => crate::tr!("rewind.mode.conversation"),
        RewindMode::Files => crate::tr!("rewind.mode.files"),
    }
    .into_owned()
}

/// Why no rewind can start now, as the disabled action explains it.
pub(crate) fn blocked_reason(blocked: tcode_protocol::RewindBlocked, harness: &str) -> String {
    match blocked {
        tcode_protocol::RewindBlocked::InProgress => crate::tr!("rewind.blocked.in_progress"),
        tcode_protocol::RewindBlocked::Delivering => {
            crate::tr!("rewind.blocked.delivering", harness = harness)
        }
    }
    .into_owned()
}

fn counted(key_one: &str, key: &str, count: usize) -> String {
    if count == 1 {
        crate::i18n::translate(key_one).into_owned()
    } else {
        crate::i18n::translate_with_args(key, &["count"], &[count.to_string()]).into_owned()
    }
}

/// What the rewind will do to the conversation, in the selected scope.
fn summary(panel: &Panel) -> String {
    let target = &panel.target;
    if !panel.selected.includes_conversation() {
        return crate::tr!("rewind.summary.files_only").into_owned();
    }
    if target.rewind.later_history == LaterHistory::Branch {
        return crate::tr!("rewind.summary.branch").into_owned();
    }
    if panel.steer {
        return crate::tr!("rewind.summary.steer").into_owned();
    }
    let count = target.later_turns.max(1);
    match target.rewind.later_history {
        LaterHistory::HiddenUntilNextSend => {
            counted("rewind.summary.hidden_one", "rewind.summary.hidden", count)
        }
        _ => counted(
            "rewind.summary.truncated_one",
            "rewind.summary.truncated",
            count,
        ),
    }
}

/// The panel's body: what happens, the scopes the Harness restores, what it
/// would restore, the limits, and Rewind / Cancel.
pub(crate) fn panel_body(
    panel: Panel,
    actions: PanelActions,
    compact: bool,
    cx: &App,
) -> AnyElement {
    let harness = panel.target.harness.clone();
    let note = |text: String| div().w_full().text_secondary(compact, cx).child(text);
    let header = (!compact).then(|| {
        v_flex()
            .gap_0p5()
            .child(
                div()
                    .text_title(cx)
                    .child(crate::tr!("rewind.title").into_owned()),
            )
            .child(note(
                crate::tr!("rewind.via", harness = harness.as_str()).into_owned(),
            ))
    });
    let choices = (panel.choices.len() > 1).then(|| {
        let count = panel.choices.len();
        let group = panel.choices.iter().enumerate().fold(
            RadioGroup::new("rewind-choices")
                .axis(Axis::Vertical)
                .aria_label(crate::tr!("rewind.restore").into_owned()),
            |group, (index, &mode)| {
                let select = actions.select.clone();
                let checked = mode == panel.selected;
                group.child(choice_row(mode, checked, index, count, select, compact, cx))
            },
        );
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_caption(cx)
                    .child(crate::tr!("rewind.restore").into_owned()),
            )
            .child(group)
            .into_any_element()
    });
    // One scope is no choice: it is named, not offered.
    let choices = choices.or_else(|| {
        let mode = panel.selected;
        Some(
            h_flex()
                .gap_1()
                .debug_selector(move || format!("rewind-option-{mode:?}"))
                .child(
                    div()
                        .text_caption(cx)
                        .child(crate::tr!("rewind.restore").into_owned()),
                )
                .child(div().text_secondary(compact, cx).child(mode_label(mode)))
                .into_any_element(),
        )
    });
    let preview = panel
        .preview
        .as_ref()
        .map(|preview| preview_block(preview, &harness, panel.files_expanded, &actions, cx));
    let includes_files = panel.selected.includes_files();
    let target = &panel.target;
    let notes = [
        (panel.no_files).then(|| crate::tr!("rewind.preview.no_files").into_owned()),
        (includes_files && !target.rewind.restores_command_edits).then(|| {
            crate::tr!("rewind.note.command_edits", harness = harness.as_str()).into_owned()
        }),
        (includes_files && target.worktree)
            .then(|| crate::tr!("rewind.note.worktree").into_owned()),
        (target.turn_running && target.rewind.running_turn == agent::RunningTurnPolicy::Interrupt)
            .then(|| crate::tr!("rewind.note.running", harness = harness.as_str()).into_owned()),
        (target.queued > 0).then(|| {
            counted(
                "rewind.note.queued_one",
                "rewind.note.queued",
                target.queued,
            )
        }),
        (target.delegated_after > 0).then(|| {
            counted(
                "rewind.note.delegated_one",
                "rewind.note.delegated",
                target.delegated_after,
            )
        }),
        panel
            .selected
            .includes_conversation()
            .then(|| crate::tr!("rewind.summary.prompt_back").into_owned()),
    ];
    let destructive = panel.selected.includes_conversation()
        && target.rewind.later_history == LaterHistory::Truncated;
    let confirm = Button::new("rewind-confirm")
        .label(crate::tr!("rewind.confirm").into_owned())
        .touch(compact)
        .when(destructive, |button| button.danger())
        .when(!destructive, |button| button.primary())
        .on_click({
            let confirm = actions.confirm.clone();
            move |_, window, cx| confirm(window, cx)
        });
    let cancel = Button::new("rewind-cancel")
        .label(crate::tr!("rewind.cancel").into_owned())
        .touch(compact)
        .outline()
        .on_click({
            let cancel = actions.cancel.clone();
            move |_, window, cx| cancel(window, cx)
        });
    v_flex()
        .id("rewind-panel")
        .debug_selector(|| "rewind-panel".into())
        .when(!compact, |panel| panel.w(px(PANEL_WIDTH)))
        .w_full()
        .p_3()
        .gap_3()
        .children(header)
        .child(div().w_full().text_body(compact, cx).child(summary(&panel)))
        .children(choices)
        .children(preview)
        .child(
            v_flex()
                .gap_1()
                .children(notes.into_iter().flatten().map(note)),
        )
        .child(
            h_flex()
                .w_full()
                .justify_end()
                .gap_2()
                .child(cancel)
                .child(confirm),
        )
        .into_any_element()
}

fn choice_row(
    mode: RewindMode,
    checked: bool,
    index: usize,
    count: usize,
    select: SelectHandler,
    compact: bool,
    cx: &App,
) -> Radio {
    let ring = if checked {
        cx.theme().primary
    } else {
        cx.theme().border
    };
    let indicator = div()
        .size(px(14.))
        .flex_none()
        .rounded_full()
        .border_1()
        .border_color(ring)
        .flex()
        .items_center()
        .justify_center()
        .when(checked, |indicator| {
            indicator.child(div().size(px(6.)).rounded_full().bg(ring))
        });
    Radio::new(SharedString::from(format!("rewind-option-{mode:?}")))
        .checked(checked)
        .set_position(index + 1, count)
        .accessibility_label(mode_label(mode))
        .on_change(move |chosen, _, window, cx| {
            if chosen {
                select(mode, window, cx);
            }
        })
        .debug_selector(move || format!("rewind-option-{mode:?}"))
        .w_full()
        .cursor_pointer()
        .rounded(crate::material::radius_button(cx))
        .px_2()
        .py(if compact { px(8.) } else { px(4.) })
        .hover(|row| row.bg(cx.theme().muted))
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .text_body(compact, cx)
                .child(indicator)
                .child(mode_label(mode)),
        )
}

fn line_counts(insertions: Option<u64>, deletions: Option<u64>, cx: &App) -> Option<AnyElement> {
    (insertions.is_some() || deletions.is_some()).then(|| {
        h_flex()
            .gap_1()
            .flex_none()
            .text_token(&cx.theme().tokens.typography.xs)
            .children(insertions.map(|count| {
                div()
                    .text_color(cx.theme().success)
                    .child(format!("+{count}"))
            }))
            .children(deletions.map(|count| {
                div()
                    .text_color(cx.theme().danger)
                    .child(format!("−{count}"))
            }))
            .into_any_element()
    })
}

fn conflict_label(kind: RewindConflictKind) -> String {
    match kind {
        RewindConflictKind::ModifiedExternally => {
            crate::tr!("rewind.preview.conflict.modified_externally")
        }
        RewindConflictKind::CreatedExternally => {
            crate::tr!("rewind.preview.conflict.created_externally")
        }
        RewindConflictKind::DeletedExternally => {
            crate::tr!("rewind.preview.conflict.deleted_externally")
        }
        RewindConflictKind::Other => crate::tr!("rewind.preview.conflict.other"),
    }
    .into_owned()
}

fn path_list(rows: Vec<AnyElement>, id: &'static str) -> AnyElement {
    div()
        .id(id)
        .w_full()
        .max_h(px(LIST_MAX_HEIGHT))
        .overflow_y_scroll_area()
        .child(v_flex().w_full().gap_0p5().children(rows))
        .into_any_element()
}

fn preview_block(
    preview: &PreviewState,
    harness: &str,
    expanded: bool,
    actions: &PanelActions,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let xs = &cx.theme().tokens.typography.xs;
    let mono = cx.theme().mono_font_family.clone();
    match preview {
        PreviewState::Checking => h_flex()
            .gap_2()
            .items_center()
            .debug_selector(|| "rewind-preview-checking".into())
            .text_token(xs)
            .text_color(muted)
            .child(crate::widgets::Spinner::new().xsmall().color(muted))
            .child(crate::tr!("rewind.preview.checking").into_owned())
            .into_any_element(),
        PreviewState::Refused(failure) => div()
            .text_token(xs)
            .text_color(cx.theme().warning)
            .child(
                crate::tr!(
                    "rewind.preview.unavailable",
                    reason = super::dividers::rewind_reason(failure.reason, harness)
                )
                .into_owned(),
            )
            .into_any_element(),
        PreviewState::Ready(preview) => {
            let files = &preview.files;
            let summary = (!files.files.is_empty()).then(|| {
                let toggle = Button::new("rewind-preview-files")
                    .text()
                    .xsmall()
                    .aria_expanded(expanded)
                    .label(if expanded {
                        crate::tr!("rewind.preview.hide_files").into_owned()
                    } else {
                        crate::tr!("rewind.preview.show_files").into_owned()
                    })
                    .on_click({
                        let toggle = actions.toggle_files.clone();
                        move |_, window, cx| toggle(window, cx)
                    });
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .debug_selector(|| "rewind-preview-files".into())
                    .text_token(xs)
                    .child(counted(
                        "rewind.preview.files_one",
                        "rewind.preview.files",
                        files.files.len(),
                    ))
                    .children(line_counts(files.insertions, files.deletions, cx))
                    .child(div().flex_1())
                    .child(toggle)
            });
            let list = (expanded && !files.files.is_empty()).then(|| {
                path_list(
                    files
                        .files
                        .iter()
                        .map(|file| {
                            h_flex()
                                .w_full()
                                .gap_2()
                                .text_token(xs)
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .overflow_hidden()
                                        .font_family(mono.clone())
                                        .text_color(muted)
                                        .child(file.path.clone()),
                                )
                                .children(line_counts(file.insertions, file.deletions, cx))
                                .into_any_element()
                        })
                        .collect(),
                    "rewind-preview-file-list",
                )
            });
            let conflicts = (!preview.conflicts.is_empty()).then(|| {
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .debug_selector(|| "rewind-preview-conflicts".into())
                    .child(
                        div()
                            .text_token(xs)
                            .text_color(cx.theme().warning)
                            .child(counted(
                                "rewind.preview.conflicts_one",
                                "rewind.preview.conflicts",
                                preview.conflicts.len(),
                            )),
                    )
                    .child(path_list(
                        preview
                            .conflicts
                            .iter()
                            .map(|conflict| {
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .text_token(xs)
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .overflow_hidden()
                                            .font_family(mono.clone())
                                            .child(conflict.path.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(muted)
                                            .child(conflict_label(conflict.kind)),
                                    )
                                    .into_any_element()
                            })
                            .collect(),
                        "rewind-preview-conflict-list",
                    ))
            });
            let skipped = (files.skipped_links > 0).then(|| {
                div().text_token(xs).text_color(muted).child(
                    crate::tr!(
                        "rewind.preview.skipped_links",
                        count = files.skipped_links.to_string()
                    )
                    .into_owned(),
                )
            });
            v_flex()
                .w_full()
                .gap_1()
                .children(summary)
                .children(list)
                .children(conflicts)
                .children(skipped)
                .into_any_element()
        }
    }
}

/// The way back a divider offers: the undo while the Harness keeps the rewind
/// undoable, then the branch it left, in a thread of its own.
pub(crate) enum DividerUndo {
    Available(Handler),
    InProgress,
    OpenBranch(Handler),
}

/// The divider a rewind leaves: what it did, the turns it removed to read
/// back (only what Tcode's own log kept; nothing is sent to the Harness),
/// and the Harness's undo while it lasts.
pub(crate) fn rewound_divider(
    entry: &TimelineEntry,
    expanded: bool,
    on_toggle: Handler,
    undo: Option<DividerUndo>,
    harness: String,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let EntryContent::Rewound(rewound) = &entry.content else {
        return super::dividers::rewind_divider(entry, &harness, cx);
    };
    let key = entry.id.clone();
    let label = super::dividers::rewound_label(rewound, &harness);
    let tint = if rewound.incomplete.is_some() {
        cx.theme().warning
    } else {
        cx.theme().muted_foreground
    };
    let removable = !rewound.removed.is_empty();
    let branch = rewound.later_history == LaterHistory::Branch;
    let rewound = Arc::new((**rewound).clone());
    let motion = motion.clone();
    crate::motion::Sampled::new(move |window, cx| {
        let label_element = if removable {
            Button::new(SharedString::from(format!("rewound-toggle-{key}")))
                .motion(&motion)
                .ghost()
                .small()
                .aria_expanded(expanded)
                .text_token(&cx.theme().tokens.typography.xs)
                .icon(
                    crate::widgets::Chevron::owned_icon(
                        &motion,
                        SharedString::from(format!("rewound-chevron-{key}")),
                        expanded,
                        window,
                        cx,
                    )
                    .text_color(tint),
                )
                .child(div().text_color(tint).child(label.clone()))
                .on_click({
                    let on_toggle = on_toggle.clone();
                    move |_, window, cx| on_toggle(window, cx)
                })
                .debug_selector({
                    let selector = format!("rewound-toggle-{key}");
                    move || selector
                })
                .into_any_element()
        } else {
            div()
                .text_token(&cx.theme().tokens.typography.xs)
                .text_color(tint)
                .child(label.clone())
                .into_any_element()
        };
        let undo = undo.as_ref().map(|undo| match undo {
            DividerUndo::Available(on_undo) => {
                Button::new(SharedString::from(format!("rewound-undo-{key}")))
                    .text()
                    .xsmall()
                    .label(if branch {
                        crate::tr!("rewind.return_to_branch").into_owned()
                    } else {
                        crate::tr!("rewind.undo").into_owned()
                    })
                    .on_click({
                        let on_undo = on_undo.clone();
                        move |_, window, cx| on_undo(window, cx)
                    })
                    .debug_selector({
                        let selector = if branch {
                            format!("rewound-return-{key}")
                        } else {
                            format!("rewound-undo-{key}")
                        };
                        move || selector
                    })
                    .into_any_element()
            }
            DividerUndo::OpenBranch(on_open) => {
                Button::new(SharedString::from(format!("rewound-open-branch-{key}")))
                    .text()
                    .xsmall()
                    .label(crate::tr!("rewind.open_branch").into_owned())
                    .on_click({
                        let on_open = on_open.clone();
                        move |_, window, cx| on_open(window, cx)
                    })
                    .debug_selector({
                        let selector = format!("rewound-open-branch-{key}");
                        move || selector
                    })
                    .into_any_element()
            }
            DividerUndo::InProgress => div()
                .text_token(&cx.theme().tokens.typography.xs)
                .text_color(cx.theme().muted_foreground)
                .child(crate::tr!("rewind.undoing").into_owned())
                .into_any_element(),
        });
        let row = h_flex()
            .id(SharedString::from(format!("rewound-{key}")))
            .debug_selector({
                let selector = format!("rewound-{key}");
                move || selector
            })
            .w_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(super::dividers::divider_stub(cx))
            .child(label_element)
            .when_some(undo, |row, undo| {
                row.child(
                    div()
                        .text_token(&cx.theme().tokens.typography.xs)
                        .text_color(cx.theme().muted_foreground)
                        .child("·"),
                )
                .child(undo)
            })
            .child(super::dividers::divider_stub(cx));
        let body = motion
            .shown(
                format!("rewound-fold-{key}"),
                expanded.then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|fold| fold.fold(removed_turns(&key, &rewound, &harness, cx)));
        v_flex()
            .w_full()
            .gap_1()
            .child(row)
            .children(body)
            .into_any_element()
    })
    .into_any_element()
}

/// The turns a rewind removed, read-only: who said what, and how much work
/// went on between.
fn removed_turns(key: &str, rewound: &Rewound, harness: &str, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let mut rows: Vec<AnyElement> = Vec::new();
    let mut work = 0usize;
    let flush = |work: &mut usize, rows: &mut Vec<AnyElement>| {
        if *work > 0 {
            rows.push(
                div()
                    .text_token(&cx.theme().tokens.typography.xs)
                    .text_color(muted)
                    .child(if *work == 1 {
                        crate::tr!("chat.work_log_tool_one").into_owned()
                    } else {
                        crate::tr!("chat.work_log_tools", count = work.to_string()).into_owned()
                    })
                    .into_any_element(),
            );
            *work = 0;
        }
    };
    let said = |who: String, text: &str| {
        v_flex()
            .w_full()
            .gap_0p5()
            .child(
                div()
                    .text_token(&cx.theme().tokens.typography.xs)
                    .text_color(muted)
                    .child(who),
            )
            .child(div().w_full().text_body(false, cx).child(text.to_owned()))
            .into_any_element()
    };
    for entry in &rewound.removed {
        match &entry.content {
            EntryContent::Item(ItemContent::UserMessage {
                text, context_len, ..
            })
            | EntryContent::Steer {
                text, context_len, ..
            } => {
                flush(&mut work, &mut rows);
                let visible = context_len
                    .and_then(|len| text.get(len..))
                    .unwrap_or(text)
                    .trim();
                rows.push(said(crate::tr!("rewind.you").into_owned(), visible));
            }
            EntryContent::Feedback { text } => {
                flush(&mut work, &mut rows);
                rows.push(said(crate::tr!("rewind.you").into_owned(), text.trim()));
            }
            EntryContent::Item(ItemContent::AssistantMessage { text, .. })
            | EntryContent::Item(ItemContent::Plan { markdown: text })
                if !text.trim().is_empty() =>
            {
                flush(&mut work, &mut rows);
                rows.push(said(harness.to_owned(), text.trim()));
            }
            EntryContent::Item(
                ItemContent::CommandExecution { .. }
                | ItemContent::FileChange { .. }
                | ItemContent::ToolCall { .. }
                | ItemContent::Subagent { .. }
                | ItemContent::ImageRead { .. }
                | ItemContent::WebSearch { .. }
                | ItemContent::Other { .. },
            ) => work += 1,
            _ => {}
        }
    }
    flush(&mut work, &mut rows);
    crate::material::card(cx)
        .w_full()
        .block_mouse_except_scroll()
        .child(
            div()
                .id(SharedString::from(format!("rewound-removed-{key}")))
                .debug_selector({
                    let selector = format!("rewound-removed-{key}");
                    move || selector
                })
                .w_full()
                .max_h(px(REMOVED_MAX_HEIGHT))
                .overflow_y_scroll_area()
                .child(v_flex().w_full().gap_3().opacity(0.8).children(rows)),
        )
        .into_any_element()
}
