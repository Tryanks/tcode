//! The machines of a group asking each other over `tcode/1`: the streams
//! another member opens here, which reach the host as
//! [`Query::Machine`] from a [`Principal::Machine`], and the requests this
//! machine's host sends out through its [`MachineLink`].
//!
//! [`MachineLink`]: tcode_client::machines::MachineLink
use std::sync::atomic::AtomicU64;

use tcode_client::machines::{
    MACHINE_REFUSES, MACHINE_UNREACHABLE, MachineCall, MachineInfo, NO_GROUP, NOT_MEMBER,
    PROTOCOL_MISMATCH,
};
use tcode_protocol::{
    ClientMessage, ClientPayload, GroupDeviceAccess, HostMessage, MachineRequest,
    MachineRequestKind, MachineResponse, Query, QueryResponse,
};

use super::*;

/// How long a probe of the group's machines waits for each one's answer: the
/// listing is waited on, and a machine that does not answer within it is
/// listed as unreachable instead of holding back the others.
const PROBE_BUDGET: Duration = Duration::from_secs(5);

fn error(code: &str, message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: code.into(),
        message: message.into(),
    }
}

fn host_gone() -> ProtocolError {
    error(
        MACHINE_UNREACHABLE,
        "the machine's host stopped before it answered",
    )
}

type Waiting =
    Arc<Mutex<HashMap<u64, async_channel::Sender<Result<MachineResponse, ProtocolError>>>>>;

/// The host as one member machine's connection reaches it: one mux
/// attachment under that machine's principal, its streams' requests told
/// apart by id.
pub(super) struct MachineBridge {
    to_host: async_channel::Sender<String>,
    waiting: Waiting,
    next: AtomicU64,
    reader: Option<tokio::task::AbortHandle>,
}

impl MachineBridge {
    pub(super) fn attach(shared: &Shared, machine_id: String, machine_name: String) -> Self {
        let attachment = shared.mux.attach(
            Principal::Machine {
                machine_id,
                machine_name,
            },
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let waiting: Waiting = Arc::default();
        let routes = waiting.clone();
        let from_host = attachment.from_host;
        let reader = shared.tasks.spawn(async move {
            while let Ok(line) = from_host.recv().await {
                let Ok(HostMessage::QueryResult { id, result }) =
                    tcode_protocol::decode_host_line(&line)
                else {
                    continue;
                };
                let Some(waiter) = routes.lock().unwrap().remove(&id) else {
                    continue;
                };
                let _ = waiter.try_send(match result {
                    Ok(QueryResponse::Machine(response)) => Ok(response),
                    Ok(other) => Err(error(
                        "unexpected_reply",
                        format!("the host answered a machine request with {other:?}"),
                    )),
                    Err(error) => Err(error),
                });
            }
            // Whatever still waits hears that the host is gone.
            routes.lock().unwrap().clear();
        });
        Self {
            to_host: attachment.to_host,
            waiting,
            next: AtomicU64::new(1),
            reader,
        }
    }

    async fn ask(&self, request: MachineRequest) -> Result<MachineResponse, ProtocolError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = async_channel::bounded(1);
        self.waiting.lock().unwrap().insert(id, reply);
        let line = tcode_protocol::encode_line(&ClientMessage {
            id,
            key: None,
            payload: ClientPayload::Query(Query::Machine(request)),
            // The mux gives every line the attachment's principal.
            principal: None,
        })?;
        if self.to_host.send(line).await.is_err() {
            self.waiting.lock().unwrap().remove(&id);
            return Err(host_gone());
        }
        answer.recv().await.unwrap_or_else(|_| Err(host_gone()))
    }
}

impl Drop for MachineBridge {
    fn drop(&mut self) {
        self.to_host.close();
        if let Some(reader) = &self.reader {
            reader.abort();
        }
    }
}

impl StreamTask {
    /// Answer one request of a member machine's stream: the protocol must be
    /// this machine's, and this machine's switches must take its kind.
    pub(super) async fn serve_machine(
        &self,
        bridge: &MachineBridge,
        protocol_version: u32,
        mut send: SendStream,
        mut reader: LineReader,
    ) -> io::Result<()> {
        let line = wire::read_line(&mut reader, wire::MAX_LINE)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no machine request"))?;
        let result = if protocol_version != tcode_protocol::PROTOCOL_VERSION {
            Err(error(
                PROTOCOL_MISMATCH,
                format!(
                    "this machine speaks protocol {}, the caller {protocol_version}",
                    tcode_protocol::PROTOCOL_VERSION
                ),
            ))
        } else {
            match serde_json::from_str::<MachineRequest>(&line) {
                Err(decode) => Err(ProtocolError::decode(decode.to_string())),
                Ok(request) => {
                    let remote = self.connection.remote_id().to_string();
                    match self.shared.admit_machine_request(&remote, request.kind()) {
                        Ok(()) => bridge.ask(request).await,
                        Err(refused) => Err(refused),
                    }
                }
            }
        };
        wire::write_line(&mut send, &HostLine::MachineReply { result }).await?;
        send.finish().map_err(io::Error::other)?;
        Ok(())
    }
}

impl Shared {
    /// The name of `remote` while it is another member machine of this
    /// machine's group.
    pub(super) fn member_machine(&self, remote: &str) -> Option<String> {
        if remote == self.endpoint.id().to_string() {
            return None;
        }
        let state = self.state.lock().unwrap();
        let group = state.identity.group.as_ref()?;
        group
            .state
            .machine(remote)
            .filter(|machine| machine.entry.is_member())
            .map(|machine| machine.name.clone())
    }

    /// Whether this machine takes a request of `kind` from `remote` now. What
    /// answers something this machine sent out always comes back.
    fn admit_machine_request(
        &self,
        remote: &str,
        kind: MachineRequestKind,
    ) -> Result<(), ProtocolError> {
        if self.member_machine(remote).is_none() {
            return Err(error(
                NOT_MEMBER,
                "the calling machine is not a member of this machine's group",
            ));
        }
        let state = self.state.lock().unwrap();
        let identity = &state.identity;
        let host = &identity.host_name;
        let accepts = identity.accepts();
        let (taken, what) = match kind {
            MachineRequestKind::Ping | MachineRequestKind::Reply => return Ok(()),
            MachineRequestKind::Thread => (accepts.threads, "Accept threads"),
            MachineRequestKind::Project => (accepts.projects, "Accept projects"),
        };
        if taken {
            return Ok(());
        }
        Err(error(
            MACHINE_REFUSES,
            match identity.group_device_access() {
                GroupDeviceAccess::Operate => format!("{what} is off on «{host}»"),
                GroupDeviceAccess::View => {
                    format!("«{host}» lets the group's other devices only view it")
                }
                GroupDeviceAccess::None => {
                    format!("«{host}» is not open to the group's other devices")
                }
            },
        ))
    }

    /// Send `request` to `machine_id`, another member of this machine's
    /// group, on a connection made with this machine's own key and kept for
    /// the next request while it lives. A machine that is not a member is
    /// refused here without being dialled. An error is the transport's.
    pub(super) async fn machine_request(
        &self,
        machine_id: &str,
        request: MachineRequest,
    ) -> io::Result<Result<MachineResponse, ProtocolError>> {
        let peer = {
            let state = self.state.lock().unwrap();
            let Some(group) = &state.identity.group else {
                return Ok(Err(error(NO_GROUP, "this machine is not in a group")));
            };
            match group
                .state
                .machine(machine_id)
                .filter(|machine| machine.entry.is_member())
            {
                Some(machine) if machine.id != self.endpoint.id().to_string() => machine.clone(),
                Some(_) => return Ok(Err(error(NOT_MEMBER, "that is this machine"))),
                None => {
                    return Ok(Err(error(
                        NOT_MEMBER,
                        "that machine is not a member of this machine's group",
                    )));
                }
            }
        };
        let connection = self.machine_connection(&peer).await?;
        let (mut send, recv) = connection.open_bi().await.map_err(io::Error::other)?;
        wire::write_line(
            &mut send,
            &ClientLine::Machine {
                protocol_version: tcode_protocol::PROTOCOL_VERSION,
            },
        )
        .await?;
        wire::write_line(&mut send, &request).await?;
        send.finish().map_err(io::Error::other)?;
        let mut reader = wire::reader(recv);
        let line = wire::read_line(&mut reader, wire::MAX_LINE)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no machine reply"))?;
        let reply: HostLine = serde_json::from_str(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(match reply {
            HostLine::MachineReply { result } => result,
            HostLine::HelloRejected { .. } | HostLine::Refused { .. } => Err(error(
                NOT_MEMBER,
                "that machine does not count this one among its group",
            )),
            other => Err(error(
                "unexpected_reply",
                format!("that machine answered {other:?}"),
            )),
        })
    }

    /// The live connection to `peer`, or a new one dialled where its group
    /// entry and the last connections say it is.
    async fn machine_connection(&self, peer: &GroupMachine) -> io::Result<Connection> {
        let kept = self.outbound.lock().unwrap().get(&peer.id).cloned();
        if let Some(connection) = kept.filter(|connection| connection.close_reason().is_none()) {
            return Ok(connection);
        }
        let addr = crate::client::dial_addr(&peer.id, peer.relay.as_deref(), &peer.addrs)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid machine id"))?;
        let connection = tokio::time::timeout(
            GROUP_DIAL_BUDGET,
            self.endpoint.connect(addr, wire::ALPN_MAIN),
        )
        .await
        .map_err(|_| timed_out("dialling the machine"))?
        .map_err(|error| io::Error::other(format!("{error:#}")))?;
        let hints = connection_hints(&connection);
        {
            let mut state = self.state.lock().unwrap();
            let hinted =
                state.identity.group.as_mut().is_some_and(|group| {
                    group.state.learn_hints(&peer.id, hints.relay, hints.addrs)
                });
            if hinted && let Err(error) = state.identity.save() {
                log::warn!("could not record where {} was reached: {error}", peer.id);
            }
        }
        self.outbound
            .lock()
            .unwrap()
            .insert(peer.id.clone(), connection.clone());
        Ok(connection)
    }

    async fn answer_call(self: Arc<Self>, call: MachineCall) {
        match call {
            MachineCall::Request {
                machine,
                request,
                reply,
            } => {
                let result = self
                    .machine_request(&machine, *request)
                    .await
                    .unwrap_or_else(|failure| Err(error(MACHINE_UNREACHABLE, failure.to_string())));
                let _ = reply.try_send(result);
            }
            MachineCall::Machines { probe, reply } => {
                let _ = reply.try_send(self.machines(probe).await);
            }
        }
    }

    /// The group's member machines; with `probe`, each other one pinged at
    /// once.
    async fn machines(self: &Arc<Self>, probe: bool) -> Result<Vec<MachineInfo>, ProtocolError> {
        let this = self.endpoint.id().to_string();
        let mut machines: Vec<MachineInfo> = {
            let state = self.state.lock().unwrap();
            let group = state
                .identity
                .group
                .as_ref()
                .ok_or_else(|| error(NO_GROUP, "this machine is not in a group"))?;
            group
                .state
                .member_machines()
                .map(|machine| MachineInfo {
                    id: machine.id.clone(),
                    name: machine.name.clone(),
                    label: machine.label.as_ref().and_then(|label| label.text.clone()),
                    this_machine: machine.id == this,
                    accepts_threads: machine.accepts.map(|accepts| accepts.threads),
                    accepts_projects: machine.accepts.map(|accepts| accepts.projects),
                    reachable: None,
                    protocol_ok: None,
                    relayed: None,
                })
                .collect()
        };
        if !probe {
            return Ok(machines);
        }
        let mut probes = tokio::task::JoinSet::new();
        for machine in machines.iter().filter(|machine| !machine.this_machine) {
            let (shared, id) = (self.clone(), machine.id.clone());
            probes.spawn(async move {
                let answer = tokio::time::timeout(
                    PROBE_BUDGET,
                    shared.machine_request(&id, MachineRequest::Ping),
                )
                .await;
                (id, answer)
            });
        }
        while let Some(Ok((id, answer))) = probes.join_next().await {
            let Some(machine) = machines.iter_mut().find(|machine| machine.id == id) else {
                continue;
            };
            let (reachable, protocol_ok) = match answer {
                Ok(Ok(Ok(MachineResponse::Pong { protocol_version }))) => (
                    true,
                    Some(protocol_version == tcode_protocol::PROTOCOL_VERSION),
                ),
                Ok(Ok(Err(refused))) if refused.code == PROTOCOL_MISMATCH => (true, Some(false)),
                Ok(Ok(_)) => (true, None),
                Ok(Err(_)) | Err(_) => (false, None),
            };
            machine.reachable = Some(reachable);
            machine.protocol_ok = protocol_ok;
            machine.relayed = reachable
                .then(|| {
                    self.outbound
                        .lock()
                        .unwrap()
                        .get(&id)
                        .map(|c| !path_info(c).direct)
                })
                .flatten();
        }
        Ok(machines)
    }
}

/// Answer the host's calls to the other machines for as long as this machine
/// runs; each call runs on its own.
pub(super) async fn serve_link(
    shared: std::sync::Weak<Shared>,
    calls: async_channel::Receiver<MachineCall>,
) {
    while let Ok(call) = calls.recv().await {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let tasks = shared.tasks.clone();
        tasks.spawn(shared.answer_call(call));
    }
}
