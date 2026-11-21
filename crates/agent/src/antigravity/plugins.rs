//! Antigravity CLI's native plugin management, driven through `agy plugin …`
//! with one child process per command.

use std::path::PathBuf;

use serde::Deserialize;

use crate::plugin_command::antigravity::op_args;
use crate::{
    AgentError, DeclaredComponents, PluginAction, PluginContext, PluginInstallation, PluginListing,
    PluginOp, PluginOpOutcome, PluginScope, PluginSource, PluginSourceKind, ProviderKind,
    ProviderPluginEntry, Tri,
};

pub(crate) async fn list(context: &PluginContext) -> Result<PluginListing, AgentError> {
    let output = run_cli(context, &args(&["plugin", "list", "--json"])).await?;
    let stdout = succeeded(output)?;
    let listed = parse_listed(&stdout)?;
    Ok(listing(listed))
}

pub(crate) async fn run(
    context: &PluginContext,
    op: &PluginOp,
) -> Result<PluginOpOutcome, AgentError> {
    let argv = op_args(op)?;
    let stdout = succeeded(run_cli(context, &argv).await?)?;
    let message = stdout.trim();
    Ok(PluginOpOutcome::Done {
        diagnostics: if message.is_empty() {
            Vec::new()
        } else {
            vec![("message".into(), message.to_string())]
        },
    })
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

fn management_command(
    context: &PluginContext,
    args: &[String],
) -> Result<std::process::Command, AgentError> {
    let binary = crate::resolve_binary(context.binary_path.as_deref(), "agy")?;
    let mut command = crate::process::command(binary);
    command.args(args).current_dir(&context.cwd);
    for (key, value) in context.launch_env.pairs(ProviderKind::Antigravity) {
        command.env(key, value);
    }
    Ok(command)
}

struct CliOutput {
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

async fn run_cli(context: &PluginContext, args: &[String]) -> Result<CliOutput, AgentError> {
    let display = format!("agy {}", args.join(" "));
    let output = crate::process::tree_output(management_command(context, args)?)
        .await
        .map_err(|error| AgentError::Spawn(format!("spawning `{display}`: {error}")))?;
    Ok(CliOutput {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn succeeded(output: CliOutput) -> Result<String, AgentError> {
    if output.success {
        Ok(output.stdout)
    } else {
        let message = [output.stderr.trim(), output.stdout.trim()]
            .into_iter()
            .find(|text| !text.is_empty())
            .map_or_else(|| output.status.clone(), str::to_string);
        Err(AgentError::Provider(message))
    }
}

/// One row of `agy plugin list --json` (documented in `docs/plugins.md`,
/// observed on `agy` 1.5.13).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    name: String,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    skills: Option<Vec<String>>,
    #[serde(default)]
    agents: Option<Vec<String>>,
    #[serde(default)]
    hooks: Option<Vec<String>>,
    #[serde(default)]
    mcp_servers: Option<Vec<String>>,
}

fn parse_listed(stdout: &str) -> Result<Vec<Listed>, AgentError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(trimmed).map_err(|error| {
        AgentError::Protocol(format!("could not read `agy plugin list --json`: {error}"))
    })
}

fn listing(listed: Vec<Listed>) -> PluginListing {
    let mut entries: Vec<ProviderPluginEntry> = listed
        .into_iter()
        .map(|row| {
            let source_kind = match row.source.as_deref() {
                Some("antigravity-plugins-official") => PluginSourceKind::Official,
                Some(source) if source.starts_with("http://") || source.starts_with("https://") => {
                    PluginSourceKind::Git
                }
                Some(source) if source.starts_with('/') || source.starts_with('.') => {
                    PluginSourceKind::LocalPath
                }
                Some(_) => PluginSourceKind::ThirdParty,
                None => PluginSourceKind::Unknown,
            };
            let enabled = match row.enabled {
                Some(true) => Tri::Yes,
                Some(false) => Tri::No,
                None => Tri::Unknown,
            };
            let mut actions = Vec::new();
            match row.enabled {
                Some(true) => actions.push(PluginAction::Disable {
                    scope: PluginScope::User,
                }),
                Some(false) => actions.push(PluginAction::Enable {
                    scope: PluginScope::User,
                }),
                None => {
                    actions.push(PluginAction::Enable {
                        scope: PluginScope::User,
                    });
                    actions.push(PluginAction::Disable {
                        scope: PluginScope::User,
                    });
                }
            }
            actions.push(PluginAction::Uninstall {
                scope: PluginScope::User,
            });
            let id = match row.source.as_deref() {
                Some(marketplace) if source_kind == PluginSourceKind::Official => {
                    format!("{}@{marketplace}", row.name)
                }
                _ => row.name.clone(),
            };
            ProviderPluginEntry {
                id,
                name: row.name,
                description: row.description,
                version: row.version.clone(),
                latest_version: None,
                source: PluginSource {
                    marketplace: row.source.filter(|_| {
                        matches!(
                            source_kind,
                            PluginSourceKind::Official | PluginSourceKind::ThirdParty
                        )
                    }),
                    kind: source_kind,
                },
                installations: vec![PluginInstallation {
                    scope: PluginScope::User,
                    location: row.path,
                    version: row.version,
                    scope_enabled: row.enabled,
                    other_project: false,
                }],
                enabled,
                always_on: None,
                declared: Some(DeclaredComponents {
                    skills: row.skills,
                    agents: row.agents,
                    hooks: row.hooks,
                    mcp_servers: row.mcp_servers,
                    untrusted_hooks: None,
                    lsp_servers: None,
                    apps: None,
                }),
                errors: Vec::new(),
                actions,
                diagnostics: Vec::new(),
            }
        })
        .collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    PluginListing {
        entries,
        marketplaces: Vec::new(),
        marketplace_actions: Vec::new(),
        errors: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `agy plugin list --json` in the shape documented by `docs/plugins.md`
    /// for Antigravity CLI 1.5.13; names and paths are invented.
    #[test]
    fn listed_plugins_map_enabled_state_sources_and_components() {
        let json = r#"[
            {
                "name": "sample-official",
                "enabled": true,
                "version": "1.2.3",
                "source": "antigravity-plugins-official",
                "path": "/home/me/.gemini/config/plugins/sample-official",
                "skills": ["skill-a", "skill-b"],
                "agents": ["helper"],
                "mcpServers": ["srv"]
            },
            {
                "name": "sample-local",
                "enabled": false,
                "version": "0.1.0",
                "source": "/work/my-plugin",
                "path": "/home/me/.gemini/config/plugins/sample-local"
            }
        ]"#;
        let result = listing(parse_listed(json).unwrap());
        assert_eq!(result.entries.len(), 2);

        let local = &result.entries[0];
        assert_eq!(local.name, "sample-local");
        assert_eq!(local.enabled, Tri::No);
        assert_eq!(local.source.kind, PluginSourceKind::LocalPath);
        assert_eq!(
            local.actions,
            vec![
                PluginAction::Enable {
                    scope: PluginScope::User
                },
                PluginAction::Uninstall {
                    scope: PluginScope::User
                }
            ]
        );

        let official = &result.entries[1];
        assert_eq!(official.id, "sample-official@antigravity-plugins-official");
        assert_eq!(official.enabled, Tri::Yes);
        assert_eq!(official.source.kind, PluginSourceKind::Official);
        let declared = official.declared.as_ref().unwrap();
        assert_eq!(
            declared.skills.as_deref(),
            Some(&["skill-a".to_string(), "skill-b".to_string()][..])
        );
        assert_eq!(
            declared.mcp_servers.as_deref(),
            Some(&["srv".to_string()][..])
        );
    }
}
