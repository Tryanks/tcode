//! What every nest test binary shares: a session of its own to be hosted in.
//!
//! The session is sway on its headless backend, started by the test in a
//! runtime directory of its own, so the nest draws on a real compositor's
//! surface, is paced by its frame callbacks and hears its pointer. The test
//! is the host: a client of that session with one fullscreen surface, which
//! it hands to the nest. What the session shows is read back the way a
//! screenshot tool reads it.
#![allow(dead_code)]

pub mod page;

use std::ffi::c_void;
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::ptr::NonNull;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_output::WlOutput, wl_registry::WlRegistry,
    wl_seat::WlSeat, wl_shm, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{
    xdg_surface, xdg_surface::XdgSurface, xdg_toplevel, xdg_toplevel::XdgToplevel, xdg_wm_base,
    xdg_wm_base::XdgWmBase,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

/// The session's one output, which the host's surface fills.
pub const WIDTH: u32 = 800;
pub const HEIGHT: u32 = 600;

/// A headless sway and the test's connection to it as the host.
pub struct Session {
    sway: Child,
    connection: Connection,
    _queue: EventQueue<Client>,
    surface: WlSurface,
    pointer: ZwlrVirtualPointerV1,
    _toplevel: XdgToplevel,
    /// Asks the observer what the session shows at a point of the output.
    look: std::sync::mpsc::Sender<(u32, u32)>,
    seen: async_channel::Receiver<[u8; 3]>,
}

#[derive(Default)]
struct Client {
    configured: Option<u32>,
    /// The layout of the screenshot the session offers.
    offered: Option<(wl_shm::Format, u32, u32, u32)>,
    /// Whether the screenshot asked for was taken, once the session says.
    taken: Option<bool>,
}

impl Session {
    /// Starts sway in a fresh `XDG_RUNTIME_DIR`, where [`wayland_nest::Nest`]
    /// also binds its socket, and maps the host's surface.
    ///
    /// Each test binary holds one test and calls this before starting
    /// anything else, so no other thread reads the environment while it
    /// changes.
    pub fn start() -> Self {
        let runtime = runtime_dir();
        let config = runtime.join("sway.conf");
        std::fs::write(
            &config,
            format!("output HEADLESS-1 resolution {WIDTH}x{HEIGHT}\ndefault_border none\n"),
        )
        .expect("write sway's configuration");
        // The crate is below the process helpers `services` has, and sway
        // is this test's session, not a harness.
        #[allow(clippy::disallowed_methods)]
        let sway = std::process::Command::new("sway")
            .arg("-c")
            .arg(&config)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start sway, the session these tests are hosted in");
        let stream = connect(&runtime);
        let observer = connect_again(&runtime);
        let connection = Connection::from_socket(stream).expect("wayland connection");
        let (globals, mut queue) = registry_queue_init::<Client>(&connection).expect("registry");
        let qh = queue.handle();
        let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).expect("wl_compositor");
        let wm_base: XdgWmBase = globals.bind(&qh, 1..=2, ()).expect("xdg_wm_base");
        let seat: WlSeat = globals.bind(&qh, 5..=8, ()).expect("wl_seat");
        let pointers: ZwlrVirtualPointerManagerV1 =
            globals.bind(&qh, 1..=2, ()).expect("virtual pointer");

        let surface = compositor.create_surface(&qh, ());
        let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_fullscreen(None);
        surface.commit();
        let mut client = Client::default();
        while client.configured.is_none() {
            queue.blocking_dispatch(&mut client).expect("dispatch");
        }
        xdg.ack_configure(client.configured.take().unwrap_or_default());
        let pointer = pointers.create_virtual_pointer(Some(&seat), &qh, ());
        connection.flush().expect("flush");
        let (look, looks) = std::sync::mpsc::channel();
        let (saw, seen) = async_channel::unbounded();
        let observer = observer.expect("a second connection to sway");
        std::thread::spawn(move || observe(observer, looks, saw));
        Self {
            sway,
            connection,
            _queue: queue,
            surface,
            pointer,
            _toplevel: toplevel,
            look,
            seen,
        }
    }

    /// The session's `wl_display`, as the host gives it to the nest.
    pub fn display(&self) -> NonNull<c_void> {
        NonNull::new(self.connection.backend().display_ptr().cast()).expect("a display")
    }

    /// The host's `wl_surface`, which the nest draws a view on.
    pub fn surface(&self) -> NonNull<c_void> {
        NonNull::new(self.surface.id().as_ptr().cast()).expect("a surface")
    }

    /// Moves the session's pointer to `(x, y)` of the output and clicks.
    pub fn click(&self, x: u32, y: u32) {
        use wayland_client::protocol::wl_pointer::ButtonState;
        const BTN_LEFT: u32 = 0x110;
        self.pointer.motion_absolute(0, x, y, WIDTH, HEIGHT);
        self.pointer.frame();
        for state in [ButtonState::Pressed, ButtonState::Released] {
            self.pointer.button(0, BTN_LEFT, state);
            self.pointer.frame();
        }
        self.connection.flush().expect("flush");
    }

    /// What the session shows at `(x, y)` of the output, read after this
    /// call: red, green and blue.
    pub async fn shown(&self, x: u32, y: u32) -> [u8; 3] {
        self.look.send((x, y)).expect("the observer is running");
        self.seen.recv().await.expect("the observer is running")
    }
}

/// Answers each point asked for with what the session shows there, as a
/// screenshot tool would: a client of its own taking a screenshot of the
/// output. The session takes one with its next frame, so asking again waits
/// for the session, not for a clock.
fn observe(
    stream: UnixStream,
    looks: std::sync::mpsc::Receiver<(u32, u32)>,
    saw: async_channel::Sender<[u8; 3]>,
) {
    let connection = Connection::from_socket(stream).expect("wayland connection");
    let (globals, mut queue) = registry_queue_init::<Client>(&connection).expect("registry");
    let qh = queue.handle();
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).expect("wl_shm");
    let output: WlOutput = globals.bind(&qh, 1..=4, ()).expect("wl_output");
    let screencopy: ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=3, ()).expect("screencopy");
    let mut client = Client::default();
    for (x, y) in looks {
        let frame = screencopy.capture_output(0, &output, &qh, ());
        client.offered = None;
        while client.offered.is_none() {
            queue.blocking_dispatch(&mut client).expect("dispatch");
        }
        let (format, width, height, stride) = client.offered.take().expect("a layout");
        assert!(
            matches!(
                format,
                wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 | wl_shm::Format::Xbgr8888
            ),
            "a screenshot format this test does not read: {format:?}"
        );
        let len = (stride * height) as usize;
        let fd =
            rustix::fs::memfd_create("nest-test", rustix::fs::MemfdFlags::CLOEXEC).expect("memfd");
        rustix::fs::ftruncate(&fd, len as u64).expect("size the memfd");
        let file = std::fs::File::from(fd);
        let pool = shm.create_pool(file.as_fd(), len as i32, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &qh,
            (),
        );
        pool.destroy();
        frame.copy(&buffer);
        client.taken = None;
        while client.taken.is_none() {
            queue.blocking_dispatch(&mut client).expect("dispatch");
        }
        assert_eq!(client.taken, Some(true), "the session took no screenshot");
        buffer.destroy();
        // SAFETY: the session has finished writing the screenshot.
        let pixels = unsafe { memmap2::Mmap::map(&file) }.expect("map the screenshot");
        let at = (y * stride + x * 4) as usize;
        let pixel = &pixels[at..at + 4];
        let rgb = match format {
            wl_shm::Format::Xbgr8888 => [pixel[0], pixel[1], pixel[2]],
            _ => [pixel[2], pixel[1], pixel[0]],
        };
        if saw.send_blocking(rgb).is_err() {
            return;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.sway.kill();
        let _ = self.sway.wait();
    }
}

/// Points `XDG_RUNTIME_DIR` at a fresh directory.
fn runtime_dir() -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = std::env::temp_dir().join(format!("wayland-nest-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the runtime directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("restrict the runtime directory");
    // SAFETY: see `Session::start`; no other thread is running yet.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
    dir
}

/// Connects to sway once it listens. sway says so by creating its socket
/// after the lock file beside it, so the directory is watched for the socket.
fn connect(runtime: &Path) -> UnixStream {
    use rustix::fs::inotify;
    let watch = inotify::init(inotify::CreateFlags::CLOEXEC).expect("inotify");
    inotify::add_watch(&watch, runtime, inotify::WatchFlags::CREATE).expect("watch");
    let mut buffer = [std::mem::MaybeUninit::uninit(); 4096];
    let mut reader = inotify::Reader::new(watch, &mut buffer);
    loop {
        let socket = std::fs::read_dir(runtime)
            .expect("read the runtime directory")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
            });
        if let Some(socket) = socket
            && let Ok(stream) = UnixStream::connect(&socket)
        {
            return stream;
        }
        reader.next().expect("wait for sway's socket");
    }
}

/// A second connection to the session `connect` reached.
fn connect_again(runtime: &Path) -> Option<UnixStream> {
    std::fs::read_dir(runtime)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
        })
        .find_map(|path| UnixStream::connect(path).ok())
}

impl Dispatch<XdgSurface, ()> for Client {
    fn event(
        client: &mut Self,
        _: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            client.configured = Some(serial);
        }
    }
}

impl Dispatch<XdgWmBase, ()> for Client {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for Client {
    fn event(
        client: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: wayland_client::WEnum::Value(format),
                width,
                height,
                stride,
            } => client.offered = Some((format, width, height, stride)),
            zwlr_screencopy_frame_v1::Event::Ready { .. } => client.taken = Some(true),
            zwlr_screencopy_frame_v1::Event::Failed => client.taken = Some(false),
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgToplevel, ()> for Client {
    fn event(
        _: &mut Self,
        _: &XdgToplevel,
        _: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Client: ignore WlCompositor);
delegate_noop!(Client: ignore WlSurface);
delegate_noop!(Client: ignore WlShm);
delegate_noop!(Client: ignore WlShmPool);
delegate_noop!(Client: ignore WlBuffer);
delegate_noop!(Client: ignore WlSeat);
delegate_noop!(Client: ignore WlOutput);
delegate_noop!(Client: ZwlrVirtualPointerManagerV1);
delegate_noop!(Client: ZwlrVirtualPointerV1);
delegate_noop!(Client: ZwlrScreencopyManagerV1);
