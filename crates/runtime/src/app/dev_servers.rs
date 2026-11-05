//! The dev servers a thread's preview offers to open: the host machine's TCP
//! listeners held by the user's processes, as every client sees them.
//!
//! No operating system tells when a listener opens or closes, so the host
//! reads its listeners again every [`REFRESH`] — and only while some client
//! subscribes to a [`Topic::DevServers`], which a client does while it shows
//! the picks. With no subscriber left, the next read finds that out and stops.

use super::*;
use std::cmp::Reverse;
use tcode_protocol::DevServer;
use tcode_services::listening::{self, Listener};

const REFRESH: Duration = Duration::from_secs(2);

/// How many picks a thread is offered.
const SHOWN: usize = 8;

/// Ports dev servers choose by default (Vite, Astro, Angular, Next/CRA,
/// Rails, Django, Flask, http.server, Jupyter, ...).
const COMMON_PORTS: &[u16] = &[
    3000, 3001, 4000, 4173, 4200, 4321, 5000, 5173, 5174, 8000, 8080, 8081, 8888, 9000,
];

/// Where operating systems hand out ports to whoever asks for any: a
/// listener here is someone's helper rather than a server the user started,
/// unless it runs in the thread's directory.
const EPHEMERAL: u16 = 49152;

#[derive(Default)]
pub(super) struct DevServerReads {
    /// The last read, while the reads run.
    listeners: Option<Vec<Listener>>,
    /// Each watched thread's directory as the system names it, symlinks
    /// resolved as in a process's working directory, and the picks it was
    /// last sent.
    threads: HashMap<String, (PathBuf, Vec<DevServer>)>,
    running: bool,
}

impl AppState {
    pub(super) fn dev_servers_subscribed(&mut self, cx: &mut HostCx) {
        if std::mem::replace(&mut self.dev_servers.running, true) {
            return;
        }
        let host = cx.clone();
        cx.spawn_detached(async move {
            while let Ok(Some(threads)) = host
                .enqueue_and_wait(|state, _| state.dev_server_threads())
                .await
            {
                let (listeners, threads) = host
                    .unblock(move || {
                        let listeners = listening::listeners().unwrap_or_else(|error| {
                            log::warn!("could not read the machine's TCP listeners: {error}");
                            Vec::new()
                        });
                        let threads: Vec<(String, PathBuf)> = threads
                            .into_iter()
                            .map(|(id, dir)| (id, fs::canonicalize(&dir).unwrap_or(dir)))
                            .collect();
                        (listeners, threads)
                    })
                    .await;
                host.enqueue(move |state, cx| state.read_dev_servers(listeners, threads, cx));
                smol::Timer::after(REFRESH).await;
            }
        });
    }

    /// The watched threads and their directories, or none once nobody
    /// watches, which ends the reads.
    fn dev_server_threads(&mut self) -> Option<Vec<(String, PathBuf)>> {
        let watched: Vec<&String> = self
            .subscriptions
            .iter()
            .filter_map(|topic| match topic {
                Topic::DevServers { session_id } => Some(session_id),
                _ => None,
            })
            .collect();
        if watched.is_empty() {
            self.dev_servers = DevServerReads::default();
            return None;
        }
        Some(
            watched
                .into_iter()
                .filter_map(|id| Some((id.clone(), self.find_meta(id)?.cwd)))
                .collect(),
        )
    }

    fn read_dev_servers(
        &mut self,
        mut listeners: Vec<Listener>,
        threads: Vec<(String, PathBuf)>,
        cx: &mut HostCx,
    ) {
        if !self.dev_servers.running {
            return;
        }
        // Tcode's own: its MCP host, Preview tunnels, the browser listener.
        let own = std::process::id();
        listeners.retain(|listener| listener.pid != own);
        self.dev_servers.listeners = Some(listeners);
        let mut kept = HashMap::new();
        for (session_id, dir) in threads {
            let topic = Topic::DevServers {
                session_id: session_id.clone(),
            };
            if !self.subscriptions.contains(&topic) {
                continue;
            }
            let servers = self.rank_dev_servers(Some(&dir));
            // A thread new to the reads was answered from another
            // directory's view, or not at all.
            let sent = self.dev_servers.threads.remove(&session_id);
            if sent.is_none_or(|(_, sent)| sent != servers) {
                cx.emit(HostEvent::Domain(EventEnvelope {
                    request_id: None,
                    topic,
                    event: ServerEvent::DevServersReplaced(servers.clone()),
                }));
            }
            kept.insert(session_id, (dir, servers));
        }
        self.dev_servers.threads = kept;
    }

    /// The answer to subscribing: the picks of the last read, or none before
    /// the first, which the read then sends.
    pub(super) fn dev_servers_snapshot(&self, session_id: &str) -> Option<Vec<DevServer>> {
        self.dev_servers.listeners.as_ref()?;
        let dir = match self.dev_servers.threads.get(session_id) {
            Some((dir, _)) => Some(dir.clone()),
            None => self.find_meta(session_id).map(|meta| meta.cwd),
        };
        Some(self.rank_dev_servers(dir.as_deref()))
    }

    /// A thread's picks: what runs in its directory first, latest started
    /// first; then what listens on a port dev servers choose; then the rest,
    /// latest started first.
    fn rank_dev_servers(&self, thread: Option<&Path>) -> Vec<DevServer> {
        let listeners = self.dev_servers.listeners.as_deref().unwrap_or_default();
        let mut picks: Vec<(&Listener, Option<String>)> = listeners
            .iter()
            .map(|listener| {
                let inside = thread.and_then(|thread| {
                    let relative = listener.cwd.as_deref()?.strip_prefix(thread).ok()?;
                    Some(
                        relative
                            .components()
                            .map(|part| part.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/"),
                    )
                });
                (listener, inside)
            })
            .filter(|(listener, inside)| inside.is_some() || listener.port < EPHEMERAL)
            .collect();
        picks.sort_by_key(|(listener, inside)| {
            let outside = inside.is_none();
            (
                outside,
                outside && !COMMON_PORTS.contains(&listener.port),
                Reverse(listener.started),
                listener.port,
            )
        });
        let mut ports = HashSet::new();
        picks
            .into_iter()
            .filter(|(listener, _)| ports.insert(listener.port))
            .take(SHOWN)
            .map(|(listener, cwd)| DevServer {
                port: listener.port,
                pid: listener.pid,
                process: listener.process.clone(),
                cwd,
            })
            .collect()
    }
}
