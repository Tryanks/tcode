use super::*;

/// The call that started `agent` on `host-b` as a subagent of the thread it
/// is in, as the lead's harness reports it once it answered.
const START: &str = "start-on-b";

fn started_on_b() -> Vec<tcode_core::session::StoredEvent> {
    let call = agent::ItemContent::ToolCall {
        image_reads: Vec::new(),
        name: "mcp__tcode_control__start_thread_on_machine".into(),
        input: serde_json::json!({
            "machine": "host-b",
            "project_id": "project-b",
            "provider": "codex",
            "title": "agent",
            "message": "Survey the parser.",
            "subagent": true,
            "request_id": "start-1",
        }),
        output: Some(
            serde_json::json!({
                "thread_id": "agent",
                "link": "tcode-thread://agent@host-b",
                "created": true,
                "title": "agent",
                "subagent": true,
                "machine": { "id": "host-b", "name": "host-b" },
            })
            .to_string(),
        ),
        status: agent::ItemStatus::Completed,
        family: None,
    };
    vec![
        agent::AgentEvent::TurnStarted {
            turn_id: "turn-1".into(),
        }
        .into(),
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: START.into(),
            parent_item_id: None,
            content: call,
        })
        .into(),
        agent::AgentEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: agent::TurnStatus::Completed,
            usage: None,
        }
        .into(),
    ]
}

/// A subagent a lead started on another machine of its group is that
/// machine's thread: while the window is linked to that machine it stands as
/// that machine's store says, whatever the lead's host last heard; once that
/// link is down it stands as out of reach, not as failed, since its machine
/// may still be running it, and its card in the lead says so on a line of
/// its own.
#[gpui::test]
fn an_agent_elsewhere_stands_as_its_machine_says_and_out_of_reach_once_unlinked(
    cx: &mut TestAppContext,
) {
    use crate::agents_panel::{AgentState, Agents};
    use tcode_core::control::{RemoteChild, RemoteChildState};
    use tcode_core::settlement::AgentExecution;
    cx.update(|cx| cx.set_reduce_motion(true));
    let (shell, hosts, _client, cx) = mount_workspace(cx, two_hosts(), Default::default(), 1200.);
    let (key_a, key_b) = (
        HostKey::Remote("host-a".into()),
        HostKey::Remote("host-b".into()),
    );
    let (a, b) = (
        store_on(&shell, "host-a", cx),
        store_on(&shell, "host-b", cx),
    );
    hosts[&key_a].come_up(&a, Vec::new(), None, cx);
    hosts[&key_b].come_up(&b, Vec::new(), None, cx);
    let thread = |id: &str| {
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            PANES_CWD.into(),
            None,
        );
        meta.id = id.into();
        meta.title = id.into();
        meta
    };
    // The lead's host last heard its agent running.
    let mut lead = thread("lead");
    lead.remote_children = vec![RemoteChild {
        machine: "host-b".into(),
        thread_id: "agent".into(),
        title: "agent".into(),
        request_id: "start-1".into(),
        subagent: true,
        state: RemoteChildState::Running,
        updated_at: 1,
        pending: None,
        report_taken: None,
        report_taken_unsent: false,
    }];
    let index = |sessions| {
        (
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions,
                projects: Vec::new(),
            }),
        )
    };
    send_events(&hosts[&key_a].host, vec![index(vec![lead])]);
    // Its own machine has it at rest: its turn ended, and says so as the host
    // does for a thread with a lead.
    let at_rest = |execution| {
        let mut summary = tcode_protocol::IndexSummary::default();
        summary.activity.insert(
            "agent".into(),
            tcode_protocol::SessionActivity {
                agent: Some(tcode_protocol::AgentStatus {
                    execution,
                    delivery: tcode_core::settlement::AgentDelivery::Running,
                    report: tcode_core::settlement::AgentReport::Expected,
                    run_started_at: None,
                    run_completed_at: None,
                }),
                working: false,
                turn_running: false,
                background: Default::default(),
                unsettled: 0,
                wakes: Default::default(),
                waiting_for_approval: false,
                waiting_for_input: false,
                asking: Vec::new(),
                settle_blocked: false,
                failed: false,
                unread: false,
                last_activity_at: None,
                fork: tcode_protocol::ForkAvailability::Available,
                latest_activity: None,
            },
        );
        (
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary,
                sessions: vec![thread("agent")],
                projects: Vec::new(),
            }),
        )
    };
    send_events(&hosts[&key_b].host, vec![at_rest(AgentExecution::Finished)]);
    serve_status(&hosts[&key_a].host, "lead", false);
    send_events(
        &hosts[&key_a].host,
        vec![(
            Topic::SessionEvents {
                session_id: "lead".into(),
            },
            ServerEvent::SessionSnapshot {
                total: 3,
                total_turns: 1,
                truncated: false,
                from: 0,
                end: 3,
                records: started_on_b(),
            },
        )],
    );
    a.update(cx, |store, cx| store.show_thread("lead".into(), cx));
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("lead"), cx)
    });
    crate::test_frames::draw_settled(cx);
    let standing = |cx: &mut VisualTestContext| {
        a.read_with(cx, |store, cx| {
            Agents::of(store, "lead", cx)
                .lines()
                .into_iter()
                .map(|line| (line.key, line.state))
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        standing(cx),
        [(
            "agent@host-b".to_owned(),
            Some(AgentState::Execution(AgentExecution::Finished))
        )]
    );
    assert!(
        cx.debug_bounds(format!("thread-card-{START}").leak())
            .is_some()
    );
    assert!(
        cx.debug_bounds(format!("thread-card-notes-{START}").leak())
            .is_none()
    );

    // Its turn is interrupted on its machine, which the lead's record cannot
    // tell from running.
    send_events(
        &hosts[&key_b].host,
        vec![at_rest(AgentExecution::Interrupted)],
    );
    cx.run_until_parked();
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        standing(cx),
        [(
            "agent@host-b".to_owned(),
            Some(AgentState::Execution(AgentExecution::Interrupted))
        )]
    );

    hosts[&key_b]
        .host
        .states
        .try_send(tcode_client::ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        })
        .unwrap();
    cx.run_until_parked();
    crate::test_frames::draw_settled(cx);
    assert_eq!(
        standing(cx),
        [("agent@host-b".to_owned(), Some(AgentState::Unreachable))]
    );
    assert!(
        cx.debug_bounds(format!("thread-card-notes-{START}").leak())
            .is_some()
    );
}
