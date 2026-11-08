//! Session-keyed ownership of native preview browsers.
//!
//! This module is the lifecycle seam for native children: slots, platform
//! creation, visibility, page events, key migration, pruning, and
//! teardown all live here. Callers must preserve two ordering facts:
//!
//! - Native children outlive GPUI layout nodes. Before a Preview panel can be
//!   unmounted, call [`BrowserLifecycle::hide_except`] with the key it may keep
//!   mounted (or `None` to let its page go). Only call
//!   [`BrowserLifecycle::show_for`] after the owning WebView element has been
//!   laid out for the current frame.
//! - A page's calls wait on its events (`events::PageEvents`), not on time:
//!   lb-wry drops evaluation callbacks before a first navigation commits, and
//!   a page agent exists only once a document has started.

use std::collections::HashMap;
use std::rc::Weak;
use std::time::{Duration, Instant};

#[cfg(target_os = "android")]
use super::android::WebView;
#[cfg(target_os = "windows")]
use super::webview2::WebView;
use gpui::{App, AppContext as _, Context, Entity, EntityId, WeakEntity, Window};
#[cfg(not(any(target_os = "android", target_os = "windows")))]
use gpui_webview::WebView;
use serde_json::Value;
use tcode_protocol::PreviewResponse;

use super::capture;
use super::events::{Answer, Event, LoadError, Outcome, PageEvents, WaitFor};
use super::page_agent;
use super::{ReplyTx, unavailable_message};
use crate::store::WorkspaceStore;

/// How long a call into a page agent may go unanswered: a page whose script
/// is stuck, or blocked by a dialog, never answers.
const CALL_BOUND: Duration = Duration::from_secs(15);

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
    #[cfg(test)]
    Stub,
    #[cfg(any(target_os = "windows", target_os = "android"))]
    Creating {
        id: u64,
        phase: CreationPhase,
        pending_url: Option<String>,
    },
    Ready {
        view: Entity<WebView>,
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        remote: Option<super::remote::RemoteBrowser>,
        page: Box<Page>,
    },
}

/// One page's identity for its event stream, which outlives key
/// migrations, and what it reported.
struct Page {
    id: u64,
    events: PageEvents,
}

impl WebViewSlot {
    fn ready(&self) -> Option<&Entity<WebView>> {
        match self {
            #[cfg(any(target_os = "windows", target_os = "android"))]
            Self::Creating { .. } => None,
            #[cfg(test)]
            Self::Stub => None,
            Self::Ready { view, .. } => Some(view),
        }
    }

    fn is_ready(&self) -> bool {
        self.ready().is_some()
    }

    fn page(&self) -> Option<&Page> {
        match self {
            #[cfg(any(target_os = "windows", target_os = "android"))]
            Self::Creating { .. } => None,
            #[cfg(test)]
            Self::Stub => None,
            Self::Ready { page, .. } => Some(page),
        }
    }

    fn page_mut(&mut self) -> Option<&mut Page> {
        match self {
            #[cfg(any(target_os = "windows", target_os = "android"))]
            Self::Creating { .. } => None,
            #[cfg(test)]
            Self::Stub => None,
            Self::Ready { page, .. } => Some(page),
        }
    }

    fn availability(&self) -> Availability {
        match self {
            #[cfg(any(target_os = "windows", target_os = "android"))]
            Self::Creating { phase, .. } => Availability::Starting(*phase),
            #[cfg(test)]
            Self::Stub => Availability::Unavailable,
            Self::Ready { view, .. } => Availability::Ready(view.clone()),
        }
    }
}

enum Creator {
    Available(platform::Adapter),
    Unavailable {
        /// Preserve an initialized WebView2 environment for already-ready Windows views.
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
    /// Where the user's own input in a page is reported.
    store: WeakEntity<WorkspaceStore>,
    slots: HashMap<String, WebViewSlot>,
    next_page: u64,
    /// The session and key each panel addresses.
    active_identity: HashMap<EntityId, (String, String)>,
    /// The key each panel last showed; one panel per pane can show a page.
    /// Any other page still on screen is leaving: it fades out
    /// (`PreviewPanel::native_fade`) and hides when that lands.
    shown: HashMap<EntityId, String>,
    /// The size the Preview panel last showed a page at: what a conversation
    /// nobody is looking at would get on screen.
    #[cfg(not(target_os = "android"))]
    panel_size: Option<gpui::Size<gpui::Pixels>>,
    creator: Creator,
    /// Windows gives the proxy to its WebView2 environment, in the adapter.
    #[cfg(not(target_os = "windows"))]
    proxy: Option<tcode_traverse::preview::ProxyEntry>,
    endpoint: Option<tcode_traverse::preview::PreviewEndpoint>,
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "android"))]
    _native_proxy: Option<tcode_traverse::preview::NativeProxy>,
}

pub(super) struct KeyReconciliation {
    pub(super) key: Option<String>,
    pub(super) migrated_from: Option<String>,
}

impl BrowserLifecycle {
    pub(super) fn new(
        owner: Weak<()>,
        store: WeakEntity<WorkspaceStore>,
        proxy: Result<Option<crate::store::PreviewTarget>, String>,
    ) -> Self {
        let endpoint = proxy.as_ref().ok().and_then(|target| {
            target.as_ref().map(|(host, tunnels)| {
                tcode_traverse::preview::PreviewEndpoint::new(host, tunnels.clone())
            })
        });
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "android"))]
        let mut native_proxy = None;
        // The browser engine is pointed at a loopback bridge; macOS and iOS
        // map URLs per route instead and need no entry.
        let proxy: Result<Option<tcode_traverse::preview::ProxyEntry>, String> =
            proxy.and_then(|target| {
                target
                    .map(|_target| {
                        #[cfg(any(
                            target_os = "windows",
                            target_os = "linux",
                            target_os = "android"
                        ))]
                        {
                            let bridge = tcode_traverse::preview::NativeProxy::new(
                                endpoint.clone().unwrap(),
                            )?;
                            let entry = bridge.entry();
                            native_proxy = Some(bridge);
                            Ok(entry)
                        }
                        #[cfg(any(target_os = "macos", target_os = "ios"))]
                        Ok(tcode_traverse::preview::ProxyEntry {
                            origin: String::new(),
                        })
                    })
                    .transpose()
            });
        let creator = match proxy
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|proxy| platform::Adapter::new(proxy.as_ref()))
        {
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
            #[cfg(not(target_os = "windows"))]
            proxy: proxy.ok().flatten(),
            endpoint,
            #[cfg(any(target_os = "windows", target_os = "linux", target_os = "android"))]
            _native_proxy: native_proxy,
            store,
            slots: HashMap::new(),
            next_page: 0,
            active_identity: HashMap::new(),
            shown: HashMap::new(),
            #[cfg(not(target_os = "android"))]
            panel_size: None,
            creator,
        }
    }

    pub(super) fn refresh_endpoint(&self, host: &tcode_client::pairing::PairedHost) {
        if let Some(endpoint) = &self.endpoint
            && let Err(error) = endpoint.update(host)
        {
            log::warn!("preview: could not refresh paired machine address: {error}");
        }
    }

    pub(super) fn active_identity(&self, owner: EntityId) -> Option<&(String, String)> {
        self.active_identity.get(&owner)
    }

    /// Whether a panel has `key`'s page on screen.
    pub(super) fn is_shown(&self, key: &str) -> bool {
        self.shown.values().any(|shown| shown == key)
    }

    /// The panel `owner` is gone: it shows and addresses nothing any more.
    pub(super) fn forget_owner(&mut self, owner: EntityId, cx: &mut Context<Self>) {
        self.active_identity.remove(&owner);
        self.hide_except(owner, None, cx);
    }

    #[cfg(test)]
    pub(crate) fn seed_stub(&mut self, key: &str) {
        self.slots.insert(key.to_owned(), WebViewSlot::Stub);
    }

    #[cfg(test)]
    pub(crate) fn has_view(&self, key: &str) -> bool {
        self.slots.contains_key(key)
    }

    fn owner_is_live(&self) -> bool {
        self.owner.upgrade().is_some()
    }

    /// Get or lazily create the browser for `key`.
    ///
    /// Windows and Android retain the newest URL while asynchronous creation runs.
    /// The synchronous adapter starts with no document.
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

    /// Navigate a session; calls wait for the document it loads.
    /// An asynchronous creation in progress retains the newest requested URL instead.
    pub fn navigate(
        &mut self,
        key: &str,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Availability {
        let availability = self.ensure(key, Some(url), window, cx);
        match &availability {
            Availability::Ready(webview) => {
                let webview = webview.clone();
                self.load(key, &webview, url, cx);
            }
            #[cfg(any(target_os = "windows", target_os = "android"))]
            Availability::Starting(_) => {
                if let Some(WebViewSlot::Creating { pending_url, .. }) = self.slots.get_mut(key) {
                    *pending_url = Some(url.to_string());
                }
            }
            #[cfg(not(any(target_os = "windows", target_os = "android")))]
            Availability::Starting(_) => {}
            Availability::Unavailable => {}
        }
        availability
    }

    /// Load `url` into `key`'s page: calls wait for the document it loads.
    fn load(&mut self, key: &str, webview: &Entity<WebView>, url: &str, cx: &mut Context<Self>) {
        // Calls wait for the next document now rather than from
        // WebKit's asynchronous didStart, so a wait_for issued right
        // after this navigation cannot read the old page or fail on
        // its record.
        if let Some(page) = self.slots.get_mut(key).and_then(WebViewSlot::page_mut) {
            let outcome = page.events.navigate();
            self.apply(key, outcome, cx);
        }
        let raw = webview.read(cx).raw();
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let mapped = match self
            .remote_browser(key)
            .map(|remote| remote.navigate(url))
            .transpose()
        {
            Ok(mapped) => mapped,
            Err(_) => {
                cx.notify();
                return;
            }
        };
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let url = mapped.as_deref().unwrap_or(url);
        // lb-wry unwraps `NSURL URLWithString:`, which is nil for a
        // string WebKit cannot load: report it as the page would.
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if objc2_foundation::NSURL::URLWithString(&objc2_foundation::NSString::from_str(url))
            .is_none()
        {
            if let Some(page) = self.slots.get_mut(key).and_then(WebViewSlot::page_mut) {
                let outcome = page.events.handle(Event::Failed(LoadError {
                    url: url.to_owned(),
                    code: "InvalidURL".into(),
                    message: "this is not a URL the browser can load".into(),
                }));
                self.apply(key, outcome, cx);
            }
            return;
        }
        if let Err(error) = raw.load_url(url) {
            log::warn!("preview: failed to navigate {key}: {error}");
        }
    }

    /// Show exactly `key` and let every other ready native child go.
    ///
    /// Call this only after the selected WebView element has been laid out in
    /// the current frame. Pass `None` to let every child go.
    ///
    /// On macOS and iOS a page fades in and out (`PreviewPanel::native_fade`).
    /// Gap: on Windows, Linux and Android it shows and hides at once.
    /// gpui-fast-windows' native surface has no opacity (its
    /// `DCompositionCreateDevice` visual is not an `IDCompositionVisual3`),
    /// gpui-webview has none, and an X11 child window or an Android view is
    /// not faded here; a snapshot drawn by GPUI in the page's place is the
    /// open alternative.
    pub fn set_visible(&mut self, key: Option<&str>, cx: &mut Context<Self>) {
        self.shown.clear();
        self.show_for(cx.entity_id(), key, cx);
    }

    /// Show `key`'s page for the panel `owner`, whose element has laid it out
    /// for this frame, letting go of what that panel showed. Pages other
    /// panels show are theirs to place.
    pub(super) fn show_for(&mut self, owner: EntityId, key: Option<&str>, cx: &mut Context<Self>) {
        #[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(unused_mut))]
        let mut fading = self.set_wanted(owner, key);
        for (candidate, slot) in &self.slots {
            let Some(view) = slot.ready() else {
                continue;
            };
            let visible = Some(candidate.as_str()) == key;
            if visible {
                view.update(cx, |view, _| {
                    #[cfg(any(target_os = "macos", target_os = "ios"))]
                    if !view.visible() {
                        fade::set_alpha(view.raw(), 0.);
                        fading = true;
                    }
                    set_webview_visible(view, true);
                });
            } else if !self.shown.values().any(|shown| shown == candidate) {
                leave(view, cx);
            }
            #[cfg(not(target_os = "android"))]
            if visible {
                let size = view.read(cx).bounds().size;
                if size.width > gpui::px(0.) && size.height > gpui::px(0.) {
                    self.panel_size = Some(size);
                }
            }
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if fading {
            cx.notify();
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        let _ = fading;
    }

    /// Record the page `owner` wants on screen; whether that changed, so a
    /// fade starts.
    fn set_wanted(&mut self, owner: EntityId, key: Option<&str>) -> bool {
        if self.shown.get(&owner).map(String::as_str) == key {
            return false;
        }
        match key {
            Some(key) => self.shown.insert(owner, key.to_owned()),
            None => self.shown.remove(&owner),
        };
        true
    }

    /// Give a hidden browser the viewport it would have on screen: `canvas`
    /// when the conversation fixed one, else the panel's, else `fallback`.
    /// A browser that was never shown is otherwise laid out at 0×0, which
    /// leaves an agent driving a background conversation with an empty page.
    #[cfg(not(target_os = "android"))]
    pub fn stage_hidden(
        &self,
        key: &str,
        canvas: Option<gpui::Size<gpui::Pixels>>,
        fallback: gpui::Size<gpui::Pixels>,
        cx: &App,
    ) {
        let Some(view) = self.ready_view(key) else {
            return;
        };
        let view = view.read(cx);
        if view.visible() {
            return;
        }
        let own = Some(view.bounds().size)
            .filter(|size| size.width > gpui::px(0.) && size.height > gpui::px(0.));
        let size = canvas.or(self.panel_size).or(own).unwrap_or(fallback);
        #[cfg(target_os = "windows")]
        view.stage(size);
        #[cfg(not(target_os = "windows"))]
        let rect = wry::Rect {
            position: wry::dpi::LogicalPosition::new(-20000., -20000.).into(),
            size: wry::dpi::LogicalSize::new(f64::from(size.width), f64::from(size.height)).into(),
        };
        #[cfg(not(target_os = "windows"))]
        if let Err(error) = view.set_bounds(rect) {
            log::debug!("preview: failed to size hidden webview {key}: {error}");
        }
        #[cfg(target_os = "ios")]
        super::ios::set_frame(
            view.raw(),
            gpui::Bounds::new(gpui::point(gpui::px(-20000.), gpui::px(-20000.)), size),
        );
    }

    /// Hide children `owner` cannot keep mounted, without showing a newly
    /// selected child before its GPUI owner has current bounds. Pages other
    /// panels show stay.
    pub(super) fn hide_except(
        &mut self,
        owner: EntityId,
        key: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if self.shown.get(&owner).map(String::as_str) != key && self.set_wanted(owner, None) {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            cx.notify();
        }
        for (candidate, slot) in &self.slots {
            if Some(candidate.as_str()) == key || self.is_shown(candidate) {
                continue;
            }
            let Some(view) = slot.ready() else {
                continue;
            };
            leave(view, cx);
        }
    }

    /// Hide `key`'s page, or every page, now, with no fade, for a pointer
    /// that must not reach one before the next frame.
    pub fn hide_now(&mut self, key: Option<&str>, cx: &mut Context<Self>) {
        self.shown
            .retain(|_, shown| key.is_some_and(|key| key != shown));
        for (_, slot) in self
            .slots
            .iter()
            .filter(|(candidate, _)| key.is_none_or(|key| key == candidate.as_str()))
        {
            if let Some(view) = slot.ready() {
                view.update(cx, |view, _| set_webview_visible(view, false));
            }
        }
    }

    /// Whether any page is held, made or on its way.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(super) fn has_pages(&self) -> bool {
        !self.slots.is_empty()
    }

    /// Every ready page and whether the panel wants it on screen.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(super) fn native_pages(
        &self,
    ) -> Vec<crate::motion::native_fade::NativeEntry<Entity<WebView>>> {
        self.slots
            .iter()
            .filter_map(|(key, slot)| {
                let view = slot.ready()?;
                // Keyed by the browser, which keeps its fade when a draft's
                // key migrates to the stored thread's.
                Some((
                    view.entity_id().to_string().into(),
                    view.clone(),
                    self.is_shown(key),
                ))
            })
            .collect()
    }

    /// Reconcile the physical session with its stable cache key. Draft -> stored
    /// commits retain the physical id, so every cached lifecycle fact moves as
    /// one operation. The returned old key lets chrome preserve an in-progress
    /// address-bar edit while it updates only its mirror identity.
    pub(super) fn reconcile_key(
        &mut self,
        owner: EntityId,
        current: Option<(String, String)>,
    ) -> KeyReconciliation {
        let migration = match (self.active_identity.get(&owner), current.as_ref()) {
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
        match &current {
            Some(current) => self.active_identity.insert(owner, current.clone()),
            None => self.active_identity.remove(&owner),
        };
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
    }

    /// Tear down one ready or in-progress browser generation.
    pub fn drop_view(&mut self, key: &str, cx: &App) {
        self.close_page(key, cx);
        self.slots.remove(key);
    }

    /// Tear down every browser whose session key is no longer live.
    pub fn prune(&mut self, live: impl Fn(&str, &App) -> bool, cx: &App) {
        let deleted = self
            .slots
            .keys()
            .filter(|key| !live(key, cx))
            .cloned()
            .collect::<Vec<_>>();
        for key in deleted {
            self.close_page(&key, cx);
            self.slots.remove(&key);
        }
    }

    /// The last navigation failure the platform reported for this browser, if
    /// the current page is still the one it left behind.
    pub(super) fn load_error(&self, key: &str, cx: &App) -> Option<LoadError> {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Some(remote) = self.remote_browser(key) {
            let routes = remote.routes.borrow();
            if let Some(message) = routes.error() {
                return Some(LoadError {
                    url: routes.current_url().unwrap_or_default().into(),
                    code: "RemotePreview".into(),
                    message,
                });
            }
        }
        let _ = cx;
        let mut error = self.slots.get(key)?.page()?.events.load_error()?.clone();
        error.url = self.logical_url(key, &error.url);
        Some(error)
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn remote_browser(&self, key: &str) -> Option<&super::remote::RemoteBrowser> {
        match self.slots.get(key)? {
            WebViewSlot::Ready { remote, .. } => remote.as_ref(),
            #[cfg(test)]
            WebViewSlot::Stub => None,
        }
    }

    pub(super) fn logical_url(&self, _key: &str, actual: &str) -> String {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Some(remote) = self.remote_browser(_key) {
            return remote.routes.borrow().logical_url(actual);
        }
        actual.into()
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(super) fn remote_url(&self, key: &str) -> Option<String> {
        self.remote_browser(key)?
            .routes
            .borrow()
            .current_url()
            .map(str::to_owned)
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(super) fn external_url(&self, key: &str, logical: &str) -> Option<String> {
        match self.remote_browser(key) {
            Some(remote) => remote.routes.borrow().external_url(logical),
            None if self.proxy.is_some() => None,
            None => Some(logical.into()),
        }
    }

    /// The page is going away: its callers hear so, and its engine stops
    /// reporting.
    fn close_page(&mut self, key: &str, cx: &App) {
        if let Some(view) = self.ready_view(key) {
            page_agent::forget(view.read(cx).raw());
        }
        if let Some(page) = self.slots.get_mut(key).and_then(WebViewSlot::page_mut) {
            page.events.close();
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

    /// A new page's event stream: what its engine and agent report, and the
    /// nonce the agent signs with.
    fn open_page(
        &mut self,
        nonce: String,
        loading: bool,
    ) -> (
        Box<Page>,
        page_agent::Events,
        async_channel::Receiver<Event>,
    ) {
        self.next_page += 1;
        let (events, receiver) = async_channel::unbounded();
        (
            Box::new(Page {
                id: self.next_page,
                events: PageEvents::new(nonce, loading),
            }),
            events,
            receiver,
        )
    }

    /// Feed a page's events to it for as long as its engine reports any.
    fn watch_page(&self, id: u64, events: async_channel::Receiver<Event>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |lifecycle, cx| lifecycle.page_event(id, event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn page_event(&mut self, id: u64, event: Event, cx: &mut Context<Self>) {
        log::trace!("preview: page {id} {event:.300?}");
        let Some(key) = self
            .slots
            .iter()
            .find_map(|(key, slot)| (slot.page()?.id == id).then(|| key.clone()))
        else {
            return;
        };
        if let Event::NewWindow(url) = &event {
            if let Some(view) = self.ready_view(&key) {
                self.load(&key, &view, url, cx);
            }
            return;
        }
        let Some(page) = self.slots.get_mut(&key).and_then(WebViewSlot::page_mut) else {
            return;
        };
        let outcome = page.events.handle(event);
        self.apply(&key, outcome, cx);
    }

    /// Carry out what a page's state change asks for.
    fn apply(&mut self, key: &str, outcome: Outcome, cx: &mut Context<Self>) {
        if let Some(view) = self.ready_view(key) {
            for script in &outcome.scripts {
                page_agent::run(view.read(cx).raw(), script);
            }
            for (script, reply) in outcome.evaluations {
                evaluate_in_page(&view, &script, reply, cx);
            }
        }
        if outcome.user_acted {
            log::debug!("preview: the user acted on {key}'s page");
            let _ = self
                .store
                .update(cx, |store, _| store.report_page_input(key));
        }
        if outcome.changed {
            cx.notify();
        }
    }

    /// Bound a call so a page that never answers does not hold its caller.
    fn bound(&self, key: &str, call: u64, after: Duration, cx: &mut Context<Self>) {
        let Some(page) = self
            .slots
            .get(key)
            .and_then(WebViewSlot::page)
            .map(|page| page.id)
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(after).await;
            let _ = this.update(cx, |lifecycle, _| {
                if let Some(page) = lifecycle
                    .slots
                    .values_mut()
                    .filter_map(WebViewSlot::page_mut)
                    .find(|candidate| candidate.id == page)
                {
                    page.events.expire(call);
                }
            });
        })
        .detach();
    }

    /// Ask `key`'s page agent to run `method`.
    pub fn call(
        &mut self,
        key: &str,
        method: &'static str,
        args: Value,
        cx: &mut Context<Self>,
    ) -> async_channel::Receiver<Result<Value, String>> {
        let (answer, answered): (Answer, _) = async_channel::bounded(1);
        let Some(page) = self.slots.get_mut(key).and_then(WebViewSlot::page_mut) else {
            let _ = answer.try_send(Err("preview browser is not open".into()));
            return answered;
        };
        let (id, outcome) = page.events.call(method, args, answer);
        self.apply(key, outcome, cx);
        self.bound(key, id, CALL_BOUND, cx);
        answered
    }

    /// Wait until `key`'s page meets every condition, or `timeout` passes.
    pub fn wait(
        &mut self,
        key: &str,
        selector: Option<String>,
        text: Option<String>,
        url_includes: Option<String>,
        timeout: Duration,
        cx: &mut Context<Self>,
    ) -> async_channel::Receiver<Result<Value, String>> {
        let (answer, answered): (Answer, _) = async_channel::bounded(1);
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let routes = self.remote_browser(key).map(|remote| remote.routes.clone());
        let url = url_includes.map(|needle| -> super::events::UrlMatch {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            if let Some(routes) = routes.clone() {
                return Box::new(move |actual| {
                    routes.borrow().logical_url(actual).contains(&needle)
                });
            }
            Box::new(move |actual| actual.contains(&needle))
        });
        let Some(page) = self.slots.get_mut(key).and_then(WebViewSlot::page_mut) else {
            let _ = answer.try_send(Err("preview browser is not open".into()));
            return answered;
        };
        let (id, outcome) = page.events.wait(
            WaitFor {
                selector,
                text,
                url,
                deadline: Instant::now() + timeout,
            },
            answer,
        );
        self.apply(key, outcome, cx);
        self.bound(key, id, timeout, cx);
        answered
    }

    /// Tcode loads a document into `view` itself: calls wait for it.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(super) fn loading(&mut self, view: &Entity<WebView>, cx: &mut Context<Self>) {
        let Some((key, page)) = self.slots.iter_mut().find_map(|(key, slot)| match slot {
            WebViewSlot::Ready {
                view: candidate,
                page,
                ..
            } if candidate == view => Some((key.clone(), page)),
            _ => None,
        }) else {
            return;
        };
        let outcome = page.events.navigate();
        self.apply(&key, outcome, cx);
    }

    /// Whether `key`'s page agent can answer now.
    pub(super) fn page_settled(&self, key: &str) -> bool {
        self.slots
            .get(key)
            .and_then(WebViewSlot::page)
            .is_some_and(|page| page.events.settled())
    }

    /// A picture of `key`'s page: what is on screen, the element
    /// `reference` names, or the whole document.
    pub fn screenshot(
        &mut self,
        key: &str,
        reference: Option<String>,
        full_page: bool,
        cx: &mut Context<Self>,
    ) -> async_channel::Receiver<Result<PreviewResponse, String>> {
        let (reply, pictured) = async_channel::bounded(1);
        // Android draws a WebView only while it is on screen.
        #[cfg(target_os = "android")]
        if !self.is_shown(key) {
            let _ = reply.try_send(Err(
                "preview is not visible; open the Preview panel before taking a screenshot".into(),
            ));
            return pictured;
        }
        let Some(view) = self.ready_view(key) else {
            let _ = reply.try_send(Err("preview browser is not open".into()));
            return pictured;
        };
        let Some(reference) = reference else {
            let region = if full_page {
                capture::Region::FullPage
            } else {
                capture::Region::Viewport
            };
            capture::capture(&view, region, reply, cx);
            return pictured;
        };
        let measured = self.call(key, "rect", serde_json::json!({ "ref": reference }), cx);
        cx.spawn(async move |_, cx| {
            let element = match measured.recv().await {
                Ok(Ok(value)) => capture::ElementBox::from_agent(&value)
                    .ok_or_else(|| "the page agent measured no box".to_string()),
                Ok(Err(error)) => Err(error),
                Err(_) => Err("preview browser was closed".into()),
            };
            match element {
                Ok(element) => {
                    cx.update(|cx| {
                        capture::capture(&view, capture::Region::Element(element), reply, cx)
                    });
                }
                Err(error) => {
                    let _ = reply.send(Err(error)).await;
                }
            }
        })
        .detach();
        pictured
    }

    /// Whether a navigation is replacing `key`'s document.
    pub(super) fn page_loading(&self, key: &str) -> bool {
        self.slots
            .get(key)
            .and_then(WebViewSlot::page)
            .is_some_and(|page| page.events.navigating())
    }

    /// The console since `key`'s last navigation.
    pub fn page_summary(&self, key: &str) -> Option<Value> {
        Some(self.slots.get(key)?.page()?.events.summary())
    }

    pub(super) fn reload(&self, key: &str, cx: &App) {
        if let Some(view) = self.ready_view(key)
            && let Err(error) = view.read(cx).raw().reload()
        {
            log::debug!("preview: failed to reload: {error}");
        }
    }

    /// Evaluate `script` in `key`'s page, in the page's own world, once no
    /// navigation is replacing the document.
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
        let Some(page) = self.slots.get_mut(key).and_then(WebViewSlot::page_mut) else {
            let _ = reply.try_send(Err("preview browser is not open".into()));
            return;
        };
        let outcome = page.events.evaluate(script.to_owned(), reply);
        self.apply(key, outcome, cx);
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

    #[cfg(any(target_os = "windows", target_os = "android"))]
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

    #[cfg(any(target_os = "windows", target_os = "android"))]
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
        controller: webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2CompositionController,
        nonce: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prepared = match WebView::prepare(controller.clone(), window) {
            Ok(prepared) => prepared,
            Err(error) => {
                super::webview2::discard(&controller);
                self.record_unavailable(error, cx);
                return;
            }
        };
        let raw = prepared.raw();
        let (page, events, receiver) = self.open_page(nonce.clone(), pending_url.is_some());
        page_agent::install(raw, events, &nonce);
        if let Some(url) = &pending_url
            && let Err(error) = raw.load_url(url)
        {
            log::warn!("preview: failed to replay URL for {key}: {error}");
        }
        let id = page.id;
        let webview = cx.new(|cx| WebView::new(prepared, window, cx));
        self.slots.insert(
            key,
            WebViewSlot::Ready {
                view: webview,
                page,
            },
        );
        self.watch_page(id, receiver, cx);
        cx.notify();
    }
}

/// Evaluate `script` in the page's own world; `reply` gets its JSON value.
fn evaluate_in_page(
    view: &Entity<WebView>,
    script: &str,
    reply: ReplyTx,
    cx: &Context<BrowserLifecycle>,
) {
    #[cfg(target_os = "android")]
    view.read(cx).raw().evaluate_json(script, reply, cx);
    #[cfg(not(target_os = "android"))]
    {
        let result = view.read(cx).raw().evaluate_script_with_callback(script, {
            let reply = reply.clone();
            move |raw: String| {
                let value = preview_mcp::page::parse_result(&raw);
                let _ = reply.try_send(Ok(PreviewResponse::Json(value)));
            }
        });
        if result.is_err() {
            let _ = reply.try_send(Err("failed to evaluate script in preview".into()));
        }
    }
}

/// A page the panel no longer wants on screen. On macOS and iOS it stays
/// until `PreviewPanel::native_fade` has faded it out.
fn leave(view: &Entity<WebView>, cx: &mut App) {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let _ = (view, cx);
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    view.update(cx, |view, _| {
        if view.visible() {
            set_webview_visible(view, false);
        }
    });
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod fade {
    use gpui::{App, Bounds, Entity, Pixels};
    use gpui_webview::WebView;

    use crate::motion::native_fade::NativeView;

    pub(super) fn set_alpha(raw: &wry::WebView, alpha: f32) {
        #[cfg(target_os = "macos")]
        {
            use wry::WebViewExtMacOS as _;
            raw.webview().setAlphaValue(f64::from(alpha));
        }
        #[cfg(target_os = "ios")]
        {
            use wry::WebViewExtIOS as _;
            raw.webview().setAlpha(f64::from(alpha));
        }
    }

    impl NativeView for Entity<WebView> {
        fn shown(&self, cx: &App) -> bool {
            self.read(cx).visible()
        }

        fn hide(&self, cx: &mut App) {
            self.update(cx, |view, _| super::set_webview_visible(view, false));
        }

        fn set_alpha(&self, alpha: f32, cx: &App) {
            set_alpha(self.read(cx).raw(), alpha);
        }

        /// UIKit delivers a touch to the page before GPUI sees it, so no GPUI
        /// hitbox can cover a leaving page there.
        #[cfg(target_os = "ios")]
        fn set_inert(&self, inert: bool, cx: &App) {
            use wry::WebViewExtIOS as _;
            self.read(cx)
                .raw()
                .webview()
                .setUserInteractionEnabled(!inert);
        }

        fn bounds(&self, cx: &App) -> Bounds<Pixels> {
            self.read(cx).bounds()
        }
    }
}

#[cfg(not(target_os = "android"))]
fn set_webview_visible(view: &mut WebView, visible: bool) {
    if let Err(error) = view.set_visible(visible) {
        log::debug!("preview: failed to set webview visibility to {visible}: {error}");
    }
}

#[cfg(target_os = "android")]
fn set_webview_visible(view: &mut WebView, visible: bool) {
    if visible {
        view.show();
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

#[cfg(not(any(target_os = "windows", target_os = "android")))]
mod platform {
    use super::*;

    pub(super) struct Adapter;

    impl Adapter {
        pub(super) fn new(
            _proxy: Option<&tcode_traverse::preview::ProxyEntry>,
        ) -> Result<Self, String> {
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
        // No first document: lb-wry holds evaluations back until a
        // navigation commits, and the page's evaluations wait for its first
        // document (`PageEvents`).
        let nonce = page_agent::nonce();
        let (page, events, receiver) = lifecycle.open_page(nonce.clone(), false);
        let builder = page_agent::observe(wry::WebViewBuilder::new().with_devtools(true), &events);
        let builder = match super::super::proxy::builder(builder, lifecycle.proxy.as_ref()) {
            Ok(builder) => builder,
            Err(error) => {
                lifecycle.record_unavailable(error, cx);
                return Availability::Unavailable;
            }
        };
        #[cfg(target_os = "linux")]
        let wayland = super::super::wayland::active(window);
        #[cfg(target_os = "linux")]
        let built = if wayland {
            super::super::wayland::build(builder, window, cx)
        } else {
            WebView::build(builder, window, cx)
        };
        #[cfg(not(target_os = "linux"))]
        let built = WebView::build(builder, window, cx);
        let webview = match built {
            Ok(webview) => cx.new(|_| webview),
            Err(error) => {
                lifecycle.record_unavailable(error.to_string(), cx);
                return Availability::Unavailable;
            }
        };
        page_agent::install(webview.read(cx).raw(), events, &nonce);
        lifecycle.watch_page(page.id, receiver, cx);
        #[cfg(target_os = "linux")]
        if !wayland {
            trap_gdk_x_errors();
        }
        #[cfg(target_os = "ios")]
        super::super::ios::adopt(webview.read(cx).raw());
        webview.update(cx, |view, _| set_webview_visible(view, false));
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let remote = lifecycle
            .endpoint
            .clone()
            .map(|endpoint| super::super::remote::RemoteBrowser::install(&webview, endpoint, cx));
        lifecycle.slots.insert(
            key.to_string(),
            WebViewSlot::Ready {
                view: webview.clone(),
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                remote,
                page,
            },
        );
        Availability::Ready(webview)
    }
}

/// gpui-webview releases a webview by destroying its X11 composition
/// surface through GPUI's xcb connection while GDK's Xlib connection still
/// holds unsent requests for the WebKit windows inside it. Those requests then
/// fail with `BadWindow`, which GDK's default handler turns into `exit(1)` —
/// on closing a page, deleting its thread, or quitting. GDK's connection
/// carries only the embedded WebKit windows, so its X errors are trapped for
/// the rest of the process once GTK is up.
#[cfg(target_os = "linux")]
fn trap_gdk_x_errors() {
    static TRAPPED: std::sync::Once = std::sync::Once::new();
    TRAPPED.call_once(gdk::error_trap_push);
}

#[cfg(target_os = "windows")]
mod platform {
    use std::future::Future as _;
    use std::rc::Rc;

    use super::super::webview2;
    use super::*;

    const SMOKE_CREATION_PAUSE: Duration = Duration::from_millis(50);

    pub(super) struct Adapter {
        environment: Rc<webview2::Environment>,
        /// Whether pages are in-private: those of an attached browser.
        private: bool,
        next_creation_id: u64,
        smoke_creation_pause: bool,
    }

    impl Adapter {
        pub(super) fn new(
            proxy: Option<&tcode_traverse::preview::ProxyEntry>,
        ) -> Result<Self, String> {
            Ok(Self {
                environment: Rc::new(webview2::Environment::new(proxy)?),
                private: proxy.is_some(),
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
        let (environment, private, creation_id, smoke_creation) = match &mut lifecycle.creator {
            Creator::Available(adapter) => {
                adapter.next_creation_id = adapter.next_creation_id.wrapping_add(1).max(1);
                (
                    adapter.environment.clone(),
                    adapter.private,
                    adapter.next_creation_id,
                    adapter.smoke_creation_pause,
                )
            }
            Creator::Unavailable { .. } => return Availability::Unavailable,
        };
        let parent = match webview2::parent_window(window) {
            Ok(parent) => parent,
            Err(error) => {
                lifecycle.record_unavailable(error, cx);
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
        let nonce = page_agent::nonce();

        cx.spawn_in(window, async move |lifecycle, cx| {
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

            // The HWND may be destroyed while WebView2 builds the controller
            // by design; creation then fails, or completes with a controller
            // discarded unless the same window/panel/generation are still live.
            let mut build = Box::pin(webview2::create(environment, parent, private));
            let first_poll = std::future::poll_fn(|task_cx| {
                std::task::Poll::Ready(match build.as_mut().poll(task_cx) {
                    std::task::Poll::Ready(result) => Some(result),
                    std::task::Poll::Pending => None,
                })
            })
            .await;
            let built = match first_poll {
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
            };

            if let Ok(controller) = &built
                && let Ok(webview) = webview2::core_webview(controller)
                && let Err(error) = page_agent::prepare(webview, nonce.clone()).await
            {
                log::warn!("preview: the page agent could not be installed: {error}");
            }

            match built {
                Ok(controller) => {
                    let mut controller = Some(controller);
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
                                    controller.take().expect("controller already consumed"),
                                    nonce.clone(),
                                    window,
                                    cx,
                                );
                                true
                            })
                        }),
                        Ok(Ok(true))
                    );
                    if let Some(controller) = controller {
                        webview2::discard(&controller);
                    }
                    if !installed {
                        log::debug!(
                            "preview: discarded stale creation {creation_id} for {creation_key}"
                        );
                    }
                }
                Err(error) => {
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
}

#[cfg(target_os = "android")]
mod platform {
    use super::*;
    use futures::StreamExt as _;
    pub(super) struct Adapter {
        next_creation_id: u64,
    }
    impl Adapter {
        pub(super) fn new(
            _proxy: Option<&tcode_traverse::preview::ProxyEntry>,
        ) -> Result<Self, String> {
            Ok(Self {
                next_creation_id: 0,
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
        let initial_url = initial_url.unwrap_or("about:blank").to_owned();
        let nonce = page_agent::nonce();
        let agent = page_agent::scripts(&nonce);
        let (page, page_events, receiver) = lifecycle.open_page(nonce, true);
        let (raw, mut events) = match super::super::android::RawWebView::new(
            &initial_url,
            &agent,
            lifecycle.proxy.as_ref(),
        ) {
            Ok(created) => created,
            Err(error) => {
                lifecycle.record_unavailable(error, cx);
                return Availability::Unavailable;
            }
        };
        let Creator::Available(adapter) = &mut lifecycle.creator else {
            return Availability::Unavailable;
        };
        adapter.next_creation_id = adapter.next_creation_id.wrapping_add(1).max(1);
        let id = adapter.next_creation_id;
        lifecycle.slots.insert(
            key.to_owned(),
            WebViewSlot::Creating {
                id,
                phase: CreationPhase::Queued,
                pending_url: Some(initial_url.clone()),
            },
        );
        cx.spawn_in(window, async move |this, cx| {
            let created = futures::future::select(
                Box::pin(events.next()),
                Box::pin(cx.background_executor().timer(Duration::from_secs(10))),
            )
            .await;
            let result = match created {
                futures::future::Either::Left((Some(event), _)) if event.kind == 7 => Ok(()),
                futures::future::Either::Left((Some(event), _)) => Err(event.message),
                _ => Err("Android WebView creation timed out or was cancelled".into()),
            };
            let _ = this.update(cx, |lifecycle, cx| {
                if !lifecycle.owner_is_live() || !lifecycle.has_creation(id) {
                    return;
                }
                match result {
                    Err(error) => lifecycle.record_unavailable(error, cx),
                    Ok(()) => {
                        let Some((key, pending_url)) = lifecycle.remove_creation(id) else {
                            return;
                        };
                        if let Some(url) = pending_url
                            && url != initial_url
                        {
                            let _ = raw.load_url(&url);
                        }
                        let id = page.id;
                        let view = cx.new(|cx| WebView::new(raw, events, page_events, cx));
                        cx.observe(&view, |_, _, cx| cx.notify()).detach();
                        lifecycle
                            .slots
                            .insert(key, WebViewSlot::Ready { view, page });
                        lifecycle.watch_page(id, receiver, cx);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        Availability::Starting(CreationPhase::Queued)
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Weak;

    use gpui::{AppContext as _, TestAppContext};

    use super::BrowserLifecycle;

    /// Two panes can show a page each: one panel letting its page go, or
    /// moving to another, leaves the other panel's on screen.
    #[gpui::test]
    fn each_panel_keeps_the_page_it_shows(cx: &mut TestAppContext) {
        let (left, right) = (cx.new(|_| ()).entity_id(), cx.new(|_| ()).entity_id());
        let lifecycle = cx
            .new(|_| BrowserLifecycle::new(Weak::new(), gpui::WeakEntity::new_invalid(), Ok(None)));
        lifecycle.update(cx, |lifecycle, cx| {
            lifecycle.show_for(left, Some("thread-1"), cx);
            lifecycle.show_for(right, Some("thread-2"), cx);
            assert!(lifecycle.is_shown("thread-1") && lifecycle.is_shown("thread-2"));

            lifecycle.show_for(left, Some("thread-3"), cx);
            assert!(!lifecycle.is_shown("thread-1"));
            assert!(lifecycle.is_shown("thread-2") && lifecycle.is_shown("thread-3"));

            lifecycle.hide_except(right, None, cx);
            assert!(!lifecycle.is_shown("thread-2"));
            assert!(lifecycle.is_shown("thread-3"));

            lifecycle.forget_owner(left, cx);
            assert!(!lifecycle.is_shown("thread-3"));
        });
    }
}
