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

/// Claude Code's context-window model suffixes (`claude-opus-5[1m]`). The
/// CLI compares model ids with `/\[1m\]$/i` and its canonical-name
/// normalizer already accepts `[2m]`; this list is deliberately explicit
/// rather than matching any `[...]`.
pub const CONTEXT_WINDOW_SUFFIXES: &[&str] = &["[1m]", "[2m]"];

/// Drop a listed Claude Code context suffix from a model id, case-
/// insensitively as the CLI does. The CLI echoes the launch id
/// (`claude-opus-5[1m]`) in `init` while the API reports the bare id, and
/// the two name one model.
pub fn strip_context_window_suffix(model: &str) -> &str {
    CONTEXT_WINDOW_SUFFIXES
        .iter()
        .find_map(|suffix| {
            let base = model.len().checked_sub(suffix.len())?;
            model
                .is_char_boundary(base)
                .then(|| model.split_at(base))
                .filter(|(_, tail)| tail.eq_ignore_ascii_case(suffix))
                .map(|(head, _)| head)
        })
        .unwrap_or(model)
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
