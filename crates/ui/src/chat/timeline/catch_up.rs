//! A thread that ran while it was not shown, come back to: what the reader
//! sees once its window arrives, against a real host.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext, px, size};
use tcode_protocol::{ClientMessage, ClientPayload, Subscription, Topic};

use crate::chat::ChatView;
use crate::chat::tests::{answer_event, user_event};
use crate::store::WorkspaceStore;
use crate::store::tests::LocalHostDriver;
use crate::test_frames::{draw, draw_settled};
use crate::window_state::WindowState;

fn turn(turn: usize, answer: &str) -> Vec<agent::AgentEvent> {
    vec![
        agent::AgentEvent::TurnStarted {
            turn_id: turn.to_string(),
        },
        user_event(&format!("user-{turn}"), &format!("Question {turn}")),
        answer_event(&format!("answer-{turn}"), answer),
        agent::AgentEvent::TurnCompleted {
            turn_id: turn.to_string(),
            status: agent::TurnStatus::Completed,
            usage: None,
        },
    ]
}

/// A turn of `count` records, alternating a short answer and a command whose
/// output is about five kilobytes, so 400 of them are more than a window.
fn long_run(turn: usize, count: usize) -> Vec<agent::AgentEvent> {
    let mut events = vec![
        agent::AgentEvent::TurnStarted {
            turn_id: turn.to_string(),
        },
        user_event(&format!("user-{turn}"), "Work through the whole list."),
    ];
    for index in 0..count {
        events.push(if index % 2 == 0 {
            answer_event(&format!("step-{index}"), &format!("Step {index} is done."))
        } else {
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: format!("command-{index}"),
                parent_item_id: None,
                content: agent::ItemContent::CommandExecution {
                    command: format!("check {index}"),
                    output: format!("{index} checked\n").repeat(350),
                    exit_code: Some(0),
                    status: agent::ItemStatus::Completed,
                },
            })
        });
    }
    events.push(answer_event("finished", "Every step is done."));
    events.push(agent::AgentEvent::TurnCompleted {
        turn_id: turn.to_string(),
        status: agent::TurnStatus::Completed,
        usage: None,
    });
    events
}

struct Workspace {
    host: tcode_runtime::pipe::SpawnedHost,
    driver: LocalHostDriver,
    store: Entity<WorkspaceStore>,
    /// Every message the client sent its host.
    sent: Arc<Mutex<Vec<ClientMessage>>>,
    first: String,
    second: String,
}

impl Workspace {
    /// A host with two threads, the first holding three finished turns, and
    /// a client attached to it whose messages are recorded on their way.
    fn new(cx: &mut TestAppContext) -> Self {
        use tcode_runtime::pipe::{HostServices, spawn_host};
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let root = crate::store::tests::scratch_root("catch-up");
        let disk = tcode_services::store::SessionStore::open_at(root).unwrap();
        let host = spawn_host(disk, HostServices::default()).unwrap();
        let (first, second) = smol::block_on(host.update_state_for_test(|state, cx| {
            let mut thread = |name: &str| {
                let id = state.start_draft(name.into(), std::env::temp_dir(), cx);
                let active = state.residents.live.get_mut(&id).unwrap();
                active.draft = false;
                let meta = active.meta.clone();
                state.sessions.push(meta);
                id
            };
            (thread("first"), thread("second"))
        }))
        .unwrap();
        let (to_host, requests) = async_channel::unbounded::<String>();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let recorded = sent.clone();
        let host_sender = host.to_host.clone();
        std::thread::spawn(move || {
            while let Ok(line) = requests.recv_blocking() {
                recorded
                    .lock()
                    .unwrap()
                    .push(tcode_protocol::decode_client_line(&line).unwrap());
                if host_sender.send_blocking(line).is_err() {
                    break;
                }
            }
            // The client closed its link: the host's inbox closes with it.
            host_sender.close();
        });
        let mut driver = LocalHostDriver::from_link(tcode_client::HostLink::new(
            to_host,
            host.from_host.clone(),
        ));
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        driver.wait_until(cx, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        let workspace = Self {
            host,
            driver,
            store,
            sent,
            first,
            second,
        };
        workspace.provider(
            &workspace.first,
            (0..3)
                .flat_map(|index| turn(index, "An answer.\n\nWith a second paragraph."))
                .collect(),
        );
        workspace
    }

    /// The thread's provider reports `events`, as it does whether or not
    /// any client shows the thread.
    fn provider(&self, thread: &str, events: Vec<agent::AgentEvent>) {
        let thread = thread.to_owned();
        smol::block_on(self.host.update_state_for_test(move |state, cx| {
            for event in events {
                state.provider_event_for_test(&thread, event, cx);
            }
        }))
        .unwrap();
    }

    /// Show `thread` in `chat` as the shell does when its pane switches.
    fn show(&self, chat: &Entity<ChatView>, thread: &str, cx: &mut VisualTestContext) {
        self.store
            .update(cx, |store, cx| store.show_alone(thread.to_owned(), cx));
        chat.update(cx, |chat, cx| chat.set_session(Some(thread.to_owned()), cx));
    }

    /// The subscriptions to `thread`'s events the client sent.
    fn events_subscriptions(&self, thread: &str) -> Vec<Subscription> {
        let topic = Topic::SessionEvents {
            session_id: thread.to_owned(),
        };
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| match &message.payload {
                ClientPayload::Subscribe(subscription) if subscription.topic == topic => {
                    Some(subscription.clone())
                }
                _ => None,
            })
            .collect()
    }
}

/// One display frame: time passes, the frames the window asked for are
/// delivered, and it draws.
fn frame(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(Duration::from_millis(16));
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    draw(cx);
}

/// Where every row painted in the last frame was, by row.
fn painted(
    chat: &Entity<ChatView>,
    cx: &mut VisualTestContext,
) -> Vec<(usize, gpui::Bounds<gpui::Pixels>)> {
    let rows = chat
        .read_with(cx, |chat, _| chat.timeline())
        .read_with(cx, |chat, _| chat.rows.len());
    (0..rows)
        .filter_map(|row| {
            cx.debug_bounds(format!("timeline-row-{row}").leak())
                .map(|bounds| (row, bounds))
        })
        .collect()
}

/// A thread that ran on while another was shown, come back to, is at its
/// latest entry on the first frame its window draws, brought there by one
/// subscription, with nothing in it moving; what arrives live after that
/// rises in as it always does. So for a run that fits one window (100
/// records, which the window continues) and for one longer than a window
/// (400, answered with the thread's tail).
#[gpui::test]
fn a_thread_come_back_to_is_at_its_latest_at_once(cx: &mut TestAppContext) {
    for (count, continued) in [(100, true), (400, false)] {
        let mut workspace = Workspace::new(cx);
        let window_state = cx.new(|_| WindowState::default());
        let store = workspace.store.clone();
        let window =
            cx.add_window(|window, cx| ChatView::new(store, window_state, None, window, cx));
        let chat = window.root(cx).unwrap();
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(size(px(1_024.), px(700.)));
        let (first, second) = (workspace.first.clone(), workspace.second.clone());
        workspace.show(&chat, &first, cx);
        let store = workspace.store.clone();
        workspace.driver.wait_until(cx, "the first thread", |cx| {
            workspace_last(&store, &first, cx).as_deref() == Some("answer-2")
        });
        draw_settled(cx);
        workspace.show(&chat, &second, cx);
        workspace.driver.wait_until(cx, "the second thread", |cx| {
            store.read_with(cx, |store, cx| !store.chat_loading_for(Some(&second), cx))
        });
        draw_settled(cx);

        workspace.provider(&first, long_run(3, count));
        workspace.sent.lock().unwrap().clear();
        workspace.show(&chat, &first, cx);
        workspace
            .driver
            .wait_until(cx, "the first thread's window", |cx| {
                workspace_last(&store, &first, cx).as_deref() == Some("finished")
            });
        let asked = workspace.events_subscriptions(&first);
        assert_eq!(asked.len(), 1, "{count}: one window brought it: {asked:?}");
        assert!(
            asked[0].after.is_some(),
            "{count}: it asked from what it kept"
        );
        let held_from = store.read_with(cx, |store, _| store.history_available_for(Some(&first)));
        assert_eq!(
            held_from, !continued,
            "{count}: a run that fits is continued, a longer one is the tail"
        );

        // The first frame drawn with its window is the thread at rest: the
        // frames after it, until the window asks for no more, move no row.
        draw(cx);
        let latest = chat
            .read_with(cx, |chat, _| chat.timeline())
            .read_with(cx, |chat, _| chat.rows.len() - 1);
        let first_frame = painted(&chat, cx);
        assert_eq!(
            first_frame.last().map(|(row, _)| *row),
            Some(latest),
            "{count}: the first frame shows the latest entry"
        );
        draw_settled(cx);
        assert_eq!(
            painted(&chat, cx),
            first_frame,
            "{count}: rows moved after the first frame"
        );

        // The reader asks again and the agent answers, live.
        workspace.provider(
            &first,
            vec![
                agent::AgentEvent::TurnStarted {
                    turn_id: "4".into(),
                },
                user_event("user-4", "And now?"),
                answer_event("answer-4", &"Here is what comes next.\n\n".repeat(5)),
            ],
        );
        workspace.driver.wait_until(cx, "the live answer", |cx| {
            workspace_last(&store, &first, cx).as_deref() == Some("answer-4")
        });
        draw(cx);
        let arrived = chat
            .read_with(cx, |chat, _| chat.timeline())
            .read_with(cx, |chat, _| chat.rows.len() - 1);
        let mut heights = Vec::new();
        for _ in 0..12 {
            heights.push(
                painted(&chat, cx)
                    .into_iter()
                    .find(|(row, _)| *row == arrived)
                    .map(|(_, bounds)| bounds.size.height),
            );
            frame(cx);
        }
        assert!(
            heights.windows(2).any(|pair| pair[0] < pair[1]),
            "{count}: the live answer did not rise in: {heights:?}"
        );
        workspace.driver.shutdown(cx, &workspace.host);
    }
}

fn workspace_last(
    store: &Entity<WorkspaceStore>,
    thread: &str,
    cx: &TestAppContext,
) -> Option<String> {
    store.read_with(cx, |store, cx| {
        store
            .with_timeline_for(
                Some(thread),
                |timeline| timeline.entries.last().map(|entry| entry.id.clone()),
                cx,
            )
            .flatten()
    })
}
