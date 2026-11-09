//! A WebKitGTK page on the nest, driven as the preview drives it: GTK in this
//! process on the nest's socket, a frame presented before the next is
//! answered, input sent in view coordinates.

use std::time::{Duration, Instant};

use gtk::prelude::*;
use wayland_nest::{Frame, Nest, PointerEvent, ViewId, button, evdev_key, modifier};
use webkit2gtk::WebViewExt as _;

pub const VIEW: ViewId = ViewId(0);
pub const WIDTH: u32 = 400;
pub const HEIGHT: u32 = 300;
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// The page's background, `rgb(0, 128, 0)`, as the nest's native-endian
/// premultiplied ARGB.
pub const BACKGROUND: [u8; 4] = 0xff00_8000_u32.to_ne_bytes();

/// A green page with a `<select>` at the top left, a text field below it and
/// a box that keeps animating at the top right. The page reports its state
/// through its title.
pub const PAGE: &str = r#"<!doctype html>
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

pub struct Page {
    pub nest: Nest,
    pub window: gtk::Window,
    pub webview: webkit2gtk::WebView,
    pub frame: Option<Frame>,
}

pub fn pump_gtk() {
    while gtk::events_pending() {
        gtk::main_iteration_do(false);
    }
}

impl Page {
    /// Starts the nest and GTK on it, then opens [`PAGE`] in a view of
    /// [`WIDTH`] × [`HEIGHT`] that has the keyboard, and waits for its first
    /// frame.
    pub fn open() -> Self {
        let nest = Nest::start().expect("start the nest");
        nest.start_gtk().expect("start GTK on the nest");
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
        page.run_until("the page's first frame", |state, frame| {
            state.is_object() && pixel(frame, WIDTH - 1, HEIGHT - 1) == BACKGROUND
        });
        page
    }

    /// Clicks the page's text field and waits for its focus.
    pub fn focus_field(&mut self) {
        self.click(100.0, 214.0);
        self.run_until("the field's focus", |state, _| state["active"] == "INPUT");
    }

    /// Types `text` as fast as it can be sent: a character the US layout
    /// types by its key, any other through [`Nest::type_char`].
    pub fn type_mixed(&self, text: &str) {
        for c in text.chars() {
            match evdev_key(&c.to_string()) {
                Some(_) => self.press(&[], &c.to_string()),
                None => self.nest.type_char(c),
            }
        }
    }

    /// The page's reported state.
    pub fn state(&self) -> serde_json::Value {
        self.webview
            .title()
            .and_then(|title| serde_json::from_str(&title).ok())
            .unwrap_or_default()
    }

    /// Presents frames as the host does, each answered once shown, until
    /// `done` holds for the page's state and its last frame.
    pub fn run_until(
        &mut self,
        what: &str,
        mut done: impl FnMut(&serde_json::Value, &Frame) -> bool,
    ) {
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
            let frame = super::next_frame(&self.nest, VIEW, Duration::from_millis(50), pump_gtk);
            if let Some(frame) = frame {
                self.frame = Some(frame);
                self.nest.presented(VIEW);
            }
        }
    }

    pub fn click(&self, x: f64, y: f64) {
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
    pub fn press(&self, modifiers: &[u32], key: &str) {
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

pub fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * frame.width + x) * 4) as usize;
    frame.pixels[at..at + 4].try_into().expect("four bytes")
}
