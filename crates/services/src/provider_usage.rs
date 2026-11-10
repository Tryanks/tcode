//! Provider account usage fetching and normalization.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent::{LaunchEnv, ProviderKind};
use chrono::DateTime;
use serde_json::Value;
use tcode_core::usage::{ProviderUsage, UsageWindow, UsageWindowKind};

fn error_usage(fetched_at: u64, message: impl Into<String>) -> ProviderUsage {
    ProviderUsage {
        fetched_at,
        error: Some(message.into()),
        ..ProviderUsage::default()
    }
}

fn unix_seconds(value: &Value) -> Option<u64> {
    value.as_u64()
}

fn rfc3339_seconds(value: &Value) -> Option<u64> {
    let timestamp = DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()?
        .timestamp();
    u64::try_from(timestamp).ok()
}

fn codex_window(value: &Value) -> Option<UsageWindow> {
    let minutes = u32::try_from(value.get("windowDurationMins")?.as_u64()?).ok()?;
    Some(UsageWindow {
        kind: UsageWindowKind::from_minutes(minutes),
        scope: None,
        used_percent: value.get("usedPercent")?.as_f64()? as f32,
        resets_at: value.get("resetsAt").and_then(unix_seconds),
    })
}

/// Normalize the account-wide `rateLimits` object returned by Codex.
pub fn parse_codex_rate_limits(value: &Value, fetched_at: u64) -> ProviderUsage {
    let Some(rate_limits) = value.get("rateLimits").filter(|value| !value.is_null()) else {
        return error_usage(fetched_at, "no rate limits reported");
    };
    let plan = rate_limits
        .get("planType")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut windows: Vec<_> = ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| rate_limits.get(key).filter(|value| !value.is_null()))
        .filter_map(codex_window)
        .collect();
    windows.sort_by_key(|window| match window.kind {
        UsageWindowKind::FiveHour => 0,
        UsageWindowKind::Weekly => 1,
        UsageWindowKind::Other { .. } => 2,
    });
    ProviderUsage {
        fetched_at,
        plan,
        windows,
        error: None,
    }
}

fn claude_window(
    value: &Value,
    kind: UsageWindowKind,
    scope: Option<String>,
) -> Option<UsageWindow> {
    let used_percent = value
        .get("percent")
        .or_else(|| value.get("utilization"))?
        .as_f64()? as f32;
    Some(UsageWindow {
        kind,
        scope,
        used_percent,
        resets_at: value.get("resets_at").and_then(rfc3339_seconds),
    })
}

/// Normalize Claude Code's `get_usage` response.
pub fn parse_claude_usage(value: &Value, fetched_at: u64) -> ProviderUsage {
    let plan = value
        .get("subscription_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if value.get("rate_limits_available").and_then(Value::as_bool) == Some(false) {
        return ProviderUsage {
            plan,
            ..error_usage(fetched_at, "usage not available")
        };
    }
    let Some(rate_limits) = value.get("rate_limits").filter(|value| !value.is_null()) else {
        return ProviderUsage {
            plan,
            ..error_usage(fetched_at, "usage not available")
        };
    };

    let mut five_hour = Vec::new();
    let mut weekly = Vec::new();
    let mut scoped = Vec::new();
    if let Some(limits) = rate_limits.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let scope = limit
                .pointer("/scope/model/display_name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if limit.get("kind").and_then(Value::as_str) == Some("session") {
                if let Some(window) = claude_window(limit, UsageWindowKind::FiveHour, scope) {
                    five_hour.push(window);
                }
            } else if limit.get("group").and_then(Value::as_str) == Some("weekly")
                && let Some(window) = claude_window(limit, UsageWindowKind::Weekly, scope.clone())
            {
                if scope.is_some() {
                    scoped.push(window);
                } else {
                    weekly.push(window);
                }
            }
        }
    } else {
        if let Some(window) = rate_limits
            .get("five_hour")
            .filter(|value| !value.is_null())
            .and_then(|value| claude_window(value, UsageWindowKind::FiveHour, None))
        {
            five_hour.push(window);
        }
        if let Some(window) = rate_limits
            .get("seven_day")
            .filter(|value| !value.is_null())
            .and_then(|value| claude_window(value, UsageWindowKind::Weekly, None))
        {
            weekly.push(window);
        }
        for (key, label) in [("seven_day_opus", "Opus"), ("seven_day_sonnet", "Sonnet")] {
            if let Some(window) = rate_limits
                .get(key)
                .filter(|value| !value.is_null())
                .and_then(|value| {
                    claude_window(value, UsageWindowKind::Weekly, Some(label.to_owned()))
                })
            {
                scoped.push(window);
            }
        }
    }
    five_hour.extend(weekly);
    five_hour.extend(scoped);
    ProviderUsage {
        fetched_at,
        plan,
        windows: five_hour,
        error: None,
    }
}

/// Fetch and normalize usage for providers that expose account limits.
pub async fn fetch_provider_usage(
    provider: ProviderKind,
    binary: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Option<ProviderUsage> {
    let payload = match provider {
        ProviderKind::Codex => agent::codex::read_rate_limits(binary, launch_env).await,
        ProviderKind::ClaudeCode => agent::claude::read_usage(binary, launch_env).await,
        ProviderKind::Pi
        | ProviderKind::OpenCode
        | ProviderKind::Cursor
        | ProviderKind::Grok
        | ProviderKind::Acp => return None,
    };
    usage_from_payload(provider, payload.map_err(|error| error.to_string()))
}

/// Normalize a usage payload in the harness's own shape, read by a
/// short-lived process or asked of a running session
/// (`SessionCommand::ReadAccountUsage`).
pub fn usage_from_payload(
    provider: ProviderKind,
    payload: Result<Value, String>,
) -> Option<ProviderUsage> {
    let fetched_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let parse = match provider {
        ProviderKind::Codex => parse_codex_rate_limits,
        ProviderKind::ClaudeCode => parse_claude_usage,
        ProviderKind::Pi
        | ProviderKind::OpenCode
        | ProviderKind::Cursor
        | ProviderKind::Grok
        | ProviderKind::Acp => return None,
    };
    Some(match payload {
        Ok(value) => parse(&value, fetched_at),
        Err(error) => error_usage(fetched_at, error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `get_usage` in the shape Claude Code 2.1.322's control schema
    /// describes (`rate_limits` with nullable `five_hour`, `seven_day`,
    /// `seven_day_opus`, `seven_day_sonnet` windows of `utilization` and
    /// ISO 8601 `resets_at`); the numbers are invented.
    #[test]
    fn claude_usage_lists_the_session_week_and_model_weeks() {
        let usage = parse_claude_usage(
            &serde_json::json!({
                "subscription_type": "max",
                "rate_limits_available": true,
                "rate_limits": {
                    "five_hour": {"utilization": 12.0, "resets_at": "2026-01-02T03:00:00.000Z"},
                    "seven_day": {"utilization": 40.0, "resets_at": "2026-01-08T00:00:00.000Z"},
                    "seven_day_oauth_apps": null,
                    "seven_day_opus": null,
                    "seven_day_sonnet": {"utilization": 5.0, "resets_at": null}
                }
            }),
            7,
        );
        assert_eq!(usage.plan.as_deref(), Some("max"));
        assert_eq!(usage.error, None);
        assert_eq!(
            usage.windows,
            [
                UsageWindow {
                    kind: UsageWindowKind::FiveHour,
                    scope: None,
                    used_percent: 12.0,
                    resets_at: Some(1_767_322_800),
                },
                UsageWindow {
                    kind: UsageWindowKind::Weekly,
                    scope: None,
                    used_percent: 40.0,
                    resets_at: Some(1_767_830_400),
                },
                UsageWindow {
                    kind: UsageWindowKind::Weekly,
                    scope: Some("Sonnet".into()),
                    used_percent: 5.0,
                    resets_at: None,
                },
            ]
        );

        // An API-key session: "rate_limits will be null".
        let api_key = parse_claude_usage(
            &serde_json::json!({
                "subscription_type": null,
                "rate_limits_available": false,
                "rate_limits": null
            }),
            7,
        );
        assert_eq!(api_key.error.as_deref(), Some("usage not available"));
        assert!(api_key.windows.is_empty());
    }

    /// `account/rateLimits/read` in the shape of codex-cli 0.167.0's
    /// app-server schema (`GetAccountRateLimitsResponse.rateLimits`, a
    /// `RateLimitSnapshot` of `RateLimitWindow`s); the numbers are invented.
    #[test]
    fn codex_rate_limits_list_the_five_hour_window_before_the_week() {
        let usage = parse_codex_rate_limits(
            &serde_json::json!({
                "rateLimits": {
                    "limitId": "codex",
                    "planType": "plus",
                    "primary": {"usedPercent": 30, "windowDurationMins": 10080, "resetsAt": 1_767_830_400},
                    "secondary": {"usedPercent": 4, "windowDurationMins": 300, "resetsAt": null},
                    "credits": null
                }
            }),
            7,
        );
        assert_eq!(usage.plan.as_deref(), Some("plus"));
        assert_eq!(
            usage.windows,
            [
                UsageWindow {
                    kind: UsageWindowKind::FiveHour,
                    scope: None,
                    used_percent: 4.0,
                    resets_at: None,
                },
                UsageWindow {
                    kind: UsageWindowKind::Weekly,
                    scope: None,
                    used_percent: 30.0,
                    resets_at: Some(1_767_830_400),
                },
            ]
        );
    }

    #[test]
    #[ignore = "requires installed and signed-in Codex and Claude Code CLIs"]
    fn live_provider_usage() {
        smol::block_on(async {
            for provider in [ProviderKind::Codex, ProviderKind::ClaudeCode] {
                let usage = fetch_provider_usage(provider, None, LaunchEnv::default())
                    .await
                    .expect("supported provider");
                println!("{provider:?}: {usage:#?}");
                assert_eq!(usage.error, None);
                assert!(!usage.windows.is_empty());
            }
        });
    }
}
