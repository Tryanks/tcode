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
    CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::{Broker, OrchestrateOp};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DispatchParams {
    provider: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    access: Option<String>,
    title: String,
    brief: String,
    #[serde(default)]
    cwd: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StatusParams {
    #[serde(default)]
    thread_id: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendParams {
    thread_id: String,
    message: String,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ThreadParams {
    thread_id: String,
}

#[derive(Clone)]
pub struct OrchestrateTools {
    broker: Broker,
    parent_id: String,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl OrchestrateTools {
    fn new(broker: Broker, parent_id: String) -> Self {
        Self {
            broker,
            parent_id,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Dispatch a brief to a new child tcode thread and return its thread id. access is one of read_only (review/investigation: the child cannot change files; actions beyond that pause for user approval), workspace_write (edits auto-approved inside the workspace), or full (default; no approval prompts)."
    )]
    async fn dispatch(
        &self,
        Parameters(p): Parameters<DispatchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(OrchestrateOp::Dispatch {
                parent_id: self.parent_id.clone(),
                provider: p.provider,
                model: p.model,
                effort: p.effort,
                access: p.access,
                title: p.title,
                brief: p.brief,
                cwd: p.cwd,
            })
            .await)
    }

    #[tool(description = "List child thread status, optionally for one thread.")]
    async fn status(
        &self,
        Parameters(p): Parameters<StatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(OrchestrateOp::Status {
                parent_id: self.parent_id.clone(),
                thread_id: p.thread_id,
            })
            .await)
    }

    #[tool(
        description = "Send a follow-up message to one of this session's child threads. If the child has a turn in flight the message is steered into it immediately; otherwise it is queued and sent as the child's next turn. The response reports which (delivery: steered | queued)."
    )]
    async fn send(
        &self,
        Parameters(p): Parameters<SendParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(OrchestrateOp::Send {
                parent_id: self.parent_id.clone(),
                thread_id: p.thread_id,
                message: p.message,
            })
            .await)
    }

    #[tool(description = "Read a finished child thread's final assistant message.")]
    async fn result(
        &self,
        Parameters(p): Parameters<ThreadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(OrchestrateOp::Result {
                parent_id: self.parent_id.clone(),
                thread_id: p.thread_id,
            })
            .await)
    }

    #[tool(description = "Cancel and shut down one of this session's child threads.")]
    async fn cancel(
        &self,
        Parameters(p): Parameters<ThreadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(OrchestrateOp::Cancel {
                parent_id: self.parent_id.clone(),
                thread_id: p.thread_id,
            })
            .await)
    }

    async fn run(&self, op: OrchestrateOp) -> CallToolResult {
        match self.broker.invoke(op).await {
            Ok(value) => CallToolResult::success(vec![Content::text(value.to_string())]),
            Err(message) => CallToolResult::error(vec![Content::text(message)]),
        }
    }
}

#[tool_handler]
impl ServerHandler for OrchestrateTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "Dispatch and coordinate work in isolated child tcode threads.".into(),
            ),
        }
    }
}

pub type Service = StreamableHttpService<OrchestrateTools>;
pub type Services = HashMap<String, Service>;

pub fn service(broker: Broker, parent_id: String) -> Service {
    StreamableHttpService::new(
        move || Ok(OrchestrateTools::new(broker.clone(), parent_id.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

pub async fn serve(
    listener: std::net::TcpListener,
    services: Arc<RwLock<Services>>,
) -> std::io::Result<()> {
    let app = Router::new()
        .route("/mcp", any(handle))
        .with_state(services);
    listener.set_nonblocking(true)?;
    axum::serve(tokio::net::TcpListener::from_std(listener)?, app).await
}

async fn handle(
    State(services): State<Arc<RwLock<Services>>>,
    req: axum::extract::Request,
) -> Response {
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let service = token.and_then(|token| services.read().unwrap().get(token).cloned());
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
    async fn broker_op_reply_roundtrip_preserves_parent() {
        let (tx, rx) = async_channel::unbounded();
        let broker = Broker {
            requests: tx,
            timeout: std::time::Duration::from_secs(2),
        };
        let resolver = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            assert!(
                matches!(request.op, OrchestrateOp::Status { parent_id, thread_id: None } if parent_id == "parent")
            );
            request.reply.send(Ok(serde_json::json!([]))).await.unwrap();
        });
        let result = OrchestrateTools::new(broker, "parent".into())
            .run(OrchestrateOp::Status {
                parent_id: "parent".into(),
                thread_id: None,
            })
            .await;
        assert_eq!(result.is_error, Some(false));
        resolver.await.unwrap();
    }

    #[test]
    fn all_tools_are_registered() {
        let (tx, _rx) = async_channel::unbounded();
        let tools = OrchestrateTools::new(
            Broker {
                requests: tx,
                timeout: std::time::Duration::from_secs(1),
            },
            "p".into(),
        );
        let mut names: Vec<_> = tools
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["cancel", "dispatch", "result", "send", "status"]);
    }
}
