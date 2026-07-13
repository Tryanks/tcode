//! Live ACP probe: launch a real ACP agent, run one turn, print the canonical
//! event trace.
//!
//! ```text
//! cargo run -p agent --example acp_probe -- <cwd> "<prompt>" <command> [args…]
//! # e.g. a registry npx recipe:
//! cargo run -p agent --example acp_probe -- /tmp/x "list files" \
//!     npx --yes @agentclientprotocol/claude-agent-acp@0.58.1
//! ```
//! Approvals are answered with the agent's own first `allow_once` option (via
//! `ApprovalDecision::Approve`), so the permission path is exercised end to end.

use std::path::PathBuf;

use agent::{
    AcpAgent, AcpLaunch, AgentEvent, ApprovalDecision, ApprovalMode, InteractionMode, LaunchEnv,
    ProviderKind, SessionCommand, SessionOptions, TurnStatus, start_session,
};

fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let cwd = PathBuf::from(
        args.next()
            .expect("usage: acp_probe <cwd> <prompt> <command> [args…]"),
    );
    let prompt = args.next().expect("missing prompt");
    let command = args.next().expect("missing command");
    let launch_args: Vec<String> = args.collect();

    smol::block_on(async move {
        let opts = SessionOptions {
            cwd,
            model: None,
            resume: None,
            binary_path: None,
            approval_mode: ApprovalMode::Supervised,
            option_selections: Vec::new(),
            interaction_mode: InteractionMode::Build,
            mcp_server: None,
            orchestrate_server: None,
            launch_env: LaunchEnv::default(),
            extra_args: Vec::new(),
            acp: Some(AcpAgent {
                id: "probe".into(),
                name: command.clone(),
                launch: AcpLaunch::Custom {
                    command,
                    args: launch_args,
                    env: Vec::new(),
                },
            }),
        };
        let session = match start_session(ProviderKind::Acp, opts).await {
            Ok(session) => session,
            Err(err) => {
                println!("START FAILED: {err}");
                std::process::exit(1);
            }
        };
        session
            .commands
            .send(SessionCommand::SendTurn {
                text: prompt,
                options: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();

        while let Ok(event) = session.events.recv().await {
            match &event {
                AgentEvent::Delta { kind, text, .. } => println!("DELTA {kind:?}: {text:?}"),
                AgentEvent::ApprovalRequested(request) => {
                    println!("APPROVAL {:?} options={:?}", request.kind, request.options);
                    session
                        .commands
                        .send(SessionCommand::RespondApproval {
                            request_id: request.id.clone(),
                            decision: ApprovalDecision::Approve,
                        })
                        .await
                        .unwrap();
                }
                AgentEvent::TurnCompleted { status, usage, .. } => {
                    println!("TURN {status:?} usage={usage:?}");
                    let _ = session.commands.send(SessionCommand::Shutdown).await;
                    if !matches!(status, TurnStatus::Completed) {
                        std::process::exit(1);
                    }
                }
                other => println!("{other:?}"),
            }
        }
        println!("session closed");
    });
}
