//! Live interrupt probe: starts a session, sends a long-running turn, then
//! interrupts it after a delay and expects `TurnCompleted { status: Interrupted }`.
//!
//! Usage: cargo run -p agent --example interrupt_probe -- <codex|claude> [cwd]

use std::time::Duration;

use agent::{AgentEvent, ProviderKind, SessionCommand, SessionOptions, TurnStatus, start_session};

fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let provider = match args.next().as_deref() {
        Some("codex") => ProviderKind::Codex,
        Some("claude") => ProviderKind::ClaudeCode,
        _ => {
            eprintln!("usage: interrupt_probe <codex|claude> [cwd]");
            std::process::exit(2);
        }
    };
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
            approval_mode: Default::default(),
            option_selections: Vec::new(),
            interaction_mode: Default::default(),
            mcp_server: None,
            launch_env: Default::default(),
            extra_args: Vec::new(),
        };
        let handle = start_session(provider, opts).await.expect("start session");
        handle
            .commands
            .send(SessionCommand::SendTurn {
                text: "Count from 1 to 500 slowly, one number per line, \
                       thinking carefully about each number."
                    .into(),
                options: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();

        // Give the turn time to actually start producing output, then interrupt.
        let commands = handle.commands.clone();
        smol::spawn(async move {
            smol::Timer::after(Duration::from_secs(5)).await;
            eprintln!("--- sending Interrupt ---");
            commands.send(SessionCommand::Interrupt).await.ok();
            smol::Timer::after(Duration::from_secs(30)).await;
            eprintln!("--- interrupt timed out, forcing shutdown ---");
            commands.send(SessionCommand::Shutdown).await.ok();
        })
        .detach();

        let mut status: Option<TurnStatus> = None;
        while let Ok(event) = handle.events.recv().await {
            match &event {
                AgentEvent::Delta { .. } => {} // too chatty to print
                other => println!("{}", serde_json::to_string(other).unwrap()),
            }
            match event {
                AgentEvent::TurnCompleted { status: s, .. } => {
                    status = Some(s);
                    handle.commands.send(SessionCommand::Shutdown).await.ok();
                }
                AgentEvent::SessionClosed { .. } => break,
                _ => {}
            }
        }
        match status {
            Some(TurnStatus::Interrupted) => {
                eprintln!("OK: turn was interrupted");
                0
            }
            other => {
                eprintln!("FAIL: expected Interrupted, got {other:?}");
                1
            }
        }
    });
    std::process::exit(exit_code);
}
