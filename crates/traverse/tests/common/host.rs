//! The machine's own host, as the desktop and headless binaries compose it,
//! on a store of its own under `root`.
use std::{path::Path, sync::Arc};

use tcode_runtime::pipe::{HostServices, SpawnedHost, spawn_host};
use tcode_services::store::SessionStore;
use tcode_traverse::HostMux;

pub fn spawn(
    root: &Path,
    services: HostServices,
    seed: impl FnOnce(&SessionStore),
) -> (SpawnedHost, HostMux) {
    let store = SessionStore::open_at(root.join("store")).unwrap();
    seed(&store);
    let runtime = spawn_host(store, services).unwrap();
    let mux = HostMux::new(runtime.attachments.clone(), runtime.from_host.clone())
        .with_original_backend(Arc::new(runtime.clone()));
    (runtime, mux)
}

pub fn stop(runtime: SpawnedHost, mux: &HostMux) {
    let attachment = mux.attach(
        tcode_protocol::Principal::Full,
        tcode_client::attachment::AttachmentPolicy::LOCAL,
    );
    let link = tcode_client::HostLink::new(attachment.to_host, attachment.from_host);
    tcode_traverse::block_on(async {
        let pump = tcode_traverse::runtime().spawn({
            let link = link.clone();
            async move { link.pump().await }
        });
        link.shutdown().await.unwrap();
        runtime.to_host.close();
        runtime.stopped.recv().await.unwrap();
        link.close();
        pump.await.unwrap();
    });
}
