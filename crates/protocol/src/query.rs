use std::path::PathBuf;

use agent::FileChange;
use serde::{Deserialize, Serialize};

/// Read-only or result-bearing host operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Query {
    ListActiveWorkspace,
    ScanExternalHistory,
    GenerateCommitMessage {
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
    IsDirectory {
        path: PathBuf,
    },
}

/// Typed response paired with a [`Query`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum QueryResponse {
    ActiveWorkspace(Vec<PathEntry>),
    ExternalHistory(Vec<RecentDir>),
    CommitMessage(String),
    GitDiff(GitDiffResult),
    FileBytes(#[serde(with = "crate::wire::base64_bytes")] Vec<u8>),
    SavedAttachment(PathBuf),
    UserFileRemoved,
    IsDirectory(bool),
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
