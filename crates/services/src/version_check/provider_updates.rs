//! Assessment of provider CLI updates from process-runner facts.

use std::path::Path;

use agent::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallSource {
    Brew,
    Npm,
    Bun,
    Pnpm,
    Native,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckInput<'a> {
    pub binary_path: Option<&'a Path>,
    pub installed_output: Option<&'a str>,
    pub latest_output: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    InstalledVersionUnavailable,
    LatestVersionUnavailable,
    InvalidInstalledVersion,
    InvalidLatestVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    UpToDate {
        current: String,
        latest: String,
        install_source: InstallSource,
    },
    UpdateAvailable {
        current: String,
        latest: String,
        install_source: InstallSource,
    },
    Unknown {
        reason: UnknownReason,
        current: Option<String>,
        latest: Option<String>,
        install_source: InstallSource,
    },
}

/// Assess a provider update from facts gathered by the runtime's approved
/// process helpers. This owns source inference, parsing, normalization, and
/// comparison, but never spawns a process.
pub fn check(input: CheckInput<'_>) -> Assessment {
    let install_source = input
        .binary_path
        .map(detect_install_source)
        .unwrap_or_default();
    let current = input.installed_output.map(normalize_version);
    let latest = input.latest_output.map(normalize_version);

    let Some(installed_output) = input.installed_output else {
        return Assessment::Unknown {
            reason: UnknownReason::InstalledVersionUnavailable,
            current,
            latest,
            install_source,
        };
    };
    let Some(latest_output) = input.latest_output else {
        return Assessment::Unknown {
            reason: UnknownReason::LatestVersionUnavailable,
            current,
            latest,
            install_source,
        };
    };
    let Some(installed_version) = parse_version(installed_output) else {
        return Assessment::Unknown {
            reason: UnknownReason::InvalidInstalledVersion,
            current,
            latest,
            install_source,
        };
    };
    let Some(latest_version) = parse_version(latest_output) else {
        return Assessment::Unknown {
            reason: UnknownReason::InvalidLatestVersion,
            current,
            latest,
            install_source,
        };
    };
    let current = format_version(installed_version);
    let latest = format_version(latest_version);

    if latest_version > installed_version {
        Assessment::UpdateAvailable {
            current,
            latest,
            install_source,
        }
    } else {
        Assessment::UpToDate {
            current,
            latest,
            install_source,
        }
    }
}

/// The npm package queried by the runtime adapter for a provider's latest
/// published version.
pub fn npm_package(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::ClaudeCode => "@anthropic-ai/claude-code",
        ProviderKind::Codex => "@openai/codex",
        ProviderKind::Pi => "@earendil-works/pi-coding-agent",
        ProviderKind::OpenCode => "opencode-ai",
        ProviderKind::Acp => "",
    }
}

/// Guess the install source from a resolved binary path. Existing paths are
/// canonicalized so package-manager symlinks are classified by their targets.
pub fn detect_install_source(path: &Path) -> InstallSource {
    let canonical = path.canonicalize();
    let path = canonical.as_deref().unwrap_or(path);
    let raw = path.to_string_lossy();
    let p = raw.replace('\\', "/");
    let brew = cfg!(not(windows))
        && (p.contains("/Cellar/")
            || p.contains("/opt/homebrew/")
            || p.contains("/homebrew/")
            || p.contains("/usr/local/Cellar/"));
    if p.contains("/.bun/") || p.contains("/bun/install/") {
        InstallSource::Bun
    } else if p.contains("/.pnpm")
        || p.contains("/pnpm/")
        || p.contains("/Library/pnpm")
        || p.contains("/AppData/Local/pnpm")
    {
        InstallSource::Pnpm
    } else if p.contains("/node_modules/")
        || p.contains("/.nvm/")
        || p.contains("/.volta/")
        || p.contains("/fnm")
        || p.contains("/npm/")
        || p.contains("/lib/node_modules/")
        || p.contains("/AppData/Roaming/npm")
    {
        InstallSource::Npm
    } else if brew {
        InstallSource::Brew
    } else if p.contains("/.local/") {
        InstallSource::Native
    } else {
        InstallSource::Unknown
    }
}

fn brew_formula(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::ClaudeCode => "claude-code",
        ProviderKind::Codex => "codex",
        ProviderKind::Pi => "pi-coding-agent",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Acp => "",
    }
}

/// Return the provider's self-update command for its inferred install source.
pub fn update_command(provider: ProviderKind, source: InstallSource) -> Option<Vec<String>> {
    if provider == ProviderKind::Acp {
        return None;
    }
    let s = |value: &str| value.to_string();
    let package = || format!("{}@latest", npm_package(provider));
    match (provider, source) {
        (provider, InstallSource::Brew) => {
            Some(vec![s("brew"), s("upgrade"), s(brew_formula(provider))])
        }
        (_, InstallSource::Npm) => Some(vec![s("npm"), s("install"), s("-g"), package()]),
        (_, InstallSource::Bun) => Some(vec![s("bun"), s("i"), s("-g"), package()]),
        (_, InstallSource::Pnpm) => Some(vec![s("pnpm"), s("add"), s("-g"), package()]),
        (ProviderKind::ClaudeCode, InstallSource::Native) => Some(vec![s("claude"), s("update")]),
        (ProviderKind::Pi, InstallSource::Native) => Some(vec![s("pi"), s("update"), s("self")]),
        (ProviderKind::OpenCode, InstallSource::Native) => Some(vec![s("opencode"), s("upgrade")]),
        _ => None,
    }
}

pub fn update_command_string(provider: ProviderKind, source: InstallSource) -> Option<String> {
    update_command(provider, source).map(|parts| parts.join(" "))
}

/// Parse the first provider-version token from loose, human-facing CLI output.
///
/// Unlike app release tags, provider CLIs historically accept `MAJOR.MINOR`
/// and default the absent patch to zero. That behavior predates this refactor
/// and is preserved for compatibility with provider output shapes.
pub(super) fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    text.split_whitespace().find_map(parse_version_token)
}

fn parse_version_token(token: &str) -> Option<(u32, u32, u32)> {
    let token = token.trim_start_matches('v');
    let core = token.split(['-', '+', ' ']).next().unwrap_or(token);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    // Reject a bare integer; two-part provider versions are intentionally
    // accepted and normalized with a zero patch.
    (core.matches('.').count() >= 1).then_some((major, minor, patch))
}

fn normalize_version(raw: &str) -> String {
    parse_version(raw).map_or_else(|| raw.to_string(), format_version)
}

fn format_version((major, minor, patch): (u32, u32, u32)) -> String {
    format!("{major}.{minor}.{patch}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn input<'a>(installed: Option<&'a str>, latest: Option<&'a str>) -> CheckInput<'a> {
        CheckInput {
            binary_path: None,
            installed_output: installed,
            latest_output: latest,
        }
    }

    #[test]
    fn parses_and_compares_provider_outputs_through_check() {
        let cases = [
            ("2.1.206 (Claude Code)", "2.1.207", true, "2.1.206"),
            ("codex-cli 0.144.1", "0.145.0", true, "0.144.1"),
            ("2.1.207", "2.1.207", false, "2.1.207"),
            ("2.1.208", "2.1.207", false, "2.1.208"),
            ("v1.2.3-beta.1", "1.2.4", true, "1.2.3"),
            ("1.2", "1.2.1", true, "1.2.0"),
        ];
        for (installed, latest, available, normalized) in cases {
            let assessment = check(input(Some(installed), Some(latest)));
            match assessment {
                Assessment::UpdateAvailable { current, .. } if available => {
                    assert_eq!(current, normalized)
                }
                Assessment::UpToDate { current, .. } if !available => {
                    assert_eq!(current, normalized)
                }
                other => panic!("unexpected assessment for {installed}: {other:?}"),
            }
        }
    }

    #[test]
    fn invalid_and_missing_outputs_are_typed_unknowns() {
        assert!(matches!(
            check(input(None, Some("2.0.0"))),
            Assessment::Unknown {
                reason: UnknownReason::InstalledVersionUnavailable,
                current: None,
                latest: Some(latest),
                ..
            } if latest == "2.0.0"
        ));
        assert!(matches!(
            check(input(Some("1.0.0"), None)),
            Assessment::Unknown {
                reason: UnknownReason::LatestVersionUnavailable,
                current: Some(current),
                latest: None,
                ..
            } if current == "1.0.0"
        ));
        assert!(matches!(
            check(input(Some("build 5"), Some("2.0.0"))),
            Assessment::Unknown {
                reason: UnknownReason::InvalidInstalledVersion,
                current: Some(current),
                ..
            } if current == "build 5"
        ));
        assert!(matches!(
            check(input(Some("1.0.0"), Some("nonsense"))),
            Assessment::Unknown {
                reason: UnknownReason::InvalidLatestVersion,
                latest: Some(latest),
                ..
            } if latest == "nonsense"
        ));
    }

    #[test]
    fn infers_install_source_through_check() {
        let paths = [
            ("/Users/x/.local/bin/claude", InstallSource::Native),
            (
                "/Users/x/.nvm/versions/node/v20/bin/codex",
                InstallSource::Npm,
            ),
            ("/Users/x/.bun/bin/claude", InstallSource::Bun),
            ("/Users/x/Library/pnpm/codex", InstallSource::Pnpm),
            ("/usr/bin/codex", InstallSource::Unknown),
            (
                r"C:\Users\x\AppData\Roaming\npm\claude.cmd",
                InstallSource::Npm,
            ),
            (
                r"C:\Users\x\AppData\Local\pnpm\codex.cmd",
                InstallSource::Pnpm,
            ),
            (r"C:\Users\x\.bun\bin\claude.exe", InstallSource::Bun),
            (
                r"C:\Program Files\nodejs\node_modules\npm\bin\codex.cmd",
                InstallSource::Npm,
            ),
            (r"C:\tools\codex.exe", InstallSource::Unknown),
        ];
        for (path, expected) in paths {
            let path = PathBuf::from(path);
            let assessment = check(CheckInput {
                binary_path: Some(&path),
                installed_output: Some("1.0.0"),
                latest_output: Some("1.0.0"),
            });
            assert!(matches!(
                assessment,
                Assessment::UpToDate { install_source, .. } if install_source == expected
            ));
        }
    }

    #[test]
    fn maps_update_commands_per_source_and_provider() {
        use InstallSource::*;
        use ProviderKind::*;
        let table: [(ProviderKind, InstallSource, Option<&str>); 28] = [
            (Codex, Npm, Some("npm install -g @openai/codex@latest")),
            (
                ClaudeCode,
                Npm,
                Some("npm install -g @anthropic-ai/claude-code@latest"),
            ),
            (
                Pi,
                Npm,
                Some("npm install -g @earendil-works/pi-coding-agent@latest"),
            ),
            (OpenCode, Npm, Some("npm install -g opencode-ai@latest")),
            (Codex, Bun, Some("bun i -g @openai/codex@latest")),
            (
                ClaudeCode,
                Bun,
                Some("bun i -g @anthropic-ai/claude-code@latest"),
            ),
            (
                Pi,
                Bun,
                Some("bun i -g @earendil-works/pi-coding-agent@latest"),
            ),
            (OpenCode, Bun, Some("bun i -g opencode-ai@latest")),
            (Codex, Pnpm, Some("pnpm add -g @openai/codex@latest")),
            (
                ClaudeCode,
                Pnpm,
                Some("pnpm add -g @anthropic-ai/claude-code@latest"),
            ),
            (
                Pi,
                Pnpm,
                Some("pnpm add -g @earendil-works/pi-coding-agent@latest"),
            ),
            (OpenCode, Pnpm, Some("pnpm add -g opencode-ai@latest")),
            (Codex, Brew, Some("brew upgrade codex")),
            (ClaudeCode, Brew, Some("brew upgrade claude-code")),
            (Pi, Brew, Some("brew upgrade pi-coding-agent")),
            (OpenCode, Brew, Some("brew upgrade opencode")),
            (ClaudeCode, Native, Some("claude update")),
            (Pi, Native, Some("pi update self")),
            (OpenCode, Native, Some("opencode upgrade")),
            (Codex, Native, None),
            (Codex, Unknown, None),
            (ClaudeCode, Unknown, None),
            (Pi, Unknown, None),
            (OpenCode, Unknown, None),
            (Acp, Npm, None),
            (Acp, Brew, None),
            (Acp, Native, None),
            (Acp, Unknown, None),
        ];
        for (provider, source, expected) in table {
            assert_eq!(
                update_command_string(provider, source).as_deref(),
                expected,
                "{provider:?} / {source:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn canonicalizes_package_manager_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root =
            std::env::temp_dir().join(format!("tcode-version-check-{}", uuid::Uuid::new_v4()));
        let prefix = root.join("opt/homebrew");
        let target = prefix.join("lib/node_modules/@openai/codex/bin/codex.js");
        let launcher = prefix.join("bin/codex");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(&target).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&target, permissions).unwrap();
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        symlink("../lib/node_modules/@openai/codex/bin/codex.js", &launcher).unwrap();

        let assessment = check(CheckInput {
            binary_path: Some(&launcher),
            installed_output: Some("1.0.0"),
            latest_output: Some("1.0.0"),
        });
        assert!(matches!(
            assessment,
            Assessment::UpToDate {
                install_source: InstallSource::Npm,
                ..
            }
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}
