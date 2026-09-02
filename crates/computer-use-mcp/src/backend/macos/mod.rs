mod ax;
mod background;
mod capture;
mod focus;
mod input;
mod overlay;

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
    ActionKind, ActionRequest, ActionResult, BackendError, BackendErrorCode, Delivery,
    ObserveRequest, RootFilters, RootInfo, RootObservation, matches_root_filters,
};
use crate::outline::{UiNode, is_text_sparse};

use self::background::{BackgroundActivation, BackgroundDispatcher};
use self::focus::FocusGuard;

pub(super) struct MacosBackend;

impl MacosBackend {
    pub(super) fn list_roots(&self, filters: &RootFilters) -> Result<Vec<RootInfo>, BackendError> {
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
            if matches_root_filters(&root, filters) {
                roots.push(root);
            }
        }
        // CGWindowListCopyWindowInfo is documented front-to-back. Preserve that
        // ordering so the first result is the frontmost eligible root.
        Ok(roots)
    }

    pub(super) fn observe(
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
        let text_sparse = is_text_sparse(&tree);
        let should_capture = request.capture.should_capture(text_sparse);
        let screenshot = should_capture
            .then(|| capture::capture_window(root))
            .transpose()?;
        Ok(RootObservation {
            root: root.clone(),
            tree,
            text_sparse,
            screenshot,
            screenshot_mime: "image/jpeg",
        })
    }

    pub(super) fn perform_action(
        &self,
        root: &RootInfo,
        request: &ActionRequest,
    ) -> Result<ActionResult, BackendError> {
        reflect_overlay(root, request);
        match request.kind {
            ActionKind::Press => {
                let target = target(root, request)?;
                Ok(match target.press() {
                    Ok(()) => ActionResult::worked("AXPress completed", Delivery::Ax),
                    Err(error) => ActionResult::didnt(error.to_string(), Delivery::None),
                })
            }
            ActionKind::Click => {
                if request.target_path.is_some()
                    && request.target_actions.iter().any(|a| a == "press")
                {
                    let target = target(root, request)?;
                    if target.press().is_ok() {
                        return Ok(ActionResult::worked(
                            "AXPress completed for click target",
                            Delivery::Ax,
                        ));
                    }
                    let (x, y) = target.frame().center();
                    return Ok(
                        match background(root, |dispatcher| {
                            dispatcher.click(x, y, request.button, request.click_count)
                        }) {
                            Ok(()) => ActionResult::unknown(
                                "AXPress was rejected; click events were posted directly to the target pid",
                                Delivery::BackgroundPid,
                            ),
                            Err(error) => ActionResult::didnt(
                                format!(
                                    "AXPress was rejected; background click delivery failed: {error}"
                                ),
                                Delivery::None,
                            ),
                        },
                    );
                }
                let (x, y) = action_point(root, request)?;
                Ok(
                    match background(root, |dispatcher| {
                        dispatcher.click(x, y, request.button, request.click_count)
                    }) {
                        Ok(()) => ActionResult::unknown(
                            "click events were posted directly to the target pid",
                            Delivery::BackgroundPid,
                        ),
                        Err(error) => ActionResult::didnt(
                            format!("background click delivery failed: {error}"),
                            Delivery::None,
                        ),
                    },
                )
            }
            ActionKind::SetText => {
                let text = request.text.as_deref().ok_or_else(|| {
                    BackendError::new(BackendErrorCode::InvalidAction, "set_text requires text")
                })?;
                let target = target(root, request)?;
                match target.set_text(text) {
                    Ok(()) => Ok(ActionResult::worked("AXValue was set", Delivery::Ax)),
                    Err(ax_error) => {
                        let click_point = if target.focus().is_err() {
                            let frame = target.frame();
                            if !frame.has_area() {
                                return Ok(ActionResult::didnt(
                                    format!(
                                        "{ax_error}; the target also rejected focus and has no clickable frame"
                                    ),
                                    Delivery::None,
                                ));
                            }
                            Some(frame.center())
                        } else {
                            None
                        };
                        Ok(
                            match background(root, |dispatcher| {
                                if let Some((x, y)) = click_point {
                                    dispatcher.click(x, y, super::MouseButton::Left, 1)?;
                                }
                                dispatcher.keypress(&["cmd+a".into()])?;
                                dispatcher.type_text(text)
                            }) {
                                Ok(()) => ActionResult::unknown(
                                    format!(
                                        "{ax_error}; keyboard replacement events were posted directly to the target pid"
                                    ),
                                    Delivery::BackgroundPid,
                                ),
                                Err(error) => ActionResult::didnt(
                                    format!(
                                        "{ax_error}; background keyboard replacement delivery failed: {error}"
                                    ),
                                    Delivery::None,
                                ),
                            },
                        )
                    }
                }
            }
            ActionKind::TypeText => {
                let text = request.text.as_deref().ok_or_else(|| {
                    BackendError::new(BackendErrorCode::InvalidAction, "type_text requires text")
                })?;
                let click_point = if request.target_path.is_some() {
                    let target = target(root, request)?;
                    if target.focus().is_err() {
                        let frame = target.frame();
                        if !frame.has_area() {
                            return Ok(ActionResult::didnt(
                                "target rejected focus and has no clickable frame",
                                Delivery::None,
                            ));
                        }
                        Some(frame.center())
                    } else {
                        None
                    }
                } else {
                    None
                };
                let attempt = background(root, |dispatcher| {
                    if let Some((x, y)) = click_point {
                        dispatcher.click(x, y, super::MouseButton::Left, 1)?;
                    }
                    dispatcher.type_text(text)
                });
                Ok(keyboard_result_with_optional_foreground(
                    root,
                    attempt,
                    |root| {
                        let focus_guard = FocusGuard::acquire(root);
                        if !focus_guard.is_ready() {
                            return Err(BackendError::new(
                                BackendErrorCode::OperationFailed,
                                "foreground HID retry could not activate and raise the target window",
                            ));
                        }
                        if let Some((x, y)) = click_point {
                            input::click(x, y, super::MouseButton::Left, 1)?;
                        }
                        input::type_text(text)
                    },
                    "Unicode keyboard events",
                ))
            }
            ActionKind::Keypress => {
                let keys = request.keys.as_deref().ok_or_else(|| {
                    BackendError::new(BackendErrorCode::InvalidAction, "keypress requires keys")
                })?;
                let click_point = if request.target_path.is_some() {
                    let target = target(root, request)?;
                    if target.focus().is_err() {
                        let frame = target.frame();
                        if !frame.has_area() {
                            return Ok(ActionResult::didnt(
                                "keypress target rejected focus and has no clickable frame",
                                Delivery::None,
                            ));
                        }
                        Some(frame.center())
                    } else {
                        None
                    }
                } else {
                    None
                };
                let attempt = background(root, |dispatcher| {
                    if let Some((x, y)) = click_point {
                        dispatcher.click(x, y, super::MouseButton::Left, 1)?;
                    }
                    dispatcher.keypress(keys)
                });
                Ok(keyboard_result_with_optional_foreground(
                    root,
                    attempt,
                    |root| {
                        let focus_guard = FocusGuard::acquire(root);
                        if !focus_guard.is_ready() {
                            return Err(BackendError::new(
                                BackendErrorCode::OperationFailed,
                                "foreground HID retry could not activate and raise the target window",
                            ));
                        }
                        if let Some((x, y)) = click_point {
                            input::click(x, y, super::MouseButton::Left, 1)?;
                        }
                        input::keypress(keys)
                    },
                    "keyboard events",
                ))
            }
            ActionKind::Scroll => {
                let action_point = if request.target_path.is_some()
                    || (request.x.is_some() && request.y.is_some())
                {
                    Some(action_point(root, request)?)
                } else {
                    None
                };
                Ok(
                    match background(root, |dispatcher| {
                        if let Some((x, y)) = action_point {
                            dispatcher.move_mouse(x, y)?;
                        }
                        dispatcher.scroll(
                            request.scroll_x.unwrap_or(0.0),
                            request.scroll_y.unwrap_or(0.0),
                        )
                    }) {
                        Ok(()) => ActionResult::unknown(
                            "scroll-wheel events were posted directly to the target pid",
                            Delivery::BackgroundPid,
                        ),
                        Err(error) => ActionResult::didnt(
                            format!("background scroll delivery failed: {error}"),
                            Delivery::None,
                        ),
                    },
                )
            }
            ActionKind::Drag => {
                let path = request.path.as_deref().ok_or_else(|| {
                    BackendError::new(BackendErrorCode::InvalidAction, "drag requires a path")
                })?;
                Ok(
                    match background(root, |dispatcher| dispatcher.drag(path, request.button)) {
                        Ok(()) => ActionResult::unknown(
                            "drag events were posted directly to the target pid",
                            Delivery::BackgroundPid,
                        ),
                        Err(error) => ActionResult::didnt(
                            format!("background drag delivery failed: {error}"),
                            Delivery::None,
                        ),
                    },
                )
            }
            ActionKind::MoveMouse => {
                let (x, y) = action_point(root, request)?;
                Ok(
                    match background(root, |dispatcher| dispatcher.move_mouse(x, y)) {
                        Ok(()) => ActionResult::unknown(
                            "mouse-move event was posted directly to the target pid",
                            Delivery::BackgroundPid,
                        ),
                        Err(error) => ActionResult::didnt(
                            format!("background mouse-move delivery failed: {error}"),
                            Delivery::None,
                        ),
                    },
                )
            }
        }
    }
}

pub(super) fn frontmost_pid() -> Option<u32> {
    ax::frontmost_application_pid()
}

fn background(
    root: &RootInfo,
    action: impl FnOnce(&BackgroundDispatcher) -> Result<(), BackendError>,
) -> Result<(), BackendError> {
    let _activation = BackgroundActivation::acquire(root)?;
    let dispatcher = BackgroundDispatcher::new(root)?;
    action(&dispatcher)
}

fn keyboard_result_with_optional_foreground(
    root: &RootInfo,
    background_attempt: Result<(), BackendError>,
    foreground_attempt: impl FnOnce(&RootInfo) -> Result<(), BackendError>,
    action_name: &str,
) -> ActionResult {
    match background_attempt {
        Ok(()) => ActionResult::unknown(
            format!("{action_name} were posted directly to the target pid"),
            Delivery::BackgroundPid,
        ),
        Err(background_error) if crate::config::get().allow_foreground_fallback => {
            match foreground_attempt(root) {
                Ok(()) => ActionResult::unknown(
                    format!(
                        "background PID delivery failed ({background_error}); {action_name} were retried through foreground HID delivery"
                    ),
                    Delivery::ForegroundHid,
                ),
                Err(foreground_error) => ActionResult::didnt(
                    format!(
                        "background PID delivery failed ({background_error}); foreground HID retry also failed: {foreground_error}"
                    ),
                    Delivery::None,
                ),
            }
        }
        Err(error) => ActionResult::didnt(
            format!("background PID delivery failed: {error}"),
            Delivery::None,
        ),
    }
}

fn reflect_overlay(root: &RootInfo, request: &ActionRequest) {
    let enabled = crate::config::get().show_agent_cursor;
    overlay::set_enabled(enabled);
    if !enabled {
        return;
    }
    use overlay::OverlayActionKind as K;
    match request.kind {
        ActionKind::Drag => {
            if let Some(path) = request.path.as_ref()
                && let (Some(first), Some(last)) = (path.first(), path.last())
            {
                overlay::show_drag(root.pid, (first[0], first[1]), (last[0], last[1]));
            }
        }
        ActionKind::TypeText | ActionKind::SetText | ActionKind::Keypress => {
            if let Ok(point) = action_point(root, request) {
                overlay::show_action(root.pid, K::Keyboard, point);
            }
        }
        other => {
            let kind = match other {
                ActionKind::Scroll => K::Scroll,
                ActionKind::MoveMouse => K::Move,
                _ => K::Click,
            };
            if let Ok(point) = action_point(root, request) {
                overlay::show_action(root.pid, kind, point);
            }
        }
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
