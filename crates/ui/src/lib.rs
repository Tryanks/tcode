mod acp_panel;
mod add_project_dialog;
pub mod assets;
mod attachments;
mod chat;
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
pub mod markdown;
pub(crate) mod material;
mod orchestrate_settings;
mod palette;
mod pasteboard;
mod plan_panel;
mod preview_panel;
pub(crate) mod provider_card;
mod provider_dialog;
mod provider_model_picker;
pub(crate) mod provider_models;
pub(crate) mod provider_status;
pub(crate) mod runtime_event;
pub mod settings;
mod settings_page;
mod shell;
mod shortcut;
mod sidebar;
pub mod store;
mod terminal_drawer;
pub(crate) mod time;
pub(crate) mod toast;
mod window_caption;
mod window_state;
mod workspace_walk;

pub use i18n::{
    LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE, apply_locale, resolve_locale, set_locale,
    translate, translate_with_args,
};
pub(crate) use shell::window_drag_area;
pub use shell::{AppShell, Quit, TogglePalette};
pub use window_state::WindowState;
