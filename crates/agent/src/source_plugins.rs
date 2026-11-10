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
/// Installs download packages.
const MUTATION_TIMEOUT: Duration = Duration::from_secs(300);

async fn run_cli(
    provider: ProviderKind,
    default: &str,
    context: &PluginContext,
    args: &[String],
    timeout: Duration,
) -> Result<String, AgentError> {
    let display = format!("{default} {}", args.join(" "));
    let binary = crate::resolve_binary(context.binary_path.as_deref(), default)?;
    let mut command = crate::process::command(binary);
    command.args(args).current_dir(&context.cwd);
    for (key, value) in context.launch_env.pairs(provider) {
        command.env(key, value);
    }
    let mut command = smol::process::Command::from(command);
    // `output()` gives the child a null stdin, so a prompt can never wait on
    // input, and dropping it on timeout kills the child.
    command.kill_on_drop(true);
    let output = smol::future::or(async { Some(command.output().await) }, async {
        smol::Timer::after(timeout).await;
        None
    })
    .await
    .ok_or_else(|| AgentError::Provider(format!("`{display}` timed out")))?
    .map_err(|error| AgentError::Spawn(format!("spawning `{display}`: {error}")))?;
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

/// A package `pi list` printed: its source, the scope of the heading above
/// it, and the path on the line below it.
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
                    packages.push((source.to_string(), scope, None));
                }
            }
            (_, path) => {
                if let Some(package) = packages.last_mut().filter(|package| package.2.is_none()) {
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
    let stdout = run_cli(ProviderKind::Pi, "pi", context, &list_args, LIST_TIMEOUT).await?;
    let mut entries: Vec<ProviderPluginEntry> = Vec::new();
    for (source, scope, location) in parse_pi_list(&stdout) {
        // A listing away from a project addresses no project package.
        if scope == PluginScope::Project && !context.project {
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
            None => entries.push(entry(&source, installation, actions)),
        }
    }
    Ok(PluginListing {
        entries,
        marketplaces: Vec::new(),
        marketplace_actions: Vec::new(),
        errors: Vec::new(),
    })
}

pub(crate) async fn run_pi(
    context: &PluginContext,
    op: &PluginOp,
) -> Result<PluginOpOutcome, AgentError> {
    run_cli(
        ProviderKind::Pi,
        "pi",
        context,
        &pi::op_args(op)?,
        MUTATION_TIMEOUT,
    )
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
                LIST_TIMEOUT,
            )
            .await
        }
    };
    let config_dir = run(&["debug", "paths", "config"]).await?;
    let config = run(&["debug", "config", "--standalone"]).await?;
    global_documents(&config, Path::new(config_dir.trim()))
}

/// The plugins the documents name. An entry may be a specifier or a
/// `[specifier, options]` pair.
fn configured_plugins(documents: &[Value]) -> Vec<String> {
    documents
        .iter()
        .filter_map(|document| document.get("plugins")?.as_array())
        .flatten()
        .filter_map(|plugin| {
            plugin
                .as_str()
                .or_else(|| plugin.as_array()?.first()?.as_str())
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

pub(crate) async fn list_opencode(context: &PluginContext) -> Result<PluginListing, AgentError> {
    let run = |args: &'static [&'static str]| {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        async move {
            run_cli(
                ProviderKind::OpenCode,
                "opencode",
                context,
                &args,
                LIST_TIMEOUT,
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
        .map(|out| parse_opencode_list(&out))
        .unwrap_or_else(|error| {
            errors.push(format!("opencode plugin list: {error}"));
            Vec::new()
        });
    let checked = checked
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
        MUTATION_TIMEOUT,
    )
    .await
    .map(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Printed by pi 1.5.0 with `--approve` in a project holding a package.
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

    /// Printed by OpenCode 2.2.5: only the global document's plugins count.
    #[test]
    fn opencode_lists_what_the_global_configuration_names() {
        let config = r#"[
            {"type":"document","path":"/home/me/.config/opencode/opencode.json",
             "info":{"plugins":["opencode-wakatime",["with-options@1.0.0",{"a":1}]]}},
            {"type":"document","path":"/work/proj/opencode.json","info":{"plugins":["project-only"]}},
            {"type":"directory","path":"/home/me/.config/opencode"}
        ]"#;
        assert_eq!(
            configured_plugins(
                &global_documents(config, Path::new("/home/me/.config/opencode")).unwrap()
            ),
            ["opencode-wakatime", "with-options@1.0.0"]
        );
    }

    /// Printed by OpenCode 2.2.5, a row for a loaded and for a failed plugin.
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

    /// A CLI that answers each command as the installed one does and logs
    /// its argv, driven through the entry points the host calls.
    #[cfg(unix)]
    mod driven {
        use super::*;
        use crate::{LaunchEnv, list_plugins, run_plugin_op};
        use std::os::unix::fs::PermissionsExt as _;

        struct FakeCli {
            dir: PathBuf,
            context: PluginContext,
        }

        impl FakeCli {
            fn new(name: &str, script: &str) -> Self {
                let dir = std::env::temp_dir()
                    .join(format!("tcode-source-plugins-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&dir).unwrap();
                let binary = dir.join(name);
                std::fs::write(
                    &binary,
                    format!(
                        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}/argv.log'\ncase \"$*\" in\n{script}\n*) echo \"unexpected: $*\" >&2; exit 2 ;;\nesac\n",
                        dir.display()
                    ),
                )
                .unwrap();
                std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
                let context = PluginContext {
                    binary_path: Some(binary),
                    launch_env: LaunchEnv::default(),
                    cwd: dir.clone(),
                    project: true,
                };
                Self { dir, context }
            }

            fn calls(&self) -> Vec<String> {
                std::fs::read_to_string(self.dir.join("argv.log"))
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect()
            }
        }

        impl Drop for FakeCli {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        #[test]
        fn pi_lists_both_scopes_and_changes_a_package_in_its_scope() {
            let pi = FakeCli::new(
                "pi",
                &format!(
                    "'list --approve') printf '{}' ;;\n'install npm:new -l --approve') echo Installed ;;\n'remove npm:pi-extmgr') echo Removed ;;\n'update --extension npm:pi-extmgr --approve') echo Updated ;;",
                    PI_LIST.replace('\n', "\\n")
                ),
            );
            let listing = smol::block_on(list_plugins(ProviderKind::Pi, &pi.context)).unwrap();
            let package = &listing.entries[0];
            assert_eq!(package.id, "npm:pi-extmgr");
            assert_eq!(package.enabled, Tri::Unknown, "only `pi config` knows");
            assert_eq!(package.installations.len(), 2);
            assert_eq!(
                package.actions,
                [
                    PluginAction::Uninstall {
                        scope: PluginScope::User
                    },
                    PluginAction::Update {
                        scope: PluginScope::User
                    },
                    PluginAction::Uninstall {
                        scope: PluginScope::Project
                    },
                    PluginAction::Update {
                        scope: PluginScope::Project
                    },
                ]
            );
            assert!(!package.actions.iter().any(|action| matches!(
                action,
                PluginAction::Enable { .. } | PluginAction::Disable { .. }
            )));
            for op in [
                PluginOp::Install {
                    id: "npm:new".into(),
                    scope: PluginScope::Project,
                    accept_command: None,
                },
                PluginOp::Uninstall {
                    id: "npm:pi-extmgr".into(),
                    scope: PluginScope::User,
                },
                PluginOp::Update {
                    id: "npm:pi-extmgr".into(),
                    scope: PluginScope::Project,
                    accept_command: None,
                },
            ] {
                smol::block_on(run_plugin_op(ProviderKind::Pi, &pi.context, &op)).unwrap();
            }
            assert_eq!(pi.calls().len(), 4);
        }

        /// A background server that has not loaded the plugins yet answers
        /// `plugin list` with none and `plugin check` with none; they stay
        /// listed, because the configuration names them.
        #[test]
        fn opencode_lists_what_is_configured_whatever_the_server_has_loaded() {
            let opencode = FakeCli::new(
                "opencode",
                "'debug paths config') echo /home/me/.config/opencode ;;\n'debug config --standalone') echo '[{\"type\":\"document\",\"path\":\"/home/me/.config/opencode/opencode.json\",\"info\":{\"plugins\":[\"opencode-wakatime\"]}}]' ;;\n'plugin list') echo 'No plugins found' ;;\n'plugin check') echo 'No package plugins found' ;;\n'plugin remove opencode-wakatime') echo removed ;;",
            );
            let listing =
                smol::block_on(list_plugins(ProviderKind::OpenCode, &opencode.context)).unwrap();
            assert_eq!(listing.entries.len(), 1);
            assert_eq!(listing.entries[0].id, "opencode-wakatime");
            assert_eq!(listing.entries[0].errors, Vec::<String>::new());
            smol::block_on(run_plugin_op(
                ProviderKind::OpenCode,
                &opencode.context,
                &PluginOp::Uninstall {
                    id: "opencode-wakatime".into(),
                    scope: PluginScope::User,
                },
            ))
            .unwrap();
            assert!(
                smol::block_on(run_plugin_op(
                    ProviderKind::OpenCode,
                    &opencode.context,
                    &PluginOp::Install {
                        id: "a".into(),
                        scope: PluginScope::Project,
                        accept_command: None,
                    },
                ))
                .is_err(),
                "the CLI changes the global configuration only"
            );
        }
    }
}
