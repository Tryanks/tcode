use super::*;
use crate::chat::tests::*;
use crate::test_frames::{draw, draw_settled};
use agent::ItemStatus;
use gpui::TestAppContext;
use std::sync::atomic::Ordering;
use tcode_core::session::TurnMeta;

#[gpui::test]
fn a_paused_reader_on_the_live_window_stays_put_as_it_folds(cx: &mut TestAppContext) {
    use gpui::{ListOffset, Modifiers, VisualTestContext, px};
    use tcode_core::session::SteeringStatus;

    let tool = |id: &str| {
        entry(
            id,
            EntryContent::Item(ItemContent::ToolCall {
                image_reads: Vec::new(),
                name: "read".into(),
                input: serde_json::json!({ "path": id }),
                output: None,
                status: ItemStatus::Completed,
                family: None,
            }),
        )
    };
    let mut timeline = Timeline::default();
    timeline.turns = vec![TurnMeta {
        running: true,
        ..TurnMeta::default()
    }];
    timeline.entries.push(entry("user", user_item("go")));
    for index in 0..20 {
        timeline.entries.push(tool(&format!("tool-{index}")));
    }
    // Steers queued behind the live window keep content below it.
    for index in 0..12 {
        timeline.entries.push(entry(
            &format!("steer-{index}"),
            EntryContent::Steer {
                text: format!("queued steer {index}"),
                status: SteeringStatus::Pending,
                attachments: Vec::new(),
            },
        ));
    }

    let mut thread = ScriptedThread::new(cx, log_of(&timeline), 0, true);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (view, cx) = thread.open(cx);
    let cx: &mut VisualTestContext = cx;
    thread.deliver(cx);
    draw_settled(cx);
    view.update(cx, |chat, cx| {
        chat.list_state.scroll_to(ListOffset::default());
        cx.notify();
    });
    draw_settled(cx);
    let header = cx
        .debug_bounds("worklog-header-0-tool-0")
        .expect("collapsed work log");
    cx.simulate_click(header.center(), Modifiers::default());
    draw_settled(cx);
    // The reader rests two activities into the live window.
    view.read_with(cx, |chat, _| chat.timeline())
        .update(cx, |chat, cx| {
            let live = chat
                .rows
                .iter()
                .position(|row| row.part == RowPart::WorkLogLive)
                .expect("an expanded live work log");
            chat.list_state.scroll_to(ListOffset {
                item_ix: live,
                offset_in_item: px(70.),
            });
            cx.notify();
        });
    draw_settled(cx);
    let reading = cx
        .debug_bounds("activity-row-tool-18")
        .expect("the activity under the reader")
        .top();
    let top = |cx: &mut VisualTestContext| {
        cx.debug_bounds("activity-row-tool-18")
            .map(|bounds| bounds.top())
    };

    let update = |thread: &mut ScriptedThread, events, cx: &mut VisualTestContext| {
        thread.send_window(events);
        thread.deliver(cx);
        draw_settled(cx);
        draw_settled(cx);
    };
    let completed = |id: &str| match &tool(id).content {
        EntryContent::Item(content) => agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: content.clone(),
        }),
        _ => unreachable!("a tool is an item"),
    };
    update(
        &mut thread,
        vec![completed("tool-20"), completed("tool-21")],
        cx,
    );
    assert_eq!(
        top(cx),
        Some(reading),
        "new activities folding the live window moved the reader"
    );

    thread.set_running(false);
    update(
        &mut thread,
        vec![agent::AgentEvent::TurnCompleted {
            turn_id: "turn-0".into(),
            status: agent::TurnStatus::Completed,
            usage: None,
        }],
        cx,
    );
    // Settled, the activity takes the folded rows' indent but keeps its
    // line.
    assert_eq!(
        top(cx),
        Some(reading),
        "the live window settling moved the reader"
    );
}

/// A `tcode_control` anchor opens its thread at the turn of the entry it
/// names: here the user message of a turn above the window the thread
/// opens with, in a log whose streamed replies the host merges on the
/// wire, so no held record index stands for a log position.
#[gpui::test]
fn an_anchor_link_opens_its_thread_at_the_anchored_turn(cx: &mut TestAppContext) {
    use gpui::{VisualTestContext, px, size};

    const TURNS: u64 = 12;
    const DELTAS: u64 = 40;
    // Turn `t` takes the rows from `t * (DELTAS + 5)`; its user message is
    // the second, and the position a `tcode_control` anchor names it by.
    let events = {
        let mut events = Vec::new();
        for turn in 0..TURNS {
            let answer = format!("answer-{turn}");
            events.push(agent::AgentEvent::TurnStarted {
                turn_id: turn.to_string(),
            });
            events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: format!("user-{turn}"),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: format!("Question {turn}"),
                    attachments: Vec::new(),
                },
            }));
            events.push(agent::AgentEvent::ItemStarted(agent::ThreadItem {
                id: answer.clone(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: String::new(),
                    summarized: false,
                },
            }));
            events.extend((0..DELTAS).map(|_| agent::AgentEvent::Delta {
                item_id: answer.clone(),
                kind: agent::DeltaKind::AssistantText,
                text: "word ".into(),
            }));
            events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: answer,
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "word ".repeat(DELTAS as usize),
                    summarized: false,
                },
            }));
            events.push(agent::AgentEvent::TurnCompleted {
                turn_id: turn.to_string(),
                status: agent::TurnStatus::Completed,
                usage: None,
            });
        }
        events
    };
    let rows = events.len() as u64;
    let (host, session_id) = seeded_log_host(cx, events);
    cx.update(|cx| cx.set_reduce_motion(true));
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
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
    let link = driver.link.clone();
    let page = driver.await_with_events(
        cx,
        link.query(tcode_protocol::Query::SessionHistoryPage {
            session_id: session_id.clone(),
            before: u64::MAX,
            limit: 1,
        }),
    );
    assert!(
        matches!(
            page,
            Ok(tcode_protocol::QueryResponse::SessionHistoryPage { end, .. }) if end == rows
        ),
        "the recorded events are the log's only rows: {page:?}"
    );
    let window_state = cx.new(|_| WindowState::default());
    let (chat, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
        ChatView::new(
            store.clone(),
            window_state,
            Some(session_id.clone()),
            window,
            cx,
        )
    });
    let cx: &mut VisualTestContext = cx;
    cx.simulate_resize(size(px(1_024.), px(700.)));
    crate::store::tests::place_shown_threads(&store, cx);
    // The shell binds its pane to the thread the link opens.
    chat.update(cx, |chat, cx| {
        chat.set_session(Some(session_id.clone()), cx)
    });

    let opened = |cx: &mut VisualTestContext| {
        let view = chat.clone();
        let (row, highlighted, first) =
            view.read_with(cx, |chat, _| chat.timeline())
                .read_with(cx, |chat, cx| {
                    let row = chat.list_state.logical_scroll_top().item_ix;
                    (
                        chat.rows.get(row).map(|row| row.turn),
                        chat.highlighted_turn,
                        chat.workspace_store
                            .read(cx)
                            .absolute_turn_for(chat.session_id.as_deref(), 0),
                    )
                });
        let turn = row.filter(|turn| highlighted == Some(first + *turn))?;
        store
            .read_with(cx, |store, cx| {
                store.with_timeline_for(
                    store.sole_thread().as_deref(),
                    |timeline| {
                        timeline
                            .entries
                            .iter()
                            .find(|entry| {
                                entry.turn == turn
                                    && matches!(
                                        entry.content,
                                        EntryContent::Item(ItemContent::UserMessage { .. })
                                    )
                            })
                            .map(|entry| entry.id.clone())
                    },
                    cx,
                )
            })
            .flatten()
    };
    // Turn 1 is above the opening window; turn 9 is inside it, in the
    // thread already open.
    for turn in [1, 9] {
        let expected = format!("user-{turn}");
        store.update(cx, |store, cx| {
            let position = turn * (DELTAS + 5) + 1;
            store.show_thread_at_position(session_id.clone(), position, cx);
        });
        driver.settle_until(cx, &expected, |cx| {
            opened(cx).as_deref() == Some(expected.as_str())
        });
    }
    driver.shutdown(cx, &host);
}

/// A reader following the tail keeps following while the last message
/// grows in place. A scroll up, however slow, leaves the reply streaming
/// below the reader, until they are back at the end.
#[gpui::test]
fn a_streaming_reply_is_followed_until_the_reader_scrolls_up(cx: &mut TestAppContext) {
    use gpui::{VisualTestContext, px};

    let user = |turn: usize| {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: format!("user-{turn}"),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: format!("Question {turn}"),
                attachments: Vec::new(),
            },
        })
    };
    // Earlier turns fill more than a screen above the streaming one.
    let mut events = Vec::new();
    for turn in 0..4 {
        events.extend([
            agent::AgentEvent::TurnStarted {
                turn_id: turn.to_string(),
            },
            user(turn),
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: format!("answer-{turn}"),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "An earlier answer.\n\n".repeat(8),
                    summarized: false,
                },
            }),
            agent::AgentEvent::TurnCompleted {
                turn_id: turn.to_string(),
                status: agent::TurnStatus::Completed,
                usage: None,
            },
        ]);
    }
    events.extend([
        agent::AgentEvent::TurnStarted {
            turn_id: "4".into(),
        },
        user(4),
        agent::AgentEvent::ItemStarted(agent::ThreadItem {
            id: "streaming".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: String::new(),
                summarized: false,
            },
        }),
    ]);
    let mut thread = ScriptedThread::new(cx, events, 0, true);
    // The streaming turn's working indicator rests.
    cx.update(|cx| cx.set_reduce_motion(true));
    let (view, cx) = thread.open(cx);
    let cx: &mut VisualTestContext = cx;
    thread.deliver(cx);
    draw_settled(cx);
    let list = view.read_with(cx, |chat, _| chat.list_state.clone());
    assert!(list.is_following_tail());
    assert!(
        list.max_offset_for_scrollbar().y > list.viewport_bounds().size.height,
        "the thread scrolls"
    );

    let store = thread.store.clone();
    let streamed = |cx: &TestAppContext| {
        store.read_with(cx, |store, cx| {
            store
                .with_timeline_for(
                    store.sole_thread().as_deref(),
                    |timeline| {
                        timeline
                            .entries
                            .last()
                            .and_then(|entry| match &entry.content {
                                EntryContent::Item(ItemContent::AssistantMessage {
                                    text, ..
                                }) => Some(text.len()),
                                _ => None,
                            })
                    },
                    cx,
                )
                .flatten()
                .unwrap_or(0)
        })
    };
    let mut stream = |delta: usize, cx: &mut VisualTestContext| {
        let before = streamed(cx);
        thread.stream(vec![agent::AgentEvent::Delta {
            item_id: "streaming".into(),
            kind: agent::DeltaKind::AssistantText,
            text: format!("Paragraph {delta} of the streamed answer.\n\n"),
        }]);
        thread.deliver(cx);
        assert_ne!(streamed(cx), before, "the delta arrived");
        draw_settled(cx);
    };
    for delta in 0..12 {
        stream(delta, cx);
        assert!(list.is_following_tail(), "delta {delta} paused following");
    }

    // A trackpad scrolled slowly sends packets of under a pixel, and the
    // reply goes on streaming between them. The packets take the reader
    // up from the end, by what they add up to and what streamed in below
    // since, and the reader stays where they took them.
    let scroll = |delta: f32, phase: gpui::TouchPhase, cx: &mut VisualTestContext| {
        cx.simulate_event(gpui::ScrollEvent {
            position: list.viewport_bounds().center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(delta))),
            touch_phase: phase,
            ..Default::default()
        });
        draw_settled(cx);
    };
    scroll(0., gpui::TouchPhase::Started, cx);
    for delta in 12..18 {
        stream(delta, cx);
        scroll(0.5, gpui::TouchPhase::Moved, cx);
    }
    scroll(0., gpui::TouchPhase::Ended, cx);
    let read = list.scroll_px_offset_for_scrollbar().y;
    assert!(
        !list.is_following_tail(),
        "six packets of half a pixel left the reader following"
    );
    for delta in 18..24 {
        stream(delta, cx);
        assert!(!list.is_following_tail(), "delta {delta} resumed following");
        assert_eq!(
            list.scroll_px_offset_for_scrollbar().y,
            read,
            "delta {delta} moved the reader"
        );
    }
    // Back at the end, the reader follows the reply again.
    scroll(-100_000., gpui::TouchPhase::Moved, cx);
    stream(24, cx);
    assert!(list.is_following_tail());
}

#[gpui::test]
fn scroll_pill_over_inline_image_does_not_open_lightbox(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, ScrollDelta, ScrollEvent, TouchPhase, point, px};

    let path = std::env::temp_dir().join(format!(
        "tcode-pill-image-{}-{}.png",
        std::process::id(),
        NEXT_RESIDENCY_TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    image::RgbaImage::from_pixel(400, 1000, image::Rgba([40, 80, 120, 255]))
        .save(&path)
        .unwrap();
    let mut timeline = synthetic_markdown_timeline(30);
    Arc::make_mut(timeline.entries.last_mut().unwrap()).content = assistant(&format!(
        "![shot]({})\n\n{}",
        path.display(),
        "Text after the screenshot.\n\n".repeat(80)
    ));
    let (host, session_id) = seeded_chat_host(cx, timeline, false);
    cx.update(|cx| cx.set_reduce_motion(true));
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            driver.link.clone(),
            crate::store::WorkspaceAttachment::Local,
            None,
            None,
            cx,
        )
    });
    store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
    driver.wait_until(cx, "chat baseline", |cx| {
        store.read_with(cx, |store, cx| {
            store.baseline_ready() && !store.chat_loading_for(store.sole_thread().as_deref(), cx)
        })
    });
    let window_state = cx.new(|_| WindowState::default());
    struct LoadedImageChat {
        chat: Entity<ChatView>,
        sources: Vec<gpui::ImageSource>,
        ready: std::rc::Rc<std::cell::Cell<bool>>,
    }
    impl gpui::Render for LoadedImageChat {
        fn render(
            &mut self,
            window: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            let mut loaded = true;
            for source in &self.sources {
                let gpui::ImageSource::Custom(load) = source else {
                    unreachable!("host images load through the query plane");
                };
                match load(window, cx) {
                    Some(Ok(_)) => {}
                    Some(Err(error)) => panic!("inline host image load failed: {error:?}"),
                    None => loaded = false,
                }
            }
            self.ready.set(loaded);
            self.chat.read(cx).frame()
        }
    }
    let ready = std::rc::Rc::new(std::cell::Cell::new(false));
    let image_sources = vec![crate::store::image_preview(
        store.read_with(cx, |store, _| store.image_host()),
        path.clone(),
        crate::store::MAX_PREVIEW_PIXELS as f32,
        false,
    )];
    let (root, cx) = cx.add_window_view(|window, cx| {
        let chat = cx.new(|cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        let surface = cx.new(|_| LoadedImageChat {
            chat,
            sources: image_sources,
            ready: ready.clone(),
        });
        gpui_base::Root::new(surface, window, cx)
    });
    let view = root.read_with(cx, |root, cx| {
        root.view()
            .clone()
            .downcast::<LoadedImageChat>()
            .unwrap()
            .read(cx)
            .chat
            .clone()
    });
    cx.simulate_resize(gpui::size(px(393.), px(600.)));
    driver.settle_until(cx, "the inline host image to load", |_| ready.get());
    let list = view.read_with(cx, |chat, _| chat.list_state.clone());
    let row = cx.debug_bounds("timeline-row-89").unwrap_or_else(|| {
        panic!(
            "last assistant row: selected={:?}, list_items={}",
            store.read_with(cx, |store, _| store.sole_thread()),
            list.item_count()
        )
    });
    cx.simulate_event(ScrollEvent {
        position: list.viewport_bounds().center(),
        delta: ScrollDelta::Pixels(point(px(0.), list.viewport_bounds().top() - row.top())),
        touch_phase: TouchPhase::Moved,
        ..Default::default()
    });
    draw_settled(cx);
    let pill = cx
        .debug_bounds("scroll-to-end")
        .expect("pill above the image");
    let click = |position, cx: &mut gpui::VisualTestContext| {
        cx.simulate_mouse_move(position, None, Modifiers::default());
        cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
        draw_settled(cx);
    };
    click(point(pill.center().x, pill.top() - px(40.)), cx);
    driver.settle_until(cx, "the exposed image's explicit read", |cx| {
        cx.debug_bounds("image-viewer-image").is_some()
    });
    cx.simulate_keystrokes("escape");
    draw_settled(cx);
    assert!(cx.debug_bounds("image-viewer-image").is_none());
    assert!(!list.is_following_tail());

    let pill = cx.debug_bounds("scroll-to-end").unwrap();
    click(pill.center(), cx);
    assert!(list.is_following_tail(), "pill click must reach the tail");
    assert!(
        cx.debug_bounds("image-viewer-image").is_none(),
        "pill click opened the image underneath"
    );
    driver.shutdown(cx, &host);
    std::fs::remove_file(path).unwrap();
}

/// A reader scrolled back who sends a message is taken to the end once
/// the host has it, and follows the reply from there; with the setting
/// off, or when the host refuses the send, they stay where they were.
#[gpui::test]
fn a_message_sent_from_behind_the_end_brings_the_conversation_there(cx: &mut TestAppContext) {
    use gpui::{Modifiers, point, px};
    use tcode_protocol::{ClientPayload, Command};

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Outcome {
        Delivered,
        SettingOff,
        Refused,
    }
    for case in [Outcome::Delivered, Outcome::SettingOff, Outcome::Refused] {
        let (host, session_id) = seeded_chat_host_on(
            cx,
            log_of(&synthetic_markdown_timeline(15)),
            false,
            Some(agent::ProviderKind::ClaudeCode),
            tcode_runtime::app::scripted_provider(agent::ProviderKind::ClaudeCode).launcher,
        );
        cx.update(|cx| cx.set_reduce_motion(true));
        // The host refuses a send for a thread it does not have.
        let (to_proxy, proxy) = async_channel::unbounded::<String>();
        let to_host = host.to_host.clone();
        std::thread::spawn(move || {
            while let Ok(line) = proxy.recv_blocking() {
                let mut message = tcode_protocol::decode_client_line(&line).unwrap();
                if case == Outcome::Refused
                    && let ClientPayload::Command(Command::SendTurn { session_id, .. }) =
                        &mut message.payload
                {
                    *session_id = "gone".into();
                }
                let line = tcode_protocol::encode_line(&message).unwrap();
                if to_host.send_blocking(line).is_err() {
                    break;
                }
            }
            // The client closing its link stops the host.
            to_host.close();
        });
        let mut driver = crate::store::tests::LocalHostDriver::from_link(
            tcode_client::HostLink::new(to_proxy, host.from_host.clone()),
        );
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
        if case == Outcome::SettingOff {
            store.update(cx, |store, _| store.set_scroll_to_latest_on_send(false));
            driver.wait_until(cx, "the setting off", |cx| {
                store.read_with(cx, |store, _| !store.settings().scroll_to_latest_on_send)
            });
        }
        store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, "the conversation", |cx| {
            store.read_with(cx, |store, cx| {
                !store.chat_loading_for(store.sole_thread().as_deref(), cx)
            })
        });
        let window_state = cx.new(|_| WindowState::default());
        let (view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        cx.simulate_resize(gpui::size(px(800.), px(700.)));
        draw_settled(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let height = list.viewport_bounds().size.height;
        let center = list.viewport_bounds().center();
        cx.simulate_mouse_move(center, None, Modifiers::default());
        for (phase, delta) in [
            (gpui::TouchPhase::Started, px(0.)),
            (gpui::TouchPhase::Moved, height * 1.5),
        ] {
            cx.simulate_event(gpui::ScrollEvent {
                position: center,
                delta: gpui::ScrollDelta::Pixels(point(px(0.), delta)),
                touch_phase: phase,
                ..Default::default()
            });
        }
        draw_settled(cx);
        assert!(!list.is_following_tail());
        let offset = || -list.scroll_px_offset_for_scrollbar().y;
        let away = offset();

        view.update_in(cx, |chat, window, cx| chat.focus_composer(window, cx));
        cx.simulate_input("Where were we?");
        draw(cx);
        let send = cx.debug_bounds("send-message").expect("send button");
        cx.simulate_click(send.center(), Modifiers::default());
        driver.wait_until(cx, "the send's outcome", |cx| {
            store.read_with(cx, |store, cx| {
                let deliveries = store.delivery_messages_for(Some(&session_id), cx);
                match case {
                    Outcome::Refused => deliveries.iter().any(|(.., failure, _)| failure.is_some()),
                    _ => deliveries.is_empty(),
                }
            })
        });
        draw_settled(cx);
        if case == Outcome::Delivered {
            assert!(list.is_following_tail(), "the send left the reader behind");
        } else {
            assert!(!list.is_following_tail(), "{case:?} moved the reader");
            assert!(
                (offset() - away).abs() <= px(1.),
                "{case:?} moved the reader: {away:?} -> {:?}",
                offset()
            );
        }
        driver.shutdown(cx, &host);
    }
}

/// Jump to latest shows once the reader is well behind the end; a page
/// of history or a new turn leaves the reader where they were, and the
/// jump takes them back to following the end.
#[gpui::test]
fn jump_to_latest_shows_behind_the_end_and_the_reader_keeps_their_place(cx: &mut TestAppContext) {
    use gpui::{Context, IntoElement, Render, VisualTestContext, Window, point, px};

    struct TouchChat(Entity<ChatView>);
    impl Render for TouchChat {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.0.read(cx).frame()
        }
    }
    // Five records a turn: its start, three items and its end. The
    // thread opens on its last thirty turns.
    const RECORDS: usize = 5;
    let log = |turns: usize| log_of(&synthetic_markdown_timeline(turns));
    let mut thread = ScriptedThread::new(cx, log(120), 90 * RECORDS, false);
    cx.update(|cx| cx.set_reduce_motion(true));
    let store = thread.store.clone();
    let window_state = cx.new(|_| WindowState::default());
    let (root, cx) = cx.add_window_view(|window, cx| {
        TouchChat(
            cx.new(|cx| ChatView::new(store, window_state, Some(THREAD.to_owned()), window, cx)),
        )
    });
    thread.deliver(cx);
    let view = root.read_with(cx, |root, _| root.0.clone());
    cx.simulate_resize(gpui::size(px(393.), px(852.)));
    draw_settled(cx);
    let list = view.read_with(cx, |chat, _| chat.list_state.clone());
    let height = list.viewport_bounds().size.height;
    assert!(height > px(0.));
    assert!(cx.debug_bounds("scroll-to-end").is_none());
    let scroll = |distance, phase, cx: &mut VisualTestContext| {
        cx.simulate_event(gpui::ScrollEvent {
            position: list.viewport_bounds().center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), distance)),
            touch_phase: phase,
            ..Default::default()
        });
        draw_settled(cx);
    };
    // Small wheel deltas must accumulate, rather than snapping back while
    // the viewport is still inside the one-screen follow threshold.
    for _ in 0..12 {
        scroll(height / 4., gpui::TouchPhase::Moved, cx);
    }
    assert!(
        cx.debug_bounds("scroll-to-end").is_some(),
        "jump to latest must appear three screens above the tail (position {:?}, end {:?})",
        list.logical_scroll_top(),
        list.is_scrolled_to_end()
    );
    assert!(!list.is_following_tail());
    // Reach a nonzero intra-row offset through the same real wheel path;
    // a row-boundary anchor would miss prepend regressions that reset it.
    scroll(
        list.logical_scroll_top().offset_in_item - px(7.),
        gpui::TouchPhase::Moved,
        cx,
    );
    let before = list.logical_scroll_top();
    assert!(before.item_ix > 0);
    assert_eq!(before.offset_in_item, px(7.));
    // A page of twenty earlier turns.
    assert!(
        thread.answer_page(20 * RECORDS).is_some(),
        "a page was asked for"
    );
    thread.deliver(cx);
    draw_settled(cx);
    assert!(
        cx.debug_bounds("scroll-to-end").is_some(),
        "prepend hides jump"
    );
    assert_eq!(list.logical_scroll_top().item_ix, before.item_ix + 60);
    assert_eq!(
        list.logical_scroll_top().offset_in_item,
        before.offset_in_item
    );

    let button = cx.debug_bounds("scroll-to-end").unwrap();
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    draw_settled(cx);
    assert!(cx.debug_bounds("scroll-to-end").is_none());
    assert!(list.is_following_tail());
    thread.stream(log(121).split_off(120 * RECORDS));
    thread.deliver(cx);
    draw_settled(cx);
    assert!(cx.debug_bounds("scroll-to-end").is_none());
    assert!(list.is_following_tail());

    // A touch pan reaches the list's own wheel handler. No store
    // notification drives this UI.
    scroll(height * 3., gpui::TouchPhase::Started, cx);
    assert!(
        cx.debug_bounds("scroll-to-end").is_some(),
        "a touch pan hides jump"
    );
    assert!(!list.is_following_tail());
    let before = list.logical_scroll_top();
    thread.stream(log(122).split_off(121 * RECORDS));
    thread.deliver(cx);
    draw_settled(cx);
    assert!(cx.debug_bounds("scroll-to-end").is_some());
    assert_eq!(
        list.logical_scroll_top().item_ix,
        before.item_ix,
        "new turn steals reading position"
    );
    assert_eq!(
        list.logical_scroll_top().offset_in_item,
        before.offset_in_item
    );
    assert!(!list.is_following_tail());
}

/// A scrolling screenshot moves the timeline tile by tile from wherever it
/// was, learns how far it really moved once layout has settled, and leaves
/// it where it was.
#[gpui::test]
fn a_scrolling_capture_starts_where_the_reader_is_stops_at_the_ends_and_restores_them(
    cx: &mut TestAppContext,
) {
    use gpui::{FollowMode, ListOffset, px};
    let (store, window_state, session_id) = seed_chat(cx, synthetic_markdown_timeline(60));
    let (view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
        ChatView::new(store, window_state, Some(session_id), window, cx)
    });
    cx.simulate_resize(gpui::size(px(393.), px(852.)));
    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    };
    // The rows on screen are built and measured before a capture starts.
    crate::test_frames::draw_settled(cx);
    let list = view.read_with(cx, |chat, _| chat.list_state.clone());
    assert!(list.is_following_tail());
    // Rows measured for the first time move the list; the capturer scrolls
    // again after each frame until nothing moves.
    let settle = |offset: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
        for _ in 0..8 {
            let scroll = view
                .read_with(cx, |chat, _| chat.timeline())
                .update(cx, |chat, cx| chat.capture_scroll(offset, cx))
                .expect("a capture is in progress");
            if !scroll.moved {
                return scroll.reached;
            }
            draw(cx);
        }
        panic!("the timeline kept moving");
    };

    // Following the tail: the capture starts at the bottom.
    assert!(
        view.read_with(cx, |chat, _| chat.timeline())
            .update(cx, |chat, cx| chat.capture_begin(cx))
    );
    let viewport = view
        .read_with(cx, |chat, _| chat.timeline())
        .read_with(cx, |chat, _| chat.capture_viewport());
    assert!(viewport.is_some());
    assert!(
        !list.is_following_tail(),
        "layout must not snap back to the end"
    );
    assert_eq!(
        settle(px(0.), cx),
        px(0.),
        "the first tile is the screen itself"
    );
    assert_eq!(settle(px(-400.), cx), px(-400.));
    assert_eq!(
        settle(px(400.), cx),
        px(0.),
        "nothing lies below the end of the content"
    );
    settle(px(-1.0e6), cx);
    assert_eq!(
        list.logical_scroll_top().item_ix,
        0,
        "the tile above the first turn stops at the top"
    );
    view.read_with(cx, |chat, _| chat.timeline())
        .update(cx, |chat, cx| chat.capture_end(cx));
    assert!(list.is_following_tail());
    assert_eq!(
        view.read_with(cx, |chat, _| chat.timeline())
            .update(cx, |chat, cx| chat.capture_scroll(px(0.), cx)),
        None,
        "tiles after the end are refused"
    );

    // Reading in the middle: the anchor comes back exactly, still paused.
    list.set_follow_mode(FollowMode::Normal);
    let anchor = ListOffset {
        item_ix: 10,
        offset_in_item: px(7.),
    };
    list.scroll_to(anchor);
    view.update(cx, |_, cx| cx.notify());
    draw(cx);
    assert!(
        view.read_with(cx, |chat, _| chat.timeline())
            .update(cx, |chat, cx| chat.capture_begin(cx))
    );
    assert_eq!(settle(px(0.), cx), px(0.));
    assert_eq!(settle(px(300.), cx), px(300.));
    assert!(list.logical_scroll_top().item_ix >= 10);
    view.read_with(cx, |chat, _| chat.timeline())
        .update(cx, |chat, cx| chat.capture_end(cx));
    let restored = list.logical_scroll_top();
    assert_eq!(restored.item_ix, anchor.item_ix);
    assert_eq!(restored.offset_in_item, anchor.offset_in_item);
    assert!(!list.is_following_tail());
}

/// A long streamed conversation opens with one partial turn: the snapshot
/// holds only its last records. The first page completes that turn and
/// adds an earlier one; the reader's position must survive rather than
/// resetting to the tail.
#[gpui::test]
fn completing_the_only_partial_turn_keeps_the_reading_anchor(cx: &mut TestAppContext) {
    use gpui::{FollowMode, ListOffset, px};
    let mut full = synthetic_markdown_timeline(2);
    // Tall enough that the reader can scroll into the loaded content.
    let long_answer = "A paragraph of the answer.\n\n".repeat(80);
    full.entries.pop();
    full.entries
        .push(at_turn(entry("assistant-1", assistant(&long_answer)), 1));
    // The window holds the long answer and its turn's end: records 8
    // and 9 of the turns' starts, items and ends.
    let mut thread = ScriptedThread::new(cx, log_of(&full), 8, false);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (view, cx) = thread.open(cx);
    thread.deliver(cx);
    for _ in 0..2 {
        view.update(cx, |_, cx| cx.notify());
        draw_settled(cx);
    }
    let (list, placeholder, leading) =
        view.read_with(cx, |chat, _| chat.timeline())
            .read_with(cx, |chat, _| {
                (
                    chat.list_state.clone(),
                    chat.history_placeholder_height,
                    chat.leading_space(),
                )
            });
    assert!(placeholder > px(0.), "earlier history is still unloaded");
    list.set_follow_mode(FollowMode::Normal);
    list.scroll_to(ListOffset {
        item_ix: 0,
        offset_in_item: leading + px(20.),
    });
    view.update(cx, |_, cx| cx.notify());
    draw_settled(cx);
    assert!(
        thread.answer_page(8).is_some(),
        "the first page was asked for"
    );
    thread.deliver(cx);
    draw_settled(cx);
    let anchor = list.logical_scroll_top();
    // The page adds the earlier turn's three rows and the user bubble
    // above the answer.
    assert_eq!(
        (anchor.item_ix, anchor.offset_in_item),
        (5, px(20.)),
        "the completed turn keeps its reader at the same content offset"
    );
    assert!(
        !list.is_following_tail(),
        "the page must not reset the list to its tail"
    );
}

/// A reader scrolled up a thread is not sent back to its end when a page
/// of history lands, even one that re-indexes the rows: a background
/// agent that a later turn reported on moves back to the turn that
/// started it once the page holds that turn.
#[gpui::test]
fn a_page_that_moves_a_background_agent_back_keeps_the_reader_in_place(cx: &mut TestAppContext) {
    use gpui::{VisualTestContext, point, px};

    const TURNS: usize = 12;
    let agent = |status| agent::ThreadItem {
        id: "agent-0".into(),
        parent_item_id: None,
        content: agent::ItemContent::Subagent {
            agent_type: "general-purpose".into(),
            description: "Survey the code".into(),
            status,
            summary: None,
            model: None,
            effort: None,
        },
    };
    let events: Vec<_> = (0..TURNS)
        .flat_map(|turn| {
            let mut events = vec![
                agent::AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                },
                user_event(&format!("user-{turn}"), &format!("Question {turn}")),
            ];
            match turn {
                2 => events.push(agent::AgentEvent::ItemStarted(agent(
                    agent::ItemStatus::InProgress,
                ))),
                8 => events.push(agent::AgentEvent::ItemCompleted(agent(
                    agent::ItemStatus::Completed,
                ))),
                _ => {}
            }
            events.extend([
                answer_event(
                    &format!("answer-{turn}"),
                    &format!("Answer {turn}, one paragraph of several.\n\n").repeat(8),
                ),
                agent::AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                },
            ]);
            events
        })
        .collect();
    // The window opens at turn 6, after the agent started.
    let held_from = events
        .iter()
        .position(
            |event| matches!(event, agent::AgentEvent::TurnStarted { turn_id } if turn_id == "6"),
        )
        .unwrap();
    let mut thread = ScriptedThread::new(cx, events, held_from, false);
    // The page in flight shows a spinner, which rests only with motion
    // reduced.
    cx.update(|cx| cx.set_reduce_motion(true));
    let (view, cx) = thread.open(cx);
    draw_settled(cx);
    let list = view.read_with(cx, |chat, _| chat.list_state.clone());
    assert!(list.is_following_tail());
    // The reader scrolls up a screen.
    for (phase, delta) in [
        (gpui::TouchPhase::Started, 0.),
        (gpui::TouchPhase::Moved, 700.),
        (gpui::TouchPhase::Ended, 0.),
    ] {
        cx.simulate_event(gpui::ScrollEvent {
            position: list.viewport_bounds().center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(delta))),
            touch_phase: phase,
            ..Default::default()
        });
    }
    draw_settled(cx);
    assert!(!list.is_following_tail());
    let on_screen = |cx: &mut VisualTestContext| {
        (6..TURNS)
            .filter_map(|turn| {
                let bounds = cx.debug_bounds(format!("user-bubble-user-{turn}").leak())?;
                bounds
                    .intersects(&list.viewport_bounds())
                    .then_some((turn, bounds.top()))
            })
            .collect::<Vec<_>>()
    };
    let reading = on_screen(cx);
    assert!(!reading.is_empty(), "a question is on screen");
    let rows = list.item_count();

    // The page holds every earlier turn, the agent's start among them.
    assert!(thread.answer_page(held_from).is_some());
    cx.run_until_parked();
    draw_settled(cx);
    assert!(list.item_count() > rows, "the page landed");
    assert!(
        !list.is_following_tail(),
        "the page sent the reader back to the end"
    );
    let now = on_screen(cx);
    for (turn, top) in &reading {
        let moved = now
            .iter()
            .find(|(seen, _)| seen == turn)
            .map(|(_, now)| *now);
        assert!(
            moved.is_some_and(|now| (now - *top).abs() < px(1.)),
            "question {turn} moved from {top:?} to {moved:?}"
        );
    }
}

/// What the reader opened or selected in a turn stays with that turn
/// when a page of history lands above it and renumbers the window's
/// turns: its expanded Work Log stays expanded, its changed files stay
/// shown in full, and the text selected in its answer stays selected.
#[gpui::test]
fn a_page_of_history_leaves_what_the_reader_opened_with_its_turn(cx: &mut TestAppContext) {
    use gpui::{Modifiers, MouseButton, VisualTestContext, point, px};

    let command = |id: String| {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.clone(),
            parent_item_id: None,
            content: agent::ItemContent::CommandExecution {
                command: format!("cargo test {id}"),
                input: Vec::new(),
                output: "ok\n".into(),
                exit_code: Some(0),
                status: ItemStatus::Completed,
            },
        })
    };
    // The last turn changes four files, which its card shows three of.
    let changed = |turn: usize| {
        let files = if turn == 3 { 4 } else { 1 };
        (0..files)
            .map(|file| {
                let path = format!("t{turn}_f{file}.rs");
                format!(
                    "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n\
                         @@ -1 +1 @@\n-fn old() {{}}\n+fn new() {{}}\n"
                )
            })
            .collect::<String>()
    };
    let turn = |turn: usize| {
        [
            agent::AgentEvent::TurnStarted {
                turn_id: turn.to_string(),
            },
            user_event(&format!("user-{turn}"), &format!("Question {turn}")),
            command(format!("cmd-{turn}-a")),
            command(format!("cmd-{turn}-b")),
            answer_event(
                &format!("answer-{turn}"),
                &format!("Answer {turn} reads the whole build and every test it runs."),
            ),
            agent::AgentEvent::TurnChangesUpdated {
                turn_id: turn.to_string(),
                changes: agent::file_changes_from_unified_diff(&changed(turn)).changes,
                completeness: agent::ChangeCompleteness::Exact,
            },
            agent::AgentEvent::TurnCompleted {
                turn_id: turn.to_string(),
                status: agent::TurnStatus::Completed,
                usage: None,
            },
        ]
    };
    let events: Vec<_> = (0..4).flat_map(turn).collect();
    // The window holds turns 1 to 3; turn 0 is the page.
    let mut thread = ScriptedThread::new(cx, events, 7, false);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (view, cx) = thread.open(cx);
    thread.deliver(cx);
    draw_settled(cx);
    let click = |selector: &'static str, cx: &mut VisualTestContext| {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector}"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        draw_settled(cx);
    };
    click("worklog-header-2-cmd-3-a", cx);
    // The control that shows the rest follows the third file's chip.
    let third = cx
        .debug_bounds("changed-file-chip-file-t3_f2.rs")
        .expect("turn 3's third file");
    cx.simulate_click(
        point(third.right() + px(16.), third.center().y),
        Modifiers::default(),
    );
    draw_settled(cx);
    let answer = cx
        .debug_bounds("markdown-block-0")
        .expect("the last answer, the last message painted");
    let (start, end) = (
        point(answer.left() + px(2.), answer.center().y),
        point(answer.left() + px(120.), answer.center().y),
    );
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    draw_settled(cx);
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    draw_settled(cx);
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    draw_settled(cx);
    let timeline = view.read_with(cx, |chat, _| chat.timeline());
    let opened = |cx: &mut VisualTestContext| {
        let selected = timeline.read_with(cx, |timeline, cx| {
            timeline.selection.read(cx).selected_text(cx)
        });
        (
            cx.debug_bounds("activity-row-cmd-3-a").is_some(),
            cx.debug_bounds("changed-file-chip-file-t3_f3.rs").is_some(),
            selected,
        )
    };
    let before = opened(cx);
    assert!(before.0 && before.1, "turn 3's Work Log and its files open");
    assert!(
        before.2.starts_with("Answer 3"),
        "the drag selected turn 3's answer: {:?}",
        before.2
    );

    assert_eq!(
        thread.answer_page(7),
        Some(7),
        "the window asked for the page"
    );
    thread.deliver(cx);
    draw_settled(cx);
    assert_eq!(
        thread.store.read_with(cx, |store, cx| {
            store.with_timeline_for(Some(THREAD), |timeline| timeline.turns.len(), cx)
        }),
        Some(4),
        "the page landed"
    );
    assert_eq!(opened(cx), before, "what turn 3 had open went elsewhere");
}
