//! A WebKitGTK page on the nest, hosted as the preview hosts it: GTK in this
//! process on the nest's socket and run by [`run_gtk`], the view drawn on a
//! surface of the session, the session's pointer on that surface.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;

use gtk::prelude::*;
use wayland_nest::{Crop, Event, Nest, Placement, ViewId, evdev_key, modifier, run_gtk};
use webkit2gtk::WebViewExt as _;

use super::{HEIGHT, Session, WIDTH};

pub const VIEW: ViewId = ViewId(0);
/// The page's background, `rgb(0, 128, 0)`.
pub const BACKGROUND: [u8; 3] = [0, 128, 0];

/// A green page with a `<select>` at the top left, a file input under it and
/// a text field below that. The page reports its state through its title.
pub const PAGE: &str = r#"<!doctype html>
<style>
  html, body { margin: 0; background: rgb(0, 128, 0); }
  select { position: absolute; left: 10px; top: 10px; width: 150px; height: 28px; }
  #file { position: absolute; left: 10px; top: 400px; }
  #field { position: absolute; left: 10px; top: 200px; width: 300px; height: 28px; }
</style>
<select><option>one</option><option>two</option><option>three</option>
<option>four</option><option>five</option><option>six</option></select>
<input id="file" type="file">
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
    pub session: Session,
    pub nest: Nest,
    pub window: gtk::Window,
    pub webview: webkit2gtk::WebView,
    /// Told each time the page's title, where it reports its state, changes.
    titles: async_channel::Receiver<()>,
    events: async_channel::Receiver<Event>,
    gtk: Pin<Box<dyn Future<Output = Infallible>>>,
    /// The host's clipboard: what the page is given when it pastes, and
    /// where what it copies goes.
    pub clipboard: Option<String>,
}

impl Page {
    /// Starts a session, the nest in it and GTK on the nest, then opens
    /// [`PAGE`] in a view that fills the host's surface and has the
    /// keyboard, and waits for the session to show it.
    pub fn open() -> Self {
        let session = Session::start();
        // SAFETY: the session's connection and surface outlive the test.
        let nest = unsafe { Nest::start(session.display()) }.expect("start the nest");
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
        // SAFETY: as above.
        unsafe { nest.attach(VIEW, session.surface()) };
        nest.place(
            VIEW,
            Some(Placement {
                width: WIDTH,
                height: HEIGHT,
                crop: Crop {
                    x: 0,
                    y: 0,
                    width: WIDTH as i32,
                    height: HEIGHT as i32,
                },
                scale: 1.0,
            }),
        );
        webview.load_html(PAGE, None);
        window.show_all();
        nest.focus(Some(VIEW));
        let events = nest.events();
        let mut page = Page {
            session,
            nest,
            window,
            webview,
            titles,
            events,
            gtk: Box::pin(run_gtk()),
            clipboard: None,
        };
        page.until_shown("the page", WIDTH - 1, HEIGHT - 1, |shown| {
            shown == BACKGROUND
        });
        page
    }

    /// Clicks the page's text field and waits for its focus.
    pub fn focus_field(&mut self) {
        self.session.click(100, 214);
        self.run_until("the field's focus", |state| state["active"] == "INPUT");
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

    /// Runs GTK, answering the page as the host does, until `done` holds for
    /// the page's state. Looks again at each change of state and each event
    /// of the nest.
    pub fn run_until(&mut self, what: &str, mut done: impl FnMut(&serde_json::Value) -> bool) {
        while !done(&self.state()) {
            self.turn(what);
        }
    }

    /// Runs GTK until the page has copied to the host's clipboard again;
    /// returns what it copied.
    pub fn next_copy(&mut self) -> String {
        loop {
            if let Some(Event::Copied(text)) = self.turn("a copy") {
                return text;
            }
        }
    }

    /// Runs GTK until the session shows something at `(x, y)` of the view
    /// that `done` accepts.
    pub fn until_shown(
        &mut self,
        what: &str,
        x: u32,
        y: u32,
        mut done: impl FnMut([u8; 3]) -> bool,
    ) {
        loop {
            let shown = drive(&mut self.gtk, self.session.shown(x, y));
            if done(shown) {
                return;
            }
            log::debug!("waiting for {what}: the session shows {shown:?}");
        }
    }

    /// Runs GTK until the page's state changes or the nest sends an event,
    /// which is handled as the host handles it and returned.
    fn turn(&mut self, what: &str) -> Option<Event> {
        let (titles, events) = (self.titles.clone(), self.events.clone());
        let woke = drive(&mut self.gtk, async {
            futures_lite::future::or(async { Some(events.recv().await) }, async {
                let _ = titles.recv().await;
                None
            })
            .await
        });
        match woke {
            None => None,
            Some(Ok(Event::Paste(paste))) => {
                paste.answer(self.clipboard.clone());
                None
            }
            Some(Ok(Event::Copied(text))) => {
                self.clipboard = Some(text.clone());
                Some(Event::Copied(text))
            }
            Some(Ok(event)) => Some(event),
            Some(Err(_)) => panic!("the nest stopped before {what}: {}", self.state()),
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

impl Drop for Page {
    fn drop(&mut self) {
        // As the host does before it closes its connection to the session.
        self.nest.leave_host();
    }
}

/// Runs `future` to its end with GTK running beside it, as the host's loop
/// runs both.
fn drive<T>(
    gtk: &mut Pin<Box<dyn Future<Output = Infallible>>>,
    future: impl Future<Output = T>,
) -> T {
    let mut future = std::pin::pin!(future);
    futures_lite::future::block_on(std::future::poll_fn(|cx| {
        let _ = gtk.as_mut().poll(cx);
        future.as_mut().poll(cx)
    }))
}
