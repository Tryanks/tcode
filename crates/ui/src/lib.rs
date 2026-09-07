mod acp_panel;
mod add_project_dialog;
pub mod assets;
/// The window's link to a host: its transport tasks and workspace store.
pub mod attachment;
mod attachments;
pub mod chat;
mod commit_dialog;
mod composer;
mod composer_trigger;
mod context_meter;
mod conversation_ui;
pub(crate) mod diff;
#[doc(hidden)]
pub mod gallery_support;
pub(crate) mod git;
mod highlight;
pub mod i18n;
pub mod icon;
/// macOS TCC permission status and grant flow. Compiled only where the platform
/// actually has one; every other build shows the host/unsupported note instead.
#[cfg(all(feature = "local-permissions", target_os = "macos"))]
mod local_permissions;
pub mod markdown;
// Shared material helpers are also used by the phone shell.
pub mod material;
mod orchestrate_settings;
pub mod overlay;
/// The shared pairing form: endpoint-bound fingerprints, stale-result
/// generations and fixed-origin behavior, reused by every client shell.
pub mod pairing;
pub mod palette;
mod pasteboard;
mod plan_panel;
mod preview_panel;
pub(crate) mod provider_card;
mod provider_dialog;
mod provider_model_picker;
pub(crate) mod provider_models;
pub(crate) mod provider_status;
pub mod remote;
mod run;
pub(crate) mod runtime_event;
mod scroll;
pub mod settings;
mod settings_page;
mod shell;
mod shortcut;
pub mod sidebar;
pub mod sizing;
pub mod store;
mod terminal_drawer;
mod terminal_key_bar;
pub mod theme;
mod thread_export;
pub mod time;
pub(crate) mod toast;
pub(crate) mod usage;
pub mod widgets;
mod window_caption;
/// The window's outer seam (system insets, software keyboard) and the one
/// layout rule derived from it.
pub mod window_seam;
mod window_state;
mod workspace_walk;

pub use i18n::{
    LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE, apply_locale, resolve_locale, set_locale,
    translate, translate_with_args,
};
pub use run::{ShellOptions, THEME_JSON, flattened_theme_json, last_host_target, run_shell};
pub(crate) use shell::window_drag_area;
pub use shell::{AppShell, Quit, ShellSetup, TogglePalette, handle_back};
pub use window_seam::WindowSeam;
pub use window_state::{OpenThread, WindowState};

/// Where this client may keep its own files (the WebView2 profile is the only
/// current user). Bootstrap owns the location; the UI never resolves it, so a
/// remote attachment cannot be tricked into reading the host's data directory.
static CLIENT_DATA_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_client_data_dir(dir: std::path::PathBuf) {
    let _ = CLIENT_DATA_DIR.set(dir);
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn client_data_dir() -> Option<&'static std::path::Path> {
    CLIENT_DATA_DIR.get().map(std::path::PathBuf::as_path)
}
