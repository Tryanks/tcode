use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent::ProviderKind;
use serde_json::Value;

use super::{InstallSource, npm_package};

#[derive(Debug, Clone)]
pub struct Installation {
    pub source: InstallSource,
    pub update: Option<UpdateCommand>,
    latest: LatestQuery,
}

#[derive(Debug, Clone)]
pub struct UpdateCommand {
    pub(super) program: PathBuf,
    pub(super) args: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) env: Vec<(String, String)>,
    pub requires_terminal: bool,
}

impl UpdateCommand {
    /// Render only the command and working directory, never provider secrets.
    pub fn display(&self) -> String {
        self.display_for_shell(cfg!(windows))
    }

    /// A compact label; copying uses `display` with the complete install context.
    pub fn summary(&self) -> String {
        std::iter::once(
            self.program
                .file_name()
                .unwrap_or(self.program.as_os_str())
                .to_string_lossy()
                .into_owned(),
        )
        .chain(self.args.iter().cloned())
        .map(|part| quote(&part, cfg!(windows)))
        .collect::<Vec<_>>()
        .join(" ")
    }

    fn display_for_shell(&self, windows: bool) -> String {
        let command = std::iter::once(self.program.to_string_lossy().into_owned())
            .chain(self.args.iter().cloned())
            .map(|part| quote(&part, windows))
            .collect::<Vec<_>>()
            .join(" ");
        let bindings = self
            .env
            .iter()
            .filter(|(key, _)| DISPLAY_ENV.contains(&key.as_str()))
            .map(|(key, value)| {
                if windows {
                    format!("$env:{key} = {}; ", quote(value, true))
                } else {
                    format!("{key}={} ", quote(value, false))
                }
            })
            .collect::<String>();
        if windows {
            format!(
                "Set-Location -LiteralPath {}; {bindings}& {command}",
                quote(&self.cwd.to_string_lossy(), true)
            )
        } else {
            format!(
                "cd {} && {bindings}{command}",
                quote(&self.cwd.to_string_lossy(), false)
            )
        }
    }

    pub async fn run(&self) -> bool {
        if self.requires_terminal {
            return false;
        }
        run_process(self, Duration::from_secs(300)).await.is_some()
    }
}

fn quote(value: &str, windows: bool) -> String {
    if windows {
        format!("'{}'", value.replace('\'', "''"))
    } else if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-/.:@=+".contains(c))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

// These values select installation roots or configuration scopes. Provider
// credentials and arbitrary environment values must never reach UI text.
const DISPLAY_ENV: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "PATH",
    "MISE_DATA_DIR",
    "MISE_CONFIG_DIR",
    "MISE_CONFIG_FILE",
    "MISE_GLOBAL_CONFIG_FILE",
    "MISE_SYSTEM_CONFIG_FILE",
    "MISE_ENV",
    "MISE_LOCKED",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "VOLTA_HOME",
    "ASDF_DATA_DIR",
    "PNPM_HOME",
    "BUN_INSTALL",
    "BUN_INSTALL_GLOBAL_DIR",
    "BUN_INSTALL_BIN",
    "SCOOP",
    "SCOOP_GLOBAL",
    "ChocolateyInstall",
    "NPM_CONFIG_PREFIX",
    "npm_config_prefix",
    "CLAUDE_CONFIG_DIR",
];

#[derive(Debug, Clone)]
pub(super) enum LatestQuery {
    None,
    Registry(String),
    Package {
        command: UpdateCommand,
        yarn: bool,
    },
    Claude {
        channel: String,
        maximum: Option<(u32, u32, u32)>,
    },
    Mise {
        command: UpdateCommand,
        tool: String,
        current: String,
    },
    Brew {
        command: UpdateCommand,
        target: String,
        cask: bool,
    },
    System(super::system_managers::Query),
}

pub(super) struct SystemProbe;

impl SystemProbe {
    pub(super) async fn output(&self, command: &UpdateCommand) -> Option<String> {
        run_process(command, Duration::from_secs(12)).await
    }

    pub(super) fn canonical(&self, path: &Path) -> Option<PathBuf> {
        path.canonicalize().ok()
    }

    pub(super) fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    pub(super) fn which(&self, name: &str, env: &[(String, String)]) -> Option<PathBuf> {
        let path = env_value(env, "PATH")?;
        let mut names = Vec::new();
        if cfg!(windows) && Path::new(name).extension().is_none() {
            names.extend(
                env_value(env, "PATHEXT")
                    .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
                    .split(';')
                    .map(|extension| format!("{name}{extension}")),
            );
        }
        names.push(name.to_string());
        std::env::split_paths(&path)
            .filter(|path| path.is_absolute())
            .find_map(|dir| {
                names.iter().map(|name| dir.join(name)).find(|path| {
                    let Ok(metadata) = std::fs::metadata(path) else {
                        return false;
                    };
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt as _;
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    }
                    #[cfg(not(unix))]
                    {
                        metadata.is_file()
                    }
                })
            })
    }
}

async fn run_process(command: &UpdateCommand, timeout: Duration) -> Option<String> {
    use smol::io::AsyncReadExt as _;

    let mut process = crate::process::async_command(&command.program);
    process
        .args(&command.args)
        .envs(command.env.iter().cloned())
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .current_dir(&command.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = process.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    smol::future::race(
        async {
            let mut output = Vec::new();
            stdout.read_to_end(&mut output).await.ok()?;
            child
                .status()
                .await
                .ok()?
                .success()
                .then(|| String::from_utf8(output).ok())
                .flatten()
        },
        async {
            smol::Timer::after(timeout).await;
            None
        },
    )
    .await
}

pub(super) fn env_value(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter()
        .rev()
        .find(|(name, _)| {
            if cfg!(windows) {
                name.eq_ignore_ascii_case(key)
            } else {
                name == key
            }
        })
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var(key).ok())
}

pub(super) struct Context<'a> {
    pub probe: &'a SystemProbe,
    pub provider: ProviderKind,
    pub binary: PathBuf,
    pub canonical: PathBuf,
    pub cwd: PathBuf,
    pub env: &'a [(String, String)],
}

impl Context<'_> {
    pub fn command(&self, name: &str, args: &[&str]) -> Option<UpdateCommand> {
        Some(self.at(self.probe.which(name, self.env)?, args))
    }

    pub fn at(&self, program: PathBuf, args: &[&str]) -> UpdateCommand {
        UpdateCommand {
            program,
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            cwd: self.cwd.clone(),
            env: self.env.to_vec(),
            requires_terminal: false,
        }
    }

    pub fn managed(&self, source: InstallSource, update: Option<UpdateCommand>) -> Installation {
        self.installation(source, update, LatestQuery::None)
    }

    pub fn installation(
        &self,
        source: InstallSource,
        update: Option<UpdateCommand>,
        latest: LatestQuery,
    ) -> Installation {
        Installation {
            source,
            update,
            latest,
        }
    }

    pub fn home(&self) -> Option<PathBuf> {
        env_value(self.env, if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
    }

    pub fn same(&self, first: &Path, second: &Path) -> bool {
        self.probe
            .canonical(first)
            .zip(self.probe.canonical(second))
            .is_some_and(|(first, second)| first == second)
    }
}

pub async fn resolve_installation(
    provider: ProviderKind,
    binary: &Path,
    env: &[(String, String)],
) -> Installation {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut env = env.to_vec();
    for key in DISPLAY_ENV {
        if !env.iter().any(|(name, _)| name == key)
            && let Ok(value) = std::env::var(key)
        {
            env.push(((*key).to_string(), value));
        }
    }
    resolve(&SystemProbe, provider, binary, &env, cwd).await
}

async fn resolve(
    probe: &SystemProbe,
    provider: ProviderKind,
    binary: &Path,
    env: &[(String, String)],
    cwd: PathBuf,
) -> Installation {
    let context = Context {
        probe,
        provider,
        binary: binary.to_path_buf(),
        canonical: probe
            .canonical(binary)
            .unwrap_or_else(|| binary.to_path_buf()),
        cwd,
        env,
    };
    if provider == ProviderKind::Acp || probe.canonical(binary).is_none() {
        return context.managed(InstallSource::Unknown, None);
    }
    if let Some(installation) = mise(&context).await {
        return installation;
    }
    if let Some(installation) = version_managers(&context).await {
        return installation;
    }
    if let Some(installation) = super::system_managers::resolve(&context).await {
        return installation;
    }
    if let Some(installation) = brew(&context).await {
        return installation;
    }
    if let Some(installation) = javascript(&context).await {
        return installation;
    }
    native(&context).unwrap_or_else(|| context.managed(InstallSource::Unknown, None))
}

/// Run on the host's blocking worker: release registries use synchronous HTTP.
pub fn latest_version(installation: &Installation) -> Option<String> {
    smol::block_on(latest(&SystemProbe, &installation.latest))
}

async fn latest(probe: &SystemProbe, query: &LatestQuery) -> Option<String> {
    match query {
        LatestQuery::None => None,
        LatestQuery::Registry(package) => {
            let url = format!("https://registry.npmjs.org/{package}/latest");
            let response = ureq::get(&url)
                .timeout(Duration::from_secs(10))
                .call()
                .ok()?;
            let mut bytes = Vec::new();
            response.into_reader().read_to_end(&mut bytes).ok()?;
            let json: Value = serde_json::from_slice(&bytes).ok()?;
            json.get("version")?.as_str().map(str::to_string)
        }
        LatestQuery::Package { command, yarn } => {
            package_latest(&probe.output(command).await?, *yarn)
        }
        LatestQuery::Claude { channel, maximum } => {
            let url = format!("https://downloads.claude.ai/claude-code-releases/{channel}");
            let response = ureq::get(&url)
                .timeout(Duration::from_secs(10))
                .call()
                .ok()?;
            let mut version = String::new();
            response.into_reader().read_to_string(&mut version).ok()?;
            let parsed = super::parse_version(&version)?;
            if maximum.is_some_and(|maximum| parsed > maximum) {
                return None;
            }
            Some(version.trim().to_string())
        }
        LatestQuery::Mise {
            command,
            tool,
            current,
        } => {
            let json: Value = serde_json::from_str(&probe.output(command).await?).ok()?;
            let entries = json.as_object()?;
            match entries.get(tool) {
                Some(entry) => entry.get("latest")?.as_str().map(str::to_string),
                None if entries.is_empty() => Some(current.clone()),
                None => None,
            }
        }
        LatestQuery::Brew {
            command,
            target,
            cask,
        } => {
            let json: Value = serde_json::from_str(&probe.output(command).await?).ok()?;
            brew_latest(&json, target, *cask)
        }
        LatestQuery::System(query) => super::system_managers::latest(probe, query).await,
    }
}

/// The version a package manager's `view`/`info <package>@latest version --json`
/// printed. npm 12.2.2 prints the one matching version as a one-element array
/// (`lib/commands/view.js`), and pnpm 10.34.9's `view` prints the same; bun
/// 1.4.4's `info` prints the bare string; yarn's `info --json` prints lines of
/// `{"type":"inspect","data":...}`.
fn package_latest(output: &str, yarn: bool) -> Option<String> {
    if yarn {
        return output
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|value| {
                value
                    .get("data")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
    }
    match serde_json::from_str::<Value>(output).ok()? {
        Value::String(version) => Some(version),
        Value::Array(versions) => match versions.as_slice() {
            [Value::String(version)] => Some(version.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn binary_name(provider: ProviderKind) -> &'static str {
    provider.harness().binary_name
}

fn within(context: &Context<'_>, path: &Path, root: &Path) -> bool {
    path.starts_with(root)
        || context
            .probe
            .canonical(root)
            .is_some_and(|root| path.starts_with(root))
}

async fn mise(context: &Context<'_>) -> Option<Installation> {
    let data = env_value(context.env, "MISE_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env_value(context.env, "XDG_DATA_HOME").map(|path| PathBuf::from(path).join("mise"))
        })
        .or_else(|| context.home().map(|home| home.join(".local/share/mise")));
    let shims = env_value(context.env, "MISE_SHIMS_DIR")
        .map(PathBuf::from)
        .or_else(|| data.as_ref().map(|data| data.join("shims")));
    let is_shim = shims
        .as_ref()
        .is_some_and(|root| context.binary.starts_with(root));
    let in_data = data
        .as_ref()
        .is_some_and(|root| within(context, &context.canonical, &root.join("installs")));
    let fallback = || (is_shim || in_data).then(|| context.managed(InstallSource::Mise, None));
    let Some(list) = context.command("mise", &["ls", "--json"]) else {
        return fallback();
    };
    let Some(json) = context
        .probe
        .output(&list)
        .await
        .and_then(|json| serde_json::from_str::<Value>(&json).ok())
    else {
        return fallback();
    };
    let Some(tools) = json.as_object() else {
        return fallback();
    };
    let which = context.at(
        list.program.clone(),
        &["which", binary_name(context.provider)],
    );
    let selected = context
        .probe
        .output(&which)
        .await
        .and_then(|path| context.probe.canonical(Path::new(path.trim())));
    let Some(selected) = selected else {
        return fallback();
    };
    if !is_shim && selected != context.canonical {
        return fallback();
    }
    for (tool, records) in tools {
        for record in records.as_array().into_iter().flatten() {
            let Some(root) = record.get("install_path").and_then(Value::as_str) else {
                continue;
            };
            if !within(context, &selected, Path::new(root)) {
                continue;
            }
            let metadata = context.at(list.program.clone(), &["tool", tool, "--json"]);
            let backend = context
                .probe
                .output(&metadata)
                .await
                .and_then(|output| serde_json::from_str::<Value>(&output).ok())
                .and_then(|value| {
                    value
                        .get("backend")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            let Some(backend) = backend else {
                return Some(context.managed(InstallSource::Mise, None));
            };
            if runtime_backend(&backend) {
                return if is_shim {
                    Some(context.managed(InstallSource::Mise, None))
                } else {
                    None
                };
            }
            if !provider_backend(context.provider, &backend) {
                return Some(context.managed(InstallSource::Mise, None));
            }
            if record.get("active").and_then(Value::as_bool) != Some(true)
                || record.get("installed").and_then(Value::as_bool) != Some(true)
            {
                return Some(context.managed(InstallSource::Mise, None));
            }
            let source = record
                .get("source")
                .and_then(|value| value.get("path"))
                .and_then(Value::as_str);
            let requested = record.get("requested_version").and_then(Value::as_str);
            let current = record.get("version").and_then(Value::as_str);
            let (Some(_), Some(requested), Some(current)) = (source, requested, current) else {
                return Some(context.managed(InstallSource::Mise, None));
            };
            // External path/ref aliases are not a release channel that can be upgraded safely.
            if requested.starts_with("path:") || requested.starts_with("ref:") {
                return Some(context.managed(InstallSource::Mise, None));
            }
            let update = context.at(list.program.clone(), &["upgrade", "--yes", "--", tool]);
            let query = context.at(list.program.clone(), &["outdated", "--json", "--", tool]);
            return Some(context.installation(
                InstallSource::Mise,
                Some(update),
                LatestQuery::Mise {
                    command: query,
                    tool: tool.clone(),
                    current: current.to_string(),
                },
            ));
        }
    }
    fallback()
}

fn runtime_backend(backend: &str) -> bool {
    matches!(
        backend,
        "core:node"
            | "core:bun"
            | "core:deno"
            | "asdf:nodejs"
            | "vfox:nodejs"
            | "aqua:oven-sh/bun"
            | "github:oven-sh/bun"
            | "npm:pnpm"
            | "npm:yarn"
    )
}

fn provider_backend(provider: ProviderKind, backend: &str) -> bool {
    if let Some(package) = backend.strip_prefix("npm:") {
        return recognized_package(provider, package);
    }
    let Some((kind, repository)) = backend.split_once(':') else {
        return false;
    };
    if !matches!(kind, "aqua" | "github" | "ubi") {
        return false;
    }
    matches!(
        (provider, repository),
        (ProviderKind::ClaudeCode, "anthropics/claude-code")
            | (ProviderKind::Codex, "openai/codex")
            | (ProviderKind::Pi, "earendil-works/pi")
            | (ProviderKind::OpenCode, "anomalyco/opencode")
    )
}

async fn version_managers(context: &Context<'_>) -> Option<Installation> {
    let home = context.home()?;
    let volta = env_value(context.env, "VOLTA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".volta"));
    if context.binary.starts_with(&volta) || within(context, &context.canonical, &volta) {
        let Some(which) = context.command("volta", &["which", binary_name(context.provider)])
        else {
            return Some(context.managed(InstallSource::Volta, None));
        };
        let actual = context
            .probe
            .output(&which)
            .await
            .and_then(|path| context.probe.canonical(Path::new(path.trim())));
        let shim = context.binary.parent() == Some(volta.join("bin").as_path());
        let Some(actual) = actual.filter(|actual| shim || *actual == context.canonical) else {
            return Some(context.managed(InstallSource::Volta, None));
        };
        if within(context, &actual, &volta.join("tools/image/packages"))
            && let Some(package) = package_for_binary(context, &actual)
        {
            let target = format!("{}@latest", package.name);
            let npm = context.at(which.program.clone(), &["which", "npm"]);
            let latest = if let Some(npm) = context
                .probe
                .output(&npm)
                .await
                .and_then(|path| context.probe.canonical(Path::new(path.trim())))
            {
                LatestQuery::Package {
                    command: context.at(npm, &["view", &target, "version", "--json"]),
                    yarn: false,
                }
            } else {
                LatestQuery::None
            };
            let update = context.at(which.program, &["install", &target]);
            return Some(context.installation(InstallSource::Volta, Some(update), latest));
        }
        return Some(context.managed(InstallSource::Volta, None));
    }
    let asdf = env_value(context.env, "ASDF_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".asdf"));
    if context.binary.starts_with(asdf.join("shims"))
        || within(context, &context.canonical, &asdf.join("installs"))
    {
        // asdf install does not activate a version; updating .tool-versions is a
        // separate policy decision, so never substitute a nested npm update.
        return Some(context.managed(InstallSource::Asdf, None));
    }
    None
}

/// The version `brew info --json=v2` offers `target`, unless the formula is
/// pinned: a formula's `versions.stable`, a cask's `version`.
fn brew_latest(info: &Value, target: &str, cask: bool) -> Option<String> {
    let record = info
        .get(if cask { "casks" } else { "formulae" })?
        .as_array()?
        .iter()
        .find(|record| brew_name(record, cask) == Some(target))?;
    if record.get("pinned").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if cask {
        record.get("version")?.as_str().map(str::to_string)
    } else {
        record
            .get("versions")?
            .get("stable")?
            .as_str()
            .map(str::to_string)
    }
}

fn brew_name(record: &Value, cask: bool) -> Option<&str> {
    record
        .get(if cask { "full_token" } else { "full_name" })
        .or_else(|| record.get(if cask { "token" } else { "name" }))?
        .as_str()
}

async fn brew(context: &Context<'_>) -> Option<Installation> {
    let info = context.command("brew", &["info", "--json=v2", "--installed"])?;
    let json: Value = serde_json::from_str(&context.probe.output(&info).await?).ok()?;
    let prefix_command = context.at(info.program.clone(), &["--prefix"]);
    let prefix = PathBuf::from(context.probe.output(&prefix_command).await?.trim());
    for cask in [false, true] {
        for record in json
            .get(if cask { "casks" } else { "formulae" })
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(target) = brew_name(record, cask) else {
                continue;
            };
            let owned_name = record
                .get(if cask { "token" } else { "name" })
                .and_then(Value::as_str)?;
            if !provider_brew_package(context.provider, owned_name) {
                continue;
            }
            let owns = if cask {
                let Some(token) = record.get("token").and_then(Value::as_str) else {
                    continue;
                };
                within(
                    context,
                    &context.canonical,
                    &prefix.join("Caskroom").join(token),
                ) && record
                    .get("installed")
                    .is_some_and(|installed| !installed.is_null())
            } else {
                let Some(name) = record.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let root = prefix.join("Cellar").join(name);
                record
                    .get("installed")
                    .and_then(Value::as_array)
                    .is_some_and(|versions| {
                        versions.iter().any(|version| {
                            version
                                .get("version")
                                .and_then(Value::as_str)
                                .is_some_and(|version| {
                                    within(context, &context.canonical, &root.join(version))
                                })
                        })
                    })
            };
            if !owns {
                continue;
            }
            let kind = if cask { "--cask" } else { "--formula" };
            let query = context.at(info.program.clone(), &["info", "--json=v2", kind, target]);
            let update = if record.get("pinned").and_then(Value::as_bool) == Some(true) {
                None
            } else {
                Some(context.at(info.program.clone(), &["upgrade", kind, "--", target]))
            };
            return Some(context.installation(
                InstallSource::Brew,
                update,
                LatestQuery::Brew {
                    command: query,
                    target: target.to_string(),
                    cask,
                },
            ));
        }
    }
    None
}

fn provider_brew_package(provider: ProviderKind, name: &str) -> bool {
    match provider {
        ProviderKind::ClaudeCode => matches!(name, "claude-code" | "claude-code@latest"),
        ProviderKind::Codex => matches!(name, "codex" | "codex@alpha"),
        ProviderKind::Pi => name == "pi-coding-agent",
        ProviderKind::OpenCode => matches!(name, "opencode" | "opencode-v2"),
        ProviderKind::Cursor | ProviderKind::Grok | ProviderKind::Acp => false,
    }
}

struct Package {
    name: String,
    root: PathBuf,
}

fn recognized_package(provider: ProviderKind, name: &str) -> bool {
    !name.is_empty() && name == npm_package(provider)
}

/// A node_modules component alone proves nothing. The owning manifest must
/// name this provider and its declared bin must resolve to the selected file.
fn package_for_binary(context: &Context<'_>, binary: &Path) -> Option<Package> {
    for root in binary.ancestors().skip(1) {
        let Some(json) = context.probe.read(&root.join("package.json")) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<Value>(&json) else {
            continue;
        };
        let Some(name) = manifest.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !recognized_package(context.provider, name) {
            continue;
        }
        let Some(bin) = manifest.get("bin") else {
            continue;
        };
        let path = bin.as_str().or_else(|| {
            bin.get(binary_name(context.provider))
                .and_then(Value::as_str)
        });
        if path.is_some_and(|path| context.same(&root.join(path), binary)) {
            return Some(Package {
                name: name.to_string(),
                root: root.to_path_buf(),
            });
        }
    }
    None
}

async fn javascript(context: &Context<'_>) -> Option<Installation> {
    let Some(package) = package_for_binary(context, &context.canonical) else {
        return javascript_windows(context).await;
    };
    // Each candidate must report the exact global package root. The existence
    // of another copy of the package in a manager's store is not ownership.
    for (manager, source, query_args) in [
        ("pnpm", InstallSource::Pnpm, vec!["root", "--global"]),
        ("yarn", InstallSource::Yarn, vec!["global", "dir"]),
        ("npm", InstallSource::Npm, vec!["root", "--global"]),
    ] {
        let Some(query) = context.command(manager, &query_args) else {
            continue;
        };
        let Some(root) = context.probe.output(&query).await else {
            continue;
        };
        let mut root = PathBuf::from(root.trim());
        if manager == "yarn" {
            root.push("node_modules");
        }
        if !context.same(&root.join(&package.name), &package.root) {
            continue;
        }
        if let Some(installation) = javascript_plan(
            context,
            source,
            query.program,
            &root,
            &package,
            cfg!(windows),
        )
        .await
        {
            return Some(installation);
        }
    }
    // Bun's global package and bin directories can be independently configured.
    // Verify its reported bin symlink and bind both locations explicitly.
    if let Some(query) = context.command("bun", &["pm", "bin", "--global"])
        && let Some(bin) = context.probe.output(&query).await
    {
        let bin = PathBuf::from(bin.trim());
        if context.same(&bin.join(binary_name(context.provider)), &context.canonical)
            && let Some(global_dir) = bun_global_directory(context, &package)
        {
            let target = format!("{}@latest", package.name);
            let mut update = context.at(query.program, &["add", "--global", "--", &target]);
            update.env.retain(|(name, _)| {
                !matches!(name.as_str(), "BUN_INSTALL_GLOBAL_DIR" | "BUN_INSTALL_BIN")
            });
            update.env.push((
                "BUN_INSTALL_GLOBAL_DIR".into(),
                global_dir.to_string_lossy().into_owned(),
            ));
            update
                .env
                .push(("BUN_INSTALL_BIN".into(), bin.to_string_lossy().into_owned()));
            let mut latest = update.clone();
            latest.args = vec!["info".into(), target, "version".into(), "--json".into()];
            return Some(context.installation(
                InstallSource::Bun,
                Some(update),
                LatestQuery::Package {
                    command: latest,
                    yarn: false,
                },
            ));
        }
    }
    None
}

fn bun_global_directory<'a>(context: &Context<'_>, package: &'a Package) -> Option<&'a Path> {
    package
        .root
        .ancestors()
        .filter(|root| root.file_name().is_some_and(|name| name == "node_modules"))
        .filter_map(Path::parent)
        .find(|directory| {
            // Isolated linking puts another node_modules inside the .bun store;
            // only the global project's manifest and direct link own the install.
            context
                .probe
                .read(&directory.join("package.json"))
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .is_some_and(|json| {
                    json.get("dependencies")
                        .is_some_and(|dependencies| dependencies.get(&package.name).is_some())
                })
                && context.same(
                    &directory.join("node_modules").join(&package.name),
                    &package.root,
                )
        })
}

async fn javascript_plan(
    context: &Context<'_>,
    source: InstallSource,
    program: PathBuf,
    root: &Path,
    package: &Package,
    windows: bool,
) -> Option<Installation> {
    let target = format!("{}@latest", package.name);
    let update = match source {
        InstallSource::Npm => {
            let prefix_query = context.at(program.clone(), &["prefix", "--global"]);
            let prefix = context.probe.output(&prefix_query).await?;
            let prefix = prefix.trim();
            let expected_root = if windows {
                PathBuf::from(prefix).join("node_modules")
            } else {
                PathBuf::from(prefix).join("lib/node_modules")
            };
            if !context.same(&expected_root, root) {
                return None;
            }
            context.at(
                program,
                &["install", "--global", "--prefix", prefix, "--", &target],
            )
        }
        InstallSource::Pnpm => {
            let bin_query = context.at(program.clone(), &["bin", "--global"]);
            let bin = context.probe.output(&bin_query).await?;
            let bin = PathBuf::from(bin.trim());
            // pnpm's global root has a layout version below global-dir.
            let global_dir = root.parent()?.parent()?;
            context.at(
                program,
                &[
                    "add",
                    "--global",
                    "--global-dir",
                    &global_dir.to_string_lossy(),
                    "--global-bin-dir",
                    &bin.to_string_lossy(),
                    "--",
                    &target,
                ],
            )
        }
        InstallSource::Yarn => {
            let dir = root.parent()?;
            let prefix_query = context.at(program.clone(), &["global", "bin"]);
            let bin = context.probe.output(&prefix_query).await?;
            let prefix = Path::new(bin.trim()).parent()?;
            context.at(
                program,
                &[
                    "global",
                    "add",
                    "--global-folder",
                    &dir.to_string_lossy(),
                    "--prefix",
                    &prefix.to_string_lossy(),
                    "--",
                    &target,
                ],
            )
        }
        _ => unreachable!(),
    };
    let mut query = update.clone();
    query.args = vec![
        if source == InstallSource::Yarn {
            "info"
        } else {
            "view"
        }
        .into(),
        target,
        "version".into(),
        "--json".into(),
    ];
    if source == InstallSource::Npm
        && let Some(prefix) = update
            .args
            .iter()
            .position(|arg| arg == "--prefix")
            .and_then(|index| update.args.get(index + 1))
    {
        query
            .args
            .extend(["--global".into(), "--prefix".into(), prefix.clone()]);
    }
    Some(context.installation(
        source,
        Some(update),
        LatestQuery::Package {
            command: query,
            yarn: source == InstallSource::Yarn,
        },
    ))
}

async fn javascript_windows(context: &Context<'_>) -> Option<Installation> {
    if !context
        .canonical
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd"))
    {
        return None;
    }
    let wrapper = context.probe.read(&context.canonical)?;
    let name = npm_package(context.provider);
    if name.is_empty() {
        return None;
    }
    for (manager, source, args, bin_args) in [
        (
            "pnpm",
            InstallSource::Pnpm,
            vec!["root", "--global"],
            vec!["bin", "--global"],
        ),
        (
            "yarn",
            InstallSource::Yarn,
            vec!["global", "dir"],
            vec!["global", "bin"],
        ),
        (
            "npm",
            InstallSource::Npm,
            vec!["root", "--global"],
            vec!["prefix", "--global"],
        ),
    ] {
        let Some(query) = context.command(manager, &args) else {
            continue;
        };
        let Some(root) = context.probe.output(&query).await else {
            continue;
        };
        let mut root = PathBuf::from(root.trim());
        if source == InstallSource::Yarn {
            root.push("node_modules");
        }
        let bin_query = context.at(query.program.clone(), &bin_args);
        let Some(bin) = context.probe.output(&bin_query).await else {
            continue;
        };
        let bin = PathBuf::from(bin.trim());
        if !context.same(context.canonical.parent()?, &bin) {
            continue;
        }
        let package_root = root.join(name);
        let Some(json) = context.probe.read(&package_root.join("package.json")) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<Value>(&json) else {
            continue;
        };
        if manifest.get("name").and_then(Value::as_str) != Some(name) {
            continue;
        }
        let Some(declared) = manifest.get("bin").and_then(|bin| {
            bin.as_str().or_else(|| {
                bin.get(binary_name(context.provider))
                    .and_then(Value::as_str)
            })
        }) else {
            continue;
        };
        let target = package_root.join(declared);
        if context.probe.canonical(&target).is_none() {
            continue;
        }
        let Some(relative) = windows_relative(&bin, &target) else {
            continue;
        };
        // A wrapper that merely mentions the bin could run anything.
        if !agent::CmdShim::parse(&wrapper).is_some_and(|shim| shim.target == relative) {
            continue;
        }
        let package = Package {
            name: name.to_string(),
            root: package_root,
        };
        if let Some(installation) = javascript_plan(
            context,
            source,
            query.program.clone(),
            &root,
            &package,
            true,
        )
        .await
        {
            return Some(installation);
        }
    }
    None
}

fn windows_relative(from: &Path, to: &Path) -> Option<String> {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let shared = from
        .iter()
        .zip(&to)
        .take_while(|(from, to)| from == to)
        .count();
    if shared == 0 {
        return None;
    }
    let relative = std::iter::repeat_n("..".to_string(), from.len() - shared)
        .chain(
            to[shared..]
                .iter()
                .map(|part| part.as_os_str().to_string_lossy().into_owned()),
        )
        .collect::<Vec<_>>()
        .join("\\");
    (!relative.contains(['%', '"', '\r', '\n'])).then_some(relative)
}

fn native(context: &Context<'_>) -> Option<Installation> {
    let home = context.home()?;
    match context.provider {
        ProviderKind::ClaudeCode
            if within(
                context,
                &context.canonical,
                &home.join(".local/share/claude/versions"),
            ) =>
        {
            if env_value(context.env, "DISABLE_UPDATES")
                .is_some_and(|value| value != "0" && !value.is_empty())
            {
                return Some(context.managed(InstallSource::Native, None));
            }
            Some(context.installation(
                InstallSource::Native,
                Some(context.at(context.binary.clone(), &["update"])),
                claude_channel(context, &home).unwrap_or(LatestQuery::None),
            ))
        }
        ProviderKind::OpenCode if context.canonical == home.join(".opencode/bin/opencode") => {
            Some(context.installation(
                InstallSource::Native,
                Some(context.at(context.binary.clone(), &["upgrade", "--method", "curl"])),
                LatestQuery::Registry(npm_package(ProviderKind::OpenCode).into()),
            ))
        }
        _ => None,
    }
}

fn claude_channel(context: &Context<'_>, home: &Path) -> Option<LatestQuery> {
    let config = env_value(context.env, "CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let managed = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    } else if cfg!(windows) {
        PathBuf::from(env_value(context.env, "ProgramFiles")?)
            .join("ClaudeCode/managed-settings.json")
    } else {
        PathBuf::from("/etc/claude-code/managed-settings.json")
    };
    let mut channel = "latest".to_string();
    let mut maximum = None;
    for path in [
        config.join("settings.json"),
        context.cwd.join(".claude/settings.json"),
        context.cwd.join(".claude/settings.local.json"),
        managed,
    ] {
        let Some(raw) = context.probe.read(&path) else {
            continue;
        };
        let json: Value = serde_json::from_str(&raw).ok()?;
        if let Some(value) = json.get("autoUpdatesChannel") {
            channel = value.as_str()?.to_string();
            if !matches!(channel.as_str(), "stable" | "latest") {
                return None;
            }
        }
        if let Some(value) = json.get("requiredMaximumVersion") {
            maximum = Some(super::parse_version(value.as_str()?)?);
        }
    }
    Some(LatestQuery::Claude { channel, maximum })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// npm 12.2.2 and pnpm 10.34.9 answer `view <package>@latest version
    /// --json` with a one-element array, bun 1.4.4's `info` with the bare
    /// string; the version is invented.
    /// `brew info --json=v2` as Homebrew 7.0.13 builds it (`cmd/info.rb`
    /// wraps `Formula#to_hash` and `Cask#to_h`); names and versions invented.
    #[test]
    fn brew_offers_a_formulas_stable_or_a_casks_version() {
        let info = serde_json::json!({
            "formulae": [{
                "name": "codex",
                "full_name": "codex",
                "tap": "homebrew/core",
                "versions": {"stable": "9.8.7", "head": null, "bottle": true},
                "installed": [{"version": "9.8.6", "installed_on_request": true}],
                "pinned": false
            }],
            "casks": [{
                "token": "claude-code",
                "full_token": "claude-code",
                "tap": "homebrew/cask",
                "version": "6.5.4",
                "installed": "6.5.3"
            }]
        });
        assert_eq!(brew_latest(&info, "codex", false).as_deref(), Some("9.8.7"));
        assert_eq!(
            brew_latest(&info, "claude-code", true).as_deref(),
            Some("6.5.4")
        );
    }

    #[test]
    fn a_package_managers_latest_version_is_read_in_its_own_shape() {
        assert_eq!(
            package_latest("[\n  \"9.8.7\"\n]\n", false).as_deref(),
            Some("9.8.7")
        );
        assert_eq!(
            package_latest("\"9.8.7\"\n", false).as_deref(),
            Some("9.8.7")
        );
    }

    #[test]
    fn display_quotes_paths_and_preserves_ownership_context_without_secrets() {
        let command = UpdateCommand {
            program: "/tools/owner's mise".into(),
            args: vec!["upgrade".into(), "npm:@openai/codex".into()],
            cwd: "/work/a b".into(),
            env: vec![
                ("MISE_DATA_DIR".into(), "/data/a b".into()),
                ("ANTHROPIC_API_KEY".into(), "private-secret".into()),
            ],
            requires_terminal: false,
        };
        assert_eq!(
            command.display_for_shell(false),
            "cd '/work/a b' && MISE_DATA_DIR='/data/a b' '/tools/owner'\\''s mise' upgrade npm:@openai/codex"
        );
        assert_eq!(
            command.display_for_shell(true),
            "Set-Location -LiteralPath '/work/a b'; $env:MISE_DATA_DIR = '/data/a b'; & '/tools/owner''s mise' 'upgrade' 'npm:@openai/codex'"
        );
    }
}
