mod dialog;
mod notification;

pub use dialog::{AlertDialog, Dialog, DialogActions, DialogButtons, DialogContent};
pub use notification::{Notification, NotificationType};

use std::rc::Rc;

use gpui::{
    App, AppContext as _, Context, Div, ElementId, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Refineable as _, Render, Stateful, StyleRefinement, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};

use crate::theme::ActiveTheme as _;
use crate::touch_selection::WindowTouchSelectionOverlay;
use dialog::ActiveDialog;
use notification::NotificationList;

/// The release guard for popups gpui-base dismisses on a press outside them.
///
/// gpui-base closes a popover, menu or sheet on the outside press itself,
/// and GPUI then delivers that press's release, and the click it completes,
/// to whatever the popup uncovered. Arming the guard from the dismissal
/// swallows that one release; the next press disarms it, so a dismissal by
/// Escape or by choosing an item costs nothing. Mount `release_listener`
/// with the trigger, not the transient surface, which is gone by the release.
#[derive(Clone, Default)]
pub(crate) struct OutsideDismissal(Rc<std::cell::Cell<bool>>);

impl OutsideDismissal {
    pub(crate) fn new(id: ElementId, window: &mut Window, cx: &mut App) -> Self {
        window
            .use_keyed_state((id, "outside-dismissal"), cx, |_, _| Self::default())
            .read(cx)
            .clone()
    }

    /// The popup was dismissed; the release of the press that did it, if
    /// one is in progress, is swallowed.
    pub(crate) fn dismissed(&self) {
        self.0.set(true);
    }

    /// Dismissed by `event`, which is swallowed as well: a press that closes
    /// a menu or a picker does nothing else.
    pub(crate) fn consume(&self, window: &mut Window, cx: &mut App) {
        self.dismissed();
        window.prevent_default();
        cx.stop_propagation();
    }

    pub(crate) fn release_listener(&self) -> impl IntoElement {
        let armed = self.0.clone();
        gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let disarm = armed.clone();
                window.on_mouse_event(move |_: &gpui::MouseDownEvent, phase, _, _| {
                    if phase.capture() {
                        disarm.set(false);
                    }
                });
                window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, window, cx| {
                    if phase.capture() && armed.replace(false) {
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

/// tcode's per-window presentation on the Base [`gpui_base::Root`]: the
/// modal and toast layers, and the touch surfaces the window text selection
/// leaves behind.
pub(crate) struct Overlays {
    dialogs: Vec<ActiveDialog>,
    notifications: Entity<NotificationList>,
    touch_selection: Entity<WindowTouchSelectionOverlay>,
}

/// Register the overlays every window root mounts. Call before opening windows.
pub(crate) fn init(cx: &mut App) {
    gpui_base::Root::register_plugin(cx, Overlays::new);
}

impl Overlays {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            dialogs: Vec::new(),
            notifications: cx.new(|cx| NotificationList::new(window, cx)),
            touch_selection: cx.new(|cx| WindowTouchSelectionOverlay::new(window, cx)),
        }
    }

    fn update<R>(
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) -> R,
    ) -> R {
        let overlays = gpui_base::Root::read(window, cx)
            .plugin::<Self>()
            .expect("tcode_ui::overlay::init must run before the window opens");
        overlays.update(cx, |overlays, cx| f(overlays, window, cx))
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

impl gpui_base::RootPlugin for Overlays {
    fn style(&self, surface: &mut Stateful<Div>, _window: &mut Window, cx: &mut App) {
        surface.style().refine(
            &StyleRefinement::default()
                .bg(crate::material::canvas(cx))
                .text_color(cx.theme().foreground)
                .font_family(cx.theme().font_family.clone()),
        );
    }
}

impl Render for Overlays {
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
        let seam = crate::window_seam::content_insets(window);
        div()
            .absolute()
            .inset_0()
            // The edit menu floats above what was selected, under any dialog.
            .child(self.touch_selection.clone())
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
    fn has_notification<T: Sized + 'static>(&mut self, cx: &mut App) -> bool;
    fn clear_notifications(&mut self, cx: &mut App);
}

impl OverlayExt for Window {
    fn open_dialog<F>(&mut self, cx: &mut App, build: F)
    where
        F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static,
    {
        Overlays::update(self, cx, move |host, window, cx| {
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
        Overlays::update(self, cx, |host, window, cx| host.close_dialog(window, cx));
    }

    fn close_all_dialogs(&mut self, cx: &mut App) {
        Overlays::update(self, cx, |host, window, cx| {
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
        Overlays::update(self, cx, |host, window, cx| {
            host.notifications
                .update(cx, |list, cx| list.push(note, window, cx));
        });
    }

    fn remove_notification<T: Sized + 'static>(&mut self, cx: &mut App) {
        Overlays::update(self, cx, |host, window, cx| {
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
        Overlays::update(self, cx, |host, window, cx| {
            host.notifications.update(cx, |list, cx| {
                list.close((std::any::TypeId::of::<T>(), key), window, cx)
            });
        });
    }

    fn has_notification<T: Sized + 'static>(&mut self, cx: &mut App) -> bool {
        Overlays::update(self, cx, |host, _, cx| {
            host.notifications
                .read(cx)
                .contains_type(std::any::TypeId::of::<T>())
        })
    }

    fn clear_notifications(&mut self, cx: &mut App) {
        Overlays::update(self, cx, |host, window, cx| {
            host.notifications
                .update(cx, |list, cx| list.clear(window, cx));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, size};

    struct Body;

    impl Render for Body {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    fn overlays(root: &Entity<gpui_base::Root>, cx: &mut VisualTestContext) -> Entity<Overlays> {
        root.read_with(cx, |root, _| root.plugin::<Overlays>().unwrap())
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            // A frame that reuses any view keeps every debug bound of the
            // previous one, so a dismissed toast would still be found.
            window.refresh();
            _ = window.draw(cx);
        });
    }

    #[gpui::test]
    fn compact_toast_obeys_seam_replaces_and_wide_keeps_card(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (root, cx) = cx.add_window_view(|window, cx| {
            let body = cx.new(|_| Body);
            gpui_base::Root::new(body, window, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        crate::window_seam::occlude_for_test(
            cx,
            gpui::Edges {
                bottom: px(300.),
                ..Default::default()
            },
        );
        cx.update(|window, cx| {
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
        overlays(&root, cx).read_with(cx, |root, cx| {
            root.notifications.read(cx).assert_messages(cx, &["Second"]);
        });
        cx.simulate_resize(size(px(1200.), px(800.)));
        crate::window_seam::occlude_for_test(cx, gpui::Edges::default());
        cx.update(|window, cx| {
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
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (root, cx) = cx.add_window_view(|window, cx| {
            let body = cx.new(|_| Body);
            gpui_base::Root::new(body, window, cx)
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
        overlays(&root, cx).read_with(cx, |root, cx| {
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
        overlays(&root, cx).read_with(cx, |root, cx| {
            assert_eq!(root.dialogs.len(), 1);
            root.notifications.read(cx).assert_messages(cx, &[]);
        });
    }
}
