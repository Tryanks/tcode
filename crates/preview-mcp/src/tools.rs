//! The rmcp tool surface (streamable-HTTP `ServerHandler`) and its axum host
//! with bearer-token auth. Each tool turns its arguments into a [`PreviewOp`],
//! runs it through the [`Broker`], and maps the reply into an MCP result.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::{Broker, PreviewOp, PreviewReply};

/// Optional-URL argument for `preview_open`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenParams {
    /// URL to load. When omitted, the tool just shows the current webview.
    #[serde(default)]
    pub url: Option<String>,
}

/// Required-URL argument for `preview_navigate`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NavigateParams {
    /// Absolute URL (or `localhost:PORT`) to navigate the preview to.
    pub url: String,
}

/// A JavaScript expression to evaluate in the page.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvaluateParams {
    /// A JS expression; its value is returned (must be JSON-serializable).
    pub js: String,
}

/// A CSS selector to target.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClickParams {
    /// CSS selector of the element to click (first match).
    pub selector: String,
}

/// A CSS selector plus text to type.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeParams {
    /// CSS selector of the field to type into (first match).
    pub selector: String,
    /// Text to set as the field's value.
    pub text: String,
}

/// The MCP server handler: one shared [`Broker`] plus the generated tool router.
#[derive(Clone)]
pub struct PreviewTools {
    broker: Broker,
    session_id: String,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl PreviewTools {
    pub fn new(broker: Broker, session_id: String) -> Self {
        Self {
            broker,
            session_id,
            tool_router: Self::tool_router(),
        }
    }

    /// Open the preview browser, optionally navigating to a URL, and report status.
    #[tool(
        description = "Open the tcode preview browser (optionally at a URL) and return its status."
    )]
    async fn preview_open(
        &self,
        Parameters(params): Parameters<OpenParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.run(PreviewOp::Open { url: params.url }).await)
    }

    /// Navigate the preview browser to a URL.
    #[tool(description = "Navigate the tcode preview browser to a URL and return its status.")]
    async fn preview_navigate(
        &self,
        Parameters(params): Parameters<NavigateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.run(PreviewOp::Navigate { url: params.url }).await)
    }

    /// Report the preview browser's current URL, title, and loading state.
    #[tool(description = "Report the preview browser's current URL, title, and loading state.")]
    async fn preview_status(&self) -> Result<CallToolResult, ErrorData> {
        Ok(self.run(PreviewOp::Status).await)
    }

    /// Evaluate a JavaScript expression in the page and return its value.
    #[tool(
        description = "Evaluate a JavaScript expression in the preview page and return its value."
    )]
    async fn preview_evaluate(
        &self,
        Parameters(params): Parameters<EvaluateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self.run(PreviewOp::Evaluate { js: params.js }).await)
    }

    /// Click the first element matching a CSS selector.
    #[tool(description = "Click the first element matching a CSS selector in the preview page.")]
    async fn preview_click(
        &self,
        Parameters(params): Parameters<ClickParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(PreviewOp::Click {
                selector: params.selector,
            })
            .await)
    }

    /// Type text into the first element matching a CSS selector.
    #[tool(
        description = "Type text into the first element matching a CSS selector in the preview page."
    )]
    async fn preview_type(
        &self,
        Parameters(params): Parameters<TypeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(PreviewOp::Type {
                selector: params.selector,
                text: params.text,
            })
            .await)
    }

    /// Build a DOM outline of the page's interactive elements (role/name/selector).
    #[tool(
        description = "Snapshot the preview page: URL, title, visible text, and interactive elements (role/name/selector)."
    )]
    async fn preview_snapshot(&self) -> Result<CallToolResult, ErrorData> {
        Ok(self.run(PreviewOp::Snapshot).await)
    }

    /// Capture the visible preview region as a PNG image.
    #[tool(description = "Capture a screenshot of the visible preview browser region as a PNG.")]
    async fn preview_screenshot(&self) -> Result<CallToolResult, ErrorData> {
        Ok(self.run(PreviewOp::Screenshot).await)
    }

    /// Route one op through the broker and map its reply into a tool result.
    async fn run(&self, op: PreviewOp) -> CallToolResult {
        log::info!("preview-mcp: tool invoked: {op:?}");
        match self.broker.invoke(&self.session_id, op).await {
            Ok(PreviewReply::Json(value)) => {
                let text =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Ok(PreviewReply::Image { mime, data_base64 }) => {
                CallToolResult::success(vec![ContentBlock::image(data_base64, mime)])
            }
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PreviewTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Drive the tcode embedded preview browser: open/navigate URLs, inspect and \
                 automate the page, and capture screenshots.",
            )
    }
}

pub type Service = StreamableHttpService<PreviewTools, LocalSessionManager>;

pub struct ServiceEntry {
    pub session_id: String,
    service: Service,
}

pub type Services = HashMap<String, ServiceEntry>;

pub fn service(broker: Broker, session_id: String) -> ServiceEntry {
    let service_session_id = session_id.clone();
    let service = StreamableHttpService::new(
        move || {
            Ok(PreviewTools::new(
                broker.clone(),
                service_session_id.clone(),
            ))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    ServiceEntry {
        session_id,
        service,
    }
}

/// Serve the streamable-HTTP MCP endpoint at `/mcp` on `listener`, resolving
/// each bearer token to its per-session service.
pub async fn serve(
    listener: std::net::TcpListener,
    services: Arc<RwLock<Services>>,
) -> std::io::Result<()> {
    let app = Router::new()
        .route("/mcp", any(handle))
        .with_state(services);

    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    axum::serve(listener, app).await
}

/// Bearer-gate every request, then hand it to the rmcp streamable-HTTP service.
async fn handle(
    State(services): State<Arc<RwLock<Services>>>,
    req: axum::extract::Request,
) -> Response {
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let service = token.and_then(|token| {
        services.read().unwrap().get(token).map(|entry| {
            log::debug!(
                "preview-mcp: authorized request for session {}",
                entry.session_id
            );
            entry.service.clone()
        })
    });
    let Some(service) = service else {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    };
    let response = service.handle(req).await;
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, axum::body::Body::new(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn broker_roundtrip_with_fake_resolver() {
        // Fake UI: echoes op kind back as JSON.
        let (tx, rx) = async_channel::unbounded::<crate::BrokerRequest>();
        let broker = Broker {
            requests: tx,
            timeout: std::time::Duration::from_secs(2),
        };
        let resolver = tokio::spawn(async move {
            while let Ok(request) = rx.recv().await {
                assert_eq!(request.session_id, "session-a");
                let reply = match &request.op {
                    PreviewOp::Status => {
                        PreviewReply::Json(serde_json::json!({ "url": "https://x/" }))
                    }
                    PreviewOp::Screenshot => PreviewReply::Image {
                        mime: "image/png".into(),
                        data_base64: "AAA".into(),
                    },
                    _ => PreviewReply::Json(serde_json::json!({ "ok": true })),
                };
                let _ = request.reply.send(Ok(reply)).await;
            }
        });

        let tools = PreviewTools::new(broker, "session-a".into());
        let status = tools.run(PreviewOp::Status).await;
        assert_eq!(status.is_error, Some(false));
        let shot = tools.run(PreviewOp::Screenshot).await;
        assert_eq!(shot.is_error, Some(false));

        resolver.abort();
    }

    #[tokio::test]
    async fn broker_reports_disconnect_as_error() {
        let (tx, rx) = async_channel::unbounded::<crate::BrokerRequest>();
        drop(rx); // no UI listening
        let broker = Broker {
            requests: tx,
            timeout: std::time::Duration::from_millis(200),
        };
        let tools = PreviewTools::new(broker, "session-a".into());
        let result = tools.run(PreviewOp::Status).await;
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn tools_are_registered() {
        let (tx, _rx) = async_channel::unbounded::<crate::BrokerRequest>();
        let broker = Broker {
            requests: tx,
            timeout: std::time::Duration::from_secs(1),
        };
        let tools = PreviewTools::new(broker, "session-a".into());
        let names: Vec<String> = tools
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for expected in [
            "preview_open",
            "preview_navigate",
            "preview_status",
            "preview_evaluate",
            "preview_click",
            "preview_type",
            "preview_snapshot",
            "preview_screenshot",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing tool {expected}"
            );
        }
    }
}
