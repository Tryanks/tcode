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
//! Windows creation is deliberately asynchronous. WebView2 construction is
//! asynchronous underneath, but wry's synchronous `build_as_child` waits by
//! running a nested Win32 message pump. That pump can dispatch GPUI teardown
//! while the parent HWND is still underneath the creation call. We instead use
//! `build_as_child_async` on GPUI's window foreground executor and generation-tag
//! each pending child, so teardown can cancel the slot without re-entering GPUI
//! or allowing a stale completion to replace a newer preview. macOS keeps the
//! proven synchronous child-view path.
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
type ReplyTx = smol::channel::Sender<Result<PreviewReply, String>>;

#[cfg(not(target_os = "linux"))]
pub use native::PreviewPanel;

#[cfg(target_os = "linux")]
pub use placeholder::PreviewPanel;

#[cfg(not(target_os = "linux"))]
mod native {
    use std::collections::{HashMap, HashSet};
    #[cfg(target_os = "windows")]
    use std::future::Future as _;
    #[cfg(target_os = "windows")]
    use std::rc::Rc;
    use std::time::Duration;

    use crate::theme::ActiveTheme as _;
    use crate::widgets::button::{Button, ButtonVariants as _};
    use crate::widgets::input::{Input, InputEvent, InputState};
    use crate::{icon::IconName, sizing::Sizable as _};
    use gpui::{
        AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
        Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
    };
    use gpui_base::{h_flex, v_flex};
    use gpui_wry::WebView;
    use preview_mcp::{PreviewOp, PreviewReply, js, ports};
    use raw_window_handle::HasWindowHandle as _;

    use super::{
        ReplyTx, normalize_url, preview_key_for_session, unavailable_message, visible_preview_key,
    };
    use crate::store::WorkspaceStore;
    use crate::window_caption;
    use crate::window_state::WindowState;

    const STARTING_MESSAGE: &str = "preview is starting; retry the operation shortly";
    #[cfg(target_os = "windows")]
    const SMOKE_CREATION_QUEUED: &str = "preview creation is queued";
    #[cfg(target_os = "windows")]
    const SMOKE_CREATION_IN_FLIGHT: &str = "preview creation is in flight";
    #[cfg(target_os = "windows")]
    const SMOKE_CREATION_PAUSE: Duration = Duration::from_millis(50);

    enum WebViewSlot {
        #[cfg(target_os = "windows")]
        Creating {
            id: u64,
            phase: CreationPhase,
            pending_url: Option<String>,
        },
        Ready(Entity<WebView>),
    }

    impl WebViewSlot {
        fn ready(&self) -> Option<&Entity<WebView>> {
            match self {
                #[cfg(target_os = "windows")]
                Self::Creating { .. } => None,
                Self::Ready(view) => Some(view),
            }
        }

        fn is_ready(&self) -> bool {
            self.ready().is_some()
        }
    }

    #[cfg(target_os = "windows")]
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CreationPhase {
        Queued,
        InFlight,
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    enum WebViewAvailability {
        Starting,
        Ready(Entity<WebView>),
        Unavailable,
    }

    fn set_webview_visible(view: &mut WebView, visible: bool) {
        // gpui-wry keeps its own visibility bit but does not expose the native
        // result. Repeat the idempotent native operation once so teardown races
        // are observable instead of silently discarded.
        if visible {
            view.show();
            if let Err(error) = view.raw().set_visible(true) {
                log::debug!("preview: failed to show native webview: {error}");
            }
        } else {
            view.hide();
            if let Err(error) = view.raw().focus_parent() {
                log::debug!("preview: failed to focus parent while hiding webview: {error}");
            }
            if let Err(error) = view.raw().set_visible(false) {
                log::debug!("preview: failed to hide native webview: {error}");
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn windows_web_context() -> Result<Rc<smol::lock::Mutex<wry::WebContext>>, String> {
        let store = tcode_services::store::SessionStore::open_default()
            .map_err(|error| format!("failed to resolve tcode data directory: {error}"))?;
        let user_data_dir = store.root().join("WebView2");
        std::fs::create_dir_all(&user_data_dir).map_err(|error| {
            format!(
                "failed to create WebView2 user-data directory {}: {error}",
                user_data_dir.display()
            )
        })?;
        log::debug!(
            "preview: using WebView2 user-data directory {}",
            user_data_dir.display()
        );
        Ok(Rc::new(smol::lock::Mutex::new(wry::WebContext::new(Some(
            user_data_dir,
        )))))
    }

    #[cfg(target_os = "windows")]
    fn drop_raw_webview(raw: wry::WebView, key: &str, reason: &str) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(raw))).is_err() {
            log::error!("preview: raw webview drop panicked for {key} after {reason}");
        }
    }

    fn wait_timeout_message(pending: &[String]) -> String {
        format!(
            "preview_wait_for timed out; unmet conditions: {}",
            pending.join(", ")
        )
    }

    pub struct PreviewPanel {
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        /// One native WebView slot per session id, created on first use.
        webviews: HashMap<String, WebViewSlot>,
        /// All Windows previews share one explicit app-local WebView2 profile.
        /// The async mutex keeps wry's `&mut WebContext` borrow exclusive across
        /// concurrent child creations without blocking the UI thread.
        #[cfg(target_os = "windows")]
        web_context: Option<Rc<smol::lock::Mutex<wry::WebContext>>>,
        /// Monotonic identity that prevents a cancelled completion from filling
        /// a replacement slot at the same conversation key.
        #[cfg(target_os = "windows")]
        next_creation_id: u64,
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
        /// Harness-only routing override. Normal runs leave this unset and use
        /// the active conversation identity from `WorkspaceStore`.
        smoke_active_key: Option<String>,
        smoke_visible: bool,
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
                InputState::new(window, cx).placeholder(crate::tr!("preview.url_placeholder"))
            });
            #[cfg(target_os = "windows")]
            let (web_context, webview_error) = match windows_web_context() {
                Ok(context) => (Some(context), None),
                Err(error) => {
                    log::warn!("preview: no webview ({error})");
                    (None, Some(error))
                }
            };
            #[cfg(not(target_os = "windows"))]
            let webview_error = None;
            let subscriptions = vec![
                cx.observe(&store, |this, _, cx| {
                    // Native child views outlive GPUI layout nodes. Visibility
                    // therefore follows WorkspaceStore directly, even while this
                    // entity is no longer mounted in the right-panel tree.
                    this.prune_deleted_webviews(cx);
                    this.sync_visibility(cx);
                    cx.notify();
                }),
                cx.subscribe_in(&url_input, window, Self::on_url_event),
            ];
            Self {
                store,
                window_state,
                webviews: HashMap::new(),
                #[cfg(target_os = "windows")]
                web_context,
                #[cfg(target_os = "windows")]
                next_creation_id: 0,
                warm: HashSet::new(),
                url_input,
                mirrored: None,
                active_identity: None,
                dev_ports: Vec::new(),
                port_scan_generation: 0,
                webview_error,
                smoke_active_key: None,
                smoke_visible: false,
                _subscriptions: subscriptions,
            }
        }

        /// Reconcile the stable conversation key with the physical session id.
        /// Draft -> stored-thread commits retain the same session id, so move
        /// all cached browser state across that one key transition.
        fn active_key(&mut self, cx: &Context<Self>) -> Option<String> {
            if let Some(key) = &self.smoke_active_key {
                return Some(key.clone());
            }
            let current = self.store.read(cx).preview_active_identity();

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
            let active_session_id = self.store.read(cx).active_session_id();
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
            let visible = if self.smoke_active_key.is_some() {
                self.smoke_visible.then_some(active).flatten()
            } else {
                let window_state = self.window_state.read(cx);
                visible_preview_key(
                    active.as_deref(),
                    window_state.route,
                    window_state.palette_open,
                    self.store.read(cx).preview_panel_showing(),
                )
                .map(str::to_string)
            };
            for (key, slot) in &self.webviews {
                let Some(view) = slot.ready() else {
                    continue;
                };
                let should_show = Some(key) == visible.as_ref();
                view.update(cx, |view, _| {
                    if should_show && allow_show {
                        set_webview_visible(view, true);
                    } else if !should_show {
                        set_webview_visible(view, false);
                    }
                });
            }
        }

        /// Get or lazily create the WebView for `session_id`.
        ///
        /// `Unavailable` when the platform webview cannot be created — on
        /// Windows that usually means the WebView2 runtime is absent. Only the
        /// preview browser needs it, so this is a missing feature, not a dead
        /// app: the tab explains itself and every other surface keeps working.
        fn ensure_webview(
            &mut self,
            session_id: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> WebViewAvailability {
            if let Some(slot) = self.webviews.get(session_id) {
                return match slot {
                    #[cfg(target_os = "windows")]
                    WebViewSlot::Creating { .. } => WebViewAvailability::Starting,
                    WebViewSlot::Ready(view) => WebViewAvailability::Ready(view.clone()),
                };
            }
            if self.webview_error.is_some() {
                return WebViewAvailability::Unavailable;
            }

            #[cfg(target_os = "windows")]
            {
                self.start_webview_creation(session_id, window, cx)
            }

            #[cfg(not(target_os = "windows"))]
            {
                self.create_webview_sync(session_id, window, cx)
            }
        }

        fn record_webview_error(&mut self, error: String, cx: &mut Context<Self>) {
            log::warn!("preview: no webview ({error})");
            self.webview_error = Some(error);
            // An unavailable platform component invalidates every queued build.
            // Already-ready children remain usable until their normal teardown.
            self.webviews.retain(|_, slot| slot.is_ready());
            cx.notify();
        }

        #[cfg(not(target_os = "windows"))]
        fn create_webview_sync(
            &mut self,
            session_id: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> WebViewAvailability {
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
                Err(error) => {
                    self.record_webview_error(error, cx);
                    return WebViewAvailability::Unavailable;
                }
            };
            let webview = cx.new(|cx| {
                let mut view = WebView::new(raw, window, cx);
                set_webview_visible(&mut view, false);
                view
            });
            self.webviews
                .insert(session_id.to_string(), WebViewSlot::Ready(webview.clone()));
            WebViewAvailability::Ready(webview)
        }

        #[cfg(target_os = "windows")]
        fn start_webview_creation(
            &mut self,
            session_id: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> WebViewAvailability {
            let Some(web_context) = self.web_context.clone() else {
                return WebViewAvailability::Unavailable;
            };
            let parent = match window.window_handle() {
                Ok(handle) => handle.as_raw(),
                Err(error) => {
                    self.record_webview_error(error.to_string(), cx);
                    return WebViewAvailability::Unavailable;
                }
            };

            self.next_creation_id = self.next_creation_id.wrapping_add(1).max(1);
            let creation_id = self.next_creation_id;
            let creation_key = session_id.to_string();
            let pending_url = self.store.read(cx).preview_url(session_id);
            let smoke_creation = self.smoke_active_key.is_some();
            self.webviews.insert(
                creation_key.clone(),
                WebViewSlot::Creating {
                    id: creation_id,
                    phase: CreationPhase::Queued,
                    pending_url,
                },
            );

            cx.spawn_in(window, async move |panel, cx| {
                // WebViewBuilder borrows WebContext mutably for the lifetime of
                // its future. Serialize that borrow without blocking GPUI; a
                // cancelled queued slot is discarded before wry is ever polled.
                let mut web_context = web_context.lock().await;
                let slot_is_live = panel
                    .read_with(cx, |panel, _| panel.has_creation(creation_id))
                    .unwrap_or(false);
                if !slot_is_live {
                    return;
                }
                if cx.update(|_, _| ()).is_err() {
                    let _ = panel.update(cx, |panel, cx| {
                        panel.remove_creation(creation_id);
                        cx.notify();
                    });
                    return;
                }

                // SAFETY: this task is confined to GPUI's UI thread and just
                // revalidated the owning GPUI window without yielding. wry reads
                // the handle synchronously on the first poll, before awaiting
                // WebView2's environment/controller callbacks. The HWND may be
                // destroyed after that await by design; the async wry path then
                // completes with either an error or a raw child we immediately
                // discard unless the same window/panel/slot are still live.
                let parent = unsafe { raw_window_handle::WindowHandle::borrow_raw(parent) };
                let built = {
                    let builder = wry::WebViewBuilder::new_with_web_context(&mut web_context)
                        .with_devtools(true)
                        .with_url("about:blank");
                    let mut build = Box::pin(builder.build_as_child_async(&parent));
                    let first_poll = std::future::poll_fn(|task_cx| {
                        std::task::Poll::Ready(match build.as_mut().poll(task_cx) {
                            std::task::Poll::Ready(result) => Some(result),
                            std::task::Poll::Pending => None,
                        })
                    })
                    .await;
                    match first_poll {
                        Some(result) => result,
                        None => {
                            let _ = panel.update(cx, |panel, cx| {
                                if panel.mark_creation_in_flight(creation_id) {
                                    cx.notify();
                                }
                            });
                            // Give the lifecycle harness a deterministic window
                            // in which to remove an actually-polled creation.
                            if smoke_creation {
                                cx.background_executor().timer(SMOKE_CREATION_PAUSE).await;
                            }
                            build.as_mut().await
                        }
                    }
                };
                drop(web_context);

                match built {
                    Ok(raw) => {
                        let mut raw = Some(raw);
                        let installed = matches!(
                            cx.update(|window, app| {
                                panel.update(app, |panel, cx| {
                                    let Some((key, pending_url)) =
                                        panel.remove_creation(creation_id)
                                    else {
                                        return false;
                                    };
                                    panel.install_created_webview(
                                        key,
                                        pending_url,
                                        raw.take().expect("raw webview already consumed"),
                                        window,
                                        cx,
                                    );
                                    true
                                })
                            }),
                            Ok(Ok(true))
                        );
                        if let Some(raw) = raw {
                            drop_raw_webview(raw, &creation_key, "creation cancellation");
                        }
                        if !installed {
                            log::debug!(
                                "preview: discarded stale creation {creation_id} for {creation_key}"
                            );
                        }
                    }
                    Err(error) => {
                        let error = error.to_string();
                        let recorded = matches!(
                            cx.update(|_, app| {
                                panel.update(app, |panel, cx| {
                                    if !panel.has_creation(creation_id) {
                                        return false;
                                    }
                                    panel.record_webview_error(error.clone(), cx);
                                    true
                                })
                            }),
                            Ok(Ok(true))
                        );
                        if !recorded {
                            log::debug!(
                                "preview: creation {creation_id} for {creation_key} failed after teardown: {error}"
                            );
                        }
                    }
                }
            })
            .detach();

            WebViewAvailability::Starting
        }

        #[cfg(target_os = "windows")]
        fn has_creation(&self, creation_id: u64) -> bool {
            self.webviews.values().any(|slot| {
                matches!(
                    slot,
                    WebViewSlot::Creating { id, .. } if *id == creation_id
                )
            })
        }

        #[cfg(target_os = "windows")]
        fn mark_creation_in_flight(&mut self, creation_id: u64) -> bool {
            for slot in self.webviews.values_mut() {
                if let WebViewSlot::Creating { id, phase, .. } = slot
                    && *id == creation_id
                {
                    *phase = CreationPhase::InFlight;
                    return true;
                }
            }
            false
        }

        #[cfg(target_os = "windows")]
        fn remove_creation(&mut self, creation_id: u64) -> Option<(String, Option<String>)> {
            let key = self.webviews.iter().find_map(|(key, slot)| {
                matches!(
                    slot,
                    WebViewSlot::Creating { id, .. } if *id == creation_id
                )
                .then(|| key.clone())
            })?;
            let WebViewSlot::Creating { pending_url, .. } = self.webviews.remove(&key)? else {
                unreachable!("creation key stopped referring to a creating slot");
            };
            Some((key, pending_url))
        }

        #[cfg(target_os = "windows")]
        fn install_created_webview(
            &mut self,
            key: String,
            pending_url: Option<String>,
            raw: wry::WebView,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            let url = self.store.read(cx).preview_url(&key).or(pending_url);
            let warm = if let Some(url) = &url {
                match raw.load_url(url) {
                    Ok(()) => true,
                    Err(error) => {
                        log::warn!("preview: failed to replay URL for {key}: {error}");
                        false
                    }
                }
            } else {
                false
            };
            if let Err(error) = raw.set_bounds(wry::Rect::default()) {
                log::debug!("preview: failed to reset created webview bounds for {key}: {error}");
            }
            if let Err(error) = raw.set_visible(false) {
                log::debug!("preview: failed to hide created webview for {key}: {error}");
            }
            let webview = cx.new(|cx| {
                let mut view = WebView::new(raw, window, cx);
                set_webview_visible(&mut view, false);
                view
            });
            self.webviews
                .insert(key.clone(), WebViewSlot::Ready(webview));
            if warm {
                self.warm.insert(key);
            }
            self.sync_visibility(cx);
            cx.notify();
        }

        pub(crate) fn smoke_create(
            &mut self,
            key: &str,
            url: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Result<(), String> {
            self.smoke_active_key = Some(key.to_string());
            self.smoke_visible = true;
            match self.navigate(key, url, window, cx) {
                WebViewAvailability::Ready(_) => Ok(()),
                WebViewAvailability::Unavailable => Err(self
                    .webview_error
                    .clone()
                    .unwrap_or_else(|| unavailable_message("unknown creation failure"))),
                WebViewAvailability::Starting => {
                    #[cfg(target_os = "windows")]
                    if let Some(WebViewSlot::Creating { phase, .. }) = self.webviews.get(key) {
                        return Err(match phase {
                            CreationPhase::Queued => SMOKE_CREATION_QUEUED,
                            CreationPhase::InFlight => SMOKE_CREATION_IN_FLIGHT,
                        }
                        .into());
                    }
                    Err(STARTING_MESSAGE.into())
                }
            }
        }

        pub(crate) fn smoke_set_visible(&mut self, key: Option<&str>, cx: &mut Context<Self>) {
            if let Some(key) = key {
                self.smoke_active_key = Some(key.to_string());
                self.smoke_visible = true;
            } else {
                self.smoke_visible = false;
            }
            self.update_visibility(true, cx);
            cx.notify();
        }

        pub(crate) fn smoke_drop(&mut self, key: &str, cx: &mut Context<Self>) {
            self.drop_webview(key);
            cx.notify();
        }

        fn drop_webview(&mut self, key: &str) {
            self.webviews.remove(key);
            self.warm.remove(key);
            if self.mirrored.as_deref() == Some(key) {
                self.mirrored = None;
            }
        }

        fn prune_deleted_webviews(&mut self, cx: &Context<Self>) {
            if self.smoke_active_key.is_some() {
                return;
            }
            let live = self.store.read(cx).preview_live_keys();
            let deleted = self
                .webviews
                .keys()
                .filter(|key| !live.contains(*key))
                .cloned()
                .collect::<Vec<_>>();
            for key in deleted {
                self.drop_webview(&key);
            }
        }

        /// Navigate one conversation's WebView to `url`, remembering it.
        fn navigate(
            &mut self,
            key: &str,
            url: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> WebViewAvailability {
            let url = normalize_url(url);
            self.store
                .update(cx, |store, cx| store.set_preview_url(key, url.clone(), cx));
            let availability = self.ensure_webview(key, window, cx);
            match &availability {
                WebViewAvailability::Ready(webview) => {
                    match webview.read(cx).raw().load_url(&url) {
                        Ok(()) => {
                            // A navigation flushes lb-wry's pending-scripts buffer,
                            // so subsequent evaluate callbacks will fire.
                            self.warm.insert(key.to_string());
                        }
                        Err(error) => {
                            log::warn!("preview: failed to navigate {key}: {error}");
                        }
                    }
                }
                #[cfg(target_os = "windows")]
                WebViewAvailability::Starting => {
                    if let Some(WebViewSlot::Creating { pending_url, .. }) =
                        self.webviews.get_mut(key)
                    {
                        *pending_url = Some(url);
                    }
                }
                #[cfg(not(target_os = "windows"))]
                WebViewAvailability::Starting => {}
                WebViewAvailability::Unavailable => {}
            }
            self.sync_visibility(cx);
            cx.notify();
            availability
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
            if let Some(view) = self.webviews.get(session_id).and_then(WebViewSlot::ready) {
                let _ = view.read(cx).raw().evaluate_script(script);
            }
        }

        // ---- chrome actions -------------------------------------------------

        fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            if let Some(id) = self.active_key(cx)
                && let WebViewAvailability::Ready(view) = self.ensure_webview(&id, window, cx)
            {
                view.update(cx, |view, _| {
                    if let Err(error) = view.back() {
                        log::debug!("preview: failed to navigate back during teardown: {error}");
                    }
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
                self.drop_webview(&key);
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
                let ports = cx
                    .background_executor()
                    .spawn(async { ports::scan_listening() })
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
            let browser = self.store.read(cx).preview_browser_settings();
            if !browser.enabled {
                let _ = reply.try_send(Err(crate::tr!("browser.disabled_error").into_owned()));
                return;
            }
            if matches!(&op, PreviewOp::Evaluate { .. }) && !browser.allow_evaluate {
                let _ = reply.try_send(Err(
                    crate::tr!("browser.evaluate_disabled_error").into_owned()
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
            let (status_reply, status_result) = smol::channel::bounded(1);
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
            match self.ensure_webview(key, window, cx) {
                WebViewAvailability::Ready(_) => {}
                WebViewAvailability::Starting => {
                    let _ = reply.try_send(Err(STARTING_MESSAGE.into()));
                    return;
                }
                WebViewAvailability::Unavailable => {
                    let error = self.webview_error.clone().unwrap_or_default();
                    let _ = reply.try_send(Err(unavailable_message(&error)));
                    return;
                }
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
                    let (probe_reply, probe_result) = smol::channel::bounded(1);
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
            match self.ensure_webview(session_id, window, cx) {
                WebViewAvailability::Ready(_) => {}
                WebViewAvailability::Starting => {
                    let _ = reply.try_send(Err(STARTING_MESSAGE.into()));
                    return;
                }
                WebViewAvailability::Unavailable => {
                    let error = self.webview_error.clone().unwrap_or_default();
                    let _ = reply.try_send(Err(unavailable_message(&error)));
                    return;
                }
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
            let Some(view) = self.webviews.get(session_id).and_then(WebViewSlot::ready) else {
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
                if self.store.read(cx).active_session_id().as_deref() != Some(session_id) {
                    let _ = reply.try_send(Err(
                        "preview is not visible; the user is viewing another conversation".into(),
                    ));
                    return;
                }
                visible_preview_key(
                    Some(key),
                    window_state.route,
                    window_state.palette_open,
                    self.store.read(cx).preview_panel_showing(),
                ) == Some(key)
            };
            if !visible {
                let _ = reply.try_send(Err(
                    "preview is not visible; open the Preview panel before taking a screenshot"
                        .into(),
                ));
                return;
            }

            let Some(view) = self.webviews.get(key).and_then(WebViewSlot::ready) else {
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
            if !self.store.read(cx).preview_browser_settings().enabled {
                return v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .px_8()
                    .text_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("browser.disabled_panel"));
            }
            let active = self.active_key(cx);

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
                    WebViewAvailability::Ready(view) => {
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
                    WebViewAvailability::Starting => v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .px_8()
                        .text_center()
                        .text_color(cx.theme().muted_foreground)
                        .child("Preview is starting…")
                        .into_any_element(),
                    WebViewAvailability::Unavailable => v_flex()
                        .flex_1()
                        .gap_2()
                        .items_center()
                        .justify_center()
                        .px_8()
                        .text_center()
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!("preview.unavailable"))
                        .child(
                            div()
                                .text_size(gpui::px(13.))
                                .child(crate::tr!("preview.unavailable_hint")),
                        )
                        .into_any_element(),
                },
                None => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("preview.no_session"))
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
                let (diff_open, right_tab) = self.store.read(cx).window_caption_state();
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
                        .tooltip(crate::tr!("preview.back"))
                        .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx))),
                )
                .child(
                    Button::new("preview-forward")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::ArrowRight)
                        .tooltip(crate::tr!("preview.forward"))
                        .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx))),
                )
                .child(
                    Button::new("preview-reload")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::Replace)
                        .tooltip(crate::tr!("preview.reload"))
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                )
                .child(div().flex_1().min_w_0().child(Input::new(&self.url_input)))
                .child(
                    Button::new("preview-ports")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::Globe)
                        .tooltip(crate::tr!("preview.scan_ports"))
                        .on_click(cx.listener(|this, _, _, cx| this.rescan_ports(cx))),
                )
                .child(
                    Button::new("preview-open-external")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::ExternalLink)
                        .tooltip(crate::tr!("preview.open_external"))
                        .on_click(cx.listener(|this, _, _, cx| this.open_in_system_browser(cx))),
                )
                .child(
                    Button::new("preview-close")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::Close)
                        .tooltip(crate::tr!("preview.close"))
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
    use crate::theme::ActiveTheme as _;
    use gpui::{Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Window};
    use gpui_base::v_flex;
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
            let _ = reply.try_send(Err(crate::tr!("preview.unsupported_linux").into_owned()));
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
                .child(crate::tr!("preview.unsupported_linux"))
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
