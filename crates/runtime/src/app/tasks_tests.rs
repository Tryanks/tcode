use super::*;
use crate::app::test_support::*;
use serde_json::{Value, json};
use std::sync::Mutex;
use tcode_protocol::Command;

/// One MCP client connection to a thread's task tools, as its provider makes it.
struct Client {
    url: String,
    token: String,
    session: Option<String>,
    next: u64,
}

impl Client {
    fn connect(registration: &agent::McpRegistration) -> Self {
        let (init, session) = rpc(
            &registration.url,
            &registration.bearer_token,
            None,
            json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"tasks-fixture","version":"1"}}}),
        );
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "{init}"
        );
        rpc(
            &registration.url,
            &registration.bearer_token,
            session.as_deref(),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        );
        Self {
            url: registration.url.clone(),
            token: registration.bearer_token.clone(),
            session,
            next: 1,
        }
    }

    /// The tool's answer: its JSON on success, its message as `Err` otherwise.
    fn call(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        self.next += 1;
        let (reply, _) = rpc(
            &self.url,
            &self.token,
            self.session.as_deref(),
            json!({"jsonrpc":"2.0","id":self.next,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        );
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: {reply}"))
            .to_owned();
        if reply["result"]["isError"] == true {
            Err(text)
        } else {
            Ok(serde_json::from_str(&text).unwrap())
        }
    }
}

impl Client {
    /// The tool's answer to a call that asks for progress, each progress
    /// notification handed to `progress` as it arrives, before the answer.
    fn call_with_progress(
        &mut self,
        name: &str,
        arguments: Value,
        mut progress: impl FnMut(&Value),
    ) -> Result<Value, String> {
        use std::io::BufRead as _;
        self.next += 1;
        let body = json!({"jsonrpc":"2.0","id":self.next,"method":"tools/call","params":{"name":name,"arguments":arguments,"_meta":{"progressToken":"progress"}}});
        let mut request = ureq::post(&self.url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json, text/event-stream")
            .set("Content-Type", "application/json")
            .set("MCP-Protocol-Version", "2025-11-25");
        if let Some(session) = &self.session {
            request = request.set("Mcp-Session-Id", session);
        }
        let response = request
            .send_bytes(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        for line in std::io::BufReader::new(response.into_reader()).lines() {
            let line = line.unwrap();
            let Some(data) = line
                .strip_prefix("data: ")
                .filter(|data| !data.trim().is_empty())
            else {
                continue;
            };
            let message: Value = serde_json::from_str(data).unwrap();
            if message["method"] == "notifications/progress" {
                progress(&message["params"]);
                continue;
            }
            let text = message["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned();
            return if message["result"]["isError"] == true {
                Err(text)
            } else {
                Ok(serde_json::from_str(&text).unwrap())
            };
        }
        panic!("{name}: the stream ended without an answer");
    }
}

/// Calls made off the host thread, as a provider makes them while the host runs.
struct Calls<T>(Arc<Mutex<Option<T>>>, std::thread::JoinHandle<()>);

impl<T: Send + 'static> Calls<T> {
    fn spawn(work: impl FnOnce() -> T + Send + 'static) -> Self {
        let result = Arc::new(Mutex::new(None));
        let done = result.clone();
        let handle = std::thread::spawn(move || *done.lock().unwrap() = Some(work()));
        Self(result, handle)
    }

    fn wait(self, cx: &mut TestAppContext) -> T {
        cx.run_until(|_| self.0.lock().unwrap().is_some());
        self.1.join().unwrap();
        self.0.lock().unwrap().take().unwrap()
    }
}

struct Thread {
    store: TestStore,
    state: TestEntity,
    registration: agent::McpRegistration,
    commands: smol::channel::Receiver<SessionCommand>,
    cwd: PathBuf,
}

const ID: &str = "thread";

/// A thread of `provider` with a live provider and its task tools served.
fn thread(cx: &mut TestAppContext, provider: ProviderKind, turn_running: bool) -> Thread {
    let store = TestStore::new("tcode-tasks");
    let cwd = store.root().join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let mut host = mcp_host::Host::bind().unwrap();
    let server = tasks_mcp::start(&mut host);
    host.start().unwrap();
    let (commands, receiver) = smol::channel::unbounded();
    let registration = state.update(cx, |state, cx| {
        state.attach_tasks_mcp(Some(server), cx);
        let mut meta = SessionMeta::new(provider, cwd.clone(), None);
        meta.id = ID.into();
        state.sessions.push(meta.clone());
        let mut session = ActiveSession::new(meta.clone(), false, Vec::new());
        session.runtime = Runtime::Live(commands);
        session.turn_in_flight = turn_running;
        state.install_selected(session);
        state.tasks_registration_for(&meta).unwrap()
    });
    Thread {
        store,
        state,
        registration,
        commands: receiver,
        cwd,
    }
}

impl Thread {
    fn gate(&self) -> PathBuf {
        self.store
            .root()
            .join(format!("gate-{}", uuid::Uuid::new_v4()))
    }

    fn manager(&self) -> TaskManager {
        self.state
            .read(|state| state.mcp.tasks.manager.clone().unwrap())
    }

    fn sent_turns(&self) -> Vec<String> {
        self.sent_deliveries()
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    fn sent_deliveries(&self) -> Vec<(u64, String)> {
        let mut turns = Vec::new();
        while let Ok(command) = next_command(&self.commands) {
            if let SessionCommand::SendTurn {
                text, delivery_id, ..
            } = command
            {
                turns.push((delivery_id, text));
            }
        }
        turns
    }

    /// The provider takes the delivery and runs its turn to the end.
    fn run_turn(&self, cx: &mut TestAppContext, delivery_id: u64) {
        self.state.update(cx, |state, cx| {
            state.provider_event_for_test(ID, AgentEvent::TurnAccepted { delivery_id }, cx);
            state.provider_event_for_test(ID, turn_completed(), cx);
        });
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        if let Some(manager) = self.state.read(|state| state.mcp.tasks.manager.clone()) {
            manager.stop_all(ID, false);
        }
    }
}

fn turn_completed() -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: "turn-1".into(),
        status: agent::TurnStatus::Completed,
        usage: None,
    }
}

fn gated(gate: &Path, then: &str) -> String {
    format!(
        "while [ ! -e '{}' ]; do sleep 0.02; done; {then}",
        gate.display()
    )
}

/// Only a harness without background commands of its own is given the task tools: Claude
/// Code, Codex and Grok launch with exactly the registrations they had without them.
#[test]
fn the_task_tools_reach_only_harnesses_without_background_commands() {
    for (provider, offered) in [
        (ProviderKind::ClaudeCode, false),
        (ProviderKind::Codex, false),
        (ProviderKind::Grok, false),
        (ProviderKind::Pi, true),
        (ProviderKind::OpenCode, true),
        (ProviderKind::Cursor, true),
        (ProviderKind::Acp, true),
    ] {
        let cx = &mut TestAppContext::default();
        let store = TestStore::new("tcode-tasks-offered");
        let scripted = scripted_provider(provider);
        let state = cx.new_entity({
            let mut state = TestClientState::new((*store).clone());
            state.set_provider_launcher_for_test(scripted.launcher.clone());
            state
        });
        let mut host = mcp_host::Host::bind().unwrap();
        let server = tasks_mcp::start(&mut host);
        state.update(cx, |state, cx| {
            state.attach_tasks_mcp(Some(server), cx);
            let meta = SessionMeta::new(provider, store.root().clone(), None);
            let id = meta.id.clone();
            state.sessions.push(meta.clone());
            state.install_selected(ActiveSession::new(meta, false, Vec::new()));
            state.ensure_session_started(&id, cx);
        });
        let launches = scripted.launches.clone();
        cx.run_until(|_| !launches.is_empty());
        let names: Vec<String> = launches
            .try_recv()
            .unwrap()
            .mcp_servers
            .into_iter()
            .map(|server| server.name)
            .collect();
        let expected: &[&str] = if offered { &["tcode_tasks"] } else { &[] };
        assert_eq!(names, expected, "{provider:?}");
    }
}

/// What a thread's permission mode makes of a task command: a mode that already runs every
/// command unasked runs it, any other asks the user, and a member's turn in a space that
/// answers no approvals declines it.
#[test]
fn a_task_command_runs_unasked_only_where_the_thread_runs_everything_unasked() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-tasks-admission");
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    let selection = |id: &str, value: &str| OptionSelection {
        id: id.into(),
        value: serde_json::Value::String(value.into()),
    };
    state.update(cx, |state, _| {
        for (provider, selections, admission) in [
            (ProviderKind::Pi, vec![], Admission::Run),
            (ProviderKind::Cursor, vec![], Admission::Run),
            (ProviderKind::OpenCode, vec![], Admission::Ask),
            (
                ProviderKind::OpenCode,
                vec![selection("permission_mode", "auto")],
                Admission::Run,
            ),
            // An ACP agent's modes are its own.
            (
                ProviderKind::Acp,
                vec![selection(agent::ACP_MODE_OPTION_ID, "bypass")],
                Admission::Ask,
            ),
        ] {
            let mut meta = SessionMeta::new(provider, PathBuf::from("/x"), None);
            meta.option_selections = selections;
            assert_eq!(state.task_admission(&meta), admission, "{provider:?}");
        }

        let meta = SessionMeta::new(ProviderKind::OpenCode, PathBuf::from("/x"), None);
        let mut session = ActiveSession::new(meta.clone(), false, Vec::new());
        session.turn_policy = Some(tcode_protocol::SpacePolicy {
            approval: SpaceApproval::ReviewedOnly,
            ..tcode_protocol::SpacePolicy::new_space()
        });
        state.residents.live.insert(meta.id.clone(), session);
        assert!(matches!(state.task_admission(&meta), Admission::Decline(_)));
        state.mcp.tasks.allowed.insert(meta.id.clone());
        assert_eq!(state.task_admission(&meta), Admission::Run);
    });
}

/// A command that exits within the call answers with its code; one the user moves to the
/// background from its call's card goes on there, is listed with the thread's background
/// work and stops from there, and the end of each wakes the idle thread with a `[control]`
/// message, as an agent's.
#[test]
fn a_run_answers_its_exit_or_goes_on_in_the_background_and_wakes_the_thread() {
    let cx = &mut TestAppContext::default();
    let thread = thread(cx, ProviderKind::Pi, false);
    let registration = thread.registration.clone();
    let exited = Calls::spawn(move || {
        Client::connect(&registration).call(
            "task_run",
            json!({"command":"echo hi; exit 4","yield_after_s":600}),
        )
    })
    .wait(cx)
    .unwrap();
    assert_eq!(
        (&exited["status"], &exited["exit_code"], &exited["output"]),
        (&json!("exited"), &json!(4), &json!("hi\n"))
    );
    assert_eq!(exited["cwd"], json!(thread.cwd));

    thread.state.update(cx, |state, cx| {
        state.provider_event_for_test(
            ID,
            AgentEvent::ItemStarted(agent::ThreadItem {
                id: "call-2".into(),
                parent_item_id: None,
                content: ItemContent::ToolCall {
                    image_reads: Vec::new(),
                    name: "mcp__tcode_tasks__task_run".into(),
                    input: json!({}),
                    output: None,
                    status: agent::ItemStatus::InProgress,
                },
            }),
            cx,
        );
    });
    let gate = thread.gate();
    let command = gated(&gate, "echo built");
    let registration = thread.registration.clone();
    let moved = Calls::spawn(move || {
        Client::connect(&registration)
            .call("task_run", json!({"command":command,"yield_after_s":600}))
    });
    let manager = thread.manager();
    cx.run_until(|_| manager.foreground_for_item(ID, "call-2").is_some());
    thread.state.dispatch_command(
        cx,
        1,
        Command::MoveToBackground {
            session_id: ID.into(),
            item_id: "call-2".into(),
        },
    );
    let moved = moved.wait(cx).unwrap();
    assert_eq!(
        (&moved["status"], &moved["moved_by"]),
        (&json!("backgrounded"), &json!("user"))
    );
    let listed = thread.state.read(|state| {
        state
            .session_status_snapshot(ID)
            .unwrap()
            .background_tasks
            .into_iter()
            .map(|row| (row.task.id, row.task.item_id, row.listed))
            .collect::<Vec<_>>()
    });
    assert_eq!(listed, [("t2".into(), Some("call-2".into()), true)]);
    std::fs::write(&gate, "").unwrap();
    let mut turns = Vec::new();
    cx.run_until(|_| {
        turns.extend(thread.sent_deliveries());
        !turns.is_empty()
    });
    let [(delivery_id, wake)] = &turns[..] else {
        panic!("{turns:?}");
    };
    assert!(
        wake.starts_with("[control] task t2 exited with code 0 after "),
        "{wake}"
    );
    assert!(wake.ends_with(":\nbuilt"), "{wake}");
    assert!(matches!(
        tcode_core::notice::host_notice(wake),
        Some(tcode_core::notice::HostNotice::Control(messages)) if messages.len() == 1
    ));
    thread.run_turn(cx, *delivery_id);

    let registration = thread.registration.clone();
    let started = Calls::spawn(move || {
        Client::connect(&registration).call("task_start", json!({"command":"sleep 600"}))
    })
    .wait(cx)
    .unwrap();
    assert_eq!(started["task_id"], "t3");
    thread.state.dispatch_command(
        cx,
        2,
        Command::StopBackgroundTask {
            session_id: ID.into(),
            task_id: Some("t3".into()),
        },
    );
    let mut turns = Vec::new();
    cx.run_until(|_| {
        turns.extend(thread.sent_turns());
        !turns.is_empty()
    });
    assert!(
        turns[0].starts_with("[control] task t3 was stopped after "),
        "{turns:?}"
    );
}

/// Tasks that end while a turn runs wake the thread once that turn is over, never into it,
/// in one message however many ended.
#[test]
fn exits_during_a_turn_wake_the_thread_after_it_in_one_message() {
    let cx = &mut TestAppContext::default();
    let thread = thread(cx, ProviderKind::Pi, true);
    let gate = thread.gate();
    let commands = [gated(&gate, "echo first"), gated(&gate, "exit 2")];
    let registration = thread.registration.clone();
    let started = Calls::spawn(move || {
        let mut client = Client::connect(&registration);
        commands.map(|command| {
            client
                .call("task_start", json!({ "command": command }))
                .unwrap()["task_id"]
                .clone()
        })
    })
    .wait(cx);
    assert_eq!(started, [json!("t1"), json!("t2")]);
    std::fs::write(&gate, "").unwrap();
    cx.run_until(|state| {
        state
            .resident(ID)
            .unwrap()
            .queue
            .iter()
            .any(|message| message.text.contains("task t1 ") && message.text.contains("task t2 "))
    });
    let queued = thread.state.read(|state| {
        let queue = &state.resident(ID).unwrap().queue;
        assert_eq!(queue.len(), 1);
        (queue[0].text.clone(), queue[0].origin)
    });
    assert_eq!(queued.1, MessageOrigin::Agent);
    assert!(
        next_command(&thread.commands).is_err(),
        "delivered into the running turn"
    );
    assert!(matches!(
        tcode_core::notice::host_notice(&queued.0),
        Some(tcode_core::notice::HostNotice::Control(messages)) if messages.len() == 2
    ));

    thread.state.update(cx, |state, cx| {
        state.provider_event_for_test(ID, turn_completed(), cx)
    });
    let mut turns = Vec::new();
    cx.run_until(|_| {
        turns.extend(thread.sent_turns());
        !turns.is_empty()
    });
    assert_eq!(turns, [queued.0]);
}

/// Where the thread's permission mode asks, a command waits on the user's answer in Tcode's
/// approval UI: declined, it never runs; allowed for the thread, later ones run unasked.
#[test]
fn a_command_the_mode_asks_for_waits_on_the_users_approval() {
    let cx = &mut TestAppContext::default();
    let thread = thread(cx, ProviderKind::OpenCode, true);
    let marker = thread.store.root().join("ran");
    let answer = |cx: &mut TestAppContext, option: &str| {
        let request = {
            let mut request = None;
            cx.run_until(|state| {
                request = state
                    .session_status_snapshot(ID)
                    .unwrap()
                    .pending_approvals
                    .into_iter()
                    .next();
                request.is_some()
            });
            request.unwrap()
        };
        assert!(matches!(
            &request.kind,
            agent::ApprovalKind::ExecCommand { command, .. } if command.starts_with("touch ")
        ));
        thread.state.dispatch_command(
            cx,
            3,
            Command::RespondApproval {
                session_id: ID.into(),
                request_id: request.id,
                decision: ApprovalDecision::Option(option.into()),
            },
        );
    };
    let start = |registration: &agent::McpRegistration, marker: &Path| {
        let registration = registration.clone();
        let command = format!("touch '{}'", marker.display());
        Calls::spawn(move || {
            Client::connect(&registration).call("task_start", json!({ "command": command }))
        })
    };
    let declined = start(&thread.registration, &marker);
    answer(cx, DECLINE);
    let declined = declined.wait(cx).unwrap_err();
    assert!(declined.contains("declined"), "{declined}");
    assert!(thread.manager().list(ID).is_empty());

    let allowed = start(&thread.registration, &marker);
    answer(cx, ALLOW_THREAD);
    assert_eq!(allowed.wait(cx).unwrap()["task_id"], "t1");
    let unasked = start(&thread.registration, &marker).wait(cx).unwrap();
    assert_eq!(unasked["task_id"], "t2");
    assert!(
        thread
            .state
            .read(|state| state.approval_requests(ID).is_empty())
    );
}

/// A turn the user interrupts takes the command its `task_run` call waits on with it.
#[test]
fn an_interrupted_turn_stops_the_command_its_call_waits_on() {
    let cx = &mut TestAppContext::default();
    let thread = thread(cx, ProviderKind::Pi, true);
    let registration = thread.registration.clone();
    let run = Calls::spawn(move || {
        Client::connect(&registration).call(
            "task_run",
            json!({"command":"sleep 600","yield_after_s":600}),
        )
    });
    let manager = thread.manager();
    cx.run_until(|_| !manager.list(ID).is_empty());
    thread.state.update(cx, |state, cx| {
        state.provider_event_for_test(
            ID,
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: agent::TurnStatus::Interrupted,
                usage: None,
            },
            cx,
        );
    });
    let ended = run.wait(cx).unwrap();
    assert_eq!(
        (&ended["status"], &ended["stopped"]),
        (&json!("exited"), &json!(true))
    );
}

/// A run that asks for `delta` progress streams its output while it runs, with what no
/// notification carried in the answer; it runs with the environment the call gives, and the
/// user's "move to background" on the card of the call it names reaches it.
#[test]
fn a_run_streams_its_output_and_answers_to_the_card_of_its_call() {
    let cx = &mut TestAppContext::default();
    let thread = thread(cx, ProviderKind::Pi, true);
    let registration = thread.registration.clone();
    let path = std::env::var("PATH").unwrap();
    let seen = Arc::new(Mutex::new(false));
    let first_seen = seen.clone();
    let run = Calls::spawn(move || {
        let mut streamed = String::new();
        let answer = Client::connect(&registration).call_with_progress(
            "task_run",
            json!({
                "command": "echo \"first $TCODE_PROBE ${HOME-unset}\"; sleep 600",
                "yield_after_s": 600,
                "call_id": "bash-call",
                "output_progress": "delta",
                "env": {"PATH": path, "TCODE_PROBE": "from-pi"},
            }),
            |progress| {
                streamed.push_str(progress["message"].as_str().unwrap_or_default());
                if streamed.contains('\n') {
                    *first_seen.lock().unwrap() = true;
                }
            },
        );
        (streamed, answer)
    });
    cx.run_until(|_| *seen.lock().unwrap());
    thread.state.dispatch_command(
        cx,
        1,
        Command::MoveToBackground {
            session_id: ID.into(),
            item_id: "bash-call".into(),
        },
    );
    let (streamed, answer) = run.wait(cx);
    let answer = answer.unwrap();
    assert_eq!(
        (&answer["status"], &answer["moved_by"]),
        (&json!("backgrounded"), &json!("user"))
    );
    assert_eq!(
        format!("{streamed}{}", answer["output_rest"].as_str().unwrap()),
        "first from-pi unset\n"
    );
}
