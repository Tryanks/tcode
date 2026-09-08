//! In-process `tcode_computer_use` MCP server: pi-computer-use-style desktop
//! automation for every provider (accessibility-tree observation, state-scoped
//! refs, transactional actions). See `docs/computer-use.md` for the design.
//!
//! Served over the shared loopback streamable-HTTP host with a distinct bearer
//! token. macOS uses AX/CGEvent and Windows adapts the `uiautomation` crate;
//! other platforms report that computer use is unsupported.

pub mod backend;
pub mod config;
mod feedback;
pub mod outline;
pub mod permissions;
pub mod state;
pub mod tools;

/// Return the frontmost application pid on macOS, or `None` elsewhere.
pub use backend::frontmost_pid;

/// A running computer-use MCP server and the bearer token required to access it.
pub struct ComputerUseMcpServer {
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/computer-use`.
    pub url: String,
    /// Per-session tokens and feedback cancellation controls.
    pub tokens: TokenRegistry,
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
    let sessions = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let feedback = sessions.clone();
    let services = mcp_host::TokenRegistry::new(move |scope| {
        let mut sessions = feedback.lock().unwrap();
        let session = sessions
            .get(&scope)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(feedback::FeedbackSession::new);
        sessions.insert(scope, std::sync::Arc::downgrade(&session));
        tools::service(session)
    });
    let tokens = TokenRegistry { services, sessions };
    host.mount(mcp_host::route("/computer-use", &tokens.services));

    log::info!("computer-use-mcp: serving at {url}");
    ComputerUseMcpServer { url, tokens }
}

/// Uses the shared authenticated registry; weak feedback controls do not keep a
/// mounted service alive after its actual route and handlers have been dropped.
#[derive(Clone)]
pub struct TokenRegistry {
    services: mcp_host::TokenRegistry<tools::Service>,
    sessions: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, std::sync::Weak<feedback::FeedbackSession>>,
        >,
    >,
}

impl TokenRegistry {
    pub fn register(&self, session_id: &str) -> String {
        self.services.register(session_id)
    }
    pub fn cancel(&self, session_id: &str) {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .and_then(std::sync::Weak::upgrade)
        {
            session.cancel();
        }
    }
    pub fn revoke(&self, session_id: &str, token: &str) {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap()
            .remove(session_id)
            .and_then(|session| session.upgrade())
        {
            session.stop();
        }
        self.services.revoke(token);
    }
}

#[cfg(test)]
mod feedback_lifecycle_tests {
    use super::*;

    #[test]
    fn mounted_services_own_feedback_and_registrations_cancel_independently() {
        let mut host = mcp_host::Host::bind().unwrap();
        let server = start(&mut host);
        let first = server.tokens.register("first");
        let second = server.tokens.register("second");
        assert!(
            first != second,
            "sessions need distinct authorization scopes"
        );
        let session = |id: &str| {
            server.tokens.sessions.lock().unwrap()[id]
                .upgrade()
                .unwrap()
        };
        let first_run = session("first").begin(None);
        let second_run = session("second").begin(None);
        server.tokens.cancel("first");
        assert!(!first_run.is_current());
        assert!(second_run.is_current());
        let retained_handler = session("first");
        let next_turn = retained_handler.begin(None);
        assert!(next_turn.is_current(), "Stop does not disable future turns");
        server.tokens.revoke("first", &first);
        assert!(!next_turn.is_current());
        assert!(
            !retained_handler.begin(None).is_current(),
            "a retained handler cannot restart feedback after revocation"
        );
        assert!(second_run.is_current());
        drop(server);
        assert!(
            second_run.is_current(),
            "registration metadata does not own mounted service lifetime"
        );
        drop(host);
        assert!(
            !second_run.is_current(),
            "last mounted service drop invalidates delayed publications"
        );
    }

    #[test]
    fn mcp_cancellation_invalidates_pending_feedback_before_the_handler_resumes() {
        let session = feedback::FeedbackSession::new();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let mut success = session.begin(None);
        let published_success = success.ticket();
        success.complete();
        drop(success);
        assert!(
            published_success.is_current(),
            "successful actions retain their bounded tail"
        );
        let cancelled = session.begin(Some(cancellation.clone()));
        let pending = cancelled.ticket();
        cancellation.cancel();
        assert!(!pending.is_current());
        assert!(
            published_success.is_current(),
            "request cancellation does not invalidate another request"
        );
    }
}
