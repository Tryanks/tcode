//! Provider auth JSON and JWT parsing.

use serde::Deserialize;
use tcode_core::provider_status::{AuthStatus, ProviderAuth};

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
    match raw
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
        .as_str()
    {
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

#[cfg(test)]
mod tests {
    use super::*;

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
