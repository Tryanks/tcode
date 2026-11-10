use std::path::Path;

use base64::Engine as _;
use tcode_runtime::pipe::{HostServices, SpawnedHost};
use tcode_traverse::HostMux;

pub fn recorded_host(root: &Path, bytes: &[u8]) -> (SpawnedHost, HostMux, String, String) {
    recorded_host_with(root, bytes, HostServices::default())
}

pub fn recorded_host_with(
    root: &Path,
    bytes: &[u8],
    services: HostServices,
) -> (SpawnedHost, HostMux, String, String) {
    let project = tcode_core::project::Project::from_root(root.to_owned());
    let mut meta =
        tcode_core::project::SessionMeta::new(agent::ProviderKind::Codex, root.to_owned(), None);
    meta.project_id = Some(project.id.clone());
    let (runtime, mux) = crate::real_host::spawn(root, services, |store| {
        store.upsert_project(&project).unwrap();
        store.upsert_meta(&meta).unwrap();
        store
            .append_event(
                &meta.id,
                1,
                &agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: "recorded-image".into(),
                    parent_item_id: None,
                    content: agent::ItemContent::ImageRead {
                        path: "original.png".into(),
                        image: Some(agent::Attachment {
                            media_type: "image/png".into(),
                            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                            source_path: None,
                        }),
                    },
                }),
            )
            .unwrap();
    });
    (runtime, mux, meta.id, project.id)
}
