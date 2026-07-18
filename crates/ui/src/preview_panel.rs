//! The right-panel "Preview" tab: an embedded browser (native `gpui-wry`
//! WebView) with a chrome row, plus the bridge that lets the agent drive it
//! through the preview MCP server.
//!
//! One WebView is created lazily per conversation destination and cached;
//! switching threads (or project drafts) shows that conversation's view and
//! hides the others. The chrome row offers
//! back/forward/reload (via raw `wry` `evaluate_script` / history), a URL entry,
//! open-in-system-browser, and localhost dev-port quick-picks.
//!
//! ## Platform support
//!
//! macOS + Windows get the real WebView. **Linux does not**: lb-wry's
//! `build_as_child` is X11-only there *and* requires a GTK main loop (`gtk::init`
//! plus `gtk::main_iteration_do` pumped on the UI thread), while gpui's Linux
//! backend runs calloop/xcb and never pumps GTK — the webview would panic at
//! construction and could never be driven. So `wry`/`gpui-wry` are not even
//! dependencies on Linux (see the `[target.'cfg(not(target_os = "linux"))']`
//! table in Cargo.toml); the tab renders a placeholder and every `preview_*` MCP
//! tool answers with an error. The MCP server itself still starts, harmlessly.
//!
//! ## Known caveat — native overlay
//!
//! A `gpui-wry` WebView is a **native child view drawn over** the gpui window,
//! not composited into gpui's scene. It therefore covers any gpui popover /
//! dialog that overlaps its bounds. We mitigate the common case by hiding the
//! WebView whenever its owning Preview panel closes, another right-panel tab or
//! conversation is selected, the command palette opens, or we leave the chat
//! route. A fully general fix (hiding on every popover) would need popover-layer
//! state we don't currently track, so overlapping in-webview popovers are a
//! known limitation (documented, not fixed).

use preview_mcp::PreviewReply;
#[cfg(any(not(target_os = "linux"), test))]
use tcode_runtime::app::Route;

#[cfg(any(not(target_os = "linux"), test))]
fn visible_preview_key(
    active_key: Option<&str>,
    route: Route,
    palette_open: bool,
    preview_panel_showing: bool,
) -> Option<&str> {
    (route == Route::Chat && !palette_open && preview_panel_showing)
        .then_some(active_key)
        .flatten()
}

/// Resolve an MCP request's physical session id to the stable WebView key.
/// Only the active surface can be an unsent project draft; every background
/// request therefore keys directly by its stored session id.
#[cfg(any(not(target_os = "linux"), test))]
fn preview_key_for_session(
    requested_session_id: &str,
    active_session_id: Option<&str>,
    active_key: Option<&str>,
) -> String {
    if active_session_id == Some(requested_session_id) {
        active_key.unwrap_or(requested_session_id).to_string()
    } else {
        requested_session_id.to_string()
    }
}

/// The reply channel a broker request is answered on.
type ReplyTx = async_channel::Sender<Result<PreviewReply, String>>;

#[cfg(not(target_os = "linux"))]
pub use native::PreviewPanel;

#[cfg(target_os = "linux")]
pub use placeholder::PreviewPanel;

#[cfg(not(target_os = "linux"))]
mod native {
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    use gpui::{
        AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
        Styled as _, Subscription, Window, div,
    };
    use gpui_component::{
        ActiveTheme as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState},
        v_flex,
    };
    use gpui_wry::WebView;
    use preview_mcp::{PreviewOp, PreviewReply, js, ports};
    use raw_window_handle::HasWindowHandle as _;

    use super::{
        ReplyTx, normalize_url, preview_key_for_session, unavailable_message, visible_preview_key,
    };
    use tcode_runtime::app::AppState;

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
        /// Last physical session id + stable conversation key. When an unsent
        /// draft is committed its physical id stays the same but its key moves
        /// from `draft:<project>` to the stored session id; this lets the live
        /// WebView move with it instead of being replaced by a blank one.
        active_identity: Option<(String, String)>,
        /// Discovered localhost dev-server ports (populated by the "Ports" button).
        dev_ports: Vec<u16>,
        /// Why the platform webview could not be created (Windows without the
        /// WebView2 runtime). Set once; the tab then explains itself instead of
        /// retrying on every frame.
        webview_error: Option<String>,
        _subscriptions: Vec<Subscription>,
    }

    impl PreviewPanel {
        pub fn new(
            app_state: Entity<AppState>,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            let url_input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(tcode_i18n::tr!("preview.url_placeholder"))
            });
            let subscriptions = vec![
                cx.observe(&app_state, |this, _, cx| {
                    // Native child views outlive GPUI layout nodes. Visibility
                    // therefore follows AppState directly, even while this
                    // entity is no longer mounted in the right-panel tree.
                    this.sync_visibility(cx);
                    cx.notify();
                }),
                cx.subscribe_in(&url_input, window, Self::on_url_event),
            ];
            Self {
                app_state,
                webviews: HashMap::new(),
                warm: HashSet::new(),
                urls: HashMap::new(),
                url_input,
                mirrored: None,
                active_identity: None,
                dev_ports: Vec::new(),
                webview_error: None,
                _subscriptions: subscriptions,
            }
        }

        /// Reconcile the stable conversation key with the physical session id.
        /// Draft -> stored-thread commits retain the same session id, so move
        /// all cached browser state across that one key transition.
        fn active_key(&mut self, cx: &Context<Self>) -> Option<String> {
            let current = {
                let state = self.app_state.read(cx);
                state.active_session_id().and_then(|session_id| {
                    state
                        .active_conversation_ui_key()
                        .map(|key| (session_id.to_string(), key))
                })
            };

            if let (Some((old_session, old_key)), Some((session, key))) =
                (self.active_identity.as_ref(), current.as_ref())
                && old_session == session
                && old_key != key
            {
                if let Some(view) = self.webviews.remove(old_key) {
                    if self.webviews.contains_key(key) {
                        drop(view);
                    } else {
                        self.webviews.insert(key.clone(), view);
                    }
                }
                if let Some(url) = self.urls.remove(old_key)
                    && !self.urls.contains_key(key)
                {
                    self.urls.insert(key.clone(), url);
                }
                if self.warm.remove(old_key) {
                    self.warm.insert(key.clone());
                }
                if self.mirrored.as_deref() == Some(old_key) {
                    self.mirrored = Some(key.clone());
                }
            }

            self.active_identity = current.clone();
            current.map(|(_, key)| key)
        }

        fn routed_key(&mut self, session_id: &str, cx: &Context<Self>) -> String {
            let active_key = self.active_key(cx);
            let active_session_id = self
                .app_state
                .read(cx)
                .active_session_id()
                .map(str::to_string);
            preview_key_for_session(
                session_id,
                active_session_id.as_deref(),
                active_key.as_deref(),
            )
        }

        /// Hide native children that no longer belong to the visible Preview
        /// panel. This deliberately never shows a child: an opening transition
        /// may still have stale bounds until `render` mounts its GPUI owner.
        /// `AppShell` calls this before it removes Preview from the layout tree.
        pub fn sync_visibility(&mut self, cx: &mut Context<Self>) {
            self.update_visibility(false, cx);
        }

        /// Full show/hide synchronization, called only while `PreviewPanel` is
        /// mounted and has laid out the WebView owner for this frame.
        fn sync_mounted_visibility(&mut self, cx: &mut Context<Self>) {
            self.update_visibility(true, cx);
        }

        fn update_visibility(&mut self, allow_show: bool, cx: &mut Context<Self>) {
            let active = self.active_key(cx);
            let visible = {
                let state = self.app_state.read(cx);
                visible_preview_key(
                    active.as_deref(),
                    state.route,
                    state.palette_open,
                    state.preview_panel_showing(),
                )
                .map(str::to_string)
            };
            for (key, view) in &self.webviews {
                let should_show = Some(key) == visible.as_ref();
                view.update(cx, |view, _| {
                    if should_show && allow_show {
                        view.show();
                    } else if !should_show {
                        view.hide();
                    }
                });
            }
        }

        /// Get or lazily create the WebView for `session_id`.
        ///
        /// `None` when the platform webview cannot be created — on Windows that
        /// means the WebView2 runtime is absent. Only the preview browser needs
        /// it, so this is a missing feature, not a dead app: the tab explains
        /// itself and every other surface keeps working.
        fn ensure_webview(
            &mut self,
            session_id: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Option<Entity<WebView>> {
            if let Some(view) = self.webviews.get(session_id) {
                return Some(view.clone());
            }
            if self.webview_error.is_some() {
                return None;
            }
            // Start on about:blank so lb-wry begins a navigation and flushes its
            // pending-scripts buffer, making later `evaluate_script` callbacks
            // fire (see the `warm` field docs).
            let builder = wry::WebViewBuilder::new()
                .with_devtools(true)
                .with_url("about:blank");
            let built = window
                .window_handle()
                .map_err(|err| err.to_string())
                .and_then(|handle| {
                    builder
                        .build_as_child(&handle)
                        .map_err(|err| err.to_string())
                });
            let raw = match built {
                Ok(raw) => raw,
                Err(err) => {
                    log::warn!("preview: no webview ({err})");
                    self.webview_error = Some(err);
                    return None;
                }
            };
            let webview = cx.new(|cx| {
                let mut view = WebView::new(raw, window, cx);
                view.hide();
                view
            });
            self.webviews
                .insert(session_id.to_string(), webview.clone());
            Some(webview)
        }

        /// Navigate one conversation's WebView to `url`, remembering it.
        fn navigate(&mut self, key: &str, url: &str, window: &mut Window, cx: &mut Context<Self>) {
            let url = normalize_url(url);
            let Some(webview) = self.ensure_webview(key, window, cx) else {
                cx.notify();
                return;
            };
            webview.update(cx, |view, _| view.load_url(&url));
            // A navigation flushes lb-wry's pending-scripts buffer, so subsequent
            // evaluate callbacks will fire.
            self.warm.insert(key.to_string());
            self.urls.insert(key.to_string(), url);
            self.sync_visibility(cx);
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
                if !url.is_empty()
                    && let Some(key) = self.active_key(cx)
                {
                    self.navigate(&key, &url, window, cx);
                }
            }
        }

        /// Run raw JS on the active WebView via history/reload (fire-and-forget).
        fn eval_fire(&self, session_id: &str, script: &str, cx: &Context<Self>) {
            if let Some(view) = self.webviews.get(session_id) {
                let _ = view.read(cx).raw().evaluate_script(script);
            }
        }

        // ---- chrome actions -------------------------------------------------

        fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            if let Some(id) = self.active_key(cx)
                && let Some(view) = self.ensure_webview(&id, window, cx)
            {
                view.update(cx, |view, _| {
                    let _ = view.back();
                });
            }
        }

        fn go_forward(&mut self, cx: &Context<Self>) {
            if let Some(id) = self.active_key(cx) {
                self.eval_fire(&id, "history.forward();", cx);
            }
        }

        fn reload(&mut self, cx: &Context<Self>) {
            if let Some(id) = self.active_key(cx) {
                self.eval_fire(&id, "location.reload();", cx);
            }
        }

        /// Hand the current URL to the OS browser. `cx.open_url` is gpui's
        /// cross-platform launcher (`open` / `ShellExecute` / `xdg-open`).
        fn open_in_system_browser(&mut self, cx: &Context<Self>) {
            if let Some(id) = self.active_key(cx)
                && let Some(url) = self.urls.get(&id)
            {
                cx.open_url(url);
            }
        }

        fn rescan_ports(&mut self, cx: &mut Context<Self>) {
            self.dev_ports = ports::scan_listening();
            cx.notify();
        }

        // ---- broker bridge --------------------------------------------------

        /// Resolve one automation op from the MCP server against the active WebView.
        /// Answers `reply` immediately for actions, or from the JS callback for
        /// value-returning ops.
        pub fn handle_op(
            &mut self,
            session_id: String,
            op: PreviewOp,
            reply: ReplyTx,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            let key = self.routed_key(&session_id, cx);
            log::info!("preview: handling op {op:?} for session {session_id}");

            // Gate on the Browser settings: a disabled browser rejects every op;
            // `allow_evaluate` gates only `preview_evaluate`.
            let browser = self.app_state.read(cx).settings.browser.clone();
            if !browser.enabled {
                let _ = reply.try_send(Err(tcode_i18n::tr!("browser.disabled_error").into_owned()));
                return;
            }
            if matches!(&op, PreviewOp::Evaluate { .. }) && !browser.allow_evaluate {
                let _ = reply.try_send(Err(
                    tcode_i18n::tr!("browser.evaluate_disabled_error").into_owned()
                ));
                return;
            }

            match op {
                PreviewOp::Open { url } => {
                    self.app_state.update(cx, |state, cx| {
                        state.open_preview_panel_for(&session_id, cx)
                    });
                    if let Some(url) = url.as_deref() {
                        self.navigate(&key, url, window, cx);
                    } else if let Some(home) = browser
                        .home_url
                        .as_deref()
                        .map(str::trim)
                        .filter(|home| !home.is_empty())
                    {
                        // No explicit target: fall back to the configured home URL.
                        self.navigate(&key, home, window, cx);
                    } else {
                        self.ensure_webview(&key, window, cx);
                        self.sync_visibility(cx);
                    }
                    if let Some(err) = &self.webview_error {
                        let _ = reply.try_send(Err(unavailable_message(err)));
                        return;
                    }
                    let payload = serde_json::json!({
                        "ok": true,
                        "url": self.urls.get(&key),
                        "note": "call preview_status for live page state once loaded",
                    });
                    let _ = reply.try_send(Ok(PreviewReply::Json(payload)));
                }
                PreviewOp::Navigate { url } => {
                    self.app_state.update(cx, |state, cx| {
                        state.open_preview_panel_for(&session_id, cx)
                    });
                    self.navigate(&key, &url, window, cx);
                    if let Some(err) = &self.webview_error {
                        let _ = reply.try_send(Err(unavailable_message(err)));
                        return;
                    }
                    let payload = serde_json::json!({
                        "ok": true,
                        "url": self.urls.get(&key),
                        "note": "page is loading; call preview_status for live state",
                    });
                    let _ = reply.try_send(Ok(PreviewReply::Json(payload)));
                }
                PreviewOp::Status => self.eval_json(&key, js::STATUS, reply, window, cx),
                PreviewOp::Snapshot => self.eval_json(&key, js::SNAPSHOT, reply, window, cx),
                PreviewOp::Evaluate { js: expr } => {
                    self.eval_json(&key, &js::evaluate(&expr), reply, window, cx)
                }
                PreviewOp::Click { selector } => {
                    self.eval_json(&key, &js::click(&selector), reply, window, cx)
                }
                PreviewOp::Type { selector, text } => {
                    self.eval_json(&key, &js::type_text(&selector, &text), reply, window, cx)
                }
                PreviewOp::Screenshot => self.screenshot(&session_id, &key, reply, window, cx),
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
            if self.ensure_webview(session_id, window, cx).is_none() {
                let err = self.webview_error.clone().unwrap_or_default();
                let _ = reply.try_send(Err(unavailable_message(&err)));
                return;
            }
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
        ///
        /// macOS only. Windows has no comparable CLI and Wayland forbids screen
        /// capture outright, so elsewhere the tool reports a normal MCP error
        /// rather than pretending.
        #[cfg(target_os = "macos")]
        fn screenshot(
            &mut self,
            session_id: &str,
            key: &str,
            reply: ReplyTx,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            use base64::Engine as _;
            use gpui::px;

            let visible = {
                let state = self.app_state.read(cx);
                if state.active_session_id() != Some(session_id) {
                    let _ = reply.try_send(Err(
                        "preview is not visible; the user is viewing another conversation".into(),
                    ));
                    return;
                }
                visible_preview_key(
                    Some(key),
                    state.route,
                    state.palette_open,
                    state.preview_panel_showing(),
                ) == Some(key)
            };
            if !visible {
                let _ = reply.try_send(Err(
                    "preview is not visible; open the Preview panel before taking a screenshot"
                        .into(),
                ));
                return;
            }

            let Some(view) = self.webviews.get(key) else {
                let _ = reply.try_send(Err("preview browser is not open".into()));
                return;
            };
            let wv_bounds = view.read(cx).bounds();
            if wv_bounds.size.width <= px(0.) || wv_bounds.size.height <= px(0.) {
                let _ = reply.try_send(Err("preview browser has no visible area".into()));
                return;
            }
            let window_origin = window.bounds().origin;
            let region = super::screen_region(window_origin, wv_bounds);

            match tcode_runtime::ui_facade::capture_screen_region(&region) {
                Ok(bytes) => {
                    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    let _ = reply.try_send(Ok(PreviewReply::Image {
                        mime: "image/png".into(),
                        data_base64,
                    }));
                }
                Err(err) => {
                    let _ = reply.try_send(Err(err));
                }
            }
        }

        /// See the macOS implementation: screen capture has no portable
        /// equivalent, so this is a plain tool error off macOS.
        #[cfg(not(target_os = "macos"))]
        fn screenshot(
            &mut self,
            _session_id: &str,
            _key: &str,
            reply: ReplyTx,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            let _ = reply.try_send(Err(super::SCREENSHOT_UNSUPPORTED.into()));
        }
    }

    impl Render for PreviewPanel {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            // When the embedded browser is turned off in Settings → Browser, hide
            // the chrome and webview entirely and show a quiet placeholder.
            if !self.app_state.read(cx).settings.browser.enabled {
                return v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .px_8()
                    .text_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("browser.disabled_panel"));
            }
            let active = self.active_key(cx);

            // Honor a queued `--open-preview <url>` navigation once a session exists.
            if active.is_some()
                && let Some(url) = self
                    .app_state
                    .update(cx, |state, _| state.take_pending_preview_url())
            {
                self.navigate(active.as_deref().unwrap(), &url, window, cx);
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

            let body: AnyElement = match &active {
                Some(id) => match self.ensure_webview(id, window, cx) {
                    Some(view) => div().flex_1().min_h_0().child(view).into_any_element(),
                    None => v_flex()
                        .flex_1()
                        .gap_2()
                        .items_center()
                        .justify_center()
                        .px_8()
                        .text_center()
                        .text_color(cx.theme().muted_foreground)
                        .child(tcode_i18n::tr!("preview.unavailable"))
                        .child(
                            div()
                                .text_size(gpui::px(13.))
                                .child(tcode_i18n::tr!("preview.unavailable_hint")),
                        )
                        .into_any_element(),
                },
                None => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("preview.no_session"))
                    .into_any_element(),
            };

            // `ensure_webview` creates children hidden; make the owning
            // conversation visible only after the current layout owns it.
            self.sync_mounted_visibility(cx);

            v_flex()
                .size_full()
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
                .child(
                    Button::new("preview-back")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::ArrowLeft)
                        .tooltip(tcode_i18n::tr!("preview.back"))
                        .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx))),
                )
                .child(
                    Button::new("preview-forward")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::ArrowRight)
                        .tooltip(tcode_i18n::tr!("preview.forward"))
                        .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx))),
                )
                .child(
                    Button::new("preview-reload")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::Replace)
                        .tooltip(tcode_i18n::tr!("preview.reload"))
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                )
                .child(div().flex_1().min_w_0().child(Input::new(&self.url_input)))
                .child(
                    Button::new("preview-ports")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::Globe)
                        .tooltip(tcode_i18n::tr!("preview.scan_ports"))
                        .on_click(cx.listener(|this, _, _, cx| this.rescan_ports(cx))),
                )
                .child(
                    Button::new("preview-open-external")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::ExternalLink)
                        .tooltip(tcode_i18n::tr!("preview.open_external"))
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
                            if let Some(key) = this.active_key(cx) {
                                this.navigate(&key, &url, window, cx);
                            }
                        })),
                );
            }
            Some(row)
        }
    }
}

/// Linux: no WebView (see the module docs). The tab still exists — it renders a
/// muted placeholder — and the preview MCP server still starts, but every tool
/// call answers with an error instead of driving a browser that cannot exist.
#[cfg(target_os = "linux")]
mod placeholder {
    use gpui::{Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Window};
    use gpui_component::{ActiveTheme as _, v_flex};
    use preview_mcp::PreviewOp;

    use super::ReplyTx;
    use tcode_runtime::app::AppState;

    pub struct PreviewPanel;

    impl PreviewPanel {
        pub fn new(
            _app_state: Entity<AppState>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Self {
            Self
        }

        /// Every `preview_*` tool is unavailable here; the broker turns this
        /// `Err` into a normal MCP tool error.
        pub fn handle_op(
            &mut self,
            session_id: String,
            op: PreviewOp,
            reply: ReplyTx,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            log::info!(
                "preview: rejecting op {op:?} for session {session_id} (unsupported on Linux)"
            );
            let _ = reply.try_send(Err(
                tcode_i18n::tr!("preview.unsupported_linux").into_owned()
            ));
        }

        pub fn sync_visibility(&mut self, _cx: &mut Context<Self>) {}
    }

    impl Render for PreviewPanel {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .child(tcode_i18n::tr!("preview.unsupported_linux"))
        }
    }
}

/// The error `preview_screenshot` reports where screen capture has no reliable
/// implementation (Windows has no `screencapture` equivalent; Wayland forbids it
/// outright). Linux has no webview at all, so it never gets this far.
#[cfg(not(target_os = "linux"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
const SCREENSHOT_UNSUPPORTED: &str = "preview_screenshot is only supported on macOS";

/// Compute a `screencapture -R x,y,w,h` region string from the window origin and
/// the WebView's window-relative bounds. macOS-only, like its one caller.
#[cfg(target_os = "macos")]
fn screen_region(
    window_origin: gpui::Point<gpui::Pixels>,
    wv: gpui::Bounds<gpui::Pixels>,
) -> String {
    let x = f32::from(window_origin.x + wv.origin.x).round() as i32;
    let y = f32::from(window_origin.y + wv.origin.y).round() as i32;
    let w = f32::from(wv.size.width).round() as i32;
    let h = f32::from(wv.size.height).round() as i32;
    format!("{x},{y},{w},{h}")
}

/// What an automation tool answers when the platform webview cannot be created
/// (Windows without the WebView2 runtime): say so plainly, with the underlying
/// error, rather than leaving the agent to guess why nothing happened.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn unavailable_message(err: &str) -> String {
    format!(
        "the preview browser is unavailable on this machine \
         (the system webview component could not be created: {err})"
    )
}

/// Add a scheme to a bare host/port (so `localhost:5173` becomes a real URL).
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.contains("://") || trimmed.starts_with("about:") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routed_session_uses_active_draft_key_only_for_the_active_surface() {
        assert_eq!(
            preview_key_for_session(
                "physical-draft",
                Some("physical-draft"),
                Some("draft:project-a")
            ),
            "draft:project-a"
        );
        assert_eq!(
            preview_key_for_session(
                "stored-background",
                Some("physical-draft"),
                Some("draft:project-a")
            ),
            "stored-background"
        );
        assert_eq!(
            preview_key_for_session(
                "stored-active",
                Some("stored-active"),
                Some("stored-active")
            ),
            "stored-active"
        );
    }

    #[test]
    fn normalize_url_adds_a_scheme_to_bare_hosts() {
        assert_eq!(normalize_url("localhost:5173"), "http://localhost:5173");
        assert_eq!(normalize_url(" https://x.dev "), "https://x.dev");
        assert_eq!(normalize_url("about:blank"), "about:blank");
    }

    #[test]
    fn native_overlay_is_visible_only_while_preview_owns_it() {
        assert_eq!(
            visible_preview_key(Some("thread-a"), Route::Chat, false, true),
            Some("thread-a")
        );
        assert_eq!(
            visible_preview_key(Some("thread-a"), Route::Chat, false, false),
            None,
            "closing Preview or selecting Diff/Plan must hide the native child"
        );
        assert_eq!(
            visible_preview_key(Some("thread-b"), Route::Chat, true, true),
            None,
            "the command palette must cover the whole workspace"
        );
        assert_eq!(
            visible_preview_key(Some("thread-b"), Route::Settings, false, true),
            None,
            "leaving Chat unmounts the preview layout"
        );
        assert_eq!(visible_preview_key(None, Route::Chat, false, true), None);
    }

    /// The capture region is the window origin plus the WebView's own bounds.
    /// macOS-only, like the `screencapture` shell-out it feeds.
    #[cfg(target_os = "macos")]
    #[test]
    fn screen_region_is_the_window_origin_plus_the_webview_bounds() {
        assert_eq!(
            screen_region(
                gpui::point(gpui::px(10.), gpui::px(20.)),
                gpui::Bounds {
                    origin: gpui::point(gpui::px(5.), gpui::px(5.)),
                    size: gpui::size(gpui::px(100.), gpui::px(50.)),
                }
            ),
            "15,25,100,50"
        );
    }

    /// Off macOS (but where a webview exists — i.e. Windows) `preview_screenshot`
    /// surfaces a plain tool error instead of a broken capture. On Linux there is
    /// no webview at all and the whole panel is a placeholder.
    #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
    #[test]
    fn screenshot_is_unsupported_off_macos() {
        assert!(SCREENSHOT_UNSUPPORTED.contains("only supported on macOS"));
    }
}
