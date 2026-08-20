use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::DictationEvent;

type EventCallback = unsafe extern "C" fn(*mut c_void, c_int, *const c_char);

unsafe extern "C" {
    fn voice_is_supported(locale: *const c_char) -> bool;
    fn voice_start(
        locale: *const c_char,
        context: *mut c_void,
        callback: EventCallback,
    ) -> *mut c_void;
    fn voice_start_file(
        locale: *const c_char,
        path: *const c_char,
        context: *mut c_void,
        callback: EventCallback,
    ) -> *mut c_void;
    fn voice_stop(session: *mut c_void);
    fn voice_free(session: *mut c_void);
}

struct CallbackState {
    callback: Box<dyn Fn(DictationEvent) + Send + Sync>,
    terminal: AtomicBool,
}

unsafe extern "C" fn event_callback(context: *mut c_void, kind: c_int, text: *const c_char) {
    let pointer = context.cast::<CallbackState>();

    // Keep a temporary strong reference for the duration of this callback. The
    // raw context owns another reference until the terminal event below.
    unsafe { Arc::increment_strong_count(pointer) };
    let state = unsafe { Arc::from_raw(pointer) };

    if state.terminal.load(Ordering::Acquire) {
        return;
    }

    let string = || {
        if text.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(text) }
                .to_string_lossy()
                .into_owned()
        }
    };
    let (event, terminal) = match kind {
        0 => (DictationEvent::Ready, false),
        1 => (DictationEvent::Volatile(string()), false),
        2 => (DictationEvent::Final(string()), false),
        3 => (DictationEvent::Error(string()), true),
        4 => (DictationEvent::Ended, true),
        _ => return,
    };

    if terminal
        && state
            .terminal
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return;
    }

    let _ = catch_unwind(AssertUnwindSafe(|| (state.callback)(event)));

    if terminal {
        // Release the strong reference transferred to Swift as the context.
        drop(unsafe { Arc::from_raw(pointer) });
    }
}

pub struct Session {
    raw: *mut c_void,
    _callback: Arc<CallbackState>,
}

// The shim serializes access to its opaque session and accepts stop/free from
// any thread. CallbackState itself contains only Send + Sync data.
unsafe impl Send for Session {}

impl Session {
    pub fn stop(&self) {
        unsafe { voice_stop(self.raw) };
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { voice_free(self.raw) };
    }
}

pub fn is_supported() -> bool {
    let locale = CString::new(super::preferred_locale("en")).expect("locale contains no NUL");
    unsafe { voice_is_supported(locale.as_ptr()) }
}

pub fn start(
    locale: &str,
    on_event: Box<dyn Fn(DictationEvent) + Send + Sync>,
) -> Result<Session, String> {
    start_inner(locale, None, on_event)
}

pub fn start_file(
    locale: &str,
    path: &str,
    on_event: Box<dyn Fn(DictationEvent) + Send + Sync>,
) -> Result<Session, String> {
    start_inner(locale, Some(path), on_event)
}

fn start_inner(
    locale: &str,
    path: Option<&str>,
    on_event: Box<dyn Fn(DictationEvent) + Send + Sync>,
) -> Result<Session, String> {
    let locale = CString::new(locale).map_err(|_| "locale contains an interior NUL byte")?;
    let path = path
        .map(CString::new)
        .transpose()
        .map_err(|_| "audio path contains an interior NUL byte")?;
    let callback = Arc::new(CallbackState {
        callback: on_event,
        terminal: AtomicBool::new(false),
    });
    let context = Arc::into_raw(Arc::clone(&callback)).cast_mut().cast();
    let raw = unsafe {
        match path.as_ref() {
            Some(path) => voice_start_file(locale.as_ptr(), path.as_ptr(), context, event_callback),
            None => voice_start(locale.as_ptr(), context, event_callback),
        }
    };

    if raw.is_null() {
        drop(unsafe { Arc::from_raw(context.cast::<CallbackState>()) });
        return Err("failed to create dictation session".into());
    }

    Ok(Session {
        raw,
        _callback: callback,
    })
}
