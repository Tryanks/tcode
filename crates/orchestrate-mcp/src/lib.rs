//! In-process MCP server for dispatching work to child tcode threads.

use std::sync::{Arc, RwLock};
use std::time::Duration;

mod tools;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrchestrateOp {
    Dispatch {
        parent_id: String,
        provider: String,
        model: Option<String>,
        effort: Option<String>,
        title: String,
        brief: String,
        cwd: Option<String>,
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
}

#[derive(Debug)]
pub struct BrokerRequest {
    pub op: OrchestrateOp,
    pub reply: async_channel::Sender<Result<serde_json::Value, String>>,
}

#[derive(Clone)]
pub struct Broker {
    requests: async_channel::Sender<BrokerRequest>,
    timeout: Duration,
}

impl Broker {
    pub async fn invoke(&self, op: OrchestrateOp) -> Result<serde_json::Value, String> {
        let (tx, rx) = async_channel::bounded(1);
        self.requests
            .send(BrokerRequest { op, reply: tx })
            .await
            .map_err(|_| "tcode orchestrator is not available".to_string())?;
        match tokio::time::timeout(self.timeout, rx.recv()).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("tcode orchestrator dropped the request".to_string()),
            Err(_) => Err("orchestrator operation timed out".to_string()),
        }
    }
}

#[derive(Clone)]
pub struct TokenRegistry {
    inner: Arc<RwLock<tools::Services>>,
    broker: Broker,
}

impl TokenRegistry {
    /// Mint a distinct bearer token whose tool calls are permanently scoped to
    /// `parent_session_id`.
    pub fn register(&self, parent_session_id: &str) -> String {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let service = tools::service(self.broker.clone(), parent_session_id.to_string());
        self.inner.write().unwrap().insert(token.clone(), service);
        token
    }

    pub fn revoke(&self, token: &str) {
        self.inner.write().unwrap().remove(token);
    }
}

pub struct OrchestrateMcpServer {
    pub url: String,
    pub tokens: TokenRegistry,
    pub requests: async_channel::Receiver<BrokerRequest>,
}

pub fn start() -> std::io::Result<OrchestrateMcpServer> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/mcp");
    let (req_tx, req_rx) = async_channel::unbounded();
    let broker = Broker {
        requests: req_tx,
        timeout: Duration::from_secs(30),
    };
    let services = Arc::new(RwLock::new(tools::Services::new()));
    let tokens = TokenRegistry {
        inner: services.clone(),
        broker,
    };

    std::thread::Builder::new()
        .name("orchestrate-mcp".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    log::error!("orchestrate-mcp: failed to build tokio runtime: {err}");
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(err) = tools::serve(listener, services).await {
                    log::error!("orchestrate-mcp: server exited with error: {err}");
                }
            });
        })?;

    log::info!("orchestrate-mcp: serving at {url}");
    Ok(OrchestrateMcpServer {
        url,
        tokens,
        requests: req_rx,
    })
}
