//! The frame source: `AChoreographer` vsync callbacks on the GPUI thread and
//! the surface's frame-rate vote.

use super::host;
use ndk::native_window::NativeWindow;
use std::{
    cell::{Cell, OnceCell, RefCell},
    ffi::c_void,
    ptr::NonNull,
};

type FrameCallback64 = unsafe extern "C" fn(frame_time_nanos: i64, data: *mut c_void);

#[link(name = "android")]
unsafe extern "C" {
    /// API 24.
    fn AChoreographer_getInstance() -> *mut c_void;
    /// API 29.
    fn AChoreographer_postFrameCallback64(
        choreographer: *mut c_void,
        callback: FrameCallback64,
        data: *mut c_void,
    );
    /// API 30.
    fn ANativeWindow_setFrameRate(window: *mut c_void, frame_rate: f32, compatibility: i8) -> i32;
}

/// `ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_DEFAULT`, the value for UI content.
const FRAME_RATE_COMPATIBILITY_DEFAULT: i8 = 0;

pub(super) struct Vsync {
    choreographer: NonNull<c_void>,
    /// A posted callback cannot be withdrawn, so at most one is outstanding.
    /// It carries no pointer and acts only on the state when it fires.
    posted: Cell<bool>,
    surface: RefCell<Option<NativeWindow>>,
    voting: Cell<bool>,
    rate: Cell<f32>,
}

thread_local! {
    /// Set once, on the GPUI thread.
    static VSYNC: OnceCell<Vsync> = const { OnceCell::new() };
}

/// Binds the frame source to the GPUI thread, whose looper android-activity
/// prepared and polls: `AChoreographer_getInstance` binds to the calling
/// thread's looper, and the callbacks run inside that poll.
pub(super) fn initialize() {
    VSYNC.with(|slot| {
        slot.get_or_init(|| {
            // SAFETY: called on the thread whose looper the callbacks need.
            let choreographer = unsafe { AChoreographer_getInstance() };
            Vsync {
                choreographer: NonNull::new(choreographer)
                    .expect("the GPUI thread has a looper for AChoreographer"),
                posted: Cell::new(false),
                surface: RefCell::new(None),
                voting: Cell::new(false),
                rate: Cell::new(0.0),
            }
        });
    });
}

/// Runs `f` on the frame source, once [`initialize`] has run on this thread.
pub(super) fn with(f: impl FnOnce(&Vsync)) {
    let _ = VSYNC.try_with(|slot| slot.get().map(f));
}

unsafe extern "C" fn on_vsync(_frame_time_nanos: i64, _data: *mut c_void) {
    with(Vsync::fired);
}

impl Vsync {
    /// Frames are wanted: vote and make sure the next vsync calls back.
    pub(super) fn start(&self) {
        if self.surface.borrow().is_none() {
            return;
        }
        if !self.voting.replace(true) {
            self.vote(self.rate.get());
        }
        if !self.posted.replace(true) {
            // SAFETY: `choreographer` belongs to this thread, and `on_vsync`
            // reads only this thread's state.
            unsafe {
                AChoreographer_postFrameCallback64(
                    self.choreographer.as_ptr(),
                    on_vsync,
                    std::ptr::null_mut(),
                )
            };
        }
    }

    /// Frames stopped: release the vote. A posted callback still fires once
    /// and finds frames unwanted.
    pub(super) fn stop(&self) {
        if self.voting.replace(false) {
            // Zero releases the vote while the last buffer remains visible.
            self.vote(0.0);
        }
    }

    pub(super) fn attach(&self, surface: NativeWindow) {
        *self.surface.borrow_mut() = Some(surface);
        if self.voting.get() {
            self.vote(self.rate.get());
        }
    }

    /// Releases the vote and the window while Android still keeps it valid.
    pub(super) fn detach(&self) {
        self.stop();
        self.surface.borrow_mut().take();
    }

    pub(super) fn set_rate(&self, rate: f32) {
        if self.rate.replace(rate) != rate && self.voting.get() {
            self.vote(rate);
        }
    }

    fn fired(&self) {
        self.posted.set(false);
        if self.surface.borrow().is_some() {
            host::vsync();
        }
    }

    fn vote(&self, rate: f32) {
        let surface = self.surface.borrow();
        let Some(surface) = surface.as_ref() else {
            return;
        };
        // SAFETY: the window is held, and Android keeps it valid until
        // `TerminateWindow` has detached it.
        let status = unsafe {
            ANativeWindow_setFrameRate(
                surface.ptr().as_ptr().cast(),
                rate,
                FRAME_RATE_COMPATIBILITY_DEFAULT,
            )
        };
        if status != 0 {
            log::warn!("ANativeWindow_setFrameRate({rate}) failed: {status}");
        }
    }
}
