mod cursor;
mod ffi;
mod geometry;

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use cursor::CursorUi;

use self::ffi::{class, dispatch_main, send_id, send_void};
use self::geometry::is_finite_point;

static ENABLED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayActionKind {
    Click,
    Scroll,
    Drag,
    Keyboard,
    Move,
}

pub(crate) fn set_enabled(on: bool) {
    let was_enabled = ENABLED.swap(on, Ordering::AcqRel);
    if was_enabled && !on {
        clear();
    }
}

pub(crate) fn show_action(pid: u32, kind: OverlayActionKind, ax_screen_point: (f64, f64)) {
    if !ENABLED.load(Ordering::Acquire) || !is_finite_point(ax_screen_point) {
        return;
    }
    enqueue(UiCommand::ShowAction {
        pid,
        kind,
        point: ax_screen_point,
    });
}

pub(crate) fn show_drag(pid: u32, from: (f64, f64), to: (f64, f64)) {
    if !ENABLED.load(Ordering::Acquire) || !is_finite_point(from) || !is_finite_point(to) {
        return;
    }
    enqueue(UiCommand::ShowDrag { pid, from, to });
}

pub(crate) fn clear() {
    enqueue(UiCommand::Clear);
}

enum UiCommand {
    ShowAction {
        pid: u32,
        kind: OverlayActionKind,
        point: (f64, f64),
    },
    ShowDrag {
        pid: u32,
        from: (f64, f64),
        to: (f64, f64),
    },
    Clear,
}

struct OverlayState {
    cursor: Option<CursorUi>,
    target_pid: Option<u32>,
    poll_armed: bool,
}

impl OverlayState {
    fn show_action(&mut self, pid: u32, kind: OverlayActionKind, point: (f64, f64)) {
        self.set_target(pid);
        let Some(display) = ffi::display_frame_for_ax_point(point) else {
            return;
        };
        if self.cursor.is_none() {
            self.cursor = CursorUi::new();
        }
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.show(kind, point, display, is_target_frontmost(pid));
        }
    }

    fn show_drag(&mut self, pid: u32, from: (f64, f64), to: (f64, f64)) {
        self.set_target(pid);
        let Some(from_display) = ffi::display_frame_for_ax_point(from) else {
            return;
        };
        let to_display = ffi::display_frame_for_ax_point(to).unwrap_or(from_display);
        if self.cursor.is_none() {
            self.cursor = CursorUi::new();
        }
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.show_drag(from, to, from_display, to_display, is_target_frontmost(pid));
        }
    }

    fn set_target(&mut self, pid: u32) {
        self.target_pid = Some(pid);
        if !self.poll_armed
            && ffi::dispatch_main_after(FOREGROUND_POLL_INTERVAL_NS, poll_foreground)
        {
            self.poll_armed = true;
        }
    }

    fn refresh_visibility(&mut self) {
        let Some(pid) = self.target_pid else {
            return;
        };
        let is_frontmost = is_target_frontmost(pid);
        if !is_frontmost && !target_process_exists(pid) {
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
        self.target_pid = None;
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
            target_pid: None,
            poll_armed: false,
        })
    };
}

fn enqueue(command: UiCommand) {
    let context = Box::into_raw(Box::new(command)).cast::<c_void>();
    if !dispatch_main(context, run_command) {
        // SAFETY: context came from Box::into_raw above and dispatch rejected it,
        // so no callback can own or free it.
        drop(unsafe { Box::from_raw(context.cast::<UiCommand>()) });
    }
}

// SAFETY: libdispatch calls this only with the Box<UiCommand> created by enqueue.
unsafe extern "C" fn run_command(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: enqueue passes exactly one Box<UiCommand> to a callback that
    // libdispatch invokes exactly once.
    let command = unsafe { Box::from_raw(context.cast::<UiCommand>()) };
    let should_run = matches!(*command, UiCommand::Clear) || ENABLED.load(Ordering::Acquire);
    if !should_run {
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
        match *command {
            UiCommand::ShowAction { pid, kind, point } => state.show_action(pid, kind, point),
            UiCommand::ShowDrag { pid, from, to } => state.show_drag(pid, from, to),
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
        if ENABLED.load(Ordering::Acquire) && state.target_pid.is_some() {
            state.refresh_visibility();
            if state.target_pid.is_some()
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
