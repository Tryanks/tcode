//! macOS TCC permission checks and requests, shared by the computer-use
//! backend and the Settings → Computer Use / Browser pages.
//!
//! tcode is itself the signed `.app` the grants attach to, so there is no
//! helper-app attribution to worry about. Screen Recording grants only take
//! effect after the app restarts (macOS shows its own "Quit & Reopen" dialog);
//! callers must persist any restart-continuity marker *before* requesting.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    Accessibility,
    ScreenRecording,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionStatus {
    pub accessibility: bool,
    pub screen_recording: bool,
}

impl PermissionStatus {
    pub fn granted(&self, kind: PermissionKind) -> bool {
        match kind {
            PermissionKind::Accessibility => self.accessibility,
            PermissionKind::ScreenRecording => self.screen_recording,
        }
    }

    pub fn all_granted(&self) -> bool {
        self.accessibility && self.screen_recording
    }
}

/// Non-prompting snapshot of both TCC grants for this process.
pub fn check() -> PermissionStatus {
    imp::check()
}

/// Fire the OS prompt for one permission kind. Accessibility prompts inline;
/// Screen Recording prompts at most once per TCC reset, after which the user
/// must flip the toggle in System Settings — pair this with
/// [`open_settings_pane`]. Returns the (possibly already-granted) status.
pub fn request(kind: PermissionKind) -> bool {
    imp::request(kind)
}

/// Deep-link System Settings to the Privacy & Security pane for `kind`.
pub fn open_settings_pane(kind: PermissionKind) {
    imp::open_settings_pane(kind)
}

/// Start a fresh instance of tcode and return; the caller is responsible for
/// quitting the current instance afterwards. Prefers relaunching the enclosing
/// `.app` bundle (so LaunchServices identity — and thus TCC attribution — is
/// preserved); falls back to re-spawning the bare executable in dev builds.
pub fn relaunch_app() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let bundle = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(std::path::Path::to_path_buf);
    match bundle {
        Some(app) => {
            std::process::Command::new("open")
                .arg("-n")
                .arg(app)
                .spawn()?;
        }
        None => {
            std::process::Command::new(exe).spawn()?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{PermissionKind, PermissionStatus};
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::{CFString, CFStringRef};

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    pub(super) fn check() -> PermissionStatus {
        PermissionStatus {
            accessibility: unsafe { AXIsProcessTrusted() },
            screen_recording: unsafe { CGPreflightScreenCaptureAccess() },
        }
    }

    pub(super) fn request(kind: PermissionKind) -> bool {
        match kind {
            PermissionKind::Accessibility => unsafe {
                let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
                let options = CFDictionary::from_CFType_pairs(&[(
                    key.as_CFType(),
                    CFBoolean::true_value().as_CFType(),
                )]);
                AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
            },
            PermissionKind::ScreenRecording => unsafe { CGRequestScreenCaptureAccess() },
        }
    }

    pub(super) fn open_settings_pane(kind: PermissionKind) {
        let pane = match kind {
            PermissionKind::Accessibility => "Privacy_Accessibility",
            PermissionKind::ScreenRecording => "Privacy_ScreenCapture",
        };
        let url = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{PermissionKind, PermissionStatus};

    pub(super) fn check() -> PermissionStatus {
        // Non-macOS platforms have no TCC; the backend is a stub there and the
        // settings UI shows the platform as unsupported rather than ungranted.
        PermissionStatus::default()
    }

    pub(super) fn request(_kind: PermissionKind) -> bool {
        false
    }

    pub(super) fn open_settings_pane(_kind: PermissionKind) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_granted_maps_kinds() {
        let status = PermissionStatus {
            accessibility: true,
            screen_recording: false,
        };
        assert!(status.granted(PermissionKind::Accessibility));
        assert!(!status.granted(PermissionKind::ScreenRecording));
        assert!(!status.all_granted());
    }

    #[test]
    fn kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&PermissionKind::ScreenRecording).unwrap(),
            "\"screen_recording\""
        );
    }
}
