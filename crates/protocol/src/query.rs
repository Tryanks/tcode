use std::path::PathBuf;

use agent::FileChange;
use serde::{Deserialize, Serialize};

/// Read-only or result-bearing host operation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Query {
    ListActiveWorkspace,
    ScanExternalHistory,
    GenerateCommitMessage {
        included: Option<Vec<String>>,
    },
    SecretPresence {
        profile_id: String,
        name: String,
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
    RemoveUserFile {
        path: PathBuf,
    },
    IsDirectory {
        path: PathBuf,
    },
    RelativizeToWorkspace {
        path: String,
        cwd: PathBuf,
    },
}

/// Typed response paired with a [`Query`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum QueryResponse {
    ActiveWorkspace(Vec<PathEntry>),
    ExternalHistory(Vec<RecentDir>),
    CommitMessage(String),
    SecretPresence(bool),
    GitDiff(GitDiffResult),
    FileBytes(#[serde(with = "crate::wire::base64_bytes")] Vec<u8>),
    UserFileRemoved,
    IsDirectory(bool),
    RelativePath(String),
}

/// Protocol mirror of `tcode_services::git::GitDiffScope`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffScope {
    WorkingTree,
    Branch,
    #[serde(other)]
    Unknown,
}

/// Protocol mirror of `tcode_services::git::GitFileText`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileText {
    pub old: Option<String>,
    pub new: Option<String>,
}

/// Protocol mirror of `tcode_services::git::GitDiffResult`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitDiffResult {
    pub changes: Vec<FileChange>,
    pub texts: Vec<GitFileText>,
    pub truncated: bool,
    pub error: Option<String>,
    pub branches: Vec<String>,
    pub default_base: Option<String>,
}

impl PartialEq for GitDiffResult {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}

impl Eq for GitDiffResult {}

/// Protocol mirror of `tcode_services::workspace::PathEntry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathEntry {
    pub rel_path: String,
    pub basename: String,
    pub parent: String,
    pub is_dir: bool,
}

/// Protocol mirror of `tcode_services::import::SourceTool`.
#[non_exhaustive]
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

/// Protocol mirror of `tcode_services::import::ExternalThread`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalThread {
    pub source: SourceTool,
    pub file: PathBuf,
    pub external_id: String,
    pub title_hint: Option<String>,
    pub last_active_ms: u64,
}

/// Protocol mirror of `tcode_services::import::RecentDir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentDir {
    pub path: PathBuf,
    pub last_active_ms: u64,
    pub threads: Vec<ExternalThread>,
}
