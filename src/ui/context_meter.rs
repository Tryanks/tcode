//! Circular context-window meter math and token formatting.
//!
//! A faithful port of the numeric parts of T3's `lib/contextWindow.ts` +
//! `ContextWindowMeter.tsx`: derive the used-percentage from a [`TokenUsage`],
//! flag the >90% overloaded state, and format token counts (`42k`, `1.2m`).

use agent::TokenUsage;

/// The used tokens a meter reflects: the provider's reported total-in-use, else
/// the input-token count.
pub fn used_tokens(usage: &TokenUsage) -> Option<u64> {
    usage.used_tokens.or(usage.input_tokens)
}

/// Used-percentage of the context window (0..=100), or `None` when either the
/// used count or the window size is unknown. Capped at 100 (T3).
pub fn used_percentage(usage: &TokenUsage) -> Option<f32> {
    let used = used_tokens(usage)? as f32;
    let max = usage.context_window? as f32;
    if max <= 0.0 {
        return None;
    }
    Some((used / max * 100.0).min(100.0))
}

/// Whether the meter is in the red "overloaded" band (>90%).
pub fn is_overloaded(percentage: f32) -> bool {
    percentage > 90.0
}

/// Format the percentage label the way T3 does: one decimal below 10%
/// (trimming a trailing `.0`), otherwise a whole number. `None` propagates.
pub fn format_percentage(percentage: Option<f32>) -> Option<String> {
    let value = percentage?;
    if !value.is_finite() {
        return None;
    }
    if value < 10.0 {
        let s = format!("{value:.1}");
        let s = s.strip_suffix(".0").map(str::to_string).unwrap_or(s);
        Some(format!("{s}%"))
    } else {
        Some(format!("{}%", value.round() as i64))
    }
}

/// Format a token count exactly like T3's `formatContextWindowTokens`:
/// `<1000` verbatim, `<10_000` → `x.yk` (trim `.0`), `<1_000_000` → `Nk`,
/// else `x.ym` (trim `.0`).
pub fn format_tokens(value: Option<u64>) -> String {
    let Some(v) = value else {
        return "0".to_string();
    };
    let v = v as f64;
    if v < 1_000.0 {
        format!("{}", v.round() as i64)
    } else if v < 10_000.0 {
        trim_dot_zero(format!("{:.1}", v / 1_000.0)) + "k"
    } else if v < 1_000_000.0 {
        format!("{}k", (v / 1_000.0).round() as i64)
    } else {
        trim_dot_zero(format!("{:.1}", v / 1_000_000.0)) + "m"
    }
}

fn trim_dot_zero(s: String) -> String {
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(used: Option<u64>, window: Option<u64>) -> TokenUsage {
        TokenUsage {
            used_tokens: used,
            context_window: window,
            ..Default::default()
        }
    }

    #[test]
    fn percentage_and_overload() {
        assert_eq!(
            used_percentage(&usage(Some(100_000), Some(200_000))),
            Some(50.0)
        );
        // Capped at 100 even past the window.
        assert_eq!(
            used_percentage(&usage(Some(300_000), Some(200_000))),
            Some(100.0)
        );
        // Unknown window → None.
        assert_eq!(used_percentage(&usage(Some(100), None)), None);
        assert!(is_overloaded(95.0));
        assert!(!is_overloaded(90.0));
    }

    #[test]
    fn used_falls_back_to_input_tokens() {
        let u = TokenUsage {
            input_tokens: Some(1_234),
            ..Default::default()
        };
        assert_eq!(used_tokens(&u), Some(1_234));
    }

    #[test]
    fn percentage_label_format() {
        assert_eq!(format_percentage(Some(5.0)), Some("5%".to_string()));
        assert_eq!(format_percentage(Some(5.5)), Some("5.5%".to_string()));
        assert_eq!(format_percentage(Some(42.4)), Some("42%".to_string()));
        assert_eq!(format_percentage(Some(90.6)), Some("91%".to_string()));
        assert_eq!(format_percentage(None), None);
    }

    #[test]
    fn token_format_matches_t3() {
        assert_eq!(format_tokens(Some(0)), "0");
        assert_eq!(format_tokens(Some(999)), "999");
        assert_eq!(format_tokens(Some(1_500)), "1.5k");
        assert_eq!(format_tokens(Some(4_000)), "4k");
        assert_eq!(format_tokens(Some(42_000)), "42k");
        assert_eq!(format_tokens(Some(200_000)), "200k");
        assert_eq!(format_tokens(Some(1_000_000)), "1m");
        assert_eq!(format_tokens(Some(1_250_000)), "1.2m");
        assert_eq!(format_tokens(None), "0");
    }
}
