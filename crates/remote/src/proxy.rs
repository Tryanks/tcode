//! Token-authenticated host-network egress. HTTPS stays an opaque byte tunnel.
use std::io;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_lite::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use url::Url;

use crate::{server::Shared, wire::Request};

fn credential(request: &Request) -> Option<String> {
    let value = request.headers.get("proxy-authorization")?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    let (user, token) = decoded.split_once(':')?;
    (user == "tcode").then(|| token.to_owned())
}

pub(crate) async fn handle<S>(mut stream: S, request: Request, shared: &Shared) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let token =
        credential(&request).filter(|token| shared.auth.lock().unwrap().token_is_valid(token));
    let Some(token) = token else {
        log::warn!("preview proxy authentication rejected");
        return stream.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"tcode-preview\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
    };
    let connect = request.method == "CONNECT";
    let target = Url::parse(&if connect {
        format!("https://{}/", request.path)
    } else {
        request.path.clone()
    })
    .map_err(io::Error::other)?;
    if target.host_str().is_none()
        || !target.username().is_empty()
        || target.password().is_some()
        || (connect && (target.path() != "/" || target.query().is_some()))
    {
        return Err(io::Error::other("invalid proxy target"));
    }
    let chunked = request.headers.get("transfer-encoding").map(String::as_str);
    let length = request
        .headers
        .get("content-length")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(io::Error::other)?
        .unwrap_or(0);
    if chunked.is_some_and(|value| !value.eq_ignore_ascii_case("chunked"))
        || (chunked.is_some() && request.headers.contains_key("content-length"))
    {
        return crate::wire::response(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            b"ambiguous request framing",
        )
        .await;
    }
    let upgrade = request
        .headers
        .get("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let endpoint = &target;
    let authority = authority(&target);
    let established = futures_lite::future::race(
        async {
            let socket = smol::net::TcpStream::connect((
                endpoint.host_str().unwrap().trim_matches(['[', ']']),
                endpoint.port_or_known_default().unwrap(),
            ))
            .await?;
            let mut socket = socket;
            if !connect {
                let path = &target[url::Position::BeforePath..url::Position::AfterQuery];
                let connection = if upgrade { "Upgrade" } else { "close" };
                let mut head = format!(
                    "{} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: {connection}\r\n",
                    request.method
                );
                let nominated = request
                    .headers
                    .get("connection")
                    .map(String::as_str)
                    .unwrap_or_default();
                for (key, value) in &request.headers {
                    if matches!(
                        key.as_str(),
                        "host"
                            | "proxy-authorization"
                            | "proxy-connection"
                            | "connection"
                            | "keep-alive"
                            | "trailer"
                    ) || (!matches!(
                        key.as_str(),
                        "content-length" | "transfer-encoding" | "upgrade"
                    ) && nominated
                        .split(',')
                        .any(|name| name.trim().eq_ignore_ascii_case(key)))
                    {
                        continue;
                    }
                    head.push_str(&format!("{key}: {value}\r\n"));
                }
                head.push_str("\r\n");
                socket.write_all(head.as_bytes()).await?;
            }
            Ok(socket)
        },
        async {
            smol::Timer::after(Duration::from_secs(10)).await;
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "proxy connect timed out",
            ))
        },
    )
    .await;
    let socket = match established {
        Ok(socket) => socket,
        Err(error) => {
            crate::wire::response(
                &mut stream,
                "502 Bad Gateway",
                "text/plain",
                b"host proxy connection failed",
            )
            .await?;
            return Err(error);
        }
    };
    log::info!(
        "preview proxy {} {authority}",
        if connect { "CONNECT" } else { "HTTP" }
    );
    if connect {
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    }
    let (mut reader, mut writer) = futures_util::io::AsyncReadExt::split(stream);
    let activity = Mutex::new(Instant::now());
    futures_lite::future::race(
        async {
            if connect || upgrade {
                futures_lite::future::try_zip(
                    async {
                        copy(&mut reader, socket.clone(), &activity).await?;
                        socket.shutdown(std::net::Shutdown::Write)
                    },
                    async {
                        copy(socket.clone(), &mut writer, &activity).await?;
                        writer.close().await
                    },
                )
                .await
                .map(|_| ())
            } else {
                futures_lite::future::race(
                    async {
                        upload(
                            &mut reader,
                            socket.clone(),
                            length,
                            chunked.is_some(),
                            &activity,
                        )
                        .await?;
                        // One HTTP request per authenticated connection; never forward
                        // a pipelined Proxy-Authorization header to the destination.
                        std::future::pending::<io::Result<()>>().await
                    },
                    copy(socket.clone(), &mut writer, &activity),
                )
                .await
            }
        },
        async {
            loop {
                futures_lite::future::race(
                    async {
                        smol::Timer::after(Duration::from_secs(5)).await;
                    },
                    async {
                        let _ = shared.shutdown.recv().await;
                    },
                )
                .await;
                if shared.shutdown.is_closed()
                    || activity.lock().unwrap().elapsed() >= Duration::from_secs(60)
                    || !shared.auth.lock().unwrap().token_is_valid(&token)
                {
                    return Ok(());
                }
            }
        },
    )
    .await
}

fn authority(url: &Url) -> String {
    format!(
        "{}:{}",
        url.host_str().unwrap(),
        url.port_or_known_default().unwrap()
    )
}

async fn copy(
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    activity: &Mutex<Instant>,
) -> io::Result<()> {
    let mut bytes = [0_u8; 16 * 1024];
    loop {
        let count = reader.read(&mut bytes).await?;
        if count == 0 {
            return Ok(());
        }
        *activity.lock().unwrap() = Instant::now();
        writer.write_all(&bytes[..count]).await?;
        *activity.lock().unwrap() = Instant::now();
    }
}

async fn upload(
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    length: u64,
    chunked: bool,
    activity: &Mutex<Instant>,
) -> io::Result<()> {
    if !chunked {
        return copy(reader.take(length), writer, activity).await;
    }
    loop {
        let line = line_read(&mut reader).await?;
        let size = u64::from_str_radix(line.trim().split(';').next().unwrap_or_default(), 16)
            .map_err(io::Error::other)?;
        if size == 0 {
            let mut total = 0;
            loop {
                let trailer = line_read(&mut reader).await?;
                total += trailer.len();
                if total > crate::wire::MAX_HEAD_BYTES {
                    return Err(io::Error::other("trailers too large"));
                }
                if trailer == "\r\n" {
                    break;
                }
            }
            writer.write_all(b"0\r\n\r\n").await?;
            return Ok(());
        }
        writer.write_all(format!("{size:x}\r\n").as_bytes()).await?;
        copy((&mut reader).take(size), &mut writer, activity).await?;
        let mut crlf = [0; 2];
        reader.read_exact(&mut crlf).await?;
        if crlf != *b"\r\n" {
            return Err(io::Error::other("invalid chunk terminator"));
        }
        writer.write_all(b"\r\n").await?;
    }
}

async fn line_read(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<String> {
    let mut bytes = Vec::new();
    while bytes.len() < crate::wire::MAX_HEAD_BYTES {
        let mut byte = [0];
        reader.read_exact(&mut byte).await?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n") {
            return String::from_utf8(bytes).map_err(io::Error::other);
        }
    }
    Err(io::Error::other("proxy body framing line too large"))
}
