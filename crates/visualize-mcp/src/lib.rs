use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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

const MAX_HTML_BYTES: usize = agent::McpRegistration::VISUALIZE_MAX_HTML_BYTES;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct VisualizeInput {
    /// Short name of the visual. It is not drawn with the page; it names the call where the
    /// page cannot be shown.
    pub title: String,
    /// Self-contained static HTML fragment with optional `<style>` and inline `<svg>`.
    /// JavaScript and external URLs are not executed or fetched. Provide either `html` or `path`.
    #[serde(default)]
    pub html: Option<String>,
    /// Absolute path of an HTML file on the host, for a page too long to pass inline: write
    /// the file, then pass its path. Tcode reads the file itself each time the visual is shown,
    /// so leave the file in place; its contents are not returned to you. Ignored when `html`
    /// is given.
    #[serde(default)]
    pub path: Option<String>,
}

pub type Service = StreamableHttpService<VisualizeTools, LocalSessionManager>;
pub type TokenRegistry = mcp_host::TokenRegistry<Service>;

/// Whether threads are given the tool, as the host's settings say now. The server outlives
/// any one reading of them, so the host writes each change here.
#[derive(Clone)]
pub struct Offered(Arc<AtomicBool>);

impl Offered {
    pub fn set(&self, offered: bool) {
        self.0.store(offered, Ordering::Relaxed);
    }

    fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

pub struct VisualizeMcpServer {
    pub url: String,
    pub tokens: TokenRegistry,
    pub offered: Offered,
}

pub fn start(host: &mut mcp_host::Host) -> VisualizeMcpServer {
    let offered = Offered(Arc::new(AtomicBool::new(true)));
    let tokens = TokenRegistry::new({
        let offered = offered.clone();
        move |_session_id| {
            let offered = offered.clone();
            StreamableHttpService::new(
                move || Ok(VisualizeTools::new(offered.clone())),
                Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default(),
            )
        }
    });
    host.mount(mcp_host::route("/visualize", &tokens));
    VisualizeMcpServer {
        url: host.url("/visualize"),
        tokens,
        offered,
    }
}

#[derive(Clone)]
pub struct VisualizeTools {
    tool_router: ToolRouter<Self>,
    offered: Offered,
}

#[tool_router]
impl VisualizeTools {
    /// A thread registered while the setting was on keeps its endpoint when it is turned
    /// off: a connection opened then lists no tool, and one opened before is refused its
    /// calls.
    fn new(offered: Offered) -> Self {
        let mut tool_router = Self::tool_router();
        if !offered.get() {
            for tool in tool_router.list_all() {
                tool_router.remove_route(&tool.name);
            }
        }
        Self {
            tool_router,
            offered,
        }
    }

    #[tool(
        description = "Show a self-contained static HTML + CSS + inline SVG visual directly in this conversation thread. It is drawn straight into the reply column, on the thread's own background, with no frame, title bar or padding around it. Pass the page inline as `html`, or for a very long page write it to a file on the host and pass that file's absolute `path`: Tcode reads the file itself to draw it, its contents are not returned to you, and the file must stay in place for the visual to show. Do not announce the tool or restate everything shown in the visual; keep prose focused on what it helps the user see or decide. Use Markdown tables for simple tables and Mermaid for simple node-edge graphs; use `visualize` for richer charts, metric dashboards, architecture overviews, comparison matrices, timelines, and visual explainers. Layout: fluid width (roughly 360px on phone to 720px on desktop), let content determine height (avoid `vh` and `height: 100%`). Leave the outermost element with no background, border, banner title, margin or padding: start with the content. Look: flat and quiet, like the app around it. Do not draw cards: no box with its own background, border or shadow around a group of content, at any level, and no padding or margin added to make one. Set groups apart with whitespace, a small heading, or a 1px `var(--border)` rule between them, and let numbers, tables and charts sit directly on the thread's background. Colour is for data (a chart series, a status), not decoration: no decorative gradients, glows or heavy shadows, and text stays `var(--foreground)` or `var(--muted-foreground)`. Prefer few sizes and weights, generous spacing, and alignment over boxes inside boxes. Styling: Flexbox, CSS Grid (including `repeat(auto-fit, minmax(..., 1fr))`), tables, `light-dark()`, `color-mix()`, `@media (prefers-color-scheme)`, theme CSS variables (`--background`, `--foreground`, `--card`, `--muted`, `--muted-foreground`, `--border`, `--primary`, `--accent`, `--destructive`, `--success`, `--warning`, `--info`, `--chart-1`..`--chart-6`, `--font-sans`, `--font-mono`, `--radius`), and inline `<svg viewBox=\"...\">` (which also resolves `var(--...)` and `currentColor`). Static only: no JavaScript and no external URLs."
    )]
    async fn visualize(&self, Parameters(input): Parameters<VisualizeInput>) -> CallToolResult {
        self.answer(&input)
    }

    fn answer(&self, input: &VisualizeInput) -> CallToolResult {
        if !self.offered.get() {
            return CallToolResult::error(vec![ContentBlock::text(
                "tcode_visualize is turned off in Settings → Control",
            )]);
        }
        render_response(input)
    }
}

fn render_response(input: &VisualizeInput) -> CallToolResult {
    let refused = |reason: String| CallToolResult::error(vec![ContentBlock::text(reason)]);
    let title = input.title.trim();
    if title.is_empty() {
        return refused("title must not be empty".into());
    }
    let inline = input.html.as_deref().filter(|html| !html.trim().is_empty());
    let path = input.path.as_deref().map(str::trim).unwrap_or_default();
    let html = match inline {
        Some(html) if html.len() > MAX_HTML_BYTES => {
            return refused(format!(
                "html exceeds maximum size of {MAX_HTML_BYTES} bytes"
            ));
        }
        Some(html) => std::borrow::Cow::Borrowed(html),
        None if path.is_empty() => {
            return refused("either non-empty `html` or `path` must be provided".into());
        }
        None => match read_page(std::path::Path::new(path)) {
            Ok(html) => std::borrow::Cow::Owned(html),
            Err(reason) => return refused(format!("cannot show {path}: {reason}")),
        },
    };
    let mut payload = serde_json::json!({
        "rendered": true,
        "title": title,
    });
    if contains_script_tag(&html) {
        payload["note"] = serde_json::Value::String(
            "JavaScript (<script>) is not executed; only static HTML, CSS, and inline SVG are rendered.".into(),
        );
    }
    CallToolResult::success(vec![ContentBlock::text(payload.to_string())])
}

/// The page in the file at `path`. The client reads the file itself to draw it; it is read
/// here so the model is told now when there would be nothing to draw.
fn read_page(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read as _;

    if !path.is_absolute() {
        return Err("path must be an absolute file path".into());
    }
    // Before it is opened: opening a FIFO waits for a writer, and a device has no end.
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_HTML_BYTES as u64 + 1).read_to_end(&mut bytes))
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_HTML_BYTES {
        return Err(format!(
            "file exceeds maximum size of {MAX_HTML_BYTES} bytes"
        ));
    }
    let html = String::from_utf8(bytes).map_err(|_| "file is not UTF-8 text".to_owned())?;
    if html.trim().is_empty() {
        return Err("file is empty".into());
    }
    Ok(html)
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
and custom inline SVG graphics. Do not announce the tool or restate the visual in prose: \
focus text only on key takeaways or decisions. The tool's own description says how to \
pass a page and how it should look.";

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
        });
        assert_eq!(empty_title.is_error, Some(true));

        let empty_html = render_response(&VisualizeInput {
            title: "Chart".into(),
            html: Some("   ".into()),
            path: None,
        });
        assert_eq!(empty_html.is_error, Some(true));
    }

    #[test]
    fn accepts_valid_html_and_warns_when_script_present() {
        let ok = render_response(&VisualizeInput {
            title: "Overview".into(),
            html: Some("<div class=\"card\">hello</div>".into()),
            path: None,
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
        });
        assert_ne!(scripted.is_error, Some(true));
        let text = scripted.content[0].as_text().unwrap().text.as_str();
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["rendered"], true);
        assert!(value["note"].as_str().unwrap().contains("JavaScript"));
    }

    /// A thread that started with the tool keeps its connection when the setting is turned
    /// off; from then on its calls are refused.
    #[test]
    fn a_call_made_once_the_setting_is_off_is_refused() {
        let offered = Offered(Arc::new(AtomicBool::new(true)));
        let tools = VisualizeTools::new(offered.clone());
        let input = VisualizeInput {
            title: "Overview".into(),
            html: Some("<div>hello</div>".into()),
            path: None,
        };
        assert_ne!(tools.answer(&input).is_error, Some(true));

        offered.set(false);
        assert_eq!(tools.answer(&input).is_error, Some(true));
    }

    /// A directory of the test's own.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tcode-visualize-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn from_file(path: &std::path::Path) -> CallToolResult {
        render_response(&VisualizeInput {
            title: "File Visual".into(),
            html: None,
            path: Some(path.to_string_lossy().into_owned()),
        })
    }

    /// A page passed as a file is for the reader: none of it goes back to the model.
    #[test]
    fn a_file_page_is_accepted_without_returning_its_contents() {
        let dir = scratch("accepted");
        let page = dir.join("page.html");
        std::fs::write(&page, "<div>from file</div>").unwrap();
        let result = from_file(&page);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_ne!(result.is_error, Some(true));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(text).unwrap(),
            serde_json::json!({ "rendered": true, "title": "File Visual" })
        );
    }

    #[test]
    fn a_path_that_is_no_readable_page_is_refused() {
        let dir = scratch("refused");
        let oversized = dir.join("oversized.html");
        std::fs::write(&oversized, "a".repeat(MAX_HTML_BYTES + 1)).unwrap();
        let refused = [
            ("a directory", from_file(&dir)),
            ("a missing file", from_file(&dir.join("missing.html"))),
            ("an oversized file", from_file(&oversized)),
        ];
        std::fs::remove_dir_all(&dir).unwrap();

        for (what, result) in refused {
            assert_eq!(result.is_error, Some(true), "{what} was accepted");
        }
    }
}
