use std::{
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use tcode_protocol::{OriginalTarget, Principal, ProtocolError};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use uuid::Uuid;

use super::{Request, Shared, json_response};
use crate::http::response;
use crate::mux::AdmittedOriginal;

/// The bound on an original that makes no progress.
const IDLE: Duration = Duration::from_secs(60);
/// Lifetime of a one-time browser download ticket.
const TICKET_TTL: Duration = Duration::from_secs(30);
/// Cap on unclaimed download tickets held in memory at once.
const MAX_TICKETS: usize = 64;

pub(super) struct DownloadTicket {
    device: Uuid,
    token: String,
    path: PathBuf,
    filename: String,
    created: Instant,
}

fn authenticate_request(
    request: &Request,
    shared: &Shared,
) -> Result<(Uuid, String), (&'static str, &'static [u8])> {
    let token = request
        .headers
        .get("authorization")
        .and_then(|header| header.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned();
    let device = shared.auth.lock().unwrap().device_id_for_token(&token);
    let Some(device) = device else {
        return Err(("401 Unauthorized", b"invalid token"));
    };
    if let Some(origin) = request.headers.get("origin") {
        let same = url::Url::parse(origin).ok().is_some_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.authority()
                    == request
                        .headers
                        .get("host")
                        .map(String::as_str)
                        .unwrap_or_default()
        });
        if !same {
            return Err(("403 Forbidden", b"foreign origin"));
        }
    }
    Ok((device, token))
}

pub(super) async fn serve<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request: Request,
    shared: &Arc<Shared>,
) -> io::Result<()> {
    // Marked by every revocation and token rotation from here on.
    let devices_changed = shared.devices_changed.subscribe();
    let (device, token) = match authenticate_request(&request, shared) {
        Ok(auth) => auth,
        Err((status, body)) => return response(stream, status, "text/plain", body).await,
    };
    let target = request
        .target
        .split_once('?')
        .and_then(|(_, query)| {
            url::form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == "target")
        })
        .and_then(|(_, value)| serde_json::from_str::<OriginalTarget>(&value).ok());
    let Some(target) = target else {
        return response(stream, "400 Bad Request", "text/plain", b"invalid target").await;
    };
    let transfer = tokio::time::timeout(
        IDLE,
        shared.mux.open_original(
            format!("browser:{device}"),
            Principal::Full,
            target,
            shared.policy,
        ),
    )
    .await;
    let transfer = match transfer {
        Ok(Ok(transfer)) => transfer,
        Ok(Err(error)) => return failure(stream, error).await,
        Err(error) => return failure(stream, tcode_client::original::error(error)).await,
    };
    stream_original(
        stream,
        &request,
        shared,
        device,
        token,
        devices_changed,
        transfer,
        None,
    )
    .await
}

pub(super) async fn request_download<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request: Request,
    shared: &Arc<Shared>,
) -> io::Result<()> {
    let (device, token) = match authenticate_request(&request, shared) {
        Ok(auth) => auth,
        Err((status, body)) => return response(stream, status, "text/plain", body).await,
    };
    if !request.headers.get("content-type").is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    }) {
        return response(
            stream,
            "415 Unsupported Media Type",
            "text/plain",
            b"application/json required",
        )
        .await;
    }
    #[derive(Deserialize)]
    struct DownloadRequest {
        path: String,
    }
    let Ok(body) = serde_json::from_slice::<DownloadRequest>(&request.body) else {
        return response(
            stream,
            "400 Bad Request",
            "text/plain",
            b"malformed request",
        )
        .await;
    };
    if body.path.is_empty() {
        return response(stream, "400 Bad Request", "text/plain", b"empty path").await;
    }
    let path = PathBuf::from(&body.path);
    // Validate existence, regular-file kind, and authorization before issuing a
    // ticket so failures surface to the client instead of navigating to an error page.
    let transfer = tokio::time::timeout(
        IDLE,
        shared.mux.open_original(
            format!("browser:{device}"),
            Principal::Full,
            OriginalTarget::File { path: path.clone() },
            shared.policy,
        ),
    )
    .await;
    match transfer {
        Ok(Ok(transfer)) => drop(transfer),
        Ok(Err(error)) => return failure(stream, error).await,
        Err(error) => return failure(stream, tcode_client::original::error(error)).await,
    }
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let ticket = URL_SAFE_NO_PAD.encode(random);
    let filename = download_filename(&body.path);
    {
        let now = Instant::now();
        let mut tickets = shared.download_tickets.lock().unwrap();
        tickets.retain(|_, entry| now.saturating_duration_since(entry.created) < TICKET_TTL);
        while tickets.len() >= MAX_TICKETS {
            if let Some(oldest) = tickets
                .iter()
                .min_by_key(|(_, entry)| entry.created)
                .map(|(key, _)| key.clone())
            {
                tickets.remove(&oldest);
            } else {
                break;
            }
        }
        tickets.insert(
            ticket.clone(),
            DownloadTicket {
                device,
                token,
                path,
                filename: filename.clone(),
                created: now,
            },
        );
    }
    json_response(
        stream,
        "200 OK",
        &serde_json::json!({ "ticket": ticket, "filename": filename }),
    )
    .await
}

pub(super) async fn serve_download<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request: Request,
    shared: &Arc<Shared>,
) -> io::Result<()> {
    let ticket_id = request
        .target
        .split_once('?')
        .and_then(|(_, query)| {
            url::form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == "ticket")
        })
        .map(|(_, value)| value.into_owned());
    let Some(ticket_id) = ticket_id.filter(|id| !id.is_empty()) else {
        return response(stream, "400 Bad Request", "text/plain", b"missing ticket").await;
    };
    let ticket = {
        let now = Instant::now();
        let mut tickets = shared.download_tickets.lock().unwrap();
        tickets.retain(|_, entry| now.saturating_duration_since(entry.created) < TICKET_TTL);
        tickets.remove(&ticket_id)
    };
    let Some(ticket) = ticket else {
        return response(
            stream,
            "404 Not Found",
            "text/plain",
            b"download ticket expired or invalid",
        )
        .await;
    };
    let devices_changed = shared.devices_changed.subscribe();
    if shared
        .auth
        .lock()
        .unwrap()
        .device_id_for_token(&ticket.token)
        != Some(ticket.device)
    {
        return response(stream, "401 Unauthorized", "text/plain", b"invalid token").await;
    }
    let transfer = tokio::time::timeout(
        IDLE,
        shared.mux.open_original(
            format!("browser:{}", ticket.device),
            Principal::Full,
            OriginalTarget::File { path: ticket.path },
            shared.policy,
        ),
    )
    .await;
    let transfer = match transfer {
        Ok(Ok(transfer)) => transfer,
        Ok(Err(error)) => return failure(stream, error).await,
        Err(error) => return failure(stream, tcode_client::original::error(error)).await,
    };
    stream_original(
        stream,
        &request,
        shared,
        ticket.device,
        ticket.token,
        devices_changed,
        transfer,
        Some(&ticket.filename),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn stream_original<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request: &Request,
    shared: &Arc<Shared>,
    device: Uuid,
    token: String,
    mut devices_changed: tokio::sync::watch::Receiver<()>,
    transfer: AdmittedOriginal,
    download_filename: Option<&str>,
) -> io::Result<()> {
    let source = &transfer.source;
    let len = source.metadata.len;
    let (start, end, partial) = match request.headers.get("range") {
        None => (0, len, false),
        Some(range) => match parse_range(range, len) {
            Some((start, end)) => (start, end, true),
            None => {
                stream.write_all(format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await?;
                return stream.flush().await;
            }
        },
    };
    // Downloads force octet-stream + attachment + nosniff so an HTML or SVG file
    // on the host can never execute in the browser listener's origin.
    let mime = if download_filename.is_some() {
        "application/octet-stream"
    } else {
        source
            .metadata
            .mime
            .as_deref()
            .filter(|mime| !mime.contains(['\r', '\n']))
            .unwrap_or("application/octet-stream")
    };
    let mut head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\nConnection: close\r\n",
        if partial {
            "206 Partial Content"
        } else {
            "200 OK"
        },
        end - start
    );
    if let Some(filename) = download_filename {
        head.push_str(&format!(
            "Content-Disposition: {}\r\nX-Content-Type-Options: nosniff\r\n",
            content_disposition(filename)
        ));
    }
    if partial {
        head.push_str(&format!(
            "Content-Range: bytes {start}-{}/{len}\r\n",
            end - 1
        ));
    }
    if download_filename.is_none()
        && let Some((width, height)) = source.metadata.dimensions
    {
        head.push_str(&format!("X-Image-Dimensions: {width}x{height}\r\n"));
    }
    head.push_str("\r\n");
    if len == 0 {
        let complete = tokio::time::timeout(IDLE, source.blocks.recv()).await;
        match complete {
            Ok(Ok(Ok(block))) if block.is_empty() => {}
            Ok(Ok(Err(error))) => return failure(stream, error).await,
            _ => {
                return failure(
                    stream,
                    tcode_client::original::error("invalid empty original"),
                )
                .await;
            }
        }
    }
    let body = async {
        write_progress(stream, head.as_bytes()).await?;
        let mut position = 0_u64;
        let mut held = Vec::new();
        while position < len {
            let block = tokio::time::timeout(IDLE, source.blocks.recv())
                .await
                .map_err(io::Error::other)?
                .map_err(io::Error::other)?
                .map_err(|error| io::Error::other(error.message))?;
            if block.is_empty() {
                return Err(io::Error::other("original ended short"));
            }
            let next = position + block.len() as u64;
            if next > len {
                return Err(io::Error::other("original exceeded length"));
            }
            let from = start.max(position);
            let to = end.min(next);
            if from < to {
                if !held.is_empty() {
                    write_progress(stream, &held).await?;
                }
                held = block[(from - position) as usize..(to - position) as usize].to_vec();
            }
            position = next;
        }
        if len > 0 {
            let completion = tokio::time::timeout(IDLE, source.blocks.recv())
                .await
                .map_err(io::Error::other)?
                .map_err(io::Error::other)?
                .map_err(|error| io::Error::other(error.message))?;
            if !completion.is_empty() {
                return Err(io::Error::other("missing original completion"));
            }
        }
        if shared.auth.lock().unwrap().device_id_for_token(&token) != Some(device) {
            return Err(io::Error::other("token revoked"));
        }
        write_progress(stream, &held).await?;
        stream.flush().await
    };
    let revoked = async {
        loop {
            // Cannot fail: `shared` holds the sender.
            let _ = devices_changed.changed().await;
            if shared.auth.lock().unwrap().device_id_for_token(&token) != Some(device) {
                break;
            }
        }
    };
    tokio::select! {
        result = body => result,
        _ = revoked => Err(io::Error::other("token revoked")),
    }
}

fn download_filename(path: &str) -> String {
    let segment = path
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    let cleaned: String = segment.chars().filter(|ch| !ch.is_control()).collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "download".to_owned()
    } else {
        cleaned
    }
}

fn content_disposition(filename: &str) -> String {
    let ascii: String = filename
        .chars()
        .filter(|ch| !ch.is_control())
        .map(|ch| {
            if ch.is_ascii_graphic() && !matches!(ch, '"' | '\\' | ';') || ch == ' ' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let ascii = if ascii.trim().is_empty() {
        "download"
    } else {
        ascii.as_str()
    };
    let mut encoded = String::with_capacity(filename.len());
    for byte in filename.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(byte) {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

fn parse_range(value: &str, len: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    if start >= len {
        return None;
    }
    let end = if end.is_empty() {
        len
    } else {
        end.parse::<u64>().ok()?.saturating_add(1).min(len)
    };
    (end > start).then_some((start, end))
}

async fn write_progress<S: AsyncWrite + Unpin>(stream: &mut S, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let count = tokio::time::timeout(IDLE, stream.write(bytes))
            .await
            .map_err(io::Error::other)??;
        if count == 0 {
            return Err(io::Error::other("original write stopped"));
        }
        bytes = &bytes[count..];
    }
    Ok(())
}

async fn failure<S: AsyncWrite + Unpin>(stream: &mut S, error: ProtocolError) -> io::Result<()> {
    let status = match error.code.as_str() {
        "out_of_scope" => "403 Forbidden",
        "not_found" => "404 Not Found",
        "not_a_file" => "400 Bad Request",
        "changed" => "409 Conflict",
        "busy" => "429 Too Many Requests",
        _ => "500 Internal Server Error",
    };
    response(stream, status, "text/plain", error.message.as_bytes()).await
}
