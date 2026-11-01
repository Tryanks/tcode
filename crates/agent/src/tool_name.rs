/// A tool call's name split into the MCP server that provides the tool, when
/// the name says, and the tool's own name. Providers spell the pair
/// differently: Claude Code `mcp__server__tool`, Codex `server/tool`, Grok
/// `server__tool`, others `server.tool`; a built-in tool is a plain name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolName<'a> {
    pub server: Option<&'a str>,
    pub tool: &'a str,
}

impl<'a> ToolName<'a> {
    pub fn parse(name: &'a str) -> Self {
        let pair = name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .or_else(|| name.split_once('/'))
            .or_else(|| name.split_once("__"))
            .or_else(|| name.split_once('.'))
            .filter(|(server, tool)| !server.is_empty() && !tool.is_empty());
        match pair {
            Some((server, tool)) => Self {
                server: Some(server),
                tool,
            },
            None => Self {
                server: None,
                tool: name,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every spelling recorded on the wire names the same server and tool.
    #[test]
    fn provider_spellings_name_the_same_server_and_tool() {
        for (name, server, tool) in [
            // Claude Code.
            (
                "mcp__tcode_computer_use__observe_ui",
                Some("tcode_computer_use"),
                "observe_ui",
            ),
            (
                "mcp__claude_ai_Claude_Docs__batch",
                Some("claude_ai_Claude_Docs"),
                "batch",
            ),
            // Codex app-server.
            (
                "tcode_computer_use/observe_ui",
                Some("tcode_computer_use"),
                "observe_ui",
            ),
            (
                "codex_apps/github.search_installed_repositories_v2",
                Some("codex_apps"),
                "github.search_installed_repositories_v2",
            ),
            // Grok `use_tool`.
            ("tcode_probe__echo_upper", Some("tcode_probe"), "echo_upper"),
            // Built-ins.
            ("Read", None, "Read"),
            ("report_result", None, "report_result"),
            ("mcp__", None, "mcp__"),
        ] {
            assert_eq!(ToolName::parse(name), ToolName { server, tool }, "{name}");
        }
    }
}
