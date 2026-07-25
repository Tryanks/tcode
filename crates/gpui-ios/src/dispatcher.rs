//! `PlatformDispatcher` over Grand Central Dispatch.
//!
//! The Android backend hand-rolls a priority queue and thread pool, because
//! GPUI's reusable one is cfg-gated away from that target and Android has no
//! equivalent system facility. iOS does: libdispatch is part of libSystem, it is
//! already the scheduler UIKit itself runs on, and its global queues map
//! directly onto GPUI's priorities. Reusing it is both less code and better
//! behaved than a second thread pool competing with the system's.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use gpui::{PlatformDispatcher, Priority, RunnableVariant};

// libdispatch, from libSystem. Declared rather than pulled in as a crate: these
// five symbols are the entire surface used, and a dependency would add a
// version to track for no benefit.
#[allow(non_camel_case_types)]
type dispatch_queue_t = *mut c_void;
#[allow(non_camel_case_types)]
type dispatch_function_t = extern "C" fn(*mut c_void);

unsafe extern "C" {
    static _dispatch_main_q: c_void;

    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> dispatch_queue_t;
    fn dispatch_async_f(queue: dispatch_queue_t, context: *mut c_void, work: dispatch_function_t);
    fn dispatch_after_f(
        when: u64,
        queue: dispatch_queue_t,
        context: *mut c_void,
        work: dispatch_function_t,
    );
    fn dispatch_time(base: u64, delta: i64) -> u64;
}

/// libdispatch's quality-of-service classes, from `dispatch/queue.h`.
const QOS_CLASS_USER_INTERACTIVE: isize = 0x21;
const QOS_CLASS_USER_INITIATED: isize = 0x19;
const QOS_CLASS_UTILITY: isize = 0x11;

/// `DISPATCH_TIME_NOW`.
const DISPATCH_TIME_NOW: u64 = 0;

fn main_queue() -> dispatch_queue_t {
    // Taking the address of an extern static is how libdispatch's own C macro
    // resolves the main queue, and `&raw const` needs no unsafe block: nothing
    // is read, only the pointer formed.
    &raw const _dispatch_main_q as dispatch_queue_t
}

fn global_queue(priority: Priority) -> dispatch_queue_t {
    // Mapped onto QoS rather than collapsed to one queue: the OS uses these to
    // decide thread priority and, on battery, whether to run the work at all.
    // Telling it everything is user-interactive would be a lie that costs
    // battery.
    let qos = match priority {
        // Audio work asks for the strongest guarantee an app can request. iOS
        // reserves true realtime threads for AVAudioSession clients, so
        // user-interactive is the honest ceiling rather than a promise the
        // platform will not keep.
        Priority::RealtimeAudio | Priority::High => QOS_CLASS_USER_INTERACTIVE,
        Priority::Medium => QOS_CLASS_USER_INITIATED,
        Priority::Low => QOS_CLASS_UTILITY,
    };
    // SAFETY: a well-known QoS constant with no flags.
    unsafe { dispatch_get_global_queue(qos, 0) }
}

/// Trampoline: libdispatch calls a C function with an opaque pointer, so the
/// runnable crosses as a leaked box and is reclaimed exactly once here.
extern "C" fn run_boxed(context: *mut c_void) {
    // SAFETY: `context` came from `Box::into_raw` in the paired dispatch call,
    // and libdispatch invokes this exactly once per submission.
    let runnable = unsafe { Box::from_raw(context.cast::<RunnableVariant>()) };
    runnable.run();
}

fn submit(queue: dispatch_queue_t, runnable: RunnableVariant) {
    let context = Box::into_raw(Box::new(runnable)).cast::<c_void>();
    // SAFETY: the queue is a libSystem global, and `run_boxed` reclaims the box.
    unsafe { dispatch_async_f(queue, context, run_boxed) };
}

pub struct IosDispatcher;

impl IosDispatcher {
    pub fn new() -> Self {
        Self
    }
}

impl Default for IosDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformDispatcher for IosDispatcher {
    fn is_main_thread(&self) -> bool {
        // `pthread_main_np` rather than `NSThread.isMainThread`: it needs no
        // Objective-C runtime call and is valid from any thread, including ones
        // GPUI spawned that never touched UIKit.
        unsafe extern "C" {
            fn pthread_main_np() -> i32;
        }
        // SAFETY: a libSystem call with no arguments and no preconditions.
        unsafe { pthread_main_np() != 0 }
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        submit(global_queue(priority), runnable);
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        // The main queue is serial and has no QoS to choose; priority is
        // meaningful only in ordering against other main-queue work, which
        // libdispatch already handles FIFO.
        submit(main_queue(), runnable);
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let context = Box::into_raw(Box::new(runnable)).cast::<c_void>();
        // Saturating: a delay beyond i64 nanoseconds is ~292 years, and
        // wrapping would fire the timer immediately — the opposite of intent.
        let delta = i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX);
        // SAFETY: `run_boxed` reclaims the box exactly once when the timer fires.
        unsafe {
            let when = dispatch_time(DISPATCH_TIME_NOW, delta);
            dispatch_after_f(when, global_queue(Priority::Medium), context, run_boxed);
        }
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        // iOS has no public realtime thread API for apps — that is reserved for
        // audio units through `AVAudioSession`. User-interactive QoS is the
        // highest an app can honestly ask for, so it is what this returns rather
        // than pretending to a guarantee the platform does not offer.
        extern "C" fn run_closure(context: *mut c_void) {
            // SAFETY: paired with the `Box::into_raw` below.
            let closure = unsafe { Box::from_raw(context.cast::<Box<dyn FnOnce() + Send>>()) };
            closure();
        }
        let context = Box::into_raw(Box::new(f)).cast::<c_void>();
        // SAFETY: `run_closure` reclaims the box exactly once.
        unsafe { dispatch_async_f(global_queue(Priority::High), context, run_closure) };
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

impl IosDispatcher {
    /// Symmetry with the Android dispatcher, which owns a hand-rolled main-queue
    /// and must be pumped. libdispatch delivers to the main queue itself, so
    /// there is nothing to drain — the entry point exists so the shell's wake
    /// callback has the same shape on both platforms.
    pub fn drain_main_thread(&self) {}
}
