//! Browser shell for the shared tcode client.
//!
//! Everything the user sees lives in `client-app`; this file is the part a
//! browser cannot share — a `web-sys` WebSocket, the page's query string, and
//! the wasm-bindgen entry point.

use std::{borrow::Cow, cell::RefCell, sync::Arc};

use client_app::{
    ClientApp, ClientConnection, ClientIdentity, ConnectionStore, Inbox, StoredConnection,
    Transport, TransportEvent, TransportFactory,
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

/// Push one event onto the shared inbox.
///
/// The inbox is an `Arc<Mutex<_>>` because native transports push from a socket
/// thread; the browser is single-threaded so the lock is never contended, but
/// the type is shared with those platforms and so is used the same way here.
fn push(inbox: &Inbox, event: TransportEvent) {
    inbox
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_back(event);
}

fn connect(url: &str, hello: &str, incoming: Inbox) -> Result<WebSocket, JsValue> {
    let socket = WebSocket::new(url)?;

    let on_open = Closure::<dyn FnMut(Event)>::new({
        let socket = socket.clone();
        let hello = hello.to_owned();
        let incoming = incoming.clone();
        move |_| {
            if socket.send_with_str(&hello).is_err() {
                push(&incoming, TransportEvent::Error);
            } else {
                push(&incoming, TransportEvent::Open);
            }
        }
    });
    socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    on_open.forget();

    let on_message = Closure::<dyn FnMut(MessageEvent)>::new({
        let incoming = incoming.clone();
        move |event: MessageEvent| {
            if let Some(text) = event.data().as_string() {
                push(&incoming, TransportEvent::Text(text));
            }
        }
    });
    socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();

    let on_error = Closure::<dyn FnMut(Event)>::new({
        let incoming = incoming.clone();
        move |_| push(&incoming, TransportEvent::Error)
    });
    socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    on_error.forget();

    let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
        push(&incoming, TransportEvent::Closed(event.reason()));
    });
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    on_close.forget();

    Ok(socket)
}

/// Persists the paired connection in the browser's `localStorage`.
///
/// Stateless: each call re-reads `window.localStorage`, so the handle is
/// trivially `Send + Sync` for the shared client's bounds even though `Storage`
/// is neither. The token is the secret this exists to keep out of the address
/// bar; the URL rides along only so a reload need not re-type it.
struct LocalStorageStore;

const URL_KEY: &str = "tcode.host_url";
const TOKEN_KEY: &str = "tcode.sync_token";

impl LocalStorageStore {
    fn storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }
}

impl ConnectionStore for LocalStorageStore {
    fn load(&self) -> Option<StoredConnection> {
        let storage = Self::storage()?;
        // No token means no pairing; the URL alone is just a prefill and is not
        // worth restoring a session for.
        let token = storage.get_item(TOKEN_KEY).ok().flatten()?;
        let url = storage.get_item(URL_KEY).ok().flatten().unwrap_or_default();
        Some(StoredConnection { url, token })
    }

    fn save(&self, connection: &StoredConnection) {
        let Some(storage) = Self::storage() else {
            return;
        };
        // Best-effort: storage can be disabled or full. Failing to persist is
        // not worth interrupting a working session — the user simply pairs
        // again next load.
        let _ = storage.set_item(URL_KEY, &connection.url);
        let _ = storage.set_item(TOKEN_KEY, &connection.token);
    }

    fn clear(&self) {
        let Some(storage) = Self::storage() else {
            return;
        };
        let _ = storage.remove_item(URL_KEY);
        let _ = storage.remove_item(TOKEN_KEY);
    }
}

fn page_config() -> Result<ClientConnection, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("window is unavailable"))?;
    let search = window.location().search()?;
    let params = UrlSearchParams::new_with_str(&search)?;
    Ok(ClientConnection {
        identity: ClientIdentity {
            // Per page load, not per installation: a browser has no stable
            // handle to offer, and claiming one would let a host think two tabs
            // are the same device.
            client_id: format!("tcode-web-{}", js_sys::Date::now() as u64),
            display_name: "tcode web".into(),
            platform: "web".into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
        },
        // `?url=` prefills the address only — a URL is not a secret. The token
        // is never read from the page: it is earned by pairing and kept in
        // localStorage, deliberately never `?token=`, which would leave a
        // durable credential in the address bar, in history, and in referrers.
        url: params.get("url").filter(|value| !value.is_empty()),
        store: Arc::new(LocalStorageStore),
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
