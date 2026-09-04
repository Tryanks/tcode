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
];

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
