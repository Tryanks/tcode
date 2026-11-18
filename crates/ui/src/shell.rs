//! One responsive shell, on every client.
//!
//! [`AppShell`] is the window: it owns navigation, the layout rule and the
//! back stack, and it survives everything below it. Beneath it sit its
//! *links* — one per host, each with the store it fills — and the *column*,
//! whose panes each show a thread of one of those hosts. Replacing the
//! workspace replaces both; it does not replace the window's navigation
//! root, and resizing the window never touches either.
//!
//! Layout is decided by one rule (`crate::window_seam`): a desktop build is
//! always the wide split, and a mobile build is a navigation stack — hosts,
//! threads, thread, panel — under 900px of usable width and the wide split at
//! or above it. Wide and unattached, the shell is the hosts page and nothing
//! else until it attaches somewhere.

mod compact;
mod events;
mod hosts;
mod navigation;
mod pane_layout;
mod wide;

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use crate::overlay::{Notification, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::tabs::{TabBar, TabItem};
use crate::window_state::{NavigationPanel, NavigationSnapshot};
use gpui::{
    AnyElement, AnyView, AnyWindowHandle, App, AppContext as _, ClipboardItem, Context, Div,
    ElementId, Entity, EntityId, Focusable as _, Global, HitboxBehavior, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Render, SharedString,
    Styled as _, Subscription, Task, TouchPhase, WeakEntity, Window, actions, canvas, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{
    NavMotion, NavOperation, NavStack, NavStackState, ResizableState, h_flex, h_resizable,
    motion::PresencePhase, motion::Transition, resizable_panel, v_flex,
};
use tcode_client::host::ClientHost;
use tcode_core::ui::RightTab;
use tcode_protocol::{
    RuntimeEffect, RuntimeError, RuntimeNotification as RuntimeEvent, RuntimeOperationId,
    RuntimeToast,
};

use crate::attachment::{Attachment, LocalTransport, same_target};
use crate::chat::ChatView;
use crate::device::DevicePreferences;
use crate::icon::{Icon, IconName};
use crate::palette::CommandPalette;
use crate::panes::{
    CloseOtherPanes, ClosePane, FocusPane, MoveThreadToPane, OpenInNewPane, PaneLayout, Side,
    SplitDown, SplitRight, SwapPane, ThreadPanel, ZoomPane,
};
use crate::preview_panel::PreviewPanel;
use crate::remote::{AttachmentTarget, HostRequest, RemotePanel};
use crate::runtime_event::{
    RuntimeEventSeverity, RuntimeToastAction, RuntimeToastDisposition, present_runtime_event,
    present_runtime_toast,
};
use crate::settings_page::SettingsPage;
use crate::sidebar::{QuickChat, SessionsSidebar, StartDraftForProject};
use crate::store::{HostKey, ShowThread, StoreChange, ThreadKey, TopicKind, WorkspaceStore};
use crate::toast::{RuntimeToastNotification, ToastAction, ToastId, ToastKind};
use crate::window_caption;
use crate::window_state::{Destination, OpenPanel, Route, WindowState};
use gpui_base::dock::{DockArea, DockEvent, DockLayout, DockPlacement, InsertTarget, PanelId};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

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
const WINDOW_DRAGGABLE: bool = cfg!(not(any(
    target_os = "ios",
    target_os = "android",
    target_family = "wasm"
)));

/// Make `el` a window-drag handle (the window has no separate titlebar, so the
/// column top rows are the drag surface): a press arms a move, and the first
/// drag calls `start_window_move`. Child buttons that stop propagation on
/// mouse-down won't arm a drag.
pub(crate) fn window_drag_area<E: gpui::InteractiveElement>(
    id: impl Into<ElementId>,
    el: E,
    window: &mut Window,
    cx: &mut App,
) -> E {
    if !WINDOW_DRAGGABLE {
        return el;
    }
    let state = window.use_keyed_state(id, cx, |_, _| WindowDragState { should_move: false });
    el.on_mouse_down_out(window.listener_for(&state, |state, _, _, _| {
        state.should_move = false;
    }))
    .on_mouse_down(
        MouseButton::Left,
        window.listener_for(&state, |state, event: &MouseDownEvent, window, _| {
            // A press that moves the window leaves the focus where it was.
            window.prevent_default();
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

/// One entry of the compact [`NavStack`], standing for one entry of the
/// window's navigation history. It owns nothing: it renders whichever shell
/// destination it stands for, so every page stays mounted while a push or pop
/// animates. One view per *entry*, not per destination — the same destination
/// can appear twice (Hosts visited from an open thread, a lead under its agent).
struct DestinationView {
    shell: WeakEntity<AppShell>,
    destination: Destination,
}

impl Render for DestinationView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let destination = self.destination;
        let page = cx.entity_id();
        self.shell
            .upgrade()
            .map(|shell| {
                shell.update(cx, |shell, cx| match destination {
                    Destination::Hosts => shell.render_hosts_page(window, cx),
                    Destination::Pair => shell.render_pair_page(window, cx),
                    Destination::Threads => shell.render_threads_page(window, cx),
                    Destination::Thread => shell.render_thread_page(page, window, cx),
                    Destination::Panel => shell.render_panel_page(window, cx),
                    Destination::PullRequest => shell.render_pull_request_page(window, cx),
                    Destination::Settings => {
                        shell.render_settings_page(Destination::Settings, window, cx)
                    }
                    Destination::SettingsSection
                    | Destination::SettingsThreadRules
                    | Destination::SettingsSourceControlHost
                    | Destination::SettingsHarness => {
                        shell.render_settings_page(destination, window, cx)
                    }
                    Destination::ArrangeThreads => shell.render_arrange_page(window, cx),
                })
            })
            .unwrap_or_else(|| div().into_any_element())
    }
}

/// The window's views of its hosts' threads. Nothing here is touched by a
/// resize.
struct Column {
    /// The wide layout's chat column, a dock of [`ThreadPanel`]s, each over
    /// its own thread's host.
    thread_panes: Entity<DockArea>,
    /// The column's splits gliding toward the layout an edit made.
    glides: crate::panes::Glides,
    /// The window's size, and whether it was maximised and fullscreen, on
    /// the last frame that drew the column, for a jump to glide from.
    window_seen: Option<(gpui::Size<Pixels>, (bool, bool))>,
    /// The pane zoomed over the column.
    zoom: crate::panes::Zoom,
    /// A layout being put in while the column fades.
    layout_swap: crate::panes::LayoutSwap,
    /// The wide routes on screen: one at rest, two or more while the
    /// window changes route.
    routes: crate::motion::Scene<Route>,
    /// The compact thread page's body on screen: the skeleton while a
    /// thread is restored, then its chat.
    thread_body: crate::motion::Scene<ThreadBody>,
    /// The body last made current: a chat stays drawn unseen beneath it
    /// until it [appears](ChatView::appears).
    thread_body_shown: Option<ThreadBody>,
    /// The threads under the compact page's agent, outermost first.
    leads: Vec<ThreadPage>,
    /// Agents Back left, on their pages until those have slid off.
    leaving: Vec<ThreadPage>,
    /// Set by the panes' renderer in a frame that draws a split's handle
    /// held by the pointer: the handle's axis and where the pointer was.
    split_handle_held: crate::panes::HandleHeld,
    /// Where the drop's placeholder last showed a dragged thread landing, for
    /// its ghost to go into: the dock has cleared its drop by the time the
    /// drop arrives.
    drop_landing: Rc<Cell<Option<gpui::Bounds<Pixels>>>>,
    /// The split whose handle the pointer holds, found as the hold began.
    held_split: Option<gpui_base::dock::NodeId>,
    /// The window's current pane, one as long as a pane exists: the last one
    /// pressed anywhere, entered by the keyboard or placed into. The keyboard
    /// leaving it changes nothing. What acts on the window as a whole reads
    /// it, it is where the next thread lands and the compact layout's page,
    /// and the host never learns of it.
    current_pane: Entity<ThreadPanel>,
    /// What each pane's chat was last handed, by chat: compared here, not
    /// read back from the chat, whose every render would otherwise draw
    /// the shell again.
    handed: HashMap<gpui::EntityId, HandedToChat>,
    /// The workspace split's state (sidebar · chat), owned here so the glide
    /// can give the sidebar the size it lands at: gpui-base's group latches
    /// the first size it measures for a panel, which for one that grew out
    /// of nothing is nothing.
    split: Entity<ResizableState>,
    /// The workspace split's panels gliding as the sidebar collapses and
    /// expands.
    panel_glide: crate::motion::resize_glide::ResizeGlide,
    sidebar_width: Rc<Cell<Pixels>>,
    /// Collapsed-only overlay visibility. Purely transient and never persisted;
    /// expanded/non-workspace renders clear it synchronously.
    sidebar_overlay_visible: bool,
    sidebar_focus: gpui::FocusHandle,
    // Native exit events leave Window::mouse_position at its last inside position.
    sidebar_pointer_in_window: bool,
    _subscriptions: Vec<Subscription>,
}

/// What the shell last handed a pane's chat.
#[derive(Clone, PartialEq)]
struct HandedToChat {
    edges: crate::chat::WindowEdges,
    recedes: bool,
    close: Option<crate::motion::Shown<gpui::WeakEntity<ThreadPanel>>>,
    controls: Option<PaneControls>,
    host: Option<SharedString>,
}

/// The window's link to one host, the store it fills and the views over
/// that store alone. Once the link closes it listens to nothing, and the
/// store stays for the views still drawing it.
struct Link {
    target: AttachmentTarget,
    store: Entity<WorkspaceStore>,
    open: Option<Attachment>,
    /// The host's preview panel: it answers the preview tools and owns the
    /// pages each pane's drawer shows.
    preview: Entity<PreviewPanel>,
    settings_page: Entity<SettingsPage>,
    /// Whether this host's saved record has been stamped with the time this
    /// window reached it.
    stamped: bool,
    /// Whether the link was up when the window last looked.
    connected: bool,
    attention: crate::attention::ApprovalAttention,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Link {
    fn key(&self) -> HostKey {
        self.target.key()
    }

    fn link(&self) -> Option<tcode_client::HostLink> {
        self.open.as_ref().map(Attachment::link)
    }

    fn close(&mut self, cx: &mut App) {
        if let Some(open) = self.open.take() {
            open.close(cx).close();
        }
        self._subscriptions.clear();
        self._tasks.clear();
    }
}

impl Column {
    /// The chat column's panes, in the layout's order. A closing pane is no
    /// longer one of them: nothing acts on it, focuses it or counts it.
    fn panes(&self, cx: &App) -> Vec<Entity<ThreadPanel>> {
        self.all_panes(cx)
            .into_iter()
            .filter(|pane| !pane.read(cx).is_closing())
            .collect()
    }

    /// Every pane the dock draws, closing ones too, in the layout's order.
    fn all_panes(&self, cx: &App) -> Vec<Entity<ThreadPanel>> {
        let area = self.thread_panes.read(cx);
        area.layout(DockPlacement::Center)
            .map(|tree| {
                tree.panels()
                    .filter_map(|panel| area.panel(panel)?.view().downcast::<ThreadPanel>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The closing panes whose split has stopped gliding, for the shell to
    /// take out.
    fn closed_panes(&self, cx: &App) -> Vec<Entity<ThreadPanel>> {
        let Some(tree) = self.thread_panes.read(cx).layout(DockPlacement::Center) else {
            return Vec::new();
        };
        self.all_panes(cx)
            .into_iter()
            .filter(|pane| {
                pane.read(cx).is_closing()
                    && tree
                        .find_panel_node(PanelId::from(pane.entity_id()))
                        .and_then(|group| crate::panes::parent_split(tree.root(), group))
                        .is_none_or(|split| !self.glides.gliding(split))
            })
            .collect()
    }

    /// The layout once every closing pane has gone.
    fn final_layout(&self, cx: &App) -> Option<gpui_base::dock::PaneTree> {
        let mut tree = self
            .thread_panes
            .read(cx)
            .layout(DockPlacement::Center)?
            .clone();
        for pane in self.all_panes(cx) {
            if pane.read(cx).is_closing() {
                tree.remove_panel(PanelId::from(pane.entity_id()));
            }
        }
        Some(tree)
    }

    /// The pane the compact layout draws.
    fn page(&self) -> &Entity<ThreadPanel> {
        &self.current_pane
    }

    /// The compact page's conversation.
    fn chat(&self, cx: &App) -> Entity<ChatView> {
        self.page().read(cx).chat()
    }

    /// What the page `page` draws in place of the page pane's conversation.
    fn thread_page(&self, page: EntityId) -> Option<&ThreadPage> {
        self.leads
            .iter()
            .chain(&self.leaving)
            .find(|kept| kept.page == page)
    }
}

/// What the compact thread page's body shows.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ThreadBody {
    Skeleton,
    Chat,
}

/// A conversation drawn on a page of the compact stack instead of the
/// page pane's, whose timeline it leases for as long as it is kept.
#[derive(Clone)]
struct ThreadPage {
    page: EntityId,
    chat: Entity<ChatView>,
    frame: Entity<crate::chat::ChatFrame>,
    thread: ThreadKey,
}

/// Which pane commands the window offers now; none in the compact layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PaneControls {
    pub(crate) split_right: bool,
    pub(crate) split_down: bool,
    pub(crate) close: bool,
    pub(crate) close_others: bool,
    pub(crate) zoom: bool,
    pub(crate) zoomed: bool,
    /// The sides, in [`Side::ALL`]'s order, with a pane to swap with.
    pub(crate) neighbours: [bool; 4],
    /// Whether the pane shows a thread to move into a neighbour.
    pub(crate) moves: bool,
}

impl PaneControls {
    /// The sides with a pane to swap with.
    pub(crate) fn sides(self) -> impl Iterator<Item = Side> {
        Side::ALL
            .into_iter()
            .zip(self.neighbours)
            .filter_map(|(side, beside)| beside.then_some(side))
    }
}

/// This window's [`PaneControls`] for `pane`, or the pane a command acts on;
/// `None` where there are no panes.
pub(crate) fn pane_controls(window: &Window, cx: &App) -> Option<PaneControls> {
    let shell = current_shell(cx)?;
    let shell = shell.read(cx);
    shell.pane_controls(&shell.acting_pane()?, window, cx)
}

/// Give the keyboard to the current pane, as a click in it does: its composer
/// takes the caret. A pane command raised from outside the panes then acts on
/// that pane and leaves the keyboard there.
pub(crate) fn select_current_pane(window: &mut Window, cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    let shell = shell.read(cx);
    if shell.compact(cx) {
        return;
    }
    let Some(handle) = shell
        .column
        .as_ref()
        .map(|column| column.current_pane.focus_handle(cx))
    else {
        return;
    };
    if !handle.contains_focused(window, cx) {
        window.focus(&handle, cx);
    }
}

/// What held the keyboard closed: it goes back to a composer.
pub(crate) fn return_keyboard(window: &mut Window, cx: &mut App) {
    if let Some(shell) = current_shell(cx) {
        shell.update(cx, |shell, cx| shell.return_keyboard(window, cx));
    }
}

/// A command a pane's own control raises, or that a window-wide one raises
/// for the pane it acts on.
#[derive(Clone, Copy)]
pub(crate) enum PaneCommand {
    Split(gpui_base::Placement),
    Close,
    CloseOthers,
    Zoom,
    Swap(Side),
    Move(Side),
}

/// Run `command` on `pane`, once the dispatch that carried it ends.
pub(crate) fn pane_command(pane: Entity<ThreadPanel>, command: PaneCommand, cx: &mut App) {
    let Some((window, shell)) = current_window_shell(cx) else {
        return;
    };
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, cx| {
            shell.update(cx, |shell, cx| shell.run_on_pane(pane, command, window, cx));
        });
    });
}

/// Give the keyboard to the composer of the pane holding it, as a row of a
/// pane header's menu leaves it: off the menu's button, as a pane command
/// from the palette leaves it. Focus a row sent out of the panes stays there.
pub(crate) fn focus_pane_composer(window: &mut Window, cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    let shell = shell.read(cx);
    if shell.compact(cx) {
        return;
    }
    let Some(chat) = shell.column.as_ref().and_then(|column| {
        let pane = column
            .panes(cx)
            .into_iter()
            .find(|pane| pane.focus_handle(cx).contains_focused(window, cx))?;
        let pane = pane.read(cx);
        pane.session_id().is_some().then(|| pane.chat())
    }) else {
        return;
    };
    chat.update(cx, |chat, cx| chat.focus_composer(window, cx));
}

/// The layouts the palette offers now: those the column has room for.
pub(crate) fn pane_layouts(window: &Window, cx: &App) -> Vec<PaneLayout> {
    let Some(shell) = current_shell(cx) else {
        return Vec::new();
    };
    let shell = shell.read(cx);
    PaneLayout::ALL
        .into_iter()
        .filter(|layout| shell.plan_layout(*layout, window, cx).is_some())
        .collect()
}

/// Whether a pane of this window shows `thread`. The compact layout has no
/// panes.
pub(crate) fn shown_in_pane(thread: &ThreadKey, cx: &App) -> bool {
    current_shell(cx).is_some_and(|shell| {
        let shell = shell.read(cx);
        !shell.compact(cx) && shell.pane_showing(thread, cx).is_some()
    })
}

/// The thread of the current pane, for what acts on the whole window.
pub(crate) fn current_thread(cx: &App) -> Option<ThreadKey> {
    current_shell(cx)?.read(cx).page_thread(cx)
}

/// The store of the current pane's host, for a command about that pane.
pub(crate) fn current_store(cx: &App) -> Option<Entity<WorkspaceStore>> {
    current_shell(cx)?.read(cx).store(cx)
}

/// The store of `host`, while the window is linked to it.
pub(crate) fn store_of_host(host: &HostKey, cx: &App) -> Option<Entity<WorkspaceStore>> {
    Some(current_shell(cx)?.read(cx).link_of(host)?.store.clone())
}

/// Whether the window is over several hosts, so what acts on one names it.
pub(crate) fn several_hosts(cx: &App) -> bool {
    current_shell(cx).is_some_and(|shell| shell.read(cx).links.len() > 1)
}

/// Show `host`'s settings in place of the ones shown, on the same section.
pub(crate) fn choose_settings_host(host: HostKey, cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    let shell = shell.read(cx);
    let (Some(from), Some(to)) = (
        shell
            .settings_link(cx)
            .map(|link| link.settings_page.clone()),
        shell.link_of(&host).map(|link| link.settings_page.clone()),
    ) else {
        return;
    };
    let window_state = shell.window_state.clone();
    SettingsPage::carry_section(&from, &to, cx);
    window_state.update(cx, |state, cx| {
        state.settings_host = Some(host);
        cx.notify();
    });
}

/// Whether a pane of this window shows `thread`, the compact page included.
pub(crate) fn pane_shows(thread: &ThreadKey, cx: &App) -> bool {
    current_shell(cx).is_some_and(|shell| shell.read(cx).pane_showing(thread, cx).is_some())
}

/// The chat that shows an agent opened from `lead` beside it, for what
/// knows only the lead (the right panel, which shows a thread's
/// agents): the chat of the pane showing `lead`, or else of a pane showing
/// `lead` beside its own lead, which has room for one agent beside it. A
/// thread shown in a pane and beside its lead in another is the pane's.
/// None in the compact layout, which shows one thread at a time.
pub(crate) fn side_host_for(lead: &ThreadKey, cx: &App) -> Option<Entity<ChatView>> {
    let shell = current_shell(cx)?;
    let shell = shell.read(cx);
    if shell.compact(cx) {
        return None;
    }
    let column = shell.column.as_ref()?;
    let chats: Vec<_> = column
        .panes(cx)
        .into_iter()
        .filter(|pane| pane.read(cx).host(cx) == lead.host)
        .map(|pane| pane.read(cx).chat())
        .collect();
    let lead = lead.session_id.as_str();
    let showing = |shown: fn(&ChatView) -> Option<&str>| {
        chats
            .iter()
            .find(|chat| shown(chat.read(cx)) == Some(lead))
            .cloned()
    };
    showing(ChatView::session_id).or_else(|| showing(ChatView::side_session_id))
}

/// Show the agent `session_id` on a page over its lead in a compact window;
/// otherwise, and with no thread page to stand on, show it where it lands.
pub(crate) fn open_agent_page(
    store: &Entity<WorkspaceStore>,
    session_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let thread = store.read(cx).thread_key(&session_id);
    let opened = current_shell(cx).is_some_and(|shell| {
        shell.update(cx, |shell, cx| shell.open_over_lead(thread, window, cx))
    });
    if !opened {
        store.update(cx, |store, cx| store.show_thread(session_id, cx));
    }
}

/// Whether one of `panes` draws `thread` beside its own, which keeps it open
/// when a pane showing it as its own goes.
fn drawn_beside(panes: &[Entity<ThreadPanel>], thread: &ThreadKey, cx: &App) -> bool {
    panes.iter().any(|pane| shows_beside(pane, thread, cx))
}

/// Whether `pane` draws `thread` beside its own.
fn shows_beside(pane: &Entity<ThreadPanel>, thread: &ThreadKey, cx: &App) -> bool {
    let pane = pane.read(cx);
    pane.host(cx) == thread.host
        && pane.chat().read(cx).side_session_id() == Some(thread.session_id.as_str())
}

/// What fills one panel of a layout about to be installed.
#[derive(Clone)]
enum Slot {
    Pane(Entity<ThreadPanel>),
    /// A new pane showing this thread.
    Thread(ThreadKey),
    Waiting,
}

/// Whether the machine behind `store` refused this device for good, so its
/// link stopped reconnecting.
fn refused(store: &Entity<WorkspaceStore>, cx: &App) -> bool {
    matches!(
        store.read(cx).connection_state(),
        tcode_client::ConnectionState::Offline { reason } if reason.is_terminal()
    )
}

/// What bootstrap tells the shell that is not the window itself.
#[derive(Default)]
pub struct ShellSetup {
    /// How to reach a host running inside this process. A client that can never
    /// host has none.
    pub local: Option<LocalTransport>,
    /// Why that host did not start at launch. Machines says so from the first
    /// frame, whatever the window attaches to, and the window attaches to that
    /// host only when asked again.
    pub local_failure: Option<String>,
    /// The host the window shows first at launch, opened beside the
    /// workspace's hosts though it is not one of them; a host reached only
    /// alone opens alone. `None` shows the workspace's first host.
    pub initial: Option<AttachmentTarget>,
    /// A browser link that could not be paired opens the normal form with the
    /// one-shot failure visible after its URL fragment has been cleared.
    pub initial_pairing_error: Option<String>,
    /// The settings of the host in this process, read before the window
    /// opens: a device preference still unset takes its value from them
    /// rather than from whichever host the window reaches first.
    pub local_settings: Option<tcode_core::settings::Settings>,
    /// Mobile bootstrap restores client-local navigation. Desktop defaults to
    /// a fresh path; the phone example can opt into the mobile policy.
    pub restore_navigation: bool,
    /// A window opened in place of one that closed: where that one was. It
    /// stands in for `initial` and for any saved navigation.
    pub reopen: Option<ShellPlace>,
}

/// Where a window was: the host it was attached to and the thread and page
/// it showed there.
pub struct ShellPlace {
    target: Option<AttachmentTarget>,
    navigation: NavigationSnapshot,
}

struct NavigationPersistence {
    scheduled: Option<NavigationSnapshot>,
    task: Option<Task<()>>,
}

struct PendingNavigationRestore {
    thread: ThreadKey,
    panel: NavigationPanel,
    panel_open: bool,
}

pub struct AppShell {
    window_state: Entity<WindowState>,
    setup: ShellSetup,
    /// This client's identity, saved hosts and preferences: the
    /// [`crate::remote::ClientAttachment`]'s.
    client: Rc<dyn ClientHost>,
    /// The hosts this window is linked to.
    links: Vec<Link>,
    /// The panes, each over one of `links`' hosts.
    column: Option<Column>,
    /// Whether `links` is one host reached only alone, a space's, in place
    /// of this device's workspace.
    alone: bool,
    /// The threads of every host in `links`, wide beside the chat and as
    /// the compact threads page.
    sidebar: Entity<SessionsSidebar>,
    /// The command palette over every host in `links`.
    palette: Entity<CommandPalette>,
    /// Saved hosts, pairing by invitation and authentication repair: the hosts
    /// destination, and the same panel Settings → Remote shows.
    hosts: Entity<RemotePanel>,
    nav: Entity<NavStackState>,
    back_swipe: Option<BackSwipe>,
    // Settlement outlives the pan contact, including GPUI's fling cancellation.
    page_back: Option<PageBack>,
    /// The navigation history as the compact stack currently has it mounted.
    /// [`AppShell::sync_nav`] reconciles it with `window_state`'s history, which
    /// is the only authority for where the window is.
    mounted: Vec<Destination>,
    /// Whether the compact stack is changing, which gpui-base's stack does
    /// not say: set as it starts and by each frame that draws a page leaving.
    /// What changes the pages themselves waits for it to end.
    pages_changing: Rc<Cell<bool>>,
    /// Which surface the compact panel destination is showing.
    panel_shows_terminal: bool,
    navigation: Option<NavigationPersistence>,
    pending_navigation_restore: Option<PendingNavigationRestore>,
    /// A host numbers its own operations.
    operation_toasts: HashMap<(HostKey, RuntimeOperationId), ToastId>,
    next_toast_id: ToastId,
    /// Tracks the palette's open state across frames so it can be focused on the
    /// open transition.
    palette_was_open: bool,
    /// The title last given to the OS window. The title is in the current
    /// language, and a language switch reaches the shell only as a redraw.
    window_title: String,
    /// Names what the window shows: a new one each time the column is
    /// replaced or dropped.
    scene_key: u64,
    /// The wide window's workspaces on screen: the current one, and while
    /// it rises, the ones it replaced.
    attachments: crate::motion::Scene<u64>,
    /// Workspaces the wide window has left, still drawn beneath the one
    /// replacing them until it has risen over them. Their links are closed
    /// and they listen to nothing.
    departed: Vec<Departed>,
    /// Set while a departed workspace is drawn.
    departing: Option<Departing>,
    /// Whether the window turned compact or wide since it was last drawn.
    layout_flipped: bool,
    /// This frame's layout, whose sidebar and conversation glide from where
    /// one layout had them to where the other has them when the window
    /// turns compact or wide. The two layouts are different trees over the
    /// same views, so neither can be drawn beside the other for a
    /// cross-fade; what they share moves instead.
    layout_reflow: Option<crate::motion::reflow::Reflow>,
    /// Opens the workspace's new members once the saved machines say who
    /// they are.
    _saved_hosts: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// A workspace the window left, as it was when it left.
struct Departed {
    key: u64,
    column: Option<Column>,
    links: Vec<Link>,
    sidebar: Entity<SessionsSidebar>,
    route: Route,
    destination: Destination,
}

/// How a departed workspace is drawn.
struct Departing {
    destination: Destination,
    /// The hosts surface is one set of elements; the workspace arriving
    /// draws it when it shows it too.
    hosts_withheld: bool,
}

struct BackSwipe {
    origin: gpui::Point<Pixels>,
    progress: crate::edge_back::BackProgress,
    sheet: Option<Entity<crate::overlay::SheetBack>>,
    last_moved_at: Instant,
    finished: bool,
}

struct PageBack {
    previous: AnyView,
    current: AnyView,
    progress: f32,
    target: Option<f32>,
}

/// A pane alone in the chat column is not split narrower than this, plus
/// whatever its header holds beyond the plain one (`header_extra_width_for`),
/// so its header keeps the tiers it has without panes.
const CHAT_MIN_WIDTH: f32 = crate::chat::HEADER_COMPACT_MIN_WIDTH;
/// A pane among others may narrow to its header's overflow tier, which holds
/// the pane's close button among others; panes side by side add up
/// ([`AppShell::column_min_width`]).
pub(crate) const PANE_MIN_WIDTH: f32 =
    crate::chat::HEADER_OVERFLOW_MIN_WIDTH + crate::chat::PANE_CLOSE_WIDTH;
/// The sidebar's width in a new window.
pub(crate) const SIDEBAR_WIDTH: f32 = 255.;
/// The narrowest the sidebar is dragged to while it shows.
pub(crate) const SIDEBAR_MIN_WIDTH: f32 = 220.;
/// A new desktop window's width.
pub const DEFAULT_WINDOW_WIDTH: f32 = 1200.;
/// Collapsed only: width of the window's left-edge activation region.
const SIDEBAR_HOVER_EDGE: f32 = 12.;
/// Compact push/pop duration.
const NAV_TRANSITION: Duration = Duration::from_millis(200);

impl AppShell {
    pub fn new(
        window_state: Entity<WindowState>,
        mut setup: ShellSetup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let device = DevicePreferences::entity(cx);
        let preferences_failures = cx.subscribe_in(
            &device,
            window,
            |_, _, failure: &crate::device::PreferencesFailure, window, cx| {
                let message = match failure {
                    crate::device::PreferencesFailure::Unreadable(error) => {
                        crate::tr!("errors.file_unreadable", error = error.to_string())
                    }
                    crate::device::PreferencesFailure::Unsaved(error) => crate::tr!(
                        "errors.device_preferences_unsaved",
                        error = error.to_string()
                    ),
                };
                window.push_notification(Notification::error(message.into_owned()), cx);
            },
        );
        // This client's identity, saved hosts and preferences.
        let client = cx.global::<crate::remote::ClientAttachment>().host();
        DevicePreferences::install(client.clone(), cx);
        // The window lists no saved machines while they cannot be read.
        // Said once the window holds its overlays.
        // Nor can the machine this data directory hosts be told from the
        // saved ones while its own file cannot be read.
        for error in [client.load_hosts().err(), client.local_machine_id().err()]
            .into_iter()
            .flatten()
        {
            let message =
                crate::tr!("errors.file_unreadable", error = error.to_string()).into_owned();
            cx.defer_in(window, move |_, window, cx| {
                window.push_notification(Notification::error(message), cx);
            });
        }
        if let Some(settings) = &setup.local_settings
            && DevicePreferences::adopt(settings, cx)
        {
            crate::settings::apply_locale(DevicePreferences::language(cx).as_deref());
        }
        if let Some(mode) = DevicePreferences::appearance(cx) {
            crate::theme::apply_theme(mode, window, cx);
        }
        let mut restored = match setup.reopen.take() {
            Some(place) => {
                setup.initial = place.target;
                Some(place.navigation)
            }
            None => setup
                .restore_navigation
                .then(|| {
                    DevicePreferences::navigation(cx).and_then(NavigationSnapshot::from_preferences)
                })
                .flatten(),
        };
        if let Some(thread) = restored
            .as_ref()
            .and_then(|snapshot| snapshot.thread.as_ref())
        {
            match &thread.host {
                HostKey::Local => {
                    if setup.local.is_some() {
                        setup.initial = Some(AttachmentTarget::Local);
                    }
                }
                HostKey::Remote(host_id) => {
                    if let Some(saved) = client
                        .load_hosts()
                        .ok()
                        .and_then(|hosts| hosts.into_iter().find(|saved| saved.host_id == *host_id))
                    {
                        setup.initial = Some(AttachmentTarget::Remote(Box::new(saved)));
                    }
                }
            }
        }
        // A host whose outbox cannot be read opens first too: opening it
        // says why.
        if let Some(host) = client.load_hosts().ok().and_then(|hosts| {
            hosts.into_iter().find(|host| {
                client.outbox_storage(&host.host_id).is_some_and(|storage| {
                    storage.load().map_or(true, |entries| !entries.is_empty())
                })
            })
        }) {
            setup.initial = Some(AttachmentTarget::Remote(Box::new(host)));
            restored = None;
        }
        // Seed the layout from the window before any child view exists, so the
        // first frame is already the right one rather than a wide split that
        // reflows on the second.
        let compact = crate::window_seam::window_is_compact(window, cx);
        window_state.update(cx, |state, cx| {
            state.set_compact(compact, cx);
        });
        let subscriptions = vec![
            preferences_failures,
            // The one place navigation reaches the stack: whoever moved the
            // history — a nav bar, the sidebar, the palette, Android Back —
            // gets the same push or pop out of it.
            cx.observe_in(&window_state, window, |this, _, window, cx| {
                if this.mounted != this.window_state.read(cx).history() {
                    this.blur_navigation(window, cx);
                }
                this.open_requested_settings_section(cx);
                this.sync_nav(NavMotion::Animated, cx);
                this.schedule_navigation_save(cx);
                cx.notify();
            }),
            cx.subscribe(&window_state, |this, _, event: &OpenPanel, cx| {
                this.show_panel(event.0.id(), cx);
                if this.window_state.read(cx).compact {
                    this.go(Destination::Panel, cx);
                }
            }),
            // The one width observer. It updates the layout only when the rule
            // flips; `render` keeps its own width tracking for the split.
            cx.observe_window_bounds(window, |this, window, cx| {
                this.sync_layout(window, cx);
            }),
        ];
        let hosts = cx.new(|cx| {
            let mut panel = RemotePanel::new(window_state.clone(), window, cx);
            panel.set_pairing_error(setup.initial_pairing_error.clone());
            panel
        });
        let sidebar = cx.new(|cx| SessionsSidebar::new(Vec::new(), window_state.clone(), cx));
        let palette = cx.new(|cx| CommandPalette::new(window_state.clone(), window, cx));
        let mut shell = Self {
            window_state,
            client,
            links: Vec::new(),
            column: None,
            alone: false,
            sidebar,
            palette,
            hosts,
            nav: cx.new(|_| NavStackState::new()),
            back_swipe: None,
            page_back: None,
            pages_changing: Rc::default(),
            mounted: Vec::new(),
            panel_shows_terminal: false,
            navigation: setup.restore_navigation.then_some(NavigationPersistence {
                scheduled: None,
                task: None,
            }),
            pending_navigation_restore: None,
            operation_toasts: HashMap::new(),
            next_toast_id: 1,
            palette_was_open: false,
            window_title: String::new(),
            scene_key: 0,
            attachments: Default::default(),
            departed: Vec::new(),
            departing: None,
            layout_flipped: false,
            layout_reflow: None,
            _saved_hosts: None,
            _subscriptions: subscriptions,
            setup,
        };
        let local_started = match shell.setup.local_failure.take() {
            Some(reason) => {
                shell
                    .hosts
                    .update(cx, |hosts, cx| hosts.set_local_failure(Some(reason), cx));
                if shell.setup.initial == Some(AttachmentTarget::Local) {
                    shell.setup.initial = None;
                }
                false
            }
            None => true,
        };
        let initial = shell.setup.initial.take();
        shell.open_workspace(initial, local_started, window, cx);
        shell._saved_hosts = shell.client.saved_hosts_changes().map(|changes| {
            cx.spawn_in(window, async move |this, cx| {
                while changes.recv().await.is_ok() {
                    if this
                        .update_in(cx, |shell, window, cx| shell.open_new_members(window, cx))
                        .is_err()
                    {
                        return;
                    }
                }
            })
        });
        if let Some(mut snapshot) = restored {
            let pane = shell.column.as_ref().and_then(|column| {
                let thread = snapshot.thread.as_ref()?;
                shell
                    .link_of(&thread.host)
                    .map(|_| column.current_pane.clone())
            });
            if pane.is_none() {
                snapshot.without_thread();
            }
            shell
                .window_state
                .update(cx, |state, cx| state.restore(&snapshot, cx));
            shell.panel_shows_terminal = snapshot.panel == NavigationPanel::Terminal;
            if let Some(pane) = pane
                && let Some(thread) = snapshot.thread
            {
                // Bind once, before the baseline can trigger an empty-workspace
                // draft fallback. Validation below never binds it again.
                shell.bind_pane(&pane, &thread, window, cx);
                shell.return_keyboard(window, cx);
                shell.pending_navigation_restore = Some(PendingNavigationRestore {
                    thread,
                    panel: snapshot.panel,
                    panel_open: snapshot.history.contains(&Destination::Panel),
                });
            }
        }
        shell.schedule_navigation_save(cx);
        shell
    }

    pub fn window_state(&self) -> Entity<WindowState> {
        self.window_state.clone()
    }

    /// The workspace of the current pane's host, if any.
    pub fn store(&self, cx: &App) -> Option<Entity<WorkspaceStore>> {
        Some(self.current_link(cx)?.store.clone())
    }

    /// The workspace of the host in this process, while the window is
    /// linked to it.
    pub fn local_store(&self) -> Option<Entity<WorkspaceStore>> {
        Some(self.link_of(&HostKey::Local)?.store.clone())
    }

    pub fn link(&self, cx: &App) -> Option<tcode_client::HostLink> {
        self.current_link(cx)?.link()
    }

    /// Where this window is, for a window that opens once it has closed
    /// ([`ShellSetup::reopen`]).
    pub fn current_place(&self, cx: &App) -> ShellPlace {
        ShellPlace {
            target: self.current_link(cx).map(|link| link.target.clone()),
            navigation: self.navigation_snapshot(cx),
        }
    }

    fn link_of(&self, host: &HostKey) -> Option<&Link> {
        self.links.iter().find(|link| link.key() == *host)
    }

    fn link_of_mut(&mut self, host: &HostKey) -> Option<&mut Link> {
        self.links.iter_mut().find(|link| link.key() == *host)
    }

    /// The link of the current pane's host, which what acts on the window as
    /// a whole acts on.
    fn current_link(&self, cx: &App) -> Option<&Link> {
        self.link_of(&self.column.as_ref()?.current_pane.read(cx).host(cx))
    }

    /// The link whose settings the Settings route shows.
    fn settings_link(&self, cx: &App) -> Option<&Link> {
        self.window_state
            .read(cx)
            .settings_host
            .as_ref()
            .and_then(|host| self.link_of(host))
            .or_else(|| self.current_link(cx))
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

/// This window's shell, for the surfaces inside it that act on the window
/// rather than on themselves (the Hosts rows and their menu).
fn current_shell(cx: &App) -> Option<Entity<AppShell>> {
    cx.try_global::<ShellBackTarget>()?.shell.upgrade()
}

/// The window the platform's gestures arrive at, with its shell.
pub(crate) fn current_window_shell(cx: &App) -> Option<(AnyWindowHandle, Entity<AppShell>)> {
    let target = cx.try_global::<ShellBackTarget>()?;
    Some((target.window, target.shell.upgrade()?))
}

impl AppShell {
    /// The chat timeline while it is the page on screen with nothing layered
    /// over it: a scrolling screenshot of anything else would read back the
    /// palette, a dialog, another page or the right panel over the chat.
    pub(crate) fn capture_chat(&self, cx: &App) -> Option<Entity<ChatView>> {
        let column = self.column.as_ref()?;
        let store = column.page().read(cx).store().read(cx);
        let state = self.window_state.read(cx);
        let on_screen = if state.compact {
            state.destination() == Destination::Thread
        } else {
            let session_id = self.page_thread(cx).map(|thread| thread.session_id);
            state.route() == Route::Chat
                && !store
                    .panel_state_for(session_id.as_deref())
                    .right_panel_open
        };
        (on_screen
            && !state.palette_mounted()
            && self.pending_navigation_restore.is_none()
            && !gpui_base::GlobalState::is_in_deferred_context(cx))
        .then(|| column.chat(cx))
    }
}

/// App-level dispatch also reaches the shell when no child has keyboard focus.
pub(crate) fn navigate_thread(action: &crate::shortcut::NavigateThread, cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    let current = current_thread(cx);
    shell.update(cx, |shell, cx| {
        let sidebar = shell.sidebar.clone();
        if sidebar.update(cx, |sidebar, cx| {
            sidebar.navigate_thread(action, current.as_ref(), cx)
        }) {
            shell.window_state.update(cx, |state, cx| {
                state.close_palette(cx);
                state.go(Destination::Thread, cx);
            });
        }
    });
}

/// Pane commands are app-level, so they reach the shell from anywhere in the
/// window, with nothing focused too, once the dispatch that carried them ends.
pub(crate) fn init_pane_actions(cx: &mut App) {
    fn act(
        cx: &mut App,
        command: impl FnOnce(&mut AppShell, &mut Window, &mut Context<AppShell>) + 'static,
    ) {
        let Some((window, shell)) = current_window_shell(cx) else {
            return;
        };
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                shell.update(cx, |shell, cx| command(shell, window, cx));
            });
        });
    }
    fn act_on_pane(cx: &mut App, command: PaneCommand) {
        act(cx, move |shell, window, cx| {
            if let Some(pane) = shell.acting_pane() {
                shell.run_on_pane(pane, command, window, cx);
            }
        });
    }
    cx.on_action(|_: &SplitRight, cx| {
        act_on_pane(cx, PaneCommand::Split(gpui_base::Placement::Right))
    });
    cx.on_action(|_: &SplitDown, cx| {
        act_on_pane(cx, PaneCommand::Split(gpui_base::Placement::Bottom))
    });
    cx.on_action(|action: &OpenInNewPane, cx| {
        let thread = action.0.clone();
        act(cx, move |shell, window, cx| {
            if let Some(from) = shell.acting_pane() {
                shell.open_in_new_pane(thread, &from, &[gpui_base::Placement::Right], window, cx);
            }
        })
    });
    cx.on_action(|layout: &PaneLayout, cx| {
        let layout = *layout;
        act(cx, move |shell, window, cx| {
            shell.apply_layout(layout, window, cx)
        })
    });
    cx.on_action(|_: &ClosePane, cx| act_on_pane(cx, PaneCommand::Close));
    cx.on_action(|_: &CloseOtherPanes, cx| act_on_pane(cx, PaneCommand::CloseOthers));
    cx.on_action(|_: &ZoomPane, cx| act_on_pane(cx, PaneCommand::Zoom));
    cx.on_action(|swap: &SwapPane, cx| act_on_pane(cx, PaneCommand::Swap(swap.0)));
    cx.on_action(|to: &MoveThreadToPane, cx| act_on_pane(cx, PaneCommand::Move(to.0)));
    cx.on_action(|direction: &FocusPane, cx| {
        let direction = *direction;
        act(cx, move |shell, window, cx| {
            shell.focus_pane(direction, window, cx)
        })
    });
}

pub(crate) fn undo_thread_action(action: &crate::sidebar::ThreadUndo, cx: &mut App) {
    let Some((window, shell)) = current_window_shell(cx) else {
        return;
    };
    let action = action.clone();
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, cx| {
            shell.update(cx, |shell, cx| shell.on_thread_undo(&action, window, cx));
        });
    });
}

/// Ask this window's shell for `request`.
pub(crate) fn request_host(request: HostRequest, window: &mut Window, cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    // The Hosts page and the pairing form ask from their own listeners,
    // and opening or leaving a host updates them in turn.
    window.defer(cx, move |window, cx| {
        shell.update(cx, |shell, cx| match request {
            HostRequest::Open(target) => shell.open_host(target, window, cx),
            HostRequest::Leave(host) => shell.leave_host(&host, window, cx),
            HostRequest::Move { host, earlier } => shell.move_host(&host, earlier, cx),
        });
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

/// Whether the top sheet or a compact page can answer the native edge gesture.
pub fn can_swipe_back(cx: &mut App) -> bool {
    let Some((window, shell)) = current_window_shell(cx) else {
        return false;
    };
    window
        .update(cx, |_, window, cx| {
            crate::overlay::active_sheet(window, cx).is_some() || {
                let shell = shell.read(cx);
                !crate::overlay::dialog_open(window, cx)
                    && shell.compact(cx)
                    && !shell.window_state.read(cx).palette_mounted()
                    && !gpui_base::GlobalState::is_in_deferred_context(cx)
                    && shell.nav.read(cx).depth() > 1
            }
        })
        .unwrap_or(false)
}

/// UIKit supplies continuous translation and velocity; thresholds stay in Rust.
pub fn update_edge_back(phase: TouchPhase, travel: f32, velocity: f32, cx: &mut App) {
    let Some((window, shell)) = current_window_shell(cx) else {
        return;
    };
    let _ = window.update(cx, |_, window, cx| {
        shell.update(cx, |shell, cx| {
            if phase == TouchPhase::Started {
                shell.begin_edge_back(
                    gpui::point(px(0.), px(0.)),
                    window.viewport_size().width,
                    window,
                    cx,
                );
            }
            if let Some(swipe) = &mut shell.back_swipe {
                swipe.progress.travel = px(travel);
                swipe.progress.velocity = velocity;
            }
            shell.move_edge_back(phase, window, cx);
        });
    });
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The window may have been resized without a bounds notification (a
        // first frame, or an inset change that narrowed the content box).
        self.sync_layout(window, cx);
        self.glide_panes(window, cx);
        self.swap_layout(window, cx);
        self.zoom_panes(window, cx);
        self.sync_panes(window, cx);
        if let Some(link) = self.current_link(cx) {
            let session_id = self.page_thread(cx).map(|thread| thread.session_id);
            let title = link
                .store
                .read(cx)
                .shell_window_title_for(session_id.as_deref(), cx);
            if title != self.window_title {
                window.set_window_title(&title);
                self.window_title = title;
            }
        }
        // The palette takes focus on the frame it opens, at either width, and
        // gives it back as it starts to leave.
        let (palette_open, focus_query, palette_leaving) = {
            let state = self.window_state.read(cx);
            (
                state.palette_mounted(),
                state.palette_focuses_query,
                state.palette_leaving(),
            )
        };
        if palette_leaving && self.palette.focus_handle(cx).contains_focused(window, cx) {
            cx.defer_in(window, |this, window, cx| {
                this.return_keyboard(window, cx);
            });
        }
        if palette_open && !self.palette_was_open && self.current_link(cx).is_some() {
            self.palette
                .update(cx, |p, cx| p.open(focus_query, window, cx));
        }
        self.palette_was_open = palette_open;
        let reflow = crate::motion::reflow("shell-layout", window, cx)
            .reflowed(std::mem::take(&mut self.layout_flipped));
        self.layout_reflow = Some(reflow.clone());
        let body = if self.compact(cx) {
            // A compact window's pages slide over the ones they replace, so
            // the window turning wide again draws its route at rest rather
            // than rising over the route it last drew wide.
            self.departed.clear();
            self.attachments = Default::default();
            if let Some(column) = self.column.as_mut() {
                column.routes = Default::default();
            }
            self.render_compact(window, cx)
        } else {
            self.render_wide(window, cx)
        };
        let body = reflow.render(body);
        let body = if crate::window_seam::is_mobile(cx) && !cfg!(target_os = "ios") {
            self.swipe_back_stack(body, cx).into_any_element()
        } else {
            body
        };
        let dismissal = self
            .current_link(cx)
            .map(|_| self.palette.read(cx).outside_dismissal.clone());
        div()
            .size_full()
            // gpui-base transfers focus between controls, but leaves it
            // intact on empty backgrounds. Mobile users need that tap to
            // dismiss the software keyboard too.
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                if crate::window_seam::is_mobile(cx) && !window.default_prevented() {
                    window.blur(cx);
                }
            })
            // Both layouts must select UI text explicitly: GPUI's default
            // fallback stack prefers Lilex once the terminal font is loaded.
            .font_family(cx.theme().font_family.clone())
            // First, so it hears each scroll packet before any area acts on it.
            .child(crate::scroll::gesture_recorder())
            // Phones and tablets have no middle-button autoscroll convention.
            .when(
                !cfg!(any(target_os = "ios", target_os = "android")),
                |this| this.child(crate::scroll::MiddleAutoscroll),
            )
            .children(dismissal.map(|dismissal| dismissal.release_listener().into_any_element()))
            .child(body)
            // Each host's pages, which panes of several hosts show at once.
            .children(
                self.links
                    .iter()
                    .filter_map(|link| link.preview.read(cx).native_fade(cx)),
            )
    }
}

#[cfg(test)]
pub(crate) mod tests;
