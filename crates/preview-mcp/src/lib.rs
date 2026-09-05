//! In-process MCP server exposing the embedded preview browser to the agent.
//!
//! The GUI process owns a native WebView (see `src/ui/preview_panel.rs`). The
//! agent CLIs (`claude`, `codex`) are separate child processes; to let them
//! drive that WebView we run a small [Model Context Protocol] server over
//! **streamable HTTP** on `127.0.0.1:<random port>`, guarded by a bearer token,
//! and register it with each spawned agent.
//!
//! A tool call arrives on the tokio HTTP runtime, is turned into a
//! [`PreviewOp`], and handed to the UI process through the [`Broker`]: a
//! request rides an [`async_channel`] into the gpui main thread, which resolves
//! it against the live WebView (running JS via `evaluate_script`, or shelling
//! out to `screencapture`) and answers on a per-request reply channel. This
//! mirrors T3's `PreviewAutomationBroker` request→deferred→respond pattern,
//! reduced to what a single native WebView can do without CDP.
//!
//! [Model Context Protocol]: https://modelcontextprotocol.io

use std::time::Duration;

pub mod js;
pub mod ports;
mod tools;

/// Fixed preview canvas presets as `(id, portrait_width, portrait_height)` in
/// CSS pixels.
pub const PREVIEW_PRESETS: &[(&str, u32, u32)] = &[
    ("iphone-se", 375, 667),
    ("iphone-xr", 414, 896),
    ("iphone-12-pro", 390, 844),
    ("iphone-14-pro-max", 430, 932),
    ("pixel-7", 412, 915),
    ("galaxy-s20-ultra", 412, 915),
    ("ipad-mini", 768, 1024),
    ("ipad-air", 820, 1180),
    ("ipad-pro-12-9", 1024, 1366),
    ("surface-pro-7", 912, 1368),
];

/// A single automation operation requested by the agent, routed to the UI.
///
/// Names/semantics mirror T3's preview toolkit, reduced to the subset a raw
/// WKWebView (`evaluate_script` + `load_url`, no Chrome DevTools Protocol) can
/// serve.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PreviewOp {
    /// Open a URL (creating/showing the webview); `None` just reports status.
    Open { url: Option<String> },
    /// Navigate the current webview to `url`.
    Navigate { url: String },
    /// Report the current URL / title / loading state.
    Status,
    /// Evaluate a JS expression in the page and return its value.
    Evaluate { js: String },
    /// Dispatch a real click at the center of the first `selector` match.
    Click { selector: String },
    /// Focus `selector` and type `text` into it (dispatching input events).
    Type { selector: String, text: String },
    /// Set a fixed WebView canvas, or clear it when both dimensions are `None`.
    Resize {
        width: Option<u32>,
        height: Option<u32>,
    },
    /// Dispatch a keyboard press to the focused page element.
    Press { key: String, modifiers: Vec<String> },
    /// Scroll the window or the first element matching `selector`.
    Scroll {
        delta_x: f64,
        delta_y: f64,
        selector: Option<String>,
    },
    /// Poll page state until all requested conditions match or time out.
    WaitFor {
        selector: Option<String>,
        text: Option<String>,
        url_includes: Option<String>,
        timeout_ms: u64,
    },
    /// Build a DOM outline of interactive elements (role/name/selector), capped.
    Snapshot,
    /// Capture the visible webview region as a PNG.
    Screenshot,
}

/// The UI's answer to a [`PreviewOp`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PreviewReply {
    /// A JSON payload (status, snapshot, evaluate result, `{ "ok": true }`, …).
    Json(serde_json::Value),
    /// A base64-encoded image plus its MIME type (screenshot).
    Image { mime: String, data_base64: String },
}

/// One in-flight automation request handed to the UI: an [`PreviewOp`] plus a
/// bounded channel the UI sends the outcome back on. `Ok` = success payload,
/// `Err` = human-readable failure (surfaced to the agent as a tool error).
#[derive(Debug)]
pub struct BrokerRequest {
    pub session_id: String,
    pub op: PreviewOp,
    pub reply: async_channel::Sender<Result<PreviewReply, String>>,
}

/// The server-side half of the broker: MCP tool handlers call [`Broker::invoke`]
/// to run an op against the UI and await the reply. Cloneable so every tool
/// call shares the one request channel.
pub type Broker = mcp_host::Broker<BrokerRequest>;
pub type TokenRegistry = mcp_host::TokenRegistry<tools::Service>;

/// A running preview MCP server: the URL + per-session bearer-token issuer to
/// register with agents, and the receiver the UI pumps to service automation
/// requests.
pub struct PreviewMcpServer {
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/preview`.
    pub url: String,
    /// Per-session bearer-token registry.
    pub tokens: TokenRegistry,
    /// Automation requests to resolve against the live WebView. The UI consumes
    /// this (single consumer); dropping it makes [`Broker::invoke`] fail fast.
    pub requests: async_channel::Receiver<BrokerRequest>,
}

pub fn start(host: &mut mcp_host::Host) -> PreviewMcpServer {
    let url = host.url("/preview");
    let (req_tx, req_rx) = async_channel::unbounded::<BrokerRequest>();
    let broker = Broker::new(
        req_tx,
        Duration::from_secs(65),
        mcp_host::BrokerErrors {
            unavailable: "preview UI is not available",
            dropped: "preview UI dropped the request",
            timed_out: "preview operation timed out",
        },
    );
    let tokens = TokenRegistry::new(move |session_id| tools::service(broker.clone(), session_id));
    host.mount(mcp_host::route("/preview", &tokens));

    log::info!("preview-mcp: serving at {url}");
    PreviewMcpServer {
        url,
        tokens,
        requests: req_rx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (TokenRegistry, async_channel::Receiver<BrokerRequest>) {
        let (requests, receiver) = async_channel::unbounded();
        let broker = Broker::new(
            requests,
            Duration::from_secs(2),
            mcp_host::BrokerErrors {
                unavailable: "preview UI is not available",
                dropped: "preview UI dropped the request",
                timed_out: "preview operation timed out",
            },
        );
        let registry =
            TokenRegistry::new(move |session_id| tools::service(broker.clone(), session_id));
        (registry, receiver)
    }

    #[test]
    fn registered_tokens_are_distinct() {
        let (registry, _requests) = registry();
        let token_a = registry.register("session-a");
        let token_b = registry.register("session-b");
        assert_ne!(token_a, token_b);
        assert!(registry.contains(&token_a));
        assert!(registry.contains(&token_b));
    }

    #[test]
    fn revoked_token_is_removed() {
        let (registry, _requests) = registry();
        let token = registry.register("session-a");
        registry.revoke(&token);
        assert!(!registry.contains(&token));
    }
}
