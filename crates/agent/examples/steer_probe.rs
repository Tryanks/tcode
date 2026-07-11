//! Live steering probe (Group B): start a long-running Claude turn, then send a
//! second `SendTurn` *while it is still running* and show that the CLI accepts
//! the mid-turn user message as steering and changes the turn's outcome.
//!
//! ```text
//! cargo run -p agent --example steer_probe -- claude /tmp
//! ```
//!
//! Exits 0 when the steered turn completes and the steering marker shows up in
//! the assistant output; 1 otherwise.

use std::path::PathBuf;
use std::time::Duration;

use agent::{
    AgentEvent, ApprovalMode, InteractionMode, ProviderKind, SessionCommand, SessionOptions,
    TurnStatus, start_session,
};

/// The word the steering message asks for — it appears nowhere in the first
/// prompt, so seeing it proves the mid-turn message reached the model.
const MARKER: &str = "BANANA";

fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let provider = match args.next().as_deref() {
        Some("claude") => ProviderKind::ClaudeCode,
        Some("codex") => ProviderKind::Codex,
        other => {
            eprintln!("usage: steer_probe <claude|codex> [cwd]  (got {other:?})");
            std::process::exit(2);
        }
    };
    let cwd = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    let exit_code = smol::block_on(async move {
        let opts = SessionOptions {
            cwd,
            model: None,
            resume: None,
            binary_path: None,
            approval_mode: ApprovalMode::FullAccess,
            option_selections: Vec::new(),
            interaction_mode: InteractionMode::Build,
            mcp_server: None,
            launch_env: Default::default(),
            extra_args: Vec::new(),
            acp: None,
        };
        let handle = match start_session(provider, opts).await {
            Ok(handle) => handle,
            Err(err) => {
                eprintln!("failed to start session: {err}");
                return 1;
            }
        };

        // 1. A deliberately long turn: a real 60s shell loop, so the turn is
        //    still running when the steering message lands.
        let first = "Use the Bash tool to run exactly this command: \
                     for i in $(seq 1 60); do echo tick $i; sleep 1; done \
                     Then report the last tick you saw.";
        eprintln!("steer_probe: TURN 1 -> {first:?}");
        handle
            .commands
            .send(SessionCommand::SendTurn {
                text: first.into(),
                options: None,
                attachments: Vec::new(),
            })
            .await
            .expect("command channel closed");

        // 2. Once the turn is visibly running, steer it mid-flight.
        let steer_commands = handle.commands.clone();
        smol::spawn(async move {
            smol::Timer::after(Duration::from_secs(10)).await;
            let steer = format!(
                "Change of plan: stop the ticking task, do not wait for it, \
                 and reply with exactly one word: {MARKER}"
            );
            eprintln!("steer_probe: STEERING (mid-turn) -> {steer:?}");
            let _ = steer_commands
                .send(SessionCommand::SendTurn {
                    text: steer,
                    options: None,
                    attachments: Vec::new(),
                })
                .await;
        })
        .detach();

        let mut assistant = String::new();
        let mut turns_started = 0usize;
        while let Ok(event) = handle.events.recv().await {
            match &event {
                AgentEvent::TurnStarted { turn_id } => {
                    turns_started += 1;
                    eprintln!("steer_probe: TurnStarted {turn_id}");
                }
                AgentEvent::ItemCompleted(item) => {
                    if let agent::ItemContent::AssistantMessage { text } = &item.content {
                        eprintln!("steer_probe: assistant block: {:?}", text.trim());
                        assistant.push_str(text);
                        assistant.push('\n');
                    }
                }
                AgentEvent::Error { message, .. } => {
                    eprintln!("steer_probe: provider error: {message}");
                }
                AgentEvent::TurnCompleted { status, .. } => {
                    eprintln!("steer_probe: TurnCompleted status={status:?}");
                    let steered = assistant.to_uppercase().contains(MARKER);
                    println!("--- transcript ---\n{}", assistant.trim());
                    println!("--- steering marker {MARKER} present: {steered} ---");
                    println!("--- SendTurns issued: 2, TurnStarted seen: {turns_started} ---");
                    let _ = handle.commands.send(SessionCommand::Shutdown).await;
                    return match (status, steered) {
                        (TurnStatus::Completed, true) => 0,
                        (status, steered) => {
                            eprintln!("steer_probe: FAILED (status={status:?}, steered={steered})");
                            1
                        }
                    };
                }
                _ => {}
            }
        }
        eprintln!("steer_probe: stream closed before completion");
        1
    });
    std::process::exit(exit_code);
}
