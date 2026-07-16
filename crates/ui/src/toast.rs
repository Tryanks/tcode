//! Rich in-app toast system (ported from T3's `ui/toast.tsx` + `toast.logic.ts`).
//!
//! Unlike gpui-component's fire-and-forget `Notification` (kept for trivial
//! notices), a [`ToastCenter`] toast is a live, updateable entity: a
//! long-running flow pushes one toast and mutates it in place through its
//! lifecycle (queued → running → success/error) via the returned [`ToastId`].
//! Toasts support kinds (success/info/warning/error/loading-with-progress), an
//! expandable detail body, optional action buttons, auto-dismiss for terminal
//! states, and a stacked bottom-right layout.

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

/// Opaque handle to a pushed toast, used to update or dismiss it in place.
pub type ToastId = u64;

/// The visual/semantic kind of a toast. `Loading` optionally carries a 0.0–1.0
/// progress fraction (rendered as a thin bar); the rest are terminal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToastKind {
    Success,
    Info,
    Warning,
    Error,
    Loading { progress: Option<f32> },
}

impl ToastKind {
    fn is_terminal(self) -> bool {
        !matches!(self, ToastKind::Loading { .. })
    }

    fn icon(self) -> IconName {
        match self {
            ToastKind::Success => IconName::CircleCheck,
            ToastKind::Info => IconName::Info,
            ToastKind::Warning => IconName::TriangleAlert,
            ToastKind::Error => IconName::CircleX,
            ToastKind::Loading { .. } => IconName::LoaderCircle,
        }
    }
}

/// Callback invoked when a toast action button is clicked.
pub type ToastActionHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// An action button rendered in a toast's action row.
#[derive(Clone)]
pub struct ToastAction {
    pub label: SharedString,
    pub handler: ToastActionHandler,
}

impl ToastAction {
    pub fn new(
        label: impl Into<SharedString>,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            handler: Rc::new(handler),
        }
    }
}

/// A toast specification passed to [`ToastCenter::push`].
pub struct ToastSpec {
    pub kind: ToastKind,
    pub title: SharedString,
    /// Optional secondary/expandable body (e.g. a command's failure output).
    pub detail: Option<SharedString>,
    pub actions: Vec<ToastAction>,
    /// Auto-dismiss when the toast reaches a terminal, non-error state. Errors
    /// persist so the user can read/copy the detail.
    pub auto_dismiss: bool,
}

impl ToastSpec {
    pub fn new(kind: ToastKind, title: impl Into<SharedString>) -> Self {
        Self {
            kind,
            title: title.into(),
            detail: None,
            actions: Vec::new(),
            auto_dismiss: true,
        }
    }

    pub fn loading(title: impl Into<SharedString>) -> Self {
        Self::new(ToastKind::Loading { progress: None }, title)
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

struct Toast {
    id: ToastId,
    kind: ToastKind,
    title: SharedString,
    detail: Option<SharedString>,
    actions: Vec<ToastAction>,
    auto_dismiss: bool,
    expanded: bool,
}

/// The stacked toast overlay. Rendered by `AppShell` on top of the workspace;
/// mutated by long-running flows through [`ToastId`] handles.
pub struct ToastCenter {
    toasts: Vec<Toast>,
    next_id: ToastId,
}

impl Default for ToastCenter {
    fn default() -> Self {
        Self {
            toasts: Vec::new(),
            next_id: 1,
        }
    }
}

impl ToastCenter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a new toast, returning its id for later in-place updates.
    pub fn push(&mut self, spec: ToastSpec, cx: &mut Context<Self>) -> ToastId {
        let id = self.next_id;
        self.next_id += 1;
        self.toasts.push(Toast {
            id,
            kind: spec.kind,
            title: spec.title,
            detail: spec.detail,
            actions: spec.actions,
            auto_dismiss: spec.auto_dismiss,
            expanded: false,
        });
        self.arm_auto_dismiss(id, spec.kind, spec.auto_dismiss, cx);
        cx.notify();
        id
    }

    /// Replace an existing toast's kind/title/detail in place (progress flows).
    /// Clears the previous action row unless the caller re-adds actions via
    /// [`Self::set_actions`]. No-op if the id is unknown (already dismissed).
    pub fn update(
        &mut self,
        id: ToastId,
        kind: ToastKind,
        title: impl Into<SharedString>,
        detail: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let auto_dismiss = if let Some(toast) = self.toasts.iter_mut().find(|t| t.id == id) {
            toast.kind = kind;
            toast.title = title.into();
            toast.detail = detail;
            toast.actions.clear();
            toast.auto_dismiss
        } else {
            return;
        };
        self.arm_auto_dismiss(id, kind, auto_dismiss, cx);
        cx.notify();
    }

    /// Replace the action row of an existing toast.
    pub fn set_actions(&mut self, id: ToastId, actions: Vec<ToastAction>, cx: &mut Context<Self>) {
        if let Some(toast) = self.toasts.iter_mut().find(|t| t.id == id) {
            toast.actions = actions;
            cx.notify();
        }
    }

    /// Dismiss a toast by id.
    pub fn dismiss(&mut self, id: ToastId, cx: &mut Context<Self>) {
        self.toasts.retain(|t| t.id != id);
        cx.notify();
    }

    fn toggle_expanded(&mut self, id: ToastId, cx: &mut Context<Self>) {
        if let Some(toast) = self.toasts.iter_mut().find(|t| t.id == id) {
            toast.expanded = !toast.expanded;
            cx.notify();
        }
    }

    /// Arm an auto-dismiss timer when `kind` is a terminal, non-error state.
    fn arm_auto_dismiss(
        &self,
        id: ToastId,
        kind: ToastKind,
        auto_dismiss: bool,
        cx: &mut Context<Self>,
    ) {
        if !auto_dismiss || !kind.is_terminal() || matches!(kind, ToastKind::Error) {
            return;
        }
        let millis = match kind {
            ToastKind::Warning => 6_000,
            _ => 4_000,
        };
        cx.spawn(async move |this, cx| {
            smol::Timer::after(Duration::from_millis(millis)).await;
            let _ = this.update(cx, |center, cx| {
                // Only dismiss if the toast is still in the same terminal state
                // (a later `update` back to Loading re-arms nothing and keeps it).
                if center.toasts.iter().any(|t| t.id == id && t.kind == kind) {
                    center.dismiss(id, cx);
                }
            });
        })
        .detach();
    }

    fn accent(kind: ToastKind, cx: &App) -> (gpui::Hsla, gpui::Hsla) {
        match kind {
            ToastKind::Success => (cx.theme().success, cx.theme().success_foreground),
            ToastKind::Info | ToastKind::Loading { .. } => {
                (cx.theme().info, cx.theme().info_foreground)
            }
            ToastKind::Warning => (cx.theme().warning, cx.theme().warning_foreground),
            ToastKind::Error => (cx.theme().danger, cx.theme().danger_foreground),
        }
    }

    fn render_toast(&self, toast: &Toast, cx: &mut Context<Self>) -> impl IntoElement {
        let (accent, accent_foreground) = Self::accent(toast.kind, cx);
        let id = toast.id;

        let icon = Icon::new(toast.kind.icon())
            .small()
            .text_color(accent_foreground);

        let mut header = h_flex()
            .w_full()
            .gap_2()
            .items_start()
            .child(div().flex_none().mt_0p5().child(icon))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.))
                    .font_medium()
                    .child(toast.title.clone()),
            );

        // Dismiss control (top-right) for terminal toasts.
        if toast.kind.is_terminal() {
            header = header.child(
                Button::new(("toast-dismiss", id as usize))
                    .ghost()
                    .xsmall()
                    .rounded(crate::material::radius_button())
                    .icon(IconName::Close)
                    .on_click(cx.listener(move |center, _, _, cx| {
                        center.dismiss(id, cx);
                    })),
            );
        }

        let mut card = v_flex().flex_1().min_w_0().p_3().gap_2().child(header);

        // Progress bar for loading toasts with a known fraction.
        if let ToastKind::Loading {
            progress: Some(fraction),
        } = toast.kind
        {
            let pct = fraction.clamp(0., 1.) * 100.;
            card = card.child(
                div()
                    .w_full()
                    .h(px(4.))
                    .rounded_full()
                    .bg(cx.theme().muted)
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(accent)
                            .w(gpui::relative(pct / 100.)),
                    ),
            );
        }

        // Expandable detail body ("Show details" / "Hide details").
        if let Some(detail) = toast.detail.clone() {
            let expanded = toast.expanded;
            card = card.child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .id(("toast-detail-toggle", id as usize))
                            .flex_none()
                            .cursor_pointer()
                            .text_size(px(11.))
                            .font_medium()
                            .text_color(cx.theme().muted_foreground)
                            .child(if expanded {
                                tcode_i18n::tr!("toast.hide_details")
                            } else {
                                tcode_i18n::tr!("toast.show_details")
                            })
                            .on_click(cx.listener(move |center, _, _, cx| {
                                center.toggle_expanded(id, cx);
                            })),
                    )
                    .when(expanded, |this| {
                        this.child(
                            div()
                                .id(("toast-detail-scroll", id as usize))
                                .max_h(px(160.))
                                .overflow_y_scroll()
                                .p_2()
                                .rounded(crate::material::radius_input())
                                .bg(cx.theme().muted)
                                .text_size(px(11.))
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_color(cx.theme().foreground)
                                .child(detail),
                        )
                    }),
            );
        }

        // Action row (copy-detail for errors + caller-supplied actions).
        let mut action_row = h_flex().w_full().gap_2().justify_end();
        let mut has_actions = false;
        if matches!(toast.kind, ToastKind::Error)
            && let Some(detail) = toast.detail.clone()
        {
            has_actions = true;
            action_row = action_row.child(
                Button::new(("toast-copy", id as usize))
                    .ghost()
                    .xsmall()
                    .rounded_full()
                    .bg(accent.opacity(0.12))
                    .text_color(accent_foreground)
                    .icon(IconName::Copy)
                    .label(tcode_i18n::tr!("toast.copy_error"))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(detail.to_string()));
                    }),
            );
        }
        for action in &toast.actions {
            has_actions = true;
            let handler = action.handler.clone();
            action_row = action_row.child(
                Button::new(("toast-action", id as usize))
                    .outline()
                    .xsmall()
                    .rounded_full()
                    .bg(accent.opacity(0.12))
                    .text_color(accent_foreground)
                    .label(action.label.clone())
                    .on_click(move |_, window, cx| handler(window, cx)),
            );
        }
        if has_actions {
            card = card.child(action_row);
        }

        // A thin left semantic rail echoes the event-card language.
        crate::material::overlay_contour(
            h_flex()
                .w(px(360.))
                .items_stretch()
                .rounded(crate::material::radius_overlay())
                .overflow_hidden(),
            cx,
        )
        .child(div().flex_none().w(px(2.)).rounded_full().bg(accent))
        .child(card)
    }
}

impl Render for ToastCenter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.toasts.is_empty() {
            return div();
        }
        // Newest at the bottom of the visual stack, anchored bottom-right.
        let items: Vec<_> = self
            .toasts
            .iter()
            .map(|toast| self.render_toast(toast, cx).into_any_element())
            .collect();
        div()
            .absolute()
            .bottom(px(16.))
            .right(px(16.))
            .child(v_flex().gap_2().items_end().children(items))
    }
}
