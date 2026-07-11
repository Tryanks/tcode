//! Provider status snapshots: what the Settings → Providers card shows.
//!
//! Everything here is pure. The probes that produce a [`ProviderSnapshot`]
//! (spawning `claude auth status --json`, reading Codex's `auth.json`) live in
//! [`crate::app`]; this module owns the *derivation*: the status dot, the exact
//! headline/detail copy (T3 §2), and the auth-label vocabulary (T3 §3).

use agent::ProviderKind;
use serde::Deserialize;

use crate::settings::provider_label;

/// The wire status of a provider, mirroring T3's `ready | warning | error`
/// enum. There is no `pending` (a missing snapshot is its own case), and no
/// `disabled` either: whether a provider is disabled is a *settings* fact (the
/// card's switch), not something a probe can observe — [`summarize`] applies it.
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
    /// Installed and reachable, but we could not determine auth state.
    Unknown,
}

/// What a provider probe learned about the signed-in account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAuth {
    pub status: AuthStatus,
    /// e.g. `Claude Max Subscription`, `ChatGPT Pro Subscription`, `OpenAI API Key`.
    pub label: Option<String>,
    /// Account email, when the CLI exposes one. Rendered redacted until the
    /// user clicks the reveal control.
    pub email: Option<String>,
}

/// One provider's probe result (installed? version? authenticated?).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderSnapshot {
    /// Unix seconds of the probe that produced this snapshot.
    pub checked_at: Option<u64>,
    /// Whether the CLI was found (settings override, else PATH).
    pub installed: bool,
    /// Normalized `a.b.c` version, when `--version` succeeded.
    pub version: Option<String>,
    /// `None` until the first probe lands (the "Checking provider status" case).
    pub status: Option<ProviderStatusKind>,
    pub auth: Option<ProviderAuth>,
    /// The probe's diagnostic detail, shown after the status summary.
    pub message: Option<String>,
    /// A probe is currently in flight.
    pub checking: bool,
}

/// The status dot's color role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusDot {
    /// `ready` → green/success.
    Success,
    /// `warning` (and the pre-snapshot "checking" state) → warning color.
    Warning,
    /// `error` → destructive/red.
    Error,
    /// `disabled` → amber.
    Amber,
}

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
pub fn summarize(snapshot: Option<&ProviderSnapshot>, enabled: bool) -> StatusSummary {
    let t = |key: &str| rust_i18n::t!(key).into_owned();
    let Some(snapshot) = snapshot else {
        return StatusSummary {
            dot: StatusDot::Warning,
            headline: t("providers.status.checking"),
            detail: t("providers.status.checking_detail"),
            email: None,
        };
    };
    let message = snapshot
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    let detail_or = |fallback: &str| message.clone().unwrap_or_else(|| t(fallback));

    // A snapshot with no status is a *placeholder* for an in-flight probe (it
    // reports neither installation nor auth yet), so it reads as "checking" —
    // exactly like having no snapshot at all. This has to precede the
    // installed/disabled checks, whose fields are still at their defaults here.
    let Some(status) = snapshot.status else {
        return StatusSummary {
            dot: StatusDot::Warning,
            headline: t("providers.status.checking"),
            detail: t("providers.status.checking_detail"),
            email: None,
        };
    };
    // Disabled and not-installed short-circuit before any auth consideration.
    if !enabled {
        return StatusSummary {
            dot: StatusDot::Amber,
            headline: t("providers.status.disabled"),
            detail: detail_or("providers.status.disabled_detail"),
            email: None,
        };
    }
    if !snapshot.installed {
        return StatusSummary {
            dot: StatusDot::Error,
            headline: t("providers.status.not_found"),
            detail: detail_or("providers.status.not_found_detail"),
            email: None,
        };
    }
    let dot = match status {
        ProviderStatusKind::Ready => StatusDot::Success,
        ProviderStatusKind::Warning => StatusDot::Warning,
        ProviderStatusKind::Error => StatusDot::Error,
    };

    let auth = snapshot.auth.as_ref();
    let email = auth.and_then(|a| a.email.clone()).filter(|e| !e.is_empty());
    let label = auth
        .and_then(|a| a.label.clone())
        .filter(|l| !l.is_empty());

    match auth.map(|a| a.status) {
        Some(AuthStatus::Authenticated) => {
            // `Authenticated as <email> · <label>`, else `Authenticated · <label>`,
            // else a bare `Authenticated`.
            let headline = match (&email, &label) {
                (Some(_), Some(label)) => rust_i18n::t!(
                    "providers.status.authenticated_as_with_label",
                    email = EMAIL_SLOT,
                    label = label
                )
                .into_owned(),
                (Some(_), None) => {
                    rust_i18n::t!("providers.status.authenticated_as", email = EMAIL_SLOT)
                        .into_owned()
                }
                (None, Some(label)) => {
                    rust_i18n::t!("providers.status.authenticated_with_label", label = label)
                        .into_owned()
                }
                (None, None) => t("providers.status.authenticated"),
            };
            StatusSummary {
                dot,
                headline,
                detail: message.unwrap_or_default(),
                email,
            }
        }
        Some(AuthStatus::Unauthenticated) => StatusSummary {
            dot,
            headline: t("providers.status.not_authenticated"),
            detail: message.unwrap_or_default(),
            email: None,
        },
        _ => match status {
            ProviderStatusKind::Warning => StatusSummary {
                dot,
                headline: t("providers.status.needs_attention"),
                detail: detail_or("providers.status.needs_attention_detail"),
                email: None,
            },
            ProviderStatusKind::Error => StatusSummary {
                dot,
                headline: t("providers.status.unavailable"),
                detail: detail_or("providers.status.unavailable_detail"),
                email: None,
            },
            // Ready with unknown auth.
            ProviderStatusKind::Ready => StatusSummary {
                dot,
                headline: t("providers.status.available"),
                detail: detail_or("providers.status.available_detail"),
                email: None,
            },
        },
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

// ---------------------------------------------------------------------------
// Claude: `claude auth status --json`
// ---------------------------------------------------------------------------

/// The subset of `claude auth status --json` we consume. Verified against
/// claude 2.1.x, which prints:
/// `{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty",
///   "email":"…","orgId":"…","orgName":"…","subscriptionType":"max"}`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAuthStatus {
    #[serde(default)]
    pub logged_in: bool,
    #[serde(default)]
    pub auth_method: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub subscription_type: Option<String>,
}

/// Map `claude auth status --json` onto the card's auth line.
///
/// Labels follow T3: `Claude API Key`, or `Claude <plan> Subscription` with the
/// plan normalized to Max / Max 5x / Max 20x / Pro / Team / Enterprise / Free.
pub fn parse_claude_auth(json: &str) -> Option<ProviderAuth> {
    let status: ClaudeAuthStatus = serde_json::from_str(json).ok()?;
    if !status.logged_in {
        return Some(ProviderAuth {
            status: AuthStatus::Unauthenticated,
            label: None,
            email: None,
        });
    }
    let is_api_key = status
        .auth_method
        .as_deref()
        .map(|m| {
            let m = m.to_ascii_lowercase();
            m.contains("apikey") || m.contains("api_key") || m.contains("api key")
        })
        .unwrap_or(false);
    let label = if is_api_key {
        Some("Claude API Key".to_string())
    } else {
        status
            .subscription_type
            .as_deref()
            .and_then(normalize_claude_plan)
            .map(|plan| format!("Claude {plan} Subscription"))
    };
    Some(ProviderAuth {
        status: AuthStatus::Authenticated,
        label,
        email: status.email.filter(|e| !e.is_empty()),
    })
}

/// Normalize Claude's `subscriptionType` to its display plan name.
fn normalize_claude_plan(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
        "max" => Some("Max"),
        "max_5x" | "max5x" => Some("Max 5x"),
        "max_20x" | "max20x" => Some("Max 20x"),
        "pro" => Some("Pro"),
        "team" => Some("Team"),
        "enterprise" => Some("Enterprise"),
        "free" => Some("Free"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Codex: `$CODEX_HOME/auth.json`
// ---------------------------------------------------------------------------

/// Parse Codex's `auth.json`. Verified against codex 0.5x: the file carries
/// `auth_mode` (`chatgpt` | `apikey`), an optional `OPENAI_API_KEY`, and — for
/// ChatGPT logins — `tokens.id_token`, a JWT whose claims hold the account
/// `email` and `https://api.openai.com/auth`.`chatgpt_plan_type`.
///
/// `codex login status` prints only "Logged in using ChatGPT" (no email, no
/// plan, no `--json`), so `auth.json` is the only structured source.
pub fn parse_codex_auth(json: &str) -> Option<ProviderAuth> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let mode = value
        .get("auth_mode")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_api_key = value
        .get("OPENAI_API_KEY")
        .and_then(|k| k.as_str())
        .is_some_and(|k| !k.is_empty());

    if mode == "apikey" || (mode.is_empty() && has_api_key) {
        return Some(ProviderAuth {
            status: AuthStatus::Authenticated,
            label: Some("OpenAI API Key".to_string()),
            // The API-key login carries no account email.
            email: None,
        });
    }
    let Some(id_token) = value
        .get("tokens")
        .and_then(|t| t.get("id_token"))
        .and_then(|t| t.as_str())
    else {
        return Some(ProviderAuth {
            status: AuthStatus::Unauthenticated,
            label: None,
            email: None,
        });
    };
    let claims = decode_jwt_claims(id_token)?;
    let email = claims
        .get("email")
        .and_then(|e| e.as_str())
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    let plan = claims
        .get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(|p| p.as_str())
        .and_then(normalize_chatgpt_plan);
    Some(ProviderAuth {
        status: AuthStatus::Authenticated,
        label: Some(match plan {
            Some(plan) => format!("ChatGPT {plan} Subscription"),
            None => "ChatGPT Subscription".to_string(),
        }),
        email,
    })
}

/// Normalize `chatgpt_plan_type` to its T3 display plan name.
fn normalize_chatgpt_plan(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
        "free" => Some("Free"),
        "go" => Some("Go"),
        "plus" => Some("Plus"),
        "pro" => Some("Pro"),
        "pro_5x" => Some("Pro 5x"),
        "pro_20x" => Some("Pro 20x"),
        "team" => Some("Team"),
        "business" => Some("Business"),
        "enterprise" => Some("Enterprise"),
        "edu" => Some("Edu"),
        _ => None,
    }
}

/// Decode a JWT's claim set (the middle segment; base64url, unpadded). The
/// token is never verified — we only read the account claims Codex stored.
fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The exact per-provider probe messages T3 shows for a missing CLI (§3).
pub fn missing_cli_message(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => rust_i18n::t!("providers.probe.codex_missing").into_owned(),
        ProviderKind::ClaudeCode => rust_i18n::t!("providers.probe.claude_missing").into_owned(),
    }
}

/// The message shown when the CLI is present but its version command failed.
pub fn failed_cli_message(provider: ProviderKind) -> String {
    rust_i18n::t!(
        "providers.probe.failed_run",
        provider = provider_label(provider)
    )
    .into_owned()
}

/// The message shown when the CLI ran but its auth state could not be read.
pub fn indeterminate_auth_message(provider: ProviderKind) -> String {
    rust_i18n::t!(
        "providers.probe.indeterminate_auth",
        provider = provider_label(provider)
    )
    .into_owned()
}

/// The message shown when the CLI is signed out.
pub fn unauthenticated_message(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => rust_i18n::t!("providers.probe.codex_signed_out").into_owned(),
        ProviderKind::ClaudeCode => rust_i18n::t!("providers.probe.claude_signed_out").into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(summarize(Some(&with_message), true).detail, "custom diagnostic");
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
    fn parses_claude_auth_status_json() {
        // Real shape from `claude auth status --json` (claude 2.1.x).
        let auth = parse_claude_auth(
            r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty",
                "email":"dev@example.com","orgId":"x","orgName":"y","subscriptionType":"max"}"#,
        )
        .unwrap();
        assert_eq!(auth.status, AuthStatus::Authenticated);
        assert_eq!(auth.label.as_deref(), Some("Claude Max Subscription"));
        assert_eq!(auth.email.as_deref(), Some("dev@example.com"));

        // Plan normalization.
        for (raw, expected) in [
            ("max_20x", "Claude Max 20x Subscription"),
            ("max_5x", "Claude Max 5x Subscription"),
            ("pro", "Claude Pro Subscription"),
            ("team", "Claude Team Subscription"),
            ("enterprise", "Claude Enterprise Subscription"),
            ("free", "Claude Free Subscription"),
        ] {
            let json = format!(r#"{{"loggedIn":true,"subscriptionType":"{raw}"}}"#);
            assert_eq!(
                parse_claude_auth(&json).unwrap().label.as_deref(),
                Some(expected)
            );
        }

        // API-key logins.
        let auth =
            parse_claude_auth(r#"{"loggedIn":true,"authMethod":"apiKey","email":null}"#).unwrap();
        assert_eq!(auth.label.as_deref(), Some("Claude API Key"));

        // Signed out.
        let auth = parse_claude_auth(r#"{"loggedIn":false}"#).unwrap();
        assert_eq!(auth.status, AuthStatus::Unauthenticated);

        // Garbage in → nothing claimed.
        assert!(parse_claude_auth("not json").is_none());
    }

    #[test]
    fn parses_codex_auth_json() {
        use base64::Engine as _;
        let claims = serde_json::json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": { "chatgpt_plan_type": "pro" },
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let json = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": serde_json::Value::Null,
            "tokens": { "id_token": format!("header.{payload}.signature") },
        })
        .to_string();

        let auth = parse_codex_auth(&json).unwrap();
        assert_eq!(auth.status, AuthStatus::Authenticated);
        assert_eq!(auth.label.as_deref(), Some("ChatGPT Pro Subscription"));
        assert_eq!(auth.email.as_deref(), Some("dev@example.com"));

        // API-key mode: no email is exposed.
        let auth = parse_codex_auth(r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x"}"#).unwrap();
        assert_eq!(auth.label.as_deref(), Some("OpenAI API Key"));
        assert_eq!(auth.email, None);

        // ChatGPT mode with no token → signed out.
        let auth = parse_codex_auth(r#"{"auth_mode":"chatgpt"}"#).unwrap();
        assert_eq!(auth.status, AuthStatus::Unauthenticated);

        assert!(parse_codex_auth("not json").is_none());
    }

    #[test]
    fn chatgpt_plan_names_cover_the_t3_vocabulary() {
        for (raw, expected) in [
            ("free", "Free"),
            ("go", "Go"),
            ("plus", "Plus"),
            ("pro", "Pro"),
            ("pro_5x", "Pro 5x"),
            ("pro_20x", "Pro 20x"),
            ("team", "Team"),
            ("business", "Business"),
            ("enterprise", "Enterprise"),
            ("edu", "Edu"),
        ] {
            assert_eq!(normalize_chatgpt_plan(raw), Some(expected));
        }
        // An unknown plan degrades to the generic label rather than inventing one.
        assert_eq!(normalize_chatgpt_plan("mystery"), None);
    }
}
