//! Drawing the user back to Tcode when a thread starts waiting on a Harness
//! approval while no Tcode window is active. The window system is asked for
//! attention: the Dock icon bounces once on macOS, the taskbar button flashes
//! on Windows until the window comes to the front, and an X11 window is
//! marked urgent. Activating the app brings the window, so nothing is
//! clicked. Where this device turned system notifications on, the thread is
//! also posted to the operating system's notification center: a click opens
//! it, and the notification is taken back once the thread stops waiting.

use std::collections::{HashMap, HashSet};

use gpui::{
    App, Entity, Global, SharedString, SystemNotification, SystemNotificationResponse, WeakEntity,
    Window,
};

use crate::device::DevicePreferences;
use crate::store::WorkspaceStore;

/// The threads one window's host has waiting on an approval.
#[derive(Default)]
pub(crate) struct ApprovalAttention {
    waiting: HashSet<String>,
}

impl ApprovalAttention {
    /// Follow `store`, posting and taking back its threads' notifications,
    /// and say whether a thread has just started waiting on an approval
    /// while no Tcode window is active.
    pub(crate) fn update(&mut self, store: &Entity<WorkspaceStore>, cx: &mut App) -> bool {
        let read = store.read(cx);
        let waiting: HashSet<String> = read
            .sidebar_sessions()
            .iter()
            .filter(|meta| read.pending_approval_for(&meta.id))
            .map(|meta| meta.id.clone())
            .collect();
        let started: Vec<String> = waiting.difference(&self.waiting).cloned().collect();
        for thread in self.waiting.difference(&waiting) {
            Notices::withdraw(store, thread, cx);
        }
        self.waiting = waiting;
        let away = cx.active_window().is_none();
        if away && DevicePreferences::system_notifications(cx) {
            for thread in &started {
                Notices::post(store, thread, cx);
            }
        }
        away && !started.is_empty()
    }
}

/// The notifications this process has posted and not taken back, by tag,
/// each with the thread a click opens. One for every window: the platform
/// has one response handler, and a thread two windows show is posted once.
#[derive(Default)]
struct Notices(HashMap<SharedString, (WeakEntity<WorkspaceStore>, String)>);

impl Global for Notices {}

impl Notices {
    fn tag(store: &Entity<WorkspaceStore>, thread: &str, cx: &App) -> SharedString {
        format!("{}/{thread}", store.read(cx).host_key().name()).into()
    }

    fn post(store: &Entity<WorkspaceStore>, thread: &str, cx: &mut App) {
        let tag = Self::tag(store, thread, cx);
        let read = store.read(cx);
        let Some(meta) = read.thread_meta(thread) else {
            return;
        };
        let notification = SystemNotification {
            tag: tag.clone(),
            title: meta.title.clone().into(),
            body: crate::tr!("attention.waiting", harness = read.harness_name_of(meta))
                .into_owned()
                .into(),
            actions: Vec::new(),
        };
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
            cx.on_system_notification_response(Self::open);
        }
        let posted = &mut cx.global_mut::<Self>().0;
        if posted.contains_key(&tag) {
            return;
        }
        posted.insert(tag, (store.downgrade(), thread.to_owned()));
        cx.show_system_notification(notification);
    }

    fn withdraw(store: &Entity<WorkspaceStore>, thread: &str, cx: &mut App) {
        let tag = Self::tag(store, thread, cx);
        if cx
            .try_global::<Self>()
            .is_some_and(|posted| posted.0.contains_key(&tag))
        {
            cx.global_mut::<Self>().0.remove(&tag);
            cx.dismiss_system_notification(&tag);
        }
    }

    /// A click brings Tcode to the front on the thread it was about.
    fn open(response: SystemNotificationResponse, cx: &mut App) {
        cx.activate(true);
        let Some((store, thread)) = cx
            .try_global::<Self>()
            .and_then(|posted| posted.0.get(&response.tag))
            .cloned()
        else {
            return;
        };
        let _ = store.update(cx, |store, cx| store.show_thread(thread, cx));
    }
}

/// Ask for the user's attention to `window`. GPUI's request bounces the Dock
/// icon once on macOS (AppKit cancels it when the app is activated) and sets
/// the X11 urgency hint, which it clears on activation; its Wayland backend
/// has no attention request, so a Wayland session gets none. On Windows GPUI
/// flashes the taskbar button once; it flashes here until the window comes
/// to the front. Phones and browsers ask for nothing.
pub(crate) fn request(window: &Window) {
    #[cfg(all(target_os = "windows", feature = "window-attention"))]
    {
        flash_until_foreground(window);
    }
    #[cfg(not(any(
        all(target_os = "windows", feature = "window-attention"),
        target_family = "wasm",
        target_os = "ios",
        target_os = "android"
    )))]
    {
        window.request_attention();
    }
    #[cfg(any(target_family = "wasm", target_os = "ios", target_os = "android"))]
    {
        let _ = window;
    }
}

#[cfg(all(target_os = "windows", feature = "window-attention"))]
fn flash_until_foreground(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx,
    };
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let info = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd: HWND(handle.hwnd.get() as *mut _),
        dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: `info` is a valid FLASHWINFO for this window's own HWND; the
    // return value only says whether the button was highlighted before.
    let _ = unsafe { FlashWindowEx(&info) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use std::collections::HashMap;
    use tcode_core::project::SessionMeta;
    use tcode_protocol::{
        EventEnvelope, ForkAvailability, HostMessage, IndexSnapshot, IndexSummary, ServerEvent,
        SessionActivity, Topic, encode_line,
    };

    /// The host's index with one thread, waiting on an approval or not.
    fn index(meta: &SessionMeta, waiting: bool) -> String {
        encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: IndexSummary {
                    activity: HashMap::from([(
                        meta.id.clone(),
                        SessionActivity {
                            working: false,
                            turn_running: false,
                            background: Default::default(),
                            unsettled: 0,
                            wakes: Default::default(),
                            waiting_for_approval: waiting,
                            waiting_for_input: false,
                            settle_blocked: false,
                            failed: false,
                            unread: false,
                            last_activity_at: None,
                            fork: ForkAvailability::Available,
                            agent: None,
                            latest_activity: None,
                        },
                    )]),
                    ..Default::default()
                },
                sessions: vec![meta.clone()],
                projects: Vec::new(),
            }),
        }))
        .unwrap()
    }

    /// A store attached to a scripted host, with one thread whose index the
    /// test sends, and a window that is not in front.
    fn away(
        cx: &mut TestAppContext,
    ) -> (
        Entity<WorkspaceStore>,
        async_channel::Sender<String>,
        SessionMeta,
        &mut gpui::VisualTestContext,
    ) {
        cx.update(crate::test_client::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump = link.clone();
        cx.background_executor
            .spawn(async move { pump.pump().await })
            .detach();
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        let meta = SessionMeta::new(agent::ProviderKind::Codex, "/work".into(), None);
        let window: &mut gpui::VisualTestContext = cx.add_empty_window();
        window.deactivate_window();
        window.run_until_parked();
        (store, incoming, meta, window)
    }

    fn show(
        attention: &mut ApprovalAttention,
        store: &Entity<WorkspaceStore>,
        incoming: &async_channel::Sender<String>,
        meta: &SessionMeta,
        waiting: bool,
        cx: &mut gpui::VisualTestContext,
    ) -> bool {
        incoming.try_send(index(meta, waiting)).unwrap();
        cx.run_until_parked();
        cx.run_until_parked();
        cx.update(|_, cx| attention.update(store, cx))
    }

    /// Away from Tcode a thread that starts to wait on an approval asks for
    /// attention once, and again only after it stopped and starts again; in
    /// front the window already shows it.
    #[gpui::test]
    fn a_thread_starting_to_wait_while_tcode_is_away_asks_for_attention_once(
        cx: &mut TestAppContext,
    ) {
        let (store, incoming, meta, window) = away(cx);
        let mut attention = ApprovalAttention::default();
        let mut show = |waiting, cx: &mut gpui::VisualTestContext| {
            show(&mut attention, &store, &incoming, &meta, waiting, cx)
        };

        assert!(show(true, window));
        assert!(!show(true, window), "a thread still waiting asks once");
        assert!(!show(false, window));
        assert!(show(true, window), "waiting again asks again");
        assert!(!show(false, window));

        window.update(|window, _| window.activate_window());
        window.run_until_parked();
        assert!(!show(true, window), "the window shows it");
    }

    /// System notifications are off until this device turns them on. Then a
    /// thread that starts waiting while Tcode is away is posted once under
    /// its title, taken back when it stops waiting, and a click on it opens
    /// that thread.
    #[gpui::test]
    fn a_device_that_turned_notifications_on_is_told_and_a_click_opens_the_thread(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| cx.set_app_identity("com.tryanks.tcode", "Tcode"));
        let (store, incoming, meta, window) = away(cx);
        let mut attention = ApprovalAttention::default();
        let mut show = |waiting, cx: &mut gpui::VisualTestContext| {
            show(&mut attention, &store, &incoming, &meta, waiting, cx)
        };

        assert!(show(true, window), "the window still asks for attention");
        assert!(window.shown_system_notifications().is_empty());
        assert!(!show(false, window));
        assert!(window.dismissed_system_notifications().is_empty());

        window.update(|_, cx| DevicePreferences::set_system_notifications(true, cx));
        show(true, window);
        show(true, window);
        let shown = window.shown_system_notifications();
        assert_eq!(shown.len(), 1, "a thread still waiting is posted once");
        assert_eq!(shown[0].title.as_ref(), meta.title);

        let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let _opened = window.update({
            let opened = opened.clone();
            |_, cx| {
                cx.subscribe(&store, move |_, shown: &crate::store::ShowThread, _| {
                    opened.borrow_mut().push(shown.session_id.clone())
                })
            }
        });
        window.simulate_system_notification_response(gpui::SystemNotificationResponse {
            tag: shown[0].tag.clone(),
            action_id: None,
        });
        window.run_until_parked();
        assert_eq!(*opened.borrow(), std::slice::from_ref(&meta.id));

        show(false, window);
        assert_eq!(
            window.dismissed_system_notifications(),
            std::slice::from_ref(&shown[0].tag)
        );
        assert!(window.delivered_system_notifications().is_empty());
    }
}
