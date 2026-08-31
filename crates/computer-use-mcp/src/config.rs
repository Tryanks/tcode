//! Live feature configuration, pushed by the app whenever settings load or
//! change. The MCP server outlives any single settings snapshot, so tools read
//! the current value at call time instead of capturing one at startup.

use std::sync::RwLock;
use tcode_core::settings::ComputerUseSettings;
pub use tcode_core::settings::ImageMode;

static CONFIG: RwLock<ComputerUseSettings> = RwLock::new(ComputerUseSettings {
    enabled: false,
    allow_input: true,
    image_mode: ImageMode::Auto,
    allow_foreground_fallback: false,
    show_agent_cursor: true,
});

pub fn set(config: ComputerUseSettings) {
    *CONFIG.write().unwrap() = config;
}

pub fn get() -> ComputerUseSettings {
    CONFIG.read().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_background_settings_default_for_empty_and_legacy_values() {
        for json in [
            "{}",
            r#"{"enabled":true,"image_mode":"always","allow_input":false}"#,
        ] {
            let settings: ComputerUseSettings = serde_json::from_str(json).unwrap();
            assert!(!settings.allow_foreground_fallback);
            assert!(settings.show_agent_cursor);
        }

        let configured: ComputerUseSettings =
            serde_json::from_str(r#"{"allow_foreground_fallback":true,"show_agent_cursor":false}"#)
                .unwrap();
        assert!(configured.allow_foreground_fallback);
        assert!(!configured.show_agent_cursor);
    }
}
