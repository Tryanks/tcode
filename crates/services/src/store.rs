//! On-disk persistence for tcode sessions.
//!
//! Layout (under the platform data dir, e.g. `~/Library/Application Support/tcode/`):
//!   * `sessions.json` — a JSON array of [`SessionMeta`], the session index.
//!   * `<id>.jsonl`     — one line per received [`AgentEvent`] (append-only).
//!
//! Replaying a session = read its `.jsonl`, parse each line into an
//! [`AgentEvent`], and fold them with [`tcode_core::session::fold_events`] into a
//! [`tcode_core::session::Timeline`].

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

#[cfg(test)]
use agent::ApprovalMode;
use agent::{AgentEvent, ModelSpec, ProviderCommand, ProviderKind};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use tcode_core::project::{Checkpoint, WorktreeInfo};
use tcode_core::project::{IndexFile, Project, SessionMeta, migrate_index};
use tcode_core::session::StoredEvent;

/// On-disk envelope wrapping each event with its record time. Kept private:
/// callers deal in [`StoredEvent`] (which tolerates the legacy bare form).
#[derive(Serialize, Deserialize)]
struct EventEnvelope {
    ts: u64,
    event: AgentEvent,
}

/// Cheap, cloneable handle to the on-disk data directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
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
        Ok(Self { root })
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

    /// Append one event to the session's JSONL log, wrapped in a timestamped
    /// envelope (`{"ts": <unix_ms>, "event": {…}}`).
    pub fn append_event(&self, id: &str, ts: u64, event: &AgentEvent) -> std::io::Result<()> {
        let envelope = EventEnvelope {
            ts,
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
        file.write_all(b"\n")
    }

    /// Read and parse every persisted event for a session (skipping bad lines).
    ///
    /// Each line is tolerantly parsed as either a timestamped envelope
    /// (`{"ts":…,"event":…}`) or a legacy bare event (`{"type":…}`), so logs
    /// written before the envelope format still replay (with `ts == None`).
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
                Ok(stored) => events.push(stored),
                Err(err) => log::warn!("skipping unparseable event in {id}.jsonl: {err}"),
            }
        }
        events
    }

    /// Count the persisted events for a session (number of parseable JSONL
    /// lines). Used to record a checkpoint's `event_offset` before a turn's user
    /// message is appended, so a later revert can truncate deterministically.
    pub fn event_count(&self, id: &str) -> usize {
        self.read_events(id).len()
    }

    /// Truncate a session's JSONL log to its first `keep` events, discarding the
    /// rest (used by revert-to-checkpoint). Rewrites the file atomically. A
    /// `keep` at or beyond the current length is a no-op.
    pub fn truncate_events(&self, id: &str, keep: usize) -> std::io::Result<()> {
        let path = self.events_path(id);
        let events = self.read_events(id);
        if keep >= events.len() {
            return Ok(());
        }
        let tmp = path.with_extension("jsonl.tmp");
        {
            let mut file = File::create(&tmp)?;
            for stored in events.iter().take(keep) {
                let line = match stored.ts {
                    Some(ts) => serde_json::to_string(&EventEnvelope {
                        ts,
                        event: stored.event.clone(),
                    }),
                    None => serde_json::to_string(&stored.event),
                }
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                file.write_all(line.as_bytes())?;
                file.write_all(b"\n")?;
            }
            file.flush()?;
        }
        fs::rename(&tmp, &path)
    }

    /// Remove a session from the index and delete its event log.
    pub fn remove_session(&self, id: &str) -> std::io::Result<()> {
        let mut file = self.read_file();
        file.sessions.retain(|meta| meta.id != id);
        self.write_file(&file)?;
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
            event: envelope.event,
        }),
        Err(_envelope_err) => match serde_json::from_str::<AgentEvent>(line) {
            Ok(event) => Ok(StoredEvent { ts: None, event }),
            // Both forms failed: the line is genuinely corrupt. The bare-event
            // error is the more informative one (the envelope attempt always
            // fails on a bare event merely because `ts` is missing).
            Err(bare_err) => Err(bare_err),
        },
    }
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
        assert!(meta.checkpoints.is_empty());

        // Absent fields are skipped on serialize (keeps legacy files clean).
        let json = serde_json::to_string(&meta).unwrap();
        assert!(!json.contains("archived_at"));
        assert!(!json.contains("worktree"));
        assert!(!json.contains("checkpoints"));

        // A populated meta round-trips every new field.
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/wt"), None);
        meta.archived_at = Some(1234);
        meta.worktree = Some(WorktreeInfo {
            root_project_path: PathBuf::from("/proj"),
            base: "main".into(),
            branch: "tcode/abc".into(),
        });
        meta.checkpoints = vec![Checkpoint {
            turn: 2,
            commit: "deadbeef".into(),
            event_offset: 7,
        }];
        let json = serde_json::to_string(&meta).unwrap();
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.archived_at, Some(1234));
        assert_eq!(back.worktree, meta.worktree);
        assert_eq!(back.checkpoints, meta.checkpoints);
    }

    #[test]
    fn truncate_events_keeps_prefix_and_rewrites_log() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let id = "trunc";
        for turn in 0..4 {
            store
                .append_event(
                    id,
                    turn as u64,
                    &AgentEvent::TurnStarted {
                        turn_id: format!("t{turn}"),
                    },
                )
                .unwrap();
        }
        assert_eq!(store.event_count(id), 4);

        // Keeping the first 2 events discards the tail.
        store.truncate_events(id, 2).unwrap();
        let events = store.read_events(id);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].ts, Some(0));
        assert_eq!(events[1].ts, Some(1));

        // A keep beyond the length is a no-op.
        store.truncate_events(id, 10).unwrap();
        assert_eq!(store.event_count(id), 2);
        let _ = fs::remove_dir_all(store.root());
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
}
