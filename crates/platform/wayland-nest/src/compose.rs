//! Pixel storage and compositing for the nest's software composition.
//!
//! Every surface's committed buffer is copied out as premultiplied ARGB8888
//! (the client's buffer is released immediately), then a view's surfaces are
//! blended into one frame.

use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::wayland::shm::BufferData;

/// A surface's last committed contents, `width * 4` bytes per row.
pub(crate) struct Image {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

/// Largest buffer edge accepted from the client; a bound on allocations.
const MAX_EDGE: i32 = 16384;

impl Image {
    /// Copies a `wl_shm` buffer. Only the two formats every client must be
    /// able to use are accepted; GTK draws with them.
    pub(crate) fn import(ptr: *const u8, len: usize, data: BufferData) -> Option<Self> {
        let opaque = match data.format {
            wl_shm::Format::Argb8888 => false,
            wl_shm::Format::Xrgb8888 => true,
            _ => return None,
        };
        if data.width <= 0
            || data.height <= 0
            || data.width > MAX_EDGE
            || data.height > MAX_EDGE
            || data.offset < 0
            || data.stride < data.width * 4
        {
            return None;
        }
        let row = data.width as usize * 4;
        let stride = data.stride as usize;
        let offset = data.offset as usize;
        let end = offset + stride * (data.height as usize - 1) + row;
        if end > len {
            return None;
        }
        // SAFETY: the range was checked against the pool mapping's length,
        // and smithay keeps the mapping alive for this call.
        let source = unsafe { std::slice::from_raw_parts(ptr.add(offset), end - offset) };
        let mut pixels = Vec::with_capacity(row * data.height as usize);
        for y in 0..data.height as usize {
            pixels.extend_from_slice(&source[y * stride..y * stride + row]);
        }
        if opaque {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel[alpha_byte()] = 0xff;
            }
        }
        Some(Self {
            width: data.width,
            height: data.height,
            pixels,
        })
    }
}

/// The byte of a native-endian ARGB8888 pixel that holds alpha.
const fn alpha_byte() -> usize {
    if cfg!(target_endian = "little") { 3 } else { 0 }
}

/// A frame being composited.
pub(crate) struct Canvas {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

impl Canvas {
    pub(crate) fn new(width: i32, height: i32) -> Self {
        let width = width.clamp(1, MAX_EDGE);
        let height = height.clamp(1, MAX_EDGE);
        Self {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
        }
    }

    /// Source-over blends `image` with its top-left corner at `(x, y)`.
    pub(crate) fn draw(&mut self, image: &Image, x: i32, y: i32) {
        let left = x.max(0);
        let top = y.max(0);
        let right = (x + image.width).min(self.width);
        let bottom = (y + image.height).min(self.height);
        if left >= right || top >= bottom {
            return;
        }
        let a = alpha_byte();
        let span = (right - left) as usize * 4;
        for row in top..bottom {
            let source_start = ((row - y) * image.width + (left - x)) as usize * 4;
            let target_start = (row * self.width + left) as usize * 4;
            let source = &image.pixels[source_start..source_start + span];
            let target = &mut self.pixels[target_start..target_start + span];
            for (s, t) in source
                .as_chunks::<4>()
                .0
                .iter()
                .zip(target.as_chunks_mut::<4>().0)
            {
                let alpha = s[a] as u32;
                if alpha == 0xff {
                    t.copy_from_slice(s);
                } else if alpha != 0 {
                    let keep = 0xff - alpha;
                    for i in 0..4 {
                        t[i] = (s[i] as u32 + (t[i] as u32 * keep + 0x7f) / 0xff).min(0xff) as u8;
                    }
                }
            }
        }
    }
}
