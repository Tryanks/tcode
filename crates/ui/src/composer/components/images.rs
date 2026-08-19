use super::super::*;

#[derive(Clone)]
/// A pending image attachment: validated, persisted to the session attachments
/// dir, and shown in the composer thumbnail strip. Kept per active session.
pub(in super::super) struct PendingImage {
    /// On-disk path of the persisted copy (also the thumbnail image source).
    pub(in super::super) path: PathBuf,
    /// Display name.
    pub(in super::super) name: String,
}

pub(in super::super) enum PendingImageSource {
    Bytes(Vec<u8>),
    Path(PathBuf),
}

pub(in super::super) enum AddImageError {
    Attachment(tcode_core::attachments::AttachError),
    Persist(std::io::Error),
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

pub(in super::super) fn transcode_image_to_png(bytes: &[u8]) -> image::ImageResult<Vec<u8>> {
    let image = image::load_from_memory(bytes)?;
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png)?;
    Ok(png.into_inner())
}

impl Composer {
    /// Reset the thumbnail strip when the active session changes (its pending
    /// images belong to a specific session).
    pub(in super::super) fn sync_images_session(&mut self, cx: &mut Context<Self>) {
        let id = self.workspace_store.read(cx).active_session_id();
        if id != self.images_session {
            self.images_session = id;
            self.pending_images.clear();
            self.image_load_generation = self.image_load_generation.wrapping_add(1);
            self.pending_image_loads = 0;
        }
    }

    /// Validate, decode/transcode, and persist an image off the main thread.
    /// Completion appends only while the same session still owns the strip.
    pub(in super::super) fn add_image(
        &mut self,
        name: String,
        mime: String,
        source: PendingImageSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.sync_images_session(cx);
        let initial_size = match &source {
            PendingImageSource::Bytes(bytes) => bytes.len() as u64,
            PendingImageSource::Path(_) => 0,
        };
        if let Err(err) = validate_attachment(
            &name,
            &mime,
            initial_size,
            self.pending_images.len() + self.pending_image_loads,
        ) {
            window.push_notification(Notification::error(attach_error_message(&err)), cx);
            return false;
        }
        let session_id = self.workspace_store.read(cx).active_session_id();
        let attachments_dir = self
            .workspace_store
            .read(cx)
            .composer_state()
            .attachments_dir;
        let current_count = self.pending_images.len() + self.pending_image_loads;
        self.pending_image_loads += 1;
        let generation = self.image_load_generation;
        let result_name = name.clone();
        let workspace_store = self.workspace_store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    let bytes = match source {
                        PendingImageSource::Bytes(bytes) => bytes,
                        PendingImageSource::Path(path) => {
                            let size = std::fs::metadata(&path)
                                .map_err(AddImageError::Persist)?
                                .len();
                            validate_attachment(&name, &mime, size, current_count)
                                .map_err(AddImageError::Attachment)?;
                            std::fs::read(path).map_err(AddImageError::Persist)?
                        }
                    };
                    validate_attachment(&name, &mime, bytes.len() as u64, current_count)
                        .map_err(AddImageError::Attachment)?;
                    let (mime, bytes) = if is_wire_ready_image(&mime) {
                        (mime, bytes)
                    } else {
                        let bytes = transcode_image_to_png(&bytes).map_err(|_| {
                            AddImageError::Attachment(
                                tcode_core::attachments::AttachError::UnsupportedType {
                                    name: name.clone(),
                                },
                            )
                        })?;
                        ("image/png".to_string(), bytes)
                    };
                    let dir = attachments_dir.ok_or_else(|| {
                        AddImageError::Persist(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "no active session",
                        ))
                    })?;
                    let ext = image_extension(&mime, &name);
                    Ok::<_, AddImageError>((dir, bytes, ext))
                })
                .await;
            let result = match prepared {
                Ok((dir, bytes, ext)) => workspace_store
                    .update(cx, |store, cx| {
                        store.save_attachment_to_dir(dir, bytes, ext, cx)
                    })
                    .await
                    .map_err(AddImageError::Persist),
                Err(error) => Err(error),
            };
            let _ = this.update_in(cx, |composer, window, cx| {
                if composer.image_load_generation != generation
                    || composer.images_session != session_id
                    || composer.workspace_store.read(cx).active_session_id() != session_id
                {
                    return;
                }
                composer.pending_image_loads = composer.pending_image_loads.saturating_sub(1);
                match result {
                    Ok(path) => {
                        composer.pending_images.push(PendingImage {
                            path,
                            name: result_name,
                        });
                        cx.notify();
                    }
                    Err(AddImageError::Attachment(err)) => window
                        .push_notification(Notification::error(attach_error_message(&err)), cx),
                    Err(AddImageError::Persist(err)) => window.push_notification(
                        Notification::error(
                            crate::tr!("errors.persist_event", error = err).into_owned(),
                        ),
                        cx,
                    ),
                }
            });
        })
        .detach();
        true
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
        let mut accepted_image = false;
        if let Some(item) = cx.read_from_clipboard() {
            for entry in &item.entries {
                match entry {
                    ClipboardEntry::Image(image) => {
                        let mime = image.format().mime_type().to_string();
                        let bytes = image.bytes().to_vec();
                        accepted_image |= self.add_image_bytes(
                            "pasted-image".to_string(),
                            mime,
                            bytes,
                            window,
                            cx,
                        );
                    }
                    ClipboardEntry::ExternalPaths(paths) => {
                        for path in paths.paths() {
                            if mime_from_path(path).starts_with("image/") {
                                accepted_image |= self.add_image_path(path.clone(), window, cx);
                            }
                        }
                    }
                    ClipboardEntry::String(_) => {}
                }
            }
        }
        if !accepted_image && let Some((mime, bytes)) = crate::pasteboard::read_pasteboard_image() {
            accepted_image =
                self.add_image_bytes("pasted-image".to_string(), mime, bytes, window, cx);
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

    pub(in super::super) fn render_image_strip(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.pending_images.is_empty() {
            return None;
        }
        let mut row = h_flex().w_full().gap_1().flex_wrap();
        for (index, image) in self.pending_images.iter().enumerate() {
            let path = image.path.clone();
            let name = image.name.clone();
            row = row.child(
                h_flex()
                    .id(("thumb", index))
                    .flex_none()
                    .h(px(22.))
                    .max_w(px(220.))
                    .gap_1()
                    .items_center()
                    .pl(px(2.))
                    .pr_1()
                    .rounded(crate::material::radius_chip())
                    .overflow_hidden()
                    .bg(cx.theme().secondary)
                    .cursor_pointer()
                    .child(
                        img(path)
                            .size(px(18.))
                            .rounded(crate::material::radius_chip()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.5))
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(name),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_image_preview(index, window, cx);
                    }))
                    .child(
                        div()
                            .id(("thumb-x", index))
                            .flex_none()
                            .size(px(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(crate::material::radius_chip())
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().muted))
                            .child(
                                Icon::new(IconName::Close)
                                    .xsmall()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_image(index, cx);
                            })),
                    ),
            );
        }
        Some(row.into_any_element())
    }

    /// Open the clicked thumbnail as a window-level lightbox (see
    /// [`crate::attachments::open_image_lightbox`]).
    pub(in super::super) fn open_image_preview(
        &self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(image) = self.pending_images.get(index) else {
            return;
        };
        crate::attachments::open_image_lightbox(image.path.clone(), image.name.clone(), window, cx);
    }
}
