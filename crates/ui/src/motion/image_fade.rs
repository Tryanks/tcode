//! Pictures that fade in over what stands in for them once they decode.
//!
//! GPUI's `img` decodes asynchronously and draws nothing (or its loading
//! element) until the decode lands, then the picture at once; it tells its
//! caller nothing about where it stands. Every source Tcode draws is a
//! `store::images` reader, so [`image_load`] asks the same reader `img` will
//! ask, and [`image_arrival`] turns the answer into an opacity that rises
//! once it lands. Asking first starts the read `img` would have started; it
//! is the same read, decoded once.

use std::sync::Arc;

use gpui::{App, ElementId, ImageCacheError, ImageSource, Pixels, RenderImage, Size, Window, px};

/// Where a source's decode stands this frame.
#[derive(Clone)]
pub(crate) enum ImageLoad {
    /// Still decoding, or still being read.
    Loading,
    /// Decoded: what `img` draws.
    Loaded(Arc<RenderImage>),
    /// It will not decode: `img` draws its fallback.
    Failed,
}

impl ImageLoad {
    /// Whether `img` draws something of its own: the picture, or its
    /// fallback.
    pub(crate) fn settled(&self) -> bool {
        !matches!(self, Self::Loading)
    }

    /// The picture's own size, once decoded.
    pub(crate) fn size(&self) -> Option<Size<Pixels>> {
        match self {
            Self::Loaded(image) => {
                let size = image.size(0);
                (size.width.0 > 0 && size.height.0 > 0)
                    .then(|| gpui::size(px(size.width.0 as f32), px(size.height.0 as f32)))
            }
            Self::Loading | Self::Failed => None,
        }
    }
}

/// Where `source`'s decode stands, as `img` will find it this frame.
pub(crate) fn image_load(source: &ImageSource, window: &mut Window, cx: &mut App) -> ImageLoad {
    let decoded: Option<Result<Arc<RenderImage>, ImageCacheError>> = match source {
        ImageSource::Custom(load) => load(window, cx),
        ImageSource::Render(image) => Some(Ok(image.clone())),
        ImageSource::Resource(_) | ImageSource::Image(_) => {
            // GPUI's memo would keep this picture for the session.
            if cfg!(debug_assertions) {
                panic!("draw pictures through `store::images`, which holds them within a budget");
            }
            // `img` draws it all the same, so it is not left invisible.
            Some(Err(ImageCacheError::Asset(
                "not read by store::images".into(),
            )))
        }
    };
    match decoded {
        None => ImageLoad::Loading,
        Some(Ok(image)) => ImageLoad::Loaded(image),
        Some(Err(_)) => ImageLoad::Failed,
    }
}

/// The opacity to draw `source`'s `img` (and so its fallback) at, over what
/// stands in for it: 0 while it decodes, rising to 1 along `curve` once it
/// has. A picture already decoded when it is first drawn — a page of
/// history, a thread opened again — is whole at once; reduced motion shows
/// it whole the frame it lands. `id` names the picture's place; `clock`
/// keeps its fade.
pub(crate) fn image_arrival(
    clock: super::Clock<'_>,
    id: impl Into<ElementId>,
    source: &ImageSource,
    curve: impl Into<super::Curve>,
    window: &mut Window,
    cx: &mut App,
) -> f32 {
    let settled = image_load(source, window, cx).settled();
    clock.transition(
        super::child_id(&id.into(), "image-arrival"),
        if settled { 1f32 } else { 0. },
        &curve.into(),
        window,
        cx,
    )
}
