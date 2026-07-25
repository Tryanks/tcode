//! On-disk persistence for tcode sessions.
//!
//! Layout (under the platform data dir, e.g. `~/Library/Application Support/tcode/`):
//!   * `sessions.json` — a JSON array of [`SessionMeta`], the session index.
//!   * `<id>.jsonl`     — one line per received [`AgentEvent`] (append-only).
//!
//! Replaying a session = read its `.jsonl`, parse each line into an
//! [`AgentEvent`], and fold them with [`tcode_core::session::fold_events`] into a
//! [`tcode_core::session::Timeline`].

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use agent::ApprovalMode;
use agent::{AgentEvent, ModelSpec, ProviderCommand, ProviderKind};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use tcode_core::project::WorktreeInfo;
use tcode_core::project::{IndexFile, Project, SessionMeta, migrate_index};
use tcode_core::session::StoredEvent;

/// On-disk envelope wrapping each event with its record time and log position.
/// Kept private: callers deal in [`StoredEvent`] (which tolerates the legacy
/// bare form).
#[derive(Serialize, Deserialize)]
struct EventEnvelope {
    ts: u64,
    /// 1-based position in this session's log. Absent on lines written before
    /// the field existed; those are numbered by position on read, so a log may
    /// mix the two forms and still present one contiguous sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
    event: AgentEvent,
}

/// Cheap, cloneable handle to the on-disk data directory.
///
/// Clones share the sequence counters: they are per *store*, not per handle,
/// so two clones appending to one session cannot hand out the same `seq`.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    /// Next `seq` to assign, per session id.
    ///
    /// Normally primed by `read_events`, which has already parsed the log and
    /// so knows the answer for free. `scan_next_seq` is the fallback for a
    /// session appended to without being read first. Either way it is computed
    /// once per session, never per append — that would make writing a log
    /// quadratic in its length.
    next_seq: Arc<Mutex<HashMap<String, u64>>>,
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
            next_seq: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("sessions.json")
    }

    fn events_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    fn models_path(&self, provider: ProviderKind) -> PathBuf {
        let name = match provider {
            ProviderKind::Codex => "codex",
            ProviderKind::ClaudeCode => "claude",
            ProviderKind::Pi => "pi",
            ProviderKind::OpenCode => "opencode",
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
            ProviderKind::Acp => {
                let id = acp_agent_id?;
                // Registry ids are external input and may contain path separators.
                // Hex keeps the filename reversible and collision-free without
                // allowing an id to escape the data directory.
                let mut encoded = String::with_capacity(id.len() * 2);
                const HEX: &[u8; 16] = b"0123456789abcdef";
                for byte in id.as_bytes() {
                    encoded.push(HEX[(byte >> 4) as usize] as char);
                    encoded.push(HEX[(byte & 0x0f) as usize] as char);
                }
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

    /// Load the whole index file (projects + sessions), tolerating the old
    /// bare-array schema and deriving implicit projects for orphan sessions.
    pub fn read_file(&self) -> IndexFile {
        let path = self.index_path();
        let Ok(bytes) = fs::read(&path) else {
            return IndexFile::default();
        };
        // Current schema is an object; the legacy schema was a bare array.
        let parsed = serde_json::from_slice::<IndexFile>(&bytes).or_else(|_| {
            serde_json::from_slice::<Vec<SessionMeta>>(&bytes).map(|sessions| IndexFile {
                projects: Vec::new(),
                sessions,
            })
        });
        match parsed {
            Ok(file) => migrate_index(file),
            Err(err) => {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0);
                let corrupt_path = self.root.join(format!("sessions.json.corrupt-{timestamp}"));
                match fs::rename(&path, &corrupt_path) {
                    Ok(()) => log::warn!(
                        "failed to parse sessions.json: {err}; preserved it as {}",
                        corrupt_path.display()
                    ),
                    Err(rename_err) => log::warn!(
                        "failed to parse sessions.json: {err}; failed to preserve corrupt index: {rename_err}"
                    ),
                }
                IndexFile::default()
            }
        }
    }

    /// Load the session index (newest first). Empty if missing / unreadable.
    pub fn load_index(&self) -> Vec<SessionMeta> {
        let mut metas = self.read_file().sessions;
        metas.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
        metas
    }

    /// Load the persisted project list.
    pub fn load_projects(&self) -> Vec<Project> {
        self.read_file().projects
    }

    /// Persist a whole index file (used to flush migration on startup).
    pub fn persist_index(&self, file: &IndexFile) -> std::io::Result<()> {
        self.write_file(file)
    }

    /// Insert or replace a meta in the index (by id), then persist.
    pub fn upsert_meta(&self, meta: &SessionMeta) -> std::io::Result<()> {
        let mut file = self.read_file();
        if let Some(existing) = file.sessions.iter_mut().find(|m| m.id == meta.id) {
            *existing = meta.clone();
        } else {
            file.sessions.push(meta.clone());
        }
        self.write_file(&file)
    }

    /// Insert or replace a project (by id), then persist.
    pub fn upsert_project(&self, project: &Project) -> std::io::Result<()> {
        let mut file = self.read_file();
        if let Some(existing) = file.projects.iter_mut().find(|p| p.id == project.id) {
            *existing = project.clone();
        } else {
            file.projects.push(project.clone());
        }
        self.write_file(&file)
    }

    /// Remove a project from the index. Its sessions are removed separately so
    /// their event logs receive the same cleanup as an ordinary thread delete.
    pub fn remove_project(&self, id: &str) -> std::io::Result<()> {
        let mut file = self.read_file();
        file.projects.retain(|project| project.id != id);
        self.write_file(&file)
    }

    fn write_file(&self, file: &IndexFile) -> std::io::Result<()> {
        let tmp = self.index_path().with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, data)?;
        fs::rename(&tmp, self.index_path())
    }

    /// Next unused `seq` for a session, scanning its log once and caching the
    /// result. The scan takes the larger of the line count and the highest
    /// explicit `seq`, so a log that mixes legacy (position-numbered) lines
    /// with newer explicit ones never reissues a number.
    fn reserve_seq(&self, id: &str) -> u64 {
        let mut counters = self
            .next_seq
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let next = counters
            .entry(id.to_owned())
            .or_insert_with(|| scan_next_seq(&self.events_path(id)));
        let seq = *next;
        *next += 1;
        seq
    }

    /// Forget a session's cached counter, so the next append rescans the log.
    /// Needed wherever the file changes behind the cache's back.
    fn forget_seq(&self, id: &str) {
        self.next_seq
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
    }

    /// Append one event to the session's JSONL log, wrapped in an envelope
    /// (`{"ts": <unix_ms>, "seq": <n>, "event": {…}}`), returning the `seq` it
    /// was assigned.
    ///
    /// `seq` is assigned here rather than by the caller because this is the
    /// point every write funnels through, so the numbers it hands out are
    /// dense and gap-free by construction.
    pub fn append_event(&self, id: &str, ts: u64, event: &AgentEvent) -> std::io::Result<u64> {
        let seq = self.reserve_seq(id);
        let envelope = EventEnvelope {
            ts,
            seq: Some(seq),
            event: event.clone(),
        };
        let line = serde_json::to_string(&envelope)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
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
        file.write_all(b"\n")?;
        Ok(seq)
    }

    /// Read and parse every persisted event for a session (skipping bad lines).
    ///
    /// Each line is tolerantly parsed as either a timestamped envelope
    /// (`{"ts":…,"event":…}`) or a legacy bare event (`{"type":…}`), so logs
    /// written before the envelope format still replay (with `ts == None`).
    /// Every returned event carries `Some(seq)`: explicit where the line has
    /// one, positional otherwise.
    pub fn read_events(&self, id: &str) -> Vec<StoredEvent> {
        let path = self.events_path(id);
        let Ok(file) = File::open(&path) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match parse_stored_line(trimmed) {
                // Legacy lines carry no `seq`; number them by position so the
                // sequence a caller sees is contiguous from 1 regardless of
                // when each line was written. `events.len()` counts only lines
                // that parsed, which is what keeps it contiguous — a corrupt
                // line is skipped rather than consuming a number.
                Ok(mut stored) => {
                    stored.seq = stored.seq.or(Some(events.len() as u64 + 1));
                    events.push(stored);
                }
                Err(err) => log::warn!("skipping unparseable event in {id}.jsonl: {err}"),
            }
        }
        // Replaying already parsed the whole log, so the highest seq is in
        // hand. Priming the counter here means the common path — open a
        // session, then append to it — never pays for `scan_next_seq`, which
        // would re-read a log that can run to tens of megabytes.
        //
        // `or_insert` rather than overwrite: a counter that already exists has
        // seen appends this read may predate, and must not be walked back.
        if let Some(highest) = events.last().and_then(|stored| stored.seq) {
            self.next_seq
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(id.to_owned())
                .or_insert(highest + 1);
        }
        events
    }

    /// Atomically clone one session's append-only event log. A missing source
    /// is an empty transcript and therefore succeeds without creating a file.
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
        fs::rename(tmp, dst)?;
        // The destination's log just changed behind any cached counter (a fork
        // reuses an id whose counter may still say 1). Rescan on next append.
        self.forget_seq(dst_id);
        Ok(())
    }

    /// Remove a session from the index and delete its event log.
    pub fn remove_session(&self, id: &str) -> std::io::Result<()> {
        let mut file = self.read_file();
        file.sessions.retain(|meta| meta.id != id);
        self.write_file(&file)?;
        self.forget_seq(id);
        match fs::remove_file(self.events_path(id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

/// Parse one JSONL line into a [`StoredEvent`], accepting both the timestamped
/// envelope and the legacy bare-event form. Envelope is tried first; a bare
/// event lacks the `ts`/`event` keys so it can't masquerade as one, and an
/// envelope lacks the top-level `type` tag so it can't parse as a bare event.
fn parse_stored_line(line: &str) -> Result<StoredEvent, serde_json::Error> {
    match serde_json::from_str::<EventEnvelope>(line) {
        Ok(envelope) => Ok(StoredEvent {
            ts: Some(envelope.ts),
            seq: envelope.seq,
            event: envelope.event,
        }),
        Err(_envelope_err) => match serde_json::from_str::<AgentEvent>(line) {
            Ok(event) => Ok(StoredEvent {
                ts: None,
                seq: None,
                event,
            }),
            // Both forms failed: the line is genuinely corrupt. The bare-event
            // error is the more informative one (the envelope attempt always
            // fails on a bare event merely because `ts` is missing).
            Err(bare_err) => Err(bare_err),
        },
    }
}

/// The first `seq` that is safe to assign in an existing log.
///
/// The cold path. Opening a session calls `read_events`, which primes the
/// counter from a parse it was doing anyway; this runs only when something
/// appends to a session that was never read in this process. It reads each
/// line's `seq` rather than parsing events, so it costs a file read rather
/// than a full deserialization of a log that can reach tens of megabytes.
///
/// Takes the larger of the parsed-line count and the highest explicit `seq`
/// because a log may hold both forms — legacy lines get positional numbers on
/// read, and reusing one of those for a new line would break the order the
/// numbers exist to provide. A missing file is an empty log, so `seq` starts
/// at 1.
fn scan_next_seq(path: &PathBuf) -> u64 {
    let Ok(file) = File::open(path) else {
        return 1;
    };
    let mut lines = 0_u64;
    let mut highest = 0_u64;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A corrupt line still occupies a position: `read_events` skips it and
        // therefore never issues its number, so counting it here only leaves a
        // gap. Overshooting is safe; colliding is not.
        lines += 1;
        if let Ok(seq) = serde_json::from_str::<SeqOnly>(trimmed)
            && let Some(seq) = seq.seq
        {
            highest = highest.max(seq);
        }
    }
    lines.max(highest) + 1
}

/// Just enough of an envelope to read its `seq` (see [`scan_next_seq`]).
#[derive(Deserialize)]
struct SeqOnly {
    #[serde(default)]
    seq: Option<u64>,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
    fn index_roundtrip_and_sort() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let mut a = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/a"), None);
        a.updated_at = 100;
        let mut b = SessionMeta::new(ProviderKind::ClaudeCode, PathBuf::from("/b"), None);
        b.updated_at = 200;
        store.upsert_meta(&a).unwrap();
        store.upsert_meta(&b).unwrap();

        let index = store.load_index();
        assert_eq!(index.len(), 2);
        // newest first
        assert_eq!(index[0].id, b.id);
        assert_eq!(index[1].id, a.id);

        // upsert replaces
        let mut a2 = a.clone();
        a2.title = "renamed".into();
        store.upsert_meta(&a2).unwrap();
        let index = store.load_index();
        assert_eq!(index.len(), 2);
        assert_eq!(
            index.iter().find(|m| m.id == a.id).unwrap().title,
            "renamed"
        );
        let _ = fs::remove_dir_all(store.root());
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
    fn append_and_read_events() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let id = "sess-1";
        store
            .append_event(
                id,
                1_000,
                &AgentEvent::TurnStarted {
                    turn_id: "t1".into(),
                },
            )
            .unwrap();
        store
            .append_event(
                id,
                2_000,
                &AgentEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            )
            .unwrap();
        let events = store.read_events(id);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].ts, Some(1_000));
        assert!(matches!(events[0].event, AgentEvent::TurnStarted { .. }));
        assert_eq!(events[1].ts, Some(2_000));
        assert!(matches!(
            events[1].event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        ));
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn reader_tolerates_legacy_bare_events_and_envelopes() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let id = "mixed";
        // A pre-M3 bare event, a new envelope, a blank line, and a corrupt line.
        let contents = concat!(
            r#"{"type":"turn_started","turn_id":"legacy"}"#,
            "\n",
            r#"{"ts":1730000000000,"event":{"type":"turn_completed","turn_id":"new","status":"completed","usage":null}}"#,
            "\n",
            "\n",
            "{not valid json}\n",
        );
        fs::write(store.events_path(id), contents).unwrap();

        let events = store.read_events(id);
        assert_eq!(events.len(), 2);
        // Legacy bare event replays with no timestamp.
        assert_eq!(events[0].ts, None);
        assert!(matches!(events[0].event, AgentEvent::TurnStarted { .. }));
        // Envelope carries the recorded timestamp.
        assert_eq!(events[1].ts, Some(1_730_000_000_000));
        assert!(matches!(
            events[1].event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        ));
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn append_writes_recoverable_envelope() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let id = "roundtrip";
        store
            .append_event(
                id,
                42,
                &AgentEvent::TurnStarted {
                    turn_id: "t".into(),
                },
            )
            .unwrap();
        let raw = fs::read_to_string(store.events_path(id)).unwrap();
        assert!(raw.contains("\"ts\":42"));
        assert!(raw.contains("\"turn_started\""));
        let events = store.read_events(id);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ts, Some(42));
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn append_separates_event_from_truncated_last_line() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let id = "truncated";
        fs::write(store.events_path(id), br#"{"type":"turn_started"#).unwrap();

        store
            .append_event(
                id,
                7,
                &AgentEvent::TurnCompleted {
                    turn_id: "turn-1".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            )
            .unwrap();

        let events = store.read_events(id);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].event,
            AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            }
        ));
        let bytes = fs::read(store.events_path(id)).unwrap();
        assert!(bytes.starts_with(b"{\"type\":\"turn_started\n"));
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn corrupt_index_is_preserved_before_returning_empty() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let corrupt_bytes = b"not valid session json";
        fs::write(store.index_path(), corrupt_bytes).unwrap();

        assert!(store.load_index().is_empty());
        assert!(!store.index_path().exists());
        let backups: Vec<_> = fs::read_dir(store.root())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("sessions.json.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read(backups[0].path()).unwrap(), corrupt_bytes);
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn legacy_bare_array_index_loads_and_derives_projects() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        // Old-format file: a bare JSON array with no project_id fields.
        let legacy = serde_json::json!([
            {
                "id": "s1", "title": "One", "provider": "claude_code",
                "cwd": "/work/alpha", "created_at": 1, "updated_at": 10
            },
            {
                "id": "s2", "title": "Two", "provider": "codex",
                "cwd": "/work/alpha", "created_at": 2, "updated_at": 20
            },
            {
                "id": "s3", "title": "Three", "provider": "codex",
                "cwd": "/work/beta", "created_at": 3, "updated_at": 30
            }
        ]);
        fs::write(store.index_path(), legacy.to_string()).unwrap();

        let file = store.read_file();
        // Two distinct roots -> two derived projects, deduped by root.
        assert_eq!(file.projects.len(), 2);
        let alpha = file
            .projects
            .iter()
            .find(|p| p.root == std::path::Path::new("/work/alpha"))
            .unwrap();
        assert_eq!(alpha.name, "alpha");
        // Both alpha sessions share the same derived project.
        let s1 = file.sessions.iter().find(|s| s.id == "s1").unwrap();
        let s2 = file.sessions.iter().find(|s| s.id == "s2").unwrap();
        assert_eq!(s1.project_id, Some(alpha.id.clone()));
        assert_eq!(s2.project_id, s1.project_id);
        let s3 = file.sessions.iter().find(|s| s.id == "s3").unwrap();
        assert_ne!(s3.project_id, s1.project_id);
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn session_meta_approval_mode_defaults_to_full_access_when_absent() {
        // An index entry written before the permission-mode milestone has no
        // `approval_mode` key; it loads as the serde default, now `FullAccess`
        // (T3 parity — the app-wide default changed from Supervised).
        let legacy = serde_json::json!({
            "id": "s1", "title": "One", "provider": "codex",
            "cwd": "/work/alpha", "created_at": 1, "updated_at": 10
        });
        let meta: SessionMeta = serde_json::from_value(legacy).unwrap();
        assert_eq!(meta.approval_mode, ApprovalMode::FullAccess);

        // A newer entry with an explicit mode round-trips.
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
        assert_eq!(meta.approval_mode, ApprovalMode::FullAccess);
        meta.approval_mode = ApprovalMode::Supervised;
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"approval_mode\":\"supervised\""));
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.approval_mode, ApprovalMode::Supervised);
    }

    #[test]
    fn session_meta_fork_fields_are_legacy_safe_and_roundtrip() {
        let legacy = serde_json::json!({
            "id": "s1", "title": "One", "provider": "codex",
            "cwd": "/work/alpha", "created_at": 1, "updated_at": 10
        });
        let mut meta: SessionMeta = serde_json::from_value(legacy).unwrap();
        assert_eq!(meta.forked_from, None);
        assert!(!meta.pending_fork);
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("forked_from"));
        assert!(!json.contains("pending_fork"));

        meta.forked_from = Some("source".into());
        meta.pending_fork = true;
        let back: SessionMeta =
            serde_json::from_str(&serde_json::to_string(&meta).unwrap()).unwrap();
        assert_eq!(back.forked_from.as_deref(), Some("source"));
        assert!(back.pending_fork);
    }

    #[test]
    fn orchestration_fields_are_legacy_safe_and_roundtrip() {
        let legacy = serde_json::json!({
            "id": "s1", "title": "One", "provider": "codex",
            "cwd": "/work/alpha", "created_at": 1, "updated_at": 10
        });
        let meta: SessionMeta = serde_json::from_value(legacy).unwrap();
        assert_eq!(meta.parent_session_id, None);
        assert!(!meta.orchestrate_enabled);
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("parent_session_id"));
        assert!(!json.contains("orchestrate_enabled"));

        let mut meta = meta;
        meta.parent_session_id = Some("parent".into());
        meta.orchestrate_enabled = true;
        let back: SessionMeta =
            serde_json::from_str(&serde_json::to_string(&meta).unwrap()).unwrap();
        assert_eq!(back.parent_session_id.as_deref(), Some("parent"));
        assert!(back.orchestrate_enabled);
    }

    #[test]
    fn migrate_index_is_idempotent() {
        let file = IndexFile {
            projects: Vec::new(),
            sessions: vec![SessionMeta::new(
                ProviderKind::Codex,
                PathBuf::from("/work/gamma"),
                None,
            )],
        };
        let once = migrate_index(file);
        assert_eq!(once.projects.len(), 1);
        let twice = migrate_index(once.clone());
        assert_eq!(twice.projects.len(), 1);
        assert_eq!(once.sessions[0].project_id, twice.sessions[0].project_id);
    }

    #[test]
    fn archived_at_and_worktree_default_absent_and_roundtrip() {
        // Legacy index entry without the new fields loads with them absent.
        let legacy = serde_json::json!({
            "id": "s1", "title": "One", "provider": "codex",
            "cwd": "/work/alpha", "created_at": 1, "updated_at": 10
        });
        let meta: SessionMeta = serde_json::from_value(legacy).unwrap();
        assert_eq!(meta.archived_at, None);
        assert_eq!(meta.worktree, None);

        // Absent fields are skipped on serialize (keeps legacy files clean).
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("archived_at"));
        assert!(!json.contains("worktree"));

        // A populated meta round-trips every new field.
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/wt"), None);
        meta.archived_at = Some(1234);
        meta.worktree = Some(WorktreeInfo {
            root_project_path: PathBuf::from("/proj"),
            base: "main".into(),
            branch: "tcode/abc".into(),
        });
        let json = serde_json::to_string(&meta).unwrap();
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.archived_at, Some(1234));
        assert_eq!(back.worktree, meta.worktree);
    }

    #[test]
    fn legacy_checkpoint_metadata_is_ignored() {
        let legacy = serde_json::json!({
            "id": "s1", "title": "One", "provider": "codex",
            "cwd": "/work/alpha", "created_at": 1, "updated_at": 10,
            "checkpoints": [{"turn": 2, "commit": "deadbeef", "event_offset": 7}]
        });
        let meta: SessionMeta = serde_json::from_value(legacy).unwrap();
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("checkpoints"));
    }

    #[test]
    fn remove_session_deletes_meta_and_event_log() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/project"), None);
        store.upsert_meta(&meta).unwrap();
        store
            .append_event(
                &meta.id,
                1,
                &AgentEvent::TurnStarted {
                    turn_id: "turn-1".into(),
                },
            )
            .unwrap();
        assert!(store.events_path(&meta.id).is_file());

        store.remove_session(&meta.id).unwrap();

        assert!(store.load_index().is_empty());
        assert!(!store.events_path(&meta.id).exists());
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn clone_events_copies_contents_and_missing_source_is_a_noop() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        store
            .append_event(
                "source",
                7,
                &AgentEvent::TurnStarted {
                    turn_id: "turn-1".into(),
                },
            )
            .unwrap();
        store.clone_events("source", "fork").unwrap();
        assert_eq!(
            fs::read(store.events_path("fork")).unwrap(),
            fs::read(store.events_path("source")).unwrap()
        );

        store.clone_events("missing", "empty-fork").unwrap();
        assert!(!store.events_path("empty-fork").exists());
        let _ = fs::remove_dir_all(store.root());
    }

    // -- event sequence numbers ---------------------------------------------

    fn assistant(text: &str) -> AgentEvent {
        AgentEvent::Delta {
            item_id: "item-1".into(),
            kind: agent::DeltaKind::AssistantText,
            text: text.into(),
        }
    }

    fn seqs(store: &SessionStore, id: &str) -> Vec<u64> {
        store
            .read_events(id)
            .into_iter()
            .map(|stored| stored.seq.expect("read_events always assigns a seq"))
            .collect()
    }

    #[test]
    fn appended_events_are_numbered_from_one_and_the_number_is_returned() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let assigned: Vec<u64> = ["a", "b", "c"]
            .iter()
            .map(|text| store.append_event("s1", 1, &assistant(text)).unwrap())
            .collect();

        assert_eq!(assigned, vec![1, 2, 3], "append must hand back its own seq");
        assert_eq!(seqs(&store, "s1"), vec![1, 2, 3]);
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn sequences_are_per_session_not_global() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        store.append_event("s1", 1, &assistant("a")).unwrap();
        store.append_event("s2", 1, &assistant("b")).unwrap();
        store.append_event("s1", 1, &assistant("c")).unwrap();

        assert_eq!(seqs(&store, "s1"), vec![1, 2]);
        assert_eq!(seqs(&store, "s2"), vec![1]);
        let _ = fs::remove_dir_all(store.root());
    }

    /// Logs written before `seq` existed carry only `ts`. They must still
    /// present a contiguous sequence, or a client resuming from a cursor would
    /// silently skip the whole legacy prefix.
    #[test]
    fn legacy_lines_without_seq_are_numbered_by_position() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let path = store.events_path("s1");
        fs::write(
            &path,
            "{\"ts\":10,\"event\":{\"type\":\"session_closed\"}}\n\
             {\"type\":\"session_closed\"}\n\
             {\"ts\":30,\"event\":{\"type\":\"session_closed\"}}\n",
        )
        .unwrap();

        assert_eq!(seqs(&store, "s1"), vec![1, 2, 3]);
        let _ = fs::remove_dir_all(store.root());
    }

    /// The case the counter exists to survive: a log that already has legacy
    /// lines, reopened by a fresh process, then appended to. The new lines must
    /// continue past the positional numbers rather than restart and collide.
    #[test]
    fn appending_to_a_legacy_log_continues_past_the_positional_numbers() {
        let root = temp_root();
        let store = SessionStore::open_at(root.clone()).unwrap();
        fs::write(
            store.events_path("s1"),
            "{\"ts\":10,\"event\":{\"type\":\"session_closed\"}}\n\
             {\"ts\":20,\"event\":{\"type\":\"session_closed\"}}\n",
        )
        .unwrap();

        // A separate handle: nothing is cached, exactly like a restart.
        let reopened = SessionStore::open_at(root.clone()).unwrap();
        assert_eq!(reopened.append_event("s1", 30, &assistant("x")).unwrap(), 3);

        let observed = seqs(&reopened, "s1");
        assert_eq!(observed, vec![1, 2, 3]);
        assert!(
            observed.windows(2).all(|w| w[1] == w[0] + 1),
            "sequence must stay contiguous across the format change: {observed:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// A fork copies the log verbatim, so the destination inherits its
    /// sequence. Appending to the fork must not restart at 1 and overwrite the
    /// inherited numbering.
    #[test]
    fn cloning_a_log_carries_its_sequence_into_the_fork() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        store.append_event("src", 1, &assistant("a")).unwrap();
        store.append_event("src", 2, &assistant("b")).unwrap();

        // Touch the destination first so a stale counter would exist to catch.
        store.append_event("dst", 1, &assistant("scratch")).unwrap();
        store.clone_events("src", "dst").unwrap();

        assert_eq!(store.append_event("dst", 3, &assistant("c")).unwrap(), 3);
        assert_eq!(seqs(&store, "dst"), vec![1, 2, 3]);
        let _ = fs::remove_dir_all(store.root());
    }

    /// Deleting a session frees its id. If the counter outlived the file, the
    /// next session reusing that id would start numbering mid-sequence.
    #[test]
    fn removing_a_session_resets_its_sequence() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        store.append_event("s1", 1, &assistant("a")).unwrap();
        store.append_event("s1", 2, &assistant("b")).unwrap();
        store.remove_session("s1").unwrap();

        assert_eq!(store.append_event("s1", 3, &assistant("c")).unwrap(), 1);
        let _ = fs::remove_dir_all(store.root());
    }

    /// Opening a session must leave the counter primed, so the append that
    /// follows does not re-read a log the replay just parsed. Checked directly
    /// rather than through behaviour, because a working-but-rescanning
    /// implementation passes every black-box assertion here.
    #[test]
    fn reading_a_log_primes_the_counter_so_appends_skip_the_rescan() {
        let root = temp_root();
        let store = SessionStore::open_at(root.clone()).unwrap();
        store.append_event("s1", 1, &assistant("a")).unwrap();
        store.append_event("s1", 2, &assistant("b")).unwrap();

        // A fresh handle has no cached counters, exactly like a restart.
        let reopened = SessionStore::open_at(root.clone()).unwrap();
        let primed = |store: &SessionStore| -> Option<u64> {
            store.next_seq.lock().unwrap().get("s1").copied()
        };
        assert_eq!(primed(&reopened), None, "counter must start cold");

        assert_eq!(reopened.read_events("s1").len(), 2);
        assert_eq!(
            primed(&reopened),
            Some(3),
            "replay must hand the counter its answer"
        );
        assert_eq!(reopened.append_event("s1", 3, &assistant("c")).unwrap(), 3);
        let _ = fs::remove_dir_all(root);
    }

    /// `seq` must not appear on the wire for events that never had one, so a
    /// reader can tell "no seq recorded" from "seq zero".
    #[test]
    fn envelopes_omit_seq_when_absent_and_carry_it_when_present() {
        let with = serde_json::to_string(&EventEnvelope {
            ts: 5,
            seq: Some(9),
            event: assistant("a"),
        })
        .unwrap();
        assert!(with.contains("\"seq\":9"), "{with}");

        let without = serde_json::to_string(&EventEnvelope {
            ts: 5,
            seq: None,
            event: assistant("a"),
        })
        .unwrap();
        assert!(!without.contains("seq"), "{without}");
    }
}
