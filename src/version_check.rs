//! Provider CLI version checks and self-update command mapping (s3 §6).
//!
//! Pure helpers: parse a version out of `<provider> --version` output, compare
//! it against the latest published version, guess how the binary was installed
//! (Homebrew / npm / native installer), and derive the update command for that
//! install source. All I/O (spawning `--version`, `npm view`, the update
//! command itself) lives in [`crate::app`]; this module stays unit-testable.

use std::path::Path;

use agent::ProviderKind;

/// The npm package name whose published version is the provider's "latest".
/// `npm view <pkg> version` works for both providers (verified 2026-07); brew's
/// JSON was unreliable here, so npm is the single source of truth for "latest".
pub fn npm_package(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::ClaudeCode => "@anthropic-ai/claude-code",
        ProviderKind::Codex => "@openai/codex",
    }
}

/// How a provider CLI was installed, inferred from its resolved binary path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallSource {
    /// Homebrew (`/opt/homebrew/…`, `/usr/local/…`, `…/Cellar/…`).
    Brew,
    /// A global npm/pnpm/bun/volta/fnm/nvm install.
    Npm,
    /// The provider's native installer (e.g. `~/.local/bin`).
    Native,
    #[default]
    Unknown,
}

/// Guess the install source from a resolved binary path.
pub fn detect_install_source(path: &Path) -> InstallSource {
    let p = path.to_string_lossy();
    if p.contains("/Cellar/")
        || p.contains("/opt/homebrew/")
        || p.contains("/homebrew/")
        || p.contains("/usr/local/Cellar/")
    {
        InstallSource::Brew
    } else if p.contains("/node_modules/")
        || p.contains("/.nvm/")
        || p.contains("/.volta/")
        || p.contains("/fnm")
        || p.contains("/.pnpm")
        || p.contains("/.bun/")
        || p.contains("/npm/")
        || p.contains("/lib/node_modules/")
    {
        InstallSource::Npm
    } else if p.contains("/.local/") {
        InstallSource::Native
    } else {
        InstallSource::Unknown
    }
}

/// The command (program + args) that updates the provider for a given install
/// source, or `None` when we don't know how to update it (the UI then shows a
/// copyable manual instruction). Mapping:
/// - Brew: `brew upgrade <formula>`
/// - Npm: `npm install -g <pkg>@latest`
/// - Native (Claude): `claude update`
pub fn update_command(provider: ProviderKind, source: InstallSource) -> Option<Vec<String>> {
    let s = |v: &str| v.to_string();
    match (provider, source) {
        (ProviderKind::ClaudeCode, InstallSource::Brew) => {
            Some(vec![s("brew"), s("upgrade"), s("claude-code")])
        }
        (ProviderKind::Codex, InstallSource::Brew) => {
            Some(vec![s("brew"), s("upgrade"), s("codex")])
        }
        (provider, InstallSource::Npm) => Some(vec![
            s("npm"),
            s("install"),
            s("-g"),
            format!("{}@latest", npm_package(provider)),
        ]),
        (ProviderKind::ClaudeCode, InstallSource::Native) => Some(vec![s("claude"), s("update")]),
        // Native Codex has no documented self-update subcommand; fall through.
        _ => None,
    }
}

/// Parse the first semver-looking `MAJOR.MINOR.PATCH` token out of a version
/// line, tolerating leading program names and trailing suffixes:
/// - `"2.1.206 (Claude Code)"` → `(2, 1, 206)`
/// - `"codex-cli 0.144.1"` → `(0, 144, 1)`
/// - `"2.1.207"` → `(2, 1, 207)`
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    for token in text.split_whitespace() {
        if let Some(v) = parse_semver_token(token) {
            return Some(v);
        }
    }
    None
}

fn parse_semver_token(token: &str) -> Option<(u32, u32, u32)> {
    // Trim a leading `v` and any build/prerelease suffix.
    let token = token.trim_start_matches('v');
    let core = token.split(['-', '+', ' ']).next().unwrap_or(token);
    let mut parts = core.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    // Reject a bare "1" or a two-part-only line masquerading as a version.
    core.matches('.').count().ge(&1).then_some((major, minor, patch))
}

/// Whether `latest` is strictly newer than `installed` (both parsed from raw
/// `--version` / `npm view` output). `false` if either can't be parsed.
pub fn is_update_available(installed: &str, latest: &str) -> bool {
    match (parse_version(installed), parse_version(latest)) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_versions_from_provider_output() {
        assert_eq!(parse_version("2.1.206 (Claude Code)"), Some((2, 1, 206)));
        assert_eq!(parse_version("codex-cli 0.144.1"), Some((0, 144, 1)));
        assert_eq!(parse_version("2.1.207"), Some((2, 1, 207)));
        assert_eq!(parse_version("v1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("nonsense"), None);
        // A bare integer is not a version.
        assert_eq!(parse_version("build 5"), None);
    }

    #[test]
    fn compares_versions() {
        assert!(is_update_available("2.1.206 (Claude Code)", "2.1.207"));
        assert!(!is_update_available("2.1.207", "2.1.207"));
        assert!(!is_update_available("2.1.208", "2.1.207"));
        assert!(is_update_available("codex-cli 0.144.1", "0.145.0"));
        // Unparseable → no update claimed.
        assert!(!is_update_available("unknown", "2.0.0"));
    }

    #[test]
    fn detects_install_source_from_path() {
        assert_eq!(
            detect_install_source(&PathBuf::from("/opt/homebrew/bin/codex")),
            InstallSource::Brew
        );
        assert_eq!(
            detect_install_source(&PathBuf::from("/Users/x/.local/bin/claude")),
            InstallSource::Native
        );
        assert_eq!(
            detect_install_source(&PathBuf::from(
                "/Users/x/.nvm/versions/node/v20/bin/codex"
            )),
            InstallSource::Npm
        );
        assert_eq!(
            detect_install_source(&PathBuf::from("/usr/bin/codex")),
            InstallSource::Unknown
        );
    }

    #[test]
    fn maps_update_commands() {
        assert_eq!(
            update_command(ProviderKind::Codex, InstallSource::Brew),
            Some(vec!["brew".into(), "upgrade".into(), "codex".into()])
        );
        assert_eq!(
            update_command(ProviderKind::ClaudeCode, InstallSource::Npm),
            Some(vec![
                "npm".into(),
                "install".into(),
                "-g".into(),
                "@anthropic-ai/claude-code@latest".into()
            ])
        );
        assert_eq!(
            update_command(ProviderKind::ClaudeCode, InstallSource::Native),
            Some(vec!["claude".into(), "update".into()])
        );
        // Native Codex has no known self-update command.
        assert_eq!(update_command(ProviderKind::Codex, InstallSource::Native), None);
    }
}
