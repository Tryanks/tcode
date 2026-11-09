//! A socket an earlier run left behind is removed when the nest starts, if
//! that run's process is gone.
#![cfg(target_os = "linux")]

mod common;

use std::os::unix::net::UnixListener;

use wayland_nest::Nest;

#[test]
fn a_gone_runs_socket_is_removed_at_start() {
    let runtime = common::runtime_dir();
    // No process has this id: Linux caps process ids at 2^22.
    let gone = runtime.join(format!("tcode-nest-{}", i32::MAX));
    let running = runtime.join(format!(
        "tcode-nest-{}",
        rustix::process::getppid()
            .expect("a parent process")
            .as_raw_pid()
    ));
    for path in [&gone, &running] {
        drop(UnixListener::bind(path).expect("leave a socket behind"));
    }

    let _nest = Nest::start().expect("start the nest");

    assert!(!gone.exists(), "the gone run's socket is removed");
    assert!(running.exists(), "a running process's socket is kept");
}
