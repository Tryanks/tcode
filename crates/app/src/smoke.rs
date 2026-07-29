//! Headless-ish acceptance mode: `tcode --smoke "<provider>|<cwd>|<prompt>"`
//! launches the real app, auto-creates a session, sends the prompt, and
//! auto-approves approvals; `--smoke-resume "<prompt>"` continues the most
//! recently updated stored session (exercising the resume cursor).
//!
//! Exit codes: 0 = turn completed, 1 = turn failed / fatal error, 2 = timeout.

use std::path::PathBuf;
use std::time::Duration;

use agent::ProviderKind;
use tcode_protocol::Command;
use tcode_runtime::pipe::HostHandle;

const SMOKE_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone)]
pub enum SmokeSpec {
    New {
        provider: ProviderKind,
        /// Which ACP agent to run, when `provider == Acp` (`--smoke "acp:<id>|…"`).
        acp_agent_id: Option<String>,
        /// Which provider profile to run against, for the native providers
        /// (`--smoke "claude:<profile-id>|…"`). `None` = the built-in profile.
        profile_id: Option<String>,
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
            // `acp:<agent-id>` runs an installed ACP agent; the native providers
            // keep their bare names, optionally suffixed with `:<profile-id>` to
            // run a user-created profile (e.g. `claude:klaude-kode`).
            let (provider, acp_agent_id, profile_id) = match parts.next() {
                Some(token) if token.starts_with("acp:") => (
                    ProviderKind::Acp,
                    Some(token.trim_start_matches("acp:").to_string()),
                    None,
                ),
                Some(token) => {
                    let (name, profile) = match token.split_once(':') {
                        Some((name, profile)) => (name, Some(profile.to_string())),
                        None => (token, None),
                    };
                    let kind = match name {
                        "codex" => ProviderKind::Codex,
                        "claude" => ProviderKind::ClaudeCode,
                        _ => usage(),
                    };
                    (kind, None, profile)
                }
                None => usage(),
            };
            let cwd = PathBuf::from(parts.next().unwrap_or_else(|| usage()));
            let prompt = parts.next().unwrap_or_else(|| usage()).to_string();
            Some(SmokeSpec::New {
                provider,
                acp_agent_id,
                profile_id,
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
        "usage: tcode [--smoke \"<codex|claude[:<profile-id>]|acp:<agent-id>>|<cwd>|<prompt>\"] [--smoke-resume \"<prompt>\"]"
    );
    std::process::exit(2);
}

/// Arm smoke mode and kick off the scripted flow. Call after the window opens.
pub fn drive(spec: SmokeSpec, host: HostHandle) {
    std::thread::spawn(|| {
        std::thread::sleep(SMOKE_TIMEOUT);
        log::error!("smoke: timed out after {SMOKE_TIMEOUT:?}");
        std::process::exit(2);
    });

    if let Err(error) = host.dispatch(Command::SetSmokeMode { auto_approve: true }) {
        log::error!("smoke: failed to enable smoke mode: {}", error.message);
        std::process::exit(1);
    }
    match spec {
        SmokeSpec::New {
            provider,
            acp_agent_id,
            profile_id,
            cwd,
            prompt,
        } => {
            log::info!(
                "smoke: creating {} session in {} (profile: {})",
                acp_agent_id
                    .clone()
                    .unwrap_or(provider.display_name().to_string()),
                cwd.display(),
                profile_id.as_deref().unwrap_or("built-in"),
            );
            if let Err(error) = host.dispatch(Command::CreateSession {
                provider,
                cwd,
                model: None,
                project_id: None,
                acp_agent_id,
                profile_id,
            }) {
                log::error!("smoke: failed to create session: {}", error.message);
                std::process::exit(1);
            }
            if let Err(error) = host.dispatch(Command::SendTurn {
                text: prompt,
                attachment_paths: Vec::new(),
            }) {
                log::error!("smoke: failed to send prompt: {}", error.message);
                std::process::exit(1);
            }
        }
        SmokeSpec::Resume { prompt } => {
            log::info!("smoke: resuming the most recently updated stored session");
            if let Err(error) = host.dispatch(Command::OpenLatestSession) {
                log::error!("smoke: failed to open latest session: {}", error.message);
                std::process::exit(1);
            }
            if let Err(error) = host.dispatch(Command::SendTurn {
                text: prompt,
                attachment_paths: Vec::new(),
            }) {
                log::error!("smoke: failed to send resume prompt: {}", error.message);
                std::process::exit(1);
            }
        }
    }
}
