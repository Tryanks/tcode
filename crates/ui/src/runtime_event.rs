use agent::ProviderKind;
use tcode_core::git::GitAction;
use tcode_protocol::{
    GitActionRequest, MergeWorktreeFailure, NoticeSeverity, PluginOperationTarget, RuntimeError,
    RuntimeNotice, RuntimeNotification as RuntimeEvent, RuntimeOperationId, RuntimeToast,
};

use crate::toast::ToastKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RuntimeEventSeverity {
    Error,
    Warning,
    Success,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PresentedRuntimeEvent {
    pub severity: RuntimeEventSeverity,
    pub message: String,
}

pub(super) fn present_runtime_event(event: &RuntimeEvent) -> PresentedRuntimeEvent {
    let (severity, message) = match event {
        RuntimeEvent::Error(error) => {
            let message = match error {
                RuntimeError::External(message) | RuntimeError::ProviderMessage(message) => {
                    message.clone()
                }
                RuntimeError::PersistSettings { error } => {
                    crate::tr!("errors.persist_settings", error = error).into_owned()
                }
                RuntimeError::SettingsUnreadable { error } => {
                    crate::tr!("errors.settings_unreadable", error = error).into_owned()
                }
                RuntimeError::FileUnreadable { error } => {
                    crate::tr!("errors.file_unreadable", error = error).into_owned()
                }
                RuntimeError::McpUnavailable { error } => {
                    crate::tr!("errors.mcp_unavailable", error = error).into_owned()
                }
                RuntimeError::TerminalStart { error } => {
                    crate::tr!("errors.terminal_start", error = error).into_owned()
                }
                RuntimeError::TerminalRestart { error } => {
                    crate::tr!("errors.terminal_restart", error = error).into_owned()
                }
                RuntimeError::PersistProject { error } => {
                    crate::tr!("errors.persist_project", error = error).into_owned()
                }
                RuntimeError::WorktreeRemove { error } => {
                    crate::tr!("errors.worktree_remove", error = error).into_owned()
                }
                RuntimeError::DeleteSession { error } => {
                    crate::tr!("errors.delete_session", error = error).into_owned()
                }
                RuntimeError::DeleteProject { error } => {
                    crate::tr!("errors.delete_project", error = error).into_owned()
                }
                RuntimeError::PersistEvent { error } => {
                    crate::tr!("errors.persist_event", error = error).into_owned()
                }
                RuntimeError::WorktreeAdd { error } => {
                    crate::tr!("errors.worktree_add", error = error).into_owned()
                }
                RuntimeError::PersistSession { error } => {
                    crate::tr!("errors.persist_session", error = error).into_owned()
                }
                RuntimeError::TitleGenerationFailed => {
                    crate::tr!("errors.title_generation_failed").into_owned()
                }
                RuntimeError::TitleGenerationEmpty => {
                    crate::tr!("errors.title_generation_empty").into_owned()
                }
                RuntimeError::ProcessGone => crate::tr!("errors.process_gone").into_owned(),
                RuntimeError::SteerUnsupported { agent } => {
                    crate::tr!("composer.steer_unsupported", agent = agent).into_owned()
                }
                RuntimeError::DirtyTree => crate::tr!("notice.dirty_tree").into_owned(),
                RuntimeError::ProviderStart { harness, error } => {
                    crate::tr!("errors.provider_start", harness = harness, error = error)
                        .into_owned()
                }
                RuntimeError::ProviderBatchLineBreak { program } => crate::tr!(
                    "errors.provider_batch_line_break",
                    program = program.to_string_lossy()
                )
                .into_owned(),
                RuntimeError::WorkingDirectoryMissing { cwd, .. } => crate::tr!(
                    "errors.working_directory_missing",
                    cwd = cwd.to_string_lossy()
                )
                .into_owned(),
                RuntimeError::ProviderClosed {
                    harness,
                    reason: Some(reason),
                } => crate::tr!(
                    "errors.provider_closed_reason",
                    harness = harness,
                    reason = reason
                )
                .into_owned(),
                RuntimeError::ProviderClosed {
                    harness,
                    reason: None,
                } => crate::tr!("errors.provider_closed", harness = harness).into_owned(),
                RuntimeError::PersistSessionIndex { error } => {
                    crate::tr!("errors.persist_session_index", error = error).into_owned()
                }
                RuntimeError::ProfileConfig { error } => {
                    crate::tr!("errors.profile_config", error = error).into_owned()
                }
            };
            (RuntimeEventSeverity::Error, message)
        }
        RuntimeEvent::Notice(notice) => {
            let severity = match notice.severity() {
                NoticeSeverity::Success => RuntimeEventSeverity::Success,
                NoticeSeverity::Warning => RuntimeEventSeverity::Warning,
            };
            let message = match notice {
                RuntimeNotice::ProviderMessage(message) => message.clone(),
                RuntimeNotice::TcodeUpdateAvailable { version } => {
                    crate::tr!("notice.tcode_update_available", version = version).into_owned()
                }
                RuntimeNotice::SwitchedBranch { branch } => {
                    crate::tr!("notice.switched_branch", branch = branch).into_owned()
                }
                RuntimeNotice::WorktreeSeeded {
                    copied_files,
                    skipped,
                } => crate::tr!(
                    "notice.worktree_seeded",
                    copied = copied_files,
                    skipped = skipped.len()
                )
                .into_owned(),
                RuntimeNotice::WorktreeMergedFastForward => {
                    crate::tr!("notice.worktree_merged_fast_forward").into_owned()
                }
                RuntimeNotice::WorktreeMergedCommit => {
                    crate::tr!("notice.worktree_merged_commit").into_owned()
                }
                RuntimeNotice::ReviewRecorded { items, coverage } => {
                    crate::tr!("notice.review_recorded", items = items, coverage = coverage)
                        .into_owned()
                }
                RuntimeNotice::ReferenceBoundaryCrossed {
                    thread_title,
                    title,
                    ..
                } => crate::tr!(
                    "notice.reference_boundary_crossed",
                    thread = thread_title,
                    title = title
                )
                .into_owned(),
                RuntimeNotice::WorktreeMergeFailed { reason, detail } => match reason {
                    MergeWorktreeFailure::Missing => {
                        crate::tr!("notice.worktree_merge_missing").into_owned()
                    }
                    MergeWorktreeFailure::DirtyWorktree => {
                        crate::tr!("notice.worktree_merge_dirty_worktree").into_owned()
                    }
                    MergeWorktreeFailure::DestinationDetached => {
                        crate::tr!("notice.worktree_merge_destination_detached").into_owned()
                    }
                    MergeWorktreeFailure::DirtyDestination => {
                        crate::tr!("notice.worktree_merge_dirty_destination").into_owned()
                    }
                    MergeWorktreeFailure::DivergedConflict => {
                        crate::tr!("notice.worktree_merge_conflict").into_owned()
                    }
                    MergeWorktreeFailure::Git => crate::tr!(
                        "notice.worktree_merge_git_error",
                        error = detail.as_deref().unwrap_or_default()
                    )
                    .into_owned(),
                },
            };
            (severity, message)
        }
        RuntimeEvent::Toast(_) => unreachable!("rich toasts use present_runtime_toast"),
        RuntimeEvent::Effect(_) => unreachable!("runtime effects are not presentable"),
    };

    PresentedRuntimeEvent { severity, message }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RuntimeToastDisposition {
    Push,
    Start(RuntimeOperationId),
    Update(RuntimeOperationId),
    Finish(RuntimeOperationId),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PresentedRuntimeToast {
    pub disposition: RuntimeToastDisposition,
    pub kind: ToastKind,
    pub title: String,
    pub detail: Option<String>,
    pub action: Option<RuntimeToastAction>,
    pub progress: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RuntimeToastAction {
    RetryGit(GitActionRequest),
    UpdateProviders(Vec<ProviderKind>),
    /// Stop the profile's running plugin change.
    CancelPluginOperation(String),
    /// Stop the ACP agent's install, by registry id.
    CancelAcpInstall(String),
    /// Stop the running harness updates.
    CancelProviderUpdate,
}

fn git_action_toast_titles(action: GitAction) -> (String, String) {
    match action {
        GitAction::Commit => (
            crate::tr!("git.toast.committing").into_owned(),
            crate::tr!("git.toast.committed").into_owned(),
        ),
        GitAction::CommitPush => (
            crate::tr!("git.toast.committing_pushing").into_owned(),
            crate::tr!("git.toast.committed_pushed").into_owned(),
        ),
        GitAction::Push => (
            crate::tr!("git.toast.pushing").into_owned(),
            crate::tr!("git.toast.pushed").into_owned(),
        ),
        GitAction::Pull => (
            crate::tr!("git.toast.pulling").into_owned(),
            crate::tr!("git.toast.pulled").into_owned(),
        ),
        GitAction::PublishBranch => (
            crate::tr!("git.toast.publishing").into_owned(),
            crate::tr!("git.toast.published").into_owned(),
        ),
        GitAction::InitializeGit => (
            crate::tr!("git.toast.initializing").into_owned(),
            crate::tr!("git.toast.initialized").into_owned(),
        ),
    }
}

/// What a native plugin operation changes, as its toasts name it.
fn plugin_operation_name(target: &PluginOperationTarget) -> &str {
    match target {
        PluginOperationTarget::Plugin { plugin, .. } => plugin,
        PluginOperationTarget::AddMarketplace { source } => source,
        PluginOperationTarget::RemoveMarketplace { name } => name,
    }
}

/// Started, succeeded and failed titles for a native plugin operation.
fn plugin_operation_toast_titles(target: &PluginOperationTarget) -> [String; 3] {
    use agent::PluginActionKind;
    let (name, keys) = match target {
        PluginOperationTarget::Plugin { action, plugin, .. } => (
            plugin.as_str(),
            match action {
                PluginActionKind::Install => [
                    "providers.plugins.toast.installing",
                    "providers.plugins.toast.installed",
                    "providers.plugins.toast.install_failed",
                ],
                PluginActionKind::Uninstall => [
                    "providers.plugins.toast.uninstalling",
                    "providers.plugins.toast.uninstalled",
                    "providers.plugins.toast.uninstall_failed",
                ],
                PluginActionKind::Enable => [
                    "providers.plugins.toast.enabling",
                    "providers.plugins.toast.enabled",
                    "providers.plugins.toast.enable_failed",
                ],
                PluginActionKind::Disable => [
                    "providers.plugins.toast.disabling",
                    "providers.plugins.toast.disabled",
                    "providers.plugins.toast.disable_failed",
                ],
                PluginActionKind::Update => [
                    "providers.plugins.toast.updating",
                    "providers.plugins.toast.updated",
                    "providers.plugins.toast.update_failed",
                ],
            },
        ),
        PluginOperationTarget::AddMarketplace { source } => (
            source.as_str(),
            [
                "providers.plugins.toast.adding_marketplace",
                "providers.plugins.toast.added_marketplace",
                "providers.plugins.toast.add_marketplace_failed",
            ],
        ),
        PluginOperationTarget::RemoveMarketplace { name } => (
            name.as_str(),
            [
                "providers.plugins.toast.removing_marketplace",
                "providers.plugins.toast.removed_marketplace",
                "providers.plugins.toast.remove_marketplace_failed",
            ],
        ),
    };
    keys.map(|key| crate::tr!(key, name = name).into_owned())
}

pub(super) fn present_runtime_toast(toast: &RuntimeToast) -> PresentedRuntimeToast {
    let (disposition, kind, title, detail, action) = match toast {
        RuntimeToast::ProviderUpdatesAvailable { updates } => {
            let automatic: Vec<_> = updates
                .iter()
                .filter(|update| update.automatic)
                .map(|update| update.provider)
                .collect();
            let title = if let [update] = updates.as_slice() {
                crate::tr!(
                    "providers.updates_available_one",
                    provider = update.provider.display_name(),
                    version = &update.version
                )
                .into_owned()
            } else {
                crate::tr!("providers.updates_available_many", count = updates.len()).into_owned()
            };
            return PresentedRuntimeToast {
                disposition: RuntimeToastDisposition::Push,
                kind: ToastKind::Info,
                title,
                detail: (updates.len() > 1).then(|| {
                    updates
                        .iter()
                        .map(|update| {
                            format!("{} v{}", update.provider.display_name(), update.version)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
                action: (!automatic.is_empty())
                    .then_some(RuntimeToastAction::UpdateProviders(automatic)),
                progress: None,
            };
        }
        RuntimeToast::ProviderUpdateStarted { operation, run }
        | RuntimeToast::ProviderUpdateProgress { operation, run }
        | RuntimeToast::ProviderUpdateFinished { operation, run } => {
            let finished = matches!(toast, RuntimeToast::ProviderUpdateFinished { .. });
            let failed =
                crate::i18n::join_list(run.failed.iter().map(|provider| provider.display_name()));
            let title = if finished {
                if run.failed.is_empty() {
                    crate::tr!("providers.updated_count", count = run.completed).into_owned()
                } else {
                    crate::tr!(
                        "providers.update_result",
                        updated = run.completed - run.failed.len(),
                        failed = run.failed.len(),
                        providers = &failed
                    )
                    .into_owned()
                }
            } else {
                crate::tr!(
                    "providers.update_progress",
                    provider = run
                        .current
                        .map(|provider| provider.display_name())
                        .unwrap_or_default(),
                    completed = run.completed,
                    total = run.total
                )
                .into_owned()
            };
            return PresentedRuntimeToast {
                disposition: if finished {
                    RuntimeToastDisposition::Finish(*operation)
                } else if matches!(toast, RuntimeToast::ProviderUpdateStarted { .. }) {
                    RuntimeToastDisposition::Start(*operation)
                } else {
                    RuntimeToastDisposition::Update(*operation)
                },
                kind: if !finished {
                    ToastKind::Loading
                } else if run.failed.is_empty() {
                    ToastKind::Success
                } else if run.failed.len() == run.total {
                    ToastKind::Error
                } else {
                    ToastKind::Warning
                },
                title,
                detail: (!failed.is_empty()).then(|| {
                    crate::tr!("providers.update_failures", providers = failed).into_owned()
                }),
                action: (!finished).then_some(RuntimeToastAction::CancelProviderUpdate),
                progress: (!finished)
                    .then_some(100. * run.completed as f32 / run.total.max(1) as f32),
            };
        }
        RuntimeToast::ProviderUpdateCancelled { operation, run } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Info,
            crate::tr!(
                "providers.update_stopped",
                provider = run
                    .current
                    .map(|provider| provider.display_name())
                    .unwrap_or_default()
            )
            .into_owned(),
            (!run.failed.is_empty()).then(|| {
                crate::tr!(
                    "providers.update_failures",
                    providers = crate::i18n::join_list(
                        run.failed.iter().map(|provider| provider.display_name())
                    )
                )
                .into_owned()
            }),
            None,
        ),
        RuntimeToast::GitBusy => (
            RuntimeToastDisposition::Push,
            ToastKind::Warning,
            crate::tr!("git.toast.busy").into_owned(),
            None,
            None,
        ),
        RuntimeToast::GitStarted { operation, action } => (
            RuntimeToastDisposition::Start(*operation),
            ToastKind::Loading,
            git_action_toast_titles(*action).0,
            None,
            None,
        ),
        RuntimeToast::GitSucceeded { operation, action } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Success,
            git_action_toast_titles(*action).1,
            None,
            None,
        ),
        RuntimeToast::GitFailed {
            operation,
            detail,
            retry,
        } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Error,
            crate::tr!("git.toast.failed").into_owned(),
            Some(detail.clone()),
            Some(RuntimeToastAction::RetryGit(retry.clone())),
        ),
        RuntimeToast::CommitMessageGenerated { message } => (
            RuntimeToastDisposition::Push,
            ToastKind::Info,
            crate::tr!("git.toast.message_generated").into_owned(),
            Some(message.clone()),
            None,
        ),
        RuntimeToast::CommitMessageFailed { detail } => (
            RuntimeToastDisposition::Push,
            ToastKind::Error,
            crate::tr!("git.toast.failed").into_owned(),
            Some(detail.clone()),
            None,
        ),
        RuntimeToast::AcpInstallStarted {
            operation,
            id,
            name,
        } => (
            RuntimeToastDisposition::Start(*operation),
            ToastKind::Loading,
            crate::tr!("providers.acp.installing", name = name).into_owned(),
            None,
            Some(RuntimeToastAction::CancelAcpInstall(id.clone())),
        ),
        RuntimeToast::AcpInstallCancelled { operation, name } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Info,
            crate::tr!("providers.acp.install_cancelled", name = name).into_owned(),
            None,
            None,
        ),
        RuntimeToast::AcpInstallSucceeded { operation, name } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Success,
            crate::tr!("providers.acp.installed_toast", name = name).into_owned(),
            None,
            None,
        ),
        RuntimeToast::AcpInstallFailed {
            operation,
            name,
            detail,
        } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Error,
            crate::tr!("providers.acp.install_failed", name = name).into_owned(),
            Some(detail.clone()),
            None,
        ),
        RuntimeToast::PluginOperationStarted {
            operation,
            profile_id,
            target,
        } => {
            let [title, _, _] = plugin_operation_toast_titles(target);
            (
                RuntimeToastDisposition::Start(*operation),
                ToastKind::Loading,
                title,
                None,
                Some(RuntimeToastAction::CancelPluginOperation(
                    profile_id.clone(),
                )),
            )
        }
        RuntimeToast::PluginOperationCancelled {
            operation, target, ..
        } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Info,
            crate::tr!(
                "providers.plugins.toast.cancelled",
                name = plugin_operation_name(target)
            )
            .into_owned(),
            None,
            None,
        ),
        RuntimeToast::PluginOperationSucceeded {
            operation, target, ..
        } => {
            let [_, title, _] = plugin_operation_toast_titles(target);
            (
                RuntimeToastDisposition::Finish(*operation),
                ToastKind::Success,
                title,
                None,
                None,
            )
        }
        RuntimeToast::PluginOperationFailed {
            operation,
            target,
            detail,
            ..
        } => {
            let [_, _, title] = plugin_operation_toast_titles(target);
            (
                RuntimeToastDisposition::Finish(*operation),
                ToastKind::Error,
                title,
                Some(detail.clone()),
                None,
            )
        }
        RuntimeToast::PullRequestWatch { .. } | RuntimeToast::PullRequestStack { .. } => {
            unreachable!("the shell presents pull request toasts")
        }
    };

    PresentedRuntimeToast {
        disposition,
        kind,
        title,
        detail,
        action,
        progress: None,
    }
}

#[cfg(test)]
mod tests {
    use agent::ProviderKind;

    use super::*;

    #[test]
    fn merged_provider_toast_offers_only_automatic_updates_in_both_locales() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for language in [crate::LANGUAGE_ENGLISH, crate::LANGUAGE_SIMPLIFIED_CHINESE] {
            crate::set_locale(language);
            let mut updates = vec![
                tcode_protocol::ProviderUpdateAvailable {
                    provider: ProviderKind::ClaudeCode,
                    version: "2.1.0".into(),
                    automatic: false,
                },
                tcode_protocol::ProviderUpdateAvailable {
                    provider: ProviderKind::Codex,
                    version: "1.2.0".into(),
                    automatic: false,
                },
            ];
            let manual = present_runtime_toast(&RuntimeToast::ProviderUpdatesAvailable {
                updates: updates.clone(),
            });
            assert!(manual.action.is_none());
            assert_eq!(
                manual.detail.as_deref(),
                Some("Claude Code v2.1.0\nCodex v1.2.0")
            );
            assert_eq!(
                manual.title,
                if language == crate::LANGUAGE_ENGLISH {
                    "Updates available for 2 Harnesses"
                } else {
                    "有 2 个 Harness 可更新"
                }
            );
            updates[1].automatic = true;
            let automatic =
                present_runtime_toast(&RuntimeToast::ProviderUpdatesAvailable { updates });
            assert_eq!(
                automatic.action,
                Some(RuntimeToastAction::UpdateProviders(vec![
                    ProviderKind::Codex
                ]))
            );
        }
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }
}
