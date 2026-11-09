//! What every nest test binary shares: a runtime directory of its own and
//! waiting for the nest's events.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

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

/// Waits for the next event `pick` accepts, running `pump` between polls
/// (the GTK main loop, when the client lives on this thread). Other events
/// are dropped.
pub fn wait_for<T>(
    nest: &Nest,
    timeout: Duration,
    mut pump: impl FnMut(),
    mut pick: impl FnMut(Event) -> Option<T>,
) -> Option<T> {
    let events = nest.events();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        pump();
        match events.try_recv() {
            Ok(event) => {
                if let Some(found) = pick(event) {
                    return Some(found);
                }
            }
            Err(async_channel::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(async_channel::TryRecvError::Closed) => return None,
        }
    }
    None
}

/// The next frame of `view`.
pub fn next_frame(
    nest: &Nest,
    view: ViewId,
    timeout: Duration,
    pump: impl FnMut(),
) -> Option<Frame> {
    wait_for(nest, timeout, pump, |event| match event {
        Event::Frame { view: id, frame } if id == view => Some(frame),
        _ => None,
    })
}
