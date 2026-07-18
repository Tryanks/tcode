//! In-process `tcode_computer_use` MCP server: pi-computer-use-style desktop
//! automation for every provider (accessibility-tree observation, state-scoped
//! refs, transactional actions). See `docs/computer-use.md` for the design.
//!
//! Served over streamable HTTP on `127.0.0.1:<random port>` with a bearer
//! token, mirroring `preview-mcp` / `orchestrate-mcp`. The macOS backend talks
//! to the AX C API, CGEvent, and `screencapture`; other platforms serve a stub
//! that reports the platform as unsupported.

pub mod backend;
pub mod outline;
pub mod permissions;
pub mod state;
pub mod tools;

use std::sync::{Arc, RwLock};

/// A running computer-use MCP server and the bearer token required to access it.
pub struct ComputerUseMcpServer {
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/mcp`.
    pub url: String,
    /// Bearer token presented by every registered provider session.
    pub token: String,
}

/// Bind a random loopback port and start the authenticated streamable-HTTP MCP
/// server on a dedicated tokio runtime thread.
pub fn start() -> std::io::Result<ComputerUseMcpServer> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/mcp");
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut services = tools::Services::new();
    services.insert(token.clone(), tools::service());
    let services = Arc::new(RwLock::new(services));

    std::thread::Builder::new()
        .name("computer-use-mcp".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    log::error!("computer-use-mcp: failed to build tokio runtime: {err}");
                    return;
                }
            };
            runtime.block_on(async move {
                if let Err(err) = tools::serve(listener, services).await {
                    log::error!("computer-use-mcp: server exited with error: {err}");
                }
            });
        })?;

    log::info!("computer-use-mcp: serving at {url}");
    Ok(ComputerUseMcpServer { url, token })
}
