//! GTK 3, WebKitGTK 4.1 and the libraries they bind, loaded when [`load`] is
//! called instead of when the process starts.
//!
//! A binary that links this crate keeps none of those libraries in its
//! `DT_NEEDED`, so it starts on a system without them: every function they
//! export is a local trampoline (see `build.rs`) that jumps through a slot
//! [`load`] fills. Until a successful [`load`], calling any of them aborts the
//! process with a message, so callers ask [`load`] first and treat an error as
//! "this machine has no WebKitGTK".

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CStr, c_void};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicPtr, Ordering};

    include!(concat!(env!("OUT_DIR"), "/symbols.rs"));

    /// Where each trampoline jumps: `unloaded` until [`load`] resolves it.
    static SLOTS: [AtomicPtr<c_void>; COUNT] =
        [const { AtomicPtr::new(unloaded as *mut c_void) }; COUNT];

    core::arch::global_asm!(
        include_str!(concat!(env!("OUT_DIR"), "/trampolines.s")),
        slots = sym SLOTS,
    );

    /// GObject's table of parameter types: the one variable, where everything
    /// else is a function, that a binding reads from these libraries (glib
    /// 0.18's `param_spec.rs` declares it itself; no `-sys` crate does). A
    /// trampoline cannot stand for a variable, so this is the binary's own
    /// copy of the pointer, taken once the library is loaded. GObject sets
    /// it when the library initializes and never again.
    #[unsafe(no_mangle)]
    #[allow(
        non_upper_case_globals,
        reason = "the name is the library's symbol, which the binding links by"
    )]
    static g_param_spec_types: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    /// What a trampoline runs when its library was never loaded, or the loaded
    /// one lacks the function the build machine's had.
    extern "C" fn unloaded() -> ! {
        eprintln!(
            "tcode: a GTK or WebKitGTK function was called that is not loaded \
             (gtk_dlopen::load failed, was not called, or the installed library is older)"
        );
        std::process::abort()
    }

    pub fn load() -> Result<(), String> {
        static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
        RESULT.get_or_init(load_all).clone()
    }

    fn load_all() -> Result<(), String> {
        let mut handles = Vec::with_capacity(LIBRARIES.len());
        for (soname, _) in LIBRARIES {
            // SAFETY: `soname` is NUL-terminated; a library's constructors are
            // what the dynamic loader would have run at startup.
            let handle =
                unsafe { libc::dlopen(soname.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
            if handle.is_null() {
                return Err(last_error(soname));
            }
            handles.push(handle);
        }
        for ((_, range), handle) in LIBRARIES.iter().zip(handles) {
            for index in range.clone() {
                // SAFETY: `handle` is open and never closed; the name is
                // NUL-terminated.
                let address = unsafe { libc::dlsym(handle, NAMES[index].as_ptr()) };
                if !address.is_null() {
                    SLOTS[index].store(address, Ordering::Release);
                }
            }
        }
        // SAFETY: every library is loaded globally by now; the name is
        // NUL-terminated.
        let types = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"g_param_spec_types".as_ptr()) };
        if types.is_null() {
            return Err("libgobject-2.0 has no g_param_spec_types".into());
        }
        // SAFETY: the symbol is a pointer GObject's initializer has set.
        g_param_spec_types.store(unsafe { *types.cast::<*mut c_void>() }, Ordering::Release);
        Ok(())
    }

    fn last_error(soname: &CStr) -> String {
        // SAFETY: dlerror returns a NUL-terminated string or null.
        let error = unsafe { libc::dlerror() };
        if error.is_null() {
            format!("{} could not be loaded", soname.to_string_lossy())
        } else {
            // SAFETY: checked non-null above.
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        }
    }
}

/// Loads the GTK stack once per process; later calls return the first answer.
/// The error is the dynamic loader's message for the first library that could
/// not be loaded.
#[cfg(target_os = "linux")]
pub use linux::load;
