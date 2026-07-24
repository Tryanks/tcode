use agent::RewindMode;
use tcode_core::git::GitAction;
use tcode_runtime::event::{
    GitActionRequest, RuntimeEffect, RuntimeError, RuntimeEvent, RuntimeNotice, RuntimeOperationId,
    RuntimeToast,
};

use crate::toast::ToastKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RuntimeEventSeverity {
    Error,
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
                    tcode_i18n::tr!("errors.persist_settings", error = error).into_owned()
                }
                RuntimeError::UpdateUnknown { provider } => {
                    tcode_i18n::tr!("errors.update_unknown", provider = provider.display_name())
                        .into_owned()
                }
                RuntimeError::UpdateFailed { provider } => {
                    tcode_i18n::tr!("errors.update_failed", provider = provider.display_name())
                        .into_owned()
                }
                RuntimeError::TerminalStart { error } => {
                    tcode_i18n::tr!("errors.terminal_start", error = error).into_owned()
                }
                RuntimeError::TerminalRestart { error } => {
                    tcode_i18n::tr!("errors.terminal_restart", error = error).into_owned()
                }
                RuntimeError::PersistProject { error } => {
                    tcode_i18n::tr!("errors.persist_project", error = error).into_owned()
                }
                RuntimeError::WorktreeRemove { error } => {
                    tcode_i18n::tr!("errors.worktree_remove", error = error).into_owned()
                }
                RuntimeError::DeleteSession { error } => {
                    tcode_i18n::tr!("errors.delete_session", error = error).into_owned()
                }
                RuntimeError::DeleteProject { error } => {
                    tcode_i18n::tr!("errors.delete_project", error = error).into_owned()
                }
                RuntimeError::NativeRewindBlocked => {
                    tcode_i18n::tr!("chat.rewind_blocked").into_owned()
                }
                RuntimeError::PersistEvent { error } => {
                    tcode_i18n::tr!("errors.persist_event", error = error).into_owned()
                }
                RuntimeError::WorktreeAdd { error } => {
                    tcode_i18n::tr!("errors.worktree_add", error = error).into_owned()
                }
                RuntimeError::PersistSession { error } => {
                    tcode_i18n::tr!("errors.persist_session", error = error).into_owned()
                }
                RuntimeError::ProcessGone => tcode_i18n::tr!("errors.process_gone").into_owned(),
                RuntimeError::SteerUnsupported { agent } => {
                    tcode_i18n::tr!("composer.steer_unsupported", agent = agent).into_owned()
                }
                RuntimeError::DirtyTree => tcode_i18n::tr!("notice.dirty_tree").into_owned(),
                RuntimeError::ProviderStart { error } => {
                    tcode_i18n::tr!("errors.provider_start", error = error).into_owned()
                }
                RuntimeError::ProviderClosed {
                    reason: Some(reason),
                } => tcode_i18n::tr!("errors.provider_closed_reason", reason = reason).into_owned(),
                RuntimeError::ProviderClosed { reason: None } => {
                    tcode_i18n::tr!("errors.provider_closed").into_owned()
                }
                RuntimeError::PersistSessionIndex { error } => {
                    tcode_i18n::tr!("errors.persist_session_index", error = error).into_owned()
                }
            };
            (RuntimeEventSeverity::Error, message)
        }
        RuntimeEvent::Notice(notice) => {
            let message = match notice {
                RuntimeNotice::ProviderMessage(message) => message.clone(),
                RuntimeNotice::UpdateAvailable { provider, version } => tcode_i18n::tr!(
                    "notice.update_available",
                    provider = provider.display_name(),
                    version = version
                )
                .into_owned(),
                RuntimeNotice::UpdatingProvider { provider } => tcode_i18n::tr!(
                    "notice.updating_provider",
                    provider = provider.display_name()
                )
                .into_owned(),
                RuntimeNotice::UpdateDone { provider } => {
                    tcode_i18n::tr!("notice.update_done", provider = provider.display_name())
                        .into_owned()
                }
                RuntimeNotice::NativeRewindCompleted { mode } => match mode {
                    RewindMode::Files => tcode_i18n::tr!("chat.rewind_files_done").into_owned(),
                    RewindMode::Conversation => {
                        tcode_i18n::tr!("chat.rewind_conversation_done").into_owned()
                    }
                    RewindMode::FilesAndConversation => {
                        tcode_i18n::tr!("chat.rewind_all_done").into_owned()
                    }
                },
                RuntimeNotice::PlanSaved { file } => {
                    tcode_i18n::tr!("plan.saved_workspace", file = file).into_owned()
                }
                RuntimeNotice::SwitchedBranch { branch } => {
                    tcode_i18n::tr!("notice.switched_branch", branch = branch).into_owned()
                }
                RuntimeNotice::ScheduledFired { parked: false } => {
                    tcode_i18n::tr!("notice.scheduled_sent").into_owned()
                }
                RuntimeNotice::ScheduledFired { parked: true } => {
                    tcode_i18n::tr!("notice.scheduled_queued").into_owned()
                }
                RuntimeNotice::ScheduledDropped => {
                    tcode_i18n::tr!("notice.scheduled_dropped").into_owned()
                }
            };
            (RuntimeEventSeverity::Success, message)
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
            tcode_i18n::tr!("git.toast.committing").into_owned(),
            tcode_i18n::tr!("git.toast.committed").into_owned(),
        ),
        GitAction::CommitPush => (
            tcode_i18n::tr!("git.toast.committing_pushing").into_owned(),
            tcode_i18n::tr!("git.toast.committed_pushed").into_owned(),
        ),
        GitAction::Push => (
            tcode_i18n::tr!("git.toast.pushing").into_owned(),
            tcode_i18n::tr!("git.toast.pushed").into_owned(),
        ),
        GitAction::Pull => (
            tcode_i18n::tr!("git.toast.pulling").into_owned(),
            tcode_i18n::tr!("git.toast.pulled").into_owned(),
        ),
        GitAction::PublishBranch => (
            tcode_i18n::tr!("git.toast.publishing").into_owned(),
            tcode_i18n::tr!("git.toast.published").into_owned(),
        ),
        GitAction::InitializeGit => (
            tcode_i18n::tr!("git.toast.initializing").into_owned(),
            tcode_i18n::tr!("git.toast.initialized").into_owned(),
        ),
    }
}

pub(super) fn present_runtime_toast(toast: &RuntimeToast) -> PresentedRuntimeToast {
    let (disposition, kind, title, detail, retry) = match toast {
        RuntimeToast::GitBusy => (
            RuntimeToastDisposition::Push,
            ToastKind::Warning,
            tcode_i18n::tr!("git.toast.busy").into_owned(),
            None,
            None,
        ),
        RuntimeToast::GitStarted { operation, action } => (
            RuntimeToastDisposition::Start(*operation),
            ToastKind::Loading { progress: None },
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
            tcode_i18n::tr!("git.toast.failed").into_owned(),
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
            tcode_i18n::tr!("git.toast.failed").into_owned(),
            Some(detail.clone()),
            None,
        ),
        RuntimeToast::AcpInstallStarted { operation, name } => (
            RuntimeToastDisposition::Start(*operation),
            ToastKind::Loading { progress: None },
            tcode_i18n::tr!("providers.acp.installing", name = name).into_owned(),
            None,
            None,
        ),
        RuntimeToast::AcpInstallSucceeded { operation, name } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Success,
            tcode_i18n::tr!("providers.acp.installed_toast", name = name).into_owned(),
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
            tcode_i18n::tr!("providers.acp.install_failed", name = name).into_owned(),
            Some(detail.clone()),
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
            language: Some(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE.to_string()),
        });
        let chinese = tcode_i18n::tr!("chat.new_thread").into_owned();

        apply_runtime_effect(&RuntimeEffect::ApplyLocale {
            language: Some(tcode_i18n::LANGUAGE_ENGLISH.to_string()),
        });
        let english = tcode_i18n::tr!("chat.new_thread").into_owned();

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
        ];
        let notices = vec![
            RuntimeNotice::ProviderMessage("provider-warning\0diagnostic".into()),
            RuntimeNotice::UpdateAvailable {
                provider: ProviderKind::Codex,
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
        ];
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

        for locale in [
            tcode_i18n::LANGUAGE_ENGLISH,
            tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE,
        ] {
            tcode_i18n::set_locale(locale);
            for error in &errors {
                let presented = present_runtime_event(&RuntimeEvent::Error(error.clone()));
                assert_eq!(presented.severity, RuntimeEventSeverity::Error);
                assert!(!presented.message.is_empty());
            }
            for notice in &notices {
                let presented = present_runtime_event(&RuntimeEvent::Notice(notice.clone()));
                assert_eq!(presented.severity, RuntimeEventSeverity::Success);
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
                        assert_eq!(presented.kind, ToastKind::Loading { progress: None });
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
                }
            }

            let title_pairs = [
                (
                    GitAction::Commit,
                    tcode_i18n::tr!("git.toast.committing").into_owned(),
                    tcode_i18n::tr!("git.toast.committed").into_owned(),
                ),
                (
                    GitAction::CommitPush,
                    tcode_i18n::tr!("git.toast.committing_pushing").into_owned(),
                    tcode_i18n::tr!("git.toast.committed_pushed").into_owned(),
                ),
                (
                    GitAction::Push,
                    tcode_i18n::tr!("git.toast.pushing").into_owned(),
                    tcode_i18n::tr!("git.toast.pushed").into_owned(),
                ),
                (
                    GitAction::Pull,
                    tcode_i18n::tr!("git.toast.pulling").into_owned(),
                    tcode_i18n::tr!("git.toast.pulled").into_owned(),
                ),
                (
                    GitAction::PublishBranch,
                    tcode_i18n::tr!("git.toast.publishing").into_owned(),
                    tcode_i18n::tr!("git.toast.published").into_owned(),
                ),
                (
                    GitAction::InitializeGit,
                    tcode_i18n::tr!("git.toast.initializing").into_owned(),
                    tcode_i18n::tr!("git.toast.initialized").into_owned(),
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

        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
    }
}
