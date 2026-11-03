/// A tool call's name split into the MCP server that provides the tool, when
/// the name says, and the tool's own name. Claude Code spells the pair
/// `mcp__server__tool`; every other adapter names it `server/tool` (Codex
/// does on the wire, the OpenCode and Grok adapters rename their own
/// spellings to it). A built-in tool is a plain name, whatever it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolName<'a> {
    pub server: Option<&'a str>,
    pub tool: &'a str,
}

impl<'a> ToolName<'a> {
    pub fn parse(name: &'a str) -> Self {
        let pair = match name.strip_prefix("mcp__") {
            Some(rest) => rest.split_once("__"),
            None => name.split_once('/'),
        }
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

    /// Grok's `server__tool` spelling of an MCP tool as `server/tool`;
    /// `None` for a name not in that spelling.
    pub fn slashed(name: &str) -> Option<String> {
        if name.starts_with("mcp__") || name.contains('/') {
            return None;
        }
        let (server, tool) = name.split_once("__")?;
        (!server.is_empty() && !tool.is_empty()).then(|| format!("{server}/{tool}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every spelling recorded on the wire names the same server and tool,
    /// and a plain name is a built-in whatever it holds.
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
            // Codex app-server, and OpenCode and Grok as their adapters name them.
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
            ("tcode_probe/echo_upper", Some("tcode_probe"), "echo_upper"),
            // Built-ins.
            ("Read", None, "Read"),
            ("report_result", None, "report_result"),
            ("mcp__", None, "mcp__"),
            ("mcp__tcode_control", None, "mcp__tcode_control"),
            ("foo__bar", None, "foo__bar"),
            ("a.b", None, "a.b"),
            ("Read file.txt", None, "Read file.txt"),
        ] {
            assert_eq!(ToolName::parse(name), ToolName { server, tool }, "{name}");
        }
    }
}
