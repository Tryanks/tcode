//! A WebKitGTK page on the nest, driven as the preview drives it: GTK in this
//! process on the nest's socket, a frame presented before the next is
//! answered, input sent in view coordinates.

use std::future::Future as _;
use std::task::Poll;

use gtk::glib;
use gtk::prelude::*;
use wayland_nest::{Event, Frame, Nest, PointerEvent, ViewId, button, evdev_key, modifier};
use webkit2gtk::WebViewExt as _;

pub const VIEW: ViewId = ViewId(0);
pub const WIDTH: u32 = 400;
pub const HEIGHT: u32 = 300;
/// The page's background, `rgb(0, 128, 0)`, as the nest's native-endian
/// premultiplied ARGB.
pub const BACKGROUND: [u8; 4] = 0xff00_8000_u32.to_ne_bytes();

/// A green page with a `<select>` at the top left and a text field below it.
/// The page reports its state through its title.
pub const PAGE: &str = r#"<!doctype html>
<style>
  html, body { margin: 0; background: rgb(0, 128, 0); }
  select { position: absolute; left: 10px; top: 10px; width: 150px; height: 28px; }
  input { position: absolute; left: 10px; top: 200px; width: 300px; height: 28px; }
</style>
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
    /// Told each time the page's title, where it reports its state, changes.
    titles: async_channel::Receiver<()>,
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
        let (title_changed, titles) = async_channel::unbounded();
        webview.connect_title_notify(move |_| {
            let _ = title_changed.try_send(());
        });
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
            titles,
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

    /// Runs GTK until the nest sends an event `pick` accepts; other events
    /// are dropped. `None` when the nest has stopped.
    pub fn wait_for<T>(&self, mut pick: impl FnMut(Event) -> Option<T>) -> Option<T> {
        let events = self.nest.events();
        glib::MainContext::default().block_on(async move {
            while let Ok(event) = events.recv().await {
                if let Some(found) = pick(event) {
                    return Some(found);
                }
            }
            None
        })
    }

    /// Presents frames as the host does, each answered once shown, until
    /// `done` holds for the page's state and its last frame. Runs GTK while
    /// it waits, and looks again at each frame and each change of state.
    pub fn run_until(
        &mut self,
        what: &str,
        mut done: impl FnMut(&serde_json::Value, &Frame) -> bool,
    ) {
        let events = self.nest.events();
        loop {
            if let Some(frame) = &self.frame
                && done(&self.state(), frame)
            {
                return;
            }
            let woke = {
                let mut event = Box::pin(events.recv());
                let mut title = Box::pin(self.titles.recv());
                glib::MainContext::default().block_on(std::future::poll_fn(|cx| {
                    if let Poll::Ready(event) = event.as_mut().poll(cx) {
                        return Poll::Ready(Some(event));
                    }
                    if title.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(None);
                    }
                    Poll::Pending
                }))
            };
            match woke {
                Some(Ok(Event::Frame { view, frame })) if view == VIEW => {
                    self.frame = Some(frame);
                    self.nest.presented(VIEW);
                }
                Some(Ok(_)) | None => {}
                Some(Err(_)) => panic!("the nest stopped before {what}: {}", self.state()),
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
