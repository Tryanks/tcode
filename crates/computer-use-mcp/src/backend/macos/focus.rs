use std::time::{Duration, Instant};

use core_graphics::display::CGDisplay;
use core_graphics::event::CGEvent;
use core_graphics::geometry::CGPoint;

use super::super::RootInfo;
use super::{ax, input};

pub(super) struct FocusGuard {
    previous_pid: Option<u32>,
}

impl FocusGuard {
    pub(super) fn acquire(root: &RootInfo) -> Self {
        let Some(previous_pid) = ax::frontmost_application_pid() else {
            log::debug!(
                "could not determine the frontmost macOS application; skipping focus guard"
            );
            return Self::noop();
        };
        if previous_pid == root.pid {
            return Self::noop();
        }
        if !ax::activate_application(root.pid) {
            log::debug!(
                "could not activate target macOS application pid {}; skipping focus guard",
                root.pid
            );
            return Self::noop();
        }
        if let Err(error) = ax::raise_window(root) {
            log::debug!("could not raise target macOS window: {error}; skipping focus guard");
            Self::restore_after_failed_acquire(previous_pid);
            return Self::noop();
        }

        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match ax::frontmost_application_pid() {
                Some(pid) if pid == root.pid => {
                    return Self {
                        previous_pid: Some(previous_pid),
                    };
                }
                Some(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Some(_) => {
                    log::debug!(
                        "target macOS application pid {} did not become frontmost within 500ms; skipping focus guard",
                        root.pid
                    );
                    Self::restore_after_failed_acquire(previous_pid);
                    return Self::noop();
                }
                None => {
                    log::debug!(
                        "could not poll the frontmost macOS application; skipping focus guard"
                    );
                    Self::restore_after_failed_acquire(previous_pid);
                    return Self::noop();
                }
            }
        }
    }

    fn noop() -> Self {
        Self { previous_pid: None }
    }

    fn restore_after_failed_acquire(previous_pid: u32) {
        if !ax::activate_application(previous_pid) {
            log::debug!(
                "could not restore macOS application pid {previous_pid} after focus acquisition failed"
            );
        }
    }
}

impl Drop for FocusGuard {
    fn drop(&mut self) {
        if let Some(previous_pid) = self.previous_pid
            && !ax::activate_application(previous_pid)
        {
            log::debug!("could not restore previous macOS application pid {previous_pid}");
        }
    }
}

pub(super) struct CursorGuard {
    saved_point: Option<CGPoint>,
}

impl CursorGuard {
    pub(super) fn acquire() -> Self {
        let saved_point = match input::event_source().and_then(|source| {
            CGEvent::new(source).map_err(|()| {
                super::super::BackendError::new(
                    super::super::BackendErrorCode::OperationFailed,
                    "CoreGraphics could not create an event to read the cursor position",
                )
            })
        }) {
            Ok(event) => Some(event.location()),
            Err(error) => {
                log::debug!("could not save the macOS cursor position: {error}");
                None
            }
        };
        Self { saved_point }
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        if let Some(point) = self.saved_point
            && let Err(error) = CGDisplay::warp_mouse_cursor_position(point)
        {
            log::debug!("could not restore the macOS cursor position: {error:?}");
        }
    }
}
