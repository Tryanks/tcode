use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use agent::ProviderKind;
use gpui::{
    AnyElement, App, Div, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, Styled as _, div, px,
};
use gpui_base::h_flex;

/// How long a divider's label takes to cross-fade to its next one.
const LABEL: std::time::Duration = std::time::Duration::from_millis(200);

/// Width of the hairline stub flanking a divider label. Short on purpose: a
/// full-width rule would cut the flow in two, and these events only annotate it.
const STUB_WIDTH: f32 = 24.;

/// One hairline stub of the divider grammar, shared with the centered
/// disclosure rows so every ambient notification flanks its label identically.
pub(crate) fn divider_stub(cx: &App) -> Div {
    div().h(px(1.)).w(px(STUB_WIDTH)).bg(cx.theme().border)
}

/// The one divider grammar: a centered small label between two hairline stubs.
/// `tint` colors the label alone — the rules stay border-quiet no matter what
/// the event is, so no divider can shout louder than another.
fn divider(id: SharedString, label: String, tint: Hsla, cx: &App) -> AnyElement {
    divider_in(id, label, tint, None, cx)
}

/// [`divider`] whose label changes (compaction running, then done) and
/// cross-fades as it does, on `motion`'s clock when the chat keeps it.
fn divider_in(
    id: SharedString,
    label: String,
    tint: Hsla,
    motion: Option<&crate::motion::owned::Owned>,
    cx: &App,
) -> AnyElement {
    let stub = || divider_stub(cx);
    let label_id = SharedString::from(format!("{id}-label"));
    let motion = motion.cloned();
    let label = crate::motion::Sampled::new(move |window, cx| {
        let build = |label: &String, _: &mut gpui::Window, _: &mut App| {
            div().child(label.clone()).into_any_element()
        };
        crate::motion::Clock::from(motion.as_ref()).cross_fade(
            label_id,
            label,
            &crate::motion::Curve::new(LABEL),
            window,
            cx,
            build,
        )
    });
    h_flex()
        .id(id)
        .w_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(stub())
        .child(
            div()
                .min_w_0()
                .flex_shrink_1()
                .text_center()
                .text_token(&cx.theme().tokens.typography.xs)
                .text_color(tint)
                .child(label),
        )
        .child(stub())
        .into_any_element()
}

pub(crate) fn relay_divider(
    id: &str,
    from: ProviderKind,
    to: ProviderKind,
    moved_from: Option<&std::path::Path>,
    cx: &App,
) -> AnyElement {
    let label = match moved_from {
        Some(dir) => crate::tr!(
            "chat.moved",
            from = dir.to_string_lossy(),
            to = to.display_name()
        ),
        None => crate::tr!(
            "chat.relayed",
            from = from.display_name(),
            to = to.display_name()
        ),
    };
    divider(
        SharedString::from(format!("relay-{id}")),
        label.into_owned(),
        cx.theme().muted_foreground,
        cx,
    )
}

pub(crate) fn model_change_divider(
    id: &str,
    from: Option<&str>,
    to: &str,
    reason: Option<&str>,
    cx: &App,
) -> AnyElement {
    let label = match from {
        Some(from) => crate::tr!("chat.model_changed", from = from, to = to).into_owned(),
        None => crate::tr!("chat.model_changed_to", to = to).into_owned(),
    };
    let label = match reason {
        Some(reason) if !reason.is_empty() => format!("{label} ({reason})"),
        _ => label,
    };
    // A model swap is worth noticing, so the text carries a warning tint — but
    // nothing else about the row differs from a relay.
    divider(
        SharedString::from(format!("model-change-{id}")),
        label,
        cx.theme().warning,
        cx,
    )
}

/// Context compaction rewrites what the model remembers, so it announces
/// itself at model-swap prominence rather than hiding in the work log.
pub(crate) fn context_compacted_divider(
    id: &str,
    compaction: &agent::Compaction,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let mut label = if compaction.in_progress {
        crate::tr!("chat.context_compacting").into_owned()
    } else {
        crate::tr!("chat.context_compacted").into_owned()
    };
    if let Some(trigger) = &compaction.trigger {
        let trigger = match trigger.as_str() {
            "manual" => crate::tr!("chat.context_manual").into_owned(),
            "auto" => crate::tr!("chat.context_auto").into_owned(),
            other => other.to_owned(),
        };
        label.push_str(&format!(" · {trigger}"));
    }
    if let Some(tokens) = compaction.pre_tokens {
        label.push_str(&format!(
            " · {}",
            crate::tr!(
                "chat.context_pre_tokens",
                tokens = crate::context_meter::format_tokens(Some(tokens))
            )
        ));
    }
    divider_in(
        SharedString::from(format!("context-compacted-{id}")),
        label,
        cx.theme().warning,
        Some(motion),
        cx,
    )
}

pub(crate) fn control_action_divider(
    id: &str,
    action: &agent::ControlAction,
    cx: &App,
) -> AnyElement {
    let mut label = crate::tr!(
        "chat.control_action",
        actor = action.actor_title.as_str(),
        action = crate::control::action_label(&action.action)
    )
    .into_owned();
    if let Some(detail) = action.detail.as_deref().filter(|detail| !detail.is_empty()) {
        label.push_str(&format!(" ({detail})"));
    }
    divider(
        SharedString::from(format!("control-action-{id}")),
        label,
        cx.theme().muted_foreground,
        cx,
    )
}

pub(crate) fn context_window_changed_divider(id: &str, window: u64, cx: &App) -> AnyElement {
    divider(
        SharedString::from(format!("context-window-changed-{id}")),
        crate::tr!(
            "chat.context_window_changed",
            window = agent::context_window::format(window)
        )
        .into_owned(),
        cx.theme().muted_foreground,
        cx,
    )
}

pub(crate) fn settings_kept_divider(id: &str, cx: &App) -> AnyElement {
    divider(
        SharedString::from(format!("settings-kept-{id}")),
        crate::tr!("chat.settings_kept").into_owned(),
        cx.theme().muted_foreground,
        cx,
    )
}

pub(crate) fn session_moved_divider(
    id: &str,
    from: &std::path::Path,
    to: &std::path::Path,
    cx: &App,
) -> AnyElement {
    divider(
        SharedString::from(format!("moved-{id}")),
        crate::tr!(
            "chat.session_moved",
            from = from.to_string_lossy(),
            to = to.to_string_lossy()
        )
        .into_owned(),
        cx.theme().muted_foreground,
        cx,
    )
}

/// What Tcode did with a worktree the harness asked it for.
pub(crate) fn worktree_divider(
    id: &str,
    request: &agent::WorktreeRequest,
    answer: &agent::WorktreeAnswer,
    cx: &App,
) -> AnyElement {
    use agent::{WorktreeAnswer, WorktreeRequest};
    let (label, tint) = match (request, answer) {
        (WorktreeRequest::Create { name, .. }, WorktreeAnswer::Ready { path, reused }) => (
            if *reused {
                crate::tr!(
                    "chat.worktree_reused",
                    name = name,
                    path = path.to_string_lossy()
                )
            } else {
                crate::tr!(
                    "chat.worktree_created",
                    name = name,
                    path = path.to_string_lossy()
                )
            },
            cx.theme().muted_foreground,
        ),
        (WorktreeRequest::Create { name, .. }, WorktreeAnswer::Refused { error }) => (
            crate::tr!("chat.worktree_create_refused", name = name, error = error),
            cx.theme().warning,
        ),
        (WorktreeRequest::Remove { path }, WorktreeAnswer::Refused { error }) => (
            crate::tr!(
                "chat.worktree_remove_refused",
                path = path.to_string_lossy(),
                error = error
            ),
            cx.theme().warning,
        ),
        (WorktreeRequest::Remove { path }, _) => (
            crate::tr!("chat.worktree_removed", path = path.to_string_lossy()),
            cx.theme().muted_foreground,
        ),
        (WorktreeRequest::Create { name, .. }, WorktreeAnswer::Removed) => (
            crate::tr!("chat.worktree_removed", path = name),
            cx.theme().muted_foreground,
        ),
    };
    divider(
        SharedString::from(format!("worktree-{id}")),
        label.into_owned(),
        tint,
        cx,
    )
}

/// Why a harness did not rewind, in the user's words.
pub(crate) fn rewind_reason(reason: agent::RewindFailureReason, harness: &str) -> String {
    use agent::RewindFailureReason as Reason;
    match reason {
        Reason::TurnRunning => crate::tr!("rewind.reason.turn_running"),
        Reason::Busy => crate::tr!("rewind.reason.busy", harness = harness),
        Reason::TargetNotFound => crate::tr!("rewind.reason.target_not_found"),
        Reason::ConversationChanged => crate::tr!("rewind.reason.conversation_changed"),
        Reason::CannotCutHere => crate::tr!("rewind.reason.cannot_cut_here"),
        Reason::NothingToRestore => crate::tr!("rewind.reason.nothing_to_restore"),
        Reason::Conflicts => crate::tr!("rewind.reason.conflicts"),
        Reason::UsageLimited => crate::tr!("rewind.reason.usage_limited"),
        Reason::Expired => crate::tr!("rewind.reason.expired"),
        Reason::SaveFailed => crate::tr!("rewind.reason.save_failed", harness = harness),
        Reason::Unsupported => crate::tr!("rewind.reason.unsupported"),
        Reason::ProcessGone => crate::tr!("rewind.reason.process_gone", harness = harness),
        Reason::Other => crate::tr!("rewind.reason.other", harness = harness),
    }
    .into_owned()
}

/// What a rewind did, as one line: how much of the conversation it took
/// back, or that it restored files alone, and the part that failed.
pub(crate) fn rewound_label(rewound: &tcode_core::session::Rewound, harness: &str) -> String {
    let mut label = if !rewound.mode.includes_conversation() {
        crate::tr!("rewind.divider.files").into_owned()
    } else if rewound.later_history == agent::LaterHistory::Branch {
        crate::tr!("rewind.divider.branch").into_owned()
    } else if rewound.removed.is_empty() {
        crate::tr!("rewind.divider.conversation").into_owned()
    } else if rewound.removed_turns == 1 {
        crate::tr!("rewind.divider.turns_one").into_owned()
    } else {
        crate::tr!("rewind.divider.turns", count = rewound.removed_turns.max(1)).into_owned()
    };
    if let Some(incomplete) = &rewound.incomplete {
        let reason = rewind_reason(incomplete.failure.reason, harness);
        label = if rewound.mode.includes_conversation() {
            crate::tr!("rewind.incomplete.files", reason = reason)
        } else {
            crate::tr!("rewind.incomplete.conversation", reason = reason)
        }
        .into_owned();
    }
    label
}

pub(crate) fn rewind_divider(
    entry: &tcode_core::session::TimelineEntry,
    harness: &str,
    cx: &App,
) -> AnyElement {
    let (label, tint) = match &entry.content {
        tcode_core::session::EntryContent::Rewound(rewound) => (
            rewound_label(rewound, harness),
            if rewound.incomplete.is_some() {
                cx.theme().warning
            } else {
                cx.theme().muted_foreground
            },
        ),
        tcode_core::session::EntryContent::RewindFailed { mode, failure } => (
            if mode.is_some() {
                crate::tr!(
                    "rewind.failed",
                    reason = rewind_reason(failure.reason, harness)
                )
            } else {
                crate::tr!(
                    "rewind.undo_failed",
                    reason = rewind_reason(failure.reason, harness)
                )
            }
            .into_owned(),
            cx.theme().warning,
        ),
        _ => unreachable!("a rewind segment holds a rewind entry"),
    };
    divider(
        SharedString::from(format!("rewind-{}", entry.id)),
        label,
        tint,
        cx,
    )
}
