use super::*;
use tcode_services::store::Mutation;

/// A drain of the queue is committed in transactions of at most this many
/// writes and about this many bytes; streamed deltas then share a commit
/// instead of each paying for one.
const MAX_BATCH_WRITES: usize = 256;
const MAX_BATCH_BYTES: usize = 4 << 20;

pub(super) enum StoreWrite {
    AppendEvent {
        id: String,
        ts: u64,
        event: Box<AgentEvent>,
    },
    UpsertMeta {
        meta: Box<SessionMeta>,
        initial: bool,
    },
    UpsertProject(Project),
    RemoveSession(String),
    RemoveProject(String),
    /// The source's event log copied to a new thread, committed together with
    /// the new thread's meta.
    Fork {
        src: String,
        meta: Box<SessionMeta>,
        completion: smol::channel::Sender<Result<(), String>>,
    },
    SaveCommands {
        key: CommandsCacheKey,
        commands: Vec<ProviderCommand>,
    },
    InvalidateCommands(CommandsCacheKey),
    WriteTerminalUi(Vec<u8>),
    WriteSettings(Vec<u8>),
    SetProfileSecret {
        profile_id: String,
        key: String,
        value: Option<String>,
    },
    ClearProfileSecrets(String),
    /// Answered once every write queued before it has committed, or with the
    /// first store failure since the writer started: a write that failed is
    /// never certified by a later flush.
    Flush(smol::channel::Sender<Result<(), String>>),
}

/// What the writer reports back to the host.
pub(super) enum StoreWriteFailure {
    Error(RuntimeError),
    Warning(String),
    /// The store stopped serving after a panic or a broken connection; the
    /// failed writes are reported separately.
    StoreFailed(String),
}

/// One store write inside a batch: how its failure is reported, and who waits
/// for its commit.
struct Member {
    failure: fn(String) -> RuntimeError,
    completion: Option<smol::channel::Sender<Result<(), String>>>,
}

#[derive(Default)]
struct Batch {
    mutations: Vec<Mutation>,
    members: Vec<Member>,
    bytes: usize,
}

impl StoreWrite {
    /// The store changes a write makes, or `None` for a write that is not a
    /// database change.
    fn mutations(&self) -> Option<Result<Vec<Mutation>, String>> {
        Some(Ok(match self {
            StoreWrite::AppendEvent { id, ts, event } => {
                match Mutation::append_event(id, *ts, event) {
                    Ok(mutation) => vec![mutation],
                    Err(error) => return Some(Err(error.to_string())),
                }
            }
            StoreWrite::UpsertMeta { meta, .. } => vec![Mutation::upsert_meta((**meta).clone())],
            StoreWrite::UpsertProject(project) => vec![Mutation::upsert_project(project.clone())],
            StoreWrite::RemoveSession(id) => vec![Mutation::remove_session(id)],
            StoreWrite::RemoveProject(id) => vec![Mutation::remove_project(id)],
            StoreWrite::Fork { src, meta, .. } => vec![
                Mutation::clone_events(src, &meta.id),
                Mutation::upsert_meta((**meta).clone()),
            ],
            _ => return None,
        }))
    }

    fn member(self) -> Member {
        let (failure, completion): (fn(String) -> RuntimeError, _) = match self {
            StoreWrite::AppendEvent { .. } => (|error| RuntimeError::PersistEvent { error }, None),
            StoreWrite::UpsertMeta { initial: true, .. } => {
                (|error| RuntimeError::PersistSession { error }, None)
            }
            StoreWrite::UpsertMeta { initial: false, .. } => {
                (|error| RuntimeError::PersistSessionIndex { error }, None)
            }
            StoreWrite::UpsertProject(_) => (|error| RuntimeError::PersistProject { error }, None),
            StoreWrite::RemoveSession(_) => (|error| RuntimeError::DeleteSession { error }, None),
            StoreWrite::RemoveProject(_) => (|error| RuntimeError::DeleteProject { error }, None),
            StoreWrite::Fork { completion, .. } => (
                |error| RuntimeError::PersistSession { error },
                Some(completion),
            ),
            StoreWrite::Flush(completion) => (
                |error| RuntimeError::PersistEvent { error },
                Some(completion),
            ),
            StoreWrite::SaveCommands { .. }
            | StoreWrite::InvalidateCommands(_)
            | StoreWrite::WriteTerminalUi(_)
            | StoreWrite::WriteSettings(_)
            | StoreWrite::SetProfileSecret { .. }
            | StoreWrite::ClearProfileSecrets(_) => {
                (|error| RuntimeError::PersistSettings { error }, None)
            }
        };
        Member {
            failure,
            completion,
        }
    }

    /// Fail a write the writer never accepted: its waiter hears why, and
    /// anything else is reported like a failed write.
    pub(super) fn reject(self, reason: &str) -> Option<StoreWriteFailure> {
        let reports = !matches!(self, StoreWrite::Flush(_) | StoreWrite::Fork { .. });
        let member = self.member();
        if let Some(completion) = member.completion {
            let _ = completion.try_send(Err(reason.to_owned()));
        }
        reports.then(|| StoreWriteFailure::Error((member.failure)(reason.to_owned())))
    }
}

pub(super) fn atomic_write(path: PathBuf, bytes: Vec<u8>) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)
}

pub(super) struct StoreWriter {
    pub(super) store: SessionStore,
    pub(super) settings_store: SettingsStore,
    pub(super) terminal_preferences_path: PathBuf,
    pub(super) failures: smol::channel::Sender<StoreWriteFailure>,
    /// The first failed database write, until the writer stops.
    unresolved: Option<String>,
}

impl StoreWriter {
    pub(super) fn new(
        store: SessionStore,
        settings_store: SettingsStore,
        terminal_preferences_path: PathBuf,
        failures: smol::channel::Sender<StoreWriteFailure>,
    ) -> Self {
        Self {
            store,
            settings_store,
            terminal_preferences_path,
            failures,
            unresolved: None,
        }
    }

    /// Serve `writes` until every sender is gone and the queue is drained.
    pub(super) fn run(mut self, writes: smol::channel::Receiver<StoreWrite>) {
        while let Ok(first) = writes.recv_blocking() {
            let mut queue = vec![first];
            while queue.len() < MAX_BATCH_WRITES {
                match writes.try_recv() {
                    Ok(write) => queue.push(write),
                    Err(_) => break,
                }
            }
            self.process(queue);
        }
    }

    /// Database writes accumulate into one transaction; any other write is an
    /// ordered boundary that commits what came before it first.
    fn process(&mut self, queue: Vec<StoreWrite>) {
        let mut batch = Batch::default();
        for write in queue {
            match write.mutations() {
                Some(Ok(mutations)) => {
                    batch.bytes += mutations.iter().map(Mutation::payload_len).sum::<usize>();
                    batch.mutations.extend(mutations);
                    batch.members.push(write.member());
                    if batch.bytes >= MAX_BATCH_BYTES {
                        self.commit(&mut batch);
                    }
                }
                Some(Err(error)) => {
                    let member = write.member();
                    self.unresolved.get_or_insert_with(|| error.clone());
                    self.report(StoreWriteFailure::Error((member.failure)(error)));
                }
                None => {
                    self.commit(&mut batch);
                    if let Some(failure) = self.run_file_write(write) {
                        self.report(failure);
                    }
                }
            }
        }
        self.commit(&mut batch);
    }

    fn commit(&mut self, batch: &mut Batch) {
        let Batch {
            mutations, members, ..
        } = std::mem::take(batch);
        if members.is_empty() {
            return;
        }
        match self.store.apply(&mutations) {
            Ok(()) => {
                for member in members {
                    if let Some(completion) = member.completion {
                        let _ = completion.try_send(Ok(()));
                    }
                }
            }
            // The whole batch failed together; it is reported, not replayed.
            Err(error) => {
                let error = error.to_string();
                self.unresolved.get_or_insert_with(|| error.clone());
                for member in members {
                    match member.completion {
                        // The waiter reports its own failure.
                        Some(completion) => {
                            let _ = completion.try_send(Err(error.clone()));
                        }
                        None => {
                            self.report(StoreWriteFailure::Error((member.failure)(error.clone())))
                        }
                    }
                }
                if self.store.is_failed() {
                    self.report(StoreWriteFailure::StoreFailed(error));
                }
            }
        }
    }

    fn report(&self, failure: StoreWriteFailure) {
        let _ = self.failures.send_blocking(failure);
    }

    fn run_file_write(&mut self, write: StoreWrite) -> Option<StoreWriteFailure> {
        let settings_failure = |err: std::io::Error| {
            StoreWriteFailure::Error(RuntimeError::PersistSettings {
                error: err.to_string(),
            })
        };
        match write {
            StoreWrite::SaveCommands { key, commands } => {
                self.store.save_commands(&key, &commands).err().map(|err| {
                    StoreWriteFailure::Warning(format!(
                        "failed to persist {key:?} command cache: {err}"
                    ))
                })
            }
            StoreWrite::InvalidateCommands(key) => {
                self.store.invalidate_commands(&key).err().map(|err| {
                    StoreWriteFailure::Warning(format!(
                        "failed to invalidate {key:?} command cache: {err}"
                    ))
                })
            }
            StoreWrite::WriteTerminalUi(bytes) => {
                atomic_write(self.terminal_preferences_path.clone(), bytes)
                    .err()
                    .map(|err| {
                        StoreWriteFailure::Warning(format!(
                            "failed to persist terminal UI state: {err}"
                        ))
                    })
            }
            StoreWrite::WriteSettings(bytes) => {
                atomic_write(self.store.root().join("settings.json"), bytes)
                    .err()
                    .map(settings_failure)
            }
            StoreWrite::SetProfileSecret {
                profile_id,
                key,
                value,
            } => self
                .settings_store
                .set_profile_secret(&profile_id, &key, value.as_deref())
                .err()
                .map(settings_failure),
            StoreWrite::ClearProfileSecrets(profile_id) => self
                .settings_store
                .clear_profile_secrets(&profile_id)
                .err()
                .map(settings_failure),
            StoreWrite::Flush(completion) => {
                let _ = completion.try_send(match &self.unresolved {
                    None => Ok(()),
                    Some(error) => Err(format!("an earlier session-store write failed: {error}")),
                });
                None
            }
            StoreWrite::AppendEvent { .. }
            | StoreWrite::UpsertMeta { .. }
            | StoreWrite::UpsertProject(_)
            | StoreWrite::RemoveSession(_)
            | StoreWrite::RemoveProject(_)
            | StoreWrite::Fork { .. } => unreachable!("database writes are batched"),
        }
    }
}
