//! How the record of `tcode_control` writes reads in Settings → Control and in a
//! thread: the words for each operation, and the day an entry falls on.

/// Whether an operation's recorded detail is words a reader can use rather
/// than an id only the host can resolve.
pub(crate) fn action_detail_readable(key: &str) -> bool {
    use tcode_core::control::action;
    !matches!(
        key,
        action::NOTIFY_WHEN
            | action::CANCEL_NOTIFY
            | action::RESPOND_INPUT
            | action::RESPOND_APPROVAL
            | action::TERMINAL_INPUT
            | action::TERMINAL_CLOSE
            | action::REWIND
            | action::RETRY
            | action::SET_OPTION
            | action::STEER_QUEUED
            | action::DROP_QUEUED
    )
}

/// Which day an activity entry falls on, as the list groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ActivityDay {
    Today,
    Yesterday,
    Earlier,
}

impl ActivityDay {
    /// The day of `at_millis` seen from `now`, by the reader's calendar.
    pub(crate) fn of(at_millis: u64, now: &chrono::DateTime<chrono::Local>) -> Self {
        let Some(at) = chrono::DateTime::from_timestamp_millis(at_millis as i64) else {
            return Self::Earlier;
        };
        let day = at.with_timezone(&now.timezone()).date_naive();
        let today = now.date_naive();
        if day >= today {
            Self::Today
        } else if today.pred_opt() == Some(day) {
            Self::Yesterday
        } else {
            Self::Earlier
        }
    }

    pub(crate) fn label(self) -> String {
        match self {
            Self::Today => crate::tr!("control.settings.activity_today"),
            Self::Yesterday => crate::tr!("control.settings.activity_yesterday"),
            Self::Earlier => crate::tr!("control.settings.activity_earlier"),
        }
        .into_owned()
    }
}

/// Whether a `ControlAction`'s operation only changed how the thread stands
/// in Tcode (where it is listed, what it is called, who watches it) and
/// nothing its runs do.
pub(crate) fn changes_only_the_thread(key: &str) -> bool {
    use tcode_core::control::action;
    matches!(
        key,
        action::SETTLE
            | action::SETTLED_ITSELF
            | action::UNSETTLE
            | action::ARCHIVE
            | action::UNARCHIVE
            | action::PIN
            | action::UNPIN
            | action::RENAME
            | action::MARK_READ
            | action::MARK_UNREAD
            | action::FOLLOW_UP
            | action::FOLLOW_UP_OFF
            | action::PARK
            | action::PARK_OFF
            | action::KEEP
            | action::KEEP_OFF
            | action::SETTLE_SELF
            | action::NOTIFY_WHEN
            | action::CANCEL_NOTIFY
    )
}

/// A `ControlAction`'s operation in words; an operation this client does not
/// know keeps the key the host recorded.
pub(crate) fn action_label(key: &str) -> String {
    use tcode_core::control::action;
    let key = match key {
        action::SETTLE => "control.action.settle",
        action::SETTLED_ITSELF => "control.action.settled_itself",
        action::UNSETTLE => "control.action.unsettle",
        action::ARCHIVE => "control.action.archive",
        action::UNARCHIVE => "control.action.unarchive",
        action::PIN => "control.action.pin",
        action::UNPIN => "control.action.unpin",
        action::RENAME => "control.action.rename",
        action::MARK_READ => "control.action.mark_read",
        action::MARK_UNREAD => "control.action.mark_unread",
        action::FOLLOW_UP => "control.action.follow_up",
        action::FOLLOW_UP_OFF => "control.action.follow_up_off",
        action::PARK => "control.action.park",
        action::PARK_OFF => "control.action.park_off",
        action::KEEP => "control.action.keep",
        action::KEEP_OFF => "control.action.keep_off",
        action::SETTLE_SELF => "control.action.settle_self",
        action::NOTIFY_WHEN => "control.action.notify_when",
        action::CANCEL_NOTIFY => "control.action.cancel_notify",
        action::RESPOND_INPUT => "control.action.respond_input",
        action::RESPOND_APPROVAL => "control.action.respond_approval",
        action::TERMINAL_OPEN => "control.action.terminal_open",
        action::TERMINAL_INPUT => "control.action.terminal_input",
        action::TERMINAL_CLOSE => "control.action.terminal_close",
        action::DELETE_THREADS => "control.action.delete_threads",
        action::REWIND => "control.action.rewind",
        action::PATCH_SETTINGS => "control.action.patch_settings",
        action::SET_MODEL_GUIDE => "control.action.set_model_guide",
        action::START_THREAD => "control.action.start_thread",
        action::SEND_QUEUED => "control.action.send_queued",
        action::SEND_STEERED => "control.action.send_steered",
        action::SEND_SCHEDULED => "control.action.send_scheduled",
        action::INTERRUPT => "control.action.interrupt",
        action::CANCEL => "control.action.cancel",
        action::RETRY => "control.action.retry",
        action::COMPACT => "control.action.compact",
        action::SET_MODEL => "control.action.set_model",
        action::SET_OPTION => "control.action.set_option",
        action::FORK => "control.action.fork",
        action::STEER_QUEUED => "control.action.steer_queued",
        action::DROP_QUEUED => "control.action.drop_queued",
        action::PR_LINK => "control.action.pr_link",
        action::PR_UNLINK => "control.action.pr_unlink",
        action::PR_WATCH => "control.action.pr_watch",
        action::PR_UNWATCH => "control.action.pr_unwatch",
        action::PR_REFRESH => "control.action.pr_refresh",
        action::PR_COMMENT => "control.action.pr_comment",
        action::PR_REPLY => "control.action.pr_reply",
        action::PR_RESOLVE => "control.action.pr_resolve",
        action::GIT_CHECKOUT => "control.action.git_checkout",
        action::GIT_MERGE_WORKTREE => "control.action.git_merge_worktree",
        action::GIT_COMMIT => "control.action.git_commit",
        action::GIT_COMMIT_PUSH => "control.action.git_commit_push",
        action::GIT_PUSH => "control.action.git_push",
        action::GIT_PULL => "control.action.git_pull",
        action::GIT_PUBLISH_BRANCH => "control.action.git_publish_branch",
        action::GIT_INITIALIZE => "control.action.git_initialize",
        action::CREATE_PROJECT => "control.action.create_project",
        action::MOVE => "control.action.move",
        _ => return key.to_owned(),
    };
    crate::tr!(key).into_owned()
}
