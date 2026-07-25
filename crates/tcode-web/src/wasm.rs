//! Browser shell for the shared tcode client.
//!
//! Everything the user sees lives in `client-app`; this file is the part a
//! browser cannot share — a `web-sys` WebSocket, the page's query string, and
//! the wasm-bindgen entry point.

use std::{borrow::Cow, cell::RefCell, collections::VecDeque, rc::Rc, sync::Arc};

use client_app::{
    ClientApp, ClientConnection, ClientIdentity, Inbox, Transport, TransportEvent, TransportFactory,
};
use gpui::{App, AppContext as _, ApplicationHandle, WindowOptions};
use gpui_component::Root;
use wasm_bindgen::{JsCast as _, closure::Closure, prelude::*};
use web_sys::{CloseEvent, Event, MessageEvent, UrlSearchParams, WebSocket};

thread_local! {
    // WebPlatform::run returns after installing browser callbacks. This handle
    // deliberately owns GPUI for the rest of the page lifetime.
    static APPLICATION: RefCell<Option<ApplicationHandle>> = const { RefCell::new(None) };
}

/// A browser WebSocket.
struct WebTransport {
    socket: WebSocket,
}

impl Transport for WebTransport {
    fn send(&self, text: &str) -> Result<(), String> {
        self.socket.send_with_str(text).map_err(js_error)
    }
}

struct WebTransportFactory;

// SAFETY: wasm is single-threaded here, and the factory holds no state. The
// bounds exist because the shared client is written for platforms that do have
// threads.
unsafe impl Send for WebTransportFactory {}
unsafe impl Sync for WebTransportFactory {}

impl TransportFactory for WebTransportFactory {
    fn connect(&self, url: &str, hello: &str, inbox: Inbox) -> Result<Box<dyn Transport>, String> {
        let socket = connect(url, hello, inbox).map_err(js_error)?;
        Ok(Box::new(WebTransport { socket }))
    }
}

fn connect(
    url: &str,
    hello: &str,
    incoming: Rc<RefCell<VecDeque<TransportEvent>>>,
) -> Result<WebSocket, JsValue> {
    let socket = WebSocket::new(url)?;

    let on_open = Closure::<dyn FnMut(Event)>::new({
        let socket = socket.clone();
        let hello = hello.to_owned();
        let incoming = incoming.clone();
        move |_| {
            if socket.send_with_str(&hello).is_err() {
                incoming.borrow_mut().push_back(TransportEvent::Error);
            } else {
                incoming.borrow_mut().push_back(TransportEvent::Open);
            }
        }
    });
    socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    on_open.forget();

    let on_message = Closure::<dyn FnMut(MessageEvent)>::new({
        let incoming = incoming.clone();
        move |event: MessageEvent| {
            if let Some(text) = event.data().as_string() {
                incoming.borrow_mut().push_back(TransportEvent::Text(text));
            }
        }
    });
    socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    let on_error = Closure::<dyn FnMut(Event)>::new({
        let incoming = incoming.clone();
        move |_| incoming.borrow_mut().push_back(TransportEvent::Error)
    });
    socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    on_error.forget();

    let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
        incoming
            .borrow_mut()
            .push_back(TransportEvent::Closed(event.reason()));
    });
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    on_close.forget();

    Ok(socket)
}

fn page_config() -> Result<ClientConnection, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("window is unavailable"))?;
    let search = window.location().search()?;
    let params = UrlSearchParams::new_with_str(&search)?;
    Ok(ClientConnection {
        url: params.get("url").filter(|value| !value.is_empty()),
        token: params.get("token").filter(|value| !value.is_empty()),
        identity: ClientIdentity {
            // Per page load, not per installation: a browser has no stable
            // handle to offer, and claiming one would let a host think two tabs
            // are the same device.
            client_id: format!("tcode-web-{}", js_sys::Date::now() as u64),
            display_name: "tcode web".into(),
            platform: "web".into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
        },
    })
}

/// Delivery ids only need to be unique within this page's lifetime, since the
/// host correlates them per session and the client forgets them once accepted.
fn next_delivery_id() -> u64 {
    thread_local! {
        static NEXT: RefCell<u64> = RefCell::new(js_sys::Date::now() as u64);
    }
    NEXT.with(|next| {
        let mut next = next.borrow_mut();
        *next = next.wrapping_add(1);
        *next
    })
}

fn js_error(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    gpui_platform::web_init();
    let config = page_config()?;
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets::new(
        "https://longbridge.github.io/gpui-component/gallery/",
    ));
    let handle = app.run_embedded(move |cx: &mut App| {
        gpui_component::init(cx);
        tcode_ui::markdown::init(cx);
        let fonts: Vec<Cow<'static, [u8]>> = vec![
            Cow::Borrowed(tcode_ui::assets::DM_SANS),
            Cow::Borrowed(tcode_ui::assets::LILEX_REGULAR),
            Cow::Borrowed(tcode_ui::assets::LILEX_BOLD),
            Cow::Borrowed(tcode_ui::assets::LILEX_ITALIC),
            Cow::Borrowed(tcode_ui::assets::LILEX_BOLD_ITALIC),
        ];
        cx.text_system()
            .add_fonts(fonts)
            .expect("bundled web fonts must load");
        cx.open_window(WindowOptions::default(), |window, cx| {
            let app =
                cx.new(|cx| ClientApp::new(config, Arc::new(WebTransportFactory), window, cx));
            cx.new(|cx| Root::new(app, window, cx).bordered(false))
        })
        .expect("the GPUI web window must open");
        cx.activate(true);
    });
    APPLICATION.with(|slot| {
        *slot.borrow_mut() = Some(handle);
    });
    Ok(())
}
