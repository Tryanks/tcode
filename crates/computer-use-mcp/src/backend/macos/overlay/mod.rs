mod cursor;
mod ffi;
mod geometry;

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cursor::CursorUi;

use self::ffi::{class, dispatch_main, send_id, send_void};
use self::geometry::is_finite_point;

static SUBMISSIONS: Mutex<Submissions> = Mutex::new(Submissions {
    enabled: false,
    revision: 0,
    owner: None,
});

// The producer and main queue share this ordering boundary. Invalidation cannot
// race a producer into enqueueing an obsolete action after the clear callback.
struct Submissions {
    enabled: bool,
    revision: u64,
    owner: Option<(u64, u64)>,
}

impl Submissions {
    fn reserve(
        &mut self,
        feedback: Option<crate::feedback::FeedbackTicket>,
        now: Instant,
    ) -> Publication {
        self.revision += 1;
        self.owner = feedback
            .as_ref()
            .map(|ticket| (ticket.owner, ticket.action));
        Publication {
            revision: self.revision,
            expires_at: now + ACTION_LIFETIME,
            feedback,
        }
    }

    fn accepts(&self, queued: &QueuedCommand, now: Instant) -> bool {
        queued.publication.revision == self.revision
            && (matches!(queued.command, UiCommand::Clear)
                || (self.enabled && queued.publication.is_current(now)))
    }

    fn clear(
        &mut self,
        owner: Option<u64>,
        action: Option<u64>,
        now: Instant,
    ) -> Option<QueuedCommand> {
        if let Some(owner) = owner
            && !self.owner.is_some_and(|current| {
                current.0 == owner && action.is_none_or(|action| current.1 == action)
            })
        {
            return None;
        }
        Some(QueuedCommand {
            command: UiCommand::Clear,
            publication: self.reserve(None, now),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Publication {
    revision: u64,
    expires_at: Instant,
    feedback: Option<crate::feedback::FeedbackTicket>,
}

impl Publication {
    fn is_current(&self, now: Instant) -> bool {
        now < self.expires_at
            && self
                .feedback
                .as_ref()
                .is_none_or(|ticket| ticket.is_current())
    }
}

struct QueuedCommand {
    command: UiCommand,
    publication: Publication,
}

const ACTION_LIFETIME: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayActionKind {
    Click,
    Scroll,
    Drag,
    Keyboard,
    Move,
}

pub(crate) fn set_enabled(on: bool) {
    let mut submissions = SUBMISSIONS.lock().unwrap();
    let was_enabled = submissions.enabled;
    submissions.enabled = on;
    if was_enabled
        && !on
        && let Some(command) = submissions.clear(None, None, Instant::now())
    {
        enqueue(command);
    }
}

// Reserve before AX point lookup: an older, slow lookup cannot overwrite a
// newer action or a disable/cancel that arrived while the lookup was blocked.
pub(crate) fn begin_action(
    feedback: Option<crate::feedback::FeedbackTicket>,
) -> Option<Publication> {
    let mut submissions = SUBMISSIONS.lock().unwrap();
    (submissions.enabled && feedback.as_ref().is_none_or(|ticket| ticket.is_current()))
        .then(|| submissions.reserve(feedback, Instant::now()))
}

pub(crate) fn show_action(
    publication: Publication,
    pid: u32,
    window_id: u32,
    kind: OverlayActionKind,
    point: (f64, f64),
) {
    if is_finite_point(point) {
        enqueue(QueuedCommand {
            publication,
            command: UiCommand::ShowAction {
                pid,
                window_id,
                kind,
                point,
            },
        });
    }
}

pub(crate) fn show_drag(
    publication: Publication,
    pid: u32,
    window_id: u32,
    from: (f64, f64),
    to: (f64, f64),
) {
    if is_finite_point(from) && is_finite_point(to) {
        enqueue(QueuedCommand {
            publication,
            command: UiCommand::ShowDrag {
                pid,
                window_id,
                from,
                to,
            },
        });
    }
}

pub(crate) fn clear(owner: Option<u64>, action: Option<u64>) {
    let mut submissions = SUBMISSIONS.lock().unwrap();
    if let Some(command) = submissions.clear(owner, action, Instant::now()) {
        enqueue(command);
    }
}

enum UiCommand {
    ShowAction {
        pid: u32,
        window_id: u32,
        kind: OverlayActionKind,
        point: (f64, f64),
    },
    ShowDrag {
        pid: u32,
        window_id: u32,
        from: (f64, f64),
        to: (f64, f64),
    },
    Clear,
}

struct OverlayState {
    cursor: Option<CursorUi>,
    target: Option<Target>,
    poll_armed: bool,
}

#[derive(Clone, Debug)]
struct Target {
    pid: u32,
    window_id: u32,
    publication: Publication,
}

impl OverlayState {
    fn show_action(&mut self, target: Target, kind: OverlayActionKind, point: (f64, f64)) {
        if !ffi::window_exists(target.window_id) {
            self.clear();
            return;
        }
        self.set_target(target.clone());
        let Some(display) = ffi::display_frame_for_ax_point(point) else {
            return;
        };
        if self.cursor.is_none() {
            self.cursor = CursorUi::new();
        }
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.show(kind, point, display, is_target_frontmost(target.pid));
        }
    }

    fn show_drag(&mut self, target: Target, from: (f64, f64), to: (f64, f64)) {
        if !ffi::window_exists(target.window_id) {
            self.clear();
            return;
        }
        self.set_target(target.clone());
        let Some(from_display) = ffi::display_frame_for_ax_point(from) else {
            return;
        };
        let to_display = ffi::display_frame_for_ax_point(to).unwrap_or(from_display);
        if self.cursor.is_none() {
            self.cursor = CursorUi::new();
        }
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.show_drag(
                from,
                to,
                from_display,
                to_display,
                is_target_frontmost(target.pid),
            );
        }
    }

    fn set_target(&mut self, target: Target) {
        self.target = Some(target);
        if !self.poll_armed
            && ffi::dispatch_main_after(FOREGROUND_POLL_INTERVAL_NS, poll_foreground)
        {
            self.poll_armed = true;
        }
    }

    fn refresh_visibility(&mut self, now: Instant, is_frontmost: bool, target_exists: bool) {
        let Some(target) = self.target.as_ref() else {
            return;
        };
        if !target.publication.is_current(now) || !target_exists {
            self.clear();
            return;
        }
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.set_visible(is_frontmost);
        }
    }

    fn clear(&mut self) {
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.hide();
        }
        self.target = None;
    }
}

const FOREGROUND_POLL_INTERVAL_NS: i64 = 200_000_000;

fn is_target_frontmost(pid: u32) -> bool {
    super::ax::frontmost_application_pid() == Some(pid)
}

fn target_process_exists(pid: u32) -> bool {
    i32::try_from(pid).ok().is_some_and(|pid| {
        class(c"NSRunningApplication").is_some_and(|application| {
            ffi::send_id_i32(
                application,
                c"runningApplicationWithProcessIdentifier:",
                pid,
            )
            .is_some()
        })
    })
}

// This thread-local is intentionally read only by callbacks submitted to the
// process main queue. Objective-C window and layer pointers therefore never
// cross back into background-thread UI code.
thread_local! {
    static MAIN_STATE: RefCell<OverlayState> = const {
        RefCell::new(OverlayState {
            cursor: None,
            target: None,
            poll_armed: false,
        })
    };
}

fn enqueue(command: QueuedCommand) {
    let context = Box::into_raw(Box::new(command)).cast::<c_void>();
    if !dispatch_main(context, run_command) {
        // SAFETY: context came from Box::into_raw above and dispatch rejected it,
        // so no callback can own or free it.
        drop(unsafe { Box::from_raw(context.cast::<QueuedCommand>()) });
    }
}

// SAFETY: libdispatch calls this only with the Box<QueuedCommand> created by enqueue.
unsafe extern "C" fn run_command(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: enqueue passes exactly one Box<QueuedCommand> to a callback that
    // libdispatch invokes exactly once.
    let command = unsafe { Box::from_raw(context.cast::<QueuedCommand>()) };
    let submissions = SUBMISSIONS.lock().unwrap();
    if !submissions.accepts(&command, Instant::now()) {
        return;
    }

    let pool = class(c"NSAutoreleasePool").and_then(|pool| send_id(pool, c"new"));
    // sharedApplication initializes AppKit if needed; it does not activate the app.
    let _ =
        class(c"NSApplication").and_then(|application| send_id(application, c"sharedApplication"));
    let _ = MAIN_STATE.try_with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        match command.command {
            UiCommand::ShowAction {
                pid,
                window_id,
                kind,
                point,
            } => state.show_action(
                Target {
                    pid,
                    window_id,
                    publication: command.publication.clone(),
                },
                kind,
                point,
            ),
            UiCommand::ShowDrag {
                pid,
                window_id,
                from,
                to,
            } => state.show_drag(
                Target {
                    pid,
                    window_id,
                    publication: command.publication.clone(),
                },
                from,
                to,
            ),
            UiCommand::Clear => state.clear(),
        }
    });
    if let Some(pool) = pool {
        let _ = send_void(pool, c"drain");
    }
}

// SAFETY: libdispatch calls this with the null context supplied when the poll is armed.
unsafe extern "C" fn poll_foreground(_context: *mut c_void) {
    let pool = class(c"NSAutoreleasePool").and_then(|pool| send_id(pool, c"new"));
    let _ = MAIN_STATE.try_with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        state.poll_armed = false;
        if SUBMISSIONS.lock().unwrap().enabled
            && let Some(target) = state.target.clone()
        {
            state.refresh_visibility(
                Instant::now(),
                is_target_frontmost(target.pid),
                target_process_exists(target.pid) && ffi::window_exists(target.window_id),
            );
            if state.target.is_some()
                && ffi::dispatch_main_after(FOREGROUND_POLL_INTERVAL_NS, poll_foreground)
            {
                state.poll_armed = true;
            }
        } else {
            state.clear();
        }
    });
    if let Some(pool) = pool {
        let _ = send_void(pool, c"drain");
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
#[allow(dead_code)] // Entry point for the opt-in main-thread integration runner.
pub(crate) fn verify_native() {
    let _ = class(c"NSApplication").and_then(|app| send_id(app, c"sharedApplication"));
    let foreground = super::ax::frontmost_application_pid();
    let closed_window = cursor::verify_native();
    let mut state = OverlayState {
        cursor: None,
        target: None,
        poll_armed: true,
    };
    let target = Target {
        pid: std::process::id(),
        window_id: closed_window,
        publication: Publication {
            revision: 1,
            expires_at: Instant::now() + ACTION_LIFETIME,
            feedback: None,
        },
    };
    state.show_action(target, OverlayActionKind::Move, (300.0, 200.0));
    assert!(
        state.target.is_none() && state.cursor.is_none(),
        "a queued action reaching a closed native target must not recreate a marker"
    );
    assert_eq!(
        super::ax::frontmost_application_pid(),
        foreground,
        "overlay must not activate an app"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(publication: Publication) -> QueuedCommand {
        QueuedCommand {
            publication,
            command: UiCommand::ShowAction {
                pid: 42,
                window_id: 7,
                kind: OverlayActionKind::Click,
                point: (10.0, 20.0),
            },
        }
    }
    fn submissions() -> Submissions {
        Submissions {
            enabled: true,
            revision: 0,
            owner: None,
        }
    }
    fn target(publication: Publication) -> Target {
        Target {
            pid: 42,
            window_id: 7,
            publication,
        }
    }

    #[test]
    fn inactive_action_retires_target_before_foreground_refresh() {
        let now = Instant::now();
        let mut state = OverlayState {
            cursor: None,
            target: None,
            poll_armed: true,
        };
        state.set_target(target(submissions().reserve(None, now)));
        state.refresh_visibility(now, true, true);
        assert!(state.target.is_some());
        state.refresh_visibility(now + Duration::from_secs(2), false, true);
        assert!(
            state.target.is_none(),
            "idle feedback must not reappear on foreground refresh"
        );
        state.refresh_visibility(now + Duration::from_secs(3), true, true);
        assert!(state.target.is_none());
    }

    #[test]
    fn newer_action_renews_lifetime_but_closed_window_retires_it() {
        let now = Instant::now();
        let mut state = OverlayState {
            cursor: None,
            target: None,
            poll_armed: true,
        };
        let mut submissions = submissions();
        state.set_target(target(submissions.reserve(None, now)));
        state.set_target(target(
            submissions.reserve(None, now + Duration::from_millis(500)),
        ));
        state.refresh_visibility(now + ACTION_LIFETIME, false, true);
        assert!(
            state.target.is_some(),
            "backgrounding does not retire a live action"
        );
        state.refresh_visibility(now + ACTION_LIFETIME, true, false);
        assert!(
            state.target.is_none(),
            "closed window retires feedback while its app stays open"
        );
    }

    #[test]
    fn delayed_callbacks_cannot_restore_cleared_superseded_or_expired_feedback() {
        let now = Instant::now();
        let mut submissions = submissions();
        let first = action(submissions.reserve(None, now));
        assert!(submissions.accepts(&first, now));
        let clear = submissions.clear(None, None, now).unwrap();
        assert!(!submissions.accepts(&first, now));
        assert!(submissions.accepts(&clear, now));
        let newer = action(submissions.reserve(None, now));
        assert!(
            !submissions.accepts(&clear, now),
            "late clear cannot erase newer feedback"
        );
        assert!(submissions.accepts(&newer, now));
        assert!(
            !submissions.accepts(&newer, now + ACTION_LIFETIME),
            "queue delay consumes lifetime"
        );
        submissions.enabled = false;
        let _disable = submissions.clear(None, None, now);
        submissions.enabled = true;
        assert!(
            !submissions.accepts(&newer, now),
            "re-enable cannot revive pre-disable work"
        );
    }

    #[test]
    fn cancelling_one_session_does_not_clear_another_sessions_newer_feedback() {
        let now = Instant::now();
        let mut submissions = submissions();
        let first = crate::feedback::FeedbackSession::new();
        let second = crate::feedback::FeedbackSession::new();
        let old_run = first.begin(None);
        let current_run = second.begin(None);
        let old = action(submissions.reserve(Some(old_run.ticket()), now));
        let current = action(submissions.reserve(Some(current_run.ticket()), now));
        first.cancel();
        assert!(!submissions.accepts(&old, now));
        assert!(
            submissions
                .clear(Some(old_run.ticket().owner), None, now)
                .is_none()
        );
        assert!(submissions.accepts(&current, now));
        second.cancel();
        assert!(
            !submissions.accepts(&current, now),
            "even an undelivered cancel callback invalidates the marker"
        );
        let restarted = second.begin(None);
        let current = action(submissions.reserve(Some(restarted.ticket()), now));
        assert!(
            submissions
                .clear(
                    Some(current_run.ticket().owner),
                    Some(current_run.ticket().action),
                    now
                )
                .is_none(),
            "a dropped older request cannot clear its successor"
        );
        assert!(submissions.accepts(&current, now));
        drop(second);
        assert!(
            !submissions.accepts(&current, now),
            "actual service teardown invalidates queued publication"
        );
    }
}
