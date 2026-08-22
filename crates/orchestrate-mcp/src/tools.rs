use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
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
    profile: Option<String>,
    #[serde(default)]
    access: Option<String>,
    title: String,
    brief: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Override Settings → Orchestrate child-worktree isolation for this dispatch. When true and cwd resolves to a Git repository root, the child runs on branch tcode/<thread-id> in a dedicated worktree. The response includes its path and branch. Non-Git cwd or creation failure falls back to cwd and reports a warning."
    )]
    worktree: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "Override the auto-archive policy for this child. By default (per Settings → Orchestrate) a completed child is archived once its terminal result reaches you; failed children always stay visible. Set false to keep a completed child in the sidebar; send to an archived child unarchives it."
    )]
    archive_on_complete: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "Character cap for the inline result text in the completion callback (default 1200; 0 = unlimited). Raise it or pass 0 when you will need the full report anyway — cheaper than a follow-up result call."
    )]
    result_max_chars: Option<u32>,
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
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ArchiveParams {
    thread_ids: Vec<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ApproveParams {
    thread_id: String,
    #[serde(default)]
    request_id: Option<String>,
    decision: String,
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
        description = "Dispatch a brief to a new child tcode thread and return its thread id. profile is the provider-profile id from the fleet table, required when the entry names one. access is one of read_only (review/investigation: read-only actions run without prompts; anything that mutates pauses for user approval), workspace_write (edits auto-approved inside the workspace), or full (default; no approval prompts). worktree optionally isolates the child in tcode/<thread-id> and overrides the Orchestrate setting; the response identifies the path and branch or explains fallback. Completed children are auto-archived after their result is delivered unless archive_on_complete: false; failed children stay visible for retries."
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
                profile: p.profile,
                access: p.access,
                title: p.title,
                brief: p.brief,
                cwd: p.cwd,
                worktree: p.worktree,
                archive_on_complete: p.archive_on_complete,
                result_max_chars: p.result_max_chars,
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

    #[tool(
        description = "Archive a batch of this session's child threads by id. Completed children are auto-archived by default, so this is mainly for failed children you will not retry and children dispatched with archive_on_complete: false. Archived threads vanish from the user's sidebar but are fully recoverable in Settings → Archived Threads, and their transcripts remain readable via status/result. Archiving a running child shuts it down; cancel first for a clean stop."
    )]
    async fn archive(
        &self,
        Parameters(p): Parameters<ArchiveParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(OrchestrateOp::Archive {
                parent_id: self.parent_id.clone(),
                thread_ids: p.thread_ids,
            })
            .await)
    }

    #[tool(
        description = "Answer a child thread's pending permission approval. decision is one of approve (this request only), approve_for_session (stop asking for similar requests this session), or deny. request_id may be omitted when the child has exactly one pending request."
    )]
    async fn approve(
        &self,
        Parameters(p): Parameters<ApproveParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(self
            .run(OrchestrateOp::Approve {
                parent_id: self.parent_id.clone(),
                thread_id: p.thread_id,
                request_id: p.request_id,
                decision: p.decision,
            })
            .await)
    }

    async fn run(&self, op: OrchestrateOp) -> CallToolResult {
        run_op(&self.broker, op).await
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReportResultParams {
    #[schemars(
        description = "The complete final report, verbatim. Do not summarize or truncate; this exact text is what the orchestrator receives."
    )]
    text: String,
}

/// The single-tool server registered with child threads: a channel for pushing
/// the full RESULT text to the orchestrator instead of relying on whatever the
/// child's last message happens to be.
#[derive(Clone)]
pub struct ChildReportTools {
    broker: Broker,
    child_id: String,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ChildReportTools {
    fn new(broker: Broker, child_id: String) -> Self {
        Self {
            broker,
            child_id,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Deliver the full text of your final report (RESULT) to the orchestrator that dispatched this thread. Call it once when your work is complete, before ending your turn; calling again replaces the previous report (last call wins). The text reaches the orchestrator verbatim and in full when this turn ends. If you never call this, only your final assistant message is sent back — truncated when long."
    )]
    async fn report_result(
        &self,
        Parameters(p): Parameters<ReportResultParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(run_op(
            &self.broker,
            OrchestrateOp::ReportResult {
                child_id: self.child_id.clone(),
                text: p.text,
            },
        )
        .await)
    }
}

async fn run_op(broker: &Broker, op: OrchestrateOp) -> CallToolResult {
    match broker
        .invoke(|reply| crate::BrokerRequest { op, reply })
        .await
    {
        Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
        Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ChildReportTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Report this thread's final result to the orchestrator that dispatched it.",
            )
    }
}

pub type ChildService = StreamableHttpService<ChildReportTools, LocalSessionManager>;

pub fn child_service(broker: Broker, child_id: String) -> ChildService {
    StreamableHttpService::new(
        move || Ok(ChildReportTools::new(broker.clone(), child_id.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OrchestrateTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::from_build_env())
            .with_instructions("Dispatch and coordinate work in isolated child tcode threads.")
    }
}

pub type Service = StreamableHttpService<OrchestrateTools, LocalSessionManager>;

pub fn service(broker: Broker, parent_id: String) -> Service {
    StreamableHttpService::new(
        move || Ok(OrchestrateTools::new(broker.clone(), parent_id.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker(
        requests: async_channel::Sender<crate::BrokerRequest>,
        timeout: std::time::Duration,
    ) -> Broker {
        Broker::new(
            requests,
            timeout,
            mcp_host::BrokerErrors {
                unavailable: "tcode orchestrator is not available",
                dropped: "tcode orchestrator dropped the request",
                timed_out: "orchestrator operation timed out",
            },
        )
    }

    #[tokio::test]
    async fn broker_op_reply_roundtrip_preserves_parent() {
        let (tx, rx) = async_channel::unbounded();
        let broker = broker(tx, std::time::Duration::from_secs(2));
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
        let tools =
            OrchestrateTools::new(broker(tx, std::time::Duration::from_secs(1)), "p".into());
        let mut names: Vec<_> = tools
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "approve", "archive", "cancel", "dispatch", "result", "send", "status"
            ]
        );
    }

    #[test]
    fn child_service_exposes_only_report_result() {
        let (tx, _rx) = async_channel::unbounded();
        let tools = ChildReportTools::new(
            broker(tx, std::time::Duration::from_secs(1)),
            "child".into(),
        );
        let names: Vec<_> = tools
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(names, ["report_result"]);
    }

    #[tokio::test]
    async fn report_result_carries_child_scope() {
        let (tx, rx) = async_channel::unbounded();
        let broker = broker(tx, std::time::Duration::from_secs(2));
        let resolver = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            assert!(
                matches!(request.op, OrchestrateOp::ReportResult { child_id, text }
                    if child_id == "child" && text == "full report")
            );
            request
                .reply
                .send(Ok(serde_json::json!({ "ok": true })))
                .await
                .unwrap();
        });
        let result = ChildReportTools::new(broker, "child".into())
            .report_result(Parameters(ReportResultParams {
                text: "full report".into(),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        resolver.await.unwrap();
    }
}
