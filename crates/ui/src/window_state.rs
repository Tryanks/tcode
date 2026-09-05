use gpui::{Context, Entity, EventEmitter};

use crate::store::WorkspaceStore;

/// Window-global UI state owned by the GPUI layer.
pub struct WindowState {
    /// Opt-in touch layout; desktop windows keep the default rendering.
    pub compact: bool,
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
            compact: false,
            route: Route::Chat,
            palette_open: false,
            sidebar_collapsed,
            quit_prompt_epoch: 0,
            quit_prompt_open: false,
            pending_settings_section: None,
        }
    }

    pub fn with_compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }

    pub fn open_thread(&mut self, cx: &mut Context<Self>) {
        if self.compact {
            cx.emit(OpenThread);
        }
    }

    pub fn toggle_sidebar_collapsed(
        &mut self,
        store: &Entity<WorkspaceStore>,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        store.update(cx, |store, _cx| {
            store.set_sidebar_collapsed(self.sidebar_collapsed)
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

#[derive(Clone, Copy)]
pub struct OpenThread;
impl EventEmitter<OpenThread> for WindowState {}
