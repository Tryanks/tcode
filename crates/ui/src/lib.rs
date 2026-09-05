mod acp_panel;
#[cfg(feature = "desktop")]
mod add_project_dialog;
#[cfg(not(feature = "desktop"))]
#[path = "add_project_dialog_portable.rs"]
mod add_project_dialog;
mod app_activation;
pub mod assets;
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
pub mod markdown;
// The phone shell paints with the same material tiers as the desktop
// (docs/mobile-design.md §3.0), so the module is part of the crate API.
pub mod material;
mod orchestrate_settings;
pub mod overlay;
pub mod palette;
mod pasteboard;
mod plan_panel;
#[cfg(feature = "desktop")]
mod preview_panel;
#[cfg(not(feature = "desktop"))]
#[path = "preview_panel_portable.rs"]
mod preview_panel;
pub(crate) mod provider_card;
mod provider_dialog;
mod provider_model_picker;
pub(crate) mod provider_models;
pub(crate) mod provider_status;
#[cfg(feature = "remote")]
pub mod remote;
pub(crate) mod runtime_event;
mod scroll;
pub mod settings;
#[cfg(feature = "desktop")]
mod settings_page;
#[cfg(not(feature = "desktop"))]
#[path = "settings_page_portable.rs"]
mod settings_page;
mod shell;
mod shortcut;
pub mod sidebar;
pub mod sizing;
pub mod store;
#[cfg(feature = "terminal")]
mod terminal_drawer;
pub mod theme;
#[cfg(feature = "desktop")]
mod thread_export;
#[cfg(not(feature = "desktop"))]
#[path = "thread_export_portable.rs"]
mod thread_export;
pub mod time;
pub(crate) mod toast;
pub(crate) mod usage;
pub mod widgets;
mod window_caption;
mod window_state;
mod workspace_walk;

pub use i18n::{
    LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE, apply_locale, resolve_locale, set_locale,
    translate, translate_with_args,
};
pub(crate) use shell::window_drag_area;
pub use shell::{AppShell, Quit, TogglePalette};
pub use window_state::{OpenThread, WindowState};
