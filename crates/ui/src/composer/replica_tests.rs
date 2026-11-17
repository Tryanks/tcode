use super::*;
use crate::test_frames::draw_settled;
use gpui::{Modifiers, TestAppContext};
use std::time::Duration;
use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, SessionStatus, Topic, encode_line};

fn attach(
    cx: &mut TestAppContext,
) -> (
    Entity<WorkspaceStore>,
    async_channel::Sender<String>,
    async_channel::Receiver<String>,
    SessionStatus,
) {
    attach_as(crate::store::WorkspaceAttachment::Local, cx)
}

fn attach_as(
    attachment: crate::store::WorkspaceAttachment,
    cx: &mut TestAppContext,
) -> (
    Entity<WorkspaceStore>,
    async_channel::Sender<String>,
    async_channel::Receiver<String>,
    SessionStatus,
) {
    let (store, incoming, outgoing, status, _) = attach_over(attachment, cx);
    (store, incoming, outgoing, status)
}

/// [`attach_as`], with the link the transport reports its path on.
fn attach_over(
    attachment: crate::store::WorkspaceAttachment,
    cx: &mut TestAppContext,
) -> (
    Entity<WorkspaceStore>,
    async_channel::Sender<String>,
    async_channel::Receiver<String>,
    SessionStatus,
    tcode_client::HostLink,
) {
    cx.update(crate::test_client::init);
    cx.update(crate::markdown::init);
    let status = crate::store::tests::draft_status();
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let pump = link.clone();
    cx.background_executor
        .spawn(async move {
            pump.pump().await;
        })
        .detach();
    let store = cx.new(|cx| WorkspaceStore::new_attached(link.clone(), attachment, None, None, cx));
    crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
    store.update(cx, |store, cx| {
        store.show_alone(status.session_id.clone(), cx)
    });
    (store, incoming, outgoing, status, link)
}

/// A composer in a window, seeded with its thread's status, ready to send.
fn composer_on<'a>(
    store: &Entity<WorkspaceStore>,
    incoming: &async_channel::Sender<String>,
    mut status: SessionStatus,
    cx: &'a mut TestAppContext,
) -> (Entity<Composer>, &'a mut gpui::VisualTestContext) {
    status.draft = false;
    let session_id = status.session_id.clone();
    replace(
        incoming,
        Topic::SessionStatus {
            session_id: session_id.clone(),
        },
        ServerEvent::SessionStatusReplaced(Box::new(status)),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    let store = store.clone();
    let slot = std::rc::Rc::new(std::cell::OnceCell::new());
    let view = slot.clone();
    let (_root, cx) = cx.add_window_view(move |window, cx| {
        let composer = cx.new(|cx| Composer::new(store, Some(session_id), window, cx));
        let _ = view.set(composer.clone());
        gpui_base::Root::new(composer, window, cx)
    });
    let composer: Entity<Composer> = slot.get().unwrap().clone();
    // An upload's spinner never rests otherwise.
    cx.update(|_, cx| cx.set_reduce_motion(true));
    cx.simulate_resize(gpui::size(px(1024.), px(768.)));
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        composer.update(cx, |composer, cx| composer.focus(window, cx));
    });
    draw_settled(cx);
    (composer, cx)
}

/// Drop `paths` on the composer card, as the platform delivers a drag.
fn drop_paths(paths: Vec<PathBuf>, cx: &mut gpui::VisualTestContext) {
    let position = cx.debug_bounds("composer-card").unwrap().center();
    cx.simulate_event(gpui::FileDropEvent::Entered {
        position,
        paths: ExternalPaths(paths.into()),
    });
    cx.simulate_event(gpui::FileDropEvent::Pending { position });
    cx.simulate_event(gpui::FileDropEvent::Submit { position });
    draw_settled(cx);
}

fn sent_texts(outgoing: &async_channel::Receiver<String>) -> Vec<String> {
    std::iter::from_fn(|| outgoing.try_recv().ok())
        .filter_map(
            |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                tcode_protocol::ClientPayload::Command(tcode_protocol::Command::SendTurn {
                    text,
                    ..
                }) => Some(text),
                _ => None,
            },
        )
        .collect()
}

/// Answer the next upload query the client sent, returning it.
fn answer_upload(
    outgoing: &async_channel::Receiver<String>,
    incoming: &async_channel::Sender<String>,
    answer: impl FnOnce(&tcode_protocol::Query) -> Result<tcode_protocol::QueryResponse, String>,
) -> Option<tcode_protocol::Query> {
    while let Ok(line) = outgoing.try_recv() {
        let message = tcode_protocol::decode_client_line(&line).unwrap();
        if let tcode_protocol::ClientPayload::Query(
            query @ (tcode_protocol::Query::BeginUpload { .. }
            | tcode_protocol::Query::WriteUpload { .. }
            | tcode_protocol::Query::DiscardUpload { .. }),
        ) = message.payload
        {
            let result = answer(&query).map_err(|message| tcode_protocol::ProtocolError {
                code: "io_error".into(),
                message,
            });
            incoming
                .try_send(
                    encode_line(&HostMessage::QueryResult {
                        id: message.id,
                        result,
                    })
                    .unwrap(),
                )
                .unwrap();
            return Some(query);
        }
    }
    None
}

fn remote() -> crate::store::WorkspaceAttachment {
    crate::store::WorkspaceAttachment::Remote {
        host_id: "laptop".into(),
        host_name: "Laptop".into(),
    }
}

/// A file and a folder on the host's own disk are attached in place: the
/// message carries their own paths, and nothing is uploaded.
#[gpui::test]
fn a_local_file_or_folder_is_sent_as_its_own_path(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, status) = attach(cx);
    let root = crate::store::tests::scratch_root("composer-local-file");
    std::fs::create_dir_all(&root).unwrap();
    let notes = root.join("notes.txt");
    std::fs::write(&notes, "notes").unwrap();
    let (_composer, cx) = composer_on(&store, &incoming, status, cx);
    drop_paths(vec![notes.clone(), root.clone()], cx);
    assert!(cx.debug_bounds("attached-file-0").is_some());
    assert!(cx.debug_bounds("attached-file-1").is_some());
    while outgoing.try_recv().is_ok() {}
    cx.simulate_input("read these");
    cx.simulate_keystrokes("enter");
    draw_settled(cx);
    assert_eq!(
        sent_texts(&outgoing),
        [format!(
            "read these\n\n{}\n{}",
            notes.display(),
            root.display()
        )]
    );
    assert!(cx.debug_bounds("attached-file-0").is_none());
    let _ = std::fs::remove_dir_all(root);
}

/// The desktop's "+" opens the system's file picker for any number of
/// files, and what is chosen attaches as a drop would: here, files on the
/// host's own disk, sent as their own paths.
#[cfg(feature = "native-dialogs")]
#[gpui::test]
fn the_desktop_plus_attaches_picked_files_as_a_drop_would(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, status) = attach(cx);
    let root = crate::store::tests::scratch_root("composer-picked-files");
    std::fs::create_dir_all(&root).unwrap();
    let notes = root.join("notes.txt");
    std::fs::write(&notes, "notes").unwrap();
    let plan = root.join("plan.md");
    std::fs::write(&plan, "plan").unwrap();
    let (_composer, cx) = composer_on(&store, &incoming, status, cx);
    let plus = cx
        .debug_bounds("attach-files")
        .expect("the desktop draws a +");
    cx.simulate_click(plus.center(), gpui::Modifiers::default());
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|options| {
        assert!(options.files && options.multiple);
        Some(vec![notes.clone(), plan.clone()])
    });
    draw_settled(cx);
    assert!(cx.debug_bounds("attached-file-0").is_some());
    assert!(cx.debug_bounds("attached-file-1").is_some());
    while outgoing.try_recv().is_ok() {}
    cx.simulate_input("read these");
    cx.simulate_keystrokes("enter");
    draw_settled(cx);
    let sent = sent_texts(&outgoing);
    assert_eq!(
        sent,
        [format!(
            "read these\n\n{}\n{}",
            notes.display(),
            plan.display()
        )]
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Copying a file in the file manager and pasting it attaches the file.
#[gpui::test]
fn a_pasted_file_is_attached_rather_than_refused(cx: &mut TestAppContext) {
    let (store, incoming, _outgoing, status) = attach(cx);
    let root = crate::store::tests::scratch_root("composer-pasted-file");
    std::fs::create_dir_all(&root).unwrap();
    let notes = root.join("notes.txt");
    std::fs::write(&notes, "notes").unwrap();
    let (composer, cx) = composer_on(&store, &incoming, status, cx);
    cx.update(|window, cx| {
        let item = gpui::ClipboardItem {
            entries: vec![gpui::ClipboardEntry::ExternalPaths(ExternalPaths(
                vec![notes.clone()].into(),
            ))],
        };
        composer.update(cx, |composer, cx| {
            assert!(composer.paste_images(&item, window, cx));
        });
    });
    draw_settled(cx);
    assert!(cx.debug_bounds("attached-file-0").is_some());
    let _ = std::fs::remove_dir_all(root);
}

/// A remote client uploads the file in chunks and the message carries the
/// host's path. Until the last chunk lands a send cannot lose the file; a
/// failed chunk holds the message until the retry goes through.
#[gpui::test]
fn a_remote_file_uploads_in_chunks_and_a_failure_can_be_retried(cx: &mut TestAppContext) {
    use tcode_protocol::{Query, QueryResponse};
    let (store, incoming, outgoing, status) = attach_as(remote(), cx);
    let root = crate::store::tests::scratch_root("composer-remote-file");
    std::fs::create_dir_all(&root).unwrap();
    let notes = root.join("notes.txt");
    let content = vec![7u8; tcode_core::attachments::UPLOAD_CHUNK_BYTES + 3];
    std::fs::write(&notes, &content).unwrap();
    let (_composer, cx) = composer_on(&store, &incoming, status, cx);
    let host_path = PathBuf::from("/host/Documents/Tcode/2026-11-08/uploads/s/u/notes.txt");

    drop_paths(vec![notes], cx);
    let begun = answer_upload(&outgoing, &incoming, |_| {
        Ok(QueryResponse::UploadBegun(host_path.clone()))
    })
    .expect("the client asks the host for a path");
    assert!(matches!(
        begun,
        Query::BeginUpload { ref name, size, .. }
            if name == "notes.txt" && size == content.len() as u64
    ));
    draw_settled(cx);

    cx.simulate_input("read this");
    cx.simulate_keystrokes("enter");
    draw_settled(cx);
    let first = answer_upload(&outgoing, &incoming, |_| Err("connection lost".into()))
        .expect("the first chunk goes out");
    assert!(matches!(
        first,
        Query::WriteUpload { offset: 0, ref bytes, .. }
            if bytes.len() == tcode_core::attachments::UPLOAD_CHUNK_BYTES
    ));
    draw_settled(cx);
    assert!(
        cx.debug_bounds("attached-file-0").is_some(),
        "the file stays"
    );
    cx.simulate_keystrokes("enter");
    draw_settled(cx);
    assert_eq!(sent_texts(&outgoing), Vec::<String>::new());

    let retry = cx.debug_bounds("attached-file-retry-0").unwrap().center();
    cx.simulate_click(retry, gpui::Modifiers::none());
    draw_settled(cx);
    let mut received = Vec::new();
    let mut discarded = false;
    while let Some(query) = answer_upload(&outgoing, &incoming, |query| {
        Ok(match query {
            Query::BeginUpload { .. } => QueryResponse::UploadBegun(host_path.clone()),
            Query::WriteUpload { .. } => QueryResponse::UploadWritten,
            _ => QueryResponse::UploadDiscarded,
        })
    }) {
        match query {
            Query::WriteUpload { offset, bytes, .. } => {
                assert_eq!(offset as usize, received.len());
                received.extend(bytes);
            }
            Query::DiscardUpload { path, .. } => {
                assert_eq!(path, host_path);
                discarded = true;
            }
            _ => {}
        }
        draw_settled(cx);
    }
    assert!(discarded, "the failed attempt leaves nothing on the host");
    assert_eq!(received, content);

    let send = cx.debug_bounds("send-message").unwrap().center();
    cx.simulate_click(send, gpui::Modifiers::none());
    draw_settled(cx);
    assert_eq!(
        sent_texts(&outgoing),
        [format!("read this\n\n{}", host_path.display())]
    );
    let _ = std::fs::remove_dir_all(root);
}

/// A folder has no remote path to carry, and a file over the device's
/// ceiling never leaves a link that cannot be trusted with it: one whose
/// path is not known yet, or a relay. On the machine's own network it
/// goes.
#[gpui::test]
fn a_remote_client_refuses_folders_and_files_over_the_link_ceiling(cx: &mut TestAppContext) {
    let path = |direct, lan| tcode_protocol::PathInfo {
        direct,
        relay: (!direct).then(|| "https://relay.example.invalid".into()),
        lan,
        probing_direct: false,
    };
    for (path, attached) in [
        (None, false),
        (Some(path(false, false)), false),
        (Some(path(true, true)), true),
    ] {
        let (store, incoming, outgoing, status, link) = attach_over(remote(), cx);
        if let Some(path) = path.clone() {
            link.set_connection_state(tcode_client::ConnectionState::Connected {
                path: Some(path),
            });
        }
        let root = crate::store::tests::scratch_root("composer-remote-refused");
        std::fs::create_dir_all(&root).unwrap();
        let large = root.join("large.bin");
        std::fs::File::create(&large)
            .unwrap()
            .set_len(tcode_core::attachments::DEFAULT_REMOTE_BYTES + 1)
            .unwrap();
        let (_composer, cx) = composer_on(&store, &incoming, status, cx);
        drop_paths(vec![root.clone(), large], cx);
        assert_eq!(
            cx.debug_bounds("attached-file-0").is_some(),
            attached,
            "{path:?}"
        );
        assert!(cx.debug_bounds("attached-file-1").is_none(), "{path:?}");
        if !attached {
            assert_eq!(
                answer_upload(&outgoing, &incoming, |_| Err("unexpected".into())),
                None
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }
}

fn replace(incoming: &async_channel::Sender<String>, topic: Topic, event: ServerEvent) {
    incoming
        .try_send(
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic,
                event,
            }))
            .unwrap(),
        )
        .unwrap();
}

/// The prompt a rewind gives back goes into the composer of the client
/// that asked for the rewind, ahead of what the user had typed, which it
/// never replaces; another client's rewind leaves this composer alone.
#[gpui::test]
fn a_rewound_prompt_goes_before_the_draft_of_the_client_that_asked(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, status) = attach(cx);
    let session_id = status.session_id.clone();
    let topic = Topic::SessionStatus {
        session_id: session_id.clone(),
    };
    replace(
        &incoming,
        topic.clone(),
        ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    let own = store.read_with(cx, |store, _| store.client_id().to_owned());
    let (composer, cx) = cx.add_window_view(|window, cx| {
        Composer::new(store.clone(), Some(session_id.clone()), window, cx)
    });
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        composer.update(cx, |composer, cx| composer.focus(window, cx));
    });
    cx.simulate_input("and also this");
    for (client_id, text) in [
        ("another-client", "asked elsewhere"),
        (own.as_str(), "the restored prompt"),
    ] {
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::RewindPrefill {
                session_id: session_id.clone(),
                client_id: client_id.to_owned(),
                text: text.to_owned(),
                attachments: Vec::new(),
            },
        );
        cx.run_until_parked();
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    while outgoing.try_recv().is_ok() {}
    cx.simulate_keystrokes("enter");
    draw_settled(cx);
    assert_eq!(
        sent_texts(&outgoing),
        ["the restored prompt\n\nand also this"]
    );
}

#[gpui::test]
fn composer_history_survives_submission_and_destination_switches(cx: &mut TestAppContext) {
    for is_draft in [false, true] {
        let (store, incoming, outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        status.draft = is_draft;
        status.project_id = Some("project-a".into());
        let restored_status = status.clone();
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        cx.run_until_parked();
        let (composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            composer.update(cx, |composer, cx| composer.focus(window, cx));
        });
        cx.simulate_input("Keep this prompt");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let send = cx.debug_bounds("send-message").expect("send button");
        cx.simulate_click(send.center(), Modifiers::default());
        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| composer.focus(window, cx));
            window.draw(cx).clear(cx);
        });
        assert_eq!(sent_texts(&outgoing), ["Keep this prompt"]);
        // Undo brings back what was sent.
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        });
        status.session_id = "other-thread".into();
        status.project_id = Some("project-b".into());
        store.update(cx, |store, cx| {
            store.show_alone(status.session_id.clone(), cx)
        });
        composer.update(cx, |composer, cx| {
            composer.set_session(Some(status.session_id.clone()), cx)
        });
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| composer.update(cx, |composer, cx| composer.focus(window, cx)));
        cx.simulate_input("Other destination draft");
        // An input method is still composing the last words.
        let field = composer.read_with(cx, |composer, _| composer.input.clone());
        cx.update(|window, cx| {
            field.update(cx, |input, cx| {
                use gpui::EntityInputHandler as _;
                input.replace_and_mark_text_in_range(None, "，你好", None, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        store.update(cx, |store, cx| {
            store.show_alone(restored_status.session_id.clone(), cx)
        });
        composer.update(cx, |composer, cx| {
            composer.set_session(Some(restored_status.session_id.clone()), cx)
        });
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: restored_status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(restored_status.clone())),
        );
        cx.run_until_parked();
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            composer.update(cx, |composer, cx| composer.focus(window, cx));
        });
        // What the first thread or draft held came back with it.
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(sent_texts(&outgoing), ["Keep this prompt"]);

        store.update(cx, |store, cx| {
            store.show_alone(status.session_id.clone(), cx)
        });
        composer.update(cx, |composer, cx| {
            composer.set_session(Some(status.session_id.clone()), cx)
        });
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            composer.update(cx, |composer, cx| composer.focus(window, cx));
        });
        // What was being composed was committed as the thread was left,
        // so what is typed now follows it rather than replacing it.
        cx.simulate_input("x");
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(sent_texts(&outgoing), ["Other destination draft，你好x"]);
    }
}

/// A platform's dictation the test speaks for: what it would hear is
/// sent on the channel it was started with.
#[derive(Default)]
struct Recognizer {
    events: std::cell::RefCell<Option<async_channel::Sender<tcode_client::speech::SpeechEvent>>>,
    finished: std::rc::Rc<std::cell::Cell<bool>>,
}

struct Listening(std::rc::Rc<std::cell::Cell<bool>>);

impl tcode_client::speech::SpeechSession for Listening {
    fn finish(&mut self) {
        self.0.set(true);
    }
}

impl tcode_client::speech::SpeechRecognizer for Recognizer {
    fn is_available(&self, _: &str) -> bool {
        true
    }
    fn start(
        &self,
        _: &str,
        events: async_channel::Sender<tcode_client::speech::SpeechEvent>,
    ) -> Result<Box<dyn tcode_client::speech::SpeechSession>, tcode_client::speech::SpeechError>
    {
        *self.events.borrow_mut() = Some(events);
        Ok(Box::new(Listening(self.finished.clone())))
    }
}

/// Dictation started from the mic writes where the cursor was, and a
/// send pressed while it listens asks the recognizer to finish and goes
/// once its last words are in the draft, with them.
#[gpui::test]
fn a_send_during_dictation_goes_with_the_recognizers_last_words(cx: &mut TestAppContext) {
    use tcode_client::speech::SpeechEvent;
    let (store, incoming, outgoing, status) = attach(cx);
    let recognizer = std::rc::Rc::new(Recognizer::default());
    cx.update(|cx| crate::test_client::dictate_with(recognizer.clone(), cx));
    let (_composer, cx) = composer_on(&store, &incoming, status, cx);
    cx.simulate_input("Note: ");
    draw_settled(cx);
    let mic = cx.debug_bounds("voice-mic").expect("mic button");
    cx.simulate_click(mic.center(), Modifiers::default());
    let events = recognizer
        .events
        .borrow()
        .clone()
        .expect("the mic starts the platform's dictation");
    for event in [
        SpeechEvent::Ready,
        SpeechEvent::Hypothesis("fix the".into()),
        SpeechEvent::Phrase("fix the bug".into()),
    ] {
        events.try_send(event).unwrap();
        cx.run_until_parked();
    }

    // Sent with the button: the test platform types an Enter the field
    // passes on as a line break, which neither macOS nor Windows does.
    draw_settled(cx);
    let send = cx.debug_bounds("send-message").expect("send button");
    cx.simulate_click(send.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        sent_texts(&outgoing).is_empty(),
        "the send waits for what is spoken but not yet written"
    );
    assert!(
        recognizer.finished.get(),
        "the recognizer is asked to finish"
    );

    events
        .try_send(SpeechEvent::Phrase(" before lunch".into()))
        .unwrap();
    events.try_send(SpeechEvent::Finished).unwrap();
    cx.run_until_parked();
    assert_eq!(sent_texts(&outgoing), ["Note: fix the bug before lunch"]);
}

/// A value the provider reports as unavailable is shown but never sent.
#[gpui::test]
fn unavailable_permission_rows_cannot_be_chosen(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, mut status) = attach(cx);
    let session_id = Some(status.session_id.clone());
    let mut descriptor = agent::permission_control(status.provider).unwrap();
    let OptionDescriptor::Select { id, options, .. } = &mut descriptor else {
        panic!("the draft's provider publishes a select permission control");
    };
    let id = id.clone();
    options[1].unavailable = Some("Not available for this account".into());
    let chosen = options[0].value.clone();
    status
            .provider_option_descriptors
            .retain(|existing| !matches!(existing, OptionDescriptor::Select { id: existing, .. } if *existing == id));
    status.provider_option_descriptors.push(descriptor);
    replace(
        &incoming,
        Topic::SessionStatus {
            session_id: status.session_id.clone(),
        },
        ServerEvent::SessionStatusReplaced(Box::new(status)),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    let (_composer, cx) = cx
        .add_window_view(|window, cx| Composer::new(store.clone(), session_id.clone(), window, cx));
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    cx.update(|window, cx| _ = window.draw(cx));
    while outgoing.try_recv().is_ok() {}
    let sent = || {
        std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(
                |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                    tcode_protocol::ClientPayload::Command(
                        tcode_protocol::Command::SetActiveOption { id, value, .. },
                    ) => Some((id, value)),
                    _ => None,
                },
            )
            .collect::<Vec<_>>()
    };
    let click = |cx: &mut gpui::VisualTestContext, selector: &'static str| {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector}"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
    };
    click(cx, "permission-chip");
    click(cx, "permission-row-1");
    cx.run_until_parked();
    assert_eq!(sent(), []);
    assert!(
        cx.debug_bounds("permission-pane").is_some(),
        "the menu stays open"
    );
    click(cx, "permission-row-0");
    cx.run_until_parked();
    assert_eq!(sent(), [(id, Some(serde_json::Value::String(chosen)))]);
}

/// A draft on the built-in Claude account of a host with a second account
/// of that CLI, which lists the same two models.
struct TwoAccounts {
    root: std::path::PathBuf,
    host: tcode_runtime::pipe::SpawnedHost,
    driver: crate::store::tests::LocalHostDriver,
    store: gpui::Entity<WorkspaceStore>,
    session_id: String,
    launches: smol::channel::Receiver<agent::SessionOptions>,
}

fn two_accounts(cx: &mut TestAppContext, name: &str) -> TwoAccounts {
    cx.update(|cx| cx.set_reduce_motion(true));
    use agent::ProviderKind;

    cx.update(crate::test_client::init);
    cx.update(crate::markdown::init);
    let root = crate::store::tests::scratch_root(name);
    let project = tcode_core::project::Project::from_root(root.clone());
    let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
    disk.upsert_project(&project).unwrap();
    let spec = |id: &str| agent::ModelSpec {
        id: id.into(),
        display_name: id.into(),
        is_default: false,
        options: Vec::new(),
    };
    disk.save_models(
        ProviderKind::ClaudeCode,
        &[spec("first-model"), spec("shared-model")],
    )
    .unwrap();
    let mut settings = tcode_core::settings::Settings::default();
    settings.profiles.insert(
        "second-account".to_owned(),
        tcode_core::settings::ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: tcode_core::settings::ProviderSettings {
                home_path: Some("/profile-homes/second-account".into()),
                ..Default::default()
            },
        },
    );
    tcode_services::settings::SettingsStore::new(root.clone())
        .save(&settings)
        .unwrap();
    let scripted = tcode_runtime::app::scripted_provider(ProviderKind::ClaudeCode);
    let launches = scripted.launches.clone();
    let host = tcode_runtime::pipe::spawn_host(
        disk,
        tcode_runtime::pipe::HostServices {
            provider_launcher: scripted.launcher.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    let mut driver = crate::store::tests::LocalHostDriver::new(&host);
    let link = driver.link.clone();
    let tcode_protocol::CommandResponse::SessionId(Some(session_id)) = driver
        .await_with_events(
            cx,
            link.command(tcode_protocol::Command::StartDraft {
                project_id: project.id.clone(),
                cwd: root.clone(),
            }),
        )
        .unwrap()
    else {
        panic!("the host starts a draft");
    };
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
    store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
    driver.wait_until(cx, "the draft's status", |cx| {
        store.read_with(cx, |store, cx| {
            store
                .composer_state_for(Some(&session_id), cx)
                .active_model
                .is_some()
        })
    });
    TwoAccounts {
        root,
        host,
        driver,
        store,
        session_id,
        launches,
    }
}

fn show_rail(composer: &Entity<Composer>, rail: PickerRail, cx: &mut gpui::VisualTestContext) {
    composer.update(cx, |composer, cx| {
        composer.picker_rail = Some(rail);
        cx.notify();
    });
    crate::test_frames::draw_settled(cx);
}

fn open_picker(cx: &mut gpui::VisualTestContext) {
    let picker = cx.debug_bounds("model-picker").expect("model picker");
    cx.simulate_click(picker.center(), Modifiers::default());
    crate::test_frames::draw_settled(cx);
}

/// Two accounts of one CLI list the same ids. A model picked under the
/// second reads as selected there alone, and the thread runs on it.
#[gpui::test]
fn a_model_is_selected_under_the_account_it_was_picked_on(cx: &mut TestAppContext) {
    use crate::test_frames::draw_settled;
    use agent::ProviderKind;

    let TwoAccounts {
        root,
        host,
        mut driver,
        store,
        session_id,
        launches,
    } = two_accounts(cx, "account-selection");
    let session = Some(session_id.clone());
    let (composer, cx) =
        cx.add_window_view(|window, cx| Composer::new(store.clone(), session, window, cx));
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    draw_settled(cx);

    open_picker(cx);
    show_rail(&composer, PickerRail::Profile("second-account".into()), cx);
    cx.simulate_keystrokes("secondary-2");
    draw_settled(cx);
    driver.wait_until(cx, "the pick to reach the thread", |cx| {
        store.read_with(cx, |store, cx| {
            store
                .composer_state_for(Some(&session_id), cx)
                .active_model
                .is_some_and(|active| {
                    active.model.as_deref() == Some("shared-model")
                        && active.profile_id.as_deref() == Some("second-account")
                })
        })
    });
    draw_settled(cx);

    open_picker(cx);
    assert!(
        cx.debug_bounds("model-row-current").is_some(),
        "the account the model was picked on shows it selected"
    );
    show_rail(&composer, PickerRail::Profile("claude".into()), cx);
    assert!(
        cx.debug_bounds("model-row-current").is_none(),
        "the other account lists the same id, unselected"
    );
    cx.simulate_keystrokes("escape");
    draw_settled(cx);

    store.update(cx, |store, _| {
        store.send_turn_for(session_id.clone(), "hello".into(), Vec::new())
    });
    driver.wait_until(cx, "the provider launch", |_| !launches.is_empty());
    let launch = launches.try_recv().unwrap();
    assert_eq!(launch.model.as_deref(), Some("shared-model"));
    assert!(
        launch
            .launch_env
            .pairs(ProviderKind::ClaudeCode)
            .contains(&(
                "CLAUDE_CONFIG_DIR".to_owned(),
                "/profile-homes/second-account".to_owned()
            )),
        "the thread launches on the second account"
    );
    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
}

/// The star on a second account's row lights that row alone: the first
/// account's row of the same id stays dark, Favorites lists the one row,
/// and its shortcut picks it on the account it was starred under.
#[gpui::test]
fn a_favorite_under_the_second_account_stars_only_that_row(cx: &mut TestAppContext) {
    use crate::test_frames::draw_settled;

    let TwoAccounts {
        root,
        host,
        mut driver,
        store,
        session_id,
        ..
    } = two_accounts(cx, "account-favorites");
    let session = Some(session_id.clone());
    let (composer, cx) =
        cx.add_window_view(|window, cx| Composer::new(store.clone(), session, window, cx));
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    draw_settled(cx);

    open_picker(cx);
    show_rail(&composer, PickerRail::Profile("second-account".into()), cx);
    let star = cx
        .debug_bounds("model-fav-1")
        .expect("the second row's star");
    cx.simulate_click(star.center(), Modifiers::default());
    driver.wait_until(cx, "the star to reach the host", |cx| {
        store.read_with(cx, |store, _| !store.settings().favorite_models.is_empty())
    });
    draw_settled(cx);

    let starred = |profile: &str, cx: &gpui::VisualTestContext| {
        store.read_with(cx, |store, _| {
            store
                .picker_models_for_profile(profile)
                .into_iter()
                .filter(|model| model.favorite)
                .map(|model| model.id)
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(starred("second-account", cx), ["shared-model"]);
    assert!(starred("claude", cx).is_empty());

    show_rail(&composer, PickerRail::Favorites, cx);
    assert!(cx.debug_bounds("model-fav-0").is_some());
    assert!(
        cx.debug_bounds("model-fav-1").is_none(),
        "Favorites lists the starred row, not its namesake on the other account"
    );
    // Focus went with the star's button when its row moved to the top,
    // so the picker is closed and opened again, its search taking the
    // keyboard.
    open_picker(cx);
    assert!(
        cx.debug_bounds("model-fav-0").is_none(),
        "the picker closed"
    );
    open_picker(cx);
    show_rail(&composer, PickerRail::Favorites, cx);
    cx.simulate_keystrokes("secondary-1");
    driver.wait_until(cx, "the favourite's pick", |cx| {
        store.read_with(cx, |store, cx| {
            store
                .composer_state_for(Some(&session_id), cx)
                .active_model
                .is_some_and(|active| {
                    active.model.as_deref() == Some("shared-model")
                        && active.profile_id.as_deref() == Some("second-account")
                })
        })
    });
    driver.shutdown(cx, &host);
    let _ = std::fs::remove_dir_all(root);
}

#[gpui::test]
fn switching_language_relocalizes_the_model_search_placeholder(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
    let (store, incoming, _outgoing, status) = attach(cx);
    let session_id = Some(status.session_id.clone());
    replace(
        &incoming,
        Topic::SessionStatus {
            session_id: status.session_id.clone(),
        },
        ServerEvent::SessionStatusReplaced(Box::new(status)),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    let (composer, cx) = cx
        .add_window_view(|window, cx| Composer::new(store.clone(), session_id.clone(), window, cx));
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    cx.update(|window, cx| _ = window.draw(cx));
    let picker = cx.debug_bounds("model-picker").expect("model picker");
    cx.simulate_click(picker.center(), Modifiers::default());
    crate::settings::assert_follows_language_switch(cx, "composer.search_models", |cx| {
        composer.read_with(cx, |composer, cx| {
            let search = composer.model_search.read(cx);
            search.presentation().placeholder().to_string()
        })
    });
}

#[gpui::test]
fn the_phone_queue_buttons_release_height_sends_steer_queue_or_nothing(cx: &mut TestAppContext) {
    use gpui::{TouchEvent, TouchId, TouchPhase, point};

    let (store, incoming, outgoing, mut status) = attach(cx);
    status.activity.turn_running = true;
    status.steering_supported = true;
    let mut unsupported = status.clone();
    unsupported.steering_supported = false;
    // A phone sends only once its screen is current, so every baseline
    // topic is answered.
    let session_id = status.session_id.clone();
    let baseline = [
        (
            Topic::Index,
            ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                summary: Default::default(),
                sessions: Vec::new(),
                projects: Vec::new(),
            }),
        ),
        (
            Topic::Settings,
            ServerEvent::SettingsSnapshot(tcode_core::settings::Settings::default()),
        ),
        (
            Topic::SessionStatus {
                session_id: session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        ),
        (
            Topic::SessionPlan {
                session_id: session_id.clone(),
            },
            ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                session_id: session_id.clone(),
                steps: Vec::new(),
                explanation: None,
            }),
        ),
        (
            Topic::SessionEvents {
                session_id: session_id.clone(),
            },
            ServerEvent::SessionSnapshot {
                from: 0,
                end: 0,
                records: Vec::new(),
                total: 0,
                total_turns: 0,
                truncated: false,
            },
        ),
    ];
    for (topic, event) in baseline {
        replace(&incoming, topic, event);
    }
    cx.run_until_parked();
    cx.run_until_parked();
    let (composer, cx) = cx.add_window_view(|window, cx| {
        Composer::new(store.clone(), Some(session_id.clone()), window, cx)
    });
    composer.update(cx, |composer, cx| composer.set_compact(true, cx));
    cx.simulate_resize(gpui::size(px(393.), px(800.)));
    let touch = |cx: &mut gpui::VisualTestContext, phase, position| {
        cx.simulate_event(TouchEvent {
            id: TouchId(1),
            phase,
            position,
            predicted_position: None,
            force: None,
        });
        cx.update(|window, cx| _ = window.draw(cx));
    };
    let sent = |cx: &mut gpui::VisualTestContext| {
        cx.run_until_parked();
        let mut sent = Vec::new();
        while let Ok(line) = outgoing.try_recv() {
            match tcode_protocol::decode_client_line(&line).unwrap().payload {
                tcode_protocol::ClientPayload::Command(tcode_protocol::Command::Steer {
                    text,
                    ..
                }) => sent.push(("steer", text)),
                tcode_protocol::ClientPayload::Command(tcode_protocol::Command::SendTurn {
                    text,
                    ..
                }) => sent.push(("queue", text)),
                _ => {}
            }
        }
        sent
    };
    assert!(sent(cx).is_empty());

    for (gesture, expected) in [
        ("tap", "queue"),
        ("hold", "queue"),
        ("hold-slide", "steer"),
        ("slide", "steer"),
        ("hold-sideways", "queue"),
        ("steer-sideways", "steer"),
    ] {
        composer.update_in(cx, |composer, window, cx| composer.focus(window, cx));
        cx.simulate_input(gesture);
        cx.update(|window, cx| _ = window.draw(cx));
        let button = cx
            .debug_bounds("steer-turn")
            .expect("the Queue button is shown");
        let center = button.center();
        touch(cx, TouchPhase::Started, center);
        let mut lift = center;
        if matches!(gesture, "hold" | "hold-slide" | "hold-sideways") {
            cx.executor()
                .advance_clock(gpui::GestureTuning::default().long_press_duration);
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }
        if matches!(gesture, "slide" | "steer-sideways") {
            touch(
                cx,
                TouchPhase::Moved,
                point(center.x, button.top() - px(1.)),
            );
        }
        if gesture != "tap" {
            cx.update(|window, cx| _ = window.draw(cx));
            let upper = cx
                .debug_bounds("steer-gesture-steer")
                .expect("holding reveals the upper Steer button");
            let lower = cx
                .debug_bounds("steer-gesture-queue")
                .expect("holding reveals the lower Queue button");
            lift = match gesture {
                // Level with Queue queues wherever the finger is
                // horizontally; anywhere above the gap under Steer steers.
                "hold-sideways" => point(center.x - px(120.), lower.top() - px(4.)),
                "steer-sideways" => point(center.x - px(120.), upper.top() - px(40.)),
                "slide" | "hold-slide" => upper.center(),
                _ => lift,
            };
            touch(cx, TouchPhase::Moved, lift);
        }
        assert!(
            sent(cx).is_empty(),
            "nothing is sent before the finger lifts"
        );
        touch(cx, TouchPhase::Ended, lift);
        assert_eq!(sent(cx), vec![(expected, gesture.to_string())], "{gesture}");
    }

    composer.update_in(cx, |composer, window, cx| composer.focus(window, cx));
    cx.simulate_input("kept");
    for gesture in ["down", "cancel"] {
        cx.update(|window, cx| _ = window.draw(cx));
        let button = cx.debug_bounds("steer-turn").unwrap();
        touch(cx, TouchPhase::Started, button.center());
        let away = point(button.center().x - px(120.), button.bottom() + px(1.));
        if gesture == "cancel" {
            touch(
                cx,
                TouchPhase::Moved,
                point(button.center().x, button.top() - px(1.)),
            );
            let upper = cx.debug_bounds("steer-gesture-steer").unwrap();
            touch(cx, TouchPhase::Moved, upper.center());
            touch(cx, TouchPhase::Cancelled, upper.center());
        } else {
            touch(cx, TouchPhase::Moved, away);
            touch(cx, TouchPhase::Ended, away);
        }
        assert!(sent(cx).is_empty(), "{gesture} sends nothing");
    }

    replace(
        &incoming,
        Topic::SessionStatus {
            session_id: unsupported.session_id.clone(),
        },
        ServerEvent::SessionStatusReplaced(Box::new(unsupported)),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    cx.update(|window, cx| _ = window.draw(cx));
    let button = cx.debug_bounds("steer-turn").unwrap();
    touch(cx, TouchPhase::Started, button.center());
    cx.executor()
        .advance_clock(gpui::GestureTuning::default().long_press_duration);
    cx.run_until_parked();
    cx.update(|window, cx| _ = window.draw(cx));
    assert!(cx.debug_bounds("steer-gesture-steer").is_none());
    assert!(cx.debug_bounds("steer-gesture-queue").is_none());
    touch(cx, TouchPhase::Cancelled, button.center());
    assert!(sent(cx).is_empty());

    // What nothing sent is still the draft.
    touch(cx, TouchPhase::Started, button.center());
    touch(cx, TouchPhase::Ended, button.center());
    assert_eq!(sent(cx), vec![("queue", "kept".to_string())]);
}

/// A stop the harness has not answered for a while offers to kill its
/// process, counted by this client's clock from when it first saw the
/// stop: the host's stamp names the stop, and the host's clock is not
/// this one. The offer itself only asks.
#[gpui::test]
fn an_unanswered_stop_offers_force_terminate_behind_a_confirmation(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, mut status) = attach(cx);
    let session_id = Some(status.session_id.clone());
    status.activity.turn_running = true;
    status.stopping = true;
    // Stamped by a host whose clock is far behind this one.
    status.stopping_since = Some(1);
    replace(
        &incoming,
        Topic::SessionStatus {
            session_id: status.session_id.clone(),
        },
        ServerEvent::SessionStatusReplaced(Box::new(status)),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    let (_root, cx) = cx.add_window_view(|window, cx| {
        let composer = cx.new(|cx| Composer::new(store.clone(), session_id.clone(), window, cx));
        gpui_base::Root::new(composer, window, cx)
    });
    cx.simulate_resize(gpui::size(px(1024.), px(768.)));
    cx.update(|_, cx| cx.set_reduce_motion(true));
    cx.update(|window, cx| _ = window.draw(cx));
    assert!(
        cx.debug_bounds("force-terminate").is_none(),
        "a stop just seen is not offered"
    );

    cx.executor().advance_clock(crate::composer::UNANSWERED);
    cx.run_until_parked();
    cx.update(|window, cx| _ = window.draw(cx));
    while outgoing.try_recv().is_ok() {}
    let button = cx
        .debug_bounds("force-terminate")
        .expect("offered after the wait");
    cx.simulate_click(button.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.update(|window, cx| crate::overlay::dialog_open(window, cx)));
    assert!(
        !std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
            tcode_protocol::decode_client_line(&line).unwrap().payload,
            tcode_protocol::ClientPayload::Command(tcode_protocol::Command::ForceTerminate { .. })
        )),
        "the process is killed only once the user confirms"
    );
}

/// A provider the next turn waits on, which runs no turn yet, can be
/// stopped from the composer.
#[gpui::test]
fn a_turn_still_being_prepared_offers_stop(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, mut status) = attach(cx);
    let session_id = Some(status.session_id.clone());
    status.preparing_turn = true;
    replace(
        &incoming,
        Topic::SessionStatus {
            session_id: status.session_id.clone(),
        },
        ServerEvent::SessionStatusReplaced(Box::new(status)),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    let (_root, cx) = cx.add_window_view(|window, cx| {
        let composer = cx.new(|cx| Composer::new(store.clone(), session_id.clone(), window, cx));
        gpui_base::Root::new(composer, window, cx)
    });
    cx.simulate_resize(gpui::size(px(1024.), px(768.)));
    cx.update(|_, cx| cx.set_reduce_motion(true));
    cx.update(|window, cx| _ = window.draw(cx));
    while outgoing.try_recv().is_ok() {}
    let stop = cx.debug_bounds("stop-turn").expect("Stop is offered");
    cx.simulate_click(stop.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
            tcode_protocol::decode_client_line(&line).unwrap().payload,
            tcode_protocol::ClientPayload::Command(tcode_protocol::Command::Interrupt { .. })
        ))
    );
}

/// The option chosen is the answer, whatever the free-form field holds.
/// A select field with a default arrives with the default in that field
/// (OpenCode's `form_question` makes it the prefill); choosing another
/// option answers with the one chosen.
#[gpui::test]
fn a_chosen_option_is_the_answer_whatever_the_custom_field_holds(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, mut status) = attach(cx);
    let option = |label: &str| agent::UserInputOption {
        label: label.into(),
        description: String::new(),
    };
    status.activity.turn_running = true;
    status.pending_user_input = Some(tcode_core::session::PendingUserInput {
        request_id: "ask".into(),
        questions: vec![UserInputQuestion {
            id: "color".into(),
            header: "Color".into(),
            question: "Which color?".into(),
            options: vec![option("Red"), option("Blue")],
            multi_select: false,
            prefill: Some("Red".into()),
            optional: false,
        }],
        delivery: agent::UserInputDelivery::Blocking,
    });
    let (_composer, cx) = composer_on(&store, &incoming, status, cx);
    while outgoing.try_recv().is_ok() {}
    cx.simulate_keystrokes("2");
    cx.executor().advance_clock(Duration::from_millis(200));
    cx.run_until_parked();
    let answers: Vec<_> = std::iter::from_fn(|| outgoing.try_recv().ok())
        .filter_map(
            |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                tcode_protocol::ClientPayload::Command(
                    tcode_protocol::Command::RespondUserInput { answers, .. },
                ) => Some(answers),
                _ => None,
            },
        )
        .collect();
    assert_eq!(
        answers,
        [serde_json::Map::from_iter([(
            "color".to_owned(),
            serde_json::json!("Blue")
        )])]
    );
}

/// A required question is not passed empty; an optional one is, and the
/// answers then go with it empty, for the provider to leave out.
#[gpui::test]
fn an_optional_question_is_submitted_empty(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, mut status) = attach(cx);
    let question = |id: &str, optional| UserInputQuestion {
        id: id.into(),
        header: id.into(),
        question: format!("{id}?"),
        options: Vec::new(),
        multi_select: false,
        prefill: None,
        optional,
    };
    status.activity.turn_running = true;
    status.pending_user_input = Some(tcode_core::session::PendingUserInput {
        request_id: "ask".into(),
        questions: vec![question("region", false), question("note", true)],
        delivery: agent::UserInputDelivery::Blocking,
    });
    let (_composer, cx) = composer_on(&store, &incoming, status, cx);
    while outgoing.try_recv().is_ok() {}
    let mut answer = |text: &str| {
        if !text.is_empty() {
            cx.simulate_input(text);
        }
        cx.simulate_keystrokes("enter");
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(
                |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                    tcode_protocol::ClientPayload::Command(
                        tcode_protocol::Command::RespondUserInput { answers, .. },
                    ) => Some(answers),
                    _ => None,
                },
            )
            .collect::<Vec<serde_json::Map<String, serde_json::Value>>>()
    };
    assert!(answer("").is_empty());
    assert!(answer("eu-west").is_empty());
    assert_eq!(
        answer(""),
        [serde_json::Map::from_iter([
            ("region".to_owned(), serde_json::json!("eu-west")),
            ("note".to_owned(), serde_json::json!("")),
        ])]
    );
}

#[gpui::test]
fn queue_edit_waits_for_host_removal_and_in_flight_actions_are_disabled(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, mut status) = attach(cx);
    let session_id = Some(status.session_id.clone());
    status.queued_messages = vec![tcode_protocol::QueuedMessageStatus {
        id: 7,
        editable: false,
        delivery_key: None,
        text: "Keep this queued until the host accepts the edit".into(),
        fire_at_unix_secs: Some(u64::MAX),
        space_held: false,
    }];
    let topic = Topic::SessionStatus {
        session_id: status.session_id.clone(),
    };
    replace(
        &incoming,
        topic.clone(),
        ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    let (composer, cx) = cx
        .add_window_view(|window, cx| Composer::new(store.clone(), session_id.clone(), window, cx));
    cx.simulate_resize(gpui::size(px(1024.), px(768.)));
    // Each step is drawn once; with motion reduced it is drawn at rest.
    cx.update(|_, cx| cx.set_reduce_motion(true));
    cx.update(|window, cx| _ = window.draw(cx));
    // What the send button sends: the draft.
    let send = |cx: &mut gpui::VisualTestContext| {
        let button = cx.debug_bounds("send-message").expect("the send button");
        cx.simulate_click(button.center(), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        sent_texts(&outgoing)
    };
    for action in ["queue-drop-7", "queue-steer-7"] {
        let button = cx.debug_bounds(action).expect("queue action visible");
        cx.simulate_click(button.center(), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
    }
    while let Ok(line) = outgoing.try_recv() {
        let message = tcode_protocol::decode_client_line(&line).unwrap();
        assert!(
            !matches!(
                message.payload,
                tcode_protocol::ClientPayload::Command(
                    tcode_protocol::Command::DropQueued { .. }
                        | tcode_protocol::Command::SteerQueued { .. }
                )
            ),
            "disabled actions must not send queue commands"
        );
    }
    status.queued_messages[0].editable = true;
    replace(
        &incoming,
        topic.clone(),
        ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    cx.update(|window, cx| _ = window.draw(cx));
    let drop_button = cx.debug_bounds("queue-drop-7").unwrap();
    cx.simulate_click(drop_button.center(), Modifiers::default());
    cx.update(|window, cx| _ = window.draw(cx));
    assert!(
        std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
            tcode_protocol::decode_client_line(&line).unwrap().payload,
            tcode_protocol::ClientPayload::Command(tcode_protocol::Command::DropQueued {
                id: 7,
                ..
            })
        )),
        "the enabled drop must reach the host"
    );
    assert!(
        cx.debug_bounds("queued-row-7").is_some(),
        "the pending host drop is still queued"
    );
    status.queued_messages.clear();
    replace(
        &incoming,
        topic.clone(),
        ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    cx.update(|window, cx| _ = window.draw(cx));
    assert!(cx.debug_bounds("queue-drop-7").is_none());
    assert_eq!(
        send(cx),
        ["Keep this queued until the host accepts the edit"]
    );
    status
        .queued_messages
        .push(tcode_protocol::QueuedMessageStatus {
            id: 8,
            editable: true,
            delivery_key: None,
            text: "Queued text".into(),
            fire_at_unix_secs: None,
            space_held: false,
        });
    replace(
        &incoming,
        topic.clone(),
        ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    cx.update(|window, cx| _ = window.draw(cx));
    let drop_button = cx.debug_bounds("queue-drop-8").unwrap();
    cx.simulate_click(drop_button.center(), Modifiers::default());
    cx.update(|window, cx| composer.update(cx, |composer, cx| composer.focus(window, cx)));
    cx.simulate_input("New draft while the drop is pending");
    cx.run_until_parked();
    status.queued_messages.clear();
    replace(
        &incoming,
        topic,
        ServerEvent::SessionStatusReplaced(Box::new(status)),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    cx.update(|window, cx| _ = window.draw(cx));
    assert_eq!(send(cx), ["New draft while the drop is pending"]);
}

/// Dragging a queued row onto another asks the host to send it in that
/// row's place; letting go where it started asks nothing.
#[gpui::test]
fn dragging_a_queued_row_moves_it_in_the_host_queue(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, mut status) = attach(cx);
    let session_id = Some(status.session_id.clone());
    status.queued_messages = (1..=3)
        .map(|id| tcode_protocol::QueuedMessageStatus {
            id,
            editable: true,
            delivery_key: None,
            text: format!("Queued {id}"),
            fire_at_unix_secs: None,
            space_held: false,
        })
        .collect();
    replace(
        &incoming,
        Topic::SessionStatus {
            session_id: status.session_id.clone(),
        },
        ServerEvent::SessionStatusReplaced(Box::new(status)),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    let (_composer, cx) = cx
        .add_window_view(|window, cx| Composer::new(store.clone(), session_id.clone(), window, cx));
    cx.simulate_resize(gpui::size(px(1024.), px(768.)));
    let draw = |cx: &mut gpui::VisualTestContext| cx.update(|window, cx| _ = window.draw(cx));
    draw(cx);
    let left = gpui::MouseButton::Left;
    let drag = |cx: &mut gpui::VisualTestContext, id: u64, to: gpui::Point<gpui::Pixels>| {
        let start = cx
            .debug_bounds(format!("queued-row-{id}").leak())
            .unwrap()
            .center();
        cx.simulate_mouse_down(start, left, Modifiers::default());
        cx.simulate_mouse_move(
            start + gpui::point(px(0.), px(8.)),
            left,
            Modifiers::default(),
        );
        draw(cx);
        cx.simulate_mouse_move(to, left, Modifiers::default());
        draw(cx);
        cx.simulate_mouse_up(to, left, Modifiers::default());
        draw(cx);
    };
    let moves = |outgoing: &async_channel::Receiver<String>| {
        std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(
                |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                    tcode_protocol::ClientPayload::Command(
                        tcode_protocol::Command::MoveQueued { id, before, .. },
                    ) => Some((id, before)),
                    _ => None,
                },
            )
            .collect::<Vec<_>>()
    };

    let first = cx.debug_bounds("queued-row-1").unwrap();
    drag(cx, 3, gpui::point(first.center().x, first.top() + px(2.)));
    assert_eq!(moves(&outgoing), [(3, Some(1))]);

    let last = cx.debug_bounds("queued-row-3").unwrap();
    drag(cx, 1, gpui::point(last.center().x, last.bottom() - px(2.)));
    assert_eq!(moves(&outgoing), [(1, None)]);

    let second = cx.debug_bounds("queued-row-2").unwrap().center();
    drag(cx, 2, second);
    assert!(moves(&outgoing).is_empty(), "a row dropped in place stays");
}

/// Choosing a thread from the `@` menu references it: the host is asked to
/// add the reference and the draft keeps no mention text.
#[gpui::test]
fn an_at_thread_is_referenced_rather_than_written_into_the_draft(cx: &mut TestAppContext) {
    let (store, incoming, outgoing, mut status) = attach(cx);
    let session_id = Some(status.session_id.clone());
    status.draft = false;
    status.control = Some(tcode_protocol::ControlStatus {
        grant: tcode_core::control::ControlGrant::default_grant(),
        source: tcode_core::control::ControlGrantSource::Default,
        pending_requests: Vec::new(),
        references: Vec::new(),
        self_settle: None,
        tools_offered: true,
    });
    let mut current =
        tcode_core::project::SessionMeta::new(status.provider, std::env::temp_dir(), None);
    current.id = status.session_id.clone();
    current.title = "Current".into();
    let mut other = current.clone();
    other.id = "release-thread".into();
    other.title = "Release notes".into();
    replace(
        &incoming,
        Topic::Index,
        ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
            summary: Default::default(),
            sessions: vec![current, other],
            projects: Vec::new(),
        }),
    );
    replace(
        &incoming,
        Topic::SessionStatus {
            session_id: status.session_id.clone(),
        },
        ServerEvent::SessionStatusReplaced(Box::new(status)),
    );
    cx.run_until_parked();
    cx.run_until_parked();
    let (composer, cx) = cx
        .add_window_view(|window, cx| Composer::new(store.clone(), session_id.clone(), window, cx));
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        composer.update(cx, |composer, cx| composer.focus(window, cx));
    });
    cx.simulate_input("@release");
    // The menu has folded open by the time it is clicked.
    crate::test_frames::draw_settled(cx);
    composer.read_with(cx, |composer, cx| {
        assert_eq!(composer.menu_rows(cx).0[0].primary, "Release notes");
    });
    let row = cx.debug_bounds("menu-row-0").expect("thread row");
    cx.simulate_click(row.center(), Modifiers::default());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    assert!(
        std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
            tcode_protocol::decode_client_line(&line).unwrap().payload,
            tcode_protocol::ClientPayload::Command(
                tcode_protocol::Command::AddThreadReference { thread_id, .. }
            ) if thread_id == "release-thread"
        )),
        "no AddThreadReference was sent"
    );
    cx.simulate_input("go");
    cx.simulate_keystrokes("enter");
    draw_settled(cx);
    assert_eq!(sent_texts(&outgoing), ["go"], "the draft kept no mention");
}
