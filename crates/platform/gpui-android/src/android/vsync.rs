//! The NDK frame source: `AChoreographer` vsync callbacks on the GPUI thread
//! and the surface's frame-rate vote, on devices whose `libandroid.so` has
//! them. Elsewhere the Java activity's `Choreographer` drives frames.

use super::host;
use ndk::native_window::NativeWindow;
use std::{
    cell::{Cell, OnceCell, RefCell},
    ffi::{CStr, c_void},
    ptr::NonNull,
};

type FrameCallback64 = unsafe extern "C" fn(frame_time_nanos: i64, data: *mut c_void);
type GetInstance = unsafe extern "C" fn() -> *mut c_void;
type PostFrameCallback64 =
    unsafe extern "C" fn(choreographer: *mut c_void, callback: FrameCallback64, data: *mut c_void);
type SetFrameRate =
    unsafe extern "C" fn(window: *mut c_void, frame_rate: f32, compatibility: i8) -> i32;

/// `ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_DEFAULT`, the value for UI content.
const FRAME_RATE_COMPATIBILITY_DEFAULT: i8 = 0;

pub(super) struct Vsync {
    choreographer: NonNull<c_void>,
    post_frame_callback: PostFrameCallback64,
    /// API 30+; on API 29 frames run natively without a vote, as they did in
    /// Java, whose `Surface.setFrameRate` is API 30 too.
    set_frame_rate: Option<SetFrameRate>,
    /// A posted callback cannot be withdrawn, so at most one is outstanding.
    /// It carries no pointer and acts only on the state when it fires.
    posted: Cell<bool>,
    surface: RefCell<Option<NativeWindow>>,
    voting: Cell<bool>,
    rate: Cell<f32>,
}

thread_local! {
    /// Set once per GPUI thread; `None` there means the Java path.
    static VSYNC: OnceCell<Option<Vsync>> = const { OnceCell::new() };
}

/// Resolves the NDK path on the GPUI thread, whose looper android-activity
/// prepared and polls: `AChoreographer_getInstance` binds to the calling
/// thread's looper, and the callbacks run inside that poll.
pub(super) fn initialize() {
    VSYNC.with(|slot| {
        slot.get_or_init(|| {
            let vsync = resolve();
            if vsync.is_none() {
                log::info!("AChoreographer_postFrameCallback64 unavailable; frames run from Java");
            }
            vsync
        });
    });
}

/// Runs `f` on the NDK frame source; returns `false` on the Java path.
pub(super) fn with(f: impl FnOnce(&Vsync)) -> bool {
    VSYNC
        .try_with(|slot| slot.get().and_then(Option::as_ref).map(f).is_some())
        .unwrap_or(false)
}

fn resolve() -> Option<Vsync> {
    // SAFETY: `libandroid.so` is a system library every app process has
    // loaded; the handle is kept for the process.
    let library = unsafe { libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW) };
    if library.is_null() {
        return None;
    }
    // SAFETY: each type is the NDK's prototype for that name.
    let (get_instance, post_frame_callback, set_frame_rate) = unsafe {
        (
            symbol::<GetInstance>(library, c"AChoreographer_getInstance")?,
            symbol::<PostFrameCallback64>(library, c"AChoreographer_postFrameCallback64")?,
            symbol::<SetFrameRate>(library, c"ANativeWindow_setFrameRate"),
        )
    };
    // SAFETY: called on the thread whose looper the callbacks need.
    let choreographer = NonNull::new(unsafe { get_instance() })?;
    Some(Vsync {
        choreographer,
        post_frame_callback,
        set_frame_rate,
        posted: Cell::new(false),
        surface: RefCell::new(None),
        voting: Cell::new(false),
        rate: Cell::new(0.0),
    })
}

/// # Safety
/// `T` must be the function pointer type of `name`.
unsafe fn symbol<T: Copy>(library: *mut c_void, name: &CStr) -> Option<T> {
    const { assert!(size_of::<T>() == size_of::<*mut c_void>()) };
    // SAFETY: `library` is a live handle and `name` is NUL-terminated.
    let address = unsafe { libc::dlsym(library, name.as_ptr()) };
    // SAFETY: the caller names `T` as the symbol's type.
    (!address.is_null()).then(|| unsafe { std::mem::transmute_copy(&address) })
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
                (self.post_frame_callback)(
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
        let Some(set_frame_rate) = self.set_frame_rate else {
            return;
        };
        let surface = self.surface.borrow();
        let Some(surface) = surface.as_ref() else {
            return;
        };
        // SAFETY: the window is held, and Android keeps it valid until
        // `TerminateWindow` has detached it.
        let status = unsafe {
            set_frame_rate(
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
