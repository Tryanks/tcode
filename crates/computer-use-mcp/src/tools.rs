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

#[derive(Debug, serde::Serialize)]
pub struct SmokeStep {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, serde::Serialize)]
pub struct SmokeVerdict {
    pub steps: Vec<SmokeStep>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub struct SmokeRun {
    pub verdict: SmokeVerdict,
    pub exit_code: i32,
}

/// Scripted TextEdit pass used by `cu-smoke`; it invokes the same dispatch
/// functions as MCP calls, with one direct global Cmd+N bootstrap for
/// TextEdit's no-window launch state.
pub async fn run_smoke() -> SmokeRun {
    dispatch::run_smoke().await
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
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use base64::Engine as _;
    use serde_json::json;

    use crate::backend::{
        ActionKind as BackendActionKind, ActionOutcome, ActionRequest, ActionResult, Backend,
        CapturePolicy, MouseButton as BackendMouseButton, ObserveRequest, RootFilters, RootInfo,
        RootKind as BackendRootKind, RootObservation,
    };
    use crate::outline::{self, UiNode};

    const DEFAULT_TIMEOUT_MS: u64 = 3_000;
    const MAX_TIMEOUT_MS: u64 = 30_000;
    const POLL_INTERVAL_MS: u64 = 100;

    #[derive(Default)]
    struct RootRegistry {
        next_ref: u64,
        by_identity: HashMap<String, String>,
        by_ref: HashMap<String, RootInfo>,
    }

    impl RootRegistry {
        fn refresh(&mut self, roots: Vec<RootInfo>) -> Vec<RootInfo> {
            if self.next_ref == 0 {
                self.next_ref = 1;
            }
            roots
                .into_iter()
                .map(|mut root| {
                    let identity = root.identity();
                    let ref_id = self
                        .by_identity
                        .entry(identity)
                        .or_insert_with(|| {
                            let ref_id = format!("@r{}", self.next_ref);
                            self.next_ref += 1;
                            ref_id
                        })
                        .clone();
                    root.ref_id.clone_from(&ref_id);
                    self.by_ref.insert(ref_id, root.clone());
                    root
                })
                .collect()
        }

        fn get(&self, ref_id: &str) -> Option<RootInfo> {
            self.by_ref.get(ref_id).cloned()
        }
    }

    static ROOTS: OnceLock<Mutex<RootRegistry>> = OnceLock::new();
    static OBSERVATION_TRANSACTION: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

    fn roots() -> &'static Mutex<RootRegistry> {
        ROOTS.get_or_init(|| Mutex::new(RootRegistry::default()))
    }

    fn observation_transaction() -> &'static tokio::sync::Mutex<()> {
        OBSERVATION_TRANSACTION.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    pub(super) async fn run_smoke() -> SmokeRun {
        #[cfg(not(target_os = "macos"))]
        {
            SmokeRun {
                verdict: SmokeVerdict {
                    steps: Vec::new(),
                    ok: false,
                    reason: Some("unsupported platform: computer use requires macOS".into()),
                },
                exit_code: 2,
            }
        }

        #[cfg(target_os = "macos")]
        {
            let permissions = crate::permissions::check();
            if !permissions.all_granted() {
                let mut missing = Vec::new();
                if !permissions.accessibility {
                    missing.push("accessibility");
                }
                if !permissions.screen_recording {
                    missing.push("screen_recording");
                }
                return SmokeRun {
                    verdict: SmokeVerdict {
                        steps: Vec::new(),
                        ok: false,
                        reason: Some(format!("missing permissions: {}", missing.join(", "))),
                    },
                    exit_code: 2,
                };
            }

            let mut steps = Vec::new();
            let open_status = tcode_services::process::command("open")
                .arg("-a")
                .arg("TextEdit")
                .status();
            match open_status {
                Ok(status) if status.success() => steps.push(SmokeStep {
                    name: "launch_textedit".into(),
                    ok: true,
                    detail: status.to_string(),
                }),
                Ok(status) => {
                    return smoke_failure(
                        steps,
                        "launch_textedit",
                        format!("open exited with {status}"),
                        1,
                    );
                }
                Err(error) => {
                    return smoke_failure(
                        steps,
                        "launch_textedit",
                        format!("failed to spawn open: {error}"),
                        1,
                    );
                }
            }
            tokio::time::sleep(Duration::from_millis(700)).await;

            // TextEdit can launch with no document and therefore no root. A
            // global Cmd+N is the only action that does not need a state ref.
            let backend = crate::backend::platform_backend();
            let bootstrap = ActionRequest {
                kind: BackendActionKind::Keypress,
                target_path: None,
                target_frame: None,
                target_role: None,
                target_title: None,
                target_actions: Vec::new(),
                x: None,
                y: None,
                text: None,
                keys: Some(vec!["cmd+n".into()]),
                scroll_x: None,
                scroll_y: None,
                path: None,
                button: BackendMouseButton::Left,
                click_count: 1,
            };
            match backend.perform_action(&RootInfo::default(), &bootstrap) {
                Ok(result) if result.outcome != ActionOutcome::Didnt => steps.push(SmokeStep {
                    name: "fresh_document".into(),
                    ok: true,
                    detail: result.message,
                }),
                Ok(result) => {
                    return smoke_failure(steps, "fresh_document", result.message, 1);
                }
                Err(error) => {
                    return smoke_failure(steps, "fresh_document", error.to_string(), 1);
                }
            }
            tokio::time::sleep(Duration::from_millis(700)).await;

            let roots_result = find_roots(FindRootsParams {
                text: None,
                app: Some("TextEdit".into()),
                bundle_id: None,
                pid: None,
                kind: Some(RootKind::Window),
            })
            .await;
            let roots_text = match successful_text(&roots_result) {
                Ok(text) => text,
                Err(error) => return smoke_failure(steps, "find_roots", error, 1),
            };
            let root_ref = roots_text
                .lines()
                .find_map(|line| line.trim_start().strip_prefix("@r"))
                .and_then(|suffix| suffix.split_whitespace().next())
                .map(|suffix| format!("@r{suffix}"));
            let Some(root_ref) = root_ref else {
                return smoke_failure(
                    steps,
                    "find_roots",
                    "no TextEdit window root was found".into(),
                    1,
                );
            };
            steps.push(SmokeStep {
                name: "find_roots".into(),
                ok: true,
                detail: root_ref.clone(),
            });

            let observe_result = observe_ui(ObserveUiParams {
                root: Some(root_ref),
                mode: Some(ObserveMode::Semantic),
            })
            .await;
            let observe_text = match successful_text(&observe_result) {
                Ok(text) => text,
                Err(error) => return smoke_failure(steps, "observe_ui", error, 1),
            };
            let Some(state_id) = extract_state_id(&observe_text) else {
                return smoke_failure(
                    steps,
                    "observe_ui",
                    "observe_ui response had no state_id".into(),
                    1,
                );
            };
            let observation = match crate::state::global().lock().unwrap().get(&state_id) {
                Ok(observation) => observation,
                Err(error) => {
                    return smoke_failure(steps, "observe_ui", error.to_string(), 1);
                }
            };
            let Some(target_ref) = editable_ref(&observation.tree) else {
                return smoke_failure(
                    steps,
                    "observe_ui",
                    "TextEdit tree contained no editable text element".into(),
                    1,
                );
            };
            steps.push(SmokeStep {
                name: "observe_ui".into(),
                ok: true,
                detail: format!("state_id={state_id} target={target_ref}"),
            });

            let nonce = format!("tcode-cu-smoke-{}", uuid::Uuid::new_v4().simple());
            let act_result = act_ui(ActUiParams {
                state_id,
                actions: vec![UiAction {
                    action: UiActionKind::TypeText,
                    r#ref: Some(target_ref),
                    x: None,
                    y: None,
                    text: Some(nonce.clone()),
                    keys: None,
                    scroll_x: None,
                    scroll_y: None,
                    path: None,
                    button: None,
                    click_count: None,
                }],
                expect: Some(UiCondition {
                    r#ref: None,
                    scope_ref: None,
                    text: Some(nonce.clone()),
                    role: None,
                    value: None,
                    until: Some(ConditionUntil::Present),
                    timeout_ms: Some(5_000),
                }),
            })
            .await;
            let act_text = match successful_text(&act_result) {
                Ok(text) => text,
                Err(error) => return smoke_failure(steps, "act_ui", error, 1),
            };
            let Some(successor_id) = extract_state_id(&act_text) else {
                return smoke_failure(
                    steps,
                    "act_ui",
                    "act_ui response had no successor state_id".into(),
                    1,
                );
            };
            if act_text.contains("\"outcome\": \"didnt\"") {
                return smoke_failure(steps, "act_ui", act_text, 1);
            }
            steps.push(SmokeStep {
                name: "act_ui".into(),
                ok: true,
                detail: format!("state_id={successor_id} nonce={nonce}"),
            });

            let wait_result = wait_for(WaitForParams {
                state_id: successor_id,
                condition: UiCondition {
                    r#ref: None,
                    scope_ref: None,
                    text: Some(nonce),
                    role: None,
                    value: None,
                    until: Some(ConditionUntil::Present),
                    timeout_ms: Some(3_000),
                },
            })
            .await;
            let wait_text = match successful_text(&wait_result) {
                Ok(text) => text,
                Err(error) => return smoke_failure(steps, "wait_for", error, 1),
            };
            if !wait_text.contains("\"status\": \"matched\"") {
                return smoke_failure(steps, "wait_for", wait_text, 1);
            }
            steps.push(SmokeStep {
                name: "wait_for".into(),
                ok: true,
                detail: extract_state_id(&wait_text).unwrap_or_else(|| "matched".into()),
            });
            SmokeRun {
                verdict: SmokeVerdict {
                    steps,
                    ok: true,
                    reason: None,
                },
                exit_code: 0,
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn smoke_failure(
        mut steps: Vec<SmokeStep>,
        name: &str,
        detail: String,
        exit_code: i32,
    ) -> SmokeRun {
        steps.push(SmokeStep {
            name: name.into(),
            ok: false,
            detail: detail.clone(),
        });
        SmokeRun {
            verdict: SmokeVerdict {
                steps,
                ok: false,
                reason: Some(detail),
            },
            exit_code,
        }
    }

    #[cfg(target_os = "macos")]
    fn successful_text(result: &CallToolResult) -> Result<String, String> {
        let text = result
            .content
            .iter()
            .find_map(|content| content.as_text())
            .map(|content| content.text.clone())
            .unwrap_or_else(|| "tool returned no text content".into());
        if result.is_error == Some(true) {
            Err(text)
        } else {
            Ok(text)
        }
    }

    #[cfg(target_os = "macos")]
    fn extract_state_id(text: &str) -> Option<String> {
        for line in text.lines() {
            let line = line.trim();
            if let Some(state_id) = line.strip_prefix("state_id:") {
                return Some(state_id.trim().to_string());
            }
            if line.starts_with("\"state_id\"") {
                return line.split('"').nth(3).map(str::to_string);
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    fn editable_ref(tree: &UiNode) -> Option<String> {
        fn collect(node: &UiNode, candidates: &mut Vec<(bool, String)>) {
            if matches!(
                outline::canonical_role(&node.role).as_str(),
                "text_area" | "text_field" | "search_field"
            ) && node.enabled
            {
                candidates.push((node.focused, node.ref_id.clone()));
            }
            for child in &node.children {
                collect(child, candidates);
            }
        }
        let mut candidates = Vec::new();
        collect(tree, &mut candidates);
        candidates.sort_by_key(|(focused, _)| !focused);
        candidates.into_iter().next().map(|(_, ref_id)| ref_id)
    }

    pub(super) async fn find_roots(params: FindRootsParams) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let backend = crate::backend::platform_backend();
        let filters = RootFilters {
            text: params.text,
            app: params.app,
            bundle_id: params.bundle_id,
            pid: params.pid,
            kind: params.kind.map(root_kind),
        };
        let discovered = match backend.list_roots(&filters) {
            Ok(roots) => roots,
            Err(error) => return backend_error(error),
        };
        let roots = roots().lock().unwrap().refresh(discovered);
        let mut lines = vec![format!("roots: {} (frontmost first)", roots.len())];
        for root in roots {
            lines.push(format!(
                "{} {} app=\"{}\" bundle_id=\"{}\" pid={} title=\"{}\" window_id={} frame=({:.0},{:.0},{:.0},{:.0})",
                root.ref_id,
                root.kind,
                escaped(&root.app_name),
                escaped(&root.bundle_id),
                root.pid,
                escaped(&root.title),
                root.window_id,
                root.frame.x,
                root.frame.y,
                root.frame.w,
                root.frame.h
            ));
        }
        bounded_success(None, lines.join("\n"), Vec::new())
    }

    pub(super) async fn observe_ui(params: ObserveUiParams) -> CallToolResult {
        let permissions = permissions();
        let needs_accessibility = !matches!(params.mode, Some(ObserveMode::Visual));
        let needs_screen_recording =
            matches!(params.mode, Some(ObserveMode::Visual | ObserveMode::Fused));
        if let Some(result) =
            permission_gate(permissions, needs_accessibility, needs_screen_recording)
        {
            return result;
        }
        let config = crate::config::get();
        if config.image_mode == crate::config::ImageMode::Always
            && let Some(result) = permission_gate(permissions, false, true)
        {
            return result;
        }
        let _transaction = observation_transaction().lock().await;
        let backend = crate::backend::platform_backend();
        let root = match resolve_root(backend.as_ref(), params.root.as_deref()) {
            Ok(root) => root,
            Err(result) => return *result,
        };
        let capture = capture_policy(config.image_mode, params.mode, &permissions);
        let request = ObserveRequest {
            semantic: !matches!(params.mode, Some(ObserveMode::Visual)),
            capture,
        };
        let observed = match backend.observe(&root, request) {
            Ok(observed) => observed,
            Err(error) => return backend_error(error),
        };
        save_observation(observed, capture_warning(config.image_mode, &permissions))
    }

    pub(super) async fn search_ui(params: SearchUiParams) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let observation = match crate::state::global().lock().unwrap().get(&params.state_id) {
            Ok(observation) => observation,
            Err(error) => return tool_error(&error.to_string()),
        };
        let results = outline::search(
            &observation.tree,
            params.text.as_deref(),
            params.role.as_deref(),
        );
        let mut lines = vec![format!(
            "state_id: {}\nmatches: {} (showing {})",
            observation.state_id,
            results.total,
            results.matches.len()
        )];
        for result in results.matches {
            lines.push(format!(
                "score={} {}",
                result.score,
                outline::render_line(result.node, 0)
            ));
        }
        bounded_success(Some(&observation.state_id), lines.join("\n"), Vec::new())
    }

    pub(super) async fn expand_ui(params: ExpandUiParams) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let observation = match crate::state::global().lock().unwrap().get(&params.state_id) {
            Ok(observation) => observation,
            Err(error) => return tool_error(&error.to_string()),
        };
        let depth = params.depth.unwrap_or(3).min(12) as usize;
        let expanded = match outline::render_expanded(&observation.tree, &params.r#ref, depth) {
            Ok(expanded) => expanded,
            Err(error) => return tool_error(&error),
        };
        bounded_success(
            Some(&observation.state_id),
            format!("state_id: {}\n{expanded}", observation.state_id),
            Vec::new(),
        )
    }

    pub(super) async fn inspect_ui(params: InspectUiParams) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let observation = match crate::state::global().lock().unwrap().get(&params.state_id) {
            Ok(observation) => observation,
            Err(error) => return tool_error(&error.to_string()),
        };
        let Some(node) = observation.tree.find(&params.r#ref) else {
            return tool_error(&crate::state::StateError::UnknownElement(params.r#ref).to_string());
        };
        let value = json!({
            "state_id": observation.state_id,
            "ref": node.ref_id,
            "role": node.role,
            "title": node.title,
            "value": node.value,
            "description": node.description,
            "frame": node.frame,
            "actions": node.actions,
            "enabled": node.enabled,
            "focused": node.focused,
            "child_count": node.children.len(),
        });
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        bounded_success(Some(&observation.state_id), text, Vec::new())
    }

    pub(super) async fn act_ui(params: ActUiParams) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        if !crate::config::get().allow_input {
            return tool_error(
                "observe-only mode is enabled in Settings → Computer Use; input actions are disabled",
            );
        }
        if params.actions.is_empty() {
            return tool_error("act_ui requires at least one action");
        }
        let _transaction = observation_transaction().lock().await;
        let previous = match crate::state::global()
            .lock()
            .unwrap()
            .validate_for_action(&params.state_id)
        {
            Ok(observation) => observation,
            Err(error) => return tool_error(&error.to_string()),
        };
        let backend = crate::backend::platform_backend();
        let mut step_results = Vec::new();
        let mut stopped_at = None;
        for (index, action) in params.actions.iter().enumerate() {
            let result = match prepare_action(&previous.tree, action) {
                Ok(request) => backend
                    .perform_action(&previous.root, &request)
                    .unwrap_or_else(|error| ActionResult::didnt(error.to_string())),
                Err(error) => ActionResult::didnt(error),
            };
            let didnt = result.outcome == ActionOutcome::Didnt;
            step_results.push(json!({
                "index": index + 1,
                "action": action_name(action.action),
                "outcome": result.outcome,
                "message": result.message,
            }));
            if didnt {
                stopped_at = Some(index + 1);
                break;
            }
        }

        let expectation_preexisting = params
            .expect
            .as_ref()
            .is_some_and(|condition| condition_satisfied(&previous.tree, condition));
        let (mut successor, expectation_status, root_changed) = match poll_successor(
            backend.as_ref(),
            &previous,
            params.expect.as_ref(),
            expectation_preexisting,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => return backend_error(error),
        };
        outline::assign_refs_from_previous(&previous.tree, &mut successor.tree);
        let successor = crate::state::global().lock().unwrap().insert_observation(
            successor.root,
            successor.tree,
            successor.screenshot_png,
        );
        let diff = outline::diff_trees(&previous.tree, &successor.tree);
        let expectation_failed = expectation_status == "failed";
        let any_unknown = step_results
            .iter()
            .any(|result| result["outcome"] == "unknown");
        let outcome = if stopped_at.is_some() || expectation_failed {
            "didnt"
        } else if params.expect.is_some() && expectation_status == "verified" {
            "worked"
        } else if any_unknown {
            "unknown"
        } else {
            "worked"
        };
        let report = json!({
            "state_id": successor.state_id,
            "previous_state_id": previous.state_id,
            "outcome": outcome,
            "stopped_at": stopped_at,
            "steps": step_results,
            "expect": expectation_status,
            "root_changed": root_changed,
            "diff_confidence": diff.confidence,
        });
        let mut text = serde_json::to_string_pretty(&report).unwrap_or_else(|_| report.to_string());
        text.push('\n');
        if diff.use_full_view || root_changed {
            text.push_str("successor full view:\n");
            text.push_str(&outline::render_folded(&successor.tree));
        } else {
            text.push_str(&diff.text);
        }
        bounded_success(Some(&successor.state_id), text, Vec::new())
    }

    pub(super) async fn read_text(params: ReadTextParams) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let offset = match params.offset.map(usize::try_from).transpose() {
            Ok(offset) => offset,
            Err(_) => return tool_error("offset is too large for this platform"),
        };
        let page = if params.r#ref.starts_with("@o") {
            crate::state::global().lock().unwrap().read_output(
                &params.r#ref,
                params.state_id.as_deref(),
                offset,
            )
        } else if params.r#ref.starts_with("@e") {
            let Some(state_id) = params.state_id.as_deref() else {
                return tool_error("state_id is required when reading an @e element ref");
            };
            crate::state::global().lock().unwrap().page_element_text(
                state_id,
                &params.r#ref,
                offset.unwrap_or(0),
            )
        } else {
            return tool_error("read_text ref must be an @e element or @o continuation");
        };
        let page = match page {
            Ok(page) => page,
            Err(error) => return tool_error(&error.to_string()),
        };
        let owner = page.owner_state.as_deref().unwrap_or("none");
        let continuation = if page.eof {
            "eof".to_string()
        } else {
            format!(
                "continue with ref {} offset {}",
                page.output_ref, page.next_offset
            )
        };
        bounded_success(
            page.owner_state.as_deref(),
            format!(
                "ref: {}\nstate_id: {}\noffset: {}\nnext_offset: {}\ntotal_bytes: {}\neof: {}\n{}\n---\n{}",
                page.output_ref,
                owner,
                page.offset,
                page.next_offset,
                page.total_bytes,
                page.eof,
                continuation,
                page.text
            ),
            Vec::new(),
        )
    }

    pub(super) async fn wait_for(params: WaitForParams) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let _transaction = observation_transaction().lock().await;
        let previous = match crate::state::global()
            .lock()
            .unwrap()
            .validate_for_action(&params.state_id)
        {
            Ok(observation) => observation,
            Err(error) => return tool_error(&error.to_string()),
        };
        let backend = crate::backend::platform_backend();
        let timeout = Duration::from_millis(
            params
                .condition
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );
        let deadline = Instant::now() + timeout;
        let mut polls = 0_u64;
        let (mut observed, root_changed, matched) = loop {
            polls += 1;
            let (mut observed, root_changed) = match observe_with_root_fallback(
                backend.as_ref(),
                &previous.root,
                ObserveRequest {
                    semantic: true,
                    capture: CapturePolicy::Never,
                },
            ) {
                Ok(observed) => observed,
                Err(error) => return backend_error(error),
            };
            outline::assign_refs_from_previous(&previous.tree, &mut observed.tree);
            if condition_satisfied(&observed.tree, &params.condition) {
                break (observed, root_changed, true);
            }
            if Instant::now() >= deadline {
                break (observed, root_changed, false);
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        };
        outline::assign_refs_from_previous(&previous.tree, &mut observed.tree);
        let successor = crate::state::global().lock().unwrap().insert_observation(
            observed.root,
            observed.tree,
            observed.screenshot_png,
        );
        let status = if matched { "matched" } else { "timeout" };
        let report = json!({
            "state_id": successor.state_id,
            "previous_state_id": previous.state_id,
            "status": status,
            "until": until_name(params.condition.until),
            "polls": polls,
            "root_changed": root_changed,
        });
        let mut text = serde_json::to_string_pretty(&report).unwrap_or_else(|_| report.to_string());
        text.push('\n');
        text.push_str(&outline::render_folded(&successor.tree));
        bounded_success(Some(&successor.state_id), text, Vec::new())
    }

    #[cfg(target_os = "macos")]
    type PermissionSnapshot = crate::permissions::PermissionStatus;

    #[cfg(not(target_os = "macos"))]
    #[derive(Clone, Copy)]
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
    fn permission_gate(
        permissions: PermissionSnapshot,
        needs_accessibility: bool,
        needs_screen_recording: bool,
    ) -> Option<CallToolResult> {
        if needs_accessibility && !permissions.accessibility {
            return Some(tool_error(
                "Accessibility permission is missing; grant it in tcode Settings → Computer Use.",
            ));
        }
        if needs_screen_recording && !permissions.screen_recording {
            return Some(tool_error(
                "Screen Recording permission is missing; grant it in tcode Settings → Computer Use.",
            ));
        }
        None
    }

    #[cfg(not(target_os = "macos"))]
    fn permission_gate(
        _permissions: PermissionSnapshot,
        _needs_accessibility: bool,
        _needs_screen_recording: bool,
    ) -> Option<CallToolResult> {
        Some(backend_error(crate::backend::BackendError::unsupported()))
    }

    fn root_kind(kind: RootKind) -> BackendRootKind {
        match kind {
            RootKind::Window => BackendRootKind::Window,
            RootKind::Dialog => BackendRootKind::Dialog,
            RootKind::Sheet => BackendRootKind::Sheet,
            RootKind::Menu => BackendRootKind::Menu,
            RootKind::Popover => BackendRootKind::Popover,
        }
    }

    fn resolve_root(
        backend: &dyn Backend,
        requested: Option<&str>,
    ) -> Result<RootInfo, Box<CallToolResult>> {
        if let Some(requested) = requested
            && let Some(root) = roots().lock().unwrap().get(requested)
        {
            return Ok(root);
        }
        let discovered = backend
            .list_roots(&RootFilters::default())
            .map_err(|error| Box::new(backend_error(error)))?;
        let discovered = roots().lock().unwrap().refresh(discovered);
        match requested {
            Some(requested) => discovered
                .into_iter()
                .find(|root| root.ref_id == requested)
                .ok_or_else(|| {
                    Box::new(tool_error(&format!(
                        "root ref {requested} is no longer available; call find_roots again"
                    )))
                }),
            None => discovered
                .into_iter()
                .next()
                .ok_or_else(|| Box::new(tool_error("no on-screen desktop roots were found"))),
        }
    }

    fn capture_policy(
        configured: crate::config::ImageMode,
        requested: Option<ObserveMode>,
        permissions: &PermissionSnapshot,
    ) -> CapturePolicy {
        #[cfg(not(target_os = "macos"))]
        let _ = permissions;
        if configured == crate::config::ImageMode::Never {
            return CapturePolicy::Never;
        }
        if matches!(requested, Some(ObserveMode::Visual | ObserveMode::Fused))
            || configured == crate::config::ImageMode::Always
        {
            return CapturePolicy::Always;
        }
        #[cfg(target_os = "macos")]
        if !permissions.screen_recording {
            return CapturePolicy::Never;
        }
        CapturePolicy::IfSparse
    }

    fn capture_warning(
        configured: crate::config::ImageMode,
        permissions: &PermissionSnapshot,
    ) -> Option<&'static str> {
        #[cfg(target_os = "macos")]
        if configured == crate::config::ImageMode::Auto && !permissions.screen_recording {
            return Some(
                "screenshot omitted in auto mode because Screen Recording permission is missing",
            );
        }
        let _ = (configured, permissions);
        None
    }

    fn save_observation(observed: RootObservation, warning: Option<&str>) -> CallToolResult {
        let screenshot_for_response = observed.screenshot_png.clone();
        let observation = crate::state::global().lock().unwrap().insert_observation(
            observed.root,
            observed.tree,
            observed.screenshot_png,
        );
        let mut text = format!(
            "state_id: {}\nroot: {} app=\"{}\" title=\"{}\"\nelements: {} interactive: {}",
            observation.state_id,
            observation.root.ref_id,
            escaped(&observation.root.app_name),
            escaped(&observation.root.title),
            count_nodes(&observation.tree),
            outline::interactive_count(&observation.tree)
        );
        if let Some(warning) = warning {
            text.push_str("\nwarning: ");
            text.push_str(warning);
        }
        text.push('\n');
        text.push_str(&outline::render_folded(&observation.tree));
        let extra = screenshot_for_response
            .map(|png| {
                Content::image(
                    base64::engine::general_purpose::STANDARD.encode(png),
                    "image/png",
                )
            })
            .into_iter()
            .collect();
        bounded_success(Some(&observation.state_id), text, extra)
    }

    fn prepare_action(tree: &UiNode, action: &UiAction) -> Result<ActionRequest, String> {
        let target = action.r#ref.as_deref().map(|ref_id| {
            let node = tree.find(ref_id).ok_or_else(|| {
                crate::state::StateError::UnknownElement(ref_id.to_string()).to_string()
            })?;
            let path = outline::path_to_ref(tree, ref_id).ok_or_else(|| {
                crate::state::StateError::UnknownElement(ref_id.to_string()).to_string()
            })?;
            Ok::<_, String>((node, path))
        });
        let target = target.transpose()?;
        Ok(ActionRequest {
            kind: match action.action {
                UiActionKind::Press => BackendActionKind::Press,
                UiActionKind::Click => BackendActionKind::Click,
                UiActionKind::SetText => BackendActionKind::SetText,
                UiActionKind::TypeText => BackendActionKind::TypeText,
                UiActionKind::Keypress => BackendActionKind::Keypress,
                UiActionKind::Scroll => BackendActionKind::Scroll,
                UiActionKind::Drag => BackendActionKind::Drag,
                UiActionKind::MoveMouse => BackendActionKind::MoveMouse,
            },
            target_path: target.as_ref().map(|(_, path)| path.clone()),
            target_frame: target.as_ref().map(|(node, _)| node.frame),
            target_role: target.as_ref().map(|(node, _)| node.role.clone()),
            target_title: target.as_ref().map(|(node, _)| node.title.clone()),
            target_actions: target
                .as_ref()
                .map(|(node, _)| node.actions.clone())
                .unwrap_or_default(),
            x: action.x,
            y: action.y,
            text: action.text.clone(),
            keys: action.keys.clone(),
            scroll_x: action.scroll_x,
            scroll_y: action.scroll_y,
            path: action.path.clone(),
            button: match action.button.unwrap_or(MouseButton::Left) {
                MouseButton::Left => BackendMouseButton::Left,
                MouseButton::Right => BackendMouseButton::Right,
                MouseButton::Middle => BackendMouseButton::Middle,
            },
            click_count: action.click_count.unwrap_or(1),
        })
    }

    async fn poll_successor(
        backend: &dyn Backend,
        previous: &crate::state::Observation,
        condition: Option<&UiCondition>,
        preexisting: bool,
    ) -> Result<(RootObservation, &'static str, bool), crate::backend::BackendError> {
        let timeout = Duration::from_millis(
            condition
                .and_then(|condition| condition.timeout_ms)
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );
        let deadline = Instant::now() + timeout;
        loop {
            let (mut observed, root_changed) = observe_with_root_fallback(
                backend,
                &previous.root,
                ObserveRequest {
                    semantic: true,
                    capture: CapturePolicy::Never,
                },
            )?;
            outline::assign_refs_from_previous(&previous.tree, &mut observed.tree);
            let status = match condition {
                None => Some("not_requested"),
                Some(_) if preexisting => Some("preexisting"),
                Some(condition) if condition_satisfied(&observed.tree, condition) => {
                    Some("verified")
                }
                Some(_) if Instant::now() >= deadline => Some("failed"),
                Some(_) => None,
            };
            if let Some(status) = status {
                return Ok((observed, status, root_changed));
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }

    fn observe_with_root_fallback(
        backend: &dyn Backend,
        root: &RootInfo,
        request: ObserveRequest,
    ) -> Result<(RootObservation, bool), crate::backend::BackendError> {
        match backend.observe(root, request) {
            Ok(observed) => Ok((observed, false)),
            Err(original_error) => {
                let discovered = backend.list_roots(&RootFilters::default())?;
                let discovered = roots().lock().unwrap().refresh(discovered);
                let Some(successor_root) = discovered.into_iter().next() else {
                    return Err(original_error);
                };
                backend
                    .observe(&successor_root, request)
                    .map(|observed| (observed, successor_root.identity() != root.identity()))
            }
        }
    }

    fn condition_satisfied(tree: &UiNode, condition: &UiCondition) -> bool {
        let scope = match condition.scope_ref.as_deref() {
            Some(ref_id) => tree.find(ref_id),
            None => Some(tree),
        };
        let Some(scope) = scope else {
            return matches!(condition.until, Some(ConditionUntil::Absent));
        };
        let present = if let Some(ref_id) = condition.r#ref.as_deref() {
            scope
                .find(ref_id)
                .is_some_and(|node| node_matches(node, condition))
        } else {
            any_node_matches(scope, condition)
        };
        match condition.until.unwrap_or(ConditionUntil::Present) {
            ConditionUntil::Present => present,
            ConditionUntil::Absent => !present,
        }
    }

    fn any_node_matches(node: &UiNode, condition: &UiCondition) -> bool {
        node_matches(node, condition)
            || node
                .children
                .iter()
                .any(|child| any_node_matches(child, condition))
    }

    fn node_matches(node: &UiNode, condition: &UiCondition) -> bool {
        let text_matches = condition.text.as_deref().is_none_or(|text| {
            contains_case_insensitive(&node.title, text)
                || contains_case_insensitive(&node.value, text)
                || contains_case_insensitive(&node.description, text)
        });
        let role_matches = condition.role.as_deref().is_none_or(|role| {
            outline::canonical_role(&node.role) == outline::canonical_role(role)
        });
        let value_matches = condition
            .value
            .as_deref()
            .is_none_or(|value| contains_case_insensitive(&node.value, value));
        text_matches && role_matches && value_matches
    }

    fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
        haystack.to_lowercase().contains(&needle.to_lowercase())
    }

    fn until_name(until: Option<ConditionUntil>) -> &'static str {
        match until.unwrap_or(ConditionUntil::Present) {
            ConditionUntil::Present => "present",
            ConditionUntil::Absent => "absent",
        }
    }

    fn action_name(action: UiActionKind) -> &'static str {
        match action {
            UiActionKind::Press => "press",
            UiActionKind::Click => "click",
            UiActionKind::SetText => "set_text",
            UiActionKind::TypeText => "type_text",
            UiActionKind::Keypress => "keypress",
            UiActionKind::Scroll => "scroll",
            UiActionKind::Drag => "drag",
            UiActionKind::MoveMouse => "move_mouse",
        }
    }

    fn count_nodes(node: &UiNode) -> usize {
        1 + node.children.iter().map(count_nodes).sum::<usize>()
    }

    fn escaped(value: &str) -> String {
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(['\n', '\r'], " ")
    }

    fn bounded_success(
        owner_state: Option<&str>,
        text: String,
        mut extra: Vec<Content>,
    ) -> CallToolResult {
        let text = crate::state::global()
            .lock()
            .unwrap()
            .bound_model_text(owner_state, text);
        let mut content = vec![Content::text(text)];
        content.append(&mut extra);
        CallToolResult::success(content)
    }

    fn backend_error(error: crate::backend::BackendError) -> CallToolResult {
        let text = serde_json::to_string(&error).unwrap_or_else(|_| error.to_string());
        CallToolResult::error(vec![Content::text(text)])
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
