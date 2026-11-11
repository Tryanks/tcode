//! The right-panel "Preview" tab.
//!
//! The panel itself is shared by every client: the URL field, open-externally,
//! copy-URL, the per-conversation URL/canvas state and the loading/error copy
//! are ordinary replicated-store UI. One client renders a thread's pages
//! (`store::preview`); the panel on any other says where they are and offers
//! to show them here. What is native is the embedded browser —
//! creating a `gpui-webview` child view, driving its history and JS, and snapshotting
//! it — and that lives behind [`preview_backend`].
//!
//! ## Where a backend exists
//!
//! macOS, Windows, Linux, Android and iOS, and only with the `native-preview`
//! feature (`cfg(preview_backend)`, set by the build script). On Linux
//! `gpui-webview` embeds WebKitGTK as an X11 child and drives GTK from GPUI;
//! when GPUI runs on Wayland, GTK runs on a nested compositor whose frames are
//! shown on a native composition surface instead (see [`wayland`]).
//! Android hosts activity-owned WebViews through JNI; iOS adds a `gpui-webview`
//! child to the GPUI host view and places it itself (see [`ios`]); Windows
//! drives a WebView2 composition controller itself (see [`webview2`]). A client
//! without a backend — the browser — neither offers the tab
//! ([`WorkspaceStore::offers_preview`](crate::store::WorkspaceStore::offers_preview))
//! nor subscribes to the agent's preview requests.
//!
//! Windows creation is deliberately asynchronous. WebView2 construction is
//! asynchronous underneath; waiting for it synchronously means a nested Win32
//! message pump, which can dispatch GPUI teardown while the parent HWND is
//! still underneath the creation call. Its completions are awaited on GPUI's
//! window foreground executor instead, and each pending page is
//! generation-tagged, so teardown can cancel the slot without re-entering GPUI
//! or allowing a stale completion to replace a newer preview. macOS keeps the
//! proven synchronous child-view path.
//!
//! ## Load errors
//!
//! A navigation that fails — an untrusted certificate, a dead port — leaves the
//! previous document on screen, so the JavaScript status probe cannot see it.
//! [`load_error`] observes the platform's own navigation callbacks (WKWebView's
//! delegate, WebView2's `NavigationCompleted`) and keeps the last failure per
//! webview; `preview_status` reports it as `load_error` and `preview_wait_for`
//! fails with it instead of waiting out its timeout.
//!
//! ## Native overlays
//!
//! On macOS and Linux `gpui-webview`, and on Windows [`webview2`], use GPUI
//! Fast's window composition to put deferred GPUI overlays above the native
//! page. Their element owns document bounds and clipping.
//! GPUI overlays are not reliably painted above the page after a window resize,
//! so the palette and the compact menu still hide the page on every platform.
//! Tab/conversation switch and route changes hide inactive pages without
//! discarding their loaded documents; the desktop panel shutting lets its
//! conversation's page go ([`PreviewPanel::release_page`]).

use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_base::{h_flex, v_flex};
#[cfg(preview_backend)]
use tcode_protocol::PreviewResponse;

use crate::icon::IconName;
use crate::material;
use crate::store::{PreviewPlace, PreviewReleased, WorkspaceStore};
use crate::theme::ActiveTheme as _;
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::menu::DropdownMenu as _;
use crate::window_caption;
use crate::window_state::{Route, WindowState};

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// Whether this client can embed a browser. Views ask before offering an action
/// that needs one, so nothing renders an enabled control that cannot work.
///
/// On Linux `gpui-webview` parents WebKitGTK to an X11 window; a client that
/// GPUI connected to Wayland hosts it through [`wayland`] instead, which needs
/// a runtime directory for its socket. GPUI picks Wayland whenever
/// `WAYLAND_DISPLAY` names one, and `guess_compositor` reports that same choice;
/// a window cannot change display server once open.
pub(crate) fn preview_backend() -> bool {
    #[cfg(all(preview_backend, target_os = "linux"))]
    {
        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(
            || match gpui::GraphicalEnvironment::detect().guess_compositor() {
                "X11" => true,
                "Wayland" => wayland::supported(),
                _ => false,
            },
        )
    }
    #[cfg(not(all(preview_backend, target_os = "linux")))]
    {
        cfg!(preview_backend)
    }
}

#[cfg(preview_backend)]
mod capture;
#[cfg(preview_backend)]
mod events;
#[cfg(preview_backend)]
pub(crate) mod lifecycle;
#[cfg(preview_backend)]
mod page_agent;
/// The reply channel a broker request is answered on.
#[cfg(preview_backend)]
type ReplyTx = async_channel::Sender<Result<PreviewResponse, String>>;

// Only a build with a backend routes by these; the tests below pin the contract
// on every target so a portable edit cannot quietly change it.
#[cfg_attr(not(preview_backend), allow(dead_code))]
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

/// What an automation tool answers when the platform webview cannot be created:
/// say so plainly, with the underlying error, rather than leaving the agent to
/// guess why nothing happened.
#[cfg(preview_backend)]
fn unavailable_message(err: &str) -> String {
    format!(
        "the preview browser is unavailable on this machine \
         (the system webview component could not be created: {err})"
    )
}

/// An action on the current preview URL. The compact toolbar reaches these
/// through its overflow menu, which addresses items by action rather than by
/// callback.
#[derive(gpui::Action, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_preview, no_json)]
pub(crate) enum PreviewAction {
    Close,
    CopyUrl,
    OpenExternal,
    DevServers,
}

/// Add a scheme to a bare host/port (so `localhost:5173` becomes a real URL).
fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.contains("://")
        || ["about:", "data:", "blob:"]
            .iter()
            .any(|scheme| trimmed.starts_with(scheme))
    {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

pub struct PreviewPanel {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// The thread whose page this panel shows. None on the attachment's
    /// panel, which draws nothing and answers the preview tools.
    session_id: Option<String>,
    /// Another pane is drawn over this one, under which a native page would
    /// still show.
    covered: bool,
    /// Whether the panel's pane reaches the window's top right corner.
    at_window_corner: bool,
    /// The shared address-bar input (reflects the active session's URL).
    url_input: Entity<InputState>,
    /// Session id whose URL is currently mirrored into `url_input`.
    mirrored: Option<String>,
    /// The last URL copied into the editor; preserve an unsent edit until the
    /// actual page URL or conversation changes.
    mirrored_url: Option<String>,
    compact_overflow_open: bool,
    /// Pages the desktop panel let go of while their thread's turn was
    /// running, by key, with that thread: let go once the turn ends.
    idle_releases: std::collections::HashMap<String, String>,
    /// The client last shown rendering this panel's thread, for the note
    /// saying so while it fades.
    rendered_on: String,
    /// The panel's motion, kept here: the window draws the panel under
    /// another parent as it turns compact or wide.
    motion: crate::motion::owned::Owned,
    /// The thread whose dev servers on the host the panel shows as picks.
    dev_servers: Option<String>,
    #[cfg(preview_backend)]
    backend: backend::Backend,
    _subscriptions: Vec<Subscription>,
}

impl PreviewPanel {
    /// The attachment's panel, owner of its pages.
    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        #[cfg(preview_backend)]
        let backend = backend::Backend::new(store.downgrade(), store.read(cx).preview_proxy(), cx);
        let mut panel = Self::with_backend(
            store.clone(),
            window_state,
            None,
            #[cfg(preview_backend)]
            backend,
            window,
            cx,
        );
        panel._subscriptions.push(cx.subscribe(
            &store,
            |this, _, released: &PreviewReleased, cx| {
                let key = this
                    .store
                    .read(cx)
                    .preview_key_for(&released.session_id, cx)
                    .unwrap_or_else(|| released.session_id.clone());
                this.idle_releases.remove(&key);
                this.drop_webview(&key, cx);
                cx.notify();
            },
        ));
        panel
    }

    /// A panel on this panel's pages, showing `session_id`'s in a pane.
    pub(crate) fn for_pane(
        &self,
        session_id: Option<String>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let (store, window_state) = (self.store.clone(), self.window_state.clone());
        cx.new(|cx| {
            #[cfg(preview_backend)]
            let backend = self.backend.sharing();
            Self::with_backend(
                store,
                window_state,
                session_id,
                #[cfg(preview_backend)]
                backend,
                window,
                cx,
            )
        })
    }

    fn with_backend(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        session_id: Option<String>,
        #[cfg(preview_backend)] backend: backend::Backend,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url_input = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            cx.observe(&store, |this, _, cx| {
                // Native child views outlive GPUI layout nodes. Visibility
                // therefore follows WorkspaceStore directly, even while this
                // entity is no longer mounted in the right-panel tree.
                this.prune_deleted_webviews(cx);
                this.release_idle_pages(cx);
                this.sync_visibility(cx);
                cx.notify();
            }),
            #[cfg(preview_backend)]
            cx.subscribe(
                &store,
                |this, _, state: &tcode_client::ConnectionState, cx| {
                    if matches!(
                        state,
                        tcode_client::ConnectionState::Syncing { .. }
                            | tcode_client::ConnectionState::Connected { .. }
                    ) {
                        this.refresh_preview_endpoint(cx);
                    }
                },
            ),
            cx.observe(&window_state, |this, _, cx| {
                this.sync_visibility(cx);
                cx.notify();
            }),
            cx.subscribe_in(&url_input, window, Self::on_url_event),
            cx.on_release(|this, cx| {
                if let Some(session_id) = this.dev_servers.take() {
                    this.store
                        .update(cx, |store, _| store.hide_dev_servers(&session_id));
                }
            }),
            #[cfg(preview_backend)]
            {
                let owner = cx.entity_id();
                cx.on_release(move |this, cx| {
                    this.lifecycle()
                        .update(cx, |lifecycle, cx| lifecycle.forget_owner(owner, cx));
                })
            },
        ];
        let mut panel = Self {
            store: store.clone(),
            window_state,
            session_id,
            covered: false,
            at_window_corner: false,
            url_input,
            mirrored: None,
            mirrored_url: None,
            compact_overflow_open: false,
            idle_releases: Default::default(),
            rendered_on: String::new(),
            motion: Default::default(),
            dev_servers: None,
            #[cfg(preview_backend)]
            backend,
            _subscriptions: subscriptions,
        };
        panel.observe_backend(cx);
        panel
    }

    /// Show `session_id`'s page, or none.
    pub(crate) fn set_session(this: &Entity<Self>, session_id: Option<String>, cx: &mut App) {
        if this.read(cx).session_id != session_id {
            this.update(cx, |panel, cx| {
                // The picks shown follow the thread.
                if let Some(shown) = panel.dev_servers.take() {
                    panel.store.update(cx, |store, cx| {
                        store.hide_dev_servers(&shown);
                        if let Some(session_id) = &session_id {
                            store.show_dev_servers(session_id, cx);
                        }
                    });
                    panel.dev_servers.clone_from(&session_id);
                }
                panel.session_id = session_id;
                cx.notify();
            });
        }
    }

    pub(crate) fn set_at_window_corner(this: &Entity<Self>, at_corner: bool, cx: &mut App) {
        if this.read(cx).at_window_corner != at_corner {
            this.update(cx, |panel, cx| {
                panel.at_window_corner = at_corner;
                cx.notify();
            });
        }
    }

    /// Another pane is drawn over this panel's, or no longer.
    pub(crate) fn set_covered(this: &Entity<Self>, covered: bool, cx: &mut App) {
        if this.read(cx).covered != covered {
            this.update(cx, |panel, cx| {
                panel.covered = covered;
                panel.sync_visibility(cx);
                cx.notify();
            });
        }
    }

    /// Keep the panel's motion in `scene`, its pane's drawer's: the page and
    /// its address follow the thread it shows.
    pub(crate) fn share_scene(&mut self, scene: &crate::motion::Epoch) {
        self.motion = crate::motion::owned::Owned::in_scene(scene);
    }

    /// The stable conversation key the chrome is currently addressing.
    fn active_key(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let current = self.identity(cx);
        self.reconcile_active_key(current, cx)
    }

    /// The panel's thread, and its preview's stable key.
    fn identity(&self, cx: &App) -> Option<(String, String)> {
        let session_id = self.session_id.as_deref()?;
        let key = self.store.read(cx).preview_key_for(session_id, cx)?;
        Some((session_id.to_owned(), key))
    }

    /// Mirror a URL into the store, then navigate whatever backend exists.
    fn navigate(&mut self, key: &str, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let url = normalize_url(url);
        self.store
            .update(cx, |store, cx| store.set_preview_url(key, url.clone(), cx));
        self.navigate_backend(key, &url, window, cx);
        self.sync_visibility(cx);
        cx.notify();
    }

    /// The user sent the page to `url`: it shows here, taken from any
    /// other client rendering it.
    fn navigate_here(&mut self, key: &str, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session_id) = self.session_id.clone() {
            self.store
                .update(cx, |store, cx| store.show_preview_here(&session_id, cx));
        }
        self.user_acted(cx);
        self.navigate(key, url, window, cx);
    }

    /// Show the host's dev servers for this panel's thread as picks, or stop.
    fn toggle_dev_servers(&mut self, cx: &mut Context<Self>) {
        let shown = match self.dev_servers.take() {
            Some(shown) => {
                self.store
                    .update(cx, |store, _| store.hide_dev_servers(&shown));
                None
            }
            None => self.session_id.clone(),
        };
        if let Some(session_id) = &shown {
            self.store
                .update(cx, |store, cx| store.show_dev_servers(session_id, cx));
        }
        self.dev_servers = shown;
        cx.notify();
    }

    /// The user acted on this panel's page, which the page the agent last
    /// read no longer reflects.
    fn user_acted(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = self.session_id.clone() {
            self.store
                .update(cx, |store, _| store.report_preview_input(&session_id));
        }
    }

    /// Where this panel's thread's pages render. Shown here while no client
    /// renders them, they become this client's.
    fn place(&mut self, cx: &mut Context<Self>) -> PreviewPlace {
        let Some(session_id) = self.session_id.clone().filter(|_| preview_backend()) else {
            return PreviewPlace::Here;
        };
        let place = self.store.read(cx).preview_place(&session_id, cx);
        match &place {
            PreviewPlace::Settling => self
                .store
                .update(cx, |store, _| store.claim_preview(&session_id)),
            PreviewPlace::Elsewhere(device) => self.rendered_on.clone_from(device),
            PreviewPlace::Here => {}
        }
        place
    }

    fn show_here(&mut self, cx: &mut Context<Self>) {
        if let Some(session_id) = self.session_id.clone() {
            self.store
                .update(cx, |store, cx| store.show_preview_here(&session_id, cx));
            cx.notify();
        }
    }

    /// The thread's page is on another client: say which, and offer to
    /// bring it here.
    fn render_elsewhere(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::sizing::Sizable as _;
        material::empty_state(
            crate::icon::Icon::new(IconName::Monitor),
            crate::tr!("preview.elsewhere", device = self.rendered_on),
            crate::tr!("preview.elsewhere_hint"),
            cx,
        )
        .child(
            crate::widgets::button::Button::new("preview-show-here")
                .motion(&self.motion)
                .small()
                .label(crate::tr!("preview.show_here"))
                .on_click(cx.listener(|this, _, _, cx| this.show_here(cx))),
        )
        .into_any_element()
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
                self.navigate_here(&key, &url, window, cx);
            }
        }
    }

    fn active_url(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let key = self.active_key(cx)?;
        self.store.read(cx).preview_url(&key)
    }

    /// Hand the current URL to the OS browser. `cx.open_url` is gpui's
    /// cross-platform launcher (`open` / `ShellExecute` / `xdg-open`, and the
    /// browser's own `window.open`).
    fn open_in_system_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(all(preview_backend, any(target_os = "macos", target_os = "ios")))]
        let url = self.external_preview_url(cx);
        #[cfg(not(all(preview_backend, any(target_os = "macos", target_os = "ios"))))]
        let url = self.active_url(cx);
        if let Some(url) = url {
            cx.open_url(&url);
        } else {
            use crate::overlay::{Notification, OverlayExt as _};
            window.push_notification(
                Notification::error(crate::tr!("preview.external_unavailable")),
                cx,
            );
        }
    }

    fn copy_url(&mut self, cx: &mut Context<Self>) {
        if let Some(url) = self.active_url(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(url));
        }
    }

    /// Release the conversation's browser and chrome without changing layout.
    /// Desktop close additionally collapses the panel; compact close stays at URL entry.
    pub(crate) fn release_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.active_key(cx) {
            self.drop_webview(&key, cx);
            self.store
                .update(cx, |store, cx| store.clear_preview_chrome(&key, cx));
        }
        // Un-mirror so a later reopen refreshes the address bar from the
        // (now empty) URL map instead of showing the stale address.
        self.mirrored = None;
        cx.notify();
    }

    /// The desktop panel has shut on its conversation: let its page go,
    /// keeping its URL and canvas for the panel to load again. While the
    /// thread's turn runs, its agent may be driving the page through the
    /// preview tools, which reach it with the panel shut; the page is let
    /// go once the turn ends, unless the panel opens again.
    pub(crate) fn release_page(&mut self, cx: &mut Context<Self>) {
        let Some((session_id, key)) = self.identity(cx) else {
            return;
        };
        if self.store.read(cx).turn_running_for(&session_id) {
            self.idle_releases.insert(key, session_id);
        } else {
            self.drop_webview(&key, cx);
        }
    }

    fn release_idle_pages(&mut self, cx: &mut Context<Self>) {
        if self.idle_releases.is_empty() {
            return;
        }
        let store = self.store.read(cx);
        let mut idle = Vec::new();
        self.idle_releases.retain(|key, session_id| {
            if store.right_panel_open_for_key(key) {
                return false;
            }
            if store.turn_running_for(session_id) {
                return true;
            }
            idle.push(key.clone());
            false
        });
        for key in idle {
            self.drop_webview(&key, cx);
        }
    }

    fn close_panel(&mut self, cx: &mut Context<Self>) {
        self.release_preview(cx);
        self.store.update(cx, |store, cx| {
            store.close_preview_panel_for(self.session_id.as_deref(), cx)
        });
        cx.notify();
    }

    /// The wide chrome's actions on the page, each its own button.
    fn render_inline_actions(&self, offer_ports: bool, cx: &mut Context<Self>) -> AnyElement {
        let expanded = self
            .store
            .read(cx)
            .panel_state_for(self.session_id.as_deref())
            .right_panel_expanded;
        h_flex()
            .gap(cx.theme().tokens.spacing.xs)
            .items_center()
            .when(offer_ports, |chrome| {
                chrome.child(
                    material::toolbar_icon_button(
                        "preview-ports",
                        IconName::Globe,
                        crate::tr!("preview.dev_servers"),
                        false,
                    )
                    .selected(self.dev_servers.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_dev_servers(cx))),
                )
            })
            .child(
                material::toolbar_icon_button(
                    "preview-copy-url",
                    IconName::Copy,
                    crate::tr!("preview.copy_url"),
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.copy_url(cx))),
            )
            .child(
                material::toolbar_icon_button(
                    "preview-open-external",
                    IconName::ExternalLink,
                    crate::tr!("preview.open_external"),
                    false,
                )
                .on_click(
                    cx.listener(|this, _, window, cx| this.open_in_system_browser(window, cx)),
                ),
            )
            .child(
                material::toolbar_icon_button(
                    "preview-expand",
                    if expanded {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    },
                    if expanded {
                        crate::tr!("diff.restore_width")
                    } else {
                        crate::tr!("diff.expand_width")
                    },
                    false,
                )
                .debug_selector(|| "preview-expand".into())
                .on_click(cx.listener(|this, _, _, cx| {
                    let session_id = this.session_id.clone();
                    this.store.update(cx, |store, cx| {
                        store.toggle_diff_expanded_for(session_id.as_deref(), cx)
                    });
                })),
            )
            .child(
                material::toolbar_icon_button(
                    "preview-close",
                    IconName::Close,
                    crate::tr!("preview.close"),
                    false,
                )
                .on_click(cx.listener(|this, _, _, cx| this.close_panel(cx))),
            )
            .into_any_element()
    }

    /// The compact chrome's one menu of the actions on the page.
    fn render_overflow_actions(&self, offer_ports: bool, cx: &mut Context<Self>) -> AnyElement {
        let panel = cx.entity().downgrade();
        let shows_dev_servers = self.dev_servers.is_some();
        material::toolbar_icon_button(
            "preview-overflow",
            IconName::Ellipsis,
            crate::tr!("mobile.more_actions"),
            true,
        )
        .dropdown_menu(move |menu, _, cx| {
            let _ = panel.update(cx, |panel, cx| {
                panel.compact_overflow_open = true;
                panel.sync_visibility(cx);
                cx.notify();
            });
            let panel = panel.clone();
            cx.on_release(move |_, cx| {
                let _ = panel.update(cx, |panel, cx| {
                    panel.compact_overflow_open = false;
                    cx.notify();
                });
            })
            .detach();
            menu.menu(
                crate::tr!("preview.close_compact").into_owned(),
                Box::new(PreviewAction::Close),
            )
            .menu(
                crate::tr!("preview.copy_url").into_owned(),
                Box::new(PreviewAction::CopyUrl),
            )
            .menu(
                crate::tr!("preview.open_external").into_owned(),
                Box::new(PreviewAction::OpenExternal),
            )
            .when(offer_ports, |menu| {
                menu.menu_with_check(
                    crate::tr!("preview.dev_servers").into_owned(),
                    shows_dev_servers,
                    Box::new(PreviewAction::DevServers),
                )
            })
        })
        .into_any_element()
    }

    fn render_chrome(
        &self,
        here: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Windows: an open Preview tab is the rightmost column, so its
        // chrome row is a top strip hosting the caption buttons, which reach
        // the window's true top-right corner.
        let hosts_caption = self.at_window_corner && {
            let panel = self
                .store
                .read(cx)
                .panel_state_for(self.session_id.as_deref());
            window_caption::hosts_caption_for_state(
                window_caption::CaptionSurface::Preview,
                self.window_state.read(cx).route(),
                panel.right_panel_open,
                panel.right_tab,
            )
        };
        // The host's dev servers, wherever this client is: a picked one
        // loads on the client rendering the page, through the Preview
        // tunnels when that is not the host.
        let offer_ports =
            preview_backend() && self.store.read(cx).space_allows(|policy| policy.preview);
        // Compact: one toolbar row on the page inset, with 44pt touch targets.
        // Close is a right-panel affordance — the Panel page's Back leaves it —
        // so it is not drawn here at all.
        let compact = self.window_state.read(cx).compact;
        let kind = if hosts_caption {
            material::Bar::Strip
        } else {
            material::Bar::Toolbar
        };
        material::bar("preview-chrome", kind, compact, cx)
            .when(hosts_caption, |chrome| chrome.pr_0())
            // Back / forward / reload drive a page. Without a backend, or
            // with the page on another client, they are absent rather than
            // present-but-dead.
            .children(
                here.then(|| self.history_controls(cx))
                    .into_iter()
                    .flatten(),
            )
            .child(div().flex_1().min_w_0().child(
                Input::new(&self.url_input).placeholder(crate::tr!("preview.url_placeholder")),
            ))
            // Compact: the address field is the row. Everything that acts *on*
            // the current URL goes into one overflow menu rather than squeezing
            // the field down to a word. Turning compact or wide cross-fades the
            // buttons and the menu while their room glides, on the panel's
            // clock, since the window draws the panel under another parent then.
            .child({
                let swap = self
                    .motion
                    .swap("actions", usize::from(compact), 2, CHANGE, window, cx)
                    .across();
                let mut layers = Vec::new();
                if swap.shows(0) {
                    layers.push((0, self.render_inline_actions(offer_ports, cx)));
                }
                if swap.shows(1) {
                    layers.push((1, self.render_overflow_actions(offer_ports, cx)));
                }
                swap.render(layers)
            })
            .children(hosts_caption.then(|| window_caption::caption_controls(window, cx)))
    }

    pub(crate) fn on_preview_action(
        &mut self,
        action: &PreviewAction,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            PreviewAction::Close => self.release_preview(cx),
            PreviewAction::CopyUrl => self.copy_url(cx),
            PreviewAction::OpenExternal => self.open_in_system_browser(_window, cx),
            PreviewAction::DevServers => self.toggle_dev_servers(cx),
        }
    }

    #[cfg(preview_backend)]
    fn render_note(
        &self,
        icon: IconName,
        title: String,
        detail: Option<String>,
        cx: &Context<Self>,
    ) -> AnyElement {
        material::empty_state(
            crate::icon::Icon::new(icon),
            title,
            detail.unwrap_or_default(),
            cx,
        )
        .into_any_element()
    }
}

impl Render for PreviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // When the embedded browser is turned off in Settings → Preview, hide
        // the chrome and webview entirely and show a quiet placeholder.
        if !self.store.read(cx).preview_browser_settings().enabled {
            return v_flex().size_full().child(material::empty_state(
                crate::icon::Icon::new(IconName::EyeOff),
                crate::tr!("browser.disabled_panel"),
                "",
                cx,
            ));
        }
        let active = self.active_key(cx);
        let current_url = active
            .as_ref()
            .and_then(|id| self.store.read(cx).preview_url(id));
        if active != self.mirrored || current_url != self.mirrored_url {
            let value = current_url.clone().unwrap_or_default();
            self.url_input
                .update(cx, |state, cx| state.set_value(&value, window, cx));
            self.mirrored = active.clone();
            self.mirrored_url = current_url;
        }

        let place = self.place(cx);
        let state = match place {
            PreviewPlace::Here => 0,
            PreviewPlace::Elsewhere(_) => 1,
            PreviewPlace::Settling => 2,
        };
        let fade = crate::motion::CrossFade::new("preview-place", state, 3, CHANGE, window, cx);
        let mut layers = Vec::new();
        for layer in (0..3).filter(|layer| fade.shows(*layer)) {
            let content = match layer {
                0 if state == 0 => self.render_body(active.as_deref(), window, cx),
                // A page leaving fades out natively once it is let go; drawn
                // again, it would be made anew.
                0 => continue,
                1 => self.render_elsewhere(cx),
                _ => material::empty_state(
                    crate::icon::Icon::new(IconName::Globe),
                    crate::tr!("preview.starting"),
                    "",
                    cx,
                )
                .into_any_element(),
            };
            layers.push((
                layer,
                v_flex().size_full().child(content).into_any_element(),
            ));
        }
        let body = fade.fill().render(layers).flex_1().min_h_0().w_full();
        let panel = v_flex()
            .size_full()
            .on_action(cx.listener(Self::on_preview_action))
            .child(self.render_chrome(state == 0, window, cx))
            .children(self.render_port_row(window, cx))
            .child(body);
        #[cfg(all(preview_backend, not(target_os = "android")))]
        let panel = panel.child(self.yield_to_resize_handles(active.as_deref(), cx));
        panel
    }
}

#[cfg(not(preview_backend))]
mod portable {
    use super::*;

    impl PreviewPanel {
        pub(crate) fn set_compact_panel_selected(
            &mut self,
            _selected: bool,
            _cx: &mut Context<Self>,
        ) {
        }

        pub(super) fn observe_backend(&mut self, _cx: &mut Context<Self>) {}

        pub(super) fn reconcile_active_key(
            &mut self,
            current: Option<(String, String)>,
            _cx: &mut Context<Self>,
        ) -> Option<String> {
            current.map(|(_, key)| key)
        }

        pub(super) fn navigate_backend(
            &mut self,
            _key: &str,
            _url: &str,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
        }

        pub(super) fn drop_webview(&mut self, key: &str, _cx: &mut Context<Self>) {
            if self.mirrored.as_deref() == Some(key) {
                self.mirrored = None;
            }
        }

        pub(super) fn prune_deleted_webviews(&mut self, cx: &mut Context<Self>) {
            if self
                .mirrored
                .as_ref()
                .is_some_and(|key| !self.store.read(cx).preview_key_live(key))
            {
                self.mirrored = None;
            }
        }

        pub(super) fn history_controls(&self, _cx: &mut Context<Self>) -> Vec<AnyElement> {
            Vec::new()
        }

        pub(super) fn render_port_row(
            &self,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<AnyElement> {
            None
        }

        pub fn sync_visibility(&mut self, _cx: &mut Context<Self>) {}

        pub(crate) fn native_fade(&self, _cx: &App) -> Option<AnyElement> {
            None
        }

        pub(crate) fn observe_native_fade<T: 'static>(
            _panel: &gpui::Entity<Self>,
            _cx: &mut Context<T>,
        ) -> Option<gpui::Subscription> {
            None
        }

        /// Unreachable: without a backend the store never offers the tab.
        pub(super) fn render_body(
            &mut self,
            _active: Option<&str>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> AnyElement {
            div().into_any_element()
        }
    }
}

#[cfg(preview_backend)]
mod backend;

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn switching_language_relocalizes_the_address_placeholder(cx: &mut gpui::TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let window_state = cx.new(|_| WindowState::default());
        let (panel, cx) = cx.add_window_view(|window, cx| {
            PreviewPanel::new(store.clone(), window_state, window, cx)
        });
        crate::settings::assert_follows_language_switch(cx, "preview.url_placeholder", |cx| {
            panel.read_with(cx, |panel, cx| {
                let input = panel.url_input.read(cx);
                input.presentation().placeholder().to_string()
            })
        });
    }

    #[test]
    fn normalize_url_adds_a_scheme_to_bare_hosts() {
        assert_eq!(normalize_url("localhost:5173"), "http://localhost:5173");
        assert_eq!(normalize_url(" https://x.dev "), "https://x.dev");
        assert_eq!(normalize_url("about:blank"), "about:blank");
        assert_eq!(
            normalize_url("data:text/html,%3Ch1%3E"),
            "data:text/html,%3Ch1%3E"
        );
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
}

#[cfg(all(preview_backend, target_os = "android"))]
mod android;
#[cfg(all(preview_backend, target_os = "android"))]
mod android_geometry;

#[cfg(all(preview_backend, target_os = "ios"))]
mod ios;

#[cfg(all(preview_backend, target_os = "windows"))]
mod webview2;

#[cfg(all(preview_backend, target_os = "linux"))]
mod wayland;

#[cfg(all(
    preview_backend,
    any(
        target_os = "macos",
        target_os = "windows",
        target_os = "linux",
        target_os = "ios"
    )
))]
mod proxy;

#[cfg(all(preview_backend, any(target_os = "macos", target_os = "ios")))]
mod remote;
