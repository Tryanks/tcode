mod acp_panel;
mod add_project_dialog;
mod agents_panel;
pub mod assets;
/// The window's link to a host: its transport tasks and workspace store.
pub mod attachment;
mod attachments;
mod attention;
mod change_project_root_dialog;
pub mod chat;
mod collaboration_settings;
mod commit_dialog;
mod composer;
mod composer_trigger;
mod context_meter;
mod control;
mod conversation_ui;
mod device;
pub(crate) mod diff;
mod edge_back;
mod env_editor;
mod fuzzy;
pub(crate) mod git;
mod haptics;
mod harness_dialog;
mod highlight;
mod host_file;
mod host_permissions;
pub mod html;
pub mod i18n;
pub mod icon;
mod image_viewer;
/// macOS TCC permission status and grant flow. Compiled only where the platform
/// actually has one; other attachments query the host for read-only status.
#[cfg(all(feature = "local-permissions", target_os = "macos"))]
mod local_permissions;
pub mod markdown;
// Shared material helpers are also used by the phone shell.
#[cfg(target_os = "macos")]
mod macos_backdrop;
pub mod material;
mod motion;
mod move_project_dialog;
mod move_thread_dialog;
pub mod overlay;
/// The shared pairing form: origin validation, stale-result
/// generations and fixed-origin behavior, reused by every client shell.
pub mod pairing;
pub mod palette;
mod panes;
mod pasteboard;
mod plan_panel;
mod plugins_settings;
mod preview_panel;
mod project_icon;
pub(crate) mod provider_card;
mod provider_login_dialog;
mod provider_model_picker;
pub(crate) mod provider_models;
pub(crate) mod provider_status;
mod recent_cache;
pub mod remote;
mod run;
pub(crate) mod runtime_event;
mod scroll;
pub mod scroll_capture;
pub mod settings;
mod settings_page;
pub mod settings_rows;
mod shell;
mod shortcut;
pub mod sidebar;
pub mod sizing;
mod source_control_settings;
pub mod store;
mod terminal_drawer;
mod terminal_key_bar;
#[cfg(test)]
mod test_client;
#[cfg(test)]
mod test_frames;
pub mod theme;
mod thread_export;
pub mod time;
pub(crate) mod toast;
mod touch_selection;
pub mod typography;
pub(crate) mod usage;
pub mod widgets;
mod width_frame;
mod window_caption;
pub(crate) mod window_seam;
mod window_state;
mod workspace;
mod workspace_walk;

pub use i18n::{
    LANGUAGE_ENGLISH, LANGUAGE_JAPANESE, LANGUAGE_SIMPLIFIED_CHINESE, apply_locale, resolve_locale,
    set_locale, translate, translate_with_args,
};
pub use run::{
    ShellOptions, THEME_JSON, flattened_theme_json, init_client, last_host_target,
    open_client_window, run_shell,
};
pub(crate) use shell::window_drag_area;
pub use shell::{
    AppShell, DEFAULT_WINDOW_WIDTH, Quit, ShellPlace, ShellSetup, TogglePalette, can_swipe_back,
    handle_back, update_edge_back,
};
pub use thread_export::format_size;
pub use window_seam::force_mobile_layout;
pub use window_state::WindowState;

/// Keep how the desktop window closed for the next time it opens: whether it
/// was maximized, and the size it has, or returns to, when it is not. With no
/// `size` the one already kept stays. Both are read back from
/// [`tcode_client::host::ClientPreferences`].
pub fn remember_window(
    size: Option<gpui::Size<gpui::Pixels>>,
    maximized: bool,
    cx: &mut gpui::App,
) {
    let points = |length: gpui::Pixels| length.as_f32().round() as u32;
    let size = size.map(|size| (points(size.width), points(size.height)));
    device::DevicePreferences::set_window(size, maximized, cx);
}

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

/// Whether this client can embed a browser. Views ask before offering an action
/// that needs one, so nothing renders an enabled control that cannot work.
///
/// A build with a backend offers the Preview wherever it runs; what the
/// machine lacks for it (WebKitGTK, which `gtk_dlopen` loads when the first
/// page is built, or a runtime directory for the Wayland nest's socket) is
/// said in the panel when no page opens.
pub(crate) fn preview_backend() -> bool {
    cfg!(preview_backend)
}

mod pull_requests;
