//! What every nest test binary shares: a runtime directory of its own and
//! waiting for the nest's events.
#![allow(dead_code)]

pub mod page;

use std::path::PathBuf;

use wayland_nest::{Event, Frame, Nest, ViewId};

/// Points `XDG_RUNTIME_DIR` at a fresh directory, where [`Nest::start`] binds
/// its socket.
///
/// Each test binary holds one test and calls this before starting anything
/// else, so no other thread reads the environment while it changes.
pub fn runtime_dir() -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = std::env::temp_dir().join(format!("wayland-nest-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the runtime directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("restrict the runtime directory");
    // SAFETY: see above; no other thread is running yet.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
    dir
}

/// Blocks until the nest sends an event `pick` accepts; other events are
/// dropped. `None` when the nest has stopped. A client on this thread waits
/// with [`page::Page::wait_for`], which runs its GTK loop meanwhile.
pub fn wait_for<T>(nest: &Nest, mut pick: impl FnMut(Event) -> Option<T>) -> Option<T> {
    let events = nest.events();
    while let Ok(event) = events.recv_blocking() {
        if let Some(found) = pick(event) {
            return Some(found);
        }
    }
    None
}

/// The next frame of `view`.
pub fn next_frame(nest: &Nest, view: ViewId) -> Option<Frame> {
    wait_for(nest, |event| match event {
        Event::Frame { view: id, frame } if id == view => Some(frame),
        _ => None,
    })
}
