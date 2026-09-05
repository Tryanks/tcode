//! Host-owned image paths loaded through the query plane and GPUI's asset cache.
use gpui::{App, Asset, Global, Image, ImageCacheError, ImageSource};
use std::{path::PathBuf, sync::Arc};
use tcode_client::HostLink;
use tcode_protocol::{Query, QueryResponse};

pub(super) struct HostImages {
    pub link: HostLink,
    pub namespace: u64,
}
impl Global for HostImages {}

struct HostImage;
impl Asset for HostImage {
    type Source = (u64, PathBuf);
    type Output = Result<Arc<Image>, ImageCacheError>;
    fn load(
        (_, path): Self::Source,
        cx: &mut App,
    ) -> impl std::future::Future<Output = Self::Output> + Send + 'static {
        let host = cx.global::<HostImages>().link.clone();
        async move {
            let bytes = match host.query(Query::ReadFileBytes { path }).await {
                Ok(QueryResponse::FileBytes(bytes)) => bytes,
                result => {
                    return Err(std::io::Error::other(format!(
                        "host image read failed: {result:?}"
                    ))
                    .into());
                }
            };
            let format = image::guess_format(&bytes)?;
            let format = gpui::ImageFormat::from_mime_type(format.to_mime_type())
                .ok_or_else(|| std::io::Error::other("unsupported host image format"))?;
            Ok(Arc::new(Image::from_bytes(format, bytes)))
        }
    }
}

pub(crate) fn host_image(path: PathBuf) -> ImageSource {
    ImageSource::from(move |window: &mut gpui::Window, cx: &mut App| {
        let namespace = cx.try_global::<HostImages>()?.namespace;
        match window.use_asset::<HostImage>(&(namespace, path.clone()), cx)? {
            Ok(image) => image.use_render_image(window, cx).map(Ok),
            Err(error) => Some(Err(error)),
        }
    })
}
