#![allow(dead_code)]

mod border;
mod cursor;
mod ffi;
mod geometry;

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use border::BorderUi;
use cursor::CursorUi;

use self::ffi::{class, dispatch_main, send_id, send_void};
use self::geometry::{is_finite_point, is_valid_frame};
use crate::outline::Frame;

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
        enqueue(UiCommand::Clear);
    }
}

pub(crate) fn show_action(
    kind: OverlayActionKind,
    ax_screen_point: (f64, f64),
    window_frame: Frame,
) {
    if !ENABLED.load(Ordering::Acquire) || !is_finite_point(ax_screen_point) {
        return;
    }
    enqueue(UiCommand::ShowAction {
        kind,
        point: ax_screen_point,
        window_frame,
    });
}

pub(crate) fn show_drag(from: (f64, f64), to: (f64, f64), window_frame: Frame) {
    if !ENABLED.load(Ordering::Acquire) || !is_finite_point(from) || !is_finite_point(to) {
        return;
    }
    enqueue(UiCommand::ShowDrag {
        from,
        to,
        window_frame,
    });
}

pub(crate) fn highlight_window(window_frame: Frame) {
    if !ENABLED.load(Ordering::Acquire) || !is_valid_frame(window_frame) {
        return;
    }
    enqueue(UiCommand::Highlight(window_frame));
}

pub(crate) fn clear() {
    if ENABLED.load(Ordering::Acquire) {
        enqueue(UiCommand::Clear);
    }
}

enum UiCommand {
    ShowAction {
        kind: OverlayActionKind,
        point: (f64, f64),
        window_frame: Frame,
    },
    ShowDrag {
        from: (f64, f64),
        to: (f64, f64),
        window_frame: Frame,
    },
    Highlight(Frame),
    Clear,
}

struct OverlayState {
    cursor: Option<CursorUi>,
    border: Option<BorderUi>,
}

impl OverlayState {
    fn show_action(&mut self, kind: OverlayActionKind, point: (f64, f64), window_frame: Frame) {
        if is_valid_frame(window_frame) {
            self.highlight(window_frame);
        }
        let Some(display) = ffi::display_frame_for_ax_point(point) else {
            return;
        };
        if self.cursor.is_none() {
            self.cursor = CursorUi::new();
        }
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.show(kind, point, display);
        }
    }

    fn show_drag(&mut self, from: (f64, f64), to: (f64, f64), window_frame: Frame) {
        if is_valid_frame(window_frame) {
            self.highlight(window_frame);
        }
        let Some(from_display) = ffi::display_frame_for_ax_point(from) else {
            return;
        };
        let to_display = ffi::display_frame_for_ax_point(to).unwrap_or(from_display);
        if self.cursor.is_none() {
            self.cursor = CursorUi::new();
        }
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.show_drag(from, to, from_display, to_display);
        }
    }

    fn highlight(&mut self, window_frame: Frame) {
        let Some(display) = ffi::display_frame_for_ax_point(window_frame.center()) else {
            return;
        };
        if self.border.is_none() {
            self.border = BorderUi::new();
        }
        if let Some(border) = self.border.as_mut() {
            border.show(window_frame, display);
        }
    }

    fn clear(&mut self) {
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.hide();
        }
        if let Some(border) = self.border.as_mut() {
            border.hide();
        }
    }
}

// This thread-local is intentionally read only by `run_command`, which is
// exclusively submitted to the process main queue. Objective-C window and
// layer pointers therefore never cross back into background-thread UI code.
thread_local! {
    static MAIN_STATE: RefCell<OverlayState> = const {
        RefCell::new(OverlayState {
            cursor: None,
            border: None,
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
            UiCommand::ShowAction {
                kind,
                point,
                window_frame,
            } => state.show_action(kind, point, window_frame),
            UiCommand::ShowDrag {
                from,
                to,
                window_frame,
            } => state.show_drag(from, to, window_frame),
            UiCommand::Highlight(window_frame) => state.highlight(window_frame),
            UiCommand::Clear => state.clear(),
        }
    });
    if let Some(pool) = pool {
        let _ = send_void(pool, c"drain");
    }
}
