//! On-disk persistence for tcode sessions.
//!
//! Layout (under the platform data dir, e.g. `~/Library/Application Support/tcode/`):
//!   * `sessions.json` — a JSON array of [`SessionMeta`], the session index.
//!   * `<id>.jsonl`     — one line per received [`AgentEvent`] (append-only).
//!
//! Replaying a session = read its `.jsonl`, parse each line into an
//! [`AgentEvent`], and fold them with `crate::session::fold_events`.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use agent::{AgentEvent, ProviderKind, ResumeCursor};
use serde::{Deserialize, Serialize};

/// One persisted event, optionally tagged with the wall-clock time (unix ms)
/// at which it was recorded. Legacy `.jsonl` lines (bare events) replay with
/// `ts == None`; envelope lines carry the recorded timestamp.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub ts: Option<u64>,
    pub event: AgentEvent,
}

/// On-disk envelope wrapping each event with its record time. Kept private:
/// callers deal in [`StoredEvent`] (which tolerates the legacy bare form).
#[derive(Serialize, Deserialize)]
struct EventEnvelope {
    ts: u64,
    event: AgentEvent,
}

/// A project groups sessions (threads) that share a working-directory root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub created_at: u64,
}

impl Project {
    /// Create a project rooted at `root`, deriving a display name from its
    /// last path component (falling back to the full path).
    pub fn from_root(root: PathBuf) -> Self {
        let name = project_name_from_root(&root);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            root,
            created_at: now_secs(),
        }
    }
}

/// Derive a project display name from a directory path.
pub fn project_name_from_root(root: &std::path::Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| root.display().to_string())
}

/// Index entry describing one persisted session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub provider: ProviderKind,
    pub cwd: PathBuf,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub resume_cursor: Option<ResumeCursor>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl SessionMeta {
    pub fn new(provider: ProviderKind, cwd: PathBuf, model: Option<String>) -> Self {
        let now = now_secs();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: format!("New {} session", provider.display_name()),
            provider,
            cwd,
            project_id: None,
            model,
            resume_cursor: None,
            created_at: now,
            updated_at: now,
        }
    }
}

/// On-disk shape of `sessions.json` (current schema). Old files were a bare
/// `Vec<SessionMeta>`; [`SessionStore::read_file`] tolerates both.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexFile {
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub sessions: Vec<SessionMeta>,
}

/// Ensure every session belongs to a project, deriving implicit projects from
/// each orphan session's cwd (deduped by root). Idempotent.
pub fn migrate_index(mut file: IndexFile) -> IndexFile {
    // Map existing project roots to their ids so derived projects dedupe.
    let mut root_to_id: std::collections::HashMap<PathBuf, String> = file
        .projects
        .iter()
        .map(|p| (p.root.clone(), p.id.clone()))
        .collect();

    for session in &mut file.sessions {
        if session
            .project_id
            .as_ref()
            .is_some_and(|id| file.projects.iter().any(|p| &p.id == id))
        {
            continue;
        }
        let root = session.cwd.clone();
        let project_id = if let Some(id) = root_to_id.get(&root) {
            id.clone()
        } else {
            let project = Project::from_root(root.clone());
            let id = project.id.clone();
            root_to_id.insert(root, id.clone());
            file.projects.push(project);
            id
        };
        session.project_id = Some(project_id);
    }
    file
}

/// Cheap, cloneable handle to the on-disk data directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    /// Open (creating if needed) the store under the platform data dir.
    pub fn open_default() -> std::io::Result<Self> {
        let root = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tcode");
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
        metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
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
    use agent::TurnStatus;

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
    fn append_and_read_events() {
        let store = SessionStore::open_at(temp_root()).unwrap();
        let id = "sess-1";
        store
            .append_event(
                &id,
                1_000,
                &AgentEvent::TurnStarted {
                    turn_id: "t1".into(),
                },
            )
            .unwrap();
        store
            .append_event(
                &id,
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
        assert!(matches!(
            events[0].event,
            AgentEvent::TurnStarted { .. }
        ));
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
            .append_event(id, 42, &AgentEvent::TurnStarted { turn_id: "t".into() })
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
            .find(|p| p.root == PathBuf::from("/work/alpha"))
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
