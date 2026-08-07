use std::rc::Rc;

use gpui::{
    App, ClipboardItem, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    notification::{Notification, NotificationType},
    v_flex,
};

pub type ToastId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Info,
    Warning,
    Error,
    Loading,
}

pub type ToastActionHandler = Rc<dyn Fn(&mut Window, &mut App)>;

pub struct ToastAction {
    pub label: SharedString,
    pub handler: ToastActionHandler,
}

pub struct RuntimeToastNotification;

pub fn notification(
    id: ToastId,
    kind: ToastKind,
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    action: Option<ToastAction>,
) -> Notification {
    let mut notification = Notification::new()
        .id1::<RuntimeToastNotification>(id as usize)
        .message(title)
        .autohide(false);
    notification = match kind {
        ToastKind::Success => notification.with_type(NotificationType::Success),
        ToastKind::Info => notification.with_type(NotificationType::Info),
        ToastKind::Warning => notification.with_type(NotificationType::Warning),
        ToastKind::Error => notification.with_type(NotificationType::Error),
        ToastKind::Loading => notification.icon(Icon::new(IconName::LoaderCircle)),
    };

    if let Some(detail) = detail {
        notification = notification.content(move |_, window, cx| {
            let expanded =
                window.use_keyed_state(("toast-detail-expanded", id as usize), cx, |_, _| false);
            let is_expanded = *expanded.read(cx);
            let toggle_label = if is_expanded {
                crate::tr!("toast.hide_details")
            } else {
                crate::tr!("toast.show_details")
            };
            let mut content = v_flex().mt_2().gap_2().child(
                Button::new(("toast-detail-toggle", id as usize))
                    .ghost()
                    .xsmall()
                    .label(toggle_label)
                    .on_click(window.listener_for(&expanded, |expanded, _, _, cx| {
                        *expanded = !*expanded;
                        cx.notify();
                    })),
            );
            if is_expanded {
                content = content.child(
                    div()
                        .id(("toast-detail-scroll", id as usize))
                        .max_h_40()
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(crate::material::radius_input())
                        .bg(cx.theme().muted)
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(detail.clone()),
                );
            }
            if kind == ToastKind::Error {
                let detail = detail.clone();
                content = content.child(
                    h_flex().justify_end().child(
                        Button::new(("toast-copy", id as usize))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .label(crate::tr!("toast.copy_error"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    detail.to_string(),
                                ));
                            }),
                    ),
                );
            }
            content.into_any_element()
        });
    }

    if let Some(action) = action {
        let handler = action.handler;
        let label = action.label;
        notification = notification.action(move |_, _, _| {
            let handler = handler.clone();
            Button::new(("toast-action", id as usize))
                .outline()
                .label(label.clone())
                .on_click(move |_, window, cx| handler(window, cx))
        });
    }

    notification
}
