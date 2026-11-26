//! The machines of a group over real loopback endpoints, each with its own
//! host as the desktop and headless binaries compose it: what one machine
//! asks another, who may ask, what the switches let through, and what the
//! group's devices may do while a machine lets them only view it.
#[path = "common/host.rs"]
mod real_host;

use std::path::PathBuf;

use serde_json::Value;
use tcode_client::ConnectionState;
use tcode_client::host::Transport;
use tcode_client::machines::{MACHINE_REFUSES, MachineLink, NOT_MEMBER};
use tcode_client::pairing::{PairInvite, PairedHost, parse_pair_url};
use tcode_protocol::{
    GroupAction, GroupDeviceAccess, HostingAction, HostingState, MachineCaller, MachineRequest,
    MachineResponse, PROTOCOL_VERSION, ProtocolError,
};
use tcode_traverse::wire::{self, ClientLine, DeviceClaim, HelloRejection, HostLine};
use tcode_traverse::{DeviceIdentity, HostConfig, HostMux, TraverseHost};

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tcode-machines-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A machine as the binaries compose it: its host, the mux in front of it,
/// and the link between the two that the machine serves.
struct Machine {
    host: Option<TraverseHost>,
    runtime: Option<tcode_runtime::pipe::SpawnedHost>,
    mux: HostMux,
    link: MachineLink,
}

impl Machine {
    fn start(dir: &TestDir, name: &str) -> Self {
        let link = MachineLink::new();
        let (runtime, mux) = real_host::spawn(
            &dir.0,
            tcode_runtime::pipe::HostServices {
                machines: Some(link.clone()),
                ..Default::default()
            },
            |_| {},
        );
        let mux = mux.with_machine_link(link.clone());
        let host = TraverseHost::start(
            mux.clone(),
            HostConfig {
                host_name: name.into(),
                data_dir: dir.0.clone(),
                traverse: Vec::new(),
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .unwrap();
        Self {
            host: Some(host),
            runtime: Some(runtime),
            mux,
            link,
        }
    }

    fn host(&self) -> &TraverseHost {
        self.host.as_ref().unwrap()
    }

    fn id(&self) -> String {
        self.host().endpoint_id()
    }

    /// Where a dial on this loopback reaches the machine.
    fn addr(&self) -> iroh::EndpointAddr {
        iroh::EndpointAddr::from_parts(
            self.id().parse().unwrap(),
            [iroh::TransportAddr::Ip(std::net::SocketAddr::from((
                std::net::Ipv4Addr::LOCALHOST,
                self.host().addr().port,
            )))],
        )
    }

    fn group(&self, action: GroupAction) -> HostingState {
        self.host()
            .hosting(HostingAction::Group(action))
            .unwrap_or_else(|error| panic!("{error:?}"))
    }

    fn state(&self) -> HostingState {
        self.host().hosting(HostingAction::State).unwrap()
    }

    /// What `machine` answers this machine's host.
    fn call(
        &self,
        machine: &str,
        request: MachineRequest,
    ) -> Result<MachineResponse, ProtocolError> {
        tcode_traverse::block_on(self.link.call(machine, request))
    }

    /// Stop the endpoint, leaving the host and its files.
    fn stop_hosting(&mut self) {
        if let Some(host) = self.host.take() {
            host.shutdown();
        }
    }

    /// Waits until `check` holds, which this machine says through its
    /// hosting state as it changes.
    fn eventually(&self, what: &str, check: impl Fn(&HostingState) -> bool) {
        let changes = self.host().hosting_events();
        while !check(&self.state()) {
            changes
                .recv_blocking()
                .unwrap_or_else(|_| panic!("never saw {what}"));
        }
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.stop_hosting();
        if let Some(runtime) = self.runtime.take() {
            real_host::stop(runtime, &self.mux);
        }
    }
}

/// `b` joins the group `a` starts, as Settings → Remote does after a typed
/// address.
fn join(a: &Machine, b: &Machine) {
    a.group(GroupAction::Create {
        name: "Home".into(),
    });
    b.group(GroupAction::Join {
        link: a.host().new_invitation().url(),
        address: Some("127.0.0.1".into()),
    });
}

fn code(result: Result<MachineResponse, ProtocolError>) -> String {
    match result {
        Ok(response) => panic!("answered {response:?}"),
        Err(error) => error.code,
    }
}

/// An endpoint that dials as `key` and speaks the wire itself.
fn raw(key: iroh::SecretKey) -> iroh::Endpoint {
    tcode_traverse::block_on(
        iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .secret_key(key)
            .transport_config(wire::transport_config())
            .bind(),
    )
    .unwrap()
}

/// The key a machine keeps in its `traverse.json`.
fn machine_key(dir: &TestDir) -> iroh::SecretKey {
    let file: Value =
        serde_json::from_slice(&std::fs::read(dir.0.join("traverse.json")).unwrap()).unwrap();
    let hex = file["secret_key"].as_str().unwrap();
    let mut bytes = [0_u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap();
    }
    iroh::SecretKey::from_bytes(&bytes)
}

/// Open a stream with `first` and read the machine's first answer.
async fn first_answer(connection: &iroh::endpoint::Connection, first: &ClientLine) -> HostLine {
    let (mut send, recv) = connection.open_bi().await.unwrap();
    wire::write_line(&mut send, first).await.unwrap();
    let mut reader = wire::reader(recv);
    wire::read_control_within::<HostLine>(&mut reader, wire::MAX_CONTROL_LINE)
        .await
        .unwrap()
}

/// One request on a machine stream, as another member sends it.
async fn machine_stream(
    connection: &iroh::endpoint::Connection,
    request: &MachineRequest,
) -> HostLine {
    let (mut send, recv) = connection.open_bi().await.unwrap();
    wire::write_line(
        &mut send,
        &ClientLine::Machine {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await
    .unwrap();
    wire::write_line(&mut send, request).await.unwrap();
    send.finish().unwrap();
    let mut reader = wire::reader(recv);
    wire::read_control_within::<HostLine>(&mut reader, wire::MAX_LINE)
        .await
        .unwrap()
}

fn pong() -> HostLine {
    HostLine::MachineReply {
        result: Ok(MachineResponse::Pong {
            protocol_version: PROTOCOL_VERSION,
        }),
    }
}

#[test]
fn only_the_group_s_machines_open_machine_streams_and_they_open_nothing_else() {
    let a_dir = TestDir::new("admit-a");
    let b_dir = TestDir::new("admit-b");
    let a = Machine::start(&a_dir, "Desk");
    let mut b = Machine::start(&b_dir, "Laptop");
    join(&a, &b);

    assert_eq!(
        b.call(&a.id(), MachineRequest::Ping),
        Ok(MachineResponse::Pong {
            protocol_version: PROTOCOL_VERSION
        })
    );
    let outsider = iroh::SecretKey::generate().public().to_string();
    assert_eq!(code(b.call(&outsider, MachineRequest::Ping)), NOT_MEMBER);

    // A device paired with the machine.
    let device = raw(iroh::SecretKey::generate());
    let invite: PairInvite = parse_pair_url(&a.host().new_invitation().url()).unwrap();
    tcode_traverse::block_on(async {
        let pairing = device.connect(a.addr(), wire::ALPN_PAIR).await.unwrap();
        let paired = first_answer(
            &pairing,
            &ClientLine::Pair {
                secret: invite.secret.clone(),
                device: DeviceClaim {
                    name: "Phone".into(),
                    platform: None,
                },
                space: None,
            },
        )
        .await;
        assert!(matches!(paired, HostLine::Paired { .. }), "{paired:?}");
        pairing.close(0_u32.into(), b"done");
        let connection = device.connect(a.addr(), wire::ALPN_MAIN).await.unwrap();
        assert!(
            matches!(
                machine_stream(&connection, &MachineRequest::Ping).await,
                HostLine::Refused { .. }
            ),
            "a device opens no machine stream"
        );
    });
    tcode_traverse::block_on(device.close());

    // Nobody the machine knows.
    let stranger = raw(iroh::SecretKey::generate());
    tcode_traverse::block_on(async {
        let connection = stranger.connect(a.addr(), wire::ALPN_MAIN).await.unwrap();
        assert_eq!(
            machine_stream(&connection, &MachineRequest::Ping).await,
            HostLine::HelloRejected {
                reason: HelloRejection::Unpaired
            }
        );
    });
    tcode_traverse::block_on(stranger.close());

    // The member machine itself, under its own key.
    let b_id = b.id();
    b.stop_hosting();
    let as_b = raw(machine_key(&b_dir));
    let connection =
        tcode_traverse::block_on(async { as_b.connect(a.addr(), wire::ALPN_MAIN).await.unwrap() });
    tcode_traverse::block_on(async {
        for first in [
            ClientLine::Hello {
                protocol_version: PROTOCOL_VERSION,
                device: DeviceClaim {
                    name: "Laptop".into(),
                    platform: None,
                },
            },
            ClientLine::Connect {
                host: "127.0.0.1".into(),
                port: 1,
            },
            ClientLine::Ssh,
            ClientLine::Original {
                target: tcode_protocol::OriginalTarget::File {
                    path: a_dir.0.join("traverse.json"),
                },
            },
        ] {
            let answer = first_answer(&connection, &first).await;
            assert!(
                matches!(answer, HostLine::Refused { .. }),
                "{first:?} was answered {answer:?}"
            );
        }
        assert_eq!(
            machine_stream(&connection, &MachineRequest::Ping).await,
            pong()
        );
    });

    // Removed from the group: refused on the connection it holds, and not
    // admitted again.
    a.group(GroupAction::RemoveMachine(b_id));
    tcode_traverse::block_on(async {
        let HostLine::MachineReply { result } =
            machine_stream(&connection, &MachineRequest::Ping).await
        else {
            panic!("no machine reply");
        };
        assert_eq!(code(result), NOT_MEMBER);
        connection.close(0_u32.into(), b"done");
        let again = as_b.connect(a.addr(), wire::ALPN_MAIN).await.unwrap();
        assert_eq!(
            machine_stream(&again, &MachineRequest::Ping).await,
            HostLine::HelloRejected {
                reason: HelloRejection::Unpaired
            }
        );
    });
    tcode_traverse::block_on(as_b.close());
}

/// A request that has passed the gate reaches the host, whose answer for
/// these is not built yet; one that has not is answered by the gate.
fn passes(result: Result<MachineResponse, ProtocolError>) -> bool {
    match code(result).as_str() {
        "not_implemented" => true,
        MACHINE_REFUSES => false,
        other => panic!("unexpected refusal {other}"),
    }
}

#[test]
fn a_machine_s_switches_gate_threads_and_projects_and_replies_always_come_back() {
    let a_dir = TestDir::new("gate-a");
    let b_dir = TestDir::new("gate-b");
    let a = Machine::start(&a_dir, "Desk");
    let b = Machine::start(&b_dir, "Laptop");
    join(&a, &b);
    let to_a = a.id();
    let thread = || MachineRequest::Control {
        caller: MachineCaller {
            thread_id: "lead".into(),
            title: "Lead".into(),
            permission_mode: None,
            request_id: None,
        },
        tool: "start_thread".into(),
        args: serde_json::json!({}),
    };
    let project = || MachineRequest::MoveStatus {
        move_id: "move".into(),
    };
    let reply = || MachineRequest::ReportTaken {
        child_thread_id: "child".into(),
    };

    assert!(passes(b.call(&to_a, thread())));
    assert!(
        !passes(b.call(&to_a, project())),
        "projects are off until switched on"
    );
    assert!(passes(b.call(&to_a, reply())));

    a.group(GroupAction::SetAcceptThreads(false));
    let switched = a.group(GroupAction::SetAcceptProjects(true));
    let info = switched.group.unwrap();
    assert_eq!((info.accept_threads, info.accept_projects), (false, true));
    assert!(!passes(b.call(&to_a, thread())));
    assert!(passes(b.call(&to_a, project())));
    assert!(passes(b.call(&to_a, reply())));
    b.eventually("the desk report what it takes", |state| {
        state.group.as_ref().is_some_and(|group| {
            group.machines.iter().any(|machine| {
                machine.id == to_a
                    && machine.accepts_threads == Some(false)
                    && machine.accepts_projects == Some(true)
            })
        })
    });

    a.group(GroupAction::SetAcceptThreads(true));
    for access in [GroupDeviceAccess::View, GroupDeviceAccess::None] {
        a.group(GroupAction::SetDeviceAccess(access));
        assert!(!passes(b.call(&to_a, thread())), "{access:?}");
        assert!(!passes(b.call(&to_a, project())), "{access:?}");
        assert!(passes(b.call(&to_a, reply())), "{access:?}");
    }

    // What one member calls a machine, every member calls it.
    let to_b = b.id();
    a.group(GroupAction::SetLabel {
        machine: to_b.clone(),
        label: Some("  Travel laptop ".into()),
    });
    b.eventually("the desk's label for the laptop", |state| {
        state.group.as_ref().is_some_and(|group| {
            group.machines.iter().any(|machine| {
                machine.id == to_b && machine.label.as_deref() == Some("Travel laptop")
            })
        })
    });
}

fn device(dir: &TestDir, name: &str) -> DeviceIdentity {
    let device = DeviceIdentity::load_or_create(&dir.0).unwrap();
    device.set_details(name.into(), None);
    device.set_lan_options(tcode_traverse::lan::LanOptions {
        browse: tcode_traverse::lan::Browse::Off,
        multicast_lock: None,
    });
    device
}

fn pair(machine: &Machine, device: &DeviceIdentity) -> PairedHost {
    let invite = parse_pair_url(&machine.host().new_invitation().url()).unwrap();
    tcode_traverse::pair_blocking(&invite, Some(std::net::Ipv4Addr::LOCALHOST.into()), device)
        .unwrap()
}

fn wait_state(transport: &Transport, wanted: impl Fn(&ConnectionState) -> bool) {
    let mut seen = Vec::new();
    while let Ok(state) = transport.state.recv_blocking() {
        if wanted(&state) {
            return;
        }
        seen.push(state);
    }
    panic!("did not observe the wanted state; saw {seen:?}");
}

fn up(state: &ConnectionState) -> bool {
    matches!(
        state,
        ConnectionState::Syncing { .. } | ConnectionState::Connected { .. }
    )
}

/// Send one client message and read the host's answer to it.
fn ask(transport: &Transport, id: u64, payload: Value) -> Value {
    let line = serde_json::json!({"id": id, "payload": payload});
    transport
        .to_host
        .send_blocking(format!("{line}\n"))
        .unwrap();
    while let Ok(line) = transport.from_host.recv_blocking() {
        let value: Value = serde_json::from_str(line.trim_end()).unwrap();
        let answered = value["content"]["id"].as_u64() == Some(id)
            || value["content"]["request_id"].as_u64() == Some(id);
        if answered {
            return value;
        }
    }
    panic!("no answer to {id}");
}

fn command_accepted(transport: &Transport, id: u64) -> bool {
    let ack = ask(
        transport,
        id,
        serde_json::json!({"type": "command", "content": {"type": "cycle_project_sort"}}),
    );
    assert_eq!(ack["type"], "ack");
    ack["content"]["result"].get("Ok").is_some()
}

/// The device connected while the machine let the group's devices operate
/// it is closed when they may only view it, and comes back reading what it
/// read before and changing nothing; once the machine admits none of them,
/// its hello is refused for that reason.
#[test]
fn a_device_connected_before_view_only_comes_back_unable_to_change_anything() {
    let dir = TestDir::new("view");
    let desk = Machine::start(&dir, "Desk");
    let phone_dir = TestDir::new("view-phone");
    let phone = device(&phone_dir, "Phone");
    let saved = pair(&desk, &phone);
    desk.group(GroupAction::Create {
        name: "Home".into(),
    });
    let transport = tcode_traverse::connect(&saved, &phone);
    wait_state(&transport, up);
    assert!(command_accepted(&transport, 1));

    desk.group(GroupAction::SetDeviceAccess(GroupDeviceAccess::View));
    wait_state(&transport, |state| {
        matches!(state, ConnectionState::Reconnecting { .. })
    });
    wait_state(&transport, up);
    assert!(!command_accepted(&transport, 2));
    let scope = ask(
        &transport,
        3,
        serde_json::json!({"type": "subscribe", "content": {"topic": {"type": "scope"}, "after": null}}),
    );
    assert_eq!(scope["content"]["event"]["type"], "scope_snapshot");
    assert_eq!(scope["content"]["event"]["content"]["view_only"], true);
    let index = ask(
        &transport,
        4,
        serde_json::json!({"type": "subscribe", "content": {"topic": {"type": "index"}, "after": null}}),
    );
    assert_eq!(index["content"]["event"]["type"], "index_snapshot");
    let ping = ask(
        &transport,
        5,
        serde_json::json!({"type": "query", "content": {"type": "ping"}}),
    );
    assert_eq!(ping["content"]["result"]["Ok"]["type"], "pong");

    desk.group(GroupAction::SetDeviceAccess(GroupDeviceAccess::None));
    wait_state(&transport, |state| {
        *state
            == ConnectionState::Offline {
                reason: tcode_client::ConnectionFailure::AccessClosed,
            }
    });
}
