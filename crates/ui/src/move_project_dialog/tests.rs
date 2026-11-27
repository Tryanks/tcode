use std::cell::RefCell;
use std::future::Future as _;

use gpui::{TestAppContext, VisualTestContext};
use tcode_protocol::{
    ClientPayload, Command, EventEnvelope, GroupDeviceAccess, GroupInfo, GroupMachineInfo,
    HostMessage, HostingState, MoveBlocker, ProjectMovePreview, Query, QueryResponse, ServerEvent,
    Topic,
};

use super::*;
use crate::store::WorkspaceAttachment;

struct Body;

impl Render for Body {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

/// The machine that holds the project, answered by hand.
struct Holder {
    link: tcode_client::HostLink,
    incoming: async_channel::Sender<String>,
    outgoing: async_channel::Receiver<String>,
    unanswered: RefCell<Vec<tcode_protocol::ClientMessage>>,
}

impl Holder {
    fn send(&self, message: HostMessage, cx: &mut VisualTestContext) {
        self.incoming
            .try_send(tcode_protocol::encode_line(&message).unwrap())
            .unwrap();
        let mut pump = std::pin::pin!(self.link.pump());
        let mut task = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(pump.as_mut().poll(&mut task).is_pending());
        cx.run_until_parked();
        crate::test_frames::draw_settled(cx);
    }

    fn take(&self, mut wanted: impl FnMut(&ClientPayload) -> bool) -> Vec<(u64, ClientPayload)> {
        let mut unanswered = self.unanswered.borrow_mut();
        unanswered.extend(
            std::iter::from_fn(|| self.outgoing.try_recv().ok())
                .map(|line| tcode_protocol::decode_client_line(&line).unwrap()),
        );
        let (taken, kept) = unanswered
            .drain(..)
            .partition::<Vec<_>, _>(|message| wanted(&message.payload));
        *unanswered = kept;
        taken
            .into_iter()
            .map(|message| (message.id, message.payload))
            .collect()
    }

    fn previews(&self) -> Vec<(u64, String)> {
        self.take(|payload| {
            matches!(
                payload,
                ClientPayload::Query(Query::ProjectMovePreview { .. })
            )
        })
        .into_iter()
        .map(|(id, payload)| match payload {
            ClientPayload::Query(Query::ProjectMovePreview {
                project_id,
                machine,
            }) => {
                assert_eq!(project_id, "app");
                (id, machine)
            }
            _ => unreachable!(),
        })
        .collect()
    }

    fn commands(&self) -> Vec<Command> {
        self.take(|payload| matches!(payload, ClientPayload::Command(_)))
            .into_iter()
            .map(|(_, payload)| match payload {
                ClientPayload::Command(command) => command,
                _ => unreachable!(),
            })
            .collect()
    }

    fn answer_preview(&self, id: u64, preview: ProjectMovePreview, cx: &mut VisualTestContext) {
        self.send(
            HostMessage::QueryResult {
                id,
                result: Ok(QueryResponse::ProjectMovePreview(preview)),
            },
            cx,
        );
    }
}

fn machine(id: &str, this_machine: bool, accepts_projects: Option<bool>) -> GroupMachineInfo {
    GroupMachineInfo {
        id: id.into(),
        name: id.into(),
        this_machine,
        label: None,
        accepts_threads: None,
        accepts_projects,
    }
}

fn preview(blockers: Vec<MoveBlocker>) -> ProjectMovePreview {
    ProjectMovePreview {
        default_root: "/home/me/app".into(),
        git: true,
        branches: 2,
        bytes: 4 << 20,
        dirty_checkouts: 0,
        threads: 3,
        archived_threads: 0,
        ignored_files: vec![".env".into()],
        blockers,
        relayed: false,
    }
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is not drawn"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    crate::test_frames::draw_settled(cx);
}

/// Move goes to the machine that holds the project with the place typed and
/// the ignored files as the box was left, to the machine chosen last.
#[gpui::test]
fn a_move_is_sent_to_the_holder_with_the_place_typed_and_the_box_as_left(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
    cx.update(|cx| {
        crate::test_client::init(cx);
        crate::shortcut::init(cx);
        cx.set_reduce_motion(true);
    });
    let (to_host, outgoing) = async_channel::unbounded();
    let (incoming, from_host) = async_channel::unbounded();
    let link = tcode_client::HostLink::new(to_host, from_host);
    let store = cx.new(|cx| {
        WorkspaceStore::new_attached(
            link.clone(),
            WorkspaceAttachment::Remote {
                host_id: "home".into(),
                host_name: "Home".into(),
            },
            None,
            None,
            cx,
        )
    });
    crate::store::tests::seed_full_scope(&store, &incoming, vec![], cx);
    let holder = Holder {
        link,
        incoming,
        outgoing,
        unanswered: RefCell::default(),
    };
    let (_, cx) = cx.add_window_view(|window, cx| {
        let body = cx.new(|_| Body);
        gpui_base::Root::new(body, window, cx)
    });
    holder.send(
        HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexUpsertProject(crate::store::tests::project_at(
                "app",
                std::path::Path::new("/home/me/app"),
            )),
        }),
        cx,
    );
    cx.update(|window, cx| open(store.clone(), "app".into(), window, cx));
    crate::test_frames::draw_settled(cx);
    holder.send(
        HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Hosting,
            event: ServerEvent::HostingReplaced(Box::new(HostingState {
                created_space_id: None,
                enabled: true,
                expires_in_secs: 0,
                host_id: "home".into(),
                host_name: "Home".into(),
                invite: None,
                devices: Vec::new(),
                spaces: Vec::new(),
                traverse: Vec::new(),
                group: Some(GroupInfo {
                    id: "group".into(),
                    name: "Desk".into(),
                    machines: vec![
                        machine("home", true, Some(true)),
                        machine("studio", false, Some(true)),
                        machine("lab", false, Some(true)),
                        machine("laptop", false, Some(false)),
                    ],
                    declined: Vec::new(),
                    accept_threads: true,
                    accept_projects: true,
                    device_access: GroupDeviceAccess::Operate,
                }),
            })),
        }),
        cx,
    );
    assert!(
        cx.debug_bounds("move-project-machine-home").is_none(),
        "the machine that holds the project is not a destination"
    );

    click(cx, "move-project-machine-studio");
    let [(_, machine)] = holder.previews().try_into().expect("one preview asked");
    assert_eq!(machine, "studio");
    click(cx, "move-project-machine-lab");
    let [(id, machine)] = holder.previews().try_into().expect("one preview asked");
    assert_eq!(machine, "lab");
    holder.answer_preview(id, preview(Vec::new()), cx);
    click(cx, "move-project-root");
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-a"
    } else {
        "ctrl-a"
    });
    cx.simulate_input("/srv/app");
    click(cx, "move-project-ignored");
    click(cx, "move-project-apply");
    assert_eq!(
        holder.commands(),
        [Command::MoveProject {
            project_id: "app".into(),
            machine: "lab".into(),
            root: "/srv/app".into(),
            include_ignored: false,
        }]
    );
}
