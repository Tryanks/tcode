//! Plugin management for the harnesses that keep a list of package sources
//! and no catalog: pi and OpenCode. Neither CLI prints JSON for its packages,
//! so only what each prints stably is read, and nothing is offered that its
//! CLI cannot do without a person at a terminal.
//!
//! pi turns a package's resources on and off only in `pi config`, a TUI, so
//! it has no switch here. `pi list` reads project packages only for a command
//! that trusts the project, which the person is doing by listing or changing
//! them from Tcode; `--approve` is one command's trust, never remembered.
//!
//! OpenCode adds and removes plugins in its global configuration only, so
//! that is what is listed: a project's own `opencode.json` is not changed by
//! any `opencode plugin` command. `opencode plugin list` and `check` answer
//! from a server's loaded plugins, which a server just started has not got
//! to, so the configured list comes from `opencode debug config` and they
//! only add versions and update states when they have them.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::plugin_command::{opencode, pi};
use crate::{
    AgentError, PluginAction, PluginContext, PluginInstallation, PluginListing, PluginOp,
    PluginOpOutcome, PluginScope, PluginSource, PluginSourceKind, ProviderKind,
    ProviderPluginEntry, Tri,
};

const LIST_TIMEOUT: Duration = Duration::from_secs(30);

async fn run_cli(
    provider: ProviderKind,
    default: &str,
    context: &PluginContext,
    args: &[String],
    bound: Option<Duration>,
) -> Result<String, AgentError> {
    cli_stdout(cli_output(provider, default, context, args, bound).await?)
}

async fn cli_output(
    provider: ProviderKind,
    default: &str,
    context: &PluginContext,
    args: &[String],
    bound: Option<Duration>,
) -> Result<Output, AgentError> {
    let display = format!("{default} {}", args.join(" "));
    let binary = crate::resolve_binary(context.binary_path.as_deref(), default)?;
    let mut command = crate::process::command(binary);
    command.args(args).current_dir(&context.cwd);
    for (key, value) in context.launch_env.pairs(provider) {
        command.env(key, value);
    }
    // A listing, which no one can stop, is bounded; a change runs until it
    // ends or the user cancels it, which drops this and stops the command.
    let output = crate::process::tree_output(command);
    match bound {
        Some(bound) => smol::future::or(async { Some(output.await) }, async {
            smol::Timer::after(bound).await;
            None
        })
        .await
        .ok_or_else(|| AgentError::Provider(format!("`{display}` timed out")))?,
        None => output.await,
    }
    .map_err(|error| AgentError::Spawn(format!("spawning `{display}`: {error}")))
}

fn cli_stdout(output: Output) -> Result<String, AgentError> {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .map_or_else(|| output.status.to_string(), str::to_string);
    Err(AgentError::Provider(message))
}

fn source_kind(source: &str) -> PluginSourceKind {
    if source.starts_with("git:") || source.starts_with("github:") || source.contains("://") {
        PluginSourceKind::Git
    } else if source.starts_with('.') || source.starts_with('/') || source.starts_with('~') {
        PluginSourceKind::LocalPath
    } else {
        PluginSourceKind::Npm
    }
}

/// The name a source goes by: an npm package without its registry prefix or
/// version, else the last part of its path.
fn source_name(source: &str) -> String {
    let source = source.strip_prefix("npm:").unwrap_or(source);
    if source_kind(source) == PluginSourceKind::Npm {
        return match source.rfind('@') {
            Some(at) if at > 0 => source[..at].to_string(),
            _ => source.to_string(),
        };
    }
    source
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(source)
        .trim_end_matches(".git")
        .to_string()
}

fn entry(
    source: &str,
    installation: PluginInstallation,
    actions: Vec<PluginAction>,
) -> ProviderPluginEntry {
    ProviderPluginEntry {
        id: source.to_string(),
        name: source_name(source),
        version: installation.version.clone(),
        latest_version: None,
        description: None,
        source: PluginSource {
            marketplace: None,
            kind: source_kind(source),
        },
        installations: vec![installation],
        enabled: Tri::Unknown,
        declared: None,
        always_on: None,
        errors: Vec::new(),
        actions,
        diagnostics: Vec::new(),
    }
}

fn done(stdout: String) -> PluginOpOutcome {
    PluginOpOutcome::Done {
        diagnostics: vec![("message".into(), stdout.trim().to_string())],
    }
}

/// pi reads a source as a local path unless it names a registry or a remote.
fn pi_source_is_local(source: &str) -> bool {
    ![
        "npm:", "git:", "github:", "http:", "https:", "ssh:", "builtin:",
    ]
    .iter()
    .any(|prefix| source.starts_with(prefix))
}

/// A package `pi list` printed: its source, the scope of the heading above
/// it, and the path on the line below it.
///
/// pi stores a local source relative to its scope's settings directory but
/// resolves the source given to `pi remove` or `pi update` against the
/// working directory, so a local package goes by the path pi resolved it to.
/// A package whose settings entry filters its resources is marked
/// ` (filtered)`, which is not part of its source.
fn parse_pi_list(stdout: &str) -> Vec<(String, PluginScope, Option<PathBuf>)> {
    let mut packages = Vec::new();
    let mut scope = None;
    for line in stdout.lines() {
        let indent = line.len() - line.trim_start().len();
        let text = line.trim();
        match (indent, text) {
            (_, "") => {}
            (0, "User packages:") => scope = Some(PluginScope::User),
            (0, "Project packages:") => scope = Some(PluginScope::Project),
            (0, _) => scope = None,
            (2, source) => {
                if let Some(scope) = scope {
                    let source = source.strip_suffix(" (filtered)").unwrap_or(source);
                    packages.push((source.to_string(), scope, None));
                }
            }
            (_, path) => {
                if let Some(package) = packages.last_mut().filter(|package| package.2.is_none()) {
                    if pi_source_is_local(&package.0) {
                        package.0 = path.to_string();
                    }
                    package.2 = Some(PathBuf::from(path));
                }
            }
        }
    }
    packages
}

pub(crate) async fn list_pi(context: &PluginContext) -> Result<PluginListing, AgentError> {
    let mut list_args = vec!["list".to_string()];
    if context.project {
        list_args.push("--approve".to_string());
    }
    let stdout = run_cli(
        ProviderKind::Pi,
        "pi",
        context,
        &list_args,
        Some(LIST_TIMEOUT),
    )
    .await?;
    Ok(PluginListing {
        entries: pi_entries(&stdout, context.project),
        marketplaces: Vec::new(),
        marketplace_actions: Vec::new(),
        errors: Vec::new(),
    })
}

fn pi_entries(stdout: &str, project: bool) -> Vec<ProviderPluginEntry> {
    let mut entries: Vec<ProviderPluginEntry> = Vec::new();
    for (source, scope, location) in parse_pi_list(stdout) {
        // A listing away from a project addresses no project package.
        if scope == PluginScope::Project && !project {
            continue;
        }
        let installation = PluginInstallation {
            scope,
            location,
            version: None,
            scope_enabled: None,
            other_project: false,
        };
        let actions = vec![
            PluginAction::Uninstall { scope },
            PluginAction::Update { scope },
        ];
        match entries.iter_mut().find(|entry| entry.id == source) {
            Some(listed) => {
                listed.installations.push(installation);
                listed.actions.extend(actions);
            }
            None => {
                let mut listed = entry(&source, installation, actions);
                if pi_source_is_local(&source) {
                    listed.source.kind = PluginSourceKind::LocalPath;
                    if let Some(name) = Path::new(&source).file_name() {
                        listed.name = name.to_string_lossy().into_owned();
                    }
                }
                entries.push(listed);
            }
        }
    }
    entries
}

pub(crate) async fn run_pi(
    context: &PluginContext,
    op: &PluginOp,
) -> Result<PluginOpOutcome, AgentError> {
    run_cli(ProviderKind::Pi, "pi", context, &pi::op_args(op)?, None)
        .await
        .map(done)
}

#[derive(Deserialize)]
struct ConfigSource {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    info: Option<Value>,
}

/// What each document of the global configuration resolves to, from
/// `opencode debug config`, which needs no server and masks secrets.
pub(crate) fn global_documents(stdout: &str, config_dir: &Path) -> Result<Vec<Value>, AgentError> {
    let sources: Vec<ConfigSource> = serde_json::from_str(stdout).map_err(|error| {
        AgentError::Protocol(format!("`opencode debug config` output: {error}"))
    })?;
    Ok(sources
        .into_iter()
        .filter(|source| {
            source.kind == "document"
                && source
                    .path
                    .as_deref()
                    .is_some_and(|path| path.parent() == Some(config_dir))
        })
        .filter_map(|source| source.info)
        .collect())
}

pub(crate) async fn opencode_global_config(
    context: &PluginContext,
) -> Result<Vec<Value>, AgentError> {
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        async move {
            run_cli(
                ProviderKind::OpenCode,
                "opencode",
                context,
                &args,
                Some(LIST_TIMEOUT),
            )
            .await
        }
    };
    let config_dir = run(&["debug", "paths", "config"]).await?;
    let config = run(&["debug", "config", "--standalone"]).await?;
    global_documents(&config, Path::new(config_dir.trim()))
}

/// The plugins the documents name. An entry is a specifier or a
/// `{"package", "options"}` object. An entry that switches plugins rather
/// than naming one (one starting with `-` or `opencode.`, `*`, or one
/// ending `.*`) is not a plugin: OpenCode 2.2.7's `cli.plugin.list` passes
/// over the same entries.
fn configured_plugins(documents: &[Value]) -> Vec<String> {
    documents
        .iter()
        .filter_map(|document| document.get("plugins")?.as_array())
        .flatten()
        .filter_map(|plugin| plugin.as_str().or_else(|| plugin.get("package")?.as_str()))
        .filter(|target| {
            !(target.starts_with('-')
                || *target == "*"
                || target.ends_with(".*")
                || target.starts_with("opencode."))
        })
        .map(str::to_string)
        .collect()
}

/// `(id, version, source)` per row of `opencode plugin list`'s table.
fn parse_opencode_list(stdout: &str) -> Vec<(String, Option<String>, String)> {
    stdout
        .lines()
        .skip_while(|line| !line.starts_with("ID"))
        .skip(1)
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let (id, version, source) = (columns.next()?, columns.next()?, columns.next()?);
            Some((
                id.to_string(),
                (version != "-").then(|| version.to_string()),
                source.to_string(),
            ))
        })
        .collect()
}

/// `(name, state)` per line of `opencode plugin check`, indented under a
/// section heading: `  name [version] (state)`.
fn parse_opencode_check(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter(|line| line.starts_with(' '))
        .filter_map(|line| {
            let line = line.trim();
            let (head, state) = line.strip_suffix(')')?.rsplit_once(" (")?;
            let name = head.split_whitespace().next()?;
            Some((name.to_string(), state.to_string()))
        })
        .collect()
}

/// `opencode plugin check` prints its report and then exits 1 when any
/// plugin's check failed; a command that fails outright prints nothing to
/// stdout.
fn opencode_check_report(output: Output) -> Result<String, AgentError> {
    if output.stdout.is_empty() {
        return cli_stdout(output);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) async fn list_opencode(context: &PluginContext) -> Result<PluginListing, AgentError> {
    let run = |args: &'static [&'static str]| {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        async move {
            cli_output(
                ProviderKind::OpenCode,
                "opencode",
                context,
                &args,
                Some(LIST_TIMEOUT),
            )
            .await
        }
    };
    let configured = configured_plugins(&opencode_global_config(context).await?);
    if configured.is_empty() {
        return Ok(PluginListing {
            entries: Vec::new(),
            marketplaces: Vec::new(),
            marketplace_actions: Vec::new(),
            errors: Vec::new(),
        });
    }
    // Answered by OpenCode's background server, which the CLI starts itself.
    let (loaded, checked) =
        smol::future::zip(run(&["plugin", "list"]), run(&["plugin", "check"])).await;
    let mut errors = Vec::new();
    let loaded = loaded
        .and_then(cli_stdout)
        .map(|out| parse_opencode_list(&out))
        .unwrap_or_else(|error| {
            errors.push(format!("opencode plugin list: {error}"));
            Vec::new()
        });
    let checked = checked
        .and_then(opencode_check_report)
        .map(|out| parse_opencode_check(&out))
        .unwrap_or_else(|error| {
            errors.push(format!("opencode plugin check: {error}"));
            Vec::new()
        });
    let entries = configured
        .iter()
        .map(|source| {
            let row = loaded.iter().find(|row| row.2 == *source);
            let scope = PluginScope::User;
            let mut listed = entry(
                source,
                PluginInstallation {
                    scope,
                    location: None,
                    version: row.and_then(|row| row.1.clone()),
                    scope_enabled: None,
                    other_project: false,
                },
                vec![
                    PluginAction::Uninstall { scope },
                    PluginAction::Update { scope },
                ],
            );
            let id = row.map(|row| row.0.as_str());
            if let Some((_, state)) = checked
                .iter()
                .find(|(name, _)| name == source || Some(name.as_str()) == id)
                && state != "current"
            {
                listed
                    .errors
                    .push(format!("opencode plugin check: {state}"));
            }
            listed
        })
        .collect();
    Ok(PluginListing {
        entries,
        marketplaces: Vec::new(),
        marketplace_actions: Vec::new(),
        errors,
    })
}

pub(crate) async fn run_opencode(
    context: &PluginContext,
    op: &PluginOp,
) -> Result<PluginOpOutcome, AgentError> {
    run_cli(
        ProviderKind::OpenCode,
        "opencode",
        context,
        &opencode::op_args(op)?,
        None,
    )
    .await
    .map(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pi list --approve` in a project holding a package, in the shape pi
    /// 1.5.1 prints it (`dist/package-manager-cli.js`, the `list` case: a
    /// heading per scope, each source indented two spaces, its installed path
    /// four).
    const PI_LIST: &str = "User packages:\n  npm:pi-extmgr\n    /home/me/.pi/agent/npm/node_modules/pi-extmgr\n\nProject packages:\n  npm:pi-extmgr\n    /work/proj/.pi/npm/node_modules/pi-extmgr\n  git:github.com/me/tools\n    /work/proj/.pi/git/tools\n";

    #[test]
    fn pi_packages_are_read_per_heading() {
        assert_eq!(
            parse_pi_list(PI_LIST),
            [
                (
                    "npm:pi-extmgr".to_string(),
                    PluginScope::User,
                    Some(PathBuf::from(
                        "/home/me/.pi/agent/npm/node_modules/pi-extmgr"
                    ))
                ),
                (
                    "npm:pi-extmgr".to_string(),
                    PluginScope::Project,
                    Some(PathBuf::from("/work/proj/.pi/npm/node_modules/pi-extmgr"))
                ),
                (
                    "git:github.com/me/tools".to_string(),
                    PluginScope::Project,
                    Some(PathBuf::from("/work/proj/.pi/git/tools"))
                ),
            ]
        );
        assert_eq!(parse_pi_list("No packages installed.\n"), []);
    }

    /// pi 1.5.1 prints a package whose settings entry is an object (one that
    /// filters its resources) as `<source> (filtered)`, and a local source as
    /// stored: relative to the scope's settings directory, here the agent
    /// directory (`dist/package-manager-cli.js`'s `list` case,
    /// `dist/core/package-manager.js`'s `listConfiguredPackages` and
    /// `normalizePackageSourceForSettings`). `pi remove` and `pi update`
    /// resolve a local source against the working directory
    /// (`getSourceMatchKeyForInput`, `getPackageIdentity` without a scope).
    #[test]
    fn pi_packages_go_by_the_source_pi_matches_from_any_directory() {
        let listed = pi_entries(
            "User packages:\n  npm:pi-extmgr (filtered)\n    /home/me/.pi/agent/npm/node_modules/pi-extmgr\n  ../tools/my-ext\n    /home/me/.pi/tools/my-ext\n\nProject packages:\n  npm:pi-extmgr\n    /work/proj/.pi/npm/node_modules/pi-extmgr\n",
            true,
        );
        assert_eq!(
            listed
                .iter()
                .map(|entry| (
                    entry.id.as_str(),
                    entry.name.as_str(),
                    entry.source.kind,
                    entry
                        .installations
                        .iter()
                        .map(|installation| installation.scope)
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            [
                (
                    "npm:pi-extmgr",
                    "pi-extmgr",
                    PluginSourceKind::Npm,
                    vec![PluginScope::User, PluginScope::Project]
                ),
                (
                    "/home/me/.pi/tools/my-ext",
                    "my-ext",
                    PluginSourceKind::LocalPath,
                    vec![PluginScope::User]
                ),
            ]
        );
    }

    /// `opencode debug config --standalone` prints the server's configuration
    /// sources (OpenCode 2.2.6, `cli.debug.config`); a plugin entry is a
    /// specifier or a `{package, options}` object (the configuration schema's
    /// `plugins` union, read the same way by `cli.plugin.list`). Only the
    /// global document's plugins count, and the switches that entry list
    /// also holds (`packages/cli/src/commands/handlers/plugin/list.ts` at
    /// v2.2.7 skips a target starting with `-` or `opencode.`, `*`, and one
    /// ending `.*`) are not plugins.
    #[test]
    fn opencode_lists_what_the_global_configuration_names() {
        let config = r#"[
            {"type":"document","path":"/home/me/.config/opencode/opencode.json",
             "info":{"plugins":["*","-opencode-wakatime-legacy","opencode.builtin","acme.*",
                                "opencode-wakatime",
                                {"package":"opencode-notify@2.0.0","options":{"sound":false}}]}},
            {"type":"document","path":"/work/proj/opencode.json","info":{"plugins":["project-only"]}},
            {"type":"directory","path":"/home/me/.config/opencode"}
        ]"#;
        assert_eq!(
            configured_plugins(
                &global_documents(config, Path::new("/home/me/.config/opencode")).unwrap()
            ),
            ["opencode-wakatime", "opencode-notify@2.0.0"]
        );
    }

    /// A row for a loaded and for a failed plugin, in the shape OpenCode
    /// 2.2.6 prints them: `cli.plugin.list`'s `ID  VERSION  SOURCE` table
    /// (`-` for no id or version), `cli.plugin.check`'s indented
    /// `name version (state)` under a runtime heading.
    #[test]
    fn opencode_tables_map_loaded_plugins_to_their_specifiers() {
        assert_eq!(
            parse_opencode_list(
                "ID                 VERSION  SOURCE\nopencode.wakatime  1.4.0    opencode-wakatime\n-                  -        broken@1.3.0\n"
            ),
            [
                (
                    "opencode.wakatime".to_string(),
                    Some("1.4.0".to_string()),
                    "opencode-wakatime".to_string()
                ),
                ("-".to_string(), None, "broken@1.3.0".to_string()),
            ]
        );
        assert_eq!(
            parse_opencode_check(
                "Server\n  opencode.wakatime 1.4.0 (current)\n  broken@1.3.0 (check failed)\n"
            ),
            [
                ("opencode.wakatime".to_string(), "current".to_string()),
                ("broken@1.3.0".to_string(), "check failed".to_string()),
            ]
        );
    }

    fn exited(code: i32, stdout: &str, stderr: &str) -> Output {
        #[cfg(unix)]
        let status = std::os::unix::process::ExitStatusExt::from_raw(code << 8);
        #[cfg(windows)]
        let status = std::os::windows::process::ExitStatusExt::from_raw(code as u32);
        Output {
            status,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// OpenCode 2.2.6's `cli.plugin.check` writes its report, then sets exit
    /// code 1 if any plugin's check failed; a check that cannot run at all
    /// (here `cli.plugin.inspect`'s "Plugin is not configured") writes only
    /// the error, to stderr, and exits 1.
    #[test]
    fn a_failed_plugin_check_keeps_every_plugins_state() {
        let report = opencode_check_report(exited(
            1,
            "Server\n  opencode.wakatime 1.4.0 (current)\n  broken@1.3.0 (check failed)\n",
            "",
        ))
        .unwrap();
        assert_eq!(
            parse_opencode_check(&report),
            [
                ("opencode.wakatime".to_string(), "current".to_string()),
                ("broken@1.3.0".to_string(), "check failed".to_string()),
            ]
        );
        let Err(AgentError::Provider(message)) = opencode_check_report(exited(
            1,
            "",
            "Error: Plugin is not configured: broken@1.3.0\n",
        )) else {
            panic!("a check that did not run is an error");
        };
        assert_eq!(message, "Error: Plugin is not configured: broken@1.3.0");
    }

    #[test]
    fn a_source_goes_by_its_package_or_repository_name() {
        for (source, name, kind) in [
            ("npm:pi-extmgr", "pi-extmgr", PluginSourceKind::Npm),
            ("npm:@foo/bar@1.2.0", "@foo/bar", PluginSourceKind::Npm),
            (
                "opencode-wakatime@1.3.0",
                "opencode-wakatime",
                PluginSourceKind::Npm,
            ),
            (
                "git:github.com/me/tools.git",
                "tools",
                PluginSourceKind::Git,
            ),
            (
                "https://github.com/me/tools",
                "tools",
                PluginSourceKind::Git,
            ),
            ("./local/path", "path", PluginSourceKind::LocalPath),
        ] {
            assert_eq!(source_name(source), name, "{source}");
            assert_eq!(source_kind(source), kind, "{source}");
        }
    }
}
