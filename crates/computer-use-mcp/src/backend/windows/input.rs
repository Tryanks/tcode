//! Input delivered as window messages posted to the target's own windows.
//! Nothing here moves the system cursor, changes the foreground window or
//! enters the global input stream, so the user keeps their pointer and focus.
//! The target sees the messages but not a matching cursor position or key
//! state: an application that hit-tests with `GetCursorPos`, reads modifiers
//! with `GetKeyState`, or reads raw input may not respond to them.

use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, ChildWindowFromPointEx,
    GCL_STYLE, GUITHREADINFO, GetClassLongW, GetGUIThreadInfo, GetWindowThreadProcessId, IsWindow,
    PostMessageW,
};

use super::super::{BackendError, BackendErrorCode, MouseButton, RootInfo, parse_key_chord};
use super::map::{
    PostedMessage, char_messages, click_messages, drag_messages, hover_message, key_messages,
    wheel_messages,
};

const EM_SETSEL: u32 = 0x00B1;

pub(super) fn click(
    root: &RootInfo,
    x: f64,
    y: f64,
    button: MouseButton,
    click_count: u32,
) -> Result<(), BackendError> {
    if !(1..=3).contains(&click_count) {
        return Err(invalid("click_count must be between 1 and 3"));
    }
    let screen = screen_point(x, y, "click coordinates")?;
    let window = window_at(root_window(root)?, screen);
    // SAFETY: `window` was validated by window_at; reading its class style has
    // no memory-safety requirements.
    let double_clicks = unsafe { GetClassLongW(window, GCL_STYLE) } & CS_DBLCLKS.0 != 0;
    let messages = click_messages(
        client_point(window, screen),
        button,
        click_count,
        double_clicks,
    );
    for (index, message) in messages.iter().enumerate() {
        post(window, message)?;
        // A pause between presses keeps their message times apart, as a hand's
        // would be.
        if index > 0 && index % 2 == 0 && index + 1 < messages.len() {
            thread::sleep(Duration::from_millis(45));
        }
    }
    Ok(())
}

pub(super) fn move_mouse(root: &RootInfo, x: f64, y: f64) -> Result<(), BackendError> {
    let screen = screen_point(x, y, "mouse coordinates")?;
    let window = window_at(root_window(root)?, screen);
    post(window, &hover_message(client_point(window, screen)))
}

/// Every message goes to the window under the first point, in its client
/// coordinates, as a captured drag's would.
pub(super) fn drag(
    root: &RootInfo,
    path: &[[f64; 2]],
    button: MouseButton,
) -> Result<(), BackendError> {
    if path.len() < 2 {
        return Err(invalid("drag requires at least two path points"));
    }
    let points = path
        .iter()
        .map(|point| screen_point(point[0], point[1], "drag path coordinates"))
        .collect::<Result<Vec<_>, _>>()?;
    let window = window_at(root_window(root)?, points[0]);
    let client: Vec<(i32, i32)> = points
        .iter()
        .map(|&point| client_point(window, point))
        .collect();
    for message in drag_messages(&client, button) {
        post(window, &message)?;
        thread::sleep(Duration::from_millis(12));
    }
    Ok(())
}

pub(super) fn scroll(
    root: &RootInfo,
    (x, y): (f64, f64),
    scroll_x: f64,
    scroll_y: f64,
) -> Result<(), BackendError> {
    let screen = screen_point(x, y, "scroll coordinates")?;
    let window = window_at(root_window(root)?, screen);
    for message in wheel_messages((screen.x, screen.y), scroll_x, scroll_y) {
        post(window, &message)?;
    }
    Ok(())
}

/// The window that has keyboard focus in the target's UI thread, which is not
/// necessarily the foreground thread; the root window when it reports none.
pub(super) fn keyboard_window(root: &RootInfo) -> Result<HWND, BackendError> {
    let root = root_window(root)?;
    // SAFETY: `root` is a live HWND and the thread id is only used to query
    // that thread's GUI state into an initialized, correctly sized struct.
    unsafe {
        let thread = GetWindowThreadProcessId(root, None);
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..GUITHREADINFO::default()
        };
        if thread != 0
            && GetGUIThreadInfo(thread, &mut info).is_ok()
            && !info.hwndFocus.is_invalid()
            && IsWindow(Some(info.hwndFocus)).as_bool()
        {
            return Ok(info.hwndFocus);
        }
    }
    Ok(root)
}

pub(super) fn keypress(window: HWND, keys: &[String]) -> Result<(), BackendError> {
    let chord = parse_key_chord(keys).map_err(invalid)?;
    // SAFETY: MapVirtualKeyW only translates the given key code.
    let scan = |key: u16| unsafe { MapVirtualKeyW(u32::from(key), MAPVK_VK_TO_VSC) };
    for message in key_messages(chord, scan).map_err(invalid)? {
        post(window, &message)?;
    }
    Ok(())
}

pub(super) fn type_text(window: HWND, text: &str) -> Result<(), BackendError> {
    for message in char_messages(text) {
        post(window, &message)?;
    }
    Ok(())
}

/// Standard edit and rich-edit controls select their whole text on
/// `EM_SETSEL(0, -1)`; other windows ignore the message.
pub(super) fn select_all(window: HWND) -> Result<(), BackendError> {
    post(
        window,
        &PostedMessage {
            message: EM_SETSEL,
            wparam: 0,
            lparam: -1,
        },
    )
}

fn root_window(root: &RootInfo) -> Result<HWND, BackendError> {
    let window = HWND(root.window_id as usize as *mut _);
    // SAFETY: IsWindow accepts any handle value.
    if window.is_invalid() || !unsafe { IsWindow(Some(window)) }.as_bool() {
        return Err(operation("the target window no longer exists"));
    }
    Ok(window)
}

/// The deepest visible, enabled child of `root` under the screen point,
/// whatever other windows cover it.
fn window_at(root: HWND, screen: POINT) -> HWND {
    let mut current = root;
    loop {
        let client = client_point(current, screen);
        // SAFETY: `current` is `root` or a child returned by the previous call.
        let child = unsafe {
            ChildWindowFromPointEx(
                current,
                POINT {
                    x: client.0,
                    y: client.1,
                },
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            )
        };
        if child.is_invalid() || child == current {
            return current;
        }
        current = child;
    }
}

fn client_point(window: HWND, screen: POINT) -> (i32, i32) {
    let mut point = screen;
    // SAFETY: `point` is initialized writable memory; on failure it keeps the
    // screen coordinates, which still name a point for the message.
    let _ = unsafe { ScreenToClient(window, &mut point) };
    (point.x, point.y)
}

fn post(window: HWND, message: &PostedMessage) -> Result<(), BackendError> {
    // SAFETY: posting copies three scalar values into the window's queue; no
    // pointer is passed.
    unsafe {
        PostMessageW(
            Some(window),
            message.message,
            WPARAM(message.wparam),
            LPARAM(message.lparam),
        )
    }
    .map_err(|error| operation(format!("PostMessageW failed: {error}")))
}

fn screen_point(x: f64, y: f64, description: &str) -> Result<POINT, BackendError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(invalid(format!("{description} must be finite")));
    }
    let (x, y) = (x.round(), y.round());
    let range = f64::from(i32::MIN)..=f64::from(i32::MAX);
    if !range.contains(&x) || !range.contains(&y) {
        return Err(invalid(format!("{description} exceed the Windows range")));
    }
    Ok(POINT {
        x: x as i32,
        y: y as i32,
    })
}

fn invalid(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::InvalidAction, message)
}

fn operation(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::OperationFailed, message)
}
