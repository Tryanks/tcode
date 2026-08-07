//! Runtime-owned views and temporary DTO re-exports for UI migration.

use std::io;
use std::path::Path;

pub use tcode_services::import::{ExternalThread, RecentDir, SourceTool};
pub use tcode_services::workspace::PathEntry;

pub fn open_in_zed(cwd: &Path) -> io::Result<()> {
    tcode_services::desktop::open_in_zed(cwd)
}

pub fn relativize_to_workspace(path: &str, cwd: &Path) -> String {
    tcode_services::user_files::relativize_to_workspace(path, cwd)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

pub use tcode_protocol::AcpMarketplaceItem;
