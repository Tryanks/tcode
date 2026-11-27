//! Projects moving between two machines of a group over real loopback
//! endpoints, each machine a real host on a store of its own as the binaries
//! compose it, driven by a client of each.
use std::path::{Path, PathBuf};

use agent::{AgentEvent, ProviderKind, TurnStatus};
use tcode_core::project::{Project, SessionMeta, WorktreeInfo};
use tcode_protocol::{
    Command, CommandResponse, GroupAction, MachineRequest, MachineResponse, MovePart, MovePause,
    MovePhase, ProjectMoveStatus, ProtocolError, Query, QueryResponse, ServerEvent, Subscription,
    Topic,
};
use tcode_services::store::SessionStore;

use super::real_group::{Machine, TestDir};

impl Machine {
    /// The next list of moving projects this machine publishes that `wanted`
    /// takes.
    fn moves_until(&self, wanted: impl Fn(&[ProjectMoveStatus]) -> bool) -> Vec<ProjectMoveStatus> {
        loop {
            let envelope = self
                .events
                .recv_blocking()
                .expect("the host stopped before the move did");
            if let ServerEvent::ProjectMovesReplaced(moves) = envelope.event
                && wanted(&moves)
            {
                return moves;
            }
        }
    }

    /// The projects and threads this machine lists, archived ones included.
    fn listed(&self) -> (Vec<Project>, Vec<SessionMeta>) {
        self.subscribe(Topic::Index);
        let index = loop {
            let envelope = self.events.recv_blocking().unwrap();
            if let ServerEvent::IndexSnapshot(index) = envelope.event {
                break index;
            }
        };
        self.client
            .unsubscribe(Subscription {
                known_index: None,
                topic: Topic::Index,
                after: None,
            })
            .unwrap();
        let Ok(QueryResponse::ArchivedSessions(archived)) = self.query(Query::ArchivedSessions)
        else {
            panic!("no archived threads");
        };
        let mut sessions = index.sessions;
        sessions.extend(archived.sessions);
        (index.projects, sessions)
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = tcode_services::process::command("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", "tcode")
        .env("GIT_AUTHOR_EMAIL", "tcode@localhost")
        .env("GIT_COMMITTER_NAME", "tcode")
        .env("GIT_COMMITTER_EMAIL", "tcode@localhost")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A project of the desk's: a repository with a change in it, a thread on
/// its root, one in a Tcode worktree and one archived, each with a turn.
struct Seeded {
    project: Project,
    threads: Vec<SessionMeta>,
    worktree: PathBuf,
}

/// Seed the desk's store before its host starts, with `bulk` bytes of
/// incompressible data committed.
fn seed(dir: &Path, bulk: usize) -> Seeded {
    let root = dir.join("code").join("app");
    std::fs::create_dir_all(&root).unwrap();
    git(
        &root,
        &["-c", "init.templateDir=", "init", "--quiet", "-b", "main"],
    );
    git(&root, &["config", "commit.gpgSign", "false"]);
    std::fs::write(root.join("readme.md"), "app\n").unwrap();
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let bytes: Vec<u8> = (0..bulk)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    std::fs::write(root.join("bulk.bin"), bytes).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "initial"]);
    std::fs::write(root.join("readme.md"), "app, changed\n").unwrap();
    let data = dir.join("store");
    let worktree = tcode_services::worktree::provision(&root, "feature", None, &data).unwrap();
    let project = Project {
        id: "desk-project".into(),
        name: "app".into(),
        root: root.clone(),
        quick_chat: false,
        icon_path: None,
        permission_defaults: Default::default(),
        created_at: 1,
    };
    let mut on_root = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
    on_root.title = "On the root".into();
    let mut in_worktree = SessionMeta::new(ProviderKind::Codex, worktree.path.clone(), None);
    in_worktree.title = "In a worktree".into();
    in_worktree.worktree = Some(WorktreeInfo {
        root_project_path: root.clone(),
        base: worktree.base.clone(),
        branch: worktree.branch.clone(),
    });
    let mut archived = SessionMeta::new(ProviderKind::ClaudeCode, root.clone(), None);
    archived.title = "Archived".into();
    archived.archived_at = Some(2);
    let mut threads = vec![on_root, in_worktree, archived];
    let attachments = tcode_services::user_files::attachment_dir(&data, &threads[0].id);
    std::fs::create_dir_all(&attachments).unwrap();
    std::fs::write(attachments.join("note.txt"), "an attachment\n").unwrap();
    let store = SessionStore::open_at(data).unwrap();
    store.upsert_project(&project).unwrap();
    for meta in &mut threads {
        meta.project_id = Some(project.id.clone());
        store.upsert_meta(meta).unwrap();
        for event in [
            AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: TurnStatus::Completed,
                usage: None,
            },
        ] {
            store.append_event(&meta.id, 1, &event).unwrap();
        }
    }
    store.close().unwrap();
    Seeded {
        project,
        threads,
        worktree: worktree.path,
    }
}

/// The desk with `seeded`'s project and a laptop that takes projects, in one
/// group.
fn desk_and_laptop(desk_dir: &TestDir, laptop_dir: &TestDir) -> (Machine, Machine) {
    let desk = Machine::start(&desk_dir.0, "Desk");
    let laptop = Machine::start(&laptop_dir.0, "Laptop");
    desk.group(GroupAction::Create {
        name: "Home".into(),
    });
    laptop.group(GroupAction::Join {
        link: desk.endpoint.as_ref().unwrap().new_invitation().url(),
        address: Some("127.0.0.1".into()),
    });
    laptop.group(GroupAction::SetAcceptProjects(true));
    desk.subscribe(Topic::ProjectMoves);
    desk.moves_until(|moves| moves.is_empty());
    (desk, laptop)
}

fn move_to(
    desk: &Machine,
    laptop: &Machine,
    root: &Path,
) -> Result<CommandResponse, ProtocolError> {
    desk.command(Command::MoveProject {
        project_id: "desk-project".into(),
        machine: laptop.id(),
        root: root.display().to_string(),
        include_ignored: false,
    })
}

fn code(result: Result<CommandResponse, ProtocolError>) -> String {
    result.expect_err("the command was accepted").code
}

fn rename(id: &str) -> Command {
    Command::RenameSession {
        session_id: id.into(),
        title: "Renamed".into(),
    }
}

/// The project moves with every thread: the laptop lists it at the root it
/// was given with each thread, its history and a `thread_moved` entry, the
/// worktree thread in a worktree of the laptop's own; the desk lists neither,
/// says where each thread went, and keeps neither the move's parts nor its
/// refs.
#[test]
fn a_project_moves_with_its_threads_and_the_desk_says_where_they_went() {
    let (desk_dir, laptop_dir) = (TestDir::new("desk"), TestDir::new("laptop"));
    let seeded = seed(&desk_dir.0, 1024);
    let (desk, laptop) = desk_and_laptop(&desk_dir, &laptop_dir);
    let history: Vec<_> = seeded
        .threads
        .iter()
        .map(|meta| desk.store.read_events(&meta.id).unwrap())
        .collect();
    let root = laptop_dir.0.join("code").join("app");

    move_to(&desk, &laptop, &root).unwrap();
    let begun = desk.moves_until(|moves| !moves.is_empty());
    desk.moves_until(|moves| moves.is_empty());
    assert!(
        !desk_dir
            .0
            .join("store")
            .join("moves")
            .join("out")
            .join(&begun[0].move_id)
            .exists(),
        "the desk keeps the move's parts"
    );
    assert_eq!(
        git(&seeded.project.root, &["for-each-ref", "refs/tcode/move"]),
        ""
    );

    let (projects, threads) = laptop.listed();
    let [project] = projects.as_slice() else {
        panic!("the laptop lists {projects:?}");
    };
    assert_eq!((project.name.as_str(), &project.root), ("app", &root));
    for (meta, before) in seeded.threads.iter().zip(&history) {
        let arrived = threads
            .iter()
            .find(|arrived| arrived.id == meta.id)
            .unwrap_or_else(|| panic!("{} did not arrive", meta.title));
        assert_eq!(arrived.project_id.as_deref(), Some(project.id.as_str()));
        assert_eq!(arrived.archived_at, meta.archived_at);
        let events = laptop.store.read_events(&meta.id).unwrap();
        let (moved, kept) = events.split_last().unwrap();
        assert_eq!(
            kept.iter().map(|record| &record.event).collect::<Vec<_>>(),
            before
                .iter()
                .map(|record| &record.event)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            moved.event,
            AgentEvent::ThreadMoved {
                from_machine_name: "Desk".into(),
            }
        );
    }
    let in_worktree = threads
        .iter()
        .find(|meta| meta.id == seeded.threads[1].id)
        .unwrap();
    assert!(
        in_worktree
            .cwd
            .starts_with(laptop_dir.0.join("store").join("worktrees"))
    );
    assert_eq!(
        std::fs::read_to_string(
            tcode_services::user_files::attachment_dir(
                &laptop_dir.0.join("store"),
                &seeded.threads[0].id
            )
            .join("note.txt")
        )
        .unwrap(),
        "an attachment\n"
    );

    let (projects, threads) = desk.listed();
    assert!(projects.is_empty(), "{projects:?}");
    assert!(threads.is_empty(), "{threads:?}");
    assert!(
        seeded.project.root.join("bulk.bin").exists(),
        "the desk keeps the directory"
    );
    let first = seeded.threads[0].id.clone();
    assert_eq!(
        desk.query(Query::MovedThread {
            thread_id: first.clone()
        }),
        Ok(QueryResponse::MovedThread(Some(laptop.id())))
    );
    assert_eq!(
        code(desk.command(rename(&first))),
        super::moves::THREAD_MOVED
    );
}

/// The laptop's folder for the first thread's attachments, which a move
/// cannot put the desk's into while it is there: the laptop takes every part
/// and then refuses the commit, which leaves the move paused with all of it
/// staged.
fn laptop_refuses_the_commit(laptop_dir: &TestDir, seeded: &Seeded) -> PathBuf {
    let taken = tcode_services::user_files::attachment_dir(
        &laptop_dir.0.join("store"),
        &seeded.threads[0].id,
    );
    std::fs::create_dir_all(&taken).unwrap();
    taken
}

/// The next list in which a move is paused for the laptop's refusal.
fn refused(desk: &Machine) -> ProjectMoveStatus {
    desk.moves_until(|moves| {
        moves
            .iter()
            .any(|status| matches!(status.paused, Some(MovePause::Failed { .. })))
    })
    .remove(0)
}

/// A move the laptop refused at the end and then cancelled leaves the desk's
/// project, threads, repository and worktree as they were and the laptop with
/// nothing staged; while it waited the project's threads took no command, and
/// after it they do.
#[test]
fn a_cancelled_move_leaves_both_machines_as_they_were() {
    let (desk_dir, laptop_dir) = (TestDir::new("desk"), TestDir::new("laptop"));
    let seeded = seed(&desk_dir.0, 1024);
    let (desk, laptop) = desk_and_laptop(&desk_dir, &laptop_dir);
    laptop_refuses_the_commit(&laptop_dir, &seeded);
    let root = &seeded.project.root;
    let status = git(root, &["status", "--porcelain=v2", "--branch"]);
    let refs = git(root, &["for-each-ref"]);
    let listed = desk.listed();

    move_to(&desk, &laptop, &laptop_dir.0.join("app")).unwrap();
    let waiting = refused(&desk);
    let staged = laptop_dir
        .0
        .join("store")
        .join("moves")
        .join("in")
        .join(&waiting.move_id);
    assert!(staged.exists(), "the laptop holds nothing of the move");
    let first = seeded.threads[0].id.clone();
    assert_eq!(
        code(desk.command(rename(&first))),
        super::moves::PROJECT_MOVING
    );
    desk.command(Command::CancelProjectMove {
        project_id: seeded.project.id.clone(),
        force: false,
    })
    .unwrap();

    assert_eq!(desk.listed(), listed);
    assert_eq!(git(root, &["status", "--porcelain=v2", "--branch"]), status);
    assert_eq!(git(root, &["for-each-ref"]), refs);
    assert!(seeded.worktree.join(".git").exists());
    assert!(
        !desk_dir
            .0
            .join("store")
            .join("moves")
            .join("out")
            .join(&waiting.move_id)
            .exists()
    );
    assert!(!staged.exists(), "the laptop keeps what it staged");
    assert!(laptop.listed().0.is_empty());
    desk.command(rename(&first)).unwrap();
}

/// A move tried again goes on from the bytes the laptop holds, not from the
/// start: the laptop here holds half of the code, as an interrupted send
/// leaves it, and the project arrives whole.
#[test]
fn a_move_tried_again_resumes_where_the_laptop_is() {
    let (desk_dir, laptop_dir) = (TestDir::new("desk"), TestDir::new("laptop"));
    let seeded = seed(&desk_dir.0, 1024);
    let (desk, laptop) = desk_and_laptop(&desk_dir, &laptop_dir);
    let taken = laptop_refuses_the_commit(&laptop_dir, &seeded);
    let root = laptop_dir.0.join("app");

    move_to(&desk, &laptop, &root).unwrap();
    let waiting = refused(&desk);
    assert_eq!(waiting.done, waiting.total, "{waiting:?}");
    let code = laptop_dir
        .0
        .join("store")
        .join("moves")
        .join("in")
        .join(&waiting.move_id)
        .join("code.bundle");
    let length = std::fs::metadata(&code).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&code)
        .unwrap()
        .set_len(length / 2)
        .unwrap();
    std::fs::remove_dir_all(taken).unwrap();

    desk.command(Command::ResumeProjectMove {
        project_id: seeded.project.id.clone(),
    })
    .unwrap();
    let resumed = desk.moves_until(|moves| {
        moves
            .iter()
            .any(|status| status.phase == MovePhase::SendingCode)
    });
    assert_eq!(resumed[0].paused, None);
    assert_eq!(
        resumed[0].done,
        waiting.total - (length - length / 2),
        "{resumed:?}"
    );
    desk.moves_until(|moves| moves.is_empty());
    let (projects, threads) = laptop.listed();
    assert_eq!(projects.len(), 1);
    assert_eq!(threads.len(), seeded.threads.len());
    assert_eq!(
        std::fs::read(root.join("bulk.bin")).unwrap(),
        std::fs::read(seeded.project.root.join("bulk.bin")).unwrap()
    );
}

/// A commit asked again after the project arrived answers the same project
/// and makes no second one; a move bringing a thread the laptop already has
/// is refused before anything is staged.
#[test]
fn a_repeated_commit_makes_nothing_twice_and_a_known_thread_is_refused() {
    let (desk_dir, laptop_dir) = (TestDir::new("desk"), TestDir::new("laptop"));
    let seeded = seed(&desk_dir.0, 1024);
    let (desk, laptop) = desk_and_laptop(&desk_dir, &laptop_dir);
    move_to(&desk, &laptop, &laptop_dir.0.join("app")).unwrap();
    let begun = desk.moves_until(|moves| !moves.is_empty());
    desk.moves_until(|moves| moves.is_empty());
    let (projects, threads) = laptop.listed();

    let again = smol::block_on(desk.link.call(
        &laptop.id(),
        MachineRequest::MoveCommit {
            move_id: begun[0].move_id.clone(),
        },
    ));
    assert_eq!(
        again,
        Ok(MachineResponse::MoveCommitted {
            project_id: projects[0].id.clone()
        })
    );
    assert_eq!(laptop.listed(), (projects, threads));

    let known = smol::block_on(desk.link.call(
        &laptop.id(),
        MachineRequest::MoveBegin {
            move_id: "another".into(),
            project_name: "app".into(),
            root: laptop_dir.0.join("elsewhere").display().to_string(),
            git: true,
            thread_ids: vec![seeded.threads[0].id.clone()],
            parts: vec![MovePart {
                name: "manifest.json".into(),
                size: 1,
                sha256: String::new(),
            }],
        },
    ));
    assert_eq!(
        known.map_err(|error| error.code),
        Err("thread_exists".into())
    );
    assert!(
        !laptop_dir
            .0
            .join("store")
            .join("moves")
            .join("in")
            .join("another")
            .exists()
    );
}
