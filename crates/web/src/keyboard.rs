//! Browser input workarounds for `gpui-fast-web`.
//!
//! 1. Software keyboard restoration on iOS: gpui-fast-web summons a dismissed
//!    keyboard for a tap into the focused field by blurring and focusing its
//!    hidden input in one task; iOS coalesces that pair into nothing, so on an
//!    iPad the keyboard stays down. Blurring the input in the tap's own
//!    `pointerdown` puts the two in separate tasks: the `pointerup` that
//!    requests the keyboard then focuses an input that is not focused, which
//!    iOS honours as a user gesture. The blur does not deactivate the window:
//!    GPUI follows `document.hasFocus`.
//! 2. Linux middle-button paste suppression: gpui-fast-web cancels
//!    `pointerdown` and `pointerup` on the canvas, not `mousedown`, `mouseup`,
//!    or `auxclick`. On Linux (e.g. Firefox with `middlemouse.paste`), an
//!    uncanceled middle-button `auxclick` dispatches `paste` with the primary
//!    selection onto `document.activeElement` — the focused hidden IME
//!    textarea — whenever the user middle-clicks or finishes a middle-button
//!    scroll anywhere on the canvas.

use wasm_bindgen::{JsCast as _, prelude::*};

use crate::host::window;

pub(crate) fn install() {
    let blurred: std::rc::Rc<std::cell::RefCell<Option<web_sys::HtmlElement>>> =
        std::rc::Rc::default();
    let down = Closure::<dyn FnMut(web_sys::PointerEvent)>::new({
        let blurred = blurred.clone();
        move |event: web_sys::PointerEvent| {
            if event.pointer_type() != "touch" || !keyboard_dismissed() {
                return;
            }
            let Some(input) = focused_text_input() else {
                return;
            };
            input.blur().ok();
            *blurred.borrow_mut() = Some(input);
        }
    });
    // A tap that did not request the keyboard leaves the input blurred; a
    // hardware keyboard, which keeps the software one down, would then type
    // into nothing. Its receiver comes back after the tap, as gpui-fast-web
    // restores it when it dismisses the keyboard itself.
    let up = Closure::<dyn FnMut(web_sys::PointerEvent)>::new({
        let blurred = blurred.clone();
        move |_: web_sys::PointerEvent| {
            let Some(input) = blurred.borrow_mut().take() else {
                return;
            };
            let restore = Closure::once_into_js(move || {
                let focused = window()
                    .document()
                    .and_then(|document| document.active_element());
                if focused.as_ref() != Some(input.as_ref()) {
                    input.focus().ok();
                }
            });
            let _ = window().set_timeout_with_callback(restore.unchecked_ref());
        }
    });
    let window = window();
    // Capture, so the blur lands before gpui-fast-web's canvas handlers see
    // the press; the release is observed after they have run.
    let _ = window.add_event_listener_with_callback_and_bool(
        "pointerdown",
        down.as_ref().unchecked_ref(),
        true,
    );
    let _ = window.add_event_listener_with_callback("pointerup", up.as_ref().unchecked_ref());
    down.forget();
    up.forget();

    suppress_middle_click_paste(&window);
}

/// Cancels browser middle-button mouse defaults (`mousedown`, `mouseup`,
/// `auxclick`) so Linux primary-selection paste and native autoscroll do not
/// fire on the hidden IME textarea while GPUI handles middle clicks and
/// autoscroll through pointer events.
fn suppress_middle_click_paste(window: &web_sys::Window) {
    const MIDDLE_BUTTON: i16 = 1;
    let handler = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(|event: web_sys::MouseEvent| {
        if event.button() == MIDDLE_BUTTON {
            event.prevent_default();
        }
    });
    for event_name in ["mousedown", "mouseup", "auxclick"] {
        let _ = window.add_event_listener_with_callback_and_bool(
            event_name,
            handler.as_ref().unchecked_ref(),
            true,
        );
    }
    handler.forget();
}

/// The hidden text input gpui-fast-web keeps focused for a focused field.
fn focused_text_input() -> Option<web_sys::HtmlElement> {
    let element = window().document()?.active_element()?;
    if !element.is_instance_of::<web_sys::HtmlTextAreaElement>() {
        return None;
    }
    element.dyn_into().ok()
}

/// Whether no software keyboard covers the page: the visual viewport is the
/// whole layout viewport, unzoomed. The page's viewport meta leaves the
/// keyboard resizing only the visual viewport.
fn keyboard_dismissed() -> bool {
    let window = window();
    let (Some(viewport), Some(root)) = (
        window.visual_viewport(),
        window
            .document()
            .and_then(|document| document.document_element()),
    ) else {
        return false;
    };
    (viewport.scale() - 1.).abs() < 0.001
        && (viewport.height() - f64::from(root.client_height())).abs() < 1.
        && (viewport.width() - f64::from(root.client_width())).abs() < 1.
}
