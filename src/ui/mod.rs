mod acp_panel;
mod add_project_dialog;
mod attachments;
mod chat;
mod commit_dialog;
mod composer;
mod composer_trigger;
mod context_meter;
mod diff_panel;
mod palette;
mod plan_panel;
mod preview_panel;
pub mod provider_card;
mod settings_page;
mod sidebar;
mod terminal_drawer;
pub(crate) mod toast;
mod workspace_walk;

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Context, Div, ElementId, Entity, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Render, Styled as _,
    Subscription, Window, actions, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Root, h_flex,
    resizable::{ResizableState, h_resizable, resizable_panel},
};

use chat::ChatView;
use diff_panel::DiffPanel;
use palette::CommandPalette;
use preview_panel::PreviewPanel;
use settings_page::SettingsPage;
use sidebar::SessionsSidebar;
pub(crate) use toast::ToastCenter;

use crate::app::{AppState, RightTab, Route};

actions!(tcode, [Quit, TogglePalette]);

/// Transient per-frame state backing [`window_drag_area`].
struct WindowDragState {
    should_move: bool,
}

impl Render for WindowDragState {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Make `el` a window-drag handle (the window has no separate titlebar, so the
/// column top rows are the drag surface). Mirrors gpui-component's `TitleBar`
/// mechanics: a press arms a move, the first drag calls `start_window_move`.
/// Child buttons that stop propagation on mouse-down won't arm a drag.
pub(crate) fn window_drag_area(
    id: impl Into<ElementId>,
    el: Div,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    let state = window.use_keyed_state(id, cx, |_, _| WindowDragState { should_move: false });
    el.on_mouse_down_out(window.listener_for(&state, |state, _, _, _| {
        state.should_move = false;
    }))
    .on_mouse_down(
        MouseButton::Left,
        window.listener_for(&state, |state, event: &MouseDownEvent, window, _| {
            // Double-click zooms/maximizes the window like a native titlebar.
            if event.click_count >= 2 {
                state.should_move = false;
                window.titlebar_double_click();
            } else {
                state.should_move = true;
            }
        }),
    )
    .on_mouse_up(
        MouseButton::Left,
        window.listener_for(&state, |state, _, _, _| {
            state.should_move = false;
        }),
    )
    .on_mouse_move(window.listener_for(&state, |state, _, window, _| {
        if state.should_move {
            state.should_move = false;
            window.start_window_move();
        }
    }))
}

pub struct AppShell {
    app_state: Entity<AppState>,
    sidebar: Entity<SessionsSidebar>,
    chat: Entity<ChatView>,
    diff: Entity<DiffPanel>,
    preview: Entity<PreviewPanel>,
    settings_page: Entity<SettingsPage>,
    palette: Entity<CommandPalette>,
    toasts: Entity<ToastCenter>,
    /// Tracks the palette's open state across frames so it can be focused on the
    /// open transition.
    palette_was_open: bool,
    /// The workspace split's state, owned here rather than left to the group's
    /// internal state so the right panel can be given a real width when it
    /// opens. A fresh panel's size starts at the group's 100px minimum and the
    /// group latches the *first* width it measures — which, for a panel that
    /// appears mid-session, is that minimum. Left alone, the diff panel opens
    /// pinned to its 320px floor.
    split: Entity<ResizableState>,
    /// The width the right panel opens at: the default until the user drags a
    /// handle, then whatever they chose (for this run).
    right_width: Rc<Cell<Pixels>>,
    /// Whether the open right panel has already been given its width.
    right_sized: bool,
    _subscriptions: Vec<Subscription>,
}

/// The right panel's default width (`docs/DESIGN.md`).
const RIGHT_PANEL_WIDTH: f32 = 560.;

impl AppShell {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let window_title = |state: &AppState| -> String {
            match state.active.as_ref() {
                Some(active) if active.draft => rust_i18n::t!("chat.new_thread").into_owned(),
                Some(active) => active.meta.title.clone(),
                None => "tcode".to_string(),
            }
        };
        window.set_window_title(&window_title(app_state.read(cx)));
        let subscription = cx.observe_in(&app_state, window, move |_, state, window, cx| {
            window.set_window_title(&window_title(state.read(cx)));
            cx.notify();
        });
        let preview = cx.new(|cx| PreviewPanel::new(app_state.clone(), window, cx));

        // Pump preview automation requests from the MCP server into the live
        // WebView. The receiver is taken once; requests are resolved on the gpui
        // main thread (WKWebView `evaluate_script` must run there).
        let requests = app_state.update(cx, |state, _| state.preview_requests.take());
        if let Some(requests) = requests {
            let preview = preview.clone();
            cx.spawn_in(window, async move |_, cx| {
                while let Ok(request) = requests.recv().await {
                    let preview_mcp::BrokerRequest { op, reply } = request;
                    if preview
                        .update_in(cx, |panel, window, cx| {
                            panel.handle_op(op, reply, window, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        // The rich toast overlay, shared with AppState so long-running git flows
        // can mutate a single progress toast in place.
        let toasts = cx.new(|_| ToastCenter::new());
        app_state.update(cx, |state, _| state.set_toast_center(toasts.clone()));

        Self {
            sidebar: cx.new(|cx| SessionsSidebar::new(app_state.clone(), cx)),
            chat: cx.new(|cx| ChatView::new(app_state.clone(), window, cx)),
            diff: cx.new(|cx| DiffPanel::new(app_state.clone(), cx)),
            preview,
            settings_page: cx.new(|cx| SettingsPage::new(app_state.clone(), window, cx)),
            palette: cx.new(|cx| CommandPalette::new(app_state.clone(), window, cx)),
            toasts,
            app_state,
            palette_was_open: false,
            split: cx.new(|_| ResizableState::default()),
            right_width: Rc::new(Cell::new(px(RIGHT_PANEL_WIDTH))),
            right_sized: false,
            _subscriptions: vec![subscription],
        }
    }

    fn on_toggle_palette(
        &mut self,
        _: &TogglePalette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.app_state
            .update(cx, |state, cx| state.toggle_palette(cx));
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sheet_layer = Root::render_sheet_layer(window, cx);
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);
        let route = self.app_state.read(cx).route;
        let palette_open = self.app_state.read(cx).palette_open;
        // Focus the palette's search input on the open transition.
        if palette_open && !self.palette_was_open {
            self.palette.update(cx, |p, cx| p.focus(window, cx));
        }
        self.palette_was_open = palette_open;
        let collapsed = self.app_state.read(cx).sidebar_collapsed;
        let diff_open = self.app_state.read(cx).diff_panel_open();
        let right_tab = self.app_state.read(cx).right_tab();
        // "Expanded" (full-width) is a diff-only affordance; the preview tab
        // always shares the split so the webview keeps a stable size.
        let diff_expanded =
            self.app_state.read(cx).diff_panel_expanded() && right_tab != RightTab::Preview;

        // The full-page settings route replaces the chat workspace entirely.
        if route == Route::Settings {
            return div()
                .id("app-shell")
                .size_full()
                .bg(cx.theme().background)
                .text_color(cx.theme().foreground)
                .on_action(cx.listener(Self::on_toggle_palette))
                .child(self.settings_page.clone())
                .child(self.toasts.clone())
                .children(sheet_layer)
                .children(dialog_layer)
                .children(notification_layer);
        }

        // Which entity fills the right panel: the Preview tab shows the embedded
        // browser; Diff/Plan share the DiffPanel container.
        let right_panel = |shell: &Self| -> AnyElement {
            if right_tab == RightTab::Preview {
                shell.preview.clone().into_any_element()
            } else {
                shell.diff.clone().into_any_element()
            }
        };

        // Sidebar | chat | right panel live in ONE resizable group. Nesting a
        // second group inside the chat panel does not shrink the chat: it keeps
        // its full width and the right panel is painted over it, clipping the
        // timeline and the composer mid-word. A flat group makes the chat a real
        // flex sibling of the right panel, so it reflows — the guarantee
        // `docs/DESIGN.md` makes for the chat column.
        let chat_visible = !(diff_open && diff_expanded);
        // The right panel is the last of three panels (sidebar · chat · right),
        // and the sidebar is only a panel when it is expanded.
        let right_ix = if collapsed { 1 } else { 2 };

        // Give the right panel its width once the group knows about it (the
        // panel count is synced while the group renders, so this lands on the
        // frame after it opens — the group notifies, so that frame comes).
        if diff_open && chat_visible {
            if !self.right_sized {
                let width = self.right_width.get();
                let sized = self.split.update(cx, |state, cx| {
                    if state.sizes().len() > right_ix {
                        state.resize_panel(right_ix, width, window, cx);
                        true
                    } else {
                        false
                    }
                });
                self.right_sized = sized;
            }
        } else {
            self.right_sized = false;
        }

        let chat_panel = resizable_panel()
            .visible(chat_visible)
            .child(self.chat.clone());
        let right = resizable_panel()
            .visible(diff_open)
            .size(px(RIGHT_PANEL_WIDTH))
            .size_range(px(320.)..px(1400.))
            .child(right_panel(self));

        // Remember a dragged width so re-opening the panel restores it.
        let remembered = self.right_width.clone();
        let group = |id: &'static str| {
            h_resizable(id)
                .with_state(&self.split)
                .on_resize(move |state, _, cx| {
                    if let Some(size) = state.read(cx).sizes().get(right_ix) {
                        remembered.set(*size);
                    }
                })
        };

        let workspace: AnyElement = if collapsed {
            // `h_flex` centers its children on the cross axis, so the icon strip
            // needs an explicit full height — without it it (and the whole chat
            // column) float vertically centered in the window.
            h_flex()
                .size_full()
                .child(
                    div()
                        .flex_none()
                        .w(px(48.))
                        .h_full()
                        .child(self.sidebar.clone()),
                )
                .child(group("chat-diff-panels").child(chat_panel).child(right))
                .into_any_element()
        } else {
            group("workspace-panels")
                .child(
                    resizable_panel()
                        .flex_none()
                        .size(px(255.))
                        .size_range(px(220.)..px(380.))
                        .child(self.sidebar.clone()),
                )
                .child(chat_panel)
                .child(right)
                .into_any_element()
        };

        // No separate titlebar: the sidebar and chat columns run to the window
        // top (the native traffic lights overlay the sidebar's top-left).
        div()
            .id("app-shell")
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_toggle_palette))
            .child(
                div()
                    .id("workspace")
                    .flex_1()
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .child(workspace),
            )
            .when(palette_open, |this| this.child(self.palette.clone()))
            .child(self.toasts.clone())
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}
