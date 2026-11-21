//! Pictures Tcode rasterizes itself from SVG: how large one is drawn and how
//! many drawn ones are kept, for Mermaid diagrams and for the SVG of an HTML
//! visual alike. `markdown::mermaid` says why they are rasterized here and
//! not by GPUI's `img`.

use std::{collections::VecDeque, rc::Rc, sync::Arc};

use gpui::{App, ImageSource, Pixels, RenderImage, Size, SvgRenderer, px, size};

/// The most pixels a picture is rasterized with, so a huge one does not
/// take hundreds of megabytes; it is shown far smaller than that anyway.
pub(crate) const MAX_PIXELS: f32 = 16_000_000.;

/// The scale a picture of `natural` size is rasterized at when it is shown
/// at up to `shown` times that size: twice what it is shown at, as GPUI
/// rasterizes SVG, and within [`MAX_PIXELS`].
pub(crate) fn scale(natural: Size<f32>, shown: f32) -> f32 {
    if natural.width <= 0. || natural.height <= 0. {
        return gpui::SMOOTH_SVG_SCALE_FACTOR;
    }
    let mut scale = gpui::SMOOTH_SVG_SCALE_FACTOR * shown;
    let pixels = natural.width * natural.height * scale * scale;
    if pixels > MAX_PIXELS {
        scale *= (MAX_PIXELS / pixels).sqrt();
    }
    scale
}

/// `svg` drawn at `scale`, and the size it is shown at.
pub(crate) fn rasterize(
    svg: &[u8],
    scale: f32,
    renderer: &SvgRenderer,
) -> Result<(Arc<RenderImage>, Size<Pixels>), String> {
    let image = renderer
        .render_single_frame(svg, scale / gpui::SMOOTH_SVG_SCALE_FACTOR)
        .map_err(|error| error.to_string())?;
    let device = image.size(0);
    let shown = size(
        px(device.width.0 as f32 / scale),
        px(device.height.0 as f32 / scale),
    );
    Ok((image, shown))
}

/// A rasterized picture.
#[derive(Clone)]
pub(crate) struct Picture(Rc<PictureData>);

struct PictureData {
    image: crate::store::images::Owned,
    size: Size<Pixels>,
}

impl Picture {
    pub(crate) fn new(image: Arc<RenderImage>, size: Size<Pixels>, cx: &mut App) -> Self {
        Self(Rc::new(PictureData {
            image: crate::store::images::Owned::new(image, cx),
            size,
        }))
    }

    pub(crate) fn size(&self) -> Size<Pixels> {
        self.0.size
    }

    pub(crate) fn source(&self) -> ImageSource {
        self.0.image.source()
    }

    pub(crate) fn bytes(&self) -> usize {
        let device = self.0.image.image().size(0);
        device.width.0.max(0) as usize * device.height.0.max(0) as usize * 4
    }
}

const RECENT_CAPACITY: usize = 32;
const RECENT_BYTES: usize = 64 * 1024 * 1024;

/// The most recent results of drawing pictures, for a document rebuilt or
/// opened again and for a theme switched back: the last
/// [`RECENT_CAPACITY`], within [`RECENT_BYTES`] of pixels.
pub(crate) struct Recent<K, V> {
    entries: VecDeque<(K, V)>,
    bytes: usize,
    weigh: fn(&V) -> usize,
}

impl<K: PartialEq, V: Clone> Recent<K, V> {
    /// `weigh` is the bytes of pixels a result holds.
    pub(crate) fn new(weigh: fn(&V) -> usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            weigh,
        }
    }

    pub(crate) fn get(&mut self, key: &K) -> Option<V> {
        let ix = self.entries.iter().position(|(entry, _)| entry == key)?;
        let entry = self.entries.remove(ix)?;
        let value = entry.1.clone();
        self.entries.push_back(entry);
        Some(value)
    }

    pub(crate) fn insert(&mut self, key: K, value: V) {
        if let Some(ix) = self.entries.iter().position(|(entry, _)| *entry == key)
            && let Some((_, old)) = self.entries.remove(ix)
        {
            self.bytes -= (self.weigh)(&old);
        }
        self.bytes += (self.weigh)(&value);
        self.entries.push_back((key, value));
        while self.entries.len() > 1
            && (self.entries.len() > RECENT_CAPACITY || self.bytes > RECENT_BYTES)
        {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= (self.weigh)(&old);
            }
        }
    }
}
