//! Agent Client Protocol provider: any agent from the ACP registry.
//!
//! Codex and Claude Code keep their native clients (they expose steering,
//! structured questions and richer tool payloads that ACP cannot carry); this
//! module covers the rest of the ecosystem through one protocol.

use crate::{AgentError, SessionHandle, SessionOptions};

pub async fn start(_opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    Err(AgentError::Protocol("ACP provider not implemented yet".into()))
}
