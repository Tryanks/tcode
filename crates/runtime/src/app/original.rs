use std::fs::File;
use std::io::{self, Read as _};
use std::time::SystemTime;

use base64::engine::general_purpose::STANDARD;
use base64::read::DecoderReader;
use tcode_client::original::{OriginalSource, error};
use tcode_protocol::{ClientPayload, OriginalMetadata, OriginalTarget, Principal, ProtocolError};

use super::{AppState, HostCx, StoreWrite, history::SessionLog};

const BLOCK: usize = 64 * 1024;
type Answer = async_channel::Sender<Result<OriginalSource, ProtocolError>>;

impl AppState {
    pub(crate) fn start_original(
        &mut self,
        principal: Principal,
        target: OriginalTarget,
        answer: Answer,
        cx: &mut HostCx,
    ) {
        self.observe_principal(&principal);
        if let Err(error) = self.authorize(&principal, &ClientPayload::Query(target.query()), cx) {
            let _ = answer.try_send(Err(error));
            return;
        }
        let snapshot = match &target {
            OriginalTarget::Item { session_id, .. } => Some(self.original_snapshot(session_id, cx)),
            _ => None,
        };
        let store = self.store.clone();
        let host = cx.clone();
        cx.spawn_background(async move {
            let result = async {
                if answer.is_closed() {
                    return Err(error("original read cancelled"));
                }
                let mut source = match &target {
                    OriginalTarget::File { path } => {
                        let requested_path = path.clone();
                        host.unblock(move || {
                            let path = requested_path.canonicalize().map_err(io_error)?;
                            let file = File::open(&path).map_err(io_error)?;
                            let metadata = file.metadata().map_err(io_error)?;
                            if !metadata.is_file() {
                                return Err(error("original is not a regular file"));
                            }
                            let identity = (metadata.len(), metadata.modified().map_err(io_error)?);
                            Ok(Source::File {
                                file,
                                path,
                                requested_path,
                                identity,
                            })
                        })
                        .await?
                    }
                    OriginalTarget::Item {
                        session_id,
                        item_id,
                        image_index,
                    } => {
                        let end = snapshot
                            .unwrap()
                            .recv()
                            .await
                            .map_err(error)?
                            .map_err(error)?;
                        let (session_id, item_id, image_index) =
                            (session_id.clone(), item_id.clone(), *image_index);
                        host.unblock(move || {
                            let log = SessionLog::new(
                                store.read_log_until(&session_id, end).map_err(io_error)?,
                            );
                            let (row, image) = log.original_image(&item_id, image_index)?;
                            let len = tcode_services::image_bytes::decoded_len(&image.data_base64)
                                .map_err(io_error)?;
                            let mime = image.media_type.clone();
                            Ok(Source::Item {
                                log: Box::new(log),
                                item_id,
                                image_index,
                                identity: (row, len),
                                mime,
                            })
                        })
                        .await?
                    }
                };
                let checked = match &source {
                    Source::File { path, .. } => OriginalTarget::File { path: path.clone() },
                    _ => target.clone(),
                };
                check(&host, principal.clone(), checked).await?;
                let (blocks, received) = async_channel::bounded(2);
                if answer
                    .try_send(Ok(OriginalSource {
                        metadata: source.metadata(),
                        blocks: received,
                    }))
                    .is_err()
                {
                    return Ok(());
                }
                let output = blocks.clone();
                let tail = host
                    .unblock(move || {
                        let read: Box<dyn io::Read + '_> = match &mut source {
                            Source::File { file, .. } => Box::new(file),
                            Source::Item {
                                log,
                                item_id,
                                image_index,
                                ..
                            } => {
                                let (_, image) = log.original_image(item_id, *image_index)?;
                                Box::new(DecoderReader::new(
                                    image.data_base64.as_bytes(),
                                    &STANDARD,
                                ))
                            }
                        };
                        let tail = stream_blocks(read, &output)?;
                        if let Source::File {
                            file,
                            identity,
                            requested_path,
                            ..
                        } = &source
                            && (file_identity(file.metadata().map_err(io_error)?)? != *identity
                                || file_identity(
                                    std::fs::metadata(requested_path).map_err(io_error)?,
                                )? != *identity)
                        {
                            return Err(changed());
                        }
                        Ok((source, tail))
                    })
                    .await;
                let finish = async {
                    let (source, tail) = tail?;
                    check(&host, principal, target.clone()).await?;
                    if let Source::Item {
                        identity,
                        item_id,
                        image_index,
                        ..
                    } = source
                    {
                        let OriginalTarget::Item { session_id, .. } = target else {
                            unreachable!()
                        };
                        let snapshot_id = session_id.clone();
                        let (store, snapshot) = host
                            .enqueue_and_wait(move |state, cx| {
                                (
                                    state.store.clone(),
                                    state.original_snapshot(&snapshot_id, cx),
                                )
                            })
                            .await
                            .map_err(|_| error("host stopped"))?;
                        let end = snapshot.recv().await.map_err(error)?.map_err(error)?;
                        host.unblock(move || {
                            let log = SessionLog::new(
                                store.read_log_until(&session_id, end).map_err(io_error)?,
                            );
                            let (row, image) = log
                                .original_image(&item_id, image_index)
                                .map_err(|_| changed())?;
                            let len = tcode_services::image_bytes::decoded_len(&image.data_base64)
                                .map_err(io_error)?;
                            if identity != (row, len) {
                                return Err(changed());
                            }
                            Ok(())
                        })
                        .await?;
                    }
                    if !tail.is_empty() {
                        blocks.send(Ok(tail)).await.map_err(error)?;
                    }
                    blocks.send(Ok(Vec::new())).await.map_err(error)?;
                    Ok(())
                }
                .await;
                if let Err(error) = finish {
                    let _ = blocks.send(Err(error)).await;
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                let _ = answer.try_send(Err(error));
            }
        })
        .detach();
    }

    fn original_snapshot(
        &mut self,
        id: &str,
        cx: &mut HostCx,
    ) -> async_channel::Receiver<Result<u64, String>> {
        let (end, snapshot) = async_channel::bounded(1);
        self.enqueue_store_write(StoreWrite::LogEnd { id: id.into(), end }, cx);
        snapshot
    }
}

async fn check(
    host: &HostCx,
    principal: Principal,
    target: OriginalTarget,
) -> Result<(), ProtocolError> {
    host.enqueue_and_wait(move |state, cx| {
        state.authorize(&principal, &ClientPayload::Query(target.query()), cx)
    })
    .await
    .map_err(|_| error("host stopped"))?
}

enum Source {
    File {
        file: File,
        path: std::path::PathBuf,
        requested_path: std::path::PathBuf,
        identity: (u64, SystemTime),
    },
    Item {
        log: Box<SessionLog>,
        item_id: String,
        image_index: usize,
        identity: (u64, u64),
        mime: String,
    },
}

impl Source {
    fn metadata(&self) -> OriginalMetadata {
        match self {
            Self::File { path, identity, .. } => OriginalMetadata {
                len: identity.0,
                mime: Some(tcode_core::attachments::mime_from_path(path)),
            },
            Self::Item { identity, mime, .. } => OriginalMetadata {
                len: identity.1,
                mime: Some(mime.clone()),
            },
        }
    }
}

fn stream_blocks(
    mut read: Box<dyn io::Read + '_>,
    output: &async_channel::Sender<Result<Vec<u8>, ProtocolError>>,
) -> Result<Vec<u8>, ProtocolError> {
    let mut held = Vec::new();
    loop {
        if output.is_closed() {
            return Err(error("original read cancelled"));
        }
        let mut block = vec![0; BLOCK];
        let count = read.read(&mut block).map_err(io_error)?;
        if count == 0 {
            return Ok(held);
        }
        block.truncate(count);
        if !held.is_empty() {
            output.send_blocking(Ok(held)).map_err(error)?;
        }
        held = block;
    }
}

fn file_identity(metadata: std::fs::Metadata) -> Result<(u64, SystemTime), ProtocolError> {
    Ok((metadata.len(), metadata.modified().map_err(io_error)?))
}

fn changed() -> ProtocolError {
    ProtocolError {
        code: "changed".into(),
        message: "original changed during the read".into(),
    }
}

fn io_error(err: io::Error) -> ProtocolError {
    if err.kind() == io::ErrorKind::NotFound {
        ProtocolError {
            code: "not_found".into(),
            message: err.to_string(),
        }
    } else {
        error(err)
    }
}
