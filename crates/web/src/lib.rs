//! Browser shell. Native workspace builds intentionally compile an empty lib.
#![cfg(target_family = "wasm")]

mod host;
mod transport;

use std::{borrow::Cow, cell::RefCell, rc::Rc};

use host::{WebHost, window};
use tcode_mobile::host::{MobileHost as _, PairRequest, Transport};
use wasm_bindgen::prelude::*;

thread_local! {
    static APPLICATION: RefCell<Option<gpui::ApplicationHandle>> = const { RefCell::new(None) };
    static CANVAS_OBSERVER: RefCell<Option<CanvasObserver>> = const { RefCell::new(None) };
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

/// GPUI prepares its own graphics canvas asynchronously. Adopt that canvas
/// (including a replacement on WebGPU → WebGL fallback), as Eauth does.
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
                    let _ = canvas.set_attribute("aria-label", "tcode remote client");
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
    gpui_platform::web_init();
    prepare_canvas(canvas_id)?;
    let application = gpui_platform::application().run_embedded(|cx| {
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(include_bytes!("../assets/NotoSans-Regular.ttf")),
                Cow::Borrowed(include_bytes!("../../../assets/fonts/DMSans[wght].ttf")),
            ])
            .expect("failed to load browser fonts");
        tcode_mobile::run_with_host(cx, Rc::new(WebHost::new()));
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

struct DebugConnection {
    transport: Transport,
    history: Vec<String>,
    index_snapshots: usize,
}

/// Exercise the actual MobileHost methods without depending on screen state.
/// The retained transport also permits restart/replay verification.
#[wasm_bindgen]
pub async fn debug_pair_and_connect(code: String) -> String {
    let (tx, rx) = async_channel::bounded(1);
    let started = APPLICATION.with(|slot| {
        let slot = slot.borrow();
        let Some(application) = slot.as_ref() else {
            return false;
        };
        application.update(|cx| {
            let (addr, port) = WebHost.fixed_pairing_endpoint().unwrap();
            WebHost.pair(
                PairRequest { addr, port, code },
                cx,
                Box::new(move |result, _cx| {
                    let _ = tx.try_send(result);
                }),
            );
        });
        true
    });
    if !started {
        return serde_json::json!({"error":"call start first"}).to_string();
    }
    let paired = match rx.recv().await {
        Ok(Ok(host)) => host,
        Ok(Err(error)) => return serde_json::json!({"error":error}).to_string(),
        Err(error) => return serde_json::json!({"error":error.to_string()}).to_string(),
    };
    let transport = WebHost.connect(&paired);
    let _ = transport.to_host.try_send(
        r#"{"id":1,"payload":{"type":"subscribe","content":{"topic":{"type":"index"}}}}"#.into(),
    );
    let line = transport
        .from_host
        .recv()
        .await
        .unwrap_or_else(|error| serde_json::json!({"error":error.to_string()}).to_string());
    DEBUG.with(|slot| {
        *slot.borrow_mut() = Some(DebugConnection {
            transport,
            history: Vec::new(),
            index_snapshots: usize::from(is_index_snapshot(&line)),
        })
    });
    line
}

/// Returns the last ConnectionState, its observed transitions, and received
/// index snapshot count. Omits tokens and unrelated host payloads.
#[wasm_bindgen]
pub fn debug_connection_state() -> String {
    DEBUG.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(debug) = slot.as_mut() else {
            return serde_json::json!({"state":"Offline"}).to_string();
        };
        while let Ok(state) = debug.transport.state.try_recv() {
            if debug.history.len() == 64 {
                debug.history.remove(0);
            }
            debug.history.push(format!("{state:?}"));
        }
        while let Ok(line) = debug.transport.from_host.try_recv() {
            debug.index_snapshots += usize::from(is_index_snapshot(&line));
        }
        serde_json::json!({
            "state": debug.history.last(),
            "history": debug.history,
            "index_snapshots": debug.index_snapshots,
        })
        .to_string()
    })
}

fn is_index_snapshot(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .is_ok_and(|value| value["content"]["event"]["type"].as_str() == Some("index_snapshot"))
}
