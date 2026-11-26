//! A project and its threads moving from this machine to another of the
//! group: the commands, the preview, the `ProjectMoves` topic, and the
//! driver that sends the parts and commits them.
//!
//! While a move runs this machine deletes nothing: the project is frozen
//! ([`AppState::move_refusal`]), every part is staged on the other machine,
//! and only once it has answered `MoveCommitted` are the threads and the
//! project removed here, each thread leaving a redirect to the machine it went
//! to. Every move is journalled before anything else happens, so a restart
//! resumes it from where its journal says it was.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tcode_client::machines::{MACHINE_REFUSES, MACHINE_UNREACHABLE, MachineLink, NO_GROUP};
use tcode_core::ui::ConversationDestination;
use tcode_protocol::{
    Command, MachineRequest, MachineResponse, MoveBlocker, MovePart, MovePhase, MoveState,
    ProjectMovePreview, ProjectMoveStatus,
};
use tcode_services::project_move as disk;
use tcode_services::store::Mutation;

use crate::app::*;

/// The project is moving to another machine and takes no work until the move
/// ends or is cancelled.
pub(crate) const PROJECT_MOVING: &str = "project_moving";
/// The thread moved to another machine of the group, which the message names.
pub(crate) const THREAD_MOVED: &str = "thread_moved";

fn refusal(code: &str, message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: code.into(),
        message: message.into(),
    }
}

fn host_stopped() -> ProtocolError {
    refusal("transport_closed", "Host stopped before the move was done.")
}

/// What this machine keeps of moving projects, both ways.
#[derive(Default)]
pub(crate) struct Moves {
    /// Moves leaving this machine, by the project that moves.
    outgoing: BTreeMap<String, Outgoing>,
    /// Cancelled moves whose other machine has yet to hear it, by move id.
    aborting: HashMap<String, Journal>,
    /// A wait to tell them again is running.
    aborts_waiting: bool,
    /// Threads that moved away, and the machine each went to.
    pub(super) moved_threads: HashMap<String, String>,
    /// Moves that arrived here, and the project each made.
    pub(super) arrived: HashMap<String, String>,
    /// Moves being committed here now.
    pub(super) committing: HashSet<String>,
    next_run: u64,
}

struct Outgoing {
    journal: Journal,
    phase: MovePhase,
    done: u64,
    total: u64,
    error: Option<String>,
    commit_unknown: bool,
    /// Which driver may act for the move: a cancel moves it on, so a driver
    /// still running stops at its next step.
    run: u64,
    driver: Option<HostTask<()>>,
    /// Waits since the move last got anywhere, for the next one's length.
    waits: u32,
}

/// What survives a restart of a move leaving this machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    move_id: String,
    project_id: String,
    machine: String,
    root: String,
    include_ignored: bool,
    project_name: String,
    project_root: PathBuf,
    thread_ids: Vec<String>,
    stage: Stage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Stage {
    /// Building or sending the parts.
    Sending,
    /// `MoveCommit` went out and its answer has not come back.
    CommitSent,
    /// The other machine made the project; what is left here goes.
    Committed { project_id: String },
    /// Cancelled; the other machine has yet to discard its staging.
    Aborting,
}

impl Journal {
    fn record(&self) -> Mutation {
        Mutation::put_move_record(
            &self.move_id,
            serde_json::to_vec(self).expect("a move journal serializes"),
        )
    }
}

/// How long a move whose other machine cannot be reached waits before it
/// tries again: longer after each wait, up to two minutes.
fn wait_after(waits: u32) -> Duration {
    Duration::from_secs(5u64.saturating_mul(1 << waits.min(5)).min(120))
}

/// Why a driver stopped short of the end.
enum Stop {
    /// A cancel took the move over.
    Superseded,
    /// The other machine could not be reached or does not take projects now:
    /// the move waits and tries again.
    Unreachable(String),
    /// The other machine refused or something failed here: the move waits
    /// for the user to try again or cancel.
    Failed(String),
}

fn stop_for(error: ProtocolError) -> Stop {
    match error.code.as_str() {
        MACHINE_UNREACHABLE | NO_GROUP | MACHINE_REFUSES => Stop::Unreachable(error.message),
        _ => Stop::Failed(format!("{}: {}", error.code, error.message)),
    }
}

impl AppState {
    /// Take up the moves the store journalled, and what it keeps of the moves
    /// that ended, at host start. Read here rather than off the host so the
    /// freeze holds before the first command.
    pub(crate) fn resume_project_moves(&mut self, cx: &mut HostCx) {
        match self.store.moved_threads() {
            Ok(moved) => self.moves.moved_threads = moved,
            Err(error) => log::warn!("could not read where moved threads went: {error}"),
        }
        match self.store.moves_in() {
            Ok(arrived) => self.moves.arrived = arrived,
            Err(error) => log::warn!("could not read the moves that arrived: {error}"),
        }
        let records = match self.store.project_moves() {
            Ok(records) => records,
            Err(error) => {
                log::error!("could not read the projects moving away: {error}");
                return;
            }
        };
        for body in records {
            let journal: Journal = match serde_json::from_slice(&body) {
                Ok(journal) => journal,
                Err(error) => {
                    log::error!("skipping a project move this build cannot read: {error}");
                    continue;
                }
            };
            if journal.stage == Stage::Aborting {
                self.moves.aborting.insert(journal.move_id.clone(), journal);
                continue;
            }
            let phase = match journal.stage {
                Stage::Sending => MovePhase::Preparing,
                _ => MovePhase::Committing,
            };
            let project_id = journal.project_id.clone();
            self.insert_outgoing(journal, phase);
            self.start_move_driver(&project_id, cx);
        }
        if !self.moves.aborting.is_empty() {
            self.retry_aborts(0, cx);
        }
    }

    fn insert_outgoing(&mut self, journal: Journal, phase: MovePhase) -> u64 {
        self.moves.next_run += 1;
        let run = self.moves.next_run;
        self.moves.outgoing.insert(
            journal.project_id.clone(),
            Outgoing {
                commit_unknown: journal.stage == Stage::CommitSent,
                journal,
                phase,
                done: 0,
                total: 0,
                error: None,
                run,
                driver: None,
                waits: 0,
            },
        );
        run
    }

    /// Whether `project_id` is moving away now.
    pub(crate) fn project_moving(&self, project_id: &str) -> bool {
        self.moves.outgoing.contains_key(project_id)
    }

    /// Whether the thread `id` belongs to a project moving away now.
    pub(crate) fn thread_moving(&self, id: &str) -> bool {
        self.moves.outgoing.values().any(|outgoing| {
            outgoing
                .journal
                .thread_ids
                .iter()
                .any(|moving| moving == id)
        }) || self
            .current_meta(id)
            .and_then(|meta| meta.project_id.as_deref())
            .is_some_and(|project| self.project_moving(project))
    }

    /// The machine the thread `id` moved to, when it moved away.
    pub(crate) fn moved_thread_machine(&self, id: &str) -> Option<&str> {
        self.moves.moved_threads.get(id).map(String::as_str)
    }

    /// Why `command` may not run while its project moves away, or names a
    /// thread that already did. Reading stays open; everything else that
    /// would change the project's threads or start work in it is refused.
    pub(crate) fn move_refusal(&self, command: &Command) -> Option<ProtocolError> {
        let project = match command {
            Command::MoveSession { project_id, .. }
            | Command::SetProjectIcon { project_id, .. }
            | Command::SetProjectRoot { project_id, .. }
            | Command::DeleteProject { project_id }
            | Command::StartDraft { project_id, .. }
            | Command::StartExternalImport { project_id, .. } => Some(project_id),
            _ => None,
        };
        if project.is_some_and(|project| self.project_moving(project)) {
            return Some(moving());
        }
        let id = command.session_id()?;
        if matches!(
            command,
            Command::MarkSessionRead { .. }
                | Command::MarkSessionUnread { .. }
                | Command::LoadBranches { .. }
                | Command::SetTerminalHeight { .. }
                | Command::ToggleTerminalPanel { .. }
                | Command::CloseTerminalPanel { .. }
        ) {
            return None;
        }
        if self.thread_moving(id) {
            return Some(moving());
        }
        if self.current_meta(id).is_none()
            && let Some(machine) = self.moved_thread_machine(id)
        {
            return Some(moved_away(id, machine));
        }
        None
    }

    /// Every thread that goes with `project_id`: its own, and the threads
    /// under them.
    fn moving_threads(&self, project_id: &str) -> Vec<SessionMeta> {
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        for meta in self.sessions.iter() {
            if meta.project_id.as_deref() != Some(project_id) {
                continue;
            }
            for id in descendant_session_ids(self.sessions.iter(), &meta.id) {
                if seen.insert(id.clone()) {
                    ids.push(id);
                }
            }
        }
        ids.iter().filter_map(|id| self.find_meta(id)).collect()
    }

    /// What stands in the way of moving `project` with `threads` on this
    /// machine.
    fn local_blockers(&self, project: &Project, threads: &[SessionMeta]) -> Vec<MoveBlocker> {
        let mut blockers = Vec::new();
        if project.quick_chat {
            blockers.push(MoveBlocker::QuickChat);
        }
        let (mut running, mut pending, mut queued, mut terminals) = (0, 0, 0, 0);
        let parked = |destination: ConversationDestination| {
            self.terminal_workspaces
                .get(&destination)
                .map_or(0, |workspace| workspace.terminals.len())
        };
        for meta in threads {
            terminals += parked(ConversationDestination::Thread(meta.id.clone()));
            pending += self.approval_requests(&meta.id).len();
            let Some(active) = self.resident(&meta.id) else {
                continue;
            };
            if active.turn_in_flight
                || active.delivery_in_flight.is_some()
                || active.background_task_count > 0
                || active.preparing_turn()
                || active.timeline.turn_running
            {
                running += 1;
            }
            if active.timeline.pending_user_input.is_some() {
                pending += 1;
            }
            if !active.queue.is_empty() {
                queued += 1;
            }
            terminals += active.terminal_workspace.terminals.len();
        }
        terminals += parked(ConversationDestination::ProjectDraft(project.id.clone()));
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        if running > 0 {
            blockers.push(MoveBlocker::ThreadsRunning(count(running)));
        }
        if pending > 0 {
            blockers.push(MoveBlocker::PendingApprovals(count(pending)));
        }
        if queued > 0 {
            blockers.push(MoveBlocker::QueuedMessages(count(queued)));
        }
        if terminals > 0 {
            blockers.push(MoveBlocker::TerminalsOpen(count(terminals)));
        }
        blockers
    }

    fn move_target(&self, project_id: &str) -> Result<(MachineLink, Project), ProtocolError> {
        let link = self.machine_link()?.clone();
        let project = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .cloned()
            .ok_or_else(|| refusal("unknown_project", "This project is no longer available."))?;
        Ok((link, project))
    }

    /// Start moving `project_id` to `machine`, at `root` there; or, for a
    /// move of it that waits, try again now. Answers once the project is
    /// frozen and its move journalled; the move goes on from there.
    pub(crate) fn move_project(
        &mut self,
        project_id: &str,
        machine: &str,
        root: &str,
        include_ignored: bool,
        cx: &mut HostCx,
    ) -> HostTask<Result<(), ProtocolError>> {
        if let Some(outgoing) = self.moves.outgoing.get(project_id) {
            let result =
                if outgoing.journal.machine == machine && outgoing.phase != MovePhase::Cancelling {
                    if outgoing.phase == MovePhase::Paused && outgoing.driver.is_none() {
                        self.start_move_driver(project_id, cx);
                    }
                    Ok(())
                } else {
                    Err(refusal(
                        "move_in_progress",
                        "This project is already moving to another machine.",
                    ))
                };
            return cx.spawn_background(async move { result });
        }
        let prepared = self.move_target(project_id).and_then(|(link, project)| {
            let threads = self.moving_threads(project_id);
            blocked(self.local_blockers(&project, &threads))?;
            Ok((link, project, threads))
        });
        let host = cx.clone();
        let (machine, root, project_id) =
            (machine.to_owned(), root.to_owned(), project_id.to_owned());
        cx.spawn_background(async move {
            let (link, project, threads) = prepared?;
            let checkouts = {
                let (root, threads) = (project.root.clone(), threads.clone());
                host.unblock(move || interrupted_checkouts(&root, &threads))
                    .await
            };
            blocked(checkouts)?;
            let thread_ids: Vec<String> = threads.iter().map(|meta| meta.id.clone()).collect();
            match link
                .call(
                    &machine,
                    MachineRequest::MovePreflight {
                        project_name: project.name.clone(),
                        root: Some(root.clone()),
                        thread_ids: thread_ids.clone(),
                    },
                )
                .await?
            {
                MachineResponse::MovePreflight { .. } => {}
                other => return Err(unexpected(&other)),
            }
            host.enqueue_and_wait(move |app, cx| {
                if app.project_moving(&project.id) {
                    return Err(refusal(
                        "move_in_progress",
                        "This project is already moving to another machine.",
                    ));
                }
                let threads = app.moving_threads(&project.id);
                blocked(app.local_blockers(&project, &threads))?;
                let journal = Journal {
                    move_id: uuid::Uuid::new_v4().to_string(),
                    project_id: project.id.clone(),
                    machine,
                    root,
                    include_ignored,
                    project_name: project.name.clone(),
                    project_root: project.root.clone(),
                    thread_ids: threads.iter().map(|meta| meta.id.clone()).collect(),
                    stage: Stage::Sending,
                };
                app.stop_moving_providers(&journal.thread_ids, cx);
                app.insert_outgoing(journal, MovePhase::Preparing);
                app.start_move_driver(&project_id, cx);
                Ok(())
            })
            .await
            .map_err(|()| host_stopped())?
        })
    }

    /// Stop the providers of threads that are about to move, so nothing works
    /// in them while their history leaves.
    fn stop_moving_providers(&mut self, ids: &[String], cx: &mut HostCx) {
        for id in ids {
            if self.residents.live.contains_key(id) {
                self.shutdown_active(id, cx);
            } else if let Some(parked) = self.residents.evict(id) {
                parked.stop_provider();
            }
        }
    }

    /// Run the move of `project_id` from where its journal is.
    fn start_move_driver(&mut self, project_id: &str, cx: &mut HostCx) {
        let Ok(link) = self.machine_link().cloned() else {
            // Nothing reaches the group yet; the wait tries again.
            self.pause_move(project_id, None, Stop::Unreachable("no group".into()), cx);
            return;
        };
        let Some(outgoing) = self.moves.outgoing.get_mut(project_id) else {
            return;
        };
        outgoing.error = None;
        if outgoing.phase == MovePhase::Paused {
            outgoing.phase = match outgoing.journal.stage {
                Stage::Sending => MovePhase::Preparing,
                _ => MovePhase::Committing,
            };
        }
        let driver = Driver {
            host: cx.clone(),
            link,
            store: self.store.clone(),
            project_id: project_id.to_owned(),
            run: outgoing.run,
        };
        outgoing.driver = Some(cx.spawn_background(driver.drive()));
        self.publish_project_moves(cx);
    }

    /// The move's driver stopped for `stop`: it waits, trying again by itself
    /// when the other machine could not be reached.
    fn pause_move(&mut self, project_id: &str, run: Option<u64>, stop: Stop, cx: &mut HostCx) {
        let Some(outgoing) = self
            .moves
            .outgoing
            .get_mut(project_id)
            .filter(|outgoing| run.is_none_or(|run| run == outgoing.run))
        else {
            return;
        };
        outgoing.driver = None;
        let retry = match stop {
            Stop::Superseded => return,
            Stop::Unreachable(message) => {
                outgoing.error = Some(message);
                true
            }
            Stop::Failed(message) => {
                outgoing.error = Some(message);
                false
            }
        };
        outgoing.phase = MovePhase::Paused;
        if retry {
            let (wait, run) = (wait_after(outgoing.waits), outgoing.run);
            outgoing.waits += 1;
            let timer = cx.clock().timer(wait);
            let host = cx.clone();
            let project_id = project_id.to_owned();
            cx.spawn_detached(async move {
                timer.await;
                host.enqueue(move |app, cx| {
                    if app.moves.outgoing.get(&project_id).is_some_and(|outgoing| {
                        outgoing.run == run
                            && outgoing.phase == MovePhase::Paused
                            && outgoing.driver.is_none()
                    }) {
                        app.start_move_driver(&project_id, cx);
                    }
                });
            });
        }
        self.publish_project_moves(cx);
    }

    /// Stop the move of `project_id` before the other machine commits it:
    /// the other machine discards what it staged and the project here is as
    /// it was. Once the commit went out with no answer, only `force` ends it,
    /// leaving the other machine with whatever it did.
    pub(crate) fn cancel_project_move(
        &mut self,
        project_id: &str,
        force: bool,
        cx: &mut HostCx,
    ) -> HostTask<Result<(), ProtocolError>> {
        let refused = |error| cx.spawn_background(async move { Err(error) });
        let Some(outgoing) = self.moves.outgoing.get_mut(project_id) else {
            return refused(refusal("no_project_move", "This project is not moving."));
        };
        if outgoing.phase == MovePhase::Cancelling {
            return refused(refusal("move_cancelling", "This move is being cancelled."));
        }
        match outgoing.journal.stage {
            Stage::Committed { .. } => {
                return refused(refusal(
                    "move_committed",
                    "The other machine has the project now; this machine is letting it go.",
                ));
            }
            Stage::CommitSent if !force => {
                return refused(refusal(
                    "move_commit_unknown",
                    "The other machine was asked to take the project and has not said whether \
                     it did; the move can only be cancelled by accepting that it may have.",
                ));
            }
            _ => {}
        }
        self.moves.next_run += 1;
        outgoing.run = self.moves.next_run;
        outgoing.phase = MovePhase::Cancelling;
        outgoing.error = None;
        let driver = outgoing.driver.take();
        let mut journal = outgoing.journal.clone();
        journal.stage = Stage::Aborting;
        self.publish_project_moves(cx);
        let link = self.machine_link().ok().cloned();
        let store = self.store.clone();
        let host = cx.clone();
        let project_id = project_id.to_owned();
        cx.spawn_background(async move {
            // A request the driver has in flight lands before the abort, so
            // the abort is the last the other machine hears of the move.
            if let Some(driver) = driver {
                driver.await;
            }
            let recorded = {
                let (store, record) = (store.clone(), journal.record());
                host.unblock(move || store.apply(&[record])).await
            };
            let aborted = match &link {
                Some(link) => abort(link, &journal).await,
                None => false,
            };
            let data_dir = store.root().clone();
            let ended = {
                let journal = journal.clone();
                host.unblock(move || {
                    disk::end_outgoing(&data_dir, &journal.move_id, &journal.project_root)
                })
                .await
            };
            if let Err(error) = ended {
                log::warn!(
                    "could not clear what move {} left: {error}",
                    journal.move_id
                );
            }
            let keep_trying = !aborted && !force && recorded.is_ok();
            if !keep_trying {
                let (store, id) = (store.clone(), journal.move_id.clone());
                if let Err(error) = host
                    .unblock(move || store.apply(&[Mutation::remove_move_record(&id)]))
                    .await
                {
                    log::warn!("could not end the record of a cancelled move: {error}");
                }
            }
            host.enqueue_and_wait(move |app, cx| {
                app.moves.outgoing.remove(&project_id);
                if keep_trying {
                    app.moves.aborting.insert(journal.move_id.clone(), journal);
                    app.retry_aborts(0, cx);
                }
                app.publish_project_moves(cx);
            })
            .await
            .map_err(|()| host_stopped())
        })
    }

    /// Tell the other machines of cancelled moves that never heard it, after
    /// a wait.
    fn retry_aborts(&mut self, waits: u32, cx: &mut HostCx) {
        if self.moves.aborts_waiting {
            return;
        }
        let Ok(link) = self.machine_link().cloned() else {
            return;
        };
        self.moves.aborts_waiting = true;
        let journals: Vec<Journal> = self.moves.aborting.values().cloned().collect();
        let timer = cx.clock().timer(wait_after(waits));
        let host = cx.clone();
        let store = self.store.clone();
        cx.spawn_detached(async move {
            timer.await;
            let mut told = Vec::new();
            for journal in journals {
                if abort(&link, &journal).await {
                    let (store, id) = (store.clone(), journal.move_id.clone());
                    if host
                        .unblock(move || store.apply(&[Mutation::remove_move_record(&id)]))
                        .await
                        .is_ok()
                    {
                        told.push(journal.move_id);
                    }
                }
            }
            host.enqueue(move |app, cx| {
                app.moves.aborts_waiting = false;
                for id in told {
                    app.moves.aborting.remove(&id);
                }
                if !app.moves.aborting.is_empty() {
                    app.retry_aborts(waits + 1, cx);
                }
            });
        });
    }

    /// What moving `project_id` to `machine` would take, and what stands in
    /// its way, on this machine and on that one.
    pub(crate) fn project_move_preview(
        &mut self,
        project_id: &str,
        machine: &str,
        cx: &mut HostCx,
    ) -> HostTask<Result<ProjectMovePreview, ProtocolError>> {
        let prepared = self.move_target(project_id).map(|(link, project)| {
            let threads = self.moving_threads(project_id);
            let blockers = self.local_blockers(&project, &threads);
            (link, project, threads, blockers)
        });
        let host = cx.clone();
        let machine = machine.to_owned();
        cx.spawn_background(async move {
            let (link, project, threads, mut blockers) = prepared?;
            let code = {
                let (root, threads) = (project.root.clone(), threads.clone());
                host.unblock(move || code_preview(&root, &threads)).await
            };
            let mut code = code.map_err(|message| refusal("move_preview_failed", message))?;
            blockers.append(&mut code.blockers);
            let machines = link.machines(true).await?;
            let target = machines
                .iter()
                .find(|candidate| candidate.id == machine && !candidate.this_machine);
            let mut default_root = String::new();
            match target {
                None => blockers.push(MoveBlocker::TargetUnreachable),
                Some(target) if target.reachable != Some(true) => {
                    blockers.push(MoveBlocker::TargetUnreachable)
                }
                Some(target) if target.protocol_ok == Some(false) => {
                    blockers.push(MoveBlocker::TargetVersion)
                }
                Some(_) => match link
                    .call(
                        &machine,
                        MachineRequest::MovePreflight {
                            project_name: project.name.clone(),
                            root: None,
                            thread_ids: threads.iter().map(|meta| meta.id.clone()).collect(),
                        },
                    )
                    .await
                {
                    Ok(MachineResponse::MovePreflight { default_root: root }) => {
                        default_root = root
                    }
                    Ok(other) => return Err(unexpected(&other)),
                    Err(error) => match error.code.as_str() {
                        MACHINE_REFUSES => blockers.push(MoveBlocker::TargetRefuses),
                        MACHINE_UNREACHABLE => blockers.push(MoveBlocker::TargetUnreachable),
                        tcode_client::machines::PROTOCOL_MISMATCH => {
                            blockers.push(MoveBlocker::TargetVersion)
                        }
                        _ => return Err(error),
                    },
                },
            }
            Ok(ProjectMovePreview {
                default_root,
                git: code.git,
                branches: code.branches,
                bytes: code.bytes,
                dirty_checkouts: code.dirty_checkouts,
                threads: threads
                    .iter()
                    .filter(|meta| meta.archived_at.is_none())
                    .count() as u32,
                archived_threads: threads
                    .iter()
                    .filter(|meta| meta.archived_at.is_some())
                    .count() as u32,
                ignored_files: code.ignored_files,
                blockers,
                relayed: target.and_then(|target| target.relayed).unwrap_or(false),
            })
        })
    }

    /// The projects moving from this machine, as [`tcode_protocol::Topic::ProjectMoves`]
    /// answers.
    pub(crate) fn project_moves_snapshot(&self) -> Vec<ProjectMoveStatus> {
        self.moves
            .outgoing
            .values()
            .map(|outgoing| ProjectMoveStatus {
                move_id: outgoing.journal.move_id.clone(),
                project_id: outgoing.journal.project_id.clone(),
                machine: outgoing.journal.machine.clone(),
                phase: outgoing.phase,
                done: outgoing.done,
                total: outgoing.total,
                error: outgoing.error.clone(),
                commit_unknown: outgoing.commit_unknown,
            })
            .collect()
    }

    fn publish_project_moves(&self, cx: &mut HostCx) {
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic: Topic::ProjectMoves,
            event: ServerEvent::ProjectMovesReplaced(self.project_moves_snapshot()),
        }));
    }

    /// The other machine has the project: remove it and its threads here,
    /// their Tcode worktrees with them, leaving where each thread went. The
    /// project's directory stays.
    fn let_moved_project_go(&mut self, journal: &Journal, cx: &mut HostCx) {
        let Some(outgoing) = self.moves.outgoing.remove(&journal.project_id) else {
            return;
        };
        for id in &outgoing.journal.thread_ids {
            self.moves
                .moved_threads
                .insert(id.clone(), journal.machine.clone());
        }
        let mut ids = outgoing.journal.thread_ids.clone();
        for meta in self.moving_threads(&journal.project_id) {
            if !ids.contains(&meta.id) {
                ids.push(meta.id);
            }
        }
        if self
            .projects
            .iter()
            .any(|project| project.id == journal.project_id)
        {
            let task = self.remove_project_and_threads(&journal.project_id, &ids, true, cx);
            cx.spawn_detached(async move {
                if let Err(error) = task.await {
                    log::warn!("{}", error.message);
                }
            });
        }
        self.publish_project_moves(cx);
    }
}

fn moving() -> ProtocolError {
    refusal(
        PROJECT_MOVING,
        "This project is moving to another machine; wait for the move to end or cancel it.",
    )
}

fn moved_away(id: &str, machine: &str) -> ProtocolError {
    refusal(
        THREAD_MOVED,
        format!("Thread {id} moved to machine {machine} of the group."),
    )
}

fn unexpected(answer: &MachineResponse) -> ProtocolError {
    refusal(
        "unexpected_reply",
        format!("the other machine answered {answer:?}"),
    )
}

/// `blockers` as the refusal of a move, when there are any.
fn blocked(blockers: Vec<MoveBlocker>) -> Result<(), ProtocolError> {
    if blockers.is_empty() {
        return Ok(());
    }
    Err(refusal(
        "move_blocked",
        serde_json::to_string(&blockers).unwrap_or_default(),
    ))
}

/// The checkouts of the project that are mid-merge or mid-rebase, or have
/// unmerged entries.
fn interrupted_checkouts(root: &std::path::Path, threads: &[SessionMeta]) -> Vec<MoveBlocker> {
    if !disk::is_git_project(root) {
        return Vec::new();
    }
    disk::checkouts(root, threads)
        .into_iter()
        .filter(|(_, path)| disk::git::interrupted(path).unwrap_or(false))
        .map(|(_, path)| MoveBlocker::MergeInProgress {
            path: path.display().to_string(),
        })
        .collect()
}

#[derive(Default)]
struct CodePreview {
    git: bool,
    branches: u32,
    bytes: u64,
    dirty_checkouts: u32,
    ignored_files: Vec<String>,
    blockers: Vec<MoveBlocker>,
}

/// What a move would carry of the code at `root`: for a repository its
/// objects, branches, checkouts with changes and the files
/// `.worktreeinclude` would add; for a directory, its files.
fn code_preview(root: &std::path::Path, threads: &[SessionMeta]) -> Result<CodePreview, String> {
    if !disk::is_git_project(root) {
        return Ok(CodePreview {
            bytes: directory_bytes(root),
            ..CodePreview::default()
        });
    }
    let checkouts = disk::checkouts(root, threads);
    let mut preview = CodePreview {
        git: true,
        branches: disk::git::branch_count(root)?,
        bytes: disk::git::object_bytes(root)?,
        blockers: interrupted_checkouts(root, threads),
        ..CodePreview::default()
    };
    for (_, path) in &checkouts {
        if disk::git::dirty(path)? {
            preview.dirty_checkouts += 1;
        }
    }
    let mut seen = HashSet::new();
    for (_, files) in disk::ignored_files(&checkouts)? {
        for file in files {
            if seen.insert(file.clone()) {
                preview.ignored_files.push(file);
            }
        }
    }
    Ok(preview)
}

fn directory_bytes(root: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(entry.path()),
                Ok(kind) if kind.is_file() => {
                    total += entry.metadata().map_or(0, |meta| meta.len());
                }
                _ => {}
            }
        }
    }
    total
}

/// Tell the other machine of `journal`'s move to discard what it staged;
/// whether it heard. A machine that answers anything but a failure to reach
/// it has heard.
async fn abort(link: &MachineLink, journal: &Journal) -> bool {
    match link
        .call(
            &journal.machine,
            MachineRequest::MoveAbort {
                move_id: journal.move_id.clone(),
            },
        )
        .await
    {
        Ok(_) => true,
        Err(error) => !matches!(
            error.code.as_str(),
            MACHINE_UNREACHABLE | NO_GROUP | MACHINE_REFUSES
        ),
    }
}

/// One run of a move leaving this machine, off the host: it reads and
/// changes the move only through the host, and stops as soon as a cancel
/// has taken the move over.
struct Driver {
    host: HostCx,
    link: MachineLink,
    store: tcode_services::store::SessionStore,
    project_id: String,
    run: u64,
}

impl Driver {
    async fn drive(self) {
        let Some(journal) = self.journal().await else {
            return;
        };
        let outcome = match journal.stage.clone() {
            Stage::Sending => self.send(journal).await,
            Stage::CommitSent => self.settle_commit(journal).await,
            Stage::Committed { .. } => self.finish(journal).await,
            Stage::Aborting => Ok(()),
        };
        if let Err(stop) = outcome {
            let (project_id, run) = (self.project_id.clone(), self.run);
            self.host
                .enqueue(move |app, cx| app.pause_move(&project_id, Some(run), stop, cx));
        }
    }

    /// The move's journal while this driver still acts for it.
    async fn journal(&self) -> Option<Journal> {
        let (project_id, run) = (self.project_id.clone(), self.run);
        self.host
            .enqueue_and_wait(move |app, _| {
                app.moves
                    .outgoing
                    .get(&project_id)
                    .filter(|outgoing| outgoing.run == run)
                    .map(|outgoing| outgoing.journal.clone())
            })
            .await
            .ok()
            .flatten()
    }

    /// Change the move while this driver still acts for it, and publish it.
    async fn update(
        &self,
        change: impl FnOnce(&mut Outgoing) + Send + 'static,
    ) -> Result<(), Stop> {
        let (project_id, run) = (self.project_id.clone(), self.run);
        let current = self
            .host
            .enqueue_and_wait(move |app, cx| {
                let Some(outgoing) = app
                    .moves
                    .outgoing
                    .get_mut(&project_id)
                    .filter(|outgoing| outgoing.run == run)
                else {
                    return false;
                };
                change(outgoing);
                app.publish_project_moves(cx);
                true
            })
            .await
            .unwrap_or(false);
        if current {
            Ok(())
        } else {
            Err(Stop::Superseded)
        }
    }

    /// Write `journal` as the move's record, and keep it as the move's.
    async fn record(&self, journal: &Journal) -> Result<(), Stop> {
        let (store, record) = (self.store.clone(), journal.record());
        self.host
            .unblock(move || store.apply(&[record]))
            .await
            .map_err(|error| Stop::Failed(format!("could not record the move: {error}")))?;
        let journal = journal.clone();
        self.update(move |outgoing| outgoing.journal = journal)
            .await
    }

    async fn call(
        &self,
        journal: &Journal,
        request: MachineRequest,
    ) -> Result<MachineResponse, Stop> {
        self.link
            .call(&journal.machine, request)
            .await
            .map_err(stop_for)
    }

    async fn send(&self, journal: Journal) -> Result<(), Stop> {
        self.record(&journal).await?;
        self.update(|outgoing| outgoing.phase = MovePhase::Preparing)
            .await?;
        let parts = self.parts(&journal).await?;
        let total = parts.iter().map(|part| part.size).sum::<u64>();
        let begun = self
            .call(
                &journal,
                MachineRequest::MoveBegin {
                    move_id: journal.move_id.clone(),
                    project_name: journal.project_name.clone(),
                    root: journal.root.clone(),
                    git: parts.iter().any(|part| part.name == disk::CODE_BUNDLE),
                    thread_ids: journal.thread_ids.clone(),
                    parts: parts.clone(),
                },
            )
            .await?;
        let MachineResponse::MoveBegun { received } = begun else {
            return Err(Stop::Failed(unexpected(&begun).message));
        };
        let held = |name: &str| {
            received
                .iter()
                .find(|(part, _)| part == name)
                .map_or(0, |(_, held)| *held)
        };
        let mut done: u64 = parts
            .iter()
            .map(|part| held(&part.name).min(part.size))
            .sum();
        self.update(move |outgoing| {
            outgoing.phase = MovePhase::SendingCode;
            outgoing.total = total;
            outgoing.done = done;
            outgoing.waits = 0;
        })
        .await?;
        let out = disk::outgoing_dir(self.store.root(), &journal.move_id);
        for part in &parts {
            let phase = if disk::is_code_part(&part.name) {
                MovePhase::SendingCode
            } else {
                MovePhase::SendingThreads
            };
            let mut offset = held(&part.name).min(part.size);
            while offset < part.size {
                let path = out.join(&part.name);
                let length = (part.size - offset).min(disk::CHUNK_BYTES as u64);
                let bytes = self
                    .host
                    .unblock(move || read_at(&path, offset, length as usize))
                    .await
                    .map_err(|error| Stop::Failed(format!("could not read a part: {error}")))?;
                let answer = self
                    .call(
                        &journal,
                        MachineRequest::MoveChunk {
                            move_id: journal.move_id.clone(),
                            part: part.name.clone(),
                            offset,
                            bytes,
                        },
                    )
                    .await?;
                let MachineResponse::MoveProgress { received } = answer else {
                    return Err(Stop::Failed(unexpected(&answer).message));
                };
                let received = received.min(part.size);
                done = (done + received).saturating_sub(offset);
                offset = received;
                self.update(move |outgoing| {
                    outgoing.phase = phase;
                    outgoing.done = done;
                })
                .await?;
            }
        }
        self.commit(journal).await
    }

    /// The move's parts: those built before when they are intact, else built
    /// now from the store as it holds every write made so far.
    async fn parts(&self, journal: &Journal) -> Result<Vec<MovePart>, Stop> {
        let out = disk::outgoing_dir(self.store.root(), &journal.move_id);
        let reused = {
            let out = out.clone();
            self.host.unblock(move || disk::built_parts(&out)).await
        };
        if let Some(parts) = reused {
            return Ok(parts);
        }
        let ids = journal.thread_ids.clone();
        let (barrier, threads) = self
            .host
            .enqueue_and_wait(move |app, cx| {
                let threads: Vec<SessionMeta> =
                    ids.iter().filter_map(|id| app.find_meta(id)).collect();
                (app.store_write_barrier(cx), threads)
            })
            .await
            .map_err(|()| Stop::Superseded)?;
        match barrier.recv().await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(Stop::Failed(format!("the store failed: {error}"))),
            Err(_) => return Err(Stop::Superseded),
        }
        let store = self.store.clone();
        let journal = journal.clone();
        self.host
            .unblock(move || {
                disk::prepare(
                    &store,
                    &out,
                    &disk::Outgoing {
                        move_id: &journal.move_id,
                        project_name: &journal.project_name,
                        root: &journal.project_root,
                        include_ignored: journal.include_ignored,
                        threads: &threads,
                    },
                )
            })
            .await
            .map_err(Stop::Failed)
    }

    /// Send `MoveCommit`, recorded as sent before it goes, and settle what it
    /// answers.
    async fn commit(&self, mut journal: Journal) -> Result<(), Stop> {
        journal.stage = Stage::CommitSent;
        self.update(|outgoing| outgoing.phase = MovePhase::Committing)
            .await?;
        self.record(&journal).await?;
        let answer = self
            .link
            .call(
                &journal.machine,
                MachineRequest::MoveCommit {
                    move_id: journal.move_id.clone(),
                },
            )
            .await;
        self.settle(journal, answer).await
    }

    async fn settle(
        &self,
        mut journal: Journal,
        answer: Result<MachineResponse, ProtocolError>,
    ) -> Result<(), Stop> {
        match answer {
            Ok(MachineResponse::MoveCommitted { project_id }) => {
                journal.stage = Stage::Committed { project_id };
                self.record(&journal).await?;
                self.finish(journal).await
            }
            Ok(other) => Err(Stop::Failed(unexpected(&other).message)),
            Err(error) => match stop_for(error) {
                Stop::Failed(message) => {
                    // The other machine answered: it did not take the project.
                    journal.stage = Stage::Sending;
                    self.record(&journal).await?;
                    self.update(|outgoing| outgoing.commit_unknown = false)
                        .await?;
                    Err(Stop::Failed(message))
                }
                stop => {
                    self.update(|outgoing| outgoing.commit_unknown = true)
                        .await?;
                    Err(stop)
                }
            },
        }
    }

    /// The commit went out and no answer came: ask the other machine what
    /// became of it.
    async fn settle_commit(&self, mut journal: Journal) -> Result<(), Stop> {
        self.update(|outgoing| outgoing.phase = MovePhase::Committing)
            .await?;
        let state = self
            .call(
                &journal,
                MachineRequest::MoveStatus {
                    move_id: journal.move_id.clone(),
                },
            )
            .await?;
        match state {
            MachineResponse::MoveState(MoveState::Committed { project_id }) => {
                self.settle(journal, Ok(MachineResponse::MoveCommitted { project_id }))
                    .await
            }
            MachineResponse::MoveState(MoveState::Staging) => {
                let answer = self
                    .link
                    .call(
                        &journal.machine,
                        MachineRequest::MoveCommit {
                            move_id: journal.move_id.clone(),
                        },
                    )
                    .await;
                self.settle(journal, answer).await
            }
            MachineResponse::MoveState(MoveState::Unknown) => {
                // The other machine lost what it staged: everything goes again.
                journal.stage = Stage::Sending;
                self.update(|outgoing| outgoing.commit_unknown = false)
                    .await?;
                self.send(journal).await
            }
            other => Err(Stop::Failed(unexpected(&other).message)),
        }
    }

    /// Let the project go here now that the other machine has it.
    async fn finish(&self, journal: Journal) -> Result<(), Stop> {
        let redirects: Vec<Mutation> = journal
            .thread_ids
            .iter()
            .map(|id| Mutation::redirect_thread(id, &journal.machine))
            .collect();
        let store = self.store.clone();
        self.host
            .unblock(move || store.apply(&redirects))
            .await
            .map_err(|error| {
                Stop::Failed(format!("could not record where the threads went: {error}"))
            })?;
        let let_go = journal.clone();
        self.host
            .enqueue_and_wait(move |app, cx| app.let_moved_project_go(&let_go, cx))
            .await
            .map_err(|()| Stop::Superseded)?;
        let (store, data_dir) = (self.store.clone(), self.store.root().clone());
        let ended = self
            .host
            .unblock(move || {
                let ended = disk::end_outgoing(&data_dir, &journal.move_id, &journal.project_root);
                store
                    .apply(&[Mutation::remove_move_record(&journal.move_id)])
                    .map_err(|error| error.to_string())
                    .and(ended)
            })
            .await;
        if let Err(error) = ended {
            log::warn!("a moved project left something behind: {error}");
        }
        Ok(())
    }
}

fn read_at(path: &std::path::Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
