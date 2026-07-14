//! Provider status presentation with core-owned semantics.
//!
//! Redaction, localized copy, and probe-message presentation remain app-owned
//! here; pure summary derivation lives in `tcode_core` and auth JSON parsing
//! lives in the services crate.

use agent::ProviderKind;

use crate::settings::provider_label;

use tcode_core::provider_status::{ProviderProbeDiagnostic, derive_summary};
pub use tcode_core::provider_status::{
    ProviderSnapshot, ProviderSummary, ProviderSummaryDetail, ProviderSummaryHeadline, StatusDot,
};

#[cfg(test)]
use tcode_core::provider_status::{AuthStatus, ProviderAuth, ProviderStatusKind};

/// The derived card summary: dot + headline + detail (+ the email that the
/// headline embeds, so the card can render it with the reveal control).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSummary {
    pub dot: StatusDot,
    /// The headline, with `{email}` standing in for a revealable email when
    /// `email` is `Some` (the card splits on it).
    pub headline: String,
    pub detail: String,
    pub email: Option<String>,
}

/// Placeholder the headline uses where a revealable email goes.
pub const EMAIL_SLOT: &str = "{email}";

/// Derive the status dot + exact headline/detail copy for a provider card.
///
/// This is a direct port of T3's `providerStatus.ts` derivation (spec §2),
/// including the authenticated-with-email variants.
pub fn summarize(
    provider: ProviderKind,
    snapshot: Option<&ProviderSnapshot>,
    enabled: bool,
) -> StatusSummary {
    let t = |key: &str| tcode_i18n::tr!(key).into_owned();
    let summary: ProviderSummary = derive_summary(snapshot, enabled);
    let (headline, email) = match summary.headline {
        ProviderSummaryHeadline::Checking => (t("providers.status.checking"), None),
        ProviderSummaryHeadline::Disabled => (t("providers.status.disabled"), None),
        ProviderSummaryHeadline::NotFound => (t("providers.status.not_found"), None),
        ProviderSummaryHeadline::Authenticated { label, email } => {
            let headline = match (&email, &label) {
                (Some(_), Some(label)) => tcode_i18n::tr!(
                    "providers.status.authenticated_as_with_label",
                    email = EMAIL_SLOT,
                    label = label
                )
                .into_owned(),
                (Some(_), None) => {
                    tcode_i18n::tr!("providers.status.authenticated_as", email = EMAIL_SLOT)
                        .into_owned()
                }
                (None, Some(label)) => {
                    tcode_i18n::tr!("providers.status.authenticated_with_label", label = label)
                        .into_owned()
                }
                (None, None) => t("providers.status.authenticated"),
            };
            (headline, email)
        }
        ProviderSummaryHeadline::NotAuthenticated => {
            (t("providers.status.not_authenticated"), None)
        }
        ProviderSummaryHeadline::NeedsAttention => (t("providers.status.needs_attention"), None),
        ProviderSummaryHeadline::Unavailable => (t("providers.status.unavailable"), None),
        ProviderSummaryHeadline::Available => (t("providers.status.available"), None),
    };
    let probe_diagnostic_message = |diagnostic| probe_diagnostic_message(provider, diagnostic);
    let detail = match summary.detail {
        ProviderSummaryDetail::None => String::new(),
        ProviderSummaryDetail::Message(message) => message,
        ProviderSummaryDetail::Diagnostic(diagnostic) => probe_diagnostic_message(diagnostic),
        ProviderSummaryDetail::Checking => t("providers.status.checking_detail"),
        ProviderSummaryDetail::Disabled => t("providers.status.disabled_detail"),
        ProviderSummaryDetail::NotFound => t("providers.status.not_found_detail"),
        ProviderSummaryDetail::NeedsAttention => t("providers.status.needs_attention_detail"),
        ProviderSummaryDetail::Unavailable => t("providers.status.unavailable_detail"),
        ProviderSummaryDetail::Available => t("providers.status.available_detail"),
    };
    StatusSummary {
        dot: summary.dot,
        headline,
        detail,
        email,
    }
}

/// Redact an email for the collapsed presentation: keep the first character of
/// the local part and the TLD, mask everything else (`t••••@•••••.com`).
pub fn redact_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return "•".repeat(email.chars().count().max(1));
    };
    let head: String = local.chars().take(1).collect();
    let local_mask = "•".repeat(local.chars().count().saturating_sub(1).max(1));
    let domain_mask = match domain.rsplit_once('.') {
        Some((name, tld)) => format!("{}.{tld}", "•".repeat(name.chars().count().max(1))),
        None => "•".repeat(domain.chars().count().max(1)),
    };
    format!("{head}{local_mask}@{domain_mask}")
}

/// The exact per-provider probe messages T3 shows for a missing CLI (§3).
pub fn missing_cli_message(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => tcode_i18n::tr!("providers.probe.codex_missing").into_owned(),
        ProviderKind::ClaudeCode => tcode_i18n::tr!("providers.probe.claude_missing").into_owned(),
        ProviderKind::Acp => String::new(),
    }
}

/// The message shown when the CLI is present but its version command failed.
pub fn failed_cli_message(provider: ProviderKind) -> String {
    tcode_i18n::tr!(
        "providers.probe.failed_run",
        provider = provider_label(provider)
    )
    .into_owned()
}

/// The message shown when the CLI ran but its auth state could not be read.
pub fn indeterminate_auth_message(provider: ProviderKind) -> String {
    tcode_i18n::tr!(
        "providers.probe.indeterminate_auth",
        provider = provider_label(provider)
    )
    .into_owned()
}

/// The message shown when the CLI is signed out.
pub fn unauthenticated_message(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => tcode_i18n::tr!("providers.probe.codex_signed_out").into_owned(),
        ProviderKind::ClaudeCode => {
            tcode_i18n::tr!("providers.probe.claude_signed_out").into_owned()
        }
        ProviderKind::Acp => String::new(),
    }
}

/// Translate a core-owned semantic probe diagnostic into app-localized copy.
pub fn probe_diagnostic_message(
    provider: ProviderKind,
    diagnostic: ProviderProbeDiagnostic,
) -> String {
    match diagnostic {
        ProviderProbeDiagnostic::MissingCli => missing_cli_message(provider),
        ProviderProbeDiagnostic::FailedCli => failed_cli_message(provider),
        ProviderProbeDiagnostic::Unauthenticated => unauthenticated_message(provider),
        ProviderProbeDiagnostic::IndeterminateAuth => indeterminate_auth_message(provider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summarize(snapshot: Option<&ProviderSnapshot>, enabled: bool) -> StatusSummary {
        super::summarize(ProviderKind::Codex, snapshot, enabled)
    }

    fn snapshot() -> ProviderSnapshot {
        ProviderSnapshot {
            installed: true,
            status: Some(ProviderStatusKind::Ready),
            ..ProviderSnapshot::default()
        }
    }

    /// T3 §2's derivation table, case by case.
    #[test]
    fn status_summary_derivation_table() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // No snapshot yet → warning dot + "checking" copy.
        let s = summarize(None, true);
        assert_eq!(s.dot, StatusDot::Warning);
        assert_eq!(s.headline, "Checking provider status");
        assert_eq!(
            s.detail,
            "Waiting for the server to report installation and authentication details."
        );

        // Disabled wins over everything else, with a server message when present.
        let s = summarize(Some(&snapshot()), false);
        assert_eq!(s.dot, StatusDot::Amber);
        assert_eq!(s.headline, "Disabled");
        assert_eq!(
            s.detail,
            "This provider is installed but disabled for new sessions in tcode."
        );

        // Not installed.
        let missing = ProviderSnapshot {
            installed: false,
            status: Some(ProviderStatusKind::Error),
            ..ProviderSnapshot::default()
        };
        let s = summarize(Some(&missing), true);
        assert_eq!(s.dot, StatusDot::Error);
        assert_eq!(s.headline, "Not found");
        assert_eq!(s.detail, "CLI not detected on PATH.");

        // Authenticated with email + label.
        let authed = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: Some("Claude Max Subscription".into()),
                email: Some("dev@example.com".into()),
            }),
            ..snapshot()
        };
        let s = summarize(Some(&authed), true);
        assert_eq!(s.dot, StatusDot::Success);
        assert_eq!(
            s.headline,
            "Authenticated as {email} · Claude Max Subscription"
        );
        assert_eq!(s.email.as_deref(), Some("dev@example.com"));

        // Authenticated, label only.
        let label_only = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: Some("OpenAI API Key".into()),
                email: None,
            }),
            ..snapshot()
        };
        assert_eq!(
            summarize(Some(&label_only), true).headline,
            "Authenticated · OpenAI API Key"
        );

        // Authenticated, nothing else known.
        let bare = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: None,
                email: None,
            }),
            ..snapshot()
        };
        assert_eq!(summarize(Some(&bare), true).headline, "Authenticated");

        // Unauthenticated.
        let signed_out = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Unauthenticated,
                label: None,
                email: None,
            }),
            message: Some("Run `codex login` and try again.".into()),
            ..snapshot()
        };
        let s = summarize(Some(&signed_out), true);
        assert_eq!(s.headline, "Not authenticated");
        assert_eq!(s.detail, "Run `codex login` and try again.");

        // Warning with unknown auth.
        let warn = ProviderSnapshot {
            status: Some(ProviderStatusKind::Warning),
            ..snapshot()
        };
        let s = summarize(Some(&warn), true);
        assert_eq!(s.dot, StatusDot::Warning);
        assert_eq!(s.headline, "Needs attention");
        assert_eq!(
            s.detail,
            "The provider is installed, but the server could not fully verify it."
        );

        // Error with unknown auth.
        let err = ProviderSnapshot {
            status: Some(ProviderStatusKind::Error),
            ..snapshot()
        };
        let s = summarize(Some(&err), true);
        assert_eq!(s.dot, StatusDot::Error);
        assert_eq!(s.headline, "Unavailable");
        assert_eq!(s.detail, "The provider failed its startup checks.");

        // Ready, auth indeterminate.
        let s = summarize(Some(&snapshot()), true);
        assert_eq!(s.headline, "Available");
        assert_eq!(
            s.detail,
            "Installed and ready, but authentication could not be verified."
        );

        // An in-flight probe (the placeholder snapshot the refresh inserts)
        // reads as "checking", not as "Not found" — its `installed` flag is
        // simply not known yet.
        let in_flight = ProviderSnapshot {
            checking: true,
            ..ProviderSnapshot::default()
        };
        let s = summarize(Some(&in_flight), true);
        assert_eq!(s.dot, StatusDot::Warning);
        assert_eq!(s.headline, "Checking provider status");

        // A server message always replaces the fallback detail.
        let with_message = ProviderSnapshot {
            message: Some("custom diagnostic".into()),
            ..snapshot()
        };
        assert_eq!(
            summarize(Some(&with_message), true).detail,
            "custom diagnostic"
        );
    }

    #[test]
    fn redacts_email_but_keeps_shape() {
        let redacted = redact_email("developer@example.com");
        assert!(redacted.starts_with('d'));
        assert!(redacted.ends_with(".com"));
        assert!(!redacted.contains("eveloper"));
        assert!(!redacted.contains("example"));
    }

    #[test]
    fn probe_diagnostic_messages_cover_every_variant() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let provider = ProviderKind::Codex;
        for (locale, expected) in [
            (
                tcode_i18n::LANGUAGE_ENGLISH,
                [
                    "Codex CLI (`codex`) is not installed or not on PATH.",
                    "Codex CLI is installed but failed to run.",
                    "Codex CLI is not authenticated. Run `codex login` and try again.",
                    "Could not verify Codex authentication status.",
                ],
            ),
            (
                tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE,
                [
                    "未安装 Codex CLI（`codex`），或其不在 PATH 中。",
                    "Codex CLI 已安装，但运行失败。",
                    "Codex CLI 未认证。请运行 `codex login` 后重试。",
                    "无法验证 Codex 的认证状态。",
                ],
            ),
        ] {
            tcode_i18n::set_locale(locale);
            for (diagnostic, expected) in [
                ProviderProbeDiagnostic::MissingCli,
                ProviderProbeDiagnostic::FailedCli,
                ProviderProbeDiagnostic::Unauthenticated,
                ProviderProbeDiagnostic::IndeterminateAuth,
            ]
            .into_iter()
            .zip(expected)
            {
                assert_eq!(probe_diagnostic_message(provider, diagnostic), expected);
            }
        }
        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_ENGLISH);
    }
}
