use super::*;

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
    CloneEvents {
        src: String,
        dst: String,
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
    Flush(smol::channel::Sender<()>),
}

pub(super) fn atomic_write(path: PathBuf, bytes: Vec<u8>) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)
}

pub(super) fn run_store_write(
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
        StoreWrite::UpsertMeta { meta, initial } => store.upsert_meta(&meta).err().map(|err| {
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
        StoreWrite::RemoveSession(id) => store.remove_session(&id).err().map(|err| {
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
    }
}
