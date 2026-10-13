//! Browser shell. Native workspace builds intentionally compile an empty lib.
#![cfg(target_family = "wasm")]

mod host;
mod transport;

use std::{borrow::Cow, cell::RefCell, rc::Rc};

use host::{WebHost, window};
#[cfg(feature = "debug-exports")]
use tcode_client::host::ClientHost as _;
use wasm_bindgen::prelude::*;

thread_local! {
    static APPLICATION: RefCell<Option<gpui::ApplicationHandle>> = const { RefCell::new(None) };
    static CANVAS_OBSERVER: RefCell<Option<CanvasObserver>> = const { RefCell::new(None) };
    #[cfg(feature = "debug-exports")]
    static DEBUG: RefCell<Option<DebugConnection>> = const { RefCell::new(None) };
}

struct CanvasObserver {
    observer: web_sys::MutationObserver,
    _callback: Closure<dyn FnMut(js_sys::Array)>,
}

impl Drop for CanvasObserver {
    fn drop(&mut self) {
        self.observer.disconnect();
    }
}

/// Adopt GPUI's asynchronously prepared canvas, including replacements
/// created when WebGPU falls back to WebGL.
fn prepare_canvas(canvas_id: &str) -> Result<(), JsValue> {
    let document = window().document().ok_or("missing document")?;
    let placeholder = document
        .get_element_by_id(canvas_id)
        .ok_or("missing canvas")?;
    if !placeholder.is_instance_of::<web_sys::HtmlCanvasElement>() {
        return Err(JsValue::from_str("start requires a canvas element"));
    }
    placeholder.remove();
    let canvas_id = canvas_id.to_owned();
    let callback = Closure::wrap(Box::new(move |records: js_sys::Array| {
        for record in records.iter() {
            let record: web_sys::MutationRecord = record.unchecked_into();
            let nodes = record.added_nodes();
            for index in 0..nodes.length() {
                if let Some(canvas) = nodes
                    .item(index)
                    .and_then(|node| node.dyn_into::<web_sys::HtmlCanvasElement>().ok())
                {
                    canvas.set_id(&canvas_id);
                    let _ = canvas.set_attribute("aria-label", "Tcode remote client");
                }
            }
        }
    }) as Box<dyn FnMut(js_sys::Array)>);
    let observer = web_sys::MutationObserver::new(callback.as_ref().unchecked_ref())?;
    let options = web_sys::MutationObserverInit::new();
    options.set_child_list(true);
    observer.observe_with_options(document.body().ok_or("missing body")?.as_ref(), &options)?;
    CANVAS_OBSERVER.with(|slot| {
        *slot.borrow_mut() = Some(CanvasObserver {
            observer,
            _callback: callback,
        })
    });
    Ok(())
}

#[wasm_bindgen]
pub async fn start(canvas_id: &str) -> Result<(), JsValue> {
    if APPLICATION.with(|slot| slot.borrow().is_some()) {
        return Ok(());
    }
    console_error_panic_hook::set_once();
    gpui_web::init_logging();
    prepare_canvas(canvas_id)?;
    let host = Rc::new(WebHost);
    // The login page already signed this browser in with the serving
    // machine; the shell reopens that record.
    let initial = tcode_ui::last_host_target(host.as_ref());
    let platform = Rc::new(gpui_web::WebPlatform::new_with_backend(
        true,
        gpui_web::WebBackendPreference::Auto,
    ));
    let http_client = std::sync::Arc::new(platform.fetch_http_client());
    let application = gpui::Application::with_platform(platform)
        .with_http_client(http_client)
        .with_assets(tcode_ui::assets::Assets)
        .run_embedded(|cx| {
            let host: Rc<dyn tcode_client::host::ClientHost> = host;
            tcode_ui::run_shell(
                cx,
                host.clone(),
                tcode_ui::ShellOptions {
                    window: gpui::WindowOptions {
                        // GPUI takes the browser window's size from the canvas
                        // element; a fixed size here would be a lie.
                        window_bounds: None,
                        titlebar: None,
                        window_background: gpui::WindowBackgroundAppearance::Opaque,
                        ..Default::default()
                    },
                    fonts: vec![
                        Cow::Borrowed(include_bytes!("../assets/NotoSans-Regular.ttf")),
                        Cow::Borrowed(include_bytes!("../../../assets/fonts/DMSans[wght].ttf")),
                    ],
                    theme_json: Cow::Owned(tcode_ui::flattened_theme_json()),
                    activate: true,
                    setup: tcode_ui::ShellSetup {
                        initial,
                        initial_pairing_error: None,
                        client_host: Some(host),
                        // A browser tab runs no host of its own.
                        local: None,
                        local_failure: None,
                        seed_blocking: false,
                        restore_navigation: false,
                    },
                    ..Default::default()
                }
                .with_bundled_monospace(),
            );
            if let Some(document) = window().document() {
                if let Some(loading) = document.get_element_by_id("loading") {
                    loading.remove();
                }
                if let Some(body) = document.body() {
                    let _ = body.set_attribute("data-tcode-ready", "true");
                }
            }
        });
    APPLICATION.with(|slot| *slot.borrow_mut() = Some(application));
    Ok(())
}

#[cfg(feature = "debug-exports")]
struct DebugConnection {
    link: tcode_client::HostLink,
    history: Rc<RefCell<Vec<String>>>,
    index_snapshots: usize,
}

#[cfg(feature = "debug-exports")]
impl Drop for DebugConnection {
    fn drop(&mut self) {
        self.link.close();
    }
}

/// Exercise the actual WebHost methods without depending on screen state:
/// connect to the machine the login page signed in with. The retained
/// transport also permits restart/replay verification.
#[wasm_bindgen]
#[cfg(feature = "debug-exports")]
pub async fn debug_connect() -> String {
    let started = APPLICATION.with(|slot| slot.borrow().is_some());
    if !started {
        return serde_json::json!({"error":"call start first"}).to_string();
    }
    let host = WebHost;
    let Some(paired) = host.last_host_id().and_then(|id| {
        host.load_hosts()
            .into_iter()
            .find(|host| host.host_id == id)
    }) else {
        return serde_json::json!({"error":"not signed in"}).to_string();
    };
    let transport = host.connect(&paired);
    let link = tcode_client::HostLink::new(transport.to_host, transport.from_host);
    if let Err(error) = link.subscribe(tcode_protocol::Subscription {
        known_index: None,
        topic: tcode_protocol::Topic::Index,
        after: None,
    }) {
        return serde_json::json!({"error": error.message}).to_string();
    }
    let history = Rc::new(RefCell::new(Vec::new()));
    wasm_bindgen_futures::spawn_local({
        let link = link.clone();
        async move { link.pump().await }
    });
    wasm_bindgen_futures::spawn_local({
        let link = link.clone();
        let history = history.clone();
        async move {
            while let Ok(state) = transport.state.recv().await {
                let mut history = history.borrow_mut();
                if history.len() == 64 {
                    history.remove(0);
                }
                history.push(format!("{state:?}"));
                link.set_connection_state(state);
            }
        }
    });
    let event = match link.events().recv().await {
        Ok(event) => event,
        Err(error) => {
            link.close();
            return serde_json::json!({"error": error.to_string()}).to_string();
        }
    };
    let index_snapshots = usize::from(matches!(
        &event.event,
        tcode_protocol::ServerEvent::IndexSnapshot(_)
    ));
    let line =
        tcode_protocol::encode_line(&tcode_protocol::HostMessage::Event(event)).unwrap_or_default();
    DEBUG.with(|slot| {
        *slot.borrow_mut() = Some(DebugConnection {
            link,
            history,
            index_snapshots,
        });
    });
    line
}

/// Returns the last ConnectionState, its observed transitions, and received
/// index snapshot count. Omits tokens and unrelated host payloads.
#[wasm_bindgen]
#[cfg(feature = "debug-exports")]
pub fn debug_connection_state() -> String {
    DEBUG.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(debug) = slot.as_mut() else {
            return serde_json::json!({"state":"Offline"}).to_string();
        };
        while let Ok(event) = debug.link.events().try_recv() {
            debug.index_snapshots += usize::from(matches!(
                event.event,
                tcode_protocol::ServerEvent::IndexSnapshot(_)
            ));
        }
        let history = debug.history.borrow();
        serde_json::json!({
            "state": history.last(),
            "history": *history,
            "index_snapshots": debug.index_snapshots,
        })
        .to_string()
    })
}
