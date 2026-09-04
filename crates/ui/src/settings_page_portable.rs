//! Portable settings shell backed by the replicated workspace settings.

use gpui::{
    Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Window, div, px,
};
use gpui_base::v_flex;

use crate::settings::ThemeMode;
use crate::store::WorkspaceStore;
use crate::theme::{self, ActiveTheme as _, ThemeMode as UiThemeMode};
use crate::window_state::WindowState;

pub(crate) fn apply_theme(mode: ThemeMode, window: &mut Window, cx: &mut gpui::App) {
    match mode {
        ThemeMode::Light => theme::change_mode(UiThemeMode::Light, Some(window), cx),
        ThemeMode::Dark => theme::change_mode(UiThemeMode::Dark, Some(window), cx),
        ThemeMode::System => theme::sync_system_appearance(Some(window), cx),
    }
}

pub struct SettingsPage {
    store: Entity<WorkspaceStore>,
    _window_state: Entity<WindowState>,
}

impl SettingsPage {
    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            store,
            _window_state: window_state,
        }
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.store.read(cx).settings();
        v_flex()
            .size_full()
            .gap_2()
            .p_4()
            .bg(cx.theme().background)
            .child(div().text_size(px(20.)).child(crate::tr!("settings.title")))
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("Theme: {:?}", settings.theme_mode)),
            )
    }
}
