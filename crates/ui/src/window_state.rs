use std::path::PathBuf;

use gpui::{Context, Entity};
use tcode_runtime::app::AppState;

/// Window-global UI state owned by the GPUI layer.
pub struct WindowState {
    pub route: Route,
    pub palette_open: bool,
    pub sidebar_collapsed: bool,
    pub quit_prompt_epoch: u64,
    pub quit_prompt_open: bool,
    pub debug_compose: Option<String>,
    pub debug_image: Option<PathBuf>,
    pub debug_diff_scope: Option<String>,
    pub debug_diff_split: bool,
    pub debug_diff_scope_menu: bool,
    pub debug_review_comment: bool,
    pub debug_palette: Option<String>,
    pub debug_settings_section: Option<String>,
    pub debug_acp_search: Option<String>,
    pub debug_acp_dialog: bool,
    pub debug_provider_expanded: Option<String>,
    pub debug_open_commit_dialog: bool,
}

impl WindowState {
    pub fn new(sidebar_collapsed: bool) -> Self {
        Self {
            route: Route::Chat,
            palette_open: false,
            sidebar_collapsed,
            quit_prompt_epoch: 0,
            quit_prompt_open: false,
            debug_compose: None,
            debug_image: None,
            debug_diff_scope: None,
            debug_diff_split: false,
            debug_diff_scope_menu: false,
            debug_review_comment: false,
            debug_palette: None,
            debug_settings_section: None,
            debug_acp_search: None,
            debug_acp_dialog: false,
            debug_provider_expanded: None,
            debug_open_commit_dialog: false,
        }
    }

    pub fn toggle_sidebar_collapsed(
        &mut self,
        app_state: &Entity<AppState>,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        app_state.update(cx, |state, cx| {
            state.set_sidebar_collapsed(self.sidebar_collapsed, cx)
        });
        cx.notify();
    }

    /// Switch to the full-page settings route (closes the palette).
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.route = Route::Settings;
        cx.notify();
    }

    /// Return from settings to the chat workspace.
    pub fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        cx.notify();
    }

    pub fn open_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = true;
        cx.notify();
    }

    pub fn close_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        cx.notify();
    }

    pub fn toggle_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = !self.palette_open;
        cx.notify();
    }
}

/// The top-level window route: the chat workspace or the full-page settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Chat,
    Settings,
}
