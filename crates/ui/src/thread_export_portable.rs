//! Portable fallback for platforms without a native save dialog.

use std::path::PathBuf;

use gpui::{Context, Entity, Window};
use tcode_protocol::ThreadExportFormat;

use crate::store::WorkspaceStore;

pub(crate) fn prompt_thread_export<V: 'static>(
    _store: Entity<WorkspaceStore>,
    _session_id: String,
    _title: String,
    _directory: PathBuf,
    _format: ThreadExportFormat,
    _window: &mut Window,
    _cx: &mut Context<V>,
) {
    log::debug!("the native thread export dialog is unavailable on this target");
}
