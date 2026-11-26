//! Machines of a group over real loopback endpoints, each a real host on a store of its own
//! as the binaries compose it, with a client of its own.
use std::path::{Path, PathBuf};

use tcode_client::HostLink;
use tcode_client::machines::MachineLink;
use tcode_protocol::{
    Command, CommandResponse, EventEnvelope, GroupAction, HostingAction, ProtocolError, Query,
    QueryResponse, Subscription, Topic,
};
use tcode_services::store::SessionStore;
use tcode_traverse::{HostConfig, HostMux, TraverseHost};

use crate::pipe::{HostServices, SpawnedHost, spawn_host};

pub(super) struct TestDir(pub(super) PathBuf);

impl TestDir {
    pub(super) fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tcode-machines-{tag}-{}", uuid::Uuid::new_v4()));
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
/// the endpoint, and a client of its own.
pub(super) struct Machine {
    pub(super) endpoint: Option<TraverseHost>,
    pub(super) runtime: Option<SpawnedHost>,
    pub(super) mux: HostMux,
    pub(super) dir: PathBuf,
    pub(super) name: String,
    pub(super) client: HostLink,
    pub(super) events: async_channel::Receiver<EventEnvelope>,
    pub(super) link: MachineLink,
    pub(super) store: SessionStore,
}

impl Machine {
    pub(super) fn start(dir: &Path, name: &str) -> Self {
        Self::start_with(dir, name, HostServices::default())
    }

    /// The machine with `services` beside its way to the group.
    pub(super) fn start_with(dir: &Path, name: &str, mut services: HostServices) -> Self {
        let link = MachineLink::new();
        let store = SessionStore::open_at(dir.join("store")).unwrap();
        services.machines = Some(link.clone());
        let runtime = spawn_host(store.clone(), services).unwrap();
        let mux = HostMux::new(runtime.attachments.clone(), runtime.from_host.clone())
            .with_machine_link(link.clone());
        let endpoint = TraverseHost::start(mux.clone(), config(dir, name)).unwrap();
        let connection = mux.attach(
            tcode_protocol::Principal::Full,
            tcode_client::attachment::AttachmentPolicy::LOCAL,
        );
        let client = HostLink::new(connection.to_host, connection.from_host);
        let pump = client.clone();
        smol::spawn(async move { pump.pump().await }).detach();
        let events = client.events();
        Self {
            endpoint: Some(endpoint),
            runtime: Some(runtime),
            mux,
            dir: dir.to_path_buf(),
            name: name.into(),
            client,
            events,
            link,
            store,
        }
    }

    pub(super) fn id(&self) -> String {
        self.endpoint.as_ref().unwrap().endpoint_id()
    }

    pub(super) fn group(&self, action: GroupAction) {
        self.endpoint
            .as_ref()
            .unwrap()
            .hosting(HostingAction::Group(action))
            .unwrap();
    }

    pub(super) fn command(&self, command: Command) -> Result<CommandResponse, ProtocolError> {
        self.client.command_blocking(command)
    }

    pub(super) fn query(&self, query: Query) -> Result<QueryResponse, ProtocolError> {
        smol::block_on(self.client.query(query))
    }

    pub(super) fn subscribe(&self, topic: Topic) {
        self.client
            .subscribe(Subscription {
                known_index: None,
                topic,
                after: None,
            })
            .unwrap();
    }

    pub(super) fn stop_hosting(&mut self) {
        if let Some(endpoint) = self.endpoint.take() {
            endpoint.shutdown();
        }
    }

    pub(super) fn start_hosting(&mut self) {
        self.endpoint =
            Some(TraverseHost::start(self.mux.clone(), config(&self.dir, &self.name)).unwrap());
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.stop_hosting();
        if let Some(runtime) = self.runtime.take() {
            smol::block_on(async {
                let _ = self.client.shutdown().await;
                runtime.to_host.close();
                let _ = runtime.stopped.recv().await;
                self.client.close();
            });
        }
    }
}

pub(super) fn config(dir: &Path, name: &str) -> HostConfig {
    HostConfig {
        host_name: name.into(),
        data_dir: dir.to_path_buf(),
        traverse: Vec::new(),
        pairing_enabled: true,
        bind_port: None,
    }
}
