use super::*;
use gpui::{TestAppContext, VisualTestContext};
use tcode_protocol::{
    ClientPayload, HostMessage, PullRequestCapabilities, PullRequestComment, PullRequestPatch,
    Query, QueryResponse, decode_client_line, encode_line,
};

const HEAD: &str = "2222222222222222222222222222222222222222";

fn files(range: std::ops::Range<usize>, next_cursor: Option<&str>) -> PullRequestFiles {
    PullRequestFiles {
        base: "1111111111111111111111111111111111111111".into(),
        head: HEAD.into(),
        files: range
            .map(|index| PullRequestFile {
                path: format!("src/{index}.rs"),
                previous_path: None,
                kind: agent::FileChangeKind::Modify,
                additions: Some(1),
                deletions: Some(0),
                patch: PullRequestPatch::Hunks("@@ -1 +1,2 @@\n a\n+b\n".into()),
                url: None,
            })
            .collect(),
        next_cursor: next_cursor.map(str::to_owned),
        complete: next_cursor.is_none(),
        changed_files: Some(150),
    }
}

/// The pull request reads the view has asked for and the host has not answered.
fn reads(requests: &async_channel::Receiver<String>) -> Vec<(u64, PullRequestRead)> {
    std::iter::from_fn(|| requests.try_recv().ok())
        .map(|line| decode_client_line(&line).unwrap())
        .filter_map(|request| match request.payload {
            ClientPayload::Query(Query::PullRequest { read, .. }) => Some((request.id, read)),
            _ => None,
        })
        .collect()
}

fn answer(
    incoming: &async_channel::Sender<String>,
    id: u64,
    response: PullRequestReadResponse,
    fresh_for: u64,
) {
    incoming
        .try_send(
            encode_line(&HostMessage::QueryResult {
                id,
                result: Ok(QueryResponse::PullRequest {
                    response: Box::new(response),
                    fresh_for,
                }),
            })
            .unwrap(),
        )
        .unwrap();
}

struct Mounted<'a> {
    view: Entity<PullRequestView>,
    requests: async_channel::Receiver<String>,
    incoming: async_channel::Sender<String>,
    _pump: gpui::Task<()>,
    cx: &'a mut VisualTestContext,
}

struct PhonePage(Entity<PullRequestView>);
impl Render for PhonePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions = self.0.update(cx, |view, cx| view.nav_actions(cx));
        v_flex()
            .size_full()
            .child(
                h_flex().children(actions.into_iter().enumerate().map(|(index, action)| {
                    div()
                        .debug_selector(move || format!("phone-action-{index}"))
                        .child(action)
                })),
            )
            .child(div().flex_1().child(self.0.clone()))
    }
}

#[gpui::test]
fn the_phone_navigation_menu_delivers_its_link_action(cx: &mut TestAppContext) {
    use gpui::VisualContext as _;
    let Mounted {
        view,
        requests,
        incoming,
        _pump,
        cx,
    } = mount(cx);
    let store = view.read_with(cx, |view, _| view.store.clone());
    let mut meta = tcode_core::project::SessionMeta::new(
        agent::ProviderKind::Codex,
        "/tmp/tcode-pr".into(),
        None,
    );
    meta.id = "session".into();
    meta.pull_requests = vec![ThreadPullRequestLink {
        key: PullRequestKey::new("github.com", "octo/repo", 7),
        url: "https://github.com/octo/repo/pull/7".into(),
        source: PullRequestSource::Dismissed,
        linked_at: None,
        snapshot: None,
        stack: Default::default(),
        sync_error: None,
        watch: None,
    }];
    incoming
        .try_send(
            encode_line(&HostMessage::Event(tcode_protocol::EventEnvelope {
                request_id: None,
                topic: tcode_protocol::Topic::Index,
                event: tcode_protocol::ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![meta],
                    projects: Vec::new(),
                }),
            }))
            .unwrap(),
        )
        .unwrap();
    crate::test_frames::draw_settled(cx);
    cx.run_until_parked();
    store.update(cx, |store, cx| store.show_alone("session".into(), cx));
    view.update(cx, |view, cx| {
        view.window_state
            .update(cx, |state, cx| state.set_compact(true, cx))
    });
    cx.replace_root_view(|_, _| PhonePage(view.clone()));
    cx.simulate_resize(gpui::size(px(393.), px(852.)));
    crate::test_frames::draw_settled(cx);
    let touch = |cx: &mut VisualTestContext, phase, position| {
        cx.simulate_event(gpui::TouchEvent {
            id: gpui::TouchId(1),
            phase,
            position,
            predicted_position: None,
            force: None,
        });
        crate::test_frames::draw_settled(cx);
    };
    let more = cx.debug_bounds("phone-action-1").unwrap().center();
    touch(cx, gpui::TouchPhase::Started, more);
    touch(cx, gpui::TouchPhase::Ended, more);
    let link = cx
        .debug_bounds("menu-item-3")
        .expect("link menu item")
        .center();
    touch(cx, gpui::TouchPhase::Started, link);
    touch(cx, gpui::TouchPhase::Ended, link);
    let commands: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok())
        .map(|line| decode_client_line(&line).unwrap().payload)
        .filter_map(|payload| match payload {
            ClientPayload::Command(command) => Some(command),
            _ => None,
        })
        .collect();
    assert!(commands.iter().any(|command| matches!(command, Command::LinkPullRequest { session_id, .. } if session_id == "session")), "phone menu must send a link command: {commands:?}");
}

/// A wide view showing octo/repo#7, its first reads asked for and unanswered.
fn mount(cx: &mut TestAppContext) -> Mounted<'_> {
    cx.update(crate::test_client::init);
    cx.update(crate::markdown::init);
    let (to_host, requests) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let pump = link.clone();
    let pump = cx.background_executor.spawn(async move {
        pump.pump().await;
    });
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
    let window_state = cx.new(|_| WindowState::default());
    let (view, cx) =
        cx.add_window_view(|_, cx| PullRequestView::new(store.clone(), window_state.clone(), cx));
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    view.update(cx, |view, cx| {
        view.show(
            "session".into(),
            PullRequestKey::new("github.com", "octo/repo", 7),
            cx,
        )
    });
    crate::test_frames::draw_settled(cx);
    Mounted {
        view,
        requests,
        incoming,
        _pump: pump,
        cx,
    }
}

/// An empty conversation on a host that offers everything.
fn conversation() -> PullRequestConversation {
    PullRequestConversation {
        description: PullRequestComment {
            id: "PR_7".into(),
            author: None,
            body: String::new(),
            created_at: "2026-10-01T00:00:00Z".into(),
            edited_at: None,
            url: None,
            review_state: None,
            reactions: Vec::new(),
            viewer_can_update: false,
            viewer_can_react: false,
        },
        comments: Vec::new(),
        threads: Vec::new(),
        complete: true,
        account: "account".into(),
        permissions: Default::default(),
        labels: Vec::new(),
        reviewers: Vec::new(),
        capabilities: PullRequestCapabilities::ALL,
    }
}

/// Answers a read other than a page of files, current for `fresh_for` seconds.
fn answer_other(
    incoming: &async_channel::Sender<String>,
    id: u64,
    read: PullRequestRead,
    fresh_for: u64,
) {
    let response = match read {
        PullRequestRead::Conversation => {
            PullRequestReadResponse::Conversation(Box::new(conversation()))
        }
        PullRequestRead::ViewedFiles => {
            PullRequestReadResponse::ViewedFiles(PullRequestViewedFiles {
                files: Vec::new(),
                complete: true,
            })
        }
        read => panic!("unexpected read {read:?}"),
    };
    answer(incoming, id, response, fresh_for);
}

#[gpui::test]
fn an_expiry_reread_keeps_the_pages_read_after_page_one_and_the_scroll(cx: &mut TestAppContext) {
    let Mounted {
        view,
        requests,
        incoming,
        _pump,
        cx,
    } = mount(cx);
    // A read in flight shows a spinner, which rests only with motion
    // reduced.
    cx.update(|_, cx| cx.set_reduce_motion(true));
    let later = 3600;
    let mut reread = None;
    for (id, read) in reads(&requests) {
        match read {
            // Page 1 lands as its expiry passes, so the view reads it again at once.
            PullRequestRead::Files { cursor: None } => answer(
                &incoming,
                id,
                PullRequestReadResponse::Files(files(0..100, Some("2"))),
                0,
            ),
            read => answer_other(&incoming, id, read, later),
        }
    }
    crate::test_frames::draw_settled(cx);
    for (id, read) in reads(&requests) {
        assert_eq!(read, PullRequestRead::Files { cursor: None });
        reread = Some(id);
    }
    let reread = reread.expect("an expired page 1 is read again");

    let scroll_to = |cx: &mut VisualTestContext, file: usize| {
        view.update(cx, |view, cx| {
            view.page()
                .unwrap()
                .files_view
                .list
                .as_ref()
                .unwrap()
                .scroll_to_file(file, cx);
            cx.notify();
        });
        crate::test_frames::draw_settled(cx);
    };
    scroll_to(cx, 95);
    let page_two = reads(&requests);
    assert_eq!(
        page_two.iter().map(|(_, read)| read).collect::<Vec<_>>(),
        [&PullRequestRead::Files {
            cursor: Some("2".into())
        }],
        "nearing the end of page 1 reads page 2"
    );
    view.update(cx, |view, cx| {
        view.page()
            .unwrap()
            .files_view
            .list
            .as_ref()
            .unwrap()
            .unified_list
            .scroll_by(px(7.));
        cx.notify();
    });
    crate::test_frames::draw_settled(cx);
    let before_append = view.read_with(cx, |view, _| {
        view.page()
            .unwrap()
            .files_view
            .list
            .as_ref()
            .unwrap()
            .unified_list
            .logical_scroll_top()
    });
    answer(
        &incoming,
        page_two[0].0,
        PullRequestReadResponse::Files(files(100..150, None)),
        later,
    );
    crate::test_frames::draw_settled(cx);
    let after_append = view.read_with(cx, |view, _| {
        view.page()
            .unwrap()
            .files_view
            .list
            .as_ref()
            .unwrap()
            .unified_list
            .logical_scroll_top()
    });
    assert_eq!(
        after_append.item_ix, before_append.item_ix,
        "paging keeps the same source row"
    );
    assert_eq!(
        after_append.offset_in_item, before_append.offset_in_item,
        "paging keeps the exact offset within the row"
    );
    scroll_to(cx, 120);

    answer(
        &incoming,
        reread,
        PullRequestReadResponse::Files(files(0..100, Some("2"))),
        later,
    );
    crate::test_frames::draw_settled(cx);
    view.update(cx, |view, _| {
        let page = view.page().unwrap();
        let held = page.files.data.as_ref().unwrap();
        assert_eq!(
            (held.files.len(), held.next_cursor.as_deref(), held.complete),
            (150, None, true),
            "the re-read of page 1 keeps the pages read after it"
        );
        assert_eq!(
            page.files_view.list.as_ref().unwrap().top_file(false),
            Some(120),
            "and the reader's place in them"
        );
    });
    assert!(reads(&requests).is_empty());
}

#[gpui::test]
fn a_reread_with_a_new_comment_keeps_the_readers_place(cx: &mut TestAppContext) {
    let Mounted {
        view,
        requests,
        incoming,
        _pump,
        cx,
    } = mount(cx);
    let with_comments = |count: usize| {
        let mut conversation = conversation();
        conversation.comments = (0..count)
            .map(|index| PullRequestComment {
                id: format!("C_{index}"),
                body: format!("Comment {index}"),
                created_at: format!("2026-10-02T00:00:{index:02}Z"),
                ..conversation.description.clone()
            })
            .collect();
        PullRequestReadResponse::Conversation(Box::new(conversation))
    };
    let later = 3600;
    for (id, read) in reads(&requests) {
        match read {
            // Lands as its expiry passes, so the view reads it again at once.
            PullRequestRead::Conversation => answer(&incoming, id, with_comments(40), 0),
            PullRequestRead::Files { cursor: None } => answer(
                &incoming,
                id,
                PullRequestReadResponse::Files(files(0..1, None)),
                later,
            ),
            read => answer_other(&incoming, id, read, later),
        }
    }
    view.update(cx, |view, cx| view.select_tab(Tab::Conversation, cx));
    crate::test_frames::draw_settled(cx);
    view.update(cx, |view, cx| {
        view.page()
            .unwrap()
            .conversation_view
            .list
            .scroll_by(px(1500.));
        cx.notify();
    });
    crate::test_frames::draw_settled(cx);
    let top = |cx: &mut VisualTestContext| {
        view.read_with(cx, |view, _| {
            view.page()
                .unwrap()
                .conversation_view
                .list
                .logical_scroll_top()
        })
    };
    let before = top(cx);
    assert!(before.item_ix > 0, "the reader scrolled into the comments");
    let reread = reads(&requests)
        .into_iter()
        .find(|(_, read)| *read == PullRequestRead::Conversation)
        .expect("an expired conversation is read again");
    answer(&incoming, reread.0, with_comments(41), later);
    crate::test_frames::draw_settled(cx);
    let after = top(cx);
    assert_eq!(
        (after.item_ix, after.offset_in_item),
        (before.item_ix, before.offset_in_item),
        "a new comment below keeps the row and offset the reader was at"
    );
}

/// The meta block has a Labels row where the host has labels: on one without them
/// (Bitbucket) there is none, not even an empty one, beside the reviewers, unless the host
/// still reports labels on the pull request.
#[gpui::test]
fn the_labels_row_is_absent_where_the_host_has_no_labels(cx: &mut TestAppContext) {
    for (labels, applied, shown) in [
        (true, false, true),
        (false, false, false),
        (false, true, true),
    ] {
        let Mounted {
            view,
            requests,
            incoming,
            _pump,
            cx,
        } = mount(cx);
        let later = 3600;
        for (id, read) in reads(&requests) {
            let response = match read {
                PullRequestRead::Conversation => {
                    PullRequestReadResponse::Conversation(Box::new(PullRequestConversation {
                        reviewers: vec![tcode_protocol::PullRequestReviewerState {
                            reviewer: tcode_protocol::PullRequestReviewer {
                                id: "reviewer".into(),
                                login: "reviewer".into(),
                                kind: tcode_protocol::PullRequestReviewerKind::User,
                            },
                            avatar_url: None,
                            verdict: None,
                        }],
                        labels: applied
                            .then(|| tcode_protocol::PullRequestLabel {
                                id: "bug".into(),
                                name: "bug".into(),
                                color: None,
                                description: None,
                            })
                            .into_iter()
                            .collect(),
                        capabilities: PullRequestCapabilities {
                            labels,
                            ..PullRequestCapabilities::ALL
                        },
                        ..conversation()
                    }))
                }
                PullRequestRead::Files { cursor: None } => {
                    PullRequestReadResponse::Files(files(0..1, None))
                }
                read => {
                    answer_other(&incoming, id, read, later);
                    continue;
                }
            };
            answer(&incoming, id, response, later);
        }
        crate::test_frames::draw_settled(cx);
        view.update(cx, |view, cx| view.select_tab(Tab::Conversation, cx));
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("pr-meta-reviewers").is_some());
        assert_eq!(
            cx.debug_bounds("pr-meta-labels").is_some(),
            shown,
            "labels offered: {labels}, applied: {applied}"
        );
    }
}

/// A host that gives no count of changed files (Bitbucket, Azure DevOps) shows the Files tab
/// without a number until the files read answers with one, never "Files 0".
#[gpui::test]
fn a_snapshot_without_a_file_count_labels_the_files_tab_without_one(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let Mounted {
        view,
        requests,
        incoming,
        _pump,
        cx,
    } = mount(cx);
    let mut meta = tcode_core::project::SessionMeta::new(
        agent::ProviderKind::ClaudeCode,
        "/tmp/tcode-pr".into(),
        None,
    );
    meta.id = "session".into();
    meta.pull_requests = vec![ThreadPullRequestLink {
        key: PullRequestKey::new("github.com", "octo/repo", 7),
        url: "https://github.com/octo/repo/pull/7".into(),
        source: PullRequestSource::Manual,
        linked_at: None,
        snapshot: Some(tcode_core::pull_request::PullRequestSnapshot {
            state: PullRequestState::Open,
            title: "Change".into(),
            head_branch: "feature".into(),
            base_branch: "main".into(),
            is_draft: false,
            updated_at: Some("2026-10-01T00:00:00Z".into()),
            synced_at: 1,
            closed_at: None,
            merged_at: None,
            author: None,
            additions: None,
            deletions: None,
            changed_files: None,
            review_decision: None,
            checks_state: None,
            mergeability: Mergeability::Unknown,
        }),
        stack: Default::default(),
        sync_error: None,
        watch: None,
    }];
    incoming
        .try_send(
            encode_line(&HostMessage::Event(tcode_protocol::EventEnvelope {
                request_id: None,
                topic: tcode_protocol::Topic::Index,
                event: tcode_protocol::ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![meta],
                    projects: Vec::new(),
                }),
            }))
            .unwrap(),
        )
        .unwrap();
    crate::test_frames::draw_settled(cx);
    cx.run_until_parked();
    crate::test_frames::draw_settled(cx);
    let label = |cx: &mut VisualTestContext| view.read_with(cx, |view, cx| view.files_label(cx));
    assert_eq!(label(cx), "Files");

    let later = 3600;
    let (id, _) = reads(&requests)
        .into_iter()
        .find(|(_, read)| *read == PullRequestRead::Files { cursor: None })
        .expect("the first page of files is read");
    answer(
        &incoming,
        id,
        PullRequestReadResponse::Files(files(0..3, None)),
        later,
    );
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        label(cx),
        "Files 150",
        "the files read's count once it answers"
    );
}

#[gpui::test]
fn switching_language_relocalizes_the_file_filter_placeholder(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
    let Mounted {
        view,
        requests,
        incoming,
        _pump,
        cx,
    } = mount(cx);
    let later = 3600;
    for (id, read) in reads(&requests) {
        match read {
            PullRequestRead::Files { cursor: None } => answer(
                &incoming,
                id,
                PullRequestReadResponse::Files(files(0..3, None)),
                later,
            ),
            read => answer_other(&incoming, id, read, later),
        }
    }
    crate::test_frames::draw_settled(cx);
    crate::settings::assert_follows_language_switch(cx, "pull_requests.files.filter", |cx| {
        view.read_with(cx, |view, cx| {
            let (filter, _) = view.file_filter.as_ref().expect("the file column");
            filter.read(cx).presentation().placeholder().to_string()
        })
    });
}
