mod cursor;
mod ffi;
mod geometry;

use std::cell::RefCell;
use std::ffi::c_void;
use std::time::Instant;

use cursor::CursorUi;

use self::ffi::{class, dispatch_main, send_id, send_void};
use crate::backend::cursor::motion::{Point, Waypoint};
pub(crate) use crate::backend::cursor::{CursorArrival, OverlayActionKind, begin_action};
use crate::backend::cursor::{Publication, QueuedCommand, UiCommand};

pub(crate) fn set_enabled(on: bool) {
    crate::backend::cursor::set_enabled(on, enqueue);
}

pub(crate) fn clear(owner: Option<u64>, action: Option<u64>) {
    crate::backend::cursor::clear(owner, action, enqueue);
}

pub(crate) fn show_action(
    publication: Publication,
    window_id: u32,
    kind: OverlayActionKind,
    point: Option<Point>,
    paced: bool,
) -> CursorArrival {
    publish(crate::backend::cursor::action(
        publication,
        window_id,
        kind,
        point,
        paced,
    ))
}

pub(crate) fn show_drag(
    publication: Publication,
    window_id: u32,
    from: Point,
    to: Point,
    paced: bool,
) -> CursorArrival {
    publish(crate::backend::cursor::drag(
        publication,
        window_id,
        from,
        to,
        paced,
    ))
}

fn publish(queued: Option<(QueuedCommand, CursorArrival)>) -> CursorArrival {
    let Some((command, arrival)) = queued else {
        return CursorArrival::none();
    };
    enqueue(command);
    arrival
}

struct OverlayState {
    cursor: Option<CursorUi>,
    target: Option<Target>,
    poll_armed: bool,
    frame_armed: bool,
}

#[derive(Clone, Debug)]
struct Target {
    window_id: u32,
    point: Option<Point>,
    publication: Publication,
}

impl OverlayState {
    fn show(&mut self, target: Target, mut waypoints: Vec<Waypoint>, now: Instant) {
        let point = target.point.or_else(|| {
            self.target
                .as_ref()
                .filter(|current| current.window_id == target.window_id)
                .and_then(|current| current.point)
        });
        let Some(shows_point) = ffi::window_shows_point(target.window_id, point) else {
            self.clear(now);
            return;
        };
        self.set_target(Target { point, ..target });
        if !shows_point {
            // Nothing is drawn there, so the action need not wait for it.
            waypoints
                .iter_mut()
                .for_each(|waypoint| waypoint.reached = None);
        }
        if self.cursor.is_none() && !waypoints.is_empty() {
            self.cursor = CursorUi::new();
        }
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.push(now, waypoints);
            cursor.set_shown(shows_point);
        }
        self.arm_frame();
    }

    fn set_target(&mut self, target: Target) {
        self.target = Some(target);
        if !self.poll_armed && ffi::dispatch_main_after(TARGET_POLL_INTERVAL_NS, poll_target) {
            self.poll_armed = true;
        }
    }

    /// `shows_point` is `None` once the target window has left the screen.
    fn refresh_visibility(&mut self, now: Instant, shows_point: Option<bool>) {
        let Some(target) = self.target.as_ref() else {
            return;
        };
        let Some(shows_point) = shows_point.filter(|_| target.publication.is_current(now)) else {
            self.clear(now);
            return;
        };
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.set_shown(shows_point);
        }
        self.arm_frame();
    }

    fn clear(&mut self, now: Instant) {
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.hide(now);
        }
        self.target = None;
        self.arm_frame();
    }

    fn arm_frame(&mut self) {
        if self.cursor.is_some()
            && !self.frame_armed
            && ffi::dispatch_main_after(FRAME_INTERVAL_NS, advance_frame)
        {
            self.frame_armed = true;
        }
    }
}

const TARGET_POLL_INTERVAL_NS: i64 = 200_000_000;
const FRAME_INTERVAL_NS: i64 = 16_666_667;

// This thread-local is intentionally read only by callbacks submitted to the
// process main queue. Objective-C window and layer pointers therefore never
// cross back into background-thread UI code.
thread_local! {
    static MAIN_STATE: RefCell<OverlayState> = const {
        RefCell::new(OverlayState {
            cursor: None,
            target: None,
            poll_armed: false,
            frame_armed: false,
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
    crate::backend::cursor::run_accepted(*command, Instant::now(), run_on_main);
}

fn run_on_main(command: UiCommand, publication: Publication) {
    let pool = class(c"NSAutoreleasePool").and_then(|pool| send_id(pool, c"new"));
    // sharedApplication initializes AppKit if needed; it does not activate the app.
    let _ =
        class(c"NSApplication").and_then(|application| send_id(application, c"sharedApplication"));
    let _ = MAIN_STATE.try_with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        let now = Instant::now();
        match command {
            UiCommand::Show {
                window_id,
                point,
                waypoints,
            } => state.show(
                Target {
                    window_id,
                    point,
                    publication,
                },
                waypoints,
                now,
            ),
            UiCommand::Clear => state.clear(now),
        }
    });
    if let Some(pool) = pool {
        let _ = send_void(pool, c"drain");
    }
}

// SAFETY: libdispatch calls this with the null context supplied when the poll is armed.
unsafe extern "C" fn poll_target(_context: *mut c_void) {
    let pool = class(c"NSAutoreleasePool").and_then(|pool| send_id(pool, c"new"));
    let _ = MAIN_STATE.try_with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        state.poll_armed = false;
        let now = Instant::now();
        if crate::backend::cursor::is_enabled()
            && let Some(target) = state.target.clone()
        {
            state.refresh_visibility(now, ffi::window_shows_point(target.window_id, target.point));
            if state.target.is_some()
                && ffi::dispatch_main_after(TARGET_POLL_INTERVAL_NS, poll_target)
            {
                state.poll_armed = true;
            }
        } else {
            state.clear(now);
        }
    });
    if let Some(pool) = pool {
        let _ = send_void(pool, c"drain");
    }
}

// SAFETY: libdispatch calls this with the null context supplied when a frame is armed.
unsafe extern "C" fn advance_frame(_context: *mut c_void) {
    let pool = class(c"NSAutoreleasePool").and_then(|pool| send_id(pool, c"new"));
    let _ = MAIN_STATE.try_with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return;
        };
        state.frame_armed = false;
        let animating = state
            .cursor
            .as_mut()
            .is_some_and(|cursor| cursor.advance(Instant::now()));
        if animating {
            state.arm_frame();
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
        frame_armed: false,
    };
    let target = Target {
        window_id: closed_window,
        point: Some((300.0, 200.0)),
        publication: crate::backend::cursor::Submissions::enabled().reserve(None, Instant::now()),
    };
    state.show(
        target,
        vec![Waypoint::new((300.0, 200.0), None)],
        Instant::now(),
    );
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
    use std::time::Duration;

    use super::*;
    use crate::backend::cursor::{LINGER, Submissions};

    fn submissions() -> Submissions {
        Submissions::enabled()
    }
    fn target(publication: Publication) -> Target {
        Target {
            window_id: 7,
            point: Some((10.0, 20.0)),
            publication,
        }
    }

    #[test]
    fn inactive_action_retires_target_before_visibility_refresh() {
        let now = Instant::now();
        let mut state = OverlayState {
            cursor: None,
            target: None,
            poll_armed: true,
            frame_armed: false,
        };
        state.set_target(target(submissions().reserve(None, now)));
        state.refresh_visibility(now, Some(true));
        assert!(state.target.is_some());
        let expired = now + LINGER + Duration::from_secs(1);
        state.refresh_visibility(expired, Some(false));
        assert!(
            state.target.is_none(),
            "idle feedback must not reappear on visibility refresh"
        );
        state.refresh_visibility(expired + Duration::from_secs(1), Some(true));
        assert!(state.target.is_none());
    }

    #[test]
    fn newer_action_renews_lifetime_but_closed_window_retires_it() {
        let now = Instant::now();
        let mut state = OverlayState {
            cursor: None,
            target: None,
            poll_armed: true,
            frame_armed: false,
        };
        let mut submissions = submissions();
        state.set_target(target(submissions.reserve(None, now)));
        state.set_target(target(
            submissions.reserve(None, now + Duration::from_millis(500)),
        ));
        state.refresh_visibility(now + LINGER, Some(false));
        assert!(
            state.target.is_some(),
            "a covered target does not retire a live action"
        );
        state.refresh_visibility(now + LINGER, None);
        assert!(
            state.target.is_none(),
            "closed window retires feedback while its app stays open"
        );
    }

    #[test]
    fn a_call_keeps_its_cursor_while_it_runs_and_for_the_linger_after_its_response() {
        let mut state = OverlayState {
            cursor: None,
            target: None,
            poll_armed: true,
            frame_armed: false,
        };
        let mut submissions = submissions();
        let session = crate::feedback::FeedbackSession::new();
        let request = tokio_util::sync::CancellationToken::new();
        let start = Instant::now();
        let mut run = session.begin(Some(request.clone()));
        state.set_target(target(submissions.reserve(Some(run.ticket()), start)));
        state.set_target(target(submissions.reserve(Some(run.ticket()), start)));
        state.refresh_visibility(start + LINGER * 4, Some(true));
        assert!(
            state.target.is_some(),
            "a call still waiting on its expectation keeps the cursor up"
        );
        let before = Instant::now();
        run.complete();
        let after = Instant::now();
        drop(run);
        // rmcp cancels the request's token as it sends the response.
        request.cancel();
        state.refresh_visibility(before + LINGER - Duration::from_millis(1), Some(true));
        assert!(
            state.target.is_some(),
            "the cursor rests on its last point after the call returns"
        );
        state.refresh_visibility(after + LINGER, Some(true));
        assert!(state.target.is_none(), "it retires once the linger is over");
    }
}
