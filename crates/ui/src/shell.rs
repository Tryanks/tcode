use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Context, Div, ElementId, Entity, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Render, Styled as _,
    Subscription, Window, actions, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Root, WindowExt as _, h_flex,
    notification::Notification,
    resizable::{ResizableState, h_resizable, resizable_panel},
};
use tcode_runtime::app::{AppEvent, AppState, RightTab, Route};
use tcode_runtime::event::{RuntimeEvent, RuntimeOperationId};

use crate::chat::ChatView;
use crate::diff_panel::DiffPanel;
use crate::palette::CommandPalette;
use crate::preview_panel::PreviewPanel;
use crate::settings_page::SettingsPage;
use crate::sidebar::SessionsSidebar;
use crate::toast::ToastCenter;

use crate::runtime_event::{
    RuntimeEventSeverity, RuntimeToastDisposition, apply_runtime_effect, present_runtime_event,
    present_runtime_toast,
};
use crate::toast::{ToastAction, ToastId, ToastKind, ToastSpec};

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
    operation_toasts: HashMap<RuntimeOperationId, ToastId>,
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
        let toasts = cx.new(|_| ToastCenter::new());
        let window_title = |state: &AppState| -> String {
            match state.active.as_ref() {
                Some(active) if active.draft => tcode_i18n::tr!("chat.new_thread").into_owned(),
                Some(active) => active.meta.title.clone(),
                None => "tcode".to_string(),
            }
        };
        window.set_window_title(&window_title(app_state.read(cx)));
        let subscription = cx.observe_in(&app_state, window, move |_, state, window, cx| {
            window.set_window_title(&window_title(state.read(cx)));
            cx.notify();
        });
        let event_subscription =
            cx.subscribe_in(&app_state, window, |this, _, event: &AppEvent, w, cx| {
                this.present_app_event(event, w, cx);
            });
        let preview = cx.new(|cx| PreviewPanel::new(app_state.clone(), window, cx));

        // Pump preview automation requests from the MCP server into the live
        // WebView. The receiver is taken once; requests are resolved on the gpui
        // main thread (WKWebView `evaluate_script` must run there).
        let requests = app_state.update(cx, |state, _| state.take_preview_requests());
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

        app_state.update(cx, |state, cx| state.pump_orchestrate_requests(cx));

        Self {
            sidebar: cx.new(|cx| SessionsSidebar::new(app_state.clone(), cx)),
            chat: cx.new(|cx| ChatView::new(app_state.clone(), window, cx)),
            diff: cx.new(|cx| DiffPanel::new(app_state.clone(), cx)),
            preview,
            settings_page: cx.new(|cx| SettingsPage::new(app_state.clone(), window, cx)),
            palette: cx.new(|cx| CommandPalette::new(app_state.clone(), window, cx)),
            toasts,
            operation_toasts: HashMap::new(),
            app_state,
            palette_was_open: false,
            split: cx.new(|_| ResizableState::default()),
            right_width: Rc::new(Cell::new(px(RIGHT_PANEL_WIDTH))),
            right_sized: false,
            _subscriptions: vec![subscription, event_subscription],
        }
    }

    fn present_app_event(&mut self, event: &AppEvent, window: &mut Window, cx: &mut Context<Self>) {
        let toast = match event {
            RuntimeEvent::Effect(effect) => {
                apply_runtime_effect(effect);
                cx.notify();
                return;
            }
            RuntimeEvent::Error(_) | RuntimeEvent::Notice(_) => {
                let presented = present_runtime_event(event);
                let notification = match presented.severity {
                    RuntimeEventSeverity::Error => Notification::error(presented.message),
                    RuntimeEventSeverity::Success => Notification::success(presented.message),
                };
                window.push_notification(notification, cx);
                return;
            }
            RuntimeEvent::Toast(toast) => toast,
        };

        let presented = present_runtime_toast(toast);
        let toast_id = match presented.disposition {
            RuntimeToastDisposition::Push => self.toasts.update(cx, |center, cx| {
                center.push(
                    toast_spec(presented.kind, presented.title, presented.detail),
                    cx,
                )
            }),
            RuntimeToastDisposition::Start(operation) => {
                let toast_id = self.toasts.update(cx, |center, cx| {
                    center.push(
                        toast_spec(presented.kind, presented.title, presented.detail),
                        cx,
                    )
                });
                self.operation_toasts.insert(operation, toast_id);
                toast_id
            }
            RuntimeToastDisposition::Finish(operation) => {
                if let Some(toast_id) = self.operation_toasts.remove(&operation) {
                    self.toasts.update(cx, |center, cx| {
                        center.update(
                            toast_id,
                            presented.kind,
                            presented.title,
                            presented.detail.map(Into::into),
                            cx,
                        );
                    });
                    toast_id
                } else {
                    self.toasts.update(cx, |center, cx| {
                        center.push(
                            toast_spec(presented.kind, presented.title, presented.detail),
                            cx,
                        )
                    })
                }
            }
        };

        if let Some(request) = presented.retry {
            let toasts = self.toasts.clone();
            let app_state = self.app_state.clone();
            let retry = ToastAction::new(
                tcode_i18n::tr!("git.toast.retry").into_owned(),
                move |_window, cx| {
                    toasts.update(cx, |center, cx| center.dismiss(toast_id, cx));
                    let request = request.clone();
                    app_state.update(cx, |state, cx| {
                        state.run_git_action(
                            request.action,
                            request.message,
                            request.included,
                            request.feature_branch,
                            cx,
                        );
                    });
                },
            );
            self.toasts.update(cx, |center, cx| {
                center.set_actions(toast_id, vec![retry], cx)
            });
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

fn toast_spec(kind: ToastKind, title: String, detail: Option<String>) -> ToastSpec {
    let spec = if matches!(kind, ToastKind::Loading { progress: None }) {
        ToastSpec::loading(title)
    } else {
        ToastSpec::new(kind, title)
    };
    match detail {
        Some(detail) => spec.detail(detail),
        None => spec,
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

        // A native WebView is not composited into GPUI and survives removal of
        // its layout node. Synchronize it before the settings early-return or a
        // right-panel tab/close transition can unmount PreviewPanel.
        self.preview
            .update(cx, |preview, cx| preview.sync_visibility(cx));

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
