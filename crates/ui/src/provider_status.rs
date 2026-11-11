//! Provider status presentation: a probe's findings in words, and the
//! account email redacted. What to say comes from the harness's description,
//! never from its kind.

pub use tcode_core::provider_status::ProviderSnapshot;
use tcode_core::provider_status::{AuthStatus, ProviderAuth, ProviderProbeDiagnostic};
use tcode_protocol::HarnessDescription;

/// The account a profile is signed in to, when its snapshot names one by email.
pub fn signed_in_account(snapshot: &ProviderSnapshot) -> Option<&ProviderAuth> {
    snapshot.auth.as_ref().filter(|auth| {
        auth.status == AuthStatus::Authenticated
            && auth
                .email
                .as_deref()
                .is_some_and(|email| !email.trim().is_empty())
    })
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

/// That the CLI `name` the snapshot probed is older than the oldest release
/// Tcode supports, when the host found it so.
pub fn outdated_detail(name: &str, snapshot: &ProviderSnapshot) -> Option<String> {
    let supported = snapshot.supported_from.as_deref()?;
    Some(
        crate::tr!(
            "providers.harness.outdated_detail",
            name = name,
            version = snapshot.version.as_deref().unwrap_or_default(),
            supported = supported
        )
        .into_owned(),
    )
}

/// What a probe found, as a harness's settings explain it: the host's own
/// message when it sent one, else its diagnostic in words the harness's
/// description fills in.
pub fn snapshot_detail(description: &HarnessDescription, snapshot: &ProviderSnapshot) -> String {
    if let Some(message) = snapshot
        .message
        .as_deref()
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return message.to_owned();
    }
    let binary = description.binary_name.clone().unwrap_or_default();
    let t = |key: &str| crate::tr!(key).into_owned();
    match snapshot.diagnostic {
        Some(ProviderProbeDiagnostic::MissingCli) => {
            crate::tr!("providers.probe.missing", binary = binary).into_owned()
        }
        Some(ProviderProbeDiagnostic::FailedCli) => {
            crate::tr!("providers.probe.failed_run", binary = binary).into_owned()
        }
        Some(ProviderProbeDiagnostic::Unauthenticated) if description.account_login => {
            t("providers.probe.signed_out_here")
        }
        Some(ProviderProbeDiagnostic::Unauthenticated) => match &description.sign_in_command {
            Some(command) => {
                crate::tr!("providers.probe.signed_out_command", command = command).into_owned()
            }
            None => crate::tr!("providers.probe.signed_out_provider", binary = binary).into_owned(),
        },
        Some(ProviderProbeDiagnostic::IndeterminateAuth) => {
            crate::tr!("providers.probe.indeterminate_auth", binary = binary).into_owned()
        }
        Some(ProviderProbeDiagnostic::SharedCredentials) => {
            t("providers.status.shared_credentials")
        }
        Some(ProviderProbeDiagnostic::MissingApiKey) => crate::tr!(
            "providers.probe.missing_key",
            variable = description
                .endpoint
                .as_ref()
                .map(|endpoint| endpoint.key_env.clone())
                .unwrap_or_default()
        )
        .into_owned(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The advice for each probe outcome comes from what the harness's
    /// description says about it, so an endpoint profile without its key is
    /// told about the key, not about signing in.
    #[test]
    fn probe_details_follow_the_harness_description() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let detail = |provider: agent::ProviderKind, diagnostic| {
            snapshot_detail(
                &HarnessDescription::native(provider),
                &ProviderSnapshot {
                    diagnostic: Some(diagnostic),
                    message: Some("  ".into()),
                    ..ProviderSnapshot::default()
                },
            )
        };
        for (locale, expected) in [
            (
                crate::LANGUAGE_ENGLISH,
                [
                    "`cursor-agent` is not on PATH on this machine.",
                    "Not signed in. Run `opencode auth login` in a terminal on this machine, then check again.",
                    "No model provider is signed in. Configure one in `pi`, then check again.",
                    "No API key is saved. Enter it as ANTHROPIC_API_KEY.",
                ],
            ),
            (
                crate::LANGUAGE_SIMPLIFIED_CHINESE,
                [
                    "此主机的 PATH 中没有 `cursor-agent`。",
                    "未登录。在此主机的终端运行 `opencode auth login`，然后重新检查。",
                    "没有已登录的模型提供方。在 `pi` 中配置一个，然后重新检查。",
                    "没有保存 API Key。请在 ANTHROPIC_API_KEY 中填入。",
                ],
            ),
        ] {
            crate::set_locale(locale);
            assert_eq!(
                [
                    detail(
                        agent::ProviderKind::Cursor,
                        ProviderProbeDiagnostic::MissingCli
                    ),
                    detail(
                        agent::ProviderKind::OpenCode,
                        ProviderProbeDiagnostic::Unauthenticated
                    ),
                    detail(
                        agent::ProviderKind::Pi,
                        ProviderProbeDiagnostic::Unauthenticated
                    ),
                    detail(
                        agent::ProviderKind::ClaudeCode,
                        ProviderProbeDiagnostic::MissingApiKey
                    ),
                ],
                expected
            );
        }
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        let failed = ProviderSnapshot {
            diagnostic: Some(ProviderProbeDiagnostic::FailedCli),
            message: Some(" exit status: 1: Error: Cannot find module ".into()),
            ..ProviderSnapshot::default()
        };
        assert_eq!(
            snapshot_detail(
                &HarnessDescription::native(agent::ProviderKind::Pi),
                &failed
            ),
            "exit status: 1: Error: Cannot find module",
            "what the CLI printed is shown as it printed it"
        );
    }

    #[test]
    fn redacts_email_but_keeps_shape() {
        for (email, expected) in [
            ("developer@example.com", "d••••••••@•••••••.com"),
            ("张三@例子.com", "张•@••.com"),
            ("a@localhost", "a•@•••••••••"),
            ("@", "•@•"),
            ("invalid", "•••••••"),
            ("", "•"),
        ] {
            assert_eq!(redact_email(email), expected, "{email}");
        }
    }
}
