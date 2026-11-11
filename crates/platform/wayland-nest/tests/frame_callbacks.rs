//! A view's next frame waits for the host: the nest answers the client's
//! frame callbacks, which GTK's frame clock waits on, only once the host has
//! presented the frame they belong to, and sends no other frame of the view
//! before then.
#![cfg(target_os = "linux")]

mod common;

use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_callback, wl_callback::WlCallback, wl_compositor::WlCompositor,
    wl_registry::WlRegistry, wl_shm, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_nest::{Event, Nest, ViewId};
use wayland_protocols::xdg::shell::client::{
    xdg_surface, xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel, xdg_wm_base,
    xdg_wm_base::XdgWmBase,
};

const VIEW: ViewId = ViewId(0);

#[derive(Default)]
struct Client {
    configured: Option<u32>,
    /// Frame callbacks answered, by the commit (1, 2) that requested them.
    done: Vec<u32>,
}

#[test]
fn a_frame_callback_is_answered_once_the_host_presents_its_frame() {
    let runtime = common::runtime_dir();
    let nest = Nest::start().expect("start the nest");
    nest.configure(VIEW, 64, 48);

    let stream = UnixStream::connect(runtime.join(nest.socket_name())).expect("connect");
    let connection = Connection::from_socket(stream).expect("wayland connection");
    let (globals, mut queue) = registry_queue_init::<Client>(&connection).expect("registry");
    let qh = queue.handle();
    let compositor: WlCompositor = globals.bind(&qh, 1..=4, ()).expect("wl_compositor");
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).expect("wl_shm");
    let wm_base: XdgWmBase = globals.bind(&qh, 1..=1, ()).expect("xdg_wm_base");

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title(VIEW.title());
    surface.commit();
    let mut client = Client::default();
    while client.configured.is_none() {
        queue.blocking_dispatch(&mut client).expect("dispatch");
    }
    xdg.ack_configure(client.configured.take().unwrap_or_default());

    // Draw, as a toolkit draws, asking to hear when to draw again.
    surface.frame(&qh, 1_u32);
    surface.attach(Some(&buffer(&shm, &qh)), 0, 0);
    surface.commit();
    connection.flush().expect("flush");
    common::next_frame(&nest, VIEW).expect("the first frame");

    // Draw again before the host has shown the first frame. The roundtrip
    // returns after the nest has handled the commit and sent whatever it
    // sends for it.
    surface.frame(&qh, 2_u32);
    surface.attach(Some(&buffer(&shm, &qh)), 0, 0);
    surface.commit();
    queue.roundtrip(&mut client).expect("roundtrip");
    assert!(
        client.done.is_empty(),
        "no callback is answered before the host presents"
    );
    assert!(
        nest.events().try_recv().is_err(),
        "no frame is sent while the last is unpresented"
    );

    // Presenting the first frame answers its callback and lets the second
    // frame through; that one's callback waits for its own presentation.
    nest.presented(VIEW);
    while client.done.is_empty() {
        queue.blocking_dispatch(&mut client).expect("dispatch");
    }
    assert_eq!(client.done, [1]);
    common::wait_for(&nest, |event| match event {
        Event::Frame { view, .. } if view == VIEW => Some(()),
        _ => None,
    })
    .expect("the second frame");
    queue.roundtrip(&mut client).expect("roundtrip");
    assert_eq!(client.done, [1]);
    nest.presented(VIEW);
    while client.done.len() < 2 {
        queue.blocking_dispatch(&mut client).expect("dispatch");
    }
    assert_eq!(client.done, [1, 2]);
}

/// An opaque 64 × 48 buffer.
fn buffer(shm: &WlShm, qh: &QueueHandle<Client>) -> WlBuffer {
    let (width, height) = (64, 48);
    let len = width * height * 4;
    let fd = rustix::fs::memfd_create("nest-test", rustix::fs::MemfdFlags::CLOEXEC).expect("memfd");
    rustix::fs::ftruncate(&fd, len as u64).expect("size the memfd");
    let pool = shm.create_pool(fd.as_fd(), len, qh, ());
    let buffer = pool.create_buffer(
        0,
        width,
        height,
        width * 4,
        wl_shm::Format::Xrgb8888,
        qh,
        (),
    );
    pool.destroy();
    buffer
}

impl Dispatch<WlCallback, u32> for Client {
    fn event(
        client: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        commit: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            client.done.push(*commit);
        }
    }
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

delegate_noop!(Client: ignore WlCompositor);
delegate_noop!(Client: ignore WlSurface);
delegate_noop!(Client: ignore WlShm);
delegate_noop!(Client: ignore WlShmPool);
delegate_noop!(Client: ignore WlBuffer);
delegate_noop!(Client: ignore XdgToplevel);
