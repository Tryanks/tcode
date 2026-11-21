use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::Deserialize;

/// Maximum accepted HTML payload size (512 KiB).
const MAX_HTML_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct VisualizeInput {
    /// Short, descriptive title shown in the card header.
    pub title: String,
    /// Self-contained static HTML fragment with optional `<style>` and inline `<svg>`.
    /// JavaScript and external URLs are not executed or fetched. Provide either `html` or `path`.
    #[serde(default)]
    pub html: Option<String>,
    /// Optional absolute path to a local `.html` file on the host. When provided, its contents
    /// are read and embedded into the tool result so remote clients and history keep working.
    #[serde(default)]
    pub path: Option<String>,
    /// Optional concise subtitle shown beside or below the title.
    #[serde(default)]
    pub description: Option<String>,
}

pub type Service = StreamableHttpService<VisualizeTools, LocalSessionManager>;
pub type TokenRegistry = mcp_host::TokenRegistry<Service>;

pub struct VisualizeMcpServer {
    pub url: String,
    pub tokens: TokenRegistry,
}

pub fn start(host: &mut mcp_host::Host) -> VisualizeMcpServer {
    let tokens = TokenRegistry::new(move |_session_id| {
        StreamableHttpService::new(
            move || Ok(VisualizeTools::new()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        )
    });
    host.mount(mcp_host::route("/visualize", &tokens));
    VisualizeMcpServer {
        url: host.url("/visualize"),
        tokens,
    }
}

#[derive(Clone)]
pub struct VisualizeTools {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl VisualizeTools {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Show a self-contained static HTML + CSS + inline SVG visual card directly in this conversation thread (pass inline `html` or a host `.html` file `path`). Do not announce the tool or restate everything shown in the visual; keep prose focused on what it helps the user see or decide. Use Markdown tables for simple tables and Mermaid for simple node-edge graphs; use `visualize` for richer charts, metric dashboards, architecture overviews, comparison matrices, timelines, and visual explainers. Layout: fluid width (roughly 360px on phone to 728px on desktop), let content determine height (avoid `vh` and `height: 100%`), leave the outer root without an extra redundant page frame/background. Styling: Flexbox, CSS Grid (including `repeat(auto-fit, minmax(..., 1fr))`), tables, `linear-gradient`, `box-shadow`, `light-dark()`, `color-mix()`, `@media (prefers-color-scheme)`, theme CSS variables (`--background`, `--foreground`, `--card`, `--muted`, `--muted-foreground`, `--border`, `--primary`, `--accent`, `--destructive`, `--success`, `--warning`, `--info`, `--chart-1`..`--chart-6`, `--font-sans`, `--font-mono`, `--radius`), and inline `<svg viewBox=\"...\">` (which also resolves `var(--...)` and `currentColor`). Static only: no JavaScript and no external URLs."
    )]
    async fn visualize(&self, Parameters(input): Parameters<VisualizeInput>) -> CallToolResult {
        render_response(&input)
    }
}

fn render_response(input: &VisualizeInput) -> CallToolResult {
    let title = input.title.trim();
    if title.is_empty() {
        return CallToolResult::error(vec![ContentBlock::text("title must not be empty")]);
    }
    let (html, from_path) = match (&input.html, &input.path) {
        (Some(html), _) if !html.trim().is_empty() => (html.clone(), false),
        (_, Some(path)) if !path.trim().is_empty() => {
            let trimmed = path.trim();
            let p = std::path::Path::new(trimmed);
            if !p.is_absolute() {
                return CallToolResult::error(vec![ContentBlock::text(
                    "path must be an absolute file path",
                )]);
            }
            let meta = match std::fs::metadata(p) {
                Ok(m) => m,
                Err(e) => {
                    return CallToolResult::error(vec![ContentBlock::text(format!(
                        "failed to read html file {trimmed}: {e}"
                    ))]);
                }
            };
            if meta.len() > MAX_HTML_BYTES as u64 {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "html file exceeds maximum size of {MAX_HTML_BYTES} bytes"
                ))]);
            }
            match std::fs::read_to_string(p) {
                Ok(contents) => (contents, true),
                Err(e) => {
                    return CallToolResult::error(vec![ContentBlock::text(format!(
                        "failed to read html file {trimmed}: {e}"
                    ))]);
                }
            }
        }
        _ => {
            return CallToolResult::error(vec![ContentBlock::text(
                "either non-empty `html` or `path` must be provided",
            )]);
        }
    };
    if html.trim().is_empty() {
        return CallToolResult::error(vec![ContentBlock::text("html must not be empty")]);
    }
    if html.len() > MAX_HTML_BYTES {
        return CallToolResult::error(vec![ContentBlock::text(format!(
            "html exceeds maximum size of {MAX_HTML_BYTES} bytes"
        ))]);
    }
    let has_script = contains_script_tag(&html);
    let mut payload = serde_json::json!({
        "rendered": true,
        "title": title,
    });
    // Embed file-backed HTML into the tool result so remote clients and
    // persisted transcripts stay self-contained even if the host file changes.
    if from_path {
        payload["html"] = serde_json::Value::String(html);
    }
    if has_script {
        payload["note"] = serde_json::Value::String(
            "JavaScript (<script>) is not executed; only static HTML, CSS, and inline SVG are rendered.".into(),
        );
    }
    CallToolResult::success(vec![ContentBlock::text(payload.to_string())])
}

fn contains_script_tag(html: &str) -> bool {
    let bytes = html.as_bytes();
    let needle = b"<script";
    if bytes.len() < needle.len() {
        return false;
    }
    bytes.windows(needle.len()).enumerate().any(|(i, window)| {
        window.eq_ignore_ascii_case(needle)
            && bytes
                .get(i + needle.len())
                .is_none_or(|b| b.is_ascii_whitespace() || matches!(b, b'>' | b'/'))
    })
}

/// Every tool's name and description in router order.
pub fn tool_descriptions() -> Vec<(String, String)> {
    VisualizeTools::tool_router()
        .list_all()
        .into_iter()
        .map(|tool| {
            (
                tool.name.to_string(),
                tool.description.as_deref().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// The server's `instructions`, which the client reads at initialization.
pub const INSTRUCTIONS: &str = "\
Proactively call `tcode_visualize` (`visualize`) whenever a chart, metric dashboard, \
architecture overview, comparison matrix, timeline, or visual explainer communicates \
clearer than prose or ASCII art. Prefer standard Markdown tables for plain tables and \
Mermaid fences for simple node-edge diagrams; use `visualize` for rich static layouts \
and custom inline SVG graphics. Do not announce the tool or restate the visual in prose — \
focus text only on key takeaways or decisions. Output self-contained static HTML + `<style>` \
+ inline `<svg viewBox=\"...\">` (no `<script>`, no external URLs, no `vh` or `height: 100%`, \
no redundant outer page frame). Use built-in theme CSS variables (`--background`, `--foreground`, \
`--card`, `--muted`, `--muted-foreground`, `--border`, `--primary`, `--accent`, `--destructive`, \
`--success`, `--warning`, `--info`, `--chart-1`..`--chart-6`, `--font-sans`, `--font-mono`, \
`--radius`) or `light-dark(...)` so visuals adapt automatically to light and dark themes.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for VisualizeTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_title_or_html() {
        let empty_title = render_response(&VisualizeInput {
            title: "  ".into(),
            html: Some("<div>ok</div>".into()),
            path: None,
            description: None,
        });
        assert_eq!(empty_title.is_error, Some(true));

        let empty_html = render_response(&VisualizeInput {
            title: "Chart".into(),
            html: Some("   ".into()),
            path: None,
            description: None,
        });
        assert_eq!(empty_html.is_error, Some(true));
    }

    #[test]
    fn accepts_valid_html_and_warns_when_script_present() {
        let ok = render_response(&VisualizeInput {
            title: "Overview".into(),
            html: Some("<div class=\"card\">hello</div>".into()),
            path: None,
            description: Some("Summary".into()),
        });
        assert_ne!(ok.is_error, Some(true));
        let text = ok.content[0].as_text().unwrap().text.as_str();
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["rendered"], true);
        assert!(value.get("note").is_none());
        assert!(value.get("html").is_none());

        let scripted = render_response(&VisualizeInput {
            title: "With Script".into(),
            html: Some("<div>hi</div><SCRIPT>alert(1)</SCRIPT>".into()),
            path: None,
            description: None,
        });
        assert_ne!(scripted.is_error, Some(true));
        let text = scripted.content[0].as_text().unwrap().text.as_str();
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["rendered"], true);
        assert!(value["note"].as_str().unwrap().contains("JavaScript"));
    }

    #[test]
    fn reads_html_from_path_and_embeds_into_result() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("tcode-vis-test-{}.html", std::process::id()));
        std::fs::write(&file, "<div>from file</div>").unwrap();
        let res = render_response(&VisualizeInput {
            title: "File Visual".into(),
            html: None,
            path: Some(file.to_string_lossy().into_owned()),
            description: None,
        });
        let _ = std::fs::remove_file(&file);
        assert_ne!(res.is_error, Some(true));
        let text = res.content[0].as_text().unwrap().text.as_str();
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["rendered"], true);
        assert_eq!(value["html"], "<div>from file</div>");
    }
}
