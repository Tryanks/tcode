mod ax;
mod capture;
mod input;

use std::collections::HashMap;
use std::ffi::c_void;

use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex};
use core_foundation::base::{CFGetTypeID, CFTypeRef, TCFType};
use core_foundation::dictionary::{CFDictionaryGetTypeID, CFDictionaryGetValue, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::window::{
    kCGNullWindowID, kCGWindowBounds, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionOnScreenOnly, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
    kCGWindowOwnerPID,
};

use super::{
    ActionKind, ActionRequest, ActionResult, Backend, BackendError, BackendErrorCode,
    CapturePolicy, ObserveRequest, RootFilters, RootInfo, RootObservation,
};
use crate::outline::{UiNode, interactive_count};

pub(super) struct MacosBackend;

impl Backend for MacosBackend {
    fn list_roots(&self, filters: &RootFilters) -> Result<Vec<RootInfo>, BackendError> {
        let options = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
        let array =
            core_graphics::window::copy_window_info(options, kCGNullWindowID).ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::ObservationFailed,
                    "CGWindowListCopyWindowInfo returned no window list",
                )
            })?;
        let array_ref = array.as_concrete_TypeRef();
        let count = unsafe { CFArrayGetCount(array_ref) }.max(0) as usize;
        let mut roots = Vec::new();
        let mut identifiers = HashMap::<u32, String>::new();
        for index in 0..count {
            let dictionary =
                unsafe { CFArrayGetValueAtIndex(array_ref, index as isize) } as CFDictionaryRef;
            if dictionary.is_null()
                || unsafe { CFGetTypeID(dictionary.cast()) } != unsafe { CFDictionaryGetTypeID() }
            {
                continue;
            }
            let layer = dictionary_i64(dictionary, unsafe { kCGWindowLayer }).unwrap_or(-1);
            if layer != 0 {
                continue;
            }
            let pid = dictionary_i64(dictionary, unsafe { kCGWindowOwnerPID })
                .and_then(|pid| u32::try_from(pid).ok())
                .unwrap_or_default();
            let window_id = dictionary_i64(dictionary, unsafe { kCGWindowNumber })
                .and_then(|id| u32::try_from(id).ok())
                .unwrap_or_default();
            if pid == 0 || window_id == 0 {
                continue;
            }
            let app_name =
                dictionary_string(dictionary, unsafe { kCGWindowOwnerName }).unwrap_or_default();
            let title = dictionary_string(dictionary, unsafe { kCGWindowName }).unwrap_or_default();
            let frame = dictionary_value(dictionary, unsafe { kCGWindowBounds })
                .and_then(|value| window_bounds(value.cast()))
                .unwrap_or_default();
            if !frame.has_area() {
                continue;
            }
            let bundle_id = identifiers
                .entry(pid)
                .or_insert_with(|| ax::application_identifier(pid))
                .clone();
            let mut root = RootInfo {
                ref_id: String::new(),
                app_name,
                bundle_id,
                pid,
                title,
                kind: super::RootKind::Window,
                window_id,
                frame,
            };
            root.kind = ax::root_kind(&root);
            if matches_filters(&root, filters) {
                roots.push(root);
            }
        }
        // CGWindowListCopyWindowInfo is documented front-to-back. Preserve that
        // ordering so the first result is the frontmost eligible root.
        Ok(roots)
    }

    fn observe(
        &self,
        root: &RootInfo,
        request: ObserveRequest,
    ) -> Result<RootObservation, BackendError> {
        let tree = if request.semantic {
            ax::observe_tree(root)?
        } else {
            UiNode {
                role: root.kind.to_string(),
                title: root.title.clone(),
                frame: root.frame,
                enabled: true,
                ..UiNode::default()
            }
        };
        let should_capture = match request.capture {
            CapturePolicy::Never => false,
            CapturePolicy::Always => true,
            CapturePolicy::IfSparse => interactive_count(&tree) <= 3,
        };
        let screenshot_png = should_capture
            .then(|| capture::capture_window(root))
            .transpose()?;
        Ok(RootObservation {
            root: root.clone(),
            tree,
            screenshot_png,
        })
    }

    fn perform_action(
        &self,
        root: &RootInfo,
        request: &ActionRequest,
    ) -> Result<ActionResult, BackendError> {
        match request.kind {
            ActionKind::Press => {
                let target = target(root, request)?;
                Ok(match target.press() {
                    Ok(()) => ActionResult::worked("AXPress completed"),
                    Err(error) => ActionResult::didnt(error.to_string()),
                })
            }
            ActionKind::Click => {
                if request.target_path.is_some()
                    && request.target_actions.iter().any(|a| a == "press")
                {
                    let target = target(root, request)?;
                    if target.press().is_ok() {
                        return Ok(ActionResult::worked("AXPress completed for click target"));
                    }
                    let (x, y) = target.frame().center();
                    input::click(x, y, request.button, request.click_count)?;
                    return Ok(ActionResult::unknown(
                        "AXPress was rejected; physical click events were posted",
                    ));
                }
                let (x, y) = action_point(root, request)?;
                input::click(x, y, request.button, request.click_count)?;
                Ok(ActionResult::unknown("physical click events were posted"))
            }
            ActionKind::SetText => {
                let text = request.text.as_deref().ok_or_else(|| {
                    BackendError::new(BackendErrorCode::InvalidAction, "set_text requires text")
                })?;
                let target = target(root, request)?;
                match target.set_text(text) {
                    Ok(()) => Ok(ActionResult::worked("AXValue was set")),
                    Err(ax_error) => {
                        if target.focus().is_err() {
                            let frame = target.frame();
                            if !frame.has_area() {
                                return Ok(ActionResult::didnt(format!(
                                    "{ax_error}; the target also rejected focus and has no clickable frame"
                                )));
                            }
                            let (x, y) = frame.center();
                            input::click(x, y, super::MouseButton::Left, 1)?;
                        }
                        input::keypress(&["cmd+a".into()])?;
                        input::type_text(text)?;
                        Ok(ActionResult::unknown(format!(
                            "{ax_error}; keyboard replacement events were posted instead"
                        )))
                    }
                }
            }
            ActionKind::TypeText => {
                let text = request.text.as_deref().ok_or_else(|| {
                    BackendError::new(BackendErrorCode::InvalidAction, "type_text requires text")
                })?;
                if request.target_path.is_some() {
                    let target = target(root, request)?;
                    if target.focus().is_err() {
                        let frame = target.frame();
                        if !frame.has_area() {
                            return Ok(ActionResult::didnt(
                                "target rejected focus and has no clickable frame",
                            ));
                        }
                        let (x, y) = frame.center();
                        input::click(x, y, super::MouseButton::Left, 1)?;
                    }
                }
                input::type_text(text)?;
                Ok(ActionResult::unknown("Unicode keyboard events were posted"))
            }
            ActionKind::Keypress => {
                if request.target_path.is_some() {
                    let target = target(root, request)?;
                    if target.focus().is_err() {
                        let frame = target.frame();
                        if !frame.has_area() {
                            return Ok(ActionResult::didnt(
                                "keypress target rejected focus and has no clickable frame",
                            ));
                        }
                        let (x, y) = frame.center();
                        input::click(x, y, super::MouseButton::Left, 1)?;
                    }
                }
                let keys = request.keys.as_deref().ok_or_else(|| {
                    BackendError::new(BackendErrorCode::InvalidAction, "keypress requires keys")
                })?;
                input::keypress(keys)?;
                Ok(ActionResult::unknown("keyboard events were posted"))
            }
            ActionKind::Scroll => {
                if request.target_path.is_some() || (request.x.is_some() && request.y.is_some()) {
                    let (x, y) = action_point(root, request)?;
                    input::move_mouse(x, y)?;
                }
                input::scroll(
                    request.scroll_x.unwrap_or(0.0),
                    request.scroll_y.unwrap_or(0.0),
                )?;
                Ok(ActionResult::unknown("scroll-wheel events were posted"))
            }
            ActionKind::Drag => {
                let path = request.path.as_deref().ok_or_else(|| {
                    BackendError::new(BackendErrorCode::InvalidAction, "drag requires a path")
                })?;
                input::drag(path, request.button)?;
                Ok(ActionResult::unknown("drag events were posted"))
            }
            ActionKind::MoveMouse => {
                let (x, y) = action_point(root, request)?;
                input::move_mouse(x, y)?;
                Ok(ActionResult::unknown("mouse-move event was posted"))
            }
        }
    }

    fn read_element_text(
        &self,
        root: &RootInfo,
        target_path: &[usize],
    ) -> Result<String, BackendError> {
        ax::read_target_text(root, target_path)
    }
}

fn target(root: &RootInfo, request: &ActionRequest) -> Result<ax::Target, BackendError> {
    let path = request.target_path.as_deref().ok_or_else(|| {
        BackendError::new(
            BackendErrorCode::InvalidAction,
            "this action requires an element ref",
        )
    })?;
    ax::locate_target(
        root,
        path,
        request.target_role.as_deref(),
        request.target_title.as_deref(),
    )
}

fn action_point(root: &RootInfo, request: &ActionRequest) -> Result<(f64, f64), BackendError> {
    if let (Some(x), Some(y)) = (request.x, request.y) {
        return Ok((x, y));
    }
    if request.target_path.is_some() {
        let live_frame = target(root, request)?.frame();
        if live_frame.has_area() {
            return Ok(live_frame.center());
        }
    }
    if let Some(frame) = request.target_frame.filter(|frame| frame.has_area()) {
        return Ok(frame.center());
    }
    Err(BackendError::new(
        BackendErrorCode::InvalidAction,
        "action requires x/y coordinates or an element with a non-empty frame",
    ))
}

fn matches_filters(root: &RootInfo, filters: &RootFilters) -> bool {
    if filters.pid.is_some_and(|pid| root.pid != pid)
        || filters.kind.is_some_and(|kind| root.kind != kind)
    {
        return false;
    }
    if filters
        .app
        .as_deref()
        .is_some_and(|app| !contains_case_insensitive(&root.app_name, app))
        || filters
            .bundle_id
            .as_deref()
            .is_some_and(|bundle| !contains_case_insensitive(&root.bundle_id, bundle))
    {
        return false;
    }
    filters.text.as_deref().is_none_or(|text| {
        contains_case_insensitive(&root.app_name, text)
            || contains_case_insensitive(&root.title, text)
            || contains_case_insensitive(&root.bundle_id, text)
    })
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn dictionary_value(dictionary: CFDictionaryRef, key: CFStringRef) -> Option<*const c_void> {
    let value = unsafe { CFDictionaryGetValue(dictionary, key.cast()) };
    (!value.is_null()).then_some(value)
}

fn dictionary_i64(dictionary: CFDictionaryRef, key: CFStringRef) -> Option<i64> {
    let value = dictionary_value(dictionary, key)? as CFTypeRef;
    if unsafe { CFGetTypeID(value) } != CFNumber::type_id() {
        return None;
    }
    // SAFETY: type checked; dictionary owns the borrowed value for this scope.
    unsafe { CFNumber::wrap_under_get_rule(value.cast()) }.to_i64()
}

fn dictionary_string(dictionary: CFDictionaryRef, key: CFStringRef) -> Option<String> {
    let value = dictionary_value(dictionary, key)? as CFTypeRef;
    if unsafe { CFGetTypeID(value) } != CFString::type_id() {
        return None;
    }
    // SAFETY: type checked; dictionary owns the borrowed value for this scope.
    Some(unsafe { CFString::wrap_under_get_rule(value.cast()) }.to_string())
}

fn window_bounds(dictionary: CFDictionaryRef) -> Option<crate::outline::Frame> {
    if unsafe { CFGetTypeID(dictionary.cast()) } != unsafe { CFDictionaryGetTypeID() } {
        return None;
    }
    let number = |key: &str| {
        let key = CFString::new(key);
        let value = dictionary_value(dictionary, key.as_concrete_TypeRef())? as CFTypeRef;
        if unsafe { CFGetTypeID(value) } != CFNumber::type_id() {
            return None;
        }
        // SAFETY: type checked; dictionary owns the borrowed value for this scope.
        unsafe { CFNumber::wrap_under_get_rule(value.cast()) }.to_f64()
    };
    Some(crate::outline::Frame {
        x: number("X")?,
        y: number("Y")?,
        w: number("Width")?,
        h: number("Height")?,
    })
}
