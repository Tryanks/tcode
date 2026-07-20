//! Live steering probe: start a long-running turn, then send `SessionCommand::Steer`
//! *while it is still running*, and show that the provider injects the message
//! into the turn that is already in flight.
//!
//! ```text
//! cargo run -p agent --example steer_probe -- claude /tmp
//! cargo run -p agent --example steer_probe -- codex  /tmp
//! cargo run -p agent --example steer_probe -- pi     /tmp
//! ```
//!
//! Two things are proven at once:
//!   1. STEERING WORKS — the marker word (which appears nowhere in the first
//!      prompt) shows up in the assistant output, so the mid-turn message
//!      really did reach the model.
//!   2. TURN ACCOUNTING IS INTACT — exactly one `TurnStarted` and one
//!      `TurnCompleted` are observed. A steered message must NOT manufacture a
//!      phantom turn. After the first `TurnCompleted` we keep draining for a
//!      few seconds specifically to catch a late second one.
//!   3. ACCEPTANCE IS CONSUMPTION-TIED — `SteerAccepted` for the request arrives
//!      after the steering write and before the turn completes.
//!
//! Exits 0 only when all three hold; 1 otherwise.

use std::path::PathBuf;
use std::time::Duration;

use agent::{
    AgentEvent, ApprovalMode, InteractionMode, ProviderKind, SessionCommand, SessionOptions,
    TurnStatus, start_session,
};

/// The word the steering message asks for — it appears nowhere in the first
/// prompt, so seeing it proves the mid-turn message reached the model.
const MARKER: &str = "BANANA";

/// How long to wait after the first `TurnCompleted` for a (bug-indicating)
/// second one before declaring the accounting clean.
const PHANTOM_TURN_GRACE: Duration = Duration::from_secs(5);

fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let provider = match args.next().as_deref() {
        Some("claude") => ProviderKind::ClaudeCode,
        Some("codex") => ProviderKind::Codex,
        Some("pi") => ProviderKind::Pi,
        other => {
            eprintln!("usage: steer_probe <claude|codex|pi> [cwd]  (got {other:?})");
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
            orchestrate_server: None,
            computer_use_server: None,
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

        // 1. A deliberately long turn: count to 60 with a real shell command
        //    between each number, so the turn is unmistakably still running (and
        //    hitting tool calls, the natural steering checkpoints) when the
        //    steering message lands.
        let first = "Count from 1 to 60 slowly, one number per line. \
                     Use the shell to run `sleep 1` between each number. \
                     Then report the last number you reached.";
        eprintln!("steer_probe: TURN 1 -> {first:?}");
        handle
            .commands
            .send(SessionCommand::SendTurn {
                delivery_id: 0,
                text: first.into(),
                options: None,
                attachments: Vec::new(),
            })
            .await
            .expect("command channel closed");

        // 2. Once the turn is visibly running, steer it mid-flight. This is a
        //    `Steer`, NOT a second `SendTurn`: the provider must inject it into
        //    the running turn rather than open a new one.
        let steer_commands = handle.commands.clone();
        smol::spawn(async move {
            smol::Timer::after(Duration::from_secs(10)).await;
            let steer = format!("STOP counting and reply with exactly: {MARKER}");
            eprintln!("steer_probe: STEERING (mid-turn) -> {steer:?}");
            let _ = steer_commands
                .send(SessionCommand::Steer {
                    request_id: "steer-probe-1".into(),
                    text: steer,
                    attachments: Vec::new(),
                })
                .await;
        })
        .detach();

        let mut assistant = String::new();
        let mut turns_started = 0usize;
        let mut turns_completed = 0usize;
        let mut first_status = None;
        let mut steer_accepted_before_completion = false;

        loop {
            // After the first completion, only wait out the grace window: any
            // event arriving in it is what we are hunting for (a phantom turn).
            let event = if turns_completed == 0 {
                handle.events.recv().await.ok()
            } else {
                let recv = handle.events.recv();
                let timeout = async {
                    smol::Timer::after(PHANTOM_TURN_GRACE).await;
                    Err(async_channel::RecvError)
                };
                smol::future::or(recv, timeout).await.ok()
            };
            let Some(event) = event else { break };

            match &event {
                AgentEvent::TurnStarted { turn_id } => {
                    turns_started += 1;
                    eprintln!("steer_probe: TurnStarted {turn_id} (#{turns_started})");
                }
                AgentEvent::ItemCompleted(item) => {
                    if let agent::ItemContent::AssistantMessage { text } = &item.content {
                        eprintln!("steer_probe: assistant block: {:?}", text.trim());
                        assistant.push_str(text);
                        assistant.push('\n');
                    }
                }
                AgentEvent::Warning { message } => {
                    eprintln!("steer_probe: WARNING: {message}");
                }
                AgentEvent::Error { message, .. } => {
                    eprintln!("steer_probe: provider error: {message}");
                }
                AgentEvent::SteerAccepted { request_id } if request_id == "steer-probe-1" => {
                    if turns_completed == 0 {
                        steer_accepted_before_completion = true;
                        eprintln!(
                            "steer_probe: SteerAccepted {request_id} (after write, before first TurnCompleted)"
                        );
                    } else {
                        eprintln!(
                            "steer_probe: SteerAccepted {request_id} (after TurnCompleted #{turns_completed})"
                        );
                    }
                }
                AgentEvent::TurnCompleted {
                    status, turn_id, ..
                } => {
                    turns_completed += 1;
                    eprintln!(
                        "steer_probe: TurnCompleted {turn_id} status={status:?} (#{turns_completed})"
                    );
                    first_status.get_or_insert(*status);
                }
                _ => {}
            }
        }

        let steered = assistant.to_uppercase().contains(MARKER);
        let clean_accounting = turns_started == 1 && turns_completed == 1;
        println!("--- transcript ---\n{}", assistant.trim());
        println!("--- steering marker {MARKER} present: {steered} ---");
        println!(
            "--- steer acceptance before completion observed: \
             {steer_accepted_before_completion} ---"
        );
        println!(
            "--- turn accounting: TurnStarted={turns_started} TurnCompleted={turns_completed} \
             (both must be 1; a steer must not create a phantom turn) ---"
        );

        let _ = handle.commands.send(SessionCommand::Shutdown).await;

        match (
            first_status,
            steered,
            clean_accounting,
            steer_accepted_before_completion,
        ) {
            (Some(TurnStatus::Completed), true, true, true) => 0,
            (status, steered, clean, accepted) => {
                eprintln!(
                    "steer_probe: FAILED (status={status:?}, steered={steered}, \
                     clean_accounting={clean}, steer_accepted_before_completion={accepted})"
                );
                1
            }
        }
    });
    std::process::exit(exit_code);
}
