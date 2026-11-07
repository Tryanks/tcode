//! The native command each plugin operation runs: the adapters run it, and a
//! client shows it to the person before anything runs.

use std::path::Path;

use crate::{PluginOp, ProviderKind};

/// The command line `op` runs for `provider`, with `program` standing for
/// the CLI the profile is configured with, as a person would type it.
/// `None` when the provider runs no command for it.
pub fn plugin_op_command(
    provider: ProviderKind,
    program: Option<&Path>,
    op: &PluginOp,
) -> Option<String> {
    let (default, argv) = match provider {
        ProviderKind::ClaudeCode => ("claude", claude::op_args(op).ok()?),
        ProviderKind::Grok => ("grok", grok::op_args(op).ok()?),
        // Codex is driven over its app server, so the request stands in
        // for a command line.
        ProviderKind::Codex => ("codex", codex_request(op)?),
        ProviderKind::Pi => ("pi", pi::op_args(op).ok()?),
        ProviderKind::OpenCode => ("opencode", opencode::op_args(op).ok()?),
        ProviderKind::Cursor | ProviderKind::Acp => return None,
    };
    let program = program.map_or_else(|| default.to_string(), |path| path.display().to_string());
    Some(shell_words::join(std::iter::once(program).chain(argv)))
}

/// Codex installs and enables plugins for the user only.
fn codex_request(op: &PluginOp) -> Option<Vec<String>> {
    if let PluginOp::Install { scope, .. }
    | PluginOp::Uninstall { scope, .. }
    | PluginOp::SetEnabled { scope, .. } = op
        && *scope != crate::PluginScope::User
    {
        return None;
    }
    let (method, target) = match op {
        PluginOp::Install { id, .. } => ("plugin/install", id.clone()),
        PluginOp::Uninstall { id, .. } => ("plugin/uninstall", id.clone()),
        PluginOp::SetEnabled { id, enabled, .. } => (
            "config/value/write",
            format!("plugins.{id}.enabled={enabled}"),
        ),
        PluginOp::Update { .. } => return None,
        PluginOp::AddMarketplace { source } => ("marketplace/add", source.clone()),
        PluginOp::RemoveMarketplace { name } => ("marketplace/remove", name.clone()),
    };
    Some(vec!["app-server".into(), method.into(), target])
}

pub(crate) mod claude {
    use crate::{AgentError, PluginOp, PluginScope};

    /// The argv for one mutation. `-y` is never passed: a marketplace-declared
    /// command runs only with the `--accept-command` hash a person accepted.
    /// Every plugin mutation names its scope, because the CLI's default
    /// auto-detects one from the working directory.
    pub(crate) fn op_args(op: &PluginOp) -> Result<Vec<String>, AgentError> {
        let args = match op {
            PluginOp::Install {
                id,
                scope,
                accept_command,
            }
            | PluginOp::Update {
                id,
                scope,
                accept_command,
            } => {
                let verb = if matches!(op, PluginOp::Install { .. }) {
                    "install"
                } else {
                    "update"
                };
                let mut args = plugin_args(verb, id, *scope)?;
                if let Some(sha256) = accept_command {
                    args.extend(["--accept-command".into(), operand(sha256)?]);
                }
                args
            }
            PluginOp::Uninstall { id, scope } => plugin_args("uninstall", id, *scope)?,
            PluginOp::SetEnabled { id, scope, enabled } => {
                plugin_args(if *enabled { "enable" } else { "disable" }, id, *scope)?
            }
            PluginOp::AddMarketplace { source } => vec![
                "plugin".into(),
                "marketplace".into(),
                "add".into(),
                operand(source)?,
                "--scope".into(),
                "user".into(),
            ],
            PluginOp::RemoveMarketplace { name } => vec![
                "plugin".into(),
                "marketplace".into(),
                "remove".into(),
                operand(name)?,
            ],
        };
        Ok(args)
    }

    fn plugin_args(verb: &str, id: &str, scope: PluginScope) -> Result<Vec<String>, AgentError> {
        Ok(vec![
            "plugin".into(),
            verb.into(),
            operand(id)?,
            "--scope".into(),
            scope_arg(scope)?.into(),
            "--json".into(),
        ])
    }

    /// A value placed in a positional slot must not be read as an option.
    pub(crate) fn operand(value: &str) -> Result<String, AgentError> {
        if value.is_empty() || value.starts_with('-') {
            return Err(AgentError::Protocol(format!(
                "refusing to pass {value:?} to `claude plugin`"
            )));
        }
        Ok(value.to_string())
    }

    fn scope_arg(scope: PluginScope) -> Result<&'static str, AgentError> {
        match scope {
            PluginScope::User => Ok("user"),
            PluginScope::Project => Ok("project"),
            PluginScope::Local => Ok("local"),
            PluginScope::Managed => Ok("managed"),
            PluginScope::Session => Err(AgentError::Protocol(
                "Claude Code session plugins have no management command".into(),
            )),
        }
    }
}

pub(crate) mod grok {
    use crate::{AgentError, PluginOp, PluginScope};

    /// The argv for one mutation, without consent: `--trust` is added only for an
    /// install whose challenge a person accepted (`install_step`), and
    /// `--confirm`, which uninstalls every plugin of a multi-plugin repository, is
    /// never passed. Plugins are named as `grok plugin list` names them; installs
    /// take the `name@marketplace` id.
    pub(crate) fn op_args(op: &PluginOp) -> Result<Vec<String>, AgentError> {
        let plugin = |verb: &str, target: String, scope: PluginScope| match scope {
            PluginScope::User => Ok(vec!["plugin".into(), verb.into(), target]),
            other => Err(AgentError::Protocol(format!(
                "Grok installs plugins for the user only, not in the {other:?} scope"
            ))),
        };
        let marketplace = |verb: &str, target: &str| {
            Ok(vec![
                "plugin".into(),
                "marketplace".into(),
                verb.into(),
                operand(target)?,
            ])
        };
        match op {
            PluginOp::Install { id, scope, .. } => plugin("install", operand(id)?, *scope),
            PluginOp::Update { id, scope, .. } => plugin("update", name(id)?, *scope),
            PluginOp::Uninstall { id, scope } => plugin("uninstall", name(id)?, *scope),
            PluginOp::SetEnabled { .. } => Err(AgentError::Protocol(
                "Grok plugin enablement cannot be read back through its CLI".into(),
            )),
            PluginOp::AddMarketplace { source } => marketplace("add", source),
            PluginOp::RemoveMarketplace { name } => marketplace("remove", name),
        }
    }

    fn name(id: &str) -> Result<String, AgentError> {
        operand(split_id(id).0)
    }

    /// A value placed in a positional slot must not be read as an option.
    pub(crate) fn operand(value: &str) -> Result<String, AgentError> {
        if value.is_empty() || value.starts_with('-') {
            return Err(AgentError::Protocol(format!(
                "refusing to pass {value:?} to `grok plugin`"
            )));
        }
        Ok(value.to_string())
    }

    fn split_id(id: &str) -> (&str, Option<&str>) {
        match id.rsplit_once('@') {
            Some((name, marketplace)) if !name.is_empty() => (name, Some(marketplace)),
            _ => (id, None),
        }
    }
}

pub(crate) mod pi {
    use crate::{AgentError, PluginOp, PluginScope};

    /// `--approve` trusts the project for this one command; pi refuses
    /// project packages without it and asks nothing of user ones.
    pub(crate) fn list_args(project: bool) -> Vec<String> {
        let mut args = vec!["list".to_string()];
        if project {
            args.push("--approve".into());
        }
        args
    }

    fn package(verb: &str, source: &str, scope: PluginScope) -> Result<Vec<String>, AgentError> {
        let mut args = vec![verb.to_string(), source_operand(source)?];
        match scope {
            PluginScope::User => {}
            PluginScope::Project => args.extend(["-l".into(), "--approve".into()]),
            other => {
                return Err(AgentError::Protocol(format!(
                    "pi keeps packages for the user or the project, not in the {other:?} scope"
                )));
            }
        }
        Ok(args)
    }

    pub(crate) fn op_args(op: &PluginOp) -> Result<Vec<String>, AgentError> {
        match op {
            PluginOp::Install { id, scope, .. } => package("install", id, *scope),
            PluginOp::Uninstall { id, scope } => package("remove", id, *scope),
            // `pi update <source>` would read a package named `pi` or `self`
            // as pi itself. The update has no scope flag; it finds the source.
            PluginOp::Update { id, scope, .. } => {
                let mut args = vec![
                    "update".to_string(),
                    "--extension".to_string(),
                    source_operand(id)?,
                ];
                if *scope == PluginScope::Project {
                    args.push("--approve".into());
                }
                Ok(args)
            }
            PluginOp::SetEnabled { .. } => Err(AgentError::Protocol(
                "pi enables package resources only in its `pi config` TUI".into(),
            )),
            PluginOp::AddMarketplace { .. } | PluginOp::RemoveMarketplace { .. } => Err(
                AgentError::Protocol("pi installs packages by source, without marketplaces".into()),
            ),
        }
    }

    pub(super) fn source_operand(value: &str) -> Result<String, AgentError> {
        if value.trim().is_empty() || value.starts_with('-') {
            return Err(AgentError::Protocol(format!(
                "refusing to pass {value:?} as a package source"
            )));
        }
        Ok(value.to_string())
    }
}

pub(crate) mod opencode {
    use super::pi::source_operand;
    use crate::{AgentError, PluginOp, PluginScope};

    /// `opencode plugin` changes the global configuration only.
    pub(crate) fn op_args(op: &PluginOp) -> Result<Vec<String>, AgentError> {
        let (verb, id, scope) = match op {
            PluginOp::Install { id, scope, .. } => ("add", id, scope),
            PluginOp::Uninstall { id, scope } => ("remove", id, scope),
            PluginOp::Update { id, scope, .. } => ("update", id, scope),
            PluginOp::SetEnabled { .. } => {
                return Err(AgentError::Protocol(
                    "`opencode plugin` has no way to disable a plugin".into(),
                ));
            }
            PluginOp::AddMarketplace { .. } | PluginOp::RemoveMarketplace { .. } => {
                return Err(AgentError::Protocol(
                    "OpenCode installs plugins by package, without marketplaces".into(),
                ));
            }
        };
        if *scope != PluginScope::User {
            return Err(AgentError::Protocol(
                "`opencode plugin` changes the global configuration only".into(),
            ));
        }
        Ok(vec!["plugin".into(), verb.into(), source_operand(id)?])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginScope;

    /// What the install popover shows is the command the adapter runs, with
    /// the profile's own CLI standing first, quoted as a shell would need it.
    #[test]
    fn the_command_shown_is_the_one_run_under_the_profiles_cli() {
        let install = PluginOp::Install {
            id: "alpha@tcode-probe".into(),
            scope: PluginScope::Project,
            accept_command: None,
        };
        assert_eq!(
            plugin_op_command(ProviderKind::ClaudeCode, None, &install).as_deref(),
            Some("claude plugin install alpha@tcode-probe --scope project --json")
        );
        assert_eq!(
            plugin_op_command(
                ProviderKind::ClaudeCode,
                Some(Path::new("/opt/my tools/claude")),
                &install
            )
            .as_deref(),
            Some("'/opt/my tools/claude' plugin install alpha@tcode-probe --scope project --json")
        );
        // Grok installs for the user only, so a project install runs nothing.
        assert_eq!(plugin_op_command(ProviderKind::Grok, None, &install), None);
        let user = PluginOp::Install {
            id: "alpha@tcode-probe".into(),
            scope: PluginScope::User,
            accept_command: None,
        };
        assert_eq!(
            plugin_op_command(ProviderKind::Grok, None, &user).as_deref(),
            Some("grok plugin install alpha@tcode-probe")
        );
        assert_eq!(plugin_op_command(ProviderKind::Cursor, None, &user), None);
    }
}
