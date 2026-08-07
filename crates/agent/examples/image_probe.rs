//! Live image-attachment probe: send a real image file to a provider alongside a
//! prompt and print the assistant's reply.
//!
//! Exercises the full attachment wire path (`SessionCommand::SendTurn.attachments`
//! → Claude `image`/`base64` content blocks, Codex `image` data-URL input entries).
//!
//! ```text
//! cargo run -p agent --example image_probe -- \
//!     claude /tmp/tcode-blue.png "What color is this image? Reply with just the color." /tmp
//! ```
//!
//! Exits 0 when the turn completes, 1 otherwise. The assistant's text is printed
//! to stdout prefixed with `ASSISTANT:`.

use std::path::PathBuf;

use agent::{
    AgentEvent, ApprovalDecision, ApprovalMode, Attachment, InteractionMode, ProviderKind,
    SessionCommand, SessionOptions, TurnStatus, start_session,
};
use base64::Engine as _;

fn mime_from_path(path: &std::path::Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        other => {
            eprintln!("image_probe: unknown extension {other:?}; defaulting to image/png");
            "image/png"
        }
    }
    .to_string()
}

fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let provider = match args.next().as_deref() {
        Some("claude") => ProviderKind::ClaudeCode,
        Some("codex") => ProviderKind::Codex,
        Some("pi") => ProviderKind::Pi,
        Some("opencode") => ProviderKind::OpenCode,
        other => {
            eprintln!(
                "usage: image_probe <claude|codex|pi|opencode> <image-path> [prompt] [cwd]  (got {other:?})"
            );
            std::process::exit(2);
        }
    };
    let image_path = match args.next() {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: image_probe <claude|codex|pi|opencode> <image-path> [prompt] [cwd]");
            std::process::exit(2);
        }
    };
    let prompt = args
        .next()
        .unwrap_or_else(|| "What color is this image? Reply with just the color.".to_string());
    let cwd = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    let bytes = match std::fs::read(&image_path) {
        Ok(b) => b,
        Err(err) => {
            eprintln!("failed to read {}: {err}", image_path.display());
            std::process::exit(2);
        }
    };
    let attachment = Attachment {
        media_type: mime_from_path(&image_path),
        data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        source_path: None,
    };
    eprintln!(
        "image_probe: provider={provider:?} image={} ({} bytes, {}) prompt={prompt:?}",
        image_path.display(),
        bytes.len(),
        attachment.media_type,
    );

    let exit_code = smol::block_on(async move {
        let opts = SessionOptions {
            cwd,
            model: None,
            resume: None,
            fork: false,
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
        handle
            .commands
            .send(SessionCommand::SendTurn {
                delivery_id: 0,
                text: prompt,
                options: None,
                attachments: vec![attachment],
            })
            .await
            .expect("session command channel closed before first turn");

        let mut reply = String::new();
        while let Ok(event) = handle.events.recv().await {
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
                AgentEvent::ItemCompleted(item) => {
                    if let agent::ItemContent::AssistantMessage { text } = &item.content {
                        reply.push_str(text);
                    }
                }
                AgentEvent::ProviderCommands { commands } => {
                    eprintln!(
                        "image_probe: provider reported {} command(s)",
                        commands.len()
                    );
                }
                AgentEvent::Error { message, fatal } => {
                    eprintln!("image_probe: provider error (fatal={fatal}): {message}");
                }
                AgentEvent::TurnCompleted { status, .. } => {
                    println!("ASSISTANT: {}", reply.trim());
                    let _ = handle.commands.send(SessionCommand::Shutdown).await;
                    return match status {
                        TurnStatus::Completed => 0,
                        _ => {
                            eprintln!("image_probe: turn ended with {status:?}");
                            1
                        }
                    };
                }
                _ => {}
            }
        }
        eprintln!("image_probe: event stream closed before the turn completed");
        1
    });
    std::process::exit(exit_code);
}
