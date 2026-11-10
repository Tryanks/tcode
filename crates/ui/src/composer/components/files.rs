//! Non-image attachments: each is a chip in the composer and a path in the
//! message text. A file this client shares a disk with the host stays where
//! it is; any other is uploaded in chunks and the path is the host's copy.

use super::super::*;
use super::images::confirm_large_transfer;
use crate::attachments::{TransferLink, TransferVerdict, transfer_verdict};
use crate::thread_export::format_size;
use crate::widgets::menu::ContextMenuExt as _;
use gpui::WeakEntity;
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;
use tcode_core::attachments::{MAX_UPLOAD_BYTES, UPLOAD_CHUNK_BYTES, is_image_attachment};

/// Where a file's bytes come from.
#[derive(Clone)]
pub(in super::super) enum FileSource {
    Path(PathBuf),
    /// A picker's result; there is no path this client could offer.
    Bytes(Arc<Vec<u8>>),
}

enum ChunkReader {
    File(std::fs::File),
    Bytes { bytes: Arc<Vec<u8>>, at: usize },
}

impl ChunkReader {
    fn open(source: &FileSource) -> std::io::Result<Self> {
        Ok(match source {
            FileSource::Path(path) => Self::File(std::fs::File::open(path)?),
            FileSource::Bytes(bytes) => Self::Bytes {
                bytes: bytes.clone(),
                at: 0,
            },
        })
    }

    /// The next chunk; empty at the end of the file.
    fn next_chunk(&mut self) -> std::io::Result<Vec<u8>> {
        match self {
            Self::File(file) => {
                let mut chunk = Vec::with_capacity(UPLOAD_CHUNK_BYTES);
                file.take(UPLOAD_CHUNK_BYTES as u64)
                    .read_to_end(&mut chunk)?;
                Ok(chunk)
            }
            Self::Bytes { bytes, at } => {
                let end = (*at + UPLOAD_CHUNK_BYTES).min(bytes.len());
                let chunk = bytes[*at..end].to_vec();
                *at = end;
                Ok(chunk)
            }
        }
    }
}

enum FileState {
    Ready,
    Uploading { sent: u64 },
    Failed { error: String },
}

/// One attached file, shown as a chip until the message is sent.
pub(in super::super) struct PendingFile {
    id: u64,
    name: String,
    directory: bool,
    /// The path the message carries, once the host has reserved it for an
    /// upload.
    path: Option<PathBuf>,
    /// What to upload; a file the host can reach itself has none.
    upload: Option<(FileSource, u64)>,
    state: FileState,
    transfer: Option<Task<()>>,
}

impl PendingFile {
    /// Names the chip for as long as it is pending, wherever it moves.
    pub(in super::super) fn key(&self) -> gpui::SharedString {
        format!("file-{}", self.id).into()
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Send one source to the host, reporting each chunk to the chip.
async fn upload(
    composer: WeakEntity<Composer>,
    store: Entity<WorkspaceStore>,
    (session_id, id, name): (String, u64, String),
    source: FileSource,
    size: u64,
    cx: &mut gpui::AsyncApp,
) -> std::io::Result<()> {
    let path = store
        .update(cx, |store, cx| {
            store.begin_upload(session_id.clone(), name, size, cx)
        })
        .await?;
    composer
        .update(cx, |composer, _| {
            composer.set_file_path(id, path.clone());
        })
        .ok();
    let executor = cx.background_executor().clone();
    let mut reader = executor
        .spawn(async move { ChunkReader::open(&source) })
        .await?;
    let mut sent = 0;
    loop {
        let (returned, chunk) = executor
            .spawn(async move {
                let chunk = reader.next_chunk();
                (reader, chunk)
            })
            .await;
        reader = returned;
        let chunk = chunk?;
        if chunk.is_empty() {
            return Ok(());
        }
        let length = chunk.len() as u64;
        store
            .update(cx, |store, cx| {
                store.write_upload(session_id.clone(), path.clone(), sent, chunk, cx)
            })
            .await?;
        sent += length;
        composer
            .update(cx, |composer, cx| composer.set_upload_sent(id, sent, cx))
            .ok();
    }
}

impl Composer {
    /// Attach a file or folder from a path on this client.
    pub(in super::super) fn add_file_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_images_session(cx);
        let session_id = self.session_id.clone();
        let generation = self.image_load_generation;
        let probed = path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let metadata = cx
                .background_executor()
                .spawn(async move { std::fs::metadata(probed) })
                .await;
            let _ = this.update_in(cx, |composer, window, cx| {
                if !composer.owns_image_load(generation, &session_id) {
                    return;
                }
                let name = file_name_of(&path);
                match metadata {
                    Ok(metadata) => composer.admit_file(path, name, metadata, window, cx),
                    Err(error) => window.push_notification(
                        Notification::error(
                            crate::tr!("attach.read_failed", name = name, error = error)
                                .into_owned(),
                        ),
                        cx,
                    ),
                }
            });
        })
        .detach();
    }

    fn admit_file(
        &mut self,
        path: PathBuf,
        name: String,
        metadata: std::fs::Metadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local = self.workspace_store.read(cx).attachment_link() == TransferLink::Local;
        if local {
            if !self
                .pending_files
                .iter()
                .any(|file| file.path.as_ref() == Some(&path))
            {
                self.push_file(name, metadata.is_dir(), Some(path), None, FileState::Ready);
                cx.notify();
            }
        } else if metadata.is_dir() {
            window.push_notification(
                Notification::error(crate::tr!("attach.directory_remote", name = name)),
                cx,
            );
        } else {
            self.admit_upload(name, FileSource::Path(path), metadata.len(), window, cx);
        }
    }

    /// Attach bytes a picker returned, which only an upload can bring to the
    /// host.
    pub(in super::super) fn add_file_bytes(
        &mut self,
        name: String,
        bytes: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_images_session(cx);
        let size = bytes.len() as u64;
        self.admit_upload(name, FileSource::Bytes(Arc::new(bytes)), size, window, cx);
    }

    /// The size and link rules every upload passes.
    fn admit_upload(
        &mut self,
        name: String,
        source: FileSource,
        size: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if size > MAX_UPLOAD_BYTES {
            window.push_notification(
                Notification::error(crate::tr!(
                    "attach.file_too_large",
                    name = name,
                    limit = format_size(MAX_UPLOAD_BYTES)
                )),
                cx,
            );
            return;
        }
        let store = self.workspace_store.read(cx);
        let link = store.attachment_link();
        let limit = crate::device::DevicePreferences::remote_attachment_limit_bytes(cx);
        match transfer_verdict(link, size, limit) {
            TransferVerdict::Send => self.start_new_upload(name, source, size, cx),
            TransferVerdict::Reject => window.push_notification(
                Notification::error(crate::tr!(
                    "attach.relay_rejected",
                    name = name,
                    size = format_size(size),
                    limit = format_size(limit)
                )),
                cx,
            ),
            TransferVerdict::Confirm => {
                let generation = self.image_load_generation;
                confirm_large_transfer(
                    cx.entity(),
                    name.clone(),
                    size,
                    window,
                    cx,
                    move |composer, _, cx| {
                        if composer.image_load_generation == generation {
                            composer.start_new_upload(name, source, size, cx);
                        }
                    },
                    |_, _| {},
                );
            }
        }
    }

    fn push_file(
        &mut self,
        name: String,
        directory: bool,
        path: Option<PathBuf>,
        upload: Option<(FileSource, u64)>,
        state: FileState,
    ) -> u64 {
        self.next_file_id += 1;
        let id = self.next_file_id;
        self.pending_files.push(PendingFile {
            id,
            name,
            directory,
            path,
            upload,
            state,
            transfer: None,
        });
        id
    }

    fn start_new_upload(
        &mut self,
        name: String,
        source: FileSource,
        size: u64,
        cx: &mut Context<Self>,
    ) {
        let id = self.push_file(
            name,
            false,
            None,
            Some((source, size)),
            FileState::Uploading { sent: 0 },
        );
        self.run_upload(id, cx);
        cx.notify();
    }

    fn run_upload(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(file) = self.pending_files.iter_mut().find(|file| file.id == id) else {
            return;
        };
        let Some((source, size)) = file.upload.clone() else {
            return;
        };
        let session_id = self.session_id.clone().unwrap_or_default();
        let name = file.name.clone();
        let store = self.workspace_store.clone();
        file.transfer = Some(cx.spawn(async move |this, cx| {
            let result = upload(
                this.clone(),
                store,
                (session_id, id, name),
                source,
                size,
                cx,
            )
            .await;
            let _ = this.update(cx, |composer, cx| {
                composer.finish_upload(id, result, cx);
            });
        }));
    }

    fn set_file_path(&mut self, id: u64, path: PathBuf) {
        if let Some(file) = self.pending_files.iter_mut().find(|file| file.id == id) {
            file.path = Some(path);
        }
    }

    fn set_upload_sent(&mut self, id: u64, sent: u64, cx: &mut Context<Self>) {
        if let Some(file) = self.pending_files.iter_mut().find(|file| file.id == id) {
            file.state = FileState::Uploading { sent };
            cx.notify();
        }
    }

    fn finish_upload(&mut self, id: u64, result: std::io::Result<()>, cx: &mut Context<Self>) {
        if let Some(file) = self.pending_files.iter_mut().find(|file| file.id == id) {
            file.transfer = None;
            file.state = match result {
                Ok(()) => FileState::Ready,
                Err(error) => FileState::Failed {
                    error: error.to_string(),
                },
            };
            cx.notify();
        }
    }

    fn retry_upload(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(file) = self.pending_files.get_mut(index) else {
            return;
        };
        let id = file.id;
        let reserved = file.path.take();
        file.state = FileState::Uploading { sent: 0 };
        if let Some(path) = reserved {
            self.discard_host_copy(path, cx);
        }
        self.run_upload(id, cx);
        cx.notify();
    }

    fn discard_host_copy(&self, path: PathBuf, cx: &mut Context<Self>) {
        let session_id = self.session_id.clone().unwrap_or_default();
        self.workspace_store
            .update(cx, |store, cx| store.discard_upload(session_id, path, cx))
            .detach();
    }

    pub(in super::super) fn remove_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.pending_files.len() {
            let file = self.pending_files.remove(index);
            if let (Some(_), Some(path)) = (file.upload, file.path) {
                self.discard_host_copy(path, cx);
            }
            cx.notify();
        }
    }

    /// Chips belong to one thread: leaving it drops them, and the host copies
    /// no message will carry.
    pub(in super::super) fn discard_files_of(&mut self, session_id: Option<String>, cx: &mut App) {
        for file in self.pending_files.drain(..) {
            if let (Some(_), Some(path), Some(session_id)) = (file.upload, file.path, &session_id) {
                self.workspace_store
                    .update(cx, |store, cx| {
                        store.discard_upload(session_id.clone(), path, cx)
                    })
                    .detach();
            }
        }
    }

    /// Whether a file has not reached the host yet, so sending now would
    /// lose it.
    pub(in super::super) fn files_unsettled(&self) -> bool {
        self.pending_files
            .iter()
            .any(|file| !matches!(file.state, FileState::Ready))
    }

    /// The paths the next message carries, as the model will read them.
    pub(in super::super) fn file_paths(&self) -> Vec<String> {
        self.pending_files
            .iter()
            .filter_map(|file| file.path.as_ref())
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    /// Open the platform's file picker and add what it returns.
    pub(in super::super) fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = cx
            .try_global::<crate::remote::ClientAttachment>()
            .map(crate::remote::ClientAttachment::host)
        else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let picked = host.pick_files().await;
            let _ = this.update_in(cx, |composer, window, cx| match picked {
                Ok(files) => {
                    for file in files {
                        let mime = if is_image_attachment(&file.mime) {
                            file.mime
                        } else {
                            mime_from_path(Path::new(&file.name))
                        };
                        if is_image_attachment(&mime) {
                            composer.add_image_bytes(file.name, mime, file.bytes, window, cx);
                        } else {
                            composer.add_file_bytes(file.name, file.bytes, window, cx);
                        }
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

    /// Open the system's file picker and attach what is chosen as if it had
    /// been dropped: images as images, any other file by its path.
    #[cfg(feature = "native-dialogs")]
    pub(in super::super) fn pick_paths(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let paths = match picked.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    let _ = this.update_in(cx, |_, window, cx| {
                        window.push_notification(
                            Notification::error(
                                crate::tr!("attach.files_failed", error = error.to_string())
                                    .into_owned(),
                            ),
                            cx,
                        );
                    });
                    return;
                }
            };
            let _ = this.update_in(cx, |composer, window, cx| {
                for path in paths {
                    composer.add_path(path, window, cx);
                }
                // The field takes the keys again, as it does after a drop.
                composer.focus(window, cx);
            });
        })
        .detach();
    }

    /// The chip of the pending file at `index`: its name and folder, and how
    /// its upload stands.
    pub(in super::super) fn render_file_chip(
        &self,
        key: &gpui::SharedString,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(file) = self.pending_files.get(index) else {
            return div().into_any_element();
        };
        let id = |part: &'static str| {
            gpui::ElementId::NamedChild(std::sync::Arc::new(part.into()), key.clone())
        };
        let path = file
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let folder = file
            .path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|parent| parent.to_string_lossy().into_owned());
        let (status, tooltip) = match &file.state {
            FileState::Ready => (None, path.clone().unwrap_or_else(|| file.name.clone())),
            FileState::Uploading { sent } => {
                let percent = file
                    .upload
                    .as_ref()
                    .filter(|(_, size)| *size > 0)
                    .map_or(0, |(_, size)| sent.saturating_mul(100) / size);
                (
                    Some(format!("{percent}%")),
                    crate::tr!("attach.uploading", name = file.name).into_owned(),
                )
            }
            FileState::Failed { error } => (
                Some(crate::tr!("attach.upload_failed").into_owned()),
                crate::tr!(
                    "attach.upload_failed_detail",
                    name = file.name,
                    error = error
                )
                .into_owned(),
            ),
        };
        let failed = matches!(file.state, FileState::Failed { .. });
        let muted = cx.theme().muted_foreground;
        let mut chip = Button::new(id("file"))
            .secondary()
            .small()
            .chip()
            .debug_selector(move || format!("attached-file-{index}"))
            .aria_label(file.name.clone())
            .max_w(px(420.))
            .overflow_hidden()
            .tooltip(tooltip)
            .icon(if file.directory {
                IconName::Folder
            } else {
                IconName::File
            })
            .loading(matches!(file.state, FileState::Uploading { .. }))
            .child(
                div()
                    .flex_none()
                    .max_w(px(220.))
                    .overflow_hidden()
                    .text_meta_mono(self.compact, cx)
                    .child(crate::widgets::fade_text(file.name.clone())),
            );
        if let Some(folder) = folder {
            chip = chip.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_meta_mono(self.compact, cx)
                    .text_color(muted)
                    .child(crate::widgets::fade_text(folder)),
            );
        }
        if let Some(status) = status {
            chip = chip.child(
                div()
                    .flex_none()
                    .text_meta_mono(self.compact, cx)
                    .when(failed, |status| status.text_color(cx.theme().danger))
                    .child(status),
            );
        }
        if failed {
            chip = chip.child(
                Button::new(id("file-retry"))
                    .ghost()
                    .xsmall()
                    .debug_selector(move || format!("attached-file-retry-{index}"))
                    .icon(IconName::Redo)
                    .aria_label(crate::tr!("attach.retry"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.retry_upload(index, cx);
                    })),
            );
        }
        let remove = crate::tr!("attach.remove_file").into_owned();
        chip.child(
            Button::new(id("file-x"))
                .ghost()
                .xsmall()
                .debug_selector(move || format!("attached-file-remove-{index}"))
                .icon(IconName::Close)
                .aria_label(remove.clone())
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.remove_file(index, cx);
                })),
        )
        .context_menu(move |menu, _, _| {
            menu.when_some(path.as_deref(), |menu, path| {
                menu.path_items(path, None).separator()
            })
            .menu(remove.clone(), Box::new(ComposerMenu::FileRemove(index)))
        })
        .into_any_element()
    }
}
