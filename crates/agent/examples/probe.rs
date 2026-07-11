//! Headless end-to-end probe for provider clients.
//!
//! Usage: cargo run -p agent --example probe -- <codex|claude> "<prompt>" [cwd]
//!
//! Prints every canonical event as one JSON line. Auto-approves any approval
//! request (only run this against throwaway directories).

use agent::{
    AgentEvent, ApprovalDecision, ProviderKind, SessionCommand, SessionOptions, start_session,
};

fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let provider = match args.next().as_deref() {
        Some("codex") => ProviderKind::Codex,
        Some("claude") => ProviderKind::ClaudeCode,
        _ => {
            eprintln!("usage: probe <codex|claude> <prompt> [cwd]");
            std::process::exit(2);
        }
    };
    let prompt = args.next().unwrap_or_else(|| {
        eprintln!("usage: probe <codex|claude> <prompt> [cwd]");
        std::process::exit(2);
    });
    let cwd = args
        .next()
        .map(Into::into)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    let exit_code = smol::block_on(async move {
        let opts = SessionOptions {
            cwd,
            model: None,
            resume: None,
            binary_path: None,
        };
        let handle = match start_session(provider, opts).await {
            Ok(handle) => handle,
            Err(err) => {
                eprintln!("failed to start session: {err}");
                return 1;
            }
        };
        handle
            .commands
            .send(SessionCommand::SendTurn { text: prompt })
            .await
            .expect("session command channel closed before first turn");

        let mut saw_completed_turn = false;
        while let Ok(event) = handle.events.recv().await {
            println!("{}", serde_json::to_string(&event).unwrap());
            match &event {
                AgentEvent::ApprovalRequested(req) => {
                    handle
                        .commands
                        .send(SessionCommand::RespondApproval {
                            request_id: req.id.clone(),
                            decision: ApprovalDecision::Approve,
                        })
                        .await
                        .ok();
                }
                AgentEvent::TurnCompleted { .. } => {
                    saw_completed_turn = true;
                    handle.commands.send(SessionCommand::Shutdown).await.ok();
                }
                AgentEvent::Error { fatal: true, .. } => {
                    handle.commands.send(SessionCommand::Shutdown).await.ok();
                }
                AgentEvent::SessionClosed { .. } => break,
                _ => {}
            }
        }
        if saw_completed_turn { 0 } else { 1 }
    });
    std::process::exit(exit_code);
}
