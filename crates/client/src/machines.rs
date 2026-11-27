//! How a machine's host reaches the other machines of its group. The host
//! asks through a [`MachineLink`]; the transport that keeps the group serves
//! the link for as long as it runs, so a call made while this machine hosts
//! nothing, or is in no group, fails at once instead of waiting.
use std::sync::{Arc, Mutex};

use tcode_protocol::{MachineRequest, MachineResponse, ProtocolError};

/// One machine of this machine's group, this one included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineInfo {
    pub id: String,
    pub name: String,
    /// What the group calls it, when anyone gave it a label.
    pub label: Option<String>,
    pub this_machine: bool,
    /// What the machine says it takes; `None` while it has not said.
    pub accepts_threads: Option<bool>,
    pub accepts_projects: Option<bool>,
    /// Filled by a probe: whether it answered, whether its protocol is this
    /// machine's, and whether a relay carried the answer.
    pub reachable: Option<bool>,
    pub protocol_ok: Option<bool>,
    pub relayed: Option<bool>,
}

/// What the transport serving a [`MachineLink`] is asked.
pub enum MachineCall {
    Request {
        machine: String,
        request: Box<MachineRequest>,
        reply: async_channel::Sender<Result<MachineResponse, ProtocolError>>,
    },
    Machines {
        probe: bool,
        reply: async_channel::Sender<Result<Vec<MachineInfo>, ProtocolError>>,
    },
}

/// The host's way to the other machines of its group. Clones share whoever
/// serves it.
#[derive(Clone, Default)]
pub struct MachineLink {
    serving: Arc<Mutex<Option<async_channel::Sender<MachineCall>>>>,
    served: Arc<Mutex<Vec<async_channel::Sender<()>>>>,
}

impl MachineLink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve this link's calls from the returned receiver from now on, in
    /// place of whatever served them. Once it is dropped, calls fail with
    /// [`NO_GROUP`] until the next one serves.
    pub fn serve(&self) -> async_channel::Receiver<MachineCall> {
        let (sender, receiver) = async_channel::unbounded();
        *self.serving.lock().unwrap() = Some(sender);
        self.served
            .lock()
            .unwrap()
            .retain(|served| served.try_send(()).is_ok() || !served.is_closed());
        receiver
    }

    /// Hears once each time something starts serving this link, and at once
    /// when something serves it now: when what this machine could not send
    /// its group may go.
    pub fn serves(&self) -> async_channel::Receiver<()> {
        let (sender, receiver) = async_channel::unbounded();
        let mut served = self.served.lock().unwrap();
        if self
            .serving
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|serving| !serving.is_closed())
        {
            let _ = sender.try_send(());
        }
        served.push(sender);
        receiver
    }

    /// Send `request` to `machine` of the group and wait for its answer. A
    /// machine that cannot be reached answers [`MACHINE_UNREACHABLE`].
    pub fn call(
        &self,
        machine: &str,
        request: MachineRequest,
    ) -> impl Future<Output = Result<MachineResponse, ProtocolError>> + Send + 'static {
        let (reply, answer) = async_channel::bounded(1);
        let sent = self.send(MachineCall::Request {
            machine: machine.to_owned(),
            request: Box::new(request),
            reply,
        });
        async move {
            sent?;
            answer.recv().await.unwrap_or_else(|_| Err(stopped()))
        }
    }

    /// The machines of the group; with `probe`, each other one is pinged
    /// first and its answer filled in.
    pub fn machines(
        &self,
        probe: bool,
    ) -> impl Future<Output = Result<Vec<MachineInfo>, ProtocolError>> + Send + 'static {
        let (reply, answer) = async_channel::bounded(1);
        let sent = self.send(MachineCall::Machines { probe, reply });
        async move {
            sent?;
            answer.recv().await.unwrap_or_else(|_| Err(stopped()))
        }
    }

    fn send(&self, call: MachineCall) -> Result<(), ProtocolError> {
        let serving = self.serving.lock().unwrap().clone();
        match serving {
            Some(serving) => serving.try_send(call).map_err(|_| stopped()),
            None => Err(stopped()),
        }
    }
}

/// This machine is in no group, or hosts nothing that reaches one.
pub const NO_GROUP: &str = "no_group";
/// The other machine could not be reached, or the connection to it failed.
pub const MACHINE_UNREACHABLE: &str = "machine_unreachable";
/// The other machine is not a member of this machine's group.
pub const NOT_MEMBER: &str = "not_member";
/// The other machine runs another protocol version.
pub const PROTOCOL_MISMATCH: &str = "protocol_mismatch";
/// The other machine's switches do not take this kind of request.
pub const MACHINE_REFUSES: &str = "machine_refuses";

fn stopped() -> ProtocolError {
    ProtocolError {
        code: NO_GROUP.into(),
        message: "this machine is not hosting a machine group".into(),
    }
}
