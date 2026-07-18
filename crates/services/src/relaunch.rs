//! Restart-continuity marker (`docs/computer-use.md` §Restart continuity).
//!
//! macOS applies some TCC grants (notably Screen Recording) only after the app
//! restarts, and may quit tcode from its own "Quit & Reopen" dialog. Before any
//! permission flow, the app drops a small `relaunch.json` marker into the data
//! dir recording which Settings page to reopen and which session was active.
//! On the next launch the marker is *taken* (read + deleted) so the app can
//! reopen the session, reopen Settings on the recorded page, and recheck.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A pending restart's continuity state. Written before a grant/relaunch and
/// consumed exactly once at the next startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaunchMarker {
    /// Which Settings page to reopen: `"computer_use"` or `"browser"`.
    pub reopen_settings: String,
    /// The session that was active when the marker was written, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session: Option<String>,
}

/// The marker file inside the data dir.
fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join("relaunch.json")
}

/// Persist the marker, overwriting any previous one.
pub fn write(data_dir: &Path, marker: &RelaunchMarker) -> std::io::Result<()> {
    let data = serde_json::to_vec_pretty(marker)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(marker_path(data_dir), data)
}

/// Read the marker and delete it (consume-once). Returns `None` when absent or
/// unparsable; the file is removed either way so a corrupt marker can't wedge
/// every future launch into a relaunch loop.
pub fn take(data_dir: &Path) -> Option<RelaunchMarker> {
    let path = marker_path(data_dir);
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_take_consumes_the_marker() {
        let root =
            std::env::temp_dir().join(format!("tcode-relaunch-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        // No marker yet.
        assert_eq!(take(&root), None);

        let marker = RelaunchMarker {
            reopen_settings: "computer_use".into(),
            active_session: Some("sess-42".into()),
        };
        write(&root, &marker).unwrap();

        // Taking returns it exactly once, then the file is gone.
        assert_eq!(take(&root), Some(marker));
        assert!(!marker_path(&root).exists());
        assert_eq!(take(&root), None);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn take_removes_a_corrupt_marker() {
        let root =
            std::env::temp_dir().join(format!("tcode-relaunch-corrupt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(marker_path(&root), b"not json").unwrap();

        assert_eq!(take(&root), None);
        // A corrupt marker is deleted so it can't wedge future launches.
        assert!(!marker_path(&root).exists());

        let _ = std::fs::remove_dir_all(root);
    }
}
