//! Provider auth JSON and JWT parsing.

use serde::Deserialize;
use tcode_core::provider_status::{AuthStatus, ProviderAuth};

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
/// Labels are `Claude API Key` or `Claude <plan> Subscription`, with the plan
/// normalized to Claude's plan names: Free / Pro / Max / Ultra / Team /
/// Enterprise.
pub fn parse_claude_auth(json: &str, plan_name: Option<&str>) -> Option<ProviderAuth> {
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
        plan_name
            .or_else(|| {
                status
                    .subscription_type
                    .as_deref()
                    .and_then(normalize_claude_plan)
            })
            .map(|plan| format!("Claude {plan} Subscription"))
    };
    Some(ProviderAuth {
        status: AuthStatus::Authenticated,
        label,
        email: status.email.filter(|e| !e.is_empty()),
    })
}

/// The plan name Claude Code shows for the signed-in account, which it keeps
/// as `oauthAccount.planDisplayName` in its account file (`.claude.json`):
/// unlike `subscriptionType`, it tells Max from Ultra.
pub fn claude_plan_name(account_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(account_json).ok()?;
    value
        .pointer("/oauthAccount/planDisplayName")?
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// [`claude_plan_name`] of the account a Claude Code child launched with
/// `launch_env` signs in as.
pub fn claude_account_plan_name(launch_env: &agent::LaunchEnv) -> Option<String> {
    let path = agent::claude::account_file(launch_env)?;
    claude_plan_name(&std::fs::read_to_string(path).ok()?)
}

/// Normalize Claude's `subscriptionType` to its display plan name. Claude
/// Code reports `max` for both the 5x and the 20x tier, which claude.com now
/// sells as Max and Ultra; only the 20x tier's own id can name Ultra.
fn normalize_claude_plan(raw: &str) -> Option<&'static str> {
    match raw
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
        .as_str()
    {
        "max" | "max_5x" | "max5x" => Some("Max"),
        "ultra" | "max_20x" | "max20x" => Some("Ultra"),
        "pro" => Some("Pro"),
        "team" => Some("Team"),
        "enterprise" => Some("Enterprise"),
        "free" => Some("Free"),
        _ => None,
    }
}

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

/// Normalize `chatgpt_plan_type` to its display plan name.
fn normalize_chatgpt_plan(raw: &str) -> Option<&'static str> {
    match raw
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
        .as_str()
    {
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

/// Parse pi's `auth.json`. Each top-level key names an upstream provider whose
/// object carries a non-empty `type`.
///
/// An empty or unreadable credential set remains indeterminate because the
/// CLI may also authenticate through environment variables.
pub fn parse_aggregator_auth(json: &str) -> Option<ProviderAuth> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let object = value.as_object()?;
    upstream_providers_auth(
        object
            .iter()
            .filter_map(|(provider, credential)| {
                credential
                    .as_object()?
                    .get("type")
                    .and_then(|value| value.as_str())
                    .filter(|kind| !kind.trim().is_empty())
                    .map(|_| provider.as_str())
            })
            .collect(),
    )
}

/// Parse `opencode auth list --format json`: the integrations OpenCode knows,
/// each with the connections (stored credentials or environment) it uses.
///
/// None connected remains indeterminate: OpenCode's own free models need no
/// sign-in.
pub fn parse_opencode_auth(json: &str) -> Option<ProviderAuth> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    upstream_providers_auth(
        value
            .as_array()?
            .iter()
            .filter(|integration| {
                integration
                    .get("connections")
                    .and_then(|connections| connections.as_array())
                    .is_some_and(|connections| !connections.is_empty())
            })
            .filter_map(|integration| integration.get("id")?.as_str())
            .collect(),
    )
}

/// Signed in to the listed upstream providers, labelled by the first three.
fn upstream_providers_auth(mut providers: Vec<&str>) -> Option<ProviderAuth> {
    if providers.is_empty() {
        return None;
    }
    providers.sort_unstable();
    let overflow = providers.len().saturating_sub(3);
    let mut label = providers.into_iter().take(3).collect::<Vec<_>>().join(", ");
    if overflow > 0 {
        label.push_str(&format!(" +{overflow}"));
    }
    Some(ProviderAuth {
        status: AuthStatus::Authenticated,
        label: Some(label),
        email: None,
    })
}
