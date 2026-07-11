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

/// A single automation operation requested by the agent, routed to the UI.
///
/// Names/semantics mirror T3's preview toolkit, reduced to the subset a raw
/// WKWebView (`evaluate_script` + `load_url`, no Chrome DevTools Protocol) can
/// serve.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Build a DOM outline of interactive elements (role/name/selector), capped.
    Snapshot,
    /// Capture the visible webview region as a PNG.
    Screenshot,
}

/// The UI's answer to a [`PreviewOp`].
#[derive(Debug, Clone)]
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
    pub op: PreviewOp,
    pub reply: async_channel::Sender<Result<PreviewReply, String>>,
}

/// The server-side half of the broker: MCP tool handlers call [`Broker::invoke`]
/// to run an op against the UI and await the reply. Cloneable so every tool
/// call shares the one request channel.
#[derive(Clone)]
pub struct Broker {
    requests: async_channel::Sender<BrokerRequest>,
    timeout: Duration,
}

impl Broker {
    /// Send `op` to the UI and await its reply (or a timeout / disconnect error).
    pub async fn invoke(&self, op: PreviewOp) -> Result<PreviewReply, String> {
        let (tx, rx) = async_channel::bounded(1);
        self.requests
            .send(BrokerRequest { op, reply: tx })
            .await
            .map_err(|_| "preview UI is not available".to_string())?;
        match tokio::time::timeout(self.timeout, rx.recv()).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("preview UI dropped the request".to_string()),
            Err(_) => Err("preview operation timed out".to_string()),
        }
    }
}

/// A running preview MCP server: the URL + bearer token to register with agents,
/// and the receiver the UI pumps to service automation requests.
pub struct PreviewMcpServer {
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/mcp`.
    pub url: String,
    /// Bearer token every request must present.
    pub bearer_token: String,
    /// Automation requests to resolve against the live WebView. The UI consumes
    /// this (single consumer); dropping it makes [`Broker::invoke`] fail fast.
    pub requests: async_channel::Receiver<BrokerRequest>,
}

/// Bind a random loopback port and start the streamable-HTTP MCP server on a
/// dedicated tokio runtime thread. Returns immediately with the bound URL and
/// token; the server keeps running for the process lifetime.
pub fn start() -> std::io::Result<PreviewMcpServer> {
    // Bind synchronously so the caller learns the port before we return.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/mcp");
    let bearer_token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );

    let (req_tx, req_rx) = async_channel::unbounded::<BrokerRequest>();
    let broker = Broker {
        requests: req_tx,
        timeout: Duration::from_secs(30),
    };
    let token = bearer_token.clone();

    std::thread::Builder::new()
        .name("preview-mcp".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    log::error!("preview-mcp: failed to build tokio runtime: {err}");
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(err) = tools::serve(listener, broker, token).await {
                    log::error!("preview-mcp: server exited with error: {err}");
                }
            });
        })?;

    log::info!("preview-mcp: serving at {url}");
    Ok(PreviewMcpServer {
        url,
        bearer_token,
        requests: req_rx,
    })
}
