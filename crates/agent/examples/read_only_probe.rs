//! Live probe for dispatch-only `ApprovalMode::ReadOnly`.
//!
//! ```text
//! cargo run -p agent --example read_only_probe -- codex
//! cargo run -p agent --example read_only_probe -- claude
//! ```

use std::time::Duration;

use agent::{
    AgentEvent, ApprovalMode, InteractionMode, ItemContent, ProviderKind, SessionCommand,
    SessionOptions, start_session,
};

const EVENT_TIMEOUT: Duration = Duration::from_secs(180);

fn main() {
    env_logger::init();
    let provider = match std::env::args().nth(1).as_deref() {
        Some("codex") => ProviderKind::Codex,
        Some("claude") => ProviderKind::ClaudeCode,
        other => {
            eprintln!("usage: read_only_probe <codex|claude> (got {other:?})");
            std::process::exit(2);
        }
    };

    let exit = smol::block_on(run(provider));
    std::process::exit(exit);
}

async fn run(provider: ProviderKind) -> i32 {
    let cwd = std::env::temp_dir().join(format!(
        "tcode-read-only-probe-{}-{}",
        provider_name(provider),
        std::process::id()
    ));
    if let Err(err) = std::fs::create_dir_all(&cwd) {
        eprintln!("probe: could not create {}: {err}", cwd.display());
        return 1;
    }
    let marker = cwd.join("must-not-exist.txt");
    let readable = cwd.join("read-me.txt");
    let _ = std::fs::remove_file(&marker);
    if let Err(err) = std::fs::write(&readable, "READ_ONLY_PROBE_OK\nsecond line\n") {
        eprintln!("probe: could not create {}: {err}", readable.display());
        let _ = std::fs::remove_dir_all(&cwd);
        return 1;
    }

    let opts = SessionOptions {
        cwd: cwd.clone(),
        model: None,
        resume: None,
        binary_path: None,
        approval_mode: ApprovalMode::ReadOnly,
        option_selections: Vec::new(),
        interaction_mode: InteractionMode::Build,
        mcp_server: None,
        orchestrate_server: None,
        launch_env: Default::default(),
        extra_args: Vec::new(),
        acp: None,
    };
    let handle = match start_session(provider, opts).await {
        Ok(handle) => handle,
        Err(err) => {
            eprintln!("probe: failed to start {provider:?}: {err}");
            let _ = std::fs::remove_dir_all(&cwd);
            return 1;
        }
    };

    eprintln!("probe: READ turn -> read {}", readable.display());
    handle
        .commands
        .send(SessionCommand::SendTurn {
            text: format!(
                "Read the file `{}` using a native file-read tool if one is available, then report its first line. Do not use a shell command and do not modify anything.",
                readable.display()
            ),
            options: None,
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    let read = observe_turn(&handle.events, "READ").await;

    eprintln!("probe: WRITE turn -> create {}", marker.display());
    handle
        .commands
        .send(SessionCommand::SendTurn {
            text: format!(
                "Create the file `{}` containing exactly `blocked`. Use a shell command or file-writing tool now; do not merely describe how.",
                marker.display()
            ),
            options: None,
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    let write = observe_turn_until_approval_or_completion(&handle.events, "WRITE").await;

    if write.approvals > 0 {
        eprintln!("probe: mutation approval surfaced; cancelling without approval");
        let _ = handle.commands.send(SessionCommand::Interrupt).await;
    }
    let _ = handle.commands.send(SessionCommand::Shutdown).await;

    let exists = marker.exists();
    eprintln!(
        "probe: SUMMARY provider={} read_completed={} read_approvals={} write_completed={} write_approvals={} file_exists={exists}",
        provider_name(provider),
        read.completed,
        read.approvals,
        write.completed,
        write.approvals,
    );
    let ok = read.completed
        && read.approvals == 0
        && match provider {
            ProviderKind::Codex | ProviderKind::ClaudeCode => write.approvals > 0 && !exists,
            ProviderKind::Acp => unreachable!(),
        };
    let _ = std::fs::remove_dir_all(&cwd);
    i32::from(!ok)
}

#[derive(Default)]
struct Observation {
    completed: bool,
    approvals: usize,
}

async fn observe_turn(events: &async_channel::Receiver<AgentEvent>, label: &str) -> Observation {
    let mut observation = Observation::default();
    loop {
        let Some(event) = recv_timeout(events).await else {
            eprintln!("probe: {label} TIMEOUT");
            break;
        };
        print_event(label, &event);
        match event {
            AgentEvent::ApprovalRequested(_) => observation.approvals += 1,
            AgentEvent::TurnCompleted { .. } => {
                observation.completed = true;
                break;
            }
            AgentEvent::Error { fatal: true, .. } | AgentEvent::SessionClosed { .. } => break,
            _ => {}
        }
    }
    observation
}

async fn observe_turn_until_approval_or_completion(
    events: &async_channel::Receiver<AgentEvent>,
    label: &str,
) -> Observation {
    let mut observation = Observation::default();
    loop {
        let Some(event) = recv_timeout(events).await else {
            eprintln!("probe: {label} TIMEOUT");
            break;
        };
        print_event(label, &event);
        match event {
            AgentEvent::ApprovalRequested(_) => {
                observation.approvals += 1;
                break;
            }
            AgentEvent::TurnCompleted { .. } => {
                observation.completed = true;
                break;
            }
            AgentEvent::Error { fatal: true, .. } | AgentEvent::SessionClosed { .. } => break,
            _ => {}
        }
    }
    observation
}

async fn recv_timeout(events: &async_channel::Receiver<AgentEvent>) -> Option<AgentEvent> {
    smol::future::or(async { events.recv().await.ok() }, async {
        smol::Timer::after(EVENT_TIMEOUT).await;
        None
    })
    .await
}

fn print_event(label: &str, event: &AgentEvent) {
    match event {
        AgentEvent::TurnStarted { turn_id } => {
            eprintln!("probe: {label} TurnStarted id={turn_id}")
        }
        AgentEvent::ItemCompleted(item) => match &item.content {
            ItemContent::CommandExecution {
                command,
                output,
                exit_code,
                ..
            } => eprintln!(
                "probe: {label} CommandCompleted command={command:?} exit={exit_code:?} output={:?}",
                output.trim()
            ),
            ItemContent::AssistantMessage { text } => {
                eprintln!("probe: {label} Assistant {:?}", text.trim())
            }
            _ => {}
        },
        AgentEvent::ApprovalRequested(request) => eprintln!(
            "probe: {label} ApprovalRequested id={} kind={:?}",
            request.id, request.kind
        ),
        AgentEvent::TurnCompleted { status, .. } => {
            eprintln!("probe: {label} TurnCompleted status={status:?}")
        }
        AgentEvent::Warning(message) => eprintln!("probe: {label} Warning {message:?}"),
        AgentEvent::Error { fatal, message } => {
            eprintln!("probe: {label} Error fatal={fatal} {message:?}")
        }
        AgentEvent::SessionClosed { reason } => {
            eprintln!("probe: {label} SessionClosed {reason:?}")
        }
        _ => {}
    }
}

fn provider_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "codex",
        ProviderKind::ClaudeCode => "claude",
        ProviderKind::Acp => "acp",
    }
}
