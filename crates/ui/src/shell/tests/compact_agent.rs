//! An agent on a phone is a page of the navigation stack over the thread it
//! was reached from, as every other compact route is: it comes in over that
//! thread, an edge swipe drags it off with the thread beneath it, and Back
//! plays the same slide. The thread beneath stays what it was left as.

use gpui::TouchPhase::{Ended, Moved, Started};

use super::{
    scenes::{asked, completed, serve_log, started},
    *,
};

const WIDTH: f32 = 393.;

/// A turn of `thread-1`'s conversation, long enough to scroll.
fn turn(index: usize) -> [agent::AgentEvent; 3] {
    [
        agent::AgentEvent::TurnStarted {
            turn_id: format!("turn-{index}"),
        },
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: format!("message-{index}"),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: format!("Keep reading position {index}\n").repeat(4),
                attachments: Vec::new(),
            },
        }),
        agent::AgentEvent::TurnCompleted {
            turn_id: format!("turn-{index}"),
            status: agent::TurnStatus::Completed,
            usage: None,
        },
    ]
}

fn serve_long_lead(host: &MountedShell, id: &str) {
    let records: Vec<tcode_core::session::StoredEvent> =
        (0..24).flat_map(turn).map(Into::into).collect();
    send_events(
        host,
        vec![
            (
                Topic::SessionStatus {
                    session_id: id.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(session_status(
                    id,
                    std::path::Path::new(PANES_CWD),
                ))),
            ),
            (
                Topic::SessionEvents {
                    session_id: id.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: records.len() as u64,
                    total_turns: 24,
                    truncated: false,
                    from: 0,
                    end: records.len() as u64,
                    records,
                },
            ),
        ],
    );
}

fn serve_agent(host: &MountedShell, id: &str) {
    let status = session_status(id, std::path::Path::new(PANES_CWD));
    serve_log(host, status, vec![started(), asked(id), completed()]);
}

fn agent_bubble(id: &str) -> &'static str {
    format!("user-bubble-{id}-ask").leak()
}

/// Where the reader left `thread-1`: a question on screen, and how far down
/// the window it was.
#[derive(Clone, Copy, Debug)]
struct Reading {
    question: &'static str,
    top: Pixels,
}

impl Reading {
    /// The question is there, where it was.
    fn assert_kept(&self, cx: &mut VisualTestContext) {
        let question = cx
            .debug_bounds(self.question)
            .expect("the lead's question is drawn");
        assert_eq!(question.top(), self.top, "the lead's scroll position");
    }
}

/// `thread-1`, with `agent-1` dispatched by it, on a phone, scrolled away
/// from its tail.
fn mount_lead_scrolled(
    cx: &mut TestAppContext,
) -> (
    Entity<AppShell>,
    MountedShell,
    &mut VisualTestContext,
    Reading,
) {
    let (shell, host, cx) = mount_lead_with_agent(cx);
    as_mobile(cx);
    serve_long_lead(&host, "thread-1");
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx) == Some(24)
    });
    cx.simulate_resize(size(px(WIDTH), px(852.)));
    crate::test_frames::draw_settled(cx);
    let chat = shell.read_with(cx, |shell, cx| shell.column.as_ref().unwrap().chat(cx));
    let viewport = chat
        .read_with(cx, |chat, _| chat.timeline())
        .read_with(cx, |chat, _| chat.capture_viewport().unwrap());
    cx.simulate_event(gpui::ScrollEvent {
        position: viewport.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(1000.))),
        ..Default::default()
    });
    crate::test_frames::draw_settled(cx);
    let (question, top) = (0..24)
        .find_map(|index| {
            let selector: &'static str = format!("user-bubble-message-{index}").leak();
            let bounds = cx.debug_bounds(selector)?;
            (bounds.top() >= viewport.top() && bounds.bottom() < viewport.bottom())
                .then_some((selector, bounds.top()))
        })
        .expect("a question on screen");
    (shell, host, cx, Reading { question, top })
}

/// `thread-1`, with `agent-1` dispatched by it, on a phone.
fn mount_lead_on_phone(
    cx: &mut TestAppContext,
) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
    let (shell, host, cx) = mount_lead_with_agent(cx);
    as_mobile(cx);
    cx.simulate_resize(size(px(WIDTH), px(852.)));
    crate::test_frames::draw_settled(cx);
    (shell, host, cx)
}

/// Open `id` from the thread the page shows, as its row does.
fn open_agent(shell: &Entity<AppShell>, id: &str, cx: &mut VisualTestContext) {
    let store = store_of(shell, cx);
    cx.update(|window, cx| {
        crate::agents_panel::open_agent(&store, id.into(), None, None, window, cx)
    });
    draw(cx);
}

fn open_agent_served(
    shell: &Entity<AppShell>,
    host: &MountedShell,
    id: &str,
    cx: &mut VisualTestContext,
) {
    open_agent(shell, id, cx);
    serve_agent(host, id);
    let id = id.to_owned();
    await_restore_update(shell, cx, move |store, _| store.timeline_settled_for(&id));
    crate::test_frames::draw_settled(cx);
}

fn focused(shell: &Entity<AppShell>, cx: &mut VisualTestContext) -> Option<String> {
    shell.read_with(cx, |shell, cx| {
        shell.page_thread(cx).map(|thread| thread.session_id)
    })
}

/// An edge swipe from an agent lands on its lead, where it was left, and
/// lets the agent go.
#[gpui::test]
fn an_edge_swipe_from_an_agent_lands_on_its_lead_where_it_was_left(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx, reading) = mount_lead_scrolled(cx);
    open_agent_served(&shell, &host, "agent-1", cx);
    assert!(cx.debug_bounds(agent_bubble("agent-1")).is_some());

    touch_drag(
        cx,
        1,
        &[
            (Started, 10., 300., 0),
            (Moved, 100., 300., 200),
            (Moved, 200., 300., 200),
            (Ended, 200., 300., 10),
        ],
    );
    crate::test_frames::draw_settled(cx);
    assert_eq!(focused(&shell, cx).as_deref(), Some("thread-1"));
    assert!(cx.debug_bounds(agent_bubble("agent-1")).is_none());
    reading.assert_kept(cx);
    store_of(&shell, cx).read_with(cx, |store, _| {
        assert!(!store.is_thread_open("agent-1"));
        assert!(store.timeline_settled_for("thread-1"));
    });
}

/// The lead stays open and held under its agent, and is let go as a thread
/// left any other way once the reader goes elsewhere from the agent: the
/// window then shows that one thread, and Back leaves it for the list.
#[gpui::test]
fn a_lead_is_held_under_its_agent_until_the_reader_goes_elsewhere(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_on_phone(cx);
    open_agent_served(&shell, &host, "agent-1", cx);
    let store = store_of(&shell, cx);
    store.read_with(cx, |store, _| {
        assert!(store.is_thread_open("thread-1"));
        assert!(store.timeline_settled_for("thread-1"));
    });

    serve_thread(&host, "thread-2");
    store.update(cx, |store, cx| store.show_thread("thread-2".into(), cx));
    await_restore_update(&shell, cx, |store, _| {
        store.timeline_settled_for("thread-2")
    });
    crate::test_frames::draw_settled(cx);
    store.read_with(cx, |store, _| {
        assert!(!store.is_thread_open("thread-1"));
        assert!(!store.is_thread_open("agent-1"));
    });
    let back = cx.debug_bounds("compact-back").expect("the compact Back");
    cx.simulate_click(back.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);
    let state = shell.read_with(cx, |shell, _| shell.window_state());
    assert_eq!(
        state.read_with(cx, |state, _| state.destination()),
        Destination::Threads
    );
}

/// An agent opened from the panels page standing on its lead takes that
/// page's place: the lead is what it comes back to.
#[gpui::test]
fn an_agent_opened_from_the_panels_stands_on_its_lead(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx, reading) = mount_lead_scrolled(cx);
    shell.update(cx, |shell, cx| shell.go(Destination::Panel, cx));
    crate::test_frames::draw_settled(cx);
    open_agent_served(&shell, &host, "agent-1", cx);
    assert!(cx.debug_bounds(agent_bubble("agent-1")).is_some());

    touch_drag(
        cx,
        1,
        &[
            (Started, 10., 300., 0),
            (Moved, 100., 300., 200),
            (Moved, 200., 300., 200),
            (Ended, 200., 300., 10),
        ],
    );
    crate::test_frames::draw_settled(cx);
    assert_eq!(focused(&shell, cx).as_deref(), Some("thread-1"));
    reading.assert_kept(cx);
}

/// An agent reached any other way than from its lead (a link, a restored
/// window) has its lead beneath it, for the swipe to land on.
#[gpui::test]
fn an_agent_not_opened_from_its_lead_swipes_back_to_it(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_lead_on_phone(cx);
    let back = cx.debug_bounds("compact-back").expect("the compact Back");
    cx.simulate_click(back.center(), gpui::Modifiers::default());
    crate::test_frames::draw_settled(cx);

    let store = store_of(&shell, cx);
    serve_agent(&host, "agent-1");
    store.update(cx, |store, cx| store.show_thread("agent-1".into(), cx));
    draw(cx);
    await_restore_update(&shell, cx, |store, _| store.timeline_settled_for("agent-1"));
    crate::test_frames::draw_settled(cx);
    serve_thread(&host, "thread-1");
    await_restore_update(&shell, cx, |store, _| {
        store.timeline_settled_for("thread-1")
    });
    crate::test_frames::draw_settled(cx);

    touch_drag(
        cx,
        1,
        &[
            (Started, 10., 300., 0),
            (Moved, 100., 300., 200),
            (Moved, 200., 300., 200),
            (Ended, 200., 300., 10),
        ],
    );
    crate::test_frames::draw_settled(cx);
    assert_eq!(focused(&shell, cx).as_deref(), Some("thread-1"));
    store.read_with(cx, |store, _| assert!(!store.is_thread_open("agent-1")));
}
