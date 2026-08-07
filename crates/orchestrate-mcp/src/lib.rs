//! In-process MCP server for dispatching work to child tcode threads.

use std::time::Duration;

mod tools;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrchestrateOp {
    Dispatch {
        parent_id: String,
        provider: String,
        model: Option<String>,
        effort: Option<String>,
        profile: Option<String>,
        access: Option<String>,
        title: String,
        brief: String,
        cwd: Option<String>,
        archive_on_complete: Option<bool>,
        result_max_chars: Option<u32>,
    },
    Status {
        parent_id: String,
        thread_id: Option<String>,
    },
    Send {
        parent_id: String,
        thread_id: String,
        message: String,
    },
    Result {
        parent_id: String,
        thread_id: String,
    },
    Cancel {
        parent_id: String,
        thread_id: String,
    },
    Archive {
        parent_id: String,
        thread_ids: Vec<String>,
    },
    Approve {
        parent_id: String,
        thread_id: String,
        request_id: Option<String>,
        decision: String,
    },
}

#[derive(Debug)]
pub struct BrokerRequest {
    pub op: OrchestrateOp,
    pub reply: async_channel::Sender<Result<serde_json::Value, String>>,
}

pub type Broker = mcp_host::Broker<BrokerRequest>;
pub type TokenRegistry = mcp_host::TokenRegistry<tools::Service>;

pub struct OrchestrateMcpServer {
    pub url: String,
    pub tokens: TokenRegistry,
    pub requests: async_channel::Receiver<BrokerRequest>,
}

pub fn start(host: &mut mcp_host::Host) -> OrchestrateMcpServer {
    let url = host.url("/orchestrate");
    let (req_tx, req_rx) = async_channel::unbounded();
    let broker = Broker::new(
        req_tx,
        Duration::from_secs(30),
        mcp_host::BrokerErrors {
            unavailable: "tcode orchestrator is not available",
            dropped: "tcode orchestrator dropped the request",
            timed_out: "orchestrator operation timed out",
        },
    );
    let tokens = TokenRegistry::new(move |parent_id| tools::service(broker.clone(), parent_id));
    host.mount(mcp_host::route("/orchestrate", &tokens));

    log::info!("orchestrate-mcp: serving at {url}");
    OrchestrateMcpServer {
        url,
        tokens,
        requests: req_rx,
    }
}
