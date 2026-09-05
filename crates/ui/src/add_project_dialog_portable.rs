//! Portable fallback for platforms without a native directory picker.

use gpui::{App, Entity, Window};

use crate::store::WorkspaceStore;

pub(super) fn open(_store: Entity<WorkspaceStore>, _window: &mut Window, _cx: &mut App) {
    log::debug!("the native Add Project dialog is unavailable on this target");
}
