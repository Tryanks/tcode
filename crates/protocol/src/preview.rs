//! Shared preview reverse-RPC payloads used by clients and the MCP broker.

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PreviewRequest {
    /// Open a URL (creating/showing the webview); `None` just reports status.
    Open { url: Option<String> },
    /// Navigate the current webview to `url`.
    Navigate { url: String },
    /// Report the current URL / title / loading state.
    Status,
    /// Evaluate a JS expression in the page and return its value.
    Evaluate { js: String },
    /// Click the element a snapshot's `ref` names, or the first `selector`
    /// match.
    Click {
        selector: Option<String>,
        r#ref: Option<String>,
    },
    /// Focus the element and type `text` into it (dispatching input events).
    Type {
        selector: Option<String>,
        r#ref: Option<String>,
        text: String,
    },
    /// Set a fixed WebView canvas, or clear it when both dimensions are `None`.
    Resize {
        width: Option<u32>,
        height: Option<u32>,
    },
    /// Dispatch a keyboard press to the focused page element.
    Press { key: String, modifiers: Vec<String> },
    /// Scroll the window, or the element `ref` or `selector` names.
    Scroll {
        delta_x: f64,
        delta_y: f64,
        selector: Option<String>,
        r#ref: Option<String>,
    },
    /// Wait until all requested conditions match or time out.
    WaitFor {
        selector: Option<String>,
        text: Option<String>,
        url_includes: Option<String>,
        timeout_ms: u64,
    },
    /// The page's accessibility outline, each element with a ref.
    Snapshot,
    /// A PNG of what is on screen, of the element a snapshot's `ref` names,
    /// or of the whole document.
    Screenshot {
        r#ref: Option<String>,
        full_page: bool,
    },
}

impl PreviewRequest {
    /// Which call this is, without what it carries: a URL, text typed into a
    /// field or a script can hold a secret, so this is what a log names.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::Navigate { .. } => "navigate",
            Self::Status => "status",
            Self::Evaluate { .. } => "evaluate",
            Self::Click { .. } => "click",
            Self::Type { .. } => "type",
            Self::Resize { .. } => "resize",
            Self::Press { .. } => "press",
            Self::Scroll { .. } => "scroll",
            Self::WaitFor { .. } => "wait_for",
            Self::Snapshot => "snapshot",
            Self::Screenshot { .. } => "screenshot",
        }
    }
}

/// The UI's answer to a [`PreviewRequest`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PreviewResponse {
    /// A JSON payload (status, snapshot, evaluate result, `{ "ok": true }`, …).
    Json(serde_json::Value),
    /// A base64-encoded image plus its MIME type (screenshot).
    Image { mime: String, data_base64: String },
}

/// A client that can render preview pages, as it names itself on its
/// [`crate::Topic::Preview`] subscription.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PreviewClient {
    /// Chosen by the client for the life of its process; stable across its
    /// reconnects.
    pub id: String,
    /// The device's name, as the user sees it.
    pub name: String,
}

/// One page of a thread's preview.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviewTab {
    /// None for a page sized before anything was loaded into it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// A fixed page size, or none to fill the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<(u32, u32)>,
}

/// The host-held preview of one thread. Exactly one attached client renders
/// its pages and receives its agent's preview requests; every other client
/// shows this state and can take rendering over.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct PreviewSession {
    pub session_id: String,
    /// None while no attached client renders the thread's pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renderer: Option<PreviewClient>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<PreviewTab>,
    /// The user acted on the page since the agent last read it with a
    /// snapshot, so what the agent read may no longer be what is there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user_acted_since_snapshot: bool,
}

/// A TCP listener on the host machine that a thread's preview offers as a
/// page to open: one of the user's own processes listening on loopback or on
/// every interface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DevServer {
    pub port: u16,
    pub pid: u32,
    /// The owning process's executable name.
    pub process: String,
    /// The process's working directory relative to the thread's, `""` at its
    /// root; none when it is outside it or the host cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}
