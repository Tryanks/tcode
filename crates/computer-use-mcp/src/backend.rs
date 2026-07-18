//! Platform backend contract and platform-neutral input descriptions.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::outline::{Frame, UiNode};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(target_os = "macos"))]
mod stub;

const UNSUPPORTED_MESSAGE: &str = "computer use is unsupported on this platform";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    #[default]
    Window,
    Dialog,
    Sheet,
    Menu,
    Popover,
}

impl fmt::Display for RootKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Window => "window",
            Self::Dialog => "dialog",
            Self::Sheet => "sheet",
            Self::Menu => "menu",
            Self::Popover => "popover",
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RootInfo {
    pub ref_id: String,
    pub app_name: String,
    pub bundle_id: String,
    pub pid: u32,
    pub title: String,
    pub kind: RootKind,
    pub window_id: u32,
    pub frame: Frame,
}

impl RootInfo {
    pub fn identity(&self) -> String {
        format!("{}:{}", self.pid, self.window_id)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RootFilters {
    pub text: Option<String>,
    pub app: Option<String>,
    pub bundle_id: Option<String>,
    pub pid: Option<u32>,
    pub kind: Option<RootKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePolicy {
    Never,
    Always,
    IfSparse,
}

#[derive(Debug, Clone, Copy)]
pub struct ObserveRequest {
    pub semantic: bool,
    pub capture: CapturePolicy,
}

#[derive(Debug, Clone)]
pub struct RootObservation {
    pub root: RootInfo,
    pub tree: UiNode,
    pub screenshot_png: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Press,
    Click,
    SetText,
    TypeText,
    Keypress,
    Scroll,
    Drag,
    MoveMouse,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone)]
pub struct ActionRequest {
    pub kind: ActionKind,
    pub target_path: Option<Vec<usize>>,
    pub target_frame: Option<Frame>,
    pub target_role: Option<String>,
    pub target_title: Option<String>,
    pub target_actions: Vec<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub text: Option<String>,
    pub keys: Option<Vec<String>>,
    pub scroll_x: Option<f64>,
    pub scroll_y: Option<f64>,
    pub path: Option<Vec<[f64; 2]>>,
    pub button: MouseButton,
    pub click_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Worked,
    Didnt,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionResult {
    pub outcome: ActionOutcome,
    pub message: String,
}

impl ActionResult {
    pub fn worked(message: impl Into<String>) -> Self {
        Self {
            outcome: ActionOutcome::Worked,
            message: message.into(),
        }
    }

    pub fn didnt(message: impl Into<String>) -> Self {
        Self {
            outcome: ActionOutcome::Didnt,
            message: message.into(),
        }
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self {
            outcome: ActionOutcome::Unknown,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendErrorCode {
    UnsupportedPlatform,
    RootNotFound,
    ObservationFailed,
    CaptureFailed,
    InvalidAction,
    OperationFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackendError {
    pub code: BackendErrorCode,
    pub message: String,
}

impl BackendError {
    pub fn new(code: BackendErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn unsupported() -> Self {
        Self::new(BackendErrorCode::UnsupportedPlatform, UNSUPPORTED_MESSAGE)
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for BackendError {}

pub trait Backend: Send + Sync {
    fn list_roots(&self, filters: &RootFilters) -> Result<Vec<RootInfo>, BackendError>;

    fn observe(
        &self,
        root: &RootInfo,
        request: ObserveRequest,
    ) -> Result<RootObservation, BackendError>;

    fn perform_action(
        &self,
        root: &RootInfo,
        request: &ActionRequest,
    ) -> Result<ActionResult, BackendError>;

    fn read_element_text(
        &self,
        root: &RootInfo,
        target_path: &[usize],
    ) -> Result<String, BackendError>;
}

pub fn platform_backend() -> Box<dyn Backend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacosBackend)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(stub::StubBackend)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyModifiers {
    pub command: bool,
    pub control: bool,
    pub option: bool,
    pub shift: bool,
    pub function: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    pub keycode: u16,
    pub modifiers: KeyModifiers,
}

pub fn parse_key_chord(keys: &[String]) -> Result<KeyChord, String> {
    let parts: Vec<String> = keys
        .iter()
        .flat_map(|part| part.split('+'))
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("keypress requires a key name or chord".into());
    }

    let mut modifiers = KeyModifiers::default();
    let mut keycode = None;
    for part in parts {
        match part.as_str() {
            "cmd" | "command" | "meta" => modifiers.command = true,
            "ctrl" | "control" => modifiers.control = true,
            "alt" | "option" => modifiers.option = true,
            "shift" => modifiers.shift = true,
            "fn" | "function" => modifiers.function = true,
            key => {
                if keycode.is_some() {
                    return Err("keypress accepts exactly one non-modifier key".into());
                }
                keycode = keycode_for_name(key);
                if keycode.is_none() {
                    return Err(format!("unknown key name: {key}"));
                }
            }
        }
    }
    let keycode = keycode.ok_or_else(|| "keypress chord has no non-modifier key".to_string())?;
    Ok(KeyChord { keycode, modifiers })
}

/// US ANSI virtual key codes. Key names are layout-independent controls or
/// physical letter/number keys; text entry uses Unicode events instead.
pub fn keycode_for_name(name: &str) -> Option<u16> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "a" => 0x00,
        "s" => 0x01,
        "d" => 0x02,
        "f" => 0x03,
        "h" => 0x04,
        "g" => 0x05,
        "z" => 0x06,
        "x" => 0x07,
        "c" => 0x08,
        "v" => 0x09,
        "b" => 0x0B,
        "q" => 0x0C,
        "w" => 0x0D,
        "e" => 0x0E,
        "r" => 0x0F,
        "y" => 0x10,
        "t" => 0x11,
        "1" => 0x12,
        "2" => 0x13,
        "3" => 0x14,
        "4" => 0x15,
        "6" => 0x16,
        "5" => 0x17,
        "=" | "equal" => 0x18,
        "9" => 0x19,
        "7" => 0x1A,
        "-" | "minus" => 0x1B,
        "8" => 0x1C,
        "0" => 0x1D,
        "]" | "right_bracket" => 0x1E,
        "o" => 0x1F,
        "u" => 0x20,
        "[" | "left_bracket" => 0x21,
        "i" => 0x22,
        "p" => 0x23,
        "enter" | "return" => 0x24,
        "l" => 0x25,
        "j" => 0x26,
        "'" | "quote" => 0x27,
        "k" => 0x28,
        ";" | "semicolon" => 0x29,
        "\\" | "backslash" => 0x2A,
        "," | "comma" => 0x2B,
        "/" | "slash" => 0x2C,
        "n" => 0x2D,
        "m" => 0x2E,
        "." | "period" => 0x2F,
        "tab" => 0x30,
        "space" => 0x31,
        "`" | "grave" => 0x32,
        "delete" | "backspace" => 0x33,
        "escape" | "esc" => 0x35,
        "f17" => 0x40,
        "f18" => 0x4F,
        "f19" => 0x50,
        "f20" => 0x5A,
        "f5" => 0x60,
        "f6" => 0x61,
        "f7" => 0x62,
        "f3" => 0x63,
        "f8" => 0x64,
        "f9" => 0x65,
        "f11" => 0x67,
        "f13" => 0x69,
        "f16" => 0x6A,
        "f14" => 0x6B,
        "f10" => 0x6D,
        "f12" => 0x6F,
        "f15" => 0x71,
        "help" | "insert" => 0x72,
        "home" => 0x73,
        "page_up" | "pageup" => 0x74,
        "forward_delete" => 0x75,
        "f4" => 0x76,
        "end" => 0x77,
        "f2" => 0x78,
        "page_down" | "pagedown" => 0x79,
        "f1" => 0x7A,
        "left" | "left_arrow" => 0x7B,
        "right" | "right_arrow" => 0x7C,
        "down" | "down_arrow" => 0x7D,
        "up" | "up_arrow" => 0x7E,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_and_chords_map_to_macos_virtual_codes() {
        assert_eq!(keycode_for_name("enter"), Some(0x24));
        assert_eq!(keycode_for_name("left_arrow"), Some(0x7B));
        assert_eq!(keycode_for_name("F12"), Some(0x6F));
        assert_eq!(keycode_for_name("definitely-not-a-key"), None);

        let chord = parse_key_chord(&["cmd+shift+s".into()]).unwrap();
        assert_eq!(chord.keycode, 0x01);
        assert!(chord.modifiers.command);
        assert!(chord.modifiers.shift);
    }
}
