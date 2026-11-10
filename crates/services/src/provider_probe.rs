//! Provider CLI probing and process execution.

use std::path::PathBuf;

use agent::{LaunchEnv, ProviderKind};
use tcode_core::provider_status::{
    AuthStatus, ProviderAuth, ProviderProbeDiagnostic, ProviderSnapshot, ProviderStatusKind,
};

use crate::provider_auth::{
    claude_account_plan_name, parse_aggregator_auth, parse_claude_auth, parse_codex_auth,
    parse_opencode_auth,
};

/// Return trimmed stdout on success, falling back to stderr when stdout is empty.
pub async fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    run_capture_env(program, args, &[]).await
}

/// [`run_capture`] with extra environment variables applied to the child.
pub async fn run_capture_env(
    program: &str,
    args: &[&str],
    env: &[(String, String)],
) -> Option<String> {
    let output = provider_command(program, env)
        .args(args)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !text.is_empty() {
        return Some(text);
    }
    let text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `<program> --version`'s answer on stdout, or what it printed otherwise:
/// its exit status and its error output, as the CLI wrote them.
async fn run_version(program: &str, env: &[(String, String)]) -> Result<String, Option<String>> {
    let output = provider_command(program, env)
        .arg("--version")
        .output()
        .await
        .map_err(|error| Some(error.to_string()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        return if stdout.is_empty() {
            Err((!stderr.is_empty()).then_some(stderr))
        } else {
            Ok(stdout)
        };
    }
    let printed = if stderr.is_empty() { stdout } else { stderr };
    Err(Some(if printed.is_empty() {
        output.status.to_string()
    } else {
        format!("{}: {printed}", output.status)
    }))
}

/// Trimmed stdout whatever the exit status, for a CLI whose exit status only
/// restates the answer it prints.
async fn run_stdout_env(program: &str, args: &[&str], env: &[(String, String)]) -> Option<String> {
    let output = provider_command(program, env)
        .args(args)
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// A provider CLI child with the profile's environment applied, never told
/// it runs inside another Claude Code session.
pub(crate) fn provider_command(program: &str, env: &[(String, String)]) -> smol::process::Command {
    let mut cmd = crate::process::async_command(program);
    cmd.env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd
}

/// Probe one provider's installation, version, and authentication state.
pub async fn probe_provider(
    provider: ProviderKind,
    binary: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> ProviderSnapshot {
    let checked_at = Some(crate::store::now_secs());
    let Some(binary) = binary else {
        let diagnostic = Some(ProviderProbeDiagnostic::MissingCli);
        return ProviderSnapshot {
            checked_at,
            installed: false,
            status: Some(ProviderStatusKind::Error),
            diagnostic,
            message: None,
            ..ProviderSnapshot::default()
        };
    };
    let program = binary.to_string_lossy().into_owned();
    let env = launch_env.pairs(provider);

    let raw_version = match run_version(&program, &env).await {
        Ok(raw_version) => raw_version,
        Err(output) => {
            return ProviderSnapshot {
                checked_at,
                installed: true,
                status: Some(ProviderStatusKind::Error),
                diagnostic: Some(ProviderProbeDiagnostic::FailedCli),
                message: output,
                binary: Some(program),
                ..ProviderSnapshot::default()
            };
        }
    };
    let version = crate::version_check::provider_updates::parse_version(&raw_version)
        .map(|(a, b, c)| format!("{a}.{b}.{c}"))
        .or(Some(raw_version));

    let auth = match provider {
        // `claude auth status` exits 1 when signed out.
        ProviderKind::ClaudeCode => {
            let plan_name = claude_account_plan_name(&launch_env);
            run_stdout_env(&program, &["auth", "status", "--json"], &env)
                .await
                .as_deref()
                .and_then(|json| parse_claude_auth(json, plan_name.as_deref()))
        }
        ProviderKind::Codex => {
            let path = agent::codex::home_dir(&launch_env).map(|home| home.join("auth.json"));
            let json = path.and_then(|path| std::fs::read_to_string(path).ok());
            json.as_deref().and_then(parse_codex_auth)
        }
        ProviderKind::Pi => {
            let path = agent::pi::agent_dir(&launch_env).map(|home| home.join("auth.json"));
            let json = path.and_then(|path| std::fs::read_to_string(path).ok());
            json.as_deref().and_then(parse_aggregator_auth)
        }
        // OpenCode 2.x keeps credentials in its database; a private server
        // lists them without starting the shared background service.
        ProviderKind::OpenCode => run_capture_env(
            &program,
            &["auth", "list", "--standalone", "--format", "json"],
            &env,
        )
        .await
        .as_deref()
        .and_then(parse_opencode_auth),
        ProviderKind::Cursor => run_capture_env(&program, &["status", "--format", "json"], &env)
            .await
            .as_deref()
            .and_then(|json| parse_cursor_auth(json, cursor_key_configured(&env))),
        ProviderKind::Grok => run_capture_env(&program, &["models"], &env)
            .await
            .as_deref()
            .and_then(parse_grok_models_auth),
        // Authentication over ACP is surfaced by the session protocol.
        ProviderKind::Acp => None,
    };

    ProviderSnapshot {
        binary: Some(program),
        ..finalize_probe(checked_at, version, auth)
    }
}

/// `cursor-agent status --format json` reports only the stored login: it says
/// `unauthenticated` while CURSOR_API_KEY or CURSOR_AUTH_TOKEN signs the CLI
/// in, so it counts as signed out only when neither is set.
fn parse_cursor_auth(json: &str, key_configured: bool) -> Option<ProviderAuth> {
    let status: serde_json::Value = serde_json::from_str(json).ok()?;
    if status.get("isAuthenticated") == Some(&serde_json::Value::Bool(true)) {
        return Some(ProviderAuth {
            status: AuthStatus::Authenticated,
            label: None,
            email: status
                .pointer("/userInfo/email")
                .and_then(serde_json::Value::as_str)
                .filter(|email| !email.is_empty())
                .map(str::to_string),
        });
    }
    (status.get("status").and_then(serde_json::Value::as_str) == Some("unauthenticated")
        && !key_configured)
        .then_some(ProviderAuth {
            status: AuthStatus::Unauthenticated,
            label: None,
            email: None,
        })
}

/// Whether Cursor's child sees an API key or auth token, from the profile's
/// environment or the one it inherits.
fn cursor_key_configured(env: &[(String, String)]) -> bool {
    ["CURSOR_API_KEY", "CURSOR_AUTH_TOKEN"].iter().any(|key| {
        match env.iter().rev().find(|(name, _)| name == key) {
            Some((_, value)) => !value.is_empty(),
            None => std::env::var_os(key).is_some_and(|value| !value.is_empty()),
        }
    })
}

/// The first line of `grok models`. Only the states observed from Grok 1.0.46
/// are recognized; the API-key line reports a configured key, which Grok does
/// not validate there.
fn parse_grok_models_auth(output: &str) -> Option<ProviderAuth> {
    let (status, label) = match output.lines().next()?.trim() {
        "You are not authenticated." => (AuthStatus::Unauthenticated, None),
        "You are using XAI_API_KEY." => (AuthStatus::Authenticated, Some("xAI API Key")),
        _ => return None,
    };
    Some(ProviderAuth {
        status,
        label: label.map(str::to_string),
        email: None,
    })
}

fn finalize_probe(
    checked_at: Option<u64>,
    version: Option<String>,
    auth: Option<ProviderAuth>,
) -> ProviderSnapshot {
    let (status, diagnostic, auth) = match &auth {
        Some(provider_auth) if provider_auth.status == AuthStatus::Unauthenticated => (
            ProviderStatusKind::Error,
            Some(ProviderProbeDiagnostic::Unauthenticated),
            auth,
        ),
        Some(_) => (ProviderStatusKind::Ready, None, auth),
        None => (
            ProviderStatusKind::Warning,
            Some(ProviderProbeDiagnostic::IndeterminateAuth),
            Some(ProviderAuth {
                status: AuthStatus::Unknown,
                label: None,
                email: None,
            }),
        ),
    };
    ProviderSnapshot {
        checked_at,
        installed: true,
        version,
        status: Some(status),
        auth,
        diagnostic,
        message: None,
        checking: false,
        login: None,
        binary: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_is_semantic_and_unlocalized() {
        let result = smol::block_on(probe_provider(
            ProviderKind::Codex,
            None,
            LaunchEnv::default(),
        ));
        assert!(!result.installed);
        assert_eq!(result.status, Some(ProviderStatusKind::Error));
        assert_eq!(result.message, None);
        assert_eq!(result.diagnostic, Some(ProviderProbeDiagnostic::MissingCli));
    }

    /// `cursor-agent status --format json` as the real binary answers it signed out
    /// (`agent/tests/fixtures/cursor/PROVENANCE.md`) reads as signed out only while neither
    /// `CURSOR_API_KEY` nor `CURSOR_AUTH_TOKEN` signs the CLI in.
    #[test]
    fn a_recorded_signed_out_cursor_is_unauthenticated_only_without_a_key_or_token() {
        let recorded = include_str!("../../agent/tests/fixtures/cursor/status_signed_out.json");
        let diagnostic = |env: &[(&str, &str)]| {
            let env: Vec<(String, String)> = env
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            finalize_probe(
                None,
                None,
                parse_cursor_auth(recorded, cursor_key_configured(&env)),
            )
            .diagnostic
        };
        assert_eq!(
            diagnostic(&[("CURSOR_API_KEY", ""), ("CURSOR_AUTH_TOKEN", "")]),
            Some(ProviderProbeDiagnostic::Unauthenticated)
        );
        for env in [
            [("CURSOR_API_KEY", "key"), ("CURSOR_AUTH_TOKEN", "")],
            [("CURSOR_API_KEY", ""), ("CURSOR_AUTH_TOKEN", "token")],
        ] {
            assert_eq!(
                diagnostic(&env),
                Some(ProviderProbeDiagnostic::IndeterminateAuth),
                "a key or token signs Cursor in without a stored login"
            );
        }
    }
}
