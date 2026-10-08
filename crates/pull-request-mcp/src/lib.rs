use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub url: Option<String>,
    pub repository: Option<String>,
    pub number: Option<u64>,
    pub host: Option<String>,
}
#[derive(Debug)]
pub enum Operation {
    Link(Target),
    Unlink(Target),
    List,
    Watch(Target),
    Unwatch(Target),
}
pub struct BrokerRequest {
    pub session_id: String,
    pub operation: Operation,
    pub reply: async_channel::Sender<Result<serde_json::Value, String>>,
}
pub type Broker = mcp_host::Broker<BrokerRequest>;
pub type Service = StreamableHttpService<PullRequestTools, LocalSessionManager>;
pub type TokenRegistry = mcp_host::TokenRegistry<Service>;
pub struct PullRequestMcpServer {
    pub url: String,
    pub tokens: TokenRegistry,
    pub requests: async_channel::Receiver<BrokerRequest>,
}
pub fn start(host: &mut mcp_host::Host) -> PullRequestMcpServer {
    let (sender, requests) = async_channel::unbounded();
    let broker = Broker::new(
        sender,
        Duration::from_secs(35),
        mcp_host::BrokerErrors {
            unavailable: "pull request host is unavailable",
            dropped: "pull request host dropped the request",
            timed_out: "pull request operation timed out",
        },
    );
    let tokens = TokenRegistry::new(move |session_id| {
        let broker = broker.clone();
        StreamableHttpService::new(
            move || Ok(PullRequestTools::new(broker.clone(), session_id.clone())),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        )
    });
    host.mount(mcp_host::route("/pull-requests", &tokens));
    PullRequestMcpServer {
        url: host.url("/pull-requests"),
        tokens,
        requests,
    }
}
#[derive(Clone)]
pub struct PullRequestTools {
    broker: Broker,
    session_id: String,
    tool_router: ToolRouter<Self>,
}
#[tool_router]
impl PullRequestTools {
    fn new(broker: Broker, session_id: String) -> Self {
        Self {
            broker,
            session_id,
            tool_router: Self::tool_router(),
        }
    }
    async fn invoke(&self, operation: Operation) -> CallToolResult {
        match self
            .broker
            .invoke(|reply| BrokerRequest {
                session_id: self.session_id.clone(),
                operation,
                reply,
            })
            .await
        {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]),
        }
    }
    #[tool(
        description = "Register every PR you create or work on for this thread, including every layer of a stack, immediately. Pass a URL or repository plus number. An existing link is preserved and succeeds with alreadyLinked=true."
    )]
    async fn link_pull_request(&self, Parameters(target): Parameters<Target>) -> CallToolResult {
        self.invoke(Operation::Link(target)).await
    }
    #[tool(
        description = "Unlink a PR from this thread. It stays unlinked, even when discovered again or found in a stack, until explicitly linked again. Pass a URL or repository plus number."
    )]
    async fn unlink_pull_request(&self, Parameters(target): Parameters<Target>) -> CallToolResult {
        self.invoke(Operation::Unlink(target)).await
    }
    #[tool(
        description = "List this thread's visible linked PRs, their source, last known state and stack position. This reads stored state without asking GitHub. Before finishing PR work, list and link anything missing."
    )]
    async fn list_thread_pull_requests(&self) -> CallToolResult {
        self.invoke(Operation::List).await
    }
    #[tool(
        description = "Have Tcode watch an open pull request for this thread, linking it first if needed. Tcode checks it every two minutes and wakes you with a message when a check fails, the required checks pass, someone else comments or reviews, or the branch starts to conflict with its base. Use this to monitor or babysit a pull request instead of polling, sleeping, or running a watcher. Only comments posted after this call wake you, so handle the existing ones first, then end your turn. A wake is news, not a merge decision: check readiness yourself before merging. When you hand the work back to the user, call unwatch_pull_request first. Watching ends when the pull request merges or closes, when its thread settles or is archived, when Tcode fails to read it 8 times in a row (a host rate limit only delays it), when the user stops this thread, or when you call unwatch_pull_request. Unsettle the thread before starting a new watch. A subagent cannot watch: its parent thread owns the pull request."
    )]
    async fn watch_pull_request(&self, Parameters(target): Parameters<Target>) -> CallToolResult {
        self.invoke(Operation::Watch(target)).await
    }
    #[tool(
        description = "Stop Tcode from watching a pull request for this thread. The pull request stays linked. Pass the URL, or repository plus number."
    )]
    async fn unwatch_pull_request(&self, Parameters(target): Parameters<Target>) -> CallToolResult {
        self.invoke(Operation::Unwatch(target)).await
    }
}

/// Every tool's name and the description its model reads, in the order they are listed.
pub fn tool_descriptions() -> &'static [(String, String)] {
    static TOOLS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    TOOLS.get_or_init(|| {
        PullRequestTools::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| {
                (
                    tool.name.to_string(),
                    tool.description.as_deref().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    })
}
#[tool_handler(router = self.tool_router)]
impl ServerHandler for PullRequestTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Link every PR and every stack layer you create or work on immediately. \
                 List and link missing PRs before finishing. Do not link background mentions. \
                 Report linking failures.",
            )
    }
}
