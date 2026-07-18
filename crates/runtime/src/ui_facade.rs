//! Runtime-owned views and temporary DTO re-exports for UI migration.

use std::io;
use std::path::Path;

pub use tcode_services::difftastic::{
    StructuralError, StructuralFile, StructuralHighlight, StructuralRow, StructuralSide,
    StructuralSpan, parse_difft_json, run_structural_diff,
};
pub use tcode_services::git::{GitDiffResult, GitDiffScope, GitFileContent};
pub use tcode_services::import::{ExternalThread, RecentDir, SourceTool};
pub use tcode_services::workspace::PathEntry;

pub fn load_git_diff(cwd: &Path, scope: GitDiffScope, base: Option<&str>) -> GitDiffResult {
    tcode_services::git::load_git_diff(cwd, scope, base)
}

pub fn open_in_zed(cwd: &Path) -> io::Result<()> {
    tcode_services::desktop::open_in_zed(cwd)
}

pub fn read_file_bytes(path: &Path) -> io::Result<Vec<u8>> {
    tcode_services::user_files::read_bytes(path)
}

pub fn remove_user_file(path: &Path) -> io::Result<()> {
    tcode_services::user_files::remove_file(path)
}

pub fn relativize_to_workspace(path: &str, cwd: &Path) -> String {
    tcode_services::user_files::relativize_to_workspace(path, cwd)
}

pub fn is_directory(path: &Path) -> bool {
    tcode_services::user_files::is_directory(path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalImportUpdate {
    Progress {
        done: usize,
        total: usize,
        tool: String,
    },
    Finished {
        imported: usize,
        skipped: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpMarketplaceItem {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub installed: bool,
    pub installing: bool,
    pub supported: bool,
}
