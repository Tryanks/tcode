//! The right-panel "Preview" tab.
//!
//! The panel itself is shared by every client: the URL field, open-externally,
//! copy-URL, the per-conversation URL/canvas state and the loading/error copy
//! are ordinary replicated-store UI. What is native is the embedded browser —
//! creating a `gpui-wry` child view, driving its history and JS, and snapshotting
//! it — and that lives behind [`PREVIEW_BACKEND`].
//!
//! ## Where a backend exists
//!
//! macOS and Windows only, and only with the `native-preview` feature. Linux is
//! excluded on purpose: lb-wry's `build_as_child` is X11-only there *and*
//! requires a GTK main loop (`gtk::init` plus `gtk::main_iteration_do` pumped on
//! the UI thread), while gpui's Linux backend runs calloop/xcb and never pumps
//! GTK — the webview would panic at construction and could never be driven.
//! Phones and the browser have no child-view seam at all. Those clients render
//! the same panel with open-externally and copy-URL, and answer every automation
//! request with an explicit "unsupported" rather than timing out.
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
//! route. Other overlapping GPUI popovers can still be covered by the native view.

use gpui::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_protocol::PreviewResponse;

use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::window_caption;
use crate::window_state::{Route, WindowState};
use crate::{icon::IconName, sizing::Sizable as _};

/// Whether this build can embed a browser. Views ask before offering an action
/// that needs one, so nothing renders an enabled control that cannot work.
pub(crate) const PREVIEW_BACKEND: bool = cfg!(all(
    feature = "native-preview",
    any(target_os = "macos", target_os = "windows")
));

#[cfg(all(
    feature = "native-preview",
    any(target_os = "macos", target_os = "windows")
))]
pub(crate) mod lifecycle;

/// The reply channel a broker request is answered on.
type ReplyTx = async_channel::Sender<Result<PreviewResponse, String>>;

// Only a build with a backend routes by these; the tests below pin the contract
// on every target so a portable edit cannot quietly change it.
#[cfg_attr(
    not(all(
        feature = "native-preview",
        any(target_os = "macos", target_os = "windows")
    )),
    allow(dead_code)
)]
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
#[cfg_attr(
    not(all(
        feature = "native-preview",
        any(target_os = "macos", target_os = "windows")
    )),
    allow(dead_code)
)]
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

/// What an automation tool answers when the platform webview cannot be created
/// (Windows without the WebView2 runtime): say so plainly, with the underlying
/// error, rather than leaving the agent to guess why nothing happened.
#[cfg_attr(not(feature = "native-preview"), allow(dead_code))]
fn unavailable_message(err: &str) -> String {
    format!(
        "the preview browser is unavailable on this machine \
         (the system webview component could not be created: {err})"
    )
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

pub struct PreviewPanel {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// The shared address-bar input (reflects the active session's URL).
    url_input: Entity<InputState>,
    /// Session id whose URL is currently mirrored into `url_input`.
    mirrored: Option<String>,
    #[cfg(all(
        feature = "native-preview",
        any(target_os = "macos", target_os = "windows")
    ))]
    /// Discovered localhost dev-server ports (populated by the "Ports" button).
    dev_ports: Vec<u16>,
    #[cfg(all(
        feature = "native-preview",
        any(target_os = "macos", target_os = "windows")
    ))]
    /// Discards a completed scan when a newer click has superseded it.
    port_scan_generation: u64,
    #[cfg(all(
        feature = "native-preview",
        any(target_os = "macos", target_os = "windows")
    ))]
    backend: backend::Backend,
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
        let mut panel = Self {
            store,
            window_state,
            url_input,
            mirrored: None,
            #[cfg(all(
                feature = "native-preview",
                any(target_os = "macos", target_os = "windows")
            ))]
            dev_ports: Vec::new(),
            #[cfg(all(
                feature = "native-preview",
                any(target_os = "macos", target_os = "windows")
            ))]
            port_scan_generation: 0,
            #[cfg(all(
                feature = "native-preview",
                any(target_os = "macos", target_os = "windows")
            ))]
            backend: backend::Backend::new(cx),
            _subscriptions: subscriptions,
        };
        panel.observe_backend(cx);
        panel
    }

    /// The stable conversation key the chrome is currently addressing.
    fn active_key(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let current = self.store.read(cx).preview_active_identity();
        self.reconcile_active_key(current, cx)
    }

    /// Mirror a URL into the store, then navigate whatever backend exists.
    fn navigate(&mut self, key: &str, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let url = normalize_url(&self.store.read(cx).rewrite_preview_url(url));
        self.store
            .update(cx, |store, cx| store.set_preview_url(key, url.clone(), cx));
        self.navigate_backend(key, &url, window, cx);
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

    fn active_url(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let key = self.active_key(cx)?;
        self.store.read(cx).preview_url(&key)
    }

    /// Hand the current URL to the OS browser. `cx.open_url` is gpui's
    /// cross-platform launcher (`open` / `ShellExecute` / `xdg-open`, and the
    /// browser's own `window.open`).
    fn open_in_system_browser(&mut self, cx: &mut Context<Self>) {
        if let Some(url) = self.active_url(cx) {
            cx.open_url(&url);
        }
    }

    fn copy_url(&mut self, cx: &mut Context<Self>) {
        if let Some(url) = self.active_url(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(url));
        }
    }

    /// The chrome's X: close the Preview tab *and* drop this conversation's
    /// WebView, so the page is torn down (scripts, media, sockets) rather
    /// than kept running behind a closed panel. The next open or agent op
    /// recreates a fresh webview on demand.
    fn close_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.active_key(cx) {
            self.drop_webview(&key, cx);
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
        // Port discovery scans *this* machine's listeners. Over a remote link
        // those ports belong to the wrong computer, so the affordance is hidden
        // rather than offering the user a list of their own dev servers as if
        // they were the host's. A host URL typed into the field still works.
        let offer_ports = PREVIEW_BACKEND && !self.store.read(cx).is_remote();
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
            // Back / forward / reload drive a page. Without a backend they are
            // absent rather than present-but-dead.
            .children(self.history_controls(cx))
            .child(div().flex_1().min_w_0().child(Input::new(&self.url_input)))
            .when(offer_ports, |chrome| {
                chrome.child(
                    Button::new("preview-ports")
                        .ghost()
                        .small()
                        .compact()
                        .icon(IconName::Globe)
                        .tooltip(crate::tr!("preview.scan_ports"))
                        .on_click(cx.listener(|this, _, _, cx| this.rescan_ports(cx))),
                )
            })
            .child(
                Button::new("preview-copy-url")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Copy)
                    .tooltip(crate::tr!("preview.copy_url"))
                    .on_click(cx.listener(|this, _, _, cx| this.copy_url(cx))),
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
            .children(hosts_caption.then(|| window_caption::caption_controls(window, cx)))
    }

    fn render_note(&self, title: String, detail: Option<String>, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .gap_2()
            .items_center()
            .justify_center()
            .px_8()
            .text_center()
            .text_color(cx.theme().muted_foreground)
            .child(title)
            .children(detail.map(|detail| div().text_size(px(13.)).child(detail)))
            .into_any_element()
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
        if active != self.mirrored {
            let value = active
                .as_ref()
                .and_then(|id| self.store.read(cx).preview_url(id))
                .unwrap_or_default();
            self.url_input
                .update(cx, |state, cx| state.set_value(&value, window, cx));
            self.mirrored = active.clone();
        }

        let body = self.render_body(active.as_deref(), window, cx);
        v_flex()
            .size_full()
            .child(self.render_chrome(window, cx))
            .children(self.render_port_row(cx))
            .child(body)
    }
}

#[cfg(not(all(
    feature = "native-preview",
    any(target_os = "macos", target_os = "windows")
)))]
mod portable {
    use tcode_protocol::PreviewRequest;

    use super::*;

    impl PreviewPanel {
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
            let live = self.store.read(cx).preview_live_keys();
            if self
                .mirrored
                .as_ref()
                .is_some_and(|key| !live.contains(key))
            {
                self.mirrored = None;
            }
        }

        pub(super) fn history_controls(&self, _cx: &mut Context<Self>) -> Vec<AnyElement> {
            Vec::new()
        }

        pub(super) fn render_port_row(&self, _cx: &mut Context<Self>) -> Option<AnyElement> {
            None
        }

        pub(super) fn rescan_ports(&mut self, _cx: &mut Context<Self>) {}

        pub fn sync_visibility(&mut self, _cx: &mut Context<Self>) {}

        pub(super) fn render_body(
            &mut self,
            active: Option<&str>,
            _window: &mut Window,
            cx: &mut Context<Self>,
        ) -> AnyElement {
            if active.is_none() {
                return self.render_note(crate::tr!("preview.no_session").into_owned(), None, cx);
            }
            self.render_note(
                crate::tr!("preview.no_backend").into_owned(),
                Some(crate::tr!("preview.no_backend_hint").into_owned()),
                cx,
            )
        }

        /// Nothing here can drive a page. Answer immediately and explicitly:
        /// a client that stayed silent would leave the agent's call to time out.
        pub fn handle_op(
            &mut self,
            session_id: String,
            op: PreviewRequest,
            reply: ReplyTx,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            log::info!("preview: rejecting op {op:?} for session {session_id} (no backend)");
            // Naming the client stops the agent retrying instead of waiting out
            // a timeout it can never satisfy here.
            let _ = reply.try_send(Err(crate::tr!("preview.unsupported_client").into_owned()));
        }
    }
}

#[cfg(all(
    feature = "native-preview",
    any(target_os = "macos", target_os = "windows")
))]
mod backend;

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use tcode_protocol::PreviewRequest;

    use super::*;

    /// A client with no embedded browser is still reachable over the preview
    /// topic. It must answer, and say why, rather than let the agent's call sit
    /// until it times out.
    #[cfg(not(all(
        feature = "native-preview",
        any(target_os = "macos", target_os = "windows")
    )))]
    #[gpui::test]
    fn a_client_without_a_backend_refuses_preview_requests_immediately(
        cx: &mut gpui::TestAppContext,
    ) {
        use tcode_runtime::pipe::{HostServices, spawn_host};

        let root = std::env::temp_dir().join(format!(
            "tcode-preview-unsupported-{}",
            tcode_services::store::now_millis()
        ));
        let host = spawn_host(
            tcode_services::store::SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .expect("spawn preview test host");
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        let window_state = cx.new(|_| WindowState::new(false));
        let (panel, cx) = cx.add_window_view(|window, cx| {
            PreviewPanel::new(store.clone(), window_state.clone(), window, cx)
        });
        let cx: &mut gpui::VisualTestContext = cx;

        let (reply, answers) = async_channel::bounded(1);
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.handle_op("session".into(), PreviewRequest::Status, reply, window, cx);
            });
        });
        cx.run_until_parked();

        let answer = answers
            .try_recv()
            .expect("an answer, not a dropped request");
        assert_eq!(
            answer,
            Err(crate::tr!("preview.unsupported_client").into_owned())
        );

        host.shutdown_blocking().expect("stop host");
        let _ = std::fs::remove_dir_all(root);
    }

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
}
