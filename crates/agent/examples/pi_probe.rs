//! Native pi RPC smoke probe.
//!
//! ```text
//! cargo run -p agent --example pi_probe -- "Reply with exactly PONG" /tmp /opt/homebrew/bin/pi
//! ```

use std::path::PathBuf;
use std::time::Duration;

use agent::{
    AgentEvent, ApprovalDecision, ApprovalMode, ProviderKind, SessionCommand, SessionOptions,
    list_models, start_session,
};

fn main() {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let prompt = args
        .next()
        .unwrap_or_else(|| "Reply with exactly PONG and nothing else.".into());
    let cwd = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("current directory"));
    let binary_path = args.next().map(PathBuf::from);
    let exit = smol::block_on(run(prompt, cwd, binary_path));
    std::process::exit(exit);
}

async fn run(prompt: String, cwd: PathBuf, binary_path: Option<PathBuf>) -> i32 {
    let model = match list_models(ProviderKind::Pi, binary_path.clone(), Default::default()).await {
        Ok(models) => {
            let default = models
                .iter()
                .find(|model| model.is_default)
                .or_else(|| models.first())
                .map(|model| model.id.clone());
            eprintln!(
                "pi_probe: model catalog count={} selected={default:?}",
                models.len()
            );
            default
        }
        Err(error) => {
            eprintln!("pi_probe: model discovery failed: {error}");
            None
        }
    };

    let handle = match start_session(
        ProviderKind::Pi,
        SessionOptions {
            cwd,
            model,
            resume: None,
            binary_path,
            approval_mode: ApprovalMode::Supervised,
            option_selections: Vec::new(),
            interaction_mode: Default::default(),
            mcp_server: None,
            orchestrate_server: None,
            computer_use_server: None,
            launch_env: Default::default(),
            extra_args: Vec::new(),
            acp: None,
        },
    )
    .await
    {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("pi_probe: startup failed: {error}");
            return 1;
        }
    };
    if handle
        .commands
        .send(SessionCommand::SendTurn {
            delivery_id: 0,
            text: prompt,
            options: None,
            attachments: Vec::new(),
        })
        .await
        .is_err()
    {
        eprintln!("pi_probe: command channel closed before prompt");
        return 1;
    }

    let mut started = false;
    let mut terminal = false;
    loop {
        let event = smol::future::or(async { handle.events.recv().await.ok() }, async {
            smol::Timer::after(Duration::from_secs(180)).await;
            None
        })
        .await;
        let Some(event) = event else {
            eprintln!("pi_probe: timed out");
            break;
        };
        println!(
            "{}",
            serde_json::to_string(&event).expect("serialize event")
        );
        match event {
            AgentEvent::SessionStarted { .. } => started = true,
            AgentEvent::ApprovalRequested(request) => {
                let _ = handle
                    .commands
                    .send(SessionCommand::RespondApproval {
                        request_id: request.id,
                        decision: ApprovalDecision::Approve,
                    })
                    .await;
            }
            AgentEvent::TurnCompleted { .. } => {
                terminal = true;
                let _ = handle.commands.send(SessionCommand::Shutdown).await;
            }
            AgentEvent::Error { .. } => {
                terminal = true;
                let _ = handle.commands.send(SessionCommand::Shutdown).await;
            }
            AgentEvent::SessionClosed { .. } => break,
            _ => {}
        }
    }
    i32::from(!(started && terminal))
}
