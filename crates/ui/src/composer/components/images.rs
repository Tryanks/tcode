use super::super::*;
use crate::attachments::{TransferVerdict, transfer_verdict};
use crate::thread_export::format_size;
use crate::widgets::menu::ContextMenuExt as _;
use gpui::StyledImage as _;
use image::GenericImageView as _;
use std::cell::RefCell;
use tcode_core::attachments::{AttachError, MAX_EDGE_PX, MAX_SOURCE_BYTES};

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a picture drawn here takes to fade in once it has decoded.
const ARRIVE: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Clone)]
/// A pending image attachment: validated, persisted to the session attachments
/// dir, and shown in the composer thumbnail strip. Kept per active session.
pub(in super::super) struct PendingImage {
    /// On-disk path of the persisted copy (also the thumbnail image source).
    pub(in super::super) path: PathBuf,
    pub(in super::super) name: String,
}

/// One slot of the strip under the field.
#[derive(Clone)]
enum StripItem {
    Image(usize, PendingImage),
    File(usize),
    Loading,
}

pub(in super::super) enum PendingImageSource {
    Bytes(Vec<u8>),
    Path(PathBuf),
}

pub(in super::super) enum AddImageError {
    Attachment(AttachError),
    Persist(std::io::Error),
}

/// Fitted bytes waiting for the host to store them.
struct PreparedImage {
    dir: PathBuf,
    bytes: Vec<u8>,
    ext: String,
}

/// Guess a file extension for a persisted attachment from its MIME type,
/// falling back to the source name's extension, then `png`.
fn image_extension(mime: &str, name: &str) -> String {
    let from_mime = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "",
    };
    if !from_mime.is_empty() {
        return from_mime.to_string();
    }
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "png".to_string())
}

fn is_wire_ready_image(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// Bring an image to what the wire and the model take: a format every
/// provider accepts, no longer than [`MAX_EDGE_PX`] on either side. Bytes
/// that already qualify pass through untouched so nothing is recompressed;
/// GIFs always do, since re-encoding keeps only their first frame.
pub(crate) fn fit_for_upload(mime: &str, bytes: Vec<u8>) -> image::ImageResult<(String, Vec<u8>)> {
    if mime == "image/gif" {
        return Ok((mime.to_string(), bytes));
    }
    let decoded = image::load_from_memory(&bytes)?;
    let (width, height) = decoded.dimensions();
    let oversized = width.max(height) > MAX_EDGE_PX;
    if is_wire_ready_image(mime) && !oversized {
        return Ok((mime.to_string(), bytes));
    }
    let image = if oversized {
        decoded.resize(
            MAX_EDGE_PX,
            MAX_EDGE_PX,
            image::imageops::FilterType::Triangle,
        )
    } else {
        decoded
    };
    let mut out = std::io::Cursor::new(Vec::new());
    // Photos stay JPEG; anything else becomes PNG, which keeps transparency.
    if mime == "image/jpeg" {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
        image.to_rgb8().write_with_encoder(encoder)?;
        Ok(("image/jpeg".to_string(), out.into_inner()))
    } else {
        image.write_to(&mut out, image::ImageFormat::Png)?;
        Ok(("image/png".to_string(), out.into_inner()))
    }
}

/// Ask before sending `size` bytes across the internet. `proceed` runs on
/// confirmation; `release` on any other way out, Escape and the backdrop
/// included.
pub(super) fn confirm_large_transfer(
    composer: Entity<Composer>,
    name: String,
    size: u64,
    window: &mut Window,
    cx: &mut Context<Composer>,
    proceed: impl FnOnce(&mut Composer, &mut Window, &mut Context<Composer>) + 'static,
    release: impl FnOnce(&mut Composer, &mut Context<Composer>) + 'static,
) {
    // Taken by whichever button fires.
    let outcome = Rc::new(RefCell::new(Some((proceed, release))));
    window.open_alert_dialog(cx, move |alert, _, _| {
        let confirmed = composer.clone();
        let outcome_ok = outcome.clone();
        let release = {
            let composer = composer.clone();
            let outcome = outcome.clone();
            move |cx: &mut App| {
                if let Some((_, release)) = outcome.borrow_mut().take() {
                    composer.update(cx, |composer, cx| release(composer, cx));
                }
            }
        };
        let on_cancel = release.clone();
        alert
            .title(crate::tr!("attach.tunnel_title"))
            .description(crate::tr!(
                "attach.tunnel_body",
                name = name,
                size = format_size(size)
            ))
            .button_props(
                DialogButtons::default()
                    .ok_text(crate::tr!("attach.tunnel_confirm"))
                    .cancel_text(crate::tr!("mobile.cancel"))
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                if let Some((proceed, _)) = outcome_ok.borrow_mut().take() {
                    confirmed.update(cx, |composer, cx| proceed(composer, window, cx));
                }
                true
            })
            .on_cancel(move |_, _, cx| {
                on_cancel(cx);
                true
            })
            .on_close(move |_, _, cx| release(cx))
    });
}

impl Composer {
    /// Reset the thumbnail strip when the active session changes (its pending
    /// images belong to a specific session).
    pub(in super::super) fn sync_images_session(&mut self, cx: &mut App) {
        if self.session_id != self.images_session {
            let previous = std::mem::replace(&mut self.images_session, self.session_id.clone());
            self.pending_images.clear();
            self.discard_files_of(previous, cx);
            self.image_load_generation = self.image_load_generation.wrapping_add(1);
            self.pending_image_loads = 0;
        }
    }

    /// Validate, fit, and persist an image off the main thread. Completion
    /// appends only while the same session still owns the strip.
    pub(in super::super) fn add_image(
        &mut self,
        name: String,
        mime: String,
        source: PendingImageSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.sync_images_session(cx);
        // The type is known now; the size is checked once the image is
        // fitted, since a phone photo starts far larger than it is sent.
        if let Err(err) = validate_attachment(&name, &mime, 0) {
            window.push_notification(Notification::error(attach_error_message(&err)), cx);
            return false;
        }
        let store = self.workspace_store.read(cx);
        let session_id = self.session_id.clone();
        let attachments_dir = self.composer_state(cx).attachments_dir.clone();
        let link = store.attachment_link();
        let remote_limit = crate::device::DevicePreferences::remote_attachment_limit_bytes(cx);
        self.pending_image_loads += 1;
        cx.notify();
        let generation = self.image_load_generation;
        let result_name = name.clone();
        cx.spawn_in(window, async move |this, cx| {
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    let too_large =
                        || AddImageError::Attachment(AttachError::TooLarge { name: name.clone() });
                    let bytes = match source {
                        PendingImageSource::Bytes(bytes) => bytes,
                        PendingImageSource::Path(path) => {
                            let size = std::fs::metadata(&path)
                                .map_err(AddImageError::Persist)?
                                .len();
                            if size > MAX_SOURCE_BYTES {
                                return Err(too_large());
                            }
                            std::fs::read(path).map_err(AddImageError::Persist)?
                        }
                    };
                    if bytes.len() as u64 > MAX_SOURCE_BYTES {
                        return Err(too_large());
                    }
                    let (mime, bytes) = fit_for_upload(&mime, bytes).map_err(|_| {
                        AddImageError::Attachment(AttachError::UnsupportedType {
                            name: name.clone(),
                        })
                    })?;
                    validate_attachment(&name, &mime, bytes.len() as u64)
                        .map_err(AddImageError::Attachment)?;
                    let dir = attachments_dir.ok_or_else(|| {
                        AddImageError::Persist(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "no active session",
                        ))
                    })?;
                    let ext = image_extension(&mime, &name);
                    Ok::<_, AddImageError>(PreparedImage { dir, bytes, ext })
                })
                .await;
            let _ = this.update_in(cx, |composer, window, cx| {
                if !composer.owns_image_load(generation, &session_id) {
                    return;
                }
                let prepared = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        composer.pending_image_loads =
                            composer.pending_image_loads.saturating_sub(1);
                        cx.notify();
                        composer.notify_add_image_error(error, window, cx);
                        return;
                    }
                };
                let size = prepared.bytes.len() as u64;
                match transfer_verdict(link, size, remote_limit) {
                    TransferVerdict::Send => composer.persist_image(
                        session_id,
                        generation,
                        result_name,
                        prepared,
                        window,
                        cx,
                    ),
                    TransferVerdict::Reject => {
                        composer.pending_image_loads =
                            composer.pending_image_loads.saturating_sub(1);
                        cx.notify();
                        window.push_notification(
                            Notification::error(
                                crate::tr!(
                                    "attach.relay_rejected",
                                    name = result_name,
                                    size = format_size(size as u64),
                                    limit = format_size(remote_limit)
                                )
                                .into_owned(),
                            ),
                            cx,
                        );
                    }
                    TransferVerdict::Confirm => {
                        confirm_large_transfer(
                            cx.entity(),
                            result_name.clone(),
                            size,
                            window,
                            cx,
                            move |composer, window, cx| {
                                composer.persist_image(
                                    session_id,
                                    generation,
                                    result_name,
                                    prepared,
                                    window,
                                    cx,
                                )
                            },
                            |composer, cx| {
                                composer.pending_image_loads =
                                    composer.pending_image_loads.saturating_sub(1);
                                cx.notify();
                            },
                        );
                    }
                }
            });
        })
        .detach();
        true
    }

    /// Whether a load started under `generation` for `session_id` still
    /// belongs to the strip on screen.
    pub(super) fn owns_image_load(&self, generation: u64, session_id: &Option<String>) -> bool {
        self.image_load_generation == generation
            && &self.images_session == session_id
            && &self.session_id == session_id
    }

    fn notify_add_image_error(
        &self,
        error: AddImageError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = match error {
            AddImageError::Attachment(err) => attach_error_message(&err),
            AddImageError::Persist(err) => {
                crate::tr!("errors.persist_event", error = err).into_owned()
            }
        };
        window.push_notification(Notification::error(message), cx);
    }

    /// Send fitted bytes to the host and, once saved, show the thumbnail.
    fn persist_image(
        &mut self,
        session_id: Option<String>,
        generation: u64,
        name: String,
        prepared: PreparedImage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PreparedImage { dir, bytes, ext } = prepared;
        let task = self.workspace_store.update(cx, |store, cx| {
            store.save_attachment_to_dir(dir, bytes, ext, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |composer, window, cx| {
                if !composer.owns_image_load(generation, &session_id) {
                    return;
                }
                composer.pending_image_loads = composer.pending_image_loads.saturating_sub(1);
                cx.notify();
                match result {
                    Ok(path) => composer.pending_images.push(PendingImage { path, name }),
                    Err(err) => {
                        composer.notify_add_image_error(AddImageError::Persist(err), window, cx)
                    }
                }
            });
        })
        .detach();
    }

    /// Open the platform's media picker (phones) and add what it returns.
    pub(in super::super) fn pick_images_from_library(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = cx.global::<crate::remote::ClientAttachment>().host();
        self.sync_images_session(cx);
        cx.spawn_in(window, async move |this, cx| {
            let picked = host.pick_images().await;
            let _ = this.update_in(cx, |composer, window, cx| match picked {
                Ok(images) => {
                    for image in images {
                        composer.add_image_bytes(image.name, image.mime, image.bytes, window, cx);
                    }
                }
                Err(error) => window.push_notification(
                    Notification::error(
                        crate::tr!(
                            "attach.pick_failed",
                            error = crate::i18n::device_failure(&error)
                        )
                        .into_owned(),
                    ),
                    cx,
                ),
            });
        })
        .detach();
    }

    pub(in super::super) fn add_image_bytes(
        &mut self,
        name: String,
        mime: String,
        bytes: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.add_image(name, mime, PendingImageSource::Bytes(bytes), window, cx)
    }

    /// Attach dropped or pasted paths, in their order: each as an image when
    /// it is one the model can see, otherwise as a file. Returns whether any
    /// was accepted.
    pub(in super::super) fn add_paths(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut accepted = false;
        let mut files = Vec::new();
        for path in paths {
            if tcode_core::attachments::is_image_attachment(&mime_from_path(&path)) {
                accepted |= self.add_image_path(path, window, cx);
            } else {
                files.push(path);
            }
        }
        if !files.is_empty() {
            self.add_file_paths(files, window, cx);
            accepted = true;
        }
        accepted
    }

    /// Add an image from a dropped file path.
    pub(in super::super) fn add_image_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".to_string());
        let mime = mime_from_path(&path);
        self.add_image(name, mime, PendingImageSource::Path(path), window, cx)
    }

    /// Pull an image off the clipboard (⌘V with image content), if present.
    /// Returns whether an image was accepted.
    pub(in super::super) fn paste_clipboard_image(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut accepted_image = cx
            .read_from_clipboard()
            .is_some_and(|item| self.paste_images(&item, window, cx));
        if !accepted_image && let Some((mime, bytes)) = crate::pasteboard::read_pasteboard_image() {
            accepted_image =
                self.add_image_bytes("pasted-image".to_string(), mime, bytes, window, cx);
        }
        accepted_image
    }

    /// Attach the images of a pasted clipboard item. Returns whether an image
    /// was accepted.
    pub(in super::super) fn paste_images(
        &mut self,
        item: &gpui::ClipboardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut accepted_image = false;
        for entry in &item.entries {
            match entry {
                ClipboardEntry::Image(image) => {
                    let mime = image.format().mime_type().to_string();
                    let bytes = image.bytes().to_vec();
                    accepted_image |=
                        self.add_image_bytes("pasted-image".to_string(), mime, bytes, window, cx);
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    accepted_image |= self.add_paths(paths.paths().to_vec(), window, cx);
                }
                ClipboardEntry::String(_) => {}
            }
        }
        accepted_image
    }

    pub(in super::super) fn remove_image(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.pending_images.len() {
            let removed = self.pending_images.remove(index);
            self.workspace_store
                .update(cx, |store, cx| store.remove_user_file(removed.path, cx))
                .detach();
            cx.notify();
        }
    }

    /// The pending images under the field, and a slot for each still
    /// loading. A thumbnail that goes narrows away and the ones after it move
    /// over; the strip folds in with its first and away with its last. Each
    /// thumbnail carries the gap after and above it, so a gap goes with it.
    pub(in super::super) fn render_image_strip(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut thumbs: Vec<(gpui::SharedString, StripItem)> = self
            .pending_images
            .iter()
            .enumerate()
            .map(|(index, image)| {
                (
                    image.path.to_string_lossy().into_owned().into(),
                    StripItem::Image(index, image.clone()),
                )
            })
            .collect();
        thumbs.extend(
            self.pending_files
                .iter()
                .enumerate()
                .map(|(index, file)| (file.key(), StripItem::File(index))),
        );
        thumbs.extend(
            (0..self.pending_image_loads)
                .map(|slot| (format!("loading-{slot}").into(), StripItem::Loading)),
        );
        let strip = (!thumbs.is_empty()).then(|| (gpui::SharedString::from("strip"), thumbs));
        crate::motion::keyed::keyed_owned(
            &self.motion,
            self.motion_key("image-strip"),
            crate::motion::keyed::Fold::Down,
            strip,
            ROWS,
            window,
            cx,
            |_, thumbs, window, cx| {
                crate::motion::keyed::keyed_owned(
                    &self.motion,
                    self.motion_key("image-thumbs"),
                    crate::motion::keyed::Fold::Across(px(0.)),
                    thumbs.clone(),
                    ROWS,
                    window,
                    cx,
                    |key, thumb, window, cx| {
                        let thumb = match thumb {
                            StripItem::Image(index, image) => {
                                self.render_thumb(key, *index, image, window, cx)
                            }
                            StripItem::File(index) => self.render_file_chip(key, *index, cx),
                            StripItem::Loading => Button::new(gpui::ElementId::NamedChild(
                                std::sync::Arc::new("thumb".into()),
                                key.clone(),
                            ))
                            .secondary()
                            .small()
                            .chip()
                            .icon(IconName::Image)
                            .loading(true)
                            .aria_label(crate::tr!("composer.loading"))
                            .into_any_element(),
                        };
                        div().pr_1().pt_1().child(thumb).into_any_element()
                    },
                )
                .render(h_flex().w_full().flex_wrap().pt_0p5())
            },
        )
        .render(v_flex().w_full())
    }

    /// The thumbnail of the pending image at `index`; `key` names it for as
    /// long as it is pending, wherever it moves in the strip.
    fn render_thumb(
        &self,
        key: &gpui::SharedString,
        index: usize,
        image: &PendingImage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = image.path.clone();
        // The picture fades in once it decodes.
        let preview: gpui::ImageSource = crate::store::image_preview(
            self.workspace_store.read(cx).image_host(),
            path.clone(),
            18.,
            false,
        );
        let arrival = crate::motion::image_arrival(
            crate::motion::Clock::from(&self.motion),
            self.motion_key(format_args!("thumb-arrival-{key}")),
            &preview,
            ARRIVE,
            window,
            cx,
        );
        let menu_path = path.to_string_lossy().into_owned();
        let name = image.name.clone();
        let id = |part: &'static str| {
            gpui::ElementId::NamedChild(std::sync::Arc::new(part.into()), key.clone())
        };
        Button::new(id("thumb"))
            .secondary()
            .small()
            .chip()
            .aria_label(name.clone())
            // A long file name truncates rather than taking the strip.
            .max_w(px(220.))
            .overflow_hidden()
            // The thumbnail is decoded at the size it draws, inside the
            // chip's height; no owner sizes a chip's leading image.
            .child(
                img(preview)
                    .opacity(arrival)
                    .id("attachment-image")
                    .size(px(18.))
                    .rounded(crate::material::radius_chip(cx))
                    .with_fallback(crate::image_viewer::unpreviewed(false, false, cx)),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_meta_mono(self.compact, cx)
                    .child(crate::widgets::fade_text(name)),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_image_preview(index, window, cx);
            }))
            .child(
                Button::new(id("thumb-x"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .aria_label(crate::tr!("composer.remove_image"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.remove_image(index, cx);
                    })),
            )
            .context_menu(move |menu, _, _| {
                menu.menu(
                    crate::tr!("markdown.open_image").into_owned(),
                    Box::new(ComposerMenu::ImageOpen(index)),
                )
                .separator()
                .path_items(&menu_path, None)
                .separator()
                .menu(
                    crate::tr!("composer.remove_image").into_owned(),
                    Box::new(ComposerMenu::ImageRemove(index)),
                )
            })
            .into_any_element()
    }

    /// Open the clicked thumbnail as a window-level lightbox (see
    /// [`crate::image_viewer::open`]).
    pub(in super::super) fn open_image_preview(
        &self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(image) = self.pending_images.get(index) else {
            return;
        };
        let host = self.workspace_store.read(cx).image_host();
        crate::image_viewer::open(
            crate::image_viewer::ViewerSource::original(
                host,
                tcode_protocol::OriginalTarget::File {
                    path: image.path.clone(),
                },
                crate::store::image_preview(host, image.path.clone(), 18., false),
            ),
            image.name.clone(),
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(image: image::DynamicImage, format: image::ImageFormat) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn fitting_bounds_the_long_edge_and_keeps_qualifying_bytes() {
        let wide = image::DynamicImage::new_rgb8(MAX_EDGE_PX * 2, 100);
        let (mime, bytes) =
            fit_for_upload("image/png", encoded(wide, image::ImageFormat::Png)).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(
            image::load_from_memory(&bytes).unwrap().dimensions(),
            (MAX_EDGE_PX, 50)
        );

        let photo = encoded(
            image::DynamicImage::new_rgb8(100, MAX_EDGE_PX * 2),
            image::ImageFormat::Jpeg,
        );
        let (mime, bytes) = fit_for_upload("image/jpeg", photo).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(
            image::load_from_memory(&bytes).unwrap().dimensions(),
            (50, MAX_EDGE_PX)
        );

        let small = encoded(
            image::DynamicImage::new_rgba8(10, 10),
            image::ImageFormat::Png,
        );
        assert_eq!(
            fit_for_upload("image/png", small.clone()).unwrap(),
            ("image/png".to_string(), small)
        );

        let bmp = encoded(
            image::DynamicImage::new_rgb8(10, 10),
            image::ImageFormat::Bmp,
        );
        let (mime, bytes) = fit_for_upload("image/bmp", bmp).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(
            image::guess_format(&bytes).unwrap(),
            image::ImageFormat::Png
        );

        let gif = encoded(
            image::DynamicImage::new_rgba8(MAX_EDGE_PX * 2, 10),
            image::ImageFormat::Gif,
        );
        assert_eq!(
            fit_for_upload("image/gif", gif.clone()).unwrap(),
            ("image/gif".to_string(), gif)
        );

        assert!(fit_for_upload("image/png", b"not an image".to_vec()).is_err());
    }
}
