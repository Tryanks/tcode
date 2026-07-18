//! Live feature configuration, pushed by the app whenever settings load or
//! change. The MCP server outlives any single settings snapshot, so tools read
//! the current value at call time instead of capturing one at startup.

use std::sync::RwLock;

/// When observations include a screenshot alongside the outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageMode {
    /// Screenshot only when the accessibility outline looks too sparse to act on.
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputerUseConfig {
    /// When false the tools are observe-only: `act_ui` rejects every action.
    pub allow_input: bool,
    pub image_mode: ImageMode,
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            allow_input: true,
            image_mode: ImageMode::Auto,
        }
    }
}

static CONFIG: RwLock<ComputerUseConfig> = RwLock::new(ComputerUseConfig {
    allow_input: true,
    image_mode: ImageMode::Auto,
});

pub fn set(config: ComputerUseConfig) {
    *CONFIG.write().unwrap() = config;
}

pub fn get() -> ComputerUseConfig {
    *CONFIG.read().unwrap()
}
