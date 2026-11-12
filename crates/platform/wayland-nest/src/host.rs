//! The nest as a client of the session's compositor, on the host's own
//! Wayland connection: where views are drawn, when, and where the session's
//! pointer is.
//!
//! The nest thread has an event queue of its own on that connection, so the
//! host's dispatching never sees these objects and the nest never waits for
//! the host's thread.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::os::fd::AsFd as _;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use calloop::{LoopHandle, RegistrationToken};
use calloop_wayland_source::WaylandSource;
use memmap2::MmapMut;
use smithay::backend::input::{Axis, AxisSource};
use smithay::input::pointer::AxisFrame;
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};

use crate::compose::Canvas;
use crate::server::State;
use crate::{Placement, ViewId};

/// A pointer to an object of the host's connection, sent to the nest thread.
pub(crate) struct HostPtr(pub NonNull<c_void>);

// SAFETY: libwayland's display and proxies may be used from any thread; the
// pointer is only handed to libwayland.
unsafe impl Send for HostPtr {}

pub(crate) struct Host {
    connection: Connection,
    qh: QueueHandle<State>,
    source: RegistrationToken,
    shm: WlShm,
    pointer: Option<WlPointer>,
    pub targets: HashMap<ViewId, Target>,
    /// The view the session's pointer is over.
    pointed: Option<ViewId>,
    /// The scrolling of the pointer frame being received.
    scroll: Option<AxisFrame>,
}

impl Host {
    /// # Safety
    ///
    /// `display` is a live `wl_display` until [`Host::leave`] returns.
    pub(crate) unsafe fn join(
        display: HostPtr,
        handle: &LoopHandle<'_, State>,
    ) -> io::Result<Self> {
        // SAFETY: guaranteed by the caller.
        let backend = unsafe {
            Backend::from_foreign_display(
                display.0.as_ptr().cast::<wayland_sys::client::wl_display>(),
            )
        };
        let connection = Connection::from_backend(backend);
        let (globals, queue) =
            registry_queue_init::<State>(&connection).map_err(io::Error::other)?;
        let qh = queue.handle();
        let shm = globals
            .bind::<WlShm, _, _>(&qh, 1..=1, ())
            .map_err(io::Error::other)?;
        // Version 5 groups a pointer's events into frames; 8 counts wheel
        // steps in 120ths.
        if let Err(error) = globals.bind::<WlSeat, _, _>(&qh, 5..=8, ()) {
            log::warn!("wayland nest: the session has no seat for the pointer: {error}");
        }
        let source = WaylandSource::new(connection.clone(), queue)
            .insert(handle.clone())
            .map_err(|error| io::Error::other(error.error))?;
        Ok(Self {
            connection,
            qh,
            source,
            shm,
            pointer: None,
            targets: HashMap::new(),
            pointed: None,
            scroll: None,
        })
    }

    /// Stops using the host's connection.
    pub(crate) fn leave(self, handle: &LoopHandle<'_, State>) {
        handle.remove(self.source);
    }

    /// # Safety
    ///
    /// `surface` is a live `wl_surface` of the host's display until the
    /// target is removed.
    pub(crate) unsafe fn attach(&mut self, view: ViewId, surface: HostPtr) -> io::Result<()> {
        // SAFETY: guaranteed by the caller.
        let id = unsafe {
            ObjectId::from_ptr(
                WlSurface::interface(),
                surface.0.as_ptr().cast::<wayland_sys::client::wl_proxy>(),
            )
        }
        .map_err(io::Error::other)?;
        let surface = WlSurface::from_id(&self.connection, id).map_err(io::Error::other)?;
        self.targets.insert(
            view,
            Target {
                surface,
                buffers: Vec::new(),
                placement: None,
                frame: None,
                stale: false,
            },
        );
        Ok(())
    }

    pub(crate) fn flush(&self) {
        if let Err(error) = self.connection.flush() {
            log::error!("wayland nest: the session's connection failed: {error}");
        }
    }

    fn view_of(&self, surface: &WlSurface) -> Option<ViewId> {
        self.targets
            .iter()
            .find_map(|(view, target)| (target.surface == *surface).then_some(*view))
    }

    /// A position on the pointed view's host surface, in the view.
    fn view_position(&self, x: f64, y: f64) -> Option<(ViewId, f64, f64)> {
        let view = self.pointed?;
        let placement = self.targets.get(&view)?.placement?;
        let scale = f64::from(placement.scale);
        Some((
            view,
            f64::from(placement.crop.x) + x * scale,
            f64::from(placement.crop.y) + y * scale,
        ))
    }
}

/// The host surface a view is drawn on.
pub(crate) struct Target {
    surface: WlSurface,
    buffers: Vec<ShmBuffer>,
    /// Where the view is shown; `None` while it is hidden.
    pub placement: Option<Placement>,
    /// The session's callback for the last frame committed with one.
    frame: Option<WlCallback>,
    /// Whether the placement changed since the last commit.
    pub stale: bool,
}

impl Target {
    /// Whether the session has yet to ask for the frame after the last one.
    pub(crate) fn awaits_session(&self) -> bool {
        self.frame.is_some()
    }

    pub(crate) fn place(&mut self, placement: Option<Placement>) {
        if self.placement == placement {
            return;
        }
        self.stale = placement.is_some();
        if placement.is_none() {
            // A hidden surface is unmapped, and the session answers no
            // callback of a surface it does not show.
            self.frame = None;
        }
        self.placement = placement;
    }

    /// Draws the placement's crop with `draw` and commits it. Returns whether
    /// the commit asked the session for a frame callback.
    pub(crate) fn present(
        &mut self,
        host: &Host,
        view: ViewId,
        placement: Placement,
        draw: impl FnOnce(&mut Canvas<'_>),
    ) -> io::Result<()> {
        let crop = placement.crop;
        let index = self.free_buffer(host, crop.width, crop.height)?;
        let buffer = &mut self.buffers[index];
        buffer.map.fill(0);
        draw(&mut Canvas {
            pixels: &mut buffer.map,
            crop,
        });
        self.surface.attach(Some(&buffer.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, crop.width, crop.height);
        if self.frame.is_none() {
            self.frame = Some(self.surface.frame(&host.qh, view));
        }
        self.surface.commit();
        buffer.busy.store(true, Ordering::Relaxed);
        self.stale = false;
        Ok(())
    }

    fn free_buffer(&mut self, host: &Host, width: i32, height: i32) -> io::Result<usize> {
        self.buffers
            .retain(|b| b.busy.load(Ordering::Relaxed) || (b.width == width && b.height == height));
        if let Some(index) = self
            .buffers
            .iter()
            .position(|b| !b.busy.load(Ordering::Relaxed))
        {
            return Ok(index);
        }
        self.buffers.push(ShmBuffer::new(host, width, height)?);
        Ok(self.buffers.len() - 1)
    }
}

struct ShmBuffer {
    buffer: WlBuffer,
    map: MmapMut,
    width: i32,
    height: i32,
    /// Whether the session may still read the buffer.
    busy: Arc<AtomicBool>,
}

impl ShmBuffer {
    fn new(host: &Host, width: i32, height: i32) -> io::Result<Self> {
        let stride = width * 4;
        let len = stride as usize * height as usize;
        let fd = rustix::fs::memfd_create("tcode-nest", rustix::fs::MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, len as u64)?;
        let file = std::fs::File::from(fd);
        // SAFETY: the memfd is private to this buffer and the session's
        // compositor, which only reads it.
        let map = unsafe { MmapMut::map_mut(&file) }?;
        let pool = host.shm.create_pool(file.as_fd(), len as i32, &host.qh, ());
        let busy = Arc::new(AtomicBool::new(false));
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            stride,
            wl_shm::Format::Argb8888,
            &host.qh,
            busy.clone(),
        );
        pool.destroy();
        Ok(Self {
            buffer,
            map,
            width,
            height,
            busy,
        })
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, Arc<AtomicBool>> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        busy: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            busy.store(false, Ordering::Relaxed);
        }
    }
}

impl Dispatch<WlCallback, ViewId> for State {
    fn event(
        state: &mut Self,
        callback: &WlCallback,
        event: wl_callback::Event,
        view: &ViewId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wl_callback::Event::Done { .. } = event else {
            return;
        };
        let Some(target) = state
            .host
            .as_mut()
            .and_then(|host| host.targets.get_mut(view))
        else {
            return;
        };
        if target.frame.as_ref() == Some(callback) {
            target.frame = None;
            state.session_wants_frame(*view);
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        else {
            return;
        };
        let Some(host) = state.host.as_mut() else {
            return;
        };
        let has_pointer = capabilities.contains(wl_seat::Capability::Pointer);
        if has_pointer && host.pointer.is_none() {
            host.pointer = Some(seat.get_pointer(qh, ()));
        } else if !has_pointer && let Some(pointer) = host.pointer.take() {
            pointer.release();
        }
    }
}

/// The session's pointer, passed on as the session reports it: every
/// `wl_pointer` of a client hears the pointer on that client's surfaces, the
/// host's own among them, and the views' surfaces are the ones followed here.
impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(host) = state.host.as_mut() else {
            return;
        };
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                host.pointed = host.view_of(&surface);
                if let Some((view, x, y)) = host.view_position(surface_x, surface_y) {
                    state.pointer_motion(view, x, y);
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if host.pointed.take().is_some() {
                    state.pointer_leave();
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some((view, x, y)) = host.view_position(surface_x, surface_y) {
                    state.pointer_motion(view, x, y);
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(pressed),
                ..
            } => {
                if host.pointed.is_some() {
                    state.pointer_button(button, pressed == wl_pointer::ButtonState::Pressed);
                }
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } => state.scroll(|frame| frame.value(smithay_axis(axis), value)),
            wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(source),
            } => {
                let source = match source {
                    wl_pointer::AxisSource::Wheel => AxisSource::Wheel,
                    wl_pointer::AxisSource::Finger => AxisSource::Finger,
                    wl_pointer::AxisSource::Continuous => AxisSource::Continuous,
                    wl_pointer::AxisSource::WheelTilt => AxisSource::WheelTilt,
                    _ => return,
                };
                state.scroll(|frame| frame.source(source));
            }
            wl_pointer::Event::AxisStop {
                axis: WEnum::Value(axis),
                ..
            } => state.scroll(|frame| frame.stop(smithay_axis(axis))),
            // What a seat older than version 8 sends for a wheel step.
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } => state.scroll(|frame| frame.v120(smithay_axis(axis), discrete * 120)),
            wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(axis),
                value120,
            } => state.scroll(|frame| frame.v120(smithay_axis(axis), value120)),
            wl_pointer::Event::Frame => {
                let scroll = host.scroll.take();
                if host.pointed.is_some() {
                    state.pointer_frame(scroll);
                }
            }
            _ => {}
        }
    }
}

impl State {
    /// Adds to the scrolling of the pointer frame being received.
    fn scroll(&mut self, add: impl FnOnce(AxisFrame) -> AxisFrame) {
        let time = self.time();
        if let Some(host) = self.host.as_mut() {
            let frame = host.scroll.take().unwrap_or_else(|| AxisFrame::new(time));
            host.scroll = Some(add(frame));
        }
    }
}

fn smithay_axis(axis: wl_pointer::Axis) -> Axis {
    match axis {
        wl_pointer::Axis::HorizontalScroll => Axis::Horizontal,
        _ => Axis::Vertical,
    }
}

delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlSurface);
