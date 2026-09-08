use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
#[cfg(not(target_arch = "wasm32"))]
use gpui_component_assets::Assets as ComponentAssets;

pub const DM_SANS: &[u8] = include_bytes!("../../../assets/fonts/DMSans[wght].ttf");
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const LILEX_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf");
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const LILEX_BOLD: &[u8] = include_bytes!("../../../assets/fonts/lilex/Lilex-Bold.ttf");
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const LILEX_ITALIC: &[u8] = include_bytes!("../../../assets/fonts/lilex/Lilex-Italic.ttf");
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const LILEX_BOLD_ITALIC: &[u8] =
    include_bytes!("../../../assets/fonts/lilex/Lilex-BoldItalic.ttf");
#[cfg(target_family = "wasm")]
pub const TERMINAL_SYMBOLS: &[u8] =
    include_bytes!("../../../assets/fonts/nerd-symbols/TcodeTerminalSymbols.ttf");
const DM_SANS_PATH: &str = "fonts/DMSans[wght].ttf";

/// Extra SVG icons bundled by tcode (not shipped by gpui-component).
const EXTRA_ICONS: &[(&str, &[u8])] = &[
    (
        "icons/archive.svg",
        include_bytes!("../../../assets/icons/archive.svg"),
    ),
    (
        "icons/folder-plus.svg",
        include_bytes!("../../../assets/icons/folder-plus.svg"),
    ),
    (
        "icons/lock.svg",
        include_bytes!("../../../assets/icons/lock.svg"),
    ),
    (
        "icons/pencil.svg",
        include_bytes!("../../../assets/icons/pencil.svg"),
    ),
    (
        "icons/unlock.svg",
        include_bytes!("../../../assets/icons/unlock.svg"),
    ),
    (
        "icons/box.svg",
        include_bytes!("../../../assets/icons/box.svg"),
    ),
    (
        "icons/ruler.svg",
        include_bytes!("../../../assets/icons/ruler.svg"),
    ),
    (
        "icons/download.svg",
        include_bytes!("../../../assets/icons/download.svg"),
    ),
    (
        "icons/git-branch.svg",
        include_bytes!("../../../assets/icons/git-branch.svg"),
    ),
    (
        "icons/rotate-ccw.svg",
        include_bytes!("../../../assets/icons/rotate-ccw.svg"),
    ),
    (
        "icons/openai.svg",
        include_bytes!("../../../assets/icons/openai.svg"),
    ),
    (
        "icons/claude.svg",
        include_bytes!("../../../assets/icons/claude.svg"),
    ),
    (
        "icons/pi.svg",
        include_bytes!("../../../assets/icons/pi.svg"),
    ),
    (
        "icons/opencode.svg",
        include_bytes!("../../../assets/icons/opencode.svg"),
    ),
    (
        "icons/wrench.svg",
        include_bytes!("../../../assets/icons/wrench.svg"),
    ),
    (
        "icons/sparkles.svg",
        include_bytes!("../../../assets/icons/sparkles.svg"),
    ),
    ("icons/mic.svg", MIC_SVG.as_bytes()),
    // Register compact-shell icons for native and browser clients.
    ("icons/chevron-left.svg", CHEVRON_LEFT_SVG.as_bytes()),
    ("icons/message-square.svg", MESSAGE_SQUARE_SVG.as_bytes()),
    (
        "icons/monitor-smartphone.svg",
        MONITOR_SMARTPHONE_SVG.as_bytes(),
    ),
];

const CHEVRON_LEFT_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m15 18-6-6 6-6"/></svg>"#;
const MESSAGE_SQUARE_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 17a2 2 0 0 1-2 2H6l-4 4V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z"/></svg>"#;
const MONITOR_SMARTPHONE_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 8V6a2 2 0 0 0-2-2H4a2 2 0 0 0-2 2v7a2 2 0 0 0 2 2h8"/><path d="M10 19v-3.96 3.15"/><path d="M7 19h5"/><rect width="6" height="10" x="16" y="12" rx="2"/></svg>"#;

/// Lucide `mic`, inlined rather than shipped as a file: it is the composer
/// dictation button's only asset and the feature is macOS-only.
const MIC_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="lucide lucide-mic"><path d="M12 19v3"/><path d="M19 10v2a7 7 0 0 1-14 0v-2"/><rect x="9" y="2" width="6" height="13" rx="3"/></svg>"#;

/// App assets layered over gpui-component's built-in icon assets.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == DM_SANS_PATH {
            return Ok(Some(Cow::Borrowed(DM_SANS)));
        }
        if let Some((_, bytes)) = EXTRA_ICONS.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            ComponentAssets.load(path)
        }
        #[cfg(target_arch = "wasm32")]
        {
            Ok(embedded_component_icon(path).map(Cow::Borrowed))
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        #[cfg(not(target_arch = "wasm32"))]
        let mut paths = ComponentAssets.list(path)?;
        #[cfg(target_arch = "wasm32")]
        let mut paths = COMPONENT_ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect::<Vec<_>>();
        if DM_SANS_PATH.starts_with(path) {
            paths.push(DM_SANS_PATH.into());
        }
        for (name, _) in EXTRA_ICONS {
            if name.starts_with(path) {
                paths.push((*name).into());
            }
        }
        Ok(paths)
    }
}

#[cfg(any(target_arch = "wasm32", test))]
include!(concat!(env!("OUT_DIR"), "/component_icons.rs"));

#[cfg(any(target_arch = "wasm32", test))]
fn embedded_component_icon(path: &str) -> Option<&'static [u8]> {
    COMPONENT_ICONS
        .iter()
        .find_map(|(name, bytes)| (*name == path).then_some(*bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_navigation_icons_are_available_without_network() {
        // These controls rendered as blank hit targets when Web fell back to
        // the component package's asynchronous network loader.
        for path in [
            "icons/settings.svg",
            "icons/panel-left.svg",
            "icons/panel-right.svg",
            "icons/panel-bottom.svg",
            "icons/globe.svg",
            "icons/map.svg",
            "icons/square-terminal.svg",
        ] {
            let bytes = embedded_component_icon(path)
                .unwrap_or_else(|| panic!("browser icon must be embedded: {path}"));
            assert!(String::from_utf8_lossy(bytes).contains("<svg"), "{path}");
        }
        assert!(embedded_component_icon("icons/not-an-icon.svg").is_none());
    }

    #[test]
    fn component_caption_icons_load_through_assets_facade() {
        for path in [
            "icons/window-minimize.svg",
            "icons/window-maximize.svg",
            "icons/window-restore.svg",
            "icons/window-close.svg",
        ] {
            let bytes = AssetSource::load(&Assets, path)
                .unwrap_or_else(|error| panic!("failed to load {path}: {error}"))
                .unwrap_or_else(|| panic!("asset was not found: {path}"));

            assert!(!bytes.is_empty(), "asset was empty: {path}");
            assert!(
                String::from_utf8_lossy(&bytes).contains("<svg"),
                "asset was not an SVG document: {path}"
            );
        }
    }
}
