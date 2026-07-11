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
    /// A global npm (or npm-compatible: nvm/volta/fnm) install.
    Npm,
    /// A global Bun install (`~/.bun/bin`).
    Bun,
    /// A global pnpm install (`~/Library/pnpm`, `~/.pnpm`, the pnpm store).
    Pnpm,
    /// The provider's native installer (e.g. `~/.local/bin`).
    Native,
    #[default]
    Unknown,
}

/// Guess the install source from a resolved binary path.
///
/// Order matters: Bun and pnpm both keep global bins in their own directories,
/// so they are matched before the generic npm/node patterns.
///
/// Backslashes are normalized to `/` first, so the Windows shapes
/// (`C:\Users\x\AppData\Roaming\npm\claude.cmd`,
/// `C:\Users\x\AppData\Local\pnpm\codex.cmd`) match the same patterns. Homebrew
/// does not exist on Windows, so [`InstallSource::Brew`] is unreachable there —
/// which matters because a Windows user directory can legitimately contain
/// "homebrew" (e.g. a checkout) and must not be misdetected.
pub fn detect_install_source(path: &Path) -> InstallSource {
    let raw = path.to_string_lossy();
    let p = raw.replace('\\', "/");
    let brew = cfg!(not(windows))
        && (p.contains("/Cellar/")
            || p.contains("/opt/homebrew/")
            || p.contains("/homebrew/")
            || p.contains("/usr/local/Cellar/"));
    if brew {
        InstallSource::Brew
    } else if p.contains("/.bun/") || p.contains("/bun/install/") {
        InstallSource::Bun
    } else if p.contains("/.pnpm")
        || p.contains("/pnpm/")
        || p.contains("/Library/pnpm")
        // Windows: pnpm's global bin dir (PNPM_HOME).
        || p.contains("/AppData/Local/pnpm")
    {
        InstallSource::Pnpm
    } else if p.contains("/node_modules/")
        || p.contains("/.nvm/")
        || p.contains("/.volta/")
        || p.contains("/fnm")
        || p.contains("/npm/")
        || p.contains("/lib/node_modules/")
        // Windows: the global npm prefix (%APPDATA%\npm) — also caught by the
        // `/npm/` pattern above, but spelled out because it is *the* npm shape
        // there and must survive any future narrowing of that pattern.
        || p.contains("/AppData/Roaming/npm")
    {
        InstallSource::Npm
    } else if p.contains("/.local/") {
        InstallSource::Native
    } else {
        InstallSource::Unknown
    }
}

/// The Homebrew formula that ships each provider.
fn brew_formula(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::ClaudeCode => "claude-code",
        ProviderKind::Codex => "codex",
    }
}

/// The command (program + args) that updates the provider for a given install
/// source, or `None` when we don't know how to update it (an unrecognized
/// explicit binary path is manual-only, exactly as in T3). Mapping (T3 §3):
///
/// | Source | Codex | Claude |
/// |---|---|---|
/// | npm | `npm install -g @openai/codex@latest` | `npm install -g @anthropic-ai/claude-code@latest` |
/// | Bun | `bun i -g @openai/codex@latest` | `bun i -g @anthropic-ai/claude-code@latest` |
/// | pnpm | `pnpm add -g @openai/codex@latest` | `pnpm add -g @anthropic-ai/claude-code@latest` |
/// | Homebrew | `brew upgrade codex` | `brew upgrade claude-code` |
/// | native | — (no self-update) | `claude update` |
pub fn update_command(provider: ProviderKind, source: InstallSource) -> Option<Vec<String>> {
    let s = |v: &str| v.to_string();
    let pkg = || format!("{}@latest", npm_package(provider));
    match (provider, source) {
        (provider, InstallSource::Brew) => {
            Some(vec![s("brew"), s("upgrade"), s(brew_formula(provider))])
        }
        (_, InstallSource::Npm) => Some(vec![s("npm"), s("install"), s("-g"), pkg()]),
        (_, InstallSource::Bun) => Some(vec![s("bun"), s("i"), s("-g"), pkg()]),
        (_, InstallSource::Pnpm) => Some(vec![s("pnpm"), s("add"), s("-g"), pkg()]),
        (ProviderKind::ClaudeCode, InstallSource::Native) => Some(vec![s("claude"), s("update")]),
        // Native Codex has no documented self-update subcommand, and an
        // unrecognized path is manual-only (no command to show or run).
        _ => None,
    }
}

/// The same command rendered as the copyable one-liner shown in the update
/// popover's code block (`None` when the source is manual-only).
pub fn update_command_string(provider: ProviderKind, source: InstallSource) -> Option<String> {
    update_command(provider, source).map(|parts| parts.join(" "))
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
    core.matches('.')
        .count()
        .ge(&1)
        .then_some((major, minor, patch))
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
            detect_install_source(&PathBuf::from("/Users/x/.nvm/versions/node/v20/bin/codex")),
            InstallSource::Npm
        );
        assert_eq!(
            detect_install_source(&PathBuf::from("/Users/x/.bun/bin/claude")),
            InstallSource::Bun
        );
        assert_eq!(
            detect_install_source(&PathBuf::from("/Users/x/Library/pnpm/codex")),
            InstallSource::Pnpm
        );
        assert_eq!(
            detect_install_source(&PathBuf::from("/usr/bin/codex")),
            InstallSource::Unknown
        );
    }

    /// The Windows path shapes: backslash-separated, `.cmd`/`.exe` shims under
    /// %APPDATA% / %LOCALAPPDATA% / the user profile.
    #[test]
    fn detects_install_source_from_windows_paths() {
        assert_eq!(
            detect_install_source(&PathBuf::from(r"C:\Users\x\AppData\Roaming\npm\claude.cmd")),
            InstallSource::Npm
        );
        assert_eq!(
            detect_install_source(&PathBuf::from(r"C:\Users\x\AppData\Local\pnpm\codex.cmd")),
            InstallSource::Pnpm
        );
        assert_eq!(
            detect_install_source(&PathBuf::from(r"C:\Users\x\.bun\bin\claude.exe")),
            InstallSource::Bun
        );
        assert_eq!(
            detect_install_source(&PathBuf::from(
                r"C:\Program Files\nodejs\node_modules\npm\bin\codex.cmd"
            )),
            InstallSource::Npm
        );
        assert_eq!(
            detect_install_source(&PathBuf::from(r"C:\tools\codex.exe")),
            InstallSource::Unknown
        );
    }

    /// Homebrew cannot be an install source on Windows: a user path that merely
    /// *contains* "homebrew" (a checkout, a WSL mount) must not be misread. On
    /// Unix the very same string is a real Homebrew install.
    #[test]
    fn brew_is_unreachable_on_windows() {
        let path = PathBuf::from("/opt/homebrew/bin/codex");
        let expected = if cfg!(windows) {
            InstallSource::Unknown
        } else {
            InstallSource::Brew
        };
        assert_eq!(detect_install_source(&path), expected);
    }

    /// The exact command table from the T3 Providers spec (§3), for every
    /// detected source × provider pair we support.
    #[test]
    fn maps_update_commands_per_source_and_provider() {
        use InstallSource::*;
        use ProviderKind::*;
        let table: [(ProviderKind, InstallSource, Option<&str>); 12] = [
            (Codex, Npm, Some("npm install -g @openai/codex@latest")),
            (
                ClaudeCode,
                Npm,
                Some("npm install -g @anthropic-ai/claude-code@latest"),
            ),
            (Codex, Bun, Some("bun i -g @openai/codex@latest")),
            (
                ClaudeCode,
                Bun,
                Some("bun i -g @anthropic-ai/claude-code@latest"),
            ),
            (Codex, Pnpm, Some("pnpm add -g @openai/codex@latest")),
            (
                ClaudeCode,
                Pnpm,
                Some("pnpm add -g @anthropic-ai/claude-code@latest"),
            ),
            (Codex, Brew, Some("brew upgrade codex")),
            (ClaudeCode, Brew, Some("brew upgrade claude-code")),
            (ClaudeCode, Native, Some("claude update")),
            // Native Codex has no documented self-update subcommand.
            (Codex, Native, None),
            // An unrecognized path is manual-only in T3: no command at all.
            (Codex, Unknown, None),
            (ClaudeCode, Unknown, None),
        ];
        for (provider, source, expected) in table {
            assert_eq!(
                update_command_string(provider, source).as_deref(),
                expected,
                "{provider:?} / {source:?}"
            );
        }
    }

    #[test]
    fn update_command_parts_match_the_rendered_string() {
        assert_eq!(
            update_command(ProviderKind::ClaudeCode, InstallSource::Npm),
            Some(vec![
                "npm".into(),
                "install".into(),
                "-g".into(),
                "@anthropic-ai/claude-code@latest".into()
            ])
        );
    }
}
