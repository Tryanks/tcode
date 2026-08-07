use gpui::{Context, Entity};
use tcode_protocol::Command;

use crate::store::WorkspaceStore;

/// Window-global UI state owned by the GPUI layer.
pub struct WindowState {
    pub route: Route,
    pub palette_open: bool,
    pub sidebar_collapsed: bool,
    pub quit_prompt_epoch: u64,
    pub quit_prompt_open: bool,
    pub pending_settings_section: Option<String>,
}

impl WindowState {
    pub fn new(sidebar_collapsed: bool) -> Self {
        Self {
            route: Route::Chat,
            palette_open: false,
            sidebar_collapsed,
            quit_prompt_epoch: 0,
            quit_prompt_open: false,
            pending_settings_section: None,
        }
    }

    pub fn toggle_sidebar_collapsed(
        &mut self,
        store: &Entity<WorkspaceStore>,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        store.update(cx, |store, cx| {
            store.dispatch(
                Command::SetSidebarCollapsed {
                    collapsed: self.sidebar_collapsed,
                },
                cx,
            )
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
