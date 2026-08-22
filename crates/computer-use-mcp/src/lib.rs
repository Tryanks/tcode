//! In-process `tcode_computer_use` MCP server: pi-computer-use-style desktop
//! automation for every provider (accessibility-tree observation, state-scoped
//! refs, transactional actions). See `docs/computer-use.md` for the design.
//!
//! Served over the shared loopback streamable-HTTP host with a distinct bearer
//! token. The macOS backend talks to the AX C API, CGEvent, `screencapture`,
//! and Vision OCR; other platforms report that computer use is unsupported.

pub mod backend;
pub mod config;
pub mod outline;
pub mod permissions;
pub mod state;
pub mod tools;

/// A running computer-use MCP server and the bearer token required to access it.
pub struct ComputerUseMcpServer {
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/computer-use`.
    pub url: String,
    /// Bearer token presented by every registered provider session.
    pub token: String,
}

/// Register the authenticated computer-use route on the shared MCP host.
pub fn start(host: &mut mcp_host::Host) -> ComputerUseMcpServer {
    let url = host.url("/computer-use");
    let tokens = mcp_host::TokenRegistry::<tools::Service>::new(|_| tools::service());
    let token = tokens.register("computer-use");
    host.mount(mcp_host::route("/computer-use", &tokens));

    log::info!("computer-use-mcp: serving at {url}");
    ComputerUseMcpServer { url, token }
}
