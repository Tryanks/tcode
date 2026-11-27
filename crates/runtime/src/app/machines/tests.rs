//! Two machines of a group, each a host as `spawn_host` starts it on a store of its own,
//! with `tcode_control` served and its harness played, joined over the group's real
//! transport on loopback (`real_group`).
use agent::{ItemContent, ItemStatus, SessionCommand, SessionOptions, ThreadItem};
use tcode_client::machines::MACHINE_REFUSES;
use tcode_core::control::RemoteChildOp;
use tcode_protocol::{Command, EventEnvelope, MachineCaller, ServerEvent, Subscription, Topic};

use super::*;
use crate::app::test_support::rpc;
use crate::app::{ScriptedProvider, scripted_provider};
use crate::pipe::HostServices;

const LAPTOP_PROJECT: &str = "laptop-project";

struct Index {
    link: tcode_client::HostLink,
    events: smol::channel::Receiver<EventEnvelope>,
    threads: HashMap<String, SessionMeta>,
}

impl Index {
    /// What client `link` shows of its host's index.
    fn of(link: tcode_client::HostLink) -> Self {
        let events = link.events();
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::Index,
            after: None,
        })
        .unwrap();
        Self {
            link,
            events,
            threads: HashMap::new(),
        }
    }

    /// The thread `id` once it is listed as `check` wants it.
    fn thread(&mut self, id: &str, check: impl Fn(&SessionMeta) -> bool) -> SessionMeta {
        self.until(|threads| threads.get(id).is_some_and(&check));
        self.threads[id].clone()
    }

    /// Show thread `id`, as a client that selects it does, once its log has come.
    fn show(&mut self, id: &str) {
        for topic in [
            Topic::SessionStatus {
                session_id: id.into(),
            },
            Topic::SessionEvents {
                session_id: id.into(),
            },
        ] {
            self.link
                .subscribe(Subscription {
                    known_index: None,
                    topic,
                    after: None,
                })
                .unwrap();
        }
        loop {
            let envelope = self.events.recv_blocking().expect("the host stopped");
            let shown = matches!(
                (&envelope.topic, &envelope.event),
                (Topic::SessionEvents { session_id }, ServerEvent::SessionSnapshot { .. })
                    if session_id == id
            );
            self.apply(envelope);
            if shown {
                return;
            }
        }
    }

    fn until(&mut self, check: impl Fn(&HashMap<String, SessionMeta>) -> bool) {
        while !check(&self.threads) {
            let envelope = self.events.recv_blocking().expect("the host stopped");
            self.apply(envelope);
        }
    }

    fn apply(&mut self, envelope: EventEnvelope) {
        {
            match envelope.event {
                ServerEvent::IndexSnapshot(snapshot) => {
                    self.threads = snapshot
                        .sessions
                        .into_iter()
                        .map(|meta| (meta.id.clone(), meta))
                        .collect();
                }
                ServerEvent::IndexUpsertSession(meta) => {
                    self.threads.insert(meta.id.clone(), *meta);
                }
                ServerEvent::IndexRemoveSession { session_id } => {
                    self.threads.remove(&session_id);
                }
                _ => {}
            }
        }
    }
}

/// The next command the played harness is sent that a test plays; the usage reads a
/// harness answers on its own are answered on the way.
fn next_command(provider: &ScriptedProvider) -> SessionCommand {
    loop {
        match provider
            .commands
            .recv_blocking()
            .expect("the host holds the provider")
        {
            SessionCommand::ReadAccountUsage { reply } => {
                let _ = reply.try_send(Err("the played harness has no account".into()));
            }
            other => return other,
        }
    }
}

/// The next turn the harness is sent, taken and begun as an adapter does: its text.
fn begin_turn(provider: &ScriptedProvider, turn_id: &str) -> String {
    loop {
        if let SessionCommand::SendTurn {
            delivery_id, text, ..
        } = next_command(provider)
        {
            play(
                provider,
                [
                    AgentEvent::TurnAccepted { delivery_id },
                    AgentEvent::TurnStarted {
                        turn_id: turn_id.into(),
                    },
                ],
            );
            return text;
        }
    }
}

fn play(provider: &ScriptedProvider, events: impl IntoIterator<Item = AgentEvent>) {
    for event in events {
        provider.events.try_send(event).unwrap();
    }
}

fn completed(turn_id: &str) -> AgentEvent {
    AgentEvent::TurnCompleted {
        turn_id: turn_id.into(),
        status: TurnStatus::Completed,
        usage: None,
    }
}

/// The subagent's `report_result` call as its transcript records it.
fn reported(id: &str, text: &str) -> AgentEvent {
    AgentEvent::ItemCompleted(ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content: ItemContent::ToolCall {
            image_reads: Vec::new(),
            name: "mcp__tcode_control__report_result".into(),
            input: serde_json::json!({ "text": text }),
            output: Some("{\"ok\":true}".into()),
            status: ItemStatus::Completed,
            family: None,
        },
    })
}

/// The `tcode_control` server a thread's harness was given at launch, as the harness
/// reaches it.
struct Tools {
    url: String,
    token: String,
    session: Option<String>,
}

impl Tools {
    fn connect(options: &SessionOptions) -> Self {
        let registration = options
            .mcp_servers
            .iter()
            .find(|server| server.name == agent::McpRegistration::SERVER_NAME_CONTROL)
            .expect("the harness was given tcode_control");
        let (url, token) = (registration.url.clone(), registration.bearer_token.clone());
        let (_, session) = rpc(
            &url,
            &token,
            None,
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"played-harness","version":"1"}}}),
        );
        rpc(
            &url,
            &token,
            session.as_deref(),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        );
        Self {
            url,
            token,
            session,
        }
    }

    fn listed(&self) -> Vec<String> {
        let (listed, _) = rpc(
            &self.url,
            &self.token,
            self.session.as_deref(),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect()
    }

    fn call(&self, tool: &str, arguments: serde_json::Value) -> Result<serde_json::Value, String> {
        let (reply, _) = rpc(
            &self.url,
            &self.token,
            self.session.as_deref(),
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":tool,"arguments":arguments}}),
        );
        if let Some(error) = reply.get("error") {
            return Err(error["message"].as_str().unwrap_or_default().to_owned());
        }
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if reply["result"]["isError"] == true {
            Err(text)
        } else {
            Ok(serde_json::from_str(&text).unwrap())
        }
    }
}

/// A machine of a real group over loopback, its host given `tcode_control` and a played
/// harness, and what its client shows of its index.
struct Site {
    machine: super::real_group::Machine,
    harness: ScriptedProvider,
    index: Index,
}

fn site(dir: &super::real_group::TestDir, name: &str) -> Site {
    let harness = scripted_provider(ProviderKind::Codex);
    let mut services = HostServices {
        provider_launcher: harness.launcher.clone(),
        ..Default::default()
    };
    services.start_mcp_servers(false);
    let machine = super::real_group::Machine::start_with(&dir.0, name, services);
    let index = Index::of(machine.client.clone());
    Site {
        machine,
        harness,
        index,
    }
}

/// A project `project` on the store a machine in `dir` opens, with thread `thread` in it
/// when one is named.
fn seed_real(dir: &super::real_group::TestDir, project: &str, thread: Option<&str>) {
    let store = tcode_services::store::SessionStore::open_at(dir.0.join("store")).unwrap();
    let root = dir.0.join(project);
    std::fs::create_dir_all(&root).unwrap();
    let mut seeded = Project::from_root(root.clone());
    seeded.id = project.into();
    store.upsert_project(&seeded).unwrap();
    if let Some(thread) = thread {
        let mut meta = SessionMeta::new(ProviderKind::Codex, root, None);
        meta.id = thread.into();
        meta.title = "Lead".into();
        meta.project_id = Some(project.into());
        store.upsert_meta(&meta).unwrap();
    }
}

/// The desk, whose thread `lead` works with the laptop, and the laptop, with a project and
/// a thread of its own, in one group over the real transport; the lead's harness has run
/// one turn and was given its tools.
struct Pair {
    desk: Site,
    laptop: Site,
    laptop_id: String,
    tools: Tools,
    lead_options: SessionOptions,
    _dirs: (super::real_group::TestDir, super::real_group::TestDir),
}

fn pair(tag: &str) -> Pair {
    use tcode_protocol::GroupAction;
    let desk_dir = super::real_group::TestDir::new(&format!("{tag}-desk"));
    let laptop_dir = super::real_group::TestDir::new(&format!("{tag}-laptop"));
    seed_real(&desk_dir, "desk-project", Some("lead"));
    seed_real(&laptop_dir, LAPTOP_PROJECT, Some("laptop-own"));
    let mut desk = site(&desk_dir, "Desk");
    let laptop = site(&laptop_dir, "Laptop");
    desk.machine.group(GroupAction::Create {
        name: "Home".into(),
    });
    laptop.machine.group(GroupAction::Join {
        link: desk
            .machine
            .endpoint
            .as_ref()
            .unwrap()
            .new_invitation()
            .url(),
        address: Some("127.0.0.1".into()),
    });
    let laptop_id = laptop.machine.id();
    desk.index.show("lead");
    desk.machine
        .command(Command::SendTurn {
            session_id: "lead".into(),
            text: "Work with the laptop".into(),
            attachment_paths: Vec::new(),
        })
        .unwrap();
    let lead_options = desk.harness.launches.recv_blocking().unwrap();
    begin_turn(&desk.harness, "lead-1");
    play(&desk.harness, [completed("lead-1")]);
    let tools = Tools::connect(&lead_options);
    Pair {
        desk,
        laptop,
        laptop_id,
        tools,
        lead_options,
        _dirs: (desk_dir, laptop_dir),
    }
}

fn start_subagent(pair: &Pair, request_id: &str) -> Result<serde_json::Value, String> {
    pair.tools.call(
        "start_thread_on_machine",
        serde_json::json!({
            "machine": pair.laptop_id,
            "project_id": LAPTOP_PROJECT,
            "provider": "codex",
            "subagent": true,
            "title": "Check the laptop build",
            "message": "Build it here and report what fails.",
            "request_id": request_id,
        }),
    )
}

/// The desk's lead as its own `get_thread` shows it.
fn lead_lineage(tools: &Tools) -> serde_json::Value {
    tools
        .call("get_thread", serde_json::json!({ "thread_id": "self" }))
        .unwrap()["lineage"]
        .clone()
}

fn children(tools: &Tools) -> Vec<serde_json::Value> {
    lead_lineage(tools)["remote_children"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// A thread started on another machine is an ordinary root thread there that names its
/// starter, once per request; a machine whose threads may not reach the group is not
/// offered the tool and is refused, and a machine that takes no threads leaves nothing on
/// the caller's side.
#[test]
fn a_thread_started_on_another_machine_is_a_root_thread_there_once_per_request() {
    use tcode_protocol::GroupAction;
    let mut pair = pair("start");
    assert!(
        pair.tools
            .listed()
            .contains(&"start_thread_on_machine".to_owned())
    );

    let started = start_subagent(&pair, "build-1").unwrap();
    let child = started["thread_id"].as_str().unwrap().to_owned();
    assert_eq!(started["created"], true);
    assert_eq!(started["machine"]["id"], pair.laptop_id.as_str());
    assert_eq!(
        started["link"],
        format!("tcode-thread://{child}@{}", pair.laptop_id).as_str()
    );
    let there = pair.laptop.index.thread(&child, |_| true);
    assert_eq!(there.parent_session_id, None);
    let parent = there.remote_parent.clone().unwrap();
    assert_eq!(
        (parent.machine, parent.thread_id.as_str()),
        (pair.desk.machine.id(), "lead")
    );
    let by = there.started_by.clone().unwrap();
    assert_eq!(
        (by.machine, by.thread_id.as_str(), by.request_id.as_str()),
        (Some(pair.desk.machine.id()), "lead", "build-1")
    );

    let again = start_subagent(&pair, "build-1").unwrap();
    assert_eq!(again["thread_id"], child.as_str());
    assert_eq!(again["created"], false);
    let listed = pair
        .tools
        .call(
            "list_threads",
            serde_json::json!({ "machine": pair.laptop_id, "scope": "host" }),
        )
        .unwrap();
    let started_there = listed["threads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|thread| thread["started_by"]["request_id"] == "build-1")
        .count();
    assert_eq!(started_there, 1);
    let kept = children(&pair.tools);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0]["thread_id"], child.as_str());
    assert_eq!(kept[0]["state"], "running");

    pair.laptop
        .machine
        .group(GroupAction::SetAcceptThreads(false));
    let refusal = start_subagent(&pair, "build-2").unwrap_err();
    assert!(refusal.contains(MACHINE_REFUSES), "{refusal}");
    assert_eq!(children(&pair.tools).len(), 1);
    pair.laptop
        .machine
        .group(GroupAction::SetAcceptThreads(true));

    pair.desk
        .machine
        .command(Command::PatchSettings {
            patch: tcode_core::settings::SettingsPatch::ControlMachines(false),
        })
        .unwrap();
    let reconnected = Tools::connect(&pair.lead_options);
    let listed = reconnected.listed();
    assert!(!listed.contains(&"start_thread_on_machine".to_owned()));
    assert!(!listed.contains(&"list_machines".to_owned()));
    assert!(
        reconnected
            .call(
                "list_projects",
                serde_json::json!({ "machine": pair.laptop_id })
            )
            .unwrap_err()
            .starts_with("refused")
    );
}

/// A report made while the lead's machine is out of reach reaches the lead once that
/// machine is back, and the subagent's machine keeps it until the lead's harness took it.
#[test]
fn a_report_made_while_the_lead_s_machine_is_unreachable_reaches_the_lead_when_it_returns() {
    let mut pair = pair("report");
    let started = start_subagent(&pair, "build-1").unwrap();
    let child = started["thread_id"].as_str().unwrap().to_owned();
    let brief = begin_turn(&pair.laptop.harness, "child-1");
    assert!(brief.starts_with("Build it here and report what fails."));
    pair.desk.machine.stop_hosting();

    play(
        &pair.laptop.harness,
        [
            reported("report-1", "Two tests fail on the laptop."),
            completed("child-1"),
        ],
    );
    pair.laptop.index.thread(&child, |meta| {
        meta.expectation
            .as_ref()
            .is_some_and(|expectation| expectation.undelivered.is_some())
    });

    pair.desk.machine.start_hosting();
    let taken = begin_turn(&pair.desk.harness, "lead-2");
    assert!(taken.contains("Two tests fail on the laptop."), "{taken}");
    assert!(
        taken.contains(&format!("thread {child}@{} (", pair.laptop_id)),
        "{taken}"
    );
    pair.laptop.index.thread(&child, |meta| {
        meta.expectation
            .as_ref()
            .is_some_and(|expectation| expectation.undelivered.is_none())
    });
    let lead = pair.desk.index.thread("lead", |meta| {
        meta.remote_children
            .iter()
            .any(|kept| kept.report_taken.is_some() && !kept.report_taken_unsent)
    });
    assert_eq!(lead.remote_children[0].state, RemoteChildState::Reported);
}

/// A thread on another machine may not ask for what is not a machine's to ask, nor answer
/// the approvals of a thread it did not start.
#[test]
fn a_machine_is_refused_what_lies_outside_what_machines_may_ask() {
    let pair = pair("refused");
    let caller = || MachineCaller {
        thread_id: "lead".into(),
        title: "Lead".into(),
        permission_mode: None,
        request_id: None,
    };
    for (tool, args) in [
        (
            "delete_threads",
            serde_json::json!({ "thread_ids": ["laptop-own"] }),
        ),
        (
            "git",
            serde_json::json!({ "thread_id": "laptop-own", "op": "push" }),
        ),
        (
            "respond_approval",
            serde_json::json!({ "thread_id": "laptop-own", "cancel": true }),
        ),
    ] {
        let answer = smol::block_on(pair.desk.machine.link.call(
            &pair.laptop_id,
            MachineRequest::Control {
                caller: caller(),
                tool: tool.into(),
                args,
            },
        ));
        assert_eq!(
            answer.map_err(|error| error.code),
            Err("refused".to_owned()),
            "{tool}"
        );
    }

    // Of the laptop's threads the lead reaches only the one its machine started there:
    // another is answered as no thread at all.
    let child = start_subagent(&pair, "build-1").unwrap()["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();
    begin_turn(&pair.laptop.harness, "child-1");
    let on_laptop = |tool: &str, mut args: serde_json::Value| {
        args["machine"] = pair.laptop_id.as_str().into();
        pair.tools.call(tool, args)
    };
    let item = |answer: serde_json::Value| {
        let result = &answer["results"][0];
        (
            result["outcome"].as_str().unwrap().to_owned(),
            result["reason"].as_str().map(str::to_owned),
        )
    };
    for (tool, args) in [
        (
            "get_thread",
            serde_json::json!({ "thread_id": "laptop-own" }),
        ),
        ("outline", serde_json::json!({ "thread_id": "laptop-own" })),
        (
            "respond_input",
            serde_json::json!({ "thread_id": "laptop-own", "answers": {} }),
        ),
    ] {
        let refusal = on_laptop(tool, args).unwrap_err();
        assert!(refusal.contains("unknown thread"), "{tool}: {refusal}");
    }
    let message = |thread: &str| serde_json::json!({ "thread_ids": [thread], "text": "Status?", "mode": "queue" });
    let interrupt = |thread: &str| serde_json::json!({ "changes": [{ "thread_id": thread, "op": "interrupt" }] });
    for (tool, args) in [
        ("send_message", message("laptop-own")),
        ("control_thread", interrupt("laptop-own")),
    ] {
        assert_eq!(
            item(on_laptop(tool, args).unwrap()),
            ("skipped".to_owned(), Some("unknown_thread".to_owned())),
            "{tool}"
        );
    }
    let listed: Vec<String> = on_laptop("list_threads", serde_json::json!({ "scope": "host" }))
        .unwrap()["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(listed, std::slice::from_ref(&child));

    on_laptop("get_thread", serde_json::json!({ "thread_id": child })).unwrap();
    on_laptop("outline", serde_json::json!({ "thread_id": child })).unwrap();
    assert_eq!(
        item(on_laptop("send_message", message(&child)).unwrap()).0,
        "applied"
    );
    assert_eq!(
        item(on_laptop("control_thread", interrupt(&child)).unwrap()).0,
        "applied"
    );
}

/// Cancelling one's subagent on a machine that cannot be reached is kept, and reaches it
/// when the machine is back.
#[test]
fn a_cancel_issued_while_the_machine_is_unreachable_reaches_it_when_it_returns() {
    let mut pair = pair("cancel");
    let child = start_subagent(&pair, "build-1").unwrap()["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();
    begin_turn(&pair.laptop.harness, "child-1");
    pair.laptop.machine.stop_hosting();

    let cancelled = pair
        .tools
        .call(
            "control_thread",
            serde_json::json!({
                "machine": pair.laptop_id,
                "changes": [{ "thread_id": child, "op": "cancel" }],
            }),
        )
        .unwrap();
    assert_eq!(cancelled["results"][0]["outcome"], "applied");
    assert!(
        cancelled["results"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("queued")
    );
    assert_eq!(children(&pair.tools)[0]["pending"], "cancel");

    pair.laptop.machine.start_hosting();
    pair.laptop
        .index
        .thread(&child, |meta| meta.cancelled_at.is_some());
    pair.desk.index.thread("lead", |meta| {
        meta.remote_children
            .iter()
            .any(|kept| kept.state == RemoteChildState::Cancelled && kept.pending.is_none())
    });
}

/// A client stops or cancels a lead's subagent on another machine through the lead's
/// host, and is refused for a thread the lead did not start there.
#[test]
fn a_client_reaches_a_subagent_on_another_machine_through_its_lead_s_host() {
    let mut pair = pair("client-op");
    let child = start_subagent(&pair, "build-1").unwrap()["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();
    begin_turn(&pair.laptop.harness, "child-1");
    let laptop_id = pair.laptop_id.clone();
    let op = |thread: &str, op| Command::ControlRemoteChild {
        session_id: "lead".into(),
        machine: laptop_id.clone(),
        thread_id: thread.into(),
        op,
    };
    assert_eq!(
        pair.desk
            .machine
            .command(op("laptop-own", RemoteChildOp::Cancel))
            .map_err(|error| error.code),
        Err("not_a_remote_child".to_owned())
    );

    pair.desk
        .machine
        .command(op(&child, RemoteChildOp::Interrupt))
        .unwrap();
    while !matches!(
        next_command(&pair.laptop.harness),
        SessionCommand::Interrupt
    ) {}

    pair.desk
        .machine
        .command(op(&child, RemoteChildOp::Cancel))
        .unwrap();
    pair.laptop
        .index
        .thread(&child, |meta| meta.cancelled_at.is_some());
    pair.desk.index.thread("lead", |meta| {
        meta.remote_children
            .first()
            .is_some_and(|kept| kept.state == RemoteChildState::Cancelled && kept.pending.is_none())
    });
}
