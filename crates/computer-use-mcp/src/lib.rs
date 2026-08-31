//! In-process `tcode_computer_use` MCP server: pi-computer-use-style desktop
//! automation for every provider (accessibility-tree observation, state-scoped
//! refs, transactional actions). See `docs/computer-use.md` for the design.
//!
//! Served over the shared loopback streamable-HTTP host with a distinct bearer
//! token. macOS uses AX/CGEvent and Windows adapts the `uiautomation` crate;
//! other platforms report that computer use is unsupported.

pub mod backend;
pub mod config;
pub mod outline;
pub mod permissions;
pub mod state;
pub mod tools;

/// Return the frontmost application pid on macOS, or `None` elsewhere.
pub fn frontmost_pid() -> Option<u32> {
    backend::frontmost_pid()
}

/// A running computer-use MCP server and the bearer token required to access it.
pub struct ComputerUseMcpServer {
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/computer-use`.
    pub url: String,
    /// Bearer token presented by every registered provider session.
    pub token: String,
}

/// Diagnostic entry for `tcode --cu-smoke`: exercises the platform backend
/// (root enumeration + observing every root) against the live desktop and
/// returns a human-readable summary. Used to validate a backend on real
/// hardware without wiring an MCP client. Read-only: it never performs an
/// action, so it cannot disturb the live session. Never panics — every failure
/// becomes a line in the summary.
pub fn smoke() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let roots = match backend::list_roots(&backend::RootFilters::default()) {
        Ok(roots) => roots,
        Err(error) => return format!("cu-smoke: list_roots failed: {error}\n"),
    };
    let _ = writeln!(out, "cu-smoke: {} root(s)", roots.len());
    for root in roots.iter().take(20) {
        let observed = backend::observe(
            root,
            backend::ObserveRequest {
                semantic: true,
                capture: backend::CapturePolicy::Never,
            },
        );
        let detail = match observed {
            Ok(observation) => format!(
                "{} node(s), text_sparse={}",
                observation.tree.node_count(),
                observation.text_sparse
            ),
            Err(error) => format!("observe failed: {error}"),
        };
        let _ = writeln!(
            out,
            "  [{}] {} pid={} kind={} frame={}x{} title={:?} -> {detail}",
            root.ref_id,
            root.app_name,
            root.pid,
            root.kind,
            root.frame.w as i64,
            root.frame.h as i64,
            root.title,
        );
    }
    let _ = writeln!(out, "cu-smoke: PASS");
    out
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
