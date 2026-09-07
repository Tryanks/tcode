//! One responsive shell, on every client.
//!
//! [`AppShell`] is the window: it owns navigation, the layout rule and the
//! back stack, and it survives everything below it. Beneath it sits at most one
//! *attachment* — a link to a host and the views over that host's workspace.
//! Switching hosts replaces the attachment; it does not replace the window's
//! navigation root, and resizing the window never touches the attachment at all.
//!
//! Layout is decided by one thing (`crate::window_seam`): the width the window
//! can actually lay content out in. Under 900px the shell is a navigation stack
//! — hosts, threads, thread, panel — and at or above it the desktop split.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use crate::overlay::{Notification, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use gpui::{
    AnyElement, AnyView, AnyWindowHandle, App, AppContext as _, ClipboardItem, Context, Div,
    ElementId, Entity, Global, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    ParentElement as _, Pixels, Render, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, WeakEntity, Window, actions, div, prelude::FluentBuilder as _,
    px,
};
use gpui_base::{
    NavMotion, NavOperation, NavStack, NavStackState, ResizableState, StyledExt as _, h_flex,
    h_resizable, motion::PresencePhase, motion::Transition, resizable_panel, v_flex,
};
use tcode_client::host::ClientHost;
use tcode_core::ui::RightTab;
use tcode_protocol::{RuntimeEffect, RuntimeNotification as RuntimeEvent, RuntimeOperationId};

use crate::attachment::{Attachment, LocalTransport, same_target};
use crate::chat::ChatView;
use crate::diff::DiffPanel;
use crate::icon::{Icon, IconName};
use crate::palette::CommandPalette;
use crate::preview_panel::PreviewPanel;
#[cfg(all(
    feature = "native-preview",
    any(target_os = "macos", target_os = "windows")
))]
use crate::preview_panel::lifecycle::BrowserLifecycle;
use crate::remote::{AttachmentTarget, RemotePanel};
use crate::runtime_event::{
    RuntimeEventSeverity, RuntimeToastDisposition, apply_runtime_effect, present_runtime_event,
    present_runtime_toast,
};
use crate::settings_page::SettingsPage;
use crate::sidebar::SessionsSidebar;
use crate::store::{StoreChange, TopicKind, WorkspaceStore};
use crate::toast::{RuntimeToastNotification, ToastAction, ToastId, ToastKind};
use crate::window_caption;
use crate::window_seam::WindowSeam;
use crate::window_state::{OpenThread, Route, WindowState};

actions!(tcode, [Quit, TogglePalette]);

/// Transient per-frame state backing [`window_drag_area`].
struct WindowDragState {
    should_move: bool,
}

impl Render for WindowDragState {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Whether this client's window can be dragged by its content at all. A phone
/// or browser surface has no movable window, so the handlers below are not just
/// useless there — they would arm on presses that belong to the content.
pub(crate) const WINDOW_DRAGGABLE: bool = cfg!(not(any(
    target_os = "ios",
    target_os = "android",
    target_family = "wasm"
)));

/// Make `el` a window-drag handle (the window has no separate titlebar, so the
/// column top rows are the drag surface). Mirrors gpui-component's `TitleBar`
/// mechanics: a press arms a move, the first drag calls `start_window_move`.
/// Child buttons that stop propagation on mouse-down won't arm a drag.
pub(crate) fn window_drag_area(
    id: impl Into<ElementId>,
    el: Div,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    if !WINDOW_DRAGGABLE {
        return el;
    }
    let state = window.use_keyed_state(id, cx, |_, _| WindowDragState { should_move: false });
    el.on_mouse_down_out(window.listener_for(&state, |state, _, _, _| {
        state.should_move = false;
    }))
    .on_mouse_down(
        MouseButton::Left,
        window.listener_for(&state, |state, event: &MouseDownEvent, window, cx| {
            // A titlebar press must never begin text selection; once
            // `start_window_move` swallows the mouse-up, a gesture would
            // otherwise remain active while the window is dragged.
            window.prevent_default();
            gpui_base::GlobalState::suppress_text_selection(cx);
            // Double-click zooms/maximizes the window like a native titlebar.
            if event.click_count >= 2 {
                state.should_move = false;
                window.titlebar_double_click();
            } else {
                state.should_move = true;
            }
        }),
    )
    .on_mouse_up(
        MouseButton::Left,
        window.listener_for(&state, |state, _, _, _| {
            state.should_move = false;
        }),
    )
    .on_mouse_move(window.listener_for(&state, |state, _, window, _| {
        if state.should_move {
            state.should_move = false;
            window.start_window_move();
        }
    }))
}

/// Where a compact window currently is. A wide window shows the whole
/// hierarchy at once and only uses this to remember where a narrowing window
/// should land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Destination {
    Hosts,
    Threads,
    Thread,
    /// The thread's terminal, diff/plan or preview, full width.
    Panel,
}

impl Destination {
    fn depth(self) -> usize {
        match self {
            Destination::Hosts => 1,
            Destination::Threads => 2,
            Destination::Thread => 3,
            Destination::Panel => 4,
        }
    }
}

/// One level of the compact [`NavStack`]. It owns nothing: it renders whichever
/// shell destination it stands for, so all four stay mounted while a push or
/// pop animates.
struct DestinationView {
    shell: WeakEntity<AppShell>,
    destination: Destination,
}

impl Render for DestinationView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let destination = self.destination;
        self.shell
            .upgrade()
            .map(|shell| {
                shell.update(cx, |shell, cx| match destination {
                    Destination::Hosts => shell.render_hosts_page(window, cx),
                    Destination::Threads => shell.render_threads_page(window, cx),
                    Destination::Thread => shell.render_thread_page(window, cx),
                    Destination::Panel => shell.render_panel_page(window, cx),
                })
            })
            .unwrap_or_else(|| div().into_any_element())
    }
}

/// The views and transport for one host. Everything here is replaced when the
/// window attaches somewhere else, and nothing here is touched by a resize.
struct ShellAttachment {
    link: Attachment,
    sidebar: Entity<SessionsSidebar>,
    chat: Entity<ChatView>,
    diff: Entity<DiffPanel>,
    preview: Entity<PreviewPanel>,
    settings_page: Entity<SettingsPage>,
    palette: Entity<CommandPalette>,
    /// The workspace split's state, owned here rather than left to the group's
    /// internal state so the right panel can be given a real width when it
    /// opens. A fresh panel's size starts at the group's 100px minimum and the
    /// group latches the *first* width it measures — which, for a panel that
    /// appears mid-session, is that minimum. Left alone, the diff panel opens
    /// pinned to its 320px floor.
    split: Entity<ResizableState>,
    /// The width the right panel opens at: the default until the user drags a
    /// handle, then whatever they chose (for this run).
    right_width: Rc<Cell<Pixels>>,
    /// Whether the open right panel has already been given its width.
    right_sized: bool,
    /// Stable expanded-sidebar width. The resizable component otherwise scales
    /// every panel proportionally when the window enters or leaves fullscreen —
    /// and a trip through the compact layout is such a resize, so this is also
    /// what restores the split when the window widens again.
    sidebar_width: Rc<Cell<Pixels>>,
    /// Keep restoring the sidebar width until the panel reports it, then stop
    /// so the restore never fights an in-progress drag.
    sidebar_restore_pending: bool,
    /// Collapsed-only overlay visibility. Purely transient and never persisted;
    /// expanded/non-workspace renders clear it synchronously.
    sidebar_overlay_visible: bool,
    /// Whether this attachment's host settings have been adopted once.
    adopted: bool,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// What bootstrap tells the shell that is not the window itself.
#[derive(Default)]
pub struct ShellSetup {
    /// This client's identity, saved hosts and preferences.
    pub client_host: Option<Rc<dyn ClientHost>>,
    /// How to reach a host running inside this process. A client that can never
    /// host has none.
    pub local: Option<LocalTransport>,
    /// Where to attach at launch; `None` opens on the hosts list.
    pub initial: Option<AttachmentTarget>,
    /// A browser link that could not be paired opens the normal form with the
    /// one-shot failure visible after its URL fragment has been cleared.
    pub initial_pairing_error: Option<String>,
    /// Wait for the first snapshots while attaching. Only desktop bootstrap
    /// asks for it, and only because it applies locale and theme from them
    /// before the first frame; a single-threaded client cannot wait at all.
    pub seed_blocking: bool,
}

pub struct AppShell {
    window_state: Entity<WindowState>,
    setup: ShellSetup,
    attachment: Option<ShellAttachment>,
    /// Saved hosts, discovery, pairing and certificate repair: the hosts
    /// destination, and the same panel Settings → Remote shows.
    hosts: Entity<RemotePanel>,
    nav: Entity<NavStackState>,
    pages: Vec<AnyView>,
    destination: Destination,
    /// Which surface the compact panel destination is showing.
    panel_shows_terminal: bool,
    operation_toasts: HashMap<RuntimeOperationId, ToastId>,
    next_toast_id: ToastId,
    /// Tracks the palette's open state across frames so it can be focused on the
    /// open transition.
    palette_was_open: bool,
    /// Viewport width last seen by render; a change arms the sidebar restore.
    last_viewport_width: Option<Pixels>,
    _subscriptions: Vec<Subscription>,
}

/// The right panel's default width (`docs/DESIGN.md`).
const RIGHT_PANEL_WIDTH: f32 = 560.;
const SIDEBAR_WIDTH: f32 = 255.;
/// Collapsed only: width of the window's left-edge activation region.
const SIDEBAR_HOVER_EDGE: f32 = 12.;
/// Compact push/pop duration.
const NAV_TRANSITION: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarHoverTransition {
    Trigger(bool),
    Overlay(bool),
}

/// Apply the asymmetric sibling-hover contract. The fixed trigger can open the
/// overlay but cannot close it; once mounted, the overlay owns closing itself.
fn next_sidebar_overlay_visibility(
    currently_visible: bool,
    transition: SidebarHoverTransition,
    collapsed: bool,
    route: Route,
    popover_open: bool,
) -> bool {
    if !collapsed || route != Route::Chat {
        return false;
    }

    match transition {
        SidebarHoverTransition::Trigger(true) | SidebarHoverTransition::Overlay(true) => true,
        SidebarHoverTransition::Trigger(false) => currently_visible,
        // A menu spawned from the sidebar (new-thread dropdown, row context
        // menu) is an occluding deferred layer: hovering it reads as leaving
        // the overlay. Treat an open popover's lifetime as continued hover;
        // the render pass reaps the overlay once the popover dismisses.
        SidebarHoverTransition::Overlay(false) => currently_visible && popover_open,
    }
}

impl AppShell {
    pub fn new(
        window_state: Entity<WindowState>,
        setup: ShellSetup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Seed the layout from the window before any child view exists, so the
        // first frame is already the right one rather than a wide split that
        // reflows on the second.
        let compact = crate::window_seam::window_is_compact(window, cx);
        window_state.update(cx, |state, cx| {
            state.set_compact(compact, cx);
        });
        let subscriptions = vec![
            cx.observe(&window_state, |_, _, cx| cx.notify()),
            cx.subscribe_in(&window_state, window, |this, _, _: &OpenThread, w, cx| {
                this.open_thread(w, cx);
            }),
            // The one width observer. It updates the layout only when the rule
            // flips; `render` keeps its own width tracking for the split.
            cx.observe_window_bounds(window, |this, window, cx| {
                this.sync_layout(window, cx);
            }),
        ];
        let hosts = cx.new(|cx| {
            let mut panel = RemotePanel::new(None, window, cx);
            panel.set_pairing_error(setup.initial_pairing_error.clone());
            panel
        });
        let mut shell = Self {
            window_state,
            attachment: None,
            hosts,
            nav: cx.new(|_| NavStackState::new()),
            pages: Vec::new(),
            destination: Destination::Hosts,
            panel_shows_terminal: false,
            operation_toasts: HashMap::new(),
            next_toast_id: 1,
            palette_was_open: false,
            last_viewport_width: None,
            _subscriptions: subscriptions,
            setup,
        };
        if let Some(target) = shell.setup.initial.take() {
            shell.attach(target, window, cx);
        }
        shell
    }

    pub fn window_state(&self) -> Entity<WindowState> {
        self.window_state.clone()
    }

    /// The workspace this window is attached to, if any.
    pub fn store(&self) -> Option<Entity<WorkspaceStore>> {
        self.attachment
            .as_ref()
            .map(|attachment| attachment.link.store.clone())
    }

    pub fn link(&self) -> Option<tcode_client::HostLink> {
        self.attachment
            .as_ref()
            .map(|attachment| attachment.link.link())
    }

    #[cfg(all(
        feature = "native-preview",
        any(target_os = "macos", target_os = "windows")
    ))]
    #[allow(private_interfaces)]
    #[doc(hidden)]
    pub fn preview_lifecycle(&self, cx: &App) -> Option<Entity<BrowserLifecycle>> {
        Some(self.attachment.as_ref()?.preview.read(cx).lifecycle())
    }
}

// ---------------------------------------------------------------------------
// Attachment lifecycle
// ---------------------------------------------------------------------------

impl AppShell {
    /// Point this window at `target`, replacing the attachment beneath the
    /// navigation root. The window, its stack and its overlays stay put.
    pub fn switch_to(
        &mut self,
        target: AttachmentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .attachment
            .as_ref()
            .is_some_and(|current| same_target(&current.link.target, &target))
        {
            return;
        }
        // Never drop a working attachment for one this client cannot open.
        let reachable = match &target {
            AttachmentTarget::Local => self.setup.local.is_some(),
            AttachmentTarget::Remote(_) => self.setup.client_host.is_some(),
        };
        if !reachable {
            log::error!("this client cannot reach {target:?}");
            return;
        }
        self.attach(target, window, cx);
    }

    /// Leave the current host without opening another one. The host keeps
    /// running; only this window's link to it closes.
    pub fn detach(&mut self, cx: &mut Context<Self>) {
        let Some(attachment) = self.attachment.take() else {
            return;
        };
        attachment.link.close(cx).close();
        self.hosts.update(cx, |hosts, cx| hosts.set_store(None, cx));
        self.destination = Destination::Hosts;
        self.sync_nav(NavMotion::Animated, cx);
        cx.notify();
    }

    fn attach(&mut self, target: AttachmentTarget, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(old) = self.attachment.take() {
            old.link.close(cx).close();
        }
        let Some(link) = Attachment::open(
            target,
            self.setup.local.as_ref(),
            self.setup.client_host.clone(),
            self.setup.seed_blocking,
            cx,
        ) else {
            log::error!("this client cannot reach the requested host");
            self.destination = Destination::Hosts;
            self.sync_nav(NavMotion::Immediate, cx);
            cx.notify();
            return;
        };
        if let Some(host) = &self.setup.client_host {
            host.set_last_host_id(match &link.target {
                AttachmentTarget::Local => None,
                AttachmentTarget::Remote(host) => Some(host.host_id.as_str()),
            });
        }

        let store = link.store.clone();
        window.set_window_title(&store.read(cx).shell_window_title());
        let remote = matches!(link.target, AttachmentTarget::Remote(_));
        let preview =
            cx.new(|cx| PreviewPanel::new(store.clone(), self.window_state.clone(), window, cx));
        let sidebar = cx.new(|cx| {
            let mut sidebar = SessionsSidebar::new(store.clone(), self.window_state.clone(), cx);
            // A remote workspace has nothing to show until its first index
            // snapshot arrives; a local one is already seeded.
            sidebar.set_loading(remote, cx);
            sidebar
        });
        let subscriptions = vec![
            cx.observe_in(&store, window, move |_, store, window, cx| {
                window.set_window_title(&store.read(cx).shell_window_title());
                cx.notify();
            }),
            cx.subscribe_in(&store, window, |this, _, event: &RuntimeEvent, w, cx| {
                this.present_app_event(event, w, cx);
            }),
            cx.subscribe_in(
                &store,
                window,
                |this, _, change: &StoreChange, window, cx| {
                    if change.topic == TopicKind::Index
                        && let Some(attachment) = &this.attachment
                    {
                        attachment
                            .sidebar
                            .update(cx, |sidebar, cx| sidebar.set_loading(false, cx));
                    }
                    if change.topic == TopicKind::Settings {
                        this.adopt_host_settings(window, cx);
                    }
                    cx.notify();
                },
            ),
        ];
        // Every client pumps preview requests: one without a backend still has
        // to answer `unsupported` for anything that reaches it.
        let requests = store.read(cx).remote_preview_requests();
        let preview_pump = {
            let preview = preview.clone();
            let store = store.downgrade();
            cx.spawn_in(window, async move |_, cx| {
                while let Ok(envelope) = requests.recv().await {
                    let tcode_protocol::ServerEvent::PreviewRequest {
                        request_id,
                        session_id,
                        request,
                    } = envelope.event
                    else {
                        continue;
                    };
                    let (reply, receiver) = async_channel::bounded(1);
                    if preview
                        .update_in(cx, |panel, window, cx| {
                            panel.handle_op(session_id, request, reply, window, cx)
                        })
                        .is_err()
                    {
                        break;
                    }
                    // Each operation waits independently: a slow wait_for must
                    // not block a second client's navigation or a screenshot.
                    let store = store.clone();
                    cx.spawn(async move |cx| {
                        let response = receiver
                            .recv()
                            .await
                            .unwrap_or_else(|_| Err("preview panel dropped request".into()));
                        let _ =
                            store.update(cx, |store, _| store.preview_reply(request_id, response));
                    })
                    .detach();
                }
            })
        };

        self.attachment = Some(ShellAttachment {
            chat: cx.new(|cx| ChatView::new(store.clone(), self.window_state.clone(), window, cx)),
            diff: cx.new(|cx| DiffPanel::new(store.clone(), self.window_state.clone(), cx)),
            settings_page: cx
                .new(|cx| SettingsPage::new(store.clone(), self.window_state.clone(), window, cx)),
            palette: cx.new(|cx| {
                CommandPalette::new(store.clone(), self.window_state.clone(), window, cx)
            }),
            preview,
            sidebar,
            split: cx.new(|_| ResizableState::default()),
            right_width: Rc::new(Cell::new(px(RIGHT_PANEL_WIDTH))),
            right_sized: false,
            sidebar_width: Rc::new(Cell::new(px(SIDEBAR_WIDTH))),
            sidebar_restore_pending: false,
            sidebar_overlay_visible: false,
            adopted: false,
            link,
            _tasks: vec![preview_pump],
            _subscriptions: subscriptions,
        });
        self.hosts
            .update(cx, |hosts, cx| hosts.set_store(Some(store), cx));
        self.adopt_host_settings(window, cx);
        self.destination = Destination::Threads;
        self.sync_nav(NavMotion::Animated, cx);
        cx.notify();
    }

    /// Take the attached host's language, theme and sidebar state once its
    /// settings exist. Client preferences already override them inside the
    /// store, so this is the same value the settings page shows.
    fn adopt_host_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attachment) = &mut self.attachment else {
            return;
        };
        let store = attachment.link.store.read(cx);
        if attachment.adopted || !store.settings_hydrated() {
            return;
        }
        attachment.adopted = true;
        let settings = store.settings();
        crate::settings::apply_locale(settings.language.as_deref());
        crate::settings_page::apply_theme(settings.theme_mode, window, cx);
        let collapsed = settings.sidebar_collapsed;
        self.window_state.update(cx, |state, cx| {
            state.sidebar_collapsed = collapsed;
            cx.notify();
        });
    }
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

impl AppShell {
    fn compact(&self, cx: &App) -> bool {
        self.window_state.read(cx).compact
    }

    /// Recompute the layout from the window. Only a flip does anything, and a
    /// flip reconciles navigation to where the state already is — it is not a
    /// Back gesture and must not animate like one.
    fn sync_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let flipped = self
            .window_state
            .update(cx, |state, cx| state.set_compact(compact, cx));
        if flipped {
            self.destination = self.logical_destination(cx);
            self.sync_nav(NavMotion::Immediate, cx);
            cx.notify();
        }
    }

    /// Where a compact window belongs given the state it is actually in.
    fn logical_destination(&self, cx: &App) -> Destination {
        let Some(attachment) = &self.attachment else {
            return Destination::Hosts;
        };
        // Selection, not the host's answer about it: picking a thread navigates
        // straight away rather than after a round trip.
        let has_thread = attachment.link.store.read(cx).active_session_id().is_some();
        match self.destination {
            Destination::Hosts => Destination::Hosts,
            Destination::Panel if has_thread => Destination::Panel,
            _ if has_thread => Destination::Thread,
            _ => Destination::Threads,
        }
    }

    fn ensure_pages(&mut self, cx: &mut Context<Self>) {
        if !self.pages.is_empty() {
            return;
        }
        let shell = cx.entity().downgrade();
        self.pages = [
            Destination::Hosts,
            Destination::Threads,
            Destination::Thread,
            Destination::Panel,
        ]
        .map(|destination| {
            AnyView::from(cx.new(|_| DestinationView {
                shell: shell.clone(),
                destination,
            }))
        })
        .to_vec();
    }

    /// Drive the stack to `self.destination` by the difference in depth, so a
    /// step in either direction animates and a reconciliation does not.
    fn sync_nav(&mut self, motion: NavMotion, cx: &mut Context<Self>) {
        self.ensure_pages(cx);
        let target = self.destination.depth();
        let pages = self.pages.clone();
        self.nav.update(cx, |nav, cx| {
            while nav.depth() > target {
                if nav.pop(motion, cx).is_none() {
                    break;
                }
            }
            while nav.depth() < target {
                nav.push(pages[nav.depth()].clone(), motion, cx);
            }
        });
    }

    fn go(&mut self, destination: Destination, cx: &mut Context<Self>) {
        if self.destination == destination {
            return;
        }
        self.destination = destination;
        self.sync_nav(NavMotion::Animated, cx);
        cx.notify();
    }

    /// "Show me this thread." A wide window already does; a compact one pushes.
    fn open_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compact(cx) {
            self.go(Destination::Thread, cx);
        }
        if let Some(attachment) = &self.attachment {
            attachment
                .chat
                .update(cx, |chat, cx| chat.focus_composer(window, cx));
        }
    }

    /// Answer a platform Back gesture (or a Back control). `true` means it was
    /// consumed; `false` only at the root, where the platform closes the app.
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // A composition or a software keyboard is on top of everything.
        if WindowSeam::current(cx).insets().ime.bottom > px(0.) {
            window.blur(cx);
            return true;
        }
        // Dialogs, menus and popovers are deferred layers that own Cancel.
        if gpui_base::GlobalState::is_in_deferred_context(cx) {
            window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
            return true;
        }
        if self.window_state.read(cx).palette_open {
            self.window_state
                .update(cx, |state, cx| state.close_palette(cx));
            return true;
        }
        if self.window_state.read(cx).route == Route::Settings {
            self.window_state
                .update(cx, |state, cx| state.close_settings(cx));
            return true;
        }
        if !self.compact(cx) {
            return false;
        }
        match self.destination {
            Destination::Panel => {
                self.go(Destination::Thread, cx);
                true
            }
            // Leaving a thread keeps the connection: only the hosts list is a
            // decision about which host this window is attached to.
            Destination::Thread => {
                self.go(Destination::Threads, cx);
                true
            }
            Destination::Threads => {
                self.detach(cx);
                true
            }
            Destination::Hosts => false,
        }
    }
}

/// The window whose shell answers the platform's Back gesture. Window-scoped on
/// purpose: a first-window lookup answers for whichever window happens to be
/// first, which is not the one the gesture arrived at.
struct ShellBackTarget {
    window: AnyWindowHandle,
    shell: WeakEntity<AppShell>,
}

impl Global for ShellBackTarget {}

pub(crate) fn set_back_target(window: AnyWindowHandle, shell: &Entity<AppShell>, cx: &mut App) {
    cx.set_global(ShellBackTarget {
        window,
        shell: shell.downgrade(),
    });
}

/// Route a platform Back gesture into the shell that owns the window.
pub fn handle_back(cx: &mut App) -> bool {
    let Some(target) = cx.try_global::<ShellBackTarget>() else {
        return false;
    };
    let (window, shell) = (target.window, target.shell.clone());
    window
        .update(cx, |_, window, cx| {
            shell
                .update(cx, |shell, cx| shell.back(window, cx))
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Toasts and runtime events
// ---------------------------------------------------------------------------

impl AppShell {
    fn present_app_event(
        &mut self,
        event: &RuntimeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let toast = match event {
            RuntimeEvent::Effect(RuntimeEffect::ApplyLocale { .. }) => {
                let Some(attachment) = &self.attachment else {
                    return;
                };
                let language = attachment.link.store.read(cx).settings().language;
                apply_runtime_effect(&RuntimeEffect::ApplyLocale { language });
                cx.notify();
                return;
            }
            RuntimeEvent::Effect(RuntimeEffect::CopyToClipboard { text }) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                return;
            }
            RuntimeEvent::Error(_) | RuntimeEvent::Notice(_) => {
                let presented = present_runtime_event(event);
                let notification = match presented.severity {
                    RuntimeEventSeverity::Error => Notification::error(presented.message),
                    RuntimeEventSeverity::Warning => Notification::warning(presented.message),
                    RuntimeEventSeverity::Success => Notification::success(presented.message),
                };
                window.push_notification(notification, cx);
                return;
            }
            RuntimeEvent::Toast(toast) => toast,
        };

        let presented = present_runtime_toast(toast);
        let toast_id = match presented.disposition {
            RuntimeToastDisposition::Push => self.take_toast_id(),
            RuntimeToastDisposition::Start(operation) => {
                let toast_id = self.take_toast_id();
                self.operation_toasts.insert(operation, toast_id);
                toast_id
            }
            RuntimeToastDisposition::Finish(operation) => self
                .operation_toasts
                .remove(&operation)
                .unwrap_or_else(|| self.take_toast_id()),
        };

        let action = presented.retry.and_then(|request| {
            let store = self.store()?;
            Some(ToastAction {
                label: crate::tr!("git.toast.retry").into_owned().into(),
                handler: Rc::new(move |window, cx| {
                    window.remove_notification1::<RuntimeToastNotification>(toast_id as usize, cx);
                    let request = request.clone();
                    store.update(cx, |store, _cx| {
                        store.retry_git_action(request);
                    });
                }),
            })
        });
        self.show_toast(
            toast_id,
            presented.kind,
            (presented.title, presented.detail),
            action,
            window,
            cx,
        );
    }

    fn take_toast_id(&mut self) -> ToastId {
        let id = self.next_toast_id;
        self.next_toast_id += 1;
        id
    }

    fn show_toast(
        &mut self,
        id: ToastId,
        kind: ToastKind,
        content: (String, Option<String>),
        action: Option<ToastAction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, detail) = content;
        window.push_notification(
            crate::toast::notification(id, kind, title, detail.map(Into::into), action),
            cx,
        );

        let delay = match kind {
            ToastKind::Success | ToastKind::Info => Some(Duration::from_secs(4)),
            ToastKind::Warning => Some(Duration::from_secs(6)),
            ToastKind::Error | ToastKind::Loading => None,
        };
        if let Some(delay) = delay {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(delay).await;
                _ = this.update_in(cx, |_, window, cx| {
                    window.remove_notification1::<RuntimeToastNotification>(id as usize, cx);
                });
            })
            .detach();
        }
    }

    fn on_toggle_palette(
        &mut self,
        _: &TogglePalette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window_state
            .update(cx, |state, cx| state.toggle_palette(cx));
    }
}

// ---------------------------------------------------------------------------
// Compact chrome
// ---------------------------------------------------------------------------

/// Left inset so the nav bar's back control clears the native macOS traffic
/// lights, which a compact *desktop* window still draws over this strip.
const TRAFFIC_LIGHT_INSET: f32 = 80.;

/// Navigation header with a centered title and at most two trailing actions.
fn nav_bar(
    back: Option<AnyElement>,
    title: SharedString,
    subtitle: Option<AnyElement>,
    actions: Vec<AnyElement>,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    debug_assert!(
        actions.len() <= 2,
        "navigation bar allows at most two trailing actions"
    );
    // A compact window on the desktop is still a window: it keeps the platform's
    // own controls out of the content and stays draggable by its top strip.
    let clears_traffic_lights = cfg!(target_os = "macos") && !window.is_fullscreen();
    let hosts_caption = window_caption::compact_hosts_caption();
    let leading = h_flex()
        .absolute()
        .inset_0()
        .px(px(4.))
        .when(clears_traffic_lights, |row| row.pl(px(TRAFFIC_LIGHT_INSET)))
        .items_center()
        .children(back)
        .child(window_caption::drag_region(div().flex_1().h_full()))
        .children(actions)
        .children(hosts_caption.then(|| window_caption::caption_controls(window, cx)));
    v_flex()
        .flex_none()
        .w_full()
        .bg(crate::material::content_surface(cx))
        .child(
            window_drag_area("compact-nav-drag", div(), window, cx)
                .relative()
                .w_full()
                .h(px(52.))
                .child(
                    // Centered on the bar itself, not between the buttons, so
                    // the title does not drift with the back button's width.
                    v_flex()
                        .absolute()
                        .inset_0()
                        .px(px(96.))
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .max_w_full()
                                .min_w_0()
                                .truncate()
                                .text_size(px(17.))
                                .line_height(px(22.))
                                .font_semibold()
                                .child(title),
                        )
                        .children(subtitle),
                )
                .child(leading),
        )
        .child(crate::material::faded_hairline(cx))
}

/// Back control labelled with the parent destination.
fn back_button(id: &'static str, parent: SharedString, cx: &App) -> gpui::Stateful<Div> {
    crate::material::accessible_clickable(h_flex(), id, Role::Button, parent.clone(), cx)
        .flex_none()
        .h(px(44.))
        .pl(px(4.))
        .pr(px(10.))
        .gap(px(2.))
        .items_center()
        .rounded(px(12.))
        .cursor_pointer()
        .text_color(cx.theme().foreground)
        .active(|s| s.bg(cx.theme().foreground.opacity(0.08)))
        .child(
            Icon::empty()
                .path("icons/chevron-left.svg")
                .size(px(20.))
                .flex_none(),
        )
        .child(
            div()
                .max_w(px(96.))
                .min_w_0()
                .truncate()
                .text_size(px(15.))
                .line_height(px(20.))
                .child(parent),
        )
}

/// A nav-bar icon button: 44×44 touch target, a 20pt stroke icon in
/// `foreground`. Blue is reserved for primary actions and live state.
fn nav_icon_button(
    id: impl Into<ElementId>,
    aria_label: impl Into<SharedString>,
    icon: IconName,
    enabled: bool,
    cx: &App,
) -> gpui::Stateful<Div> {
    let theme = cx.theme();
    crate::material::accessible_clickable(div(), id, Role::Button, aria_label.into(), cx)
        .size(px(44.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(12.))
        .cursor_pointer()
        .text_color(if enabled {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .active(|s| s.bg(theme.foreground.opacity(0.08)))
        .child(Icon::new(icon).size(px(20.)))
}

fn compact_label(key: &str) -> String {
    crate::tr!(format!("mobile.{key}")).into_owned()
}

impl AppShell {
    fn render_hosts_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(
                None,
                compact_label("hosts").into(),
                None,
                vec![],
                window,
                cx,
            ))
            .child(
                div()
                    .id("compact-hosts")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(div().p(px(16.)).child(self.hosts.clone())),
            )
            .into_any_element()
    }

    fn render_threads_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let store = attachment.link.store.read(cx);
        let title = store
            .remote_host_name()
            .map(SharedString::from)
            .unwrap_or_else(|| crate::tr!("remote.connect.local").into_owned().into());
        let projects = store.projects();
        let sidebar = attachment.sidebar.clone();
        let actions = vec![
            nav_icon_button(
                "compact-new-thread",
                compact_label("new_thread"),
                IconName::Plus,
                !projects.is_empty(),
                cx,
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.start_thread(window, cx);
            }))
            .into_any_element(),
            nav_icon_button(
                "compact-settings",
                compact_label("settings"),
                IconName::Settings,
                true,
                cx,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.open_settings(cx));
            }))
            .into_any_element(),
        ];
        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(
                Some(
                    back_button("compact-back-hosts", compact_label("hosts").into(), cx)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.back(window, cx);
                        }))
                        .into_any_element(),
                ),
                title,
                None,
                actions,
                window,
                cx,
            ))
            .children(self.render_connection_banner(cx))
            .child(div().flex_1().min_h_0().child(sidebar))
            .into_any_element()
    }

    /// New thread: one project starts a draft directly, several go through the
    /// palette, which already owns "new thread in <project>" and can search.
    fn start_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attachment) = &self.attachment else {
            return;
        };
        let projects = attachment.link.store.read(cx).projects();
        let Some(project) = projects.first().cloned().filter(|_| projects.len() == 1) else {
            self.window_state
                .update(cx, |state, cx| state.open_palette(cx));
            return;
        };
        attachment.link.store.update(cx, |store, cx| {
            store.start_draft(project.id.clone(), project.root.clone(), cx)
        });
        self.open_thread(window, cx);
    }

    fn render_thread_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let store = attachment.link.store.read(cx);
        let active = store.chat_active_session();
        let project = active
            .as_ref()
            .and_then(|(_, cwd, _)| {
                store
                    .projects()
                    .into_iter()
                    .find(|project| project.root == *cwd)
            })
            .map(|project| project.name)
            .unwrap_or_default();
        let title = active
            .map(|(title, _, draft)| {
                if draft {
                    compact_label("new_thread")
                } else {
                    title
                }
            })
            .unwrap_or_else(|| compact_label("new_thread"));
        let chat = attachment.chat.clone();
        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(
                Some(
                    back_button("compact-back-threads", compact_label("threads").into(), cx)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.back(window, cx);
                        }))
                        .into_any_element(),
                ),
                title.into(),
                (!project.is_empty()).then(|| {
                    div()
                        .max_w_full()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.))
                        .line_height(px(18.))
                        .text_color(cx.theme().muted_foreground)
                        .child(project)
                        .into_any_element()
                }),
                vec![
                    nav_icon_button(
                        "compact-panels",
                        crate::tr!("chat.panels").into_owned(),
                        IconName::PanelRight,
                        true,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_panels(cx);
                    }))
                    .into_any_element(),
                ],
                window,
                cx,
            ))
            .children(self.render_connection_banner(cx))
            .child(div().flex_1().min_h_0().child(chat))
            .into_any_element()
    }

    /// The terminal, diff/plan and preview at full width. They are the same
    /// entities the wide layout puts in the split — a compact window has no
    /// room beside the timeline, not less of a product.
    fn render_panel_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let store = attachment.link.store.read(cx);
        let panel = store.panel_state();
        let parent = store
            .chat_active_session()
            .map(|(title, _, draft)| {
                if draft {
                    compact_label("new_thread")
                } else {
                    title
                }
            })
            .unwrap_or_else(|| compact_label("threads"));
        let terminal = self.panel_shows_terminal;
        let mut segments = crate::material::segmented_track("compact-panel-track", cx);
        for (id, label, selected) in [
            (
                "terminal",
                crate::tr!("terminal.title").into_owned(),
                terminal,
            ),
            (
                "diff",
                crate::tr!("diff.title").into_owned(),
                !terminal && panel.right_tab == RightTab::Diff,
            ),
            (
                "plan",
                crate::tr!("plan.tab_plan").into_owned(),
                !terminal && panel.right_tab == RightTab::Plan,
            ),
            (
                "preview",
                crate::tr!("preview.title").into_owned(),
                !terminal && panel.right_tab == RightTab::Preview,
            ),
        ] {
            segments = segments.child(
                crate::material::segment(
                    SharedString::from(format!("compact-panel-{id}")),
                    label,
                    selected,
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.show_panel(id, cx))),
            );
        }

        let body: AnyElement = if terminal {
            attachment
                .chat
                .read(cx)
                .terminal_drawer()
                .into_any_element()
        } else if panel.right_tab == RightTab::Preview {
            attachment.preview.clone().into_any_element()
        } else {
            attachment.diff.clone().into_any_element()
        };
        // The drawer sizes its grid from the space it is actually given.
        if terminal {
            let size = window.viewport_size();
            let drawer = attachment.chat.read(cx).terminal_drawer();
            let (width, height) = (f32::from(size.width), f32::from(size.height) - 110.);
            if !drawer.read(cx).is_size(width, height) {
                drawer.update(cx, |drawer, cx| drawer.resize(width, height, cx));
            }
        }

        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .child(nav_bar(
                Some(
                    back_button("compact-back-thread", parent.into(), cx)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.back(window, cx);
                        }))
                        .into_any_element(),
                ),
                crate::tr!("chat.panels").into_owned().into(),
                None,
                vec![],
                window,
                cx,
            ))
            .child(div().flex_none().px(px(16.)).py(px(8.)).child(segments))
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    /// Push the panel destination with something actually on it.
    fn open_panels(&mut self, cx: &mut Context<Self>) {
        let showing = self.attachment.as_ref().is_some_and(|attachment| {
            let panel = attachment.link.store.read(cx).panel_state();
            if self.panel_shows_terminal {
                panel.terminal_open
            } else {
                panel.right_panel_open
            }
        });
        if !showing {
            self.show_panel(
                if self.panel_shows_terminal {
                    "terminal"
                } else {
                    "diff"
                },
                cx,
            );
        }
        self.go(Destination::Panel, cx);
    }

    fn show_panel(&mut self, id: &'static str, cx: &mut Context<Self>) {
        let Some(attachment) = &self.attachment else {
            return;
        };
        let store = attachment.link.store.clone();
        let panel = store.read(cx).panel_state();
        self.panel_shows_terminal = id == "terminal";
        store.update(cx, |store, cx| match id {
            "terminal" => {
                if !panel.terminal_open {
                    store.toggle_terminal_panel(cx);
                }
            }
            tab => {
                let tab = match tab {
                    "plan" => RightTab::Plan,
                    "preview" => RightTab::Preview,
                    _ => RightTab::Diff,
                };
                if !(panel.right_panel_open && panel.right_tab == tab) {
                    match tab {
                        RightTab::Plan => store.toggle_plan_panel(cx),
                        RightTab::Preview => store.toggle_preview_panel(cx),
                        _ => store.toggle_diff_panel(cx),
                    }
                }
            }
        });
        cx.notify();
    }

    fn render_compact(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_pages(cx);
        if self.nav.read(cx).is_empty() {
            self.sync_nav(NavMotion::Immediate, cx);
        }
        let width = window.viewport_size().width;
        let palette_open = self.window_state.read(cx).palette_open;
        // 200ms lateral push/pop. GPUI has no paint transform, so each page is
        // offset by its own insets; setting `left` and `right` in opposite
        // directions slides it without resizing it.
        let stack = NavStack::new(&self.nav)
            .size_full()
            .overflow_hidden()
            .transition(Transition::new(NAV_TRANSITION))
            .item(move |page, _, _| {
                let rest = 1. - page.progress();
                let (offset, opacity) = match (page.phase(), page.operation()) {
                    (PresencePhase::Entering, Some(NavOperation::Pop)) => {
                        (width * -0.25 * rest, 1.)
                    }
                    (PresencePhase::Entering, Some(_)) => (width * rest, page.progress()),
                    (PresencePhase::Exiting, Some(NavOperation::Pop)) => {
                        (width * page.progress(), 1.)
                    }
                    (PresencePhase::Exiting, Some(_)) => (width * -0.25 * page.progress(), 1.),
                    _ => (px(0.), 1.),
                };
                page.left(offset)
                    .right(px(0.) - offset)
                    .opacity(opacity)
                    .into_any_element()
            });
        // Settings is a full-window route at every width; compact renders it as
        // its own section list (see SettingsPage).
        let settings = (self.window_state.read(cx).route == Route::Settings)
            .then(|| {
                self.attachment
                    .as_ref()
                    .map(|attachment| attachment.settings_page.clone())
            })
            .flatten();
        v_flex()
            .id("app-shell")
            .relative()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .text_color(cx.theme().foreground)
            .font_family(cx.theme().font_family.clone())
            .text_size(px(16.))
            .line_height(px(22.))
            .on_action(cx.listener(Self::on_toggle_palette))
            .child(gpui_base::TextSelectionLayer)
            .child(match settings {
                Some(page) => div().size_full().child(page).into_any_element(),
                None => stack.into_any_element(),
            })
            .when(palette_open, |el| {
                el.children(
                    self.attachment
                        .as_ref()
                        .map(|attachment| attachment.palette.clone()),
                )
            })
            .into_any_element()
    }

    /// Constrain the shell to the window's safe content rectangle. Backgrounds
    /// paint edge to edge — the surface below is the window's — and interactive
    /// content is inset exactly once, at both widths. A window the system does
    /// not occlude gets no wrapper at all.
    fn within_seam(&self, body: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let seam = WindowSeam::current(cx).content_insets();
        if seam == gpui::Edges::default() {
            return body;
        }
        div()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .pt(seam.top)
            .pb(seam.bottom)
            .pl(seam.left)
            .pr(seam.right)
            .child(body)
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Wide layout
// ---------------------------------------------------------------------------

impl AppShell {
    /// A slim status bar above the chat column, shown only over a remote link
    /// that is not currently connected.
    fn render_connection_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.attachment.as_ref()?.link.store.read(cx);
        let host = store.remote_host_name()?;
        let (text, accent) = match store.connection_state() {
            tcode_client::ConnectionState::Connected => return None,
            tcode_client::ConnectionState::Reconnecting { attempt } => (
                crate::tr!("remote.banner.reconnecting", host = host, attempt = attempt)
                    .into_owned(),
                cx.theme().warning,
            ),
            tcode_client::ConnectionState::Offline => (
                crate::tr!("remote.banner.offline", host = host).into_owned(),
                cx.theme().danger,
            ),
        };
        let reconnecting = matches!(
            store.connection_state(),
            tcode_client::ConnectionState::Reconnecting { .. }
        );
        Some(
            h_flex()
                .flex_none()
                .w_full()
                .h(px(28.))
                .px_3()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(cx.theme().border)
                .bg(accent.opacity(0.12))
                .text_size(px(12.))
                .text_color(cx.theme().foreground)
                .when(reconnecting, |bar| {
                    bar.child({
                        use crate::sizing::Sizable as _;
                        crate::widgets::spinner::Spinner::new().xsmall()
                    })
                })
                .child(text)
                .into_any_element(),
        )
    }

    fn render_wide(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let route = self.window_state.read(cx).route;
        let palette_open = self.window_state.read(cx).palette_open;
        let fullscreen = window.is_fullscreen();
        let Some(attachment) = &mut self.attachment else {
            // No host: the hosts list is the whole window.
            return div()
                .id("app-shell")
                .size_full()
                .bg(crate::material::opaque_canvas(cx))
                .text_color(cx.theme().foreground)
                .child(gpui_base::TextSelectionLayer)
                .child(
                    div()
                        .id("hosts")
                        .size_full()
                        .overflow_y_scroll()
                        .child(div().p_6().child(self.hosts.clone())),
                )
                .into_any_element();
        };
        let collapsed = self.window_state.read(cx).sidebar_collapsed;
        // The overlay is workspace-only transient state. Clear it synchronously
        // on route/expanded transitions rather than waiting for pointer input.
        if !collapsed || route != Route::Chat {
            attachment.sidebar_overlay_visible = false;
        }
        let panel = attachment.link.store.read(cx).panel_state();
        let diff_open = panel.right_panel_open;
        let right_tab = panel.right_tab;
        let diff_expanded = panel.right_panel_expanded;
        // "Expanded" (full-width) is a diff-only affordance; the preview tab
        // always shares the split so the webview keeps a stable size.
        let diff_expanded = diff_expanded && right_tab != RightTab::Preview;

        // A native WebView is not composited into GPUI and survives removal of
        // its layout node. Synchronize it before the settings early-return or a
        // right-panel tab/close transition can unmount PreviewPanel.
        attachment
            .preview
            .update(cx, |preview, cx| preview.sync_visibility(cx));

        // Root owns the translucent canvas across both routes; Settings
        // paints its own sidebar and content surfaces over it.
        if route == Route::Settings {
            return div()
                .id("app-shell")
                .size_full()
                // Fullscreen flattens the canvas under the paper (see the
                // workspace root below).
                .when(fullscreen, |this| {
                    this.bg(crate::material::opaque_canvas(cx))
                })
                .text_color(cx.theme().foreground)
                .on_action(cx.listener(Self::on_toggle_palette))
                // Register first so its bubble-phase handlers run after child
                // controls and own selection only when the press propagates.
                .child(gpui_base::TextSelectionLayer)
                .child(
                    div()
                        .id("workspace")
                        .flex_1()
                        .size_full()
                        .min_h_0()
                        .overflow_hidden()
                        .child(attachment.settings_page.clone()),
                )
                .into_any_element();
        }

        // Which entity fills the right panel: the Preview tab shows the embedded
        // browser; Diff/Plan share the DiffPanel container.
        let right_panel: AnyElement = if right_tab == RightTab::Preview {
            attachment.preview.clone().into_any_element()
        } else {
            attachment.diff.clone().into_any_element()
        };

        // Sidebar | chat | right panel live in ONE resizable group. Nesting a
        // second group inside the chat panel does not shrink the chat: it keeps
        // its full width and the right panel is painted over it, clipping the
        // timeline and the composer mid-word. A flat group makes the chat a real
        // flex sibling of the right panel, so it reflows — the guarantee
        // `docs/DESIGN.md` makes for the chat column.
        let chat_visible = !(diff_open && diff_expanded);
        // The right panel is the last of three panels (sidebar · chat · right),
        // and the sidebar is only a panel when it is expanded.
        let right_ix = if collapsed { 1 } else { 2 };

        // gpui-component preserves panel *ratios* when its container changes
        // width. Fullscreen is a container resize, but the sidebar is a fixed
        // navigation column: restore its remembered pixel width when the window
        // width changes. Only then — an every-frame restore would snap the
        // panel back mid-drag (`on_resize` records the width only on mouse-up).
        // The rescale lands on the frame where the group measures its new
        // container, which can trail the viewport change — so keep restoring
        // until the width matches, then stop. A return from the compact layout
        // is such a change, which is why the split comes back as it was.
        let viewport_width = window.viewport_size().width;
        let width_changed = self.last_viewport_width != Some(viewport_width);
        if width_changed {
            self.last_viewport_width = Some(viewport_width);
            attachment.sidebar_restore_pending = true;
        }
        if attachment.sidebar_restore_pending && !collapsed {
            let width = attachment.sidebar_width.get();
            let restored = attachment
                .split
                .update(cx, |state, cx| match state.sizes().first() {
                    Some(size) if *size != width => {
                        state.resize_panel(0, width, window, cx);
                        false
                    }
                    Some(_) => true,
                    None => false,
                });
            // The rescale happens while the group lays out, which is after this
            // runs — so a match on the very frame the viewport changed is the
            // *old* width, not a settled one. Never stop on that frame.
            if restored && !width_changed {
                attachment.sidebar_restore_pending = false;
            }
        }

        // Give the right panel its width once the group knows about it (the
        // panel count is synced while the group renders, so this lands on the
        // frame after it opens — the group notifies, so that frame comes).
        if diff_open && chat_visible {
            if !attachment.right_sized {
                let width = attachment.right_width.get();
                let sized = attachment.split.update(cx, |state, cx| {
                    if state.sizes().len() > right_ix {
                        state.resize_panel(right_ix, width, window, cx);
                        true
                    } else {
                        false
                    }
                });
                attachment.right_sized = sized;
            }
        } else {
            attachment.right_sized = false;
        }

        // Chat and right-panel reading surfaces sit above the translucent canvas.
        let chat_panel = resizable_panel().visible(chat_visible).child(
            v_flex()
                .size_full()
                .bg(crate::material::content_surface(cx))
                .shadow_sm()
                .children(self.render_connection_banner(cx))
                .child(
                    div().flex_1().min_h_0().child(
                        self.attachment
                            .as_ref()
                            .expect("attachment checked above")
                            .chat
                            .clone(),
                    ),
                ),
        );
        let attachment = self.attachment.as_ref().expect("attachment checked above");
        let right = resizable_panel()
            .visible(diff_open)
            .size(px(RIGHT_PANEL_WIDTH))
            .size_range(px(320.)..px(1400.))
            .child(
                div()
                    .size_full()
                    .bg(crate::material::content_surface(cx))
                    .shadow_sm()
                    .child(right_panel),
            );

        let remembered_right = attachment.right_width.clone();
        let remembered_sidebar = attachment.sidebar_width.clone();
        let split = attachment.split.clone();
        let sidebar = attachment.sidebar.clone();
        let overlay_width = attachment.sidebar_width.get();
        let overlay_visible = attachment.sidebar_overlay_visible;
        let palette = attachment.palette.clone();
        let group = move |id: &'static str| {
            h_resizable(id)
                .with_state(&split)
                .on_resize(move |state, _, cx| {
                    let sizes = state.read(cx).sizes();
                    if !collapsed && let Some(size) = sizes.first() {
                        remembered_sidebar.set(*size);
                    }
                    if let Some(size) = sizes.get(right_ix) {
                        remembered_right.set(*size);
                    }
                })
        };

        let workspace: AnyElement = if collapsed {
            // Zero layout width: the chat/right group owns the whole workspace
            // and runs to the window's left edge. The trigger and overlay are
            // independent absolute siblings, so neither reflows the columns.
            //
            // A popover keeps the overlay alive through its occluded-hover
            // false transition (see next_sidebar_overlay_visibility). When the
            // popover dismisses with the pointer already outside the overlay,
            // no hover event follows — reap the stale overlay here instead
            // (dismissal refreshes the window, so this pass always runs).
            let overlay_visible = if overlay_visible
                && !gpui_base::GlobalState::is_in_deferred_context(cx)
                && window.mouse_position().x > overlay_width
            {
                if let Some(attachment) = &mut self.attachment {
                    attachment.sidebar_overlay_visible = false;
                }
                false
            } else {
                overlay_visible
            };
            div()
                .relative()
                .size_full()
                .child(group("chat-diff-panels").child(chat_panel).child(right))
                // This fixed transparent strip only opens the overlay. Its
                // inevitable false transition when the overlay occludes it is
                // deliberately ignored by the state machine.
                .child(
                    div()
                        .id("sidebar-hover-trigger")
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(px(SIDEBAR_HOVER_EDGE))
                        .occlude()
                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                            this.update_sidebar_overlay(
                                SidebarHoverTransition::Trigger(*hovered),
                                cx,
                            );
                        })),
                )
                .when(overlay_visible, |this| {
                    this.child(
                        div()
                            .id("sidebar-hover-overlay")
                            .absolute()
                            .left_0()
                            .top_0()
                            .h_full()
                            .w(overlay_width)
                            // The sidebar fill is translucent, so back the
                            // floating layer with the near-opaque popover surface
                            // to prevent the chat beneath from bleeding through.
                            .bg(cx.theme().popover)
                            .shadow_lg()
                            .border_r_1()
                            .border_color(cx.theme().border)
                            // The blocker and hover listener share this hitbox:
                            // the overlay owns its full visible lifetime.
                            .occlude()
                            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                                this.update_sidebar_overlay(
                                    SidebarHoverTransition::Overlay(*hovered),
                                    cx,
                                );
                            }))
                            .child(sidebar),
                    )
                })
                .into_any_element()
        } else {
            group("workspace-panels")
                .child(
                    resizable_panel()
                        .flex_none()
                        .size(px(SIDEBAR_WIDTH))
                        .size_range(px(220.)..px(380.))
                        .child(sidebar),
                )
                .child(chat_panel)
                .child(right)
                .into_any_element()
        };

        // No separate titlebar: the sidebar and chat columns run to the window
        // top (the native traffic lights overlay the sidebar's top-left).
        div()
            .id("app-shell")
            .size_full()
            // Fullscreen only: a fullscreen Space has nothing but black behind
            // the vibrancy material, which muddies the translucent canvas —
            // cover it with its opaque base. Windowed, paint nothing here:
            // Root owns the translucent canvas.
            .when(fullscreen, |this| {
                this.bg(crate::material::opaque_canvas(cx))
            })
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_toggle_palette))
            // The window selection layer must be the first child.
            .child(gpui_base::TextSelectionLayer)
            .child(
                div()
                    .id("workspace")
                    .flex_1()
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .child(workspace),
            )
            .when(palette_open, |this| this.child(palette))
            .into_any_element()
    }

    fn update_sidebar_overlay(
        &mut self,
        transition: SidebarHoverTransition,
        cx: &mut Context<Self>,
    ) {
        let (collapsed, route) = {
            let state = self.window_state.read(cx);
            (state.sidebar_collapsed, state.route)
        };
        let popover_open = gpui_base::GlobalState::is_in_deferred_context(cx);
        let Some(attachment) = &mut self.attachment else {
            return;
        };
        let visible = next_sidebar_overlay_visibility(
            attachment.sidebar_overlay_visible,
            transition,
            collapsed,
            route,
            popover_open,
        );
        if attachment.sidebar_overlay_visible != visible {
            attachment.sidebar_overlay_visible = visible;
            cx.notify();
        }
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The window may have been resized without a bounds notification (a
        // first frame, or an inset change that narrowed the content box).
        self.sync_layout(window, cx);
        // The palette takes focus on the frame it opens, at either width.
        let palette_open = self.window_state.read(cx).palette_open;
        if palette_open
            && !self.palette_was_open
            && let Some(attachment) = &self.attachment
        {
            attachment.palette.update(cx, |p, cx| p.focus(window, cx));
        }
        self.palette_was_open = palette_open;
        let body = if self.compact(cx) {
            self.render_compact(window, cx)
        } else {
            self.render_wide(window, cx)
        };
        self.within_seam(body, cx)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui::{TestAppContext, VisualTestContext, size};
    use tcode_client::host::Transport;
    use tcode_protocol::{
        ClientPayload, Command, EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic,
        decode_client_line, encode_line,
    };

    use super::*;

    /// One shell over a transport the test holds both ends of, so what the
    /// client says to its host is observable.
    struct MountedShell {
        outgoing: async_channel::Receiver<String>,
        incoming: async_channel::Sender<String>,
    }

    fn mount(cx: &mut TestAppContext) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let (_, state) = async_channel::unbounded();
        let transport = RefCell::new(Some(Transport {
            to_host,
            from_host,
            state,
        }));
        let (shell, cx) = cx.add_window_view(move |window, cx| {
            let window_state = cx.new(|_| WindowState::new(false));
            AppShell::new(
                window_state,
                ShellSetup {
                    local: Some(Rc::new(move || {
                        transport.borrow_mut().take().expect("one attachment")
                    })),
                    initial: Some(AttachmentTarget::Local),
                    ..Default::default()
                },
                window,
                cx,
            )
        });
        (shell, MountedShell { outgoing, incoming }, cx)
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    fn resize(cx: &mut VisualTestContext, width: f32) {
        cx.simulate_resize(size(px(width), px(800.)));
        draw(cx);
    }

    /// Every line the client has sent since the last drain.
    fn sent(host: &MountedShell) -> Vec<ClientPayload> {
        let mut payloads = Vec::new();
        while let Ok(line) = host.outgoing.try_recv() {
            payloads.push(decode_client_line(&line).expect("client line").payload);
        }
        payloads
    }

    fn store_of(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Entity<WorkspaceStore> {
        shell.read_with(cx, |shell, _| shell.store().expect("attached"))
    }

    /// Resizing a window is a layout decision and nothing else: it must not
    /// touch the attachment, the selected thread, the draft or the split.
    #[gpui::test]
    fn the_breakpoint_flips_at_nine_hundred_and_disturbs_nothing_else(cx: &mut TestAppContext) {
        let (shell, host, cx) = mount(cx);
        let store = store_of(&shell, cx);
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(IndexSnapshot {
                        activity: Default::default(),
                        sessions: Vec::new(),
                        projects: Vec::new(),
                    }),
                }))
                .unwrap(),
            )
            .unwrap();
        store.update(cx, |store, cx| {
            store.drain_host_events_for_test(cx);
            store.select_session("thread-1".into());
        });

        resize(cx, 1024.);
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.compact(cx), "1024px is the wide split");
        });
        // Stand in for a sidebar drag, so the restore has something to restore.
        shell.update(cx, |shell, _| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .sidebar_width
                .set(px(300.));
        });
        let composer = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .chat
                .read(cx)
                .composer()
        });
        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| {
                composer.set_draft("half a thought", window, cx)
            });
        });
        draw(cx);
        let _ = sent(&host);

        resize(cx, 899.);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.compact(cx), "899px is compact");
            assert_eq!(
                shell.destination,
                Destination::Thread,
                "a selected thread is the compact destination"
            );
        });
        resize(cx, 900.);
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.compact(cx), "900px is wide, not compact");
        });
        resize(cx, 899.);
        shell.read_with(cx, |shell, cx| assert!(shell.compact(cx)));

        // The thread, the draft and the composer itself all survive.
        assert_eq!(
            store.read_with(cx, |store, _| store.active_session_id()),
            Some("thread-1".into())
        );
        composer.read_with(cx, |composer, cx| {
            assert_eq!(composer.draft(cx), "half a thought");
            assert!(composer.is_compact(), "the composer followed the layout");
        });
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell
                .attachment
                .as_ref()
                .unwrap()
                .chat
                .read(cx)
                .composer()
                .entity_id()),
            composer.entity_id(),
            "the composer entity must be updated, never rebuilt"
        );

        // Nothing about the link changed: no shutdown, and no fresh index
        // subscription (which is what a reattach would look like).
        for payload in sent(&host) {
            assert!(
                !matches!(
                    payload,
                    ClientPayload::Command(Command::ShutdownAllAndFlush)
                        | ClientPayload::Subscribe(tcode_protocol::Subscription {
                            topic: Topic::Index,
                            ..
                        })
                ),
                "resizing must not detach or reconnect: {payload:?}"
            );
        }

        resize(cx, 1024.);
        // The resizable group rescales its panels proportionally when its
        // container changes, and reports the new width a frame later; the
        // restore keeps snapping the sidebar back until it takes.
        draw(cx);
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.compact(cx));
            assert_eq!(
                shell
                    .attachment
                    .as_ref()
                    .expect("attached")
                    .split
                    .read(cx)
                    .sizes()
                    .first()
                    .copied(),
                Some(px(300.)),
                "the wide split comes back at the width it was left at"
            );
        });
    }

    /// The back chain: overlays first, then the navigation stack, and `false`
    /// only at the root — where the platform closes the app.
    #[gpui::test]
    fn back_unwinds_overlays_then_navigation_and_stops_at_the_root(cx: &mut TestAppContext) {
        let (shell, _host, cx) = mount(cx);
        let store = store_of(&shell, cx);
        store.update(cx, |store, _| store.select_session("thread-1".into()));
        resize(cx, 393.);

        let back = |cx: &mut VisualTestContext| {
            let consumed =
                cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx)));
            draw(cx);
            consumed
        };

        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.destination, Destination::Thread)
        });

        // An open overlay outranks navigation.
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        window_state.update(cx, |state, cx| state.open_palette(cx));
        assert!(back(cx), "back closes the overlay");
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.window_state.read(cx).palette_open);
            assert_eq!(shell.destination, Destination::Thread, "and nothing else");
        });

        // Thread → Threads keeps the connection.
        assert!(back(cx));
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.destination, Destination::Threads);
            assert!(shell.attachment.is_some(), "still attached");
        });

        // Threads → Hosts is an explicit detach.
        assert!(back(cx));
        shell.read_with(cx, |shell, _| {
            assert_eq!(shell.destination, Destination::Hosts);
            assert!(shell.attachment.is_none(), "leaving the list detaches");
        });

        assert!(!back(cx), "the root belongs to the platform");
    }

    #[test]
    fn hover_transitions_open_preserve_and_close_the_overlay() {
        for (current, transition, visible) in [
            (false, SidebarHoverTransition::Trigger(true), true),
            (false, SidebarHoverTransition::Trigger(false), false),
            (true, SidebarHoverTransition::Trigger(false), true),
            (false, SidebarHoverTransition::Overlay(true), true),
            (true, SidebarHoverTransition::Overlay(true), true),
            (true, SidebarHoverTransition::Overlay(false), false),
        ] {
            assert_eq!(
                next_sidebar_overlay_visibility(current, transition, true, Route::Chat, false),
                visible
            );
        }
    }

    #[test]
    fn open_popover_keeps_overlay_through_occluded_hover_loss() {
        assert!(next_sidebar_overlay_visibility(
            true,
            SidebarHoverTransition::Overlay(false),
            true,
            Route::Chat,
            true,
        ));
        // But a popover cannot conjure an overlay that is already closed.
        assert!(!next_sidebar_overlay_visibility(
            false,
            SidebarHoverTransition::Overlay(false),
            true,
            Route::Chat,
            true,
        ));
    }

    #[test]
    fn expanded_sidebar_forces_overlay_closed() {
        assert!(!next_sidebar_overlay_visibility(
            true,
            SidebarHoverTransition::Overlay(true),
            false,
            Route::Chat,
            false,
        ));
    }

    #[test]
    fn non_workspace_route_forces_overlay_closed() {
        assert!(!next_sidebar_overlay_visibility(
            true,
            SidebarHoverTransition::Overlay(true),
            true,
            Route::Settings,
            false,
        ));
    }
}
