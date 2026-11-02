use crate::app::test_support::*;
use std::path::PathBuf;
use tcode_core::project::Project;
use tcode_protocol::{
    ClientPayload, Command, CommandResponse, EventEnvelope, HostMessage, ProtocolError,
    ServerEvent, Subscription, Topic,
};

/// Sends one command, runs the host until it answers, and returns the answer
/// with everything the host sent on the way.
fn acked(
    state: &TestEntity,
    cx: &mut TestAppContext,
    id: u64,
    command: Command,
) -> (Result<CommandResponse, ProtocolError>, Vec<HostMessage>) {
    state.dispatch_command(cx, id, command);
    cx.run_until_sent(|message| match message {
        HostMessage::Ack { id: acked, result } if *acked == id => Some(result.clone()),
        _ => None,
    })
}

#[test]
fn set_project_icon_saves_replaces_and_resets_the_managed_override() {
    let cx = &mut TestAppContext::default();
    let store = TestStore::new("tcode-project-icon");
    let project = Project::from_root(store.root().join("project"));
    std::fs::create_dir_all(&project.root).unwrap();
    store.upsert_project(&project).unwrap();
    let state = cx.new_entity(TestClientState::new((*store).clone()));
    state.deliver(
        cx,
        100,
        ClientPayload::Subscribe(Subscription {
            known_index: None,
            topic: Topic::Index,
            after: None,
        }),
    );
    cx.run_until_parked();
    cx.drain_outgoing();
    // The picker sends the host's own thumbnail of the chosen image.
    let source = project.root.join("logo.png");
    std::fs::write(
        &source,
        include_bytes!("../../../../assets/icons/app/tcode.png"),
    )
    .unwrap();
    let png = tcode_services::project_icons::thumbnail(&source).unwrap();
    let icons = store.root().join("project-icons");
    let stored = || store.read_file().unwrap().projects[0].icon_path.clone();
    let upserts = |sent: &[HostMessage]| -> Vec<Option<PathBuf>> {
        sent.iter()
            .filter_map(|message| match message {
                HostMessage::Event(EventEnvelope {
                    topic: Topic::Index,
                    event: ServerEvent::IndexUpsertProject(upserted),
                    ..
                }) if upserted.id == project.id => Some(upserted.icon_path.clone()),
                _ => None,
            })
            .collect()
    };
    let set = |png: Option<Vec<u8>>| Command::SetProjectIcon {
        project_id: project.id.clone(),
        png,
    };

    // A reset that changes nothing still tells clients to reload the default.
    let (result, sent) = acked(&state, cx, 1, set(None));
    assert_eq!(result, Ok(CommandResponse::Unit));
    assert_eq!(upserts(&sent), [None]);
    assert_eq!(stored(), None);

    let mut previous = None::<PathBuf>;
    for id in [2, 3] {
        let (result, sent) = acked(&state, cx, id, set(Some(png.clone())));
        assert_eq!(result, Ok(CommandResponse::Unit));
        let saved = match upserts(&sent).as_slice() {
            [Some(saved)] => saved.clone(),
            other => panic!("one upsert carrying the override, got {other:?}"),
        };
        assert_eq!(saved.parent(), Some(icons.as_path()));
        assert_eq!(std::fs::read(&saved).unwrap(), png);
        assert_eq!(stored().as_ref(), Some(&saved));
        if let Some(previous) = previous.replace(saved.clone()) {
            assert_ne!(previous, saved);
            assert!(!previous.exists(), "a replaced managed copy is deleted");
        }
    }

    let (result, sent) = acked(&state, cx, 4, set(None));
    assert_eq!(result, Ok(CommandResponse::Unit));
    assert_eq!(upserts(&sent), [None]);
    assert_eq!(stored(), None);
    assert!(
        !previous.unwrap().exists(),
        "a reset deletes the managed copy"
    );
    assert!(source.exists(), "the chosen source is never touched");

    for (id, command) in [
        (
            5,
            Command::SetProjectIcon {
                project_id: "missing".into(),
                png: Some(png.clone()),
            },
        ),
        (6, set(Some(b"broken".to_vec()))),
    ] {
        let (result, sent) = acked(&state, cx, id, command);
        assert!(result.is_err(), "{id}: {result:?}");
        assert!(upserts(&sent).is_empty());
    }
    assert_eq!(stored(), None);
    assert_eq!(std::fs::read_dir(&icons).unwrap().count(), 0);
}
