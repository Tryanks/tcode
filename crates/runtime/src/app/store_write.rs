use std::sync::atomic::Ordering;

use super::compaction::{Compacted, CompactionRequest};
use super::*;
use tcode_services::store::{CompactOutcome, CompactRejection};

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

/// Runs every queued write, one at a time in queue order.
pub(super) struct StoreWriter {
    store: SessionStore,
    settings_store: SettingsStore,
    terminal_preferences_path: PathBuf,
    /// Logs compacted in this run with nothing written to them since, which
    /// another compaction would leave as they are.
    compacted: HashSet<String>,
}

impl StoreWriter {
    pub(super) fn new(
        store: SessionStore,
        settings_store: SettingsStore,
        terminal_preferences_path: PathBuf,
    ) -> Self {
        Self {
            store,
            settings_store,
            terminal_preferences_path,
            compacted: HashSet::new(),
        }
    }

    /// Run one write. Compaction's reading, folding and syncing happen on the
    /// blocking pool and are awaited here, so the writes queued behind it
    /// still wait for it while the executor thread stays free.
    pub(super) async fn run(
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
                    StoreWrite::RemoveSessions(ids) => {
                        for id in ids {
                            self.compacted.remove(id);
                        }
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
        StoreWrite::UpsertMetas { metas, initial } => store.upsert_metas(&metas).err().map(|err| {
            if initial {
                Ok(RuntimeError::PersistSession {
                    error: err.to_string(),
                })
            } else {
                Ok(RuntimeError::PersistSessionIndex {
                    error: err.to_string(),
                })
            }
        }),
        StoreWrite::UpsertProject(project) => store.upsert_project(&project).err().map(|err| {
            Ok(RuntimeError::PersistProject {
                error: err.to_string(),
            })
        }),
        StoreWrite::RemoveSessions(ids) => store.remove_sessions(&ids).err().map(|err| {
            Ok(RuntimeError::DeleteSession {
                error: err.to_string(),
            })
        }),
        StoreWrite::RemoveProject(id) => store.remove_project(&id).err().map(|err| {
            Ok(RuntimeError::DeleteProject {
                error: err.to_string(),
            })
        }),
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
    }
}
