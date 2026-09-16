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

/// Claude Code's 1M context-window model suffix (`claude-opus-5[1m]`).
pub const CONTEXT_1M_SUFFIX: &str = "[1m]";

/// Drop Claude Code's `[1m]` context suffix from a model id. Only this fixed
/// suffix is recognised: the CLI echoes the launch id (`claude-opus-5[1m]`)
/// in `init` while the API reports the bare id, and the two name one model.
pub fn strip_context_1m_suffix(model: &str) -> &str {
    model.strip_suffix(CONTEXT_1M_SUFFIX).unwrap_or(model)
}

/// Format a context-window token count for display.
pub fn format_context_window(tokens: u64) -> String {
    if tokens == 1_000_000 {
        "1M".to_owned()
    } else {
        format!("{}k", tokens / 1_000)
    }
}

/// Resolve the selected context window, falling back to the model's default
/// window in the current Claude model manifest.
pub fn resolved_context_window(model_id: &str, selections: &[OptionSelection]) -> u64 {
    crate::claude_manifest::current().resolved_context_window(model_id, selections)
}
