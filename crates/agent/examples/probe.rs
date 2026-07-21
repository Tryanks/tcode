//! Headless end-to-end probe for provider clients.
//!
//! Turn mode:
//!   cargo run -p agent --example probe -- \
//!       <codex|claude|pi|opencode> "<prompt>" [cwd] [supervised|auto_edits|full_access] \
//!       [--mode plan] [--effort <value>]
//!
//! Catalog mode:
//!   cargo run -p agent --example probe -- --list-models <codex|claude|pi|opencode>
//!
//! Turn mode prints every canonical event as one JSON line and auto-approves any
//! approval request and auto-answers any user-input request with each question's
//! first option (only run against throwaway directories). `--mode plan` sends the
//! turn in Plan interaction mode; `--effort` sets a per-turn reasoning effort
//! (Codex/OpenCode) — Claude and pi resolve it as a session option.

use agent::{
    AgentEvent, ApprovalDecision, ApprovalMode, InteractionMode, OptionSelection, ProviderKind,
    SessionCommand, SessionOptions, TurnOptions, list_models, start_session,
};

fn parse_provider(arg: Option<&str>) -> ProviderKind {
    match arg {
        Some("codex") => ProviderKind::Codex,
        Some("claude") => ProviderKind::ClaudeCode,
        Some("pi") => ProviderKind::Pi,
        Some("opencode") => ProviderKind::OpenCode,
        _ => {
            eprintln!(
                "usage: probe <codex|claude|pi|opencode> <prompt> [cwd] [mode] [--mode plan] [--effort v]"
            );
            eprintln!("       probe --list-models <codex|claude|pi|opencode>");
            std::process::exit(2);
        }
    }
}

fn main() {
    env_logger::init();
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Catalog mode: `--list-models <provider>`.
    if args.first().map(String::as_str) == Some("--list-models") {
        let provider = parse_provider(args.get(1).map(String::as_str));
        let exit_code = smol::block_on(async move {
            match list_models(provider, None, Default::default()).await {
                Ok(models) => {
                    println!("{}", serde_json::to_string_pretty(&models).unwrap());
                    0
                }
                Err(err) => {
                    eprintln!("list_models failed: {err}");
                    1
                }
            }
        });
        std::process::exit(exit_code);
    }

    // Extract optional `--mode <m>` and `--effort <v>` flags from anywhere.
    let mut interaction_mode = InteractionMode::Build;
    let mut effort: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                let value = args.get(i + 1).cloned().unwrap_or_default();
                interaction_mode = match value.as_str() {
                    "plan" => InteractionMode::Plan,
                    "build" | "default" => InteractionMode::Build,
                    other => {
                        eprintln!("unknown --mode {other:?}; use plan|build");
                        std::process::exit(2);
                    }
                };
                i += 2;
            }
            "--effort" => {
                effort = args.get(i + 1).cloned();
                i += 2;
            }
            _ => {
                positional.push(std::mem::take(&mut args[i]));
                i += 1;
            }
        }
    }

    let mut pos = positional.into_iter();
    let provider = parse_provider(pos.next().as_deref());
    let prompt = pos.next().unwrap_or_else(|| {
        eprintln!("usage: probe <codex|claude|pi|opencode> <prompt> [cwd] [mode]");
        std::process::exit(2);
    });
    let cwd = pos
        .next()
        .map(Into::into)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let approval_mode = match pos.next().as_deref() {
        None | Some("supervised") => ApprovalMode::Supervised,
        Some("auto_edits") => ApprovalMode::AutoAcceptEdits,
        Some("full_access") => ApprovalMode::FullAccess,
        Some(other) => {
            eprintln!("unknown approval mode {other:?}; use supervised|auto_edits|full_access");
            std::process::exit(2);
        }
    };
    eprintln!(
        "probe: approval_mode={approval_mode:?} interaction_mode={interaction_mode:?} effort={effort:?}"
    );

    let exit_code = smol::block_on(async move {
        // Route `--effort` into the session's reasoningEffort selection too, so
        // the Claude spawn (launch-time only) reflects it; Codex also honors the
        // per-turn override below.
        let option_selections = match &effort {
            Some(value) => vec![OptionSelection {
                id: "reasoningEffort".into(),
                value: serde_json::Value::String(value.clone()),
            }],
            None => Vec::new(),
        };
        // Claude resolves effort against a known model's descriptor, so pick a
        // concrete model when demonstrating `--effort`.
        let model = match (provider, effort.is_some()) {
            (ProviderKind::ClaudeCode, true) => Some("claude-opus-4-8".to_string()),
            _ => None,
        };
        let opts = SessionOptions {
            cwd,
            model,
            resume: None,
            fork: false,
            binary_path: None,
            approval_mode,
            option_selections,
            interaction_mode,
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
        let turn_options = TurnOptions {
            effort,
            interaction_mode: Some(interaction_mode),
        };
        handle
            .commands
            .send(SessionCommand::SendTurn {
                delivery_id: 0,
                text: prompt,
                options: Some(turn_options),
                attachments: Vec::new(),
            })
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
                AgentEvent::UserInputRequested {
                    request_id,
                    questions,
                } => {
                    // Auto-answer with each question's first option (or empty
                    // string for a free-text-only question), printing what we saw.
                    let mut answers = serde_json::Map::new();
                    for q in questions {
                        let answer = q
                            .options
                            .first()
                            .map(|o| o.label.clone())
                            .unwrap_or_default();
                        eprintln!(
                            "probe: user-input {:?} header={:?} options={:?} -> answering {:?}",
                            q.question,
                            q.header,
                            q.options.iter().map(|o| &o.label).collect::<Vec<_>>(),
                            answer
                        );
                        answers.insert(q.id.clone(), serde_json::Value::String(answer));
                    }
                    handle
                        .commands
                        .send(SessionCommand::RespondUserInput {
                            request_id: request_id.clone(),
                            answers,
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
