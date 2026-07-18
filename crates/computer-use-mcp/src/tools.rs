use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;

/// Kind of desktop root to match.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    Window,
    Dialog,
    Sheet,
    Menu,
    Popover,
}

/// Observation source to use for a desktop root.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObserveMode {
    Semantic,
    Visual,
    Fused,
}

/// Input action to apply to a state-scoped UI element or screen coordinate.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct UiAction {
    /// Action to perform: press, click, set_text, type_text, keypress, scroll, drag, or move_mouse.
    pub action: UiActionKind,
    /// State-scoped element reference to target, when the action targets an element.
    #[serde(default, rename = "ref")]
    pub r#ref: Option<String>,
    /// Absolute screen x-coordinate, when targeting a coordinate.
    #[serde(default)]
    pub x: Option<f64>,
    /// Absolute screen y-coordinate, when targeting a coordinate.
    #[serde(default)]
    pub y: Option<f64>,
    /// Text to set or type for text-entry actions.
    #[serde(default)]
    pub text: Option<String>,
    /// Key names or chord components for a keypress action.
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    /// Horizontal scroll delta for a scroll action.
    #[serde(default)]
    pub scroll_x: Option<f64>,
    /// Vertical scroll delta for a scroll action.
    #[serde(default)]
    pub scroll_y: Option<f64>,
    /// Absolute screen points describing a drag path.
    #[serde(default)]
    pub path: Option<Vec<[f64; 2]>>,
    /// Mouse button to use for click or drag actions.
    #[serde(default)]
    pub button: Option<MouseButton>,
    /// Number of clicks to issue for a click action.
    #[serde(default)]
    pub click_count: Option<u32>,
}

/// Supported desktop input action.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UiActionKind {
    Press,
    Click,
    SetText,
    TypeText,
    Keypress,
    Scroll,
    Drag,
    MoveMouse,
}

/// Mouse button used by pointer actions.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Condition evaluated against a UI state.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct UiCondition {
    /// State-scoped element reference the condition should match.
    #[serde(default, rename = "ref")]
    pub r#ref: Option<String>,
    /// State-scoped ancestor reference that limits the search scope.
    #[serde(default)]
    pub scope_ref: Option<String>,
    /// Text the matching element should contain.
    #[serde(default)]
    pub text: Option<String>,
    /// Accessibility role the matching element should have.
    #[serde(default)]
    pub role: Option<String>,
    /// Accessibility value the matching element should have.
    #[serde(default)]
    pub value: Option<String>,
    /// Whether the matching element must become present or absent.
    #[serde(default)]
    pub until: Option<ConditionUntil>,
    /// Maximum time to wait for the condition, in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Desired presence state for a UI condition.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConditionUntil {
    Present,
    Absent,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FindRootsParams {
    /// Text to match against application and window titles.
    #[serde(default)]
    text: Option<String>,
    /// Application name to match.
    #[serde(default)]
    app: Option<String>,
    /// Application bundle identifier to match.
    #[serde(default)]
    bundle_id: Option<String>,
    /// Process identifier to match.
    #[serde(default)]
    pid: Option<u32>,
    /// Desktop root kind to match.
    #[serde(default)]
    kind: Option<RootKind>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ObserveUiParams {
    /// Desktop root reference to observe; omitted selects the frontmost root.
    #[serde(default)]
    root: Option<String>,
    /// Observation source: semantic, visual, or fused.
    #[serde(default)]
    mode: Option<ObserveMode>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchUiParams {
    /// Identifier of the cached UI state to search.
    state_id: String,
    /// Text to rank against element names, values, and descriptions.
    #[serde(default)]
    text: Option<String>,
    /// Accessibility role to match.
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExpandUiParams {
    /// Identifier of the cached UI state containing the element.
    state_id: String,
    /// State-scoped element reference to expand.
    #[serde(rename = "ref")]
    r#ref: String,
    /// Maximum descendant depth to include.
    #[serde(default)]
    depth: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct InspectUiParams {
    /// Identifier of the cached UI state containing the element.
    state_id: String,
    /// State-scoped element reference to inspect.
    #[serde(rename = "ref")]
    r#ref: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ActUiParams {
    /// Identifier of the cached UI state against which actions are resolved.
    state_id: String,
    /// Ordered actions to execute as one transaction.
    actions: Vec<UiAction>,
    /// Optional postcondition used to verify the transaction outcome.
    #[serde(default)]
    expect: Option<UiCondition>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadTextParams {
    /// Identifier of the cached UI state containing the text; omitted uses the owning state encoded by the continuation.
    #[serde(default)]
    state_id: Option<String>,
    /// State-scoped element or continuation reference whose text should be read.
    #[serde(rename = "ref")]
    r#ref: String,
    /// Byte offset at which to continue reading.
    #[serde(default)]
    offset: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WaitForParams {
    /// Identifier of the cached UI state that scopes the condition.
    state_id: String,
    /// Condition fields to wait for.
    #[serde(flatten)]
    condition: UiCondition,
}

#[derive(Clone)]
pub struct ComputerUseTools {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ComputerUseTools {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Find and rank desktop window roots, returning state-scoped @rN references."
    )]
    async fn find_roots(
        &self,
        Parameters(params): Parameters<FindRootsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(dispatch::find_roots(params).await)
    }

    #[tool(
        description = "Observe a desktop root and return a folded outline, state_id, and screenshot when requested by the observation mode."
    )]
    async fn observe_ui(
        &self,
        Parameters(params): Parameters<ObserveUiParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(dispatch::observe_ui(params).await)
    }

    #[tool(
        description = "Search and rank elements in a cached UI state by text and accessibility role."
    )]
    async fn search_ui(
        &self,
        Parameters(params): Parameters<SearchUiParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(dispatch::search_ui(params).await)
    }

    #[tool(description = "Expand local outline context around a state-scoped element reference.")]
    async fn expand_ui(
        &self,
        Parameters(params): Parameters<ExpandUiParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(dispatch::expand_ui(params).await)
    }

    #[tool(
        description = "Inspect an element's full accessibility attributes, frame, and supported actions."
    )]
    async fn inspect_ui(
        &self,
        Parameters(params): Parameters<InspectUiParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(dispatch::inspect_ui(params).await)
    }

    #[tool(
        description = "Execute a transaction of desktop input actions against a cached UI state, optionally verifying a postcondition."
    )]
    async fn act_ui(
        &self,
        Parameters(params): Parameters<ActUiParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(dispatch::act_ui(params).await)
    }

    #[tool(description = "Read a bounded page of long text owned by a state-scoped reference.")]
    async fn read_text(
        &self,
        Parameters(params): Parameters<ReadTextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(dispatch::read_text(params).await)
    }

    #[tool(
        description = "Wait for a text, role, value, or referenced UI element to become present or absent."
    )]
    async fn wait_for(
        &self,
        Parameters(params): Parameters<WaitForParams>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(dispatch::wait_for(params).await)
    }
}

#[tool_handler]
impl ServerHandler for ComputerUseTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "Observe and control desktop applications through state-scoped accessibility references."
                    .into(),
            ),
        }
    }
}

pub type Service = StreamableHttpService<ComputerUseTools>;
pub type Services = HashMap<String, Service>;

pub fn service() -> Service {
    StreamableHttpService::new(
        || Ok(ComputerUseTools::new()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

pub async fn serve(
    listener: std::net::TcpListener,
    services: Arc<RwLock<Services>>,
) -> std::io::Result<()> {
    let app = Router::new()
        .route("/mcp", any(handle))
        .with_state(services);
    listener.set_nonblocking(true)?;
    axum::serve(tokio::net::TcpListener::from_std(listener)?, app).await
}

async fn handle(
    State(services): State<Arc<RwLock<Services>>>,
    req: axum::extract::Request,
) -> Response {
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let service = token.and_then(|token| services.read().unwrap().get(token).cloned());
    let Some(service) = service else {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    };
    let response = service.handle(req).await;
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, axum::body::Body::new(body))
}

mod dispatch {
    use super::*;

    const BACKEND_NOT_IMPLEMENTED: &str = "computer use backend not implemented yet";

    pub(super) async fn find_roots(params: FindRootsParams) -> CallToolResult {
        let permissions = permissions();
        let _ = (
            params.text,
            params.app,
            params.bundle_id,
            params.pid,
            params.kind,
        );
        unavailable(permissions, true, false)
    }

    pub(super) async fn observe_ui(params: ObserveUiParams) -> CallToolResult {
        let permissions = permissions();
        let needs_accessibility = !matches!(params.mode, Some(ObserveMode::Visual));
        let needs_screen_recording =
            matches!(params.mode, Some(ObserveMode::Visual | ObserveMode::Fused));
        let _ = params.root;
        unavailable(permissions, needs_accessibility, needs_screen_recording)
    }

    pub(super) async fn search_ui(params: SearchUiParams) -> CallToolResult {
        let permissions = permissions();
        let _ = (params.state_id, params.text, params.role);
        unavailable(permissions, true, false)
    }

    pub(super) async fn expand_ui(params: ExpandUiParams) -> CallToolResult {
        let permissions = permissions();
        let _ = (params.state_id, params.r#ref, params.depth);
        unavailable(permissions, true, false)
    }

    pub(super) async fn inspect_ui(params: InspectUiParams) -> CallToolResult {
        let permissions = permissions();
        let _ = (params.state_id, params.r#ref);
        unavailable(permissions, true, false)
    }

    pub(super) async fn act_ui(params: ActUiParams) -> CallToolResult {
        let permissions = permissions();
        let _ = (params.state_id, params.actions, params.expect);
        unavailable(permissions, true, false)
    }

    pub(super) async fn read_text(params: ReadTextParams) -> CallToolResult {
        let permissions = permissions();
        let _ = (params.state_id, params.r#ref, params.offset);
        unavailable(permissions, true, false)
    }

    pub(super) async fn wait_for(params: WaitForParams) -> CallToolResult {
        let permissions = permissions();
        let _ = (params.state_id, params.condition);
        unavailable(permissions, true, false)
    }

    #[cfg(target_os = "macos")]
    type PermissionSnapshot = crate::permissions::PermissionStatus;

    #[cfg(not(target_os = "macos"))]
    struct PermissionSnapshot;

    #[cfg(target_os = "macos")]
    fn permissions() -> PermissionSnapshot {
        crate::permissions::check()
    }

    #[cfg(not(target_os = "macos"))]
    fn permissions() -> PermissionSnapshot {
        PermissionSnapshot
    }

    #[cfg(target_os = "macos")]
    fn unavailable(
        permissions: PermissionSnapshot,
        needs_accessibility: bool,
        needs_screen_recording: bool,
    ) -> CallToolResult {
        if needs_accessibility && !permissions.accessibility {
            return tool_error(
                "Accessibility permission is missing; grant it in tcode Settings → Computer Use.",
            );
        }
        if needs_screen_recording && !permissions.screen_recording {
            return tool_error(
                "Screen Recording permission is missing; grant it in tcode Settings → Computer Use.",
            );
        }
        tool_error(BACKEND_NOT_IMPLEMENTED)
    }

    #[cfg(not(target_os = "macos"))]
    fn unavailable(
        _permissions: PermissionSnapshot,
        _needs_accessibility: bool,
        _needs_screen_recording: bool,
    ) -> CallToolResult {
        tool_error("computer use is unsupported on this platform")
    }

    fn tool_error(message: &str) -> CallToolResult {
        CallToolResult::error(vec![Content::text(message)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tools_are_registered() {
        let tools = ComputerUseTools::new();
        let mut names: Vec<_> = tools
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "act_ui",
                "expand_ui",
                "find_roots",
                "inspect_ui",
                "observe_ui",
                "read_text",
                "search_ui",
                "wait_for",
            ]
        );
    }
}
