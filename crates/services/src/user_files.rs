use std::path::{Path, PathBuf};

/// Return the directory used to persist attachments for a session.
pub fn attachment_dir(data_root: &Path, session_id: &str) -> PathBuf {
    attachments_root(data_root).join(session_id)
}

/// The directory every session's attachment directory lives under; a client
/// may only save attachments below it.
pub fn attachments_root(data_root: &Path) -> PathBuf {
    data_root.join("attachments")
}
