use std::path::{Path, PathBuf};

use serde_json::Value;

use super::InstallSource;
use super::installation::{
    Context, Installation, LatestQuery, SystemProbe, UpdateCommand, env_value,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Query {
    Apt(UpdateCommand),
    Apk {
        command: UpdateCommand,
        package: String,
    },
    ScoopManifest(PathBuf),
}

pub(super) async fn latest(probe: &SystemProbe, query: &Query) -> Option<String> {
    match query {
        Query::Apt(command) => {
            let output = probe.output(command).await?;
            let candidate = output
                .lines()
                .find_map(|line| line.trim().strip_prefix("Candidate:"))?
                .trim();
            if candidate == "(none)" || candidate.split_whitespace().count() != 1 {
                return None;
            }
            // The provider reports its upstream version, without Debian's epoch.
            Some(
                candidate
                    .split_once(':')
                    .map_or(candidate, |(_, version)| version)
                    .to_string(),
            )
        }
        Query::Apk { command, package } => {
            let world = probe.read(Path::new("/etc/apk/world"))?;
            if world.lines().any(|line| {
                let line = line.split('#').next().unwrap_or_default().trim();
                line.split(['@', '=', '<', '>', '~']).next() == Some(package.as_str())
                    && line.contains(['=', '<', '>', '~'])
            }) {
                return None;
            }
            let output = probe.output(command).await?;
            output.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                let installed = fields
                    .next()?
                    .strip_prefix(package.as_str())?
                    .strip_prefix('-')?;
                if !installed.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                    return None;
                }
                if !matches!(fields.next()?, "<" | "=" | ">") {
                    return None;
                }
                let available = fields.next()?;
                available
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_digit)
                    .then(|| available.to_string())
            })
        }
        Query::ScoopManifest(path) => {
            let manifest: Value = serde_json::from_str(&probe.read(path)?).ok()?;
            manifest.get("version")?.as_str().map(str::to_string)
        }
    }
}

pub(super) async fn resolve(ctx: &Context<'_>) -> Option<Installation> {
    if ctx.canonical.starts_with("/nix/store") {
        // A store path does not identify the profile, flake, or system configuration
        // that must be changed. Self-updating its contents would bypass all of them.
        return Some(ctx.managed(InstallSource::Nix, None));
    }
    if cfg!(target_os = "linux") {
        return linux(ctx).await;
    }
    if cfg!(windows) {
        return windows(ctx).await;
    }
    None
}

async fn linux(ctx: &Context<'_>) -> Option<Installation> {
    for path in [&ctx.canonical, &ctx.binary] {
        let path = path.to_str()?;
        if let Some(query) = ctx.command("dpkg-query", &["--search", "--", path])
            && let Some(output) = ctx.probe.output(&query).await
            && let Some(package) = debian_owner(&output, path)
        {
            // APT's policy candidate does not account for dpkg selections. An
            // explicit install can override a hold, so require an unheld install.
            let selection = ctx.command(
                "dpkg-query",
                &[
                    "--show",
                    "--showformat",
                    "${db:Status-Want} ${db:Status-Status}\n",
                    "--",
                    package,
                ],
            );
            let update_allowed = if let Some(selection) = selection {
                ctx.probe
                    .output(&selection)
                    .await
                    .is_some_and(|output| output.trim() == "install installed")
            } else {
                false
            };
            if !update_allowed {
                return Some(owned(ctx, InstallSource::System, package, None));
            }
            let update = terminal(
                ctx,
                "apt-get",
                &["install", "--only-upgrade", "--", package],
            );
            let latest = ctx
                .command("apt-cache", &["policy", "--", package])
                .map(|mut query| {
                    query.env.push(("LC_ALL".into(), "C".into()));
                    LatestQuery::System(Query::Apt(query))
                })
                .unwrap_or(LatestQuery::None);
            return Some(ctx.installation(
                InstallSource::System,
                update,
                latest,
                vec![package.into()],
            ));
        }
        if let Some(query) = ctx.command("rpm", &["-qf", "--queryformat", "%{NAME}\n", "--", path])
            && let Some(output) = ctx.probe.output(&query).await
            && let Some(package) = single_package(&output)
        {
            let update = terminal(ctx, "dnf", &["upgrade", "--", package])
                .or_else(|| terminal(ctx, "yum", &["upgrade", "--", package]))
                .or_else(|| terminal(ctx, "zypper", &["update", "--", package]));
            return Some(owned(ctx, InstallSource::System, package, update));
        }
        if let Some(query) = ctx.command("pacman", &["-Qoq", "--", path])
            && let Some(output) = ctx.probe.output(&query).await
            && let Some(package) = single_package(&output)
        {
            // Pacman upgrades may require a full system upgrade or an AUR helper.
            return Some(owned(ctx, InstallSource::System, package, None));
        }
        if let Some(query) = ctx.command("apk", &["info", "--who-owns", "--quiet", "--", path])
            && let Some(output) = ctx.probe.output(&query).await
            && let Some(package) = single_package(&output)
            && let Some(verify) = ctx.command("apk", &["info", "--installed", "--", package])
            && let Some(installed) = ctx.probe.output(&verify).await
            && installed.trim() == package
        {
            let update = terminal(ctx, "apk", &["upgrade", "--", package]);
            let latest = ctx
                .command("apk", &["version", "--", package])
                .map(|command| {
                    LatestQuery::System(Query::Apk {
                        command,
                        package: package.to_string(),
                    })
                })
                .unwrap_or(LatestQuery::None);
            return Some(ctx.installation(
                InstallSource::System,
                update,
                latest,
                vec![package.into()],
            ));
        }
    }
    None
}

fn debian_owner<'a>(output: &'a str, path: &str) -> Option<&'a str> {
    let mut owner = None;
    for line in output.lines() {
        let Some((package, owned_path)) = line.rsplit_once(": ") else {
            continue;
        };
        if owned_path != path {
            continue;
        }
        if !valid_package(package) || owner.is_some_and(|owner| owner != package) {
            return None;
        }
        owner = Some(package);
    }
    owner
}

fn single_package(output: &str) -> Option<&str> {
    let package = output.trim();
    valid_package(package).then_some(package)
}

fn valid_package(package: &str) -> bool {
    package
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && package
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+._:-".contains(&byte))
}

fn terminal(ctx: &Context<'_>, manager: &str, args: &[&str]) -> Option<UpdateCommand> {
    let manager = ctx.probe.which(manager, ctx.env)?;
    let privilege_command = ctx
        .probe
        .which("sudo", ctx.env)
        .or_else(|| ctx.probe.which("doas", ctx.env));
    let mut command = if let Some(privilege_command) = privilege_command {
        let mut command = ctx.at(privilege_command, &[]);
        command.args.push(manager.to_string_lossy().into_owned());
        command
            .args
            .extend(args.iter().map(|arg| (*arg).to_string()));
        command
    } else {
        ctx.at(manager, args)
    };
    command.requires_terminal = true;
    Some(command)
}

fn owned(
    ctx: &Context<'_>,
    source: InstallSource,
    package: &str,
    update: Option<UpdateCommand>,
) -> Installation {
    ctx.installation(source, update, LatestQuery::None, vec![package.to_string()])
}

async fn windows(ctx: &Context<'_>) -> Option<Installation> {
    if let Some(installation) = scoop(ctx).or_else(|| chocolatey(ctx)) {
        return Some(installation);
    }
    winget(ctx).await
}

fn scoop(ctx: &Context<'_>) -> Option<Installation> {
    let user_root = env_value(ctx.env, "SCOOP")
        .map(PathBuf::from)
        .or_else(|| ctx.home().map(|home| home.join("scoop")));
    let global_root = env_value(ctx.env, "SCOOP_GLOBAL")
        .map(PathBuf::from)
        .or_else(|| {
            env_value(ctx.env, "ProgramData").map(|root| PathBuf::from(root).join("scoop"))
        });
    for (root, global) in [(user_root.clone(), false), (global_root, true)] {
        let Some(root) = root else { continue };
        let Some(apps) = ctx.probe.canonical(&root.join("apps")) else {
            continue;
        };
        let target = if ctx.same(ctx.binary.parent()?, &root.join("shims")) {
            let shim = ctx.probe.read(&ctx.binary.with_extension("shim"))?;
            let path = shim.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "path").then(|| value.trim().trim_matches('"'))
            })?;
            ctx.probe.canonical(Path::new(path))?
        } else {
            ctx.canonical.clone()
        };
        let Ok(relative) = target.strip_prefix(&apps) else {
            continue;
        };
        let mut components = relative.components();
        let package = components.next()?.as_os_str().to_str()?;
        let version = components.next()?.as_os_str();
        if !valid_package(package) {
            continue;
        }
        let installed = apps.join(package).join(version);
        let Some(manifest) = json(ctx, &installed.join("manifest.json")) else {
            continue;
        };
        let Some(info) = json(ctx, &installed.join("install.json")) else {
            continue;
        };
        if manifest.get("version").and_then(Value::as_str).is_none()
            || info.get("architecture").and_then(Value::as_str).is_none()
        {
            continue;
        }
        let mut command = if global {
            ctx.command("scoop", &["update", "--global", package])
        } else {
            ctx.command("scoop", &["update", package])
        };
        let held = info.get("hold").and_then(Value::as_bool) == Some(true);
        if held {
            command = None;
        }
        if let Some(command) = &mut command {
            command.requires_terminal = true;
        }
        let latest = (!held)
            .then(|| {
                let bucket = info.get("bucket")?.as_str()?;
                if !valid_package(bucket) {
                    return None;
                }
                let bucket = user_root.as_ref()?.join("buckets").join(bucket);
                [
                    bucket.join("bucket").join(format!("{package}.json")),
                    bucket.join(format!("{package}.json")),
                ]
                .into_iter()
                .find(|path| json(ctx, path).is_some())
                .map(|path| LatestQuery::System(Query::ScoopManifest(path)))
            })
            .flatten()
            .unwrap_or(LatestQuery::None);
        return Some(ctx.installation(InstallSource::Scoop, command, latest, vec![package.into()]));
    }
    None
}

fn chocolatey(ctx: &Context<'_>) -> Option<Installation> {
    let root = env_value(ctx.env, "ChocolateyInstall")
        .map(PathBuf::from)
        .or_else(|| {
            env_value(ctx.env, "ProgramData").map(|root| PathBuf::from(root).join("chocolatey"))
        })?;
    let packages = ctx.probe.canonical(&root.join("lib"))?;
    let relative = ctx.canonical.strip_prefix(&packages).ok()?;
    let package = relative.components().next()?.as_os_str().to_str()?;
    if !valid_package(package) {
        return None;
    }
    let manifest = ctx
        .probe
        .read(&packages.join(package).join(format!("{package}.nuspec")))?;
    let id = manifest.split_once("<id>")?.1.split_once("</id>")?.0.trim();
    if !id.eq_ignore_ascii_case(package) {
        return None;
    }
    let mut command = ctx.command("choco", &["upgrade", package]);
    if let Some(command) = &mut command {
        command.requires_terminal = true;
    }
    Some(owned(ctx, InstallSource::Chocolatey, package, command))
}

async fn winget(ctx: &Context<'_>) -> Option<Installation> {
    const SOURCE_SUFFIX: &str = "_Microsoft.Winget.Source_8wekyb3d8bbwe";
    for directory in ctx.canonical.ancestors().skip(1) {
        let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(package) = name.strip_suffix(SOURCE_SUFFIX) else {
            continue;
        };
        if !valid_package(package)
            || ctx
                .probe
                .canonical(&directory.join(format!("{name}.db")))
                .is_none()
        {
            continue;
        }
        // Portable installs keep an installation database beside their files.
        // Also require the exact registered ID; a matching app name is insufficient.
        let query = ctx.command(
            "winget",
            &[
                "list",
                "--id",
                package,
                "--exact",
                "--source",
                "winget",
                "--disable-interactivity",
            ],
        )?;
        let output = ctx.probe.output(&query).await?;
        if !output.lines().any(|line| {
            line.split_whitespace()
                .any(|word| word.eq_ignore_ascii_case(package))
        }) {
            return None;
        }
        let mut command = ctx.command(
            "winget",
            &["upgrade", "--id", package, "--exact", "--source", "winget"],
        );
        if let Some(command) = &mut command {
            command.requires_terminal = true;
        }
        return Some(owned(ctx, InstallSource::Winget, package, command));
    }
    None
}

fn json(ctx: &Context<'_>, path: &Path) -> Option<Value> {
    serde_json::from_str(&ctx.probe.read(path)?).ok()
}
