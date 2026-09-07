use std::path::PathBuf;

use agent::FileChange;
use serde::{Deserialize, Serialize};

/// Read-only or result-bearing host operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Query {
    Ping,
    /// Records strictly before the absolute event cursor, oldest first.
    SessionHistoryPage {
        session_id: String,
        before: u64,
        limit: u32,
    },
    ListActiveWorkspace {
        session_id: String,
    },
    ScanExternalHistory,
    GenerateCommitMessage {
        session_id: String,
        included: Option<Vec<String>>,
    },
    LoadGitDiff {
        cwd: PathBuf,
        scope: GitDiffScope,
        base: Option<String>,
        ignore_whitespace: bool,
    },
    ReadFileBytes {
        path: PathBuf,
    },
    SaveAttachment {
        dir: PathBuf,
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
        ext: String,
    },
    RemoveUserFile {
        path: PathBuf,
    },
    /// Render a stored thread into a transferable artifact. Rendering belongs to
    /// the host (it owns the event log); where the bytes land belongs to the
    /// client, so nothing is written here.
    RenderThreadExport {
        session_id: String,
        format: crate::ThreadExportFormat,
    },
    /// Full-text search over the host's own stored session logs. The host owns
    /// the index, the cache and the session order; clients supply no paths.
    SearchSessionContent {
        query: String,
        limit: u32,
    },
    /// Re-render a stored command's output at `cols`. The emulator lives on the
    /// host, so a client asks for the wrapped, styled grid instead of parsing
    /// ANSI itself. `cols` outside [`STORED_OUTPUT_COLS`] is clamped into it.
    RenderStoredOutput {
        session_id: String,
        /// Timeline entry id of the command execution.
        item_id: String,
        cols: u16,
    },
}

/// Widths the host will render stored output at. Narrower than the low bound is
/// unreadable; wider is a client asking for a grid nobody can see.
pub const STORED_OUTPUT_COLS: std::ops::RangeInclusive<u16> = 20..=400;

/// Screen rows the host emulates stored output into. Trailing blank rows are
/// trimmed, so a shorter output returns a shorter frame.
pub const STORED_OUTPUT_ROWS: u16 = 16;

/// Largest export the host will put on one response frame. The transport writes
/// a single WebSocket text frame per NDJSON line and peers read with
/// tungstenite's 16 MiB frame cap, so the base64 payload (4/3 of the raw bytes)
/// plus its envelope must stay well inside that.
pub const MAX_THREAD_EXPORT_BYTES: usize = 8 * 1024 * 1024;

/// Typed response paired with a [`Query`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum QueryResponse {
    Pong,
    SessionHistoryPage {
        records: Vec<crate::SessionEventRecord>,
        from: u64,
        truncated: bool,
    },
    ActiveWorkspace(Vec<PathEntry>),
    ExternalHistory(Vec<RecentDir>),
    CommitMessage(String),
    GitDiff(GitDiffResult),
    FileBytes(#[serde(with = "crate::wire::base64_bytes")] Vec<u8>),
    SavedAttachment(PathBuf),
    UserFileRemoved,
    /// A rendered thread export. `suggested_name` is already safe for a file
    /// name on any client OS; the client picks the destination.
    ThreadExport {
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
        suggested_name: String,
        mime: String,
    },
    SessionContentHits(Vec<SessionSearchHit>),
    /// Stored output rendered into the same grid DTO the live terminal
    /// replicates. `history` is empty and there is no cursor: it is a finished
    /// screen, not a session.
    TerminalFrame(Box<crate::terminal::TerminalFrame>),
}

/// One content match in a stored session, addressed by the folded timeline
/// entry it came from. The sole owner of this shape: the search service builds
/// it and the palette renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub session_title: String,
    pub entry_id: String,
    pub turn: usize,
    pub snippet: String,
}

/// Scope used when loading a Git diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffScope {
    WorkingTree,
    Branch,
    #[serde(other)]
    Unknown,
}

/// Full base- and new-side text for one changed file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileText {
    pub old: Option<String>,
    pub new: Option<String>,
}

/// Result of loading a Git diff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiffResult {
    pub changes: Vec<FileChange>,
    pub texts: Vec<GitFileText>,
    pub truncated: bool,
    pub error: Option<String>,
    pub branches: Vec<String>,
    pub default_base: Option<String>,
}

/// One listable workspace entry (relative to the workspace root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathEntry {
    pub rel_path: String,
    pub basename: String,
    pub parent: String,
    pub is_dir: bool,
}

impl PathEntry {
    pub fn from_rel(rel_path: String, is_dir: bool) -> Self {
        let (parent, basename) = match rel_path.rfind('/') {
            Some(i) => (rel_path[..i].to_string(), rel_path[i + 1..].to_string()),
            None => (String::new(), rel_path.clone()),
        };
        Self {
            rel_path,
            basename,
            parent,
            is_dir,
        }
    }
}

/// External tool that owns an importable thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceTool {
    ClaudeCode,
    ClaudeDesktop,
    T3Code,
    CodexCli,
    CodexDesktop,
    #[serde(other)]
    Unknown,
}

impl SourceTool {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::ClaudeDesktop => "Claude Desktop",
            Self::T3Code => "T3 Code",
            Self::CodexCli => "Codex CLI",
            Self::CodexDesktop => "Codex Desktop",
            Self::Unknown => "Unknown",
        }
    }
}

/// One importable external thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalThread {
    pub source: SourceTool,
    pub file: PathBuf,
    pub external_id: String,
    pub title_hint: Option<String>,
    pub last_active_ms: u64,
}

/// Recently active directory containing external threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentDir {
    pub path: PathBuf,
    pub last_active_ms: u64,
    pub threads: Vec<ExternalThread>,
}

/// Fresh history and backwards pages are bounded independently of live cursors.
pub const SESSION_HISTORY_RECORDS: usize = 200;
pub const MAX_SESSION_HISTORY_BYTES: usize = 8 * 1024 * 1024;
