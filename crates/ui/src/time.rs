#[cfg(not(target_family = "wasm"))]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(target_family = "wasm")]
use web_time::{SystemTime, UNIX_EPOCH};

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Compact relative-time label (e.g. "5m ago") from an elapsed-seconds count.
pub fn humanize_ago(secs: u64) -> String {
    if secs < 60 {
        crate::tr!("time.just_now").into_owned()
    } else if secs < 3600 {
        crate::tr!("time.minutes_ago", count = secs / 60).into_owned()
    } else if secs < 86_400 {
        crate::tr!("time.hours_ago", count = secs / 3600).into_owned()
    } else {
        crate::tr!("time.days_ago", count = secs / 86_400).into_owned()
    }
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
