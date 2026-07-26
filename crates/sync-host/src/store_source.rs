//! A [`SessionSource`] backed by the on-disk session store.
//!
//! Reads do not go through the running app. `SessionStore` is cheap to clone
//! and safe to share, and the event log is the authoritative record anyway, so
//! the server thread reads it directly — no hop onto the UI thread, no request
//! broker, and nothing that stops working when the UI is busy. It is also what
//! lets a headless server reuse this unchanged: there is no app to ask.
//!
//! Only two things cannot come from disk, and each has its own channel:
//!
//! * **Commands** must reach the live provider actor, which the app owns. They
//!   go out on [`CommandSink`].
//! * **Liveness** — is a turn running, is someone waiting on an approval — is
//!   in-memory state the log does not describe. The app publishes it into
//!   [`LiveSessions`], which reads see immediately.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sync_protocol::{CommandRejection, SeqEvent, SessionCommand, SessionSummary};
use tcode_services::store::SessionStore;

use crate::SessionSource;

/// A command on its way from a remote client to the app that owns the session.
#[derive(Debug, Clone)]
pub struct CommandRequest {
    pub session_id: String,
    pub command: SessionCommand,
}

/// Where commands go. The app drains the receiving end.
pub type CommandSink = async_channel::Sender<CommandRequest>;

/// Per-session state that exists only in the running app.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveFlags {
    /// A turn is in flight.
    pub working: bool,
    /// The session is blocked on a human. The state a phone exists to serve.
    pub awaiting_approval: bool,
}

/// Liveness published by the app and read by the server thread.
///
/// Absent entries are not an error: a session with no live provider is simply
/// idle, which is exactly what `LiveFlags::default()` says.
#[derive(Clone, Default)]
pub struct LiveSessions(Arc<RwLock<HashMap<String, LiveFlags>>>);

impl LiveSessions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, session_id: &str) -> LiveFlags {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .copied()
            .unwrap_or_default()
    }

    pub fn set(&self, session_id: &str, flags: LiveFlags) {
        let mut map = self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if flags == LiveFlags::default() {
            // Idle is the default, so storing it would only grow the map for
            // every session ever opened.
            map.remove(session_id);
        } else {
            map.insert(session_id.to_owned(), flags);
        }
    }

    /// Forget a session — it ended, was archived, or was deleted.
    pub fn clear(&self, session_id: &str) {
        self.0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }
}

pub struct StoreSource {
    store: SessionStore,
    live: LiveSessions,
    commands: CommandSink,
}

impl StoreSource {
    pub fn new(store: SessionStore, live: LiveSessions, commands: CommandSink) -> Self {
        Self {
            store,
            live,
            commands,
        }
    }
}

impl SessionSource for StoreSource {
    fn list_sessions(&self) -> Vec<SessionSummary> {
        self.store
            .load_index()
            .into_iter()
            // Archived threads are hidden in the desktop sidebar; a remote
            // client should not be shown what the local UI has put away.
            .filter(|meta| meta.archived_at.is_none())
            .map(|meta| {
                let live = self.live.get(&meta.id);
                SessionSummary {
                    // `latest_seq` is deliberately not computed here. Answering
                    // it means reading a log, and a host can hold thousands —
                    // this machine has 1084. The client learns the real head
                    // when it subscribes.
                    latest_seq: None,
                    session_id: meta.id,
                    title: meta.title,
                    provider: meta.provider,
                    model: meta.model,
                    // The directory name, not the path: a host-absolute path
                    // means nothing on a phone, and leaks the host's layout to
                    // every client that can list sessions.
                    project: meta
                        .cwd
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned()),
                    updated_at: meta.updated_at,
                    working: live.working,
                    awaiting_approval: live.awaiting_approval,
                }
            })
            .collect()
    }

    fn read_events(&self, session_id: &str, from_seq: Option<u64>, limit: usize) -> Vec<SeqEvent> {
        let after = from_seq.unwrap_or(0);
        self.store
            .read_events(session_id)
            .into_iter()
            .filter_map(|stored| {
                // `read_events` assigns a seq to every line it returns, so the
                // filter_map drops nothing in practice. Skipping rather than
                // defaulting keeps that an invariant instead of a silent
                // renumbering if it ever stops holding.
                let seq = stored.seq?;
                (seq > after).then(|| SeqEvent {
                    seq,
                    ts: stored.ts.unwrap_or_default(),
                    event: stored.event,
                })
            })
            .take(limit)
            .collect()
    }

    fn session_exists(&self, session_id: &str) -> bool {
        self.store
            .load_index()
            .iter()
            .any(|meta| meta.id == session_id)
    }

    fn send_command(
        &self,
        session_id: &str,
        command: SessionCommand,
    ) -> Result<(), CommandRejection> {
        // Non-blocking on purpose: this runs on the server's thread, and
        // blocking it on the app would let one busy session stall every other
        // client. Whether the session is actually live is the app's answer to
        // give, and it arrives out of band.
        self.commands
            .try_send(CommandRequest {
                session_id: session_id.to_owned(),
                command,
            })
            .map_err(|err| {
                log::warn!("dropping command for {session_id}: {err}");
                CommandRejection::SessionNotLive
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{AgentEvent, ProviderKind};
    use std::path::PathBuf;
    use tcode_core::project::SessionMeta;

    fn temp_store() -> SessionStore {
        let mut root = std::env::temp_dir();
        root.push(format!("tcode-sync-host-test-{}", uuid_like()));
        SessionStore::open_at(root).expect("temp store")
    }

    /// The crate has no uuid dependency and does not need one for a directory
    /// name; process id plus a counter is unique enough within one test run.
    fn uuid_like() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn event(n: u64) -> AgentEvent {
        AgentEvent::TurnAccepted { delivery_id: n }
    }

    fn seeded(store: &SessionStore, cwd: &str, len: u64) -> String {
        let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from(cwd), None);
        store.upsert_meta(&meta).expect("meta");
        for n in 1..=len {
            store
                .append_event(&meta.id, 1_000 + n, &event(n))
                .expect("append");
        }
        meta.id
    }

    fn source(store: SessionStore) -> (StoreSource, async_channel::Receiver<CommandRequest>) {
        let (tx, rx) = async_channel::bounded(8);
        (StoreSource::new(store, LiveSessions::new(), tx), rx)
    }

    #[test]
    fn events_are_read_from_the_cursor_with_their_sequence() {
        let store = temp_store();
        let id = seeded(&store, "/work/alpha", 5);
        let (source, _rx) = source(store.clone());

        let all = source.read_events(&id, None, 100);
        assert_eq!(
            all.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(all[0].ts, 1_001);

        let tail = source.read_events(&id, Some(3), 100);
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);

        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn the_limit_bounds_a_batch() {
        let store = temp_store();
        let id = seeded(&store, "/work/alpha", 10);
        let (source, _rx) = source(store.clone());

        let batch = source.read_events(&id, None, 4);
        assert_eq!(
            batch.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        let _ = std::fs::remove_dir_all(store.root());
    }

    /// A host-absolute working directory must not reach a client: it is
    /// meaningless on a phone and describes the host's filesystem layout.
    #[test]
    fn a_summary_carries_the_project_name_not_the_host_path() {
        let store = temp_store();
        seeded(&store, "/Users/someone/work/alpha", 1);
        let (source, _rx) = source(store.clone());

        let sessions = source.list_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].project.as_deref(), Some("alpha"));
        assert_eq!(sessions[0].latest_seq, None);

        let encoded = serde_json::to_string(&sessions[0]).expect("summary encodes");
        assert!(
            !encoded.contains("/Users/someone"),
            "host path leaked into a session summary: {encoded}"
        );
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn archived_sessions_are_not_offered_to_clients() {
        let store = temp_store();
        let id = seeded(&store, "/work/alpha", 1);
        let mut meta = store
            .load_index()
            .into_iter()
            .find(|m| m.id == id)
            .expect("seeded");
        meta.archived_at = Some(42);
        store.upsert_meta(&meta).expect("archive");

        let (source, _rx) = source(store.clone());
        assert!(source.list_sessions().is_empty());
        // Still readable by id: archiving hides a thread, it does not revoke a
        // client already streaming it.
        assert!(source.session_exists(&id));
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn liveness_comes_from_the_app_not_the_log() {
        let store = temp_store();
        let id = seeded(&store, "/work/alpha", 1);
        let (tx, _rx) = async_channel::bounded(8);
        let live = LiveSessions::new();
        let source = StoreSource::new(store.clone(), live.clone(), tx);

        assert!(!source.list_sessions()[0].working);

        live.set(
            &id,
            LiveFlags {
                working: true,
                awaiting_approval: true,
            },
        );
        let summary = &source.list_sessions()[0];
        assert!(summary.working);
        assert!(summary.awaiting_approval);

        live.clear(&id);
        assert!(!source.list_sessions()[0].awaiting_approval);
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn a_command_is_handed_to_the_app() {
        let store = temp_store();
        let id = seeded(&store, "/work/alpha", 1);
        let (source, rx) = source(store.clone());

        source
            .send_command(&id, SessionCommand::Interrupt)
            .expect("queued");
        let request = rx.try_recv().expect("the app receives it");
        assert_eq!(request.session_id, id);
        assert_eq!(request.command, SessionCommand::Interrupt);
        let _ = std::fs::remove_dir_all(store.root());
    }

    /// Sending must never block the server thread: one wedged consumer would
    /// otherwise stall every connected client.
    #[test]
    fn a_full_command_channel_is_rejected_rather_than_waited_on() {
        let store = temp_store();
        let id = seeded(&store, "/work/alpha", 1);
        let (tx, _rx) = async_channel::bounded(1);
        let source = StoreSource::new(store.clone(), LiveSessions::new(), tx);

        source
            .send_command(&id, SessionCommand::Interrupt)
            .expect("first fits");
        assert_eq!(
            source.send_command(&id, SessionCommand::Interrupt),
            Err(CommandRejection::SessionNotLive)
        );
        let _ = std::fs::remove_dir_all(store.root());
    }
}
