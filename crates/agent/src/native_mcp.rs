//! The MCP servers a harness's own configuration lists at user level, read
//! from the file each CLI documents and writes itself. Listing commands are
//! not used: `claude mcp list` and `pi mcp list` connect to every server to
//! report on it. OpenCode's file is not read either: it migrates the `mcp` key
//! to `mcp.servers` and `enabled` to `disabled` as it loads, so its
//! `opencode debug config`, which holds the result and connects to nothing,
//! is the source.
//!
//! Only a server's name, command and origin leave this module: arguments,
//! environments, headers and URL paths are where such configs keep secrets.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::harness::{NativeMcpConfig, NativeMcpServer};
use crate::{LaunchEnv, PluginContext, ProviderKind};

/// What `provider` launched with `launch_env` is configured with, or `None`
/// for a harness with no such file. A file that is not there lists nothing.
/// Blocks, on OpenCode's CLI where that is the source.
pub fn servers(
    provider: ProviderKind,
    binary_path: Option<PathBuf>,
    launch_env: &LaunchEnv,
) -> Option<Result<Vec<NativeMcpServer>, String>> {
    let config = provider.harness().native_mcp?;
    if config == NativeMcpConfig::OpenCodeResolved {
        return Some(opencode_servers(binary_path, launch_env));
    }
    let path = path(provider, launch_env)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(Ok(Vec::new())),
        Err(error) => return Some(Err(format!("{}: {error}", path.display()))),
    };
    Some(parse(config, &text).map_err(|error| format!("{}: {error}", path.display())))
}

fn opencode_servers(
    binary_path: Option<PathBuf>,
    launch_env: &LaunchEnv,
) -> Result<Vec<NativeMcpServer>, String> {
    let context = PluginContext {
        binary_path,
        launch_env: launch_env.clone(),
        // A project's own configuration is not this list's, wherever it runs.
        cwd: launch_env
            .home
            .clone()
            .or_else(std::env::home_dir)
            .unwrap_or_else(std::env::temp_dir),
        project: false,
    };
    let documents = smol::block_on(crate::source_plugins::opencode_global_config(&context))
        .map_err(|error| error.to_string())?;
    Ok(opencode_listed(&documents))
}

/// A later document's server replaces an earlier one of that name, as they
/// merge in OpenCode.
fn opencode_listed(documents: &[Value]) -> Vec<NativeMcpServer> {
    let servers: BTreeMap<_, _> = documents
        .iter()
        .filter_map(|document| document.get("mcp")?.get("servers")?.as_object())
        .flatten()
        .collect();
    servers
        .into_iter()
        .filter_map(|(name, server)| {
            let (remote, target) = match server.get("command") {
                Some(command) => (false, command.get(0)?.as_str()?.to_owned()),
                None => (true, origin(server.get("url")?.as_str()?)),
            };
            Some(NativeMcpServer {
                name: name.clone(),
                remote,
                target,
                enabled: server.get("disabled").and_then(Value::as_bool) != Some(true),
            })
        })
        .collect()
}

fn path(provider: ProviderKind, launch_env: &LaunchEnv) -> Option<PathBuf> {
    match provider {
        ProviderKind::ClaudeCode => crate::claude::account_file(launch_env),
        ProviderKind::Codex => crate::codex::home_dir(launch_env).map(|h| h.join("config.toml")),
        ProviderKind::Pi => crate::pi::agent_dir(launch_env).map(|dir| dir.join("mcp.json")),
        ProviderKind::OpenCode | ProviderKind::Cursor | ProviderKind::Grok | ProviderKind::Acp => {
            None
        }
    }
}

fn parse(config: NativeMcpConfig, text: &str) -> Result<Vec<NativeMcpServer>, String> {
    let (root, key): (Value, _) = match config {
        NativeMcpConfig::OpenCodeResolved => unreachable!("opencode_servers reads the CLI"),
        NativeMcpConfig::McpServersJson => (
            serde_json::from_str(text).map_err(|error| error.to_string())?,
            "mcpServers",
        ),
        NativeMcpConfig::CodexToml => (
            toml::from_str(text).map_err(|error| error.to_string())?,
            "mcp_servers",
        ),
    };
    let Some(servers) = root.get(key).and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    Ok(servers
        .iter()
        .filter_map(|(name, server)| {
            let (remote, target) = match server.get("command") {
                Some(command) => (false, command.as_str()?.to_owned()),
                None => (true, origin(server.get("url")?.as_str()?)),
            };
            Some(NativeMcpServer {
                name: name.clone(),
                remote,
                target,
                enabled: server.get("enabled").and_then(Value::as_bool) != Some(false),
            })
        })
        .collect())
}

/// `https://host:port` of a URL, without its credentials, path or query.
fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    if scheme.is_empty() {
        host.to_owned()
    } else {
        format!("{scheme}://{host}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(name: &str, target: &str, enabled: bool) -> NativeMcpServer {
        NativeMcpServer {
            name: name.into(),
            remote: false,
            target: target.into(),
            enabled,
        }
    }

    fn remote(name: &str, target: &str, enabled: bool) -> NativeMcpServer {
        NativeMcpServer {
            name: name.into(),
            remote: true,
            target: target.into(),
            enabled,
        }
    }

    /// Each input is what that CLI's own `mcp add` wrote: claude 2.1.318,
    /// codex-cli 0.167.0, pi 1.5.0.
    #[test]
    fn each_harness_config_lists_its_servers_without_their_secrets() {
        let claude = r#"{"numStartups": 1, "mcpServers": {
            "probe-stdio": {"type": "stdio", "command": "/usr/bin/true", "args": ["--token=s3cret"],
                "env": {"SECRET": "s3cret"}},
            "probe-http": {"type": "http", "url": "https://user:s3cret@mcp.example.com:8443/mcp?token=s3cret",
                "headers": {"Authorization": "Bearer s3cret"}}
        }}"#;
        let codex = r#"
            [mcp_servers.probe-stdio]
            command = "/usr/bin/true"
            args = ["--flag"]
            [mcp_servers.probe-stdio.env]
            SECRET = "s3cret"
            [mcp_servers.probe-http]
            url = "https://mcp.example.com/mcp"
            enabled = false
        "#;
        let pi = r#"{"mcpServers": {
            "probe-stdio": {"command": "/usr/bin/true", "args": ["--flag"], "env": {"SECRET": "s3cret"}},
            "probe-http": {"url": "https://mcp.example.com/mcp"}
        }}"#;
        for (config, text, http_target, http_enabled) in [
            (
                NativeMcpConfig::McpServersJson,
                claude,
                "https://mcp.example.com:8443",
                true,
            ),
            (
                NativeMcpConfig::CodexToml,
                codex,
                "https://mcp.example.com",
                false,
            ),
            (
                NativeMcpConfig::McpServersJson,
                pi,
                "https://mcp.example.com",
                true,
            ),
        ] {
            let mut listed = parse(config, text).unwrap();
            listed.sort_by(|a, b| b.name.cmp(&a.name));
            assert_eq!(
                listed,
                [
                    local("probe-stdio", "/usr/bin/true", true),
                    remote("probe-http", http_target, http_enabled),
                ]
            );
        }
    }

    /// `opencode debug config --standalone` of 2.2.5, on a file in the old
    /// `mcp` shape and one in `mcp.servers`, each beside a project's document:
    /// one list, with what the CLI masked or left in arguments and URLs kept
    /// back. Its `disabled` is its reading of the old `enabled: false`.
    #[test]
    fn opencode_lists_what_it_resolved_from_either_shape() {
        let old = r#"[
          {"type":"document","path":"/home/me/.config/opencode/opencode.json","info":{"mcp":{"servers":{
            "probe-stdio":{"type":"local","command":["/usr/bin/true","--token=s3cret"],
              "environment":{"TOKEN":"***"}},
            "probe-http":{"type":"remote","url":"https://mcp.example.com:8443/mcp?token=s3cret",
              "headers":{"Authorization":"***"},"disabled":true}
          }}}},
          {"type":"directory","path":"/home/me/.config/opencode"},
          {"type":"document","path":"/work/opencode.json","info":{"mcp":{"servers":{
            "project":{"type":"local","command":["project"]}}}}}
        ]"#;
        let new = r#"[
          {"type":"document","path":"/home/me/.config/opencode/opencode.jsonc","info":{"mcp":{"servers":{
            "probe-stdio":{"type":"local","command":["/usr/bin/true","--token=s3cret"]},
            "probe-http":{"type":"remote","url":"https://mcp.example.com:8443/mcp","disabled":true}
          }}}}
        ]"#;
        for output in [old, new] {
            let documents = crate::source_plugins::global_documents(
                output,
                std::path::Path::new("/home/me/.config/opencode"),
            )
            .unwrap();
            assert_eq!(
                opencode_listed(&documents),
                [
                    remote("probe-http", "https://mcp.example.com:8443", false),
                    local("probe-stdio", "/usr/bin/true", true),
                ]
            );
        }
    }
}
