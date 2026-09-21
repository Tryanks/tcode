//! Browser-owned local endpoints that stand in for services on the paired
//! machine. The loopback listeners, route mapping and history are kept; the
//! tunnel to the machine is not: the HTTP forward proxy it used retired with
//! the plaintext transport.
//!
//! TODO(traverse): Traverse preview streams — carry each mapped connection
//! over a `connect` bi stream on the machine's `tcode/1` connection.
use std::{
    collections::HashMap,
    io,
    net::IpAddr,
    sync::{Arc, Mutex},
};

use smol::net::{TcpListener, TcpStream};
use tcode_client::pairing::PairedHost;
use url::Url;

/// One attachment's paired machine. Updating it retains browser loopback
/// addresses and history and retires connections made for the old address.
#[derive(Clone)]
pub struct PreviewEndpoint {
    current: Arc<Mutex<PreviewConnection>>,
}

#[derive(Clone)]
struct PreviewConnection {
    host: PairedHost,
    retired: async_channel::Receiver<()>,
    _live: async_channel::Sender<()>,
}

impl PreviewConnection {
    fn new(host: &PairedHost) -> Self {
        let (live, retired) = async_channel::bounded(1);
        Self {
            host: host.clone(),
            retired,
            _live: live,
        }
    }
}

/// The loopback bridge address and credential a native browser engine is
/// configured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEntry {
    pub origin: String,
    pub token: String,
}

impl PreviewEndpoint {
    pub fn new(host: &PairedHost) -> Result<Self, String> {
        Ok(Self {
            current: Arc::new(Mutex::new(PreviewConnection::new(host))),
        })
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
        *current = PreviewConnection::new(host);
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
                            if let Err(failure) =
                                forward(socket, &connection.host, &destination).await
                            {
                                *error.lock().unwrap() = Some((destination, failure.to_string()));
                                let _ = changed.try_send(());
                            }
                            let _ = connection.retired.recv().await;
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

/// Where a mapped connection would go. Until Traverse preview streams exist
/// every attempt fails and the panel shows why.
pub(crate) async fn forward(
    socket: TcpStream,
    host: &PairedHost,
    destination: &str,
) -> io::Result<()> {
    drop(socket);
    Err(io::Error::other(format!(
        "Remote preview of {destination} on {} is not available over Traverse yet",
        host.name
    )))
}

/// Attachment-owned bridge for native browser engines that require an OS proxy
/// address. The browser's existing proxy protocol and authentication pass through
/// unchanged to the same paired host entry as pairing and the main WebSocket.
/// Dropping the bridge cancels its listener and all accepted connections.
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
            while let Ok((browser, _)) = listener.accept().await {
                let connection = host.connection();
                // Refuse until Traverse preview streams carry the request.
                let _ = forward(browser, &connection.host, "proxy").await;
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

    /// What the browser engine is configured with. There is no credential
    /// until the bridge is carried over Traverse.
    pub fn entry(&self) -> ProxyEntry {
        ProxyEntry {
            origin: self.origin.clone(),
            token: String::new(),
        }
    }
}
