use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use crate::overlay::{Notification, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Div, ElementId, Entity,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels,
    Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window, actions, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{ResizableState, h_resizable, resizable_panel};
use tcode_core::ui::RightTab;
use tcode_runtime::event::{RuntimeEffect, RuntimeEvent, RuntimeOperationId};

use crate::chat::ChatView;
use crate::diff::DiffPanel;
use crate::palette::CommandPalette;
use crate::preview_panel::PreviewPanel;
use crate::runtime_event::{
    RuntimeEventSeverity, RuntimeToastDisposition, apply_runtime_effect, present_runtime_event,
    present_runtime_toast,
};
use crate::settings_page::SettingsPage;
use crate::sidebar::SessionsSidebar;
use crate::store::WorkspaceStore;
use crate::toast::{RuntimeToastNotification, ToastAction, ToastId, ToastKind};
use crate::window_state::{Route, WindowState};

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
        window.listener_for(&state, |state, event: &MouseDownEvent, window, cx| {
            // A titlebar press must never begin text selection; once
            // `start_window_move` swallows the mouse-up, a gesture would
            // otherwise remain active while the window is dragged.
            window.prevent_default();
            gpui_base::GlobalState::suppress_text_selection(cx);
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
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    sidebar: Entity<SessionsSidebar>,
    chat: Entity<ChatView>,
    diff: Entity<DiffPanel>,
    preview: Entity<PreviewPanel>,
    settings_page: Entity<SettingsPage>,
    palette: Entity<CommandPalette>,
    operation_toasts: HashMap<RuntimeOperationId, ToastId>,
    next_toast_id: ToastId,
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
    /// Stable expanded-sidebar width. The resizable component otherwise scales
    /// every panel proportionally when the window enters or leaves fullscreen.
    sidebar_width: Rc<Cell<Pixels>>,
    /// Viewport width last seen by render; a change arms the restore below.
    last_viewport_width: Option<Pixels>,
    /// Keep restoring the sidebar width until the panel reports it, then stop
    /// so the restore never fights an in-progress drag.
    sidebar_restore_pending: bool,
    /// Collapsed-only overlay visibility. Purely transient and never persisted;
    /// expanded/non-workspace renders clear it synchronously.
    sidebar_overlay_visible: bool,
    /// Forces the production preview entity into the right-panel layout while
    /// the lifecycle smoke driver uses synthetic conversation keys.
    preview_smoke_active: bool,
    _subscriptions: Vec<Subscription>,
}

/// The right panel's default width (`docs/DESIGN.md`).
const RIGHT_PANEL_WIDTH: f32 = 560.;
const SIDEBAR_WIDTH: f32 = 255.;
/// Collapsed only: width of the window's left-edge activation region.
const SIDEBAR_HOVER_EDGE: f32 = 12.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarHoverTransition {
    Trigger(bool),
    Overlay(bool),
}

/// Apply the asymmetric sibling-hover contract. The fixed trigger can open the
/// overlay but cannot close it; once mounted, the overlay owns closing itself.
fn next_sidebar_overlay_visibility(
    currently_visible: bool,
    transition: SidebarHoverTransition,
    collapsed: bool,
    route: Route,
    popover_open: bool,
) -> bool {
    if !collapsed || route != Route::Chat {
        return false;
    }

    match transition {
        SidebarHoverTransition::Trigger(true) | SidebarHoverTransition::Overlay(true) => true,
        SidebarHoverTransition::Trigger(false) => currently_visible,
        // A menu spawned from the sidebar (new-thread dropdown, row context
        // menu) is an occluding deferred layer: hovering it reads as leaving
        // the overlay. Treat an open popover's lifetime as continued hover;
        // the render pass reaps the overlay once the popover dismisses.
        SidebarHoverTransition::Overlay(false) => currently_visible && popover_open,
    }
}

impl AppShell {
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        window.set_window_title(&workspace_store.read(cx).shell_window_title());
        let subscription = cx.observe_in(&workspace_store, window, move |_, store, window, cx| {
            window.set_window_title(&store.read(cx).shell_window_title());
            cx.notify();
        });
        let event_subscription = cx.subscribe_in(
            &workspace_store,
            window,
            |this, _, event: &RuntimeEvent, w, cx| {
                this.present_app_event(event, w, cx);
            },
        );
        let window_subscription = cx.observe(&window_state, |_, _, cx| {
            cx.notify();
        });
        let preview = cx
            .new(|cx| PreviewPanel::new(workspace_store.clone(), window_state.clone(), window, cx));

        // Pump preview automation requests from the MCP server into the live
        // WebView. The receiver is taken once; requests are resolved on the gpui
        // main thread (WKWebView `evaluate_script` must run there).
        let requests = workspace_store.update(cx, |store, _cx| store.take_preview_requests());
        if let Some(requests) = requests {
            let preview = preview.clone();
            cx.spawn_in(window, async move |_, cx| {
                while let Ok(request) = requests.recv().await {
                    let preview_mcp::BrokerRequest {
                        session_id,
                        op,
                        reply,
                    } = request;
                    if preview
                        .update_in(cx, |panel, window, cx| {
                            panel.handle_op(session_id, op, reply, window, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        Self {
            sidebar: cx
                .new(|cx| SessionsSidebar::new(workspace_store.clone(), window_state.clone(), cx)),
            chat: cx
                .new(|cx| ChatView::new(workspace_store.clone(), window_state.clone(), window, cx)),
            diff: cx.new(|cx| DiffPanel::new(workspace_store.clone(), window_state.clone(), cx)),
            preview,
            settings_page: cx.new(|cx| {
                SettingsPage::new(workspace_store.clone(), window_state.clone(), window, cx)
            }),
            palette: cx.new(|cx| {
                CommandPalette::new(workspace_store.clone(), window_state.clone(), window, cx)
            }),
            operation_toasts: HashMap::new(),
            next_toast_id: 1,
            store: workspace_store,
            window_state,
            palette_was_open: false,
            split: cx.new(|_| ResizableState::default()),
            right_width: Rc::new(Cell::new(px(RIGHT_PANEL_WIDTH))),
            right_sized: false,
            sidebar_width: Rc::new(Cell::new(px(SIDEBAR_WIDTH))),
            last_viewport_width: None,
            sidebar_restore_pending: false,
            sidebar_overlay_visible: false,
            preview_smoke_active: false,
            _subscriptions: vec![subscription, event_subscription, window_subscription],
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn preview_smoke_create(
        &mut self,
        key: &str,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.preview_smoke_active = true;
        let result = self
            .preview
            .update(cx, |preview, cx| preview.smoke_create(key, url, window, cx));
        cx.notify();
        result
    }

    #[cfg(not(target_os = "linux"))]
    pub fn preview_smoke_set_visible(&mut self, key: Option<&str>, cx: &mut Context<Self>) {
        self.preview
            .update(cx, |preview, cx| preview.smoke_set_visible(key, cx));
        cx.notify();
    }

    #[cfg(not(target_os = "linux"))]
    pub fn preview_smoke_drop(&mut self, key: &str, cx: &mut Context<Self>) {
        self.preview
            .update(cx, |preview, cx| preview.smoke_drop(key, cx));
        cx.notify();
    }

    fn present_app_event(
        &mut self,
        event: &RuntimeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let toast = match event {
            RuntimeEvent::Effect(effect @ RuntimeEffect::ApplyLocale { .. }) => {
                apply_runtime_effect(effect);
                cx.notify();
                return;
            }
            RuntimeEvent::Effect(RuntimeEffect::CopyToClipboard { text }) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
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
            RuntimeToastDisposition::Push => self.take_toast_id(),
            RuntimeToastDisposition::Start(operation) => {
                let toast_id = self.take_toast_id();
                self.operation_toasts.insert(operation, toast_id);
                toast_id
            }
            RuntimeToastDisposition::Finish(operation) => self
                .operation_toasts
                .remove(&operation)
                .unwrap_or_else(|| self.take_toast_id()),
        };

        let action = presented.retry.map(|request| {
            let store = self.store.clone();
            ToastAction {
                label: crate::tr!("git.toast.retry").into_owned().into(),
                handler: Rc::new(move |window, cx| {
                    window.remove_notification1::<RuntimeToastNotification>(toast_id as usize, cx);
                    let request = request.clone();
                    store.update(cx, |store, _cx| {
                        store.run_git_action(
                            request.action,
                            request.message,
                            request.included,
                            request.feature_branch,
                        );
                    });
                }),
            }
        });
        self.show_toast(
            toast_id,
            presented.kind,
            (presented.title, presented.detail),
            action,
            window,
            cx,
        );
    }

    fn take_toast_id(&mut self) -> ToastId {
        let id = self.next_toast_id;
        self.next_toast_id += 1;
        id
    }

    fn show_toast(
        &mut self,
        id: ToastId,
        kind: ToastKind,
        content: (String, Option<String>),
        action: Option<ToastAction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, detail) = content;
        window.push_notification(
            crate::toast::notification(id, kind, title, detail.map(Into::into), action),
            cx,
        );

        let delay = match kind {
            ToastKind::Success | ToastKind::Info => Some(Duration::from_secs(4)),
            ToastKind::Warning => Some(Duration::from_secs(6)),
            ToastKind::Error | ToastKind::Loading => None,
        };
        if let Some(delay) = delay {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(delay).await;
                _ = this.update_in(cx, |_, window, cx| {
                    window.remove_notification1::<RuntimeToastNotification>(id as usize, cx);
                });
            })
            .detach();
        }
    }

    fn on_toggle_palette(
        &mut self,
        _: &TogglePalette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window_state
            .update(cx, |state, cx| state.toggle_palette(cx));
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let route = self.window_state.read(cx).route;
        let palette_open = self.window_state.read(cx).palette_open;
        let fullscreen = window.is_fullscreen();
        // Focus the palette's search input on the open transition.
        if palette_open && !self.palette_was_open {
            self.palette.update(cx, |p, cx| p.focus(window, cx));
        }
        self.palette_was_open = palette_open;
        let collapsed = self.window_state.read(cx).sidebar_collapsed;
        // The overlay is workspace-only transient state. Clear it synchronously
        // on route/expanded transitions rather than waiting for pointer input.
        if !collapsed || route != Route::Chat {
            self.sidebar_overlay_visible = false;
        }
        let panel = self.store.read(cx).shell_panel_state();
        let diff_open = panel.right_panel_open || self.preview_smoke_active;
        let right_tab = if self.preview_smoke_active {
            RightTab::Preview
        } else {
            panel.right_tab
        };
        let diff_expanded = panel.right_panel_expanded;
        // "Expanded" (full-width) is a diff-only affordance; the preview tab
        // always shares the split so the webview keeps a stable size.
        let diff_expanded = diff_expanded && right_tab != RightTab::Preview;

        // A native WebView is not composited into GPUI and survives removal of
        // its layout node. Synchronize it before the settings early-return or a
        // right-panel tab/close transition can unmount PreviewPanel.
        self.preview
            .update(cx, |preview, cx| preview.sync_visibility(cx));

        // The full-page settings route replaces the chat workspace entirely.
        // It composes exactly like the chat workspace below — Root owns the
        // translucent glass canvas; the settings page paints its own nav
        // (translucent `sidebar`) and content paper (`content_surface`) so the
        // window material is byte-identical across navigation. Painting an
        // opaque paper across the whole window here (behind the nav) is what
        // made settings feel like "a different app".
        if route == Route::Settings {
            return div()
                .id("app-shell")
                .size_full()
                // Fullscreen flattens the canvas under the paper (see the
                // workspace root below).
                .when(fullscreen, |this| {
                    this.bg(crate::material::opaque_canvas(cx))
                })
                .text_color(cx.theme().foreground)
                .on_action(cx.listener(Self::on_toggle_palette))
                // Register first so its bubble-phase handlers run after child
                // controls and own selection only when the press propagates.
                .child(gpui_base::TextSelectionLayer)
                .child(
                    div()
                        .id("workspace")
                        .flex_1()
                        .size_full()
                        .min_h_0()
                        .overflow_hidden()
                        .child(self.settings_page.clone()),
                );
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

        // gpui-component preserves panel *ratios* when its container changes
        // width. Fullscreen is a container resize, but the sidebar is a fixed
        // navigation column: restore its remembered pixel width when the window
        // width changes. Only then — an every-frame restore would snap the
        // panel back mid-drag (`on_resize` records the width only on mouse-up).
        // The rescale lands on the frame where the group measures its new
        // container, which can trail the viewport change — so keep restoring
        // until the width matches, then stop.
        let viewport_width = window.viewport_size().width;
        if self.last_viewport_width != Some(viewport_width) {
            self.last_viewport_width = Some(viewport_width);
            self.sidebar_restore_pending = true;
        }
        if self.sidebar_restore_pending && !collapsed {
            let width = self.sidebar_width.get();
            let restored = self
                .split
                .update(cx, |state, cx| match state.sizes().first() {
                    Some(size) if *size != width => {
                        state.resize_panel(0, width, window, cx);
                        false
                    }
                    Some(_) => true,
                    None => false,
                });
            if restored {
                self.sidebar_restore_pending = false;
            }
        }

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

        // The chat and right columns are reading surfaces (T1): they sit on a
        // near-opaque plane over the vibrancy canvas (docs/visual-redesign.md).
        let chat_panel = resizable_panel().visible(chat_visible).child(
            div()
                .size_full()
                .bg(crate::material::content_surface(cx))
                // T1 paper floats above the glass canvas.
                .shadow_sm()
                .child(self.chat.clone()),
        );
        let right = resizable_panel()
            .visible(diff_open)
            .size(px(RIGHT_PANEL_WIDTH))
            .size_range(px(320.)..px(1400.))
            .child(
                div()
                    .size_full()
                    .bg(crate::material::content_surface(cx))
                    // T1 paper floats above the glass canvas.
                    .shadow_sm()
                    .child(right_panel(self)),
            );

        // Remember a dragged width so re-opening the panel restores it.
        let remembered_right = self.right_width.clone();
        let remembered_sidebar = self.sidebar_width.clone();
        let group = |id: &'static str| {
            h_resizable(id)
                .with_state(&self.split)
                .on_resize(move |state, _, cx| {
                    let sizes = state.read(cx).sizes();
                    if !collapsed && let Some(size) = sizes.first() {
                        remembered_sidebar.set(*size);
                    }
                    if let Some(size) = sizes.get(right_ix) {
                        remembered_right.set(*size);
                    }
                })
        };

        let workspace: AnyElement = if collapsed {
            // Zero layout width: the chat/right group owns the whole workspace
            // and runs to the window's left edge. The trigger and overlay are
            // independent absolute siblings, so neither reflows the columns.
            let overlay_width = self.sidebar_width.get();
            // A popover keeps the overlay alive through its occluded-hover
            // false transition (see next_sidebar_overlay_visibility). When the
            // popover dismisses with the pointer already outside the overlay,
            // no hover event follows — reap the stale overlay here instead
            // (dismissal refreshes the window, so this pass always runs).
            if self.sidebar_overlay_visible
                && !gpui_base::GlobalState::is_in_deferred_context(cx)
                && window.mouse_position().x > overlay_width
            {
                self.sidebar_overlay_visible = false;
            }
            div()
                .relative()
                .size_full()
                .child(group("chat-diff-panels").child(chat_panel).child(right))
                // This fixed transparent strip only opens the overlay. Its
                // inevitable false transition when the overlay occludes it is
                // deliberately ignored by the state machine.
                .child(
                    div()
                        .id("sidebar-hover-trigger")
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(px(SIDEBAR_HOVER_EDGE))
                        .occlude()
                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                            let (collapsed, route) = {
                                let state = this.window_state.read(cx);
                                (state.sidebar_collapsed, state.route)
                            };
                            let visible = next_sidebar_overlay_visibility(
                                this.sidebar_overlay_visible,
                                SidebarHoverTransition::Trigger(*hovered),
                                collapsed,
                                route,
                                gpui_base::GlobalState::is_in_deferred_context(cx),
                            );
                            if this.sidebar_overlay_visible != visible {
                                this.sidebar_overlay_visible = visible;
                                cx.notify();
                            }
                        })),
                )
                .when(self.sidebar_overlay_visible, |this| {
                    this.child(
                        div()
                            .id("sidebar-hover-overlay")
                            .absolute()
                            .left_0()
                            .top_0()
                            .h_full()
                            .w(overlay_width)
                            // The sidebar fill is translucent, so back the
                            // floating layer with the near-opaque popover surface
                            // to prevent the chat beneath from bleeding through.
                            .bg(cx.theme().popover)
                            .shadow_lg()
                            .border_r_1()
                            .border_color(cx.theme().border)
                            // The blocker and hover listener share this hitbox:
                            // the overlay owns its full visible lifetime.
                            .occlude()
                            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                let (collapsed, route) = {
                                    let state = this.window_state.read(cx);
                                    (state.sidebar_collapsed, state.route)
                                };
                                let visible = next_sidebar_overlay_visibility(
                                    this.sidebar_overlay_visible,
                                    SidebarHoverTransition::Overlay(*hovered),
                                    collapsed,
                                    route,
                                    gpui_base::GlobalState::is_in_deferred_context(cx),
                                );
                                if this.sidebar_overlay_visible != visible {
                                    this.sidebar_overlay_visible = visible;
                                    cx.notify();
                                }
                            }))
                            .child(self.sidebar.clone()),
                    )
                })
                .into_any_element()
        } else {
            group("workspace-panels")
                .child(
                    resizable_panel()
                        .flex_none()
                        .size(px(SIDEBAR_WIDTH))
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
            // Fullscreen only: a fullscreen Space has nothing but black behind
            // the vibrancy material, which muddies the translucent canvas —
            // cover it with its opaque base. Windowed, paint nothing here:
            // Root owns the glass canvas (docs/visual-redesign.md §0).
            .when(fullscreen, |this| {
                this.bg(crate::material::opaque_canvas(cx))
            })
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_toggle_palette))
            // The window selection layer must be the first child.
            .child(gpui_base::TextSelectionLayer)
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transition(current: bool, transition: SidebarHoverTransition) -> bool {
        next_sidebar_overlay_visibility(current, transition, true, Route::Chat, false)
    }

    #[test]
    fn trigger_true_opens_overlay() {
        assert!(transition(false, SidebarHoverTransition::Trigger(true)));
    }

    #[test]
    fn trigger_false_preserves_current_visibility() {
        assert!(!transition(false, SidebarHoverTransition::Trigger(false)));
        assert!(transition(true, SidebarHoverTransition::Trigger(false)));
    }

    #[test]
    fn overlay_true_opens_or_keeps_overlay_open() {
        assert!(transition(false, SidebarHoverTransition::Overlay(true)));
        assert!(transition(true, SidebarHoverTransition::Overlay(true)));
    }

    #[test]
    fn overlay_false_closes_overlay() {
        assert!(!transition(true, SidebarHoverTransition::Overlay(false)));
    }

    #[test]
    fn open_popover_keeps_overlay_through_occluded_hover_loss() {
        assert!(next_sidebar_overlay_visibility(
            true,
            SidebarHoverTransition::Overlay(false),
            true,
            Route::Chat,
            true,
        ));
        // But a popover cannot conjure an overlay that is already closed.
        assert!(!next_sidebar_overlay_visibility(
            false,
            SidebarHoverTransition::Overlay(false),
            true,
            Route::Chat,
            true,
        ));
    }

    #[test]
    fn expanded_sidebar_forces_overlay_closed() {
        assert!(!next_sidebar_overlay_visibility(
            true,
            SidebarHoverTransition::Overlay(true),
            false,
            Route::Chat,
            false,
        ));
    }

    #[test]
    fn non_workspace_route_forces_overlay_closed() {
        assert!(!next_sidebar_overlay_visibility(
            true,
            SidebarHoverTransition::Overlay(true),
            true,
            Route::Settings,
            false,
        ));
    }
}
