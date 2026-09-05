use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
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
const DM_SANS_PATH: &str = "fonts/DMSans[wght].ttf";

/// Extra SVG icons bundled by tcode (not shipped by gpui-component).
const EXTRA_ICONS: &[(&str, &[u8])] = &[
    #[cfg(target_arch = "wasm32")]
    ("icons/search.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/folder.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/layout-dashboard.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect width="7" height="9" x="3" y="3" rx="1"/><rect width="7" height="5" x="14" y="3" rx="1"/><rect width="7" height="9" x="14" y="12" rx="1"/><rect width="7" height="5" x="3" y="16" rx="1"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/plus.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M5 12h14"/><path d="M12 5v14"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/arrow-up.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 7-7 7 7"/><path d="M12 19V5"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/copy.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/check.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/chevron-down.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/chevron-right.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18 6-6-6-6"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/loader.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 2v4"/><path d="m16.2 7.8 2.9-2.9"/><path d="M18 12h4"/><path d="m16.2 16.2 2.9 2.9"/><path d="M12 18v4"/><path d="m4.9 19.1 2.9-2.9"/><path d="M2 12h4"/><path d="m4.9 4.9 2.9 2.9"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/close.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/info.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/triangle-alert.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"/><path d="M12 9v4"/><path d="M12 17h.01"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/circle-x.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="m15 9-6 6"/><path d="m9 9 6 6"/></svg>"#),
    #[cfg(target_arch = "wasm32")]
    ("icons/undo.svg", br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7v6h6"/><path d="M21 17a9 9 0 0 0-9-9 9 9 0 0 0-6 2.3L3 13"/></svg>"#),
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
    // Compact-shell icons (docs/mobile-design.md §3): the nav bar's back
    // chevron and the two empty-state glyphs. Inlined for the same reason as
    // `mic` — three files' worth of assets for the phone alone — and listed
    // unconditionally so the wasm build resolves them too.
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
            component_assets().load(path)
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        #[cfg(not(target_arch = "wasm32"))]
        let mut paths = ComponentAssets.list(path)?;
        #[cfg(target_arch = "wasm32")]
        let mut paths = component_assets().list(path)?;
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

#[cfg(target_arch = "wasm32")]
fn component_assets() -> &'static ComponentAssets {
    static ASSETS: std::sync::OnceLock<ComponentAssets> = std::sync::OnceLock::new();
    ASSETS.get_or_init(ComponentAssets::default)
}

#[cfg(test)]
mod tests {
    use super::*;

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
