//! Cursor: `cursor-agent [flags] acp`, an ACP agent with Cursor's `cursor/*`
//! dialect.
//!
//! The protocol machinery is [`crate::acp_session`]; this module is Cursor's
//! dialect.

use std::path::PathBuf;

use agent_client_protocol::schema::v1 as acp;
use serde_json::json;

use crate::acp_session::{
    self, Dialect, Established, Launch, Session, Setup, State, TurnEnd, describe, is_auth_required,
    prompt_status,
};
use crate::{
    AgentError, AgentEvent, ApprovalMode, Attachment, LaunchEnv, ModelSpec, ProviderKind,
    SessionHandle, SessionOptions,
};

/// Start (or resume) a Cursor session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    acp_session::start(ProviderKind::Cursor, Cursor, opts).await
}

/// No catalog: Cursor sessions are not available yet ([`start`]).
pub async fn list_models(
    _binary_path: Option<PathBuf>,
    _launch_env: LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    Ok(Vec::new())
}

struct Cursor;

impl Dialect for Cursor {
    fn name(&self) -> &str {
        "Cursor"
    }

    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError> {
        // Not the generic `agent` name Cursor also installs: that name is too
        // common to resolve from PATH with confidence.
        let program = crate::resolve_binary(opts.binary_path.as_deref(), "cursor-agent")?;
        let mut args = opts.extra_args.clone();
        args.push("acp".to_string());
        let client_meta = json!({ "parameterizedModelPicker": true, "subagents": true });
        Ok(Launch {
            program,
            args,
            env: Vec::new(),
            client_meta: client_meta.as_object().cloned(),
        })
    }

    async fn establish(&self, setup: &Setup<'_>) -> Result<Established, AgentError> {
        let opts = setup.opts;
        if opts.fork {
            return Err(AgentError::Protocol(
                "session fork is not supported for this provider".into(),
            ));
        }
        let mcp_servers = setup.mcp_servers().await;
        let resumed = opts
            .resume
            .as_ref()
            .and_then(|cursor| cursor.str_field(&["session_id"]));
        // `authenticate` opens a browser when Cursor is signed out, so it is
        // never driven from here: a missing login is reported instead.
        let established = match resumed {
            Some(session_id) => setup
                .load(
                    acp::LoadSessionRequest::new(session_id.to_string(), opts.cwd.clone())
                        .mcp_servers(mcp_servers),
                )
                .await
                .map(|_| ()),
            None => setup
                .connection
                .send_request(
                    acp::NewSessionRequest::new(opts.cwd.clone()).mcp_servers(mcp_servers),
                )
                .block_task()
                .await
                .map(|_| ()),
        };
        match established {
            Err(err) if is_auth_required(&err) => Err(AgentError::Provider(format!(
                "Cursor is not signed in: {}. Run `cursor-agent login`, then start the session again.",
                describe(&err)
            ))),
            Err(err) => Err(AgentError::Protocol(format!(
                "Cursor could not start a session: {}",
                describe(&err)
            ))),
            Ok(()) => Err(AgentError::Provider(
                "Cursor sessions are not available yet in this build of Tcode.".into(),
            )),
        }
    }

    fn session_update(
        &self,
        state: &mut State,
        notification: acp::SessionNotification,
    ) -> Vec<AgentEvent> {
        // `session/load` replays the whole conversation before it answers;
        // tcode's own log is the conversation of record.
        if state.loading() {
            return Vec::new();
        }
        state.apply_update(notification.update)
    }

    fn turn_end(
        &self,
        _state: &mut State,
        result: Result<acp::PromptResponse, acp::Error>,
    ) -> TurnEnd {
        let (status, message) = prompt_status(&result);
        // Cursor reports no usage over ACP.
        TurnEnd {
            status,
            message,
            usage: None,
        }
    }

    async fn steer(
        &self,
        _session: &Session,
        _request_id: String,
        _text: String,
        _attachments: Vec<Attachment>,
    ) {
    }

    async fn set_approval_mode(&self, _session: &Session, _mode: ApprovalMode) {}
}
