//! Session-keyed ownership of native preview browsers.
//!
//! This module is the lifecycle seam for native children: slots, platform
//! creation, visibility, navigation warmth, key migration, pruning, and
//! teardown all live here. Callers must preserve two ordering facts:
//!
//! - Native children outlive GPUI layout nodes. Before the Preview panel can be
//!   unmounted, call [`BrowserLifecycle::hide_except`] with the key that may
//!   remain mounted (or `None` to hide every child). Only call
//!   [`BrowserLifecycle::set_visible`] after the owning WebView element has been
//!   laid out for the current frame.
//! - lb-wry drops value-operation callbacks until a first navigation has made a
//!   view warm. [`BrowserLifecycle::evaluate_json`] enforces that cold-start
//!   delay. The wait broker uses [`BrowserLifecycle::is_warm`],
//!   [`BrowserLifecycle::mark_warm`], and
//!   [`BrowserLifecycle::evaluate_ready`] to preserve the same ordering across
//!   its repeated probes.

use std::collections::{HashMap, HashSet};
use std::rc::Weak;
use std::time::Duration;

use gpui::{AppContext as _, Context, Entity, Window};
use gpui_wry::WebView;
use preview_mcp::{PreviewReply, js};

use super::{ReplyTx, unavailable_message};

const STARTING_MESSAGE: &str = "preview is starting; retry the operation shortly";
const SMOKE_CREATION_QUEUED: &str = "preview creation is queued";
const SMOKE_CREATION_IN_FLIGHT: &str = "preview creation is in flight";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationPhase {
    Queued,
    InFlight,
}

#[derive(Clone)]
pub enum Availability {
    Starting(CreationPhase),
    Ready(Entity<WebView>),
    Unavailable,
}

impl Availability {
    /// Harness-facing classification without exposing platform slot internals.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// The exact pending status used by the cross-platform lifecycle smoke run.
    pub fn pending_message(&self) -> Option<&'static str> {
        match self {
            Self::Starting(CreationPhase::Queued) => Some(SMOKE_CREATION_QUEUED),
            Self::Starting(CreationPhase::InFlight) => Some(SMOKE_CREATION_IN_FLIGHT),
            Self::Ready(_) | Self::Unavailable => None,
        }
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

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

    fn availability(&self) -> Availability {
        match self {
            #[cfg(target_os = "windows")]
            Self::Creating { phase, .. } => Availability::Starting(*phase),
            Self::Ready(view) => Availability::Ready(view.clone()),
        }
    }
}

enum Creator {
    Available(platform::Adapter),
    Unavailable {
        /// Preserve an initialized WebContext for already-ready Windows views.
        adapter: Option<platform::Adapter>,
        error: String,
    },
}

/// The browser lifecycle is an entity so the Windows adapter can complete on
/// GPUI's foreground executor without routing back through `PreviewPanel`.
/// `owner` remains weak deliberately: a completion may install only while the
/// owning panel, the GPUI window, and its exact generation are all still live.
pub struct BrowserLifecycle {
    owner: Weak<()>,
    slots: HashMap<String, WebViewSlot>,
    warm: HashSet<String>,
    active_identity: Option<(String, String)>,
    creator: Creator,
}

pub(super) struct KeyReconciliation {
    pub(super) key: Option<String>,
    pub(super) migrated_from: Option<String>,
}

impl BrowserLifecycle {
    pub(super) fn new(owner: Weak<()>) -> Self {
        let creator = match platform::Adapter::new() {
            Ok(adapter) => Creator::Available(adapter),
            Err(error) => {
                log::warn!("preview: no webview ({error})");
                Creator::Unavailable {
                    adapter: None,
                    error,
                }
            }
        };
        Self {
            owner,
            slots: HashMap::new(),
            warm: HashSet::new(),
            active_identity: None,
            creator,
        }
    }

    fn owner_is_live(&self) -> bool {
        self.owner.upgrade().is_some()
    }

    /// Get or lazily create the browser for `key`.
    ///
    /// `initial_url` is retained by the Windows queued/in-flight adapter so the
    /// newest navigation can be replayed after asynchronous construction. The
    /// synchronous adapter still starts at `about:blank`, as before.
    pub fn ensure(
        &mut self,
        key: &str,
        initial_url: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Availability {
        if let Some(slot) = self.slots.get(key) {
            return slot.availability();
        }
        if !self.owner_is_live() || matches!(self.creator, Creator::Unavailable { .. }) {
            return Availability::Unavailable;
        }
        platform::start(self, key, initial_url, window, cx)
    }

    /// Navigate a session and mark it warm only if the native load was accepted.
    /// A Windows creation in progress retains the newest requested URL instead.
    pub fn navigate(
        &mut self,
        key: &str,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Availability {
        let availability = self.ensure(key, Some(url), window, cx);
        match &availability {
            Availability::Ready(webview) => match webview.read(cx).raw().load_url(url) {
                Ok(()) => {
                    self.warm.insert(key.to_string());
                }
                Err(error) => {
                    log::warn!("preview: failed to navigate {key}: {error}");
                }
            },
            #[cfg(target_os = "windows")]
            Availability::Starting(_) => {
                if let Some(WebViewSlot::Creating { pending_url, .. }) = self.slots.get_mut(key) {
                    *pending_url = Some(url.to_string());
                }
            }
            #[cfg(not(target_os = "windows"))]
            Availability::Starting(_) => {}
            Availability::Unavailable => {}
        }
        availability
    }

    /// Show exactly `key` and hide every other ready native child.
    ///
    /// Call this only after the selected WebView element has been laid out in
    /// the current frame. Pass `None` to hide all children.
    pub fn set_visible(&mut self, key: Option<&str>, cx: &mut Context<Self>) {
        for (candidate, slot) in &self.slots {
            let Some(view) = slot.ready() else {
                continue;
            };
            let visible = Some(candidate.as_str()) == key;
            view.update(cx, |view, _| set_webview_visible(view, visible));
        }
    }

    /// Hide children that cannot remain mounted, without showing a newly
    /// selected child before its GPUI owner has current bounds.
    pub(super) fn hide_except(&mut self, key: Option<&str>, cx: &mut Context<Self>) {
        for (candidate, slot) in &self.slots {
            if Some(candidate.as_str()) == key {
                continue;
            }
            let Some(view) = slot.ready() else {
                continue;
            };
            view.update(cx, |view, _| set_webview_visible(view, false));
        }
    }

    /// Reconcile the physical session with its stable cache key. Draft -> stored
    /// commits retain the physical id, so every cached lifecycle fact moves as
    /// one operation. The returned old key lets chrome preserve an in-progress
    /// address-bar edit while it updates only its mirror identity.
    pub(super) fn reconcile_key(&mut self, current: Option<(String, String)>) -> KeyReconciliation {
        let migration = match (self.active_identity.as_ref(), current.as_ref()) {
            (Some((old_session, old_key)), Some((session, key)))
                if old_session == session && old_key != key =>
            {
                Some((old_key.clone(), key.clone()))
            }
            _ => None,
        };
        let migrated_from = migration.map(|(old_key, key)| {
            self.migrate_key(&old_key, &key);
            old_key
        });
        self.active_identity = current.clone();
        KeyReconciliation {
            key: current.map(|(_, key)| key),
            migrated_from,
        }
    }

    fn migrate_key(&mut self, old_key: &str, key: &str) {
        if let Some(slot) = self.slots.remove(old_key) {
            if self.slots.contains_key(key) {
                drop(slot);
            } else {
                self.slots.insert(key.to_string(), slot);
            }
        }
        if self.warm.remove(old_key) {
            self.warm.insert(key.to_string());
        }
    }

    /// Tear down one ready or in-progress browser generation.
    pub fn drop_view(&mut self, key: &str) {
        self.slots.remove(key);
        self.warm.remove(key);
    }

    /// Tear down every browser whose session key is no longer live.
    pub fn prune(&mut self, live_keys: &HashSet<String>) {
        let deleted = self
            .slots
            .keys()
            .filter(|key| !live_keys.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in deleted {
            self.slots.remove(&key);
            self.warm.remove(&key);
        }
    }

    pub fn unavailable_error(&self) -> Option<&str> {
        match &self.creator {
            Creator::Available(_) => None,
            Creator::Unavailable { error, .. } => Some(error),
        }
    }

    pub(super) fn ready_view(&self, key: &str) -> Option<Entity<WebView>> {
        self.slots.get(key).and_then(WebViewSlot::ready).cloned()
    }

    pub(super) fn eval_fire(&self, key: &str, script: &str, cx: &Context<Self>) {
        if let Some(view) = self.ready_view(key) {
            let _ = view.read(cx).raw().evaluate_script(script);
        }
    }

    pub(super) fn is_warm(&self, key: &str) -> bool {
        self.warm.contains(key)
    }

    pub(super) fn mark_warm(&mut self, key: &str) {
        self.warm.insert(key.to_string());
    }

    /// Evaluate JSON against a ready, warm browser. Cold browsers wait for the
    /// initial `about:blank` navigation to flush lb-wry's callback queue first.
    pub(super) fn evaluate_json(
        &mut self,
        key: &str,
        initial_url: Option<&str>,
        script: &str,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.ensure(key, initial_url, window, cx) {
            Availability::Ready(_) => {}
            Availability::Starting(_) => {
                let _ = reply.try_send(Err(STARTING_MESSAGE.into()));
                return;
            }
            Availability::Unavailable => {
                let error = self.unavailable_error().unwrap_or_default();
                let _ = reply.try_send(Err(unavailable_message(error)));
                return;
            }
        }
        if self.is_warm(key) {
            self.evaluate_ready(key, script, reply, cx);
            return;
        }

        let key = key.to_string();
        let script = script.to_string();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(700))
                .await;
            let _ = this.update(cx, |lifecycle, cx| {
                lifecycle.mark_warm(&key);
                lifecycle.evaluate_ready(&key, &script, reply, cx);
            });
        })
        .detach();
    }

    /// Run a value operation after the caller has established warmth.
    pub(super) fn evaluate_ready(
        &self,
        key: &str,
        script: &str,
        reply: ReplyTx,
        cx: &Context<Self>,
    ) {
        let Some(view) = self.ready_view(key) else {
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

    fn record_unavailable(&mut self, error: String, cx: &mut Context<Self>) {
        log::warn!("preview: no webview ({error})");
        let previous = std::mem::replace(
            &mut self.creator,
            Creator::Unavailable {
                adapter: None,
                error: error.clone(),
            },
        );
        let adapter = match previous {
            Creator::Available(adapter) => Some(adapter),
            Creator::Unavailable { adapter, .. } => adapter,
        };
        self.creator = Creator::Unavailable { adapter, error };
        // An unavailable platform component invalidates every queued build.
        // Already-ready children remain usable until their normal teardown.
        self.slots.retain(|_, slot| slot.is_ready());
        cx.notify();
    }

    #[cfg(target_os = "windows")]
    fn has_creation(&self, creation_id: u64) -> bool {
        self.slots.values().any(|slot| {
            matches!(
                slot,
                WebViewSlot::Creating { id, .. } if *id == creation_id
            )
        })
    }

    #[cfg(target_os = "windows")]
    fn mark_creation_in_flight(&mut self, creation_id: u64) -> bool {
        for slot in self.slots.values_mut() {
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
        let key = self.slots.iter().find_map(|(key, slot)| {
            matches!(
                slot,
                WebViewSlot::Creating { id, .. } if *id == creation_id
            )
            .then(|| key.clone())
        })?;
        let WebViewSlot::Creating { pending_url, .. } = self.slots.remove(&key)? else {
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
        let warm = if let Some(url) = &pending_url {
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
        self.slots.insert(key.clone(), WebViewSlot::Ready(webview));
        if warm {
            self.warm.insert(key);
        }
        cx.notify();
    }
}

fn set_webview_visible(view: &mut WebView, visible: bool) {
    // gpui-wry keeps its own visibility bit but does not expose the native
    // result. Repeat the idempotent native operation once so teardown races are
    // observable instead of silently discarded.
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

#[cfg(not(target_os = "windows"))]
mod platform {
    use raw_window_handle::HasWindowHandle as _;

    use super::*;

    pub(super) struct Adapter;

    impl Adapter {
        pub(super) fn new() -> Result<Self, String> {
            Ok(Self)
        }
    }

    pub(super) fn start(
        lifecycle: &mut BrowserLifecycle,
        key: &str,
        _initial_url: Option<&str>,
        window: &mut Window,
        cx: &mut Context<BrowserLifecycle>,
    ) -> Availability {
        // Start on about:blank so lb-wry begins a navigation and flushes its
        // pending-scripts buffer, making later evaluation callbacks fire.
        let builder = wry::WebViewBuilder::new()
            .with_devtools(true)
            .with_url("about:blank");
        let built = window
            .window_handle()
            .map_err(|error| error.to_string())
            .and_then(|handle| {
                builder
                    .build_as_child(&handle)
                    .map_err(|error| error.to_string())
            });
        let raw = match built {
            Ok(raw) => raw,
            Err(error) => {
                lifecycle.record_unavailable(error, cx);
                return Availability::Unavailable;
            }
        };
        let webview = cx.new(|cx| {
            let mut view = WebView::new(raw, window, cx);
            set_webview_visible(&mut view, false);
            view
        });
        lifecycle
            .slots
            .insert(key.to_string(), WebViewSlot::Ready(webview.clone()));
        Availability::Ready(webview)
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::future::Future as _;
    use std::rc::Rc;

    use raw_window_handle::HasWindowHandle as _;

    use super::*;

    const SMOKE_CREATION_PAUSE: Duration = Duration::from_millis(50);

    pub(super) struct Adapter {
        web_context: Rc<smol::lock::Mutex<wry::WebContext>>,
        next_creation_id: u64,
        smoke_creation_pause: bool,
    }

    impl Adapter {
        pub(super) fn new() -> Result<Self, String> {
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
            Ok(Self {
                web_context: Rc::new(smol::lock::Mutex::new(wry::WebContext::new(Some(
                    user_data_dir,
                )))),
                next_creation_id: 0,
                // Preserve the harness's deterministic in-flight cancellation
                // window without putting smoke routing state back in the panel.
                smoke_creation_pause: std::env::args().any(|arg| arg == "--preview-smoke"),
            })
        }
    }

    pub(super) fn start(
        lifecycle: &mut BrowserLifecycle,
        key: &str,
        initial_url: Option<&str>,
        window: &mut Window,
        cx: &mut Context<BrowserLifecycle>,
    ) -> Availability {
        let (web_context, creation_id, smoke_creation) = match &mut lifecycle.creator {
            Creator::Available(adapter) => {
                adapter.next_creation_id = adapter.next_creation_id.wrapping_add(1).max(1);
                (
                    adapter.web_context.clone(),
                    adapter.next_creation_id,
                    adapter.smoke_creation_pause,
                )
            }
            Creator::Unavailable { .. } => return Availability::Unavailable,
        };
        let parent = match window.window_handle() {
            Ok(handle) => handle.as_raw(),
            Err(error) => {
                lifecycle.record_unavailable(error.to_string(), cx);
                return Availability::Unavailable;
            }
        };

        let creation_key = key.to_string();
        lifecycle.slots.insert(
            creation_key.clone(),
            WebViewSlot::Creating {
                id: creation_id,
                phase: CreationPhase::Queued,
                pending_url: initial_url.map(str::to_string),
            },
        );

        cx.spawn_in(window, async move |lifecycle, cx| {
            // WebViewBuilder borrows WebContext mutably for the lifetime of its
            // future. Serialize that borrow without blocking GPUI; a cancelled
            // queued slot is discarded before wry is ever polled.
            let mut web_context = web_context.lock().await;
            let slot_is_live = lifecycle
                .read_with(cx, |lifecycle, _| {
                    lifecycle.owner_is_live() && lifecycle.has_creation(creation_id)
                })
                .unwrap_or(false);
            if !slot_is_live {
                return;
            }
            if cx.update(|_, _| ()).is_err() {
                let _ = lifecycle.update(cx, |lifecycle, cx| {
                    lifecycle.remove_creation(creation_id);
                    cx.notify();
                });
                return;
            }

            // SAFETY: this task is confined to GPUI's UI thread and just
            // revalidated the owning GPUI window without yielding. wry reads
            // the handle synchronously on the first poll, before awaiting
            // WebView2's environment/controller callbacks. The HWND may be
            // destroyed after that await by design; the async path then
            // completes with either an error or a raw child we immediately
            // discard unless the same window/panel/generation are still live.
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
                        let _ = lifecycle.update(cx, |lifecycle, cx| {
                            if lifecycle.mark_creation_in_flight(creation_id) {
                                cx.notify();
                            }
                        });
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
                            lifecycle.update(app, |lifecycle, cx| {
                                if !lifecycle.owner_is_live() {
                                    return false;
                                }
                                let Some((key, pending_url)) =
                                    lifecycle.remove_creation(creation_id)
                                else {
                                    return false;
                                };
                                lifecycle.install_created_webview(
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
                            lifecycle.update(app, |lifecycle, cx| {
                                if !lifecycle.owner_is_live()
                                    || !lifecycle.has_creation(creation_id)
                                {
                                    return false;
                                }
                                lifecycle.record_unavailable(error.clone(), cx);
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

        Availability::Starting(CreationPhase::Queued)
    }

    fn drop_raw_webview(raw: wry::WebView, key: &str, reason: &str) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(raw))).is_err() {
            log::error!("preview: raw webview drop panicked for {key} after {reason}");
        }
    }
}
