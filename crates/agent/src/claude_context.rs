//! Claude metadata helpers shared by process and type-only builds.

use serde_json::Value;

use crate::OptionSelection;

/// Parse a Claude context-window selection into a validated token count.
pub fn parse_context_window_tokens(value: &Value) -> Option<u64> {
    let tokens = match value {
        Value::Number(number) => number.as_u64()?,
        Value::String(value) => {
            let value = value.trim().to_ascii_lowercase();
            if let Some(value) = value.strip_suffix('k') {
                value.parse::<u64>().ok()?.checked_mul(1_000)?
            } else if let Some(value) = value.strip_suffix('m') {
                value.parse::<u64>().ok()?.checked_mul(1_000_000)?
            } else {
                let value = value.parse::<u64>().ok()?;
                if value < 1_000 {
                    value.checked_mul(1_000)?
                } else {
                    value
                }
            }
        }
        _ => return None,
    };
    (100_000..=1_000_000).contains(&tokens).then_some(tokens)
}

/// Return the model's native context-window size in tokens.
pub fn native_context_window(model_id: &str) -> u64 {
    match model_id.strip_suffix("[1m]").unwrap_or(model_id) {
        "claude-fable-5" | "claude-fable-5-1" | "claude-opus-5" | "claude-sonnet-5"
        | "claude-opus-4-7" | "claude-opus-4-8" => 1_000_000,
        _ => 200_000,
    }
}

/// Format a context-window token count for display.
pub fn format_context_window(tokens: u64) -> String {
    if tokens == 1_000_000 {
        "1M".to_owned()
    } else {
        format!("{}k", tokens / 1_000)
    }
}

/// Resolve the selected context window, falling back to the model's native size.
pub fn resolved_context_window(model_id: &str, selections: &[OptionSelection]) -> u64 {
    selections
        .iter()
        .find(|selection| selection.id == "contextWindow")
        .and_then(|selection| parse_context_window_tokens(&selection.value))
        .unwrap_or_else(|| native_context_window(model_id))
}
