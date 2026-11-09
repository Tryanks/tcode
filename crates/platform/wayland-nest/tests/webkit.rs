//! A WebKitGTK page on the nest, driven as the preview drives it: GTK in this
//! process on the nest's socket, a frame presented before the next is
//! answered, input sent in view coordinates.
//!
//! GTK initializes once per process, so this binary holds one test.
#![cfg(target_os = "linux")]

mod common;

use std::ffi::{CString, c_char, c_int};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use wayland_nest::{Event, Frame, Nest, PointerEvent, ViewId, button, evdev_key, modifier};
use webkit2gtk::WebViewExt as _;

const VIEW: ViewId = ViewId(0);
const WIDTH: u32 = 400;
const HEIGHT: u32 = 300;
const TIMEOUT: Duration = Duration::from_secs(30);
/// The page's background, `rgb(0, 128, 0)`, as the nest's native-endian
/// premultiplied ARGB.
const BACKGROUND: [u8; 4] = 0xff00_8000_u32.to_ne_bytes();

/// A green page with a `<select>` at the top left, a text field below it and
/// a box that keeps animating at the top right. The page reports its state
/// through its title.
const PAGE: &str = r#"<!doctype html>
<style>
  html, body { margin: 0; background: rgb(0, 128, 0); }
  select { position: absolute; left: 10px; top: 10px; width: 150px; height: 28px; }
  input { position: absolute; left: 10px; top: 200px; width: 300px; height: 28px; }
  #spinner { position: absolute; left: 360px; top: 10px; width: 20px; height: 20px;
             background: white; animation: spin 1s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
</style>
<div id="spinner"></div>
<select><option>one</option><option>two</option><option>three</option>
<option>four</option><option>five</option><option>six</option></select>
<input id="field">
<script>
  const state = { value: "", composed: [], keys: [] };
  const report = () => {
    state.active = document.activeElement.tagName;
    document.title = JSON.stringify(state);
  };
  const field = document.getElementById("field");
  field.addEventListener("focus", report);
  field.addEventListener("input", () => { state.value = field.value; report(); });
  field.addEventListener("compositionupdate", (e) => { state.composed.push(e.data); report(); });
  field.addEventListener("keydown", (e) => { state.keys.push(e.key); report(); });
  report();
</script>"#;

struct Page {
    nest: Nest,
    window: gtk::Window,
    webview: webkit2gtk::WebView,
    frame: Option<Frame>,
}

fn pump_gtk() {
    while gtk::events_pending() {
        gtk::main_iteration_do(false);
    }
}

impl Page {
    /// The page's reported state.
    fn state(&self) -> serde_json::Value {
        self.webview
            .title()
            .and_then(|title| serde_json::from_str(&title).ok())
            .unwrap_or_default()
    }

    /// Presents frames as the host does, each answered once shown, until
    /// `done` holds for the page's state and its last frame.
    fn run_until(&mut self, what: &str, mut done: impl FnMut(&serde_json::Value, &Frame) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(frame) = &self.frame
                && done(&self.state(), frame)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {}",
                self.state()
            );
            let frame = common::next_frame(&self.nest, VIEW, Duration::from_millis(50), pump_gtk);
            if let Some(frame) = frame {
                self.frame = Some(frame);
                self.nest.presented(VIEW);
            }
        }
    }

    fn click(&self, x: f64, y: f64) {
        self.nest.pointer(VIEW, PointerEvent::Motion { x, y });
        for pressed in [true, false] {
            self.nest.pointer(
                VIEW,
                PointerEvent::Button {
                    button: button::LEFT,
                    pressed,
                },
            );
        }
    }

    /// Types `key` (a character or key name) with `modifiers` held.
    fn press(&self, modifiers: &[u32], key: &str) {
        let (code, shift) = evdev_key(key).expect("a US key");
        let shift = shift.then_some(modifier::SHIFT);
        for &held in modifiers.iter().chain(shift.as_ref()) {
            self.nest.key(held, true);
        }
        self.nest.key(code, true);
        self.nest.key(code, false);
        for &held in modifiers.iter().chain(shift.as_ref()) {
            self.nest.key(held, false);
        }
    }
}

fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * frame.width + x) * 4) as usize;
    frame.pixels[at..at + 4].try_into().expect("four bytes")
}

/// Starts GTK on the nest's socket, as the preview does.
fn start_gtk(socket: &str) {
    gdk::set_allowed_backends("wayland");
    let args = ["wayland-nest-test", "--display", socket].map(|arg| CString::new(arg).unwrap());
    let mut pointers: Vec<*mut c_char> = args.iter().map(|arg| arg.as_ptr().cast_mut()).collect();
    let mut argc = pointers.len() as c_int;
    let mut argv = pointers.as_mut_ptr();
    // SAFETY: argv points at `argc` NUL-terminated strings that outlive the
    // call; GTK only reorders the pointer array.
    let started = unsafe { gtk::ffi::gtk_init_check(&mut argc, &mut argv) } != 0;
    assert!(started, "GTK connects to the nest");
    gtk::init().expect("GTK is initialized");
    // The preview selects the IM module that speaks text-input-v3.
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_im_module(Some("wayland"));
    }
}

#[test]
fn a_webkit_page_on_the_nest() {
    let _runtime = common::runtime_dir();
    let nest = Nest::start().expect("start the nest");
    start_gtk(nest.socket_name());

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title(&VIEW.title());
    window.set_decorated(false);
    let webview = webkit2gtk::WebView::new();
    window.add(&webview);
    nest.configure(VIEW, WIDTH, HEIGHT);
    webview.load_html(PAGE, None);
    window.show_all();
    nest.focus(Some(VIEW));
    let mut page = Page {
        nest,
        window,
        webview,
        frame: None,
    };

    // A frame of the configured size, once the page draws.
    page.run_until("the page's first frame", |state, frame| {
        state.is_object() && pixel(frame, WIDTH - 1, HEIGHT - 1) == BACKGROUND
    });
    let frame = page.frame.as_ref().expect("a frame");
    assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));

    // While its last frame is unpresented the view is sent no other, and
    // GTK, whose frame clock waits for the frame callbacks, draws none.
    let frame = common::next_frame(&page.nest, VIEW, TIMEOUT, pump_gtk).expect("a frame");
    page.frame = Some(frame);
    let clock = page.window.frame_clock().expect("a mapped window");
    let painted = move || clock.frame_counter();
    let held_at = painted();
    let sent = common::wait_for(&page.nest, Duration::from_secs(1), pump_gtk, |event| {
        matches!(event, Event::Frame { .. }).then_some(())
    });
    assert!(
        sent.is_none(),
        "no frame is sent before the last is presented"
    );
    let held = painted() - held_at;
    assert!(
        held <= 1,
        "GTK painted {held} frames while none was presented"
    );
    page.nest.presented(VIEW);
    page.run_until("the animation to resume", |_, _| painted() > held_at + 10);

    // The <select>'s menu composites over the page below it, and a press
    // outside dismisses it.
    let below_select = (60, 120);
    page.click(60.0, 24.0);
    page.run_until("the select's menu", |_, frame| {
        pixel(frame, below_select.0, below_select.1) != BACKGROUND
    });
    page.click(380.0, 280.0);
    page.run_until("the menu to close", |_, frame| {
        pixel(frame, below_select.0, below_select.1) == BACKGROUND
    });

    // Keys, then text an input method composes and commits.
    page.click(100.0, 214.0);
    page.run_until("the field's focus", |state, _| state["active"] == "INPUT");
    page.press(&[], "a");
    page.press(&[], "B");
    page.run_until("typed keys", |state, _| state["value"] == "aB");
    page.nest.compose_text("にほ".into(), Some(6..6));
    page.run_until("a composition", |state, _| {
        state["composed"]
            .as_array()
            .is_some_and(|composed| composed.iter().any(|text| text == "にほ"))
    });
    page.nest.commit_text("日本".into());
    page.nest.commit_text("é漢字".into());
    page.run_until("committed text", |state, _| state["value"] == "aB日本é漢字");
    // Typed keys reach the page as keys, before what WebKit reports for the
    // input method's text.
    let state = page.state();
    let keys = state["keys"]
        .as_array()
        .map(|keys| &keys[..keys.len().min(3)]);
    assert_eq!(
        keys,
        Some(&["a".into(), "Shift".into(), "B".into()][..]),
        "keys: {}",
        state["keys"]
    );

    // The host's clipboard pastes in the page, and what the page copies
    // afterwards, each time, reaches the host.
    page.nest.set_clipboard("pasted ✓".into());
    page.press(&[modifier::CONTROL], "a");
    page.press(&[modifier::CONTROL], "v");
    page.run_until("the pasted text", |state, _| state["value"] == "pasted ✓");
    for (typed, value) in [("!", "pasted ✓!"), ("?", "pasted ✓!?")] {
        page.press(&[], "end");
        page.press(&[], typed);
        page.run_until("the typed text", |state, _| state["value"] == value);
        page.press(&[modifier::CONTROL], "a");
        page.press(&[modifier::CONTROL], "c");
        let copied = common::wait_for(&page.nest, TIMEOUT, pump_gtk, |event| match event {
            Event::Clipboard(text) => Some(text),
            Event::Frame { .. } => {
                page.nest.presented(VIEW);
                None
            }
            _ => None,
        });
        assert_eq!(copied.as_deref(), Some(value));
    }
}
