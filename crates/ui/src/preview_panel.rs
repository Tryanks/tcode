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

#[cfg(any(not(target_os = "linux"), test))]
use crate::window_state::Route;
use preview_mcp::PreviewReply;

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
        Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
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
    use crate::store::WorkspaceStore;
    use crate::window_caption;
    use crate::window_state::WindowState;

    fn wait_timeout_message(pending: &[String]) -> String {
        format!(
            "preview_wait_for timed out; unmet conditions: {}",
            pending.join(", ")
        )
    }

    pub struct PreviewPanel {
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        /// One native WebView per session id, created on first use.
        webviews: HashMap<String, Entity<WebView>>,
        /// Sessions whose WebView has begun a navigation. lb-wry queues (and drops
        /// the callback of) `evaluate_script_with_callback` until the first
        /// navigation starts flushing its pending-scripts buffer, so value-returning
        /// ops must wait until a session is "warm".
        warm: HashSet<String>,
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
        /// Discards a completed scan when a newer click has superseded it.
        port_scan_generation: u64,
        /// Why the platform webview could not be created (Windows without the
        /// WebView2 runtime). Set once; the tab then explains itself instead of
        /// retrying on every frame.
        webview_error: Option<String>,
        _subscriptions: Vec<Subscription>,
    }

    impl PreviewPanel {
        pub fn new(
            store: Entity<WorkspaceStore>,
            window_state: Entity<WindowState>,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            let url_input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(tcode_i18n::tr!("preview.url_placeholder"))
            });
            let subscriptions = vec![
                cx.observe(&store, |this, _, cx| {
                    // Native child views outlive GPUI layout nodes. Visibility
                    // therefore follows WorkspaceStore directly, even while this
                    // entity is no longer mounted in the right-panel tree.
                    this.sync_visibility(cx);
                    cx.notify();
                }),
                cx.subscribe_in(&url_input, window, Self::on_url_event),
            ];
            Self {
                store,
                window_state,
                webviews: HashMap::new(),
                warm: HashSet::new(),
                url_input,
                mirrored: None,
                active_identity: None,
                dev_ports: Vec::new(),
                port_scan_generation: 0,
                webview_error: None,
                _subscriptions: subscriptions,
            }
        }

        /// Reconcile the stable conversation key with the physical session id.
        /// Draft -> stored-thread commits retain the same session id, so move
        /// all cached browser state across that one key transition.
        fn active_key(&mut self, cx: &Context<Self>) -> Option<String> {
            let current = self.store.read(cx).preview_active_identity(cx);

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
            let active_session_id = self.store.read(cx).preview_active_session_id(cx);
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
                let window_state = self.window_state.read(cx);
                visible_preview_key(
                    active.as_deref(),
                    window_state.route,
                    window_state.palette_open,
                    self.store.read(cx).preview_panel_showing(cx),
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
            self.store
                .update(cx, |store, cx| store.set_preview_url(key, url, cx));
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
                && let Some(url) = self.store.read(cx).preview_url(&id)
            {
                cx.open_url(&url);
            }
        }

        /// The chrome's X: close the Preview tab *and* drop this conversation's
        /// WebView, so the page is torn down (scripts, media, sockets) rather
        /// than kept running behind a closed panel. The next open or agent op
        /// recreates a fresh webview on demand.
        fn close_panel(&mut self, cx: &mut Context<Self>) {
            if let Some(key) = self.active_key(cx) {
                self.webviews.remove(&key);
                self.warm.remove(&key);
                self.store
                    .update(cx, |store, cx| store.clear_preview_chrome(&key, cx));
            }
            // Un-mirror so a later reopen refreshes the address bar from the
            // (now empty) URL map instead of showing the stale address.
            self.mirrored = None;
            self.store
                .update(cx, |store, cx| store.close_preview_panel(cx));
            cx.notify();
        }

        fn rescan_ports(&mut self, cx: &mut Context<Self>) {
            self.port_scan_generation = self.port_scan_generation.wrapping_add(1);
            let generation = self.port_scan_generation;
            cx.spawn(async move |this, cx| {
                let ports = tcode_runtime::blocking::unblock(
                    cx.background_executor(),
                    ports::scan_listening,
                )
                .await;
                let _ = this.update(cx, |panel, cx| {
                    if panel.port_scan_generation == generation {
                        panel.dev_ports = ports;
                        cx.notify();
                    }
                });
            })
            .detach();
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
            let browser = self.store.read(cx).preview_browser_settings(cx);
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
                    self.store.update(cx, |store, cx| {
                        store.open_preview_panel_for(&session_id, cx);
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
                        "url": self.store.read(cx).preview_url(&key),
                        "note": "call preview_status for live page state once loaded",
                    });
                    let _ = reply.try_send(Ok(PreviewReply::Json(payload)));
                }
                PreviewOp::Navigate { url } => {
                    self.store.update(cx, |store, cx| {
                        store.open_preview_panel_for(&session_id, cx);
                    });
                    self.navigate(&key, &url, window, cx);
                    if let Some(err) = &self.webview_error {
                        let _ = reply.try_send(Err(unavailable_message(err)));
                        return;
                    }
                    let payload = serde_json::json!({
                        "ok": true,
                        "url": self.store.read(cx).preview_url(&key),
                        "note": "page is loading; call preview_status for live state",
                    });
                    let _ = reply.try_send(Ok(PreviewReply::Json(payload)));
                }
                PreviewOp::Status => self.status(&key, reply, window, cx),
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
                PreviewOp::Resize { width, height } => {
                    self.store.update(cx, |store, cx| {
                        store.open_preview_panel_for(&session_id, cx);
                    });
                    let payload = match (width, height) {
                        (Some(width), Some(height)) => {
                            self.store.update(cx, |store, cx| {
                                store.set_preview_canvas(&key, Some((width, height)), cx);
                            });
                            serde_json::json!({
                                "ok": true,
                                "mode": "fixed",
                                "width": width,
                                "height": height,
                                "note": "fixed canvas is clamped to the panel if larger",
                            })
                        }
                        _ => {
                            self.store.update(cx, |store, cx| {
                                store.set_preview_canvas(&key, None, cx);
                            });
                            serde_json::json!({
                                "ok": true,
                                "mode": "fill",
                                "note": "preview fills the available panel",
                            })
                        }
                    };
                    self.sync_visibility(cx);
                    cx.notify();
                    let _ = reply.try_send(Ok(PreviewReply::Json(payload)));
                }
                // `key` is the routed conversation key; bind the keyboard key
                // apart so it cannot shadow it into `eval_json`'s session slot.
                PreviewOp::Press {
                    key: pressed,
                    modifiers,
                } => self.eval_json(&key, &js::press(&pressed, &modifiers), reply, window, cx),
                PreviewOp::Scroll {
                    delta_x,
                    delta_y,
                    selector,
                } => self.eval_json(
                    &key,
                    &js::scroll(delta_x, delta_y, selector.as_deref()),
                    reply,
                    window,
                    cx,
                ),
                PreviewOp::WaitFor {
                    selector,
                    text,
                    url_includes,
                    timeout_ms,
                } => self.wait_for(
                    &key,
                    selector,
                    text,
                    url_includes,
                    timeout_ms,
                    reply,
                    window,
                    cx,
                ),
                PreviewOp::Screenshot => self.screenshot(&session_id, &key, reply, window, cx),
            }
        }

        /// Add this conversation's canvas setting to the otherwise opaque page
        /// status object returned by JavaScript.
        fn status(
            &mut self,
            key: &str,
            reply: ReplyTx,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            let canvas = self
                .store
                .read(cx)
                .preview_canvas(key)
                .map(|(width, height)| {
                    serde_json::json!({
                        "mode": "fixed",
                        "width": width,
                        "height": height,
                    })
                })
                .unwrap_or_else(|| serde_json::json!({ "mode": "fill" }));
            let (status_reply, status_result) = async_channel::bounded(1);
            cx.spawn(async move |_, _| {
                let result = match status_result.recv().await {
                    Ok(Ok(PreviewReply::Json(mut value))) => {
                        if let Some(object) = value.as_object_mut() {
                            object.insert("canvas".into(), canvas);
                            Ok(PreviewReply::Json(value))
                        } else {
                            Err("preview status returned a non-object value".into())
                        }
                    }
                    Ok(result) => result,
                    Err(_) => Err("preview status evaluation was dropped".into()),
                };
                let _ = reply.send(result).await;
            })
            .detach();
            self.eval_json(key, js::STATUS, status_reply, window, cx);
        }

        /// Poll a one-shot page probe every 250ms until it matches or reaches
        /// its deadline. Each evaluation has its own watchdog because native
        /// WebViews can drop callbacks during navigation.
        #[allow(clippy::too_many_arguments)]
        fn wait_for(
            &mut self,
            key: &str,
            selector: Option<String>,
            text: Option<String>,
            url_includes: Option<String>,
            timeout_ms: u64,
            reply: ReplyTx,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            if self.ensure_webview(key, window, cx).is_none() {
                let err = self.webview_error.clone().unwrap_or_default();
                let _ = reply.try_send(Err(unavailable_message(&err)));
                return;
            }
            let cold = !self.warm.contains(key);
            let key = key.to_string();
            let probe = js::wait_for_probe(
                selector.as_deref(),
                text.as_deref(),
                url_includes.as_deref(),
            );
            let mut pending = Vec::new();
            if selector.is_some() {
                pending.push("selector".to_string());
            }
            if text.is_some() {
                pending.push("text".to_string());
            }
            if url_includes.is_some() {
                pending.push("urlIncludes".to_string());
            }
            cx.spawn(async move |this, cx| {
                let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
                if cold {
                    cx.background_executor()
                        .timer(Duration::from_millis(700))
                        .await;
                    if this
                        .update(cx, |panel, _| {
                            panel.warm.insert(key.clone());
                        })
                        .is_err()
                    {
                        let _ = reply
                            .send(Err("preview panel was dropped while waiting".into()))
                            .await;
                        return;
                    }
                }
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        let _ = reply.send(Err(wait_timeout_message(&pending))).await;
                        return;
                    }
                    let (probe_reply, probe_result) = async_channel::bounded(1);
                    if this
                        .update(cx, |panel, cx| {
                            panel.eval_now(&key, &probe, probe_reply.clone(), cx);
                        })
                        .is_err()
                    {
                        let _ = reply
                            .send(Err("preview panel was dropped while waiting".into()))
                            .await;
                        return;
                    }

                    let watchdog_delay = remaining.min(Duration::from_secs(5));
                    let watchdog_reply = probe_reply;
                    let watchdog_error = if remaining <= Duration::from_secs(5) {
                        wait_timeout_message(&pending)
                    } else {
                        "preview wait probe evaluation timed out".into()
                    };
                    let watchdog_timer = cx.background_executor().timer(watchdog_delay);
                    cx.background_executor()
                        .spawn(async move {
                            watchdog_timer.await;
                            let _ = watchdog_reply.try_send(Err(watchdog_error));
                        })
                        .detach();

                    let value = match probe_result.recv().await {
                        Ok(Ok(PreviewReply::Json(value))) => value,
                        Ok(Ok(PreviewReply::Image { .. })) => {
                            let _ = reply
                                .send(Err("preview wait probe returned an image".into()))
                                .await;
                            return;
                        }
                        Ok(Err(error)) => {
                            let _ = reply.send(Err(error)).await;
                            return;
                        }
                        Err(_) => {
                            let _ = reply
                                .send(Err("preview wait probe evaluation was dropped".into()))
                                .await;
                            return;
                        }
                    };
                    if value.get("matched").and_then(serde_json::Value::as_bool) == Some(true) {
                        let _ = reply.send(Ok(PreviewReply::Json(value))).await;
                        return;
                    }
                    pending = value
                        .get("pending")
                        .and_then(serde_json::Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_else(|| pending.clone());
                    cx.background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                }
            })
            .detach();
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

        /// Snapshot the native WKWebView in-process and answer with a base64 PNG.
        ///
        /// macOS only. Elsewhere the tool reports a normal MCP error rather than
        /// pretending to have a portable native-webview snapshot implementation.
        #[cfg(target_os = "macos")]
        fn screenshot(
            &mut self,
            session_id: &str,
            key: &str,
            reply: ReplyTx,
            _window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            use block2::RcBlock;
            use gpui::px;
            use objc2_app_kit::NSImage;
            use objc2_foundation::NSError;
            use objc2_web_kit::WKWebView;
            use wry::WebViewExtMacOS as _;

            let visible = {
                let window_state = self.window_state.read(cx);
                if self.store.read(cx).preview_active_session_id(cx).as_deref() != Some(session_id)
                {
                    let _ = reply.try_send(Err(
                        "preview is not visible; the user is viewing another conversation".into(),
                    ));
                    return;
                }
                visible_preview_key(
                    Some(key),
                    window_state.route,
                    window_state.palette_open,
                    self.store.read(cx).preview_panel_showing(cx),
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
            let native = view.read(cx).raw().webview();
            let webview: &WKWebView = &native;
            let callback_reply = reply.clone();
            let handler = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
                let result = if let Some(image) = unsafe { image.as_ref() } {
                    super::snapshot_reply(image)
                } else if !error.is_null() {
                    Err("WKWebView snapshot failed".into())
                } else {
                    Err("WKWebView snapshot returned no image".into())
                };
                let _ = callback_reply.try_send(result);
            });
            unsafe {
                webview.takeSnapshotWithConfiguration_completionHandler(None, &handler);
            }

            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let _ = reply.try_send(Err("WKWebView snapshot timed out after 5 seconds".into()));
            })
            .detach();
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
            if !self.store.read(cx).preview_browser_settings(cx).enabled {
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
                    .window_state
                    .update(cx, |state, _| state.pending_preview_url.take())
            {
                self.navigate(active.as_deref().unwrap(), &url, window, cx);
            }

            // Mirror the active session's URL into the address bar when it changes.
            if active != self.mirrored {
                let value = active
                    .as_ref()
                    .and_then(|id| self.store.read(cx).preview_url(id))
                    .unwrap_or_default();
                self.url_input
                    .update(cx, |state, cx| state.set_value(&value, window, cx));
                self.mirrored = active.clone();
            }

            let body: AnyElement = match &active {
                Some(id) => match self.ensure_webview(id, window, cx) {
                    Some(view) => {
                        if let Some((width, height)) = self.store.read(cx).preview_canvas(id) {
                            div()
                                .flex()
                                .flex_1()
                                .min_h_0()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .flex_none()
                                        .w(px(width as f32))
                                        .h(px(height as f32))
                                        .max_w_full()
                                        .max_h_full()
                                        .child(view),
                                )
                                .into_any_element()
                        } else {
                            div().flex_1().min_h_0().child(view).into_any_element()
                        }
                    }
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
                .child(self.render_chrome(window, cx))
                .children(self.render_port_row(cx))
                .child(body)
        }
    }

    impl PreviewPanel {
        fn render_chrome(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
            // Windows: an open Preview tab is the rightmost column, so its
            // chrome row hosts the caption buttons. The row is normally only as
            // tall as its controls — pin it to the shell's 52px top strip and
            // drop the trailing/vertical padding on the caption side so the
            // buttons reach the window's true top-right corner.
            let hosts_caption = {
                let (diff_open, right_tab) = self.store.read(cx).window_caption_state(cx);
                window_caption::hosts_caption_for_state(
                    window_caption::CaptionSurface::Preview,
                    self.window_state.read(cx).route,
                    diff_open,
                    right_tab,
                )
            };
            h_flex()
                .flex_none()
                .w_full()
                .gap_1()
                .p_1()
                .when(hosts_caption, |chrome| {
                    chrome
                        .h(px(window_caption::CAPTION_STRIP_HEIGHT))
                        .pt_0()
                        .pb_0()
                        .pr_0()
                })
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
                .child(
                    Button::new("preview-close")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::Close)
                        .tooltip(tcode_i18n::tr!("preview.close"))
                        .on_click(cx.listener(|this, _, _, cx| this.close_panel(cx))),
                )
                // Last child, so the chrome's own controls stay to its left.
                .children(hosts_caption.then(|| window_caption::caption_controls(window, cx)))
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
    use crate::store::WorkspaceStore;
    use crate::window_state::WindowState;

    pub struct PreviewPanel;

    impl PreviewPanel {
        pub fn new(
            _store: Entity<WorkspaceStore>,
            _window_state: Entity<WindowState>,
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

/// The error `preview_screenshot` reports where native-webview snapshots have no
/// implementation. Linux has no webview at all, so it never gets this far.
#[cfg(not(target_os = "linux"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
const SCREENSHOT_UNSUPPORTED: &str = "preview_screenshot is only supported on macOS";

#[cfg(target_os = "macos")]
fn snapshot_reply(image: &objc2_app_kit::NSImage) -> Result<PreviewReply, String> {
    use base64::Engine as _;
    use objc2_core_foundation::{CFMutableData, CFString};
    use objc2_image_io::CGImageDestination;

    let cg_image =
        unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }
            .ok_or_else(|| "failed to obtain CGImage from WKWebView snapshot".to_string())?;
    let data = CFMutableData::new(None, 0)
        .ok_or_else(|| "failed to allocate PNG destination data".to_string())?;
    let png_type = CFString::from_static_str("public.png");
    let destination = unsafe { CGImageDestination::with_data(&data, &png_type, 1, None) }
        .ok_or_else(|| "failed to create PNG image destination".to_string())?;
    unsafe {
        destination.add_image(&cg_image, None);
        if !destination.finalize() {
            return Err("failed to finalize WKWebView snapshot PNG".into());
        }
    }
    Ok(PreviewReply::Image {
        mime: "image/png".into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(data.to_vec()),
    })
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

    /// Off macOS (but where a webview exists — i.e. Windows) `preview_screenshot`
    /// surfaces a plain tool error instead of a broken capture. On Linux there is
    /// no webview at all and the whole panel is a placeholder.
    #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
    #[test]
    fn screenshot_is_unsupported_off_macos() {
        assert!(SCREENSHOT_UNSUPPORTED.contains("only supported on macOS"));
    }
}
