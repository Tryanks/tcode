//! GTK, started in this process on a nest and run from the host's own loop.

use std::convert::Infallible;
use std::ffi::{CString, c_char, c_int};
use std::sync::OnceLock;

use gtk::glib::ffi::{
    GPollFD, g_main_context_acquire, g_main_context_check, g_main_context_default,
    g_main_context_dispatch, g_main_context_prepare, g_main_context_query,
};
use gtk::glib::gobject_ffi::{GObject, GObjectClass, g_type_class_ref};
use gtk::glib::translate::IntoGlib as _;
use gtk::prelude::*;

use crate::Nest;

impl Nest {
    /// Starts GTK on this nest. GTK initializes once per process and reads
    /// its display only then, so every GTK window of the process lives on
    /// this nest.
    pub fn start_gtk(&self) -> Result<(), String> {
        if gtk::is_initialized() {
            return Err("GTK is already running on another display".into());
        }
        let socket = self.socket_name();
        gtk::gdk::set_allowed_backends("wayland");
        let args = ["tcode", "--display", socket]
            .map(|arg| CString::new(arg).map_err(|error| error.to_string()));
        let args = args.into_iter().collect::<Result<Vec<_>, _>>()?;
        let mut pointers: Vec<*mut c_char> =
            args.iter().map(|arg| arg.as_ptr().cast_mut()).collect();
        let mut argc = pointers.len() as c_int;
        let mut argv = pointers.as_mut_ptr();
        // SAFETY: argv points at `argc` NUL-terminated strings that outlive
        // the call; GTK only reorders the pointer array.
        let started = unsafe { gtk::ffi::gtk_init_check(&mut argc, &mut argv) } != 0;
        if !started {
            return Err(format!(
                "GTK could not connect to the nested compositor ({socket})"
            ));
        }
        // Marks GTK initialized for gtk-rs; GTK itself is already up.
        gtk::init().map_err(|error| error.to_string())?;
        let display = gtk::gdk::Display::default().ok_or("GTK has no display")?;
        if display.type_().name() != "GdkWaylandDisplay" {
            return Err(format!(
                "GTK chose {} instead of the nested compositor",
                display.type_().name()
            ));
        }
        use_text_input_v3();
        Ok(())
    }
}

/// Runs GTK on the thread that started it, for the life of the process.
///
/// The thread's own loop keeps running: this future is ready to be polled
/// again only when GTK has something to do. GLib splits one turn of its main
/// loop into the steps used here so that another loop can do the waiting;
/// the waiting itself happens on a thread of its own, which wakes this
/// future when one of GLib's descriptors is ready or its next timer is due.
pub async fn run_gtk() -> Infallible {
    let (requests, waits) = std::sync::mpsc::channel::<(Vec<libc::pollfd>, c_int)>();
    let (answers, woken) = async_channel::bounded::<Vec<libc::pollfd>>(1);
    std::thread::Builder::new()
        .name("gtk-wait".into())
        .spawn(move || {
            for (mut descriptors, timeout) in waits {
                // SAFETY: `descriptors` is a valid array of its own length.
                // A failed poll (EINTR) reports nothing ready, and the next
                // turn asks again.
                unsafe {
                    libc::poll(
                        descriptors.as_mut_ptr(),
                        descriptors.len() as libc::nfds_t,
                        timeout,
                    )
                };
                if answers.send_blocking(descriptors).is_err() {
                    return;
                }
            }
        })
        .expect("spawn the thread that waits for GTK");

    // SAFETY: the default context is GTK's, and this is GTK's thread.
    let context = unsafe { g_main_context_default() };
    // SAFETY: as above. No other thread runs this context, so it is free.
    assert!(
        unsafe { g_main_context_acquire(context) } != 0,
        "GTK's main context is run by another thread"
    );
    let mut descriptors: Vec<GPollFD> = Vec::new();
    loop {
        let mut priority = 0;
        let mut timeout = 0;
        // SAFETY: the context is acquired by this thread, and `descriptors`
        // is valid for the length passed.
        let count = unsafe {
            g_main_context_prepare(context, &mut priority);
            loop {
                let count = g_main_context_query(
                    context,
                    priority,
                    &mut timeout,
                    descriptors.as_mut_ptr(),
                    descriptors.len() as c_int,
                ) as usize;
                if count <= descriptors.len() {
                    break count;
                }
                descriptors.resize(
                    count,
                    GPollFD {
                        fd: -1,
                        events: 0,
                        revents: 0,
                    },
                );
            }
        };
        let wait = descriptors[..count]
            .iter()
            .map(|descriptor| libc::pollfd {
                fd: descriptor.fd,
                events: descriptor.events as libc::c_short,
                revents: 0,
            })
            .collect();
        requests
            .send((wait, timeout))
            .expect("the thread that waits for GTK is running");
        let ready = woken
            .recv()
            .await
            .expect("the thread that waits for GTK is running");
        for (descriptor, ready) in descriptors.iter_mut().zip(&ready) {
            descriptor.revents = ready.revents as u16;
        }
        // SAFETY: as above. GLib notices by itself when its sources changed
        // while this future waited, and then dispatches nothing this turn.
        unsafe {
            if g_main_context_check(context, priority, descriptors.as_mut_ptr(), count as c_int)
                != 0
            {
                g_main_context_dispatch(context);
            }
        }
    }
}

/// The `constructed` of `GtkIMMulticontext`'s class before
/// [`use_text_input_v3`] replaced it.
static CONSTRUCTED: OnceLock<Option<unsafe extern "C" fn(*mut GObject)>> = OnceLock::new();

/// Gives GTK's input method contexts (WebKitGTK's among them) GTK's
/// "wayland" module, the one that speaks the nest's text-input-v3.
///
/// GTK 3 picks a module for a `GtkIMMulticontext` from GTK_IM_MODULE first,
/// the `gtk-im-module` setting after it, and the locale last, which never
/// selects "wayland" (`_gtk_im_module_get_default_context_id`,
/// gtk/gtkimmodule.c in GTK 3.24). So the setting covers a session that
/// names no module. Users of fcitx or IBus name theirs in GTK_IM_MODULE,
/// and that module would neither see the nest's text input nor leave the
/// nest's keys alone; only a context's own id outranks the environment, and
/// WebKitGTK keeps its context private, so then the class sets that id on
/// every context as it is constructed. Changing the environment instead
/// would race the threads already running.
fn use_text_input_v3() {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_im_module(Some("wayland"));
    }
    if std::env::var_os("GTK_IM_MODULE").is_none_or(|module| module == "wayland") {
        return;
    }
    // SAFETY: the class is referenced for the life of the process, so the
    // pointer stays valid; GTK runs on this thread only, and its class
    // structure is writable by design (subclasses fill it in the same way).
    unsafe {
        let class =
            g_type_class_ref(gtk::IMMulticontext::static_type().into_glib()).cast::<GObjectClass>();
        if CONSTRUCTED.set((*class).constructed).is_ok() {
            (*class).constructed = Some(constructed);
        }
    }
}

unsafe extern "C" fn constructed(object: *mut GObject) {
    if let Some(Some(parent)) = CONSTRUCTED.get() {
        // SAFETY: GObject calls this with the object being constructed.
        unsafe { parent(object) };
    }
    // SAFETY: only `GtkIMMulticontext`'s class has this function.
    unsafe {
        gtk::ffi::gtk_im_multicontext_set_context_id(object.cast(), c"wayland".as_ptr());
    }
}
