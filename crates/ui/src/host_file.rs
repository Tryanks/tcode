//! Opening a host's file where the user is.
//!
//! The file is on the host and the user is at the client. Where those are one
//! machine the platform opens the file. Across a link the file reaches the
//! user only through the client's own delivery (a browser's download), so the
//! host's bytes are read over the link and handed to it.

use std::path::Path;

use gpui::{App, Window};
use tcode_protocol::{OriginalTarget, ProtocolError};

use crate::overlay::{Notification, OverlayExt as _};
use crate::remote::ClientAttachment;
use crate::store::{ImageHost, images::HostImages};

/// Open `path`, a file of `host`. A client that is elsewhere and has no
/// delivery has nowhere to open it.
pub(crate) fn open(host: ImageHost, path: &Path, window: &mut Window, cx: &mut App) {
    let Some(link) = HostImages::link(host, cx).filter(|link| link.is_remote()) else {
        cx.open_with_system(path);
        return;
    };
    let client = cx.global::<ClientAttachment>().host();
    if !client.supports_artifact_delivery() {
        return;
    }
    // The path is in the host's rules, which need not be this client's: a
    // Windows machine's path read in a browser.
    let name = path
        .to_string_lossy()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_owned();
    let mut transfer = link.read_original(OriginalTarget::File {
        path: path.to_path_buf(),
    });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let read = async {
            let mime = transfer.metadata().await?.mime.clone();
            let mut bytes = Vec::new();
            while let Some(block) = transfer.read().await? {
                bytes
                    .try_reserve(block.len())
                    .map_err(tcode_client::original::error)?;
                bytes.extend_from_slice(&block);
            }
            Ok::<_, ProtocolError>((bytes, mime))
        }
        .await;
        let _ = handle.update(cx, |_, window, cx| {
            let delivered = read
                .map_err(|error| error.message)
                .and_then(|(bytes, mime)| {
                    let mime = mime.as_deref().unwrap_or("application/octet-stream");
                    client.deliver_artifact(&name, mime, &bytes)
                });
            let notification = match delivered {
                Ok(()) => Notification::success(crate::tr!("export.downloaded", name = name)),
                Err(error) => Notification::error(crate::tr!(
                    "errors.download_file",
                    name = name,
                    error = error
                )),
            };
            window.push_notification(notification, cx);
        });
    })
    .detach();
}
