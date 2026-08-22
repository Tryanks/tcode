//! Provider CLI and tcode release version checks (s3 §6).
//!
//! Helpers parse and compare versions, infer provider install sources, and map
//! those sources to update commands. The tcode release lookup lives here beside
//! its fixture-testable JSON parser; process spawning and provider updates stay
//! in the runtime caller.

use std::io::Read as _;
use std::path::Path;
use std::time::Duration;

use agent::ProviderKind;
use serde::Deserialize;

const TCODE_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/Tryanks/tcode/releases/latest";

/// The release metadata needed by the app's update notice.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TcodeRelease {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub prerelease: bool,
}

/// Fetch the latest published tcode release. Network, rate-limit, and response
/// errors deliberately collapse to `None`: update checks must never disrupt
/// app startup or provider checks.
pub fn fetch_latest_tcode_release() -> Option<TcodeRelease> {
    let response = ureq::get(TCODE_LATEST_RELEASE_URL)
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "tcode-update-check")
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(1024 * 1024)
        .read_to_end(&mut body)
        .ok()?;
    parse_tcode_release(&body)
}

/// Parse the subset of GitHub's release JSON used by the update surface.
pub fn parse_tcode_release(bytes: &[u8]) -> Option<TcodeRelease> {
    serde_json::from_slice(bytes).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TcodeVersion {
    core: (u32, u32, u32),
    prerelease: bool,
}

fn parse_tcode_version(raw: &str) -> Option<TcodeVersion> {
    let raw = raw.trim().strip_prefix('v').unwrap_or(raw.trim());
    let without_build = raw.split_once('+').map_or(raw, |(core, _)| core);
    let (core, prerelease) = match without_build.split_once('-') {
        Some((core, suffix)) if !suffix.is_empty() => (core, true),
        Some(_) => return None,
        None => (without_build, false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(TcodeVersion {
        core: (major, minor, patch),
        prerelease,
    })
}

/// Compare a running app version with a GitHub release tag.
///
/// Prerelease releases are ignored for stable builds. A prerelease build may
/// advance to a newer prerelease numeric triple or to the stable release with
/// the same numeric triple. Malformed input returns `None`.
pub fn tcode_update_available(running: &str, latest_tag: &str) -> Option<bool> {
    let running = parse_tcode_version(running)?;
    let latest = parse_tcode_version(latest_tag)?;
    if latest.prerelease && !running.prerelease {
        return Some(false);
    }
    Some(match latest.core.cmp(&running.core) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => running.prerelease && !latest.prerelease,
    })
}

/// The npm package name whose published version is the provider's "latest".
/// `npm view <pkg> version` works for every native provider (verified 2026-07);
/// brew's JSON was unreliable here, so npm is the single source of truth for
/// "latest".
pub fn npm_package(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::ClaudeCode => "@anthropic-ai/claude-code",
        ProviderKind::Codex => "@openai/codex",
        ProviderKind::Pi => "@earendil-works/pi-coding-agent",
        ProviderKind::OpenCode => "opencode-ai",
        // ACP agents ship on their own cadence; no single npm package.
        // ACP agents are versioned by the registry, not by npm: their update
        // path is "install the newer version from the marketplace".
        ProviderKind::Acp => "",
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

/// Guess the install source from a binary path.
///
/// Existing paths are canonicalized first so a package-manager binary linked
/// from another prefix is classified by its target. If resolution fails (for
/// example, for a broken symlink or a path on another platform), the supplied
/// path is used unchanged.
///
/// Order matters: package-manager-specific paths are matched before broad
/// Homebrew prefixes, since a Homebrew `bin` symlink can target a global npm
/// installation under `/opt/homebrew/lib/node_modules`.
///
/// Backslashes are normalized to `/` first, so the Windows shapes
/// (`C:\Users\x\AppData\Roaming\npm\claude.cmd`,
/// `C:\Users\x\AppData\Local\pnpm\codex.cmd`) match the same patterns. Homebrew
/// does not exist on Windows, so [`InstallSource::Brew`] is unreachable there —
/// which matters because a Windows user directory can legitimately contain
/// "homebrew" (e.g. a checkout) and must not be misdetected.
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
    } else if brew {
        InstallSource::Brew
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
        ProviderKind::Pi => "pi-coding-agent",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Acp => "",
    }
}

/// The command (program + args) that updates the provider for a given install
/// source, or `None` when we don't know how to update it (an unrecognized
/// explicit binary path is manual-only, exactly as in T3). Mapping (T3 §3):
///
/// | Source | Codex | Claude | pi | OpenCode |
/// |---|---|---|---|---|
/// | npm | `npm install -g @openai/codex@latest` | `npm install -g @anthropic-ai/claude-code@latest` | `npm install -g @earendil-works/pi-coding-agent@latest` | `npm install -g opencode-ai@latest` |
/// | Bun | `bun i -g @openai/codex@latest` | `bun i -g @anthropic-ai/claude-code@latest` | `bun i -g @earendil-works/pi-coding-agent@latest` | `bun i -g opencode-ai@latest` |
/// | pnpm | `pnpm add -g @openai/codex@latest` | `pnpm add -g @anthropic-ai/claude-code@latest` | `pnpm add -g @earendil-works/pi-coding-agent@latest` | `pnpm add -g opencode-ai@latest` |
/// | Homebrew | `brew upgrade codex` | `brew upgrade claude-code` | `brew upgrade pi-coding-agent` | `brew upgrade opencode` |
/// | native | — (no self-update) | `claude update` | `pi update self` | `opencode upgrade` |
pub fn update_command(provider: ProviderKind, source: InstallSource) -> Option<Vec<String>> {
    // ACP agents update through the marketplace (Settings → Providers), not a
    // package manager.
    if provider == ProviderKind::Acp {
        return None;
    }
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
        (ProviderKind::Pi, InstallSource::Native) => Some(vec![s("pi"), s("update"), s("self")]),
        (ProviderKind::OpenCode, InstallSource::Native) => Some(vec![s("opencode"), s("upgrade")]),
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
    fn compares_tcode_release_versions() {
        assert_eq!(tcode_update_available("0.4.0", "v0.4.0"), Some(false));
        assert_eq!(tcode_update_available("0.4.0", "v0.4.1"), Some(true));
        assert_eq!(tcode_update_available("0.4.1", "v0.4.0"), Some(false));
    }

    #[test]
    fn handles_tcode_prereleases() {
        assert_eq!(
            tcode_update_available("0.4.0", "v0.5.0-beta.1"),
            Some(false)
        );
        assert_eq!(tcode_update_available("0.5.0-beta.1", "v0.5.0"), Some(true));
        assert_eq!(
            tcode_update_available("0.5.0-beta.1", "v0.6.0-beta.1"),
            Some(true)
        );
    }

    #[test]
    fn malformed_tcode_release_tag_has_no_comparison() {
        assert_eq!(tcode_update_available("0.4.0", "latest"), None);
        assert_eq!(tcode_update_available("0.4", "v0.4.1"), None);
    }

    #[test]
    fn parses_github_release_json() {
        let release = parse_tcode_release(
            br#"{
                "tag_name": "v0.4.1",
                "html_url": "https://github.com/Tryanks/tcode/releases/tag/v0.4.1",
                "prerelease": false,
                "assets": [{"name": "SHA256SUMS.txt"}]
            }"#,
        )
        .expect("release fixture should parse");

        assert_eq!(release.tag_name, "v0.4.1");
        assert_eq!(
            release.html_url,
            "https://github.com/Tryanks/tcode/releases/tag/v0.4.1"
        );
        assert!(!release.prerelease);
    }

    #[test]
    fn detects_install_source_from_path() {
        // Homebrew does not exist on Windows, where `detect_install_source`
        // deliberately never reports Brew (it would be an unrunnable update
        // command), so this expectation is unix-only.
        #[cfg(not(windows))]
        assert_eq!(
            detect_install_source(&PathBuf::from("/test/opt/homebrew/bin/codex")),
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
        let path = PathBuf::from("/test/opt/homebrew/bin/codex");
        let expected = if cfg!(windows) {
            InstallSource::Unknown
        } else {
            InstallSource::Brew
        };
        assert_eq!(detect_install_source(&path), expected);
    }

    #[cfg(unix)]
    fn temp_install_root() -> PathBuf {
        std::env::temp_dir().join(format!("tcode-version-check-{}", uuid::Uuid::new_v4()))
    }

    #[cfg(unix)]
    fn create_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(path.parent().expect("executable has a parent")).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn detects_npm_target_behind_homebrew_prefix_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_install_root();
        let prefix = root.join("opt/homebrew");
        let target = prefix.join("lib/node_modules/@openai/codex/bin/codex.js");
        let launcher = prefix.join("bin/codex");
        create_executable(&target);
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        symlink("../lib/node_modules/@openai/codex/bin/codex.js", &launcher).unwrap();

        let source = detect_install_source(&launcher);
        assert_eq!(source, InstallSource::Npm);
        assert_eq!(
            update_command(ProviderKind::Codex, source),
            Some(vec![
                "npm".into(),
                "install".into(),
                "-g".into(),
                "@openai/codex@latest".into(),
            ])
        );
        assert_eq!(
            update_command_string(ProviderKind::Codex, source).as_deref(),
            Some("npm install -g @openai/codex@latest")
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn genuine_homebrew_symlink_remains_brew() {
        use std::os::unix::fs::symlink;

        let root = temp_install_root();
        let prefix = root.join("opt/homebrew");
        let target = prefix.join("Cellar/codex/1.0.0/bin/codex");
        let launcher = prefix.join("bin/codex");
        create_executable(&target);
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        symlink("../Cellar/codex/1.0.0/bin/codex", &launcher).unwrap();

        assert_eq!(detect_install_source(&launcher), InstallSource::Brew);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlink_falls_back_to_supplied_path() {
        use std::os::unix::fs::symlink;

        let root = temp_install_root();
        let launcher = root.join("opt/homebrew/bin/codex");
        std::fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        symlink("../missing/codex", &launcher).unwrap();

        assert_eq!(detect_install_source(&launcher), InstallSource::Brew);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn existing_non_symlink_uses_its_own_path() {
        let root = temp_install_root();
        let executable = root.join("home/user/.local/bin/claude");
        create_executable(&executable);

        assert_eq!(detect_install_source(&executable), InstallSource::Native);

        std::fs::remove_dir_all(root).unwrap();
    }

    /// The exact command table from the T3 Providers spec (§3), for every
    /// detected source × provider pair we support.
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
            // Native Codex has no documented self-update subcommand.
            (Codex, Native, None),
            // An unrecognized path is manual-only in T3: no command at all.
            (Codex, Unknown, None),
            (ClaudeCode, Unknown, None),
            (Pi, Unknown, None),
            (OpenCode, Unknown, None),
            // ACP agents are updated through their marketplace entry.
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
