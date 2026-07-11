//! The right-panel "Preview" tab: an embedded browser (native `gpui-wry`
//! WebView) with a chrome row, plus the bridge that lets the agent drive it
//! through the preview MCP server.
//!
//! One WebView is created lazily per session and cached; switching sessions
//! shows that session's view and hides the others. The chrome row offers
//! back/forward/reload (via raw `wry` `evaluate_script` / history), a URL entry,
//! open-in-system-browser, and localhost dev-port quick-picks.
//!
//! ## Known caveat — native overlay
//!
//! A `gpui-wry` WebView is a **native child view drawn over** the gpui window,
//! not composited into gpui's scene. It therefore covers any gpui popover /
//! dialog that overlaps its bounds. We mitigate the common case by hiding the
//! WebView whenever the command palette is open or we leave the chat route; a
//! fully general fix (hiding on every popover) would need popover-layer state we
//! don't currently track, so overlapping in-webview popovers are a known
//! limitation (documented, not fixed).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::Engine as _;
use gpui::{
    AnyElement, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _, Pixels,
    Render, Styled as _, Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    h_flex, v_flex,
};
use gpui_wry::WebView;
use preview_mcp::{PreviewOp, PreviewReply, js, ports};
use raw_window_handle::HasWindowHandle as _;

use crate::app::AppState;

/// The reply channel a broker request is answered on.
type ReplyTx = async_channel::Sender<Result<PreviewReply, String>>;

pub struct PreviewPanel {
    app_state: Entity<AppState>,
    /// One native WebView per session id, created on first use.
    webviews: HashMap<String, Entity<WebView>>,
    /// Sessions whose WebView has begun a navigation. lb-wry queues (and drops
    /// the callback of) `evaluate_script_with_callback` until the first
    /// navigation starts flushing its pending-scripts buffer, so value-returning
    /// ops must wait until a session is "warm".
    warm: HashSet<String>,
    /// Last URL loaded per session (drives the address bar + reload).
    urls: HashMap<String, String>,
    /// The shared address-bar input (reflects the active session's URL).
    url_input: Entity<InputState>,
    /// Session id whose URL is currently mirrored into `url_input`.
    mirrored: Option<String>,
    /// Discovered localhost dev-server ports (populated by the "Ports" button).
    dev_ports: Vec<u16>,
    _subscriptions: Vec<Subscription>,
}

impl PreviewPanel {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rust_i18n::t!("preview.url_placeholder"))
        });
        let subscriptions = vec![
            cx.observe(&app_state, |_, _, cx| cx.notify()),
            cx.subscribe_in(&url_input, window, Self::on_url_event),
        ];
        Self {
            app_state,
            webviews: HashMap::new(),
            warm: HashSet::new(),
            urls: HashMap::new(),
            url_input,
            mirrored: None,
            dev_ports: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    /// The active session id, if any.
    fn active_id(&self, cx: &Context<Self>) -> Option<String> {
        self.app_state
            .read(cx)
            .active_session_id()
            .map(str::to_string)
    }

    /// Get or lazily create the WebView for `session_id`, returning its entity.
    fn ensure_webview(
        &mut self,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<WebView> {
        if let Some(view) = self.webviews.get(session_id) {
            return view.clone();
        }
        let webview = cx.new(|cx| {
            // Start on about:blank so lb-wry begins a navigation and flushes its
            // pending-scripts buffer, making later `evaluate_script` callbacks
            // fire (see the `warm` field docs).
            let builder = wry::WebViewBuilder::new()
                .with_devtools(true)
                .with_url("about:blank");
            let handle = window
                .window_handle()
                .expect("preview: window handle unavailable");
            let raw = builder
                .build_as_child(&handle)
                .expect("preview: failed to build webview");
            let mut view = WebView::new(raw, window, cx);
            view.hide();
            view
        });
        self.webviews
            .insert(session_id.to_string(), webview.clone());
        webview
    }

    /// Navigate the active session's WebView to `url`, remembering it.
    fn navigate(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.active_id(cx) else {
            return;
        };
        let url = normalize_url(url);
        let webview = self.ensure_webview(&session_id, window, cx);
        webview.update(cx, |view, _| {
            view.show();
            view.load_url(&url);
        });
        // A navigation flushes lb-wry's pending-scripts buffer, so subsequent
        // evaluate callbacks will fire.
        self.warm.insert(session_id.clone());
        self.urls.insert(session_id, url);
        cx.notify();
    }

    fn on_url_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            let url = input.read(cx).value().trim().to_string();
            if !url.is_empty() {
                self.navigate(&url, window, cx);
            }
        }
    }

    /// Run raw JS on the active WebView via history/reload (fire-and-forget).
    fn eval_fire(&self, session_id: &str, script: &str, cx: &Context<Self>) {
        if let Some(view) = self.webviews.get(session_id) {
            let _ = view.read(cx).raw().evaluate_script(script);
        }
    }

    // ---- chrome actions -----------------------------------------------------

    fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active_id(cx) {
            let view = self.ensure_webview(&id, window, cx);
            view.update(cx, |view, _| {
                let _ = view.back();
            });
        }
    }

    fn go_forward(&mut self, cx: &Context<Self>) {
        if let Some(id) = self.active_id(cx) {
            self.eval_fire(&id, "history.forward();", cx);
        }
    }

    fn reload(&mut self, cx: &Context<Self>) {
        if let Some(id) = self.active_id(cx) {
            self.eval_fire(&id, "location.reload();", cx);
        }
    }

    fn open_in_system_browser(&self, cx: &Context<Self>) {
        if let Some(id) = self.active_id(cx) {
            if let Some(url) = self.urls.get(&id) {
                let _ = crate::process::command("open").arg(url).spawn();
            }
        }
    }

    fn rescan_ports(&mut self, cx: &mut Context<Self>) {
        self.dev_ports = ports::scan_listening();
        cx.notify();
    }

    // ---- broker bridge ------------------------------------------------------

    /// Resolve one automation op from the MCP server against the active WebView.
    /// Answers `reply` immediately for actions, or from the JS callback for
    /// value-returning ops.
    pub fn handle_op(
        &mut self,
        op: PreviewOp,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.active_id(cx) else {
            let _ = reply.try_send(Err("no active session to preview".into()));
            return;
        };
        log::info!("preview: handling op {op:?} for session {session_id}");

        match op {
            PreviewOp::Open { url } => {
                self.app_state
                    .update(cx, |state, cx| state.open_preview_panel(cx));
                if let Some(url) = url.as_deref() {
                    self.navigate(url, window, cx);
                } else {
                    let view = self.ensure_webview(&session_id, window, cx);
                    view.update(cx, |v, _| v.show());
                }
                let payload = serde_json::json!({
                    "ok": true,
                    "url": self.urls.get(&session_id),
                    "note": "call preview_status for live page state once loaded",
                });
                let _ = reply.try_send(Ok(PreviewReply::Json(payload)));
            }
            PreviewOp::Navigate { url } => {
                self.app_state
                    .update(cx, |state, cx| state.open_preview_panel(cx));
                self.navigate(&url, window, cx);
                let payload = serde_json::json!({
                    "ok": true,
                    "url": self.urls.get(&session_id),
                    "note": "page is loading; call preview_status for live state",
                });
                let _ = reply.try_send(Ok(PreviewReply::Json(payload)));
            }
            PreviewOp::Status => self.eval_json(&session_id, js::STATUS, reply, window, cx),
            PreviewOp::Snapshot => self.eval_json(&session_id, js::SNAPSHOT, reply, window, cx),
            PreviewOp::Evaluate { js: expr } => {
                self.eval_json(&session_id, &js::evaluate(&expr), reply, window, cx)
            }
            PreviewOp::Click { selector } => {
                self.eval_json(&session_id, &js::click(&selector), reply, window, cx)
            }
            PreviewOp::Type { selector, text } => {
                self.eval_json(&session_id, &js::type_text(&selector, &text), reply, window, cx)
            }
            PreviewOp::Screenshot => self.screenshot(&session_id, reply, window, cx),
        }
    }

    /// Evaluate `script` and answer `reply` with the parsed JSON result.
    ///
    /// If the session's WebView isn't warm yet (no navigation has started, so
    /// lb-wry would silently drop the callback), create it, let `about:blank`
    /// begin loading, then re-dispatch the evaluation after a short delay.
    fn eval_json(
        &mut self,
        session_id: &str,
        script: &str,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Ensure the WebView exists (and has begun loading about:blank).
        let _ = self.ensure_webview(session_id, window, cx);
        if self.warm.contains(session_id) {
            self.eval_now(session_id, script, reply, cx);
            return;
        }
        // Cold start: wait for the initial navigation to flush pending scripts,
        // then evaluate.
        let session_id = session_id.to_string();
        let script = script.to_string();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(700))
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.warm.insert(session_id.clone());
                panel.eval_now(&session_id, &script, reply, cx);
            });
        })
        .detach();
    }

    /// Run `script` on the (already-warm) WebView, answering from the callback.
    fn eval_now(&self, session_id: &str, script: &str, reply: ReplyTx, cx: &Context<Self>) {
        let Some(view) = self.webviews.get(session_id) else {
            let _ = reply.try_send(Err("preview browser is not open".into()));
            return;
        };
        let result = view.read(cx).raw().evaluate_script_with_callback(script, {
            let reply = reply.clone();
            move |raw: String| {
                let value = js::parse_result(&raw);
                let _ = reply.try_send(Ok(PreviewReply::Json(value)));
            }
        });
        if result.is_err() {
            let _ = reply.try_send(Err("failed to evaluate script in preview".into()));
        }
    }

    /// Capture the WebView's on-screen region with `screencapture` (wry exposes
    /// no capture API) and answer with a base64 PNG. Best-effort geometry: the
    /// region is the window origin plus the WebView's laid-out bounds.
    fn screenshot(
        &mut self,
        session_id: &str,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.webviews.get(session_id) else {
            let _ = reply.try_send(Err("preview browser is not open".into()));
            return;
        };
        let wv_bounds = view.read(cx).bounds();
        if wv_bounds.size.width <= px(0.) || wv_bounds.size.height <= px(0.) {
            let _ = reply.try_send(Err("preview browser has no visible area".into()));
            return;
        }
        let window_origin = window.bounds().origin;
        let region = screen_region(window_origin, wv_bounds);

        let path = std::env::temp_dir().join(format!("tcode-preview-{}.png", std::process::id()));
        let status = crate::process::command("screencapture")
            .arg("-x")
            .arg("-R")
            .arg(&region)
            .arg(&path)
            .status();

        match status {
            Ok(s) if s.success() => match std::fs::read(&path) {
                Ok(bytes) => {
                    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    let _ = std::fs::remove_file(&path);
                    let _ = reply.try_send(Ok(PreviewReply::Image {
                        mime: "image/png".into(),
                        data_base64,
                    }));
                }
                Err(err) => {
                    let _ = reply.try_send(Err(format!("failed to read screenshot: {err}")));
                }
            },
            Ok(_) => {
                let _ = reply.try_send(Err("screencapture failed".into()));
            }
            Err(err) => {
                let _ = reply.try_send(Err(format!("failed to run screencapture: {err}")));
            }
        }
    }
}

/// Compute a `screencapture -R x,y,w,h` region string from the window origin and
/// the WebView's window-relative bounds.
fn screen_region(window_origin: gpui::Point<Pixels>, wv: Bounds<Pixels>) -> String {
    let x = f32::from(window_origin.x + wv.origin.x).round() as i32;
    let y = f32::from(window_origin.y + wv.origin.y).round() as i32;
    let w = f32::from(wv.size.width).round() as i32;
    let h = f32::from(wv.size.height).round() as i32;
    format!("{x},{y},{w},{h}")
}

/// Add a scheme to a bare host/port (so `localhost:5173` becomes a real URL).
fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.contains("://") || trimmed.starts_with("about:") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

impl Render for PreviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_id(cx);

        // Honor a queued `--open-preview <url>` navigation once a session exists.
        if active.is_some() {
            if let Some(url) = self
                .app_state
                .update(cx, |state, _| state.take_pending_preview_url())
            {
                self.navigate(&url, window, cx);
            }
        }

        // Mirror the active session's URL into the address bar when it changes.
        if active != self.mirrored {
            let value = active
                .as_ref()
                .and_then(|id| self.urls.get(id))
                .cloned()
                .unwrap_or_default();
            self.url_input
                .update(cx, |state, cx| state.set_value(&value, window, cx));
            self.mirrored = active.clone();
        }

        // Native overlay mitigation: hide the WebView while the palette is open
        // or we're off the chat route (see module docs).
        let obstruct = {
            let state = self.app_state.read(cx);
            state.palette_open || state.route != crate::app::Route::Chat
        };
        for (id, view) in &self.webviews {
            let should_show = Some(id) == active.as_ref() && !obstruct;
            view.update(cx, |v, _| {
                if should_show {
                    v.show();
                } else {
                    v.hide();
                }
            });
        }

        let body: AnyElement = match &active {
            Some(id) => {
                let view = self.ensure_webview(id, window, cx);
                div()
                    .flex_1()
                    .min_h_0()
                    .child(view)
                    .into_any_element()
            }
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("preview.no_session"))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(self.render_chrome(cx))
            .children(self.render_port_row(cx))
            .child(body)
    }
}

impl PreviewPanel {
    fn render_chrome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_none()
            .w_full()
            .gap_1()
            .p_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("preview-back")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::ArrowLeft)
                    .tooltip(rust_i18n::t!("preview.back"))
                    .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx))),
            )
            .child(
                Button::new("preview-forward")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::ArrowRight)
                    .tooltip(rust_i18n::t!("preview.forward"))
                    .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx))),
            )
            .child(
                Button::new("preview-reload")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Replace)
                    .tooltip(rust_i18n::t!("preview.reload"))
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
            .child(div().flex_1().min_w_0().child(Input::new(&self.url_input)))
            .child(
                Button::new("preview-ports")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Globe)
                    .tooltip(rust_i18n::t!("preview.scan_ports"))
                    .on_click(cx.listener(|this, _, _, cx| this.rescan_ports(cx))),
            )
            .child(
                Button::new("preview-open-external")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::ExternalLink)
                    .tooltip(rust_i18n::t!("preview.open_external"))
                    .on_click(cx.listener(|this, _, _, cx| this.open_in_system_browser(cx))),
            )
    }

    /// A row of quick-pick buttons for discovered localhost dev ports.
    fn render_port_row(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.dev_ports.is_empty() {
            return None;
        }
        let mut row = h_flex()
            .flex_none()
            .w_full()
            .gap_1()
            .px_1()
            .pb_1()
            .flex_wrap();
        for port in self.dev_ports.clone() {
            row = row.child(
                Button::new(("dev-port", port as usize))
                    .outline()
                    .small()
                    .compact()
                    .label(format!(":{port}"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let url = ports::url_for_port(port);
                        this.navigate(&url, window, cx);
                    })),
            );
        }
        Some(row)
    }
}
