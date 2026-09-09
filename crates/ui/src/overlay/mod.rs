mod dialog;
mod notification;

pub use dialog::{AlertDialog, Dialog, DialogActions, DialogButtons, DialogContent};
pub use notification::{Notification, NotificationType};

use std::rc::Rc;

use gpui::{
    AnyView, App, AppContext as _, Context, ElementId, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Styled as _, Window, div, prelude::FluentBuilder as _,
    px,
};

use crate::theme::ActiveTheme as _;
use dialog::ActiveDialog;
use notification::NotificationList;

/// Retains the dismissed pointer sequence after its popup content unmounts.
/// Mount `release_listener` with the trigger, not the transient surface.
#[derive(Clone, Default)]
pub(crate) struct OutsideDismissal(Rc<std::cell::Cell<Option<gpui::MouseButton>>>);

impl OutsideDismissal {
    pub(crate) fn new(id: ElementId, window: &mut Window, cx: &mut App) -> Self {
        window
            .use_keyed_state((id, "outside-dismissal"), cx, |_, _| Self::default())
            .read(cx)
            .clone()
    }

    pub(crate) fn consume(&self, event: &gpui::MouseDownEvent, window: &mut Window, cx: &mut App) {
        self.0.set(Some(event.button));
        window.prevent_default();
        cx.stop_propagation();
    }

    pub(crate) fn release_listener(&self) -> impl IntoElement {
        let pending = self.0.clone();
        gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, window, cx| {
                    if phase.capture() && pending.get() == Some(event.button) {
                        pending.set(None);
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .size_0()
    }
}

/// Window root that owns tcode's modal and toast layers.
pub struct OverlayHost {
    view: AnyView,
    dialogs: Vec<ActiveDialog>,
    notifications: Entity<NotificationList>,
}

struct DetachedView;

impl Render for DetachedView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
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

        // Dialogs and toasts share the shell's one safe content rectangle: a
        // dialog centred in the raw window would sit under a notch, and a toast
        // pinned to the corner would sit under the status bar.
        let compact = crate::window_seam::window_is_compact(window, cx);
        let seam = crate::window_seam::WindowSeam::current(cx).content_insets();
        div()
            .relative()
            .size_full()
            .on_key_down(|event, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if event.keystroke.key == "tab"
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                    && !modifiers.function
                {
                    let step = |window: &mut Window, cx: &mut App| {
                        if modifiers.shift {
                            window.focus_prev(cx);
                        } else {
                            window.focus_next(cx);
                        }
                    };
                    let before = window.focused(cx);
                    let trap = gpui_base::active_focus_trap(window, cx);
                    step(window, cx);
                    if let Some(trap) = trap {
                        let first = window.focused(cx);
                        while !trap.contains_focused(window, cx) {
                            step(window, cx);
                            if window.focused(cx) == first {
                                if let Some(before) = before {
                                    before.focus(window, cx);
                                }
                                break;
                            }
                        }
                    }
                    cx.stop_propagation();
                }
            })
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .font_family(cx.theme().font_family.clone())
            .child(self.view.clone())
            .when(!dialogs.is_empty(), |root| {
                root.child(
                    div()
                        .absolute()
                        .inset_0()
                        .pt(seam.top)
                        .pb(seam.bottom)
                        .pl(seam.left)
                        .pr(seam.right)
                        .children(dialogs),
                )
            })
            .child(
                div()
                    .debug_selector(|| "notification-position".into())
                    .absolute()
                    .when(compact, |el| {
                        el.left(seam.left + px(16.))
                            .right(seam.right + px(16.))
                            .bottom(seam.bottom + px(16.))
                            .flex()
                            .justify_center()
                    })
                    .when(!compact, |el| {
                        el.top_0()
                            .right_0()
                            .mt(seam.top + px(16.))
                            .mr(seam.right + px(16.))
                    })
                    .child(self.notifications.clone()),
            )
    }
}

fn open_notification_dialog(note: Entity<Notification>, window: &mut Window, cx: &mut App) {
    let title = note.read(cx).dialog_title();
    window.open_dialog(cx, move |dialog, _, _| {
        let note = note.clone();
        dialog
            .when_some(title.clone(), |dialog, title| dialog.title(title))
            .content(move |content, window, cx| {
                content.child(note.update(cx, |note, cx| note.dialog_content(window, cx)))
            })
    });
}

/// Imperative overlay operations used by application views.
pub trait OverlayExt {
    /// Release the current view graph while its replacement is constructed.
    fn detach_view(&mut self, cx: &mut App);
    fn replace_view(&mut self, view: impl Into<AnyView>, cx: &mut App);
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
    fn detach_view(&mut self, cx: &mut App) {
        let view = cx.new(|_| DetachedView).into();
        OverlayHost::update(self, cx, move |host, window, cx| {
            host.close_all_dialogs(window, cx);
            host.notifications
                .update(cx, |list, cx| list.clear(window, cx));
            host.view = view;
            cx.notify();
        });
    }

    fn replace_view(&mut self, view: impl Into<AnyView>, cx: &mut App) {
        let view = view.into();
        OverlayHost::update(self, cx, move |host, window, cx| {
            host.close_all_dialogs(window, cx);
            host.notifications
                .update(cx, |list, cx| list.clear(window, cx));
            host.view = view;
            cx.notify();
        });
    }

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
        if crate::window_seam::window_is_compact(self, cx) && note.requires_dialog() {
            let note = cx.new(|_| note);
            open_notification_dialog(note, self, cx);
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, WindowInsets, size};

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    #[gpui::test]
    fn compact_toast_obeys_seam_replaces_and_wide_keeps_card(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let body = cx.new(|_| DetachedView);
            OverlayHost::new(body, window, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.update(|window, cx| {
            cx.set_global(crate::window_seam::WindowSeam::new(|| {
                let mut insets = WindowInsets::default();
                insets.safe_area.bottom = px(34.);
                insets.ime.bottom = px(300.);
                insets
            }));
            window.push_notification(Notification::success("Copied"), cx);
        });
        draw(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        draw(cx);
        let bounds = cx.debug_bounds("compact-toast").expect("compact pill");
        assert!((bounds.center().x - px(196.5)).abs() < px(1.));
        assert!(
            (bounds.bottom() - px(536.)).abs() < px(1.),
            "bounds: {bounds:?}"
        );
        assert!(bounds.size.width <= px(361.));
        assert!(bounds.size.height >= px(44.));
        cx.update(|window, cx| window.push_notification("Second", cx));
        root.read_with(cx, |root, cx| {
            root.notifications.read(cx).assert_messages(cx, &["Second"]);
        });
        cx.simulate_resize(size(px(1200.), px(800.)));
        cx.update(|window, cx| {
            cx.set_global(crate::window_seam::WindowSeam::flush());
            window.push_notification("Wide", cx);
        });
        draw(cx);
        assert!(cx.debug_bounds("compact-toast").is_none());
        let wide = cx.debug_bounds("wide-toast").expect("wide corner card");
        assert_eq!(wide.size.width, px(356.));
        let position = cx.debug_bounds("notification-position").unwrap();
        assert_eq!(position.top(), px(16.));
        assert_eq!(position.right(), px(1184.));
    }

    #[gpui::test]
    fn compact_timeout_and_error_recovery(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let body = cx.new(|_| DetachedView);
            OverlayHost::new(body, window, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.update(|window, cx| window.push_notification("Copied", cx));
        draw(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        draw(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        draw(cx);
        assert!(cx.debug_bounds("compact-toast").is_none());
        cx.update(|window, cx| {
            window.push_notification(
                Notification::info("Removed")
                    .action(|_, _, _| crate::widgets::Button::new("undo").label("Undo")),
                cx,
            )
        });
        draw(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        draw(cx);
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        draw(cx);
        assert!(cx.debug_bounds("compact-toast").is_some());
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(2));
        draw(cx);
        assert!(cx.debug_bounds("compact-toast").is_none());
        cx.update(|window, cx| {
            window.push_notification(Notification::error("Repair required").autohide(false), cx)
        });
        draw(cx);
        root.read_with(cx, |root, cx| {
            assert_eq!(root.dialogs.len(), 1);
            root.notifications.read(cx).assert_messages(cx, &[]);
        });
        cx.update(|window, cx| window.close_dialog(cx));
        cx.simulate_resize(size(px(1200.), px(800.)));
        cx.update(|window, cx| {
            window.push_notification(Notification::error("Wide recovery").autohide(false), cx)
        });
        draw(cx);
        assert!(cx.debug_bounds("wide-toast").is_some());
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        root.read_with(cx, |root, cx| {
            assert_eq!(root.dialogs.len(), 1);
            root.notifications.read(cx).assert_messages(cx, &[]);
        });
    }
}
