//! Shows nest frames on surfaces of the host's own Wayland connection.

use std::cell::RefCell;
use std::ffi::c_void;
use std::os::fd::AsFd as _;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use memmap2::MmapMut;
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};

use crate::Frame;

/// Buffers kept per target: one on screen, one the compositor may still be
/// reading, one being filled.
const MAX_BUFFERS: usize = 3;

/// The host connection's `wl_shm`, on an event queue of its own so the host's
/// dispatching never sees these objects.
pub struct Presenter {
    connection: Connection,
    queue: RefCell<EventQueue<Releases>>,
    handle: QueueHandle<Releases>,
    shm: WlShm,
}

pub struct Releases;

impl Presenter {
    /// # Safety
    ///
    /// `display` is a live `wl_display` that outlives the presenter and every
    /// target made from it.
    pub unsafe fn new(display: NonNull<c_void>) -> Result<Self, String> {
        // SAFETY: guaranteed by the caller.
        let backend = unsafe {
            Backend::from_foreign_display(
                display.as_ptr().cast::<wayland_sys::client::wl_display>(),
            )
        };
        let connection = Connection::from_backend(backend);
        let (globals, queue) =
            registry_queue_init::<Releases>(&connection).map_err(|error| error.to_string())?;
        let handle = queue.handle();
        let shm = globals
            .bind::<WlShm, _, _>(&handle, 1..=1, ())
            .map_err(|error| error.to_string())?;
        Ok(Self {
            connection,
            queue: RefCell::new(queue),
            handle,
            shm,
        })
    }

    /// # Safety
    ///
    /// `surface` is a live `wl_surface` of the presenter's display that
    /// outlives the target.
    pub unsafe fn target(&self, surface: NonNull<c_void>) -> Result<Target, String> {
        // SAFETY: guaranteed by the caller.
        let id = unsafe {
            ObjectId::from_ptr(
                WlSurface::interface(),
                surface.as_ptr().cast::<wayland_sys::client::wl_proxy>(),
            )
        }
        .map_err(|error| error.to_string())?;
        let surface =
            WlSurface::from_id(&self.connection, id).map_err(|error| error.to_string())?;
        Ok(Target {
            surface,
            buffers: Vec::new(),
        })
    }

    fn dispatch_releases(&self) {
        if let Err(error) = self.queue.borrow_mut().dispatch_pending(&mut Releases) {
            log::debug!("wayland nest presenter: dispatch failed: {error}");
        }
    }
}

/// The part of a frame a target shows, in frame pixels. Parts outside the
/// frame are transparent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// A host surface that shows frames.
pub struct Target {
    surface: WlSurface,
    buffers: Vec<ShmBuffer>,
}

impl Target {
    /// Attaches `crop` of `frame` and commits. The buffer is the crop's size,
    /// in the host's device pixels.
    pub fn present(
        &mut self,
        presenter: &Presenter,
        frame: &Frame,
        crop: Crop,
    ) -> Result<(), String> {
        presenter.dispatch_releases();
        let width = crop.width.max(1);
        let height = crop.height.max(1);
        let index = self.free_buffer(presenter, width, height)?;
        let buffer = &mut self.buffers[index];
        buffer.fill(frame, crop);
        self.surface.attach(Some(&buffer.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, width, height);
        self.surface.commit();
        buffer.busy.store(true, Ordering::Relaxed);
        presenter
            .connection
            .flush()
            .map_err(|error| error.to_string())
    }

    fn free_buffer(
        &mut self,
        presenter: &Presenter,
        width: i32,
        height: i32,
    ) -> Result<usize, String> {
        self.buffers
            .retain(|b| b.busy.load(Ordering::Relaxed) || (b.width == width && b.height == height));
        if let Some(index) = self
            .buffers
            .iter()
            .position(|b| !b.busy.load(Ordering::Relaxed))
        {
            return Ok(index);
        }
        if self.buffers.len() >= MAX_BUFFERS {
            // The compositor holds every buffer; reuse the oldest rather than
            // growing without bound.
            let oldest = self.buffers.remove(0);
            drop(oldest);
        }
        self.buffers.push(ShmBuffer::new(presenter, width, height)?);
        Ok(self.buffers.len() - 1)
    }
}

struct ShmBuffer {
    buffer: WlBuffer,
    map: MmapMut,
    width: i32,
    height: i32,
    busy: Arc<AtomicBool>,
}

impl ShmBuffer {
    fn new(presenter: &Presenter, width: i32, height: i32) -> Result<Self, String> {
        let stride = width * 4;
        let len = stride as usize * height as usize;
        let fd = rustix::fs::memfd_create("tcode-nest", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|error| error.to_string())?;
        rustix::fs::ftruncate(&fd, len as u64).map_err(|error| error.to_string())?;
        let file = std::fs::File::from(fd);
        // SAFETY: the memfd is private to this buffer and the compositor,
        // which only reads it.
        let map = unsafe { MmapMut::map_mut(&file) }.map_err(|error| error.to_string())?;
        let pool = presenter
            .shm
            .create_pool(file.as_fd(), len as i32, &presenter.handle, ());
        let busy = Arc::new(AtomicBool::new(false));
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            stride,
            wl_shm::Format::Argb8888,
            &presenter.handle,
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

    fn fill(&mut self, frame: &Frame, crop: Crop) {
        let row = self.width as usize * 4;
        let frame_width = frame.width as i32;
        let frame_height = frame.height as i32;
        let left = crop.x.clamp(0, frame_width);
        let right = (crop.x + self.width).clamp(0, frame_width);
        for y in 0..self.height {
            let target = &mut self.map[y as usize * row..(y as usize + 1) * row];
            let source_y = crop.y + y;
            if source_y < 0 || source_y >= frame_height || left >= right {
                target.fill(0);
                continue;
            }
            let lead = ((left - crop.x) * 4) as usize;
            let span = ((right - left) * 4) as usize;
            let start = (source_y as usize * frame.width as usize + left as usize) * 4;
            target[..lead].fill(0);
            target[lead..lead + span].copy_from_slice(&frame.pixels[start..start + span]);
            target[lead + span..].fill(0);
        }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Releases {
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

impl Dispatch<WlBuffer, Arc<AtomicBool>> for Releases {
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

delegate_noop!(Releases: ignore WlShm);
delegate_noop!(Releases: ignore WlShmPool);
