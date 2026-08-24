use agent::RewindMode;
use tcode_core::git::GitAction;
use tcode_protocol::{
    GitActionRequest, MergeWorktreeFailure, NoticeSeverity, RuntimeEffect, RuntimeError,
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

pub(super) fn apply_runtime_effect(effect: &RuntimeEffect) {
    match effect {
        RuntimeEffect::ApplyLocale { language } => {
            crate::settings::apply_locale(language.as_deref());
        }
        RuntimeEffect::CopyToClipboard { .. } => {
            unreachable!("clipboard effects are applied by the app shell")
        }
    }
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
                RuntimeError::UpdateUnknown { provider } => {
                    crate::tr!("errors.update_unknown", provider = provider.display_name())
                        .into_owned()
                }
                RuntimeError::UpdateFailed { provider } => {
                    crate::tr!("errors.update_failed", provider = provider.display_name())
                        .into_owned()
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
                RuntimeError::NativeRewindBlocked => crate::tr!("chat.rewind_blocked").into_owned(),
                RuntimeError::PersistEvent { error } => {
                    crate::tr!("errors.persist_event", error = error).into_owned()
                }
                RuntimeError::WorktreeAdd { error } => {
                    crate::tr!("errors.worktree_add", error = error).into_owned()
                }
                RuntimeError::PersistSession { error } => {
                    crate::tr!("errors.persist_session", error = error).into_owned()
                }
                RuntimeError::ProcessGone => crate::tr!("errors.process_gone").into_owned(),
                RuntimeError::SteerUnsupported { agent } => {
                    crate::tr!("composer.steer_unsupported", agent = agent).into_owned()
                }
                RuntimeError::DirtyTree => crate::tr!("notice.dirty_tree").into_owned(),
                RuntimeError::ProviderStart { error } => {
                    crate::tr!("errors.provider_start", error = error).into_owned()
                }
                RuntimeError::ProviderClosed {
                    reason: Some(reason),
                } => crate::tr!("errors.provider_closed_reason", reason = reason).into_owned(),
                RuntimeError::ProviderClosed { reason: None } => {
                    crate::tr!("errors.provider_closed").into_owned()
                }
                RuntimeError::PersistSessionIndex { error } => {
                    crate::tr!("errors.persist_session_index", error = error).into_owned()
                }
                RuntimeError::ExportThread { error } => {
                    crate::tr!("errors.export_thread", error = error).into_owned()
                }
                _ => format!("Unknown runtime error: {error:?}"),
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
                RuntimeNotice::UpdateAvailable { provider, version } => crate::tr!(
                    "notice.update_available",
                    provider = provider.display_name(),
                    version = version
                )
                .into_owned(),
                RuntimeNotice::TcodeUpdateAvailable { version } => {
                    crate::tr!("notice.tcode_update_available", version = version).into_owned()
                }
                RuntimeNotice::UpdatingProvider { provider } => crate::tr!(
                    "notice.updating_provider",
                    provider = provider.display_name()
                )
                .into_owned(),
                RuntimeNotice::UpdateDone { provider } => {
                    crate::tr!("notice.update_done", provider = provider.display_name())
                        .into_owned()
                }
                RuntimeNotice::NativeRewindCompleted { mode } => match mode {
                    RewindMode::Files => crate::tr!("chat.rewind_files_done").into_owned(),
                    RewindMode::Conversation => {
                        crate::tr!("chat.rewind_conversation_done").into_owned()
                    }
                    RewindMode::FilesAndConversation => {
                        crate::tr!("chat.rewind_all_done").into_owned()
                    }
                },
                RuntimeNotice::PlanSaved { file } => {
                    crate::tr!("plan.saved_workspace", file = file).into_owned()
                }
                RuntimeNotice::SwitchedBranch { branch } => {
                    crate::tr!("notice.switched_branch", branch = branch).into_owned()
                }
                RuntimeNotice::ThreadExported { file } => {
                    crate::tr!("notice.thread_exported", file = file).into_owned()
                }
                RuntimeNotice::WorktreeSeeded {
                    copied_files,
                    skipped,
                    limit_reached,
                } => {
                    let skipped_count = skipped.len();
                    let skipped = skipped.join(", ");
                    if *limit_reached {
                        crate::tr!(
                            "notice.worktree_seeded_limit",
                            copied = copied_files,
                            skipped = skipped
                        )
                        .into_owned()
                    } else {
                        crate::tr!(
                            "notice.worktree_seeded",
                            copied = copied_files,
                            skipped = skipped_count
                        )
                        .into_owned()
                    }
                }
                RuntimeNotice::WorktreeMergedFastForward => {
                    crate::tr!("notice.worktree_merged_fast_forward").into_owned()
                }
                RuntimeNotice::WorktreeMergedCommit => {
                    crate::tr!("notice.worktree_merged_commit").into_owned()
                }
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
                _ => format!("Unknown runtime notice: {notice:?}"),
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
    Finish(RuntimeOperationId),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PresentedRuntimeToast {
    pub disposition: RuntimeToastDisposition,
    pub kind: ToastKind,
    pub title: String,
    pub detail: Option<String>,
    pub retry: Option<GitActionRequest>,
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

pub(super) fn present_runtime_toast(toast: &RuntimeToast) -> PresentedRuntimeToast {
    let (disposition, kind, title, detail, retry) = match toast {
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
            Some(retry.clone()),
        ),
        RuntimeToast::CommitMessageGenerated { message } => (
            RuntimeToastDisposition::Push,
            ToastKind::Info,
            "Generated commit message".to_string(),
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
        RuntimeToast::AcpInstallStarted { operation, name } => (
            RuntimeToastDisposition::Start(*operation),
            ToastKind::Loading,
            crate::tr!("providers.acp.installing", name = name).into_owned(),
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
        _ => (
            RuntimeToastDisposition::Push,
            ToastKind::Warning,
            "Unknown runtime notification".to_string(),
            Some(format!("{toast:?}")),
            None,
        ),
    };

    PresentedRuntimeToast {
        disposition,
        kind,
        title,
        detail,
        retry,
    }
}

#[cfg(test)]
mod tests {
    use agent::ProviderKind;

    use super::*;

    #[test]
    fn locale_effect_is_applied_only_at_ui_boundary() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        apply_runtime_effect(&RuntimeEffect::ApplyLocale {
            language: Some(crate::LANGUAGE_SIMPLIFIED_CHINESE.to_string()),
        });
        let chinese = crate::tr!("chat.new_thread").into_owned();

        apply_runtime_effect(&RuntimeEffect::ApplyLocale {
            language: Some(crate::LANGUAGE_ENGLISH.to_string()),
        });
        let english = crate::tr!("chat.new_thread").into_owned();

        assert_eq!(chinese, "新建对话");
        assert_eq!(english, "New thread");
    }

    #[test]
    fn all_runtime_events_are_presented_in_both_locales() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let errors = vec![
            RuntimeError::External("external\0diagnostic".into()),
            RuntimeError::PersistSettings { error: "x".into() },
            RuntimeError::UpdateUnknown {
                provider: ProviderKind::Codex,
            },
            RuntimeError::UpdateFailed {
                provider: ProviderKind::ClaudeCode,
            },
            RuntimeError::TerminalStart { error: "x".into() },
            RuntimeError::TerminalRestart { error: "x".into() },
            RuntimeError::PersistProject { error: "x".into() },
            RuntimeError::WorktreeRemove { error: "x".into() },
            RuntimeError::DeleteSession { error: "x".into() },
            RuntimeError::DeleteProject { error: "x".into() },
            RuntimeError::NativeRewindBlocked,
            RuntimeError::PersistEvent { error: "x".into() },
            RuntimeError::WorktreeAdd { error: "x".into() },
            RuntimeError::PersistSession { error: "x".into() },
            RuntimeError::ProcessGone,
            RuntimeError::SteerUnsupported {
                agent: "agent".into(),
            },
            RuntimeError::DirtyTree,
            RuntimeError::ProviderStart { error: "x".into() },
            RuntimeError::ProviderClosed {
                reason: Some("reason".into()),
            },
            RuntimeError::ProviderClosed { reason: None },
            RuntimeError::PersistSessionIndex { error: "x".into() },
            RuntimeError::ProviderMessage("provider-error\0diagnostic".into()),
            RuntimeError::ExportThread { error: "x".into() },
        ];
        let notices = vec![
            RuntimeNotice::ProviderMessage("provider-warning\0diagnostic".into()),
            RuntimeNotice::UpdateAvailable {
                provider: ProviderKind::Codex,
                version: "1.2.3".into(),
            },
            RuntimeNotice::TcodeUpdateAvailable {
                version: "1.2.3".into(),
            },
            RuntimeNotice::UpdatingProvider {
                provider: ProviderKind::ClaudeCode,
            },
            RuntimeNotice::UpdateDone {
                provider: ProviderKind::Acp,
            },
            RuntimeNotice::NativeRewindCompleted {
                mode: RewindMode::FilesAndConversation,
            },
            RuntimeNotice::PlanSaved {
                file: "plan.md".into(),
            },
            RuntimeNotice::SwitchedBranch {
                branch: "feature".into(),
            },
            RuntimeNotice::ThreadExported {
                file: "thread.md".into(),
            },
            RuntimeNotice::WorktreeSeeded {
                copied_files: 2,
                skipped: vec!["missing.file".into()],
                limit_reached: false,
            },
            RuntimeNotice::WorktreeSeeded {
                copied_files: 1,
                skipped: vec!["large.bin".into()],
                limit_reached: true,
            },
            RuntimeNotice::WorktreeMergedFastForward,
            RuntimeNotice::WorktreeMergedCommit,
        ];
        let warnings = [
            MergeWorktreeFailure::Missing,
            MergeWorktreeFailure::DirtyWorktree,
            MergeWorktreeFailure::DestinationDetached,
            MergeWorktreeFailure::DirtyDestination,
            MergeWorktreeFailure::DivergedConflict,
            MergeWorktreeFailure::Git,
        ]
        .map(|reason| RuntimeNotice::WorktreeMergeFailed {
            reason,
            detail: Some("git detail".into()),
        });
        let retry = GitActionRequest {
            action: GitAction::CommitPush,
            message: Some("exact message".into()),
            included: Some(vec!["a.rs".into(), "b.rs".into()]),
            feature_branch: Some("feature/exact".into()),
        };
        let toasts = vec![
            RuntimeToast::GitBusy,
            RuntimeToast::GitStarted {
                operation: RuntimeOperationId(1),
                action: GitAction::Commit,
            },
            RuntimeToast::GitSucceeded {
                operation: RuntimeOperationId(1),
                action: GitAction::Commit,
            },
            RuntimeToast::GitFailed {
                operation: RuntimeOperationId(1),
                detail: "git raw\0detail".into(),
                retry: retry.clone(),
            },
            RuntimeToast::CommitMessageGenerated {
                message: "generated raw\0message".into(),
            },
            RuntimeToast::CommitMessageFailed {
                detail: "commit raw\0detail".into(),
            },
            RuntimeToast::AcpInstallStarted {
                operation: RuntimeOperationId(2),
                name: "Agent".into(),
            },
            RuntimeToast::AcpInstallSucceeded {
                operation: RuntimeOperationId(2),
                name: "Agent".into(),
            },
            RuntimeToast::AcpInstallFailed {
                operation: RuntimeOperationId(2),
                name: "Agent".into(),
                detail: "acp raw\0detail".into(),
            },
        ];

        for locale in [crate::LANGUAGE_ENGLISH, crate::LANGUAGE_SIMPLIFIED_CHINESE] {
            crate::set_locale(locale);
            for error in &errors {
                let presented = present_runtime_event(&RuntimeEvent::Error(error.clone()));
                assert_eq!(presented.severity, RuntimeEventSeverity::Error);
                assert!(!presented.message.is_empty());
            }
            for notice in &notices {
                let presented = present_runtime_event(&RuntimeEvent::Notice(notice.clone()));
                assert!(!presented.message.is_empty());
            }
            for warning in &warnings {
                let presented = present_runtime_event(&RuntimeEvent::Notice(warning.clone()));
                assert!(!presented.message.is_empty());
            }
            for toast in &toasts {
                let presented = present_runtime_toast(toast);
                assert!(!presented.title.is_empty());
                match toast {
                    RuntimeToast::GitStarted { operation, .. }
                    | RuntimeToast::AcpInstallStarted { operation, .. } => {
                        assert_eq!(
                            presented.disposition,
                            RuntimeToastDisposition::Start(*operation)
                        );
                        assert_eq!(presented.kind, ToastKind::Loading);
                    }
                    RuntimeToast::GitSucceeded { operation, .. }
                    | RuntimeToast::AcpInstallSucceeded { operation, .. } => {
                        assert_eq!(
                            presented.disposition,
                            RuntimeToastDisposition::Finish(*operation)
                        );
                        assert_eq!(presented.kind, ToastKind::Success);
                    }
                    RuntimeToast::GitFailed { operation, .. }
                    | RuntimeToast::AcpInstallFailed { operation, .. } => {
                        assert_eq!(
                            presented.disposition,
                            RuntimeToastDisposition::Finish(*operation)
                        );
                        assert_eq!(presented.kind, ToastKind::Error);
                    }
                    RuntimeToast::GitBusy => {
                        assert_eq!(presented.disposition, RuntimeToastDisposition::Push);
                        assert_eq!(presented.kind, ToastKind::Warning);
                    }
                    RuntimeToast::CommitMessageGenerated { .. } => {
                        assert_eq!(presented.disposition, RuntimeToastDisposition::Push);
                        assert_eq!(presented.kind, ToastKind::Info);
                    }
                    RuntimeToast::CommitMessageFailed { .. } => {
                        assert_eq!(presented.disposition, RuntimeToastDisposition::Push);
                        assert_eq!(presented.kind, ToastKind::Error);
                    }
                    _ => {}
                }
            }

            let title_pairs = [
                (
                    GitAction::Commit,
                    crate::tr!("git.toast.committing").into_owned(),
                    crate::tr!("git.toast.committed").into_owned(),
                ),
                (
                    GitAction::CommitPush,
                    crate::tr!("git.toast.committing_pushing").into_owned(),
                    crate::tr!("git.toast.committed_pushed").into_owned(),
                ),
                (
                    GitAction::Push,
                    crate::tr!("git.toast.pushing").into_owned(),
                    crate::tr!("git.toast.pushed").into_owned(),
                ),
                (
                    GitAction::Pull,
                    crate::tr!("git.toast.pulling").into_owned(),
                    crate::tr!("git.toast.pulled").into_owned(),
                ),
                (
                    GitAction::PublishBranch,
                    crate::tr!("git.toast.publishing").into_owned(),
                    crate::tr!("git.toast.published").into_owned(),
                ),
                (
                    GitAction::InitializeGit,
                    crate::tr!("git.toast.initializing").into_owned(),
                    crate::tr!("git.toast.initialized").into_owned(),
                ),
            ];
            for (index, (action, started, succeeded)) in title_pairs.into_iter().enumerate() {
                let operation = RuntimeOperationId(index as u64 + 10);
                let start = present_runtime_toast(&RuntimeToast::GitStarted { operation, action });
                let success =
                    present_runtime_toast(&RuntimeToast::GitSucceeded { operation, action });
                assert_eq!(start.title, started);
                assert_eq!(success.title, succeeded);
                assert_eq!(start.disposition, RuntimeToastDisposition::Start(operation));
                assert_eq!(
                    success.disposition,
                    RuntimeToastDisposition::Finish(operation)
                );
            }

            let failed = present_runtime_toast(&RuntimeToast::GitFailed {
                operation: RuntimeOperationId(1),
                detail: "git raw\0detail".into(),
                retry: retry.clone(),
            });
            assert_eq!(failed.detail.as_deref(), Some("git raw\0detail"));
            assert_eq!(failed.retry.as_ref(), Some(&retry));
            assert_eq!(
                present_runtime_toast(&RuntimeToast::CommitMessageGenerated {
                    message: "generated raw\0message".into(),
                })
                .detail
                .as_deref(),
                Some("generated raw\0message")
            );
            assert_eq!(
                present_runtime_toast(&RuntimeToast::CommitMessageFailed {
                    detail: "commit raw\0detail".into(),
                })
                .detail
                .as_deref(),
                Some("commit raw\0detail")
            );
            assert_eq!(
                present_runtime_toast(&RuntimeToast::AcpInstallFailed {
                    operation: RuntimeOperationId(2),
                    name: "Agent".into(),
                    detail: "acp raw\0detail".into(),
                })
                .detail
                .as_deref(),
                Some("acp raw\0detail")
            );

            assert_eq!(
                present_runtime_event(&RuntimeEvent::Error(RuntimeError::External(
                    "external\0diagnostic".into()
                )))
                .message,
                "external\0diagnostic"
            );
            assert_eq!(
                present_runtime_event(&RuntimeEvent::Error(RuntimeError::ProviderMessage(
                    "provider-error\0diagnostic".into()
                )))
                .message,
                "provider-error\0diagnostic"
            );
            assert_eq!(
                present_runtime_event(&RuntimeEvent::Notice(RuntimeNotice::ProviderMessage(
                    "provider-warning\0diagnostic".into()
                )))
                .message,
                "provider-warning\0diagnostic"
            );
        }

        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }

    #[test]
    fn representative_notice_severities_are_presented() {
        let warning = present_runtime_event(&RuntimeEvent::Notice(RuntimeNotice::ProviderMessage(
            "pi MCP tools unavailable".into(),
        )));
        let success = present_runtime_event(&RuntimeEvent::Notice(RuntimeNotice::UpdateDone {
            provider: ProviderKind::Codex,
        }));

        assert_eq!(warning.severity, RuntimeEventSeverity::Warning);
        assert_eq!(success.severity, RuntimeEventSeverity::Success);
    }
}
