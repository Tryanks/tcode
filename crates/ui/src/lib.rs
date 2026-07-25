#[cfg(feature = "desktop")]
mod acp_panel;
#[cfg(feature = "desktop")]
mod add_project_dialog;
pub mod assets;
mod attachments;
mod chat;
#[cfg(feature = "desktop")]
mod commit_dialog;
#[cfg(feature = "desktop")]
mod composer;
#[cfg(feature = "desktop")]
mod composer_trigger;
#[cfg(feature = "desktop")]
mod context_meter;
pub mod diff;
#[cfg(feature = "desktop")]
pub mod git;
mod highlight;
pub mod markdown;
pub mod material;
#[cfg(feature = "desktop")]
mod orchestrate_settings;
#[cfg(feature = "desktop")]
mod palette;
#[cfg(feature = "desktop")]
mod pasteboard;
#[cfg(feature = "desktop")]
mod plan_panel;
#[cfg(feature = "desktop")]
mod preview_panel;
#[cfg(feature = "desktop")]
pub mod provider_card;
#[cfg(feature = "desktop")]
mod provider_dialog;
#[cfg(feature = "desktop")]
mod provider_model_picker;
#[cfg(feature = "desktop")]
pub mod provider_models;
#[cfg(feature = "desktop")]
pub mod provider_status;
#[cfg(feature = "desktop")]
pub mod runtime_event;
#[cfg(feature = "desktop")]
pub mod settings;
#[cfg(feature = "desktop")]
mod settings_page;
#[cfg(feature = "desktop")]
mod shell;
#[cfg(feature = "desktop")]
mod shortcut;
#[cfg(feature = "desktop")]
mod sidebar;
#[cfg(feature = "desktop")]
mod terminal_drawer;
pub mod time;
#[cfg(feature = "desktop")]
pub(crate) mod toast;
#[cfg(feature = "desktop")]
mod window_caption;
#[cfg(feature = "desktop")]
mod workspace_walk;

pub use chat::{ChatReadModel, ChatSessionReadModel, ChatView};
#[cfg(feature = "desktop")]
pub(crate) use shell::window_drag_area;
#[cfg(feature = "desktop")]
pub use shell::{AppShell, Quit, TogglePalette};
