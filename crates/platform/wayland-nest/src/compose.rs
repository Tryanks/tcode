//! Blends the embedded client's `wl_shm` buffers into a host buffer.
//!
//! Pixels are premultiplied ARGB8888 in native byte order (the `wl_shm`
//! `argb8888` layout) on both sides, so a client buffer is read where the
//! client left it and written once, into the buffer the session shows.

use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::wayland::shm::BufferData;

use crate::Crop;

/// A host buffer being drawn: `crop` of a view, `crop.width * 4` bytes a row.
pub(crate) struct Canvas<'a> {
    pub pixels: &'a mut [u8],
    pub crop: Crop,
}

impl Canvas<'_> {
    /// Source-over blends the client buffer at `ptr` with its top-left
    /// corner at `(x, y)` of the view.
    ///
    /// # Safety
    ///
    /// `ptr` is valid for `len` bytes for the duration of the call.
    pub(crate) unsafe fn draw(
        &mut self,
        ptr: *const u8,
        len: usize,
        data: BufferData,
        x: i32,
        y: i32,
    ) {
        let (Ok(width), Ok(height), Ok(stride), Ok(offset)) = (
            usize::try_from(data.width),
            usize::try_from(data.height),
            usize::try_from(data.stride),
            usize::try_from(data.offset),
        ) else {
            return;
        };
        // The client may have resized the pool since it made the buffer;
        // reading past it would leave the mapping.
        if width == 0
            || height == 0
            || stride < width * 4
            || offset + stride * (height - 1) + width * 4 > len
        {
            return;
        }
        let left = x.max(self.crop.x);
        let top = y.max(self.crop.y);
        let right = x
            .saturating_add(data.width)
            .min(self.crop.x + self.crop.width);
        let bottom = y
            .saturating_add(data.height)
            .min(self.crop.y + self.crop.height);
        if left >= right || top >= bottom {
            return;
        }
        let opaque = data.format == wl_shm::Format::Xrgb8888;
        let span = (right - left) as usize;
        let row = self.crop.width as usize * 4;
        for line in top..bottom {
            let source_start = offset + (line - y) as usize * stride + (left - x) as usize * 4;
            // SAFETY: the row lies inside the range checked above. The
            // client's memory is read through the pointer and never
            // borrowed: the client may write to it at any time.
            let source = unsafe { ptr.add(source_start) };
            let target_start =
                (line - self.crop.y) as usize * row + (left - self.crop.x) as usize * 4;
            let target = &mut self.pixels[target_start..target_start + span * 4];
            for (index, pixel) in target.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                // SAFETY: as above; `index < span`.
                let source = unsafe { source.add(index * 4).cast::<u32>().read_unaligned() };
                let alpha = if opaque { 0xff } else { source >> 24 };
                if alpha == 0xff {
                    *pixel = (source | 0xff00_0000).to_ne_bytes();
                } else if alpha != 0 {
                    *pixel = over(source, u32::from_ne_bytes(*pixel), alpha).to_ne_bytes();
                }
            }
        }
    }
}

/// Premultiplied `source` over `target`.
fn over(source: u32, target: u32, alpha: u32) -> u32 {
    let keep = 0xff - alpha;
    let mut out = 0;
    for shift in [0, 8, 16, 24] {
        let s = (source >> shift) & 0xff;
        let t = (target >> shift) & 0xff;
        out |= (s + (t * keep + 0x7f) / 0xff).min(0xff) << shift;
    }
    out
}
