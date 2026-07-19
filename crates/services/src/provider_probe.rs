//! Provider CLI probing and process execution.

use std::path::PathBuf;

use agent::{LaunchEnv, ProviderKind};
use tcode_core::provider_status::{
    AuthStatus, ProviderAuth, ProviderProbeDiagnostic, ProviderSnapshot, ProviderStatusKind,
};

use crate::provider_auth::{parse_claude_auth, parse_codex_auth};

/// The bare command name for a provider (fallback when no path resolves).
pub fn default_program(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => "codex".into(),
        ProviderKind::ClaudeCode => "claude".into(),
        ProviderKind::Pi => "pi".into(),
        ProviderKind::OpenCode => "opencode".into(),
        // ACP agents carry their own registry launch recipe.
        ProviderKind::Acp => String::new(),
    }
}

/// Locate the first executable named `name` on PATH.
pub fn which_in_path(name: &str) -> Option<PathBuf> {
    agent::find_on_path(name)
}

/// Spawn `program args...` and return its trimmed stdout on success.
pub async fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    run_capture_env(program, args, &[]).await
}

/// [`run_capture`] with extra environment variables applied to the child.
pub async fn run_capture_env(
    program: &str,
    args: &[&str],
    env: &[(String, String)],
) -> Option<String> {
    let mut cmd = crate::process::async_command(program);
    cmd.args(args)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !text.is_empty() {
        return Some(text);
    }
    // Some CLIs (pi among them) print `--version` output to stderr; a clean
    // exit with empty stdout is still a successful run.
    let text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    (!text.is_empty()).then_some(text)
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

    let Some(raw_version) = run_capture_env(&program, &["--version"], &env).await else {
        let diagnostic = Some(ProviderProbeDiagnostic::FailedCli);
        return ProviderSnapshot {
            checked_at,
            installed: true,
            status: Some(ProviderStatusKind::Error),
            diagnostic,
            message: None,
            ..ProviderSnapshot::default()
        };
    };
    let version = crate::version_check::parse_version(&raw_version)
        .map(|(a, b, c)| format!("{a}.{b}.{c}"))
        .or(Some(raw_version));

    let auth = match provider {
        ProviderKind::ClaudeCode => run_capture_env(&program, &["auth", "status", "--json"], &env)
            .await
            .as_deref()
            .and_then(parse_claude_auth),
        ProviderKind::Codex => {
            let home = launch_env
                .home
                .or_else(|| dirs::home_dir().map(|home| home.join(".codex")));
            let path = home.map(|home| home.join("auth.json"));
            // This is a small local JSON file; keep the direct read used by the
            // app rather than introducing a thread-pool hop.
            let json = path.and_then(|path| std::fs::read_to_string(path).ok());
            json.as_deref().and_then(parse_codex_auth)
        }
        // Both CLIs can aggregate credentials for several upstream model
        // providers. Installation/version are definitive; auth remains
        // indeterminate until a session/model query succeeds.
        ProviderKind::Pi | ProviderKind::OpenCode => None,
        // ACP authentication is surfaced by its session protocol.
        ProviderKind::Acp => None,
    };

    finalize_probe(checked_at, version, auth)
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
    }
}

/// Run `program args...` for a side effect and report successful exit status.
pub async fn run_status(program: &str, args: &[&str]) -> bool {
    crate::process::async_command(program)
        .args(args)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_programs_cover_native_and_acp() {
        assert_eq!(default_program(ProviderKind::Codex), "codex");
        assert_eq!(default_program(ProviderKind::ClaudeCode), "claude");
        assert_eq!(default_program(ProviderKind::Pi), "pi");
        assert_eq!(default_program(ProviderKind::OpenCode), "opencode");
        assert_eq!(default_program(ProviderKind::Acp), "");
    }

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

    #[test]
    fn auth_outcomes_are_semantic_and_unlocalized() {
        let authenticated = finalize_probe(
            Some(1),
            Some("1.2.3".into()),
            Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: Some("account".into()),
                email: None,
            }),
        );
        assert_eq!(authenticated.status, Some(ProviderStatusKind::Ready));
        assert_eq!(authenticated.message, None);
        assert_eq!(authenticated.diagnostic, None);

        let unauthenticated = finalize_probe(
            Some(1),
            Some("1.2.3".into()),
            Some(ProviderAuth {
                status: AuthStatus::Unauthenticated,
                label: None,
                email: None,
            }),
        );
        assert_eq!(unauthenticated.status, Some(ProviderStatusKind::Error));
        assert_eq!(unauthenticated.message, None);
        assert_eq!(
            unauthenticated.diagnostic,
            Some(ProviderProbeDiagnostic::Unauthenticated)
        );
        let indeterminate = finalize_probe(Some(1), Some("1.2.3".into()), None);
        assert_eq!(indeterminate.status, Some(ProviderStatusKind::Warning));
        assert_eq!(
            indeterminate.auth.as_ref().map(|auth| auth.status),
            Some(AuthStatus::Unknown)
        );
        assert_eq!(indeterminate.message, None);
        assert_eq!(
            indeterminate.diagnostic,
            Some(ProviderProbeDiagnostic::IndeterminateAuth)
        );
    }
}
