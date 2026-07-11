//! Headless-ish acceptance mode: `tcode --smoke "<provider>|<cwd>|<prompt>"`
//! launches the real app, auto-creates a session, sends the prompt, and
//! auto-approves approvals; `--smoke-resume "<prompt>"` continues the most
//! recently updated stored session (exercising the resume cursor).
//!
//! Exit codes: 0 = turn completed, 1 = turn failed / fatal error, 2 = timeout.

use std::path::PathBuf;
use std::time::Duration;

use agent::ProviderKind;
use gpui::{App, Entity};

use crate::app::{AppState, SmokeMode};

const SMOKE_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone)]
pub enum SmokeSpec {
    New {
        provider: ProviderKind,
        /// Which ACP agent to run, when `provider == Acp` (`--smoke "acp:<id>|…"`).
        acp_agent_id: Option<String>,
        cwd: PathBuf,
        prompt: String,
    },
    Resume {
        prompt: String,
    },
}

/// Parse `--smoke` / `--smoke-resume` from argv. Exits with code 2 on bad usage.
pub fn parse_args() -> Option<SmokeSpec> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--smoke") => {
            let spec = args.next().unwrap_or_else(|| usage());
            let mut parts = spec.splitn(3, '|');
            // `acp:<agent-id>` runs an installed ACP agent; the two native
            // providers keep their bare names.
            let (provider, acp_agent_id) = match parts.next() {
                Some("codex") => (ProviderKind::Codex, None),
                Some("claude") => (ProviderKind::ClaudeCode, None),
                Some(token) if token.starts_with("acp:") => (
                    ProviderKind::Acp,
                    Some(token.trim_start_matches("acp:").to_string()),
                ),
                _ => usage(),
            };
            let cwd = PathBuf::from(parts.next().unwrap_or_else(|| usage()));
            let prompt = parts.next().unwrap_or_else(|| usage()).to_string();
            Some(SmokeSpec::New {
                provider,
                acp_agent_id,
                cwd,
                prompt,
            })
        }
        Some("--smoke-resume") => {
            let prompt = args.next().unwrap_or_else(|| usage());
            Some(SmokeSpec::Resume { prompt })
        }
        _ => None,
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: tcode [--smoke \"<codex|claude|acp:<agent-id>>|<cwd>|<prompt>\"] [--smoke-resume \"<prompt>\"]"
    );
    std::process::exit(2);
}

/// Arm smoke mode and kick off the scripted flow. Call after the window opens.
pub fn drive(spec: SmokeSpec, app_state: Entity<AppState>, cx: &mut App) {
    std::thread::spawn(|| {
        std::thread::sleep(SMOKE_TIMEOUT);
        log::error!("smoke: timed out after {SMOKE_TIMEOUT:?}");
        std::process::exit(2);
    });

    app_state.update(cx, |state, cx| {
        state.smoke = Some(SmokeMode { auto_approve: true });
        match spec {
            SmokeSpec::New {
                provider,
                acp_agent_id,
                cwd,
                prompt,
            } => {
                log::info!(
                    "smoke: creating {} session in {}",
                    acp_agent_id
                        .clone()
                        .unwrap_or(provider.display_name().to_string()),
                    cwd.display()
                );
                state.create_session(provider, cwd, None, None, acp_agent_id, cx);
                state.send_turn(prompt, Vec::new(), cx);
            }
            SmokeSpec::Resume { prompt } => {
                let Some(meta) = state.sessions.first().cloned() else {
                    log::error!("smoke: no stored sessions to resume");
                    std::process::exit(1);
                };
                log::info!(
                    "smoke: resuming most recent session {} ({}, resume cursor present: {})",
                    meta.id,
                    meta.provider.display_name(),
                    meta.resume_cursor.is_some()
                );
                state.select_session(&meta.id.clone(), cx);
                state.send_turn(prompt, Vec::new(), cx);
            }
        }
    });
}
