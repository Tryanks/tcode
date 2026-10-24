use super::*;
use crate::app::active_session::{QueuedMessageKind, Runtime};
use crate::app::test_support::*;
use control_mcp::{
    DeleteThreadsParams, GetThreadParams, GitOp, GitParams, OutlineParams, PatchSettingsParams,
    RequestGrantParams, ThreadChange, ThreadOp, UpdateThreadsParams,
};
use tcode_core::control::{ControlCategory, ControlTier};
use tcode_protocol::{Command, CommandResponse, HostMessage};

fn thread(id: &str, project: &str) -> SessionMeta {
    let mut meta = SessionMeta::new(
        ProviderKind::ClaudeCode,
        PathBuf::from("/tmp/synthetic-checkout"),
        None,
    );
    meta.id = id.into();
    meta.title = format!("Thread {id}");
    meta.project_id = Some(project.into());
    meta
}

fn caller(project: &str, grant: ControlGrant) -> Caller {
    Caller {
        thread_id: "caller".into(),
        project_id: Some(project.into()),
        grant,
    }
}

fn tool_names(state: &AppState, id: &str) -> Vec<String> {
    state
        .session_status_snapshot(id)
        .unwrap()
        .injected_tools
        .map(|tools| tools.tools.into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default()
}

/// A thread whose provider takes MCP servers is registered with `tcode_control` and its
/// grant's tools are disclosed; turning the setting off withdraws both.
#[test]
fn control_tools_follow_the_grant_and_the_setting() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-registration");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let mut host = mcp_host::Host::bind().unwrap();
    let server = control_mcp::start(&mut host);
    let meta = thread("caller", "p");
    state.update(cx, |state, _| {
        state.attach_control_mcp(server);
        state.sessions.push(meta.clone());
        state.install_selected(ActiveSession::new(meta.clone(), false, Vec::new()));
        let registration = state.control_registration_for(&meta).unwrap();
        assert_eq!(
            registration.name,
            agent::McpRegistration::SERVER_NAME_CONTROL
        );
        let tools = tool_names(state, "caller");
        assert!(tools.iter().any(|name| name == "list_projects"));
        assert!(tools.iter().any(|name| name == "request_grant"));
        assert!(!tools.iter().any(|name| name == "delete_threads"));
        assert!(
            state
                .session_status_snapshot("caller")
                .unwrap()
                .control
                .unwrap()
                .tools_offered
        );
    });

    state.dispatch_command(
        cx,
        1,
        Command::PatchSettings {
            patch: SettingsPatch::ControlEnabled(false),
        },
    );
    cx.run_until_parked();
    state.update(cx, |state, _| {
        let meta = state.find_meta("caller").unwrap();
        assert!(state.control_registration_for(&meta).is_none());
        assert!(tool_names(state, "caller").is_empty());
        assert!(
            !state
                .session_status_snapshot("caller")
                .unwrap()
                .control
                .unwrap()
                .tools_offered
        );
    });
}

/// A call outside the grant is refused with the reason, and the operations no grant exposes
/// are refused or absent whatever the grant.
#[test]
fn control_calls_outside_the_grant_are_refused_with_the_reason() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-authorize");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.update(cx, |state, _| {
        state.sessions.push(thread("caller", "a"));
        state.sessions.push(thread("same", "a"));
        state.sessions.push(thread("other", "b"));
        let mut referenced = thread("referenced", "b");
        referenced.id = "referenced".into();
        state.sessions.push(referenced);
        let mut child = thread("child", "b");
        child.parent_session_id = Some("caller".into());
        state.sessions.push(child);
    });

    let observe = ControlGrant {
        tiers: [ControlTier::Observe].into_iter().collect(),
        ..ControlGrant::none()
    };
    let settle = ControlOp::UpdateThreads(UpdateThreadsParams {
        changes: vec![ThreadChange {
            thread_id: "same".into(),
            op: ThreadOp::Settle,
            expect: None,
        }],
        dry_run: None,
        request_id: None,
    });
    let delete = ControlOp::DeleteThreads(DeleteThreadsParams {
        thread_ids: vec!["same".into()],
        remove_worktree: None,
        request_id: None,
    });
    let get = |id: &str| {
        ControlOp::GetThread(GetThreadParams {
            thread_id: id.into(),
        })
    };
    state.read(|state| {
        assert_eq!(
            state.authorize_control(&caller("a", observe.clone()), &settle),
            Err("refused: `update_threads` needs tier organize; your grant: observe (project)".into())
        );
        assert_eq!(
            state.authorize_control(&caller("a", ControlGrant::default_grant()), &delete),
            Err("refused: `delete_threads` needs category delete; your grant: observe, organize, operate (project)".into())
        );
        assert_eq!(
            state.authorize_control(&caller("a", ControlGrant::default_grant()), &settle),
            Ok(())
        );
        assert_eq!(
            state.authorize_control(&caller("a", observe.clone()), &get("other")),
            Err("refused: thread other is outside your grant's scope (project)".into())
        );
        assert_eq!(
            state.authorize_control(&caller("a", observe.clone()), &get("missing")),
            Err("unknown thread: missing".into())
        );
        assert_eq!(
            state.authorize_control(&caller("a", observe.clone()), &get("child")),
            Ok(()),
            "an orchestrated child is readable across projects"
        );
        let host = ControlGrant {
            scope: ControlScope::Host,
            ..observe.clone()
        };
        assert_eq!(
            state.authorize_control(&caller("a", host), &get("other")),
            Ok(())
        );
        let push = ControlOp::Git(GitParams {
            thread_id: "same".into(),
            op: GitOp::Push,
            request_id: None,
        });
        assert_eq!(
            state.authorize_control(&caller("a", ControlGrant::default_grant()), &push),
            Err("refused: `git` needs category git_remote; your grant: observe, organize, operate (project)".into())
        );
    });

    state.update(cx, |state, cx| {
        state
            .add_thread_reference("caller", "referenced", cx)
            .unwrap();
        assert_eq!(
            state.authorize_control(
                &caller("a", observe.clone()),
                &ControlOp::Outline(OutlineParams {
                    thread_id: "referenced".into(),
                    level: None,
                    range: None,
                })
            ),
            Ok(()),
            "a referenced thread is readable across projects"
        );
    });

    let every_category = ControlGrant {
        scope: ControlScope::Host,
        tiers: ControlTier::ALL.into_iter().collect(),
        categories: ControlCategory::ALL.into_iter().collect(),
    };
    assert!(
        control_mcp::tool_descriptions(&every_category)
            .iter()
            .all(|(name, _)| !name.contains("approv")),
        "approvals are never a tool"
    );
    state.read(|state| {
        for patch in [
            SettingsPatch::ControlEnabled(false),
            SettingsPatch::ControlDefaultGrant(every_category.clone()),
            SettingsPatch::SettlementAutomation(tcode_core::control::SettlementAutomation::Off),
            SettingsPatch::RemoteHostingEnabled(true),
            SettingsPatch::SourceControlHost {
                host: "github.com".into(),
                kind: tcode_core::pull_request::HostKind::Github,
                enabled: None,
                account: None,
            },
        ] {
            let op = ControlOp::PatchSettings(PatchSettingsParams {
                patch: serde_json::to_value(&patch).unwrap(),
                request_id: None,
            });
            let refusal = state
                .authorize_control(&caller("a", every_category.clone()), &op)
                .unwrap_err();
            assert!(
                refusal.starts_with("refused: `patch_settings` never changes"),
                "{patch:?}: {refusal}"
            );
        }
        let allowed = ControlOp::PatchSettings(PatchSettingsParams {
            patch: serde_json::to_value(SettingsPatch::WordWrapDiffs(true)).unwrap(),
            request_id: None,
        });
        assert_eq!(
            state.authorize_control(&caller("a", every_category.clone()), &allowed),
            Ok(())
        );
    });
}

/// `request_grant` asks; the user's approval widens the thread's effective grant and the
/// thread hears the answer as an agent-origin message.
#[test]
fn an_approved_grant_request_widens_the_grant_and_wakes_the_thread() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-control-grant-request");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let request_id = state.update(cx, |state, cx| {
        let meta = thread("caller", "p");
        state.sessions.push(meta.clone());
        let mut session = ActiveSession::new(meta, false, Vec::new());
        session.runtime = Runtime::Starting { generation: 1 };
        state.residents.parked.insert("caller".into(), session);

        let (reply, response) = smol::channel::bounded(1);
        state.handle_control_op(
            "caller".into(),
            ControlOp::RequestGrant(RequestGrantParams {
                scope: ControlScope::Host,
                tiers: Vec::new(),
                categories: vec![ControlCategory::Delete],
                reason: "clean up finished threads".into(),
            }),
            reply,
            cx,
        );
        let answer = response.try_recv().unwrap().unwrap();
        assert_eq!(answer["status"], "pending");
        let request_id = answer["request_id"].as_str().unwrap().to_owned();
        let meta = state.find_meta("caller").unwrap();
        assert_eq!(meta.control_grant_requests[0].id, request_id);
        assert_eq!(
            meta.control_grant_requests[0].reason,
            "clean up finished threads"
        );
        assert_eq!(
            state.caller_grant(&meta),
            (ControlGrant::default_grant(), ControlGrantSource::Default)
        );
        request_id
    });
    cx.run_until_parked();
    cx.drain_outgoing();

    state.dispatch_command(
        cx,
        7,
        Command::RespondControlGrant {
            session_id: "caller".into(),
            request_id,
            approve: true,
        },
    );
    cx.run_until_parked();
    assert!(cx.drain_outgoing().iter().any(|message| matches!(
        message,
        HostMessage::Ack {
            id: 7,
            result: Ok(CommandResponse::Unit)
        }
    )));
    state.read(|state| {
        let meta = state.find_meta("caller").unwrap();
        assert!(meta.control_grant_requests.is_empty());
        let (grant, source) = state.caller_grant(&meta);
        assert_eq!(source, ControlGrantSource::Thread);
        assert_eq!(grant.scope, ControlScope::Host);
        assert!(grant.allows_tier(ControlTier::Operate));
        assert!(grant.allows_category(ControlCategory::Delete));
        assert_eq!(
            state
                .session_status_snapshot("caller")
                .unwrap()
                .control
                .unwrap()
                .grant,
            grant
        );

        let session = state.residents.parked.get("caller").unwrap();
        assert_eq!(session.queue.len(), 1);
        assert_eq!(session.queue[0].origin, MessageOrigin::Agent);
        assert_eq!(
            session.queue[0].kind,
            QueuedMessageKind::OrchestrateCallback
        );
        assert!(
            session.queue[0]
                .text
                .starts_with("[control] grant approved: delete (host)"),
            "{}",
            session.queue[0].text
        );
        let audited = session.timeline.entries.iter().any(|entry| {
            matches!(
                &entry.content,
                EntryContent::ControlAction(action)
                    if action.actor_thread_id == "caller" && action.action == "request_grant"
            )
        });
        assert!(audited, "the request is recorded in the thread");
        assert_eq!(state.control_activity(10).len(), 1);
    });
}
