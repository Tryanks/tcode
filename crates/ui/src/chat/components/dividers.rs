use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use gpui::{
    AnyElement, App, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, Styled, div, prelude::FluentBuilder as _,
};
use gpui_base::h_flex;

/// How long a divider's label takes to cross-fade to its next one.
const LABEL: std::time::Duration = std::time::Duration::from_millis(200);

/// The one divider grammar: a centred small label, led by `icon` where the
/// notice has one, and nothing on either side. `tint` colours the label and
/// its icon alike.
fn divider(id: SharedString, label: String, tint: Hsla, cx: &App) -> AnyElement {
    divider_in(id, label, tint, None, None, cx)
}

/// [`divider`] led by `icon`, whose label changes (compaction running, then
/// done) and cross-fades as it does, on `motion`'s clock when the chat keeps
/// it.
fn divider_in(
    id: SharedString,
    label: String,
    tint: Hsla,
    icon: Option<IconName>,
    motion: Option<&crate::motion::owned::Owned>,
    cx: &App,
) -> AnyElement {
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
    let selector = id.to_string();
    h_flex()
        .id(id)
        .debug_selector(move || selector)
        .w_full()
        .items_center()
        .justify_center()
        .child(
            h_flex()
                .min_w_0()
                .flex_shrink_1()
                .items_center()
                .gap_1p5()
                .text_token(&cx.theme().tokens.typography.xs)
                .text_color(tint)
                .when_some(icon.clone(), |row, icon| {
                    row.child(Icon::new(icon).flex_none())
                })
                // A label that wraps beside the icon starts each line at it.
                .child(
                    div()
                        .min_w_0()
                        .when(icon.is_none(), Styled::text_center)
                        .child(label),
                ),
        )
        .into_any_element()
}

/// Context compaction rewrites what the model remembers, so it stands in the
/// flow rather than in the work log: running, then what it left, in place.
pub(crate) fn context_compacted_divider(
    id: &str,
    compaction: &agent::Compaction,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let label = if compaction.in_progress {
        crate::tr!("chat.context_compacting").into_owned()
    } else {
        let auto = compaction.trigger.as_deref() == Some("auto");
        let label = match (auto, compaction.pre_tokens) {
            (true, Some(tokens)) => crate::tr!(
                "chat.context_compacted_auto_from",
                tokens = crate::context_meter::format_tokens(Some(tokens))
            ),
            (true, None) => crate::tr!("chat.context_compacted_auto"),
            (false, Some(tokens)) => crate::tr!(
                "chat.context_compacted_from",
                tokens = crate::context_meter::format_tokens(Some(tokens))
            ),
            (false, None) => crate::tr!("chat.context_compacted"),
        }
        .into_owned();
        // A trigger other than these two is the harness's own word for it.
        match compaction.trigger.as_deref() {
            Some(trigger) if trigger != "auto" && trigger != "manual" => {
                format!("{label} · {trigger}")
            }
            _ => label,
        }
    };
    divider_in(
        SharedString::from(format!("context-compacted-{id}")),
        label,
        cx.theme().muted_foreground,
        Some(IconName::Minimize2),
        Some(motion),
        cx,
    )
}

/// A request the harness tries again, in its own words: one line where it
/// happened, which each further attempt at that request rewrites.
pub(crate) fn request_retry_divider(
    id: &str,
    message: &str,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    divider_in(
        SharedString::from(format!("request-retry-{id}")),
        message.to_owned(),
        cx.theme().muted_foreground,
        Some(IconName::RefreshCw),
        Some(motion),
        cx,
    )
}

pub(crate) fn session_moved_divider(
    id: &str,
    from: &std::path::Path,
    to: &std::path::Path,
    cx: &App,
) -> AnyElement {
    divider_in(
        SharedString::from(format!("moved-{id}")),
        crate::tr!(
            "chat.session_moved",
            from = from.to_string_lossy(),
            to = to.to_string_lossy()
        )
        .into_owned(),
        cx.theme().muted_foreground,
        Some(IconName::ArrowRightLeft),
        None,
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
    let (label, refused) = match (request, answer) {
        (WorktreeRequest::Create { .. }, WorktreeAnswer::Ready { .. }) => {
            unreachable!("a worktree Tcode made ready stays out of the flow")
        }
        (WorktreeRequest::Create { name, .. }, WorktreeAnswer::Refused { error }) => (
            crate::tr!("chat.worktree_create_refused", name = name, error = error),
            true,
        ),
        (WorktreeRequest::Remove { path }, WorktreeAnswer::Refused { error }) => (
            crate::tr!(
                "chat.worktree_remove_refused",
                path = path.to_string_lossy(),
                error = error
            ),
            true,
        ),
        (WorktreeRequest::Remove { path }, _) => (
            crate::tr!("chat.worktree_removed", path = path.to_string_lossy()),
            false,
        ),
        (WorktreeRequest::Create { name, .. }, WorktreeAnswer::Removed) => {
            (crate::tr!("chat.worktree_removed", path = name), false)
        }
    };
    let label = label.into_owned();
    if refused {
        super::notices::alert_line(format!("worktree-{id}"), label, cx)
    } else {
        divider(
            SharedString::from(format!("worktree-{id}")),
            label,
            cx.theme().muted_foreground,
            cx,
        )
    }
}

/// Why a harness did not rewind, in the user's words, with the words it gave.
pub(crate) fn rewind_reason(failure: &agent::RewindFailure, harness: &str) -> String {
    use agent::RewindFailureReason as Reason;
    let reason = match failure.reason {
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
    };
    match failure.detail.as_deref() {
        Some(detail) => crate::tr!("rewind.reason_detail", reason = reason, detail = detail),
        None => reason,
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
        let reason = rewind_reason(&incomplete.failure, harness);
        label = if rewound.mode.includes_conversation() {
            crate::tr!("rewind.incomplete.files", reason = reason)
        } else {
            crate::tr!("rewind.incomplete.conversation", reason = reason)
        }
        .into_owned();
    }
    label
}

#[cfg(test)]
mod tests {
    use agent::{RewindFailure, RewindFailureReason};

    /// A harness that says why it did not rewind is read in its own words
    /// after the reason Tcode names, not the reason alone.
    #[test]
    fn a_rewind_failure_reads_with_the_words_its_harness_gave() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        let refused = RewindFailure::new(
            RewindFailureReason::Other,
            "no checkpoint named c-1".to_owned(),
        );
        assert_eq!(
            super::rewind_reason(&refused, "Codex"),
            "Codex refused. (no checkpoint named c-1)"
        );
        let expired = RewindFailure::new(RewindFailureReason::Expired, None);
        assert_eq!(
            super::rewind_reason(&expired, "Codex"),
            "the request expired."
        );
    }
}
