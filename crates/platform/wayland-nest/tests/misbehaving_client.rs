//! A client that sends geometry no toolkit would — a subsurface and a popup
//! offset to the edge of `i32` — does not stop the nest. GTK is that client
//! in Tcode's own process, and GDK exits the process when the compositor it
//! is connected to goes away.
#![cfg(target_os = "linux")]

mod common;

use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_registry::WlRegistry, wl_shm,
    wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_subcompositor::WlSubcompositor,
    wl_subsurface::WlSubsurface, wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_nest::{Nest, ViewId};
use wayland_protocols::xdg::shell::client::{
    xdg_popup::XdgPopup, xdg_positioner::XdgPositioner, xdg_surface, xdg_surface::XdgSurface,
    xdg_toplevel::XdgToplevel, xdg_wm_base, xdg_wm_base::XdgWmBase,
};

const VIEW: ViewId = ViewId(0);

#[derive(Default)]
struct Client {
    configured: Option<u32>,
}

#[test]
fn a_client_with_absurd_geometry_does_not_stop_the_nest() {
    let runtime = common::runtime_dir();
    let nest = Nest::start().expect("start the nest");
    nest.configure(VIEW, 64, 48);

    let stream = UnixStream::connect(runtime.join(nest.socket_name())).expect("connect");
    let connection = Connection::from_socket(stream).expect("wayland connection");
    let (globals, mut queue) = registry_queue_init::<Client>(&connection).expect("registry");
    let qh = queue.handle();
    let compositor: WlCompositor = globals.bind(&qh, 1..=4, ()).expect("wl_compositor");
    let subcompositor: WlSubcompositor = globals.bind(&qh, 1..=1, ()).expect("wl_subcompositor");
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
    surface.attach(Some(&buffer(&shm, &qh, 64, 48)), 0, 0);
    surface.commit();
    connection.flush().expect("flush");
    let first = common::next_frame(&nest, VIEW).expect("a first frame");
    assert_eq!((first.width, first.height), (64, 48));

    let child = compositor.create_surface(&qh, ());
    let subsurface = subcompositor.get_subsurface(&child, &surface, &qh, ());
    subsurface.set_position(i32::MAX, i32::MAX);
    child.attach(Some(&buffer(&shm, &qh, 8, 8)), 0, 0);
    child.commit();
    let positioner = wm_base.create_positioner(&qh, ());
    positioner.set_size(10, 10);
    positioner.set_anchor_rect(0, 0, 1, 1);
    positioner.set_offset(i32::MAX, i32::MAX);
    let popup_surface = compositor.create_surface(&qh, ());
    let popup_xdg = wm_base.get_xdg_surface(&popup_surface, &qh, ());
    let _popup = popup_xdg.get_popup(Some(&xdg), &positioner, &qh, ());
    popup_surface.commit();
    surface.commit();
    connection.flush().expect("flush");
    nest.presented(VIEW);

    let next = common::next_frame(&nest, VIEW)
        .expect("the nest still composites after the absurd geometry");
    assert_eq!((next.width, next.height), (64, 48));
    queue
        .roundtrip(&mut client)
        .expect("the nest still answers its client");
}

/// An opaque `width` × `height` buffer.
fn buffer(shm: &WlShm, qh: &QueueHandle<Client>, width: i32, height: i32) -> WlBuffer {
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
delegate_noop!(Client: ignore WlSubcompositor);
delegate_noop!(Client: ignore WlSubsurface);
delegate_noop!(Client: ignore WlSurface);
delegate_noop!(Client: ignore WlShm);
delegate_noop!(Client: ignore WlShmPool);
delegate_noop!(Client: ignore WlBuffer);
delegate_noop!(Client: ignore XdgToplevel);
delegate_noop!(Client: ignore XdgPositioner);
delegate_noop!(Client: ignore XdgPopup);
