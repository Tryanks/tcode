mod capture;
mod input;
mod pure;
mod uia;

use super::{
    ActionKind, ActionRequest, ActionResult, BackendError, BackendErrorCode, ObserveRequest,
    RootFilters, RootInfo, RootObservation,
};
use crate::outline::{UiNode, is_text_sparse};

pub(super) struct WindowsBackend;

impl WindowsBackend {
    pub(super) fn list_roots(&self, filters: &RootFilters) -> Result<Vec<RootInfo>, BackendError> {
        uia::list_roots(filters)
    }

    pub(super) fn observe(
        &self,
        root: &RootInfo,
        request: ObserveRequest,
    ) -> Result<RootObservation, BackendError> {
        let tree = if request.semantic {
            uia::observe_tree(root)?
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
        let screenshot_png = should_capture
            .then(|| capture::capture_window(root))
            .transpose()?;
        Ok(RootObservation {
            root: root.clone(),
            tree,
            text_sparse,
            screenshot_png,
        })
    }

    pub(super) fn perform_action(
        &self,
        root: &RootInfo,
        request: &ActionRequest,
    ) -> Result<ActionResult, BackendError> {
        match request.kind {
            ActionKind::Press => {
                let target = target(root, request)?;
                Ok(match target.press() {
                    Ok(message) => ActionResult::worked(message),
                    Err(uia_error) => {
                        let live_frame = target.frame();
                        let frame = live_frame
                            .has_area()
                            .then_some(live_frame)
                            .or_else(|| request.target_frame.filter(|frame| frame.has_area()));
                        let Some(frame) = frame else {
                            return Ok(ActionResult::didnt(format!(
                                "{uia_error}; the target has no clickable frame"
                            )));
                        };
                        let (x, y) = frame.center();
                        input::click(x, y, super::MouseButton::Left, 1)?;
                        ActionResult::unknown(format!(
                            "{uia_error}; SendInput mouse events were posted instead"
                        ))
                    }
                })
            }
            ActionKind::Click => {
                if request.target_path.is_some()
                    && request
                        .target_actions
                        .iter()
                        .any(|action| action == "press")
                {
                    let target = target(root, request)?;
                    match target.press() {
                        Ok(message) => return Ok(ActionResult::worked(message)),
                        Err(uia_error) => {
                            let live_frame = target.frame();
                            let frame = live_frame
                                .has_area()
                                .then_some(live_frame)
                                .or_else(|| request.target_frame.filter(|frame| frame.has_area()));
                            let Some(frame) = frame else {
                                return Ok(ActionResult::didnt(format!(
                                    "{uia_error}; the target has no clickable frame"
                                )));
                            };
                            let (x, y) = frame.center();
                            input::click(x, y, request.button, request.click_count)?;
                            return Ok(ActionResult::unknown(format!(
                                "{uia_error}; SendInput mouse events were posted instead"
                            )));
                        }
                    }
                }
                let (x, y) = action_point(root, request)?;
                input::click(x, y, request.button, request.click_count)?;
                Ok(ActionResult::unknown("SendInput mouse events were posted"))
            }
            ActionKind::SetText => {
                let text = request.text.as_deref().ok_or_else(|| {
                    BackendError::new(BackendErrorCode::InvalidAction, "set_text requires text")
                })?;
                let target = target(root, request)?;
                match target.set_text(text) {
                    Ok(message) => Ok(ActionResult::worked(message)),
                    Err(uia_error) => {
                        if target.focus().is_err() {
                            let frame = target.frame();
                            if !frame.has_area() {
                                return Ok(ActionResult::didnt(format!(
                                    "{uia_error}; the target also rejected focus and has no clickable frame"
                                )));
                            }
                            let (x, y) = frame.center();
                            input::click(x, y, super::MouseButton::Left, 1)?;
                        }
                        input::keypress(&["ctrl+a".into()])?;
                        input::type_text(text)?;
                        Ok(ActionResult::unknown(format!(
                            "{uia_error}; keyboard replacement events were posted instead"
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
                Ok(ActionResult::unknown(
                    "SendInput Unicode keyboard events were posted",
                ))
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
                Ok(ActionResult::unknown(
                    "SendInput keyboard events were posted",
                ))
            }
            ActionKind::Scroll => {
                let scroll_x = request.scroll_x.unwrap_or(0.0);
                let scroll_y = request.scroll_y.unwrap_or(0.0);
                if request.target_path.is_some()
                    && request
                        .target_actions
                        .iter()
                        .any(|action| action == "scroll")
                {
                    let target = target(root, request)?;
                    match target.scroll(scroll_x, scroll_y) {
                        Ok(message) => return Ok(ActionResult::worked(message)),
                        Err(uia_error) => {
                            let frame = target.frame();
                            if !frame.has_area() {
                                return Ok(ActionResult::didnt(format!(
                                    "{uia_error}; the target has no frame for wheel fallback"
                                )));
                            }
                            let (x, y) = frame.center();
                            input::move_mouse(x, y)?;
                            input::scroll(scroll_x, scroll_y)?;
                            return Ok(ActionResult::unknown(format!(
                                "{uia_error}; SendInput wheel events were posted instead"
                            )));
                        }
                    }
                }
                if request.target_path.is_some() || (request.x.is_some() && request.y.is_some()) {
                    let (x, y) = action_point(root, request)?;
                    input::move_mouse(x, y)?;
                }
                input::scroll(scroll_x, scroll_y)?;
                Ok(ActionResult::unknown("SendInput wheel events were posted"))
            }
            ActionKind::Drag => {
                let path = request.path.as_deref().ok_or_else(|| {
                    BackendError::new(BackendErrorCode::InvalidAction, "drag requires a path")
                })?;
                input::drag(path, request.button)?;
                Ok(ActionResult::unknown("SendInput drag events were posted"))
            }
            ActionKind::MoveMouse => {
                let (x, y) = action_point(root, request)?;
                input::move_mouse(x, y)?;
                Ok(ActionResult::unknown(
                    "a SendInput mouse-move event was posted",
                ))
            }
        }
    }
}

fn target(root: &RootInfo, request: &ActionRequest) -> Result<uia::Target, BackendError> {
    let path = request.target_path.as_deref().ok_or_else(|| {
        BackendError::new(
            BackendErrorCode::InvalidAction,
            "this action requires an element ref",
        )
    })?;
    uia::locate_target(
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
