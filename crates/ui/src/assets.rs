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
];

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
        ComponentAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = ComponentAssets.list(path)?;
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
