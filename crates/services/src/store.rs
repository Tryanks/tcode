//! On-disk persistence for tcode sessions.
//!
//! Layout (under the platform data dir, e.g. `~/Library/Application Support/tcode/`):
//!   * `tcode.redb` — the [`SessionIndex`] of projects and sessions, opened by
//!     the host alone. It replaced `sessions.json`, which its first open
//!     imports and renames to `sessions.json.migrated`.
//!   * `<id>.jsonl` — append-only `{ ts, event }` records. A log that
//!     [`SessionStore::compact_log`] rewrote begins with a layout header line;
//!     a log without one is layout epoch 0.
//!   * `<id>.jsonl.orig` — the log as it was before its first rewrite.
//!
//! Replay accepts timestamped records and legacy bare [`AgentEvent`] lines,
//! then folds [`StoredEvent`]s into a [`tcode_core::session::Timeline`].

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use agent::{AgentEvent, ModelSpec, ProviderCommand, ProviderKind};
use serde::{Deserialize, Serialize};

use tcode_core::session::StoredEvent;

mod compaction;
mod index;

pub use compaction::{CompactOutcome, CompactRejection, MalformedLine};
pub use index::{IndexWrite, LoadedIndex, SessionIndex};

/// The first line of a rewritten log. It lives in the log itself so the
/// records and the epoch they belong to are replaced in one rename and can
/// never disagree after a crash. It is neither an envelope nor a bare event,
/// so it parses as no record.
#[derive(Serialize, Deserialize)]
struct LayoutHeader {
    /// How many times the log's records were rewritten in place.
    layout_epoch: u64,
}

/// The layout epoch a log's first line declares, if it is a header.
fn layout_epoch(first_line: &str) -> Option<u64> {
    serde_json::from_str::<LayoutHeader>(first_line)
        .ok()
        .map(|header| header.layout_epoch)
}

/// How many of the log's bytes its layout header line takes; 0 without one.
fn header_len(bytes: &[u8]) -> usize {
    let first = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    match std::str::from_utf8(first).ok().and_then(layout_epoch) {
        Some(_) => (first.len() + 1).min(bytes.len()),
        None => 0,
    }
}

/// On-disk envelope wrapping each event with its record time. Kept private:
/// callers deal in [`StoredEvent`] (which tolerates the legacy bare form).
#[derive(Deserialize)]
struct EventEnvelope {
    ts: u64,
    event: AgentEvent,
}

/// The borrowed write side of [`EventEnvelope`].
#[derive(Serialize)]
struct EventRecord<'a> {
    ts: u64,
    event: &'a AgentEvent,
}

fn encode_record(ts: u64, event: &AgentEvent) -> std::io::Result<String> {
    serde_json::to_string(&EventRecord { ts, event })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Encode timestamped events as a complete event log, byte for byte what
/// appending them one at a time to an empty log writes.
pub fn encode_event_log<'a>(
    events: impl IntoIterator<Item = (u64, &'a AgentEvent)>,
) -> std::io::Result<Vec<u8>> {
    let mut log = Vec::new();
    for (ts, event) in events {
        log.extend_from_slice(encode_record(ts, event)?.as_bytes());
        log.push(b'\n');
    }
    Ok(log)
}

/// A session's event log as one read of its file saw it.
#[derive(Debug, Default)]
pub struct EventLog {
    /// The layout `records` are positioned in: the epoch the log's header
    /// declares, or 0 for a log without one.
    pub epoch: u64,
    pub records: Vec<StoredEvent>,
}

/// Cheap, cloneable handle to the on-disk data directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    /// Full event-log parses, shared by every clone of this store so tests can
    /// prove a caller served history from memory.
    #[cfg(any(test, feature = "test-support"))]
    event_reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl SessionStore {
    /// Open (creating if needed) the store under the platform data dir, or under
    /// `TCODE_DATA_DIR` when it is set — which gives a throwaway profile (its own
    /// sessions, settings and installed ACP agents) for demos and screenshots.
    pub fn open_default() -> std::io::Result<Self> {
        let root = match std::env::var_os("TCODE_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("tcode"),
        };
        Self::open_at(root)
    }

    pub fn open_at(root: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            #[cfg(any(test, feature = "test-support"))]
            event_reads: Default::default(),
        })
    }

    /// How many times [`SessionStore::read_events`] parsed a log through this
    /// store or any of its clones.
    #[cfg(any(test, feature = "test-support"))]
    pub fn event_reads(&self) -> usize {
        self.event_reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    fn events_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    fn original_events_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl.orig"))
    }

    /// Read the native event-log records' bytes without parsing or
    /// normalizing them. The layout header is the store's, not a record, so
    /// it is left out.
    pub fn read_event_log(&self, id: &str) -> std::io::Result<Vec<u8>> {
        match fs::read(self.events_path(id)) {
            Ok(mut bytes) => {
                bytes.drain(..header_len(&bytes));
                Ok(bytes)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// Atomically install a complete native event log for a session.
    pub fn write_event_log(&self, id: &str, bytes: &[u8]) -> std::io::Result<()> {
        let destination = self.events_path(id);
        let temporary = destination.with_extension("jsonl.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, destination)
    }

    fn models_path(&self, provider: ProviderKind) -> PathBuf {
        let name = match provider {
            ProviderKind::Codex => "codex",
            ProviderKind::ClaudeCode => "claude",
            ProviderKind::Pi => "pi",
            ProviderKind::OpenCode => "opencode",
            ProviderKind::Cursor => "cursor",
            ProviderKind::Grok => "grok",
            // ACP agents publish their models over the wire at session start
            // (`AgentEvent::ProviderOptions`), so there is no catalog to cache.
            ProviderKind::Acp => "acp",
        };
        self.root.join(format!("models-{name}.json"))
    }

    fn commands_path(&self, provider: ProviderKind, acp_agent_id: Option<&str>) -> Option<PathBuf> {
        let name = match provider {
            ProviderKind::Codex => "codex".to_string(),
            ProviderKind::ClaudeCode => "claude".to_string(),
            ProviderKind::Pi => "pi".to_string(),
            ProviderKind::OpenCode => "opencode".to_string(),
            ProviderKind::Cursor => "cursor".to_string(),
            ProviderKind::Grok => "grok".to_string(),
            ProviderKind::Acp => {
                let id = acp_agent_id?;
                // Registry ids are external input and may contain path separators.
                // Hex keeps the filename reversible and collision-free without
                // allowing an id to escape the data directory.
                let encoded = id
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                format!("acp-{encoded}")
            }
        };
        Some(self.root.join(format!("commands-{name}.json")))
    }

    /// Load the last-fetched model catalog for `provider` so the picker is
    /// instant offline. Empty when never fetched / unreadable.
    pub fn load_models(&self, provider: ProviderKind) -> Vec<ModelSpec> {
        let Ok(bytes) = fs::read(self.models_path(provider)) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Persist the freshly fetched model catalog for `provider`.
    pub fn save_models(&self, provider: ProviderKind, models: &[ModelSpec]) -> std::io::Result<()> {
        let path = self.models_path(provider);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(models)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)
    }

    /// Load the most recently reported command/skill list for a native provider
    /// or one specific ACP agent. Empty when missing, unreadable, or when an ACP
    /// agent id was not supplied.
    pub fn load_commands(
        &self,
        provider: ProviderKind,
        acp_agent_id: Option<&str>,
    ) -> Vec<ProviderCommand> {
        let Some(path) = self.commands_path(provider, acp_agent_id) else {
            return Vec::new();
        };
        let Ok(bytes) = fs::read(path) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Atomically persist the complete command/skill list reported by a native
    /// provider or one specific ACP agent. Empty lists are meaningful: they
    /// replace a stale non-empty cache.
    pub fn save_commands(
        &self,
        provider: ProviderKind,
        acp_agent_id: Option<&str>,
        commands: &[ProviderCommand],
    ) -> std::io::Result<()> {
        let path = self.commands_path(provider, acp_agent_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ACP command cache requires an agent id",
            )
        })?;
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(commands)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)
    }

    /// Delete a project icon the host copied into the data dir; an icon
    /// elsewhere is the user's own file.
    fn remove_project_icon(&self, path: PathBuf) {
        if path.parent() == Some(self.root.join("project-icons").as_path())
            && let Err(error) = fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("could not remove project icon {}: {error}", path.display());
        }
    }

    /// Append one event to the session's JSONL log, wrapped in a timestamped
    /// envelope (`{"ts": <unix_ms>, "event": {…}}`).
    pub fn append_event(&self, id: &str, ts: u64, event: &AgentEvent) -> std::io::Result<()> {
        let line = encode_record(ts, event)?;
        let mut file: File = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(self.events_path(id))?;
        let len = file.metadata()?.len();
        if len > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0_u8; 1];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")
    }

    /// Read and parse every persisted event for a session (skipping bad lines).
    ///
    /// Each line is tolerantly parsed as either a timestamped envelope
    /// (`{"ts":…,"event":…}`) or a legacy bare event (`{"type":…}`), so logs
    /// written before the envelope format still replay (with `ts == None`).
    pub fn read_events(&self, id: &str) -> Vec<StoredEvent> {
        self.read_log(id).records
    }

    /// [`SessionStore::read_events`] with the layout the records are in. Both
    /// come from one open file, so a rewrite renamed over the log meanwhile
    /// cannot pair one layout's epoch with the other's records.
    pub fn read_log(&self, id: &str) -> EventLog {
        #[cfg(any(test, feature = "test-support"))]
        self.event_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut log = EventLog::default();
        let Ok(file) = File::open(self.events_path(id)) else {
            return log;
        };
        for (number, line) in BufReader::new(file).lines().enumerate() {
            let Ok(line) = line else { break };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if number == 0
                && let Some(epoch) = layout_epoch(trimmed)
            {
                log.epoch = epoch;
                continue;
            }
            match parse_stored_line(trimmed) {
                Ok(stored) => log.records.push(stored),
                Err(err) => log::warn!("skipping unparseable event in {id}.jsonl: {err}"),
            }
        }
        log
    }

    /// Atomically clone one session's append-only event log. A missing source
    /// is an empty transcript and therefore succeeds without creating a file.
    /// The copy is byte for byte, layout header included: it starts in the
    /// layout its records are already in.
    pub fn clone_events(&self, src_id: &str, dst_id: &str) -> std::io::Result<()> {
        let src = self.events_path(src_id);
        let data = match fs::read(src) {
            Ok(data) => data,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        let dst = self.events_path(dst_id);
        let tmp = dst.with_extension("jsonl.tmp");
        fs::write(&tmp, data)?;
        fs::rename(tmp, dst)
    }

    /// Delete removed sessions' event logs and the originals kept from before
    /// a rewrite. Every file is attempted; the first failure is returned.
    pub fn remove_session_logs(&self, ids: &[String]) -> std::io::Result<()> {
        let removed: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut result = Ok(());
        for id in removed {
            for path in [self.events_path(id), self.original_events_path(id)] {
                match fs::remove_file(path) {
                    Err(err) if err.kind() != std::io::ErrorKind::NotFound && result.is_ok() => {
                        result = Err(err);
                    }
                    _ => {}
                }
            }
        }
        result
    }
}

/// Parse one JSONL line into a [`StoredEvent`], accepting both the timestamped
/// envelope and the legacy bare-event form. Envelope is tried first; a bare
/// event lacks the `ts`/`event` keys so it can't masquerade as one, and an
/// envelope lacks the top-level `type` tag so it can't parse as a bare event.
pub(crate) fn parse_stored_line(line: &str) -> Result<StoredEvent, serde_json::Error> {
    match serde_json::from_str::<EventEnvelope>(line) {
        Ok(envelope) => Ok(StoredEvent {
            ts: Some(envelope.ts),
            event: envelope.event,
            elided: None,
        }),
        Err(_envelope_err) => match serde_json::from_str::<AgentEvent>(line) {
            Ok(event) => Ok(StoredEvent {
                ts: None,
                event,
                elided: None,
            }),
            // Both forms failed: the line is genuinely corrupt. The bare-event
            // error is the more informative one (the envelope attempt always
            // fails on a bare event merely because `ts` is missing).
            Err(bare_err) => Err(bare_err),
        },
    }
}

pub use tcode_core::project::now_secs;

/// Current wall-clock time in unix milliseconds (used for event envelopes).
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{ProviderCommandKind, TurnStatus};

    fn temp_root() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("tcode-store-test-{}", uuid::Uuid::new_v4()));
        p
    }

    #[test]
    fn command_cache_roundtrips_per_provider_and_acp_agent() {
        let root = temp_root();
        let store = SessionStore::open_at(root.clone()).unwrap();
        let native = vec![ProviderCommand {
            name: "review".into(),
            description: Some("Review the current changes".into()),
            kind: ProviderCommandKind::Command,
        }];
        let acp = vec![ProviderCommand {
            name: "browser".into(),
            description: None,
            kind: ProviderCommandKind::Skill,
        }];
        store
            .save_commands(ProviderKind::ClaudeCode, None, &native)
            .unwrap();
        store
            .save_commands(ProviderKind::Acp, Some("vendor/agent"), &acp)
            .unwrap();

        // Reopen the store to prove the values come from disk, not memory.
        let reopened = SessionStore::open_at(root.clone()).unwrap();
        assert_eq!(
            reopened.load_commands(ProviderKind::ClaudeCode, None),
            native
        );
        assert_eq!(
            reopened.load_commands(ProviderKind::Acp, Some("vendor/agent")),
            acp
        );
        assert!(
            reopened
                .load_commands(ProviderKind::Acp, Some("different-agent"))
                .is_empty()
        );
        assert!(root.join("commands-claude.json").is_file());
        assert!(
            root.join("commands-acp-76656e646f722f6167656e74.json")
                .is_file()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn append_recovers_a_mixed_legacy_log_and_writes_the_current_envelope() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let id = "mixed";
        fs::write(store.events_path(id), concat!(
            "{\"type\":\"turn_started\",\"turn_id\":\"legacy\"}\n",
            "{\"ts\":2000,\"event\":{\"type\":\"turn_completed\",\"turn_id\":\"legacy\",\"status\":\"completed\",\"usage\":null}}\n",
            "\n{not valid json}\n{\"type\":\"turn_started"
        )).unwrap();
        store
            .append_event(
                id,
                3000,
                &AgentEvent::TurnStarted {
                    turn_id: "next".into(),
                },
            )
            .unwrap();
        let reopened = SessionStore::open_at(store.root().clone()).unwrap();
        let events = reopened.read_events(id);
        assert_eq!(events.len(), 3);
        assert_eq!(
            events.iter().map(|event| event.ts).collect::<Vec<_>>(),
            [None, Some(2000), Some(3000)]
        );
        assert!(
            matches!(&events[0].event, AgentEvent::TurnStarted { turn_id } if turn_id == "legacy")
        );
        assert!(
            matches!(&events[1].event, AgentEvent::TurnCompleted { turn_id, status: TurnStatus::Completed, .. } if turn_id == "legacy")
        );
        assert!(
            matches!(&events[2].event, AgentEvent::TurnStarted { turn_id } if turn_id == "next")
        );
        let raw = fs::read_to_string(store.events_path(id)).unwrap();
        assert_eq!(
            raw.lines().last().unwrap(),
            r#"{"ts":3000,"event":{"type":"turn_started","turn_id":"next"}}"#
        );
        fs::remove_dir_all(store.root()).unwrap();
    }
}
