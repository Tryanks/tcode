//! Browser-owned local endpoints that stand in for services on the paired
//! machine. Each accepted loopback connection becomes one Preview tunnel on
//! the attachment's Traverse connection: the machine dials the requested
//! `host:port` itself, so a dev server bound to its loopback is reachable and
//! nothing here rewrites page URLs.
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Shutdown},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_lite::{
    future,
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
};
use smol::net::{TcpListener, TcpStream};
use tcode_client::{
    host::{Tunnel, TunnelOpener},
    pairing::PairedHost,
};
use url::Url;

use crate::wire::{MAX_HEAD_BYTES, Request, read_request, response};

/// One attachment's paired machine and the tunnels to it. Updating it
/// retains browser loopback addresses and history and retires connections
/// made for the old address.
#[derive(Clone)]
pub struct PreviewEndpoint {
    current: Arc<Mutex<PreviewConnection>>,
}

#[derive(Clone)]
struct PreviewConnection {
    host: PairedHost,
    tunnels: Arc<dyn TunnelOpener>,
    retired: async_channel::Receiver<()>,
    _live: async_channel::Sender<()>,
}

impl PreviewConnection {
    fn new(host: &PairedHost, tunnels: Arc<dyn TunnelOpener>) -> Self {
        let (live, retired) = async_channel::bounded(1);
        Self {
            host: host.clone(),
            tunnels,
            retired,
            _live: live,
        }
    }

    /// Open a tunnel to `host:port`, wording a failure for the panel.
    async fn tunnel(&self, host: &str, port: u16) -> io::Result<Tunnel> {
        self.tunnels.open(host, port).await.map_err(|error| {
            let message = match error.kind() {
                io::ErrorKind::NotConnected => format!("Not connected to {}", self.host.name),
                io::ErrorKind::ConnectionRefused => format!(
                    "{} could not connect to {host}:{port}: {error}",
                    self.host.name
                ),
                io::ErrorKind::TimedOut => {
                    format!(
                        "Timed out connecting to {host}:{port} on {}",
                        self.host.name
                    )
                }
                _ => format!("Preview tunnel to {host}:{port} failed: {error}"),
            };
            io::Error::new(error.kind(), message)
        })
    }
}

/// The loopback bridge address a native browser engine is configured with.
/// The paired connection itself is the authority; the bridge asks for no
/// proxy credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEntry {
    pub origin: String,
}

impl PreviewEndpoint {
    pub fn new(host: &PairedHost, tunnels: Arc<dyn TunnelOpener>) -> Self {
        Self {
            current: Arc::new(Mutex::new(PreviewConnection::new(host, tunnels))),
        }
    }

    /// Called after the main connection has authenticated the machine again.
    pub fn update(&self, host: &PairedHost) -> Result<(), String> {
        let mut current = self.current.lock().unwrap();
        if host.host_id != current.host.host_id {
            return Err("Preview belongs to a different paired machine".into());
        }
        if current.host == *host {
            return Ok(());
        }
        // Closing a channel wakes every receiver; sending one message would
        // retire only one of several browser connections sharing this entry.
        current.retired.close();
        *current = PreviewConnection::new(host, current.tunnels.clone());
        Ok(())
    }

    fn connection(&self) -> PreviewConnection {
        self.current.lock().unwrap().clone()
    }
}

struct Route {
    remote: Url,
    port: u16,
    _listeners: Vec<smol::Task<()>>,
}

/// One browser's mapping identity and local socket lifetime. Public/LAN URLs
/// remain direct on the viewer. This is not a general browser-network proxy.
pub struct PreviewRoutes {
    host: PreviewEndpoint,
    routes: HashMap<String, Route>,
    error: Arc<Mutex<Option<(String, String)>>>,
    current: Option<String>,
    changes: async_channel::Receiver<()>,
    changed: async_channel::Sender<()>,
}

impl PreviewRoutes {
    pub fn new(host: PreviewEndpoint) -> Self {
        let (changed, changes) = async_channel::bounded(1);
        Self {
            changed,
            changes,
            host,
            routes: HashMap::new(),
            error: Arc::new(Mutex::new(None)),
            current: None,
        }
    }

    /// Explicit user/tool intent always names a remote endpoint, even when its
    /// port happens to equal an already allocated viewing port.
    pub fn navigate(&mut self, intent: &str) -> Result<String, String> {
        *self.error.lock().unwrap() = None;
        self.current = Some(intent.into());
        let result = self.map(intent);
        if let Err(error) = &result {
            *self.error.lock().unwrap() = Some((intent.into(), error.clone()));
        }
        result
    }

    /// Native reentry/history can already contain a mapped URL. Only this
    /// entry point recognizes viewing endpoints before resolving a new target.
    pub fn navigation(&mut self, actual: &str) -> Result<String, String> {
        if self.route_for_actual(actual).is_some() {
            *self.error.lock().unwrap() = None;
            self.current = Some(self.logical_url(actual));
            return Ok(actual.into());
        }
        if actual == "about:blank" {
            return Ok(actual.into());
        }
        self.navigate(actual)
    }

    pub fn current_url(&self) -> Option<&str> {
        self.current.as_deref()
    }

    pub fn logical_url(&self, actual: &str) -> String {
        let Some(route) = self.route_for_actual(actual) else {
            return actual.into();
        };
        let mut url = Url::parse(actual).unwrap();
        // A TCP route can be reused under another scheme, whose default port
        // need not match the scheme that first allocated it.
        let _ = url.set_port(route.remote.port_or_known_default());
        url.into()
    }

    /// No allocation: external browsers may use only a live route. Its URL
    /// stops working when this browser slot is closed.
    pub fn external_url(&self, logical: &str) -> Option<String> {
        let mut url = Url::parse(logical).ok()?;
        if !loopback(&url) {
            return Some(logical.into());
        }
        let route = self.routes.get(&authority(&url)?)?;
        url.set_port(Some(route.port)).ok()?;
        Some(url.into())
    }

    fn route_for_actual(&self, actual: &str) -> Option<&Route> {
        let url = Url::parse(actual).ok()?;
        self.routes.values().find(|route| {
            url.host() == route.remote.host() && url.port_or_known_default() == Some(route.port)
        })
    }

    fn map(&mut self, intent: &str) -> Result<String, String> {
        let mut url = Url::parse(intent).map_err(|_| "Invalid preview URL")?;
        if !loopback(&url) {
            return Ok(intent.into());
        }
        let destination = authority(&url).ok_or("Missing preview port")?;
        let port = if let Some(route) = self.routes.get(&destination) {
            route.port
        } else {
            let listeners =
                bind_loopback(&url).map_err(|_| "Could not allocate a local preview listener")?;
            let port = listeners[0]
                .local_addr()
                .map_err(|_| "Could not read preview port")?
                .port();
            let mut tasks = Vec::new();
            for listener in listeners {
                let listener = TcpListener::try_from(listener)
                    .map_err(|_| "Could not start preview listener")?;
                let host = self.host.clone();
                let destination = destination.clone();
                let error = self.error.clone();
                let changed = self.changed.clone();
                tasks.push(smol::spawn(async move {
                    let mut connections = Vec::new();
                    while let Ok((socket, _)) = listener.accept().await {
                        connections.retain(|task: &smol::Task<()>| !task.is_finished());
                        let connection = host.connection();
                        let destination = destination.clone();
                        let error = error.clone();
                        let changed = changed.clone();
                        connections.push(smol::spawn(async move {
                            future::race(
                                async {
                                    if let Err(failure) =
                                        forward(socket, &connection, &destination).await
                                    {
                                        *error.lock().unwrap() =
                                            Some((destination, failure.to_string()));
                                        let _ = changed.try_send(());
                                    }
                                },
                                async {
                                    let _ = connection.retired.recv().await;
                                },
                            )
                            .await;
                        }));
                    }
                }));
            }
            self.routes.insert(
                destination,
                Route {
                    remote: url.clone(),
                    port,
                    _listeners: tasks,
                },
            );
            port
        };
        url.set_port(Some(port))
            .map_err(|_| "Invalid preview port")?;
        Ok(url.into())
    }

    pub fn error(&self) -> Option<String> {
        let error = self.error.lock().unwrap();
        let (destination, message) = error.as_ref()?;
        let current = self.current.as_deref()?;
        (destination == current
            || Url::parse(current)
                .ok()
                .and_then(|url| authority(&url))
                .as_ref()
                == Some(destination))
        .then(|| message.clone())
    }

    /// The owning UI consumes wakeups; error details remain in this owner.
    pub fn changes(&self) -> async_channel::Receiver<()> {
        self.changes.clone()
    }
}

fn authority(url: &Url) -> Option<String> {
    Some(format!(
        "{}:{}",
        url.host_str()?,
        url.port_or_known_default()?
    ))
}

fn loopback(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

fn bind_loopback(url: &Url) -> io::Result<Vec<std::net::TcpListener>> {
    let host = url.host_str().unwrap();
    if host != "localhost" {
        return Ok(vec![std::net::TcpListener::bind((
            host.trim_matches(['[', ']']),
            0,
        ))?]);
    }
    // WebKit may resolve localhost to either family. Reserve both at one
    // kernel-selected port; retry only a collision on the second reservation.
    for _ in 0..8 {
        let v4 = std::net::TcpListener::bind("127.0.0.1:0")?;
        match std::net::TcpListener::bind(("::1", v4.local_addr()?.port())) {
            Ok(v6) => return Ok(vec![v4, v6]),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "Could not reserve localhost preview port",
    ))
}

/// Carry one mapped browser connection to `destination` (`host:port`, the
/// host possibly bracketed) over a tunnel. Bytes are copied verbatim in both
/// directions with half-close preserved; once the tunnel is open, browser
/// cancellation and peer shutdown are WebKit's to report, not a route error.
async fn forward(
    socket: TcpStream,
    connection: &PreviewConnection,
    destination: &str,
) -> io::Result<()> {
    let (host, port) = split_authority(destination)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid preview port"))?;
    let tunnel = connection.tunnel(host, port).await?;
    let _ = pipe(socket, tunnel).await;
    Ok(())
}

fn split_authority(authority: &str) -> Option<(&str, u16)> {
    let (host, port) = authority.rsplit_once(':')?;
    Some((host.trim_matches(['[', ']']), port.parse().ok()?))
}

/// Copy bytes both ways until both directions have ended. The browser's
/// write shutdown finishes the tunnel; the tunnel's end shuts down the
/// browser socket's write half.
async fn pipe(socket: TcpStream, tunnel: Tunnel) -> io::Result<()> {
    let Tunnel {
        mut read,
        mut write,
    } = tunnel;
    future::try_zip(
        async {
            futures_lite::io::copy(&mut socket.clone(), &mut write).await?;
            write.close().await
        },
        async {
            futures_lite::io::copy(&mut read, &mut socket.clone()).await?;
            socket.shutdown(Shutdown::Write)
        },
    )
    .await
    .map(|_| ())
}

/// Attachment-owned loopback HTTP proxy for browser engines that need an
/// OS proxy address. `CONNECT host:port` and absolute-form requests each
/// open one tunnel to `host:port`; HTTPS stays an opaque byte stream. One
/// request per connection: pipelined bytes never reach the machine.
/// Dropping the proxy cancels its listener and all accepted connections.
pub struct NativeProxy {
    origin: String,
    _listener: smol::Task<()>,
}

impl NativeProxy {
    pub fn new(host: PreviewEndpoint) -> Result<Self, String> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let origin = format!(
            "http://{}",
            listener.local_addr().map_err(|e| e.to_string())?
        );
        let listener = TcpListener::try_from(listener).map_err(|e| e.to_string())?;
        let task = smol::spawn(async move {
            let mut connections = Vec::new();
            while let Ok((browser, _)) = listener.accept().await {
                connections.retain(|task: &smol::Task<()>| !task.is_finished());
                let connection = host.connection();
                connections.push(smol::spawn(async move {
                    future::race(
                        async {
                            if let Err(error) = proxy(browser, &connection).await {
                                log::debug!("preview proxy connection ended: {error}");
                            }
                        },
                        async {
                            let _ = connection.retired.recv().await;
                        },
                    )
                    .await;
                }));
            }
        });
        Ok(Self {
            origin,
            _listener: task,
        })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// What the browser engine is configured with.
    pub fn entry(&self) -> ProxyEntry {
        ProxyEntry {
            origin: self.origin.clone(),
        }
    }
}

/// Hop-by-hop headers the proxy consumes rather than forwards.
fn hop_by_hop(name: &str, nominated: &str) -> bool {
    matches!(
        name,
        "host"
            | "proxy-authorization"
            | "proxy-connection"
            | "connection"
            | "keep-alive"
            | "trailer"
    ) || (!matches!(name, "content-length" | "transfer-encoding" | "upgrade")
        && nominated
            .split(',')
            .any(|nominee| nominee.trim().eq_ignore_ascii_case(name)))
}

async fn proxy(mut browser: TcpStream, connection: &PreviewConnection) -> io::Result<()> {
    let request = match read_request(&mut browser).await {
        Ok(request) => request,
        Err(error) => {
            response(
                &mut browser,
                "400 Bad Request",
                "text/plain",
                b"malformed request",
            )
            .await?;
            return Err(error);
        }
    };
    let connect = request.method == "CONNECT";
    let target = if connect {
        Url::parse(&format!("https://{}/", request.path)).ok()
    } else if request.path.starts_with("http://") {
        Url::parse(&request.path).ok()
    } else {
        None
    };
    let target = target.filter(|target| {
        target.host_str().is_some()
            && target.username().is_empty()
            && target.password().is_none()
            && (!connect || (target.path() == "/" && target.query().is_none()))
    });
    let Some(target) = target else {
        response(
            &mut browser,
            "400 Bad Request",
            "text/plain",
            b"expected CONNECT host:port or an absolute-form request",
        )
        .await?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid proxy target",
        ));
    };
    let chunked = request.headers.get("transfer-encoding").map(String::as_str);
    let length = request
        .headers
        .get("content-length")
        .map(|value| value.parse::<u64>())
        .transpose()
        .ok()
        .flatten();
    if chunked.is_some_and(|value| !value.eq_ignore_ascii_case("chunked"))
        || (chunked.is_some() && request.headers.contains_key("content-length"))
        || (request.headers.contains_key("content-length") && length.is_none())
    {
        response(
            &mut browser,
            "400 Bad Request",
            "text/plain",
            b"ambiguous request framing",
        )
        .await?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ambiguous request framing",
        ));
    }
    let upgrade = request
        .headers
        .get("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let host = target.host_str().unwrap().trim_matches(['[', ']']);
    let port = target.port_or_known_default().unwrap();
    let tunnel = match connection.tunnel(host, port).await {
        Ok(tunnel) => tunnel,
        Err(error) => {
            response(
                &mut browser,
                "502 Bad Gateway",
                "text/plain",
                error.to_string().as_bytes(),
            )
            .await?;
            return Err(error);
        }
    };
    let Tunnel {
        mut read,
        mut write,
    } = tunnel;
    if connect {
        browser
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    } else {
        write
            .write_all(origin_form(&request, &target, upgrade).as_bytes())
            .await?;
    }
    if connect || upgrade {
        return future::try_zip(
            async {
                futures_lite::io::copy(&mut browser.clone(), &mut write).await?;
                write.close().await
            },
            async {
                futures_lite::io::copy(&mut read, &mut browser.clone()).await?;
                browser.shutdown(Shutdown::Write)
            },
        )
        .await
        .map(|_| ());
    }
    future::race(
        async {
            upload(
                &mut browser.clone(),
                &mut write,
                length.unwrap_or(0),
                chunked.is_some(),
            )
            .await?;
            // One request per connection: whatever the browser pipelines
            // after the body is never forwarded.
            std::future::pending::<io::Result<()>>().await
        },
        async {
            futures_lite::io::copy(&mut read, &mut browser.clone()).await?;
            Ok(())
        },
    )
    .await?;
    // Finish with FIN, not RST: dropping the socket while pipelined bytes
    // sit unread resets the connection, and Windows discards the
    // already-sent response on reset. Discard what the browser sent until
    // it closes.
    browser.shutdown(Shutdown::Write)?;
    future::race(
        async {
            let mut sink = [0; 1024];
            while browser.read(&mut sink).await? != 0 {}
            Ok(())
        },
        async {
            smol::Timer::after(Duration::from_secs(1)).await;
            Ok(())
        },
    )
    .await
}

/// The request head as the origin sees it: origin-form target, its own
/// `Host`, one request per connection, and no proxy hop-by-hop headers.
fn origin_form(request: &Request, target: &Url, upgrade: bool) -> String {
    let path = &target[url::Position::BeforePath..url::Position::AfterQuery];
    let authority = format!(
        "{}:{}",
        target.host_str().unwrap(),
        target.port_or_known_default().unwrap()
    );
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
    let mut names: Vec<&String> = request.headers.keys().collect();
    names.sort();
    for name in names {
        if hop_by_hop(name, nominated) {
            continue;
        }
        head.push_str(&format!("{name}: {}\r\n", request.headers[name]));
    }
    head.push_str("\r\n");
    head
}

/// Forward exactly the declared body: `length` bytes, or chunks re-framed
/// so trailers and anything after the last chunk stay behind.
async fn upload(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    length: u64,
    chunked: bool,
) -> io::Result<()> {
    if !chunked {
        futures_lite::io::copy(reader.take(length), writer).await?;
        return Ok(());
    }
    loop {
        let line = line_read(reader).await?;
        let size = u64::from_str_radix(line.trim().split(';').next().unwrap_or_default(), 16)
            .map_err(io::Error::other)?;
        if size == 0 {
            let mut total = 0;
            loop {
                let trailer = line_read(reader).await?;
                total += trailer.len();
                if total > MAX_HEAD_BYTES {
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
        futures_lite::io::copy(reader.take(size), &mut *writer).await?;
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
    while bytes.len() < MAX_HEAD_BYTES {
        let mut byte = [0];
        reader.read_exact(&mut byte).await?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n") {
            return String::from_utf8(bytes).map_err(io::Error::other);
        }
    }
    Err(io::Error::other("proxy body framing line too large"))
}
