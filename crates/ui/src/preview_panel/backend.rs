//! The embedded-browser half of [`super::PreviewPanel`].
//!
//! Everything here needs a real system webview: child-view creation, history,
//! JS evaluation and snapshots. The panel's chrome, URL state and copy/open
//! actions stay in the parent module and compile everywhere.

use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, CursorStyle, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Styled as _, Window, div, px,
};
#[cfg(not(target_os = "android"))]
use gpui::{Bounds, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Point, canvas};
use gpui_base::v_flex;
use tcode_protocol::{PreviewRequest, PreviewResponse};

use super::lifecycle::{Availability, BrowserLifecycle};
use super::{PreviewPanel, ReplyTx, unavailable_message, visible_preview_key};
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// Whether the pointer is dragging a gpui-base resize handle. Base keeps the
/// handle's drag payload private; the resize cursor its band hands the drag is
/// the public sign of one.
fn resize_drag_active(cx: &App) -> bool {
    matches!(
        cx.active_drag_cursor_style(),
        Some(CursorStyle::ResizeColumn | CursorStyle::ResizeRow)
    )
}

/// How far a gpui-base resize band beside the panel reaches into it: a band
/// straddling the boundary 4.5px, one hugging the edge 5px
/// (`HANDLE_PADDING`, `HANDLE_SIZE`, both private to base).
#[cfg(not(target_os = "android"))]
const HANDLE_BAND_REACH: Pixels = px(5.);

/// The even margin between the panel's content area and the page's frame.
/// It is at least the band's reach, so the resize handle beside the panel
/// (the right panel's edge) is GPUI content along its whole width and never
/// lies over the page.
#[cfg(not(target_os = "android"))]
fn page_margin(cx: &App) -> Pixels {
    cx.theme().tokens.spacing.sm.max(HANDLE_BAND_REACH)
}

/// Whether a press lands on the strip left of the page where a resize handle
/// sits: the margin, the frame's border and the band's half beyond the
/// panel's edge. Base tells nothing outside a handle's own renderer that the
/// handle is pressed, and a drag starts only once the pointer is past GPUI's
/// 2px drag threshold, which from this strip can already be over the page; so
/// the press itself is the signal.
#[cfg(not(target_os = "android"))]
fn on_page_seam(page: Bounds<Pixels>, position: Point<Pixels>, cx: &App) -> bool {
    let distance = page.left() - position.x;
    position.y >= page.top()
        && position.y <= page.bottom()
        && distance > px(0.)
        && distance <= page_margin(cx) + px(1.) + HANDLE_BAND_REACH
}

pub(super) struct Backend {
    lifecycle: gpui::Entity<BrowserLifecycle>,
    /// The lifecycle holds this only weakly so an async Windows completion
    /// cannot install after its owning panel has been dropped.
    _owner: Rc<()>,
    compact_panel_selected: bool,
    /// The left button went down beside the page, where a resize handle
    /// sits, and has not come up yet.
    seam_pressed: bool,
}

impl Backend {
    pub(super) fn new(
        store: gpui::WeakEntity<crate::store::WorkspaceStore>,
        proxy: Result<Option<crate::store::PreviewTarget>, String>,
        cx: &mut Context<PreviewPanel>,
    ) -> Self {
        let owner = Rc::new(());
        let lifecycle = cx.new(|_| BrowserLifecycle::new(Rc::downgrade(&owner), store, proxy));
        Self {
            lifecycle,
            _owner: owner,
            compact_panel_selected: false,
            seam_pressed: false,
        }
    }
}

impl Backend {
    /// A backend for another panel on `self`'s pages.
    pub(super) fn sharing(&self) -> Self {
        Self {
            lifecycle: self.lifecycle.clone(),
            _owner: self._owner.clone(),
            compact_panel_selected: false,
            seam_pressed: false,
        }
    }
}

impl PreviewPanel {
    pub(super) fn refresh_preview_endpoint(&self, cx: &mut Context<Self>) {
        if let Ok(Some((host, _))) = self.store.read(cx).preview_proxy() {
            self.lifecycle().read(cx).refresh_endpoint(&host);
        }
    }

    /// The compact shell owns which panel segment is mounted, including Terminal
    /// (which does not change the stored right-panel tab).
    pub(crate) fn set_compact_panel_selected(&mut self, selected: bool, cx: &mut Context<Self>) {
        self.backend.compact_panel_selected = selected;
        self.sync_visibility(cx);
    }

    /// The pages' fades, which the window draws every frame whether or not
    /// this panel is mounted: a page outlives the panel and leaves where it
    /// was. None while there is no page: a deferred draw makes the window
    /// build its whole shell again for any change inside it (gpui-fast
    /// splices no view around a deferred draw outside the views it builds).
    pub(crate) fn native_fade(&self, cx: &App) -> Option<AnyElement> {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            let lifecycle = self.backend.lifecycle.clone();
            if !lifecycle.read(cx).has_pages() {
                return None;
            }
            Some(
                crate::motion::native_fade::NativeFade::new(
                    gpui::ElementId::NamedInteger(
                        "preview-pages".into(),
                        lifecycle.entity_id().as_u64(),
                    ),
                    move |cx| lifecycle.read(cx).native_pages(),
                )
                .into_any_element(),
            )
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        {
            let _ = cx;
            None
        }
    }

    /// Redraw the window that draws [`Self::native_fade`] whenever a fade
    /// starts: a page shown again by this panel alone, as when a resize
    /// handle is released, would otherwise wait at transparent for something
    /// else to redraw it.
    pub(crate) fn observe_native_fade<T: 'static>(
        panel: &gpui::Entity<Self>,
        cx: &mut Context<T>,
    ) -> Option<gpui::Subscription> {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            let lifecycle = panel.read(cx).backend.lifecycle.clone();
            Some(cx.observe(&lifecycle, |_, _, cx| cx.notify()))
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        {
            let _ = (panel, cx);
            None
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub(super) fn external_preview_url(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let key = self.active_key(cx)?;
        let logical = self
            .lifecycle()
            .read(cx)
            .remote_url(&key)
            .or_else(|| self.store.read(cx).preview_url(&key))?;
        self.lifecycle().read(cx).external_url(&key, &logical)
    }

    pub(super) fn observe_backend(&mut self, cx: &mut Context<Self>) {
        let lifecycle = self.lifecycle();
        self._subscriptions
            .push(cx.observe(&lifecycle, |this, _, cx| {
                this.sync_visibility(cx);
                cx.notify();
            }));
    }

    /// Reconcile the physical session with its stable cache key. Draft -> stored
    /// commits retain the same session id, so move all cached browser state
    /// across that one key transition.
    pub(super) fn reconcile_active_key(
        &mut self,
        current: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let lifecycle = self.lifecycle();
        let owner = cx.entity_id();
        if self.window_state.read(cx).compact {
            let previous = lifecycle.read(cx).active_identity(owner).cloned();
            if let Some((session, key)) = previous
                && current.as_ref().map(|(id, _)| id) != Some(&session)
            {
                self.drop_webview(&key, cx);
                self.store
                    .update(cx, |store, cx| store.clear_preview_chrome(&key, cx));
            }
        }
        let reconciliation =
            lifecycle.update(cx, |lifecycle, _| lifecycle.reconcile_key(owner, current));
        if let Some(old_key) = reconciliation.migrated_from.as_deref()
            && self.mirrored.as_deref() == Some(old_key)
        {
            self.mirrored = reconciliation.key.clone();
        }
        reconciliation.key
    }

    pub(crate) fn lifecycle(&self) -> gpui::Entity<BrowserLifecycle> {
        self.backend.lifecycle.clone()
    }

    fn preview_tab_showing(&self, cx: &App) -> bool {
        let panel = self
            .store
            .read(cx)
            .panel_state_for(self.session_id.as_deref());
        panel.right_panel_open && panel.right_tab == tcode_core::ui::RightTab::Preview
    }

    fn routed_key(&mut self, session_id: &str, cx: &mut Context<Self>) -> String {
        self.store
            .read(cx)
            .preview_key_for(session_id, cx)
            .unwrap_or_else(|| session_id.to_owned())
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
        let dragging = resize_drag_active(cx);
        let yielding = self.backend.seam_pressed || dragging;
        let active = self.active_key(cx);
        let window_state = self.window_state.read(cx);
        let visible = visible_preview_key(
            active.as_deref(),
            window_state.route(),
            window_state.palette_mounted(),
            self.preview_tab_showing(cx)
                && !self.covered
                && self.store.read(cx).preview_browser_settings().enabled
                // gpui-fast-macos hands a drag GPUI began to the native
                // view under the pointer (`send_window_event` hit-tests
                // whenever no native view owns the press) and GPUI's own view
                // then drops the event, so a resize handle stops following
                // the pointer over the page. A hidden view is not hit-tested,
                // so the page hides at once (`yield_to_resize_handles`).
                && !yielding
                && (!window_state.compact
                    || (!self.compact_overflow_open
                        && self.backend.compact_panel_selected
                        && window_state.destination() == crate::window_state::Destination::Panel)),
        )
        .map(str::to_string);
        let owner = cx.entity_id();
        self.lifecycle().update(cx, |lifecycle, cx| {
            if dragging {
                lifecycle.hide_now(None, cx);
            } else if yielding {
                lifecycle.hide_now(active.as_deref(), cx);
            } else if allow_show {
                lifecycle.show_for(owner, visible.as_deref(), cx);
            } else {
                lifecycle.hide_except(owner, visible.as_deref(), cx);
            }
        });
    }

    /// Keep the page out of the pointer's way while a resize handle is held:
    /// let it go when a handle beside it is pressed or a resize drag is seen,
    /// and show it again, at the size the drag left, when the button is
    /// released. Window listeners, so this holds whether or not anything
    /// re-renders this panel in between.
    ///
    /// The page does not fade out here: a cover would come a frame after the
    /// press, and an inward drag's first motion would land on the native view
    /// in that frame, GPUI would never see it, and the divider would not
    /// move. It hides inside the press, and fades back in where the platform
    /// allows it ([`BrowserLifecycle::show_for`]).
    #[cfg(not(target_os = "android"))]
    pub(super) fn yield_to_resize_handles(
        &self,
        active: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        let page = active.and_then(|key| self.lifecycle().read(cx).ready_view(key));
        canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                window.on_mouse_event({
                    let panel = panel.clone();
                    move |event: &MouseDownEvent, phase, _, cx| {
                        if !phase.capture() || event.button != MouseButton::Left {
                            return;
                        }
                        let Some(page) = page.as_ref() else {
                            return;
                        };
                        let page = page.read(cx);
                        if page.visible() && on_page_seam(page.bounds(), event.position, cx) {
                            let _ = panel.update(cx, |panel, cx| {
                                panel.backend.seam_pressed = true;
                                panel.sync_visibility(cx);
                            });
                        }
                    }
                });
                window.on_mouse_event({
                    let panel = panel.clone();
                    move |_: &MouseMoveEvent, phase, _, cx| {
                        if phase.capture() && resize_drag_active(cx) {
                            let _ = panel.update(cx, |panel, cx| panel.sync_visibility(cx));
                        }
                    }
                });
                window.on_mouse_event({
                    let panel = panel.clone();
                    // Capture, while the drag is still active: GPUI ends it
                    // after this event dispatches, and the render this asks
                    // for comes after that.
                    move |_: &MouseUpEvent, phase, _, cx| {
                        if !phase.capture() {
                            return;
                        }
                        let dragging = resize_drag_active(cx);
                        let _ = panel.update(cx, |panel, cx| {
                            if dragging || panel.backend.seam_pressed {
                                panel.backend.seam_pressed = false;
                                cx.notify();
                            }
                        });
                    }
                });
            },
        )
        .absolute()
        .size_0()
    }

    /// Get or lazily create the browser for one stable conversation key.
    fn ensure_webview(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Availability {
        let initial_url = self.store.read(cx).preview_url(key);
        self.lifecycle().update(cx, |lifecycle, cx| {
            lifecycle.ensure(key, initial_url.as_deref(), window, cx)
        })
    }

    pub(super) fn drop_webview(&mut self, key: &str, cx: &mut Context<Self>) {
        self.lifecycle()
            .update(cx, |lifecycle, cx| lifecycle.drop_view(key, cx));
        if self.mirrored.as_deref() == Some(key) {
            self.mirrored = None;
        }
    }

    pub(super) fn prune_deleted_webviews(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if self
            .mirrored
            .as_ref()
            .is_some_and(|key| !store.preview_key_live(key))
        {
            self.mirrored = None;
        }
        let store = self.store.clone();
        self.lifecycle().update(cx, |lifecycle, cx| {
            lifecycle.prune(|key, cx| store.read(cx).preview_key_live(key), cx)
        });
    }

    pub(super) fn navigate_backend(
        &mut self,
        key: &str,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lifecycle().update(cx, |lifecycle, cx| {
            lifecycle.navigate(key, url, window, cx);
        });
    }

    pub(super) fn history_controls(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use crate::material::toolbar_icon_button;
        let compact = self.window_state.read(cx).compact;
        vec![
            toolbar_icon_button(
                "preview-back",
                IconName::ArrowLeft,
                crate::tr!("preview.back"),
                compact,
            )
            .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx)))
            .into_any_element(),
            toolbar_icon_button(
                "preview-forward",
                IconName::ArrowRight,
                crate::tr!("preview.forward"),
                compact,
            )
            .on_click(cx.listener(|this, _, _, cx| this.go_forward(cx)))
            .into_any_element(),
            toolbar_icon_button(
                "preview-reload",
                IconName::Replace,
                crate::tr!("preview.reload"),
                compact,
            )
            .on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
            .into_any_element(),
        ]
    }

    fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.user_acted(cx);
        if let Some(id) = self.active_key(cx)
            && let Availability::Ready(view) = self.ensure_webview(&id, window, cx)
        {
            view.update(cx, |view, _| {
                if let Err(error) = view.back() {
                    log::debug!("preview: failed to navigate back during teardown: {error}");
                }
            });
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        self.user_acted(cx);
        if let Some(id) = self.active_key(cx)
            && let Some(view) = self.lifecycle().read(cx).ready_view(&id)
        {
            #[cfg(target_os = "android")]
            let forward = view.read(cx).raw().forward();
            #[cfg(not(target_os = "android"))]
            let forward = view.update(cx, |view, _| view.forward());
            if let Err(error) = forward {
                log::debug!("preview: failed to navigate forward: {error}");
            }
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.user_acted(cx);
        if let Some(id) = self.active_key(cx) {
            self.lifecycle().read(cx).reload(&id, cx);
        }
    }

    /// The host's dev servers for this panel's thread, as buttons that load
    /// one here: the row folds in once they are shown, and the servers that
    /// start and stop widen in and narrow away where they are.
    pub(super) fn render_port_row(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use crate::sizing::Sizable as _;
        use crate::widgets::button::Button;
        let servers = self
            .dev_servers
            .as_deref()
            .and_then(|session_id| self.store.read(cx).dev_servers(session_id, cx));
        let shown = crate::motion::shown("preview-dev-servers", servers, SHOW, window, cx)?;
        let compact = self.window_state.read(cx).compact;
        let gap = cx.theme().tokens.spacing.xs;
        let row = gpui_base::h_flex()
            .flex_none()
            .w_full()
            .px(gap)
            .pb(gap)
            .flex_wrap();
        if shown.value.is_empty() {
            return Some(
                shown.fold(
                    row.child(
                        div()
                            .px(gap)
                            .text_secondary(compact, cx)
                            .child(crate::tr!("preview.no_dev_servers")),
                    ),
                ),
            );
        }
        let servers = shown.value.iter().map(|server| {
            (
                gpui::SharedString::from(server.port.to_string()),
                server.clone(),
            )
        });
        let buttons =
            crate::motion::keyed_across("preview-dev-server-buttons", servers, ROWS, window, cx, {
                |_, server, _, cx| {
                    let url = format!("http://localhost:{}/", server.port);
                    let mut label = format!(":{} {}", server.port, server.process);
                    if let Some(dir) = server.cwd.as_deref().filter(|dir| !dir.is_empty()) {
                        label.push_str(&format!(" · {dir}"));
                    }
                    div()
                        .flex_none()
                        .pr(gap)
                        .child(
                            Button::new(("dev-server", server.port as usize))
                                .outline()
                                .small()
                                .compact()
                                .label(label)
                                .tooltip(url.clone())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if let Some(key) = this.active_key(cx) {
                                        this.navigate_here(&key, &url, window, cx);
                                    }
                                })),
                        )
                        .into_any_element()
                }
            })
            .render(row);
        Some(shown.fold(buttons))
    }

    pub(super) fn render_body(
        &mut self,
        active: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let placeholder = self.window_state.read(cx).compact
            && active.is_some_and(|key| {
                self.store.read(cx).preview_url(key).is_none()
                    && self.lifecycle().read(cx).ready_view(key).is_none()
            });
        #[cfg(target_os = "android")]
        if let Some(key) = active {
            let url = self
                .lifecycle()
                .read(cx)
                .ready_view(key)
                .map(|view| view.read(cx).url())
                .filter(|url| !url.is_empty());
            if let Some(url) = url
                && self.store.read(cx).preview_url(key).as_ref() != Some(&url)
            {
                self.store
                    .update(cx, |store, cx| store.set_preview_url(key, url.clone(), cx));
                self.url_input
                    .update(cx, |input, cx| input.set_value(url, window, cx));
            }
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Some(key) = active
            && let Some(url) = self.lifecycle().read(cx).remote_url(key)
            && self.store.read(cx).preview_url(key).as_ref() != Some(&url)
        {
            self.store
                .update(cx, |store, cx| store.set_preview_url(key, url.clone(), cx));
            self.url_input
                .update(cx, |input, cx| input.set_value(url, window, cx));
        }
        // The notes and the page each keep a layer, so one cross-fades into
        // the next; a page leaving is not drawn again, as its native view
        // fades out on its own once it is let go.
        let mut page: Option<AnyElement> = None;
        let state = match active {
            _ if placeholder => 0,
            Some(id) => match self.ensure_webview(id, window, cx) {
                Availability::Ready(view) => {
                    #[cfg(target_os = "ios")]
                    let view = super::ios::element(view);
                    #[cfg(target_os = "linux")]
                    let view = super::wayland::element(view, cx);
                    #[cfg(not(target_os = "android"))]
                    let margin = page_margin(cx);
                    #[cfg(target_os = "android")]
                    let margin = px(0.);
                    // gpui-webview has no corner radius or layer clip for
                    // the native view, so the page stays square inside the
                    // rounded frame and its corners cover the frame's.
                    #[cfg(target_os = "windows")]
                    let shown = view.clone();
                    let frame = div()
                        .size_full()
                        .rounded(crate::material::radius_card(cx))
                        .border_1()
                        .border_color(cx.theme().border)
                        .overflow_hidden()
                        .child(view);
                    // The page fills the panel or keeps a fixed canvas; going
                    // from one to the other its size glides, the native view
                    // following its bounds. A panel resized is followed at once.
                    let canvas = self.store.read(cx).preview_canvas(id);
                    let size = page_size(canvas, window, cx);
                    #[cfg(target_os = "windows")]
                    shown.read(cx).lead(size.ahead);
                    page = Some(
                        div()
                            .p(margin)
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .items_center()
                            .justify_center()
                            .child(size.measure_area(div().absolute().inset_0().into_any_element()))
                            .relative()
                            .child(
                                div()
                                    .flex_none()
                                    .map(|page| match size.size {
                                        Some(size) => page.w(size.width).h(size.height),
                                        None => page.size_full(),
                                    })
                                    .max_w_full()
                                    .max_h_full()
                                    .child(frame),
                            )
                            .into_any_element(),
                    );
                    4
                }
                #[cfg(any(target_os = "windows", target_os = "android"))]
                Availability::Starting => 1,
                Availability::Unavailable => 2,
            },
            None => 3,
        };
        let fade = crate::motion::CrossFade::new("preview-body", state, 5, CHANGE, window, cx);
        let mut layers = Vec::new();
        for layer in (0..5).filter(|layer| fade.shows(*layer)) {
            let content = match layer {
                0 => self.render_note(
                    IconName::Globe,
                    crate::tr!("preview.url_placeholder").into_owned(),
                    None,
                    cx,
                ),
                1 => self.render_note(
                    IconName::Globe,
                    crate::tr!("preview.starting").into_owned(),
                    None,
                    cx,
                ),
                2 => self.render_note(
                    IconName::TriangleAlert,
                    crate::tr!("preview.unavailable").into_owned(),
                    Some(
                        self.lifecycle()
                            .read(cx)
                            .unavailable_error()
                            .map(str::to_owned)
                            .unwrap_or_else(|| crate::tr!("preview.unavailable_hint").into_owned()),
                    ),
                    cx,
                ),
                3 => self.render_note(
                    IconName::Globe,
                    crate::tr!("preview.no_session").into_owned(),
                    None,
                    cx,
                ),
                _ => match page.take() {
                    Some(page) => page,
                    None => continue,
                },
            };
            layers.push((
                layer,
                v_flex().size_full().child(content).into_any_element(),
            ));
        }
        let body = fade
            .fill()
            .render(layers)
            .debug_selector(|| "preview-body".into())
            .flex_1()
            .min_h_0()
            .w_full()
            .into_any_element();
        // `ensure_webview` creates children hidden; make the owning
        // conversation visible only after the current layout owns it.
        self.sync_mounted_visibility(cx);
        // A page that failed to load says so in a banner that folds in over
        // it, and away once a load succeeds, drawn as it was.
        let error = active.and_then(|key| self.lifecycle().read(cx).load_error(key, cx));
        if let Some(shown) = crate::motion::shown("preview-load-error", error, SHOW, window, cx) {
            let error = &shown.value;
            let banner = div()
                .px(cx.theme().tokens.spacing.sm)
                .py(cx.theme().tokens.spacing.xs)
                .text_token(&cx.theme().tokens.typography.xs)
                .text_color(cx.theme().danger)
                .child(
                    crate::tr!(
                        "preview.load_error",
                        url = error.url,
                        message = error.message,
                        code = error.code
                    )
                    .into_owned(),
                );
            return v_flex()
                .size_full()
                .child(div().flex_none().child(shown.fold(banner)))
                .child(body)
                .into_any_element();
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if self.store.read(cx).is_remote() {
            return v_flex()
                .size_full()
                .child(
                    div()
                        .flex_none()
                        .px(cx.theme().tokens.spacing.sm)
                        .py(cx.theme().tokens.spacing.xs)
                        .text_secondary(false, cx)
                        .child(crate::tr!("preview.remote_forwarding").into_owned()),
                )
                .child(body)
                .into_any_element();
        }
        body
    }

    /// Resolve one automation op from the MCP server against the active WebView.
    /// Answers `reply` immediately for actions, or once the page agent or the
    /// engine has answered.
    pub fn handle_op(
        &mut self,
        session_id: String,
        op: PreviewRequest,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.routed_key(&session_id, cx);
        log::info!("preview: handling {} for session {session_id}", op.kind());

        // Gate on the Browser settings: a disabled browser rejects every op;
        // `allow_evaluate` gates only `preview_evaluate`.
        let browser = self.store.read(cx).preview_browser_settings();
        if !browser.enabled {
            let _ = reply.try_send(Err(crate::tr!("browser.disabled_error").into_owned()));
            return;
        }
        if matches!(&op, PreviewRequest::Evaluate { .. }) && !browser.allow_evaluate {
            let _ = reply.try_send(Err(
                crate::tr!("browser.evaluate_disabled_error").into_owned()
            ));
            return;
        }

        // The conversation may be one nobody is looking at: size its browser
        // as the panel would before the op drives the page. A resize only
        // changes that size, so it creates no browser.
        if !matches!(&op, PreviewRequest::Resize { .. }) {
            self.ensure_webview(&key, window, cx);
        }
        self.stage_hidden(&key, window, cx);

        match op {
            PreviewRequest::Open { url } => {
                self.store.update(cx, |store, cx| {
                    store.open_tab_for(&session_id, tcode_core::ui::RightTab::Preview, cx);
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
                self.loading_reply(&key, reply, cx);
            }
            PreviewRequest::Navigate { url } => {
                self.store.update(cx, |store, cx| {
                    store.open_tab_for(&session_id, tcode_core::ui::RightTab::Preview, cx);
                });
                self.navigate(&key, &url, window, cx);
                self.loading_reply(&key, reply, cx);
            }
            PreviewRequest::Status => self.status(&key, reply, window, cx),
            PreviewRequest::Snapshot => {
                self.agent_op(&key, "snapshot", serde_json::json!({}), reply, window, cx)
            }
            PreviewRequest::Evaluate { js: expr } => {
                self.eval_json(&key, &preview_mcp::page::evaluate(&expr), reply, window, cx)
            }
            PreviewRequest::Click { selector, r#ref } => self.agent_op(
                &key,
                "click",
                serde_json::json!({ "selector": selector, "ref": r#ref }),
                reply,
                window,
                cx,
            ),
            PreviewRequest::Type {
                selector,
                r#ref,
                text,
            } => self.agent_op(
                &key,
                "type",
                serde_json::json!({ "selector": selector, "ref": r#ref, "text": text }),
                reply,
                window,
                cx,
            ),
            PreviewRequest::Resize { width, height } => {
                self.store.update(cx, |store, cx| {
                    store.open_tab_for(&session_id, tcode_core::ui::RightTab::Preview, cx);
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
                self.stage_hidden(&key, window, cx);
                self.sync_visibility(cx);
                cx.notify();
                let _ = reply.try_send(Ok(PreviewResponse::Json(payload)));
            }
            // `key` is the routed conversation key; bind the keyboard key
            // apart so it cannot shadow it.
            PreviewRequest::Press {
                key: pressed,
                modifiers,
            } => self.agent_op(
                &key,
                "press",
                serde_json::json!({ "key": pressed, "modifiers": modifiers }),
                reply,
                window,
                cx,
            ),
            PreviewRequest::Scroll {
                delta_x,
                delta_y,
                selector,
                r#ref,
            } => self.agent_op(
                &key,
                "scroll",
                serde_json::json!({
                    "deltaX": delta_x,
                    "deltaY": delta_y,
                    "selector": selector,
                    "ref": r#ref,
                }),
                reply,
                window,
                cx,
            ),
            PreviewRequest::WaitFor {
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
            PreviewRequest::Screenshot { r#ref, full_page } => {
                self.screenshot(&key, r#ref, full_page, reply, window, cx)
            }
        }
    }

    /// Open and navigate answer once the load has begun; the next call that
    /// reads or acts on the page waits for its document.
    fn loading_reply(&mut self, key: &str, reply: ReplyTx, cx: &mut Context<Self>) {
        if let Some(error) = self
            .lifecycle()
            .read(cx)
            .unavailable_error()
            .map(str::to_string)
        {
            let _ = reply.try_send(Err(unavailable_message(&error)));
            return;
        }
        let payload = serde_json::json!({
            "ok": true,
            "url": self.store.read(cx).preview_url(key),
            "note": "the page is loading; the next preview call waits for it, and \
                     preview_status reports load_error when it failed to load",
        });
        let _ = reply.try_send(Ok(PreviewResponse::Json(payload)));
    }

    /// The browser for `key` exists, or `reply` has heard why not.
    fn ready_or_reply(
        &mut self,
        key: &str,
        reply: &ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.ensure_webview(key, window, cx) {
            Availability::Ready(_) => true,
            #[cfg(any(target_os = "windows", target_os = "android"))]
            Availability::Starting => {
                let _ = reply.try_send(Err(super::lifecycle::STARTING_MESSAGE.into()));
                false
            }
            Availability::Unavailable => {
                let error = self
                    .lifecycle()
                    .read(cx)
                    .unavailable_error()
                    .unwrap_or_default()
                    .to_string();
                let _ = reply.try_send(Err(unavailable_message(&error)));
                false
            }
        }
    }

    /// Run a page agent method and answer with its value, URLs as the agent
    /// asked for them.
    fn agent_op(
        &mut self,
        key: &str,
        method: &'static str,
        args: serde_json::Value,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ready_or_reply(key, &reply, window, cx) {
            return;
        }
        let answered = self
            .lifecycle()
            .update(cx, |lifecycle, cx| lifecycle.call(key, method, args, cx));
        self.answer_with(key, answered, reply, |_| {}, cx);
    }

    /// Forward a page's answer to `reply`, after `finish` adds to it.
    fn answer_with(
        &self,
        key: &str,
        answered: async_channel::Receiver<Result<serde_json::Value, String>>,
        reply: ReplyTx,
        finish: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let lifecycle = self.lifecycle().downgrade();
        let key = key.to_owned();
        cx.spawn(async move |_, cx| {
            // A caller that stops waiting lets the page's call go with it.
            let answer = futures_lite::future::or(async { Some(answered.recv().await) }, async {
                reply.closed().await;
                None
            });
            let Some(answer) = answer.await else {
                return;
            };
            let result = answer
                .unwrap_or_else(|_| Err("preview browser was closed".into()))
                .map(|mut value| {
                    if let Some(object) = value.as_object_mut() {
                        if let Some(actual) = object
                            .get("url")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                            && let Ok(logical) = lifecycle
                                .update(cx, |lifecycle, _| lifecycle.logical_url(&key, &actual))
                            && logical != actual
                        {
                            object.insert("actual_url".into(), actual.into());
                            object.insert("url".into(), logical.into());
                        }
                        finish(object);
                    }
                    PreviewResponse::Json(value)
                });
            let _ = reply.send(result).await;
        })
        .detach();
    }

    /// The page's URL, title and loading state, with this conversation's
    /// canvas, the platform's load error and what the console recorded
    /// since the page last navigated.
    fn status(&mut self, key: &str, reply: ReplyTx, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready_or_reply(key, &reply, window, cx) {
            return;
        }
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
        let lifecycle = self.lifecycle();
        // The page cannot see a failed navigation, so the agent would keep
        // describing whatever was on screen before it.
        let load_error = lifecycle.read(cx).load_error(key, cx);
        let summary = lifecycle.read(cx).page_summary(key);
        #[cfg(target_os = "android")]
        let native_metadata = lifecycle
            .read(cx)
            .ready_view(key)
            .map(|view| view.read(cx).metadata());
        let finish = move |object: &mut serde_json::Map<String, serde_json::Value>| {
            object.insert("canvas".into(), canvas);
            #[cfg(target_os = "android")]
            if let Some((url, title)) = native_metadata {
                object.insert("url".into(), url.into());
                object.insert("title".into(), title.into());
            }
            if let Some(load_error) = load_error {
                object.insert("load_error".into(), load_error.to_json());
            }
            if let Some(serde_json::Value::Object(summary)) = summary {
                object.extend(summary);
            }
        };
        let answered = if lifecycle.read(cx).page_settled(key) {
            lifecycle.update(cx, |lifecycle, cx| {
                lifecycle.call(key, "status", serde_json::json!({}), cx)
            })
        } else {
            // A document is on its way, or none has come: say so now rather
            // than wait for it.
            let (answer, answered) = async_channel::bounded(1);
            let url = self.store.read(cx).preview_url(key);
            let _ = answer.try_send(Ok(serde_json::json!({
                "url": url.as_deref().unwrap_or("about:blank"),
                "title": "",
                "loading": lifecycle.read(cx).page_loading(key),
            })));
            answered
        };
        self.answer_with(key, answered, reply, finish, cx);
    }

    /// Wait until the page meets every condition: the agent watches the DOM
    /// and the engine the navigations, and nothing polls.
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
        if !self.ready_or_reply(key, &reply, window, cx) {
            return;
        }
        let answered = self.lifecycle().update(cx, |lifecycle, cx| {
            lifecycle.wait(
                key,
                selector,
                text,
                url_includes,
                Duration::from_millis(timeout_ms),
                cx,
            )
        });
        self.answer_with(key, answered, reply, |_| {}, cx);
    }

    fn stage_hidden(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(not(target_os = "android"))]
        {
            let canvas = self
                .store
                .read(cx)
                .preview_canvas(key)
                .map(|(width, height)| gpui::size(px(width as f32), px(height as f32)));
            let fallback = window.viewport_size();
            self.lifecycle()
                .read(cx)
                .stage_hidden(key, canvas, fallback, cx);
        }
        #[cfg(target_os = "android")]
        let _ = (key, window, cx);
    }

    /// Evaluate in the page's own world once its document is settled.
    fn eval_json(
        &mut self,
        key: &str,
        script: &str,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial_url = self.store.read(cx).preview_url(key);
        self.lifecycle().update(cx, |lifecycle, cx| {
            lifecycle.evaluate_json(key, initial_url.as_deref(), script, reply, window, cx);
        });
    }

    /// A picture of the page: what is on screen, the element `reference`
    /// names, or the whole document.
    fn screenshot(
        &mut self,
        key: &str,
        reference: Option<String>,
        full_page: bool,
        reply: ReplyTx,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ready_or_reply(key, &reply, window, cx) {
            return;
        }
        let pictured = self.lifecycle().update(cx, |lifecycle, cx| {
            lifecycle.screenshot(key, reference, full_page, cx)
        });
        cx.spawn(async move |_, _| {
            let result = pictured
                .recv()
                .await
                .unwrap_or_else(|_| Err("preview browser was closed".into()));
            let _ = reply.send(result).await;
        })
        .detach();
    }
}

/// The size the page is drawn at this frame, and the area it is laid out in.
struct PageSize {
    size: Option<gpui::Size<Pixels>>,
    /// How much larger the page is about to be, per axis, while it glides.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    ahead: gpui::Size<Pixels>,
    area: Rc<std::cell::Cell<gpui::Size<Pixels>>>,
}

impl PageSize {
    /// An element over the page's area that measures it each frame.
    fn measure_area(&self, element: AnyElement) -> AnyElement {
        let area = self.area.clone();
        div()
            .absolute()
            .inset_0()
            .child(element)
            .child(
                gpui::canvas(move |bounds, _, _| area.set(bounds.size), |_, _, _, _| {})
                    .absolute()
                    .inset_0(),
            )
            .into_any_element()
    }
}

/// The page's size: a fixed `canvas` within the area, or the whole area,
/// gliding over [`GLIDE`] when the one changes to the other or the canvas
/// changes. gpui has no layout transition and the native view follows its
/// element's bounds, so the size is eased here; a panel resized is followed
/// at once. Before the area is known the page fills it.
fn page_size(canvas: Option<(u32, u32)>, window: &mut Window, cx: &mut App) -> PageSize {
    #[derive(Default)]
    struct Glide {
        canvas: Option<Option<(u32, u32)>>,
        from: Option<gpui::Size<Pixels>>,
        drawn: Option<gpui::Size<Pixels>>,
        started: Option<std::time::Instant>,
    }
    let area = window
        .use_keyed_state("preview-page-area", cx, |_, _| {
            Rc::new(std::cell::Cell::new(gpui::Size::default()))
        })
        .read(cx)
        .clone();
    let glide = window
        .use_keyed_state("preview-page-glide", cx, |_, _| {
            Rc::new(std::cell::RefCell::new(Glide::default()))
        })
        .read(cx)
        .clone();
    let room = area.get();
    if room.width <= px(0.) || room.height <= px(0.) {
        return PageSize {
            size: None,
            ahead: gpui::Size::default(),
            area,
        };
    }
    let target = match canvas {
        Some((width, height)) => gpui::size(
            px(width as f32).min(room.width),
            px(height as f32).min(room.height),
        ),
        None => room,
    };
    let now = cx.background_executor().now();
    let mut glide = glide.borrow_mut();
    if glide.canvas.is_some_and(|last| last != canvas) && !cx.reduce_motion() {
        glide.from = glide.drawn;
        glide.started = Some(now);
    }
    glide.canvas = Some(canvas);
    let size = match (glide.from, glide.started) {
        (Some(from), Some(started)) => {
            let progress =
                crate::motion::Curve::new(GLIDE).progress(now.saturating_duration_since(started));
            if progress >= 1. {
                glide.from = None;
                glide.started = None;
                target
            } else {
                window.request_animation_frame();
                gpui::size(
                    from.width + (target.width - from.width) * progress,
                    from.height + (target.height - from.height) * progress,
                )
            }
        }
        _ => target,
    };
    glide.drawn = Some(size);
    PageSize {
        size: Some(size),
        ahead: gpui::size(
            (target.width - size.width).max(px(0.)),
            (target.height - size.height).max(px(0.)),
        ),
        area,
    }
}
