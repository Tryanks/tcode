//! Presentation helpers for provider usage windows (Settings → Usage and the
//! composer's context-window popover).
//!
//! [`tcode_core::usage`] is presentation-free: it reports a window kind, a
//! percentage, and a reset timestamp. Everything a human reads about those —
//! the window's label, the "resets in …" copy, the bar tint — is derived here
//! so both surfaces stay in sync.

use gpui::{App, Hsla};
use tcode_core::usage::{UsageWindow, UsageWindowKind};

use crate::theme::ActiveTheme as _;

/// The window's display label, suffixed with its scope when the window is a
/// model-scoped sub-limit (e.g. "Weekly · Fable").
pub(crate) fn window_label(window: &UsageWindow) -> String {
    let base = match window.kind {
        UsageWindowKind::FiveHour => crate::tr!("usage.window.five_hour").into_owned(),
        UsageWindowKind::Weekly => crate::tr!("usage.window.weekly").into_owned(),
        UsageWindowKind::Other { minutes } => {
            crate::tr!("usage.window.minutes", count = minutes).into_owned()
        }
    };
    match &window.scope {
        Some(scope) => crate::tr!("usage.window.scoped", window = base, scope = scope).into_owned(),
        None => base,
    }
}

/// "Resets in 2h 15m" / "Resets now", or `None` when the provider did not
/// report a reset time.
pub(crate) fn resets_label(resets_at: Option<u64>, now: u64) -> Option<String> {
    let resets_at = resets_at?;
    if resets_at <= now {
        return Some(crate::tr!("usage.resets_now").into_owned());
    }
    let duration = crate::time::format_span(resets_at - now);
    Some(crate::tr!("usage.resets_in", duration = duration).into_owned())
}

/// The used percentage as a whole number (e.g. "83%").
pub(crate) fn percent_label(pct: f32) -> String {
    format!("{}%", pct.clamp(0.0, 100.0).round() as u32)
}

/// Bar tint: primary until 75%, warning to 90%, danger beyond.
pub(crate) fn bar_color(pct: f32, cx: &App) -> Hsla {
    let theme = cx.theme();
    if pct >= 90.0 {
        theme.danger
    } else if pct >= 75.0 {
        theme.warning
    } else {
        theme.primary
    }
}

/// A provider-reported plan id as a display name ("pro" → "Pro").
pub(crate) fn plan_label(plan: &str) -> String {
    let mut chars = plan.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
