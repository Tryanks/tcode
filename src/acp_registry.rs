//! The ACP agent registry (the marketplace's data layer).
//!
//! The official index lives at `cdn.agentclientprotocol.com` and lists every
//! agent that speaks the Agent Client Protocol, with a launch recipe per
//! platform. We mirror zed: fetch it, cache it in the app data dir with a
//! one-hour TTL, and fall back to the cache (however stale) when offline.
//!
//! Two agents are never surfaced: `claude-acp` and `codex-acp` are ACP adapters
//! over the very CLIs tcode already drives natively (with steering, structured
//! questions and richer tool payloads that ACP cannot express), so showing them
//! would only offer users a worse version of what they already have. The filter
//! is [`agent::HIDDEN_ACP_AGENT_IDS`] and it is enforced in [`visible_agents`].

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::{AcpLaunch, HIDDEN_ACP_AGENT_IDS};
use serde::{Deserialize, Serialize};

/// The official index (the same URL zed fetches).
pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// How long a cached index stays fresh.
pub const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// Where downloaded agents are installed, under the app data dir.
const INSTALL_DIR: &str = "acp-agents";

const CACHE_FILE: &str = "acp-registry.json";

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("could not reach the ACP registry: {0}")]
    Network(String),
    #[error("the ACP registry returned something unreadable: {0}")]
    Parse(String),
    #[error("{0}")]
    Install(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub agents: Vec<RegistryAgent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryAgent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    /// Icon URL (an SVG on the same CDN).
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub distribution: Distribution,
}

/// The launch recipes an agent publishes. `uvx` exists in the index but we (like
/// zed) do not run Python agents, so it is not deserialized.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Distribution {
    #[serde(default)]
    pub npx: Option<NpxDistribution>,
    /// Keyed by `{darwin,linux,windows}-{aarch64,x86_64}`.
    #[serde(default)]
    pub binary: BTreeMap<String, BinaryDistribution>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NpxDistribution {
    /// `name@version`, passed to `npm exec`.
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryDistribution {
    /// Download URL: a `.tar.gz` / `.tar.bz2` / `.zip`, or a bare executable.
    pub archive: String,
    /// The command inside the extracted tree (`./bin/agent`), or `node`.
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// The cache envelope: the index plus when we fetched it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedRegistry {
    fetched_at: u64,
    registry: Registry,
}

// ---------------------------------------------------------------------------
// Visibility + platform resolution
// ---------------------------------------------------------------------------

/// The agents the marketplace may show: everything except the two adapters over
/// our own native CLIs.
pub fn visible_agents(registry: &Registry) -> Vec<&RegistryAgent> {
    registry
        .agents
        .iter()
        .filter(|agent| !HIDDEN_ACP_AGENT_IDS.contains(&agent.id.as_str()))
        .collect()
}

/// The registry's platform key for the host we are running on.
pub fn platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "windows",
        other => other, // "linux"
    };
    // `x86_64` / `aarch64` are already the registry's own spellings.
    let arch = std::env::consts::ARCH;
    format!("{os}-{arch}")
}

/// How an agent will be launched on this platform once installed.
#[derive(Debug, Clone, PartialEq)]
pub enum Recipe {
    /// No install step: npm resolves the package at launch.
    Npx {
        package: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// Needs a download + extract first (see [`install`]).
    Binary(BinaryDistribution),
}

/// Resolve the recipe for `platform`. A binary distribution wins whenever this
/// platform has one (native start-up beats an `npm exec` round-trip); npx is the
/// fallback. `None` = this agent cannot run here.
pub fn resolve_recipe(agent: &RegistryAgent, platform: &str) -> Option<Recipe> {
    if let Some(binary) = agent.distribution.binary.get(platform) {
        return Some(Recipe::Binary(binary.clone()));
    }
    agent.distribution.npx.as_ref().map(|npx| Recipe::Npx {
        package: npx.package.clone(),
        args: npx.args.clone(),
        env: pairs(&npx.env),
    })
}

fn pairs(env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    env.iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Fetch + cache
// ---------------------------------------------------------------------------

fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CACHE_FILE)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// The cached index, whatever its age (`None` when there is no readable cache).
pub fn cached(data_dir: &Path) -> Option<Registry> {
    let bytes = std::fs::read(cache_path(data_dir)).ok()?;
    serde_json::from_slice::<CachedRegistry>(&bytes)
        .ok()
        .map(|cached| cached.registry)
}

/// Whether the cache is younger than [`CACHE_TTL`].
pub fn cache_is_fresh(data_dir: &Path) -> bool {
    let Ok(bytes) = std::fs::read(cache_path(data_dir)) else {
        return false;
    };
    let Ok(cached) = serde_json::from_slice::<CachedRegistry>(&bytes) else {
        return false;
    };
    now_secs().saturating_sub(cached.fetched_at) < CACHE_TTL.as_secs()
}

/// The registry, served from cache while fresh and re-fetched otherwise.
///
/// Offline with a stale cache still yields the cache: a marketplace that
/// remembers what it saw last week is strictly better than an empty one.
/// Blocking — call it from `smol::unblock`.
pub fn load(data_dir: &Path) -> Result<Registry, RegistryError> {
    if cache_is_fresh(data_dir)
        && let Some(registry) = cached(data_dir)
    {
        return Ok(registry);
    }
    match fetch() {
        Ok(registry) => {
            if let Err(err) = write_cache(data_dir, &registry) {
                log::warn!("could not cache the ACP registry: {err}");
            }
            Ok(registry)
        }
        Err(err) => match cached(data_dir) {
            Some(registry) => {
                log::warn!("serving the cached ACP registry: {err}");
                Ok(registry)
            }
            None => Err(err),
        },
    }
}

fn write_cache(data_dir: &Path, registry: &Registry) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let cached = CachedRegistry {
        fetched_at: now_secs(),
        registry: registry.clone(),
    };
    let tmp = cache_path(data_dir).with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&cached)?)?;
    std::fs::rename(tmp, cache_path(data_dir))
}

/// Fetch and parse the index. Blocking.
pub fn fetch() -> Result<Registry, RegistryError> {
    let body = http_get(REGISTRY_URL)?;
    parse(&body)
}

pub fn parse(bytes: &[u8]) -> Result<Registry, RegistryError> {
    serde_json::from_slice(bytes).map_err(|err| RegistryError::Parse(err.to_string()))
}

fn http_get(url: &str) -> Result<Vec<u8>, RegistryError> {
    let response = ureq::get(url)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|err| RegistryError::Network(err.to_string()))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|err| RegistryError::Network(err.to_string()))?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// An installed agent, as persisted in settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstalledAgent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub icon: Option<String>,
    /// The resolved recipe: exactly what we will spawn.
    pub launch: AcpLaunch,
    /// SHA-256 of the downloaded archive, for binary distributions.
    ///
    /// The registry publishes no digests (there is no `sha256` field in the
    /// schema — zed does not verify one either), so this is what we computed at
    /// install time rather than a checked-against-upstream signature. It lets us
    /// tell whether a re-download changed, which is the most integrity the index
    /// currently affords.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Extra environment for this agent's process (Settings → Providers).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Extra CLI arguments appended at launch.
    #[serde(default)]
    pub launch_args: Option<String>,
}

fn default_true() -> bool {
    true
}

impl InstalledAgent {
    /// The whitespace-split launch arguments (mirrors Claude's "Launch arguments").
    pub fn extra_args(&self) -> Vec<String> {
        self.launch_args
            .as_deref()
            .map(|args| args.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

/// Where an agent's files live: `<data>/acp-agents/<id>/<version>/`.
pub fn install_dir(data_dir: &Path, id: &str, version: &str) -> PathBuf {
    data_dir
        .join(INSTALL_DIR)
        .join(id)
        .join(if version.is_empty() {
            "latest"
        } else {
            version
        })
}

/// Install `agent` for this platform, reporting progress in bytes.
///
/// npx recipes install nothing (npm resolves the package on first launch);
/// binary recipes are downloaded, hashed, extracted, and made executable.
/// Blocking — call it from `smol::unblock`.
pub fn install(
    agent: &RegistryAgent,
    data_dir: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<InstalledAgent, RegistryError> {
    let recipe = resolve_recipe(agent, &platform_key()).ok_or_else(|| {
        RegistryError::Install(format!(
            "{} publishes no build for {}",
            agent.name,
            platform_key()
        ))
    })?;

    let (launch, archive_sha256) = match recipe {
        Recipe::Npx { package, args, env } => (AcpLaunch::Npx { package, args, env }, None),
        Recipe::Binary(binary) => {
            let dir = install_dir(data_dir, &agent.id, &agent.version);
            // A fresh install every time: a half-extracted tree from a failed
            // attempt must not be mistaken for a good one.
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            let bytes = download(&binary.archive, &mut progress)?;
            let sha = sha256(&bytes);
            extract(&binary.archive, &bytes, &dir)?;
            let command = resolve_cmd(&dir, &binary.cmd)?;
            let launch = match command {
                Command::Path(path) => AcpLaunch::Binary {
                    command: path,
                    args: binary.args.clone(),
                    env: pairs(&binary.env),
                },
                // `node ./dist/index.js` style: node comes from PATH.
                Command::OnPath(name) => AcpLaunch::Custom {
                    command: name,
                    args: binary.args.clone(),
                    env: pairs(&binary.env),
                },
            };
            (launch, Some(sha))
        }
    };

    Ok(InstalledAgent {
        id: agent.id.clone(),
        name: agent.name.clone(),
        version: agent.version.clone(),
        icon: agent.icon.clone(),
        launch,
        archive_sha256,
        enabled: true,
        env: Vec::new(),
        launch_args: None,
    })
}

/// Remove an installed agent's files (a no-op for npx recipes).
pub fn uninstall(data_dir: &Path, id: &str) -> std::io::Result<()> {
    let dir = data_dir.join(INSTALL_DIR).join(id);
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

fn download(
    url: &str,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<Vec<u8>, RegistryError> {
    let response = ureq::get(url)
        .timeout(Duration::from_secs(600))
        .call()
        .map_err(|err| RegistryError::Network(err.to_string()))?;
    let total: Option<u64> = response
        .header("Content-Length")
        .and_then(|len| len.parse().ok());
    let mut reader = response.into_reader().take(1024 * 1024 * 1024);
    let mut bytes = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buf)
            .map_err(|err| RegistryError::Network(err.to_string()))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..read]);
        progress(bytes.len() as u64, total);
    }
    Ok(bytes)
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Unpack an archive into `dir`, by the URL's extension. A URL that is not an
/// archive at all (some agents publish a bare executable) is written as the
/// binary itself.
fn extract(url: &str, bytes: &[u8], dir: &Path) -> Result<(), RegistryError> {
    let name = url
        .split('?')
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or("archive");
    let lower = name.to_ascii_lowercase();
    let unpack =
        |err: std::io::Error| RegistryError::Install(format!("could not unpack {name}: {err}"));

    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let decoder = flate2::read::GzDecoder::new(bytes);
        tar::Archive::new(decoder).unpack(dir).map_err(unpack)?;
    } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") {
        let decoder = bzip2::read::BzDecoder::new(bytes);
        tar::Archive::new(decoder).unpack(dir).map_err(unpack)?;
    } else if lower.ends_with(".tar") {
        tar::Archive::new(bytes).unpack(dir).map_err(unpack)?;
    } else if lower.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|err| RegistryError::Install(format!("could not read {name}: {err}")))?;
        zip.extract(dir)
            .map_err(|err| RegistryError::Install(format!("could not unpack {name}: {err}")))?;
    } else {
        // A bare executable (`sigit-linux-amd64`, `agent.exe`, …).
        let path = dir.join(name);
        std::fs::write(&path, bytes)?;
        make_executable(&path)?;
    }
    Ok(())
}

enum Command {
    /// An absolute path inside the extracted tree.
    Path(PathBuf),
    /// A bare program name resolved from PATH at launch (`node`).
    OnPath(String),
}

/// Resolve the recipe's `cmd` against the extracted tree.
///
/// The registry's contract (and zed's) is that `cmd` is either a `./`-relative
/// path inside the archive or the literal `node`; anything else would let the
/// index run an arbitrary program off the user's machine, so it is refused.
fn resolve_cmd(dir: &Path, cmd: &str) -> Result<Command, RegistryError> {
    if cmd == "node" {
        return Ok(Command::OnPath("node".to_string()));
    }
    let relative = cmd.strip_prefix("./").unwrap_or(cmd);
    if relative.starts_with('/') || relative.contains("..") {
        return Err(RegistryError::Install(format!(
            "the registry recipe wants to run `{cmd}`, which is not inside the downloaded archive"
        )));
    }
    let path = dir.join(relative);
    // A bare-executable download keeps the archive's file name, which need not
    // match `cmd`; fall back to the single file we wrote.
    let path = if path.exists() {
        path
    } else {
        let mut entries = std::fs::read_dir(dir)?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        if entries.len() == 1 {
            entries.remove(0)
        } else {
            return Err(RegistryError::Install(format!(
                "`{cmd}` is missing from the downloaded archive"
            )));
        }
    };
    make_executable(&path)?;
    Ok(Command::Path(path))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A slice of the real index (fetched from the CDN), including both hidden
    /// adapters and both distribution shapes.
    const SAMPLE: &str = r#"{
      "version": "1.0.0",
      "agents": [
        {
          "id": "gemini", "name": "Gemini CLI", "version": "0.50.0",
          "description": "Google's Gemini CLI",
          "repository": "https://github.com/google-gemini/gemini-cli",
          "license": "Apache-2.0",
          "icon": "https://cdn.agentclientprotocol.com/registry/v1/latest/gemini.svg",
          "distribution": { "npx": { "package": "@google/gemini-cli@0.50.0", "args": ["--acp"] } }
        },
        {
          "id": "claude-acp", "name": "Claude Agent", "version": "0.58.1",
          "description": "Adapter over Claude Code",
          "distribution": { "npx": { "package": "@agentclientprotocol/claude-agent-acp@0.58.1" } }
        },
        {
          "id": "codex-acp", "name": "Codex", "version": "1.1.2",
          "description": "Adapter over the Codex CLI",
          "distribution": { "npx": { "package": "@agentclientprotocol/codex-acp@1.1.2" } }
        },
        {
          "id": "goose", "name": "goose", "version": "1.14.0", "description": "Block's goose",
          "distribution": {
            "binary": {
              "darwin-aarch64": {
                "archive": "https://example.test/goose-aarch64-apple-darwin.tar.bz2",
                "cmd": "./goose", "args": ["acp"], "env": { "GOOSE_ACP": "1" }
              },
              "linux-x86_64": {
                "archive": "https://example.test/goose-x86_64-unknown-linux.tar.gz",
                "cmd": "./goose", "args": ["acp"]
              }
            }
          }
        },
        {
          "id": "kilo", "name": "Kilo", "version": "7.4.5", "description": "Both shapes",
          "distribution": {
            "npx": { "package": "@kilocode/cli@7.4.5", "args": ["acp"] },
            "binary": {
              "darwin-aarch64": { "archive": "https://example.test/kilo-darwin-arm64.zip", "cmd": "./kilo", "args": ["acp"] }
            }
          },
          "uvx": { "package": "ignored==1.0" }
        }
      ]
    }"#;

    fn registry() -> Registry {
        parse(SAMPLE.as_bytes()).expect("the sample index must parse")
    }

    /// The real thing, against the live CDN: fetch the index, then download +
    /// extract a binary distribution for this platform and check the resolved
    /// command actually exists and is executable. Network-bound, so it is
    /// `#[ignore]`d by default:
    /// `cargo test --bin tcode -- --ignored installs_a_real_agent`
    #[test]
    #[ignore = "hits the network"]
    fn installs_a_real_agent_from_the_live_registry() {
        let registry = fetch().expect("the live registry must be reachable");
        assert!(registry.agents.len() > 20, "the live index looks truncated");

        let platform = platform_key();
        let agent = visible_agents(&registry)
            .into_iter()
            .find(|agent| agent.distribution.binary.contains_key(&platform))
            .expect("some visible agent must ship a binary for this platform")
            .clone();

        let dir = std::env::temp_dir().join(format!("tcode-acp-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let installed = install(&agent, &dir, |_, _| {}).expect("install must succeed");
        assert_eq!(installed.id, agent.id);
        assert_eq!(installed.archive_sha256.as_ref().map(String::len), Some(64));
        match &installed.launch {
            AcpLaunch::Binary { command, .. } => {
                assert!(command.exists(), "{} was not extracted", command.display());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = std::fs::metadata(command).unwrap().permissions().mode();
                    assert!(mode & 0o111 != 0, "the agent binary must be executable");
                }
            }
            AcpLaunch::Custom { command, .. } => assert_eq!(command, "node"),
            other => panic!("expected a binary install, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_the_registry_schema() {
        let registry = registry();
        assert_eq!(registry.version, "1.0.0");
        assert_eq!(registry.agents.len(), 5);

        let gemini = &registry.agents[0];
        assert_eq!(gemini.name, "Gemini CLI");
        assert_eq!(gemini.version, "0.50.0");
        assert_eq!(gemini.license.as_deref(), Some("Apache-2.0"));
        assert!(gemini.icon.as_deref().unwrap().ends_with("gemini.svg"));
        let npx = gemini.distribution.npx.as_ref().unwrap();
        assert_eq!(npx.package, "@google/gemini-cli@0.50.0");
        assert_eq!(npx.args, vec!["--acp".to_string()]);

        let goose = &registry.agents[3];
        let binary = goose.distribution.binary.get("darwin-aarch64").unwrap();
        assert_eq!(binary.cmd, "./goose");
        assert_eq!(binary.env.get("GOOSE_ACP").map(String::as_str), Some("1"));
        assert!(goose.distribution.npx.is_none());
    }

    /// The load-bearing invariant: the two adapters over our own native CLIs are
    /// never offered in the marketplace.
    #[test]
    fn the_native_cli_adapters_are_never_visible() {
        let registry = registry();
        let ids: Vec<&str> = visible_agents(&registry)
            .iter()
            .map(|agent| agent.id.as_str())
            .collect();
        assert_eq!(ids, vec!["gemini", "goose", "kilo"]);
        for hidden in HIDDEN_ACP_AGENT_IDS {
            assert!(
                !ids.contains(&hidden),
                "`{hidden}` is an adapter over a natively-integrated CLI and must stay hidden"
            );
            assert!(
                registry.agents.iter().any(|agent| agent.id == hidden),
                "the fixture must actually contain `{hidden}`, or this test proves nothing"
            );
        }
    }

    #[test]
    fn a_binary_distribution_wins_over_npx_on_a_supported_platform() {
        let registry = registry();
        let kilo = &registry.agents[4];
        match resolve_recipe(kilo, "darwin-aarch64") {
            Some(Recipe::Binary(binary)) => {
                assert!(binary.archive.ends_with("kilo-darwin-arm64.zip"));
                assert_eq!(binary.args, vec!["acp".to_string()]);
            }
            other => panic!("expected the binary recipe, got {other:?}"),
        }
        // …but a platform with no binary build falls back to npx.
        match resolve_recipe(kilo, "linux-aarch64") {
            Some(Recipe::Npx { package, args, .. }) => {
                assert_eq!(package, "@kilocode/cli@7.4.5");
                assert_eq!(args, vec!["acp".to_string()]);
            }
            other => panic!("expected the npx recipe, got {other:?}"),
        }
    }

    #[test]
    fn an_agent_with_no_build_for_this_platform_does_not_resolve() {
        let registry = registry();
        let goose = &registry.agents[3];
        assert!(resolve_recipe(goose, "windows-aarch64").is_none());
        assert!(matches!(
            resolve_recipe(goose, "linux-x86_64"),
            Some(Recipe::Binary(_))
        ));
    }

    #[test]
    fn npx_recipes_carry_their_env() {
        let agent: RegistryAgent = serde_json::from_str(
            r#"{ "id": "x", "name": "X", "version": "1",
                 "distribution": { "npx": { "package": "x@1", "env": { "X_ACP": "1" } } } }"#,
        )
        .unwrap();
        match resolve_recipe(&agent, "linux-x86_64") {
            Some(Recipe::Npx { env, .. }) => {
                assert_eq!(env, vec![("X_ACP".to_string(), "1".to_string())]);
            }
            other => panic!("expected an npx recipe, got {other:?}"),
        }
    }

    #[test]
    fn the_platform_key_is_a_registry_key() {
        let key = platform_key();
        assert!(
            ["darwin", "linux", "windows"].contains(&key.split('-').next().unwrap()),
            "{key}"
        );
        assert!(key.contains('-'), "{key}");
    }

    /// `cmd` may only point inside the archive (or be `node`): the index must
    /// not be able to make us run an arbitrary program from the user's disk.
    #[test]
    fn cmd_may_not_escape_the_archive() {
        let dir = std::env::temp_dir().join(format!("tcode-acp-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(resolve_cmd(&dir, "/bin/sh").is_err());
        assert!(resolve_cmd(&dir, "../../../bin/sh").is_err());
        assert!(matches!(
            resolve_cmd(&dir, "node"),
            Ok(Command::OnPath(name)) if name == "node"
        ));

        std::fs::write(dir.join("agent"), "#!/bin/sh\n").unwrap();
        match resolve_cmd(&dir, "./agent") {
            Ok(Command::Path(path)) => assert_eq!(path, dir.join("agent")),
            other => panic!(
                "expected a path inside the archive, got {:?}",
                other.is_err()
            ),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A tar.gz install end to end (no network): extract, resolve, make runnable.
    #[test]
    fn a_tar_gz_archive_extracts_and_resolves_its_command() {
        let dir = std::env::temp_dir().join(format!("tcode-acp-tar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Build a tar.gz holding `./bin/agent`.
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let script = b"#!/bin/sh\necho hi\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "bin/agent", &script[..])
            .unwrap();
        let bytes = tar.into_inner().unwrap().finish().unwrap();

        extract("https://example.test/agent-1.0.tar.gz", &bytes, &dir).unwrap();
        assert!(dir.join("bin/agent").exists());
        match resolve_cmd(&dir, "./bin/agent") {
            Ok(Command::Path(path)) => {
                assert_eq!(path, dir.join("bin/agent"));
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                    assert!(mode & 0o111 != 0, "the agent binary must be executable");
                }
            }
            other => panic!("expected the extracted binary, got {:?}", other.is_err()),
        }
        assert_eq!(sha256(&bytes).len(), 64);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stale_cache_still_serves_when_the_network_is_gone() {
        let dir = std::env::temp_dir().join(format!("tcode-acp-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(cached(&dir).is_none());
        assert!(!cache_is_fresh(&dir));

        write_cache(&dir, &registry()).unwrap();
        assert!(cache_is_fresh(&dir));
        assert_eq!(cached(&dir).unwrap().agents.len(), 5);

        // Age it past the TTL: still readable, no longer fresh.
        let path = cache_path(&dir);
        let mut stale: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        stale["fetched_at"] = serde_json::json!(now_secs() - CACHE_TTL.as_secs() - 1);
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(!cache_is_fresh(&dir));
        assert_eq!(cached(&dir).unwrap().agents.len(), 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn installed_agents_split_their_launch_args() {
        let installed = InstalledAgent {
            id: "gemini".into(),
            name: "Gemini".into(),
            version: "1".into(),
            icon: None,
            launch: AcpLaunch::Npx {
                package: "x@1".into(),
                args: Vec::new(),
                env: Vec::new(),
            },
            archive_sha256: None,
            enabled: true,
            env: Vec::new(),
            launch_args: Some("  --debug   --yolo ".into()),
        };
        assert_eq!(installed.extra_args(), vec!["--debug", "--yolo"]);
    }
}
