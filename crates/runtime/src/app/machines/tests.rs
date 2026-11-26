//! Two machines of a group, each a host as `spawn_host` starts it, on a store of its own,
//! with `tcode_control` served and its harness played. Their links are served the way the
//! transport serves them: a request reaches the other host's machine entry point under the
//! asking machine's name, a machine that is down is unreachable, and Accept threads gates
//! what a thread asks. The transport's own admission is proven in `crates/traverse`.
use std::sync::Mutex;

use agent::{ApprovalClass, ItemContent, ItemStatus, SessionCommand, SessionOptions, ThreadItem};
use tcode_client::machines::{MACHINE_REFUSES, MACHINE_UNREACHABLE, MachineCall, MachineInfo};
use tcode_core::control::RemoteChildOp;
use tcode_protocol::{
    Command, EventEnvelope, MachineCaller, MachineRequestKind, ServerEvent, Subscription, Topic,
};

use super::*;
use crate::app::test_support::{TestStore, rpc};
use crate::app::{ScriptedProvider, scripted_provider};
use crate::pipe::{HostServices, SpawnedHost, spawn_host};

const DESK: &str = "desk-machine";
const LAPTOP: &str = "laptop-machine";
const LAPTOP_PROJECT: &str = "laptop-project";

struct Member {
    name: String,
    host: Option<SpawnedHost>,
    accepts_threads: bool,
}

/// The machines of the group and the hosts running on them now.
#[derive(Clone, Default)]
struct Group(Arc<Mutex<HashMap<String, Member>>>);

impl Group {
    fn machines(&self, asking: &str, probe: bool) -> Vec<MachineInfo> {
        let members = self.0.lock().unwrap();
        let mut machines: Vec<MachineInfo> = members
            .iter()
            .map(|(id, member)| {
                let other = id != asking;
                MachineInfo {
                    id: id.clone(),
                    name: member.name.clone(),
                    label: None,
                    this_machine: !other,
                    accepts_threads: Some(member.accepts_threads),
                    accepts_projects: Some(false),
                    reachable: (probe && other).then_some(member.host.is_some()),
                    protocol_ok: (probe && other).then_some(true),
                    relayed: (probe && other).then_some(false),
                }
            })
            .collect();
        machines.sort_by(|a, b| a.id.cmp(&b.id));
        machines
    }

    async fn deliver(
        &self,
        from: &str,
        to: &str,
        request: MachineRequest,
    ) -> Result<MachineResponse, ProtocolError> {
        let refusal = |code: &str| ProtocolError {
            code: code.into(),
            message: code.into(),
        };
        let (host, accepts, name) = {
            let members = self.0.lock().unwrap();
            let Some(member) = members.get(to) else {
                return Err(refusal(NOT_MEMBER));
            };
            let name = members[from].name.clone();
            (member.host.clone(), member.accepts_threads, name)
        };
        let Some(host) = host else {
            return Err(refusal(MACHINE_UNREACHABLE));
        };
        if request.kind() == MachineRequestKind::Thread && !accepts {
            return Err(refusal(MACHINE_REFUSES));
        }
        let peer = MachinePeer {
            id: from.to_owned(),
            name,
        };
        host.update_state_for_test(move |state, cx| {
            state.handle_machine_request(&peer, request, cx)
        })
        .await?
        .await
    }

    /// Serve `id`'s link from now on, as its transport does while it runs.
    fn serve(&self, id: &str, link: &MachineLink) {
        let calls = link.serve();
        let (group, id) = (self.clone(), id.to_owned());
        std::thread::spawn(move || {
            smol::block_on(async move {
                while let Ok(call) = calls.recv().await {
                    match call {
                        MachineCall::Machines { probe, reply } => {
                            let _ = reply.try_send(Ok(group.machines(&id, probe)));
                        }
                        MachineCall::Request {
                            machine,
                            request,
                            reply,
                        } => {
                            let (group, id) = (group.clone(), id.clone());
                            smol::spawn(async move {
                                let _ = reply
                                    .send(group.deliver(&id, &machine, *request).await)
                                    .await;
                            })
                            .detach();
                        }
                    }
                }
            })
        });
    }

    fn set_accepts_threads(&self, id: &str, accepts: bool) {
        self.0.lock().unwrap().get_mut(id).unwrap().accepts_threads = accepts;
    }
}

/// What the client showing a host's index sees of its threads.
struct Index {
    link: tcode_client::HostLink,
    events: smol::channel::Receiver<EventEnvelope>,
    threads: HashMap<String, SessionMeta>,
}

impl Index {
    fn open(host: &SpawnedHost) -> Self {
        Self::of(host.link())
    }

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

/// A machine of the group: its host, its played harness, and its way to the others.
struct Machine {
    id: String,
    root: PathBuf,
    host: SpawnedHost,
    link: MachineLink,
    provider: ScriptedProvider,
    index: Index,
}

impl Machine {
    fn boot(group: &Group, id: &str, name: &str, root: &Path) -> Self {
        let store = tcode_services::store::SessionStore::open_at(root.to_path_buf()).unwrap();
        let provider = scripted_provider(ProviderKind::Codex);
        let link = MachineLink::new();
        let mut services = HostServices {
            machines: Some(link.clone()),
            provider_launcher: provider.launcher.clone(),
            ..Default::default()
        };
        services.start_mcp_servers(false);
        let host = spawn_host(store, services).unwrap();
        group.0.lock().unwrap().insert(
            id.to_owned(),
            Member {
                name: name.to_owned(),
                host: Some(host.clone()),
                accepts_threads: true,
            },
        );
        group.serve(id, &link);
        let index = Index::open(&host);
        Self {
            id: id.to_owned(),
            root: root.to_path_buf(),
            host,
            link,
            provider,
            index,
        }
    }

    /// The machine stops: its host shuts down and nothing reaches it.
    fn stop(self, group: &Group) -> PathBuf {
        group.0.lock().unwrap().get_mut(&self.id).unwrap().host = None;
        self.host.shutdown_blocking().unwrap();
        self.root
    }

    fn command(&self, command: Command) {
        self.host.link().command_blocking(command).unwrap();
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

/// The desk, whose thread `lead` works with the laptop, and the laptop, with a project and
/// a thread of its own; the lead's harness has run one turn and was given its tools.
struct Scene {
    group: Group,
    desk: Machine,
    laptop: Machine,
    tools: Tools,
    lead_options: SessionOptions,
    _stores: (TestStore, TestStore),
}

fn scene(tag: &str) -> Scene {
    let desk_store = TestStore::new(&format!("tcode-machines-{tag}-desk"));
    let laptop_store = TestStore::new(&format!("tcode-machines-{tag}-laptop"));
    let desk_root = desk_store.root().join("work");
    let mut project = Project::from_root(desk_root.clone());
    project.id = "desk-project".into();
    std::fs::create_dir_all(&desk_root).unwrap();
    desk_store.upsert_project(&project).unwrap();
    let mut lead = SessionMeta::new(ProviderKind::Codex, desk_root, None);
    lead.id = "lead".into();
    lead.title = "Lead".into();
    lead.project_id = Some(project.id);
    desk_store.upsert_meta(&lead).unwrap();
    let laptop_root = laptop_store.root().join("work");
    std::fs::create_dir_all(&laptop_root).unwrap();
    let mut project = Project::from_root(laptop_root.clone());
    project.id = LAPTOP_PROJECT.into();
    laptop_store.upsert_project(&project).unwrap();
    let mut own = SessionMeta::new(ProviderKind::Codex, laptop_root, None);
    own.id = "laptop-own".into();
    own.project_id = Some(project.id);
    laptop_store.upsert_meta(&own).unwrap();

    let group = Group::default();
    let desk = Machine::boot(&group, DESK, "Desk", desk_store.root());
    let laptop = Machine::boot(&group, LAPTOP, "Laptop", laptop_store.root());
    let mut desk = desk;
    desk.index.show("lead");
    desk.command(Command::SendTurn {
        session_id: "lead".into(),
        text: "Work with the laptop".into(),
        attachment_paths: Vec::new(),
    });
    let lead_options = desk.provider.launches.recv_blocking().unwrap();
    begin_turn(&desk.provider, "lead-1");
    play(&desk.provider, [completed("lead-1")]);
    let tools = Tools::connect(&lead_options);
    Scene {
        group,
        desk,
        laptop,
        tools,
        lead_options,
        _stores: (desk_store, laptop_store),
    }
}

fn start_subagent(tools: &Tools, request_id: &str) -> Result<serde_json::Value, String> {
    tools.call(
        "start_thread_on_machine",
        serde_json::json!({
            "machine": LAPTOP,
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

/// A thread started on another machine is an ordinary root thread there that names its
/// starter, once per request; a machine whose threads may not reach the group is not
/// offered the tool and is refused, and a machine that takes no threads leaves nothing on
/// the caller's side.
#[test]
fn a_thread_started_on_another_machine_is_a_root_thread_there_once_per_request() {
    let mut scene = scene("start");
    assert!(
        scene
            .tools
            .listed()
            .contains(&"start_thread_on_machine".to_owned())
    );

    let started = start_subagent(&scene.tools, "build-1").unwrap();
    let child = started["thread_id"].as_str().unwrap().to_owned();
    assert_eq!(started["created"], true);
    assert_eq!(started["machine"]["id"], LAPTOP);
    assert_eq!(
        started["link"],
        format!("tcode-thread://{child}@{LAPTOP}").as_str()
    );
    let there = scene.laptop.index.thread(&child, |_| true);
    assert_eq!(there.parent_session_id, None);
    let parent = there.remote_parent.clone().unwrap();
    assert_eq!(
        (parent.machine.as_str(), parent.thread_id.as_str()),
        (DESK, "lead")
    );
    let by = there.started_by.clone().unwrap();
    assert_eq!(
        (
            by.machine.as_deref(),
            by.thread_id.as_str(),
            by.request_id.as_str()
        ),
        (Some(DESK), "lead", "build-1")
    );

    let again = start_subagent(&scene.tools, "build-1").unwrap();
    assert_eq!(again["thread_id"], child.as_str());
    assert_eq!(again["created"], false);
    let listed = scene
        .tools
        .call(
            "list_threads",
            serde_json::json!({ "machine": LAPTOP, "scope": "host" }),
        )
        .unwrap();
    let started_there = listed["threads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|thread| thread["started_by"]["request_id"] == "build-1")
        .count();
    assert_eq!(started_there, 1);
    let children = lead_lineage(&scene.tools)["remote_children"].clone();
    assert_eq!(children.as_array().unwrap().len(), 1);
    assert_eq!(children[0]["thread_id"], child.as_str());
    assert_eq!(children[0]["state"], "running");

    scene.group.set_accepts_threads(LAPTOP, false);
    assert!(start_subagent(&scene.tools, "build-2").is_err());
    let children = lead_lineage(&scene.tools)["remote_children"].clone();
    assert_eq!(children.as_array().unwrap().len(), 1);
    scene.group.set_accepts_threads(LAPTOP, true);

    scene.desk.command(Command::PatchSettings {
        patch: tcode_core::settings::SettingsPatch::ControlMachines(false),
    });
    let reconnected = Tools::connect(&scene.lead_options);
    let listed = reconnected.listed();
    assert!(!listed.contains(&"start_thread_on_machine".to_owned()));
    assert!(!listed.contains(&"list_machines".to_owned()));
    assert!(
        reconnected
            .call("list_projects", serde_json::json!({ "machine": LAPTOP }))
            .unwrap_err()
            .starts_with("refused")
    );
}

/// A report the subagent made while its lead's machine was stopped reaches the lead once
/// that machine runs again, and its machine keeps it until the lead's harness took it.
#[test]
fn a_report_made_while_the_lead_s_machine_is_stopped_reaches_the_lead_when_it_returns() {
    let Scene {
        group,
        desk,
        mut laptop,
        tools,
        _stores,
        ..
    } = scene("report");
    let started = start_subagent(&tools, "build-1").unwrap();
    let child = started["thread_id"].as_str().unwrap().to_owned();
    let brief = begin_turn(&laptop.provider, "child-1");
    assert!(brief.starts_with("Build it here and report what fails."));
    drop(tools);
    let desk_root = desk.stop(&group);

    play(
        &laptop.provider,
        [
            reported("report-1", "Two tests fail on the laptop."),
            completed("child-1"),
        ],
    );
    laptop.index.thread(&child, |meta| {
        meta.expectation
            .as_ref()
            .is_some_and(|expectation| expectation.undelivered.is_some())
    });

    let mut desk = Machine::boot(&group, DESK, "Desk", &desk_root);
    let taken = begin_turn(&desk.provider, "lead-2");
    assert!(taken.contains("Two tests fail on the laptop."), "{taken}");
    assert!(
        taken.contains(&format!("thread {child}@{LAPTOP} (")),
        "{taken}"
    );
    laptop.index.thread(&child, |meta| {
        meta.expectation
            .as_ref()
            .is_some_and(|expectation| expectation.undelivered.is_none())
    });
    let lead = desk.index.thread("lead", |meta| {
        meta.remote_children
            .iter()
            .any(|kept| kept.report_taken.is_some() && !kept.report_taken_unsent)
    });
    assert_eq!(lead.remote_children[0].state, RemoteChildState::Reported);

    // The next report reaches the lead's machine while the lead's turn runs, and that
    // machine stops before the lead's harness took it.
    let tools = Tools::connect(&desk.provider.launches.recv_blocking().unwrap());
    tools
        .call(
            "send_message",
            serde_json::json!({
                "machine": LAPTOP,
                "thread_ids": [child],
                "text": "Now fix them.",
                "mode": "queue",
            }),
        )
        .unwrap();
    assert!(begin_turn(&laptop.provider, "child-2").starts_with("Now fix them."));
    play(
        &laptop.provider,
        [
            reported("report-2", "Both are fixed."),
            completed("child-2"),
        ],
    );
    loop {
        if let SessionCommand::Steer { text, .. } = next_command(&desk.provider)
            && text.contains("Both are fixed.")
        {
            break;
        }
    }
    drop(tools);
    let desk_root = desk.stop(&group);
    let desk = Machine::boot(&group, DESK, "Desk", &desk_root);
    assert!(begin_turn(&desk.provider, "lead-3").contains("Both are fixed."));
    laptop.index.thread(&child, |meta| {
        meta.expectation
            .as_ref()
            .is_some_and(|expectation| expectation.undelivered.is_none())
    });
}

/// A thread on another machine may not ask for what is not a machine's to ask, nor answer
/// the approvals of a thread it did not start.
#[test]
fn a_machine_is_refused_what_lies_outside_what_machines_may_ask() {
    let scene = scene("refused");
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
        let answer = smol::block_on(scene.desk.link.call(
            LAPTOP,
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
    let child = start_subagent(&scene.tools, "build-1").unwrap()["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();
    begin_turn(&scene.laptop.provider, "child-1");
    let on_laptop = |tool: &str, mut args: serde_json::Value| {
        args["machine"] = LAPTOP.into();
        scene.tools.call(tool, args)
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
/// when the machine runs again.
#[test]
fn a_cancel_issued_while_the_machine_is_unreachable_reaches_it_when_it_returns() {
    let Scene {
        group,
        mut desk,
        laptop,
        tools,
        _stores,
        ..
    } = scene("cancel");
    let started = start_subagent(&tools, "build-1").unwrap();
    let child = started["thread_id"].as_str().unwrap().to_owned();
    begin_turn(&laptop.provider, "child-1");
    let laptop_root = laptop.stop(&group);

    let cancelled = tools
        .call(
            "control_thread",
            serde_json::json!({
                "machine": LAPTOP,
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
    assert_eq!(
        lead_lineage(&tools)["remote_children"][0]["pending"],
        "cancel"
    );

    let mut laptop = Machine::boot(&group, LAPTOP, "Laptop", &laptop_root);
    laptop
        .index
        .thread(&child, |meta| meta.cancelled_at.is_some());
    desk.index.thread("lead", |meta| {
        meta.remote_children
            .iter()
            .any(|kept| kept.state == RemoteChildState::Cancelled && kept.pending.is_none())
    });
}

/// A client stops or cancels a lead's subagent on another machine through the lead's
/// host: sent at once while that machine runs, kept and sent when it returns while it does
/// not, and refused for a thread the lead did not start there.
#[test]
fn a_client_reaches_a_subagent_on_another_machine_through_its_lead_s_host() {
    let Scene {
        group,
        mut desk,
        laptop,
        tools,
        _stores,
        ..
    } = scene("client-op");
    let child = start_subagent(&tools, "build-1").unwrap()["thread_id"]
        .as_str()
        .unwrap()
        .to_owned();
    begin_turn(&laptop.provider, "child-1");
    let op = |thread: &str, op| Command::ControlRemoteChild {
        session_id: "lead".into(),
        machine: LAPTOP.into(),
        thread_id: thread.into(),
        op,
    };
    let host = desk.host.link();
    assert_eq!(
        host.command_blocking(op("laptop-own", RemoteChildOp::Cancel))
            .map_err(|error| error.code),
        Err("not_a_remote_child".to_owned())
    );

    host.command_blocking(op(&child, RemoteChildOp::Interrupt))
        .unwrap();
    while !matches!(next_command(&laptop.provider), SessionCommand::Interrupt) {}

    let laptop_root = laptop.stop(&group);
    host.command_blocking(op(&child, RemoteChildOp::Cancel))
        .unwrap();
    desk.index.thread("lead", |meta| {
        meta.remote_children
            .first()
            .is_some_and(|kept| kept.pending == Some(RemoteChildOp::Cancel))
    });
    let mut laptop = Machine::boot(&group, LAPTOP, "Laptop", &laptop_root);
    laptop
        .index
        .thread(&child, |meta| meta.cancelled_at.is_some());
    desk.index.thread("lead", |meta| {
        meta.remote_children
            .first()
            .is_some_and(|kept| kept.state == RemoteChildState::Cancelled && kept.pending.is_none())
    });
}

/// A thread another machine's thread starts runs under no more permissive a mode than the
/// thread that started it: the lead asks before it runs anything, and so does its subagent.
#[test]
fn a_thread_started_from_another_machine_runs_no_more_permissively_than_its_starter() {
    let mut scene = scene("permission");
    let lead = scene.desk.index.thread("lead", |_| true);
    let lead_class = agent::session_approval_class(ProviderKind::Codex, &lead.option_selections);
    assert_eq!(lead_class, ApprovalClass::Manual);
    let started = start_subagent(&scene.tools, "build-1").unwrap();
    let child = started["thread_id"].as_str().unwrap();
    let there = scene.laptop.index.thread(child, |_| true);
    assert!(
        agent::session_approval_class(ProviderKind::Codex, &there.option_selections) <= lead_class
    );
}

/// A machine of a real group over loopback, its host given `tcode_control` and a played
/// harness, and what its client shows of its index.
fn real_machine(
    dir: &super::real_group::TestDir,
    name: &str,
) -> (super::real_group::Machine, ScriptedProvider, Index) {
    let provider = scripted_provider(ProviderKind::Codex);
    let mut services = HostServices {
        provider_launcher: provider.launcher.clone(),
        ..Default::default()
    };
    services.start_mcp_servers(false);
    let machine = super::real_group::Machine::start_with(&dir.0, name, services);
    let index = Index::of(machine.client.clone());
    (machine, provider, index)
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

/// Over the group's real transport, a thread starts its subagent on another machine, whose
/// allow-list refuses a machine what is not a machine's to ask and which takes no thread
/// while Accept threads is off; the subagent's report reaches the lead, and the lead taking
/// it is what that machine hears.
#[test]
fn a_subagent_on_another_machine_reports_to_its_lead_over_the_group_s_transport() {
    use tcode_protocol::GroupAction;
    let desk_dir = super::real_group::TestDir::new("threads-desk");
    let laptop_dir = super::real_group::TestDir::new("threads-laptop");
    seed_real(&desk_dir, "desk-project", Some("lead"));
    seed_real(&laptop_dir, LAPTOP_PROJECT, None);
    let (desk, desk_harness, mut desk_index) = real_machine(&desk_dir, "Desk");
    let (laptop, laptop_harness, mut laptop_index) = real_machine(&laptop_dir, "Laptop");
    desk.group(GroupAction::Create {
        name: "Home".into(),
    });
    laptop.group(GroupAction::Join {
        link: desk.endpoint.as_ref().unwrap().new_invitation().url(),
        address: Some("127.0.0.1".into()),
    });
    let laptop_id = laptop.id();

    desk_index.show("lead");
    desk.command(Command::SendTurn {
        session_id: "lead".into(),
        text: "Work with the laptop".into(),
        attachment_paths: Vec::new(),
    })
    .unwrap();
    let tools = Tools::connect(&desk_harness.launches.recv_blocking().unwrap());
    begin_turn(&desk_harness, "lead-1");
    play(&desk_harness, [completed("lead-1")]);

    let refused = smol::block_on(desk.link.call(
        &laptop_id,
        MachineRequest::Control {
            caller: MachineCaller {
                thread_id: "lead".into(),
                title: "Lead".into(),
                permission_mode: None,
                request_id: None,
            },
            tool: "delete_threads".into(),
            args: serde_json::json!({ "thread_ids": ["anything"] }),
        },
    ));
    assert_eq!(
        refused.map_err(|error| error.code),
        Err("refused".to_owned())
    );

    let start = |request_id: &str| {
        tools.call(
            "start_thread_on_machine",
            serde_json::json!({
                "machine": laptop_id,
                "project_id": LAPTOP_PROJECT,
                "provider": "codex",
                "subagent": true,
                "title": "Check the laptop build",
                "message": "Build it here and report what fails.",
                "request_id": request_id,
            }),
        )
    };
    let started = start("build-1").unwrap();
    let child = started["thread_id"].as_str().unwrap().to_owned();
    let there = laptop_index.thread(&child, |_| true);
    assert_eq!(there.parent_session_id, None);
    assert_eq!(there.remote_parent.unwrap().machine, desk.id());
    assert_eq!(there.started_by.unwrap().machine, Some(desk.id()));

    begin_turn(&laptop_harness, "child-1");
    play(
        &laptop_harness,
        [
            reported("report-1", "Two tests fail on the laptop."),
            completed("child-1"),
        ],
    );
    let taken = begin_turn(&desk_harness, "lead-2");
    assert!(taken.contains("Two tests fail on the laptop."), "{taken}");
    assert!(
        taken.contains(&format!("thread {child}@{laptop_id} (")),
        "{taken}"
    );
    laptop_index.thread(&child, |meta| {
        meta.expectation
            .as_ref()
            .is_some_and(|expectation| expectation.undelivered.is_none())
    });
    play(&desk_harness, [completed("lead-2")]);

    laptop.group(GroupAction::SetAcceptThreads(false));
    let refusal = start("build-2").unwrap_err();
    assert!(refusal.contains(MACHINE_REFUSES), "{refusal}");
    let children = lead_lineage(&tools)["remote_children"].clone();
    assert_eq!(children.as_array().unwrap().len(), 1);
    assert_eq!(children[0]["thread_id"], child.as_str());
}
