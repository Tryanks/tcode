//! Asking the window system to draw the user back to Tcode when a thread
//! starts waiting on a Harness approval while no Tcode window is active:
//! the Dock icon bounces once on macOS, the taskbar button flashes on
//! Windows until the window comes to the front, and an X11 window is marked
//! urgent. Activating the app brings the window, so nothing is clicked.

use std::collections::HashSet;

use gpui::{App, Window};

use crate::store::WorkspaceStore;

/// The threads one window's host has waiting on an approval.
#[derive(Default)]
pub(crate) struct ApprovalAttention {
    waiting: HashSet<String>,
}

impl ApprovalAttention {
    /// Follow `store`, saying whether a thread has just started waiting on
    /// an approval while no Tcode window is active.
    pub(crate) fn update(&mut self, store: &WorkspaceStore, cx: &App) -> bool {
        let waiting: HashSet<String> = store
            .sidebar_sessions()
            .iter()
            .filter(|meta| store.pending_approval_for(&meta.id))
            .map(|meta| meta.id.clone())
            .collect();
        let started = waiting.difference(&self.waiting).next().is_some();
        self.waiting = waiting;
        started && cx.active_window().is_none()
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
                            failed: false,
                            unread: false,
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

    /// Away from Tcode a thread that starts to wait on an approval asks for
    /// attention once, and again only after it stopped and starts again; in
    /// front the window already shows it.
    #[gpui::test]
    fn a_thread_starting_to_wait_while_tcode_is_away_asks_for_attention_once(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::theme::init);
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
        let mut attention = ApprovalAttention::default();
        let mut show = |waiting: bool, cx: &mut gpui::VisualTestContext| {
            incoming.try_send(index(&meta, waiting)).unwrap();
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.update(|_, cx| attention.update(store.read(cx), cx))
        };

        window.deactivate_window();
        window.run_until_parked();
        assert!(show(true, window));
        assert!(!show(true, window), "a thread still waiting asks once");
        assert!(!show(false, window));
        assert!(show(true, window), "waiting again asks again");
        assert!(!show(false, window));

        window.update(|window, _| window.activate_window());
        window.run_until_parked();
        assert!(!show(true, window), "the window shows it");
    }
}
