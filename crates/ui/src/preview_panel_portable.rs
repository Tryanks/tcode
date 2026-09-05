//! Portable preview placeholder; native webviews are a desktop-only affordance.

use gpui::{Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Window};
use gpui_base::v_flex;

use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::window_state::WindowState;

pub struct PreviewPanel;

impl PreviewPanel {
    pub fn new(
        _store: Entity<WorkspaceStore>,
        _window_state: Entity<WindowState>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self
    }

    pub fn sync_visibility(&mut self, _cx: &mut Context<Self>) {}
}

impl Render for PreviewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .text_color(cx.theme().muted_foreground)
            .child("Preview is available in the desktop build")
    }
}
