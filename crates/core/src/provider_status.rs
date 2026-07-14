//! Pure provider status data and semantic card-summary derivation.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatusKind {
    Ready,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    Authenticated,
    Unauthenticated,
    Unknown,
}

/// A presentation-free explanation for a provider probe outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProbeDiagnostic {
    MissingCli,
    FailedCli,
    Unauthenticated,
    IndeterminateAuth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAuth {
    pub status: AuthStatus,
    pub label: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderSnapshot {
    pub checked_at: Option<u64>,
    pub installed: bool,
    pub version: Option<String>,
    pub status: Option<ProviderStatusKind>,
    pub auth: Option<ProviderAuth>,
    pub diagnostic: Option<ProviderProbeDiagnostic>,
    pub message: Option<String>,
    pub checking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusDot {
    Success,
    Warning,
    Error,
    Amber,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSummary {
    pub dot: StatusDot,
    pub headline: ProviderSummaryHeadline,
    pub detail: ProviderSummaryDetail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSummaryHeadline {
    Checking,
    Disabled,
    NotFound,
    Authenticated {
        label: Option<String>,
        email: Option<String>,
    },
    NotAuthenticated,
    NeedsAttention,
    Unavailable,
    Available,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSummaryDetail {
    None,
    Message(String),
    Diagnostic(ProviderProbeDiagnostic),
    Checking,
    Disabled,
    NotFound,
    NeedsAttention,
    Unavailable,
    Available,
}

pub fn derive_summary(snapshot: Option<&ProviderSnapshot>, enabled: bool) -> ProviderSummary {
    let Some(snapshot) = snapshot else {
        return ProviderSummary {
            dot: StatusDot::Warning,
            headline: ProviderSummaryHeadline::Checking,
            detail: ProviderSummaryDetail::Checking,
        };
    };
    let message = snapshot
        .message
        .as_deref()
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_string);
    let detail_or = |fallback| match message.clone() {
        Some(message) => ProviderSummaryDetail::Message(message),
        None => snapshot
            .diagnostic
            .map(ProviderSummaryDetail::Diagnostic)
            .unwrap_or(fallback),
    };

    let Some(status) = snapshot.status else {
        return ProviderSummary {
            dot: StatusDot::Warning,
            headline: ProviderSummaryHeadline::Checking,
            detail: ProviderSummaryDetail::Checking,
        };
    };
    if !enabled {
        return ProviderSummary {
            dot: StatusDot::Amber,
            headline: ProviderSummaryHeadline::Disabled,
            detail: detail_or(ProviderSummaryDetail::Disabled),
        };
    }
    if !snapshot.installed {
        return ProviderSummary {
            dot: StatusDot::Error,
            headline: ProviderSummaryHeadline::NotFound,
            detail: detail_or(ProviderSummaryDetail::NotFound),
        };
    }

    let dot = match status {
        ProviderStatusKind::Ready => StatusDot::Success,
        ProviderStatusKind::Warning => StatusDot::Warning,
        ProviderStatusKind::Error => StatusDot::Error,
    };
    let auth = snapshot.auth.as_ref();
    let email = auth
        .and_then(|auth| auth.email.clone())
        .filter(|email| !email.is_empty());
    let label = auth
        .and_then(|auth| auth.label.clone())
        .filter(|label| !label.is_empty());

    match auth.map(|auth| auth.status) {
        Some(AuthStatus::Authenticated) => ProviderSummary {
            dot,
            headline: ProviderSummaryHeadline::Authenticated { label, email },
            detail: detail_or(ProviderSummaryDetail::None),
        },
        Some(AuthStatus::Unauthenticated) => ProviderSummary {
            dot,
            headline: ProviderSummaryHeadline::NotAuthenticated,
            detail: detail_or(ProviderSummaryDetail::None),
        },
        _ => match status {
            ProviderStatusKind::Warning => ProviderSummary {
                dot,
                headline: ProviderSummaryHeadline::NeedsAttention,
                detail: detail_or(ProviderSummaryDetail::NeedsAttention),
            },
            ProviderStatusKind::Error => ProviderSummary {
                dot,
                headline: ProviderSummaryHeadline::Unavailable,
                detail: detail_or(ProviderSummaryDetail::Unavailable),
            },
            ProviderStatusKind::Ready => ProviderSummary {
                dot,
                headline: ProviderSummaryHeadline::Available,
                detail: detail_or(ProviderSummaryDetail::Available),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ProviderSnapshot {
        ProviderSnapshot {
            installed: true,
            status: Some(ProviderStatusKind::Ready),
            ..Default::default()
        }
    }

    #[test]
    fn status_summary_derivation_table() {
        assert_eq!(
            derive_summary(None, true),
            ProviderSummary {
                dot: StatusDot::Warning,
                headline: ProviderSummaryHeadline::Checking,
                detail: ProviderSummaryDetail::Checking,
            }
        );
        assert_eq!(
            derive_summary(Some(&snapshot()), false),
            ProviderSummary {
                dot: StatusDot::Amber,
                headline: ProviderSummaryHeadline::Disabled,
                detail: ProviderSummaryDetail::Disabled,
            }
        );
        let missing = ProviderSnapshot {
            status: Some(ProviderStatusKind::Error),
            ..Default::default()
        };
        assert_eq!(
            derive_summary(Some(&missing), true).headline,
            ProviderSummaryHeadline::NotFound
        );

        let authed = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: Some("Claude Max Subscription".into()),
                email: Some("dev@example.com".into()),
            }),
            ..snapshot()
        };
        assert_eq!(
            derive_summary(Some(&authed), true).headline,
            ProviderSummaryHeadline::Authenticated {
                label: Some("Claude Max Subscription".into()),
                email: Some("dev@example.com".into())
            }
        );
        let blank_auth = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: Some(String::new()),
                email: Some(String::new()),
            }),
            ..snapshot()
        };
        assert_eq!(
            derive_summary(Some(&blank_auth), true).headline,
            ProviderSummaryHeadline::Authenticated {
                label: None,
                email: None
            }
        );

        let signed_out = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Unauthenticated,
                label: None,
                email: None,
            }),
            message: Some("  Run login  ".into()),
            ..snapshot()
        };
        let signed_out = derive_summary(Some(&signed_out), true);
        assert_eq!(
            signed_out.headline,
            ProviderSummaryHeadline::NotAuthenticated
        );
        assert_eq!(
            signed_out.detail,
            ProviderSummaryDetail::Message("Run login".into())
        );

        for (status, headline, detail, dot) in [
            (
                ProviderStatusKind::Warning,
                ProviderSummaryHeadline::NeedsAttention,
                ProviderSummaryDetail::NeedsAttention,
                StatusDot::Warning,
            ),
            (
                ProviderStatusKind::Error,
                ProviderSummaryHeadline::Unavailable,
                ProviderSummaryDetail::Unavailable,
                StatusDot::Error,
            ),
            (
                ProviderStatusKind::Ready,
                ProviderSummaryHeadline::Available,
                ProviderSummaryDetail::Available,
                StatusDot::Success,
            ),
        ] {
            let value = ProviderSnapshot {
                status: Some(status),
                ..snapshot()
            };
            assert_eq!(
                derive_summary(Some(&value), true),
                ProviderSummary {
                    dot,
                    headline,
                    detail
                }
            );
        }

        let placeholder = ProviderSnapshot {
            checking: true,
            message: Some("ignored".into()),
            ..Default::default()
        };
        assert_eq!(
            derive_summary(Some(&placeholder), false).headline,
            ProviderSummaryHeadline::Checking
        );
    }

    #[test]
    fn messages_override_only_the_existing_fallback_details() {
        for value in [
            ProviderSnapshot {
                message: Some(" custom ".into()),
                ..snapshot()
            },
            ProviderSnapshot {
                message: Some(" custom ".into()),
                status: Some(ProviderStatusKind::Warning),
                ..snapshot()
            },
            ProviderSnapshot {
                message: Some(" custom ".into()),
                status: Some(ProviderStatusKind::Error),
                ..snapshot()
            },
        ] {
            assert_eq!(
                derive_summary(Some(&value), true).detail,
                ProviderSummaryDetail::Message("custom".into())
            );
        }
        let disabled = ProviderSnapshot {
            message: Some(" custom ".into()),
            ..snapshot()
        };
        assert_eq!(
            derive_summary(Some(&disabled), false).detail,
            ProviderSummaryDetail::Message("custom".into())
        );
        let missing = ProviderSnapshot {
            status: Some(ProviderStatusKind::Error),
            message: Some(" custom ".into()),
            ..Default::default()
        };
        assert_eq!(
            derive_summary(Some(&missing), true).detail,
            ProviderSummaryDetail::Message("custom".into())
        );
        let blank = ProviderSnapshot {
            message: Some("  ".into()),
            ..snapshot()
        };
        assert_eq!(
            derive_summary(Some(&blank), true).detail,
            ProviderSummaryDetail::Available
        );
    }

    #[test]
    fn diagnostics_stay_semantic_and_messages_take_precedence() {
        for diagnostic in [
            ProviderProbeDiagnostic::MissingCli,
            ProviderProbeDiagnostic::FailedCli,
            ProviderProbeDiagnostic::Unauthenticated,
            ProviderProbeDiagnostic::IndeterminateAuth,
        ] {
            let value = ProviderSnapshot {
                diagnostic: Some(diagnostic),
                ..snapshot()
            };
            assert_eq!(
                derive_summary(Some(&value), true).detail,
                ProviderSummaryDetail::Diagnostic(diagnostic)
            );
        }

        let message = ProviderSnapshot {
            diagnostic: Some(ProviderProbeDiagnostic::FailedCli),
            message: Some("  precise message  ".into()),
            ..snapshot()
        };
        assert_eq!(
            derive_summary(Some(&message), true).detail,
            ProviderSummaryDetail::Message("precise message".into())
        );

        let blank_message = ProviderSnapshot {
            diagnostic: Some(ProviderProbeDiagnostic::Unauthenticated),
            message: Some("  ".into()),
            ..snapshot()
        };
        assert_eq!(
            derive_summary(Some(&blank_message), true).detail,
            ProviderSummaryDetail::Diagnostic(ProviderProbeDiagnostic::Unauthenticated)
        );

        let placeholder = ProviderSnapshot {
            diagnostic: Some(ProviderProbeDiagnostic::IndeterminateAuth),
            message: Some("ignored".into()),
            ..Default::default()
        };
        assert_eq!(
            derive_summary(Some(&placeholder), true).detail,
            ProviderSummaryDetail::Checking
        );
    }
}
