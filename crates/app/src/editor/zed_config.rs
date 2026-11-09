//! What Zed needs to reach a paired machine through Tcode: an ssh config of
//! Tcode's own naming every paired machine, and one entry per machine in
//! Zed's `ssh_connections` setting that points Zed's ssh at that config, so
//! plain `ssh` and other tools never see the aliases. The machine's
//! authenticity comes from iroh, not from an SSH host key.
use std::{
    io,
    path::{Path, PathBuf},
};

use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstRootNode},
};

/// The host name Zed and ssh know a machine by.
pub(super) fn alias(machine: &str) -> String {
    let prefix: String = machine.chars().take(12).collect();
    format!("tcode-{}", prefix.to_ascii_lowercase())
}

pub(super) fn ssh_config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ssh").join("config")
}

/// Write Tcode's ssh config for `machines`, each reached by running this
/// executable's `ssh-proxy` against the app that owns `data_dir`.
pub(super) fn write_ssh_config(data_dir: &Path, exe: &Path, machines: &[String]) -> io::Result<()> {
    let path = ssh_config_path(data_dir);
    let directory = path.parent().expect("the config sits in a directory");
    std::fs::create_dir_all(directory)?;
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut text =
        String::from("# Written by Tcode for Zed's remote development; Tcode rewrites it.\n");
    for machine in machines {
        text.push_str(&format!(
            "\nHost {alias}\n    ProxyCommand {exe} ssh-proxy {machine} --data-dir {data_dir}\n    StrictHostKeyChecking no\n    UserKnownHostsFile {null}\n    LogLevel ERROR\n    ServerAliveInterval 15\n",
            alias = alias(machine),
            exe = quoted(exe),
            data_dir = quoted(data_dir),
        ));
    }
    if std::fs::read_to_string(&path).is_ok_and(|current| current == text) {
        return Ok(());
    }
    tcode_traverse::identity::write_private(&path, text.as_bytes())
}

/// `path` as one ProxyCommand word: ssh expands `%` itself, and on Unix the
/// command runs through the user's shell.
fn quoted(path: &Path) -> String {
    let path = path.to_string_lossy().replace('%', "%%");
    if cfg!(windows) {
        format!("\"{path}\"")
    } else {
        let mut word = String::from("\"");
        for character in path.chars() {
            if matches!(character, '"' | '\\' | '$' | '`') {
                word.push('\\');
            }
            word.push(character);
        }
        word.push('"');
        word
    }
}

/// Where Zed reads its settings.
pub(super) fn zed_settings_path() -> Option<PathBuf> {
    let config = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("Zed")
    } else if cfg!(target_os = "macos") {
        std::env::home_dir()?.join(".config").join("zed")
    } else {
        std::env::var_os("FLATPAK_XDG_CONFIG_HOME")
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
            .map(PathBuf::from)
            .or_else(|| Some(std::env::home_dir()?.join(".config")))?
            .join("zed")
    };
    Some(config.join("settings.json"))
}

/// Zed's settings `text` with an `ssh_connections` entry for `alias` that
/// hands ssh Tcode's `config`; every other byte as the user or Zed wrote it,
/// including whatever else Zed keeps on that entry. `None` when the entry is
/// already so.
pub(super) fn with_connection(
    text: &str,
    alias: &str,
    nickname: &str,
    config: &Path,
) -> Result<Option<String>, String> {
    let text = if text.trim().is_empty() { "{}" } else { text };
    let root = CstRootNode::parse(text, &ParseOptions::default()).map_err(|e| e.to_string())?;
    let settings = root
        .object_value()
        .ok_or("Zed's settings are not a JSON object")?;
    let connections = settings
        .array_value_or_create("ssh_connections")
        .ok_or("Zed's ssh_connections setting is not a list")?;
    let config = config.to_string_lossy().into_owned();
    let args_value = || {
        CstInputValue::Array(vec![
            CstInputValue::from("-F"),
            CstInputValue::from(config.as_str()),
        ])
    };
    let string = |node: Option<jsonc_parser::cst::CstNode>| {
        node.and_then(|node| node.as_string_lit())
            .and_then(|literal| literal.decoded_value().ok())
    };
    for element in connections.elements() {
        let Some(entry) = element.as_object() else {
            continue;
        };
        if string(entry.get("host").and_then(|host| host.value())).as_deref() != Some(alias) {
            continue;
        }
        let args = entry.get("args");
        let current: Option<Vec<String>> =
            args.as_ref()
                .and_then(|args| args.array_value())
                .map(|args| {
                    args.elements()
                        .into_iter()
                        .filter_map(|arg| string(Some(arg)))
                        .collect()
                });
        if current.as_deref() == Some(&["-F".to_owned(), config.clone()][..]) {
            return Ok(None);
        }
        match args {
            Some(args) => args.set_value(args_value()),
            None => {
                entry.append("args", args_value());
            }
        }
        return Ok(Some(root.to_string()));
    }
    connections.append(CstInputValue::Object(vec![
        ("host".to_owned(), alias.into()),
        ("nickname".to_owned(), nickname.into()),
        ("args".to_owned(), args_value()),
    ]));
    Ok(Some(root.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zed_settings_gain_our_entry_and_keep_everything_the_user_and_zed_wrote() {
        let config = Path::new("/data/ssh/config");
        let user = r#"{
  // My theme.
  "theme": "One Dark",
  "ssh_connections": [
    { "host": "box", "args": ["-p", "2222"] }, // a server of mine
    {
      "host": "tcode-abc",
      "args": ["-F", "/old/ssh/config"],
      "projects": [{ "paths": ["/srv/app"] }]
    }
  ]
}
"#;
        let updated = with_connection(user, "tcode-abc", "Studio", config)
            .unwrap()
            .unwrap();
        assert_eq!(
            updated,
            r#"{
  // My theme.
  "theme": "One Dark",
  "ssh_connections": [
    { "host": "box", "args": ["-p", "2222"] }, // a server of mine
    {
      "host": "tcode-abc",
      "args": ["-F", "/data/ssh/config"],
      "projects": [{ "paths": ["/srv/app"] }]
    }
  ]
}
"#
        );
        assert_eq!(
            with_connection(&updated, "tcode-abc", "Studio", config),
            Ok(None),
            "an entry already right is left alone"
        );

        let added = with_connection("// nothing yet\n{}\n", "tcode-def", "Laptop", config)
            .unwrap()
            .unwrap();
        assert!(added.starts_with("// nothing yet\n{"), "{added}");
        let value: serde_json::Value =
            serde_json::from_str(added.trim_start_matches("// nothing yet\n")).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "ssh_connections": [{
                "host": "tcode-def",
                "nickname": "Laptop",
                "args": ["-F", "/data/ssh/config"],
            }]})
        );
    }
}
