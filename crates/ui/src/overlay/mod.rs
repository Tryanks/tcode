mod dialog;
mod notification;

pub use dialog::{AlertDialog, Dialog, DialogActions, DialogButtons, DialogContent};
pub use notification::{Notification, NotificationType};

use std::rc::Rc;

use gpui::{
    AnyView, App, AppContext as _, Context, ElementId, Entity, IntoElement, ParentElement as _,
    Render, Styled as _, Window, div,
};

use crate::theme::ActiveTheme as _;
use dialog::ActiveDialog;
use notification::NotificationList;

/// Window root that owns tcode's modal and toast layers.
pub struct OverlayHost {
    view: AnyView,
    dialogs: Vec<ActiveDialog>,
    notifications: Entity<NotificationList>,
}

impl OverlayHost {
    pub fn new(view: impl Into<AnyView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        #[cfg(all(target_os = "macos", not(test)))]
        gpui_base::install_window_hit_test_forwarder(window);

        Self {
            view: view.into(),
            dialogs: Vec::new(),
            notifications: cx.new(|cx| NotificationList::new(window, cx)),
        }
    }

    fn update<R>(
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) -> R,
    ) -> R {
        let root = window
            .root::<Self>()
            .flatten()
            .expect("window root must be tcode_ui::overlay::OverlayHost");
        root.update(cx, |root, cx| f(root, window, cx))
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.dialogs.pop() {
            if dialog.focus_handle.contains_focused(window, cx)
                && let Some(previous) = dialog
                    .previous_focus_handle
                    .and_then(|focus| focus.upgrade())
            {
                previous.focus(window, cx);
            }
            cx.notify();
        }
    }

    fn close_all_dialogs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self
            .dialogs
            .first()
            .and_then(|dialog| dialog.previous_focus_handle.clone())
            .and_then(|focus| focus.upgrade());
        self.dialogs.clear();
        if let Some(previous) = previous {
            previous.focus(window, cx);
        }
        cx.notify();
    }
}

impl Render for OverlayHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialog_count = self.dialogs.len();
        let dialogs = self
            .dialogs
            .iter()
            .enumerate()
            .map(|(index, active)| {
                let dialog = (active.builder)(Dialog::new(cx), window, cx);
                dialog
                    .layer(index, index + 1 == dialog_count)
                    .focus_handle(active.focus_handle.clone())
            })
            .collect::<Vec<_>>();

        div()
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .child(self.view.clone())
            .children(dialogs)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .mt_4()
                    .mr_4()
                    .child(self.notifications.clone()),
            )
    }
}

/// Imperative overlay operations used by application views.
pub trait OverlayExt {
    fn open_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static;
    fn open_alert_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(AlertDialog, &mut Window, &mut App) -> AlertDialog + 'static;
    fn close_dialog(&mut self, cx: &mut App);
    fn close_all_dialogs(&mut self, cx: &mut App);
    fn push_notification(&mut self, note: impl Into<Notification>, cx: &mut App);
    fn remove_notification<T: Sized + 'static>(&mut self, cx: &mut App);
    fn remove_notification1<T: Sized + 'static>(&mut self, key: impl Into<ElementId>, cx: &mut App);
    fn clear_notifications(&mut self, cx: &mut App);
}

impl OverlayExt for Window {
    fn open_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static,
    {
        OverlayHost::update(self, cx, move |host, window, cx| {
            let focus_handle = cx.focus_handle();
            let previous_focus_handle = window.focused(cx).map(|focus| focus.downgrade());
            focus_handle.focus(window, cx);
            host.dialogs.push(ActiveDialog {
                focus_handle,
                previous_focus_handle,
                builder: Rc::new(build),
            });
            cx.notify();
        });
    }

    fn open_alert_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(AlertDialog, &mut Window, &mut App) -> AlertDialog + 'static,
    {
        self.open_dialog(cx, move |_, window, cx| {
            build(AlertDialog::new(cx), window, cx).build_surface(window, cx)
        });
    }

    fn close_dialog(&mut self, cx: &mut App) {
        OverlayHost::update(self, cx, |host, window, cx| host.close_dialog(window, cx));
    }

    fn close_all_dialogs(&mut self, cx: &mut App) {
        OverlayHost::update(self, cx, |host, window, cx| {
            host.close_all_dialogs(window, cx)
        });
    }

    fn push_notification(&mut self, note: impl Into<Notification>, cx: &mut App) {
        let note = note.into();
        OverlayHost::update(self, cx, |host, window, cx| {
            host.notifications
                .update(cx, |list, cx| list.push(note, window, cx));
        });
    }

    fn remove_notification<T: Sized + 'static>(&mut self, cx: &mut App) {
        OverlayHost::update(self, cx, |host, window, cx| {
            host.notifications.update(cx, |list, cx| {
                list.close_by_type(std::any::TypeId::of::<T>(), window, cx)
            });
        });
    }

    fn remove_notification1<T: Sized + 'static>(
        &mut self,
        key: impl Into<ElementId>,
        cx: &mut App,
    ) {
        let key = key.into();
        OverlayHost::update(self, cx, |host, window, cx| {
            host.notifications.update(cx, |list, cx| {
                list.close((std::any::TypeId::of::<T>(), key), window, cx)
            });
        });
    }

    fn clear_notifications(&mut self, cx: &mut App) {
        OverlayHost::update(self, cx, |host, window, cx| {
            host.notifications
                .update(cx, |list, cx| list.clear(window, cx));
        });
    }
}
