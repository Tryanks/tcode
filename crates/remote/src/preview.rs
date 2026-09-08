//! Browser-owned local endpoints for the paired host's existing CONNECT service.
//! Only the CONNECT handshake is interpreted; browser traffic is copied verbatim.
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Shutdown},
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_lite::{
    future,
    io::{AsyncReadExt as _, AsyncWriteExt as _},
};
use smol::net::{TcpListener, TcpStream};
use tcode_client::pairing::PairedHost;
use url::Url;

struct Route {
    remote: Url,
    port: u16,
    _listeners: Vec<smol::Task<()>>,
}

/// One browser's mapping identity and local socket lifetime. Public/LAN URLs
/// remain direct on the viewer. This is not a general browser-network proxy.
pub struct PreviewRoutes {
    host: PairedHost,
    routes: HashMap<String, Route>,
    error: Arc<Mutex<Option<(String, String)>>>,
    current: Option<String>,
    changes: async_channel::Receiver<()>,
    changed: async_channel::Sender<()>,
}

impl PreviewRoutes {
    pub fn new(host: PairedHost) -> Self {
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
        let _ = url.set_port(route.remote.port());
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
        let proxy = Url::parse(&self.host.origin).map_err(|_| "Invalid paired host origin")?;
        if proxy.scheme() != "http" {
            return Err("Remote preview requires a direct HTTP paired host connection".into());
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
                        let host = host.clone();
                        let destination = destination.clone();
                        let error = error.clone();
                        let changed = changed.clone();
                        connections.push(smol::spawn(async move {
                            let keepalive = socket.clone();
                            if let Err(failure) = forward(socket, &host, &destination).await {
                                *error.lock().unwrap() = Some((destination, failure.to_string()));
                                let _ = changed.try_send(());
                            }
                            drop(keepalive);
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

async fn forward(socket: TcpStream, host: &PairedHost, destination: &str) -> io::Result<()> {
    let mut remote = future::race(async {
        let proxy = Url::parse(&host.origin).map_err(io::Error::other)?;
        let mut remote = TcpStream::connect((proxy.host_str().ok_or_else(|| io::Error::other("Missing paired host"))?.trim_matches(['[', ']']), proxy.port_or_known_default().unwrap())).await.map_err(|_| io::Error::other("Cannot connect to the paired host preview service"))?;
        let auth = STANDARD.encode(format!("tcode:{}", host.token));
        remote.write_all(format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\nProxy-Authorization: Basic {auth}\r\n\r\n").as_bytes()).await?;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") && head.len() < 16384 {
            let mut byte = [0];
            remote.read_exact(&mut byte).await?;
            head.push(byte[0]);
        }
        if !head.starts_with(b"HTTP/1.1 200 ") || !head.ends_with(b"\r\n\r\n") {
            return Err(io::Error::other(if head.starts_with(b"HTTP/1.1 407 ") { "Paired host rejected preview authentication; reconnect or pair again" } else { "Paired host could not connect to the remote preview destination" }));
        }
        Ok(remote)
    }, async {
        smol::Timer::after(Duration::from_secs(12)).await;
        Err(io::Error::other("Timed out connecting to the remote preview destination"))
    }).await?;
    let mut browser = socket.clone();
    let mut outbound = remote.clone();
    // Once CONNECT succeeds, normal browser cancellation and peer shutdown
    // can return NotConnected/BrokenPipe. WebKit owns page-load errors; a
    // closed keepalive socket must not overwrite a successfully loaded page.
    let _ = future::try_zip(
        async {
            futures_lite::io::copy(&mut browser, &mut outbound).await?;
            outbound.shutdown(Shutdown::Write)
        },
        async {
            futures_lite::io::copy(&mut remote, &mut socket.clone()).await?;
            socket.shutdown(Shutdown::Write)
        },
    )
    .await;
    Ok(())
}
