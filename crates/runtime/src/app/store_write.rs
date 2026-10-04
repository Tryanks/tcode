use std::sync::atomic::Ordering;

use super::compaction::{Compacted, CompactionRequest};
use super::*;
use tcode_services::store::{CompactOutcome, CompactRejection, IndexWrite};

pub(super) enum StoreWrite {
    AppendEvent {
        id: String,
        ts: u64,
        event: Box<AgentEvent>,
    },
    UpsertMetas {
        metas: Vec<SessionMeta>,
        initial: bool,
    },
    UpsertProject(Project),
    RemoveSessions(Vec<String>),
    RemoveProject(String),
    CloneEvents {
        src: String,
        dst: String,
        completion: smol::channel::Sender<Result<(), String>>,
    },
    WriteEventLog {
        id: String,
        bytes: Vec<u8>,
        completion: smol::channel::Sender<Result<(), String>>,
    },
    SaveCommands {
        provider: ProviderKind,
        acp_agent_id: Option<String>,
        commands: Vec<ProviderCommand>,
    },
    WriteTerminalUi(Vec<u8>),
    WriteSettings(Vec<u8>),
    SetProfileSecret {
        profile_id: String,
        key: String,
        value: Option<String>,
    },
    ClearProfileSecrets(String),
    /// Rewrite one log compacted, unless the request was withdrawn.
    CompactLog {
        id: String,
        request: Arc<CompactionRequest>,
    },
    /// Delete the originals and temporary files earlier runs' compactions
    /// left beside the logs.
    DiscardCompactionLeftovers,
    Flush(smol::channel::Sender<()>),
}

impl StoreWrite {
    fn changes_index(&self) -> bool {
        matches!(
            self,
            StoreWrite::UpsertMetas { .. }
                | StoreWrite::UpsertProject(_)
                | StoreWrite::RemoveSessions(_)
                | StoreWrite::RemoveProject(_)
        )
    }
}

/// The error a failed index write reports, by what it was for.
#[derive(Clone, Copy, PartialEq)]
enum IndexFailure {
    CreateSession,
    UpdateSession,
    PersistProject,
    DeleteSession,
    DeleteProject,
}

impl IndexFailure {
    fn error(self, error: String) -> RuntimeError {
        match self {
            Self::CreateSession => RuntimeError::PersistSession { error },
            Self::UpdateSession => RuntimeError::PersistSessionIndex { error },
            Self::PersistProject => RuntimeError::PersistProject { error },
            Self::DeleteSession => RuntimeError::DeleteSession { error },
            Self::DeleteProject => RuntimeError::DeleteProject { error },
        }
    }
}

/// Runs every queued write in queue order. Index writes queued back to back
/// are committed together, since every index commit is synced to disk.
pub(super) struct StoreWriter {
    store: SessionStore,
    index: SessionIndex,
    settings_store: SettingsStore,
    terminal_preferences_path: PathBuf,
    /// Logs compacted in this run with nothing written to them since, which
    /// another compaction would leave as they are.
    compacted: HashSet<String>,
}

impl StoreWriter {
    pub(super) fn new(
        store: SessionStore,
        index: SessionIndex,
        settings_store: SettingsStore,
        terminal_preferences_path: PathBuf,
    ) -> Self {
        Self {
            store,
            index,
            settings_store,
            terminal_preferences_path,
            compacted: HashSet::new(),
        }
    }

    /// Run queued writes until every sender is gone and the queue is empty.
    pub(super) async fn serve(
        mut self,
        writes: smol::channel::Receiver<StoreWrite>,
        failures: smol::channel::Sender<Result<RuntimeError, String>>,
        host_cx: HostCx,
    ) {
        let mut next = None;
        loop {
            let write = match next.take() {
                Some(write) => write,
                None => match writes.recv().await {
                    Ok(write) => write,
                    Err(_) => break,
                },
            };
            let reported = if write.changes_index() {
                let mut batch = vec![write];
                while let Ok(write) = writes.try_recv() {
                    if write.changes_index() {
                        batch.push(write);
                    } else {
                        next = Some(write);
                        break;
                    }
                }
                self.commit_index(batch, &host_cx).await
            } else {
                self.run(write, &host_cx).await.into_iter().collect()
            };
            for failure in reported {
                let _ = failures.send(failure).await;
            }
        }
    }

    /// Commit index writes in one transaction on the blocking pool, then
    /// delete the logs of the sessions they removed.
    async fn commit_index(
        &mut self,
        batch: Vec<StoreWrite>,
        host_cx: &HostCx,
    ) -> Vec<Result<RuntimeError, String>> {
        let mut writes = Vec::with_capacity(batch.len());
        let mut failures = Vec::with_capacity(batch.len());
        let mut removed = Vec::new();
        for write in batch {
            let (write, failure) = match write {
                StoreWrite::UpsertMetas { metas, initial } => (
                    IndexWrite::UpsertSessions(metas),
                    if initial {
                        IndexFailure::CreateSession
                    } else {
                        IndexFailure::UpdateSession
                    },
                ),
                StoreWrite::UpsertProject(project) => (
                    IndexWrite::UpsertProject(project),
                    IndexFailure::PersistProject,
                ),
                StoreWrite::RemoveSessions(ids) => {
                    for id in &ids {
                        self.compacted.remove(id);
                    }
                    removed.push(ids.clone());
                    (IndexWrite::RemoveSessions(ids), IndexFailure::DeleteSession)
                }
                StoreWrite::RemoveProject(id) => {
                    (IndexWrite::RemoveProject(id), IndexFailure::DeleteProject)
                }
                _ => unreachable!("only index writes are batched"),
            };
            writes.push(write);
            if !failures.contains(&failure) {
                failures.push(failure);
            }
        }
        let index = self.index.clone();
        let store = self.store.clone();
        let committed = host_cx
            .unblock(move || {
                index.commit(&writes)?;
                Ok::<_, std::io::Error>(
                    removed
                        .iter()
                        .filter_map(|ids| store.remove_session_logs(ids).err())
                        .collect::<Vec<_>>(),
                )
            })
            .await;
        match committed {
            Ok(log_errors) => log_errors
                .into_iter()
                .map(|error| Ok(IndexFailure::DeleteSession.error(error.to_string())))
                .collect(),
            Err(error) => failures
                .into_iter()
                .map(|failure| Ok(failure.error(error.to_string())))
                .collect(),
        }
    }

    /// Run one write. Compaction's reading, folding and syncing happen on the
    /// blocking pool and are awaited here, so the writes queued behind it
    /// still wait for it while the executor thread stays free.
    async fn run(
        &mut self,
        write: StoreWrite,
        host_cx: &HostCx,
    ) -> Option<Result<RuntimeError, String>> {
        match write {
            StoreWrite::CompactLog { id, request } => {
                self.compact(id, request, host_cx).await;
                None
            }
            StoreWrite::DiscardCompactionLeftovers => {
                let store = self.store.clone();
                host_cx
                    .unblock(move || store.discard_compaction_leftovers())
                    .await
                    .err()
                    .map(|err| Err(format!("failed to delete compaction leftovers: {err}")))
            }
            write => {
                match &write {
                    StoreWrite::AppendEvent { id, .. } | StoreWrite::WriteEventLog { id, .. } => {
                        self.compacted.remove(id);
                    }
                    StoreWrite::CloneEvents { dst, .. } => {
                        self.compacted.remove(dst);
                    }
                    _ => {}
                }
                run_store_write(
                    &self.store,
                    &self.settings_store,
                    &self.terminal_preferences_path,
                    write,
                )
            }
        }
    }

    async fn compact(&mut self, id: String, request: Arc<CompactionRequest>, host_cx: &HostCx) {
        let compacted = if request.withdrawn.load(Ordering::Relaxed) || self.compacted.contains(&id)
        {
            Compacted::Skipped
        } else {
            let store = self.store.clone();
            let read_id = id.clone();
            let pass = request.pass;
            host_cx
                .unblock(move || {
                    if pass && store.log_epoch(&read_id) > 0 {
                        Compacted::Skipped
                    } else {
                        Compacted::Done(store.compact_log(&read_id))
                    }
                })
                .await
        };
        // A failed read or write may succeed next time; anything else would
        // come out the same until the log is written to.
        if let Compacted::Done(outcome) = &compacted {
            let retry = matches!(
                outcome,
                CompactOutcome::Rejected(CompactRejection::Io(error))
                    if error.kind() != std::io::ErrorKind::NotFound
            );
            if !retry {
                self.compacted.insert(id.clone());
            }
        }
        host_cx.enqueue(move |state, cx| state.compaction_finished(id, request, compacted, cx));
    }
}

pub(super) fn atomic_write(path: PathBuf, bytes: Vec<u8>) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)
}

fn run_store_write(
    store: &SessionStore,
    settings_store: &SettingsStore,
    terminal_preferences_path: &Path,
    write: StoreWrite,
) -> Option<Result<RuntimeError, String>> {
    match write {
        StoreWrite::AppendEvent { id, ts, event } => {
            store.append_event(&id, ts, &event).err().map(|err| {
                Ok(RuntimeError::PersistEvent {
                    error: err.to_string(),
                })
            })
        }
        StoreWrite::CloneEvents {
            src,
            dst,
            completion,
        } => {
            let result = store
                .clone_events(&src, &dst)
                .map_err(|err| err.to_string());
            let _ = completion.try_send(result);
            None
        }
        StoreWrite::WriteEventLog {
            id,
            bytes,
            completion,
        } => {
            let result = store
                .write_event_log(&id, &bytes)
                .map_err(|err| err.to_string());
            let _ = completion.try_send(result);
            None
        }
        StoreWrite::SaveCommands {
            provider,
            acp_agent_id,
            commands,
        } => store
            .save_commands(provider, acp_agent_id.as_deref(), &commands)
            .err()
            .map(|err| {
                Err(format!(
                    "failed to persist {provider:?} command cache: {err}"
                ))
            }),
        StoreWrite::WriteTerminalUi(bytes) => {
            atomic_write(terminal_preferences_path.to_path_buf(), bytes)
                .err()
                .map(|err| Err(format!("failed to persist terminal UI state: {err}")))
        }
        StoreWrite::WriteSettings(bytes) => atomic_write(store.root().join("settings.json"), bytes)
            .err()
            .map(|err| {
                Ok(RuntimeError::PersistSettings {
                    error: err.to_string(),
                })
            }),
        StoreWrite::SetProfileSecret {
            profile_id,
            key,
            value,
        } => settings_store
            .set_profile_secret(&profile_id, &key, value.as_deref())
            .err()
            .map(|err| {
                Ok(RuntimeError::PersistSettings {
                    error: err.to_string(),
                })
            }),
        StoreWrite::ClearProfileSecrets(profile_id) => settings_store
            .clear_profile_secrets(&profile_id)
            .err()
            .map(|err| {
                Ok(RuntimeError::PersistSettings {
                    error: err.to_string(),
                })
            }),
        StoreWrite::Flush(completion) => {
            let _ = completion.try_send(());
            None
        }
        StoreWrite::CompactLog { .. } | StoreWrite::DiscardCompactionLeftovers => {
            unreachable!("run by StoreWriter::run")
        }
        StoreWrite::UpsertMetas { .. }
        | StoreWrite::UpsertProject(_)
        | StoreWrite::RemoveSessions(_)
        | StoreWrite::RemoveProject(_) => unreachable!("run by StoreWriter::commit_index"),
    }
}
