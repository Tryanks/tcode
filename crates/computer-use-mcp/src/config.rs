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
});

pub fn set(config: ComputerUseSettings) {
    *CONFIG.write().unwrap() = config;
}

pub fn get() -> ComputerUseSettings {
    CONFIG.read().unwrap().clone()
}
