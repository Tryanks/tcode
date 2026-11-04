//! One responsive shell, on every client.
//!
//! [`AppShell`] is the window: it owns navigation, the layout rule and the
//! back stack, and it survives everything below it. Beneath it sits at most one
//! *attachment* — a link to a host and the views over that host's workspace.
//! Switching hosts replaces the attachment; it does not replace the window's
//! navigation root, and resizing the window never touches the attachment at all.
//!
//! Layout is decided by one rule (`crate::window_seam`): a desktop build is
//! always the wide split, and a mobile build is a navigation stack — hosts,
//! threads, thread, panel — under 900px of usable width and the wide split at
//! or above it. Wide and unattached, the shell is the hosts page and nothing
//! else until it attaches somewhere.

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
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TouchPhase, WeakEntity,
    Window, actions, canvas, div, prelude::FluentBuilder as _, px,
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
use crate::icon::{Icon, IconName};
use crate::palette::CommandPalette;
use crate::panes::{
    CloseOtherPanes, ClosePane, FocusPane, MoveThreadToPane, OpenInNewPane, PaneLayout, Side,
    SplitDown, SplitRight, SwapPane, ThreadPanel, ZoomPane,
};
use crate::preview_panel::PreviewPanel;
#[cfg(preview_backend)]
use crate::preview_panel::lifecycle::BrowserLifecycle;
use crate::remote::{AttachmentTarget, RemotePanel};
use crate::runtime_event::{
    RuntimeEventSeverity, RuntimeToastAction, RuntimeToastDisposition, apply_runtime_effect,
    present_runtime_event, present_runtime_toast,
};
use crate::settings_page::SettingsPage;
use crate::sidebar::SessionsSidebar;
use crate::store::{ShowThread, StoreChange, TopicKind, WorkspaceStore};
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
pub(crate) const WINDOW_DRAGGABLE: bool = cfg!(not(any(
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
                    Destination::SettingsSection | Destination::SettingsThreadRules => {
                        shell.render_settings_page(destination, window, cx)
                    }
                    Destination::ArrangeThreads => shell.render_arrange_page(window, cx),
                })
            })
            .unwrap_or_else(|| div().into_any_element())
    }
}

/// The views and transport for one host. Everything here is replaced when the
/// window attaches somewhere else, and nothing here is touched by a resize.
struct ShellAttachment {
    link: Link,
    sidebar: Entity<SessionsSidebar>,
    /// The wide layout's chat column, a dock of [`ThreadPanel`]s.
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
    /// The attachment's preview panel: it answers the preview tools and owns
    /// the pages each pane's drawer shows.
    preview: Entity<PreviewPanel>,
    settings_page: Entity<SettingsPage>,
    palette: Entity<CommandPalette>,
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
    /// Whether this attachment's host settings have been adopted once.
    adopted: bool,
    /// Whether this attachment's saved record has been stamped with the time
    /// this window reached it.
    stamped: bool,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// What the shell last handed a pane's chat.
#[derive(Clone, PartialEq)]
struct HandedToChat {
    edges: crate::chat::WindowEdges,
    recedes: bool,
    close: Option<crate::motion::Shown<gpui::WeakEntity<ThreadPanel>>>,
    controls: Option<PaneControls>,
}

/// An attachment's link and the store it fills. The link closes when the
/// window leaves the attachment; the store stays for the views still drawing
/// it.
struct Link {
    target: AttachmentTarget,
    store: Entity<WorkspaceStore>,
    open: Option<Attachment>,
}

impl Link {
    fn link(&self) -> tcode_client::HostLink {
        self.open
            .as_ref()
            .expect("the window's attachment is open")
            .link()
    }

    fn close(&mut self, cx: &mut App) {
        if let Some(open) = self.open.take() {
            open.close(cx).close();
        }
    }
}

impl ShellAttachment {
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
    session_id: String,
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
        .attachment
        .as_ref()
        .map(|attachment| attachment.current_pane.focus_handle(cx))
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
    let Some(chat) = shell.attachment.as_ref().and_then(|attachment| {
        let pane = attachment
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

/// Whether a pane of this window shows `session_id`. The compact layout has
/// no panes.
pub(crate) fn shown_in_pane(session_id: &str, cx: &App) -> bool {
    current_shell(cx).is_some_and(|shell| {
        let shell = shell.read(cx);
        !shell.compact(cx) && shell.pane_showing(session_id, cx).is_some()
    })
}

/// The thread of the current pane, for what acts on the whole window.
pub(crate) fn current_thread(cx: &App) -> Option<String> {
    current_shell(cx)?.read(cx).page_thread(cx)
}

/// Whether a pane of this window shows `session_id`, the compact page
/// included.
pub(crate) fn pane_shows(session_id: &str, cx: &App) -> bool {
    current_shell(cx).is_some_and(|shell| shell.read(cx).pane_showing(session_id, cx).is_some())
}

/// The chat that shows an agent opened from `lead` beside it, for what
/// knows only the lead (the right panel, which shows a thread's
/// agents): the chat of the pane showing `lead`, or else of a pane showing
/// `lead` beside its own lead, which has room for one agent beside it. A
/// thread shown in a pane and beside its lead in another is the pane's.
/// None in the compact layout, which shows one thread at a time.
pub(crate) fn side_host_for(lead: &str, cx: &App) -> Option<Entity<ChatView>> {
    let shell = current_shell(cx)?;
    let shell = shell.read(cx);
    if shell.compact(cx) {
        return None;
    }
    let chats: Vec<_> = shell
        .attachment
        .as_ref()?
        .panes(cx)
        .into_iter()
        .map(|pane| pane.read(cx).chat())
        .collect();
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
    let selected = session_id.clone();
    let opened = current_shell(cx).is_some_and(|shell| {
        shell.update(cx, |shell, cx| shell.open_over_lead(session_id, window, cx))
    });
    if !opened {
        store.update(cx, |store, cx| store.show_thread(selected, cx));
    }
}

/// Whether one of `panes` draws `session_id` beside its own thread, which
/// keeps it open when a pane showing it as its own goes.
fn drawn_beside(panes: &[Entity<ThreadPanel>], session_id: &str, cx: &App) -> bool {
    panes
        .iter()
        .any(|pane| pane.read(cx).chat().read(cx).side_session_id() == Some(session_id))
}

/// What fills one panel of a layout about to be installed.
#[derive(Clone)]
enum Slot {
    Pane(Entity<ThreadPanel>),
    /// A new pane showing this thread.
    Thread(String),
    Waiting,
}

/// What bootstrap tells the shell that is not the window itself.
#[derive(Default)]
pub struct ShellSetup {
    /// This client's identity, saved hosts and preferences.
    pub client_host: Option<Rc<dyn ClientHost>>,
    /// How to reach a host running inside this process. A client that can never
    /// host has none.
    pub local: Option<LocalTransport>,
    /// Why that host did not start at launch. Machines says so from the first
    /// frame, whatever the window attaches to, and the window attaches to that
    /// host only when asked again.
    pub local_failure: Option<String>,
    /// Where to attach at launch; `None` opens on the hosts list.
    pub initial: Option<AttachmentTarget>,
    /// A browser link that could not be paired opens the normal form with the
    /// one-shot failure visible after its URL fragment has been cleared.
    pub initial_pairing_error: Option<String>,
    /// Wait for the first snapshots while attaching. Only desktop bootstrap
    /// asks for it, and only because it applies locale and theme from them
    /// before the first frame; a single-threaded client cannot wait at all.
    pub seed_blocking: bool,
    /// Mobile bootstrap restores client-local navigation. Desktop defaults to
    /// a fresh path; the phone example can opt into the mobile policy.
    pub restore_navigation: bool,
}

struct NavigationPersistence {
    scheduled: Option<NavigationSnapshot>,
    task: Option<Task<()>>,
}

struct PendingNavigationRestore {
    session_id: String,
    panel: NavigationPanel,
    panel_open: bool,
}

pub struct AppShell {
    window_state: Entity<WindowState>,
    setup: ShellSetup,
    attachment: Option<ShellAttachment>,
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
    operation_toasts: HashMap<RuntimeOperationId, ToastId>,
    next_toast_id: ToastId,
    /// Tracks the palette's open state across frames so it can be focused on the
    /// open transition.
    palette_was_open: bool,
    /// The title last given to the OS window. The title is in the current
    /// language, and a language switch reaches the shell only as a redraw.
    window_title: String,
    /// Names what the window is attached to: a new one each time the
    /// attachment is replaced or dropped.
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
    _subscriptions: Vec<Subscription>,
}

/// A workspace the window left, as it was when it left.
struct Departed {
    key: u64,
    attachment: Option<ShellAttachment>,
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
        mut setup: ShellSetup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut restored = setup
            .restore_navigation
            .then(|| {
                setup
                    .client_host
                    .as_ref()?
                    .load_preferences()
                    .navigation
                    .and_then(NavigationSnapshot::from_preferences)
            })
            .flatten();
        if let Some(snapshot) = &mut restored {
            setup.initial = setup.client_host.as_ref().and_then(|host| {
                host.load_hosts()
                    .into_iter()
                    .find(|saved| Some(&saved.host_id) == snapshot.host_id.as_ref())
                    .map(|host| AttachmentTarget::Remote(Box::new(host)))
            });
            if setup.initial.is_none() {
                snapshot.without_host();
            }
        }
        if let Some(host) = setup.client_host.as_ref().and_then(|client| {
            client.load_hosts().into_iter().find(|host| {
                client
                    .outbox_storage(&host.host_id)
                    .and_then(|storage| storage.load().ok())
                    .is_some_and(|entries| !entries.is_empty())
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
            // The one place navigation reaches the stack: whoever moved the
            // history — a nav bar, the sidebar, the palette, Android Back —
            // gets the same push or pop out of it.
            cx.observe_in(&window_state, window, |this, _, window, cx| {
                if this.mounted != this.window_state.read(cx).history() {
                    this.blur_navigation(window, cx);
                }
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
            let mut panel = RemotePanel::new(None, window_state.clone(), window, cx);
            panel.set_pairing_error(setup.initial_pairing_error.clone());
            panel
        });
        let mut shell = Self {
            window_state,
            attachment: None,
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
            _subscriptions: subscriptions,
            setup,
        };
        if let Some(reason) = shell.setup.local_failure.take() {
            shell
                .hosts
                .update(cx, |hosts, cx| hosts.set_local_failure(Some(reason), cx));
            if shell.setup.initial == Some(AttachmentTarget::Local) {
                shell.setup.initial = None;
            }
        }
        if let Some(target) = shell.setup.initial.take() {
            shell.attach(target, window, cx);
        }
        if let Some(snapshot) = restored {
            shell
                .window_state
                .update(cx, |state, cx| state.restore(&snapshot, cx));
            shell.panel_shows_terminal = snapshot.panel == NavigationPanel::Terminal;
            if let Some(session_id) = snapshot.session_id
                && let Some(pane) = shell
                    .attachment
                    .as_ref()
                    .map(|attachment| attachment.current_pane.clone())
            {
                // Bind once, before the baseline can trigger an empty-workspace
                // draft fallback. Validation below never binds it again.
                shell.bind_pane(&pane, &session_id, window, cx);
                shell.return_keyboard(window, cx);
                shell.pending_navigation_restore = Some(PendingNavigationRestore {
                    session_id,
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

    #[cfg(preview_backend)]
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
    fn navigation_snapshot(&self, cx: &App) -> NavigationSnapshot {
        let store = self.store();
        let host_id = store
            .as_ref()
            .and_then(|store| store.read(cx).remote_host_id().map(str::to_owned));
        let session_id = self.page_thread(cx);
        let panel = self
            .pending_navigation_restore
            .as_ref()
            .map(|pending| pending.panel)
            .unwrap_or_else(|| {
                if self.panel_shows_terminal {
                    NavigationPanel::Terminal
                } else {
                    match store.as_ref().map(|store| {
                        store
                            .read(cx)
                            .panel_state_for(session_id.as_deref())
                            .right_tab
                    }) {
                        Some(RightTab::Plan) => NavigationPanel::Plan,
                        Some(RightTab::Agents) => NavigationPanel::Agents,
                        Some(RightTab::PullRequests) => NavigationPanel::PullRequests,
                        Some(RightTab::Preview) => NavigationPanel::Preview,
                        _ => NavigationPanel::Diff,
                    }
                }
            });
        NavigationSnapshot {
            history: self.window_state.read(cx).history().to_vec(),
            host_id,
            session_id,
            panel,
        }
    }

    fn schedule_navigation_save(&mut self, cx: &mut Context<Self>) {
        if self.navigation.is_none() {
            return;
        }
        let Some(host) = self.setup.client_host.clone() else {
            return;
        };
        let snapshot = self.navigation_snapshot(cx);
        let navigation = self.navigation.as_mut().expect("enabled above");
        if navigation.scheduled.as_ref() == Some(&snapshot) {
            return;
        }
        navigation.scheduled = Some(snapshot.clone());
        navigation.task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let mut preferences = host.load_preferences();
            let member_settings = preferences
                .navigation
                .as_ref()
                .and_then(|navigation| navigation.get("member_settings"))
                .cloned();
            preferences.navigation = serde_json::to_value(snapshot).ok();
            if let Some(member_settings) = member_settings {
                preferences.navigation.as_mut().unwrap()["member_settings"] = member_settings;
            }
            host.save_preferences(&preferences);
        }));
    }

    fn reconcile_navigation_restore(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = &self.pending_navigation_restore else {
            return;
        };
        let Some(store) = self.store() else {
            self.pending_navigation_restore = None;
            return;
        };
        let shown = self.page_thread(cx);
        if !store.read(cx).index_hydrated() {
            if shown.as_deref() != Some(&pending.session_id) {
                self.pending_navigation_restore = None;
            }
            return;
        }
        if !store
            .read(cx)
            .sidebar_sessions()
            .iter()
            .any(|meta| meta.id == pending.session_id && meta.archived_at.is_none())
        {
            let pending = self
                .pending_navigation_restore
                .take()
                .expect("checked above");
            if shown.as_deref() == Some(&pending.session_id) {
                store.update(cx, |store, cx| store.close_thread(&pending.session_id, cx));
            }
            self.window_state
                .update(cx, |state, cx| state.discard_restored_thread(cx));
            return;
        }
        if shown.as_deref() != Some(&pending.session_id) {
            // A user selection made while syncing supersedes restoration.
            self.pending_navigation_restore = None;
            return;
        }
        if store.read(cx).chat_loading_for(shown.as_deref(), cx) {
            return;
        }
        let pending = self
            .pending_navigation_restore
            .take()
            .expect("checked above");
        if pending.panel_open {
            self.show_panel(pending.panel.id(), cx);
        } else {
            let tab = match pending.panel {
                NavigationPanel::Plan => RightTab::Plan,
                NavigationPanel::Agents => RightTab::Agents,
                NavigationPanel::PullRequests => RightTab::PullRequests,
                NavigationPanel::Preview => RightTab::Preview,
                _ => RightTab::Diff,
            };
            store.update(cx, |store, cx| {
                store.set_right_tab_for(Some(&pending.session_id), tab, cx)
            });
        }
    }

    /// Point this window at `target`, replacing the attachment beneath the
    /// navigation root. The window, its stack and its overlays stay put.
    pub fn switch_to(
        &mut self,
        target: AttachmentTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Already there: go to that host's workspace instead of tearing its
        // link down and building the same one again. A link the machine
        // refused is not reused: it stopped reconnecting, and re-pairing
        // lands here with the same machine.
        if self.attachment.as_ref().is_some_and(|current| {
            same_target(&current.link.target, &target)
                && !matches!(
                    current.link.store.read(cx).connection_state(),
                    tcode_client::ConnectionState::Offline { reason } if reason.is_terminal()
                )
        }) {
            self.go(Destination::Threads, cx);
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
        self.blur_navigation(window, cx);
        self.attach(target, window, cx);
    }

    /// Leave the current host without opening another one. The host keeps
    /// running; only this window's link to it closes.
    pub fn detach(&mut self, cx: &mut Context<Self>) {
        let Some(attachment) = self.attachment.take() else {
            return;
        };
        self.retire(Some(attachment), cx);
        self.hosts.update(cx, |hosts, cx| hosts.set_store(None, cx));
        self.window_state
            .update(cx, |state, cx| state.leave_workspace(cx));
        cx.notify();
    }

    /// Close what the window was attached to, if anything, and, in the wide
    /// layout, keep its views on screen, inert, for the next one to rise
    /// over. Its preview's page goes at once: a native view draws over
    /// everything, the workspace rising over it included.
    fn retire(&mut self, old: Option<ShellAttachment>, cx: &mut Context<Self>) {
        let old = old.map(|mut old| {
            old.link.close(cx);
            old._subscriptions.clear();
            old._tasks.clear();
            for pane in old.all_panes(cx) {
                pane.update(cx, |pane, cx| pane.release_preview(cx));
            }
            old
        });
        if !self.compact(cx) {
            self.departed.push(Departed {
                key: self.scene_key,
                attachment: old,
                route: self.window_state.read(cx).route(),
                destination: self.destination(cx),
            });
        }
        self.scene_key += 1;
    }

    fn attach(&mut self, target: AttachmentTarget, window: &mut Window, cx: &mut Context<Self>) {
        // A local host that does not start is an attachment failure like any
        // other: Hosts shows why, and whatever this window was on stays.
        let local = match (&target, &self.setup.local) {
            (AttachmentTarget::Local, Some(start)) => match start(cx) {
                Ok(transport) => Some(transport),
                Err(reason) => {
                    log::error!("the local host is unavailable: {reason}");
                    self.hosts
                        .update(cx, |hosts, cx| hosts.set_local_failure(Some(reason), cx));
                    cx.notify();
                    return;
                }
            },
            _ => None,
        };
        if local.is_some() {
            self.hosts
                .update(cx, |hosts, cx| hosts.set_local_failure(None, cx));
        }
        self.pending_navigation_restore = None;
        let old = self.attachment.take();
        self.retire(old, cx);
        let Some(link) = Attachment::open(
            target,
            local,
            self.setup.client_host.clone(),
            self.setup.seed_blocking,
            cx,
        ) else {
            log::error!("this client cannot reach the requested host");
            self.window_state
                .update(cx, |state, cx| state.leave_workspace(cx));
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
        let pending_session = store
            .read(cx)
            .pending_sessions()
            .first()
            .map(|(id, _)| id.clone());
        let preview =
            cx.new(|cx| PreviewPanel::new(store.clone(), self.window_state.clone(), window, cx));
        let sidebar =
            cx.new(|cx| SessionsSidebar::new(store.clone(), self.window_state.clone(), cx));
        let mut subscriptions = vec![
            cx.observe(&store, |this, _, cx| {
                this.reconcile_panes(cx);
                this.stamp_connected(cx);
                this.reconcile_navigation_restore(cx);
                this.schedule_navigation_save(cx);
                cx.notify();
            }),
            cx.subscribe_in(&store, window, |this, _, event: &RuntimeEvent, w, cx| {
                this.present_app_event(event, w, cx);
            }),
            cx.subscribe_in(&store, window, |this, _, shown: &ShowThread, w, cx| {
                this.show_thread(shown, w, cx);
            }),
            cx.subscribe_in(
                &store,
                window,
                |this, _, change: &StoreChange, window, cx| {
                    if change.topic == TopicKind::Settings {
                        this.adopt_host_settings(window, cx);
                    }
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

        let thread_panel = self.new_pane(&store, &preview, None, window, cx);
        let shell = cx.entity().downgrade();
        let drop_landing = Rc::new(Cell::new(None));
        let drop_target: crate::panes::DropTarget = Rc::new({
            let drop_landing = drop_landing.clone();
            move |window, cx| {
                let target = shell.upgrade()?.read(cx).drop_target(window, cx);
                drop_landing.set(target);
                target
            }
        });
        let split_handle_held = crate::panes::HandleHeld::default();
        let thread_panes = cx.new(|cx| {
            let mut area = DockArea::new("thread-panes", None, window, cx).with_renderer(Rc::new(
                crate::panes::ThreadPanes::new(drop_target, split_handle_held.clone()),
            ));
            area.set_center(DockLayout::tabs().panel(thread_panel.clone()), window, cx);
            area
        });
        subscriptions.extend(PreviewPanel::observe_native_fade(&preview, cx));
        subscriptions.push(cx.subscribe_in(
            &thread_panes,
            window,
            |this, _, event: &DockEvent, window, cx| {
                if let DockEvent::DragDrop { item, target } = event
                    && let Some(session_id) = crate::sidebar::thread_carried(item)
                {
                    this.drop_thread(session_id.to_owned(), *target, window, cx);
                }
                // The threads list draws a map of the panes.
                if matches!(event, DockEvent::LayoutChanged) {
                    cx.notify();
                }
            },
        ));
        self.attachment = Some(ShellAttachment {
            thread_panes,
            glides: Default::default(),
            window_seen: None,
            zoom: Default::default(),
            layout_swap: Default::default(),
            routes: Default::default(),
            thread_body: Default::default(),
            thread_body_shown: None,
            leads: Vec::new(),
            leaving: Vec::new(),
            split_handle_held,
            drop_landing,
            held_split: None,
            current_pane: thread_panel,
            handed: HashMap::new(),
            settings_page: cx
                .new(|cx| SettingsPage::new(store.clone(), self.window_state.clone(), window, cx)),
            palette: cx.new(|cx| {
                CommandPalette::new(store.clone(), self.window_state.clone(), window, cx)
            }),
            preview,
            sidebar,
            split: cx.new(|_| ResizableState::default()),
            panel_glide: crate::motion::resize_glide::ResizeGlide::new(
                "workspace-panels",
                gpui::Axis::Horizontal,
            ),
            sidebar_width: Rc::new(Cell::new(px(SIDEBAR_WIDTH))),
            sidebar_overlay_visible: false,
            adopted: false,
            stamped: false,
            link: Link {
                target: link.target.clone(),
                store: link.store.clone(),
                open: Some(link),
            },
            _tasks: vec![preview_pump],
            _subscriptions: subscriptions,
        });
        self.hosts
            .update(cx, |hosts, cx| hosts.set_store(Some(store.clone()), cx));
        self.adopt_host_settings(window, cx);
        // The previous host's pages are not this host's: land on its threads.
        self.window_state
            .update(cx, |state, cx| state.enter_workspace(cx));
        if let Some(id) = pending_session {
            store.update(cx, |store, cx| store.show_thread(id, cx));
        }
        cx.notify();
    }

    fn new_pane(
        &self,
        store: &Entity<WorkspaceStore>,
        preview: &Entity<PreviewPanel>,
        session_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ThreadPanel> {
        let drawer = crate::panes::Drawer::new(
            store.clone(),
            self.window_state.clone(),
            preview.clone(),
            session_id.as_deref(),
            cx,
        );
        let chat = cx.new(|cx| {
            ChatView::new(
                store.clone(),
                self.window_state.clone(),
                session_id,
                window,
                cx,
            )
        });
        let shell = cx.entity().downgrade();
        cx.new(|cx| {
            ThreadPanel::new(
                chat,
                drawer,
                move |pane, _, cx| {
                    _ = shell.update(cx, |shell, cx| shell.pane_entered(pane, cx));
                },
                window,
                cx,
            )
        })
    }

    /// A pane whose thread the store closed leaves the column, and the last
    /// one, on a compact window the page, takes what the store says follows
    /// that thread.
    fn reconcile_panes(&mut self, cx: &mut Context<Self>) {
        let compact = self.compact(cx);
        if compact {
            self.reconcile_leads(cx);
        }
        let Some(attachment) = &self.attachment else {
            return;
        };
        let panes = attachment.panes(cx);
        let store = attachment.link.store.clone();
        let current_pane = attachment.current_pane.clone();
        let mut closed: Vec<_> = panes
            .iter()
            .filter(|pane| {
                pane.read(cx)
                    .session_id()
                    .is_some_and(|session_id| !store.read(cx).is_thread_open(session_id))
            })
            .cloned()
            .collect();
        // What stays is the pane in use, of those the store emptied.
        closed.sort_by_key(|pane| *pane == current_pane);
        let mut remaining = panes.len();
        for pane in closed {
            if remaining == 1 || (compact && pane == current_pane) {
                let left = pane.read(cx).session_id().map(str::to_owned);
                let follows = left.is_some_and(|left| {
                    store.update(cx, |store, cx| store.show_after_left(&left, cx))
                });
                if !follows {
                    self.drop_leads(0, cx);
                    pane.update(cx, |pane, cx| pane.set_session(None, cx));
                }
            } else {
                if let Some(attachment) = self.attachment.as_mut()
                    && attachment.zoom.pane() == Some(&pane)
                {
                    attachment.zoom.zoom_out();
                }
                self.glide_out(&pane, cx);
                remaining -= 1;
            }
        }
        if let Some(attachment) = self.attachment.as_mut() {
            let panes = attachment.panes(cx);
            if !panes.contains(&attachment.current_pane)
                && let Some(first) = panes.first()
            {
                attachment.current_pane = first.clone();
            }
        }
    }

    /// The pointer pressed in `pane` or the keyboard entered it, its drawer
    /// included: it is the current pane.
    fn pane_entered(&mut self, pane: Entity<ThreadPanel>, cx: &mut Context<Self>) {
        if self
            .attachment
            .as_ref()
            .is_none_or(|attachment| !attachment.panes(cx).contains(&pane))
        {
            return;
        }
        self.set_current_pane(&pane, cx);
        cx.notify();
    }

    /// `pane` is the current pane. Coming from another pane, the user has
    /// moved on: a draft still in flight for that one lands nowhere.
    fn set_current_pane(&mut self, pane: &Entity<ThreadPanel>, cx: &mut Context<Self>) {
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        if attachment.current_pane != *pane {
            attachment.current_pane = pane.clone();
            attachment
                .link
                .store
                .update(cx, |store, _| store.selection_moved_on());
        }
    }

    /// The only rebind of a pane: it shows `session_id`, open from here on,
    /// and the thread it showed closes unless a side card shows that too.
    fn bind_pane(
        &mut self,
        pane: &Entity<ThreadPanel>,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attachment) = &self.attachment else {
            return;
        };
        let store = attachment.link.store.clone();
        let panes = attachment.panes(cx);
        store.update(cx, |store, cx| {
            if !store.is_thread_open(session_id) {
                store.open_thread(session_id, cx);
                cx.notify();
            }
        });
        let left = pane.read(cx).session_id().map(str::to_owned);
        if left.as_deref() != Some(session_id) {
            pane.update(cx, |pane, cx| {
                pane.set_session(Some(session_id.to_owned()), cx)
            });
            // A pane that waited with the keyboard had no composer to give it
            // to until now.
            if left.is_none() && pane.focus_handle(cx).is_focused(window) {
                pane.read(cx)
                    .chat()
                    .update(cx, |chat, cx| chat.focus_composer(window, cx));
            }
            if let Some(left) = left
                && store.read(cx).is_thread_open(&left)
                && !drawn_beside(&panes, &left, cx)
            {
                store.update(cx, |store, cx| store.close_thread(&left, cx));
            }
        }
    }

    /// Where a thread shown without naming a pane goes: the pane showing it,
    /// or beside its lead; else `replacing`'s pane, the current pane, or the
    /// first pane.
    fn place(
        &self,
        session_id: &str,
        replacing: Option<&str>,
        cx: &App,
    ) -> Option<Entity<ThreadPanel>> {
        let attachment = self.attachment.as_ref()?;
        let panes = attachment.panes(cx);
        self.pane_showing(session_id, cx)
            .or_else(|| {
                panes
                    .iter()
                    .find(|pane| {
                        pane.read(cx).chat().read(cx).side_session_id() == Some(session_id)
                    })
                    .cloned()
            })
            .or_else(|| self.pane_showing(replacing?, cx))
            .or_else(|| {
                panes
                    .contains(&attachment.current_pane)
                    .then(|| attachment.current_pane.clone())
            })
            .or_else(|| panes.first().cloned())
    }

    /// Put `shown` in its pane.
    fn show_thread(&mut self, shown: &ShowThread, window: &mut Window, cx: &mut Context<Self>) {
        let session_id = shown.session_id.as_str();
        if self.compact(cx) {
            self.unwind_leads(session_id, cx);
        }
        if let Some(pane) = self.place(session_id, shown.replacing.as_deref(), cx) {
            self.show_in_pane(&pane, session_id, shown.asked, window, cx);
        }
    }

    /// `pane` shows `session_id` and is the current pane. A thread the user
    /// `asked` for takes the compact stack and the keyboard; one the
    /// workspace went to by itself leaves them, though a lone pane's composer
    /// takes a keyboard nothing holds.
    fn show_in_pane(
        &mut self,
        pane: &Entity<ThreadPanel>,
        session_id: &str,
        asked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .pending_navigation_restore
            .as_ref()
            .is_some_and(|pending| pending.session_id != session_id)
        {
            self.pending_navigation_restore = None;
        }
        let compact = self.compact(cx);
        let chat = pane.read(cx).chat();
        let beside = chat.read(cx).side_session_id() == Some(session_id);
        if !beside {
            self.bind_pane(pane, session_id, window, cx);
        }
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        if attachment.zoom.pane().is_some_and(|zoomed| zoomed != pane) {
            attachment.zoom.zoom_out();
        }
        self.set_current_pane(pane, cx);
        self.blur_navigation(window, cx);
        self.window_state
            .update(cx, |state, cx| state.leave_route_for_chat(cx));
        if asked && compact {
            self.go(Destination::Thread, cx);
        }
        let unheld = window.focused(cx).is_none()
            && self
                .attachment
                .as_ref()
                .is_some_and(|attachment| attachment.panes(cx).len() == 1);
        if asked || unheld {
            chat.update(cx, |chat, cx| {
                if beside {
                    chat.focus_side(window, cx);
                } else {
                    chat.focus_composer(window, cx);
                }
            });
        }
        // Showing another thread can leave the destination unchanged. Its
        // checkpoint must not depend on the host delivering a notification.
        self.schedule_navigation_save(cx);
        cx.notify();
    }

    /// Record on the saved record when this window actually reached the host,
    /// so its row in Hosts can say how recently it worked. Only a live
    /// connection counts; an attempt that never came up is not one.
    fn stamp_connected(&mut self, cx: &mut Context<Self>) {
        let Some(attachment) = &mut self.attachment else {
            return;
        };
        let AttachmentTarget::Remote(host) = &attachment.link.target else {
            return;
        };
        if attachment.stamped
            || !attachment
                .link
                .store
                .read(cx)
                .connection_state()
                .is_connected()
        {
            return;
        }
        attachment.stamped = true;
        if let Some(client) = cx.try_global::<crate::remote::ClientAttachment>() {
            // The timestamp mutation must share the transport's persistence
            // transaction so it cannot restore a pre-migration address.
            client
                .host()
                .stamp_connected(&host.host_id, crate::time::now_secs());
        }
    }

    /// Take the attached host's language, theme and sidebar state once its
    /// settings exist. Client preferences already override them inside the
    /// store, so this is the same value the settings page shows.
    fn adopt_host_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attachment) = &mut self.attachment else {
            return;
        };
        let store = attachment.link.store.read(cx);
        if (attachment.adopted && store.scope().is_full()) || !store.settings_hydrated() {
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
// Panes
// ---------------------------------------------------------------------------

impl AppShell {
    /// The keyboard has nowhere left to be, or a thread was opened without
    /// naming a pane: it goes to the current pane, into its composer as soon
    /// as that is drawn. A route without the conversation keeps out of it.
    fn return_keyboard(&self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.window_state.read(cx);
        let page = !state.compact || state.destination() == Destination::Thread;
        if state.route() != Route::Chat || !page || crate::window_seam::is_mobile(cx) {
            return;
        }
        let Some(attachment) = &self.attachment else {
            return;
        };
        let pane = attachment.current_pane.clone();
        if pane.read(cx).session_id().is_some() {
            let chat = pane.read(cx).chat();
            chat.update(cx, |chat, cx| chat.focus_composer(window, cx));
        } else {
            window.focus(&pane.focus_handle(cx), cx);
        }
    }

    /// The pane a window-level command acts on.
    /// The pane commands `pane` offers now; none in the compact layout.
    fn pane_controls(
        &self,
        pane: &Entity<ThreadPanel>,
        window: &Window,
        cx: &App,
    ) -> Option<PaneControls> {
        if self.compact(cx) {
            return None;
        }
        let attachment = self.attachment.as_ref()?;
        let panes = attachment.panes(cx);
        let closable = pane
            .read(cx)
            .group()
            .is_some_and(|group| group.read(cx).is_closable(cx));
        let zoomed = attachment.zoom.is_zoomed();
        let split = |placement| self.split_allowed(pane, placement, window, cx);
        // The panes beneath a zoomed one, or one on its way back to its slot,
        // are covered, so it has no neighbours.
        let covered = attachment.zoom.pane().is_some();
        let neighbours =
            Side::ALL.map(|side| !covered && self.neighbour_of(pane, side, cx).is_some());
        Some(PaneControls {
            split_right: split(gpui_base::Placement::Right),
            split_down: split(gpui_base::Placement::Bottom),
            close: panes.len() > 1 && (closable || zoomed),
            close_others: panes.len() > 1,
            zoom: panes.len() > 1,
            zoomed,
            neighbours,
            moves: pane.read(cx).session_id().is_some(),
        })
    }

    fn acting_pane(&self) -> Option<Entity<ThreadPanel>> {
        Some(self.attachment.as_ref()?.current_pane.clone())
    }

    /// The thread of the pane last in use: the compact page's, and what the
    /// window as a whole is about.
    fn page_thread(&self, cx: &App) -> Option<String> {
        let page = self.attachment.as_ref()?.page();
        page.read(cx).session_id().map(str::to_owned)
    }

    /// The pane showing `session_id`.
    fn pane_showing(&self, session_id: &str, cx: &App) -> Option<Entity<ThreadPanel>> {
        self.attachment
            .as_ref()?
            .panes(cx)
            .into_iter()
            .find(|pane| pane.read(cx).session_id() == Some(session_id))
    }

    /// A pane on `placement`'s side of `from`, with the keyboard and waiting
    /// for a thread.
    fn add_pane(
        &mut self,
        from: &Entity<ThreadPanel>,
        placement: gpui_base::Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(Entity<ThreadPanel>, Option<String>)> {
        if self.compact(cx) || !self.split_allowed(from, placement, window, cx) {
            return None;
        }
        let attachment = self.attachment.as_ref()?;
        let node = from.read(cx).group()?.read(cx).node();
        let beside = from.read(cx).session_id().map(str::to_owned);
        let store = attachment.link.store.clone();
        let preview = attachment.preview.clone();
        let area = attachment.thread_panes.clone();
        let pane = self.new_pane(&store, &preview, None, window, cx);
        self.attachment.as_mut()?.zoom.zoom_out();
        let before = self.glide_before_edit(window, cx);
        area.update(cx, |area, cx| {
            // `DockArea::split_at` splits only a panel the area already
            // holds, and base has no other way to register one: the pane
            // joins the area, then moves beside `node`.
            area.add_panel(pane.clone(), DockPlacement::Center, None, window, cx);
            area.move_panel(
                PanelId::from(pane.entity_id()),
                InsertTarget::Split {
                    node,
                    placement,
                    size: None,
                },
                window,
                cx,
            );
        });
        self.glide_after_edit(before, cx);
        self.set_current_pane(&pane, cx);
        window.focus(&pane.focus_handle(cx), cx);
        cx.notify();
        Some((pane, beside))
    }

    /// Split `from`: the new pane shows a fresh draft in the same project,
    /// and the thread split from stays open.
    fn split_pane(
        &mut self,
        from: &Entity<ThreadPanel>,
        placement: gpui_base::Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, beside)) = self.add_pane(from, placement, window, cx) else {
            return;
        };
        if let Some(store) = self.store() {
            store.update(cx, |store, cx| {
                store.start_draft_beside(beside.as_deref(), cx);
            });
        }
    }

    /// Show `session_id` in a pane of its own, on the first of `placements`
    /// beside `from` that the column has room for; the pane already showing
    /// it takes the keyboard instead. False when no pane shows it: there was no
    /// room, or the layout has no panes.
    fn open_in_new_pane(
        &mut self,
        session_id: String,
        from: &Entity<ThreadPanel>,
        placements: &[gpui_base::Placement],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(store) = self.store().filter(|_| !self.compact(cx)) else {
            return false;
        };
        if self.pane_showing(&session_id, cx).is_some() {
            store.update(cx, |store, cx| store.show_thread(session_id, cx));
            return true;
        }
        let added = placements
            .iter()
            .find_map(|placement| self.add_pane(from, *placement, window, cx));
        if let Some((pane, _)) = &added {
            self.show_in_pane(pane, &session_id, true, window, cx);
        }
        added.is_some()
    }

    /// A thread dragged from the threads list landed on a pane: an edge opens
    /// it in a new pane there, the pane itself shows it in place of its own.
    fn drop_thread(
        &mut self,
        session_id: String,
        target: gpui_base::dock::DropTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(attachment) = self.attachment.as_ref() else {
            return;
        };
        let Some(pane) = attachment.panes(cx).into_iter().find(|pane| {
            pane.read(cx)
                .group()
                .is_some_and(|group| group.read(cx).node() == target.node())
        }) else {
            return;
        };
        // The thread's ghost goes into the drop's placeholder, which shows
        // where the thread opens.
        let into = attachment.drop_landing.take();
        match target.placement() {
            Some(placement) => {
                if !self.open_in_new_pane(session_id, &pane, &[placement], window, cx) {
                    return;
                }
            }
            None => {
                let pane = self.pane_showing(&session_id, cx).unwrap_or(pane);
                self.show_in_pane(&pane, &session_id, true, window, cx);
            }
        }
        if let Some(into) = into {
            crate::motion::drag_ghost::land_in(into, window, cx);
        }
    }

    /// Where a thread dragged over the column would land, in window
    /// coordinates: the part of the hovered pane the dock's drop names, if it
    /// lands.
    fn drop_target(&self, window: &Window, cx: &App) -> Option<gpui::Bounds<Pixels>> {
        self.attachment.as_ref()?.panes(cx).iter().find_map(|pane| {
            let indicator = pane
                .read(cx)
                .group()?
                .read(cx)
                .context(cx)
                .drop_indicator()?;
            let (bounds, to) = (indicator.bounds(), indicator.to());
            self.drop_lands(bounds, indicator.placement(), window, cx)
                .then(|| gpui::Bounds::new(bounds.origin + to.origin(), to.size()))
        })
    }

    /// Whether a thread dragged over the pane painted at `bounds` would land:
    /// one a pane already shows only takes the focus, and an edge needs room
    /// to split.
    fn drop_lands(
        &self,
        bounds: gpui::Bounds<Pixels>,
        placement: Option<gpui_base::Placement>,
        window: &Window,
        cx: &App,
    ) -> bool {
        let Some(attachment) = self.attachment.as_ref() else {
            return false;
        };
        let Some(session_id) = attachment.sidebar.read(cx).dragged_session() else {
            return false;
        };
        // The panes beneath a zoomed one are covered, and it is in no tab
        // group to drop on.
        if attachment.zoom.pane().is_some() || self.pane_showing(session_id, cx).is_some() {
            return false;
        }
        attachment
            .panes(cx)
            .into_iter()
            .find(|pane| pane.read(cx).bounds().contains(&bounds.center()))
            .is_some_and(|pane| {
                placement.is_none_or(|placement| self.split_allowed(&pane, placement, window, cx))
            })
    }

    /// Close `pane`; its thread closes and stays in the threads list. The
    /// keyboard moves to a neighbour at once and the pane glides shut. The
    /// last pane stays.
    fn close_pane(
        &mut self,
        pane: &Entity<ThreadPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compact(cx) {
            return;
        }
        self.close_pane_into(
            pane,
            |panes, at| {
                // Base hands the keyboard on only when it removes the group,
                // which the glide puts off; this is the order it would pick in:
                // the panes after this one in the layout, then those before it,
                // nearest first.
                panes[at + 1..]
                    .first()
                    .or_else(|| panes[..at].last())
                    .cloned()
            },
            window,
            cx,
        );
    }

    /// Close `pane` as [`Self::close_pane`] does, handing the keyboard to the
    /// pane `next` picks from the layout's panes and `pane`'s place among
    /// them. False where `pane` cannot close: it is the last one, or its
    /// group will not close.
    fn close_pane_into(
        &mut self,
        pane: &Entity<ThreadPanel>,
        next: impl FnOnce(&[Entity<ThreadPanel>], usize) -> Option<Entity<ThreadPanel>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(attachment) = self.attachment.as_mut() else {
            return false;
        };
        let Some(group) = pane.read(cx).group() else {
            return false;
        };
        attachment.zoom.zoom_out();
        let panes = attachment.panes(cx);
        if panes.len() < 2 || !group.read(cx).is_closable(cx) {
            return false;
        }
        let at = panes.iter().position(|held| held == pane).unwrap_or(0);
        let next = next(&panes, at);
        self.glide_out(pane, cx);
        if let Some(next) = next {
            window.focus(&next.focus_handle(cx), cx);
            self.pane_entered(next, cx);
        }
        true
    }

    /// Start `pane` leaving the column: it turns inert and its slot glides
    /// shut, after which [`Self::remove_closed`] takes it out.
    fn glide_out(&mut self, pane: &Entity<ThreadPanel>, cx: &mut Context<Self>) {
        pane.update(cx, |pane, cx| pane.set_closing(cx));
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        let area = attachment.thread_panes.read(cx);
        let size = area.bounds().size;
        if let Some(tree) = area.layout(DockPlacement::Center).cloned() {
            attachment
                .glides
                .close(&tree, size, PanelId::from(pane.entity_id()));
        }
        cx.notify();
    }

    /// Take out the closing panes whose split has stopped gliding, closing
    /// the threads they showed unless a pane shows them again by now.
    fn remove_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attachment) = self.attachment.as_ref() else {
            return;
        };
        let area = attachment.thread_panes.clone();
        let store = attachment.link.store.clone();
        let panes = attachment.panes(cx);
        for pane in attachment.closed_panes(cx) {
            let session_id = pane.read(cx).session_id().map(str::to_owned);
            area.update(cx, |area, cx| area.remove_panel(pane, window, cx));
            if let Some(session_id) = session_id
                && store.read(cx).is_thread_open(&session_id)
                && self.pane_showing(&session_id, cx).is_none()
                && !drawn_beside(&panes, &session_id, cx)
            {
                store.update(cx, |store, cx| store.close_thread(&session_id, cx));
            }
        }
    }

    /// Before an edit that adds panes: hand the dock the sizes the glides are
    /// heading for, which is what it divides, and note the layout the new
    /// panes glide out of.
    fn glide_before_edit(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<crate::panes::Before> {
        let attachment = self.attachment.as_ref()?;
        let area = attachment.thread_panes.clone();
        let (targets, before) = {
            let area = area.read(cx);
            let tree = area.layout(DockPlacement::Center)?;
            let size = area.bounds().size;
            (
                attachment.glides.targets(tree, size),
                attachment.glides.before(tree, size),
            )
        };
        area.update(cx, |area, cx| {
            for (node, sizes) in targets {
                area.set_split_sizes(node, sizes, window, cx);
            }
        });
        Some(before)
    }

    /// After an edit that added panes: glide them in out of `before`.
    fn glide_after_edit(&mut self, before: Option<crate::panes::Before>, cx: &mut Context<Self>) {
        let Some(before) = before else {
            return;
        };
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        let area = attachment.thread_panes.read(cx);
        let size = area.bounds().size;
        if let Some(tree) = area.layout(DockPlacement::Center).cloned() {
            attachment.glides.begin(&before, &tree, size);
        }
        cx.notify();
    }

    /// This frame of the column's glides: each gliding split's sizes go to
    /// the dock, each pane's content is held at the size its slot glides
    /// toward, and the closing panes whose split has landed are taken out
    /// before the dock draws them. A split's handle under the pointer stops
    /// that split's glide where it is, and the compact layout, which draws no
    /// panes, lands them. A window that jumped to another size since the
    /// last frame glides the seams there (see [`crate::panes::Glides::resize`]);
    /// one whose edge is dragged is followed at once.
    fn glide_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = self.compact(cx);
        let column_shown = !compact && self.window_state.read(cx).route() == Route::Chat;
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        let handle_held = attachment.split_handle_held.take();
        if handle_held.is_none() {
            attachment.held_split = None;
        }
        let seen = (
            window.viewport_size(),
            (window.is_maximized(), window.is_fullscreen()),
        );
        let jumped = std::mem::replace(&mut attachment.window_seen, column_shown.then_some(seen))
            .filter(|_| column_shown)
            .filter(|(size, state)| crate::panes::window_jumped(*size, seen.0, *state != seen.1))
            .map(|(size, _)| (seen.0.width - size.width, seen.0.height - size.height));
        let panes = attachment.all_panes(cx);
        let closing = panes.iter().any(|pane| pane.read(cx).is_closing());
        if attachment.glides.is_idle() && !closing && jumped.is_none() {
            return;
        }
        let area = attachment.thread_panes.clone();
        let (tree, size) = {
            let area = area.read(cx);
            (
                area.layout(DockPlacement::Center).cloned(),
                area.bounds().size,
            )
        };
        let Some(tree) = tree else {
            return;
        };
        // The column takes what the window gained or lost; it is laid out
        // at its new size only later in this frame.
        if let Some((wider, taller)) = jumped
            && size.width > px(0.)
        {
            let now = gpui::size(size.width + wider, size.height + taller);
            attachment.glides.resize(&tree, size, now);
        }
        if let Some((axis, at)) = handle_held
            && attachment.held_split.is_none()
        {
            let painted = |panel| {
                panes
                    .iter()
                    .find(|pane| PanelId::from(pane.entity_id()) == panel)
                    .map(|pane| pane.read(cx).bounds())
            };
            attachment.held_split = crate::panes::held_split(tree.root(), axis, at, &painted);
        }
        if let Some(split) = attachment.held_split {
            attachment.glides.stop(split);
        }
        let frame = if compact {
            attachment.glides.land(&tree, size)
        } else {
            attachment.glides.sample(&tree, size, window, cx)
        };
        area.update(cx, |area, cx| {
            for (node, sizes) in frame.sizes {
                area.set_split_sizes(node, sizes, window, cx);
            }
        });
        for pane in &panes {
            let held = frame
                .holds
                .get(&PanelId::from(pane.entity_id()))
                .copied()
                .unwrap_or_default();
            ThreadPanel::set_held(pane, held, cx);
        }
        if closing {
            self.remove_closed(window, cx);
        }
    }

    /// Land every glide at once and take out the panes closing, for an edit
    /// that does not divide what they leave: a zoom, a layout put in.
    fn land_glides(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        let area = attachment.thread_panes.clone();
        let (tree, size) = {
            let area = area.read(cx);
            (
                area.layout(DockPlacement::Center).cloned(),
                area.bounds().size,
            )
        };
        let Some(tree) = tree else {
            return;
        };
        let frame = attachment.glides.land(&tree, size);
        let panes = attachment.all_panes(cx);
        area.update(cx, |area, cx| {
            for (node, sizes) in frame.sizes {
                area.set_split_sizes(node, sizes, window, cx);
            }
        });
        for pane in panes {
            ThreadPanel::set_held(&pane, Default::default(), cx);
        }
        self.remove_closed(window, cx);
    }

    /// Zoom the acting pane over the column (see [`crate::panes::Zoom`]), or
    /// turn the zoomed one back into its slot from where it is; running
    /// glides land first.
    fn toggle_zoom(
        &mut self,
        pane: Entity<ThreadPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compact(cx) {
            return;
        }
        let Some(attachment) = self.attachment.as_ref() else {
            return;
        };
        let zoomed = attachment.zoom.pane() == Some(&pane);
        if !zoomed && attachment.panes(cx).len() < 2 {
            return;
        }
        self.land_glides(window, cx);
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        if zoomed {
            attachment.zoom.toggle();
        } else {
            attachment.zoom.zoom_in(pane.clone(), cx);
            self.set_current_pane(&pane, cx);
        }
        cx.notify();
    }

    /// This frame of the zoom. A zoomed pane that left the column, or is the
    /// only one left in it, is back in its slot at once, and the compact
    /// layout, which draws no panes, zooms none.
    fn zoom_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = self.compact(cx);
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        let gone = attachment.zoom.pane().is_some_and(|pane| {
            let panes = attachment.all_panes(cx);
            panes.len() < 2 || !panes.contains(pane)
        });
        if compact || gone {
            attachment.zoom.land();
        }
        attachment.zoom.sample(window, cx);
    }

    /// The chat column: the dock of panes on the reading surface, faded
    /// while a layout goes in, under the pane zoomed over it. The panes
    /// fading out to be replaced take no pointer. The column runs along the
    /// window's top, bottom and right, and its left once the sidebar is
    /// collapsed; the zoomed pane covers the dock, not the seam around it.
    fn render_chat_column(&self, collapsed: bool, window: &mut Window, cx: &mut App) -> AnyElement {
        let Some(attachment) = self.attachment.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let area = attachment.thread_panes.read(cx).bounds();
        let column = div()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .shadow_sm();
        crate::window_seam::inset_from_seam(
            column,
            "chat-column-seam",
            gpui::Edges {
                left: collapsed,
                ..gpui::Edges::all(true)
            },
            window,
            cx,
        )
        .when_some(attachment.layout_swap.opacity(), |column, opacity| {
            column.opacity(opacity)
        })
        .child(
            div()
                .relative()
                .size_full()
                .child(attachment.thread_panes.clone())
                .children(attachment.zoom.render(area, cx))
                .when(attachment.layout_swap.leaving(), |column| {
                    column.child(crate::motion::shield())
                }),
        )
        .into_any_element()
    }

    /// Spatial moves go to the nearest pane painted on that side; Next and
    /// Previous follow the layout's order and wrap. While a pane is drawn
    /// over the column the others are covered, so nothing moves.
    ///
    /// Written here because gpui-base's dock has no keyboard navigation
    /// between groups; it goes once the dock offers one.
    fn focus_pane(&mut self, direction: FocusPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.compact(cx) {
            return;
        }
        let Some(attachment) = self.attachment.as_ref() else {
            return;
        };
        if attachment.zoom.pane().is_some() {
            return;
        }
        let Some(from) = self.acting_pane() else {
            return;
        };
        let panes = attachment.panes(cx);
        let Some(at) = panes.iter().position(|pane| *pane == from) else {
            return;
        };
        let target = match direction.side() {
            None if direction == FocusPane::Next => panes.get((at + 1) % panes.len()).cloned(),
            None => panes.get((at + panes.len() - 1) % panes.len()).cloned(),
            Some(side) => self.neighbour_of(&from, side, cx),
        };
        if let Some(target) = target
            && target != from
        {
            window.focus(&target.focus_handle(cx), cx);
        }
    }

    /// The pane nearest `pane` on `side`, as they were last painted.
    fn neighbour_of(
        &self,
        pane: &Entity<ThreadPanel>,
        side: Side,
        cx: &App,
    ) -> Option<Entity<ThreadPanel>> {
        let panes = self.attachment.as_ref()?.panes(cx);
        let bounds: Vec<_> = panes.iter().map(|pane| pane.read(cx).bounds()).collect();
        let at = panes.iter().position(|held| held == pane)?;
        crate::panes::neighbour(bounds[at], &bounds, side).map(|ix| panes[ix].clone())
    }

    /// Close every pane but `pane`, as [`Self::close_pane`] closes one: each
    /// glides shut and its thread closes once it is taken out.
    fn close_other_panes(
        &mut self,
        pane: Entity<ThreadPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compact(cx) {
            return;
        }
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        attachment.zoom.zoom_out();
        for other in attachment.panes(cx) {
            if other != pane {
                self.glide_out(&other, cx);
            }
        }
        window.focus(&pane.focus_handle(cx), cx);
        self.pane_entered(pane, cx);
    }

    /// `from` and its neighbour on `side` exchange their conversations, each with its own composer, scroll and expanded rows,
    /// so a half-written message stays with its thread; the panes and their
    /// slots stay where they are. The keyboard stays in `from`, or with
    /// `follow` goes with its thread to the neighbour.
    /// Neither conversation closes. Each conversation shown travels from
    /// where it was drawn into its new pane (see
    /// [`ThreadPanel::replace_chat`]); a waiting pane's has nothing shown to
    /// carry.
    fn swap_pane(
        &mut self,
        from: Entity<ThreadPanel>,
        side: Side,
        follow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compact(cx)
            || self
                .attachment
                .as_ref()
                .is_none_or(|attachment| attachment.zoom.pane().is_some())
        {
            return;
        }
        let Some(to) = self.neighbour_of(&from, side, cx) else {
            return;
        };
        let drawn = |pane: &Entity<ThreadPanel>| {
            let pane = pane.read(cx);
            pane.session_id().is_some().then(|| pane.chat_drawn(cx))
        };
        let (ours_at, theirs_at) = (drawn(&from), drawn(&to));
        let chat = from.read(cx).chat();
        let theirs = to.update(cx, |to, cx| to.replace_chat(chat, ours_at, cx));
        from.update(cx, |from, cx| from.replace_chat(theirs, theirs_at, cx));
        let keeper = if follow { to } else { from };
        window.focus(&keeper.focus_handle(cx), cx);
        self.pane_entered(keeper, cx);
    }

    /// What a pane command does to `pane`.
    fn run_on_pane(
        &mut self,
        pane: Entity<ThreadPanel>,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            PaneCommand::Split(placement) => self.split_pane(&pane, placement, window, cx),
            PaneCommand::Close => self.close_pane(&pane, window, cx),
            PaneCommand::CloseOthers => self.close_other_panes(pane, window, cx),
            PaneCommand::Zoom => self.toggle_zoom(pane, window, cx),
            PaneCommand::Swap(side) => self.swap_pane(pane, side, false, window, cx),
            // `pane`'s thread trades places with its neighbour's and the
            // keyboard goes with it. A pane waiting for a thread has none.
            PaneCommand::Move(side) if pane.read(cx).session_id().is_some() => {
                self.swap_pane(pane, side, true, window, cx)
            }
            PaneCommand::Move(_) => {}
        }
    }

    /// The thread the pane `panel` shows.
    fn panel_session(&self, panel: PanelId, cx: &App) -> Option<String> {
        self.attachment
            .as_ref()?
            .thread_panes
            .read(cx)
            .panel(panel)?
            .view()
            .downcast::<ThreadPanel>()
            .ok()?
            .read(cx)
            .session_id()
            .map(str::to_owned)
    }

    /// The narrowest the chat column can be laid out as `tree`, whose panels
    /// show the threads `session_of` names: its panes' headers at their
    /// narrowest, side by side, and never narrower than a pane alone. Only
    /// the pane in the column's top-left corner clears the traffic lights
    /// over a collapsed sidebar.
    fn column_min_width(
        &self,
        tree: &gpui_base::dock::PaneTree,
        session_of: &dyn Fn(PanelId) -> Option<String>,
        collapsed: bool,
        fullscreen: bool,
        cx: &App,
    ) -> f32 {
        let Some(attachment) = self.attachment.as_ref() else {
            return CHAT_MIN_WIDTH;
        };
        let store = attachment.link.store.read(cx);
        let floor = if tree.panels().nth(1).is_some() {
            PANE_MIN_WIDTH
        } else {
            CHAT_MIN_WIDTH
        };
        let pane_min = |panel: PanelId, edges: crate::chat::WindowEdges| {
            let session_id = session_of(panel);
            floor
                + crate::chat::header_extra_width_for(
                    store,
                    session_id.as_deref(),
                    collapsed && edges.left,
                    fullscreen,
                    false,
                )
        };
        crate::panes::min_width(tree.root(), crate::chat::WindowEdges::BOTH, &pane_min)
            .max(CHAT_MIN_WIDTH)
    }

    /// Whether `pane` may split along `placement`: below the cap, and with
    /// the column's minimum after the split still fitting beside the sidebar.
    fn split_allowed(
        &self,
        pane: &Entity<ThreadPanel>,
        placement: gpui_base::Placement,
        window: &Window,
        cx: &App,
    ) -> bool {
        let Some(attachment) = self.attachment.as_ref() else {
            return false;
        };
        if self.compact(cx) || attachment.panes(cx).len() >= crate::panes::MAX_PANES {
            return false;
        }
        let Some(mut tree) = attachment.final_layout(cx) else {
            return false;
        };
        // From the tree rather than the pane's group: the drop indicator asks
        // while the group is rendering.
        let Some(node) = tree.find_panel_node(PanelId::from(pane.entity_id())) else {
            return false;
        };
        tree.split(node, PanelId::from_u64(u64::MAX), placement, None);
        self.column_fits(&tree, &|panel| self.panel_session(panel, cx), window, cx)
    }

    /// Whether `tree`'s minimum width, its panels showing the threads
    /// `session_of` names, fits beside the sidebar.
    fn column_fits(
        &self,
        tree: &gpui_base::dock::PaneTree,
        session_of: &dyn Fn(PanelId) -> Option<String>,
        window: &Window,
        cx: &App,
    ) -> bool {
        let Some(attachment) = self.attachment.as_ref() else {
            return false;
        };
        let collapsed = self.window_state.read(cx).sidebar_collapsed;
        let sidebar = if collapsed {
            px(0.)
        } else {
            attachment.sidebar_width.get()
        };
        px(self.column_min_width(tree, session_of, collapsed, window.is_fullscreen(), cx))
            <= window.viewport_size().width - sidebar
    }

    /// `layout` as the panes there would fill it: its tree, and what fills
    /// each of the tree's panels. `None` where the column has no room for it,
    /// or it has nothing to show: a single pane alone, a thread without
    /// agents.
    fn plan_layout(
        &self,
        layout: PaneLayout,
        window: &Window,
        cx: &App,
    ) -> Option<(gpui_base::dock::PaneTree, Vec<(PanelId, Slot)>)> {
        if self.compact(cx) {
            return None;
        }
        let attachment = self.attachment.as_ref()?;
        let current_pane = attachment.current_pane.clone();
        let panes = attachment.panes(cx);
        let first = std::iter::once(Slot::Pane(current_pane.clone()));
        let (fills, fewest): (Vec<_>, _) = match layout {
            PaneLayout::SpreadAgents => {
                let parent = current_pane.read(cx).session_id()?;
                let mut agents: Vec<_> = attachment
                    .link
                    .store
                    .read(cx)
                    .sidebar_sessions()
                    .iter()
                    .filter(|meta| {
                        meta.parent_session_id.as_deref() == Some(parent)
                            && meta.archived_at.is_none()
                            && !meta.is_settled()
                    })
                    .collect();
                agents.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));
                let agents = agents.into_iter().map(|meta| {
                    self.pane_showing(&meta.id, cx)
                        .map_or_else(|| Slot::Thread(meta.id.clone()), Slot::Pane)
                });
                (
                    first.chain(agents).take(crate::panes::MAX_PANES).collect(),
                    2,
                )
            }
            PaneLayout::Single if panes.len() < 2 => return None,
            layout => {
                let count = layout.splits(0).len() + 1;
                let others = panes
                    .iter()
                    .filter(|pane| **pane != current_pane)
                    .cloned()
                    .map(Slot::Pane);
                let fills = first
                    .chain(others)
                    .chain(std::iter::repeat(Slot::Waiting))
                    .take(count)
                    .collect();
                (fills, count)
            }
        };
        let mut tree = attachment
            .thread_panes
            .read(cx)
            .layout(DockPlacement::Center)?
            .clone();
        let anchor = PanelId::from(current_pane.entity_id());
        for panel in tree.panels().collect::<Vec<_>>() {
            if panel != anchor {
                tree.remove_panel(panel);
            }
        }
        (fewest..=fills.len()).rev().find_map(|count| {
            let mut tree = tree.clone();
            let mut ids = vec![anchor];
            for (ix, (from, placement)) in layout.splits(count).into_iter().enumerate() {
                let node = tree.find_panel_node(ids[from])?;
                let id = PanelId::from_u64(u64::MAX - ix as u64);
                tree.split(node, id, placement, None);
                ids.push(id);
            }
            // Every split opens right of or below its pane, so the last
            // input leads the tree's order.
            let slots: Vec<_> = tree.panels().zip(fills.iter().cloned()).collect();
            let session_of = |panel: PanelId| match slots.iter().find(|(id, _)| *id == panel)? {
                (_, Slot::Pane(pane)) => pane.read(cx).session_id().map(str::to_owned),
                (_, Slot::Thread(session_id)) => Some(session_id.clone()),
                (_, Slot::Waiting) => None,
            };
            self.column_fits(&tree, &session_of, window, cx)
                .then_some((tree, slots))
        })
    }

    /// Choose `layout` for the column. It goes in as a layout swap (see
    /// [`crate::panes::LayoutSwap`]): the column fades out, the layout is put
    /// in at the bottom of the fade and the column fades back in. A layout that
    /// would leave every pane where it is drawn, none zoomed, changes
    /// nothing. The current pane keeps the keyboard.
    fn apply_layout(&mut self, layout: PaneLayout, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plan) = self.plan_layout(layout, window, cx) else {
            return;
        };
        let unchanged = self.layout_unchanged(&plan, cx);
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        if !(unchanged && attachment.zoom.pane().is_none() && attachment.layout_swap.is_idle()) {
            attachment.layout_swap.choose(layout);
        }
        let current_pane = attachment.current_pane.clone();
        self.pane_entered(current_pane.clone(), cx);
        window.focus(&current_pane.focus_handle(cx), cx);
        cx.notify();
    }

    /// This frame of a layout swap: the layout goes in at the bottom of the
    /// fade. The compact layout draws no panes, so a swap stops there.
    fn swap_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = self.compact(cx);
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        if compact {
            attachment.layout_swap.land();
            return;
        }
        if let Some(layout) = attachment.layout_swap.sample(window, cx) {
            self.install_layout(layout, window, cx);
        }
    }

    /// Whether `plan` would leave every pane where the dock lays it out:
    /// nothing gliding or closing, and the same panes in the same splits at
    /// the same shares.
    fn layout_unchanged(
        &self,
        (tree, slots): &(gpui_base::dock::PaneTree, Vec<(PanelId, Slot)>),
        cx: &App,
    ) -> bool {
        let Some(attachment) = self.attachment.as_ref() else {
            return false;
        };
        let area = attachment.thread_panes.read(cx);
        let Some(now) = area.layout(DockPlacement::Center) else {
            return false;
        };
        if !attachment.glides.is_idle()
            || attachment
                .all_panes(cx)
                .iter()
                .any(|pane| pane.read(cx).is_closing())
        {
            return false;
        }
        let shown = |planned: PanelId| match slots.iter().find(|(panel, _)| *panel == planned) {
            Some((_, Slot::Pane(pane))) => Some(PanelId::from(pane.entity_id())),
            _ => None,
        };
        crate::panes::same_layout(
            now.root(),
            tree.root(),
            &|now, planned| shown(planned) == Some(now),
            area.bounds().size,
        )
    }

    /// Put `layout` in at once, as the panes are now: running glides land,
    /// the zoomed pane is back in its slot, and the panes the layout leaves
    /// out close with their threads; the current pane keeps the keyboard.
    fn install_layout(&mut self, layout: PaneLayout, window: &mut Window, cx: &mut Context<Self>) {
        self.land_glides(window, cx);
        let Some(plan) = self.plan_layout(layout, window, cx) else {
            return;
        };
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        attachment.zoom.land();
        if self.layout_unchanged(&plan, cx) {
            return;
        }
        let (tree, slots) = plan;
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        let store = attachment.link.store.clone();
        let preview = attachment.preview.clone();
        let area = attachment.thread_panes.clone();
        let current_pane = attachment.current_pane.clone();
        let before = attachment.panes(cx);
        let mut panes = HashMap::new();
        let mut threads = Vec::new();
        for (panel, slot) in slots {
            let pane = match slot {
                Slot::Pane(pane) => pane,
                Slot::Waiting | Slot::Thread(_) => {
                    let pane = self.new_pane(&store, &preview, None, window, cx);
                    if let Slot::Thread(session_id) = slot {
                        threads.push((pane.clone(), session_id));
                    }
                    pane
                }
            };
            panes.insert(panel, pane);
        }
        area.update(cx, |area, cx| {
            area.set_center(
                crate::panes::layout_of(tree.root(), &|panel| panes.get(&panel).cloned()),
                window,
                cx,
            );
        });
        let kept: Vec<_> = panes.values().cloned().collect();
        for pane in before {
            if !kept.contains(&pane)
                && let Some(session_id) = pane.read(cx).session_id().map(str::to_owned)
                && !drawn_beside(&kept, &session_id, cx)
            {
                store.update(cx, |store, cx| store.close_thread(&session_id, cx));
            }
        }
        for (pane, session_id) in threads {
            self.bind_pane(&pane, &session_id, window, cx);
        }
        self.pane_entered(current_pane.clone(), cx);
        window.focus(&current_pane.focus_handle(cx), cx);
        cx.notify();
    }

    /// Tell each pane whether it shares the column and how a zoom leaves it,
    /// and its conversation which window edges its header runs along and
    /// whether its header holds the pane's close button. The layout's tree
    /// decides the first and the last, so they hold from the frame a split
    /// lands; a pane drawn over the column is laid out as the whole column.
    ///
    /// The close button's fade is sampled here, keyed by the pane, rather
    /// than in the header: a split re-parents the pane's group in its frame,
    /// which would restart any keyed state in the header (see
    /// `panes::ThreadPanes`), so the button would snap in.
    ///
    /// Each view is updated only when what it is handed differs from what it
    /// holds: in gpui-fast an update while the window draws counts as a change
    /// to every view that read the entity, so handing a view what it already
    /// holds would draw all of them again next frame.
    fn sync_panes(&mut self, window: &mut Window, cx: &mut App) {
        let Some(attachment) = self.attachment.as_ref() else {
            return;
        };
        let compact = self.compact(cx);
        for pane in attachment.all_panes(cx) {
            let zoom = attachment.zoom.of(&pane);
            ThreadPanel::set_zoom(&pane, zoom, cx);
        }
        let tree = attachment.final_layout(cx);
        let edges: Vec<_> = attachment
            .panes(cx)
            .into_iter()
            .map(|pane| {
                let overlaid = attachment.zoom.pane() == Some(&pane);
                let edges = tree
                    .as_ref()
                    .filter(|_| !compact && !overlaid)
                    .and_then(|tree| {
                        crate::panes::edges_of(
                            tree.root(),
                            crate::chat::WindowEdges::BOTH,
                            PanelId::from(pane.entity_id()),
                        )
                    })
                    .unwrap_or(crate::chat::WindowEdges::BOTH);
                (pane, edges)
            })
            .collect();
        let among_others = edges.len() > 1;
        let mut handed = Vec::with_capacity(edges.len());
        for (pane, edges) in edges {
            let recedes = among_others && !compact && pane != attachment.current_pane;
            let close = crate::motion::shown(
                gpui::ElementId::NamedInteger("pane-close".into(), pane.entity_id().as_u64()),
                (among_others && !compact).then(|| pane.downgrade()),
                SHOW,
                window,
                cx,
            );
            ThreadPanel::set_among_others(&pane, among_others, cx);
            let controls = self.pane_controls(&pane, window, cx);
            let chat = pane.read(cx).chat();
            handed.push((
                chat,
                HandedToChat {
                    edges,
                    recedes,
                    close,
                    controls,
                },
            ));
        }
        let Some(attachment) = self.attachment.as_mut() else {
            return;
        };
        attachment
            .handed
            .retain(|chat, _| handed.iter().any(|(handed, _)| handed.entity_id() == *chat));
        for (chat, now) in handed {
            let last = attachment.handed.get(&chat.entity_id());
            if last.is_none_or(|last| last.edges != now.edges) {
                ChatView::set_edges(&chat, now.edges, cx);
            }
            if last.is_none_or(|last| last.recedes != now.recedes) {
                ChatView::set_title_recedes(&chat, now.recedes, cx);
            }
            if last.is_none_or(|last| last.close != now.close || last.controls != now.controls) {
                ChatView::set_pane(&chat, now.close.clone(), now.controls, cx);
            }
            attachment.handed.insert(chat.entity_id(), now);
        }
        let Some(attachment) = self.attachment.as_ref() else {
            return;
        };
        let shown = self.shown_threads(tree.as_ref(), cx);
        let hovered = attachment.sidebar.read(cx).hovered_thread();
        for pane in attachment.panes(cx) {
            let hovered = shown.map.iter().any(|cell| {
                cell.pane == pane.entity_id()
                    && hovered.is_some()
                    && cell.row.as_deref() == hovered.as_deref()
            });
            ThreadPanel::set_row_hovered(&pane, hovered, cx);
        }
        SessionsSidebar::set_shown(&attachment.sidebar, shown, cx);
    }

    /// What the threads list marks: nothing on a phone, nor on a route
    /// without the chat column, and of a zoomed column only the zoomed pane,
    /// the others being covered.
    /// `tree` is the column's layout without the panes closing.
    fn shown_threads(
        &self,
        tree: Option<&gpui_base::dock::PaneTree>,
        cx: &App,
    ) -> crate::sidebar::ShownThreads {
        let Some(attachment) = self.attachment.as_ref() else {
            return Default::default();
        };
        if self.compact(cx) || self.window_state.read(cx).route() != Route::Chat {
            return Default::default();
        }
        let store = attachment.link.store.read(cx);
        let row = |pane: &Entity<ThreadPanel>| {
            let thread = pane.read(cx).session_id()?;
            Some(store.roster_row(thread))
        };
        let panes = match attachment
            .zoom
            .pane()
            .filter(|_| attachment.zoom.is_zoomed())
        {
            Some(zoomed) => vec![zoomed.clone()],
            None => attachment.panes(cx),
        };
        let rows: Vec<_> = panes.iter().map(|pane| (pane, row(pane))).collect();
        let column = gpui::Bounds::new(gpui::Point::default(), {
            attachment.thread_panes.read(cx).bounds().size
        });
        let mut rects = Vec::new();
        if let Some(tree) = tree.filter(|_| panes.len() > 1) {
            crate::panes::pane_rects(tree.root(), column, &mut rects);
        }
        let share = |of: Pixels, whole: Pixels| of / whole.max(px(1.));
        let map = rects
            .into_iter()
            .filter_map(|(panel, rect)| {
                let (pane, row) = rows
                    .iter()
                    .find(|(pane, _)| PanelId::from(pane.entity_id()) == panel)?;
                Some(crate::sidebar::MapCell {
                    pane: pane.entity_id(),
                    row: row.clone(),
                    rect: gpui::bounds(
                        gpui::point(
                            share(rect.left(), column.size.width),
                            share(rect.top(), column.size.height),
                        ),
                        gpui::size(
                            share(rect.size.width, column.size.width),
                            share(rect.size.height, column.size.height),
                        ),
                    ),
                })
            })
            .collect();
        let current = match rows
            .iter()
            .find(|(pane, _)| **pane == attachment.current_pane)
        {
            Some((_, row)) => row.clone(),
            None => row(&attachment.current_pane),
        };
        crate::sidebar::ShownThreads {
            current,
            shown: rows.into_iter().filter_map(|(_, row)| row).collect(),
            map,
        }
    }
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

impl AppShell {
    fn compact(&self, cx: &App) -> bool {
        self.window_state.read(cx).compact
    }

    pub(crate) fn wake_connection(&self, wake: tcode_client::recovery::Wake) {
        if let Some(attachment) = &self.attachment {
            attachment.link.link().wake(wake);
        }
    }

    /// Recompute the layout from the window. Only a flip does anything, and a
    /// flip reconciles navigation to where the state already is — it is not a
    /// Back gesture and must not animate like one.
    fn sync_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = crate::window_seam::window_is_compact(window, cx);
        // Read first: an update while the window draws counts as a change to
        // every view that read the state, so they would all be drawn again
        // next frame though nothing changed.
        let flipped = self.compact(cx) != compact
            && self
                .window_state
                .update(cx, |state, cx| state.set_compact(compact, cx));
        if flipped {
            self.layout_flipped = true;
            self.move_edge_back(TouchPhase::Cancelled, window, cx);
            self.page_back = None;
            // The wide layout has no pages: an agent stays in its pane alone.
            if !compact {
                self.drop_leads(0, cx);
                self.release_leaving(cx);
            }
            // The visit survives the resize; only a thread selected in the wide
            // split — which is not a page there — has to become one here.
            if compact
                && self.destination(cx) == Destination::Threads
                && self.page_thread(cx).is_some()
            {
                self.window_state
                    .update(cx, |state, cx| state.go(Destination::Thread, cx));
            }
            self.sync_nav(NavMotion::Immediate, cx);
            cx.notify();
        }
    }

    /// `element` as the part `key` of the window's layout, which glides from
    /// where the other layout drew it when the window turns compact or wide.
    fn layout_part(&self, key: &'static str, element: impl IntoElement) -> AnyElement {
        match (&self.layout_reflow, &self.departing) {
            (Some(reflow), None) => reflow.part(key, element),
            _ => element.into_any_element(),
        }
    }

    fn destination(&self, cx: &App) -> Destination {
        match &self.departing {
            Some(departing) => departing.destination,
            None => self.window_state.read(cx).destination(),
        }
    }

    /// Mirror the window's navigation history into the compact stack: pop back
    /// to what the two still share, then push the rest. One view per history
    /// entry, so the same destination visited twice is two independent pages.
    fn sync_nav(&mut self, motion: NavMotion, cx: &mut Context<Self>) {
        let history = self.window_state.read(cx).history().to_vec();
        if self.mounted != history {
            self.page_back = None;
            self.pages_changing.set(motion == NavMotion::Animated);
        }
        if self.mounted != history
            && let Some(swipe) = &mut self.back_swipe
        {
            swipe.finished = true;
            if let Some(sheet) = &swipe.sheet {
                sheet.update(cx, |sheet, cx| {
                    sheet.dragging = false;
                    sheet.progress = 0.;
                    App::notify(cx, sheet.parent);
                });
            }
        }
        let common = self
            .mounted
            .iter()
            .zip(&history)
            .take_while(|(mounted, target)| mounted == target)
            .count();
        let shell = cx.entity().downgrade();
        let mut pushed = Vec::new();
        self.nav.update(cx, |nav, cx| {
            while nav.depth() > common {
                if nav.pop(motion, cx).is_none() {
                    break;
                }
            }
            for destination in &history[common.min(history.len())..] {
                let page = AnyView::from(cx.new(|_| DestinationView {
                    shell: shell.clone(),
                    destination: *destination,
                }));
                nav.push(page, motion, cx);
                pushed.push(*destination);
            }
        });
        self.mounted.truncate(common);
        self.mounted.extend(pushed);
        let state = self.window_state.read(cx);
        let on_screen = if state.compact {
            state.destination() == Destination::Thread
        } else {
            state.route() == Route::Chat
        };
        if let Some(attachment) = &self.attachment {
            attachment.link.store.update(cx, |store, cx| {
                store.set_conversation_on_screen(on_screen, cx);
            });
        }
    }

    /// Navigating dismisses a software keyboard. Only a mobile build has one,
    /// and every compact window is a mobile build's.
    fn blur_navigation(&self, window: &mut Window, cx: &mut Context<Self>) {
        if crate::window_seam::is_mobile(cx) {
            window.blur(cx);
        }
    }

    fn go(&mut self, destination: Destination, cx: &mut Context<Self>) {
        self.window_state
            .update(cx, |state, cx| state.go(destination, cx));
    }

    /// Answer a platform Back gesture (or a Back control). `true` means it was
    /// consumed; `false` only at the root, where the platform closes the app.
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // A composition or a software keyboard is on top of everything.
        if crate::window_seam::keyboard_covers_window(window) {
            window.blur(cx);
            return true;
        }
        // Dialogs, menus and popovers own Cancel.
        if crate::overlay::dialog_open(window, cx)
            || gpui_base::GlobalState::is_in_deferred_context(cx)
        {
            window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
            return true;
        }
        if self.window_state.read(cx).palette_open {
            self.window_state
                .update(cx, |state, cx| state.close_palette(cx));
            return true;
        }
        // An agent is reached from its lead, so Back returns there first.
        if self.window_state.read(cx).compact && self.destination(cx) == Destination::Thread {
            self.keep_lead_beneath(window, cx);
            let lead = self
                .attachment
                .as_ref()
                .and_then(|attachment| attachment.leads.last());
            if let Some(lead) = lead {
                // A pane of the lead's own, opened beside the agent while the
                // window was wide, would show the lead a second time once the
                // agent's pane does.
                let lead_pane = self.pane_showing(&lead.session_id, cx);
                self.pop_lead(cx);
                if let Some(pane) = lead_pane {
                    self.close_pane_into(&pane, |_, _| None, window, cx);
                }
                return true;
            }
        }
        // Everything below is one history: settings detail, settings root, a
        // Hosts visit, Pair, and the workspace pages all pop the same way.
        // Leaving a page never touches the attachment.
        let leaving_thread =
            self.window_state.read(cx).compact && self.destination(cx) == Destination::Thread;
        let consumed = self.window_state.update(cx, |state, cx| state.back(cx));
        if consumed
            && leaving_thread
            && self.destination(cx) == Destination::Threads
            && let Some(attachment) = &self.attachment
        {
            attachment
                .page()
                .update(cx, |pane, cx| pane.release_preview(cx));
        }
        consumed
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
        let attachment = self.attachment.as_ref()?;
        let state = self.window_state.read(cx);
        let on_screen = if state.compact {
            state.destination() == Destination::Thread
        } else {
            state.route() == Route::Chat
                && !attachment
                    .link
                    .store
                    .read(cx)
                    .panel_state_for(self.page_thread(cx).as_deref())
                    .right_panel_open
        };
        (on_screen
            && !state.palette_mounted()
            && self.pending_navigation_restore.is_none()
            && !gpui_base::GlobalState::is_in_deferred_context(cx))
        .then(|| attachment.chat(cx))
    }
}

/// App-level dispatch also reaches the shell when no child has keyboard focus.
pub(crate) fn navigate_thread(action: &crate::shortcut::NavigateThread, cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    let current = current_thread(cx);
    shell.update(cx, |shell, cx| {
        if let Some(attachment) = &shell.attachment
            && attachment.sidebar.update(cx, |sidebar, cx| {
                sidebar.navigate_thread(action, current.as_deref(), cx)
            })
        {
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
        let session_id = action.0.clone();
        act(cx, move |shell, window, cx| {
            if let Some(from) = shell.acting_pane() {
                shell.open_in_new_pane(
                    session_id,
                    &from,
                    &[gpui_base::Placement::Right],
                    window,
                    cx,
                );
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

/// Point this window at another host.
pub(crate) fn switch_current(target: AttachmentTarget, window: &mut Window, cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    // A child (notably the pairing confirmation) can call this from its
    // entity listener. Attachment setup updates those children in turn.
    window.defer(cx, move |window, cx| {
        shell.update(cx, |shell, cx| shell.switch_to(target, window, cx));
    });
}

/// Leave the attached host without opening another one.
pub(crate) fn detach_current(cx: &mut App) {
    let Some(shell) = current_shell(cx) else {
        return;
    };
    // The Hosts menu calls this from its panel's listener, and detaching
    // updates that panel.
    cx.defer(move |cx| shell.update(cx, |shell, cx| shell.detach(cx)));
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

/// The per-thread notification slot for its pull request watch.
struct PullRequestWatchNotice;

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
            RuntimeEvent::Error(RuntimeError::WorkingDirectoryMissing {
                project_id: Some(project_id),
                cwd,
                ..
            }) if self.store().is_some() => {
                let store = self.store().expect("checked above");
                crate::change_project_root_dialog::open_missing_directory(
                    store,
                    project_id.clone(),
                    cwd.clone(),
                    window,
                    cx,
                );
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
            RuntimeEvent::Toast(RuntimeToast::PullRequestWatch {
                session_id,
                number,
                notice,
            }) => {
                self.present_pull_request_watch(session_id, *number, notice, window, cx);
                return;
            }
            RuntimeEvent::Toast(RuntimeToast::PullRequestStack {
                session_id,
                target,
                stack,
                layers,
                base,
                result,
                late,
            }) => {
                if let Some(store) = self.store() {
                    crate::pull_requests::present_stack_result(
                        &store, session_id, target, *stack, layers, base, result, *late, window, cx,
                    );
                }
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
            RuntimeToastDisposition::Update(operation) => {
                let Some(id) = self.operation_toasts.get(&operation) else {
                    return;
                };
                *id
            }
            RuntimeToastDisposition::Finish(operation) => self
                .operation_toasts
                .remove(&operation)
                .unwrap_or_else(|| self.take_toast_id()),
        };

        let action = presented.action.and_then(|action| {
            let store = self.store()?;
            let label = match &action {
                RuntimeToastAction::RetryGit(_) => crate::tr!("git.toast.retry"),
                RuntimeToastAction::UpdateProviders(_) => crate::tr!("providers.update_all"),
            }
            .into_owned()
            .into();
            Some(ToastAction {
                label,
                handler: Rc::new(move |window, cx| {
                    window.remove_notification1::<RuntimeToastNotification>(toast_id as usize, cx);
                    store.update(cx, |store, _cx| match action.clone() {
                        RuntimeToastAction::RetryGit(request) => store.retry_git_action(request),
                        RuntimeToastAction::UpdateProviders(providers) => {
                            store.update_providers(providers)
                        }
                    });
                }),
            })
        });
        self.show_toast(
            toast_id,
            presented.kind,
            (presented.title, presented.detail, presented.progress),
            action,
            window,
            cx,
        );
    }

    /// One slot per thread: a newer wake or end of its watch replaces the older notice.
    fn present_pull_request_watch(
        &mut self,
        session_id: &str,
        number: u64,
        notice: &tcode_core::pull_request_watch::WatchNotice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store() else {
            return;
        };
        // A pane's transcript already shows the message.
        if window.is_window_active() && self.pane_showing(session_id, cx).is_some() {
            return;
        }
        let title = store
            .read(cx)
            .thread_meta(session_id)
            .map(|meta| meta.title.clone())
            .unwrap_or_default();
        let (kind, message) = crate::pull_requests::watch_notice_toast(number, notice);
        let id = session_id.to_owned();
        window.push_notification(
            Notification::new()
                .id1::<PullRequestWatchNotice>(SharedString::from(id.clone()))
                .title(title)
                .message(message)
                .with_type(kind)
                .action(move |_, _, _| {
                    use crate::sizing::Sizable as _;
                    let store = store.clone();
                    let id = id.clone();
                    crate::widgets::button::Button::new("pull-request-watch-open-thread")
                        .small()
                        .outline()
                        .label(crate::tr!("pull_requests.open_thread"))
                        .on_click(move |_, window, cx| {
                            window.remove_notification1::<PullRequestWatchNotice>(
                                SharedString::from(id.clone()),
                                cx,
                            );
                            store.update(cx, |store, cx| store.show_thread(id.clone(), cx));
                        })
                })
                .autohide(true),
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
        content: (String, Option<String>, Option<f32>),
        action: Option<ToastAction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, detail, progress) = content;
        let has_action = action.is_some();
        window.push_notification(
            crate::toast::notification(id, kind, title, detail.map(Into::into), action, progress),
            cx,
        );

        let delay = match kind {
            ToastKind::Success | ToastKind::Info => Some(Duration::from_secs(4)),
            ToastKind::Warning => Some(Duration::from_secs(6)),
            ToastKind::Error | ToastKind::Loading => None,
        };
        if let Some(delay) = delay.filter(|_| !has_action) {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(delay).await;
                _ = this.update_in(cx, |_, window, cx| {
                    window.remove_notification1::<RuntimeToastNotification>(id as usize, cx);
                });
            })
            .detach();
        }
    }

    fn on_open_source_control(
        &mut self,
        _: &crate::pull_requests::OpenSourceControl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
        self.window_state.update(cx, |state, cx| {
            state.pending_settings_section = Some("source_control".into());
            state.open_settings(cx);
        });
    }

    fn on_open_thread_anchor(
        &mut self,
        anchor: &crate::markdown::OpenThreadAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store() else {
            return;
        };
        if store.read(cx).thread_meta(&anchor.thread_id).is_none() {
            window.push_notification(
                crate::overlay::Notification::warning(
                    crate::tr!("control.anchor_missing").into_owned(),
                ),
                cx,
            );
            return;
        }
        let anchor = anchor.clone();
        store.update(cx, |store, cx| match anchor.position {
            Some(position) => store.show_thread_at_position(anchor.thread_id, position, cx),
            None => store.show_thread(anchor.thread_id, cx),
        });
    }

    fn on_thread_undo(
        &mut self,
        _: &crate::sidebar::ThreadUndo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(attachment) = &self.attachment {
            attachment
                .sidebar
                .update(cx, |sidebar, cx| sidebar.undo_lifecycle(window, cx));
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

/// The least room a nav bar keeps on each side for its controls, so the
/// centered title keeps the same box on every page whose controls fit it: two
/// 44pt icon buttons. A wider back label ([`Destination::back_label`]), which
/// never truncates, takes more.
const NAV_CONTROL_WIDTH: f32 = 104.;

/// A page that is the whole window: one of the compact stack, or the Hosts
/// page a tablet shows before it attaches. It is inset from the seam on
/// every side ([`crate::window_seam::inset_from_seam`]). The page paints
/// there rather than the shell, so those edges show whichever page covers
/// them and slide with it as it is pushed or dragged back.
fn page_surface(window: &mut Window, cx: &mut App) -> Div {
    crate::window_seam::inset_from_seam(
        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx)),
        "page-seam",
        gpui::Edges::all(true),
        window,
        cx,
    )
}

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
    let traffic_lights = window_caption::traffic_light_inset(window);
    let hosts_caption = window_caption::compact_hosts_caption();
    let (dense, tight) = (cx.theme().tokens.spacing.sm, cx.theme().tokens.spacing.xs);
    // The sides carry the strip's insets themselves, so their floor is
    // measured from the window's edge (or the platform's controls). The 44pt
    // controls keep their glyphs clear of the edge inside their own hit box,
    // so the inset is the dense one, not the page inset.
    let strip = crate::material::bar("compact-nav", crate::material::Bar::Strip, false, cx)
        .px_0()
        .gap(Pixels::ZERO)
        .pl(traffic_lights);
    v_flex()
        .flex_none()
        .w_full()
        .bg(crate::material::content_surface(cx))
        .child(
            window_drag_area("compact-nav-drag", strip, window, cx)
                // The two sides grow alike from nothing, so the title is centered
                // while it fits; a side never shrinks below its controls, so a
                // label wider than the reserved room pushes the title, which
                // truncates, instead of running into it.
                .child(
                    h_flex()
                        .flex_1()
                        .flex_basis(px(0.))
                        .min_w(px(NAV_CONTROL_WIDTH))
                        .h_full()
                        .items_center()
                        .when(traffic_lights == Pixels::ZERO, |side| side.pl(dense))
                        .pr(tight)
                        .children(back)
                        .child(window_caption::drag_region(div().flex_1().h_full())),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .max_w_full()
                                .min_w_0()
                                .overflow_hidden()
                                .text_title(cx)
                                .child(crate::widgets::fade_text(title)),
                        )
                        .children(subtitle),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .flex_basis(px(0.))
                        .min_w(px(NAV_CONTROL_WIDTH))
                        .h_full()
                        .items_center()
                        .justify_end()
                        .gap(tight)
                        .when(!hosts_caption, |side| side.pr(dense))
                        .child(window_caption::drag_region(div().flex_1().h_full()))
                        .children(actions)
                        .children(
                            hosts_caption.then(|| window_caption::caption_controls(window, cx)),
                        ),
                ),
        )
        .child(crate::material::faded_hairline(cx))
}

/// Back control labelled with the parent destination's short fixed label,
/// which is never truncated.
fn back_button(id: &'static str, parent: SharedString) -> crate::widgets::button::Button {
    use crate::widgets::button::{Button, ButtonVariants as _};
    Button::new(id)
        .ghost()
        .touch(true)
        .icon(Icon::empty().path("icons/chevron-left.svg"))
        .label(parent)
        // A control press must not arm the ancestor window drag region.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// A nav-bar icon button. Blue is reserved for primary actions and live state.
fn nav_icon_button(
    id: impl Into<ElementId>,
    aria_label: impl Into<SharedString>,
    icon: IconName,
) -> crate::widgets::button::Button {
    crate::material::toolbar_icon_button(id, icon, aria_label, true)
        // A control press must not arm the ancestor window drag region.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// The muted second line under a nav-bar title: how the machine a thread
/// list belongs to is reached, the project a thread lives in.
fn nav_subtitle(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    div()
        .max_w_full()
        .min_w_0()
        .overflow_hidden()
        .text_secondary(true, cx)
        .child(crate::widgets::fade_text(text.into()))
        .into_any_element()
}

fn compact_label(key: &str) -> String {
    crate::tr!(format!("mobile.{key}")).into_owned()
}

impl AppShell {
    /// The compact stack's topmost thread page, unless a restore is yet to
    /// say which thread it shows.
    fn top_thread_page(&self, cx: &App) -> Option<EntityId> {
        if !self.compact(cx)
            || self.departing.is_some()
            || self.pending_navigation_restore.is_some()
        {
            return None;
        }
        let at = self
            .mounted
            .iter()
            .rposition(|destination| *destination == Destination::Thread)?;
        let page = self.nav.read(cx).views().nth(at)?.entity_id();
        Some(page)
    }

    /// Open the agent `session_id` over the thread on the compact stack's
    /// thread page, or the panels page standing on it, which goes: the thread
    /// stays as it is on its page, open and held. False where no thread
    /// page can take it, for the caller to show it.
    fn open_over_lead(
        &mut self,
        session_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(page) = self.top_thread_page(cx) else {
            return false;
        };
        if !matches!(
            self.destination(cx),
            Destination::Thread | Destination::Panel
        ) || self.pane_showing(&session_id, cx).is_some()
        {
            return false;
        }
        let window_state = self.window_state.clone();
        let Some(attachment) = &mut self.attachment else {
            return false;
        };
        let store = attachment.link.store.clone();
        let pane = attachment.page().clone();
        let Some(lead) = pane.read(cx).session_id().map(str::to_owned) else {
            return false;
        };
        if attachment
            .leads
            .iter()
            .any(|kept| kept.session_id == session_id)
        {
            return false;
        }
        store.update(cx, |store, cx| {
            store.lease_timeline(&lead);
            if !store.is_thread_open(&session_id) {
                store.open_thread(&session_id, cx);
            }
        });
        let state = window_state.clone();
        let chat =
            cx.new(|cx| ChatView::new(store.clone(), state, Some(session_id.clone()), window, cx));
        let lead_chat = pane.update(cx, |pane, cx| pane.replace_chat(chat, None, cx));
        attachment.leads.push(ThreadPage {
            page,
            frame: lead_chat.read(cx).frame(),
            chat: lead_chat,
            session_id: lead,
        });
        store.update(cx, |store, cx| store.show_thread(session_id, cx));
        window_state.update(cx, |state, cx| state.stack_thread(cx));
        true
    }

    /// An agent on the compact stack's top page reached other than from its
    /// lead has the lead put beneath it, for Back and the edge swipe to
    /// uncover.
    fn keep_lead_beneath(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.top_thread_page(cx) else {
            return;
        };
        if self.destination(cx) != Destination::Thread
            || self.mounted.last() != Some(&Destination::Thread)
            || self
                .attachment
                .as_ref()
                .is_none_or(|attachment| !attachment.leads.is_empty())
        {
            return;
        }
        let Some((store, lead)) = self.agent_lead(cx) else {
            return;
        };
        store.update(cx, |store, cx| {
            store.lease_timeline(&lead);
            if !store.is_thread_open(&lead) {
                store.open_thread(&lead, cx);
            }
        });
        let window_state = self.window_state.clone();
        let state = window_state.clone();
        let chat = cx.new(|cx| ChatView::new(store, state, Some(lead.clone()), window, cx));
        if let Some(attachment) = &mut self.attachment {
            attachment.leads.push(ThreadPage {
                page,
                frame: chat.read(cx).frame(),
                chat,
                session_id: lead,
            });
        }
        window_state.update(cx, |state, cx| state.stack_thread(cx));
        self.sync_nav(NavMotion::Immediate, cx);
    }

    /// Back to the thread under the compact page's agent, whose page comes
    /// back to the page pane, as it was left. The agent closes, as
    /// showing another thread would, and stays drawn, leased, on its page
    /// until that has slid off.
    fn pop_lead(&mut self, cx: &mut Context<Self>) {
        let top = self.nav.read(cx).current().map(AnyView::entity_id);
        let window_state = self.window_state.clone();
        let Some(attachment) = &mut self.attachment else {
            return;
        };
        let Some(lead) = attachment.leads.pop() else {
            return;
        };
        let store = attachment.link.store.clone();
        let pane = attachment.page().clone();
        let agent = pane.read(cx).session_id().map(str::to_owned);
        if let Some(agent) = &agent {
            store.update(cx, |store, _| store.lease_timeline(agent));
        }
        let chat = pane.update(cx, |pane, cx| pane.replace_chat(lead.chat, None, cx));
        store.update(cx, |store, cx| {
            store.selection_moved_on();
            store.release_timeline(&lead.session_id, cx);
            if let Some(agent) = &agent
                && store.is_thread_open(agent)
            {
                store.close_thread(agent, cx);
            }
        });
        if let (Some(page), Some(session_id)) = (top, agent) {
            attachment.leaving.push(ThreadPage {
                page,
                frame: chat.read(cx).frame(),
                chat,
                session_id,
            });
        }
        window_state.update(cx, |state, cx| state.unstack_threads(1, cx));
    }

    /// Let go of the leads from the `at`th on, each closing unless it is
    /// still shown. The page of the `at`th takes the page pane's
    /// conversation again and the thread pages above it go, at once: the
    /// conversation is what it was, whichever page it is on.
    fn drop_leads(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(attachment) = &mut self.attachment else {
            return;
        };
        let store = attachment.link.store.clone();
        let dropped = attachment.leads.split_off(at.min(attachment.leads.len()));
        if dropped.is_empty() {
            return;
        }
        for lead in &dropped {
            let shown = self.pane_showing(&lead.session_id, cx).is_some();
            store.update(cx, |store, cx| {
                if !shown && store.is_thread_open(&lead.session_id) {
                    store.close_thread(&lead.session_id, cx);
                }
                store.release_timeline(&lead.session_id, cx);
            });
        }
        self.window_state
            .update(cx, |state, cx| state.unstack_threads(dropped.len(), cx));
        self.sync_nav(NavMotion::Immediate, cx);
    }

    /// A lead that closed goes, with the pages above it.
    fn reconcile_leads(&mut self, cx: &mut Context<Self>) {
        let Some(attachment) = &self.attachment else {
            return;
        };
        let store = attachment.link.store.read(cx);
        if let Some(closed) = attachment
            .leads
            .iter()
            .position(|lead| !store.is_thread_open(&lead.session_id))
        {
            self.drop_leads(closed, cx);
        }
    }

    /// Going to `session_id` from the compact stack: a lead is Back to it,
    /// as an agent archived falls back to its lead; anywhere else leaves
    /// them all.
    fn unwind_leads(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(attachment) = &self.attachment else {
            return;
        };
        if attachment.leads.is_empty()
            || attachment.page().read(cx).session_id() == Some(session_id)
        {
            return;
        }
        match attachment
            .leads
            .iter()
            .rposition(|lead| lead.session_id == session_id)
        {
            Some(at) => {
                self.drop_leads(at + 1, cx);
                self.pop_lead(cx);
            }
            None => self.drop_leads(0, cx),
        }
    }

    /// Let go of the agents that have left.
    fn release_leaving(&mut self, cx: &mut Context<Self>) {
        let Some(attachment) = &mut self.attachment else {
            return;
        };
        let store = attachment.link.store.clone();
        for left in attachment.leaving.drain(..) {
            store.update(cx, |store, cx| store.release_timeline(&left.session_id, cx));
        }
    }

    /// The compact thread page's body, `kept` on a page that keeps its
    /// conversation: otherwise the skeleton while the thread is restored,
    /// then its chat rising over it.
    fn compact_thread_body(
        &mut self,
        kept: Option<Entity<crate::chat::ChatFrame>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Under the id and layer of a body at rest, so what the chat keeps by
        // element is the same on either side of the page keeping it.
        if let Some(frame) = kept {
            return crate::motion::Scene::default()
                .sample("compact-thread-body", ThreadBody::Chat, window, cx)
                .render([frame.into_any_element()])
                .into_any_element();
        }
        let pending = self.pending_navigation_restore.is_some();
        let Some(attachment) = &mut self.attachment else {
            return div().into_any_element();
        };
        let wanted = if pending {
            ThreadBody::Skeleton
        } else {
            ThreadBody::Chat
        };
        // A chat comes in over what the page shows once it appears; until
        // then it readies itself unseen beneath, and the page holds still.
        let chat = attachment.chat(cx);
        let chat_frame = attachment.page().read(cx).frame();
        let holding = attachment
            .thread_body_shown
            .filter(|shown| *shown != wanted && wanted != ThreadBody::Skeleton)
            .filter(|_| !chat.read(cx).appears(cx));
        let current = holding.unwrap_or(wanted);
        attachment.thread_body_shown = Some(current);
        ChatView::set_held(&chat, holding.is_some(), cx);
        if holding.is_some() {
            window.request_animation_frame();
        }
        let frame = attachment
            .thread_body
            .sample("compact-thread-body", current, window, cx);
        let layers: Vec<AnyElement> = frame
            .scenes()
            .map(|key| match key {
                ThreadBody::Skeleton => crate::material::loading_skeleton(cx),
                ThreadBody::Chat => chat_frame.clone().into_any_element(),
            })
            .collect();
        frame
            .render(layers)
            .when(holding.is_some(), |body| {
                body.child(
                    div()
                        .absolute()
                        .inset_0()
                        .opacity(0.)
                        .child(chat_frame.clone())
                        .child(crate::motion::shield()),
                )
            })
            .into_any_element()
    }

    /// The lead thread of the open agent, when one is open.
    fn agent_lead(&self, cx: &App) -> Option<(Entity<WorkspaceStore>, String)> {
        let store = self.attachment.as_ref()?.link.store.clone();
        let active = self.page_thread(cx)?;
        let parent = {
            let store = store.read(cx);
            let sessions = store.sidebar_sessions();
            let parent = sessions
                .iter()
                .find(|meta| meta.id == active)?
                .parent_session_id
                .clone()?;
            sessions
                .iter()
                .any(|meta| meta.id == parent)
                .then_some(parent)?
        };
        Some((store, parent))
    }

    /// Back to whatever is under this page, labelled with that destination's
    /// short fixed label. `None` at the root, where the platform owns Back.
    fn back_control(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let label = self.window_state.read(cx).parent()?.back_label();
        Some(self.back_action(label, cx))
    }

    fn back_action(&mut self, label: SharedString, cx: &mut Context<Self>) -> AnyElement {
        back_button("compact-back", label)
            .debug_selector(|| "compact-back".into())
            .on_click(cx.listener(|this, _, window, cx| {
                this.back(window, cx);
            }))
            .into_any_element()
    }

    fn palette_action(&self, cx: &mut Context<Self>) -> AnyElement {
        nav_icon_button(
            "compact-open-palette",
            crate::tr!("sidebar.search").into_owned(),
            IconName::Search,
        )
        .debug_selector(|| "compact-open-palette".into())
        .on_click(cx.listener(|this, _, window, cx| {
            this.on_toggle_palette(&TogglePalette, window, cx);
        }))
        .into_any_element()
    }

    fn render_hosts_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let back = self.back_control(cx);
        let body = self.hosts_surface(false, window, cx);
        page_surface(window, cx)
            .debug_selector(|| "hosts-page".into())
            .child(nav_bar(
                back,
                crate::tr!("hosts.title").into_owned().into(),
                None,
                self.attachment
                    .as_ref()
                    .map(|_| self.palette_action(cx))
                    .into_iter()
                    .collect(),
                window,
                cx,
            ))
            .child(body)
            .into_any_element()
    }

    /// The pairing form as its own page, so Back returns to whatever pushed it
    /// — the Hosts list, or the Nearby row that prefilled the endpoint.
    fn render_pair_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let back = self.back_control(cx);
        let body = self.hosts_surface(true, window, cx);
        page_surface(window, cx)
            .child(nav_bar(
                back,
                crate::tr!("hosts.pair.page_title").into_owned().into(),
                None,
                vec![],
                window,
                cx,
            ))
            .child(body)
            .into_any_element()
    }

    /// Settings, as two pages of the same stack: the section list, then one
    /// section's detail. Both wear the same nav bar as every other page.
    fn render_settings_page(
        &mut self,
        destination: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = self.back_control(cx);
        let Some(settings) = self
            .attachment
            .as_ref()
            .map(|attachment| attachment.settings_page.clone())
        else {
            return div().into_any_element();
        };
        let detail = destination != Destination::Settings;
        let title = if destination == Destination::SettingsSection
            && self.destination(cx) == Destination::SettingsThreadRules
        {
            crate::tr!("settings.threads").into_owned().into()
        } else if detail {
            settings.read(cx).section_title(cx)
        } else {
            crate::tr!("settings.title").into_owned().into()
        };
        let body = settings.update(cx, |page, cx| {
            if detail {
                page.render_compact_section(destination, window, cx)
            } else {
                page.render_compact_list(cx)
            }
        });
        page_surface(window, cx)
            .child(nav_bar(
                back,
                title,
                None,
                vec![self.palette_action(cx)],
                window,
                cx,
            ))
            .child(body)
            .into_any_element()
    }

    fn render_arrange_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(sidebar) = self
            .attachment
            .as_ref()
            .map(|attachment| attachment.sidebar.clone())
        else {
            return div().into_any_element();
        };
        use crate::widgets::button::{Button, ButtonVariants as _};
        let done = div()
            // A control press must not arm the ancestor window drag region.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new("arrange-done")
                    .ghost()
                    .label(crate::tr!("sidebar.arrange_done"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.back(window, cx);
                    })),
            )
            .into_any_element();
        let body = sidebar.update(cx, |sidebar, cx| sidebar.render_arrange(window, cx));
        page_surface(window, cx)
            .debug_selector(|| "compact-arrange-page".into())
            .child(nav_bar(None, "".into(), None, vec![done], window, cx))
            .child(body)
            .into_any_element()
    }

    fn render_threads_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let back = self.back_control(cx);
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let store = attachment.link.store.read(cx);
        // A remote machine's list is headed by the machine, with the link's
        // state where every other page puts its context. The local list is
        // just Threads; the way to the machines is the Back control.
        let (title, subtitle): (SharedString, Option<SharedString>) = match store.remote_host_name()
        {
            Some(_) => (
                store.machine_label().into(),
                Some(crate::remote::connection_label(&store.connection_state()).into()),
            ),
            None => (crate::tr!("mobile.threads").into_owned().into(), None),
        };
        let sidebar = attachment.sidebar.clone();
        let actions = vec![
            nav_icon_button(
                "compact-new-thread",
                compact_label("new_thread"),
                IconName::Plus,
            )
            .debug_selector(|| "compact-new-thread".into())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.start_thread(cx);
            }))
            .into_any_element(),
            nav_icon_button(
                "compact-settings",
                compact_label("settings"),
                IconName::Settings,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.open_settings(cx));
            }))
            .into_any_element(),
        ];
        page_surface(window, cx)
            .debug_selector(|| "compact-threads-page".into())
            .child(nav_bar(
                back,
                title,
                subtitle.map(|subtitle| nav_subtitle(subtitle, cx)),
                actions,
                window,
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.layout_part("sidebar", sidebar)),
            )
            .into_any_element()
    }

    /// New thread: one project starts a draft directly, several go through the
    /// palette, which already owns "new thread in <project>" and can search.
    /// On a phone the palette opens without the keyboard: the projects are
    /// there to tap, and the search pill still raises it on demand.
    fn start_thread(&mut self, cx: &mut Context<Self>) {
        let Some(attachment) = &self.attachment else {
            return;
        };
        let projects = attachment.link.store.read(cx).projects();
        let Some(project) = projects.first().cloned().filter(|_| projects.len() == 1) else {
            let mobile = crate::window_seam::is_mobile(cx);
            self.window_state.update(cx, |state, cx| {
                if mobile {
                    state.open_palette_without_keyboard(cx);
                } else {
                    state.open_palette(cx);
                }
            });
            return;
        };
        attachment.link.store.update(cx, |store, cx| {
            store.start_draft(project.id.clone(), project.root.clone(), cx)
        });
    }

    fn render_thread_page(
        &mut self,
        page: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let kept = self
            .attachment
            .as_ref()
            .and_then(|attachment| attachment.thread_page(page))
            .cloned();
        // By the page's own place: a lead's page keeps the label it lands
        // with, and one sliding off, which has no place, was over a thread.
        let beneath = self
            .nav
            .read(cx)
            .views()
            .position(|view| view.entity_id() == page)
            .and_then(|at| self.mounted.get(at.checked_sub(1)?))
            .copied();
        let back = self.back_action(beneath.unwrap_or(Destination::Thread).back_label(), cx);
        let body =
            self.compact_thread_body(kept.as_ref().map(|kept| kept.frame.clone()), window, cx);
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let store = attachment.link.store.read(cx);
        let active = match &kept {
            Some(kept) => store.chat_active_session_for(Some(&kept.session_id), cx),
            None => store.chat_active_session_for(self.page_thread(cx).as_deref(), cx),
        };
        let project = active
            .as_ref()
            .and_then(|(_, cwd, _)| {
                store
                    .projects()
                    .into_iter()
                    .find(|project| project.root == *cwd)
            })
            .map(|project| project.name);
        // Over a remote link the project shares its line with the link's state.
        let subtitle = if !store.scope().is_full() {
            Some(store.machine_label())
        } else {
            match (project, store.remote_host_name()) {
                (Some(project), Some(_)) => Some(format!(
                    "{project} · {}",
                    crate::remote::connection_label(&store.connection_state())
                )),
                (None, Some(_)) => Some(crate::remote::connection_label(&store.connection_state())),
                (project, None) => project,
            }
        };
        let title = active
            .map(|(title, _, draft)| {
                if draft {
                    compact_label("new_thread")
                } else {
                    title
                }
            })
            .unwrap_or_else(|| {
                compact_label(if self.pending_navigation_restore.is_some() {
                    "thread"
                } else {
                    "new_thread"
                })
            });
        page_surface(window, cx)
            .debug_selector(|| "compact-thread-page".into())
            .child(nav_bar(
                Some(back),
                title.into(),
                subtitle.map(|subtitle| nav_subtitle(subtitle, cx)),
                vec![
                    self.palette_action(cx),
                    nav_icon_button(
                        "compact-panels",
                        crate::tr!("chat.panels").into_owned(),
                        IconName::PanelRight,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_panels(cx);
                    }))
                    .into_any_element(),
                ],
                window,
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.layout_part("chat", body)),
            )
            .into_any_element()
    }

    /// The terminal, diff/plan and preview at full width. They are the same
    /// entities the wide layout puts in the split — a compact window has no
    /// room beside the timeline, not less of a product.
    fn render_panel_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let back = self.back_control(cx);
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let store = attachment.link.store.read(cx);
        let session = self.page_thread(cx);
        let panel = store.panel_state_for(session.as_deref());
        let selected_panel = self
            .pending_navigation_restore
            .as_ref()
            .map(|pending| pending.panel.id());
        let terminal = selected_panel.map_or(self.panel_shows_terminal, |id| id == "terminal");
        let show_agents = session.is_some_and(|id| {
            !crate::agents_panel::Agents::of(store, &id, cx).is_empty()
                || panel.right_tab == RightTab::Agents
                || selected_panel == Some("agents")
        });
        let show_preview = store.offers_preview();
        let page_pane = attachment.page().clone();
        page_pane.update(cx, |pane, cx| {
            pane.select_compact_preview(
                self.pending_navigation_restore.is_none()
                    && !terminal
                    && panel.right_tab == RightTab::Preview,
                window,
                cx,
            );
        });
        let shell = cx.entity();
        let segments = [
            (
                "terminal",
                crate::tr!("terminal.title").into_owned(),
                terminal,
            ),
            (
                "diff",
                crate::tr!("diff.title").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Diff, |id| {
                    id == "diff"
                }),
            ),
            (
                "plan",
                crate::tr!("tasks.tab").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Plan, |id| {
                    id == "plan"
                }),
            ),
            (
                "agents",
                crate::tr!("agents.title").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Agents, |id| {
                    id == "agents"
                }),
            ),
            (
                "pr",
                crate::tr!("pull_requests.title").into_owned(),
                selected_panel.map_or(
                    !terminal && panel.right_tab == RightTab::PullRequests,
                    |id| id == "pr",
                ),
            ),
            (
                "preview",
                crate::tr!("preview.title").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Preview, |id| {
                    id == "preview"
                }),
            ),
        ]
        .into_iter()
        .filter(|(id, _, _)| (*id != "preview" || show_preview) && (*id != "agents" || show_agents))
        .map(|(id, label, selected)| {
            let shell = shell.clone();
            TabItem::new(SharedString::from(format!("compact-panel-{id}")), label)
                .selected(selected)
                .on_click(move |_, _, cx| {
                    shell.update(cx, |shell, cx| shell.show_panel(id, cx));
                })
        })
        .collect::<Vec<_>>();
        let segments = TabBar::new("compact-panel-track")
            .segmented()
            .compact(true)
            .items(segments);

        // Each body keeps its own layer, so the one left fades out under the
        // one chosen; a page leaving the Preview fades natively as it is let go.
        let current = if self.pending_navigation_restore.is_some() {
            0
        } else if terminal {
            1
        } else if panel.right_tab == RightTab::Preview {
            2
        } else {
            3
        };
        let fade =
            crate::motion::CrossFade::new("compact-panel-body", current, 4, CHANGE, window, cx);
        let layers: Vec<(usize, AnyElement)> = (0..4)
            .filter(|layer| fade.shows(*layer))
            .map(|layer| {
                let body = match layer {
                    0 => crate::material::loading_skeleton(cx),
                    1 => attachment
                        .chat(cx)
                        .read(cx)
                        .terminal_drawer()
                        .into_any_element(),
                    2 => page_pane
                        .update(cx, |pane, cx| pane.drawer_preview(window, cx))
                        .into_any_element(),
                    _ => page_pane
                        .update(cx, |pane, cx| pane.drawer_tabs(cx))
                        .into_any_element(),
                };
                (layer, body)
            })
            .collect();
        let body = fade.fill().render(layers).size_full();
        // The drawer sizes its grid from the space it is actually given.
        if terminal {
            let size = window.viewport_size();
            let drawer = attachment.chat(cx).read(cx).terminal_drawer();
            let (width, height) = (f32::from(size.width), f32::from(size.height) - 110.);
            if !drawer.read(cx).is_size(width, height) {
                drawer.update(cx, |drawer, cx| drawer.resize(width, height, cx));
            }
        }

        page_surface(window, cx)
            .child(nav_bar(
                back,
                crate::tr!("chat.panels").into_owned().into(),
                None,
                vec![],
                window,
                cx,
            ))
            .child(
                div()
                    .flex_none()
                    .px(px(crate::material::COMPACT_PAGE_INSET))
                    .py(cx.theme().tokens.spacing.sm)
                    .child(segments),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    fn render_pull_request_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = self.back_control(cx);
        let Some(attachment) = &self.attachment else {
            return div().into_any_element();
        };
        let view = attachment.chat(cx).read(cx).pull_request_view(cx);
        let session = self.page_thread(cx);
        let key = session.as_deref().and_then(|session| {
            attachment
                .link
                .store
                .read(cx)
                .open_pull_request(session)
                .cloned()
        });
        if let (Some(session), Some(key)) = (session, key) {
            view.update(cx, |view, cx| view.show(session, key, cx));
        } else if self.destination(cx) == Destination::PullRequest {
            // The thread no longer shows the pull request: back to the thread.
            let window_state = self.window_state.clone();
            window.defer(cx, move |_, cx| {
                window_state.update(cx, |state, cx| state.back(cx));
            });
        }
        let (title, actions) = view.update(cx, |view, cx| (view.nav_title(), view.nav_actions(cx)));
        page_surface(window, cx)
            .child(nav_bar(back, title, None, actions, window, cx))
            .child(div().flex_1().min_h_0().child(view))
            .into_any_element()
    }

    /// Push the panel destination with something actually on it.
    fn open_panels(&mut self, cx: &mut Context<Self>) {
        let showing = self.attachment.as_ref().is_some_and(|attachment| {
            let session = self.page_thread(cx);
            let panel = attachment
                .link
                .store
                .read(cx)
                .panel_state_for(session.as_deref());
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
        if let Some(pending) = &mut self.pending_navigation_restore {
            pending.panel = match id {
                "terminal" => NavigationPanel::Terminal,
                "plan" => NavigationPanel::Plan,
                "agents" => NavigationPanel::Agents,
                "pr" => NavigationPanel::PullRequests,
                "preview" => NavigationPanel::Preview,
                _ => NavigationPanel::Diff,
            };
            pending.panel_open = true;
        }
        let Some(attachment) = &self.attachment else {
            return;
        };
        let store = attachment.link.store.clone();
        let session = self.page_thread(cx);
        let panel = store.read(cx).panel_state_for(session.as_deref());
        self.panel_shows_terminal = id == "terminal";
        store.update(cx, |store, cx| match id {
            "terminal" => {
                if !panel.terminal_open {
                    store.toggle_terminal_panel_for(session.unwrap_or_default(), cx);
                }
            }
            tab => {
                let tab = match tab {
                    "plan" => RightTab::Plan,
                    "agents" => RightTab::Agents,
                    "pr" => RightTab::PullRequests,
                    "preview" => RightTab::Preview,
                    _ => RightTab::Diff,
                };
                if !(panel.right_panel_open && panel.right_tab == tab)
                    && let Some(session) = &session
                {
                    store.toggle_tab_panel_for(session, tab, cx);
                }
            }
        });
        self.schedule_navigation_save(cx);
        cx.notify();
    }

    fn begin_edge_back(
        &mut self,
        origin: gpui::Point<Pixels>,
        width: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .page_back
            .as_ref()
            .is_some_and(|page| page.target.is_some())
        {
            return;
        }
        let sheet = crate::overlay::active_sheet(window, cx);
        self.page_back = if sheet.is_none()
            && self.compact(cx)
            && !crate::window_seam::keyboard_covers_window(window)
        {
            let mut views = self.nav.read(cx).views().cloned().collect::<Vec<_>>();
            views.pop().and_then(|current| {
                views.pop().map(|previous| PageBack {
                    previous,
                    current,
                    progress: 0.,
                    target: None,
                })
            })
        } else {
            None
        };
        self.back_swipe = Some(BackSwipe {
            origin,
            progress: crate::edge_back::BackProgress {
                width,
                ..Default::default()
            },
            sheet,
            last_moved_at: cx.background_executor().now(),
            finished: false,
        });
    }

    fn move_edge_back(&mut self, phase: TouchPhase, window: &mut Window, cx: &mut Context<Self>) {
        let Some(swipe) = &mut self.back_swipe else {
            return;
        };
        if swipe.finished {
            return;
        }
        let ended = matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled);
        let commit = phase == TouchPhase::Ended && swipe.progress.commits();
        let progress = swipe.progress.progress();
        if let Some(sheet) = &swipe.sheet {
            let close = sheet.update(cx, |sheet, cx| {
                sheet.dragging = !ended;
                sheet.progress = if ended {
                    if commit { 1. } else { 0. }
                } else {
                    progress
                };
                App::notify(cx, sheet.parent);
                sheet.close.clone()
            });
            if commit {
                close(window, cx);
            }
        } else if let Some(page) = &mut self.page_back {
            page.progress = progress;
            if ended {
                page.target = Some(if commit { 1. } else { 0. });
            }
        } else if commit {
            self.back(window, cx);
        }
        if ended && let Some(swipe) = &mut self.back_swipe {
            swipe.finished = true;
        }
        cx.notify();
    }

    // gpui-base has no edge recognizer; observe GPUI's original pan contact so
    // ordinary taps still reach the Back control. UIKit supplies this on iOS.
    fn swipe_back_stack(
        &self,
        stack: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let shell = cx.entity().downgrade();
        // Edge capture must run before ScrollBounce can consume the release packet.
        div().relative().size_full().children([
            canvas(
                |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |bounds, hitbox, window, _| {
                    // Claiming TouchDrag would swallow edge taps. GPUI keeps
                    // a pan's original contact in ScrollEvent.position.
                    window.on_mouse_event(move |event: &gpui::ScrollEvent, phase, window, cx| {
                        if !phase.capture() {
                            return;
                        }
                        let gpui::ScrollDelta::Pixels(delta) = event.delta else {
                            return;
                        };
                        let Some(shell) = shell.upgrade() else { return };
                        shell.update(cx, |shell, cx| {
                            if delta.y != px(0.) {
                                shell.move_edge_back(TouchPhase::Cancelled, window, cx);
                                return;
                            }
                            if event.touch_phase == TouchPhase::Started {
                                shell.back_swipe = None;
                                if (crate::overlay::active_sheet(window, cx).is_none()
                                    && (!hitbox.should_handle_scroll(window)
                                        || gpui_base::GlobalState::is_in_deferred_context(cx)
                                        || shell.window_state.read(cx).palette_mounted()
                                        || !shell.compact(cx)
                                        || shell.nav.read(cx).depth() <= 1))
                                    || !bounds.contains(&event.position)
                                    || event.position.x - bounds.left()
                                        > crate::edge_back::EDGE_WIDTH
                                {
                                    return;
                                }
                                shell.begin_edge_back(
                                    event.position,
                                    bounds.size.width,
                                    window,
                                    cx,
                                );
                            }
                            let Some(swipe) = shell.back_swipe.as_mut() else {
                                return;
                            };
                            if event.position != swipe.origin {
                                return;
                            }
                            cx.stop_propagation();
                            if swipe.finished {
                                return;
                            }
                            let now = cx.background_executor().now();
                            swipe.progress.travel += delta.x;
                            if event.touch_phase == TouchPhase::Moved && delta.x != px(0.) {
                                let elapsed = now.duration_since(swipe.last_moved_at).as_secs_f32();
                                swipe.progress.velocity = f32::from(delta.x) / elapsed.max(0.001);
                                swipe.last_moved_at = now;
                            }
                            if !matches!(
                                event.touch_phase,
                                TouchPhase::Ended | TouchPhase::Cancelled
                            ) {
                                shell.move_edge_back(event.touch_phase, window, cx);
                            }
                            let Some(swipe) = shell.back_swipe.as_mut() else {
                                return;
                            };
                            if matches!(
                                event.touch_phase,
                                TouchPhase::Ended | TouchPhase::Cancelled
                            ) {
                                if now.duration_since(swipe.last_moved_at)
                                    > Duration::from_millis(100)
                                {
                                    swipe.progress.velocity = 0.;
                                }
                                let origin = swipe.origin;
                                shell.move_edge_back(event.touch_phase, window, cx);
                                if event.touch_phase == TouchPhase::Ended {
                                    crate::edge_back::catch_fling(origin, window, cx);
                                }
                            }
                        });
                    });
                },
            )
            .absolute()
            .inset_0()
            .into_any_element(),
            stack.into_any_element(),
        ])
    }

    fn render_compact(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.nav.read(cx).is_empty() {
            self.sync_nav(NavMotion::Immediate, cx);
        }
        if self.pages_changing.take() {
            window.request_animation_frame();
        } else {
            self.keep_lead_beneath(window, cx);
            self.release_leaving(cx);
        }
        let width = window.viewport_size().width;
        // gpui-base exposes NavPage::progress read-only and mounts the
        // previous entry only after pop. Bind interactive progress here, at the
        // navigation owner, until base supplies that API. Retain the same views;
        // compact ChatView excludes the terminal, so Thread and Panel never
        // mount the composer or terminal twice.
        if self.page_back.is_none() {
            crate::edge_back::settle("page-edge-back", 0., true, window, cx);
        }
        let interactive = self.page_back.as_ref().map(|page| {
            let progress = crate::edge_back::settle(
                "page-edge-back",
                page.target.unwrap_or(page.progress),
                page.target.is_none(),
                window,
                cx,
            );
            (
                page.previous.clone(),
                page.current.clone(),
                progress,
                page.target,
            )
        });
        let interactive = interactive.and_then(|(previous, current, progress, target)| {
            if target == Some(progress) {
                self.page_back = None;
                if target == Some(1.) {
                    self.back(window, cx);
                    self.sync_nav(NavMotion::Immediate, cx);
                }
                None
            } else {
                Some((previous, current, progress))
            }
        });
        // 200ms lateral push/pop. GPUI has no paint transform, so each page is
        // offset by its own insets; setting `left` and `right` in opposite
        // directions slides it without resizing it.
        let stack = if let Some((previous, current, progress)) = interactive {
            let offset = crate::edge_back::BackProgress::offset(progress, width);
            let underneath = (offset - width) * 0.25;
            div()
                .relative()
                .size_full()
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(underneath)
                        .right(-underneath)
                        .debug_selector(|| "back-previous-page".into())
                        .child(previous)
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .bg(gpui::black().opacity(0.15 * (1. - progress))),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(offset)
                        .right(-offset)
                        .debug_selector(|| "back-current-page".into())
                        .child(current),
                )
                .child(div().absolute().inset_0().occlude())
                .into_any_element()
        } else {
            let pages_changing = self.pages_changing.clone();
            NavStack::new(&self.nav)
                .size_full()
                .overflow_hidden()
                .transition(Transition::new(NAV_TRANSITION))
                .item(move |page, _, _| {
                    if page.phase() == PresencePhase::Exiting {
                        pages_changing.set(true);
                    }
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
                })
                .into_any_element()
        };
        v_flex()
            .id("app-shell")
            .relative()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .text_color(cx.theme().foreground)
            .when(cfg!(target_os = "android"), |mut element| {
                // Android's cosmic-text fallback has no script preferences. These
                // system sans families keep Han text out of arbitrary serif faces.
                element.text_style().font_fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
                    "Noto Sans CJK SC".into(),
                    "Noto Sans SC".into(),
                    "Source Han Sans SC".into(),
                ]));
                element
            })
            .text_token(&cx.theme().tokens.typography.md)
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_open_source_control))
            .key_context("TcodeShell")
            .on_action(cx.listener(Self::on_thread_undo))
            .on_action(cx.listener(Self::on_open_thread_anchor))
            // Every compact page, settings included, is one entry of the same
            // stack: one nav bar, one Back, one transition.
            .child(stack)
            .map(|el| {
                el.children(
                    self.attachment
                        .as_ref()
                        .map(|attachment| attachment.palette.clone()),
                )
            })
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Wide layout
// ---------------------------------------------------------------------------

impl AppShell {
    /// The hosts surface itself, without chrome: the same rows and the same
    /// pairing form at both widths. Compact pushes Pair onto the navigation
    /// stack; wide shows it in place, inside the Hosts route.
    /// The machines and the pairing form, each in its own layer, so the one
    /// left fades out under the one opened in its place.
    fn hosts_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pairing = self.destination(cx) == Destination::Pair;
        let fade = crate::motion::CrossFade::new(
            "hosts-body",
            usize::from(pairing),
            2,
            CHANGE,
            window,
            cx,
        );
        let layers: Vec<(usize, AnyElement)> = (0..2)
            .filter(|layer| fade.shows(*layer))
            .map(|layer| {
                let surface = self.hosts_surface(layer == 1, window, cx);
                (
                    layer,
                    v_flex().size_full().child(surface).into_any_element(),
                )
            })
            .collect();
        fade.fill()
            .render(layers)
            .flex_1()
            .min_h_0()
            .w_full()
            .into_any_element()
    }

    /// The hosts rows, or the pairing form when `pairing`; nothing in a
    /// departed workspace while the arriving one draws them.
    fn hosts_surface(
        &mut self,
        pairing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self
            .departing
            .as_ref()
            .is_some_and(|departing| departing.hosts_withheld)
        {
            return div().flex_1().into_any_element();
        }
        let hosts = self.hosts.clone();
        hosts.update(cx, |panel, cx| {
            if pairing {
                panel.render_pair(window, cx)
            } else {
                panel.render_hosts(window, cx)
            }
        })
    }

    /// The wide Hosts route's top strip: the page title and (on Windows) the
    /// caption cluster this route's content column owns. The sidebar switches
    /// wide routes, so this strip deliberately has no Back control.
    fn render_hosts_header(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let hosts_caption = window_caption::hosts_caption_for_state(
            window_caption::CaptionSurface::Hosts,
            self.window_state.read(cx).route(),
            false,
            RightTab::Diff,
        );
        let pairing = self.destination(cx) == Destination::Pair;
        let title: SharedString = if pairing {
            crate::tr!("hosts.pair.page_title").into_owned().into()
        } else {
            crate::tr!("hosts.title").into_owned().into()
        };
        // Machines is a root the sidebar switches to; Pair is pushed over it,
        // so only Pair has somewhere to go back to.
        let gap = cx.theme().tokens.spacing.xs;
        let back =
            crate::motion::shown("hosts-back-shown", pairing.then_some(()), SHOW, window, cx).map(
                |shown| {
                    shown.fold_across_gap(
                        gap,
                        back_button("hosts-back", Destination::Hosts.back_label())
                            .debug_selector(|| "hosts-back".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.window_state
                                    .update(cx, |state, cx| state.close_pair(cx));
                            })),
                    )
                },
            );
        let title = div().flex_1().min_w_0().child(crate::motion::cross_fade(
            "hosts-title",
            title,
            CHANGE,
            window,
            cx,
            |title, _, cx| crate::material::bar_title(title.clone(), cx).into_any_element(),
        ));
        // Closes the route as the sidebar's Machines toggle does; without a
        // workspace beneath it there is nothing to close to.
        let close = self
            .window_state
            .read(cx)
            .history()
            .iter()
            .any(|destination| destination.route() != Route::Hosts)
            .then(|| {
                crate::widgets::button::close_button("hosts-close", crate::tr!("hosts.close"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.window_state.update(cx, |state, cx| {
                            state.back(cx);
                        });
                    }))
            });
        // The column is centred in the strip; whatever holds the strip's
        // right end is kept clear of it.
        let right_end = if hosts_caption {
            window_caption::CAPTION_CLUSTER_WIDTH
        } else {
            0.
        } + if close.is_some() {
            crate::chat::PANE_CLOSE_WIDTH
        } else {
            0.
        };
        window_drag_area(
            "hosts-header-drag",
            crate::settings_rows::content_header(
                crate::material::bar("hosts-header", crate::material::Bar::Strip, false, cx),
                cx,
            )
            .relative(),
            window,
            cx,
        )
        .child(
            crate::settings_rows::content_header_column()
                .items_center()
                .gap(gap)
                .pr(px(right_end))
                .children(back)
                .child(window_caption::drag_region(title)),
        )
        .child(
            h_flex()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .items_center()
                .when(!hosts_caption, |end| end.pr(cx.theme().tokens.spacing.sm))
                .children(close)
                .children(hosts_caption.then(|| window_caption::caption_controls(window, cx))),
        )
        .into_any_element()
    }

    /// The wide window: the workspace it is attached to, rising over the
    /// ones it left until it covers them. gpui-base has no transition for a
    /// whole tree being replaced; the window keeps the trees it left
    /// ([`Self::retire`]) and [`crate::motion::Scene`] draws them beneath.
    fn render_wide(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let route = self.window_state.read(cx).route();
        let frame = self
            .attachments
            .sample("attachments", self.scene_key, window, cx);
        let keys: Vec<u64> = frame.scenes().copied().collect();
        self.departed
            .retain(|departed| keys.contains(&departed.key));
        let hosts_withheld = self.attachment.is_none() || route == Route::Hosts;
        let mut layers = Vec::with_capacity(keys.len());
        for key in keys {
            if key == self.scene_key {
                layers.push(self.render_workspace(route, window, cx));
                continue;
            }
            let Some(ix) = self.departed.iter().position(|left| left.key == key) else {
                layers.push(gpui::Empty.into_any_element());
                continue;
            };
            let mut left = self.departed.remove(ix);
            // Its keys would reach a thread the window has left.
            if left.attachment.as_ref().is_some_and(|attachment| {
                attachment
                    .all_panes(cx)
                    .iter()
                    .any(|pane| pane.focus_handle(cx).contains_focused(window, cx))
            }) {
                window.blur(cx);
            }
            std::mem::swap(&mut self.attachment, &mut left.attachment);
            self.departing = Some(Departing {
                destination: left.destination,
                hosts_withheld,
            });
            layers.push(self.render_workspace(left.route, window, cx));
            self.departing = None;
            std::mem::swap(&mut self.attachment, &mut left.attachment);
            self.departed.insert(ix, left);
        }
        frame.render(layers).into_any_element()
    }

    /// The wide window showing `route` of whatever `self.attachment` is.
    fn render_workspace(
        &mut self,
        route: Route,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fullscreen = window.is_fullscreen();
        if self.attachment.is_none() {
            // No host: the hosts page is the whole window, and the only page
            // there is. A tablet in landscape lands here before it attaches
            // and a desktop after it disconnects; neither has a sidebar to
            // switch routes with, so the page wears the same nav bar as its
            // compact self — the title, and on Pair the Back that returns to
            // Machines. Whatever the history says, nothing else is shown
            // until `attach` enters a workspace.
            let page = if self.destination(cx) == Destination::Pair {
                self.render_pair_page(window, cx)
            } else {
                self.render_hosts_page(window, cx)
            };
            return div()
                .id("app-shell")
                .size_full()
                // The theme's tint, not `material::canvas`, which clears it
                // over the macOS backdrop: this page keeps its tinted glass.
                .bg(cx.theme().background)
                .text_color(cx.theme().foreground)
                .child(v_flex().id("hosts").size_full().child(page))
                .into_any_element();
        }
        let Some(attachment) = &mut self.attachment else {
            unreachable!("checked above");
        };
        let collapsed = self.window_state.read(cx).sidebar_collapsed;
        // The overlay is workspace-only transient state. Clear it synchronously
        // on route/expanded transitions rather than waiting for pointer input.
        if !collapsed || route != Route::Chat {
            attachment.sidebar_overlay_visible = false;
        }

        // A native WebView is not composited into GPUI and survives removal of
        // its layout node. Synchronize it before a route change or a
        // right-panel tab/close transition can unmount PreviewPanel.
        if self.departing.is_none() {
            for pane in attachment.all_panes(cx) {
                ThreadPanel::sync_preview(&pane, cx);
            }
        }

        let scene = attachment.routes.sample("routes", route, window, cx);
        let scenes: Vec<Route> = scene.scenes().copied().collect();
        // The workspace comes back as it is, not gliding from how it was.
        if !scenes.contains(&Route::Chat) {
            attachment.panel_glide.reset();
        }
        // The sidebar both workspace routes keep is drawn once, by the lowest
        // of them: that one is opaque, so the sidebar holds still while the
        // routes change around it.
        let sidebar_scene = scenes
            .iter()
            .copied()
            .find(|shown| !collapsed && *shown != Route::Settings);
        let layers: Vec<AnyElement> = scenes
            .into_iter()
            .map(|shown| {
                let sidebar = collapsed || sidebar_scene == Some(shown);
                // Settings draws its own rail where the workspace routes draw
                // their sidebar.
                let column = scene.unshared(&shown, |above| {
                    (*above == Route::Settings) == (shown == Route::Settings)
                });
                let sidebar = sidebar.then_some(column);
                match shown {
                    Route::Settings => self.render_settings_route(fullscreen, column, cx),
                    Route::Hosts => self.render_hosts_route(sidebar, fullscreen, window, cx),
                    Route::Chat => self.render_chat_route(sidebar, window, cx),
                }
            })
            .collect();
        scene.render(layers).into_any_element()
    }

    /// The Settings route, its rail at `rail` opacity.
    fn render_settings_route(
        &mut self,
        fullscreen: bool,
        rail: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let attachment = self
            .attachment
            .as_ref()
            .expect("a workspace route has an attachment");
        SettingsPage::fade_rail(&attachment.settings_page, rail, cx);
        // Settings is a window of its own: opening it switches the whole
        // window, so the settings rail — not the workspace sidebar — is the
        // left column. Root owns the translucent canvas across both routes;
        // Settings paints its own rail and content surfaces over it.
        div()
            .id("app-shell")
            .size_full()
            // Fullscreen flattens the canvas under the paper (see the
            // workspace root below).
            .when(fullscreen, |this| {
                this.bg(crate::material::opaque_canvas(cx))
            })
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_open_source_control))
            .key_context("TcodeShell")
            .on_action(cx.listener(Self::on_thread_undo))
            .on_action(cx.listener(Self::on_open_thread_anchor))
            .child(
                div()
                    .id("workspace")
                    .flex_1()
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .debug_selector(|| "settings-route".into())
                    .child(attachment.settings_page.clone()),
            )
            .into_any_element()
    }

    /// The Hosts route; `sidebar`, the opacity it draws the sidebar at, or
    /// none to leave its place empty for the route beneath it to show.
    fn render_hosts_route(
        &mut self,
        sidebar: Option<f32>,
        fullscreen: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = self.window_state.read(cx).sidebar_collapsed;
        let attachment = self
            .attachment
            .as_ref()
            .expect("a workspace route has an attachment");
        let sidebar_width = attachment.sidebar_width.get();
        let sidebar = match sidebar {
            Some(opacity) => div()
                .size_full()
                .opacity(opacity)
                .child(self.layout_part("sidebar", attachment.sidebar.clone()))
                .into_any_element(),
            None => div().into_any_element(),
        };
        // The sidebar's column narrows away as it collapses and widens back,
        // the sidebar keeping to its far edge, as it does beside the chat.
        let shown_width = gpui_base::motion::transition(
            "hosts-route-sidebar",
            if collapsed { px(0.) } else { sidebar_width },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        // Hosts is a product surface, not a settings page: the sidebar stays,
        // because the feature area that leads here is meant to be persistent.
        let body = self.hosts_body(window, cx);
        let header = self.render_hosts_header(window, cx);
        let column = crate::window_seam::inset_from_seam(
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .debug_selector(|| "hosts-route".into())
                .bg(crate::material::content_surface(cx))
                .shadow_sm(),
            "hosts-column-seam",
            gpui::Edges {
                left: collapsed,
                ..gpui::Edges::all(true)
            },
            window,
            cx,
        )
        .child(header)
        .child(body);
        div()
            .id("app-shell")
            .size_full()
            .when(fullscreen, |this| {
                this.bg(crate::material::opaque_canvas(cx))
            })
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_open_source_control))
            .key_context("TcodeShell")
            .on_action(cx.listener(Self::on_thread_undo))
            .on_action(cx.listener(Self::on_open_thread_anchor))
            .child(
                h_flex()
                    .id("workspace")
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .when(shown_width > px(0.), |row| {
                        row.child(
                            h_flex()
                                .flex_none()
                                .w(shown_width)
                                .h_full()
                                .justify_end()
                                .overflow_hidden()
                                .child(div().flex_none().w(sidebar_width).h_full().child(sidebar)),
                        )
                    })
                    .child(column),
            )
            .into_any_element()
    }

    /// The Chat route; `sidebar` as for [`Self::render_hosts_route`].
    fn render_chat_route(
        &mut self,
        draw_sidebar: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette_open = self.window_state.read(cx).palette_mounted() && self.departing.is_none();
        let fullscreen = window.is_fullscreen();
        let collapsed = self.window_state.read(cx).sidebar_collapsed;
        let Some(attachment) = &mut self.attachment else {
            unreachable!("a workspace route has an attachment");
        };
        let split = attachment.split.clone();
        // The chat and the sidebar, which glides as it collapses and expands.
        let glide = attachment.panel_glide.sample(
            &[
                crate::motion::resize_glide::Panel::sized(
                    attachment.sidebar_width.get(),
                    !collapsed,
                )
                .kept(),
                crate::motion::resize_glide::Panel::flex(true),
            ],
            true,
            &split,
            window,
            cx,
        );

        // The chat's reading surface sits above the translucent canvas.
        let chat_panel = resizable_panel()
            .size_range(glide.range(1, gpui_base::PANEL_MIN_SIZE..Pixels::MAX))
            .child(glide.hold(
                1,
                self.layout_part("chat", self.render_chat_column(collapsed, window, cx)),
            ));
        let attachment = self.attachment.as_ref().expect("attachment checked above");
        let remembered_sidebar = attachment.sidebar_width.clone();
        let sidebar = match draw_sidebar {
            Some(opacity) => div()
                .size_full()
                .opacity(opacity)
                .child(self.layout_part("sidebar", attachment.sidebar.clone()))
                .into_any_element(),
            None => div().into_any_element(),
        };
        let overlay_width = attachment.sidebar_width.get();
        let overlay_visible = attachment.sidebar_overlay_visible;
        let palette = attachment.palette.clone();
        let group = h_resizable("workspace-panels")
            .with_state(&split)
            .on_resize(move |state, _, cx| {
                if !collapsed && let Some(size) = state.read(cx).sizes().first() {
                    remembered_sidebar.set(*size);
                }
            });
        // The sidebar is drawn once a frame: in its panel while that shows
        // it, gliding out included, and in the overlay only after.
        let sidebar_docked = glide.shows(0);
        // A popover keeps the overlay alive through its occluded-hover false
        // transition (see next_sidebar_overlay_visibility). When the popover
        // dismisses with the pointer already outside the overlay, no hover
        // event follows — reap the stale overlay here instead (dismissal
        // refreshes the window, so this pass always runs).
        let overlay_visible = if collapsed
            && overlay_visible
            && !gpui_base::GlobalState::is_in_deferred_context(cx)
            && window.mouse_position().x > overlay_width
        {
            if let Some(attachment) = &mut self.attachment {
                attachment.sidebar_overlay_visible = false;
            }
            false
        } else {
            overlay_visible && !sidebar_docked
        };
        let (docked_sidebar, overlay_sidebar) = match sidebar_docked {
            true => (Some(sidebar), None),
            false => (None, Some(sidebar)),
        };
        // The hover overlay slides in from the window's edge and back out,
        // drawn as it was while it goes.
        let overlay_in = gpui_base::motion::transition(
            "sidebar-hover-overlay-in",
            if overlay_visible { 1. } else { 0. },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let overlay_leaving = !overlay_visible && overlay_in > 0. && collapsed;
        let workspace = div()
            .relative()
            .size_full()
            .child(
                group
                    .child(
                        resizable_panel()
                            .fixed()
                            .size(overlay_width)
                            .size_range(glide.range(0, px(220.)..px(380.)))
                            .when_some(docked_sidebar, |panel, sidebar| {
                                panel.child(glide.hold(0, sidebar))
                            }),
                    )
                    .child(chat_panel),
            )
            // Collapsed, the group runs to the window's left edge. This fixed
            // transparent strip only opens the overlay, an independent
            // absolute sibling, so neither reflows the columns. Its inevitable
            // false transition when the overlay occludes it is deliberately
            // ignored by the state machine.
            .when(collapsed, |this| {
                this.child(
                    div()
                        .id("sidebar-hover-trigger")
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(px(SIDEBAR_HOVER_EDGE))
                        .occlude()
                        .on_hover(crate::motion::pointer_hover(cx.listener(
                            |this, hovered: &bool, _, cx| {
                                this.update_sidebar_overlay(
                                    SidebarHoverTransition::Trigger(*hovered),
                                    cx,
                                );
                            },
                        ))),
                )
            })
            .when_some(
                overlay_sidebar.filter(|_| overlay_visible || overlay_leaving),
                |this, sidebar| {
                    this.child(
                        crate::material::floating_panel(
                            div().id("sidebar-hover-overlay"),
                            crate::material::Side::Left,
                            cx,
                        )
                        .debug_selector(|| "sidebar-hover-overlay".into())
                        .absolute()
                        .left(px(0.) - overlay_width * (1. - overlay_in))
                        .top_0()
                        .h_full()
                        .w(overlay_width)
                        // The blocker and hover listener share this hitbox: the
                        // overlay owns its full visible lifetime.
                        .occlude()
                        .on_hover(crate::motion::pointer_hover(cx.listener(
                            |this, hovered: &bool, _, cx| {
                                this.update_sidebar_overlay(
                                    SidebarHoverTransition::Overlay(*hovered),
                                    cx,
                                );
                            },
                        )))
                        .child(sidebar)
                        .when(overlay_leaving, |overlay| {
                            overlay.child(crate::motion::shield())
                        }),
                    )
                },
            )
            .into_any_element();

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
            .on_action(cx.listener(Self::on_open_source_control))
            .key_context("TcodeShell")
            .on_action(cx.listener(Self::on_thread_undo))
            .on_action(cx.listener(Self::on_open_thread_anchor))
            .child(
                div()
                    .id("workspace")
                    .flex_1()
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .debug_selector(|| "chat-route".into())
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
            (state.sidebar_collapsed, state.route())
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
        self.glide_panes(window, cx);
        self.swap_layout(window, cx);
        self.zoom_panes(window, cx);
        self.sync_panes(window, cx);
        if let Some(attachment) = &self.attachment {
            let title = attachment
                .link
                .store
                .read(cx)
                .shell_window_title_for(self.page_thread(cx).as_deref(), cx);
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
        if palette_leaving
            && let Some(attachment) = &self.attachment
            && attachment
                .palette
                .focus_handle(cx)
                .contains_focused(window, cx)
        {
            cx.defer_in(window, |this, window, cx| {
                this.return_keyboard(window, cx);
            });
        }
        if palette_open
            && !self.palette_was_open
            && let Some(attachment) = &self.attachment
        {
            attachment
                .palette
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
            if let Some(attachment) = self.attachment.as_mut() {
                attachment.routes = Default::default();
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
            .attachment
            .as_ref()
            .map(|attachment| attachment.palette.read(cx).outside_dismissal.clone());
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
            .children(dismissal.map(|dismissal| dismissal.release_listener().into_any_element()))
            .child(body)
            .children(
                self.attachment
                    .as_ref()
                    .and_then(|attachment| attachment.preview.read(cx).native_fade(cx)),
            )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;

    use gpui::{TestAppContext, VisualTestContext, size};
    use tcode_client::host::Transport;
    use tcode_protocol::{
        ClientPayload, Command, EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic,
        decode_client_line, encode_line,
    };

    use super::*;

    mod compact_agent;
    mod docked_panel;
    mod lead_scrollbar;
    mod scenes;

    /// One shell over a transport the test holds both ends of, so what the
    /// client says to its host is observable and what the transport says
    /// about the link can be dictated.
    struct MountedShell {
        outgoing: async_channel::Receiver<String>,
        observed: RefCell<Vec<tcode_protocol::ClientMessage>>,
        incoming: async_channel::Sender<String>,
        states: async_channel::Sender<tcode_client::ConnectionState>,
    }

    struct ReturningClient {
        saved: tcode_client::pairing::PairedHost,
        /// One per attachment the test expects, in order.
        transports: RefCell<Vec<Transport>>,
        preferences: RefCell<tcode_client::host::ClientPreferences>,
        machine_exists: bool,
        saves: Cell<usize>,
        outbox: Option<std::sync::Arc<dyn tcode_client::outbox::Storage>>,
    }

    impl ClientHost for ReturningClient {
        fn outbox_storage(
            &self,
            _: &str,
        ) -> Option<std::sync::Arc<dyn tcode_client::outbox::Storage>> {
            self.outbox.clone()
        }
        fn device_name(&self) -> String {
            "returning phone".into()
        }

        fn device_id(&self) -> String {
            "returning-phone".into()
        }

        fn device_platform(&self) -> Option<String> {
            None
        }

        fn load_hosts(&self) -> Vec<tcode_client::pairing::PairedHost> {
            if self.machine_exists {
                vec![self.saved.clone()]
            } else {
                Vec::new()
            }
        }

        fn load_preferences(&self) -> tcode_client::host::ClientPreferences {
            self.preferences.borrow().clone()
        }

        fn save_preferences(&self, preferences: &tcode_client::host::ClientPreferences) {
            *self.preferences.borrow_mut() = preferences.clone();
            self.saves.set(self.saves.get() + 1);
        }

        fn save_hosts(&self, _: &[tcode_client::pairing::PairedHost]) {}

        fn last_host_id(&self) -> Option<String> {
            Some(self.saved.host_id.clone())
        }

        fn set_last_host_id(&self, host_id: Option<&str>) {
            assert_eq!(host_id, Some(self.saved.host_id.as_str()));
        }

        fn pair(
            &self,
            _: tcode_client::pairing::PairInvite,
            _: Option<std::net::IpAddr>,
        ) -> tcode_client::host::HostFuture<'_, Result<tcode_client::pairing::PairedHost, String>>
        {
            panic!("a cold start must reuse the saved pairing")
        }

        fn connect(&self, host: &tcode_client::pairing::PairedHost) -> Transport {
            assert_eq!(host.host_id, self.saved.host_id);
            let mut transports = self.transports.borrow_mut();
            assert!(!transports.is_empty(), "an unexpected connection");
            transports.remove(0)
        }
    }

    /// A returning phone: a mobile build at phone width, so the restored
    /// destination is a page of the compact stack.
    fn mount_restored<'a>(
        cx: &'a mut TestAppContext,
        history: &[&str],
        machine_exists: bool,
    ) -> (
        Entity<AppShell>,
        MountedShell,
        Rc<ReturningClient>,
        &'a mut VisualTestContext,
    ) {
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        mount_restored_at_width(cx, history, machine_exists, 393., "plan")
    }

    fn mount_restored_at_width<'a>(
        cx: &'a mut TestAppContext,
        history: &[&str],
        machine_exists: bool,
        width: f32,
        panel: &str,
    ) -> (
        Entity<AppShell>,
        MountedShell,
        Rc<ReturningClient>,
        &'a mut VisualTestContext,
    ) {
        cx.update(crate::theme::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let (states, state) = async_channel::unbounded();
        let client = Rc::new(ReturningClient {
            saved: tcode_client::pairing::PairedHost {
                host_id: "last-host".into(),
                name: "Last machine".into(),
                traverse: Vec::new(),
                relay: None,
                addrs: vec!["127.0.0.1:47503".into()],
                last_connected_unix: Some(1),
                group_id: None,
                space_id: None,
                space_name: None,
            },
            transports: RefCell::new(vec![Transport {
                originals: None,
                to_host: to_host.into(),
                from_host,
                state,
                current_host: None,
            }]),
            preferences: RefCell::new(tcode_client::host::ClientPreferences {
                navigation: Some(serde_json::json!({
                    "history": history,
                    "host_id": "last-host",
                    "session_id": "thread-a",
                    "panel": panel
                })),
                ..Default::default()
            }),
            machine_exists,
            saves: Cell::new(0),
            outbox: None,
        });
        let setup_client = client.clone();
        let window = cx.open_window(size(px(width), px(852.)), move |window, cx| {
            let state = cx.new(|_| WindowState::new(false));
            AppShell::new(
                state,
                ShellSetup {
                    initial: crate::last_host_target(setup_client.as_ref()),
                    client_host: Some(setup_client),
                    restore_navigation: true,
                    ..Default::default()
                },
                window,
                cx,
            )
        });
        let shell = window.root(cx).unwrap();
        let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
        if let Some(store) = shell.read_with(cx, |shell, _| shell.store()) {
            crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        }
        draw(cx);
        (
            shell,
            MountedShell {
                outgoing,
                observed: RefCell::new(Vec::new()),
                incoming,
                states,
            },
            client,
            cx,
        )
    }

    /// Re-pairing with a machine ends on `switch_to` with that machine's id.
    /// The link it refused has stopped reconnecting, so it is rebuilt rather
    /// than kept saying the pairing was rejected; a pairing into one of its
    /// spaces is likewise a new link, not the full one it replaces.
    #[gpui::test]
    fn repairing_rebuilds_a_link_the_machine_rejected(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, client, cx) = mount_restored(cx, &["threads"], true);
        let fresh_transport = || {
            let (to_host, _outgoing) = async_channel::unbounded();
            let (_incoming, from_host) = async_channel::unbounded();
            let (_states, state) = async_channel::unbounded();
            Transport {
                originals: None,
                to_host: to_host.into(),
                from_host,
                state,
                current_host: None,
            }
        };
        let rejected = tcode_client::ConnectionState::Offline {
            reason: tcode_client::ConnectionFailure::AuthenticationRejected,
        };
        host.states.send_blocking(rejected.clone()).unwrap();
        cx.run_until_parked();
        assert_eq!(
            store_of(&shell, cx).read_with(cx, |store, _| store.connection_state()),
            rejected
        );
        let saved = client.saved.clone();
        client.transports.borrow_mut().push(fresh_transport());
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.switch_to(
                    AttachmentTarget::Remote(Box::new(saved.clone())),
                    window,
                    cx,
                )
            });
        });
        cx.run_until_parked();
        assert!(
            client.transports.borrow().is_empty(),
            "the refused link is replaced"
        );
        assert_ne!(
            store_of(&shell, cx).read_with(cx, |store, _| store.connection_state()),
            rejected
        );

        // The working link to the same machine is kept...
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.switch_to(
                    AttachmentTarget::Remote(Box::new(saved.clone())),
                    window,
                    cx,
                )
            });
        });
        cx.run_until_parked();

        // ...and joining one of its spaces is a different link.
        client.transports.borrow_mut().push(fresh_transport());
        let member = tcode_client::pairing::PairedHost {
            space_id: Some("space-1".into()),
            space_name: Some("Shared".into()),
            ..saved
        };
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.switch_to(AttachmentTarget::Remote(Box::new(member)), window, cx)
            });
        });
        cx.run_until_parked();
        assert!(
            client.transports.borrow().is_empty(),
            "a space membership replaces the full link"
        );
    }

    #[gpui::test]
    fn pending_outbox_overrides_desktop_launch_without_a_snapshot(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        struct SavedOutbox;
        impl tcode_client::outbox::Storage for SavedOutbox {
            fn load(
                &self,
            ) -> Result<Vec<tcode_client::outbox::Entry>, tcode_protocol::ProtocolError>
            {
                Ok(vec![tcode_client::outbox::Entry {
                    key: "saved-send".into(),
                    command: Command::SendTurn {
                        session_id: "pending-thread".into(),
                        text: "survives relaunch".into(),
                        attachment_paths: Vec::new(),
                    },
                }])
            }
            fn save(
                &self,
                _: &[tcode_client::outbox::Entry],
            ) -> Result<(), tcode_protocol::ProtocolError> {
                Ok(())
            }
        }
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let (_state_tx, state) = async_channel::unbounded();
        let client = Rc::new(ReturningClient {
            saved: tcode_client::pairing::PairedHost {
                host_id: "pending-host".into(),
                name: "Pending host".into(),
                traverse: Vec::new(),
                relay: None,
                addrs: vec!["127.0.0.1:48442".into()],
                last_connected_unix: None,
                group_id: None,
                space_id: None,
                space_name: None,
            },
            transports: RefCell::new(vec![Transport {
                originals: None,
                to_host: to_host.into(),
                from_host,
                state,
                current_host: None,
            }]),
            preferences: RefCell::new(Default::default()),
            machine_exists: true,
            saves: Cell::new(0),
            outbox: Some(std::sync::Arc::new(SavedOutbox)),
        });
        let (shell, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|_| WindowState::new(false));
            AppShell::new(
                state,
                ShellSetup {
                    initial: Some(AttachmentTarget::Local),
                    client_host: Some(client),
                    restore_navigation: false,
                    ..Default::default()
                },
                window,
                cx,
            )
        });
        cx.simulate_resize(size(px(1024.), px(700.)));
        draw(cx);
        let store = store_of(&shell, cx);
        store.read_with(cx, |store, cx| {
            assert_eq!(store.remote_host_name(), Some("Pending host"));
            assert!(store.is_thread_open("pending-thread"));
            assert!(!store.chat_loading_for(Some("pending-thread"), cx));
            assert_eq!(
                store.delivery_messages_for(Some("pending-thread"), cx)[0].1,
                "survives relaunch"
            );
            assert!(store.sidebar_sessions().is_empty());
        });
        assert!(cx.debug_bounds("pending-delivery-bubble").is_some());
        assert!(cx.debug_bounds("pending-thread-row").is_some());
        as_mobile(cx);
        cx.simulate_resize(size(px(393.), px(852.)));
        let navigation = shell.read_with(cx, |shell, _| shell.window_state());
        navigation.update(cx, |state, cx| state.enter_workspace(cx));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.compact(cx));
            assert_eq!(shell.destination(cx), Destination::Threads);
        });
        cx.update(|window, cx| {
            crate::settings_page::apply_theme(tcode_core::settings::ThemeMode::Dark, window, cx)
        });
        draw(cx);
        assert!(
            cx.debug_bounds("pending-thread-row").is_some(),
            "compact Threads exposes the outbox without a cached index"
        );
        navigation.update(cx, |state, cx| state.go(Destination::Thread, cx));
        draw(cx);
        assert!(cx.debug_bounds("pending-delivery-bubble").is_some());
    }

    /// A message on its way to one thread shows in the pane showing that
    /// thread, whichever pane has the keyboard, and in no other.
    #[gpui::test]
    fn a_pending_delivery_shows_only_in_its_threads_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        cx.simulate_keystrokes("secondary-d");
        serve_thread(&host, "thread-2");
        cx.simulate_keystrokes("secondary-2");
        await_restore_update(&shell, cx, |store, cx| {
            store.is_thread_open("thread-2") && !store.chat_loading_for(Some("thread-2"), cx)
        });
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-2".into())]
        );
        store_of(&shell, cx).update(cx, |store, _| {
            store.send_turn_for("thread-2".into(), "On its way".into(), Vec::new())
        });
        let pane_of = |cx: &mut VisualTestContext| {
            let bubble = cx
                .debug_bounds("pending-delivery-bubble")
                .expect("the message on its way");
            shell.read_with(cx, |shell, cx| {
                shell
                    .attachment
                    .as_ref()
                    .expect("attached")
                    .panes(cx)
                    .iter()
                    .find(|pane| pane.read(cx).bounds().contains(&bubble.center()))
                    .and_then(|pane| pane.read(cx).session_id().map(str::to_owned))
            })
        };
        for (key, focused) in [("secondary-2", "thread-2"), ("secondary-1", "thread-1")] {
            cx.simulate_keystrokes(key);
            await_restore_update(&shell, cx, |store, _| store.is_thread_open(focused));
            crate::test_frames::draw_settled(cx);
            assert_eq!(
                pane_of(cx).as_deref(),
                Some("thread-2"),
                "with {focused} focused"
            );
        }
    }

    fn await_restore_update(
        shell: &Entity<AppShell>,
        cx: &mut VisualTestContext,
        ready: impl Fn(&WorkspaceStore, &App) -> bool,
    ) {
        let store = store_of(shell, cx);
        for _ in 0..500 {
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
            if store.read_with(cx, |store, cx| ready(store, cx)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("restore host update never arrived");
    }

    fn restore_index(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        present: bool,
        cx: &mut VisualTestContext,
    ) {
        while let Ok(line) = host.outgoing.try_recv() {
            host.observed
                .borrow_mut()
                .push(decode_client_line(&line).unwrap());
        }
        let scope_request = host
            .observed
            .borrow()
            .iter()
            .rev()
            .find_map(|message| {
                matches!(
                    message.payload,
                    ClientPayload::Subscribe(tcode_protocol::Subscription {
                        topic: Topic::Scope,
                        ..
                    })
                )
                .then_some(message.id)
            })
            .expect("Scope subscription");
        crate::store::tests::seed_full_scope_reply(
            &store_of(shell, cx),
            &host.incoming,
            scope_request,
            Vec::new(),
            cx,
        );
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            "/project".into(),
            None,
        );
        meta.id = "thread-a".into();
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        sessions: if present { vec![meta] } else { Vec::new() },
                        projects: Vec::new(),
                    }),
                }))
                .unwrap(),
            )
            .unwrap();
        await_restore_update(shell, cx, |store, _| store.index_hydrated());
    }

    fn restore_status(shell: &Entity<AppShell>, host: &MountedShell, cx: &mut VisualTestContext) {
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: "thread-a".into(),
                    },
                    event: ServerEvent::SessionStatusReplaced(Box::new(session_status(
                        "thread-a",
                        std::path::Path::new("/project"),
                    ))),
                }))
                .unwrap(),
            )
            .unwrap();
        await_restore_update(shell, cx, |store, cx| {
            store
                .chat_active_session_for(Some("thread-a"), cx)
                .is_some()
        });
    }

    /// The rest of the baseline a restored thread waits for, after
    /// [`restore_index`] and [`restore_status`].
    fn restore_settings_and_events(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        cx: &mut VisualTestContext,
    ) {
        // The host's language is applied with its settings; the assertions
        // read English.
        let settings = tcode_core::settings::Settings {
            language: Some(crate::LANGUAGE_ENGLISH.into()),
            ..Default::default()
        };
        for (topic, event) in [
            (Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
            (
                Topic::SessionPlan {
                    session_id: "thread-a".into(),
                },
                ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                    session_id: "thread-a".into(),
                    steps: Vec::new(),
                    explanation: None,
                }),
            ),
            (
                Topic::SessionEvents {
                    session_id: "thread-a".into(),
                },
                ServerEvent::SessionSnapshot {
                    from: 0,
                    end: 0,
                    records: Vec::new(),
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                },
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        await_restore_update(shell, cx, |store, _| store.baseline_ready());
    }

    /// A phone that goes back to the thread list before the thread's
    /// conversation arrives keeps the thread selected, so the conversation
    /// still loads behind the list. That is not a read; showing the thread
    /// page again is.
    #[gpui::test]
    fn a_conversation_that_loads_behind_the_thread_list_is_not_read(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads", "thread"], true);
        let reads = |host: &MountedShell| {
            sent(host)
                .into_iter()
                .filter_map(|payload| match payload {
                    ClientPayload::Command(Command::MarkSessionRead { session_id, .. }) => {
                        Some(session_id)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        host.states
            .try_send(tcode_client::ConnectionState::Syncing { path: None })
            .unwrap();
        draw(cx);
        assert!(cx.debug_bounds("compact-thread-page").is_some());
        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);

        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        restore_settings_and_events(&shell, &host, cx);
        assert_eq!(reads(&host), Vec::<String>::new());

        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        assert_eq!(reads(&host), ["thread-a"]);
    }

    /// The phone's direct path dying is reported by the transport as a new
    /// value of the state it is in naming the relay; that alone, with no
    /// host line and no touch, changes what the nav bar says about the
    /// link. A path known while the baseline still loads is already shown
    /// as the connected path: syncing is what the skeletons say, not the
    /// subtitle.
    #[gpui::test]
    fn a_path_change_alone_updates_the_connection_subtitle(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads"], true);
        let store = store_of(&shell, cx);
        let subtitle = |cx: &mut VisualTestContext| {
            draw(cx);
            store.read_with(cx, |store, _| {
                crate::remote::connection_label(&store.connection_state())
            })
        };
        let lan = tcode_protocol::PathInfo {
            direct: true,
            relay: None,
            lan: true,
            probing_direct: false,
        };
        assert_eq!(subtitle(cx), "Connected", "the baseline is not in yet");
        host.states
            .try_send(tcode_client::ConnectionState::Syncing {
                path: Some(lan.clone()),
            })
            .unwrap();
        assert_eq!(subtitle(cx), "LAN · Connected");
        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        restore_settings_and_events(&shell, &host, cx);
        assert_eq!(subtitle(cx), "LAN · Connected", "the baseline is in");
        host.states
            .try_send(tcode_client::ConnectionState::Connected { path: Some(lan) })
            .unwrap();
        assert_eq!(subtitle(cx), "LAN · Connected");
        host.states
            .try_send(tcode_client::ConnectionState::Connected {
                path: Some(tcode_protocol::PathInfo {
                    direct: false,
                    relay: Some("https://aps1-1.relay.n0.iroh.link/".into()),
                    lan: false,
                    probing_direct: false,
                }),
            })
            .unwrap();
        assert_eq!(subtitle(cx), "Relay (aps1-1) · Connected");
        assert!(
            cx.debug_bounds("compact-threads-page").is_some(),
            "the Threads page is what shows it"
        );
    }

    #[gpui::test]
    fn cold_start_restores_thread_and_selects_it_once(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        for width in [393., 1024.] {
            let (shell, host, _, cx) =
                mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, width, "plan");
            assert!(
                cx.debug_bounds("baseline-loading").is_some(),
                "the restored destination shows a skeleton before its Index baseline"
            );
            shell.read_with(cx, |shell, cx| {
                assert_eq!(
                    shell.window_state.read(cx).history(),
                    [
                        Destination::Hosts,
                        Destination::Threads,
                        Destination::Thread
                    ]
                );
                assert_eq!(shell.page_thread(cx).as_deref(), Some("thread-a"));
            });
            restore_index(&shell, &host, true, cx);
            restore_index(&shell, &host, true, cx);
            restore_status(&shell, &host, cx);
            draw(cx);
            cx.update(|window, cx| {
                let composer = shell
                    .read(cx)
                    .attachment
                    .as_ref()
                    .unwrap()
                    .chat(cx)
                    .read(cx)
                    .composer();
                assert!(!composer.read(cx).input_focus_handle(cx).is_focused(window));
            });
            let subscriptions = sent(&host).into_iter().filter(|payload| matches!(payload,
                ClientPayload::Subscribe(subscription) if subscription.topic == Topic::SessionEvents { session_id: "thread-a".into() }
            )).count();
            assert_eq!(
                subscriptions, 1,
                "baseline reconciliation must not select again"
            );
            for (topic, event) in [
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(Default::default()),
                ),
                (
                    Topic::SessionEvents {
                        session_id: "thread-a".into(),
                    },
                    ServerEvent::SessionSnapshot {
                        from: 1800,
                        end: 2000,
                        records: (0..200)
                            .map(|_| {
                                agent::AgentEvent::Warning {
                                    message: "short".into(),
                                }
                                .into()
                            })
                            .collect(),
                        total: 2000,
                        total_turns: 1,
                        truncated: false,
                    },
                ),
            ] {
                host.incoming
                    .try_send(
                        encode_line(&HostMessage::Event(EventEnvelope {
                            request_id: None,
                            topic,
                            event,
                        }))
                        .unwrap(),
                    )
                    .unwrap();
            }
            await_restore_update(&shell, cx, |store, cx| {
                !store.chat_loading_for(Some("thread-a"), cx)
            });
            draw(cx);
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
            assert!(
                sent(&host).into_iter().any(|payload| matches!(
                    payload,
                    ClientPayload::Query(tcode_protocol::Query::SessionHistoryPage {
                        before: 1800,
                        limit: 200,
                        ..
                    })
                )),
                "restored short history must prefetch before any scroll"
            );
            shell.read_with(cx, |shell, cx| {
                assert_eq!(
                    shell.window_state.read(cx).history().last(),
                    Some(&Destination::Thread)
                )
            });
            let focus = shell.read_with(cx, |shell, cx| {
                shell
                    .attachment
                    .as_ref()
                    .unwrap()
                    .chat(cx)
                    .read(cx)
                    .composer()
                    .read(cx)
                    .input_focus_handle(cx)
            });
            for rotated_width in [width, 393., 1024.] {
                resize(cx, rotated_width);
                cx.update(|window, _| assert!(!focus.is_focused(window), "cold restore at {width}px must not open the keyboard after hydration or rotation"));
            }
        }
    }

    #[gpui::test]
    fn cold_start_discards_transient_settings_and_pair_pages(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for (saved, expected) in [
            (
                vec!["hosts", "threads", "thread", "settings", "settings_section"],
                vec![
                    Destination::Hosts,
                    Destination::Threads,
                    Destination::Thread,
                ],
            ),
            (
                vec!["hosts", "threads", "hosts", "pair"],
                vec![Destination::Hosts, Destination::Threads, Destination::Hosts],
            ),
        ] {
            let (shell, _, _, cx) = mount_restored(cx, &saved, true);
            shell.read_with(cx, |shell, cx| {
                assert_eq!(shell.window_state.read(cx).history(), expected);
            });
        }
    }

    #[gpui::test]
    fn cold_start_discards_a_missing_session_after_index_in_either_delivery_order(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for status_first in [false, true] {
            let (shell, host, client, cx) =
                mount_restored(cx, &["hosts", "threads", "thread", "panel"], true);
            assert!(
                cx.debug_bounds("baseline-loading").is_some(),
                "the restored Panel also waits behind a skeleton"
            );
            shell.read_with(cx, |shell, cx| {
                assert_eq!(
                    shell.window_state.read(cx).history().last(),
                    Some(&Destination::Panel)
                )
            });
            if status_first {
                restore_status(&shell, &host, cx);
            }
            restore_index(&shell, &host, false, cx);
            shell.read_with(cx, |shell, cx| {
                assert_eq!(
                    shell.window_state.read(cx).history(),
                    [Destination::Hosts, Destination::Threads]
                );
                assert_eq!(shell.page_thread(cx), None);
            });
            cx.executor().advance_clock(Duration::from_millis(151));
            draw(cx);
            let preferences = client.load_preferences();
            assert_eq!(
                preferences.navigation.unwrap()["history"],
                serde_json::json!(["hosts", "threads"])
            );
        }
    }

    #[gpui::test]
    fn cold_start_restores_the_selected_panel_after_session_status(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        for (saved_panel, expected_tab) in [
            ("plan", RightTab::Plan),
            ("agents", RightTab::Agents),
            ("pull_requests", RightTab::PullRequests),
        ] {
            let (shell, host, client, cx) = mount_restored_at_width(
                cx,
                &["hosts", "threads", "thread", "panel"],
                true,
                393.,
                saved_panel,
            );
            restore_index(&shell, &host, true, cx);
            restore_status(&shell, &host, cx);
            assert!(
                cx.debug_bounds("baseline-loading").is_some(),
                "status alone is not the conversation baseline"
            );
            let settings = tcode_core::settings::Settings {
                language: Some(crate::LANGUAGE_ENGLISH.into()),
                ..Default::default()
            };
            for (topic, event) in [
                (Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
                (
                    Topic::SessionEvents {
                        session_id: "thread-a".into(),
                    },
                    ServerEvent::SessionSnapshot {
                        from: 0,
                        end: 0,
                        records: Vec::new(),
                        total: 0,
                        total_turns: 0,
                        truncated: false,
                    },
                ),
            ] {
                host.incoming
                    .try_send(
                        encode_line(&HostMessage::Event(EventEnvelope {
                            request_id: None,
                            topic,
                            event,
                        }))
                        .unwrap(),
                    )
                    .unwrap();
            }
            await_restore_update(&shell, cx, |store, cx| {
                !store.chat_loading_for(Some("thread-a"), cx)
            });
            shell.read_with(cx, |shell, cx| {
                assert!(shell.pending_navigation_restore.is_none());
                assert_eq!(
                    shell.window_state.read(cx).history().last(),
                    Some(&Destination::Panel)
                );
                assert_eq!(
                    shell
                        .store()
                        .unwrap()
                        .read(cx)
                        .panel_state_for(Some("thread-a"))
                        .right_tab,
                    expected_tab
                );
            });
            // A preference changed during the debounce must survive the checkpoint.
            client.preferences.borrow_mut().language = Some("zh-CN".into());
            cx.executor().advance_clock(Duration::from_millis(151));
            draw(cx);
            assert_eq!(client.load_preferences().language.as_deref(), Some("zh-CN"));
            assert_eq!(
                client.load_preferences().navigation.unwrap()["panel"],
                saved_panel
            );
        }
    }

    #[gpui::test]
    fn navigation_checkpoint_debounces_selection_and_history_without_losing_preferences(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _, client, cx) = mount_restored(cx, &["hosts", "threads", "thread"], true);
        let store = store_of(&shell, cx);
        // Showing a thread on the existing page must checkpoint without
        // waiting for a host response or a changed history.
        store.update(cx, |store, cx| store.show_thread("thread-b".into(), cx));
        draw(cx);
        assert_eq!(client.saves.get(), 0);
        cx.executor().advance_clock(Duration::from_millis(151));
        draw(cx);
        assert_eq!(client.saves.get(), 1);
        let same_page = client.load_preferences().navigation.unwrap();
        assert_eq!(same_page["session_id"], "thread-b");
        assert_eq!(
            same_page["history"],
            serde_json::json!(["hosts", "threads", "thread"])
        );

        store.update(cx, |store, cx| {
            store.show_thread("thread-c".into(), cx);
        });
        shell
            .read_with(cx, |shell, _| shell.window_state())
            .update(cx, |state, cx| state.go(Destination::Settings, cx));
        draw(cx);
        assert_eq!(client.saves.get(), 1);
        cx.executor().advance_clock(Duration::from_millis(151));
        draw(cx);
        assert_eq!(client.saves.get(), 2);
        let navigation = client.load_preferences().navigation.unwrap();
        assert_eq!(navigation["session_id"], "thread-c");
        assert_eq!(
            navigation["history"],
            serde_json::json!(["hosts", "threads", "thread", "settings"])
        );
        // The store cached preferences before this checkpoint. A theme edit
        // must retain the shell's newer history and selection.
        store.update(cx, |store, _| {
            store.set_client_theme(Some(tcode_core::settings::ThemeMode::Light))
        });
        assert_eq!(client.load_preferences().navigation, Some(navigation));
        assert_eq!(
            client.load_preferences().appearance.as_deref(),
            Some("light")
        );
    }

    #[gpui::test]
    fn cold_start_with_a_missing_machine_restores_hosts(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads", "thread"], false);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.window_state.read(cx).history(), [Destination::Hosts]);
            assert!(shell.store().is_none());
        });
        assert!(sent(&host).is_empty());
    }

    /// A tablet in landscape with nothing to attach to is the hosts page and
    /// nothing else: the wide layout has no local host and no sidebar to
    /// switch routes with, so the page is the whole window until the window
    /// attaches somewhere, Pair returns to it, and disconnecting comes back
    /// to it.
    #[gpui::test]
    fn a_wide_mobile_window_stays_on_hosts_until_it_attaches(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (shell, _host, client, cx) =
            mount_restored_at_width(cx, &["hosts"], false, 1024., "plan");
        let assert_locked_on_hosts = |cx: &mut VisualTestContext| {
            crate::test_frames::draw_settled(cx);
            assert!(
                cx.debug_bounds("hosts-page").is_some(),
                "the hosts page is the window"
            );
            assert!(
                cx.debug_bounds("hosts-route").is_none(),
                "no workspace column"
            );
            assert!(
                cx.debug_bounds("sidebar-feature-hosts").is_none(),
                "no sidebar"
            );
            assert!(
                cx.debug_bounds("compact-back").is_none(),
                "nothing to go back to"
            );
        };
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.compact(cx), "1024px on a mobile build is wide");
            assert!(shell.store().is_none());
            assert_eq!(shell.window_state.read(cx).history(), [Destination::Hosts]);
        });
        assert_locked_on_hosts(cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        // Nothing leads away: Back is the root, and a stale destination
        // still shows the hosts page.
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| assert!(!shell.back(window, cx)));
        });
        assert_locked_on_hosts(cx);
        state.update(cx, |state, cx| state.go(Destination::Threads, cx));
        draw(cx);
        assert!(cx.debug_bounds("hosts-page").is_some());
        assert!(cx.debug_bounds("compact-threads-page").is_none());
        state.update(cx, |state, cx| state.leave_workspace(cx));
        // Pair is the one page over Hosts, and its Back returns to Hosts.
        state.update(cx, |state, cx| state.go(Destination::Pair, cx));
        draw(cx);
        let back = cx
            .debug_bounds("compact-back")
            .expect("Pair returns to Machines");
        cx.simulate_click(back.center(), gpui::Modifiers::default());
        assert_locked_on_hosts(cx);

        // Attaching leaves the page for the new host's workspace...
        let saved = client.saved.clone();
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.switch_to(AttachmentTarget::Remote(Box::new(saved)), window, cx)
            });
        });
        crate::test_frames::draw_settled(cx);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.store().is_some());
            assert_eq!(shell.window_state.read(cx).route(), Route::Chat);
        });
        assert!(cx.debug_bounds("hosts-page").is_none());
        assert!(
            cx.debug_bounds("sidebar-feature-hosts").is_some(),
            "the wide workspace"
        );

        // ...and disconnecting returns to it.
        shell.update(cx, |shell, cx| shell.detach(cx));
        shell.read_with(cx, |shell, cx| {
            assert!(shell.store().is_none());
            assert_eq!(shell.window_state.read(cx).history(), [Destination::Hosts]);
        });
        assert_locked_on_hosts(cx);
    }

    /// Navigation never raises a software keyboard: on a phone every route
    /// leaves the composer unfocused, and a desktop window opening a thread
    /// focuses it.
    #[gpui::test]
    fn navigation_dismisses_mobile_keyboards_but_preserves_explicit_editing_focus(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        let store = store_of(&shell, cx);
        let composer = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .unwrap()
                .chat(cx)
                .read(cx)
                .composer()
        });
        let focus = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
        for width in [1024., 393., 1024.] {
            resize(cx, width);
            for id in ["thread-1", "thread-2"] {
                cx.update(|window, cx| {
                    if id == "thread-2" {
                        focus.focus(window, cx);
                    }
                    store.update(cx, |store, cx| store.show_thread(id.into(), cx));
                });
                draw(cx);
                cx.update(|window, _| assert!(!focus.is_focused(window)));
            }
            if shell.read_with(cx, |shell, cx| shell.compact(cx)) {
                cx.update(|window, cx| {
                    focus.focus(window, cx);
                    shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
                });
            }
            draw(cx);
            cx.update(|window, cx| {
                assert!(!focus.is_focused(window));
                store.update(cx, |store, cx| store.show_thread("thread-2".into(), cx));
            });
            draw(cx);
            cx.update(|window, _| assert!(!focus.is_focused(window)));
            for destination in [
                Destination::Panel,
                Destination::Hosts,
                Destination::Settings,
            ] {
                cx.update(|window, cx| {
                    focus.focus(window, cx);
                    shell.update(cx, |shell, cx| shell.go(destination, cx));
                });
                draw(cx);
                cx.update(|window, cx| {
                    assert!(!focus.is_focused(window));
                    store.update(cx, |store, cx| store.show_thread("thread-2".into(), cx));
                });
                draw(cx);
                cx.update(|window, _| assert!(!focus.is_focused(window)));
            }
        }
        cx.update(|window, cx| focus.focus(window, cx));
        for width in [393., 1024., 393.] {
            resize(cx, width);
            draw(cx);
            cx.update(|window, _| assert!(focus.is_focused(window)));
        }
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        resize(cx, 1024.);
        send_events(
            &host,
            vec![
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(Default::default()),
                ),
                panes_index(&[]),
            ],
        );
        serve_thread(&host, "thread-2");
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.close_settings(cx));
        store.update(cx, |store, cx| store.show_thread("thread-2".into(), cx));
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        settle_panes(cx);
        assert!(composer_has_keyboard(&shell, 0, cx));
    }

    #[gpui::test]
    fn background_provider_notices_preserve_composer_editing(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("background-notification-focus");
        let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
        for id in ["thread-a", "thread-b"] {
            let mut meta = tcode_core::project::SessionMeta::new(
                agent::ProviderKind::Codex,
                root.clone(),
                None,
            );
            meta.id = id.into();
            disk.upsert_meta(&meta).unwrap();
        }
        let host =
            tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
                .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let (shell, _transport, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            *store = WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            );
            store.show_thread("thread-a".into(), cx);
        });
        driver.wait_until(cx, &store, "selected conversation", |cx| {
            store.read_with(cx, |store, cx| {
                store.baseline_ready()
                    && !store.threads_loading()
                    && !store.chat_loading_for(Some("thread-a"), cx)
            })
        });
        draw(cx);
        let composer = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .unwrap()
                .chat(cx)
                .read(cx)
                .composer()
        });
        let focus = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
        let card = cx.debug_bounds("composer-card").unwrap();
        cx.simulate_click(card.center(), gpui::Modifiers::default());
        cx.simulate_keystrokes("draft");
        cx.simulate_keystrokes("left");
        draw(cx);
        cx.update(|window, _| assert!(focus.is_focused(window)));
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "draft"
        );

        for event in [
            agent::AgentEvent::TurnRetry {
                retry: Some(agent::TurnRetry {
                    turn_id: "background-turn".into(),
                    retry_at: Some(u64::MAX),
                }),
            },
            agent::AgentEvent::Warning {
                message: "Background status changed".into(),
            },
            agent::AgentEvent::Error {
                message: "Reconnecting…".into(),
                fatal: false,
            },
        ] {
            driver
                .await_with_events(
                    cx,
                    &store,
                    host.update_state_for_test(move |state, cx| {
                        state.provider_event_for_test("thread-b", event, cx);
                    }),
                )
                .unwrap();
            driver.step(cx, &store);
            draw(cx);
            cx.update(|window, _| {
                assert!(
                    focus.is_focused(window),
                    "background provider event stole composer focus"
                );
            });
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "draft"
            );
        }
        for width in [1024., 393.] {
            resize(cx, width);
            cx.update(|window, _| {
                assert!(focus.is_focused(window), "rotation keeps editing focus")
            });
        }
        let toast = cx
            .debug_bounds("compact-toast")
            .expect("the notice remains visible");
        cx.simulate_click(toast.center(), gpui::Modifiers::default());
        draw(cx);
        cx.update(|window, _| {
            assert!(
                !focus.is_focused(window),
                "tapping the error opens its details"
            )
        });
        cx.update(|window, cx| window.close_dialog(cx));
        draw(cx);
        cx.update(|window, _| {
            assert!(
                focus.is_focused(window),
                "closing details returns to editing"
            )
        });
        cx.simulate_keystrokes("x");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "drafxt"
        );
        driver.shutdown(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn keyboard_cover_changes_keep_editor_focus_until_explicit_dismissal(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (shell, host, _, cx) =
            mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 393., "plan");
        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        for (topic, event) in [
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::SessionEvents {
                    session_id: "thread-a".into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 0,
                    records: vec![],
                },
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-a"), cx)
        });
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        let composer = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .unwrap()
                .chat(cx)
                .read(cx)
                .composer()
        });
        let focus = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
        cx.update(|window, cx| focus.focus(window, cx));
        draw(cx);
        crate::window_seam::occlude_for_test(
            cx,
            gpui::Edges {
                bottom: px(400.),
                ..Default::default()
            },
        );
        draw(cx);
        crate::window_seam::occlude_for_test(cx, gpui::Edges::default());
        draw(cx);
        cx.update(|window, _| {
            assert!(
                focus.is_focused(window),
                "hardware input hides the software keyboard without ending editing"
            )
        });
        cx.simulate_input("keyboard draft");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "keyboard draft"
        );
        let card = cx
            .debug_bounds("composer-card")
            .expect("composer on the thread page");
        cx.simulate_click(
            gpui::point(card.center().x, card.top() - px(40.)),
            gpui::Modifiers::default(),
        );
        draw(cx);
        cx.update(|window, _| {
            assert!(
                !focus.is_focused(window),
                "tap blank timeline dismisses editing"
            )
        });
        cx.simulate_click(card.center(), gpui::Modifiers::default());
        draw(cx);
        cx.update(|window, _| assert!(focus.is_focused(window), "tap composer resumes editing"));
    }

    fn composer_of(
        shell: &Entity<AppShell>,
        cx: &VisualTestContext,
    ) -> Entity<crate::composer::Composer> {
        shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .unwrap()
                .chat(cx)
                .read(cx)
                .composer()
        })
    }

    /// Select `session` with the host describing it: a native subagent's
    /// thread, which the host makes read-only and which is running, or an
    /// ordinary one.
    fn deliver_thread(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        session: &str,
        subagent: bool,
        cx: &mut VisualTestContext,
    ) {
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            "/project".into(),
            Some("child-model".into()),
        );
        meta.id = session.into();
        meta.native_subagent = subagent.then(|| "spawn-1".into());
        let mut status = session_status(session, std::path::Path::new("/project"));
        status.requested_model = meta.model.clone();
        status.activity.turn_running = subagent;
        status.conversation_read_only = subagent;
        status.provider_option_selections = vec![agent::OptionSelection {
            id: "reasoningEffort".into(),
            value: serde_json::json!("high"),
        }];
        store_of(shell, cx).update(cx, |store, cx| store.show_thread(session.into(), cx));
        for (topic, event) in [
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (Topic::Index, ServerEvent::IndexUpsertSession(meta)),
            (
                Topic::SessionStatus {
                    session_id: session.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            ),
            (
                Topic::SessionPlan {
                    session_id: session.into(),
                },
                ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                    session_id: session.into(),
                    steps: Vec::new(),
                    explanation: None,
                }),
            ),
            (
                Topic::SessionEvents {
                    session_id: session.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 0,
                    records: vec![],
                },
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        // The app applies what the host sent as it arrives; a thread come
        // back to draws from what was kept until its window lands.
        await_restore_update(shell, cx, |store, cx| {
            !store.chat_loading_for(Some(session), cx) && store.timeline_settled_for(session)
        });
    }

    /// A tablet that rotates through phone widths and back: a subagent's
    /// thread draws its model, effort, context and permission in one row,
    /// with no card, field, send or settings drawer, so its timeline reaches
    /// lower than the lead's; nothing pressed or typed there sends, and the
    /// lead's draft is intact when its thread is reopened.
    #[gpui::test]
    fn a_subagent_thread_composer_is_its_settings_read_only(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (shell, host, _, cx) =
            mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 1200., "plan");
        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        let composer = composer_of(&shell, cx);
        let show = |cx: &mut VisualTestContext| {
            shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
        };
        deliver_thread(&shell, &host, "thread-a", false, cx);
        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| {
                composer.set_draft("parent draft", window, cx);
            })
        });
        draw(cx);
        for width in [1200., 760., 393.] {
            deliver_thread(&shell, &host, "thread-a", false, cx);
            resize(cx, width);
            show(cx);
            assert!(cx.debug_bounds("composer-card").is_some());
            let lead = cx
                .debug_bounds("chat-composer")
                .expect("the lead's composer");

            deliver_thread(&shell, &host, "child", true, cx);
            show(cx);
            sent(&host);
            for absent in [
                "composer-card",
                "composer-settings-drawer",
                "send-message",
                "stop-turn",
                "steer-turn",
                "attach-menu",
                "overflow-controls",
            ] {
                assert!(
                    cx.debug_bounds(absent).is_none(),
                    "{absent} is drawn at {width}"
                );
            }
            let row = cx
                .debug_bounds("composer-subagent")
                .expect("the subagent's settings");
            for selector in [
                "model-picker",
                "traits-chip",
                "context-meter",
                "permission-chip",
            ] {
                let bounds = cx.debug_bounds(selector).expect(selector);
                assert!(
                    bounds.size.width > px(0.) && row.contains(&bounds.center()),
                    "{selector} is not in the row at {width}: {bounds:?} in {row:?}"
                );
                cx.simulate_click(bounds.center(), gpui::Modifiers::default());
                draw(cx);
                for pane in ["permission-pane", "context-pane"] {
                    assert!(
                        cx.debug_bounds(pane).is_none(),
                        "{selector} opened {pane} at {width}"
                    );
                }
            }
            let child = cx.debug_bounds("chat-composer").unwrap();
            assert_eq!(child.bottom(), lead.bottom());
            assert!(
                child.top() > lead.top(),
                "the timeline does not take the field's height at {width}: {child:?} against {lead:?}"
            );
            cx.simulate_click(row.center(), gpui::Modifiers::default());
            cx.simulate_keystrokes("x shift-tab enter");
            draw(cx);
            assert!(
                sent(&host)
                    .iter()
                    .all(|payload| !matches!(payload, ClientPayload::Command(_))),
                "a subagent's composer must not send commands"
            );
        }
        deliver_thread(&shell, &host, "thread-a", false, cx);
        resize(cx, 1200.);
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "parent draft"
        );
        // A software keyboard waits for an explicit tap on the composer.
        let card = cx.debug_bounds("composer-card").expect("parent composer");
        cx.simulate_click(
            gpui::point(card.center().x, card.top() + px(24.)),
            gpui::Modifiers::default(),
        );
        draw(cx);
        cx.simulate_keystrokes("end x");
        draw(cx);
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "parent draftx"
        );
    }

    /// A desktop pane whose field has the keyboard turning to a subagent's
    /// thread: the keyboard stays in the pane, on the settings row, where a
    /// key types nothing and sends nothing; back on the lead, the field has
    /// it again and types.
    #[gpui::test]
    fn a_subagent_thread_keeps_the_keyboard_in_its_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let (shell, host, _, cx) =
            mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 1200., "plan");
        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        let composer = composer_of(&shell, cx);
        for width in [1200., 600.] {
            deliver_thread(&shell, &host, "thread-a", false, cx);
            resize(cx, width);
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
            let field = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
            let card = cx.debug_bounds("composer-card").expect("the lead's card");
            cx.simulate_click(
                gpui::point(card.center().x, card.top() + px(16.)),
                gpui::Modifiers::default(),
            );
            draw(cx);
            cx.update(|window, _| assert!(field.is_focused(window), "the lead's field types"));

            deliver_thread(&shell, &host, "child", true, cx);
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
            sent(&host);
            cx.update(|window, cx| {
                let composer = composer.read(cx);
                assert!(!composer.input_focus_handle(cx).is_focused(window));
                assert!(composer.has_keyboard(window, cx));
            });
            assert_eq!(
                keyboard_pane(&shell, cx),
                Some(0),
                "the keyboard left the pane at {width}"
            );
            cx.simulate_keystrokes("x y enter");
            draw(cx);
            assert!(
                sent(&host)
                    .iter()
                    .all(|payload| !matches!(payload, ClientPayload::Command(_))),
                "a key on a subagent's thread sent a command at {width}"
            );

            deliver_thread(&shell, &host, "thread-a", false, cx);
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
            cx.update(|window, _| assert!(field.is_focused(window), "the field has it back"));
            cx.simulate_keystrokes("z");
            draw(cx);
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "z".repeat(if width == 1200. { 1 } else { 2 })
            );
        }
    }

    #[gpui::test]
    fn software_keyboard_restored_terminal_does_not_raise_keyboard(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        let (shell, host, _, cx) = mount_restored_at_width(
            cx,
            &["hosts", "threads", "thread", "panel"],
            true,
            1024.,
            "terminal",
        );
        restore_index(&shell, &host, true, cx);
        let mut status = session_status("thread-a", std::path::Path::new("/project"));
        status.terminal_open = true;
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: "thread-a".into(),
                    },
                    event: ServerEvent::SessionStatusReplaced(Box::new(status)),
                }))
                .unwrap(),
            )
            .unwrap();
        await_restore_update(&shell, cx, |store, cx| {
            store
                .chat_active_session_for(Some("thread-a"), cx)
                .is_some()
        });
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::SettingsSnapshot(Default::default()),
                }))
                .unwrap(),
            )
            .unwrap();
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "thread-a".into(),
                    },
                    event: ServerEvent::SessionSnapshot {
                        total: 0,
                        total_turns: 0,
                        truncated: false,
                        from: 0,
                        end: 0,
                        records: vec![],
                    },
                }))
                .unwrap(),
            )
            .unwrap();
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-a"), cx)
        });
        assert!(store_of(&shell, cx).read_with(cx, |store, _| {
            store.panel_state_for(Some("thread-a")).terminal_open
        }));
        cx.executor().advance_clock(Duration::from_secs(1));
        draw(cx);
        cx.update(|window, cx| {
            assert!(
                window.focused(cx).is_none(),
                "a restored terminal must not steal focus after navigation has blurred"
            )
        });
    }

    #[gpui::test]
    fn timeline_selection_copies_without_focusing_the_composer(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        cx.update(|window, _| window.activate_window());
        let session = "thread-1";
        let cwd = std::path::Path::new("/tmp/tcode-selection");
        let records: Vec<agent::AgentEvent> = vec![
            agent::AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: "answer".into(),
                parent_item_id: None,
                content: agent::ItemContent::AssistantMessage {
                    text: "Selectable answer alpha beta gamma".into(),
                    summarized: false,
                },
            }),
            agent::AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: agent::TurnStatus::Completed,
                usage: None,
            },
        ];
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            cwd.into(),
            None,
        );
        meta.id = session.into();
        for (topic, event) in [
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![meta],
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::SessionStatus {
                    session_id: session.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(session_status(session, cwd))),
            ),
            (
                Topic::SessionEvents {
                    session_id: session.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: records.len() as u64,
                    records: records.into_iter().map(Into::into).collect(),
                },
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| store.show_thread(session.into(), cx));
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some(session), cx)
        });
        resize(cx, 1200.);
        let composer = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .unwrap()
                .chat(cx)
                .read(cx)
                .composer()
        });
        let focus = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
        let text = cx
            .debug_bounds("markdown-block-0")
            .expect("the answer's paragraph");
        let at = |dx: f32| gpui::point(text.left() + px(dx), text.center().y);
        let none = gpui::Modifiers::default();
        let select =
            |from: gpui::Point<Pixels>, to: gpui::Point<Pixels>, cx: &mut VisualTestContext| {
                cx.simulate_mouse_down(from, MouseButton::Left, none);
                draw(cx);
                for step in [from + gpui::point(px(5.), px(0.)), to] {
                    cx.simulate_mouse_move(step, Some(MouseButton::Left), none);
                    draw(cx);
                }
                cx.simulate_mouse_up(to, MouseButton::Left, none);
                draw(cx);
                cx.simulate_keystrokes("secondary-c");
                draw(cx);
                let copied = cx.read_from_clipboard().and_then(|item| item.text());
                cx.update(|window, _| assert!(!focus.is_focused(window), "{copied:?}"));
                copied
            };

        assert_eq!(
            select(at(1.), at(600.), cx).as_deref(),
            Some("Selectable answer alpha beta gamma")
        );

        cx.simulate_mouse_down(at(30.), MouseButton::Left, none);
        cx.simulate_mouse_up(at(30.), MouseButton::Left, none);
        draw(cx);
        cx.update(|window, _| assert!(!focus.is_focused(window)));
        assert_eq!(select(at(1.), at(60.), cx).as_deref(), Some("Select"));
    }

    #[gpui::test]
    fn a_click_in_the_thread_pane_focuses_the_composer_only_on_desktop(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        resize(cx, 1200.);
        let composer = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .unwrap()
                .chat(cx)
                .read(cx)
                .composer()
        });
        let focus = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
        let timeline = gpui::point(px(700.), px(300.));
        for mobile in [true, false] {
            cx.update(|window, cx| {
                crate::window_seam::override_mobile_for_test(cx, mobile);
                window.blur(cx);
            });
            draw(cx);
            cx.simulate_mouse_down(timeline, MouseButton::Left, gpui::Modifiers::default());
            cx.simulate_mouse_up(timeline, MouseButton::Left, gpui::Modifiers::default());
            draw(cx);
            cx.update(|window, _| {
                assert_eq!(focus.is_focused(window), !mobile, "mobile: {mobile}");
            });
        }
    }

    /// A wide window over two indexed threads, `thread-1` (listed first)
    /// shown in its only pane, the keyboard in it, its transitions done.
    fn mount_panes(
        cx: &mut TestAppContext,
    ) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        let (shell, host, cx) = mount(cx);
        cx.update(|window, cx| {
            // Focus listeners run only for an active window, as on screen.
            window.activate_window();
            crate::shortcut::init(cx);
            set_back_target(window.window_handle(), &shell, cx);
        });
        resize(cx, 2000.);
        send_events(
            &host,
            vec![
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(Default::default()),
                ),
                panes_index(&[]),
            ],
        );
        serve_thread(&host, "thread-1");
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-1"), cx)
        });
        // The window settles before a pointer reaches it.
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        (shell, host, cx)
    }

    const PANES_CWD: &str = "/tmp/tcode-panes";

    fn send_events(host: &MountedShell, events: Vec<(Topic, ServerEvent)>) {
        for (topic, event) in events {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
    }

    /// The index of [`mount_panes`]' two threads, `thread-1` listed first,
    /// with `archived` archived.
    fn panes_index(archived: &[&str]) -> (Topic, ServerEvent) {
        let sessions = [("thread-1", 2), ("thread-2", 1)].map(|(id, updated_at)| {
            let mut meta = tcode_core::project::SessionMeta::new(
                agent::ProviderKind::Codex,
                PANES_CWD.into(),
                None,
            );
            meta.id = id.into();
            meta.title = id.into();
            meta.updated_at = updated_at;
            meta.archived_at = archived.contains(&id).then_some(3);
            meta
        });
        (
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions: sessions.to_vec(),
                projects: Vec::new(),
            }),
        )
    }

    fn serve_status(host: &MountedShell, id: &str, draft: bool) {
        let mut status = session_status(id, std::path::Path::new(PANES_CWD));
        status.draft = draft;
        send_events(
            host,
            vec![(
                Topic::SessionStatus {
                    session_id: id.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            )],
        );
    }

    /// What the host answers a thread's subscriptions with: its status and an
    /// empty log.
    fn serve_thread(host: &MountedShell, id: &str) {
        serve_status(host, id, false);
        send_events(
            host,
            vec![(
                Topic::SessionEvents {
                    session_id: id.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 0,
                    records: Vec::new(),
                },
            )],
        );
    }

    /// The thread each pane on screen shows, a closing one too, in the
    /// layout's order.
    fn pane_threads(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Vec<Option<String>> {
        shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .all_panes(cx)
                .iter()
                .map(|pane| pane.read(cx).session_id().map(str::to_owned))
                .collect()
        })
    }

    /// Which pane holds the keyboard.
    fn keyboard_pane(shell: &Entity<AppShell>, cx: &mut VisualTestContext) -> Option<usize> {
        cx.update(|window, cx| {
            shell
                .read(cx)
                .attachment
                .as_ref()
                .expect("attached")
                .all_panes(cx)
                .iter()
                .position(|pane| pane.focus_handle(cx).contains_focused(window, cx))
        })
    }

    /// The chat column's width, and the width each pane on screen shows at,
    /// in the layout's order.
    fn pane_widths(shell: &Entity<AppShell>, cx: &VisualTestContext) -> (Pixels, Vec<Pixels>) {
        shell.read_with(cx, widths_of)
    }

    fn widths_of(shell: &AppShell, cx: &App) -> (Pixels, Vec<Pixels>) {
        let attachment = shell.attachment.as_ref().expect("attached");
        (
            attachment.thread_panes.read(cx).bounds().size.width,
            attachment
                .all_panes(cx)
                .iter()
                .map(|pane| pane.read(cx).bounds().size.width)
                .collect(),
        )
    }

    /// Where each pane on screen was painted, in the layout's order.
    fn pane_bounds(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Vec<gpui::Bounds<Pixels>> {
        shell.read_with(cx, |shell, cx| {
            let attachment = shell.attachment.as_ref().expect("attached");
            attachment
                .all_panes(cx)
                .iter()
                .map(|pane| pane.read(cx).bounds())
                .collect()
        })
    }

    /// Answer the scratch draft the client asked for with `draft`, and serve
    /// it.
    fn answer_draft(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        draft: &str,
        cx: &mut VisualTestContext,
    ) {
        ack_draft(host, draft);
        await_restore_update(shell, cx, |store, _| store.is_thread_open(draft));
        serve_thread(host, draft);
        await_restore_update(shell, cx, |store, cx| {
            !store.chat_loading_for(Some(draft), cx)
        });
        draw(cx);
    }

    /// The host names `draft` as the scratch draft the client asked for.
    fn ack_draft(host: &MountedShell, draft: &str) {
        let request = std::iter::from_fn(|| host.outgoing.try_recv().ok())
            .map(|line| decode_client_line(&line).expect("client line"))
            .find(|message| {
                matches!(
                    message.payload,
                    ClientPayload::Command(Command::StartScratchDraft)
                )
            })
            .expect("a draft was asked for");
        host.incoming
            .try_send(
                encode_line(&HostMessage::Ack {
                    id: request.id,
                    result: Ok(tcode_protocol::CommandResponse::SessionId(Some(
                        draft.into(),
                    ))),
                })
                .unwrap(),
            )
            .unwrap();
    }

    /// Alt-click the threads list's `row`, showing `id`, which opens it in a
    /// new pane, and serve it.
    fn open_beside_from_list(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        row: &'static str,
        id: &str,
        cx: &mut VisualTestContext,
    ) {
        let row = cx.debug_bounds(row).expect("the thread's row");
        let alt = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        cx.simulate_mouse_move(row.center(), None, alt);
        draw(cx);
        cx.simulate_click(row.center(), alt);
        draw(cx);
        serve_thread(host, id);
        await_restore_update(shell, cx, |store, cx| !store.chat_loading_for(Some(id), cx));
        // The pane glides in before the pointer reaches it.
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
    }

    /// Press on the threads list's `row` and carry it to `to`, drawing as the
    /// pointer moves; the caller releases it.
    fn drag_row(row: &'static str, to: gpui::Point<Pixels>, cx: &mut VisualTestContext) {
        let from = cx.debug_bounds(row).expect("the thread's row").center();
        let none = gpui::Modifiers::default();
        cx.simulate_mouse_move(from, None, none);
        draw(cx);
        cx.simulate_mouse_down(from, MouseButton::Left, none);
        draw(cx);
        for step in [from + gpui::point(px(20.), px(0.)), (from + to) / 2., to] {
            cx.simulate_mouse_move(step, Some(MouseButton::Left), none);
            draw(cx);
        }
        // The drop's placeholder settles where the pointer rests.
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
    }

    /// Where the column's only pane was painted.
    fn only_pane(shell: &Entity<AppShell>, cx: &VisualTestContext) -> gpui::Bounds<Pixels> {
        shell.read_with(cx, |shell, cx| {
            let panes = shell.attachment.as_ref().expect("attached").all_panes(cx);
            assert_eq!(panes.len(), 1);
            panes[0].read(cx).bounds()
        })
    }

    /// A thread dragged from the threads list to a pane's right edge shows,
    /// while it hovers, the half of the pane it would take, and opens there
    /// with the keyboard on release.
    #[gpui::test]
    fn a_thread_dropped_on_a_panes_edge_opens_beside_it(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let pane = only_pane(&shell, cx);
        let to = gpui::point(pane.right() - px(40.), pane.center().y);
        drag_row("sidebar-thread-thread-2", to, cx);
        let indicator = cx
            .debug_bounds("pane-drop-indicator")
            .expect("the pane the drop would open");
        assert!(indicator.left() >= pane.center().x && indicator.right() <= pane.right());

        let ghost = |cx: &mut VisualTestContext| cx.debug_bounds("drag-ghost-thread-thread-2");
        let carried = ghost(cx).expect("the row is carried");
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
        // The placeholder leaves where the drop landed, fading out, and the
        // row's ghost travels into it rather than back to the list.
        assert_eq!(cx.debug_bounds("pane-drop-indicator"), Some(indicator));
        assert_eq!(ghost(cx), Some(carried));
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let landing = ghost(cx).expect("the ghost, on its way in");
        let off = |ghost: gpui::Bounds<Pixels>| (ghost.center().x - indicator.center().x).abs();
        assert!(
            off(landing) < off(carried),
            "{carried:?} -> {landing:?} into {indicator:?}"
        );
        serve_thread(&host, "thread-2");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert!(cx.debug_bounds("pane-drop-indicator").is_none());
        crate::test_frames::draw_settled(cx);
        assert_eq!(ghost(cx), None, "the ghost has gone into the pane");
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-2".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-2"));
            assert!(store.is_thread_open("thread-1"));
        });
    }

    /// A thread carried from one pane to the next takes its placeholder
    /// along, from where it showed the drop to the pane it now would land
    /// in, rather than the first pane's vanishing and the next one's
    /// appearing at rest.
    #[gpui::test]
    fn a_drops_placeholder_travels_from_pane_to_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        let panes: Vec<_> = shell.read_with(cx, |shell, cx| {
            let attachment = shell.attachment.as_ref().expect("attached");
            attachment
                .all_panes(cx)
                .iter()
                .map(|pane| pane.read(cx).bounds())
                .collect()
        });
        assert_eq!(panes.len(), 2);
        drag_row("sidebar-thread-thread-2", panes[0].center(), cx);
        let first = cx
            .debug_bounds("pane-drop-indicator")
            .expect("the pane the drop would replace");
        assert!(panes[0].contains(&first.center()), "{first:?} in {panes:?}");

        let none = gpui::Modifiers::default();
        cx.simulate_mouse_move(panes[1].center(), Some(MouseButton::Left), none);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 4);
        draw(cx);
        let moving = cx
            .debug_bounds("pane-drop-indicator")
            .expect("the placeholder, on its way");
        assert!(
            first.left() < moving.left() && moving.left() < panes[1].left(),
            "between {first:?} and {:?}: {moving:?}",
            panes[1]
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE * 2);
        draw(cx);
        let landed = cx
            .debug_bounds("pane-drop-indicator")
            .expect("the pane the drop would fill");
        assert!(
            panes[1].contains(&landed.center()),
            "{landed:?} in {panes:?}"
        );
    }

    /// Dropped in a pane's middle, a thread takes that pane's place and the
    /// thread it showed closes.
    #[gpui::test]
    fn a_thread_dropped_on_a_panes_middle_replaces_its_thread(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let pane = only_pane(&shell, cx);
        drag_row("sidebar-thread-thread-2", pane.center(), cx);
        let indicator = cx
            .debug_bounds("pane-drop-indicator")
            .expect("the pane the drop would replace");
        assert!(indicator.left() < pane.center().x && indicator.right() > pane.center().x);

        cx.simulate_mouse_up(pane.center(), MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
        serve_thread(&host, "thread-2");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-2"));
            assert!(!store.is_thread_open("thread-1"));
        });
    }

    /// A press on a subagent's window in its lead's timeline opens the
    /// agent's thread beside the lead, in the lead's own pane, with the
    /// keyboard; Escape from it closes it and gives the keyboard back to the
    /// lead's composer; once shown, the same press leaves the one agent
    /// shown rather than opening it twice.
    #[gpui::test]
    fn a_subagent_window_opens_its_thread_beside_its_lead(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        // The lead is reopened below, so the host answers it with the log
        // that spawned the agent.
        cx.simulate_keystrokes("secondary-2");
        serve_thread(&host, "thread-2");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
            unreachable!("panes_index sends a snapshot");
        };
        let mut mirror = index.sessions[1].clone();
        mirror.id = "mirror".into();
        mirror.title = "mirror".into();
        mirror.parent_session_id = Some("thread-1".into());
        mirror.native_subagent = Some("spawn-1".into());
        index.sessions.push(mirror);
        let spawn = agent::AgentEvent::ItemStarted(agent::ThreadItem {
            id: "spawn-1".into(),
            parent_item_id: None,
            content: agent::ItemContent::Subagent {
                agent_type: "explore".into(),
                description: "Survey the panes".into(),
                status: agent::ItemStatus::InProgress,
                summary: None,
                model: None,
                effort: None,
            },
        });
        let records: Vec<_> = [
            agent::AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: "ask".into(),
                parent_item_id: None,
                content: agent::ItemContent::UserMessage {
                    text: "Look around".into(),
                    context_len: None,
                    attachments: vec![],
                },
            }),
            spawn,
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        send_events(
            &host,
            vec![
                (topic, ServerEvent::IndexSnapshot(index)),
                (
                    Topic::SessionEvents {
                        session_id: "thread-1".into(),
                    },
                    ServerEvent::SessionSnapshot {
                        total: records.len() as u64,
                        total_turns: 1,
                        truncated: false,
                        from: 0,
                        end: records.len() as u64,
                        records,
                    },
                ),
            ],
        );
        cx.simulate_keystrokes("secondary-1");
        await_restore_update(&shell, cx, |store, cx| {
            store.thread_meta("mirror").is_some() && !store.chat_loading_for(Some("thread-1"), cx)
        });
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        let log = cx
            .debug_bounds("worklog-header-0-spawn-1")
            .expect("the turn's work log");
        cx.simulate_mouse_move(log.center(), None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_click(log.center(), gpui::Modifiers::default());
        // The reader presses each part once it has folded in: the subagent
        // runs, so the window never rests; frames over the fold stand in.
        let settle = |cx: &mut VisualTestContext| {
            for _ in 0..16 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.update(|window, cx| window.simulate_next_frame(cx));
                draw(cx);
            }
        };
        settle(cx);
        let window = cx
            .debug_bounds("activity-row-spawn-1")
            .expect("the subagent's window");
        cx.simulate_click(window.center(), gpui::Modifiers::default());
        draw(cx);
        serve_thread(&host, "mirror");
        // The host makes a native subagent's thread read-only.
        let mut status = session_status("mirror", std::path::Path::new(PANES_CWD));
        status.conversation_read_only = true;
        send_events(
            &host,
            vec![(
                Topic::SessionStatus {
                    session_id: "mirror".into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            )],
        );
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("mirror"), cx)
                && store.native_subagent_readonly_for(Some("mirror"), cx)
        });
        settle_panes(cx);
        let store = store_of(&shell, cx);
        let lead_typing = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                shell
                    .read(cx)
                    .attachment
                    .as_ref()
                    .expect("attached")
                    .all_panes(cx)[0]
                    .read(cx)
                    .chat()
                    .read(cx)
                    .composer()
                    .read(cx)
                    .has_keyboard(window, cx)
            })
        };
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        assert!(cx.debug_bounds("chat-side-header").is_some());
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        assert!(!lead_typing(cx), "the keyboard is with the agent");
        store.read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-1"));
            assert!(store.is_thread_open("mirror"));
        });
        // The agent's composer is its settings row, which has no field: a
        // key types nowhere.
        assert!(cx.debug_bounds("composer-subagent").is_some());
        sent(&host);
        cx.simulate_keystrokes("x enter");
        draw(cx);
        assert!(
            sent(&host)
                .iter()
                .all(|payload| !matches!(payload, ClientPayload::Command(_)))
        );

        cx.simulate_keystrokes("escape");
        settle_panes(cx);
        assert!(cx.debug_bounds("chat-side-header").is_none());
        assert!(
            lead_typing(cx),
            "the keyboard is back in the lead's composer"
        );
        store.read_with(cx, |store, _| assert!(!store.is_thread_open("mirror")));

        for press in 0..2 {
            let window = cx
                .debug_bounds("activity-row-spawn-1")
                .expect("the subagent's window");
            cx.simulate_click(window.center(), gpui::Modifiers::default());
            settle_panes(cx);
            assert_eq!(
                pane_threads(&shell, cx),
                [Some("thread-1".into())],
                "press {press}"
            );
            assert!(
                cx.debug_bounds("chat-side-header").is_some(),
                "press {press}"
            );
            assert!(!lead_typing(cx), "press {press}");
            store.read_with(cx, |store, _| assert!(store.is_thread_open("mirror")));
        }
    }

    /// `id`'s log as the host answers it: a turn still running, in which it
    /// asked for each of `spawns`' native subagents.
    fn serve_spawning(host: &MountedShell, id: &str, spawns: &[&str]) {
        let records: Vec<tcode_core::session::StoredEvent> = [
            agent::AgentEvent::TurnStarted {
                turn_id: format!("{id}-turn"),
            },
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: format!("{id}-ask"),
                parent_item_id: None,
                content: agent::ItemContent::UserMessage {
                    text: "Look around".into(),
                    context_len: None,
                    attachments: vec![],
                },
            }),
        ]
        .into_iter()
        .chain(spawns.iter().map(|spawn| {
            agent::AgentEvent::ItemStarted(agent::ThreadItem {
                id: (*spawn).into(),
                parent_item_id: None,
                content: agent::ItemContent::Subagent {
                    agent_type: "explore".into(),
                    description: format!("Survey for {spawn}"),
                    status: agent::ItemStatus::InProgress,
                    summary: None,
                    model: None,
                    effort: None,
                },
            })
        }))
        .map(Into::into)
        .collect();
        serve_status(host, id, false);
        send_events(
            host,
            vec![(
                Topic::SessionEvents {
                    session_id: id.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: records.len() as u64,
                    total_turns: 1,
                    truncated: false,
                    from: 0,
                    end: records.len() as u64,
                    records,
                },
            )],
        );
    }

    /// The agent each pane on screen shows beside its thread, in the
    /// layout's order.
    fn pane_sides(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Vec<Option<String>> {
        shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .all_panes(cx)
                .iter()
                .map(|pane| {
                    pane.read(cx)
                        .chat()
                        .read(cx)
                        .side_session_id()
                        .map(str::to_owned)
                })
                .collect()
        })
    }

    /// An agent opened from what a pane draws opens in that pane, whatever
    /// else shows its lead. Here `thread-1`'s agent is open beside it on the
    /// left and, its agents spread, in a pane of its own on the right. An
    /// agent the right pane's thread ran, opened from that thread's rail row
    /// or, too narrow for the rail, from the right panel's row, shows beside
    /// it on the right; one that agent ran in turn, opened from its window
    /// in the agent's own timeline, takes its place there; Escape closes it.
    /// The left pane keeps its thread, its place and the agent beside it
    /// throughout, over leads too narrow for the card and on the card beside
    /// wider ones.
    #[gpui::test]
    fn an_agent_opens_in_the_pane_it_was_opened_from(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
            unreachable!("panes_index sends a snapshot");
        };
        // `thread-1` dispatched `agent-1`, whose provider ran `agent-2`,
        // which ran `agent-3`.
        for (id, lead, spawn) in [
            ("agent-1", "thread-1", None),
            ("agent-2", "agent-1", Some("spawn-2")),
            ("agent-3", "agent-2", Some("spawn-3")),
        ] {
            let mut agent = index.sessions[1].clone();
            agent.id = id.into();
            agent.title = id.into();
            agent.parent_session_id = Some(lead.into());
            agent.native_subagent = spawn.map(Into::into);
            agent.created_at = 3;
            index.sessions.push(agent);
        }
        send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
        await_restore_update(&shell, cx, |store, _| {
            store.thread_meta("agent-3").is_some()
        });
        settle_panes(cx);
        let press = |selector: &'static str, cx: &mut VisualTestContext| {
            let at = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("no {selector}"));
            cx.simulate_mouse_move(at.center(), None, gpui::Modifiers::default());
            draw(cx);
            cx.simulate_click(at.center(), gpui::Modifiers::default());
            draw(cx);
            // The subagents run, so their windows never rest.
            for _ in 0..16 {
                display_frame(cx);
            }
        };
        let serve_agent = |id: &str, spawns: &[&str], cx: &mut VisualTestContext| {
            serve_spawning(&host, id, spawns);
            if id != "agent-1" {
                serve_subagent_status(&host, id);
            }
            await_restore_update(&shell, cx, |store, cx| {
                !store.chat_loading_for(Some(id), cx)
                    && store.timeline_settled_for(id)
                    && (id == "agent-1" || store.native_subagent_readonly_for(Some(id), cx))
            });
            settle_panes(cx);
            settle_frames(cx);
        };
        press("rail-agent-agent-1", cx);
        serve_agent("agent-1", &["spawn-2"], cx);
        choose_from_palette(&shell, &PaneLayout::SpreadAgents.label(), cx);
        // The layout goes in as the column fades, and the agent's pane then
        // asks for its log again.
        settle_panes(cx);
        serve_agent("agent-1", &["spawn-2"], cx);
        let both = [Some("thread-1".to_owned()), Some("agent-1".to_owned())];
        assert_eq!(pane_threads(&shell, cx), both);
        assert_eq!(pane_sides(&shell, cx), [Some("agent-1".into()), None]);

        for (width, card) in [(2000., false), (2600., true)] {
            resize(cx, width);
            settle_panes(cx);
            let left = pane_bounds(&shell, cx)[0];
            if card {
                press("rail-agent-spawn-2", cx);
            } else {
                // The keyboard in the right pane, its agents in the panel.
                let right = pane_bounds(&shell, cx)[1];
                let at = gpui::point(right.center().x, right.top() + right.size.height / 4.);
                cx.simulate_mouse_move(at, None, gpui::Modifiers::default());
                draw(cx);
                cx.simulate_click(at, gpui::Modifiers::default());
                settle_panes(cx);
                assert_eq!(keyboard_pane(&shell, cx), Some(1));
                store.update(cx, |store, cx| {
                    store.toggle_tab_panel_for("agent-1", tcode_core::ui::RightTab::Agents, cx)
                });
                settle_panes(cx);
                press("agent-row-spawn-2", cx);
            }
            serve_agent("agent-2", &["spawn-3"], cx);
            assert_eq!(pane_threads(&shell, cx), both, "{width}");
            assert_eq!(pane_bounds(&shell, cx)[0], left, "{width}");
            assert_eq!(
                pane_sides(&shell, cx),
                [Some("agent-1".into()), Some("agent-2".into())],
                "{width}"
            );
            assert_eq!(
                cx.debug_bounds("chat-side-sheet").is_some(),
                !card,
                "{width}: the agents cover their leads"
            );

            if cx.debug_bounds("activity-row-spawn-3").is_none() {
                press("worklog-header-0-spawn-3", cx);
            }
            press("activity-row-spawn-3", cx);
            serve_agent("agent-3", &[], cx);
            assert_eq!(pane_threads(&shell, cx), both, "{width}");
            assert_eq!(pane_bounds(&shell, cx)[0], left, "{width}");
            assert_eq!(
                pane_sides(&shell, cx),
                [Some("agent-1".into()), Some("agent-3".into())],
                "{width}"
            );

            cx.simulate_keystrokes("escape");
            settle_panes(cx);
            assert_eq!(pane_threads(&shell, cx), both, "{width}");
            assert_eq!(
                pane_sides(&shell, cx),
                [Some("agent-1".into()), None],
                "{width}"
            );
            // Its panel stays beside it until the reader closes it, and
            // while it shows the agents the rail does not.
            store.update(cx, |store, cx| {
                store.close_right_panel_for(Some("agent-1"), cx)
            });
            settle_panes(cx);
        }
    }

    /// [`mount_panes`] with `agent-1` dispatched by `thread-1`.
    fn mount_lead_with_agent(
        cx: &mut TestAppContext,
    ) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        let (shell, host, cx) = mount_panes(cx);
        send_events(&host, vec![lead_with_agent_index("thread-1")]);
        await_restore_update(&shell, cx, |store, _| {
            store.thread_meta("agent-1").is_some()
        });
        draw(cx);
        (shell, host, cx)
    }

    /// The panes' index with `agent-1` dispatched by `thread-1`, which is
    /// titled `lead_title`.
    fn lead_with_agent_index(lead_title: &str) -> (Topic, ServerEvent) {
        let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
            unreachable!("panes_index sends a snapshot");
        };
        index.sessions[0].title = lead_title.into();
        let mut agent = index.sessions[1].clone();
        agent.id = "agent-1".into();
        agent.title = "agent-1".into();
        agent.parent_session_id = Some("thread-1".into());
        agent.created_at = 3;
        index.sessions.push(agent);
        (topic, ServerEvent::IndexSnapshot(index))
    }

    /// Let the panes' and panels' motion land: the longest of it is an
    /// agent's card growing out of what opened it beside its lead, or
    /// shrinking back into it.
    fn settle_panes(cx: &mut VisualTestContext) {
        // A glide starts on the frame that draws its change, and a display
        // draws one before any time has gone by.
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(800));
        draw(cx);
    }

    /// `id` finishes a turn that changed `file`.
    fn serve_changed_turn(host: &MountedShell, id: &str, file: &str) {
        let turn_id = format!("turn-{file}");
        let events = [
            agent::AgentEvent::TurnStarted {
                turn_id: turn_id.clone(),
            },
            agent::AgentEvent::TurnChangesUpdated {
                turn_id: turn_id.clone(),
                changes: agent::file_changes_from_unified_diff(&format!(
                    "diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n\
                     @@ -1 +1 @@\n-fn old() {{}}\n+fn new() {{}}\n",
                ))
                .expect("one file section"),
                completeness: agent::ChangeCompleteness::Exact,
            },
            agent::AgentEvent::TurnCompleted {
                turn_id,
                status: agent::TurnStatus::Completed,
                usage: None,
            },
        ];
        send_events(
            host,
            events
                .into_iter()
                .map(|event| {
                    (
                        Topic::SessionEvents {
                            session_id: id.into(),
                        },
                        ServerEvent::SessionEvent(event.into()),
                    )
                })
                .collect(),
        );
    }

    /// `id`'s status as the host reports a native subagent's: read-only,
    /// its composer only its settings row.
    fn serve_subagent_status(host: &MountedShell, id: &str) {
        let mut status = session_status(id, std::path::Path::new(PANES_CWD));
        status.conversation_read_only = true;
        send_events(
            host,
            vec![(
                Topic::SessionStatus {
                    session_id: id.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            )],
        );
    }

    /// Open `agent-1` from its lead's Agents panel, and serve it with an
    /// empty log.
    fn open_agent_from_panel(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        cx: &mut VisualTestContext,
    ) {
        let row = cx
            .debug_bounds("agent-row-agent-1")
            .expect("the agent's row in the Agents panel");
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        draw(cx);
        serve_thread(host, "agent-1");
        // The app applies what the host sent as it arrives; an agent come
        // back to draws from what was kept until its window lands.
        await_restore_update(shell, cx, |store, cx| {
            !store.chat_loading_for(Some("agent-1"), cx) && store.timeline_settled_for("agent-1")
        });
        settle_panes(cx);
    }

    /// Press the control `selector` names, and let what it moves land.
    fn press_control(selector: &'static str, cx: &mut VisualTestContext) {
        let control = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("no {selector}"));
        cx.simulate_mouse_move(control.center(), None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_click(control.center(), gpui::Modifiers::default());
        draw(cx);
        settle_panes(cx);
    }

    /// An agent opened from its lead's Agents panel shows beside the lead in
    /// the lead's own pane, the panel staying docked beside them: on a card
    /// in the side rail's place, under the lead's header, at the rail's edge
    /// and a third of what the panel leaves, the lead's column keeping its
    /// width clear of it, and the lead keeping the store's focus. Close takes
    /// it away, the panel still there, and with that closed the lead's pane,
    /// its column and the keyboard are back where they were before it opened,
    /// with the agent's thread closed. Opening and closing again leaves the
    /// same layout each time.
    /// The card's edge, dragged, widens it, and the lead's column keeps its
    /// width.
    #[gpui::test]
    fn an_agent_opens_beside_its_lead_in_the_leads_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        let store = store_of(&shell, cx);
        settle_panes(cx);
        let panes = pane_bounds(&shell, cx);
        assert_eq!(panes.len(), 1);
        let header = cx.debug_bounds("chat-title").expect("the lead's header");
        let column = cx
            .debug_bounds("composer-card")
            .expect("the lead's composer");

        for round in 0..3 {
            store.update(cx, |store, cx| {
                store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Agents, cx)
            });
            settle_panes(cx);
            let panel = cx.debug_bounds("right-panel").expect("the panel");
            open_agent_from_panel(&shell, &host, cx);
            assert_eq!(
                cx.debug_bounds("right-panel"),
                Some(panel),
                "round {round}: the panel stays beside the agent"
            );
            let beside = cx.debug_bounds("pane-conversation").expect("the lead");
            assert_eq!(beside.right(), panel.left(), "round {round}");
            assert_eq!(
                pane_threads(&shell, cx),
                [Some("thread-1".into())],
                "round {round}"
            );
            assert_eq!(pane_bounds(&shell, cx), panes, "round {round}");
            let pane = panes[0];
            let side = cx
                .debug_bounds("chat-side-header")
                .expect("the agent beside its lead");
            let card = cx.debug_bounds("chat-side-card").expect("the agent's card");
            assert!(
                card.top() >= header.bottom() && card.bottom() < pane.bottom(),
                "{card:?} under {header:?} in {pane:?}"
            );
            assert!(
                side.left() >= card.left()
                    && side.right() <= card.right()
                    && side.top() >= card.top(),
                "{side:?} on {card:?}"
            );
            assert!(
                (beside.right() - px(crate::material::CHAT_RAIL_EDGE) - card.right()).abs()
                    < px(1.),
                "{card:?} at the rail's edge of {beside:?}"
            );
            assert!(
                (card.size.width * 3. - beside.size.width).abs() < beside.size.width * 0.03,
                "{card:?} is a third of {beside:?}"
            );
            let lead = lead_card(column, cx).unwrap_or_else(|| {
                panic!("round {round}: the lead's composer keeps the width of {column:?}")
            });
            assert!(lead.right() < card.left(), "{lead:?} clear of {card:?}");
            assert_eq!(keyboard_pane(&shell, cx), Some(0), "round {round}");
            store.read_with(cx, |store, _| {
                assert!(store.is_thread_open("thread-1"));
                assert!(store.is_thread_open("agent-1"), "round {round}");
            });

            press_control("side-close", cx);
            assert!(
                cx.debug_bounds("chat-side-header").is_none(),
                "round {round}"
            );
            assert_eq!(
                pane_threads(&shell, cx),
                [Some("thread-1".into())],
                "round {round}"
            );
            assert_eq!(pane_bounds(&shell, cx), panes, "round {round}");
            assert_eq!(cx.debug_bounds("right-panel"), Some(panel), "round {round}");
            store.update(cx, |store, cx| {
                store.close_right_panel_for(Some("thread-1"), cx)
            });
            settle_panes(cx);
            assert_eq!(
                cx.debug_bounds("composer-card"),
                Some(column),
                "round {round}"
            );
            assert_eq!(cx.debug_bounds("right-panel"), None, "round {round}");
            assert_eq!(keyboard_pane(&shell, cx), Some(0), "round {round}");
            store.read_with(cx, |store, _| {
                assert!(store.is_thread_open("thread-1"));
                assert!(!store.is_thread_open("agent-1"), "round {round}");
            });
        }

        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Agents, cx)
        });
        settle_panes(cx);
        open_agent_from_panel(&shell, &host, cx);
        // Beside a docked panel the card is about as wide as it can be.
        store.update(cx, |store, cx| {
            store.close_right_panel_for(Some("thread-1"), cx)
        });
        settle_panes(cx);
        let card = cx.debug_bounds("chat-side-card").expect("the agent's card");
        let lead = lead_card(column, cx).expect("the lead's composer");
        let grip = cx
            .debug_bounds("chat-side-grip")
            .expect("the grip of the card's edge");
        assert!(
            lead.right() <= grip.left() && grip.right() <= card.left(),
            "{grip:?} in the gap between {lead:?} and {card:?}"
        );
        let edge = grip.center();
        let to = edge - gpui::point(px(10.), px(0.));
        let none = gpui::Modifiers::default();
        cx.simulate_mouse_move(edge, None, none);
        draw(cx);
        cx.simulate_mouse_down(edge, MouseButton::Left, none);
        draw(cx);
        for step in [edge - gpui::point(px(4.), px(0.)), to] {
            cx.simulate_mouse_move(step, Some(MouseButton::Left), none);
            draw(cx);
        }
        cx.simulate_mouse_up(to, MouseButton::Left, none);
        draw(cx);
        let dragged = cx.debug_bounds("chat-side-card").expect("the agent's card");
        assert_eq!(dragged.right(), card.right());
        assert!(
            (dragged.left() - (card.left() - px(10.))).abs() < px(1.),
            "{dragged:?} widened from {card:?} as far as its edge went"
        );
        let lead = lead_card(column, cx).expect("the lead's composer keeps its width");
        assert!(
            lead.right() < dragged.left(),
            "{lead:?} clear of {dragged:?}"
        );
    }

    /// The lead's composer card, `column` as the lead alone drew it, where
    /// it is painted now with the same size at the same height. With a
    /// writable agent beside the lead, the agent's composer card is a
    /// "composer-card" too, and drawn after the lead's.
    fn lead_card(
        column: gpui::Bounds<Pixels>,
        cx: &mut VisualTestContext,
    ) -> Option<gpui::Bounds<Pixels>> {
        cx.update(|window, _| {
            let scale = window.scale_factor();
            crate::test_frames::painted_quads(window)
                .iter()
                .map(|quad| {
                    let bounds = quad.bounds;
                    gpui::Bounds::new(
                        gpui::point(px(bounds.origin.x.0 / scale), px(bounds.origin.y.0 / scale)),
                        gpui::size(
                            px(bounds.size.width.0 / scale),
                            px(bounds.size.height.0 / scale),
                        ),
                    )
                })
                .find(|bounds| {
                    (bounds.size.width - column.size.width).abs() < px(0.5)
                        && (bounds.size.height - column.size.height).abs() < px(0.5)
                        && (bounds.top() - column.top()).abs() < px(0.5)
                })
        })
    }

    /// The first pane's timeline on screen.
    fn lead_viewport(shell: &Entity<AppShell>, cx: &VisualTestContext) -> gpui::Bounds<Pixels> {
        shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").all_panes(cx)[0]
                .read(cx)
                .chat()
                .read(cx)
                .capture_viewport()
                .expect("the lead's timeline")
        })
    }

    /// The timeline rows drawn with their middles between `left` and
    /// `right`, top to bottom, with their indices.
    fn rows_in(
        left: Pixels,
        right: Pixels,
        cx: &mut VisualTestContext,
    ) -> Vec<(usize, gpui::Bounds<Pixels>)> {
        (0..400)
            .filter_map(|index| {
                let row = cx.debug_bounds(format!("timeline-row-{index}").leak())?;
                (row.center().x > left && row.center().x < right).then_some((index, row))
            })
            .collect()
    }

    /// A wheel at `at`, as a trackpad's gesture moving the content down
    /// `by`, and the frames that land it.
    fn wheel(at: gpui::Point<Pixels>, by: f32, cx: &mut VisualTestContext) {
        cx.simulate_mouse_move(at, None, gpui::Modifiers::default());
        draw(cx);
        for (phase, delta) in [
            (gpui::TouchPhase::Started, 0.),
            (gpui::TouchPhase::Moved, by),
            (gpui::TouchPhase::Ended, 0.),
        ] {
            cx.simulate_event(gpui::ScrollEvent {
                position: at,
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(delta))),
                touch_phase: phase,
                ..Default::default()
            });
            draw(cx);
        }
        settle_frames(cx);
    }

    /// The side rail, and an agent's card in its place beside its lead, run
    /// from the top of the lead's timeline down to the bottom of the lead's
    /// composer card, from the first frame the card is drawn. On the card,
    /// the agent's header is at its top, its composer at its bottom and its
    /// timeline takes the rest; a wheel there scrolls the agent's timeline
    /// and leaves the lead's where it was. With the lead's terminal drawer
    /// open, the card ends above it, with the composer. On its sheet over a
    /// narrower lead, the agent takes the wheel too.
    #[gpui::test]
    fn an_agent_beside_its_lead_spans_the_slot_and_takes_the_wheel(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        let store = store_of(&shell, cx);
        serve_finished_turns(&host, "thread-1", 60, false);
        await_restore_update(&shell, cx, |store, cx| {
            store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx)
                == Some(60)
        });
        settle_frames(cx);
        let slack = px(1.);
        let rail = cx.debug_bounds("chat-rail").expect("the lead's rail");
        let lead_card = cx
            .debug_bounds("composer-card")
            .expect("the lead's composer");
        let viewport = lead_viewport(&shell, cx);
        assert!(
            (rail.top() - viewport.top()).abs() < slack
                && (rail.bottom() - lead_card.bottom()).abs() < slack,
            "{rail:?} from the top of {viewport:?} to the bottom of {lead_card:?}"
        );

        serve_finished_turns(&host, "agent-1", 30, false);
        serve_subagent_status(&host, "agent-1");
        cx.update(|window, cx| {
            crate::agents_panel::open_agent(
                &store,
                "agent-1".into(),
                crate::shell::side_host_for("thread-1", cx),
                None,
                window,
                cx,
            )
        });
        draw(cx);
        // The first frame that draws the card, however little of it is in.
        let mut first = None;
        for _ in 0..10 {
            first = cx.debug_bounds("chat-side-card");
            if first.is_some() {
                break;
            }
            display_frame(cx);
        }
        let first = first.expect("the agent's card comes in");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-1"), cx)
                && store.timeline_settled_for("agent-1")
                && store.native_subagent_readonly_for(Some("agent-1"), cx)
        });
        settle_frames(cx);
        let card = cx.debug_bounds("chat-side-card").expect("the agent's card");
        assert_eq!(
            (first.top(), first.bottom()),
            (card.top(), card.bottom()),
            "the card is as tall as it rests from its first frame"
        );
        let lead_card = cx
            .debug_bounds("composer-card")
            .expect("the lead's composer");
        let viewport = lead_viewport(&shell, cx);
        assert!(
            (card.top() - viewport.top()).abs() < slack
                && (card.bottom() - lead_card.bottom()).abs() < slack,
            "{card:?} from the top of {viewport:?} to the bottom of {lead_card:?}"
        );
        let header = cx
            .debug_bounds("chat-side-header")
            .expect("the agent's header");
        let timeline = cx
            .debug_bounds("chat-side-timeline")
            .expect("the agent's timeline");
        let composer = cx
            .debug_bounds("composer-subagent")
            .expect("the agent's composer");
        for part in [header, timeline, composer] {
            assert!(
                part.left() >= card.left() - slack
                    && part.right() <= card.right() + slack
                    && part.top() >= card.top() - slack
                    && part.bottom() <= card.bottom() + slack,
                "{part:?} on {card:?}"
            );
        }
        assert!(
            header.top() - card.top() < px(2.),
            "{header:?} at the top of {card:?}"
        );
        assert!(
            card.bottom() - composer.bottom() < px(16.),
            "{composer:?} at the bottom of {card:?}"
        );
        assert!(
            (timeline.top() - header.bottom()).abs() < slack && timeline.bottom() <= composer.top(),
            "{timeline:?} between {header:?} and {composer:?}"
        );

        let (lead_row, lead_at) = *rows_in(viewport.left(), card.left(), cx)
            .last()
            .expect("the lead's rows");
        let (agent_row, agent_at) = rows_in(card.left(), card.right(), cx)[0];
        wheel(timeline.center(), 120., cx);
        let agent_now = cx
            .debug_bounds(format!("timeline-row-{agent_row}").leak())
            .expect("the agent's row");
        assert!(
            agent_now.top() > agent_at.top() + px(60.),
            "the agent's timeline scrolled: {agent_at:?} -> {agent_now:?}"
        );
        assert_eq!(
            cx.debug_bounds(format!("timeline-row-{lead_row}").leak()),
            Some(lead_at),
            "the lead's timeline stays where it was"
        );

        // Over the lead's terminal drawer, the card still ends with the
        // lead's composer card.
        store.update(cx, |store, cx| {
            store.toggle_terminal_panel_for("thread-1".into(), cx)
        });
        settle_frames(cx);
        let card = cx.debug_bounds("chat-side-card").expect("the agent's card");
        let above = cx
            .debug_bounds("composer-card")
            .expect("the lead's composer");
        assert!(
            above.bottom() < lead_card.bottom() - px(100.),
            "the drawer opened under {above:?}"
        );
        assert!(
            (card.bottom() - above.bottom()).abs() < slack,
            "{card:?} ends with {above:?}, above the drawer"
        );
        store.update(cx, |store, cx| {
            store.toggle_terminal_panel_for("thread-1".into(), cx)
        });
        settle_frames(cx);

        // Too narrow for the card, the agent is on a sheet over the lead.
        resize(cx, 1000.);
        settle_frames(cx);
        assert_eq!(cx.debug_bounds("chat-side-card"), None);
        let timeline = cx
            .debug_bounds("chat-side-timeline")
            .expect("the agent's timeline");
        let (agent_row, agent_at) = rows_in(timeline.left(), timeline.right(), cx)[0];
        wheel(timeline.center(), 120., cx);
        let agent_now = cx
            .debug_bounds(format!("timeline-row-{agent_row}").leak())
            .expect("the agent's row");
        assert!(
            agent_now.top() > agent_at.top() + px(60.),
            "the agent's timeline scrolled: {agent_at:?} -> {agent_now:?}"
        );
    }

    /// Whether `inner` lies strictly inside the space between `from` and
    /// `to` on every edge: each of its edges between theirs, apart from
    /// both.
    fn strictly_between(
        inner: gpui::Bounds<Pixels>,
        from: gpui::Bounds<Pixels>,
        to: gpui::Bounds<Pixels>,
    ) -> bool {
        let between = |at: Pixels, a: Pixels, b: Pixels| at > a.min(b) && at < a.max(b);
        between(inner.left(), from.left(), to.left())
            && between(inner.top(), from.top(), to.top())
            && between(inner.right(), from.right(), to.right())
            && between(inner.bottom(), from.bottom(), to.bottom())
    }

    /// Whether `a` and `b` are the same rectangle, within `slack` on every
    /// edge.
    fn about(a: gpui::Bounds<Pixels>, b: gpui::Bounds<Pixels>, slack: Pixels) -> bool {
        (a.left() - b.left()).abs() <= slack
            && (a.top() - b.top()).abs() <= slack
            && (a.right() - b.right()).abs() <= slack
            && (a.bottom() - b.bottom()).abs() <= slack
    }

    /// Display frames until `selector` is no longer drawn, its bounds on
    /// each frame that drew it.
    fn frames_until_gone(
        selector: &'static str,
        cx: &mut VisualTestContext,
    ) -> Vec<gpui::Bounds<Pixels>> {
        let mut drawn = Vec::new();
        for _ in 0..80 {
            match cx.debug_bounds(selector) {
                Some(bounds) => drawn.push(bounds),
                None => return drawn,
            }
            display_frame(cx);
        }
        panic!("{selector} is still drawn after {} frames", drawn.len());
    }

    /// One frame of the agent's surface, as the renderer draws it.
    #[derive(Clone, Copy, Debug)]
    struct SurfaceFrame {
        bounds: gpui::Bounds<Pixels>,
        /// The alpha of the surface's own fill.
        fill: f32,
        /// The strongest fill of what the agent's timeline holds.
        content: f32,
        /// Whether the agent's row in the lead's rail is drawn.
        row: bool,
    }

    /// The surface named `selector`, as this frame draws it.
    fn surface_frame(selector: &'static str, cx: &mut VisualTestContext) -> Option<SurfaceFrame> {
        let bounds = cx.debug_bounds(selector)?;
        let timeline = cx.debug_bounds("chat-side-timeline");
        let row = cx.debug_bounds("rail-agent-agent-1").is_some();
        let (fill, content) = cx.update(|window, _| {
            let scale = window.scale_factor();
            let quads = crate::test_frames::painted_quads(window);
            let alpha = |quad: &gpui::Quad| quad.background.as_solid().map(|fill| fill.a);
            let fill = quads
                .iter()
                .filter(|quad| quad.bounds == bounds.scale(scale))
                .filter_map(alpha)
                .fold(0., f32::max);
            let content = timeline.map_or(0., |timeline| {
                let timeline = timeline.scale(scale);
                quads
                    .iter()
                    .filter(|quad| {
                        timeline.contains(&quad.bounds.origin)
                            && timeline.contains(&quad.bounds.bottom_right())
                    })
                    .filter_map(alpha)
                    .fold(0., f32::max)
            });
            (fill, content)
        });
        Some(SurfaceFrame {
            bounds,
            fill,
            content,
            row,
        })
    }

    /// Display frames until `selector` is no longer drawn, the surface on
    /// each frame that drew it.
    fn surface_until_gone(selector: &'static str, cx: &mut VisualTestContext) -> Vec<SurfaceFrame> {
        let mut drawn = Vec::new();
        for _ in 0..80 {
            match surface_frame(selector, cx) {
                Some(frame) => drawn.push(frame),
                None => return drawn,
            }
            display_frame(cx);
        }
        panic!("{selector} is still drawn after {} frames", drawn.len());
    }

    /// The surface named `selector` over the next `frames` display frames.
    fn surface_frames(
        selector: &'static str,
        frames: usize,
        cx: &mut VisualTestContext,
    ) -> Vec<SurfaceFrame> {
        (0..frames)
            .filter_map(|_| {
                let frame = surface_frame(selector, cx);
                display_frame(cx);
                frame
            })
            .collect()
    }

    /// Hand the window `input` and draw the frame it asks for as the app
    /// does: a moment after the input is handled, on the platform's next
    /// frame. The test platform otherwise draws a window an input dirtied
    /// at once, on the very instant the input was handled.
    fn input_then_frame(input: gpui::PlatformInput, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.dispatch_event(input, cx);
            cx.background_executor()
                .advance_clock(Duration::from_millis(1));
        });
        draw(cx);
    }

    /// Press the control `selector` with the pointer, and draw the frame
    /// after as the app does.
    fn press_side(selector: &'static str, cx: &mut VisualTestContext) -> gpui::Bounds<Pixels> {
        use gpui::InputEvent as _;
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
        let position = bounds.center();
        cx.simulate_mouse_move(position, None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_event(gpui::MouseDownEvent {
            position,
            modifiers: gpui::Modifiers::default(),
            button: gpui::MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        });
        let up = gpui::MouseUpEvent {
            position,
            modifiers: gpui::Modifiers::default(),
            button: gpui::MouseButton::Left,
            click_count: 1,
        };
        input_then_frame(up.to_platform_input(), cx);
        bounds
    }

    /// Press Escape, and draw the frame after as the app does.
    fn escape_then_frame(cx: &mut VisualTestContext) {
        use gpui::InputEvent as _;
        let escape = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("escape").expect("Escape"),
            is_held: false,
            prefer_character_input: false,
        };
        input_then_frame(escape.to_platform_input(), cx);
    }

    /// Press the agent's row in the lead's rail, and serve the agent as its
    /// thread opens; where the row was pressed.
    fn open_from_row(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        cx: &mut VisualTestContext,
    ) -> gpui::Bounds<Pixels> {
        let row = press_side("rail-agent-agent-1", cx);
        serve_finished_turns(host, "agent-1", 3, false);
        serve_subagent_status(host, "agent-1");
        await_restore_update(shell, cx, |store, _| store.timeline_settled_for("agent-1"));
        row
    }

    /// What the agent's timeline draws this frame: each quad wholly inside
    /// it, where it is and how opaque.
    fn timeline_quads(cx: &mut VisualTestContext) -> Vec<(gpui::Bounds<gpui::ScaledPixels>, f32)> {
        let Some(timeline) = cx.debug_bounds("chat-side-timeline") else {
            return Vec::new();
        };
        cx.update(|window, _| {
            let timeline = timeline.scale(window.scale_factor());
            crate::test_frames::painted_quads(window)
                .iter()
                .filter(|quad| {
                    timeline.contains(&quad.bounds.origin)
                        && timeline.contains(&quad.bounds.bottom_right())
                })
                .filter_map(|quad| Some((quad.bounds, quad.background.as_solid()?.a)))
                .filter(|(_, alpha)| *alpha > 0.)
                .collect()
        })
    }

    /// How far in what the agent's timeline drew at rest, `rest`, is now:
    /// laid out at its place while its surface moves, each quad of it stays
    /// where it was, as much of it as the surface shows.
    fn held_in(
        rest: &[(gpui::Bounds<gpui::ScaledPixels>, f32)],
        cx: &mut VisualTestContext,
    ) -> f32 {
        let now = timeline_quads(cx);
        let same = |a: gpui::Bounds<gpui::ScaledPixels>, b: gpui::Bounds<gpui::ScaledPixels>| {
            (a.origin.x.0 - b.origin.x.0).abs() < 1.
                && (a.origin.y.0 - b.origin.y.0).abs() < 1.
                && (a.size.width.0 - b.size.width.0).abs() < 1.
                && (a.size.height.0 - b.size.height.0).abs() < 1.
        };
        rest.iter()
            .filter_map(|(bounds, alpha)| {
                now.iter()
                    .find(|(drawn, _)| same(*drawn, *bounds))
                    .map(|(_, now)| now / alpha)
            })
            .fold(0., f32::max)
    }

    /// The agent's surface, `selector`, at rest, closed by `close`: it
    /// shrinks from its place into the agent's row in the rail, last drawn
    /// where the row is, holding what the agent shows on its way, which
    /// fades as the surface does, so it is never an empty block over what
    /// it covers and nothing pops as it goes.
    fn shrinks_into_row(
        selector: &'static str,
        close: impl FnOnce(&mut VisualTestContext),
        cx: &mut VisualTestContext,
    ) -> Vec<SurfaceFrame> {
        let rest = surface_frame(selector, cx).expect("the agent's surface, at rest");
        let rest_content = timeline_quads(cx);
        assert!(!rest_content.is_empty(), "the agent's timeline draws");
        let place = rest.bounds;
        close(cx);
        let mut shrinking = Vec::new();
        let mut held = Vec::new();
        while let Some(frame) = surface_frame(selector, cx) {
            assert!(shrinking.len() < 80, "{selector} is still drawn");
            shrinking.push(frame);
            held.push(held_in(&rest_content, cx));
            display_frame(cx);
        }
        let row = cx
            .debug_bounds("rail-agent-agent-1")
            .expect("the row is back in the rail");
        let last = *shrinking.last().expect("the surface shrinks");
        assert!(
            about(last.bounds, row, px(2.)),
            "it goes into {row:?}, last drawn at {last:?}"
        );
        assert!(
            shrinking
                .iter()
                .any(|frame| strictly_between(frame.bounds, row, place)),
            "it shrinks from {place:?} to {row:?} through {shrinking:?}"
        );
        let apart = |a: gpui::Bounds<Pixels>, b: gpui::Bounds<Pixels>| {
            [
                a.left() - b.left(),
                a.top() - b.top(),
                a.right() - b.right(),
                a.bottom() - b.bottom(),
            ]
            .into_iter()
            .map(|edge| edge.abs())
            .fold(px(0.), Pixels::max)
        };
        assert!(
            apart(shrinking[0].bounds, place) <= apart(shrinking[1].bounds, shrinking[0].bounds),
            "it closes from {place:?}, not from a place of its own: {shrinking:?}"
        );
        let frames: Vec<_> = shrinking.iter().zip(&held).collect();
        assert!(
            frames
                .iter()
                .any(|(frame, held)| strictly_between(frame.bounds, row, place) && **held > 0.9),
            "it holds what the agent shows on its way: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .filter(|(frame, _)| frame.fill > rest.fill * 0.5)
                .all(|(_, held)| **held > 0.5),
            "never an empty block: {frames:?} of {rest:?}"
        );
        assert!(
            shrinking
                .iter()
                .any(|frame| frame.fill > rest.fill * 0.1 && frame.fill < rest.fill * 0.9),
            "the surface turns translucent on its way: {shrinking:?}"
        );
        assert!(
            last.fill < rest.fill * 0.05 && held.last().is_some_and(|held| *held < 0.05),
            "nothing pops as it goes: {frames:?} of {rest:?}"
        );
        assert!(
            shrinking
                .iter()
                .filter(|frame| frame.fill < rest.fill)
                .all(|frame| frame.row),
            "the row is under the surface before it thins: {shrinking:?}"
        );
        shrinking
    }

    /// An agent opened from its row in the lead's rail grows out of the row
    /// into its card, and shrinks back into the row when it is closed, by
    /// Close or by Escape: partway, the card lies between the row and its
    /// place, and it rests in its place; closing, it last draws where the
    /// row is, which has come back with the rail. The surface comes in over
    /// the row from nothing; closing, it moves on the next frame, with the
    /// row already under it, holding what the agent shows as it shrinks and
    /// fading with it, so what it covered is seen rather than a block that
    /// vanishes. Over a lead too narrow for the card, the agent grows out of
    /// the row into its sheet over the lead, and Back shrinks it into the
    /// row again; closed once the row is no longer on screen, it slides off
    /// the lead without shrinking.
    #[gpui::test]
    fn an_agent_grows_out_of_its_rail_row_and_back_into_it(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        settle_frames(cx);
        let row = open_from_row(&shell, &host, cx);
        let growing = surface_frames("chat-side-card", 50, cx);
        let card = cx.debug_bounds("chat-side-card").expect("the agent's card");
        assert!(
            growing
                .iter()
                .any(|frame| strictly_between(frame.bounds, row, card)),
            "the card grows from {row:?} to {card:?} through {growing:?}"
        );
        assert!(
            about(growing[0].bounds, row, px(40.)),
            "it starts at {row:?}: {:?}",
            growing[0]
        );
        settle_frames(cx);
        assert_eq!(cx.debug_bounds("chat-side-card"), Some(card), "at rest");
        assert!(cx.debug_bounds("chat-side-header").is_some());
        let rest = surface_frame("chat-side-card", cx).expect("the agent's card");
        assert!(
            growing[0].fill < rest.fill * 0.1,
            "the row is not covered at once: {:?} of {rest:?}",
            growing[0]
        );

        let shrinking = shrinks_into_row(
            "chat-side-card",
            |cx| {
                press_side("side-close", cx);
            },
            cx,
        );
        assert!(
            shrinking[1].bounds.size.width < card.size.width,
            "it moves on the frame after the one it closed on: {shrinking:?}"
        );

        open_from_row(&shell, &host, cx);
        settle_frames(cx);
        shrinks_into_row("chat-side-card", escape_then_frame, cx);

        // A lead too narrow for the card beside it, with its rail.
        resize(cx, 1300.);
        settle_frames(cx);
        assert!(cx.debug_bounds("chat-rail").is_some(), "the rail fits");
        let row = open_from_row(&shell, &host, cx);
        let growing = frames_of("chat-side-sheet", 50, cx);
        let sheet = cx
            .debug_bounds("chat-side-sheet")
            .expect("the agent's sheet");
        assert_eq!(cx.debug_bounds("chat-side-card"), None, "no card");
        assert!(
            growing
                .iter()
                .any(|bounds| strictly_between(*bounds, row, sheet)),
            "the sheet grows from {row:?} to {sheet:?} through {growing:?}"
        );
        settle_frames(cx);
        assert_eq!(cx.debug_bounds("chat-side-sheet"), Some(sheet), "at rest");
        shrinks_into_row(
            "chat-side-sheet",
            |cx| {
                press_side("side-back", cx);
            },
            cx,
        );

        // Narrower still, the rail is gone, and with it the row.
        open_from_row(&shell, &host, cx);
        settle_frames(cx);
        resize(cx, 1000.);
        settle_frames(cx);
        assert_eq!(cx.debug_bounds("chat-rail"), None, "no rail");
        let sheet = cx
            .debug_bounds("chat-side-sheet")
            .expect("the agent's sheet");
        press_side("side-back", cx);
        let leaving = frames_until_gone("chat-side-sheet", cx);
        assert!(
            leaving
                .iter()
                .all(|bounds| bounds.size == sheet.size && bounds.top() == sheet.top()),
            "the sheet keeps its size as it leaves {sheet:?}: {leaving:?}"
        );
        assert!(
            leaving.iter().any(|bounds| bounds.left() > sheet.left()),
            "the sheet slides off the lead: {leaving:?}"
        );
    }

    /// Opened again as it shrinks back into its row, or closed as it grows
    /// out of it, the agent's surface goes on from where it is and as it is:
    /// neither its frame nor its fill jumps.
    #[gpui::test]
    fn an_agent_turned_back_midway_goes_on_from_where_it_is(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        let store = store_of(&shell, cx);
        serve_finished_turns(&host, "agent-1", 3, false);
        settle_frames(cx);
        let row = cx
            .debug_bounds("rail-agent-agent-1")
            .expect("the agent's row in the rail");
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        draw(cx);
        settle_frames(cx);
        let rest = surface_frame("chat-side-card", cx).expect("the agent's card");

        let goes_on = |before: SurfaceFrame, after: SurfaceFrame| {
            assert!(
                about(after.bounds, before.bounds, px(1.)),
                "the frame stays: {before:?} -> {after:?}"
            );
            assert!(
                (after.fill - before.fill).abs() < rest.fill * 0.02,
                "the fill stays: {before:?} -> {after:?} of {rest:?}"
            );
        };

        escape_then_frame(cx);
        for _ in 0..6 {
            display_frame(cx);
        }
        let shrinking = surface_frame("chat-side-card", cx).expect("the card, shrinking");
        assert!(
            shrinking.bounds.size.width < rest.bounds.size.width
                && shrinking.fill < rest.fill
                && shrinking.fill > 0.,
            "partway, and thinning: {shrinking:?}"
        );
        cx.update(|window, cx| {
            crate::agents_panel::open_agent(
                &store,
                "agent-1".into(),
                crate::shell::side_host_for("thread-1", cx),
                Some("rail-agent-agent-1".into()),
                window,
                cx,
            )
        });
        draw(cx);
        goes_on(
            shrinking,
            surface_frame("chat-side-card", cx).expect("the card, reopened"),
        );

        for _ in 0..4 {
            display_frame(cx);
        }
        let growing = surface_frame("chat-side-card", cx).expect("the card, growing");
        assert!(
            growing.bounds.size.width < rest.bounds.size.width,
            "partway: {growing:?}"
        );
        escape_then_frame(cx);
        goes_on(
            growing,
            surface_frame("chat-side-card", cx).expect("the card, closed again"),
        );
        let last = *surface_until_gone("chat-side-card", cx)
            .last()
            .expect("the card shrinks");
        let row = cx
            .debug_bounds("rail-agent-agent-1")
            .expect("the row is back in the rail");
        assert!(
            about(last.bounds, row, px(2.)) && last.fill < rest.fill * 0.05,
            "it goes into {row:?} unseen: {last:?}"
        );
    }

    /// Whether `inner` lies partway from `from` to `to`: each of its edges
    /// strictly between theirs where they differ, and on theirs where they
    /// are the same.
    fn partway(
        inner: gpui::Bounds<Pixels>,
        from: gpui::Bounds<Pixels>,
        to: gpui::Bounds<Pixels>,
    ) -> bool {
        let edge = |at: Pixels, a: Pixels, b: Pixels| {
            if (a - b).abs() < px(0.5) {
                (at - a).abs() < px(0.5)
            } else {
                at > a.min(b) && at < a.max(b)
            }
        };
        !about(from, to, px(0.5))
            && edge(inner.left(), from.left(), to.left())
            && edge(inner.top(), from.top(), to.top())
            && edge(inner.right(), from.right(), to.right())
            && edge(inner.bottom(), from.bottom(), to.bottom())
    }

    /// Where a frame draws the agent's card and its sheet.
    type SidePlaces = [Option<gpui::Bounds<Pixels>>; 2];

    /// The agent's card and its sheet over the next `frames` display
    /// frames, as each frame draws them, and the surface drawn.
    fn side_frames(
        frames: usize,
        cx: &mut VisualTestContext,
    ) -> (Vec<SidePlaces>, Vec<SurfaceFrame>) {
        (0..frames)
            .map(|_| {
                let drawn = [
                    cx.debug_bounds("chat-side-card"),
                    cx.debug_bounds("chat-side-sheet"),
                ];
                let surface = surface_frame("chat-side-card", cx)
                    .or_else(|| surface_frame("chat-side-sheet", cx))
                    .expect("the agent's surface");
                display_frame(cx);
                (drawn, surface)
            })
            .unzip()
    }

    /// An agent on its card beside its lead, the window narrowed until the
    /// card no longer fits beside the lead's column, grows from the card
    /// into a sheet raised over the lead: drawn in one place on every
    /// frame, partway from the card to the sheet on its way, and at rest
    /// inset from the lead's body at its sides, as high as the card and
    /// ending with the lead's composer card. On the way the surface stays
    /// opaque and what it holds stays on it, never leaving it blank. On the
    /// sheet the agent's header says it is an agent and its Back names the
    /// lead, following the lead's title; Close is gone. Widened again, the
    /// sheet shrinks back into the card where it was, and Close is back.
    #[gpui::test]
    fn an_agent_beside_its_lead_grows_into_a_sheet_and_back(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        let store = store_of(&shell, cx);
        serve_finished_turns(&host, "agent-1", 3, false);
        serve_subagent_status(&host, "agent-1");
        cx.update(|window, cx| {
            crate::agents_panel::open_agent(
                &store,
                "agent-1".into(),
                crate::shell::side_host_for("thread-1", cx),
                None,
                window,
                cx,
            )
        });
        draw(cx);
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-1"), cx)
                && store.timeline_settled_for("agent-1")
                && store.native_subagent_readonly_for(Some("agent-1"), cx)
        });
        settle_frames(cx);
        let card = cx.debug_bounds("chat-side-card").expect("the agent's card");
        assert_eq!(cx.debug_bounds("chat-side-sheet"), None);
        assert!(cx.debug_bounds("side-close").is_some(), "Close on the card");
        assert_eq!(cx.debug_bounds("chat-side-badge"), None);
        let rest = surface_frame("chat-side-card", cx).expect("the agent's card");
        let never_blank = |surfaces: &[SurfaceFrame]| {
            assert!(
                surfaces.iter().all(|frame| frame.fill >= rest.fill * 0.99),
                "the surface stays opaque: {surfaces:?} against {rest:?}"
            );
            assert!(
                surfaces
                    .iter()
                    .all(|frame| frame.content > rest.content * 0.1),
                "what it holds stays on it: {surfaces:?} against {rest:?}"
            );
        };

        resize(cx, 1000.);
        let (frames, surfaces) = side_frames(50, cx);
        settle_frames(cx);
        never_blank(&surfaces);
        assert!(
            frames
                .iter()
                .all(|drawn| drawn[0].is_none() || drawn[1].is_none()),
            "drawn in one place on each frame: {frames:?}"
        );
        assert_eq!(cx.debug_bounds("chat-side-card"), None, "no card");
        let sheet = cx
            .debug_bounds("chat-side-sheet")
            .expect("the agent's sheet");
        let left = frames
            .iter()
            .rev()
            .find_map(|drawn| drawn[0])
            .expect("the card, before the sheet");
        let growing: Vec<_> = frames.iter().filter_map(|drawn| drawn[1]).collect();
        assert!(
            growing.iter().any(|bounds| partway(*bounds, left, sheet)),
            "the card grows from {left:?} into {sheet:?} through {growing:?}"
        );
        let pane = pane_bounds(&shell, cx)[0];
        let lead_composer = cx
            .debug_bounds("composer-card")
            .expect("the lead's composer");
        let inset = px(8.);
        let resting = gpui::Bounds::from_corners(
            gpui::point(pane.left() + inset, card.top()),
            gpui::point(pane.right() - inset, lead_composer.bottom()),
        );
        assert!(
            about(sheet, resting, px(1.)),
            "{sheet:?} inset from {pane:?}, as high as {card:?}, down to the bottom of \
             {lead_composer:?}"
        );

        let header = cx
            .debug_bounds("chat-side-header")
            .expect("the agent's header");
        let badge = cx.debug_bounds("chat-side-badge").expect("the badge");
        let title = cx
            .debug_bounds("chat-side-title")
            .expect("the agent's title");
        let back = cx.debug_bounds("side-back").expect("Back");
        let named = cx
            .debug_bounds("side-back-title")
            .expect("Back names the lead");
        assert_eq!(cx.debug_bounds("side-close"), None, "no Close on the sheet");
        assert!(
            header.left() <= back.left()
                && back.right() <= badge.left()
                && badge.right() <= title.left()
                && title.right() <= header.right(),
            "Back {back:?}, then the badge {badge:?}, then the title {title:?} in {header:?}"
        );
        assert!(
            named.left() >= back.left()
                && named.right() <= back.right()
                && named.size.width > px(0.),
            "the lead's title {named:?} on Back {back:?}"
        );
        send_events(
            &host,
            vec![lead_with_agent_index(
                "thread-1, a lead with a longer title",
            )],
        );
        await_restore_update(&shell, cx, |store, _| {
            store
                .thread_meta("thread-1")
                .is_some_and(|meta| meta.title.ends_with("longer title"))
        });
        settle_frames(cx);
        let renamed = cx
            .debug_bounds("side-back-title")
            .expect("Back names the lead");
        assert!(
            renamed.size.width > named.size.width,
            "Back follows the lead's title: {named:?} -> {renamed:?}"
        );

        resize(cx, 2000.);
        let (frames, surfaces) = side_frames(50, cx);
        settle_frames(cx);
        never_blank(&surfaces);
        assert!(
            frames
                .iter()
                .all(|drawn| drawn[0].is_none() || drawn[1].is_none()),
            "drawn in one place on each frame: {frames:?}"
        );
        assert_eq!(cx.debug_bounds("chat-side-sheet"), None, "no sheet");
        assert_eq!(
            cx.debug_bounds("chat-side-card"),
            Some(card),
            "back on its card"
        );
        let left = frames
            .iter()
            .rev()
            .find_map(|drawn| drawn[1])
            .expect("the sheet, before the card");
        let shrinking: Vec<_> = frames.iter().filter_map(|drawn| drawn[0]).collect();
        assert!(
            shrinking.iter().any(|bounds| partway(*bounds, left, card)),
            "the sheet shrinks from {left:?} into {card:?} through {shrinking:?}"
        );
        assert!(cx.debug_bounds("side-close").is_some(), "Close on the card");
        assert_eq!(cx.debug_bounds("chat-side-badge"), None);
    }

    /// Where `selector` is drawn over the next `frames` display frames.
    fn frames_of(
        selector: &'static str,
        frames: usize,
        cx: &mut VisualTestContext,
    ) -> Vec<gpui::Bounds<Pixels>> {
        (0..frames)
            .filter_map(|_| {
                let bounds = cx.debug_bounds(selector);
                display_frame(cx);
                bounds
            })
            .collect()
    }

    /// On a tablet: an agent beside its lead is neither drawn nor held open
    /// while the window is compact, which shows the lead alone, and is beside
    /// its lead again once the window is wide. An agent opened from its lead
    /// in the compact window takes the lead's place, and the compact Back
    /// returns to the lead and closes the agent.
    #[gpui::test]
    fn an_agent_in_a_compact_window_takes_its_leads_place(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        as_mobile(cx);
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Agents, cx)
        });
        settle_panes(cx);
        open_agent_from_panel(&shell, &host, cx);
        assert!(cx.debug_bounds("chat-side-header").is_some());

        resize(cx, 600.);
        settle_panes(cx);
        assert!(shell.read_with(cx, |shell, cx| shell.compact(cx)));
        assert!(cx.debug_bounds("chat-side-header").is_none());
        store.read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-1"));
            assert!(!store.is_thread_open("agent-1"));
        });
        resize(cx, 2000.);
        settle_panes(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        assert!(cx.debug_bounds("chat-side-header").is_some());
        store.read_with(cx, |store, _| assert!(store.is_thread_open("agent-1")));
        press_control("side-close", cx);

        resize(cx, 600.);
        settle_panes(cx);
        cx.update(|window, cx| {
            crate::agents_panel::open_agent(
                &store,
                "agent-1".into(),
                crate::shell::side_host_for("thread-1", cx),
                None,
                window,
                cx,
            )
        });
        draw(cx);
        serve_thread(&host, "agent-1");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("agent-1"), cx) && store.timeline_settled_for("agent-1")
        });
        settle_panes(cx);
        store.read_with(cx, |store, _| {
            assert!(store.is_thread_open("agent-1"));
        });
        assert!(cx.debug_bounds("chat-side-header").is_none());

        let back = cx.debug_bounds("compact-back").expect("the compact Back");
        cx.simulate_click(back.center(), gpui::Modifiers::default());
        settle_panes(cx);
        store.read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-1"));
            assert!(!store.is_thread_open("agent-1"));
        });
        assert!(cx.debug_bounds("compact-thread-page").is_some());
    }

    /// On a phone, the Agents pill under a lead's timeline opens a sheet
    /// that shows the lead's agents.
    #[gpui::test]
    fn the_phone_agents_sheet_shows_the_leads_agents(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (_shell, _host, cx) = mount_lead_with_agent(cx);
        as_mobile(cx);
        resize(cx, 393.);
        settle_panes(cx);
        let pill = cx.debug_bounds("agents-pill").expect("the Agents pill");
        cx.simulate_click(pill.center(), gpui::Modifiers::default());
        settle_panes(cx);
        let content = cx
            .debug_bounds("touch-picker-content")
            .expect("the Agents sheet");
        let row = cx
            .debug_bounds("agent-row-agent-1")
            .expect("the agent's row");
        assert!(
            row.top() >= content.top() && row.bottom() <= content.bottom(),
            "the row {row:?} is out of the sheet's content {content:?}"
        );
    }

    /// Choose the palette's action `label`, in whatever language the host's
    /// settings left the window in.
    fn choose_from_palette(shell: &Entity<AppShell>, label: &str, cx: &mut VisualTestContext) {
        shell
            .read_with(cx, |shell, _| shell.window_state())
            .update(cx, |state, cx| state.open_palette(cx));
        draw(cx);
        cx.simulate_input(&format!(">{label}"));
        draw(cx);
        cx.simulate_keystrokes("enter");
        draw(cx);
    }

    /// Two columns from one pane keep its thread on the left and leave the
    /// right waiting; a pane command from the palette hands the keyboard back
    /// to the focused pane's composer; a single pane
    /// from two keeps the focused thread and closes the other; ⌥⌘2 lays out
    /// two columns again. Each layout goes in as its fade plays.
    #[gpui::test]
    fn a_layout_fills_its_panes_from_the_focused_one(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let settle = |cx: &mut VisualTestContext| {
            cx.executor().advance_clock(crate::edge_back::SETTLE);
            draw(cx);
        };
        choose_from_palette(&shell, &PaneLayout::Columns.label(), cx);
        settle(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
        assert!(cx.debug_bounds("waiting-pane").is_some());
        assert_eq!(keyboard_pane(&shell, cx), Some(0));

        cx.simulate_keystrokes("secondary-alt-right secondary-2");
        serve_thread(&host, "thread-2");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-2".into())]
        );
        for command in [crate::tr!("panes.zoom"), crate::tr!("panes.unzoom")] {
            choose_from_palette(&shell, &command, cx);
            settle(cx);
            assert_eq!(keyboard_pane(&shell, cx), Some(1), "{command}");
        }
        let composer = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").all_panes(cx)[1]
                .read(cx)
                .chat()
                .read(cx)
                .composer()
                .read(cx)
                .input_focus_handle(cx)
        });
        cx.update(|window, _| assert!(composer.is_focused(window), "the caret is back"));

        choose_from_palette(&shell, &PaneLayout::Single.label(), cx);
        settle(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-2"));
            assert!(!store.is_thread_open("thread-1"));
        });

        cx.simulate_keystrokes("secondary-alt-2");
        draw(cx);
        settle(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into()), None]);
    }

    /// A row of a pane header's menu leaves the keyboard in that pane's
    /// composer, as a pane command from the palette does, not on the menu's
    /// button.
    #[gpui::test]
    fn a_pane_menu_row_hands_the_keyboard_to_the_composer(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        let button = cx.debug_bounds("pane-menu").expect("the pane menu");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        draw(cx);
        let pane = keyboard_pane(&shell, cx).expect("the menu's pane has the keyboard");
        // Split right, split down, a separator, then zoom.
        let zoom = cx.debug_bounds("menu-item-3").expect("the zoom row");
        cx.simulate_click(zoom.center(), gpui::Modifiers::default());
        draw(cx);
        let (zoomed, composer) = shell.read_with(cx, |shell, cx| {
            let attachment = shell.attachment.as_ref().expect("attached");
            let composer = attachment.all_panes(cx)[pane]
                .read(cx)
                .chat()
                .read(cx)
                .composer()
                .read(cx)
                .input_focus_handle(cx);
            (attachment.zoom.is_zoomed(), composer)
        });
        assert!(zoomed, "the row ran");
        assert_eq!(keyboard_pane(&shell, cx), Some(pane));
        cx.update(|window, _| assert!(composer.is_focused(window), "the caret is back"));
    }

    /// A thread's agents spread beside it newest first, stacked on the
    /// right; one that has settled stays out.
    #[gpui::test]
    fn spreading_agents_leaves_settled_ones_out(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
            unreachable!("panes_index sends a snapshot");
        };
        for (id, created_at, settled_at) in [
            ("agent-old", 1, None),
            ("agent-new", 2, None),
            ("agent-settled", 3, Some(4)),
        ] {
            let mut agent = index.sessions[1].clone();
            agent.id = id.into();
            agent.title = id.into();
            agent.parent_session_id = Some("thread-1".into());
            agent.created_at = created_at;
            agent.settled_at = settled_at;
            index.sessions.push(agent);
        }
        send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
        await_restore_update(&shell, cx, |store, _| {
            store.thread_meta("agent-settled").is_some()
        });
        draw(cx);

        choose_from_palette(&shell, &PaneLayout::SpreadAgents.label(), cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [
                Some("thread-1".into()),
                Some("agent-new".into()),
                Some("agent-old".into())
            ]
        );
    }

    /// How opaque the chat column is painted, read from its frame's fill, and
    /// how many panes it draws, in the frame drawn now.
    fn column_scene(shell: &Entity<AppShell>, cx: &mut VisualTestContext) -> (f32, usize) {
        frame(cx, |window, cx| {
            let attachment = shell.read(cx).attachment.as_ref().expect("attached");
            let column = attachment
                .thread_panes
                .read(cx)
                .bounds()
                .scale(window.scale_factor());
            let alpha = window
                .painted_quads()
                .iter()
                .find(|quad| quad.bounds == column)
                .and_then(|quad| quad.background.as_solid())
                .expect("the column's fill")
                .a;
            (alpha, attachment.all_panes(cx).len())
        })
    }

    /// A layout goes in as a layout swap: the column fades out, the new
    /// panes are put in during the frame it is faintest, and it fades back
    /// in. A layout chosen meanwhile is the one put in, the fade going on
    /// from where it is: down to the bottom while it falls, turning back
    /// down while it rises. One that changes nothing does not fade.
    #[gpui::test]
    fn a_layout_goes_in_while_the_column_is_faded_out(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        let settle = crate::edge_back::SETTLE;
        let (rest, _) = column_scene(&shell, cx);
        let at = |after: Duration, cx: &mut VisualTestContext| {
            cx.executor().advance_clock(after);
            column_scene(&shell, cx)
        };

        cx.simulate_keystrokes("secondary-alt-2");
        assert_eq!(column_scene(&shell, cx), (rest, 1), "the key's own frame");
        let (falling, panes) = at(settle / 4, cx);
        assert!(falling < rest && panes == 1, "falling: {falling} of {rest}");
        let (lower, panes) = at(settle / 4 - Duration::from_millis(1), cx);
        assert!(
            lower < falling && panes == 1,
            "lower: {lower}, {panes} panes"
        );
        let (lowest, panes) = at(Duration::from_millis(1), cx);
        assert!(
            lowest < lower && panes == 2,
            "lowest: {lowest}, {panes} panes"
        );
        let (rising, panes) = at(settle / 4, cx);
        assert!(
            lowest < rising && rising < rest && panes == 2,
            "rising: {rising}"
        );
        assert_eq!(at(settle, cx), (rest, 2), "back at rest");

        // The keyboard stayed in the left pane, so two columns again leave
        // every pane where it is.
        let before = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").all_panes(cx)
        });
        cx.simulate_keystrokes("secondary-alt-2");
        draw(cx);
        assert_eq!(
            at(settle / 4, cx),
            (rest, 2),
            "nothing changes, nothing fades"
        );
        assert_eq!(
            shell.read_with(cx, |shell, cx| {
                shell.attachment.as_ref().expect("attached").all_panes(cx)
            }),
            before
        );

        // Falling toward four panes, one beside two is chosen instead.
        cx.simulate_keystrokes("secondary-alt-4");
        draw(cx);
        let (falling, _) = at(settle / 4, cx);
        cx.simulate_keystrokes("secondary-alt-3");
        assert_eq!(
            column_scene(&shell, cx),
            (falling, 2),
            "no jump as it goes on"
        );
        let (lowest, panes) = at(settle / 4, cx);
        assert!(lowest < falling && panes == 3, "{lowest}, {panes} panes");

        // Rising, one pane is chosen.
        let (rising, _) = at(settle / 4, cx);
        cx.simulate_keystrokes("secondary-alt-1");
        assert_eq!(column_scene(&shell, cx), (rising, 3), "no jump as it turns");
        let (turned, panes) = at(settle / 10, cx);
        assert!(turned < rising && panes == 3, "turned back down: {turned}");
        let (_, panes) = at(settle, cx);
        assert_eq!(panes, 1);
        assert_eq!(at(settle, cx), (rest, 1), "back at rest");
    }

    /// ⇧⌘↩ grows the right pane out of its slot over the column, the left
    /// pane still drawn where it was beneath it, taking neither the pointer
    /// nor the keyboard; zoomed, the pane fills the column. ⇧⌘↩ again
    /// shrinks it back into its slot.
    #[gpui::test]
    fn a_zoomed_pane_grows_over_the_column_and_back_into_its_slot(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        let painted = |cx: &mut VisualTestContext| {
            frame(cx, |_, cx| {
                let attachment = shell.read(cx).attachment.as_ref().expect("attached");
                let panes: Vec<_> = attachment
                    .all_panes(cx)
                    .iter()
                    .map(|pane| pane.read(cx).bounds())
                    .collect();
                (attachment.thread_panes.read(cx).bounds(), panes)
            })
        };
        let (column, slots) = painted(cx);
        let between = |zoomed: gpui::Bounds<Pixels>| {
            zoomed.intersect(&column) == zoomed
                && slots[1].intersect(&zoomed) == slots[1]
                && slots[1].size.width + px(1.) < zoomed.size.width
                && zoomed.size.width < column.size.width - px(1.)
        };
        let settle = crate::edge_back::SETTLE;

        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        cx.executor().advance_clock(settle / 2);
        let (_, panes) = painted(cx);
        let zoomed = cx
            .debug_bounds("zoomed-pane")
            .expect("drawn over the column");
        assert!(
            between(zoomed),
            "growing: {zoomed:?} from {slots:?} to {column:?}"
        );
        assert_eq!(panes, [slots[0], zoomed], "the left pane where it was");
        assert_eq!(keyboard_pane(&shell, cx), Some(1), "the zoomed pane's keys");
        let beneath = cx.update(|window, _| {
            let left = slots[0].scale(window.scale_factor());
            window.painted_quads().iter().any(|quad| {
                quad.bounds.intersect(&left) == quad.bounds
                    && quad.background.as_solid().is_some_and(|fill| fill.a > 0.)
            })
        });
        assert!(beneath, "the left pane is still painted beneath");
        let uncovered = gpui::point((column.left() + zoomed.left()) / 2., column.center().y);
        cx.simulate_click(uncovered, gpui::Modifiers::default());
        draw(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(1), "the click is swallowed");
        cx.simulate_keystrokes("secondary-alt-left");
        draw(cx);
        assert_eq!(
            keyboard_pane(&shell, cx),
            Some(1),
            "nor does it take the keys"
        );

        cx.executor().advance_clock(settle);
        let (_, panes) = painted(cx);
        assert_eq!(cx.debug_bounds("zoomed-pane"), Some(column));
        assert_eq!(panes, [slots[0], column], "zoomed, it fills the column");

        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        cx.executor().advance_clock(settle / 2);
        draw(cx);
        let zoomed = cx
            .debug_bounds("zoomed-pane")
            .expect("still over the column");
        assert!(between(zoomed), "shrinking: {zoomed:?}");
        cx.executor().advance_clock(settle);
        let (_, panes) = painted(cx);
        assert!(cx.debug_bounds("zoomed-pane").is_none());
        assert_eq!(panes, slots, "both back in their slots");
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
    }

    /// Tab and ⇧Tab in a zoomed pane keep the keyboard in it: the panes it
    /// covers are still laid out, tab stops and all, but never take it. Back
    /// in its slot the pane keeps the keyboard, and Tab leaves it again.
    #[gpui::test]
    fn tabbing_in_a_zoomed_pane_never_reaches_the_panes_it_covers(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        for key in ["tab", "shift-tab"] {
            for press in 0..30 {
                cx.simulate_keystrokes(key);
                draw(cx);
                assert_eq!(keyboard_pane(&shell, cx), Some(1), "{key} #{press}");
            }
        }

        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert!(cx.debug_bounds("zoomed-pane").is_none());
        assert_eq!(
            keyboard_pane(&shell, cx),
            Some(1),
            "still in the pane, now in its slot"
        );
        let reached = (0..60).any(|_| {
            cx.simulate_keystrokes("tab");
            draw(cx);
            keyboard_pane(&shell, cx) == Some(0)
        });
        assert!(
            reached,
            "back in its slot, Tab reaches the other pane again"
        );
    }

    /// ⇧⌘↩ again halfway through a zoom turns it back from where it is: the
    /// pane does not jump, and it shrinks back into its slot.
    #[gpui::test]
    fn a_zoom_turned_back_midway_shrinks_from_where_it_is(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        let slots: Vec<_> = shell.read_with(cx, |shell, cx| {
            let panes = shell.attachment.as_ref().expect("attached").all_panes(cx);
            panes.iter().map(|pane| pane.read(cx).bounds()).collect()
        });
        let settle = crate::edge_back::SETTLE;
        let frame = std::time::Duration::from_millis(16);
        let close = |a: gpui::Bounds<Pixels>, b: gpui::Bounds<Pixels>| {
            (a.left() - b.left()).abs() <= px(1.)
                && (a.top() - b.top()).abs() <= px(1.)
                && (a.right() - b.right()).abs() <= px(1.)
                && (a.bottom() - b.bottom()).abs() <= px(1.)
        };

        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        cx.executor().advance_clock(settle / 2);
        draw(cx);
        let before = cx.debug_bounds("zoomed-pane").expect("growing");
        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        let turned = cx.debug_bounds("zoomed-pane").expect("turning back");
        assert!(close(before, turned), "{before:?} then {turned:?}");
        cx.executor().advance_clock(frame);
        draw(cx);
        let after = cx.debug_bounds("zoomed-pane").expect("shrinking");
        assert!(
            after.size.width < turned.size.width,
            "it shrinks from there: {turned:?} then {after:?}"
        );

        cx.executor().advance_clock(settle);
        draw(cx);
        assert!(cx.debug_bounds("zoomed-pane").is_none());
        let panes: Vec<_> = shell.read_with(cx, |shell, cx| {
            let panes = shell.attachment.as_ref().expect("attached").all_panes(cx);
            panes.iter().map(|pane| pane.read(cx).bounds()).collect()
        });
        assert_eq!(panes, slots, "back in its slot");
    }

    /// Splitting a thread puts a draft beside it, in the thread's place in
    /// the store's focus, while the thread stays open in its own pane.
    #[gpui::test]
    fn splitting_a_thread_shows_a_draft_beside_it(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        draw(cx);
        let _ = sent(&host);

        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
        assert!(cx.debug_bounds("waiting-pane").is_some());
        // The new pane grows out of nothing beside the one it halves.
        let (column, widths) = pane_widths(&shell, cx);
        assert!(
            widths[1] < px(1.) && (widths[0] - column).abs() < px(1.),
            "{widths:?} of {column:?}"
        );
        answer_draft(&shell, &host, "draft-1", cx);
        // The hint fades out under the draft's chat, which has the keyboard
        // from the start of the fade.
        assert!(cx.debug_bounds("waiting-pane").is_some());
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let (column, widths) = pane_widths(&shell, cx);
        assert!(
            px(0.) < widths[1]
                && widths[1] < column / 2.
                && column / 2. < widths[0]
                && widths[0] < column,
            "halfway: {widths:?} of {column:?}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        let (column, widths) = pane_widths(&shell, cx);
        assert!(
            widths
                .iter()
                .all(|width| (*width - column / 2.).abs() < px(1.)),
            "landed: {widths:?} of {column:?}"
        );
        assert!(cx.debug_bounds("waiting-pane").is_none());
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("draft-1".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        store.read_with(cx, |store, _| {
            assert!(store.is_thread_open("draft-1"));
            assert!(store.is_thread_open("thread-1"));
        });

        cx.simulate_keystrokes("secondary-alt-left");
        draw(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.page_thread(cx)),
            Some("thread-1".into())
        );
    }

    /// The keyboard leaving a new pane before its draft is answered drops the
    /// draft: the user has moved on, and it would land in a pane left behind.
    #[gpui::test]
    fn leaving_a_new_pane_drops_its_draft(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        cx.simulate_keystrokes("secondary-alt-left");
        draw(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));

        ack_draft(&host, "draft-1");
        cx.run_until_parked();
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
    }

    /// Beside a draft no draft is asked for, since the host would answer with
    /// that same draft: the new pane waits with the keyboard, and the thread
    /// chosen next from the threads list fills it.
    #[gpui::test]
    fn splitting_a_draft_leaves_the_new_pane_waiting(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        serve_status(&host, "thread-1", true);
        await_restore_update(&shell, cx, |store, cx| {
            store
                .session_status_for(Some("thread-1"), cx)
                .is_some_and(|status| status.draft)
        });
        draw(cx);
        let _ = sent(&host);

        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        assert!(!sent(&host).iter().any(|payload| matches!(
            payload,
            ClientPayload::Command(Command::StartDraft { .. } | Command::StartScratchDraft)
        )));
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        assert!(cx.debug_bounds("waiting-pane").is_some());
        // The draft keeps its layout while its pane halves, and takes the
        // half once the glide lands: its header, the only one (a waiting
        // pane draws none), ends inside it.
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        let draft = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").all_panes(cx)[0]
                .read(cx)
                .bounds()
        });
        let menu = cx.debug_bounds("pane-menu").expect("the draft's pane menu");
        assert!(menu.right() <= draft.right(), "{menu:?} in {draft:?}");

        cx.simulate_keystrokes("secondary-2");
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-2".into())]
        );
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-2"));
            assert!(store.is_thread_open("thread-1"));
        });
    }

    /// A pane waiting for a thread has no chat header, so no pane menu: its
    /// corner button closes it, as ⇧⌘W and Escape do with the keyboard in it.
    #[gpui::test]
    fn a_waiting_pane_closes_from_its_corner_and_the_keyboard(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        serve_status(&host, "thread-1", true);
        await_restore_update(&shell, cx, |store, cx| {
            store
                .session_status_for(Some("thread-1"), cx)
                .is_some_and(|status| status.draft)
        });
        draw(cx);
        let split = |cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("secondary-d");
            draw(cx);
            cx.executor().advance_clock(crate::edge_back::SETTLE);
            draw(cx);
            assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
            assert_eq!(keyboard_pane(&shell, cx), Some(1));
        };
        let closed = |cx: &mut VisualTestContext, how: &str| {
            cx.executor().advance_clock(crate::edge_back::SETTLE);
            draw(cx);
            assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())], "{how}");
        };

        split(cx);
        let close = cx
            .debug_bounds("waiting-pane-close")
            .expect("the waiting pane's close button");
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        draw(cx);
        closed(cx, "corner button");
        assert!(cx.debug_bounds("waiting-pane-close").is_none());

        split(cx);
        cx.simulate_keystrokes("secondary-shift-w");
        draw(cx);
        closed(cx, "secondary-shift-w");

        split(cx);
        cx.simulate_keystrokes("escape");
        draw(cx);
        closed(cx, "escape");
    }

    /// Three panes, the keyboard in the top right one: ⌥⌘W glides the other
    /// two shut while the keyboard stays, and their threads close, still
    /// listed.
    #[gpui::test]
    fn closing_other_panes_leaves_the_focused_one(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        cx.simulate_keystrokes("secondary-shift-d");
        draw(cx);
        answer_draft(&shell, &host, "draft-1", cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        cx.simulate_keystrokes("secondary-alt-up");
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [
                Some("thread-1".into()),
                Some("thread-2".into()),
                Some("draft-1".into())
            ]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        let (column, start) = pane_widths(&shell, cx);

        cx.simulate_keystrokes("secondary-alt-w");
        draw(cx);
        assert_eq!(pane_threads(&shell, cx).len(), 3, "they glide shut first");
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let (_, halfway) = pane_widths(&shell, cx);
        assert!(
            start[1] < halfway[1] && halfway[1] < column,
            "{start:?} then {halfway:?} of {column:?}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-2"));
            assert!(!store.is_thread_open("thread-1"));
            assert!(!store.is_thread_open("draft-1"));
        });
        assert!(cx.debug_bounds("sidebar-thread-thread-1").is_some());
    }

    /// A thread the client still holds, opened again in a pane, waits on its
    /// skeleton for the host to catch it up, drawn unseen beneath it. A
    /// click in that pane gives it the keyboard, and what is typed once the
    /// host has answered goes there.
    #[gpui::test]
    fn a_click_on_a_pane_catching_up_gives_it_the_keyboard(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        let click = |at: gpui::Point<Pixels>, modifiers, cx: &mut VisualTestContext| {
            cx.simulate_mouse_move(at, None, modifiers);
            draw(cx);
            cx.simulate_click(at, modifiers);
            settle_panes(cx);
        };
        let row = |id: &'static str, cx: &mut VisualTestContext| {
            cx.debug_bounds(id).expect("the thread's row").center()
        };
        // thread-2 in thread-1's place, so thread-1 is left and kept.
        click(row("sidebar-thread-thread-2", cx), Default::default(), cx);
        serve_thread(&host, "thread-2");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        settle_panes(cx);
        let alt = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        click(row("sidebar-thread-thread-1", cx), alt, cx);
        cx.simulate_keystrokes("secondary-alt-left");
        settle_panes(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-2".into()), Some("thread-1".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        store.read_with(cx, |store, cx| {
            assert!(!store.chat_loading_for(Some("thread-1"), cx));
            assert!(!store.timeline_settled_for("thread-1"), "it is catching up");
        });

        let right = pane_bounds(&shell, cx)[1];
        click(right.center(), Default::default(), cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.page_thread(cx)),
            Some("thread-1".into())
        );

        serve_thread(&host, "thread-1");
        await_restore_update(&shell, cx, |store, _| {
            store.timeline_settled_for("thread-1")
        });
        settle_panes(cx);
        cx.simulate_input("typed as it caught up");
        draw(cx);
        let drafts: Vec<_> = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .all_panes(cx)
                .iter()
                .map(|pane| pane.read(cx).chat().read(cx).composer().read(cx).draft(cx))
                .collect()
        });
        assert_eq!(drafts, ["", "typed as it caught up"]);
    }

    /// A click in a pane whose thread is still loading gives it the keyboard,
    /// so what is typed next goes there once it has loaded, not to the pane
    /// that had the keyboard before.
    #[gpui::test]
    fn a_click_on_a_loading_pane_gives_it_the_keyboard(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        let row = cx
            .debug_bounds("sidebar-thread-thread-2")
            .expect("the thread's row");
        let alt = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        cx.simulate_mouse_move(row.center(), None, alt);
        draw(cx);
        cx.simulate_click(row.center(), alt);
        settle_panes(cx);
        cx.simulate_keystrokes("secondary-alt-left");
        settle_panes(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        assert!(
            store.read_with(cx, |store, cx| store.chat_loading_for(Some("thread-2"), cx)),
            "the right pane's thread has not been answered"
        );

        let right = pane_bounds(&shell, cx)[1];
        cx.simulate_mouse_move(right.center(), None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_click(right.center(), gpui::Modifiers::default());
        settle_panes(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.page_thread(cx)),
            Some("thread-2".into())
        );

        serve_thread(&host, "thread-2");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        settle_panes(cx);
        cx.simulate_input("typed as it loaded");
        draw(cx);
        let drafts: Vec<_> = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .all_panes(cx)
                .iter()
                .map(|pane| pane.read(cx).chat().read(cx).composer().read(cx).draft(cx))
                .collect()
        });
        assert_eq!(drafts, ["", "typed as it loaded"]);
    }

    /// [`mount_panes`]' `thread-1` on the left and `thread-2`, answered by
    /// `serve`, on the right, the keyboard on the left.
    fn side_by_side(shell: &Entity<AppShell>, serve: impl FnOnce(), cx: &mut VisualTestContext) {
        let row = cx
            .debug_bounds("sidebar-thread-thread-2")
            .expect("the thread's row");
        let alt = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        cx.simulate_mouse_move(row.center(), None, alt);
        draw(cx);
        cx.simulate_click(row.center(), alt);
        settle_panes(cx);
        serve();
        await_restore_update(shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        settle_panes(cx);
        cx.simulate_keystrokes("secondary-alt-left");
        settle_panes(cx);
        assert_eq!(
            pane_threads(shell, cx),
            [Some("thread-1".into()), Some("thread-2".into())]
        );
        assert_eq!(keyboard_pane(shell, cx), Some(0));
    }

    /// Press and release at `at`, a frame between the two as on screen.
    fn press_with_a_frame_between(at: gpui::Point<Pixels>, cx: &mut VisualTestContext) {
        cx.simulate_mouse_move(at, None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_mouse_down(at, MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_mouse_up(at, MouseButton::Left, gpui::Modifiers::default());
        settle_panes(cx);
    }

    /// The panel `session_id`'s conversation has recorded, if it is open.
    fn panel_of(
        store: &Entity<WorkspaceStore>,
        session_id: &str,
        cx: &VisualTestContext,
    ) -> Option<RightTab> {
        store.read_with(cx, |store, _| {
            let panel = store.panel_state_for(Some(session_id));
            panel.right_panel_open.then_some(panel.right_tab)
        })
    }

    /// A panel toggle in the header of the pane without the keyboard acts on
    /// that pane's thread. Neither the press nor the release moves the
    /// keyboard; the release opens the thread's panel docked in its own pane.
    /// Each toggle closes only the panel of its own thread.
    #[gpui::test]
    fn a_header_toggle_acts_on_the_thread_of_its_own_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        side_by_side(
            &shell,
            || serve_spawning(&host, "thread-2", &["spawn-1"]),
            cx,
        );
        let bounds = |selector: &'static str, cx: &mut VisualTestContext| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("no {selector}"))
        };

        let agents = bounds("agents-panel", cx);
        assert!(pane_bounds(&shell, cx)[1].contains(&agents.center()));
        cx.simulate_mouse_move(agents.center(), None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_mouse_down(
            agents.center(),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        draw(cx);
        assert_eq!(
            keyboard_pane(&shell, cx),
            Some(0),
            "the press moves nothing"
        );
        assert_eq!(
            panel_of(&store, "thread-2", cx),
            None,
            "it opens on release"
        );
        cx.simulate_mouse_up(
            agents.center(),
            MouseButton::Left,
            gpui::Modifiers::default(),
        );
        settle_panes(cx);
        assert_eq!(panel_of(&store, "thread-2", cx), Some(RightTab::Agents));
        assert_eq!(panel_of(&store, "thread-1", cx), None);
        assert_eq!(keyboard_pane(&shell, cx), Some(0), "the panel leaves it");
        assert!(
            bounds("right-panel", cx).contains(&bounds("agent-row-spawn-1", cx).center()),
            "the panel lists the agents of the thread whose toggle was pressed"
        );

        let close = bounds("diff-close", cx);
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        settle_panes(cx);
        assert_eq!(panel_of(&store, "thread-2", cx), None);
        assert_eq!(keyboard_pane(&shell, cx), Some(0), "still where it was");

        let diff = bounds("diff-panel", cx);
        press_with_a_frame_between(diff.center(), cx);
        assert_eq!(panel_of(&store, "thread-2", cx), Some(RightTab::Diff));
        assert_eq!(panel_of(&store, "thread-1", cx), None);

        // The left pane's toggle, the same distance from its pane's end,
        // opens the left thread's drawer and leaves the right thread's.
        let panes = pane_bounds(&shell, cx);
        let at = gpui::point(
            panes[0].right() - (panes[1].right() - diff.center().x),
            diff.center().y,
        );
        press_with_a_frame_between(at, cx);
        assert_eq!(panel_of(&store, "thread-1", cx), Some(RightTab::Diff));
        assert_eq!(panel_of(&store, "thread-2", cx), Some(RightTab::Diff));
    }

    /// A file in the card of an agent beside its lead opens the agent's diff
    /// in the drawer of the lead's pane, which says whose it is, and the
    /// agent stays beside its lead. The drawer closed, the lead's header's
    /// Diff toggle shows the lead's diff. The agent's card closing takes its
    /// diff with it, and the lead's panel does not take its place.
    #[gpui::test]
    fn a_changed_file_in_an_agents_card_opens_the_agents_diff(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        let store = store_of(&shell, cx);
        serve_changed_turn(&host, "thread-1", "lead.rs");
        await_restore_update(&shell, cx, |store, cx| {
            store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx)
                == Some(1)
        });
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", RightTab::Agents, cx)
        });
        settle_panes(cx);
        open_agent_from_panel(&shell, &host, cx);
        serve_changed_turn(&host, "agent-1", "agent.rs");
        serve_changed_turn(&host, "agent-1", "later.rs");
        await_restore_update(&shell, cx, |store, cx| {
            store.with_timeline_for(Some("agent-1"), |timeline| timeline.turns.len(), cx) == Some(2)
        });
        settle_panes(cx);

        let shows_agents_file = |cx: &mut VisualTestContext| {
            press_control("changed-file-chip-file-agent.rs", cx);
            draw_until(&shell, cx, &host, "diff-line-new-agent.rs-1", None);
            assert_eq!(cx.debug_bounds("diff-line-new-lead.rs-1"), None);
            assert!(cx.debug_bounds("diff-subject").is_some());
            assert_eq!(panel_of(&store, "agent-1", cx), Some(RightTab::Diff));
            store.read_with(cx, |store, _| {
                assert_eq!(
                    store.side_thread_for("thread-1").as_deref(),
                    Some("agent-1")
                );
            });
            let drawer = cx.debug_bounds("right-panel").expect("the drawer");
            assert_eq!(drawer.right(), pane_bounds(&shell, cx)[0].right());
        };
        let shows_leads_diff = |cx: &mut VisualTestContext| {
            draw_until(&shell, cx, &host, "diff-line-new-lead.rs-1", None);
            assert_eq!(cx.debug_bounds("diff-line-new-agent.rs-1"), None);
            assert_eq!(cx.debug_bounds("diff-subject"), None);
        };

        shows_agents_file(cx);
        press_control("diff-close", cx);
        assert_eq!(panel_of(&store, "agent-1", cx), None);
        assert_eq!(cx.debug_bounds("right-panel"), None);
        assert!(cx.debug_bounds("chat-side-header").is_some(), "it stays");
        press_control("view-diff-1", cx);
        draw_until(&shell, cx, &host, "diff-line-new-later.rs-1", None);
        press_control("diff-close", cx);
        press_control("diff-panel", cx);
        shows_leads_diff(cx);
        press_control("diff-close", cx);

        shows_agents_file(cx);
        press_control("side-close", cx);
        assert_eq!(cx.debug_bounds("chat-side-header"), None);
        assert_eq!(cx.debug_bounds("right-panel"), None, "nothing in its place");
        assert_eq!(panel_of(&store, "thread-1", cx), None);
        press_control("diff-panel", cx);
        shows_leads_diff(cx);
    }

    /// On a compact agent's page a changed file opens the agent's diff on the
    /// panel page over it: there is no pane beside a lead to show it in, and
    /// the page's chat is the agent's.
    #[gpui::test]
    fn a_changed_file_on_a_compact_agents_page_opens_the_agents_diff(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        as_mobile(cx);
        let store = store_of(&shell, cx);
        serve_changed_turn(&host, "thread-1", "lead.rs");
        resize(cx, 600.);
        settle_panes(cx);
        cx.update(|window, cx| {
            crate::agents_panel::open_agent(&store, "agent-1".into(), None, None, window, cx)
        });
        draw(cx);
        serve_thread(&host, "agent-1");
        serve_changed_turn(&host, "agent-1", "agent.rs");
        await_restore_update(&shell, cx, |store, cx| {
            store.with_timeline_for(Some("agent-1"), |timeline| timeline.turns.len(), cx) == Some(1)
        });
        settle_panes(cx);

        press_control("changed-file-chip-file-agent.rs", cx);
        draw_until(&shell, cx, &host, "diff-line-new-agent.rs-1", None);
        assert_eq!(cx.debug_bounds("diff-line-new-lead.rs-1"), None);
        assert_eq!(panel_of(&store, "agent-1", cx), Some(RightTab::Diff));
        assert_eq!(panel_of(&store, "thread-1", cx), None);
    }

    /// Two panes each have a panel. Diff opened from the left pane's header
    /// docks inside that pane, its conversation yielding to it and the right
    /// pane untouched; Plan opened in the right pane too docks there, each
    /// with its own thread's content. A thread moved to the other pane takes
    /// its panel along.
    #[gpui::test]
    fn each_pane_has_a_drawer_of_its_own(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        serve_changed_turn(&host, "thread-1", "left.rs");
        side_by_side(
            &shell,
            || {
                serve_thread(&host, "thread-2");
                send_events(
                    &host,
                    vec![(
                        Topic::SessionPlan {
                            session_id: "thread-2".into(),
                        },
                        ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                            session_id: "thread-2".into(),
                            steps: vec![agent::PlanStep {
                                step: "Right".into(),
                                status: agent::PlanStepStatus::Pending,
                            }],
                            explanation: None,
                        }),
                    )],
                );
            },
            cx,
        );
        settle_panes(cx);
        let panes = pane_bounds(&shell, cx);
        let inside = |pane: gpui::Bounds<Pixels>, bounds: gpui::Bounds<Pixels>| {
            pane.left() <= bounds.left()
                && bounds.right() <= pane.right()
                && pane.top() <= bounds.top()
                && bounds.bottom() <= pane.bottom()
        };

        let right_diff = cx.debug_bounds("diff-panel").expect("the Diff toggle");
        assert!(panes[1].contains(&right_diff.center()));
        let left_diff = gpui::point(
            panes[0].right() - (panes[1].right() - right_diff.center().x),
            right_diff.center().y,
        );
        cx.simulate_mouse_move(left_diff, None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_click(left_diff, gpui::Modifiers::default());
        settle_panes(cx);
        draw_until(&shell, cx, &host, "diff-line-new-left.rs-1", None);
        let conversations = |cx: &mut VisualTestContext| {
            shell.read_with(cx, |shell, cx| {
                let attachment = shell.attachment.as_ref().expect("attached");
                attachment
                    .all_panes(cx)
                    .iter()
                    .map(|pane| pane.read(cx).chat_drawn(cx))
                    .collect::<Vec<_>>()
            })
        };
        let drawer = cx.debug_bounds("right-panel").expect("the left drawer");
        assert!(inside(panes[0], drawer), "{drawer:?} in {panes:?}");
        assert_eq!(drawer.right(), panes[0].right());
        assert_eq!(panel_of(&store, "thread-2", cx), None);
        assert_eq!(pane_bounds(&shell, cx), panes);
        let [left, right] = conversations(cx)[..] else {
            panic!("two panes");
        };
        assert_eq!(left.left(), panes[0].left());
        assert_eq!(left.right(), drawer.left());
        assert_eq!(right, panes[1], "the right pane's conversation is its own");

        // Uncovered, the right pane's own header still takes a click.
        let plan = cx.debug_bounds("plan-panel").expect("the Plan toggle");
        assert!(panes[1].contains(&plan.center()));
        cx.simulate_mouse_move(plan.center(), None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_click(plan.center(), gpui::Modifiers::default());
        settle_panes(cx);
        assert_eq!(panel_of(&store, "thread-1", cx), Some(RightTab::Diff));
        assert_eq!(panel_of(&store, "thread-2", cx), Some(RightTab::Plan));
        let diff = cx
            .debug_bounds("diff-line-new-left.rs-1")
            .expect("the left thread's diff");
        let step = cx
            .debug_bounds("plan-step-0:Right")
            .expect("the right thread's plan");
        assert!(inside(panes[0], diff), "{diff:?} in {panes:?}");
        assert!(inside(panes[1], step), "{step:?} in {panes:?}");
        assert_eq!(pane_bounds(&shell, cx), panes);
        let [both_left, both_right] = conversations(cx)[..] else {
            panic!("two panes");
        };
        assert_eq!(both_left, left, "the left keeps what it had");
        assert_eq!(both_right.left(), panes[1].left());
        assert!(both_right.right() <= step.left(), "{both_right:?}");
        assert_eq!(both_right.size.width, left.size.width);

        // The keyboard stayed in the left pane, which carries its thread over.
        cx.simulate_keystrokes("secondary-alt-shift-right");
        settle_panes(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-2".into()), Some("thread-1".into())]
        );
        draw_until(&shell, cx, &host, "diff-line-new-left.rs-1", None);
        let diff = cx
            .debug_bounds("diff-line-new-left.rs-1")
            .expect("the left thread's diff");
        let step = cx
            .debug_bounds("plan-step-0:Right")
            .expect("the right thread's plan");
        assert!(inside(panes[1], diff), "{diff:?} in {panes:?}");
        assert!(inside(panes[0], step), "{step:?} in {panes:?}");
    }

    /// The drawer's width is one preference: dragged in one thread's drawer,
    /// it is the width another thread's drawer opens at, in another pane too.
    #[gpui::test]
    fn a_width_dragged_in_one_drawer_is_every_drawers(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        side_by_side(&shell, || serve_thread(&host, "thread-2"), cx);
        resize(cx, 2600.);
        settle_panes(cx);
        let open = |thread: &str, cx: &mut VisualTestContext| {
            store.update(cx, |store, cx| {
                store.toggle_tab_panel_for(thread, RightTab::Plan, cx)
            });
            settle_panes(cx);
            cx.debug_bounds("right-panel").expect("the drawer")
        };
        let panes = pane_bounds(&shell, cx);
        let first = open("thread-1", cx);
        assert_eq!(first.right(), panes[0].right());
        assert_eq!(first.size.width, px(560.));

        // As a pointer does: the drag starts on the first move and moves the
        // handle on the next, each with its frame.
        let from = gpui::point(first.left(), px(400.));
        cx.simulate_mouse_move(from, None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        let to = from + gpui::point(px(60.), px(0.));
        for step in [from + gpui::point(px(5.), px(0.)), to] {
            cx.simulate_mouse_move(step, Some(MouseButton::Left), gpui::Modifiers::default());
            draw(cx);
        }
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        settle_panes(cx);
        let chosen = cx
            .debug_bounds("right-panel")
            .expect("the drawer")
            .size
            .width;
        assert!(chosen < first.size.width, "the edge shrinks the panel");

        store.update(cx, |store, cx| {
            store.close_right_panel_for(Some("thread-1"), cx)
        });
        settle_panes(cx);
        let other = open("thread-2", cx);
        assert_eq!(other.right(), panes[1].right(), "the other pane's drawer");
        assert_eq!(other.size.width, chosen);
    }

    /// A pane too narrow for its panel, zoomed, has room for both: the panel
    /// stays inside the zoomed pane, which covers the column, docked beside
    /// its conversation.
    #[gpui::test]
    fn a_zoomed_panes_drawer_stays_inside_it(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        side_by_side(&shell, || serve_thread(&host, "thread-2"), cx);
        resize(cx, 1440.);
        settle_panes(cx);
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", RightTab::Plan, cx)
        });
        crate::test_frames::draw_settled(cx);
        let slot = pane_bounds(&shell, cx)[0];
        assert_eq!(cx.debug_bounds("right-panel"), Some(slot), "it covers");
        cx.simulate_keystrokes("secondary-shift-enter");
        crate::test_frames::draw_settled(cx);
        let zoomed = pane_bounds(&shell, cx)[0];
        assert!(
            zoomed.size.width > slot.size.width,
            "{zoomed:?} from {slot:?}"
        );
        let drawer = cx.debug_bounds("right-panel").expect("the drawer");
        assert_eq!(drawer.right(), zoomed.right());
        assert!(zoomed.left() < drawer.left());
        let conversation = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").all_panes(cx)[0]
                .read(cx)
                .chat_drawn(cx)
        });
        assert_eq!(conversation.right(), drawer.left(), "docked");
    }

    /// A file listed in the rail of the pane without the keyboard opens that
    /// thread's diff, also on a tap released before the next frame: the pane
    /// hears the keyboard enter it only once a frame is drawn, so the store
    /// has not moved yet.
    #[gpui::test]
    fn a_changed_file_opens_the_diff_of_the_thread_of_its_own_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        resize(cx, 3200.);
        side_by_side(
            &shell,
            || {
                let records: Vec<tcode_core::session::StoredEvent> = [
                    agent::AgentEvent::TurnStarted {
                        turn_id: "turn-1".into(),
                    },
                    agent::AgentEvent::TurnChangesUpdated {
                        turn_id: "turn-1".into(),
                        changes: agent::file_changes_from_unified_diff(
                            "diff --git a/src/lib.rs b/src/lib.rs\n\
                             --- a/src/lib.rs\n+++ b/src/lib.rs\n\
                             @@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n",
                        )
                        .expect("one file section"),
                        completeness: agent::ChangeCompleteness::Exact,
                    },
                    agent::AgentEvent::TurnCompleted {
                        turn_id: "turn-1".into(),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    },
                ]
                .into_iter()
                .map(Into::into)
                .collect();
                serve_status(&host, "thread-2", false);
                send_events(
                    &host,
                    vec![(
                        Topic::SessionEvents {
                            session_id: "thread-2".into(),
                        },
                        ServerEvent::SessionSnapshot {
                            total: records.len() as u64,
                            total_turns: 1,
                            truncated: false,
                            from: 0,
                            end: records.len() as u64,
                            records,
                        },
                    )],
                );
            },
            cx,
        );
        settle_frames(cx);
        let row = cx
            .debug_bounds("changed-file-row-file-src/lib.rs")
            .expect("the file's row in the rail");
        assert!(pane_bounds(&shell, cx)[1].contains(&row.center()));

        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::default());
        draw(cx);
        let press = gpui::MouseDownEvent {
            button: MouseButton::Left,
            position: row.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        };
        let release = gpui::MouseUpEvent {
            button: MouseButton::Left,
            position: row.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
        };
        cx.update(|window, cx| {
            window.dispatch_event(gpui::PlatformInput::MouseDown(press), cx);
            window.dispatch_event(gpui::PlatformInput::MouseUp(release), cx);
        });
        store.read_with(cx, |store, cx| {
            assert!(store.is_thread_open("thread-2"));
            assert_eq!(store.diff_selected_turn_for(Some("thread-2"), cx), Some(0));
        });
        assert_eq!(panel_of(&store, "thread-2", cx), Some(RightTab::Diff));
        assert_eq!(panel_of(&store, "thread-1", cx), None);
        settle_panes(cx);
        assert_eq!(
            keyboard_pane(&shell, cx),
            Some(0),
            "the keyboard stays where it was, not lost with the row"
        );
    }

    /// The way back to an agent's lead in the header of the pane without the
    /// keyboard shows the lead in that pane, not in the one with the keyboard.
    #[gpui::test]
    fn the_way_back_to_a_lead_stays_in_its_own_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
            unreachable!("panes_index sends a snapshot");
        };
        let mut agent = index.sessions[1].clone();
        agent.id = "agent-1".into();
        agent.title = "agent-1".into();
        agent.parent_session_id = Some("thread-2".into());
        index.sessions.push(agent);
        send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
        await_restore_update(&shell, cx, |store, _| {
            store.thread_meta("agent-1").is_some()
        });
        side_by_side(&shell, || serve_thread(&host, "thread-2"), cx);

        cx.simulate_keystrokes("secondary-alt-right");
        settle_panes(cx);
        store.update(cx, |store, cx| store.show_thread("agent-1".into(), cx));
        settle_panes(cx);
        serve_thread(&host, "agent-1");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("agent-1"), cx)
        });
        settle_panes(cx);
        cx.simulate_keystrokes("secondary-alt-left");
        settle_panes(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("agent-1".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(0));

        let link = cx
            .debug_bounds("agent-parent-link")
            .expect("the way back to the lead");
        press_with_a_frame_between(link.center(), cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-2".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
    }

    /// ⇧⌥⌘← exchanges the right pane's thread with the left one's, a
    /// half-written message going along with it, and the keyboard stays in
    /// the right pane, now on the left one's thread.
    #[gpui::test]
    fn swapping_panes_takes_the_thread_and_its_draft_along(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        cx.simulate_input("half a thought");
        draw(cx);

        cx.simulate_keystrokes("secondary-alt-shift-left");
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-2".into()), Some("thread-1".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1), "the keyboard stays");
        let composer = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").all_panes(cx)[0]
                .read(cx)
                .chat()
                .read(cx)
                .composer()
        });
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "half a thought"
        );
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-1"));
            assert!(store.is_thread_open("thread-1"));
            assert!(store.is_thread_open("thread-2"));
        });
        let slots = pane_bounds(&shell, cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        // Each conversation is on its way across to the other pane.
        let leftward = cx.debug_bounds("flying-chat-thread-2").expect("travels");
        let rightward = cx.debug_bounds("flying-chat-thread-1").expect("travels");
        assert!(
            slots[0].left() < leftward.left() && leftward.left() < slots[1].left(),
            "{leftward:?} between {slots:?}"
        );
        assert!(
            slots[0].left() < rightward.left() && rightward.left() < slots[1].left(),
            "{rightward:?} between {slots:?}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(cx.debug_bounds("flying-chat-thread-2"), None, "landed");
        assert_eq!(cx.debug_bounds("flying-chat-thread-1"), None, "landed");
        assert_eq!(pane_bounds(&shell, cx), slots, "the panes held still");
    }

    /// Moving the right pane's thread left trades it with the left pane's:
    /// the moved thread shows in the left pane with the keyboard, the left
    /// pane's thread in the right pane, and nothing closes.
    #[gpui::test]
    fn moving_a_thread_trades_places_with_its_neighbour(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(1));

        let mac = cx.update(|_, cx| cx.platform_info().is_mac());
        let slots = pane_bounds(&shell, cx);
        cx.simulate_keystrokes(if mac { "cmd-ctrl-left" } else { "alt-left" });
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        // The keyboard is with the moved thread while it travels left.
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        let moved = cx.debug_bounds("flying-chat-thread-2").expect("travels");
        assert!(
            slots[0].left() < moved.left() && moved.left() < slots[1].left(),
            "{moved:?} between {slots:?}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(cx.debug_bounds("flying-chat-thread-2"), None, "landed");
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-2".into()), Some("thread-1".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-2"));
            assert!(store.is_thread_open("thread-1"));
            assert!(store.is_thread_open("thread-2"));
        });
    }

    /// A window that jumps to another size (maximised, fullscreen, turned)
    /// glides the seam between its panes from where it was drawn to where
    /// the new size puts it; a window whose edge is dragged keeps its panes'
    /// shares on every frame.
    #[gpui::test]
    fn a_window_jump_glides_the_seams_and_a_dragged_edge_is_followed(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        let share = |cx: &VisualTestContext| {
            let (column, widths) = pane_widths(&shell, cx);
            (widths[0] / column, widths[0])
        };
        let (rest, _) = share(cx);

        for width in [1990., 1975., 1960., 1950.] {
            resize(cx, width);
            let (now, _) = share(cx);
            assert!(
                (now - rest).abs() < 0.005,
                "followed at {width}: {now} of {rest}"
            );
        }

        let (_, was) = share(cx);
        resize(cx, 2600.);
        let (_, start) = share(cx);
        assert!(
            (start - was).abs() < px(2.),
            "kept where it was: {start:?} of {was:?}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let (midway, halfway) = share(cx);
        assert!(
            start < halfway && midway < rest,
            "on its way: {start:?} then {halfway:?}, {midway} of {rest}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        let (landed, _) = share(cx);
        assert!((landed - rest).abs() < 0.005, "{landed} of {rest}");
    }

    /// With the keyboard in a thread's terminal, the pane keys are the
    /// terminal's: the layout's and the split's keys leave the panes alone.
    #[gpui::test]
    fn pane_keys_in_a_terminal_leave_the_panes_alone(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        store_of(&shell, cx).update(cx, |store, cx| {
            store.toggle_terminal_panel_for("thread-1".to_owned(), cx)
        });
        draw(cx);
        let terminal = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .chat(cx)
                .read(cx)
                .terminal_drawer()
                .focus_handle(cx)
        });
        cx.update(|window, cx| window.focus(&terminal, cx));
        draw(cx);
        cx.update(|window, _| {
            assert!(
                window
                    .context_stack()
                    .iter()
                    .any(|context| context.contains(crate::terminal_drawer::CONTEXT)),
                "the terminal has the keyboard"
            )
        });

        cx.simulate_keystrokes("secondary-alt-2 secondary-d");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        cx.update(|window, _| assert!(terminal.is_focused(window)));
    }

    /// The palette's pane commands and the pane menu's rows show the key
    /// that runs them.
    #[gpui::test]
    fn pane_commands_show_their_keys(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        // A test window keeps none of the glyphs it paints (only its quads and
        // underlines), so a hint is read by the width its text was laid out
        // at. Split right's key (secondary-d) is one key shorter than split
        // down's (secondary-shift-d), so each list's split-right hint must be
        // the narrower: a row showing another command's key, or the same key
        // on every row, is caught.
        let palette_hint = |label: &str, cx: &mut VisualTestContext| {
            shell
                .read_with(cx, |shell, _| shell.window_state())
                .update(cx, |state, cx| state.open_palette(cx));
            draw(cx);
            cx.simulate_input(&format!(">{label}"));
            draw(cx);
            let row = cx.debug_bounds("palette-row-0").expect("the command's row");
            let hint = cx
                .debug_bounds("palette-row-0-key")
                .expect("the row shows its key");
            assert!(row.contains(&hint.center()), "{hint:?} in {row:?}");
            cx.simulate_keystrokes("escape");
            cx.executor().advance_clock(crate::edge_back::SETTLE);
            draw(cx);
            hint.size.width
        };
        let right = palette_hint(&crate::tr!("panes.split_right"), cx);
        let down = palette_hint(&crate::tr!("panes.split_down"), cx);
        assert!(
            right < down,
            "palette: split right {right:?}, down {down:?}"
        );

        let button = cx.debug_bounds("pane-menu").expect("the pane menu");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        draw(cx);
        let menu_hint = |row: &'static str, key: &'static str, cx: &mut VisualTestContext| {
            let row = cx.debug_bounds(row).expect("the command's row");
            let hint = cx.debug_bounds(key).expect("the row shows its key");
            assert!(row.contains(&hint.center()), "{hint:?} in {row:?}");
            hint.size.width
        };
        let right = menu_hint("menu-item-0", "menu-item-0-key", cx);
        let down = menu_hint("menu-item-1", "menu-item-1-key", cx);
        assert!(right < down, "menu: split right {right:?}, down {down:?}");
    }

    /// Two columns fit beside the sidebar of a 1200pt window, each header
    /// whole. A split the window has no room for is refused; stacking still
    /// fits.
    #[gpui::test]
    fn a_split_without_room_is_refused(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        resize(cx, 1200.);
        draw(cx);
        let _ = sent(&host);
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        answer_draft(&shell, &host, "draft-1", cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("draft-1".into())]
        );
        let panes: Vec<_> = shell.read_with(cx, |shell, cx| {
            let panes = shell.attachment.as_ref().expect("attached").all_panes(cx);
            panes.iter().map(|pane| pane.read(cx).bounds()).collect()
        });
        for pane in &panes {
            assert!(pane.size.width >= px(PANE_MIN_WIDTH), "{pane:?}");
        }
        let overflow = cx
            .debug_bounds("header-overflow")
            .expect("the narrow header's one menu");
        assert!(
            overflow.right() <= panes[1].right(),
            "the right pane's header ends at {:?}, past its pane at {:?}",
            overflow.right(),
            panes[1].right()
        );
        // Its close button keeps the header's right end, past the menu.
        let close = cx
            .debug_bounds("pane-close")
            .expect("the pane's close button");
        assert!(
            overflow.right() <= close.left() && close.right() <= panes[1].right(),
            "the close button at {close:?}, the menu ending at {:?}, the pane at {:?}",
            overflow.right(),
            panes[1].right()
        );
        cx.simulate_keystrokes("secondary-shift-w");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);

        resize(cx, SIDEBAR_WIDTH + 2. * PANE_MIN_WIDTH - 10.);
        draw(cx);
        // A thread carried from the pane's middle, where it would land, to
        // its right edge, which has no room: the placeholder fades out rather
        // than vanishing, and the drop there does nothing.
        let pane = only_pane(&shell, cx);
        drag_row("sidebar-thread-thread-2", pane.center(), cx);
        assert!(cx.debug_bounds("pane-drop-indicator").is_some());
        let edge = gpui::point(pane.right() - px(20.), pane.center().y);
        cx.simulate_mouse_move(edge, Some(MouseButton::Left), gpui::Modifiers::default());
        draw(cx);
        let painted_alpha = |cx: &mut VisualTestContext| {
            let placeholder = cx
                .debug_bounds("pane-drop-indicator")
                .expect("the placeholder, still drawn while it fades out");
            cx.update(|window, _| {
                window
                    .painted_quads()
                    .iter()
                    .find(|quad| quad.bounds == placeholder.scale(window.scale_factor()))
                    .and_then(|quad| quad.background.as_solid())
                    .expect("the placeholder's fill")
                    .a
            })
        };
        let leaving = painted_alpha(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 3);
        draw(cx);
        let fading = painted_alpha(cx);
        assert!(
            fading < leaving,
            "the placeholder fades: {leaving} then {fading}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert!(cx.debug_bounds("pane-drop-indicator").is_none());
        cx.simulate_mouse_up(edge, MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        let _ = sent(&host);

        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        assert!(
            !sent(&host).iter().any(|payload| matches!(
                payload,
                ClientPayload::Command(Command::StartScratchDraft)
            ))
        );

        cx.simulate_keystrokes("secondary-shift-d");
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
    }

    /// Beside another pane, `thread-2` with a message in it, shown in the
    /// second pane, which has the keyboard.
    fn a_thread_with_a_message_beside(
        cx: &mut TestAppContext,
    ) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        let (shell, host, cx) = mount_panes(cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        let records: Vec<agent::AgentEvent> = vec![
            agent::AgentEvent::TurnStarted {
                turn_id: "turn-0".into(),
            },
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: "hello".into(),
                parent_item_id: None,
                content: agent::ItemContent::UserMessage {
                    text: "Hello there.".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
        ];
        send_events(
            &host,
            vec![(
                Topic::SessionEvents {
                    session_id: "thread-2".into(),
                },
                ServerEvent::SessionSnapshot {
                    total: records.len() as u64,
                    total_turns: 1,
                    truncated: false,
                    from: 0,
                    end: records.len() as u64,
                    records: records.into_iter().map(Into::into).collect(),
                },
            )],
        );
        await_restore_update(&shell, cx, |store, cx| {
            store.with_timeline_for(Some("thread-2"), |timeline| timeline.entries.len(), cx)
                == Some(1)
        });
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("user-bubble-hello").is_some());
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        (shell, host, cx)
    }

    fn holds_thread_2(shell: &Entity<AppShell>, cx: &mut VisualTestContext) -> bool {
        store_of(shell, cx).read_with(cx, |store, cx| {
            store
                .with_timeline_for(Some("thread-2"), |_| (), cx)
                .is_some()
        })
    }

    /// A pane whose thread closes beside others fades out showing that
    /// thread's conversation, not an empty one, while the waiting hint fades
    /// in; once it has faded, the client lets the closed thread's timeline go.
    #[gpui::test]
    fn a_closed_threads_pane_fades_out_showing_its_chat(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = a_thread_with_a_message_beside(cx);

        send_events(&host, vec![panes_index(&["thread-2"])]);
        await_restore_update(&shell, cx, |store, _| !store.is_thread_open("thread-2"));
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        assert!(
            cx.debug_bounds("user-bubble-hello").is_some(),
            "the leaving pane shows its conversation"
        );
        assert!(holds_thread_2(&shell, cx));
        assert!(!store_of(&shell, cx).read_with(cx, |store, _| store.is_thread_open("thread-2")));

        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("user-bubble-hello").is_none());
        assert!(
            !holds_thread_2(&shell, cx),
            "the closed thread's timeline was kept"
        );
    }

    /// A pane taken out while its closed thread still fades lets the
    /// thread's timeline go with it.
    #[gpui::test]
    fn a_pane_taken_out_mid_fade_lets_its_closed_thread_go(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = a_thread_with_a_message_beside(cx);

        send_events(&host, vec![panes_index(&["thread-2"])]);
        await_restore_update(&shell, cx, |store, _| !store.is_thread_open("thread-2"));
        draw(cx);
        assert!(holds_thread_2(&shell, cx));
        let panes = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").all_panes(cx)
        });
        let area = shell.read_with(cx, |shell, _| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .thread_panes
                .clone()
        });
        cx.update(|window, cx| {
            area.update(cx, |area, cx| {
                area.remove_panel(panes[1].clone(), window, cx)
            })
        });
        drop(panes);
        cx.run_until_parked();
        draw(cx);
        assert!(
            !holds_thread_2(&shell, cx),
            "the closed thread's timeline was kept"
        );
    }

    /// A thread archived while another pane has the keyboard takes its pane
    /// with it; the focused thread is left alone.
    #[gpui::test]
    fn archiving_a_background_panes_thread_closes_the_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(1));

        send_events(&host, vec![panes_index(&["thread-1"])]);
        await_restore_update(&shell, cx, |store, _| !store.is_thread_open("thread-1"));
        draw(cx);
        // It glides shut first.
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-2"));
        });
    }

    /// An archived agent leaves its pane however the panes stand: beside
    /// others the pane goes and its lead's pane, which nobody chose, does not
    /// take the keyboard; alone it stays and shows the lead.
    #[gpui::test]
    fn an_archived_agent_gives_way_to_its_lead(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        let store = store_of(&shell, cx);
        let archive_agent = |archived: bool| {
            let (topic, ServerEvent::IndexSnapshot(mut index)) = lead_with_agent_index("thread-1")
            else {
                unreachable!("the index is a snapshot");
            };
            index
                .sessions
                .iter_mut()
                .find(|meta| meta.id == "agent-1")
                .expect("the agent")
                .archived_at = archived.then_some(3);
            (topic, ServerEvent::IndexSnapshot(index))
        };
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(OpenInNewPane("agent-1".into())), cx)
        });
        draw(cx);
        serve_thread(&host, "agent-1");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("agent-1"), cx)
        });
        settle_panes(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("agent-1".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1));

        send_events(&host, vec![archive_agent(true)]);
        await_restore_update(&shell, cx, |store, _| !store.is_thread_open("agent-1"));
        draw(cx);
        assert_ne!(keyboard_pane(&shell, cx), Some(0));
        settle_panes(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);

        send_events(&host, vec![archive_agent(false)]);
        await_restore_update(&shell, cx, |store, _| {
            store
                .sidebar_sessions()
                .iter()
                .any(|meta| meta.id == "agent-1")
        });
        store.update(cx, |store, cx| store.show_thread("agent-1".into(), cx));
        serve_thread(&host, "agent-1");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("agent-1"), cx)
        });
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("agent-1".into())]);

        send_events(&host, vec![archive_agent(true)]);
        await_restore_update(&shell, cx, |store, _| !store.is_thread_open("agent-1"));
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
    }

    /// A split's handle taken while the split glides stops the glide: the
    /// panes stay where the pointer leaves them.
    #[gpui::test]
    fn a_handle_dragged_mid_glide_keeps_where_it_is_left(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 4);
        draw(cx);
        let (column, widths) = pane_widths(&shell, cx);
        assert!(
            widths[0] > column / 2. + px(150.),
            "still gliding: {widths:?}"
        );
        let left = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").all_panes(cx)[0]
                .read(cx)
                .bounds()
        });
        let handle = gpui::point(left.right(), left.center().y);
        let to = handle - gpui::point(px(120.), px(0.));
        let none = gpui::Modifiers::default();
        cx.simulate_mouse_move(handle, None, none);
        draw(cx);
        cx.simulate_mouse_down(handle, MouseButton::Left, none);
        draw(cx);
        for step in [handle - gpui::point(px(40.), px(0.)), to] {
            cx.simulate_mouse_move(step, Some(MouseButton::Left), none);
            draw(cx);
        }
        cx.simulate_mouse_up(to, MouseButton::Left, none);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        let (_, widths) = pane_widths(&shell, cx);
        assert!(
            (widths[0] - (to.x - left.left())).abs() < px(1.),
            "the pane ends where the handle was left: {widths:?}, handle at {to:?}"
        );
    }

    /// A handle dragged on one split leaves the others gliding: a pane
    /// closing in a stack beside it still glides shut.
    #[gpui::test]
    fn another_splits_handle_dragged_lets_a_close_finish(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        answer_draft(&shell, &host, "draft-1", cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        cx.simulate_keystrokes("secondary-shift-d");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("draft-1".into()), None]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(2));
        let bounds = |cx: &mut VisualTestContext| -> Vec<gpui::Bounds<Pixels>> {
            shell.read_with(cx, |shell, cx| {
                let panes = shell.attachment.as_ref().expect("attached").all_panes(cx);
                panes.iter().map(|pane| pane.read(cx).bounds()).collect()
            })
        };

        cx.simulate_keystrokes("secondary-shift-w");
        draw(cx);
        let start = bounds(cx)[2].size.height;
        let left = bounds(cx)[0];
        let handle = gpui::point(left.right(), left.center().y);
        let to = handle - gpui::point(px(120.), px(0.));
        let none = gpui::Modifiers::default();
        cx.simulate_mouse_move(handle, None, none);
        draw(cx);
        cx.simulate_mouse_down(handle, MouseButton::Left, none);
        draw(cx);
        for step in [handle - gpui::point(px(40.), px(0.)), to] {
            cx.simulate_mouse_move(step, Some(MouseButton::Left), none);
            draw(cx);
        }
        cx.simulate_mouse_up(to, MouseButton::Left, none);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        assert_eq!(pane_threads(&shell, cx).len(), 3, "still closing");
        let halfway = bounds(cx)[2].size.height;
        assert!(
            px(0.) < halfway && halfway < start - px(1.),
            "the stack's glide runs on: {start:?} then {halfway:?}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("draft-1".into())]
        );
        let (_, widths) = pane_widths(&shell, cx);
        assert!(
            (widths[0] - (to.x - left.left())).abs() < px(1.),
            "the columns stay where the handle was left: {widths:?}, handle at {to:?}"
        );
    }

    /// A pane closed while it still grows in turns back from where it is
    /// drawn, and the column ends as it was.
    #[gpui::test]
    fn a_pane_closed_while_it_grows_in_turns_back_where_it_is(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 4);
        draw(cx);
        let (column, growing) = pane_widths(&shell, cx);
        assert!(
            px(1.) < growing[1] && growing[1] < column / 2. - px(1.),
            "growing: {growing:?} of {column:?}"
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1));

        cx.simulate_keystrokes("secondary-shift-w");
        draw(cx);
        let (_, turned) = pane_widths(&shell, cx);
        assert!(
            growing
                .iter()
                .zip(&turned)
                .all(|(growing, turned)| (*growing - *turned).abs() < px(1.)),
            "no jump as it turns: {growing:?} then {turned:?}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE / 10);
        draw(cx);
        let (_, shrinking) = pane_widths(&shell, cx);
        assert!(
            shrinking[1] < turned[1],
            "it narrows from there: {turned:?} then {shrinking:?}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        let (column, widths) = pane_widths(&shell, cx);
        assert!(
            widths.len() == 1 && (widths[0] - column).abs() < px(1.),
            "{widths:?} of {column:?}"
        );
    }

    /// A split made while a pane closes divides what the column will be, from
    /// where its panes are drawn: nothing jumps, and the close still lands.
    #[gpui::test]
    fn a_split_made_while_a_pane_closes_glides_on_from_where_it_is(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        answer_draft(&shell, &host, "draft-1", cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        cx.simulate_keystrokes("secondary-alt-left");
        draw(cx);
        cx.simulate_keystrokes("secondary-shift-w");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 4);
        draw(cx);
        let (column, closing) = pane_widths(&shell, cx);
        assert!(
            closing[0] < column / 2. - px(1.) && gpui_base::PANEL_MIN_SIZE < closing[0],
            "closing: {closing:?} of {column:?}"
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1));

        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("draft-1".into()), None]
        );
        let (_, split) = pane_widths(&shell, cx);
        assert!(
            (split[0] - closing[0]).abs() < px(1.)
                && (split[1] - closing[1]).abs() < px(1.)
                && split[2] < px(1.),
            "no jump as it turns: {closing:?} then {split:?}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("draft-1".into()), None]);
        let (column, widths) = pane_widths(&shell, cx);
        assert!(
            widths
                .iter()
                .all(|width| (*width - column / 2.).abs() < px(1.)),
            "landed: {widths:?} of {column:?}"
        );
    }

    /// With motion reduced a split, a zoom, a layout and a close land in the
    /// frame they are made: the panes are at their halves at once, a zoomed
    /// pane fills the column and is back in its slot, a layout is in with
    /// the column at rest, and a closed pane is gone rather than left at the
    /// dock's floor for a frame.
    #[gpui::test]
    fn with_motion_reduced_panes_split_zoom_lay_out_and_close_at_once(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(|cx| cx.set_reduce_motion(true));
        let (shell, host, cx) = mount_panes(cx);
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        let (column, widths) = pane_widths(&shell, cx);
        assert!(
            widths.len() == 2
                && widths
                    .iter()
                    .all(|width| (*width - column / 2.).abs() < px(1.)),
            "split: {widths:?} of {column:?}"
        );
        answer_draft(&shell, &host, "draft-1", cx);
        cx.simulate_keystrokes("secondary-alt-left");
        draw(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));

        let (column, halves) = shell.read_with(cx, |shell, cx| {
            let attachment = shell.attachment.as_ref().expect("attached");
            let panes = attachment.all_panes(cx);
            (
                attachment.thread_panes.read(cx).bounds(),
                panes
                    .iter()
                    .map(|pane| pane.read(cx).bounds())
                    .collect::<Vec<_>>(),
            )
        });
        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        assert_eq!(cx.debug_bounds("zoomed-pane"), Some(column), "zoomed");
        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        assert!(cx.debug_bounds("zoomed-pane").is_none(), "unzoomed");
        let (rest, _) = column_scene(&shell, cx);
        cx.simulate_keystrokes("secondary-alt-3");
        assert_eq!(column_scene(&shell, cx), (rest, 3), "one beside two");
        cx.simulate_keystrokes("secondary-alt-2");
        assert_eq!(column_scene(&shell, cx), (rest, 2), "two columns");
        let panes: Vec<_> = shell.read_with(cx, |shell, cx| {
            let panes = shell.attachment.as_ref().expect("attached").all_panes(cx);
            panes.iter().map(|pane| pane.read(cx).bounds()).collect()
        });
        assert_eq!(panes, halves);

        let closing = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").all_panes(cx)[0].clone()
        });
        let half = closing.read_with(cx, |pane, _| pane.bounds());
        cx.simulate_keystrokes("secondary-shift-w");
        draw(cx);
        // The keystroke's own frame already leaves it out: it is not drawn
        // again, not even at the dock's floor for a frame.
        let last = closing.read_with(cx, |pane, _| pane.bounds());
        assert_eq!(last, half, "where the closed pane was last drawn");
        let (column, widths) = pane_widths(&shell, cx);
        assert!(
            widths.len() == 1 && (widths[0] - column).abs() < px(1.),
            "closed: {widths:?} of {column:?}"
        );
        assert_eq!(pane_threads(&shell, cx), [Some("draft-1".into())]);
    }

    /// Closing a pane closes its thread, which stays listed, and the store
    /// moves on to the thread of the pane the keyboard lands in.
    #[gpui::test]
    fn closing_a_pane_focuses_its_neighbours_thread(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        answer_draft(&shell, &host, "draft-1", cx);
        cx.simulate_keystrokes("secondary-alt-left");
        draw(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));

        cx.simulate_keystrokes("secondary-shift-w");
        draw(cx);
        // The pane glides shut, inert, the keyboard already in its neighbour.
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("draft-1".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        let (closing, composer) = shell.read_with(cx, |shell, cx| {
            let pane = shell.attachment.as_ref().expect("attached").all_panes(cx)[0].clone();
            let pane = pane.read(cx);
            let composer = pane.chat().read(cx).composer().read(cx);
            (pane.bounds(), composer.input_focus_handle(cx))
        });
        cx.update(|window, _| assert!(!composer.is_focused(window)));
        cx.simulate_click(closing.center(), gpui::Modifiers::default());
        draw(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(1), "the click is swallowed");
        let (column, start) = pane_widths(&shell, cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let (_, halfway) = pane_widths(&shell, cx);
        assert!(
            halfway[0] < start[0] && start[1] < halfway[1] && halfway[1] < column,
            "{start:?} then {halfway:?} of {column:?}"
        );
        // Below the dock's slot floor it stops narrowing and fades out
        // instead, gone as it is taken out.
        let floored = |cx: &mut VisualTestContext| {
            let closing = shell.read_with(cx, |shell, cx| {
                shell.attachment.as_ref().expect("attached").all_panes(cx)[0]
                    .read(cx)
                    .bounds()
            });
            assert!(
                (closing.size.width - gpui_base::PANEL_MIN_SIZE).abs() < px(1.),
                "at the floor: {closing:?}"
            );
            cx.update(|window, _| {
                let closing = closing.scale(window.scale_factor());
                window
                    .painted_quads()
                    .iter()
                    .filter(|quad| {
                        let mask = quad.content_mask.bounds;
                        !mask.is_empty() && mask.intersect(&closing) == mask
                    })
                    .filter_map(|quad| quad.background.as_solid())
                    .map(|fill| fill.a)
                    .fold(0., f32::max)
            })
        };
        cx.executor().advance_clock(crate::edge_back::SETTLE / 5);
        draw(cx);
        let fading = floored(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 8);
        draw(cx);
        let faded = floored(cx);
        assert!(
            0. < faded && faded < fading && fading < 1.,
            "the closing pane's content fades: {fading} then {faded}"
        );
        // The frame the glide lands in no longer draws the pane: no strip at
        // the floor, its neighbour has the whole column.
        let strip = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").all_panes(cx)[0]
                .read(cx)
                .bounds()
        });
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        let (landed, (column, widths)) = frame(cx, |window, cx| {
            let strip = strip.scale(window.scale_factor());
            let landed = window
                .painted_quads()
                .iter()
                .filter(|quad| {
                    let mask = quad.content_mask.bounds;
                    !mask.is_empty() && mask.intersect(&strip) == mask
                })
                .filter_map(|quad| quad.background.as_solid())
                .map(|fill| fill.a)
                .fold(0., f32::max);
            (landed, widths_of(shell.read(cx), cx))
        });
        assert!(
            landed == 0. && widths.len() == 1 && (widths[0] - column).abs() < px(1.),
            "landed: {landed} drawn in the strip, {widths:?} of {column:?}"
        );
        assert_eq!(pane_threads(&shell, cx), [Some("draft-1".into())]);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("draft-1"));
            assert!(!store.is_thread_open("thread-1"));
        });
        assert!(cx.debug_bounds("sidebar-thread-thread-1").is_some());

        cx.simulate_keystrokes("secondary-shift-w");
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("draft-1".into())],
            "the last pane stays"
        );
    }

    /// Press `at` and let go elsewhere, as a pointer that changes its mind:
    /// a press with no click.
    fn press_only(at: gpui::Point<Pixels>, cx: &mut VisualTestContext) {
        let none = gpui::Modifiers::default();
        cx.simulate_mouse_down(at, MouseButton::Left, none);
        draw(cx);
        cx.simulate_mouse_up(gpui::point(px(1.), px(1.)), MouseButton::Left, none);
        draw(cx);
    }

    /// Any press inside a pane makes it current, whatever takes the press,
    /// whether it reaches the pane or not, and the keyboard goes only where
    /// that press sends it. The next thread lands there.
    #[gpui::test]
    fn a_press_anywhere_in_a_pane_makes_it_current(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        let (topic, ServerEvent::IndexSnapshot(mut index)) = lead_with_agent_index("thread-1")
        else {
            unreachable!("an index")
        };
        let mut third = index.sessions[1].clone();
        third.id = "thread-3".into();
        third.title = "thread-3".into();
        third.updated_at = 0;
        index.sessions.push(third);
        send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
        await_restore_update(&shell, cx, |store, _| {
            store.thread_meta("thread-3").is_some()
        });
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        let store = store_of(&shell, cx);
        let panes = pane_bounds(&shell, cx);
        let current = |left: bool, what: &str, cx: &mut VisualTestContext| {
            draw(cx);
            let thread = if left { "thread-1" } else { "thread-2" };
            let shown = shell.read_with(cx, |shell, cx| shell.page_thread(cx));
            assert_eq!(shown.as_deref(), Some(thread), "{what}");
        };
        current(false, "the pane it opened in", cx);

        // A blank timeline sends the keyboard along.
        cx.simulate_click(panes[0].center(), gpui::Modifiers::default());
        current(true, "the left timeline", cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));

        // A header control stops the press, and the keyboard stays.
        let toggle = cx
            .debug_bounds("toggle-sidebar")
            .expect("the right header's");
        assert!(
            panes[1].contains(&toggle.center()),
            "{toggle:?} in {panes:?}"
        );
        press_only(toggle.center(), cx);
        current(false, "a control in the right header", cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(0), "the keyboard stays");
        // It leaves the composer and stays where it is pressed from.
        cx.update(|window, cx| window.blur(cx));
        current(false, "the keyboard leaving", cx);

        // The drawer, its pane's own.
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", RightTab::Diff, cx)
        });
        settle_panes(cx);
        current(false, "a drawer opening elsewhere", cx);
        let drawer = cx.debug_bounds("right-panel").expect("the drawer");
        press_only(drawer.center(), cx);
        current(true, "its drawer", cx);
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", RightTab::Diff, cx)
        });
        settle_panes(cx);

        // The card of an agent beside its lead.
        let chat = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().unwrap().panes(cx)[0]
                .read(cx)
                .chat()
        });
        cx.update(|window, cx| {
            crate::agents_panel::open_agent(
                &store,
                "agent-1".into(),
                Some(chat.clone()),
                None,
                window,
                cx,
            );
        });
        draw(cx);
        serve_thread(&host, "agent-1");
        await_restore_update(&shell, cx, |store, _| store.timeline_settled_for("agent-1"));
        settle_panes(cx);
        press_only(panes[1].center(), cx);
        current(false, "the right timeline", cx);
        let card = cx
            .debug_bounds("chat-side-title")
            .expect("the agent's card");
        press_only(card.center(), cx);
        current(true, "the agent's card", cx);

        // A skeleton on its way out shields what lies under it, the pane's
        // root included.
        press_only(panes[1].center(), cx);
        store.update(cx, |store, cx| store.show_thread("thread-3".into(), cx));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(800));
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx)[1],
            Some("thread-3".into()),
            "the next thread lands in the current pane"
        );
        press_only(panes[0].center(), cx);
        serve_thread(&host, "thread-3");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-3"), cx)
        });
        draw(cx);
        press_only(panes[1].center(), cx);
        draw(cx);
        let shown = shell.read_with(cx, |shell, cx| shell.page_thread(cx));
        assert_eq!(shown.as_deref(), Some("thread-3"), "under the shield");

        // A zoomed pane is current: here the one holding the keyboard, zoomed
        // from it with the other pane current.
        cx.simulate_click(panes[0].center(), gpui::Modifiers::default());
        settle_panes(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        let toggle = cx
            .debug_bounds("toggle-sidebar")
            .expect("the right header's");
        press_only(toggle.center(), cx);
        let shown = shell.read_with(cx, |shell, cx| shell.page_thread(cx));
        assert_eq!(shown.as_deref(), Some("thread-3"));
        cx.simulate_keystrokes("secondary-shift-enter");
        settle_panes(cx);
        current(true, "the pane zoomed", cx);
    }

    /// How far the selected fill has come on `row`.
    fn row_fill(row: &'static str, cx: &mut VisualTestContext) -> f32 {
        let row = cx.debug_bounds(row).expect("the thread's row");
        cx.update(|window, cx| {
            let active = cx.theme().list_active;
            let row = row.scale(window.scale_factor());
            window
                .painted_quads()
                .iter()
                .filter(|quad| quad.bounds == row)
                .filter_map(|quad| quad.background.as_solid())
                .filter(|fill| (fill.h, fill.s, fill.l) == (active.h, active.s, active.l))
                .map(|fill| fill.a)
                .fold(0., f32::max)
        })
    }

    /// The composers drawn with the hairline of the one holding the keyboard.
    fn hairlines(cx: &mut VisualTestContext) -> usize {
        cx.update(|window, cx| {
            let primary = cx.theme().primary;
            let mut cards: Vec<_> = window
                .painted_quads()
                .iter()
                .filter(|quad| {
                    quad.border_widths.left.as_f32() > 0. && quad.border_color == primary
                })
                .map(|quad| quad.bounds)
                .collect();
            cards.dedup();
            cards.len()
        })
    }

    /// Whether the keyboard is in the composer of pane `pane`, in the
    /// layout's order.
    fn composer_has_keyboard(
        shell: &Entity<AppShell>,
        pane: usize,
        cx: &mut VisualTestContext,
    ) -> bool {
        cx.update(|window, cx| {
            shell
                .read(cx)
                .attachment
                .as_ref()
                .expect("attached")
                .all_panes(cx)[pane]
                .read(cx)
                .chat()
                .read(cx)
                .composer()
                .read(cx)
                .has_keyboard(window, cx)
        })
    }

    /// One conversation on screen is simply current: its row stays lit
    /// wherever the keyboard goes, an agent beside it lights the same row,
    /// and a route without the conversation lights none.
    #[gpui::test]
    fn a_lone_panes_row_stays_lit_wherever_the_keyboard_goes(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_lead_with_agent(cx);
        let store = store_of(&shell, cx);
        let row = "sidebar-thread-thread-1";
        let lit = cx.update(|_, cx| cx.theme().list_active.a);
        let settled = |cx: &mut VisualTestContext| {
            settle_panes(cx);
            row_fill(row, cx)
        };
        assert_eq!(settled(cx), lit, "the composer");

        cx.update(|window, cx| window.blur(cx));
        assert_eq!(settled(cx), lit, "nothing holds the keyboard");

        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", RightTab::Diff, cx)
        });
        draw(cx);
        assert_eq!(settled(cx), lit, "a docked panel leaves the keyboard alone");
        let panel = cx.debug_bounds("right-panel").expect("the drawer");
        cx.simulate_click(panel.center(), gpui::Modifiers::default());
        assert_eq!(settled(cx), lit, "its drawer");
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", RightTab::Diff, cx)
        });
        settle_panes(cx);

        store.update(cx, |store, cx| {
            store.toggle_terminal_panel_for("thread-1".to_owned(), cx)
        });
        settle_panes(cx);
        assert!(
            !composer_has_keyboard(&shell, 0, cx),
            "the terminal took the keyboard"
        );
        assert_eq!(settled(cx), lit, "its terminal");
        store.update(cx, |store, cx| {
            store.toggle_terminal_panel_for("thread-1".to_owned(), cx)
        });
        settle_panes(cx);

        let chat = shell.read_with(cx, |shell, cx| shell.attachment.as_ref().unwrap().chat(cx));
        cx.update(|window, cx| {
            crate::agents_panel::open_agent(
                &store,
                "agent-1".into(),
                Some(chat.clone()),
                None,
                window,
                cx,
            );
        });
        draw(cx);
        serve_thread(&host, "agent-1");
        await_restore_update(&shell, cx, |store, _| store.timeline_settled_for("agent-1"));
        assert_eq!(settled(cx), lit, "the agent beside it lights its lead");

        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.open_palette(cx));
        draw(cx);
        assert_eq!(settled(cx), lit, "under the palette");
        state.update(cx, |state, cx| state.close_palette(cx));
        settle_panes(cx);

        state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
        draw(cx);
        assert_eq!(settled(cx), 0., "no conversation is on screen");
    }

    /// The first log of `id`: a line of text, in `markdown-block-0`.
    fn serve_answer(host: &MountedShell, id: &str) {
        let records: Vec<agent::AgentEvent> = vec![
            agent::AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: "answer".into(),
                parent_item_id: None,
                content: agent::ItemContent::AssistantMessage {
                    text: "Selectable answer alpha beta gamma".into(),
                    summarized: false,
                },
            }),
            agent::AgentEvent::TurnCompleted {
                turn_id: "turn-1".into(),
                status: agent::TurnStatus::Completed,
                usage: None,
            },
        ];
        send_events(
            host,
            vec![(
                Topic::SessionEvents {
                    session_id: id.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: records.len() as u64,
                    records: records.into_iter().map(Into::into).collect(),
                },
            )],
        );
    }

    /// Among panes one is current: its row takes the selected fill and the
    /// other shown row the weaker one. A press on the other pane's text moves
    /// them, its composer takes no keyboard and so no hairline, and the first
    /// composer's goes with the keyboard, into that text for a copy. The
    /// keyboard leaving and the palette move nothing.
    #[gpui::test]
    fn among_panes_the_current_ones_row_is_selected(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let row = cx
            .debug_bounds("sidebar-thread-thread-2")
            .expect("the thread's row");
        let alt = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        cx.simulate_mouse_move(row.center(), None, alt);
        draw(cx);
        cx.simulate_click(row.center(), alt);
        draw(cx);
        serve_status(&host, "thread-2", false);
        serve_answer(&host, "thread-2");
        await_restore_update(&shell, cx, |store, _| {
            store.timeline_settled_for("thread-2")
        });
        settle_panes(cx);
        let active = cx.update(|_, cx| cx.theme().list_active);
        let (full, weak) = (active.a, active.a * 0.5);
        let fills = |cx: &mut VisualTestContext| {
            [
                row_fill("sidebar-thread-thread-1", cx),
                row_fill("sidebar-thread-thread-2", cx),
            ]
        };
        let wait = |by: Duration, cx: &mut VisualTestContext| {
            cx.executor().advance_clock(by);
            draw(cx);
        };
        let half = crate::edge_back::SETTLE / 2;
        let settle = crate::edge_back::SETTLE;
        let panes = pane_bounds(&shell, cx);
        assert_eq!(fills(cx), [weak, full]);
        cx.simulate_click(panes[0].center(), gpui::Modifiers::default());
        draw(cx);
        wait(settle, cx);
        assert_eq!(fills(cx), [full, weak]);
        assert!(composer_has_keyboard(&shell, 0, cx));
        assert_eq!(hairlines(cx), 1);

        // A press on the right timeline's text: where a selection starts.
        let text = cx.debug_bounds("markdown-block-0").expect("the answer");
        let answer = gpui::point(text.left() + px(30.), text.center().y);
        assert!(panes[1].contains(&answer), "{answer:?} in {panes:?}");
        cx.simulate_mouse_down(answer, MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(answer, MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
        wait(half, cx);
        let [first, second] = fills(cx);
        assert!(
            weak < first && first < full && weak < second && second < full,
            "gliding: {first} {second}"
        );
        wait(settle, cx);
        assert_eq!(fills(cx), [weak, full]);
        assert!(!composer_has_keyboard(&shell, 0, cx));
        assert!(!composer_has_keyboard(&shell, 1, cx));
        assert_eq!(hairlines(cx), 0);
        cx.update(|window, cx| window.blur(cx));
        wait(settle, cx);
        assert_eq!(fills(cx), [weak, full]);

        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.open_palette(cx));
        draw(cx);
        wait(settle, cx);
        assert_eq!(fills(cx), [weak, full], "under the palette");
        assert_eq!(hairlines(cx), 0);
        state.update(cx, |state, cx| state.close_palette(cx));
        wait(settle, cx);

        // A zoomed pane covers the other, which is no longer shown.
        cx.update(|window, cx| window.dispatch_action(Box::new(crate::panes::ZoomPane), cx));
        draw(cx);
        wait(settle, cx);
        assert_eq!(fills(cx), [0., full], "zoomed");
        cx.update(|window, cx| window.dispatch_action(Box::new(crate::panes::ZoomPane), cx));
        draw(cx);
        wait(settle, cx);
        assert_eq!(fills(cx), [weak, full]);

        state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
        draw(cx);
        wait(settle, cx);
        assert_eq!(fills(cx), [0., 0.], "no conversation is on screen");
    }

    /// A shown thread's row makes it current with the keyboard in its
    /// composer and the layout as it is; another thread's replaces the
    /// current pane's, whichever pane holds the keyboard.
    #[gpui::test]
    fn a_rows_click_goes_to_its_shown_thread_or_replaces_the_current_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let (topic, ServerEvent::IndexSnapshot(mut index)) = panes_index(&[]) else {
            unreachable!("an index")
        };
        let mut third = index.sessions[1].clone();
        third.id = "thread-3".into();
        third.title = "thread-3".into();
        third.updated_at = 0;
        index.sessions.push(third);
        send_events(&host, vec![(topic, ServerEvent::IndexSnapshot(index))]);
        await_restore_update(&shell, cx, |store, _| {
            store.thread_meta("thread-3").is_some()
        });
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        let click = |selector: &'static str, cx: &mut VisualTestContext| {
            let row = cx.debug_bounds(selector).expect("the thread's row");
            cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::default());
            draw(cx);
            cx.simulate_click(row.center(), gpui::Modifiers::default());
            draw(cx);
        };
        let shown = pane_bounds(&shell, cx);

        click("sidebar-thread-thread-1", cx);
        settle_panes(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-2".into())]
        );
        assert_eq!(pane_bounds(&shell, cx), shown);
        assert!(composer_has_keyboard(&shell, 0, cx));
        let active = cx.update(|_, cx| cx.theme().list_active.a);
        assert_eq!(row_fill("sidebar-thread-thread-1", cx), active);

        // The right pane pressed where the keyboard stays out of it.
        let toggle = cx
            .debug_bounds("toggle-sidebar")
            .expect("the right header's");
        press_only(toggle.center(), cx);
        assert!(composer_has_keyboard(&shell, 0, cx));

        click("sidebar-thread-thread-3", cx);
        serve_thread(&host, "thread-3");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-3"), cx)
        });
        settle_panes(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-3".into())]
        );
        assert_eq!(pane_bounds(&shell, cx), shown);
    }

    /// Each quad of the row's layout glyph, where it lies in the glyph as a
    /// share of it, and whether it is filled; none on a row with no glyph.
    fn glyph_cells(row: &str, cx: &mut VisualTestContext) -> Vec<(gpui::Point<f32>, bool)> {
        let selector: &'static str = format!("pane-glyph-{row}").leak();
        let Some(glyph) = cx.debug_bounds(selector) else {
            return Vec::new();
        };
        cx.update(|window, cx| {
            let filled = cx.theme().sidebar_foreground;
            let bounds = glyph.scale(window.scale_factor());
            window
                .painted_quads()
                .iter()
                .filter(|quad| {
                    bounds.contains(&quad.bounds.origin)
                        && quad.bounds.size.width.0 < bounds.size.width.0
                })
                .map(|quad| {
                    let centre = quad.bounds.center() - bounds.origin;
                    (
                        gpui::point(
                            centre.x.0 / bounds.size.width.0,
                            centre.y.0 / bounds.size.height.0,
                        ),
                        quad.background.as_solid() == Some(filled),
                    )
                })
                .collect()
        })
    }

    /// `pane`'s centre, as a share of the column the `panes` make up.
    fn share_of_column(
        pane: gpui::Bounds<Pixels>,
        panes: &[gpui::Bounds<Pixels>],
    ) -> gpui::Point<f32> {
        let column = panes
            .iter()
            .copied()
            .reduce(|all, pane| all.union(&pane))
            .expect("a pane");
        let centre = pane.center() - column.origin;
        gpui::point(centre.x / column.size.width, centre.y / column.size.height)
    }

    /// With two or more panes, a shown thread's row carries a map of them
    /// with its own pane filled, wherever the layout puts that pane.
    #[gpui::test]
    fn a_rows_glyph_maps_the_column_and_fills_its_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        assert_eq!(glyph_cells("thread-1", cx), [], "a lone pane has no map");

        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        settle_panes(cx);
        let filled_at = |row: &str, pane: usize, cx: &mut VisualTestContext| {
            let panes = pane_bounds(&shell, cx);
            let cells = glyph_cells(row, cx);
            assert_eq!(cells.len(), panes.len(), "{row}: {cells:?}");
            let filled: Vec<_> = cells.iter().filter(|(_, filled)| *filled).collect();
            let [(cell, _)] = filled[..] else {
                panic!("{row}: one cell filled: {cells:?}");
            };
            let wanted = share_of_column(panes[pane], &panes);
            assert!(
                (cell.x - wanted.x).abs() < 0.1 && (cell.y - wanted.y).abs() < 0.1,
                "{row}: filled at {cell:?}, its pane at {wanted:?}"
            );
        };
        filled_at("thread-1", 0, cx);
        filled_at("thread-2", 1, cx);

        // The threads trade panes, and the glyphs follow.
        cx.simulate_keystrokes("secondary-alt-shift-left");
        settle_panes(cx);
        filled_at("thread-1", 1, cx);
        filled_at("thread-2", 0, cx);

        // A split under the right pane cuts its cell in two.
        cx.simulate_keystrokes("secondary-shift-d");
        settle_panes(cx);
        answer_draft(&shell, &host, "draft-1", cx);
        settle_panes(cx);
        assert_eq!(pane_bounds(&shell, cx).len(), 3);
        filled_at("thread-1", 1, cx);
        filled_at("thread-2", 0, cx);

        // A zoomed pane is the column alone.
        cx.update(|window, cx| window.dispatch_action(Box::new(crate::panes::ZoomPane), cx));
        settle_panes(cx);
        assert_eq!(glyph_cells("thread-1", cx), []);
    }

    /// A row with no glyph gives up no width: the glyph comes with its own,
    /// the title's room gliding narrower as it fades in and wider as it goes.
    #[gpui::test]
    fn a_rows_glyph_takes_its_width_as_it_comes_and_gives_it_back(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        let row = "pane-glyph-thread-1";
        assert_eq!(cx.debug_bounds(row), None, "a lone pane's row has none");
        let part_way = crate::edge_back::SETTLE / 4;

        // The split waits for its thread beside the first pane.
        cx.simulate_keystrokes("secondary-d");
        draw(cx);
        cx.executor().advance_clock(part_way);
        draw(cx);
        let coming = cx.debug_bounds(row).expect("the glyph, widening in");
        settle_panes(cx);
        let rest = cx.debug_bounds(row).expect("the glyph");
        assert_eq!(rest.size.width, px(14.));
        assert!(
            rest.left() < coming.left() && coming.left() < rest.right(),
            "the row makes room over the fade: {coming:?}, then {rest:?}"
        );

        cx.simulate_keystrokes("secondary-shift-w");
        draw(cx);
        cx.executor().advance_clock(part_way);
        draw(cx);
        let going = cx.debug_bounds(row).expect("the glyph, narrowing away");
        assert!(
            rest.left() < going.left() && going.left() < rest.right(),
            "the row takes its room back over the fade: {going:?} from {rest:?}"
        );
        settle_panes(cx);
        assert_eq!(pane_threads(&shell, cx).len(), 1);
        assert_eq!(cx.debug_bounds(row), None, "the width is back");
    }

    /// Hovering a row softly fills the pane showing its thread, fading in and
    /// out; a row with no pane there, or one alone in the column, fills
    /// nothing.
    #[gpui::test]
    fn hovering_a_row_softly_fills_its_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let none = gpui::Modifiers::default();
        let row = cx
            .debug_bounds("sidebar-thread-thread-1")
            .expect("the thread's row");
        let hovered = |cx: &mut VisualTestContext| {
            let fill = cx.debug_bounds("pane-row-hovered")?;
            cx.update(|window, cx| {
                let active = cx.theme().list_active;
                let fill = fill.scale(window.scale_factor());
                window
                    .painted_quads()
                    .iter()
                    .filter(|quad| quad.bounds == fill)
                    .filter_map(|quad| quad.background.as_solid())
                    .find(|colour| (colour.h, colour.s, colour.l) == (active.h, active.s, active.l))
                    .map(|colour| colour.a)
            })
        };
        cx.simulate_mouse_move(row.center(), None, none);
        settle_panes(cx);
        assert_eq!(hovered(cx), None, "a lone pane");
        cx.simulate_mouse_move(gpui::point(px(1.), px(1.)), None, none);
        settle_panes(cx);

        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        let panes = pane_bounds(&shell, cx);
        let row = cx
            .debug_bounds("sidebar-thread-thread-1")
            .expect("the thread's row");
        cx.simulate_mouse_move(gpui::point(px(1.), px(1.)), None, none);
        settle_panes(cx);
        assert_eq!(hovered(cx), None, "the pointer is on no row");

        cx.simulate_mouse_move(row.center(), None, none);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let rest = cx.update(|_, cx| cx.theme().list_active.a * 0.6);
        let coming = hovered(cx).expect("fading in");
        assert!(0. < coming && coming < rest, "{coming} of {rest}");
        let fill = cx.debug_bounds("pane-row-hovered").expect("the fill");
        assert_eq!(fill, panes[0], "over the left pane alone");
        settle_panes(cx);
        assert_eq!(hovered(cx), Some(rest));

        cx.simulate_mouse_move(gpui::point(px(1.), px(1.)), None, none);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let going = hovered(cx).expect("fading out");
        assert!(0. < going && going < rest, "{going} of {rest}");
        settle_panes(cx);
        assert_eq!(hovered(cx), None);
    }

    /// A thread restored at launch takes the keyboard once it has loaded,
    /// with no further input, though it was asked for before the window was
    /// active and before its composer was drawn.
    #[gpui::test]
    fn a_restored_thread_takes_the_keyboard_once_loaded(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, _, cx) =
            mount_restored_at_width(cx, &["hosts", "threads", "thread"], true, 1024., "plan");
        assert!(!cx.update(|window, _| window.is_window_active()));
        assert!(cx.debug_bounds("baseline-loading").is_some());
        assert!(!composer_has_keyboard(&shell, 0, cx));

        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        restore_settings_and_events(&shell, &host, cx);
        draw(cx);
        assert!(!cx.update(|window, _| window.is_window_active()));
        assert!(composer_has_keyboard(&shell, 0, cx));
    }

    /// Choosing a thread still loading gives its composer the keyboard the
    /// moment it is drawn; meanwhile the keyboard is in the pane, which it
    /// leaves to nothing else.
    #[gpui::test]
    fn a_chosen_threads_composer_takes_the_keyboard_as_it_is_drawn(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        cx.update(|window, cx| window.blur(cx));
        draw(cx);
        let row = cx
            .debug_bounds("sidebar-thread-thread-2")
            .expect("the thread's row");
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-2".into())]);
        assert!(!composer_has_keyboard(&shell, 0, cx), "not drawn yet");
        assert_eq!(keyboard_pane(&shell, cx), Some(0));

        serve_thread(&host, "thread-2");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        draw(cx);
        assert!(composer_has_keyboard(&shell, 0, cx));
    }

    /// A thread the workspace goes to by itself finds the keyboard in a lone
    /// pane with nothing holding it, and leaves it where something does.
    #[gpui::test]
    fn an_unasked_thread_takes_only_a_keyboard_nothing_holds(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        serve_thread(&host, "thread-2");
        cx.update(|window, cx| window.blur(cx));
        store.update(cx, |store, cx| store.put_back_thread("thread-2".into(), cx));
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        draw(cx);
        assert!(composer_has_keyboard(&shell, 0, cx), "nothing held it");

        // The palette's input holds it.
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.open_palette(cx));
        settle_panes(cx);
        let query = shell.read_with(cx, |shell, cx| {
            let palette = shell.attachment.as_ref().unwrap().palette.read(cx);
            palette.query_focus_handle(cx)
        });
        cx.update(|window, _| assert!(query.is_focused(window)));
        serve_thread(&host, "thread-1");
        store.update(cx, |store, cx| store.put_back_thread("thread-1".into(), cx));
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-1"), cx)
        });
        settle_panes(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        cx.update(|window, _| assert!(query.is_focused(window), "it keeps the keyboard"));
        state.update(cx, |state, cx| state.close_palette(cx));
        settle_panes(cx);

        // Beside another pane it stays nowhere.
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        cx.update(|window, cx| window.blur(cx));
        store.update(cx, |store, cx| store.put_back_thread("thread-2".into(), cx));
        settle_panes(cx);
        assert_eq!(keyboard_pane(&shell, cx), None);
    }

    /// What holds the keyboard over a conversation gives it to a composer as
    /// it closes: at once, while it still leaves, not when it has gone. A
    /// dialog with nothing to go back to does the same.
    #[gpui::test]
    fn closing_the_palette_or_a_dialog_returns_the_keyboard(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.open_palette(cx));
        settle_panes(cx);
        assert!(!composer_has_keyboard(&shell, 0, cx));
        state.update(cx, |state, cx| state.close_palette(cx));
        draw(cx);
        assert!(state.read_with(cx, |state, _| state.palette_mounted()));
        assert!(composer_has_keyboard(&shell, 0, cx), "the palette left");

        cx.update(|window, cx| {
            window.blur(cx);
            window.open_dialog(cx, |dialog, _, _| dialog);
        });
        draw(cx);
        assert!(!composer_has_keyboard(&shell, 0, cx));
        cx.update(|window, cx| window.close_dialog(cx));
        draw(cx);
        assert!(composer_has_keyboard(&shell, 0, cx), "the dialog closed");
    }

    /// Among several panes each pane's header ends with its close button,
    /// which fades and rises in as the column gains a second pane and fades
    /// out as it is left with one; pressing it closes its own pane, which
    /// glides shut.
    #[gpui::test]
    fn a_panes_header_closes_it_from_its_right_end(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let wait = |by: Duration, cx: &mut VisualTestContext| {
            cx.executor().advance_clock(by);
            draw(cx);
        };
        let half = crate::edge_back::SETTLE / 2;
        let settle = crate::edge_back::SETTLE;
        let pane_bounds = |cx: &mut VisualTestContext| -> Vec<gpui::Bounds<Pixels>> {
            shell.read_with(cx, |shell, cx| {
                let panes = shell.attachment.as_ref().expect("attached").all_panes(cx);
                panes.iter().map(|pane| pane.read(cx).bounds()).collect()
            })
        };
        // Where the button is drawn: it sinks as it fades out and rises as
        // it fades in, so its top follows how far in it is.
        let close_top = |cx: &mut VisualTestContext| {
            cx.debug_bounds("pane-close")
                .expect("the pane's close button")
                .top()
        };
        assert!(cx.debug_bounds("pane-close").is_none(), "a lone pane");

        // Stacked, so the top pane keeps its width and its header shows
        // while the split glides; the new pane waits for its draft.
        cx.simulate_keystrokes("secondary-shift-d");
        draw(cx);
        let entering = close_top(cx);
        wait(half, cx);
        let rising = close_top(cx);
        wait(settle, cx);
        let at_rest = close_top(cx);
        assert!(
            entering > rising && rising > at_rest,
            "{entering:?} then {rising:?} then {at_rest:?}"
        );
        let close = cx
            .debug_bounds("pane-close")
            .expect("the pane's close button");
        let top = pane_bounds(cx)[0];
        // Where this build draws the window's caption buttons, the top
        // pane's header ends with them, after its own controls.
        let end = top.right() - px(crate::window_caption::RIGHT_PANEL_CAPTION_WIDTH);
        assert!(
            top.contains(&close.center()) && close.right() <= end && end - close.right() < px(12.),
            "at the right end of the top pane's header: {close:?} in {top:?}"
        );

        cx.simulate_keystrokes("secondary-shift-w");
        draw(cx);
        let leaving = close_top(cx);
        wait(half, cx);
        let sinking = close_top(cx);
        assert!(
            leaving == at_rest && sinking > leaving,
            "{leaving:?} then {sinking:?}"
        );
        wait(settle, cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        assert!(
            cx.debug_bounds("pane-close").is_none(),
            "gone with the second pane"
        );

        // The draft the closed pane waited for is no longer wanted.
        let _ = sent(&host);
        cx.simulate_keystrokes("secondary-shift-d");
        draw(cx);
        answer_draft(&shell, &host, "draft-1", cx);
        wait(settle, cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("draft-1".into())]
        );
        cx.simulate_keystrokes("secondary-alt-up");
        draw(cx);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        let start = pane_bounds(cx);
        let close = cx.debug_bounds("pane-close").expect("a close button");
        assert!(start[1].contains(&close.center()), "{close:?} in {start:?}");
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        draw(cx);
        assert_eq!(pane_threads(&shell, cx).len(), 2, "it glides shut first");
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        wait(half, cx);
        let halfway = pane_bounds(cx);
        assert!(
            halfway[1].size.height < start[1].size.height
                && halfway[0].size.height > start[0].size.height,
            "{start:?} then {halfway:?}"
        );
        wait(settle, cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-1"));
            assert!(!store.is_thread_open("draft-1"));
        });
    }

    /// The window's chrome sits in its top corners, so of two stacked panes
    /// only the top one's header makes room for it: the lower header clears
    /// no traffic lights over a collapsed sidebar and hosts no caption
    /// buttons, its close button ending it.
    #[gpui::test]
    fn a_pane_stacked_below_another_leaves_the_window_chrome_to_it(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        shell
            .read_with(cx, |shell, _| shell.window_state())
            .update(cx, |state, cx| state.toggle_sidebar_collapsed(&store, cx));
        cx.simulate_keystrokes("secondary-shift-d");
        draw(cx);
        answer_draft(&shell, &host, "draft-1", cx);
        crate::test_frames::draw_settled(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("draft-1".into())]
        );

        let [top, below] = pane_bounds(&shell, cx)[..] else {
            panic!("two panes");
        };
        assert!(below.top() >= top.bottom(), "{top:?} over {below:?}");
        // The lower pane's header is drawn after the top one's.
        let toggle = cx
            .debug_bounds("toggle-sidebar")
            .expect("the lower header's sidebar toggle");
        let close = cx
            .debug_bounds("pane-close")
            .expect("the lower pane's close button");
        assert!(
            below.contains(&toggle.center())
                && toggle.left() - below.left() < px(crate::window_caption::TRAFFIC_LIGHT_INSET),
            "no traffic lights to clear: {toggle:?} in {below:?}"
        );
        assert!(
            below.contains(&close.center()) && below.right() - close.right() < px(12.),
            "no caption buttons after it: {close:?} in {below:?}"
        );
    }

    /// Choosing from the threads list a thread another pane shows moves the
    /// keyboard there instead of showing the thread twice.
    #[gpui::test]
    fn choosing_a_thread_shown_in_another_pane_focuses_that_pane(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-2".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1));

        // The threads list's first row, through its shortcut.
        cx.simulate_keystrokes("secondary-1");
        draw(cx);
        store_of(&shell, cx).read_with(cx, |store, _| {
            assert!(store.is_thread_open("thread-1"));
            assert!(store.is_thread_open("thread-2"));
        });
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-2".into())]
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
    }

    /// A thread goes to the pane showing it, or else to the pane of the
    /// control that asked, or else to the pane last in use, whichever pane
    /// the keyboard has left it in, and takes the keyboard there.
    #[gpui::test]
    fn a_shown_thread_lands_by_the_placement_order(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);

        store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-2".into())],
            "a thread a pane shows stays there"
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(0));

        store.update(cx, |store, cx| {
            store.show_thread_replacing("thread-3".into(), Some("thread-2".into()), cx)
        });
        serve_thread(&host, "thread-3");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-3"), cx)
        });
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-1".into()), Some("thread-3".into())],
            "the asking control's pane, with the keyboard in the other"
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(1));
        store.read_with(cx, |store, _| {
            assert!(!store.is_thread_open("thread-2"), "the thread it replaced");
        });

        store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
        draw(cx);
        cx.update(|window, cx| window.blur(cx));
        store.update(cx, |store, cx| store.show_thread("thread-2".into(), cx));
        serve_thread(&host, "thread-2");
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-2"), cx)
        });
        draw(cx);
        assert_eq!(
            pane_threads(&shell, cx),
            [Some("thread-2".into()), Some("thread-3".into())],
            "the current pane, with the keyboard nowhere"
        );
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
    }

    /// An agent shown beside its lead is chosen where it is: its card takes
    /// the keyboard and no pane takes the agent in the lead's place.
    #[gpui::test]
    fn choosing_an_agent_shown_beside_its_lead_selects_its_card(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_lead_with_agent(cx);
        let store = store_of(&shell, cx);
        let chat = shell.read_with(cx, |shell, cx| shell.attachment.as_ref().unwrap().chat(cx));
        cx.update(|window, cx| {
            crate::agents_panel::open_agent(
                &store,
                "agent-1".into(),
                Some(chat.clone()),
                None,
                window,
                cx,
            );
        });
        settle_panes(cx);
        cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
        draw(cx);

        store.update(cx, |store, cx| store.show_thread("agent-1".into(), cx));
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into())]);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        cx.update(|window, cx| {
            assert!(!chat.read(cx).composer().read(cx).has_keyboard(window, cx))
        });
    }

    /// The draft an empty workspace falls back on comes up in its pane and
    /// leaves a phone on its threads list: nobody asked to go there.
    #[gpui::test]
    fn a_fallback_draft_leaves_a_phone_on_its_threads(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        let project = tcode_core::project::Project {
            id: "project".into(),
            name: "Project".into(),
            root: PANES_CWD.into(),
            quick_chat: false,
            icon_path: None,
            permission_defaults: Default::default(),
            created_at: 0,
        };
        send_events(
            &host,
            vec![
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(Default::default()),
                ),
                (
                    Topic::Index,
                    ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        sessions: Vec::new(),
                        projects: vec![project],
                    }),
                ),
            ],
        );
        await_restore_update(&shell, cx, |store, _| store.index_hydrated());
        draw(cx);
        let request = std::iter::from_fn(|| host.outgoing.try_recv().ok())
            .map(|line| decode_client_line(&line).expect("client line"))
            .find(|message| {
                matches!(
                    message.payload,
                    ClientPayload::Command(Command::StartDraft { .. })
                )
            })
            .expect("the workspace asks for a draft");
        host.incoming
            .try_send(
                encode_line(&HostMessage::Ack {
                    id: request.id,
                    result: Ok(tcode_protocol::CommandResponse::SessionId(Some(
                        "draft-1".into(),
                    ))),
                })
                .unwrap(),
            )
            .unwrap();
        await_restore_update(&shell, cx, |store, _| store.is_thread_open("draft-1"));
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("draft-1".into())]);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Threads);
        });
    }

    #[gpui::test]
    fn project_choice_reuses_drafts_and_focuses_only_on_desktop(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for (mobile, compact, projects) in [
            (false, false, 2),
            (true, false, 2),
            (true, true, 2),
            (true, true, 1),
        ] {
            cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, mobile));
            new_thread_from_projects(cx, projects, compact);
        }
    }

    fn new_thread_from_projects(cx: &mut TestAppContext, project_count: usize, compact: bool) {
        use tcode_core::project::Project;
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        cx.update(crate::theme::init);
        let root =
            crate::store::tests::scratch_root(&format!("new-thread-{project_count}-{compact}"));
        let disk = SessionStore::open_at(root.clone()).unwrap();
        let projects: Vec<_> = (0..project_count)
            .map(|index| Project {
                id: format!("project-{index}"),
                name: format!("Project {index}"),
                root: root.join(format!("project-{index}")),
                quick_chat: false,
                icon_path: None,
                permission_defaults: Default::default(),
                created_at: index as u64,
            })
            .collect();
        let expected = projects.last().unwrap().clone();
        let host = spawn_host(disk, HostServices::default()).unwrap();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = projects;
            state.settings.language = Some("en".into());
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let (shell, _transport, cx) = mount(cx);
        // Establish the startup viewport before the baseline can select a draft.
        // Shrinking an already populated wide layout intentionally opens its thread.
        if compact {
            as_mobile(cx);
        }
        cx.simulate_resize(size(px(if compact { 393. } else { 1024. }), px(852.)));
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(250));
        draw(cx);
        let store = store_of(&shell, cx);
        // Keep the shell's transport deterministic; the real host link owns
        // its pump, as in the store tests, and snapshots are drained below.
        store.update(cx, |store, cx| {
            *store = WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "project baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        draw(cx);
        let mut selected_draft = None;
        for reopen in [false, true] {
            if reopen && compact {
                cx.update(|window, cx| {
                    shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
                });
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(250));
                draw(cx);
            }
            if compact {
                let plus = cx
                    .debug_bounds("compact-new-thread")
                    .expect("Threads new-thread button");
                cx.simulate_click(plus.center(), gpui::Modifiers::default());
            } else {
                shell
                    .read_with(cx, |shell, _| shell.window_state())
                    .update(cx, |state, cx| {
                        state.open_palette(cx);
                    });
            }
            draw(cx);
            if project_count > 1 || !compact {
                assert!(shell.read_with(cx, |shell, cx| shell.window_state.read(cx).palette_open));
                cx.update(|window, cx| {
                    let palette = &shell.read(cx).attachment.as_ref().unwrap().palette;
                    let query = palette.read(cx).query_focus_handle(cx);
                    // The chooser is tapped through on a phone; a desktop
                    // window (even a narrow one) types into it.
                    assert_eq!(
                        query.is_focused(window),
                        !(compact && crate::window_seam::is_mobile(cx)),
                        "project chooser raises the keyboard only off-phone"
                    );
                    assert!(
                        palette
                            .read(cx)
                            .focus_handle(cx)
                            .contains_focused(window, cx),
                        "the palette owns focus so Escape closes it"
                    );
                });
                let row = cx
                    .debug_bounds("palette-new-thread-project-1")
                    .expect("project choice");
                cx.simulate_click(row.center(), gpui::Modifiers::default());
                draw(cx);
            }
            shell.read_with(cx, |shell, cx| {
                let state = shell.window_state.read(cx);
                assert!(!state.palette_open, "choosing a project closes the chooser");
            });
            let deadline = crate::store::tests::host_deadline();
            loop {
                driver.step(cx, &store);
                draw(cx);
                let page = shell.read_with(cx, |shell, cx| shell.page_thread(cx));
                if store.read_with(cx, |store, cx| {
                    page.is_some()
                        && !store.chat_loading_for(page.as_deref(), cx)
                        && store
                            .chat_active_session_for(page.as_deref(), cx)
                            .is_some_and(|(_, cwd, draft)| cwd == expected.root && draft)
                }) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "selected project's draft and event baseline reach chat"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            shell.read_with(cx, |shell, cx| {
                let state = shell.window_state.read(cx);
                if compact {
                    assert_eq!(
                        state.history().last(),
                        Some(&Destination::Thread),
                        "a draft reused or new navigates when the host answers"
                    );
                } else {
                    assert_eq!(state.route(), Route::Chat);
                }
            });
            let focus = shell.read_with(cx, |shell, cx| {
                shell
                    .attachment
                    .as_ref()
                    .unwrap()
                    .chat(cx)
                    .read(cx)
                    .composer()
                    .read(cx)
                    .input_focus_handle(cx)
            });
            cx.update(|window, cx| {
                assert_eq!(
                    focus.is_focused(window),
                    !compact && !crate::window_seam::is_mobile(cx)
                )
            });
            if cx.update(|_, cx| crate::window_seam::is_mobile(cx)) {
                // Let the compact push finish sliding before tapping into it.
                cx.executor().advance_clock(Duration::from_millis(250));
                draw(cx);
                let card = cx.debug_bounds("composer-card").expect("draft composer");
                cx.simulate_click(
                    gpui::point(card.center().x, card.top() + px(24.)),
                    gpui::Modifiers::default(),
                );
                draw(cx);
                cx.update(|window, _| {
                    assert!(focus.is_focused(window), "explicit composer tap focuses")
                });
                // Rotate away and back to the width the scenario started at.
                let rotation = if compact {
                    [1024., 393.]
                } else {
                    [393., 1024.]
                };
                for width in rotation {
                    resize(cx, width);
                    cx.update(|window, _| {
                        assert!(focus.is_focused(window), "rotation preserves editing focus")
                    });
                }
            }
            let selected = shell.read_with(cx, |shell, cx| shell.page_thread(cx));
            assert!(selected.is_some());
            if reopen {
                assert_eq!(
                    selected, selected_draft,
                    "reopening reuses the selected draft"
                );
            }
            selected_draft = selected;
        }
        driver.shutdown(cx, &store, &host);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn header_controls_activate_after_pointer_jitter_without_dragging(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        for (topic, event) in [
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: Vec::new(),
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        await_restore_update(&shell, cx, |store, cx| !store.chat_loading_for(None, cx));
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        for (width, selector) in [(1024., "toggle-sidebar"), (393., "compact-search")] {
            if width < 900. {
                as_mobile(cx);
            }
            resize(cx, width);
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
            let start = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing {selector}"))
                .center();
            let end = start + gpui::point(px(1.), px(1.));
            cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
            // TestWindow rejects native window moves, so a leaked press fails here.
            cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
            cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
            state.read_with(cx, |state, _| {
                if width >= 900. {
                    assert!(state.sidebar_collapsed);
                } else {
                    assert!(state.palette_open);
                }
            });
        }
    }

    /// A real shell behind a threads list tested on its own: what `store`
    /// shows lands in the pane the list reads the current pane from.
    pub(crate) fn shell_behind(store: &Entity<WorkspaceStore>, cx: &mut TestAppContext) {
        let (shell, host, cx) = mount(cx);
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        let own = store_of(&shell, cx);
        cx.update(|_, cx| {
            cx.subscribe(store, move |_, shown: &ShowThread, cx| {
                // Held here, the shell's link stays open as long as the store.
                let _ = &host;
                own.update(cx, |own, cx| own.show_thread(shown.session_id.clone(), cx));
            })
            .detach();
        });
    }

    fn mount(cx: &mut TestAppContext) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        mount_initial(cx, Some(AttachmentTarget::Local))
    }

    fn mount_initial(
        cx: &mut TestAppContext,
        initial: Option<AttachmentTarget>,
    ) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        // As `run_shell` does before opening the window: the shell alone,
        // without the window root, initializes nothing of gpui-base itself.
        cx.update(crate::theme::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let (states, state) = async_channel::unbounded();
        let transport = RefCell::new(Some(Transport {
            originals: None,
            to_host: to_host.into(),
            from_host,
            state,
            current_host: None,
        }));
        let mounted = Rc::new(RefCell::new(None));
        let capture = mounted.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let window_state = cx.new(|_| WindowState::new(false));
            let shell = cx.new(|cx| {
                AppShell::new(
                    window_state,
                    ShellSetup {
                        local: Some(Rc::new(move |_| {
                            Ok(transport.borrow_mut().take().expect("one attachment"))
                        })),
                        initial,
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            });
            *capture.borrow_mut() = Some(shell.clone());
            gpui_base::Root::new(shell, window, cx)
        });
        let shell = mounted.borrow_mut().take().unwrap();
        if let Some(store) = shell.read_with(cx, |shell, _| shell.store()) {
            crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        }
        (
            shell,
            MountedShell {
                outgoing,
                observed: RefCell::new(Vec::new()),
                incoming,
                states,
            },
            cx,
        )
    }

    #[gpui::test]
    fn connecting_from_the_hosts_panel_releases_its_entity_before_attachment(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_initial(cx, None);
        as_mobile(cx);
        resize(cx, 393.);
        shell
            .read_with(cx, |shell, _| shell.window_state())
            .update(cx, |state, cx| {
                state.go(Destination::Thread, cx);
                state.go(Destination::Panel, cx);
            });
        let hosts = shell.read_with(cx, |shell, _| shell.hosts.clone());
        cx.update(|window, cx| {
            set_back_target(window.window_handle(), &shell, cx);
            // Connect runs inside RemotePanel's click listener. Attaching
            // updates that same panel to point at the new workspace store.
            hosts.update(cx, |_, cx| {
                switch_current(AttachmentTarget::Local, window, cx);
            });
        });
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.store().is_some());
            assert_eq!(
                shell.window_state.read(cx).history(),
                [Destination::Hosts, Destination::Threads]
            );
        });
        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.destination(cx)),
            Destination::Hosts
        );
        assert!(!cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
    }

    /// A local host that did not start at launch is on Machines from the
    /// first frame, whatever the window attached to. Retry asks bootstrap to
    /// start it again; a failed start keeps the machine the window is on, and
    /// one that comes up replaces it.
    #[gpui::test]
    fn a_local_host_that_does_not_start_is_shown_and_retried(cx: &mut TestAppContext) {
        struct Desk {
            saved: tcode_client::pairing::PairedHost,
            transports: RefCell<Vec<Transport>>,
        }
        impl ClientHost for Desk {
            fn device_name(&self) -> String {
                "desk".into()
            }
            fn device_id(&self) -> String {
                "desk".into()
            }
            fn device_platform(&self) -> Option<String> {
                None
            }
            fn load_hosts(&self) -> Vec<tcode_client::pairing::PairedHost> {
                vec![self.saved.clone()]
            }
            fn save_hosts(&self, _: &[tcode_client::pairing::PairedHost]) {}
            fn last_host_id(&self) -> Option<String> {
                None
            }
            fn set_last_host_id(&self, _: Option<&str>) {}
            fn pair(
                &self,
                _: tcode_client::pairing::PairInvite,
                _: Option<std::net::IpAddr>,
            ) -> tcode_client::host::HostFuture<'_, Result<tcode_client::pairing::PairedHost, String>>
            {
                unreachable!("nothing is paired here")
            }
            fn connect(&self, _: &tcode_client::pairing::PairedHost) -> Transport {
                self.transports.borrow_mut().pop().expect("one connection")
            }
        }
        fn transport() -> (Transport, async_channel::Sender<String>) {
            let (to_host, _) = async_channel::unbounded();
            let (incoming, from_host) = async_channel::unbounded();
            let (_, state) = async_channel::unbounded();
            let transport = Transport {
                originals: None,
                to_host: to_host.into(),
                from_host,
                state,
                current_host: None,
            };
            (transport, incoming)
        }

        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let busy = "another Tcode host is already using the data directory /tmp/demo";
        let (local_transport, local_incoming) = transport();
        // Answered from the back: busy on the first Retry, up on the second.
        let answers = RefCell::new(vec![Ok(local_transport), Err(busy.to_owned())]);
        let attempts = Rc::new(Cell::new(0));
        let local: LocalTransport = {
            let attempts = attempts.clone();
            Rc::new(move |_| {
                attempts.set(attempts.get() + 1);
                answers.borrow_mut().pop().expect("an unexpected start")
            })
        };
        let saved = tcode_client::pairing::PairedHost {
            host_id: "studio".into(),
            name: "Studio".into(),
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["127.0.0.1:47503".into()],
            last_connected_unix: None,
            group_id: None,
            space_id: None,
            space_name: None,
        };
        let (remote_transport, _remote_incoming) = transport();
        let desk: Rc<dyn ClientHost> = Rc::new(Desk {
            saved: saved.clone(),
            transports: RefCell::new(vec![remote_transport]),
        });
        cx.update(|cx| {
            cx.set_global(crate::remote::ClientAttachment::new(
                desk.clone(),
                true,
                switch_current,
            ))
        });
        let mounted = Rc::new(RefCell::new(None));
        let capture = mounted.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let window_state = cx.new(|_| WindowState::new(false));
            let shell = cx.new(|cx| {
                AppShell::new(
                    window_state,
                    ShellSetup {
                        client_host: Some(desk),
                        local: Some(local),
                        local_failure: Some(busy.to_owned()),
                        initial: Some(AttachmentTarget::Remote(Box::new(saved))),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            });
            *capture.borrow_mut() = Some(shell.clone());
            gpui_base::Root::new(shell, window, cx)
        });
        let shell: Entity<AppShell> = mounted.borrow_mut().take().unwrap();
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        resize(cx, 1024.);
        let attached = |cx: &mut VisualTestContext| {
            shell.read_with(cx, |shell, cx| {
                shell
                    .store()
                    .map(|store| store.read(cx).remote_host_id().map(str::to_owned))
            })
        };
        let open_machines = |cx: &mut VisualTestContext| {
            shell
                .read_with(cx, |shell, _| shell.window_state())
                .update(cx, |state, cx| state.go(Destination::Hosts, cx));
            draw(cx);
        };
        let retry = |cx: &mut VisualTestContext| {
            let button = cx.debug_bounds("hosts-local-retry").expect("Retry");
            cx.simulate_click(button.center(), gpui::Modifiers::default());
            draw(cx);
        };

        open_machines(cx);
        assert_eq!(attempts.get(), 0, "the launch's failure is not asked again");
        assert_eq!(attached(cx), Some(Some("studio".into())));
        assert!(cx.debug_bounds("hosts-local-failure").is_some());

        retry(cx);
        assert_eq!(attempts.get(), 1, "Retry starts the local host again");
        assert_eq!(
            attached(cx),
            Some(Some("studio".into())),
            "a failed start keeps the machine the window is on"
        );
        open_machines(cx);
        assert!(cx.debug_bounds("hosts-local-failure").is_some());

        retry(cx);
        assert_eq!(attempts.get(), 2);
        assert_eq!(attached(cx), Some(None), "the local host replaces it");
        let store = shell.read_with(cx, |shell, _| shell.store()).unwrap();
        crate::store::tests::seed_full_scope(&store, &local_incoming, Vec::new(), cx);
        open_machines(cx);
        // The machine left is drawn beneath the new one while it rises, and
        // the failure folds away there.
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("hosts-local-failure").is_none());
    }

    /// The compact palette opens from the threads page and a thread as a
    /// sheet on the window's bottom edge; the page beneath and what the sheet
    /// holds glide onto the window's seam as the status bar and the keyboard
    /// come; Back dismisses the keyboard before the sheet.
    #[gpui::test]
    fn compact_palette_is_reachable_from_threads_and_thread(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        for selector in ["compact-search", "compact-open-palette"] {
            let button = cx.debug_bounds(selector).expect("compact palette entry");
            cx.simulate_click(button.center(), gpui::Modifiers::default());
            draw(cx);
            assert!(state.read_with(cx, |state, _| state.palette_open));
            let card = cx.debug_bounds("palette-card").expect("palette sheet");
            assert_eq!(card.left(), px(0.));
            assert_eq!(card.right(), px(393.));
            assert_eq!(card.bottom(), px(852.));
            let position = gpui::point(px(30.), px(26.));
            let history = state.read_with(cx, |state, _| state.history().to_vec());
            cx.simulate_event(gpui::MouseDownEvent {
                button: gpui::MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            });
            draw(cx);
            cx.simulate_event(gpui::MouseUpEvent {
                button: gpui::MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            });
            assert!(!state.read_with(cx, |state, _| state.palette_open));
            assert_eq!(
                state.read_with(cx, |state, _| state.history().to_vec()),
                history
            );
            cx.simulate_click(button.center(), gpui::Modifiers::default());
            draw(cx);
            assert!(state.read_with(cx, |state, _| state.palette_open));
            cx.update(|window, cx| {
                let palette = &shell.read(cx).attachment.as_ref().unwrap().palette;
                assert!(
                    palette
                        .read(cx)
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                );
            });
            // The status bar and the keyboard cover the window: the page
            // beneath and the sheet glide onto the new seam together rather
            // than jumping there.
            let page_top = |cx: &mut VisualTestContext| {
                cx.debug_bounds(selector).expect("the page beneath").top()
            };
            let uncovered = page_top(cx);
            crate::window_seam::occlude_for_test(
                cx,
                gpui::Edges {
                    top: px(47.),
                    bottom: px(400.),
                    ..Default::default()
                },
            );
            draw(cx);
            cx.executor().advance_clock(crate::edge_back::SETTLE / 3);
            draw(cx);
            let gliding = |seen: Pixels, from: Pixels, to: Pixels, what: &str| {
                assert!(
                    from.min(to) + px(1.) < seen && seen < from.max(to) - px(1.),
                    "{what} jumped: {seen:?}, not between {from:?} and {to:?}"
                );
            };
            gliding(
                page_top(cx),
                uncovered,
                uncovered + px(47.),
                "the page's top",
            );
            gliding(
                cx.debug_bounds("palette-list").unwrap().bottom(),
                px(852.),
                px(452.),
                "the list's bottom",
            );
            cx.executor().advance_clock(crate::edge_back::SETTLE);
            draw(cx);
            assert_eq!(page_top(cx), uncovered + px(47.));
            let card = cx.debug_bounds("palette-card").unwrap();
            assert_eq!(card.top(), px(99.));
            assert_eq!(card.bottom(), px(852.), "the sheet reaches the edge");
            assert_eq!(card.size.width, px(393.));
            let list = cx.debug_bounds("palette-list").unwrap();
            assert!(list.size.height > px(0.));
            assert_eq!(list.bottom(), px(452.), "the list stays above the keyboard");
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
                assert!(
                    state.read(cx).palette_open,
                    "Back dismisses the keyboard first"
                );
            });
            crate::window_seam::occlude_for_test(cx, gpui::Edges::default());
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
            });
            assert!(!state.read_with(cx, |state, _| state.palette_open));
            state.update(cx, |state, cx| state.go(Destination::Thread, cx));
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
        }
        // With motion reduced the page lands on the seam at once.
        cx.update(|_, cx| cx.set_reduce_motion(true));
        let uncovered = cx.debug_bounds("compact-open-palette").unwrap().top();
        crate::window_seam::occlude_for_test(
            cx,
            gpui::Edges {
                top: px(47.),
                ..Default::default()
            },
        );
        draw(cx);
        assert_eq!(
            cx.debug_bounds("compact-open-palette").unwrap().top(),
            uncovered + px(47.)
        );
    }

    /// What a phone's or a tablet's status bar, home indicator and side
    /// insets cover is the surface beneath them, painted to the window's
    /// edges in either theme: a phone's page, a tablet's sidebar or reading
    /// column. Never a band of another colour, which leaves the page
    /// looking cut off above the screen's bottom.
    #[gpui::test]
    fn the_safe_areas_show_the_page_they_cover(cx: &mut TestAppContext) {
        use tcode_core::settings::ThemeMode;

        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        let topmost = |at: gpui::Point<Pixels>, cx: &mut VisualTestContext| {
            cx.update(|window, _| {
                let at = at.scale(window.scale_factor());
                crate::test_frames::painted_quads(window)
                    .into_iter()
                    .filter(|quad| {
                        quad.bounds.contains(&at) && quad.content_mask.bounds.contains(&at)
                    })
                    .filter(|quad| quad.background.as_solid().is_some_and(|fill| fill.a > 0.))
                    .max_by_key(|quad| quad.order)
                    .and_then(|quad| quad.background.as_solid())
            })
        };
        let portrait = gpui::Edges {
            top: px(59.),
            bottom: px(34.),
            ..Default::default()
        };
        let landscape = gpui::Edges {
            left: px(59.),
            right: px(59.),
            bottom: px(21.),
            ..Default::default()
        };
        let tablet = gpui::Edges {
            top: px(24.),
            bottom: px(20.),
            ..Default::default()
        };
        // Each point covered, and whether the sidebar (or Settings' rail),
        // which is clear over the window's canvas, is the surface there
        // rather than the page or reading column.
        let geometries = [
            (
                (393., 852.),
                portrait,
                vec![((196., 20.), false), ((196., 840.), false)],
            ),
            (
                (852., 393.),
                landscape,
                vec![
                    ((20., 196.), false),
                    ((832., 196.), false),
                    ((426., 385.), false),
                ],
            ),
            (
                (1024., 1366.),
                tablet,
                vec![
                    ((40., 10.), true),
                    ((40., 1356.), true),
                    ((700., 10.), false),
                    ((700., 1356.), false),
                ],
            ),
        ];
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            cx.update(|window, cx| {
                crate::settings_page::apply_theme(mode, window, cx);
            });
            for destination in [
                Destination::Thread,
                Destination::Hosts,
                Destination::Settings,
            ] {
                state.update(cx, |state, cx| state.go(destination, cx));
                for ((width, height), insets, covered) in &geometries {
                    cx.simulate_resize(size(px(*width), px(*height)));
                    crate::window_seam::occlude_for_test(cx, *insets);
                    crate::test_frames::draw_settled(cx);
                    let (page, sidebar) = cx.update(|_, cx| {
                        (crate::material::content_surface(cx), cx.theme().background)
                    });
                    for ((x, y), beside) in covered {
                        assert_eq!(
                            topmost(gpui::point(px(*x), px(*y)), cx),
                            Some(if *beside { sidebar } else { page }),
                            "{mode:?} {destination:?} at ({x}, {y}) of {width}x{height}"
                        );
                    }
                }
            }
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        frame(cx, |_, _| ());
    }

    /// Draw a frame and `read` it as the screen shows it until the next one:
    /// before the work it deferred runs, after which the test platform draws
    /// again at once.
    fn frame<R>(cx: &mut VisualTestContext, read: impl FnOnce(&mut Window, &mut App) -> R) -> R {
        cx.run_until_parked();
        cx.update(|window, cx| {
            // A frame that reuses any view keeps every debug bound of the
            // previous one, so an element no longer painted would still be found.
            window.refresh();
            _ = window.draw(cx);
            read(window, cx)
        })
    }

    fn resize(cx: &mut VisualTestContext, width: f32) {
        cx.simulate_resize(size(px(width), px(800.)));
        draw(cx);
    }

    /// Make the test app a mobile build: only there is a phone-sized window
    /// compact, a desktop window being wide at any width.
    fn as_mobile(cx: &mut VisualTestContext) {
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, true));
    }

    fn touch_drag(
        cx: &mut VisualTestContext,
        id: u64,
        steps: &[(gpui::TouchPhase, f32, f32, u64)],
    ) {
        for &(phase, x, y, elapsed) in steps {
            cx.executor().advance_clock(Duration::from_millis(elapsed));
            cx.update(|window, cx| {
                window.dispatch_event(
                    gpui::PlatformInput::Touch(gpui::TouchEvent {
                        id: gpui::TouchId(id),
                        phase,
                        position: gpui::point(px(x), px(y)),
                        predicted_position: None,
                        force: None,
                    }),
                    cx,
                );
            });
            draw(cx);
        }
    }

    #[gpui::test]
    fn edge_swipe_dismisses_sheet_before_navigating_underneath(cx: &mut TestAppContext) {
        use gpui::TouchPhase::{Ended, Moved, Started};
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        cx.update(|window, cx| shell.update(cx, |shell, cx| while shell.back(window, cx) {}));
        draw(cx);
        assert!(!cx.cx.update(can_swipe_back));
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        let history = state.read_with(cx, |state, _| state.history().to_vec());
        for (id, end_x, elapsed, closes) in [
            (1, 70., 400, false),
            (2, 180., 400, true),
            (3, 70., 40, true),
        ] {
            state.update(cx, |state, cx| state.open_palette(cx));
            draw(cx);
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
            assert!(cx.cx.update(can_swipe_back));
            let resting = cx.debug_bounds("palette-card").unwrap();
            touch_drag(
                cx,
                id,
                &[
                    (Started, 10., 600., 0),
                    (Moved, (10. + end_x) / 2., 600., elapsed / 2),
                    (Moved, end_x, 600., elapsed / 2),
                ],
            );
            let dragged = cx.debug_bounds("palette-card").unwrap();
            assert!(
                dragged.top() > resting.top(),
                "sheet follows the edge gesture vertically"
            );
            touch_drag(cx, id, &[(Ended, end_x, 600., 10)]);
            draw(cx);
            cx.executor().advance_clock(Duration::from_millis(500));
            draw(cx);
            assert_eq!(
                state.read_with(cx, |state, _| state.palette_open),
                !closes,
                "gesture {id}"
            );
            assert_eq!(
                state.read_with(cx, |state, _| state.history().to_vec()),
                history
            );
            if !closes {
                assert_eq!(cx.debug_bounds("palette-card").unwrap(), resting);
                state.update(cx, |state, cx| state.close_palette(cx));
                draw(cx);
                cx.executor().advance_clock(Duration::from_millis(250));
                draw(cx);
            } else {
                assert!(cx.debug_bounds("palette-card").is_none());
            }
        }
        let depth = shell.read_with(cx, |shell, cx| shell.nav.read(cx).depth());
        assert!(cx.cx.update(handle_back));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.nav.read(cx).depth(), depth - 1)
        });
    }

    #[gpui::test]
    fn image_viewer_takes_back_before_the_page_underneath(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        assert!(cx.cx.update(can_swipe_back));
        let depth = shell.read_with(cx, |shell, cx| shell.nav.read(cx).depth());
        cx.update(|window, cx| {
            crate::image_viewer::open(
                gpui::ImageSource::Custom(std::sync::Arc::new(|_, _| None)),
                "image.png",
                window,
                cx,
            )
        });
        draw(cx);
        assert!(!cx.cx.update(can_swipe_back));
        assert!(cx.cx.update(handle_back));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        assert!(!cx.update(|window, cx| crate::overlay::dialog_open(window, cx)));
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.nav.read(cx).depth(), depth)
        });
        assert!(cx.cx.update(can_swipe_back));
    }

    #[gpui::test]
    fn compact_edge_swipe_tracks_cancels_and_completes_before_next_tap(cx: &mut TestAppContext) {
        use gpui::TouchPhase::{Cancelled, Ended, Moved, Started};

        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        let root = shell.read_with(cx, |shell, cx| shell.nav.read(cx).depth());
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            "/tmp/tcode-compact-panel".into(),
            None,
        );
        meta.id = "thread-1".into();
        for (topic, event) in [
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![meta],
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let conversation = (0..24)
            .flat_map(|index| {
                [
                    agent::AgentEvent::TurnStarted {
                        turn_id: format!("scroll-{index}"),
                    },
                    agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                        id: format!("message-{index}"),
                        parent_item_id: None,
                        content: agent::ItemContent::UserMessage {
                            text: format!("Keep reading position {index}\n").repeat(4),
                            context_len: None,
                            attachments: Vec::new(),
                        },
                    }),
                    agent::AgentEvent::TurnCompleted {
                        turn_id: format!("scroll-{index}"),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    },
                ]
            })
            .collect();
        seed_wide_diff_with(&shell, &host, cx, conversation);
        draw(cx);
        let chat = shell.read_with(cx, |shell, cx| shell.attachment.as_ref().unwrap().chat(cx));
        let mut reading_position = None;
        for destination in [Destination::Thread, Destination::Panel] {
            shell.update(cx, |shell, cx| shell.go(destination, cx));
            draw(cx);
            cx.executor().advance_clock(Duration::from_millis(250));
            draw(cx);
            if destination == Destination::Thread {
                let viewport = chat.read_with(cx, |chat, _| chat.capture_viewport().unwrap());
                cx.simulate_event(gpui::ScrollEvent {
                    position: viewport.center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(1000.))),
                    ..Default::default()
                });
                draw(cx);
                cx.executor().advance_clock(Duration::from_secs(1));
                draw(cx);
                assert!(
                    cx.debug_bounds("scroll-to-end").is_some(),
                    "fixture scrolls away from the conversation tail"
                );
                reading_position = (0..100).find_map(|index| {
                    let selector: &'static str = format!("timeline-row-{index}").leak();
                    let bounds = cx.debug_bounds(selector)?;
                    (bounds.top() >= viewport.top() && bounds.top() < viewport.bottom())
                        .then_some((selector, bounds.top()))
                });
                assert!(
                    reading_position.is_some(),
                    "a scrolled conversation row is visible"
                );
            }
        }
        let (reading_row, reading_top) = reading_position.unwrap();
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        let history = state.read_with(cx, |state, _| state.history().to_vec());
        let composer = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .unwrap()
                .chat(cx)
                .read(cx)
                .composer()
        });
        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| {
                composer.set_draft("keep this draft", window, cx);
            })
        });
        draw_until(&shell, cx, &host, "diff-body", None);
        let line = "diff-line-new-a/deeply/nested/module/tree/with/an/absurdly/long/file_name.rs-1";
        for (id, mode, end_x, elapsed, commits) in [
            (3, crate::theme::ThemeMode::Light, 70., 400, false),
            (4, crate::theme::ThemeMode::Light, 180., 400, true),
            (5, crate::theme::ThemeMode::Light, 70., 40, true),
            (6, crate::theme::ThemeMode::Dark, 70., 400, false),
            (7, crate::theme::ThemeMode::Dark, 180., 400, true),
            (8, crate::theme::ThemeMode::Dark, 70., 40, true),
        ] {
            cx.update(|window, cx| crate::theme::change_mode(mode, Some(window), cx));
            draw(cx);
            let initial_line = cx.debug_bounds(line).expect("diff line before scrolling");
            let body = cx.debug_bounds("diff-body").unwrap();
            cx.simulate_event(gpui::ScrollEvent {
                position: body.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(-120.), px(0.))),
                ..Default::default()
            });
            draw(cx);
            cx.executor().advance_clock(Duration::from_secs(1));
            draw(cx);
            let before_swipe = cx.debug_bounds(line).expect("diff before gesture");
            assert!(
                before_swipe.left() < initial_line.left(),
                "fixture scrolls real content"
            );
            touch_drag(
                cx,
                id,
                &[
                    (Started, 10., 300., 0),
                    (Moved, 40., 300., elapsed / 2),
                    (Moved, end_x, 300., elapsed / 2),
                ],
            );
            let current = cx
                .debug_bounds("back-current-page")
                .expect("page follows finger");
            assert!((current.left() - px(end_x - 10.)).abs() < px(1.));
            let previous = cx
                .debug_bounds("back-previous-page")
                .expect("real previous page");
            assert!(previous.left() < px(0.) && previous.right() < px(393.));
            assert!(cx.debug_bounds("compact-thread-page").is_some());
            assert!(
                cx.debug_bounds("chat-composer").is_some(),
                "underlying composer is visible"
            );
            assert_eq!(
                cx.debug_bounds(reading_row).unwrap().top(),
                reading_top,
                "previous page keeps its scroll position"
            );
            assert_eq!(
                state.read_with(cx, |state, _| state.history().to_vec()),
                history
            );
            touch_drag(cx, id, &[(Ended, end_x, 300., 10)]);
            assert_eq!(
                cx.debug_bounds("back-current-page").unwrap(),
                current,
                "release starts at the finger"
            );
            cx.executor().advance_clock(Duration::from_secs(1));
            draw(cx);
            assert!(cx.debug_bounds("back-current-page").is_none());
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "keep this draft"
            );
            if commits {
                assert_eq!(
                    state.read_with(cx, |state, _| state.destination()),
                    Destination::Thread
                );
                shell.update(cx, |shell, cx| shell.go(Destination::Panel, cx));
                draw(cx);
                cx.executor().advance_clock(Duration::from_millis(250));
                draw(cx);
            } else {
                assert_eq!(
                    cx.debug_bounds(line).unwrap(),
                    before_swipe,
                    "cancel keeps scroll position for gesture {id}"
                );
                assert_eq!(
                    state.read_with(cx, |state, _| state.history().to_vec()),
                    history
                );
            }
        }
        touch_drag(cx, 9, &[(Started, 10., 300., 0), (Moved, 70., 300., 200)]);
        assert!(cx.debug_bounds("back-current-page").is_some());
        resize(cx, 1024.);
        resize(cx, 393.);
        touch_drag(cx, 9, &[(Cancelled, 70., 300., 0)]);
        assert!(cx.debug_bounds("back-current-page").is_none());
        assert_eq!(
            state.read_with(cx, |state, _| state.history().to_vec()),
            history
        );
        // Touch hardware reports a moving finger every 8–16ms, so the release
        // carries a fling velocity.
        let mut swipe = vec![(Started, 10., 300., 0)];
        swipe.extend((1..=10).map(|step| (Moved, 10. + 20. * step as f32, 300., 8)));
        swipe.push((Ended, 210., 300., 8));
        touch_drag(cx, 1, &swipe);
        cx.executor().advance_clock(Duration::from_millis(300));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.nav.read(cx).depth(), root + 1)
        });
        let back = cx
            .debug_bounds("compact-back")
            .expect("Back on revealed page");
        let x = f32::from(back.center().x);
        let y = f32::from(back.center().y);
        touch_drag(cx, 2, &[(Started, x, y, 0), (Ended, x, y, 50)]);
        shell.read_with(cx, |shell, cx| assert_eq!(shell.nav.read(cx).depth(), root));
    }

    #[gpui::test]
    fn compact_touch_drag_away_from_edge_preserves_page(cx: &mut TestAppContext) {
        use gpui::TouchPhase::{Ended, Moved, Started};

        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        let before = shell.read_with(cx, |shell, cx| {
            shell.window_state.read(cx).history().to_vec()
        });
        for (id, start_x, middle_x) in [(1, 60., 240.), (2, 60., 10.)] {
            // Catch an edge pan's fling with the next off-edge contact. GPUI
            // starts that contact directly as a pan, without a TouchDrag offer.
            touch_drag(
                cx,
                10 + id,
                &[
                    (Started, 10., 300., 0),
                    (Moved, 25., 300., 10),
                    (Ended, 25., 300., 10),
                ],
            );
            touch_drag(
                cx,
                id,
                &[
                    (Started, start_x, 300., 0),
                    (Moved, middle_x, 300., 50),
                    (Moved, 280., 300., 50),
                    (Ended, 280., 300., 10),
                ],
            );
            shell.read_with(cx, |shell, cx| {
                assert_eq!(shell.window_state.read(cx).history(), before);
                assert_eq!(shell.nav.read(cx).depth(), before.len());
            });
        }
    }

    #[gpui::test]
    fn compact_tap_push_and_back_pop_slide(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        cx.executor().advance_clock(Duration::from_millis(250));
        draw(cx);
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(100));
        draw(cx);
        let incoming = cx
            .debug_bounds("compact-thread-page")
            .expect("incoming thread");
        let outgoing = cx
            .debug_bounds("compact-threads-page")
            .expect("outgoing list");
        assert!(incoming.left() > px(0.) && incoming.left() < px(393.));
        assert!(outgoing.left() < px(0.), "push moves the list left");
        cx.executor().advance_clock(Duration::from_millis(150));
        draw(cx);
        assert_eq!(
            cx.debug_bounds("compact-thread-page").unwrap().left(),
            px(0.)
        );
        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);
        cx.executor().advance_clock(Duration::from_millis(100));
        draw(cx);
        let outgoing = cx
            .debug_bounds("compact-thread-page")
            .expect("outgoing thread");
        let incoming = cx
            .debug_bounds("compact-threads-page")
            .expect("incoming list");
        assert!(outgoing.left() > px(0.) && outgoing.left() < px(393.));
        assert!(
            incoming.left() < px(0.),
            "pop reveals the list from the left"
        );
    }

    /// Every line the client has sent since the last drain.
    fn sent(host: &MountedShell) -> Vec<ClientPayload> {
        let mut payloads: Vec<_> = host
            .observed
            .borrow_mut()
            .drain(..)
            .map(|message| message.payload)
            .collect();
        while let Ok(line) = host.outgoing.try_recv() {
            payloads.push(decode_client_line(&line).expect("client line").payload);
        }
        payloads
    }

    fn store_of(shell: &Entity<AppShell>, cx: &VisualTestContext) -> Entity<WorkspaceStore> {
        shell.read_with(cx, |shell, _| shell.store().expect("attached"))
    }

    /// A long title takes the header's free width; it is cut only once the
    /// header runs out of room, whether or not a pull request is linked.
    #[gpui::test]
    fn a_long_title_uses_the_headers_free_width(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("long-title");
        let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
        let project = tcode_core::project::Project::from_root(root.join("project"));
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            project.root.clone(),
            None,
        );
        meta.id = "first".into();
        meta.title = "Investigate why the composer drops its stop button mid-turn".into();
        meta.project_id = Some(project.id.clone());
        let host =
            tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
                .unwrap();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = vec![meta].into();
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let (shell, _transport, cx) = mount(cx);
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            *store = WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            );
            store.show_thread("first".into(), cx);
        });
        driver.wait_until(cx, &store, "selected thread baseline", |cx| {
            store.read_with(cx, |store, cx| {
                store.baseline_ready()
                    && !store.threads_loading()
                    && !store.chat_loading_for(Some("first"), cx)
            })
        });
        resize(cx, 1600.);
        shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
        draw(cx);
        let title = cx.debug_bounds("chat-title").expect("thread title");
        assert!(
            title.size.width > px(300.),
            "the title is cut to {:?} in a 1600px window",
            title.size.width
        );
        driver.shutdown(cx, &store, &host);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The model picker raises the keyboard only where it cannot cover the
    /// list: a desktop window (even a narrow one) focuses the search as it
    /// opens; a phone leaves it unfocused, with the picker still owning focus
    /// so Escape closes it.
    #[gpui::test]
    fn model_picker_focuses_its_search_only_off_phone(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("model-picker-focus");
        let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
        let project = tcode_core::project::Project::from_root(root.join("project"));
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            project.root.clone(),
            None,
        );
        meta.id = "first".into();
        meta.project_id = Some(project.id.clone());
        let host =
            tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
                .unwrap();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = vec![meta].into();
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let (shell, _transport, cx) = mount(cx);
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            *store = WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            );
            store.show_thread("first".into(), cx);
        });
        driver.wait_until(cx, &store, "selected thread baseline", |cx| {
            store.read_with(cx, |store, cx| {
                store.baseline_ready()
                    && !store.threads_loading()
                    && !store.chat_loading_for(Some("first"), cx)
            })
        });
        let search = shell.read_with(cx, |shell, cx| {
            let chat = shell.attachment.as_ref().unwrap().chat(cx).read(cx);
            chat.composer().read(cx).model_search_focus_handle(cx)
        });

        for (mobile, width) in [(false, 1200.), (false, 393.), (true, 393.)] {
            cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, mobile));
            resize(cx, width);
            shell.update(cx, |shell, cx| shell.go(Destination::Thread, cx));
            // Let the compact push animation settle so the picker is in place.
            cx.executor().advance_clock(Duration::from_millis(400));
            draw(cx);
            let picker = cx.debug_bounds("model-picker").expect("model picker");
            cx.simulate_click(picker.center(), gpui::Modifiers::default());
            draw(cx);
            draw(cx);
            let compact = shell.read_with(cx, |shell, cx| shell.compact(cx));
            assert_eq!(
                cx.debug_bounds("touch-picker-sheet").is_some(),
                compact,
                "mobile={mobile} width={width}: a compact window opens a sheet"
            );
            cx.update(|window, _| {
                assert_eq!(
                    search.is_focused(window),
                    !mobile,
                    "mobile={mobile} width={width}: the search raises the keyboard only off-phone"
                );
            });
            cx.simulate_keystrokes("escape");
            draw(cx);
            draw(cx);
            assert!(
                cx.debug_bounds("touch-picker-sheet").is_none(),
                "mobile={mobile} width={width}: Escape reaches the picker through its focus"
            );
            cx.update(|window, _| assert!(!search.is_focused(window)));
        }
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        driver.shutdown(cx, &store, &host);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn thread_shortcuts_follow_list_order_and_leave_model_picker_numbers_alone(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("thread-shortcuts");
        let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
        let project = tcode_core::project::Project::from_root(root.join("project"));
        let sessions: Vec<_> = [("third", 10), ("first", 30), ("second", 20)]
            .into_iter()
            .map(|(id, updated_at)| {
                let mut meta = tcode_core::project::SessionMeta::new(
                    agent::ProviderKind::Codex,
                    project.root.clone(),
                    None,
                );
                meta.id = id.into();
                meta.project_id = Some(project.id.clone());
                meta.updated_at = updated_at;
                meta
            })
            .collect();
        let host =
            tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
                .unwrap();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.projects = vec![project];
            state.sessions = sessions.into();
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let (shell, _transport, cx) = mount(cx);
        cx.update(|_, cx| crate::shortcut::init(cx));
        cx.update(|window, cx| set_back_target(window.window_handle(), &shell, cx));
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            *store = WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            );
            store.show_thread("first".into(), cx);
        });
        driver.wait_until(cx, &store, "selected thread baseline", |cx| {
            store.read_with(cx, |store, cx| {
                store.baseline_ready()
                    && !store.threads_loading()
                    && !store.chat_loading_for(Some("first"), cx)
            })
        });
        resize(cx, 1200.);
        draw(cx);

        let [first_key, second_key, out_of_range_key] = if cfg!(target_os = "macos") {
            ["cmd-1", "cmd-2", "cmd-9"]
        } else {
            ["ctrl-1", "ctrl-2", "ctrl-9"]
        };
        let picker = cx.debug_bounds("model-picker").expect("model picker");
        cx.simulate_click(picker.center(), gpui::Modifiers::default());
        draw(cx);
        cx.simulate_keystrokes(second_key);
        draw(cx);
        assert_eq!(
            shell
                .read_with(cx, |shell, cx| shell.page_thread(cx))
                .as_deref(),
            Some("first"),
            "model picker must keep number keys"
        );
        cx.simulate_keystrokes("escape");
        draw(cx);

        for (keys, expected) in [
            (second_key, "second"),
            ("ctrl-tab", "third"),
            ("ctrl-tab", "first"),
            ("ctrl-shift-tab", "third"),
            (out_of_range_key, "third"),
            (first_key, "first"),
        ] {
            cx.simulate_keystrokes(keys);
            driver.wait_until(cx, &store, "selected thread baseline", |cx| {
                store.read_with(cx, |store, cx| {
                    store.baseline_ready()
                        && !store.threads_loading()
                        && !store.chat_loading_for(Some(expected), cx)
                })
            });
            assert_eq!(
                shell
                    .read_with(cx, |shell, cx| shell.page_thread(cx))
                    .as_deref(),
                Some(expected),
                "{keys}"
            );
        }
        cx.update(|window, cx| window.blur(cx));
        draw(cx);
        cx.simulate_keystrokes(second_key);
        assert_eq!(
            shell
                .read_with(cx, |shell, cx| shell.page_thread(cx))
                .as_deref(),
            Some("second"),
            "shortcuts work without a focused input"
        );
        shell.update(cx, |shell, cx| shell.go(Destination::Settings, cx));
        draw(cx);
        cx.simulate_keystrokes(first_key);
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.destination(cx)),
            Destination::Thread
        );
        as_mobile(cx);
        resize(cx, 393.);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.compact(cx));
            assert_eq!(shell.destination(cx), Destination::Thread);
        });
        cx.simulate_keystrokes("ctrl-tab");
        assert_eq!(
            shell
                .read_with(cx, |shell, cx| shell.page_thread(cx))
                .as_deref(),
            Some("second"),
            "compact navigation uses the same shortcuts"
        );
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        resize(cx, 1200.);
        draw(cx);
        let row = cx
            .debug_bounds("sidebar-thread-second")
            .expect("second row");
        let settle = gpui::point(row.right() - px(18.), row.top() + px(14.));
        cx.simulate_mouse_move(settle, None, gpui::Modifiers::default());
        draw(cx);
        cx.simulate_click(settle, gpui::Modifiers::default());
        driver.wait_until(cx, &store, "thread settlement update", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .sidebar_sessions()
                    .iter()
                    .any(|meta| meta.id == "second" && meta.is_settled())
            })
        });
        draw(cx);
        assert!(cx.update(|window, cx| window.has_notification::<crate::sidebar::ThreadUndo>(cx)));
        let undo_key = if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        };
        let composer = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .unwrap()
                .chat(cx)
                .read(cx)
                .composer()
        });
        let input = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
        cx.update(|window, cx| input.focus(window, cx));
        draw(cx);
        cx.simulate_keystrokes("x");
        draw(cx);
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "x"
        );
        cx.simulate_keystrokes(undo_key);
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            ""
        );
        assert!(store.read_with(cx, |store, _| {
            store
                .sidebar_sessions()
                .iter()
                .any(|meta| meta.id == "second" && meta.is_settled())
        }));
        cx.update(|window, cx| window.blur(cx));
        draw(cx);
        cx.simulate_keystrokes(undo_key);
        driver.wait_until(cx, &store, "thread settlement update", |cx| {
            store.read_with(cx, |store, _| {
                store
                    .sidebar_sessions()
                    .iter()
                    .any(|meta| meta.id == "second" && !meta.is_settled())
            })
        });
        assert!(!cx.update(|window, cx| window.has_notification::<crate::sidebar::ThreadUndo>(cx)));
        driver.shutdown(cx, &store, &host);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A desktop tiled to half a screen is still the whole product: the width
    /// rule belongs to mobile builds, so no desktop width is compact.
    #[gpui::test]
    fn a_narrow_desktop_window_keeps_the_wide_workspace(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        for width in [1024., 899., 400.] {
            resize(cx, width);
            shell.read_with(cx, |shell, cx| {
                assert!(!shell.compact(cx), "{width}px on a desktop build is wide");
                assert_eq!(shell.window_state.read(cx).route(), Route::Chat);
            });
            assert!(
                cx.debug_bounds("sidebar-feature-hosts").is_some(),
                "{width}px keeps the workspace sidebar"
            );
            assert!(cx.debug_bounds("compact-threads-page").is_none());
        }
    }

    /// Rotating a tablet across the breakpoint is a layout decision and
    /// nothing else: it must not touch the attachment, the selected thread,
    /// the draft or the split.
    #[gpui::test]
    fn the_breakpoint_flips_at_nine_hundred_and_disturbs_nothing_else(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        as_mobile(cx);
        let store = store_of(&shell, cx);
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Index,
                    event: ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        sessions: Vec::new(),
                        projects: Vec::new(),
                    }),
                }))
                .unwrap(),
            )
            .unwrap();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        // The thread is opened and the host describes it, so the chat draws
        // it with its composer.
        deliver_thread(&shell, &host, "thread-1", false, cx);

        resize(cx, 1024.);
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.compact(cx), "1024px is the wide split");
        });
        draw(cx);
        draw(cx);
        let from = gpui::point(px(SIDEBAR_WIDTH), px(400.));
        let to = gpui::point(px(300.), px(400.));
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
        for step in [from + gpui::point(px(5.), px(0.)), to] {
            cx.simulate_mouse_move(step, Some(MouseButton::Left), gpui::Modifiers::default());
            draw(cx);
        }
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            let attachment = shell.attachment.as_ref().unwrap();
            assert_eq!(attachment.split.read(cx).sizes()[0], px(300.));
            assert_eq!(attachment.sidebar_width.get(), px(300.));
        });
        let composer = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .chat(cx)
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
                shell.destination(cx),
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
            shell.read_with(cx, |shell, cx| shell.page_thread(cx)),
            Some("thread-1".into())
        );
        composer.read_with(cx, |composer, cx| {
            assert_eq!(composer.draft(cx), "half a thought");
        });
        assert!(
            cx.debug_bounds("composer-settings-drawer").is_some(),
            "the composer followed the layout: a compact one carries the settings drawer"
        );
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell
                .attachment
                .as_ref()
                .unwrap()
                .chat(cx)
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

    /// The one Back chain: overlays first, then one step of the window's
    /// history, and `false` only at the root — where the platform closes the
    /// app. Leaving a page never touches the attachment.
    #[cfg(preview_backend)]
    #[gpui::test]
    fn compact_preview_release_and_navigation(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, _, cx) = mount_restored(cx, &["hosts", "threads", "thread"], true);
        let store = store_of(&shell, cx);
        restore_index(&shell, &host, true, cx);
        restore_status(&shell, &host, cx);
        await_restore_update(&shell, cx, |store, cx| {
            store.preview_key_for("thread-a", cx).is_some()
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        draw(cx);
        let (preview, state) = shell.read_with(cx, |shell, _| {
            (
                shell.attachment.as_ref().unwrap().preview.clone(),
                shell.window_state(),
            )
        });
        let page_pane = shell.read_with(cx, |shell, _| {
            shell.attachment.as_ref().unwrap().current_pane.clone()
        });
        let lifecycle = preview.read_with(cx, |preview, _| preview.lifecycle());
        let key = store.read_with(cx, |store, cx| {
            store.preview_key_for("thread-a", cx).unwrap()
        });
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-a", tcode_core::ui::RightTab::Preview, cx);
            store.set_preview_url(&key, "https://example.com/".into(), cx);
        });
        lifecycle.update(cx, |lifecycle, _| lifecycle.seed_stub(&key));
        state.update(cx, |state, cx| state.go(Destination::Panel, cx));
        draw(cx);
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| assert!(shell.back(window, cx)));
        });
        draw(cx);
        assert!(lifecycle.read_with(cx, |lifecycle, _| lifecycle.has_view(&key)));
        assert_eq!(
            store
                .read_with(cx, |store, _| store.preview_url(&key))
                .as_deref(),
            Some("https://example.com/")
        );
        state.update(cx, |state, cx| state.go(Destination::Panel, cx));
        draw(cx);
        // Dispatch the same action as the compact overflow menu.
        cx.update(|window, cx| {
            let preview = page_pane.update(cx, |pane, cx| pane.drawer_preview(window, cx));
            preview.update(cx, |preview, cx| {
                preview.on_preview_action(&crate::preview_panel::PreviewAction::Close, window, cx)
            });
        });
        draw(cx);
        assert!(!lifecycle.read_with(cx, |lifecycle, _| lifecycle.has_view(&key)));
        assert_eq!(
            store.read_with(cx, |store, _| store.preview_url(&key)),
            None
        );
        assert_eq!(
            preview.read_with(cx, |preview, cx| preview.url_field(cx)),
            ""
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.destination()),
            Destination::Panel
        );
        store.update(cx, |store, cx| {
            store.set_preview_url(&key, "https://example.com/again".into(), cx)
        });
        lifecycle.update(cx, |lifecycle, _| lifecycle.seed_stub(&key));
        cx.update(|window, cx| shell.update(cx, |shell, cx| assert!(shell.back(window, cx))));
        draw(cx);
        cx.update(|window, cx| shell.update(cx, |shell, cx| assert!(shell.back(window, cx))));
        draw(cx);
        assert_eq!(
            state.read_with(cx, |state, _| state.destination()),
            Destination::Threads
        );
        assert!(!lifecycle.read_with(cx, |lifecycle, _| lifecycle.has_view(&key)));
        assert_eq!(
            store.read_with(cx, |store, _| store.preview_url(&key)),
            None
        );
    }

    #[gpui::test]
    fn back_unwinds_overlays_then_the_history_and_stops_at_the_root(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
        as_mobile(cx);
        resize(cx, 393.);

        let back = |cx: &mut VisualTestContext| {
            let consumed =
                cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx)));
            draw(cx);
            consumed
        };

        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Thread)
        });

        // An open overlay outranks navigation.
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        window_state.update(cx, |state, cx| state.open_palette(cx));
        assert!(back(cx), "back closes the overlay");
        shell.read_with(cx, |shell, cx| {
            assert!(!shell.window_state.read(cx).palette_open);
            assert_eq!(
                shell.destination(cx),
                Destination::Thread,
                "and nothing else"
            );
        });

        // Thread → Threads → Hosts. Neither step detaches: only connecting
        // somewhere else, or an explicit Disconnect, does that.
        assert!(back(cx));
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Threads);
            assert!(shell.attachment.is_some(), "still attached");
        });
        assert!(back(cx));
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Hosts);
            assert!(
                shell.attachment.is_some(),
                "walking back to the hosts list is not a decision to leave the host"
            );
        });

        window_state.update(cx, |state, cx| state.go(Destination::Pair, cx));
        draw(cx);
        assert!(back(cx));
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.destination(cx)),
            Destination::Hosts
        );
        assert!(!back(cx), "the root belongs to the platform");
    }

    /// Settings is two pages of that same history, not a route with a back row
    /// of its own: detail → root → whatever Settings was opened from.
    #[gpui::test]
    fn back_leaves_a_settings_section_then_settings_then_the_page_below(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 393.);
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());

        window_state.update(cx, |state, cx| state.open_settings(cx));
        draw(cx);
        let general = cx
            .debug_bounds("settings-nav-general")
            .expect("General settings row");
        cx.simulate_click(general.center(), gpui::Modifiers::default());
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::SettingsSection)
        });

        let back = |cx: &mut VisualTestContext| {
            let consumed =
                cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx)));
            draw(cx);
            consumed
        };
        assert!(back(cx));
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Settings)
        });
        assert!(back(cx));
        shell.read_with(cx, |shell, cx| {
            assert_eq!(
                shell.destination(cx),
                Destination::Threads,
                "settings returns to the page it was opened from"
            );
        });
    }

    /// Wide, the pairing page is pushed over Machines, and its header's Back
    /// returns to Machines rather than leaving the Hosts route; Machines, a
    /// root the sidebar switches to, has no Back. What was pasted there, and
    /// the error it drew, stay behind on the page that was left.
    #[gpui::test]
    fn the_wide_pair_page_goes_back_to_machines(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        resize(cx, 1024.);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
        draw(cx);
        assert!(
            cx.debug_bounds("hosts-back").is_none(),
            "Machines is a root"
        );
        let paste = cx
            .debug_bounds("hosts-pair")
            .expect("Machines offers pasting an invitation");
        cx.simulate_click(paste.center(), gpui::Modifiers::default());
        draw(cx);
        cx.simulate_input("not an invitation");
        draw(cx);
        assert!(
            cx.debug_bounds("hosts-pair-error").is_some(),
            "the paste page flags what is not an invitation"
        );
        // Back widens in with the pairing page; it is pressed once there.
        crate::test_frames::draw_settled(cx);
        let back = cx
            .debug_bounds("hosts-back")
            .expect("the pairing page offers Back");
        cx.simulate_click(back.center(), gpui::Modifiers::default());
        // The pairing page fades out over Machines, drawn as it was.
        crate::test_frames::draw_settled(cx);
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.destination(cx)),
            Destination::Hosts
        );
        assert!(cx.debug_bounds("hosts-route").is_some());
        assert!(
            cx.debug_bounds("hosts-pair-error").is_none(),
            "the error does not follow the user back to Machines"
        );
    }

    /// A store selection can come from the sidebar or palette. In wide layout
    /// it switches the content column away from Hosts without requiring Back.
    #[gpui::test]
    fn wide_hosts_route_keeps_its_sidebar_until_a_thread_selection(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        resize(cx, 1024.);
        draw(cx);
        for thread in ["thread-1", "thread-2"] {
            let feature = cx
                .debug_bounds("sidebar-feature-hosts")
                .expect("the Machines feature row is visible");
            cx.simulate_mouse_down(
                feature.center(),
                gpui::MouseButton::Left,
                gpui::Modifiers::default(),
            );
            cx.simulate_mouse_up(
                feature.center(),
                gpui::MouseButton::Left,
                gpui::Modifiers::default(),
            );
            draw(cx);
            assert!(cx.debug_bounds("hosts-route").is_some());
            assert!(cx.debug_bounds("sidebar-feature-hosts").is_some());
            assert_eq!(
                shell.read_with(cx, |shell, cx| shell.window_state.read(cx).route()),
                Route::Hosts
            );

            let store = store_of(&shell, cx);
            store.update(cx, |store, cx| {
                store.show_thread(thread.into(), cx);
            });
            crate::test_frames::draw_settled(cx);

            shell.read_with(cx, |shell, cx| {
                assert_eq!(shell.window_state.read(cx).route(), Route::Chat);
            });
            assert!(
                cx.debug_bounds("hosts-route").is_none(),
                "the chat content replaces Machines as soon as the store selection changes"
            );
            assert_eq!(
                shell.read_with(cx, |shell, cx| shell
                    .window_state
                    .read(cx)
                    .history()
                    .to_vec()),
                vec![Destination::Hosts, Destination::Thread]
            );
        }
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
        draw(cx);
        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        crate::test_frames::draw_settled(cx);
        assert_eq!(state.read_with(cx, |state, _| state.route()), Route::Chat);
        assert!(!cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));

        // The control toggles: on Machines, or on Pair over it, a click returns
        // to the thread the window was showing.
        let click_control = |cx: &mut VisualTestContext| {
            let control = cx
                .debug_bounds("sidebar-feature-hosts")
                .expect("the Machines control is visible");
            cx.simulate_click(control.center(), gpui::Modifiers::default());
            crate::test_frames::draw_settled(cx);
        };
        for pairing in [false, true] {
            click_control(cx);
            assert_eq!(state.read_with(cx, |state, _| state.route()), Route::Hosts);
            if pairing {
                state.update(cx, |state, cx| state.go(Destination::Pair, cx));
                draw(cx);
            }
            click_control(cx);
            assert_eq!(
                state.read_with(cx, |state, _| state.history().to_vec()),
                vec![Destination::Hosts, Destination::Thread],
                "pairing: {pairing}"
            );
            assert!(cx.debug_bounds("hosts-route").is_none());
        }
    }

    /// A wide route change is a change of scene: the route left stays
    /// beneath the one arriving while it rises, and then goes. The sidebar
    /// the workspace routes share holds still throughout, drawn once.
    #[gpui::test]
    fn a_wide_route_rises_over_the_route_it_leaves(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        resize(cx, 1024.);
        crate::test_frames::draw_settled(cx);
        let sidebar = cx
            .debug_bounds("sidebar-feature-hosts")
            .expect("the workspace sidebar");
        assert!(cx.debug_bounds("chat-route").is_some());
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        let half_way = |cx: &mut VisualTestContext| {
            draw(cx);
            cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
            draw(cx);
        };

        state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
        half_way(cx);
        assert!(cx.debug_bounds("hosts-route").is_some());
        assert!(
            cx.debug_bounds("chat-route").is_some(),
            "the chat is still beneath Machines"
        );
        assert_eq!(cx.debug_bounds("sidebar-feature-hosts"), Some(sidebar));
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("chat-route").is_none());
        assert_eq!(cx.debug_bounds("sidebar-feature-hosts"), Some(sidebar));

        state.update(cx, |state, cx| state.open_settings(cx));
        half_way(cx);
        assert!(cx.debug_bounds("settings-route").is_some());
        assert!(
            cx.debug_bounds("hosts-route").is_some()
                && cx.debug_bounds("sidebar-feature-hosts").is_some(),
            "Machines and its sidebar are still beneath Settings"
        );
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("hosts-route").is_none());
        assert!(cx.debug_bounds("sidebar-feature-hosts").is_none());
    }

    /// Attaching, the workspace rises over Machines; detaching, Machines rises
    /// over the workspace, whose views are let go once it is covered.
    #[gpui::test]
    fn attaching_and_detaching_rise_over_what_the_window_showed(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_initial(cx, None);
        resize(cx, 1024.);
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("hosts-page").is_some());
        let half_way = |cx: &mut VisualTestContext| {
            draw(cx);
            cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
            draw(cx);
        };

        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.switch_to(AttachmentTarget::Local, window, cx)
            })
        });
        half_way(cx);
        assert!(
            cx.debug_bounds("chat-route").is_some(),
            "the workspace rises"
        );
        assert!(cx.debug_bounds("hosts-page").is_some(), "over Machines");
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("hosts-page").is_none());

        let left = shell.read_with(cx, |shell, _| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .current_pane
                .downgrade()
        });
        shell.update(cx, |shell, cx| shell.detach(cx));
        half_way(cx);
        assert!(cx.debug_bounds("hosts-page").is_some(), "Machines rises");
        assert!(
            cx.debug_bounds("chat-route").is_some(),
            "over the workspace the window left"
        );
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("chat-route").is_none());
        assert!(left.upgrade().is_none(), "the workspace left is let go");
    }

    /// On Machines, the sidebar's column narrows away as it collapses and
    /// widens back as it expands.
    #[gpui::test]
    fn the_hosts_routes_sidebar_collapses_gradually(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        let store = store_of(&shell, cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        resize(cx, 1024.);
        state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
        crate::test_frames::draw_settled(cx);
        let left = |cx: &mut VisualTestContext| cx.debug_bounds("hosts-route").unwrap().left();
        let expanded = left(cx);
        assert_eq!(expanded, px(SIDEBAR_WIDTH));
        let half_way = |cx: &mut VisualTestContext| {
            draw(cx);
            cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
            draw(cx);
            left(cx)
        };

        state.update(cx, |state, cx| state.toggle_sidebar_collapsed(&store, cx));
        let collapsing = half_way(cx);
        assert!(
            collapsing > px(0.) && collapsing < expanded,
            "{collapsing:?}"
        );
        crate::test_frames::draw_settled(cx);
        assert_eq!(left(cx), px(0.));

        state.update(cx, |state, cx| state.toggle_sidebar_collapsed(&store, cx));
        let expanding = half_way(cx);
        assert!(expanding > px(0.) && expanding < expanded, "{expanding:?}");
        crate::test_frames::draw_settled(cx);
        assert_eq!(left(cx), expanded);
    }

    /// A tablet turning compact or wide moves its conversation from where
    /// one layout drew it to where the other does, rather than jumping.
    #[gpui::test]
    fn the_conversation_glides_across_the_breakpoint(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_crowded_thread(cx);
        as_mobile(cx);
        resize(cx, 1024.);
        draw_until(&shell, cx, &host, "chat-composer", None);
        crate::test_frames::draw_settled(cx);
        let left = |cx: &mut VisualTestContext| cx.debug_bounds("chat-composer").unwrap().left();
        let wide = left(cx);

        resize(cx, 899.);
        assert_eq!(left(cx), wide, "the conversation starts where it was");
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let gliding = left(cx);
        crate::test_frames::draw_settled(cx);
        let compact = left(cx);
        assert!(
            compact < gliding && gliding < wide,
            "{gliding:?} between {compact:?} and {wide:?}"
        );

        resize(cx, 1024.);
        assert_eq!(left(cx), compact, "and back from where it is");
        crate::test_frames::draw_settled(cx);
        assert_eq!(left(cx), wide);

        cx.update(|_, cx| cx.set_reduce_motion(true));
        resize(cx, 899.);
        assert_eq!(left(cx), compact, "with motion reduced it is there at once");
    }

    /// A tablet turning wide while it shows a settings page moves a row's
    /// field from under its label to beside it: under the page's new parent
    /// the field is drawn where the compact row had it and glides there,
    /// narrowing from the row's width to its own; turning back it returns
    /// the same way, and with motion reduced it is there at once.
    #[gpui::test]
    fn a_settings_field_glides_beside_its_label_across_the_breakpoint(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 899.);
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        window_state.update(cx, |state, cx| state.open_settings(cx));
        draw(cx);
        let general = cx
            .debug_bounds("settings-nav-general")
            .expect("General settings row");
        cx.simulate_click(general.center(), gpui::Modifiers::default());
        crate::test_frames::draw_settled(cx);
        // Where the field is in its row, and how wide.
        let field = |cx: &mut VisualTestContext| {
            let row = cx
                .debug_bounds("settings-device-name-row")
                .expect("the device name row is drawn");
            let field = cx
                .debug_bounds("device-name-field")
                .expect("the device name field is drawn");
            (field.origin - row.origin, field.size.width, row.size.width)
        };
        let (under, under_width, row_width) = field(cx);
        assert_eq!(
            under_width,
            row_width - px(crate::material::CARD_INSET) * 2.,
            "under the label it spans the row"
        );

        resize(cx, 1024.);
        let (start, start_width, start_row) = field(cx);
        assert_eq!(
            start, under,
            "the field starts where the compact row had it"
        );
        assert!(
            start_width > px(crate::settings_rows::FIELD_WIDTH),
            "as wide as the row lets it: {start_width:?} in {start_row:?}"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let (gliding, gliding_width, _) = field(cx);
        crate::test_frames::draw_settled(cx);
        let (beside, beside_width, _) = field(cx);
        assert_eq!(beside_width, px(crate::settings_rows::FIELD_WIDTH));
        assert!(
            under.y > beside.y && under.x < beside.x,
            "{under:?} to {beside:?}"
        );
        assert!(
            gliding.y < under.y && gliding.y > beside.y && gliding.x > under.x,
            "{gliding:?} between {under:?} and {beside:?}"
        );
        assert!(
            gliding_width < under_width && gliding_width > beside_width,
            "{gliding_width:?} between {under_width:?} and {beside_width:?}"
        );

        resize(cx, 899.);
        assert_eq!(field(cx).0, beside, "and back from where it is");
        crate::test_frames::draw_settled(cx);
        assert_eq!(field(cx).0, under);

        cx.update(|_, cx| cx.set_reduce_motion(true));
        resize(cx, 1024.);
        assert_eq!(
            field(cx).0,
            beside,
            "with motion reduced it is there at once"
        );
    }

    /// Where `selector` is drawn on the frame the window crosses the
    /// breakpoint to `width`, half a glide later, and once settled.
    fn across_breakpoint<T>(
        cx: &mut VisualTestContext,
        width: f32,
        at: impl Fn(&mut VisualTestContext) -> T,
    ) -> [T; 3] {
        resize(cx, width);
        let start = at(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let gliding = at(cx);
        crate::test_frames::draw_settled(cx);
        [start, gliding, at(cx)]
    }

    /// The open palette is drawn under another parent as a tablet crosses
    /// the breakpoint: it glides from the card it hung as to the bottom
    /// sheet, growing to the window's width, and back from the sheet to the
    /// card, without coming in again on the way.
    #[gpui::test]
    fn the_open_palette_glides_between_card_and_sheet(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 1024.);
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        window_state.update(cx, |state, cx| state.open_palette(cx));
        draw(cx);
        crate::test_frames::draw_settled(cx);
        let card = |cx: &mut VisualTestContext| cx.debug_bounds("palette-card").expect("the card");
        let hanging = card(cx);
        assert!(hanging.size.width < px(899.));

        let [start, gliding, sheet] = across_breakpoint(cx, 899., card);
        assert_eq!(start, hanging, "the card starts where it hung");
        assert_eq!(sheet.size.width, px(899.), "the sheet spans the window");
        assert!(sheet.top() > hanging.top());
        assert!(
            gliding.size.width > hanging.size.width
                && gliding.size.width < sheet.size.width
                && gliding.top() > hanging.top()
                && gliding.top() < sheet.top(),
            "{gliding:?} between {hanging:?} and {sheet:?}"
        );

        let [start, gliding, end] = across_breakpoint(cx, 1024., card);
        assert_eq!(start, sheet, "and back from where it is");
        assert!(
            gliding.top() < sheet.top() && gliding.top() > hanging.top(),
            "{gliding:?} between {sheet:?} and {hanging:?}"
        );
        assert_eq!(end, hanging);
    }

    /// The pairing form is drawn under another parent as a tablet crosses
    /// the breakpoint: Connect glides from the foot of the compact page to
    /// its place beside the field, and back.
    #[gpui::test]
    fn the_pairing_form_glides_across_the_breakpoint(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_initial(cx, None);
        as_mobile(cx);
        resize(cx, 899.);
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        window_state.update(cx, |state, cx| state.go(Destination::Pair, cx));
        draw(cx);
        crate::test_frames::draw_settled(cx);
        let action = |cx: &mut VisualTestContext| {
            cx.debug_bounds("hosts-pair-submit")
                .expect("the pairing form's action")
        };
        let footed = action(cx);

        let [start, gliding, beside] = across_breakpoint(cx, 1024., action);
        assert_eq!(start.origin, footed.origin, "Connect starts at the foot");
        assert!(beside.top() < footed.top(), "{beside:?} above {footed:?}");
        assert!(
            gliding.top() < footed.top() && gliding.top() > beside.top(),
            "{gliding:?} between {footed:?} and {beside:?}"
        );

        let [start, gliding, end] = across_breakpoint(cx, 899., action);
        assert_eq!(start.origin, beside.origin, "and back from where it is");
        assert!(
            gliding.top() > beside.top() && gliding.top() < footed.top(),
            "{gliding:?} between {beside:?} and {footed:?}"
        );
        assert_eq!(end.origin, footed.origin);
    }

    /// The diff panel is drawn under another parent as a tablet crosses the
    /// breakpoint: from the compact panel page into the wide right panel,
    /// its tab strip folds in and the toolbar trades forms, both gliding, so
    /// the diff below moves to its place rather than jumping there.
    #[gpui::test]
    fn the_diff_panel_glides_its_strip_in_across_the_breakpoint(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        seed_wide_diff(&shell, &host, cx);
        as_mobile(cx);
        resize(cx, 1024.);
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
        });
        draw_until(&shell, cx, &host, "diff-body", None);
        resize(cx, 899.);
        shell.update(cx, |shell, cx| shell.go(Destination::Panel, cx));
        draw_until(&shell, cx, &host, "diff-body", None);
        crate::test_frames::draw_settled(cx);

        // How far below the panel's top the diff starts.
        let below = |cx: &mut VisualTestContext| {
            let body = cx.debug_bounds("diff-body").expect("the diff");
            let panel = cx.debug_bounds("right-panel").expect("the right panel");
            body.top() - panel.top()
        };
        let [start, gliding, end] = across_breakpoint(cx, 1024., below);
        assert!(
            cx.debug_bounds("right-panel-tabs").is_some(),
            "the wide panel draws its strip"
        );
        assert_ne!(start, end, "the strip and toolbar move the diff");
        assert!(
            (start < gliding && gliding < end) || (start > gliding && gliding > end),
            "{gliding:?} between {start:?} and {end:?}"
        );
    }

    /// The composer is drawn under another parent as a tablet crosses the
    /// breakpoint, which also takes the checkout row under its card away:
    /// the row folds out rather than going at once.
    #[gpui::test]
    fn the_checkout_row_folds_out_across_the_breakpoint(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_crowded_thread(cx);
        let session = "thread-1";
        let mut status = session_status(session, std::path::Path::new("/tmp/tcode-compact-panel"));
        status.git_branch = Some("main".into());
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: session.into(),
                    },
                    event: ServerEvent::SessionStatusReplaced(Box::new(status)),
                }))
                .unwrap(),
            )
            .unwrap();
        let store = store_of(&shell, cx);
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        as_mobile(cx);
        resize(cx, 1024.);
        draw_until(&shell, cx, &host, "composer-checkout", None);
        crate::test_frames::draw_settled(cx);

        // The checkout row's share of the composer's height: from the row's
        // top, where its fold starts, to the composer's bottom. The compact
        // settings drawer folds in under the card meanwhile, so the card's
        // own place is not the row's.
        let checkout_share = |cx: &mut VisualTestContext| {
            let composer = cx.debug_bounds("chat-composer").expect("the composer");
            cx.debug_bounds("composer-checkout")
                .map_or(px(0.), |row| composer.bottom() - row.top())
        };
        let [start, gliding, end] = across_breakpoint(cx, 899., checkout_share);
        assert!(
            start > gliding && gliding > end,
            "{gliding:?} between {start:?} and {end:?}"
        );
        assert!(cx.debug_bounds("composer-checkout").is_none());
    }

    /// Settings shares nothing with the workspace: the workspace's sidebar
    /// fades out beneath the settings rail as Settings rises, and the rail
    /// beneath the sidebar as the workspace comes back, neither drawn at full
    /// strength under the other.
    #[gpui::test]
    fn settings_and_the_workspace_fade_through_each_other(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        resize(cx, 1024.);
        crate::test_frames::draw_settled(cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        // The fills painted within the window's left column, which the
        // sidebar and the rail each fill over the canvas.
        let fills = |cx: &mut VisualTestContext| {
            cx.update(|window, _| {
                let column = px(SIDEBAR_WIDTH).scale(window.scale_factor());
                window
                    .painted_quads()
                    .iter()
                    .filter_map(|quad| {
                        let painted = quad.background.as_solid()?;
                        (painted.a > 0. && quad.bounds.right() <= column)
                            .then_some((quad.bounds, painted))
                    })
                    .collect::<Vec<_>>()
            })
        };
        let half_way = |cx: &mut VisualTestContext| {
            draw(cx);
            cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
            draw(cx);
        };
        let fades = |what: &str,
                     rest: &[(gpui::Bounds<gpui::ScaledPixels>, gpui::Hsla)],
                     cx: &mut VisualTestContext| {
            assert!(!rest.is_empty(), "{what} paints its column");
            let now = fills(cx);
            let drawn: Vec<(f32, f32)> = rest
                .iter()
                .flat_map(|(bounds, full)| {
                    now.iter()
                        .filter(move |(at, painted)| {
                            at == bounds
                                && (painted.h, painted.s, painted.l) == (full.h, full.s, full.l)
                        })
                        .map(move |(_, painted)| (painted.a, full.a))
                })
                .collect();
            assert!(!drawn.is_empty(), "{what} is still drawn, beneath");
            assert!(
                drawn.iter().all(|(alpha, full)| alpha < full),
                "{what} is drawn at full strength beneath the route rising: {drawn:?}"
            );
        };

        let sidebar = fills(cx);
        state.update(cx, |state, cx| state.open_settings(cx));
        half_way(cx);
        assert!(cx.debug_bounds("settings-route").is_some());
        fades("the workspace sidebar", &sidebar, cx);
        crate::test_frames::draw_settled(cx);

        let rail = fills(cx);
        state.update(cx, |state, cx| state.close_settings(cx));
        half_way(cx);
        assert!(cx.debug_bounds("chat-route").is_some());
        fades("the settings rail", &rail, cx);
    }

    /// The rows of Settings' Advanced subgroup span the rail as the rows above
    /// them do.
    #[gpui::test]
    fn the_settings_advanced_rows_span_the_rail(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        resize(cx, 1024.);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.open_settings(cx));
        crate::test_frames::draw_settled(cx);
        let general = cx
            .debug_bounds("settings-nav-general")
            .expect("a plain rail row");
        let advanced: Vec<_> = [
            "settings-nav-computer-use",
            "settings-nav-browser",
            "settings-nav-maintenance",
        ]
        .into_iter()
        .filter_map(|row| cx.debug_bounds(row).map(|bounds| (row, bounds)))
        .collect();
        assert!(!advanced.is_empty(), "the Advanced subgroup is open");
        for (row, bounds) in advanced {
            assert_eq!(
                (bounds.left(), bounds.size.width),
                (general.left(), general.size.width),
                "{row}"
            );
        }
    }

    /// Settings replaces the whole window, unlike the Hosts route: the left
    /// column is the settings rail, so the workspace sidebar — and the feature
    /// area in it — is gone while the route is showing.
    #[gpui::test]
    fn the_wide_settings_route_replaces_the_sidebar_with_its_own_rail(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        resize(cx, 1024.);
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        window_state.update(cx, |state, cx| state.open_settings(cx));
        crate::test_frames::draw_settled(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.window_state.read(cx).route(), Route::Settings);
        });
        assert!(
            cx.debug_bounds("settings-route").is_some(),
            "settings fills the window"
        );
        assert!(
            cx.debug_bounds("sidebar-feature-hosts").is_none(),
            "and the workspace sidebar is not beside it"
        );
        assert!(
            cx.debug_bounds("settings-device-caption").is_some(),
            "the rail carries the settings group captions instead"
        );
        assert!(
            cx.debug_bounds("settings-nav-threads").is_some(),
            "thread settings are a rail page of their own, as on a phone"
        );
        let general = cx
            .debug_bounds("settings-nav-general")
            .expect("settings navigation");
        cx.simulate_click(general.center(), gpui::Modifiers::default());
        draw(cx);
        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);
        assert_eq!(
            window_state.read_with(cx, |state, _| state.route()),
            Route::Chat
        );
        assert!(cx.debug_bounds("sidebar-feature-hosts").is_some());
        assert!(!cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
    }

    /// Visiting Hosts from an open thread is navigation, not a reconnection:
    /// the link, the selected thread and the views over it all stay put, and
    /// Back returns to exactly where the visit started.
    #[gpui::test]
    fn a_hosts_visit_keeps_the_link_and_returns_to_the_thread(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
        as_mobile(cx);
        resize(cx, 393.);
        let chat = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .expect("attached")
                .chat(cx)
                .entity_id()
        });
        let _ = sent(&host);

        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        window_state.update(cx, |state, cx| state.go(Destination::Hosts, cx));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Hosts);
            assert!(shell.attachment.is_some(), "the visit keeps the attachment");
        });
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
                "visiting hosts must not detach or reconnect: {payload:?}"
            );
        }

        assert!(cx.update(|window, cx| shell.update(cx, |shell, cx| shell.back(window, cx))));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::Thread);
            assert_eq!(
                shell
                    .attachment
                    .as_ref()
                    .expect("attached")
                    .chat(cx)
                    .entity_id(),
                chat,
                "the views over the workspace were never rebuilt"
            );
        });
        assert_eq!(
            shell.read_with(cx, |shell, cx| shell.page_thread(cx)),
            Some("thread-1".into())
        );
    }

    /// A session the diff panel can address: the host replaces this value
    /// wholesale, so the fields the panel does not read stay at their zero.
    fn session_status(session: &str, cwd: &std::path::Path) -> tcode_protocol::SessionStatus {
        tcode_protocol::SessionStatus {
            session_id: session.into(),
            title: "Compact panel".into(),
            cwd: cwd.to_path_buf(),
            attachments_dir: cwd.to_path_buf(),
            provider: agent::ProviderKind::ClaudeCode,
            requested_model: None,
            requested_profile_id: None,
            acp_agent_id: None,
            project_id: None,
            queued_messages: Vec::new(),
            review_comment_drafts: Vec::new(),
            terminals: Vec::new(),
            active_terminal_id: None,
            terminal_splits: Vec::new(),
            terminal_contexts: Vec::new(),
            terminal_open: false,
            terminal_height: 240.,
            delivery_in_flight: None,
            activity: tcode_protocol::SessionActivity {
                working: false,
                turn_running: false,
                waiting: false,
                waiting_for_approval: false,
                waiting_for_input: false,
                failed: false,
                unread: false,
                fork: tcode_protocol::ForkAvailability::Available,
                agent: None,
                latest_activity: None,
            },
            stopping: false,
            turn_retry: None,
            goal: None,
            native_rewind_blocked: false,
            checkout_blocked: false,
            conversation_read_only: false,
            terminal_limit_reached: false,
            terminal_split_available: false,
            usage: None,
            context_window: None,
            running_turn: None,
            pending_approvals: Vec::new(),
            pending_user_input: None,
            steering_supported: false,
            provider_option_descriptors: Vec::new(),
            provider_option_selections: Vec::new(),
            provider_option_requested_selections: Vec::new(),
            provider_commands: Vec::new(),
            git_branch: None,
            branches: Vec::new(),
            draft: false,
            draft_workspace: Default::default(),
            worktree: None,
            preparing_worktree: false,
            relay_confirmation: None,
            native_rewind_pending: false,
            native_rewind_prefill_available: false,
            model_pending_restart: false,
            options_pending_restart: false,
            injected_tools: None,
            control: None,
        }
    }

    /// Give the shell one selected session whose only net change is a single
    /// unbreakable 400-character line, so the diff panel has real content far
    /// wider than a phone page.
    fn seed_wide_diff(shell: &Entity<AppShell>, host: &MountedShell, cx: &mut VisualTestContext) {
        seed_wide_diff_with(shell, host, cx, Vec::new());
    }

    /// [`seed_wide_diff`], with `extra` events after the turn's changes.
    fn seed_wide_diff_with(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        cx: &mut VisualTestContext,
        extra: Vec<agent::AgentEvent>,
    ) {
        let session = "thread-1";
        let cwd = std::path::Path::new("/tmp/tcode-compact-panel");
        let path = "a/deeply/nested/module/tree/with/an/absurdly/long/file_name.rs";
        let long = "x".repeat(400);
        let diff = format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n\
             @@ -1 +1 @@\n-fn old() {{}}\n+fn new_{long}() {{}}\n"
        );
        let events = [
            (
                Topic::SessionStatus {
                    session_id: session.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(session_status(session, cwd))),
            ),
            (
                Topic::SessionEvents {
                    session_id: session.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 2 + extra.len() as u64,
                    records: [
                        agent::AgentEvent::TurnStarted {
                            turn_id: "turn-1".into(),
                        },
                        agent::AgentEvent::TurnChangesUpdated {
                            turn_id: "turn-1".into(),
                            changes: agent::file_changes_from_unified_diff(&diff)
                                .expect("one file section"),
                            completeness: agent::ChangeCompleteness::Exact,
                        },
                    ]
                    .into_iter()
                    .chain(extra)
                    .map(Into::into)
                    .collect(),
                },
            ),
        ];
        for (topic, event) in events {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let store = store_of(shell, cx);
        store.update(cx, |store, cx| store.show_thread(session.into(), cx));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
    }

    /// A reader scrolled up from a running turn reads still history: the
    /// turn's clock, out of view, no longer invalidates the timeline.
    #[gpui::test]
    fn a_running_turn_out_of_view_leaves_the_timeline_still(cx: &mut TestAppContext) {
        use std::cell::Cell;
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // With motion reduced the clock is all that moves: the scrollbar's
        // fade after the scroll, timed by the wall clock, draws no frames.
        cx.update(|cx| cx.set_reduce_motion(true));
        let (shell, host, cx) = mount(cx);
        let session = "thread-1";
        let mut status = session_status(session, std::path::Path::new("/tmp/tcode-running"));
        status.activity.working = true;
        status.activity.turn_running = true;
        status.running_turn = Some(tcode_core::session::RunningTurn {
            turn: 19,
            started_at: Some(crate::time::now_millis()),
        });
        let item = |id: String, content: agent::ItemContent| {
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id,
                parent_item_id: None,
                content,
            })
        };
        let answer = "A paragraph of the answer.\n\n".repeat(12);
        let mut records = Vec::new();
        for turn in 0..20 {
            let turn_id = format!("turn-{turn}");
            records.push(agent::AgentEvent::TurnStarted {
                turn_id: turn_id.clone(),
            });
            records.push(item(
                format!("user-{turn}"),
                agent::ItemContent::UserMessage {
                    text: format!("Question {turn}"),
                    context_len: None,
                    attachments: Vec::new(),
                },
            ));
            if turn < 19 {
                records.push(item(
                    format!("assistant-{turn}"),
                    agent::ItemContent::AssistantMessage {
                        text: answer.clone(),
                        summarized: false,
                    },
                ));
                records.push(agent::AgentEvent::TurnCompleted {
                    turn_id,
                    status: agent::TurnStatus::Completed,
                    usage: None,
                });
            }
        }
        let end = records.len() as u64;
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            "/tmp/tcode-running".into(),
            None,
        );
        meta.id = session.into();
        for (topic, event) in [
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![meta],
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::SessionStatus {
                    session_id: session.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            ),
            (
                Topic::SessionEvents {
                    session_id: session.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end,
                    records: records.into_iter().map(Into::into).collect(),
                },
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| store.show_thread(session.into(), cx));
        await_restore_update(&shell, cx, |store, cx| {
            store.composer_state_for(Some(session), cx).turn_running
                && !store.chat_loading_for(Some(session), cx)
        });
        resize(cx, 1024.);
        assert!(cx.debug_bounds("working-status-19").is_some());

        let timeline = cx.debug_bounds("timeline-row-0").map_or_else(
            || cx.debug_bounds("chat-header-scroll").unwrap().bottom() + px(200.),
            |row| row.center().y,
        );
        cx.simulate_event(gpui::ScrollEvent {
            position: gpui::point(px(600.), timeline),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(20_000.))),
            ..Default::default()
        });
        // Frames as the platform delivers them, each after a clock tick.
        let frame = |cx: &mut VisualTestContext| {
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
                let _ = window.draw(cx);
            });
        };
        // The scroll's frame, and the one after it settling what it laid out.
        frame(cx);
        frame(cx);
        draw(cx);
        assert!(cx.debug_bounds("working-status-19").is_none());

        let chat = shell.read_with(cx, |shell, cx| {
            shell.attachment.as_ref().expect("attached").chat(cx)
        });
        let invalidations = Rc::new(Cell::new(0));
        let _observer = cx.update(|_, cx| {
            let invalidations = invalidations.clone();
            cx.observe(&chat, move |_, _| {
                invalidations.set(invalidations.get() + 1)
            })
        });
        for _ in 0..5 {
            frame(cx);
        }
        assert_eq!(invalidations.get(), 0, "the clock invalidated the timeline");
    }

    #[gpui::test]
    fn switching_language_retitles_the_window(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        let (shell, host, cx) = mount(cx);
        let session = "draft-1";
        let mut status = session_status(session, std::path::Path::new("/tmp/tcode-title"));
        status.draft = true;
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionStatus {
                        session_id: session.into(),
                    },
                    event: ServerEvent::SessionStatusReplaced(Box::new(status)),
                }))
                .unwrap(),
            )
            .unwrap();
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| store.show_thread(session.into(), cx));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        crate::settings::assert_follows_language_switch(cx, "chat.new_thread", |cx| {
            cx.window_title().expect("the shell titles its window")
        });
    }

    /// The panel first asks the host for each changed file's current text, then
    /// builds its rendered files off the render thread. This workspace has no
    /// checkout, so refuse those reads — the panel then renders from the stored
    /// patch alone — and draw until `selector` is actually laid out. A
    /// working-tree read is answered with `git_diff` when given; returns how
    /// many the client made.
    fn draw_until(
        shell: &Entity<AppShell>,
        cx: &mut VisualTestContext,
        host: &MountedShell,
        selector: &'static str,
        git_diff: Option<&tcode_protocol::GitDiffResult>,
    ) -> usize {
        let mut git_diff_reads = 0;
        for _ in 0..12 {
            let store = store_of(shell, cx);
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            while let Ok(line) = host.outgoing.try_recv() {
                let line = decode_client_line(&line).expect("client line");
                let ClientPayload::Query(query) = line.payload else {
                    continue;
                };
                let result = match (&query, git_diff) {
                    (tcode_protocol::Query::LoadGitDiff { .. }, Some(diff)) => {
                        git_diff_reads += 1;
                        Ok(tcode_protocol::QueryResponse::GitDiff(diff.clone()))
                    }
                    _ => Err(tcode_protocol::ProtocolError {
                        code: "not_found".into(),
                        message: "no checkout in this test".into(),
                    }),
                };
                host.incoming
                    .try_send(
                        encode_line(&HostMessage::QueryResult {
                            id: line.id,
                            result,
                        })
                        .unwrap(),
                    )
                    .unwrap();
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(100));
            draw(cx);
            if cx.debug_bounds(selector).is_some() {
                return git_diff_reads;
            }
        }
        panic!("{selector} never laid out");
    }

    /// The compact Panel page owns the chrome: its segmented control is the only
    /// selector, so the diff panel's own tab row and its right-panel window
    /// controls are not built at all — and the page holds its 16pt inset even
    /// when the diff line is far wider than the window.
    #[gpui::test]
    fn diff_panel_controls_follow_layout_and_long_lines_stay_inside_the_compact_page(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        seed_wide_diff(&shell, &host, cx);
        resize(cx, 1024.);
        store_of(&shell, cx).update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
        });
        draw_until(&shell, cx, &host, "diff-body", None);
        assert!(cx.debug_bounds("right-panel-tabs").is_some());
        assert!(cx.debug_bounds("diff-close").is_some());
        as_mobile(cx);
        resize(cx, 393.);
        shell.update(cx, |shell, cx| shell.open_panels(cx));
        cx.executor().advance_clock(Duration::from_millis(250));
        draw_until(&shell, cx, &host, "diff-body", None);

        assert!(
            cx.debug_bounds("right-panel-tabs").is_none(),
            "the Diff | Tasks tab row is the wide layout's selector"
        );
        assert!(
            cx.debug_bounds("diff-close").is_none(),
            "close is a right-panel affordance; Back leaves the compact page"
        );

        let body = cx
            .debug_bounds("diff-body")
            .expect("the diff body laid out");
        assert_eq!(
            body.left(),
            px(crate::material::COMPACT_PAGE_INSET),
            "the body starts at the page inset"
        );
        assert_eq!(
            body.right(),
            px(393. - crate::material::COMPACT_PAGE_INSET),
            "and ends at it: a 400-character line scrolls inside the body \
             instead of running off the page"
        );
    }

    /// A pull request linked to the thread on screen widens the header's
    /// badge as its number widens in: one easing, frame by frame, never back
    /// and never in one step, and unlinking it narrows the badge the same way.
    #[gpui::test]
    fn a_linked_pull_request_widens_the_header_badge_once(cx: &mut TestAppContext) {
        let (shell, host, cx) = mount(cx);
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        let index = |numbers: &[u64]| {
            let mut meta = tcode_core::project::SessionMeta::new(
                agent::ProviderKind::ClaudeCode,
                "/tmp/tcode-badge".into(),
                None,
            );
            meta.id = "thread-1".into();
            meta.title = "Retry policy".into();
            meta.pull_requests = numbers
                .iter()
                .map(|&number| tcode_core::pull_request::ThreadPullRequestLink {
                    key: tcode_core::pull_request::PullRequestKey {
                        host: "github.com".into(),
                        repository: "acme/api".into(),
                        number,
                    },
                    source: tcode_core::pull_request::PullRequestSource::Manual,
                    url: String::new(),
                    linked_at: None,
                    snapshot: None,
                    stack: Default::default(),
                    sync_error: None,
                    watch: None,
                })
                .collect();
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![meta],
                    projects: Vec::new(),
                }),
            )
        };
        send_events(
            &host,
            vec![
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(Default::default()),
                ),
                index(&[7]),
            ],
        );
        serve_thread(&host, "thread-1");
        let store = store_of(&shell, cx);
        store.update(cx, |store, cx| store.show_thread("thread-1".into(), cx));
        await_restore_update(&shell, cx, |store, cx| {
            !store.chat_loading_for(Some("thread-1"), cx)
        });
        resize(cx, 1200.);
        crate::test_frames::draw_settled(cx);
        let width = |cx: &mut VisualTestContext| {
            cx.debug_bounds("header-pr-badge")
                .expect("the header's badge")
                .size
                .width
        };
        for numbers in [&[7, 42][..], &[7]] {
            let before = width(cx);
            send_events(&host, vec![index(numbers)]);
            await_restore_update(&shell, cx, |store, _| {
                store.pull_requests("thread-1").len() == numbers.len()
            });
            draw(cx);
            let mut widths = vec![before, width(cx)];
            for _ in 0..20 {
                cx.executor().advance_clock(Duration::from_millis(16));
                cx.update(|window, cx| window.simulate_next_frame(cx));
                draw(cx);
                widths.push(width(cx));
            }
            let after = *widths.last().unwrap();
            let widens = numbers.len() == 2;
            assert!(
                if widens {
                    after > before + px(20.)
                } else {
                    after < before - px(20.)
                },
                "{numbers:?}: {widths:?}"
            );
            let total = (after - before).abs();
            assert!(
                widths.windows(2).all(|pair| if widens {
                    pair[1] >= pair[0] - px(0.5)
                } else {
                    pair[1] <= pair[0] + px(0.5)
                }),
                "{numbers:?}: the badge went back: {widths:?}"
            );
            assert!(
                widths
                    .windows(2)
                    .all(|pair| (pair[1] - pair[0]).abs() < total / 2.),
                "{numbers:?}: the badge jumped: {widths:?}"
            );
            assert!(
                widths
                    .windows(2)
                    .filter(|pair| (pair[1] - pair[0]).abs() > px(0.5))
                    .count()
                    >= 6,
                "{numbers:?}: the badge changed in too few frames: {widths:?}"
            );
        }
    }

    /// An agent's thread with every right-panel tab, ten running agents of its
    /// own, a two-digit count on Pull requests, the longest git label, all five
    /// header toggles and a link back to a parent whose title is longer than
    /// the link shows.
    fn mount_crowded_thread(
        cx: &mut TestAppContext,
    ) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        mount_thread_with(cx, ten_agents(agent::ItemStatus::InProgress))
    }

    /// Ten agents of the thread's own, each at `status`.
    fn ten_agents(status: agent::ItemStatus) -> Vec<agent::AgentEvent> {
        (0..10)
            .map(|index| {
                agent::AgentEvent::ItemStarted(agent::ThreadItem {
                    id: format!("agent-{index}"),
                    parent_item_id: None,
                    content: agent::ItemContent::Subagent {
                        agent_type: "explore".into(),
                        description: format!("Survey module {index}"),
                        status,
                        summary: None,
                        model: None,
                        effort: None,
                    },
                })
            })
            .collect()
    }

    /// [`mount_crowded_thread`]'s thread with `events` in its log in place of
    /// its running agents, which show they run for as long as they do.
    fn mount_thread_with(
        cx: &mut TestAppContext,
        events: Vec<agent::AgentEvent>,
    ) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
        let (shell, host, cx) = mount(cx);
        cx.update(|_, cx| crate::window_seam::override_mobile_for_test(cx, false));
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            "/tmp/tcode-compact-panel".into(),
            None,
        );
        let mut parent = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::ClaudeCode,
            "/tmp/tcode-compact-panel".into(),
            None,
        );
        parent.id = "thread-0".into();
        parent.title = "Plan the checkout service retry and timeout policy".into();
        meta.id = "thread-1".into();
        meta.parent_session_id = Some(parent.id.clone());
        meta.pull_requests = (1..=10)
            .map(|number| tcode_core::pull_request::ThreadPullRequestLink {
                key: tcode_core::pull_request::PullRequestKey {
                    host: "github.com".into(),
                    repository: "acme/api".into(),
                    number,
                },
                source: tcode_core::pull_request::PullRequestSource::Manual,
                url: String::new(),
                linked_at: None,
                snapshot: None,
                stack: Default::default(),
                sync_error: None,
                watch: None,
            })
            .collect();
        let git = tcode_core::git::GitStatus {
            is_repo: true,
            has_commits: true,
            has_upstream: true,
            has_working_tree_changes: true,
            ..Default::default()
        };
        for (topic, event) in [
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![parent, meta],
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::GitStatus {
                    session_id: "thread-1".into(),
                },
                ServerEvent::GitStatusReplaced(tcode_protocol::GitStatusStatus {
                    status: Some(git),
                    busy: false,
                }),
            ),
        ] {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        seed_wide_diff_with(&shell, &host, cx, events);
        (shell, host, cx)
    }

    /// The right panel at the widths a desktop window passes through, on a thread whose header and tab strip are as full as they
    /// get, with the sidebar open and collapsed: it rests against the
    /// window's right edge at the width the user chose, as far as that leaves
    /// the conversation a pane's narrowest beside it, or everything beside
    /// the sidebar where the two do not fit, and comes back to that width
    /// after the
    /// window narrows and widens again, after a drag of its edge chose
    /// another, after a sidebar drag, and after it was expanded to everything
    /// beside the sidebar and dragged there. At every width no tab label is
    /// cut off and the close button is on screen and closes the panel.
    #[gpui::test]
    fn the_right_panel_keeps_the_width_the_user_chose(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_crowded_thread(cx);
        // Its agents run, and what shows them running rests only with motion
        // reduced.
        cx.update(|_, cx| cx.set_reduce_motion(true));
        let store = store_of(&shell, cx);
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        resize(cx, 1600.);
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
        });
        draw_until(&shell, cx, &host, "diff-body", None);
        let chosen = Rc::new(Cell::new(px(560.)));

        let walk = |cx: &mut VisualTestContext, widths: &[f32]| {
            for &width in widths {
                resize(cx, width);
                draw_until(&shell, cx, &host, "diff-body", None);
                crate::test_frames::draw_settled(cx);
                let sidebar = shell.read_with(cx, |shell, cx| {
                    let attachment = shell.attachment.as_ref().expect("attached");
                    if shell.window_state.read(cx).sidebar_collapsed {
                        px(0.)
                    } else {
                        attachment.sidebar_width.get()
                    }
                });
                let beside = px(width) - sidebar;
                let panel_min = px(crate::material::PANEL_CONTENT_MIN_WIDTH);
                let expected = if beside < panel_min + px(PANE_MIN_WIDTH) {
                    beside
                } else {
                    chosen.get().min(beside - px(PANE_MIN_WIDTH)).max(panel_min)
                };
                let panel = cx.debug_bounds("right-panel").expect("the right panel");
                assert_eq!(
                    (panel.size.width, panel.right()),
                    (expected, px(width)),
                    "{width}pt, sidebar {sidebar:?}"
                );
                let tabs = cx.debug_bounds("right-panel-tabs").expect("the tab list");
                for tab in ["diff-tab", "plan-tab", "agents-tab", "pull-requests-tab"] {
                    let bounds = cx.debug_bounds(tab).expect(tab);
                    assert!(
                        bounds.right() <= tabs.right(),
                        "{width}pt: {tab} ends at {:?}, past the tab list's {:?}",
                        bounds.right(),
                        tabs.right()
                    );
                }
                let close = cx.debug_bounds("diff-close").expect("the close button");
                assert!(close.right() <= px(width), "{width}pt: close at {close:?}");
            }
        };
        let drag = |cx: &mut VisualTestContext, from: gpui::Point<Pixels>, by: Pixels| {
            let to = from + gpui::point(by, px(0.));
            // As a pointer does: the drag starts on the first move and moves
            // the handle on the next, and the release listener exists only
            // while a resize is under way, so each step gets its frame.
            cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::default());
            for step in [from + gpui::point(px(5.), px(0.)), to] {
                cx.simulate_mouse_move(step, Some(MouseButton::Left), gpui::Modifiers::default());
                draw(cx);
            }
            cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::default());
            draw(cx);
        };

        walk(cx, &[1600., 1200., 1600., 1140., 720., 1600., 1200.]);

        // A drag of the panel's own edge chooses its width, which a narrower
        // and a wider window keep.
        let panel = cx.debug_bounds("right-panel").unwrap();
        drag(cx, gpui::point(panel.left(), px(400.)), px(60.));
        let resized = cx.debug_bounds("right-panel").unwrap().size.width;
        assert!(resized < panel.size.width, "the edge shrinks the panel");
        chosen.set(resized);
        walk(cx, &[900., 1600.]);

        // A sidebar drag moves the sidebar, never the panel.
        resize(cx, 1200.);
        draw(cx);
        draw(cx);
        drag(cx, gpui::point(px(SIDEBAR_WIDTH), px(400.)), px(40.));
        assert!(
            shell.read_with(cx, |shell, _| {
                shell
                    .attachment
                    .as_ref()
                    .expect("attached")
                    .sidebar_width
                    .get()
            }) > px(SIDEBAR_WIDTH),
            "the drag moved the sidebar"
        );
        walk(cx, &[1600.]);

        // Expanded, the panel takes everything beside the sidebar, however
        // wide the window; restored, it is back at the width the user chose.
        store.update(cx, |store, cx| {
            store.toggle_diff_expanded_for(Some("thread-1"), cx)
        });
        for width in [1600., 2000.] {
            resize(cx, width);
            draw_until(&shell, cx, &host, "diff-body", None);
            crate::test_frames::draw_settled(cx);
            let sidebar = shell.read_with(cx, |shell, _| {
                shell
                    .attachment
                    .as_ref()
                    .expect("attached")
                    .sidebar_width
                    .get()
            });
            let panel = cx.debug_bounds("right-panel").expect("the expanded panel");
            assert_eq!(
                (panel.left(), panel.right()),
                (sidebar, px(width)),
                "{width}pt: the expanded panel spans the window beside the sidebar"
            );
        }
        // Drags on the expanded panel's left edge do not choose a width.
        for _ in 0..2 {
            let edge = cx.debug_bounds("right-panel").expect("the expanded panel");
            drag(cx, gpui::point(edge.left(), px(400.)), px(12.));
        }
        store.update(cx, |store, cx| {
            store.toggle_diff_expanded_for(Some("thread-1"), cx)
        });
        walk(cx, &[1600.]);

        window_state.update(cx, |state, cx| state.toggle_sidebar_collapsed(&store, cx));
        walk(cx, &[1600., 1250., 1100., 1000., 900., 860., 720., 1600.]);

        resize(cx, 720.);
        draw_until(&shell, cx, &host, "diff-body", None);
        let close = cx.debug_bounds("diff-close").expect("the close button");
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        draw(cx);
        assert!(
            !store.read_with(cx, |store, _| store
                .panel_state_for(Some("thread-1"))
                .right_panel_open),
            "nothing covers the close button"
        );
    }

    /// A docked right panel leaves the keyboard where it was as it opens. A
    /// press in the panel takes it there, a press on the chat beside it takes
    /// it back with the panel left open, and closing the panel from inside
    /// gives it back to what had it.
    #[gpui::test]
    fn the_right_panel_gives_the_keyboard_back(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        resize(cx, 1600.);
        let input = shell.read_with(cx, |shell, cx| {
            shell
                .attachment
                .as_ref()
                .unwrap()
                .chat(cx)
                .read(cx)
                .composer()
                .read(cx)
                .input_focus_handle(cx)
        });
        let open = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| input.focus(window, cx));
            store.update(cx, |store, cx| {
                store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
            });
            crate::test_frames::draw_settled(cx);
            cx.update(|window, _| assert!(input.is_focused(window), "the keyboard stays"));
        };
        let press_panel = |cx: &mut VisualTestContext| {
            let panel = cx.debug_bounds("right-panel").unwrap();
            cx.simulate_click(panel.center(), gpui::Modifiers::default());
            draw(cx);
            cx.update(|window, _| assert!(!input.is_focused(window), "the press takes it"));
        };

        open(cx);
        press_panel(cx);
        let panel = cx.debug_bounds("right-panel").unwrap();
        let composer = cx.debug_bounds("chat-composer").unwrap();
        assert!(
            composer.right() <= panel.left(),
            "{composer:?} beside {panel:?}"
        );
        cx.simulate_click(
            gpui::point(composer.left() + px(40.), composer.top() + px(24.)),
            gpui::Modifiers::default(),
        );
        draw(cx);
        cx.update(|window, _| assert!(input.is_focused(window), "the press reaches the chat"));
        assert!(store.read_with(cx, |store, _| {
            store.panel_state_for(Some("thread-1")).right_panel_open
        }));
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
        });
        crate::test_frames::draw_settled(cx);

        open(cx);
        press_panel(cx);
        let close = cx.debug_bounds("diff-close").unwrap();
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        draw(cx);
        cx.update(|window, _| assert!(input.is_focused(window), "closing returns to the composer"));
    }

    #[gpui::test]
    fn narrow_chat_header_scrolls_to_its_panel_controls(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_crowded_thread(cx);
        let store = store_of(&shell, cx);
        // Narrower than the header at its narrowest.
        resize(cx, SIDEBAR_WIDTH + crate::chat::HEADER_OVERFLOW_MIN_WIDTH);
        // The header glides into its overflow tier before a reader can
        // reach it.
        crate::test_frames::draw_settled(cx);
        let header = cx.debug_bounds("chat-header-scroll").unwrap();
        cx.simulate_event(gpui::ScrollEvent {
            position: header.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(-1000.), px(0.))),
            ..Default::default()
        });
        draw(cx);
        let toggle = cx.debug_bounds("header-overflow").unwrap();
        assert!(
            toggle.left() >= header.left() && toggle.right() <= header.right(),
            "the last panel control must scroll into view"
        );
        cx.simulate_click(toggle.center(), gpui::Modifiers::default());
        draw(cx);
        draw(cx);
        // Terminal, tasks, then the diff.
        let diff = cx.debug_bounds("menu-item-2").expect("the diff's row");
        cx.simulate_click(diff.center(), gpui::Modifiers::default());
        draw_until(&shell, cx, &host, "diff-body", None);
        assert!(store.read_with(cx, |store, _| {
            store.panel_state_for(Some("thread-1")).right_panel_open
        }));
    }

    /// In a pane too narrow for both, the right panel slides in from the
    /// window's edge over the chat as it opens and, on Escape, back out,
    /// never moving the chat beneath; the reader opening it again on its way
    /// out turns it back from where it is, its content still drawn.
    #[gpui::test]
    fn the_right_panel_slides_in_over_the_chat_and_escape_slides_it_out(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_thread_with(cx, Vec::new());
        // As the app binds its keys before it opens a window.
        cx.update(|_, cx| crate::shortcut::init(cx));
        let store = store_of(&shell, cx);
        resize(cx, 800.);
        draw_until(&shell, cx, &host, "chat-composer", None);
        crate::test_frames::draw_settled(cx);
        let chat = cx.debug_bounds("chat-composer").expect("the composer");
        let left = |cx: &mut VisualTestContext| {
            assert_eq!(
                cx.debug_bounds("chat-composer"),
                Some(chat),
                "the chat stays where it is"
            );
            cx.debug_bounds("right-panel").map(|panel| panel.left())
        };
        let half_way = |cx: &mut VisualTestContext| {
            cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
            draw(cx);
        };
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
        });
        draw(cx);
        half_way(cx);
        let opening = left(cx).expect("the panel slides in");
        draw_until(&shell, cx, &host, "diff-body", None);
        crate::test_frames::draw_settled(cx);
        let open = left(cx).expect("the open panel");
        assert!(
            opening > open && opening < px(800.),
            "{opening:?}, at rest {open:?}"
        );

        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(!store.read_with(cx, |store, _| {
            store.panel_state_for(Some("thread-1")).right_panel_open
        }));
        half_way(cx);
        let closing = left(cx).expect("the panel slides out");
        assert!(closing > open && closing < px(800.), "{closing:?}");
        assert!(cx.debug_bounds("diff-body").is_some(), "drawn as it goes");

        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
        });
        draw(cx);
        let turning = left(cx).expect("the panel turns back");
        assert!(
            turning > open && turning <= closing,
            "{turning:?} from {closing:?} toward {open:?}"
        );
        assert!(cx.debug_bounds("diff-body").is_some(), "its content kept");
        crate::test_frames::draw_settled(cx);
        assert_eq!(left(cx), Some(open));

        cx.simulate_keystrokes("escape");
        crate::test_frames::draw_settled(cx);
        assert!(left(cx).is_none());
    }

    /// The right panel sliding shut on a page lets the page go once it is
    /// out of sight, keeping its address for the panel to load again. While
    /// the thread's turn runs, its agent may be driving the page through the
    /// preview tools with the panel shut, so the page stays until the turn
    /// ends.
    #[cfg(preview_backend)]
    #[gpui::test]
    fn the_right_panel_lets_its_page_go_once_shut_and_the_turn_is_done(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_thread_with(cx, Vec::new());
        let store = store_of(&shell, cx);
        resize(cx, 1600.);
        let preview = shell.read_with(cx, |shell, _| {
            shell.attachment.as_ref().unwrap().preview.clone()
        });
        let lifecycle = preview.read_with(cx, |preview, _| preview.lifecycle());
        let key = store.read_with(cx, |store, cx| {
            store.preview_key_for("thread-1", cx).unwrap()
        });
        let page = |cx: &mut VisualTestContext| {
            lifecycle.read_with(cx, |lifecycle, _| lifecycle.has_view(&key))
        };
        let working = |working: bool| {
            send_events(
                &host,
                vec![(
                    Topic::Index,
                    ServerEvent::IndexSummaryReplaced(tcode_protocol::IndexSummary {
                        activity: [(
                            "thread-1".to_owned(),
                            tcode_protocol::SessionActivity {
                                working,
                                turn_running: working,
                                waiting: false,
                                waiting_for_approval: false,
                                waiting_for_input: false,
                                failed: false,
                                unread: false,
                                fork: tcode_protocol::ForkAvailability::Available,
                                agent: None,
                                latest_activity: None,
                            },
                        )]
                        .into(),
                        ..Default::default()
                    }),
                )],
            );
        };
        let arrive = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
        };
        let open = |cx: &mut VisualTestContext| {
            store.update(cx, |store, cx| {
                store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Preview, cx);
                store.set_preview_url(&key, "https://example.com/".into(), cx);
            });
            lifecycle.update(cx, |lifecycle, _| lifecycle.seed_stub(&key));
            crate::test_frames::draw_settled(cx);
        };

        open(cx);
        store.update(cx, |store, cx| {
            store.close_right_panel_for(Some("thread-1"), cx)
        });
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        assert!(page(cx), "the page stays while the panel slides out");
        crate::test_frames::draw_settled(cx);
        assert!(!page(cx), "the shut panel lets its page go");
        assert_eq!(
            store
                .read_with(cx, |store, _| store.preview_url(&key))
                .as_deref(),
            Some("https://example.com/"),
            "the address stays"
        );

        // A running turn shows it runs, which rests only with motion reduced.
        cx.update(|_, cx| cx.set_reduce_motion(true));
        working(true);
        arrive(cx);
        assert!(store.read_with(cx, |store, _| store.turn_running_for("thread-1")));
        open(cx);
        store.update(cx, |store, cx| {
            store.close_right_panel_for(Some("thread-1"), cx)
        });
        crate::test_frames::draw_settled(cx);
        assert!(page(cx), "the running turn's agent may drive the page");
        working(false);
        arrive(cx);
        assert!(!page(cx), "the turn done, the page goes");
    }

    /// The sidebar glides out as it collapses and back in, the chat taking
    /// what it leaves on every frame: partway through, its content is partway
    /// between the widths it leaves and lands on, and no frame jumps to either.
    #[gpui::test]
    fn the_sidebar_glides_as_it_collapses_and_expands(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_thread_with(cx, Vec::new());
        let store = store_of(&shell, cx);
        let window_state = shell.read_with(cx, |shell, _| shell.window_state());
        resize(cx, 1600.);
        let width = |cx: &mut VisualTestContext| {
            cx.debug_bounds("chat-composer")
                .expect("the composer")
                .size
                .width
        };
        let at_rest = |cx: &mut VisualTestContext| {
            draw_until(&shell, cx, &host, "chat-composer", None);
            crate::test_frames::draw_settled(cx);
            width(cx)
        };
        // A display's frames, past the glide's end.
        let frames = |cx: &mut VisualTestContext| {
            draw(cx);
            (0..20)
                .map(|_| {
                    cx.executor().advance_clock(Duration::from_millis(16));
                    draw(cx);
                    width(cx)
                })
                .collect::<Vec<_>>()
        };
        let follows = |what: &str, from: Pixels, frames: &[Pixels], to: Pixels| {
            let travel = (to - from).abs();
            assert!(travel > px(100.), "{what}: {from:?} to {to:?}");
            assert!(
                frames
                    .iter()
                    .any(|width| (*width - from).abs() > px(1.) && (*width - to).abs() > px(1.)),
                "{what}: never partway from {from:?} to {to:?}: {frames:?}"
            );
            let mut last = from;
            for width in frames.iter().copied().chain([to]) {
                assert!(
                    (width - last).abs() <= travel / 2.,
                    "{what}: jumps from {last:?} to {width:?} on its way from {from:?} to {to:?}: {frames:?}"
                );
                last = width;
            }
        };

        let before = at_rest(cx);
        window_state.update(cx, |state, cx| state.toggle_sidebar_collapsed(&store, cx));
        let collapsing = frames(cx);
        let collapsed = at_rest(cx);
        follows("the sidebar collapsing", before, &collapsing, collapsed);
        window_state.update(cx, |state, cx| state.toggle_sidebar_collapsed(&store, cx));
        let expanding = frames(cx);
        assert_eq!(at_rest(cx), before);
        follows("the sidebar expanding", collapsed, &expanding, before);
    }

    /// With the sidebar collapsed, the window's left edge slides it in over
    /// the chat, and leaving it slides it back out.
    #[gpui::test]
    fn the_collapsed_sidebar_slides_in_from_the_edge_and_back(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_thread_with(cx, Vec::new());
        let store = store_of(&shell, cx);
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        resize(cx, 1600.);
        draw_until(&shell, cx, &host, "chat-composer", None);
        state.update(cx, |state, cx| state.toggle_sidebar_collapsed(&store, cx));
        crate::test_frames::draw_settled(cx);
        let left = |cx: &mut VisualTestContext| {
            cx.debug_bounds("sidebar-hover-overlay")
                .map(|overlay| overlay.left())
        };
        let half_way = |cx: &mut VisualTestContext| {
            draw(cx);
            cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
            draw(cx);
            left(cx)
        };
        cx.simulate_mouse_move(
            gpui::point(px(1000.), px(400.)),
            None,
            gpui::Modifiers::default(),
        );
        crate::test_frames::draw_settled(cx);
        assert_eq!(left(cx), None);

        cx.simulate_mouse_move(
            gpui::point(px(4.), px(400.)),
            None,
            gpui::Modifiers::default(),
        );
        let opening = half_way(cx).expect("the sidebar slides in");
        assert!(
            opening < px(0.) && opening > -px(SIDEBAR_WIDTH),
            "{opening:?}"
        );
        crate::test_frames::draw_settled(cx);
        assert_eq!(left(cx), Some(px(0.)));

        cx.simulate_mouse_move(
            gpui::point(px(1000.), px(400.)),
            None,
            gpui::Modifiers::default(),
        );
        let closing = half_way(cx).expect("the sidebar slides out");
        assert!(
            closing < px(0.) && closing > -px(SIDEBAR_WIDTH),
            "{closing:?}"
        );
        crate::test_frames::draw_settled(cx);
        assert_eq!(left(cx), None);
    }

    /// The window growing wider leaves the terminal drawer's height as the
    /// thread holds it: the host is told a height only when it changes.
    #[gpui::test]
    fn a_wider_window_sends_the_host_no_terminal_height(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_thread_with(cx, Vec::new());
        let store = store_of(&shell, cx);
        resize(cx, 1600.);
        store.update(cx, |store, cx| {
            store.toggle_terminal_panel_for("thread-1".to_owned(), cx)
        });
        crate::test_frames::draw_settled(cx);
        let _ = sent(&host);

        resize(cx, 1800.);
        crate::test_frames::draw_settled(cx);
        let heights: Vec<_> = sent(&host)
            .into_iter()
            .filter(|payload| {
                matches!(
                    payload,
                    ClientPayload::Command(Command::SetTerminalHeight { .. })
                )
            })
            .collect();
        assert!(heights.is_empty(), "{heights:?}");
    }

    /// The terminal drawer glides up under the conversation as it opens, to
    /// the height it was given, and back down as it closes.
    #[gpui::test]
    fn the_terminal_drawer_glides_open_and_closed(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_thread_with(cx, Vec::new());
        let store = store_of(&shell, cx);
        resize(cx, 1600.);
        let composer_bottom = |cx: &mut VisualTestContext| {
            cx.debug_bounds("chat-composer")
                .expect("the composer")
                .bottom()
        };
        let partway = |cx: &mut VisualTestContext| {
            draw(cx);
            cx.executor().advance_clock(Duration::from_millis(100));
            draw(cx);
            composer_bottom(cx)
        };
        let at_rest = |cx: &mut VisualTestContext| {
            draw_until(&shell, cx, &host, "chat-composer", None);
            crate::test_frames::draw_settled(cx);
            composer_bottom(cx)
        };
        let closed = at_rest(cx);

        store.update(cx, |store, cx| {
            store.toggle_terminal_panel_for("thread-1".to_owned(), cx)
        });
        let opening = partway(cx);
        let open = at_rest(cx);
        let height = store.read_with(cx, |store, _| {
            store.panel_state_for(Some("thread-1")).terminal_height
        });
        assert_eq!(closed - open, px(height), "the drawer rests at its height");
        assert!(
            open < opening && opening < closed,
            "the drawer is partway up: {opening:?} between {open:?} and {closed:?}"
        );
        store.update(cx, |store, cx| {
            store.toggle_terminal_panel_for("thread-1".to_owned(), cx)
        });
        let closing = partway(cx);
        assert!(
            open < closing && closing < closed,
            "the drawer is partway down: {closing:?}"
        );
        assert_eq!(at_rest(cx), closed);
    }

    /// Expand and Restore lay the right panel out again without remounting it:
    /// what its elements keep by their place in the tree — the focus a press
    /// gives the Expand button, the scroll of a list — is still theirs after.
    #[gpui::test]
    fn expanding_the_right_panel_keeps_its_elements_state(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_crowded_thread(cx);
        // Its agents run, and what shows them running rests only with motion
        // reduced.
        cx.update(|_, cx| cx.set_reduce_motion(true));
        let store = store_of(&shell, cx);
        resize(cx, 1600.);
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
        });
        draw_until(&shell, cx, &host, "diff-body", None);

        let press_expand = |cx: &mut VisualTestContext| {
            let expand = cx.debug_bounds("diff-expand").expect("the expand button");
            cx.simulate_click(expand.center(), gpui::Modifiers::default());
            crate::test_frames::draw_settled(cx);
            cx.update(|window, cx| window.focused(cx))
        };
        let expanded = press_expand(cx);
        assert!(
            store.read_with(cx, |store, _| store
                .panel_state_for(Some("thread-1"))
                .right_panel_expanded),
            "the press expands the panel"
        );
        assert!(
            expanded.is_some(),
            "the expanded panel's button keeps its focus"
        );
        let restored = press_expand(cx);
        assert!(
            !store.read_with(cx, |store, _| store
                .panel_state_for(Some("thread-1"))
                .right_panel_expanded),
            "the second press restores it"
        );
        assert_eq!(
            restored, expanded,
            "the restored panel's button is the same one"
        );
    }

    /// The working tree changes outside any turn — the user edits a file while
    /// another tab is showing. Choosing the Diff tab reads the tree again, as
    /// opening the panel does, and a tree with nothing to show draws the empty
    /// state rather than an empty list.
    #[gpui::test]
    fn diff_tab_reads_the_working_tree_again_when_chosen(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        open_working_tree_panel(&shell, &host, cx);
        let store = store_of(&shell, cx);
        let clean = tcode_protocol::GitDiffResult::default();
        assert_eq!(draw_until(&shell, cx, &host, "diff-empty", Some(&clean)), 1);
        assert!(
            cx.debug_bounds("diff-body").is_none(),
            "a clean tree draws the empty state, not an empty list"
        );

        store.update(cx, |store, cx| {
            store.set_right_tab_for(Some("thread-1"), RightTab::PullRequests, cx)
        });
        draw(cx);
        let changed = tcode_protocol::GitDiffResult {
            changes: vec![agent::FileChange {
                path: "README.md".into(),
                kind: agent::FileChangeKind::Modify,
                diff: Some("@@ -1 +1,2 @@\n # acme-api\n+Checkout service.\n".into()),
            }],
            texts: vec![tcode_protocol::GitFileText {
                old: Some("# acme-api\n".into()),
                new: Some("# acme-api\nCheckout service.\n".into()),
            }],
            ..Default::default()
        };
        store.update(cx, |store, cx| {
            store.set_right_tab_for(Some("thread-1"), RightTab::Diff, cx)
        });
        assert_eq!(
            draw_until(&shell, cx, &host, "diff-body", Some(&changed)),
            1,
            "choosing the Diff tab reads the working tree again and lists the changed file"
        );
    }

    /// A changed word in the diff wears the tint of the mode the window ends
    /// on, not of the frame a theme change began on.
    #[gpui::test]
    fn a_diffs_changed_words_take_the_new_modes_tint(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        open_working_tree_panel(&shell, &host, cx);
        let changed = tcode_protocol::GitDiffResult {
            changes: vec![agent::FileChange {
                path: "README.md".into(),
                kind: agent::FileChangeKind::Modify,
                diff: Some("@@ -1 +1 @@\n-# acme-api\n+# acme-web\n".into()),
            }],
            texts: vec![tcode_protocol::GitFileText {
                old: Some("# acme-api\n".into()),
                new: Some("# acme-web\n".into()),
            }],
            ..Default::default()
        };
        draw_until(&shell, cx, &host, "diff-body", Some(&changed));
        crate::test_frames::draw_settled(cx);
        let tint = |mode, cx: &mut VisualTestContext| {
            cx.update(|_, cx| crate::theme::palette(mode, cx).success.opacity(0.30))
        };
        let painted = |tint, cx: &mut VisualTestContext| {
            cx.update(|window, _| {
                window
                    .painted_quads()
                    .iter()
                    .any(|quad| quad.background.as_solid() == Some(tint))
            })
        };
        let light = tint(crate::theme::ThemeMode::Light, cx);
        assert!(painted(light, cx), "the added word wears the light tint");

        cx.update(|window, cx| {
            crate::settings_page::apply_theme(tcode_core::settings::ThemeMode::Dark, window, cx)
        });
        crate::test_frames::draw_settled(cx);
        crate::test_frames::draw_settled(cx);
        let dark = tint(crate::theme::ThemeMode::Dark, cx);
        assert!(painted(dark, cx), "the added word wears the dark tint");
        assert!(!painted(light, cx));
    }

    /// A thread whose turn is running, with the diff panel open on its working tree.
    fn open_working_tree_panel(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        cx: &mut VisualTestContext,
    ) {
        let session = "thread-1";
        let cwd = std::path::Path::new("/tmp/tcode-working-tree");
        let events = [
            (
                Topic::SessionStatus {
                    session_id: session.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(session_status(session, cwd))),
            ),
            (
                Topic::SessionEvents {
                    session_id: session.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 1,
                    records: vec![
                        agent::AgentEvent::TurnStarted {
                            turn_id: "turn-1".into(),
                        }
                        .into(),
                    ],
                },
            ),
        ];
        for (topic, event) in events {
            host.incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let store = store_of(shell, cx);
        store.update(cx, |store, cx| store.show_thread(session.into(), cx));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        resize(cx, 1200.);
        store.update(cx, |store, cx| {
            store.toggle_tab_panel_for(session, tcode_core::ui::RightTab::Diff, cx)
        });
    }

    /// Lets the client run and draw while the host holds every working-tree read; any other
    /// query is refused, as a workspace without a checkout would. Returns the held reads in
    /// the order the client made them.
    fn held_git_diff_reads(
        shell: &Entity<AppShell>,
        host: &MountedShell,
        cx: &mut VisualTestContext,
    ) -> Vec<u64> {
        let mut reads = Vec::new();
        for _ in 0..6 {
            store_of(shell, cx).update(cx, |store, cx| store.drain_host_events_for_test(cx));
            while let Ok(line) = host.outgoing.try_recv() {
                let line = decode_client_line(&line).expect("client line");
                let ClientPayload::Query(query) = line.payload else {
                    continue;
                };
                if matches!(query, tcode_protocol::Query::LoadGitDiff { .. }) {
                    reads.push(line.id);
                    continue;
                }
                answer_query(
                    host,
                    line.id,
                    Err(tcode_protocol::ProtocolError {
                        code: "not_found".into(),
                        message: "no checkout in this test".into(),
                    }),
                );
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(100));
            draw(cx);
        }
        reads
    }

    fn answer_query(
        host: &MountedShell,
        id: u64,
        result: Result<tcode_protocol::QueryResponse, tcode_protocol::ProtocolError>,
    ) {
        host.incoming
            .try_send(encode_line(&HostMessage::QueryResult { id, result }).unwrap())
            .unwrap();
    }

    fn answer_git_diff(host: &MountedShell, id: u64, diff: &tcode_protocol::GitDiffResult) {
        answer_query(
            host,
            id,
            Ok(tcode_protocol::QueryResponse::GitDiff(diff.clone())),
        );
    }

    /// A working tree in which each of `files` was created with `lines` lines.
    fn created_files(files: &[(&str, u32)]) -> tcode_protocol::GitDiffResult {
        let (changes, texts) = files
            .iter()
            .map(|&(path, lines)| {
                let text = (1..=lines).map(|line| format!("{path} line {line}\n"));
                let patch = format!(
                    "@@ -0,0 +1,{lines} @@\n{}",
                    text.clone()
                        .map(|line| format!("+{line}"))
                        .collect::<String>()
                );
                (
                    agent::FileChange {
                        path: path.into(),
                        kind: agent::FileChangeKind::Create,
                        diff: Some(patch),
                    },
                    tcode_protocol::GitFileText {
                        old: None,
                        new: Some(text.collect()),
                    },
                )
            })
            .unzip();
        tcode_protocol::GitDiffResult {
            changes,
            texts,
            ..Default::default()
        }
    }

    /// The agent ends a turn, so the working tree is read again.
    fn complete_turn(host: &MountedShell) {
        host.incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::SessionEvents {
                        session_id: "thread-1".into(),
                    },
                    event: ServerEvent::SessionEvent(
                        agent::AgentEvent::TurnCompleted {
                            turn_id: "turn-1".into(),
                            status: agent::TurnStatus::Completed,
                            usage: None,
                        }
                        .into(),
                    ),
                }))
                .unwrap(),
            )
            .unwrap();
    }

    /// While an agent works the tree changes under the reader. Reading it again leaves the
    /// diff they were reading on screen — not a loading state — until the new one is built.
    #[gpui::test]
    fn working_tree_refresh_keeps_the_previous_diff_until_the_new_one_lands(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        open_working_tree_panel(&shell, &host, cx);
        let reads = held_git_diff_reads(&shell, &host, cx);
        assert_eq!(reads.len(), 1, "opening the panel reads the working tree");
        answer_git_diff(&host, reads[0], &created_files(&[("first.rs", 3)]));
        assert!(held_git_diff_reads(&shell, &host, cx).is_empty());
        assert!(cx.debug_bounds("diff-line-new-first.rs-1").is_some());

        complete_turn(&host);
        let reads = held_git_diff_reads(&shell, &host, cx);
        assert_eq!(reads.len(), 1, "the finished turn reads the tree again");
        assert!(
            cx.debug_bounds("diff-line-new-first.rs-1").is_some(),
            "the previous diff stays on screen while the host reads the tree"
        );

        answer_git_diff(
            &host,
            reads[0],
            &created_files(&[("first.rs", 3), ("second.rs", 2)]),
        );
        held_git_diff_reads(&shell, &host, cx);
        assert!(
            cx.debug_bounds("diff-line-new-second.rs-1").is_some(),
            "the new diff replaces it once built"
        );
    }

    /// Swapping in the new diff keeps the reader on the line they were reading, even when a
    /// file now listed above it pushes every row down.
    #[gpui::test]
    fn working_tree_refresh_keeps_the_reading_position(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        open_working_tree_panel(&shell, &host, cx);
        let reads = held_git_diff_reads(&shell, &host, cx);
        answer_git_diff(&host, reads[0], &created_files(&[("long.rs", 300)]));
        held_git_diff_reads(&shell, &host, cx);

        let body = cx
            .debug_bounds("diff-body")
            .expect("the diff body laid out");
        // A wheel turn at a time, each eased out and drawn, as a reader scrolls.
        for _ in 0..12 {
            cx.simulate_event(gpui::ScrollEvent {
                position: body.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-100.))),
                ..Default::default()
            });
            for _ in 0..5 {
                cx.executor().advance_clock(Duration::from_millis(100));
                draw(cx);
            }
        }
        // Debug selectors are looked up by `&'static str`.
        let selector =
            |line: u32| -> &'static str { format!("diff-line-new-long.rs-{line}").leak() };
        let top_line = |cx: &mut VisualTestContext| {
            (1..=300).find_map(|line| {
                cx.debug_bounds(selector(line))
                    .filter(|bounds| bounds.top() >= body.top())
                    .map(|bounds| (line, bounds.top()))
            })
        };
        let (line, top) = top_line(cx).expect("a line of long.rs is on screen");
        assert!(line > 20, "the reader scrolled into the file: {line}");

        complete_turn(&host);
        let reads = held_git_diff_reads(&shell, &host, cx);
        answer_git_diff(
            &host,
            reads[0],
            &created_files(&[("added.rs", 20), ("long.rs", 300)]),
        );
        held_git_diff_reads(&shell, &host, cx);
        assert!(
            cx.debug_bounds("diff-line-new-added.rs-1").is_none(),
            "the file listed above is out of view"
        );
        let after = cx
            .debug_bounds(selector(line))
            .expect("the line read before the refresh is still on screen");
        assert!(
            (after.top() - top).abs() < px(1.),
            "it stays where it was: {top:?} then {:?}",
            after.top()
        );
    }

    /// Reads answered out of order: an earlier read the host finishes last is superseded and
    /// never replaces the newer diff, nor what was shown while both were pending.
    #[gpui::test]
    fn working_tree_refresh_ignores_a_superseded_read(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        open_working_tree_panel(&shell, &host, cx);
        let reads = held_git_diff_reads(&shell, &host, cx);
        answer_git_diff(&host, reads[0], &created_files(&[("first.rs", 3)]));
        held_git_diff_reads(&shell, &host, cx);

        complete_turn(&host);
        let earlier = held_git_diff_reads(&shell, &host, cx);
        complete_turn(&host);
        let later = held_git_diff_reads(&shell, &host, cx);
        assert_eq!((earlier.len(), later.len()), (1, 1));

        answer_git_diff(&host, earlier[0], &created_files(&[("stale.rs", 3)]));
        held_git_diff_reads(&shell, &host, cx);
        assert!(cx.debug_bounds("diff-line-new-stale.rs-1").is_none());
        assert!(
            cx.debug_bounds("diff-line-new-first.rs-1").is_some(),
            "a superseded read leaves the shown diff in place"
        );

        answer_git_diff(&host, later[0], &created_files(&[("newest.rs", 3)]));
        held_git_diff_reads(&shell, &host, cx);
        assert!(cx.debug_bounds("diff-line-new-newest.rs-1").is_some());
        assert!(cx.debug_bounds("diff-line-new-stale.rs-1").is_none());
    }

    #[test]
    fn sidebar_overlay_follows_hover_only_on_collapsed_workspace_routes() {
        for (current, transition, popover, visible) in [
            (false, SidebarHoverTransition::Trigger(true), false, true),
            (false, SidebarHoverTransition::Trigger(false), false, false),
            (true, SidebarHoverTransition::Trigger(false), false, true),
            (false, SidebarHoverTransition::Overlay(true), false, true),
            (true, SidebarHoverTransition::Overlay(true), false, true),
            (true, SidebarHoverTransition::Overlay(false), false, false),
            (true, SidebarHoverTransition::Overlay(false), true, true),
            (false, SidebarHoverTransition::Overlay(false), true, false),
        ] {
            assert_eq!(
                next_sidebar_overlay_visibility(current, transition, true, Route::Chat, popover),
                visible
            );
            assert!(!next_sidebar_overlay_visibility(
                current,
                transition,
                false,
                Route::Chat,
                popover
            ));
            for route in [Route::Settings, Route::Hosts] {
                assert!(!next_sidebar_overlay_visibility(
                    current, transition, true, route, popover
                ));
            }
        }
    }

    /// thread-1's timeline: a running turn holding a question, an agent
    /// when `agent`, a first answer, and the command it runs now, which has
    /// printed `lines` short lines so far: none long enough to wrap at any
    /// width the tests draw, so only motion changes their heights.
    fn reparented_timeline(lines: usize, agent: bool) -> tcode_core::session::Timeline {
        use crate::chat::tests::{assistant, entry, user_item};
        let mut timeline = tcode_core::session::Timeline::default();
        timeline.turns = vec![tcode_core::session::TurnMeta {
            running: true,
            ..Default::default()
        }];
        timeline
            .entries
            .push(entry("user", user_item("Look at the build.")));
        if agent {
            timeline.entries.push(entry(
                "agent-1",
                tcode_core::session::EntryContent::Item(agent::ItemContent::Subagent {
                    agent_type: "Explore".into(),
                    description: "Survey the code".into(),
                    status: agent::ItemStatus::InProgress,
                    summary: None,
                    model: None,
                    effort: None,
                }),
            ));
        }
        timeline
            .entries
            .push(entry("reply", assistant("I will build it first.")));
        let output: String = (0..lines).map(|line| format!("line {line}\n")).collect();
        timeline.entries.push(entry(
            "cmd",
            tcode_core::session::EntryContent::Item(agent::ItemContent::CommandExecution {
                command: "cargo build".into(),
                output,
                exit_code: None,
                status: agent::ItemStatus::InProgress,
            }),
        ));
        timeline
    }

    fn show_timeline(
        store: &Entity<WorkspaceStore>,
        timeline: tcode_core::session::Timeline,
        cx: &mut VisualTestContext,
    ) {
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test("thread-1".into(), timeline, cx)
        });
    }

    /// One display frame: time passes and the window draws what it asked
    /// for. A running turn's status never rests, so the window is stepped
    /// frame by frame rather than until it is still.
    fn display_frame(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(Duration::from_millis(16));
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(cx);
    }

    fn settle_frames(cx: &mut VisualTestContext) {
        for _ in 0..30 {
            display_frame(cx);
        }
    }

    /// From the command's header to the turn's working status: the
    /// command's window, as tall as its motion draws it.
    fn command_to_status(cx: &mut VisualTestContext) -> Pixels {
        let header = cx
            .debug_bounds("activity-row-cmd")
            .expect("the command's header");
        let status = cx
            .debug_bounds("working-status-0")
            .expect("the turn's status");
        status.top() - header.top()
    }

    fn click_command(cx: &mut VisualTestContext) {
        let header = cx
            .debug_bounds("activity-row-cmd")
            .expect("the command's header");
        cx.simulate_click(header.center(), gpui::Modifiers::default());
    }

    /// A glide's start, its end at rest, and what was drawn part way when
    /// the conversation was drawn under a new parent: strictly between the
    /// two, where motion that restarted under its new parent draws its end.
    fn assert_between(what: &str, start: Pixels, end: Pixels, seen: Pixels) {
        let (low, high) = if start < end {
            (start, end)
        } else {
            (end, start)
        };
        assert!(
            low + px(0.5) < seen && seen < high - px(0.5),
            "{what} restarted under its new parent: {seen:?}, not between {start:?} and {end:?}"
        );
    }

    /// A zoom draws the pane over the column, under another parent than its
    /// slot, from the frame it starts (`panes::Zoom`). What the
    /// conversation had under way carries on from where it was: an opening
    /// command, its output growing at the followed end, and the header's
    /// Agents toggle widening in. Element-keyed clocks drew each at its end
    /// in that frame.
    #[gpui::test]
    fn chat_motion_carries_on_when_its_pane_is_zoomed(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        show_timeline(&store, reparented_timeline(2, false), cx);
        settle_frames(cx);
        let _ = sent(&host);
        // From the plan button to the diff button: the Agents toggle
        // between them widens in with the thread's first agent. Measured
        // between the two, since the split also brings the pane's close
        // button in after them.
        let plan = |cx: &mut VisualTestContext| {
            let plan = cx.debug_bounds("plan-panel").expect("the plan button");
            let diff = cx.debug_bounds("diff-panel").expect("the diff button");
            diff.left() - plan.left()
        };
        // The latest command of a running turn opens by itself; shut, under
        // two lines of output and with no agent, is where the glides start.
        let open = command_to_status(cx);
        click_command(cx);
        settle_frames(cx);
        let start = command_to_status(cx);
        let without = plan(cx);
        assert!(
            start + px(10.) < open,
            "the command shut: {open:?} -> {start:?}"
        );
        // Where they end: the command open, its output grown, an agent come.
        click_command(cx);
        show_timeline(&store, reparented_timeline(8, true), cx);
        settle_frames(cx);
        let end = command_to_status(cx);
        let with = plan(cx);
        assert!(end > open + px(40.), "the output grew: {open:?} -> {end:?}");
        assert!(
            with > without + px(10.),
            "the toggle came: {without:?} -> {with:?}"
        );
        click_command(cx);
        show_timeline(&store, reparented_timeline(2, false), cx);
        settle_frames(cx);
        assert_eq!((command_to_status(cx), plan(cx)), (start, without));

        // Beside a second pane, all three at once, a quarter of the way in
        // when the pane is zoomed: the zoom draws it over the column, under
        // another parent than its slot, laid out at the column's size, the
        // size the glides were measured at above.
        cx.simulate_keystrokes("secondary-d");
        settle_frames(cx);
        cx.simulate_keystrokes("secondary-alt-left");
        settle_frames(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        click_command(cx);
        show_timeline(&store, reparented_timeline(8, true), cx);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 4);
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(cx);
        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        assert!(
            cx.debug_bounds("zoomed-pane").is_some(),
            "drawn over the column"
        );
        display_frame(cx);
        assert_between(
            "the opening command and its growing output",
            start,
            end,
            command_to_status(cx),
        );
        assert_between("the Agents toggle", without, with, plan(cx));
        settle_frames(cx);
        assert_eq!(
            (command_to_status(cx), plan(cx)),
            (end, with),
            "the glides came to rest where they end"
        );
    }

    /// How opaque the fill of the button `selector` is painted: none when
    /// it paints no fill.
    fn button_fill(selector: &'static str, cx: &mut VisualTestContext) -> f32 {
        let button = cx.debug_bounds(selector).expect("the button");
        cx.update(|window, _| {
            let button = button.scale(window.scale_factor());
            window
                .painted_quads()
                .iter()
                .filter(|quad| {
                    quad.bounds.contains(&button.center())
                        && (quad.bounds.size.width.0 - button.size.width.0).abs() < 1.
                        && (quad.bounds.size.height.0 - button.size.height.0).abs() < 1.
                })
                .filter_map(|quad| quad.background.as_solid().map(|fill| fill.a))
                .fold(0., f32::max)
        })
    }

    /// A header toggle's fill glides in as its panel opens. A split part
    /// way draws the pane's group under another parent: the fill carries on
    /// from where it was, on the chat's clock (`Button::motion`), rather
    /// than starting over at its end.
    #[gpui::test]
    fn a_header_toggles_fill_carries_on_when_its_pane_is_split(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        settle_frames(cx);
        let _ = sent(&host);
        let toggle = |cx: &mut VisualTestContext| {
            store.update(cx, |store, cx| {
                store.toggle_tab_panel_for("thread-1", tcode_core::ui::RightTab::Diff, cx)
            });
        };
        let off = button_fill("diff-panel", cx);
        toggle(cx);
        settle_frames(cx);
        let on = button_fill("diff-panel", cx);
        assert!(off + 0.02 < on, "the toggle's fill came: {off} -> {on}");
        toggle(cx);
        settle_frames(cx);
        assert_eq!(button_fill("diff-panel", cx), off);

        toggle(cx);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 4);
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(cx);
        cx.simulate_keystrokes("secondary-shift-d");
        draw(cx);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
        display_frame(cx);
        let partway = button_fill("diff-panel", cx);
        // In thousandths, for the half-point margin the comparison allows.
        assert_between(
            "the diff toggle's fill",
            px(off * 1000.),
            px(on * 1000.),
            px(partway * 1000.),
        );
        settle_frames(cx);
        assert_eq!(button_fill("diff-panel", cx), on, "it comes to rest lit");
    }

    /// A tablet turning compact draws the conversation on the compact
    /// thread page, under another parent than the wide layout's pane: the
    /// command opening and its output growing carry on from where they were.
    #[gpui::test]
    fn chat_motion_carries_on_when_the_window_turns_compact(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        as_mobile(cx);
        resize(cx, 1024.);
        shell.read_with(cx, |shell, cx| assert!(!shell.compact(cx)));
        let store = store_of(&shell, cx);
        show_timeline(&store, reparented_timeline(2, false), cx);
        settle_frames(cx);
        let _ = sent(&host);
        let wide_open = command_to_status(cx);
        click_command(cx);
        settle_frames(cx);
        let wide_shut = command_to_status(cx);
        assert!(wide_shut + px(10.) < wide_open);

        // A quarter of the way into opening the command and growing the
        // output, the window turns compact.
        click_command(cx);
        show_timeline(&store, reparented_timeline(8, false), cx);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 4);
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(cx);
        cx.simulate_resize(size(px(899.), px(800.)));
        draw(cx);
        shell.read_with(cx, |shell, cx| assert!(shell.compact(cx)));
        assert!(cx.debug_bounds("compact-thread-page").is_some());
        display_frame(cx);
        let partway = command_to_status(cx);
        settle_frames(cx);
        // Where both glides end on the compact page, and where they start.
        let compact_end = command_to_status(cx);
        display_frame(cx);
        assert_eq!(
            command_to_status(cx),
            compact_end,
            "the glides came to rest"
        );
        click_command(cx);
        show_timeline(&store, reparented_timeline(2, false), cx);
        settle_frames(cx);
        let compact_start = command_to_status(cx);
        assert_between(
            "the opening command and its growing output",
            compact_start,
            compact_end,
            partway,
        );
    }

    /// What the host answers `id`'s subscriptions with: its status, and a
    /// log of `turns` finished turns, each a question and its answer, the
    /// first with an agent it ran when `agent`. Only the first turn's
    /// records carry their times, so only it has a time row.
    fn serve_finished_turns(host: &MountedShell, id: &str, turns: usize, agent: bool) {
        let at = 1_780_000_000_000;
        let item = |id: String, content| {
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id,
                parent_item_id: None,
                content,
            })
        };
        let records: Vec<tcode_core::session::StoredEvent> = (0..turns)
            .flat_map(|turn| {
                let ran = (agent && turn == 0).then(|| {
                    item(
                        format!("{id}-agent"),
                        agent::ItemContent::Subagent {
                            agent_type: "Explore".into(),
                            description: "Survey the code".into(),
                            status: agent::ItemStatus::Completed,
                            summary: None,
                            model: None,
                            effort: None,
                        },
                    )
                });
                [
                    Some(agent::AgentEvent::TurnStarted {
                        turn_id: format!("{id}-turn-{turn}"),
                    }),
                    Some(item(
                        format!("{id}-ask-{turn}"),
                        agent::ItemContent::UserMessage {
                            text: format!("What is in {id}?"),
                            context_len: None,
                            attachments: vec![],
                        },
                    )),
                    ran,
                    Some(item(
                        format!("{id}-answer-{turn}"),
                        agent::ItemContent::AssistantMessage {
                            text: "A few things.".into(),
                            summarized: false,
                        },
                    )),
                    Some(agent::AgentEvent::TurnCompleted {
                        turn_id: format!("{id}-turn-{turn}"),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    }),
                ]
                .into_iter()
                .flatten()
                .map(move |event| (turn, event))
            })
            .enumerate()
            .map(|(index, (turn, event))| tcode_core::session::StoredEvent {
                ts: (turn == 0).then_some(at + index as u64 * 1_000),
                ..event.into()
            })
            .collect();
        serve_status(host, id, false);
        send_events(
            host,
            vec![(
                Topic::SessionEvents {
                    session_id: id.into(),
                },
                ServerEvent::SessionSnapshot {
                    total: records.len() as u64,
                    total_turns: turns as u64,
                    truncated: false,
                    from: 0,
                    end: records.len() as u64,
                    records,
                },
            )],
        );
    }

    /// A pane switched to another thread shows it as a new scene: on the
    /// first frame that draws it, every row and the header's controls are
    /// where they rest. A thread's first turn's time row does not slide
    /// from where the thread before drew its own first turn's (down to the
    /// last turn of one, up to the first of two), nor does the header's
    /// Agents toggle fold away or widen in as the thread with an agent
    /// gives way to one without and back.
    #[gpui::test]
    fn a_thread_switched_to_moves_nothing_from_the_one_before(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        serve_finished_turns(&host, "thread-1", 1, true);
        await_restore_update(&shell, cx, |store, cx| {
            store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx)
                == Some(1)
        });
        settle_frames(cx);
        assert!(cx.debug_bounds("turn-time-row").is_some());
        // From the plan button to the diff button: the Agents toggle
        // between them is there only with an agent.
        let plan = |cx: &mut VisualTestContext| {
            let plan = cx.debug_bounds("plan-panel").expect("the plan button");
            let diff = cx.debug_bounds("diff-panel").expect("the diff button");
            diff.left() - plan.left()
        };
        let with_agent = plan(cx);

        for (key, id, turns, agent) in [
            ("secondary-2", "thread-2", 2, false),
            ("secondary-1", "thread-1", 1, true),
        ] {
            // The thread's log is there by the frame that first draws it,
            // the frame after the one that drew the thread before.
            serve_finished_turns(&host, id, turns, agent);
            cx.simulate_keystrokes(key);
            let store = store_of(&shell, cx);
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
            assert_eq!(pane_threads(&shell, cx), [Some(id.into())]);
            let shown = store.read_with(cx, |store, cx| {
                store.with_timeline_for(Some(id), |timeline| timeline.turns.len(), cx)
            });
            assert_eq!(shown, Some(turns));
            let drawn = |cx: &mut VisualTestContext| {
                [
                    "timeline-row-0",
                    "timeline-row-1",
                    "timeline-row-2",
                    "timeline-row-3",
                    "turn-time-row",
                ]
                .map(|selector| cx.debug_bounds(selector))
            };
            let first = (drawn(cx), plan(cx));
            settle_frames(cx);
            assert_eq!(
                first,
                (drawn(cx), plan(cx)),
                "{id} is at rest from its first frame"
            );
            assert_eq!(
                first.1 == with_agent,
                agent,
                "the toggle is there with an agent"
            );
        }
    }

    /// A finished turn that ends on a message draws one footer row: the
    /// message's Copy on the leading edge and the turn's time, duration and
    /// cost against the trailing edge, at a wide window and at narrow ones,
    /// where the timing wraps toward that edge but stays in the row.
    #[gpui::test]
    fn a_finished_turns_copy_and_timing_share_one_row(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        let (shell, host, cx) = mount_panes(cx);
        serve_finished_turns(&host, "thread-1", 1, false);
        await_restore_update(&shell, cx, |store, cx| {
            store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx)
                == Some(1)
        });
        for width in [2000., 900., 640.] {
            resize(cx, width);
            settle_frames(cx);
            let bounds = |selector: &'static str, cx: &mut VisualTestContext| {
                cx.debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector} at {width}px"))
            };
            let row = bounds("turn-time-row", cx);
            let copy = bounds("copy-assistant:thread-1-answer-0", cx);
            let clock = bounds("turn-time-clock", cx);
            let total = bounds("turn-time-total", cx);
            assert!(
                row.contains(&copy.center()),
                "Copy is in the footer row at {width}px: {row:?}, {copy:?}"
            );
            assert!(
                (copy.left() - row.left()).abs() <= px(1.),
                "Copy leads the row at {width}px: {row:?}, {copy:?}"
            );
            for clause in [clock, total] {
                assert!(
                    clause.left() > copy.right() && clause.right() <= row.right(),
                    "the timing is beside Copy, in the row, at {width}px: \
                     {row:?}, {copy:?}, {clause:?}"
                );
                assert!(
                    clause.top() < copy.bottom() && clause.bottom() > copy.top(),
                    "the timing is on Copy's line at {width}px: {copy:?}, {clause:?}"
                );
            }
            assert!(
                (row.right() - total.right()).abs() <= px(1.),
                "the timing ends at the row's trailing edge at {width}px: \
                 {row:?}, {total:?}"
            );
        }
    }

    /// Resting on a finished turn's duration shows its breakdown beside the
    /// duration, not centred on the row or the page.
    #[gpui::test]
    fn a_turns_duration_shows_its_breakdown_beside_it(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        let (shell, host, cx) = mount_panes(cx);
        serve_finished_turns(&host, "thread-1", 1, false);
        await_restore_update(&shell, cx, |store, cx| {
            store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx)
                == Some(1)
        });
        settle_frames(cx);
        let total = cx
            .debug_bounds("turn-time-total")
            .expect("the turn's duration");
        cx.simulate_mouse_move(total.center(), None, gpui::Modifiers::none());
        draw(cx);
        let delay = cx.update(|_, cx| gpui_base::TooltipDefaults::global(cx).show_delay());
        cx.executor().advance_clock(delay);
        settle_frames(cx);
        let tooltip = cx.debug_bounds("tcode-tooltip").expect("the breakdown");
        assert!(
            (tooltip.center().x - total.center().x).abs() <= px(1.),
            "the breakdown is centred over or under the duration: \
             duration {total:?}, tooltip {tooltip:?}"
        );
    }

    /// As the window narrows past where the header's labels fit, its split
    /// buttons and the way back to the parent drop their labels; past where
    /// its toggles fit they give way to the overflow button. Each change
    /// cross-fades while the controls' width glides, so the git button
    /// moves over rather than jumping, and lands where the new tier puts it.
    #[gpui::test]
    fn the_chat_header_glides_between_its_tiers(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // Its agents have finished: a running one's mark turns for as long
        // as it runs, so the window would never rest.
        let (_shell, _host, cx) = mount_thread_with(cx, ten_agents(agent::ItemStatus::Completed));
        resize(cx, 1600.);
        crate::test_frames::draw_settled(cx);
        // What the header holds after its title: the git button, the tier's
        // controls and what follows them. The title's stretch takes the rest.
        let reach = |cx: &mut VisualTestContext| {
            let header = cx.debug_bounds("chat-header-scroll").expect("the header");
            let git = cx.debug_bounds("git-main").expect("the git button");
            header.right() - git.left()
        };
        let mut changes = 0;
        for width in [1300., 1100., 1000., 900., 820., 760.] {
            let before = reach(cx);
            resize(cx, width);
            cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            draw(cx);
            let partway = reach(cx);
            crate::test_frames::draw_settled(cx);
            let after = reach(cx);
            if (after - before).abs() > px(1.) {
                changes += 1;
                assert!(
                    after < before,
                    "{width}pt: the header widened: {before:?} -> {after:?}"
                );
                assert_between(
                    &format!("{width}pt: the header's tier"),
                    before,
                    after,
                    partway,
                );
            }
        }
        assert!(
            changes >= 2,
            "the labels went, then the toggles: {changes} changes"
        );
        assert!(
            cx.debug_bounds("header-overflow").is_some(),
            "the overflow tier"
        );
    }

    /// thread-1's running turn: a question, a command it runs, and a steer
    /// sent while it works, which the agent has `taken` or not yet.
    fn steered_timeline(taken: bool) -> tcode_core::session::Timeline {
        let mut timeline = tcode_core::session::Timeline::default();
        let mut events = vec![
            agent::AgentEvent::TurnStarted {
                turn_id: "0".into(),
            },
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: "user".into(),
                parent_item_id: None,
                content: agent::ItemContent::UserMessage {
                    text: "Look at the build.".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            }),
            agent::AgentEvent::ItemStarted(agent::ThreadItem {
                id: "first".into(),
                parent_item_id: None,
                content: agent::ItemContent::CommandExecution {
                    command: "cargo build".into(),
                    output: String::new(),
                    exit_code: None,
                    status: agent::ItemStatus::InProgress,
                },
            }),
            agent::AgentEvent::SteerRequested {
                request_id: "steer".into(),
                text: "Also check the docs.".into(),
                attachments: Vec::new(),
            },
        ];
        if taken {
            events.push(agent::AgentEvent::SteerAccepted {
                request_id: "steer".into(),
            });
        }
        for event in &events {
            timeline.apply_at(None, event);
        }
        timeline
    }

    /// A steer the agent takes travels from the turn's trailer to its place
    /// in the turn. A tablet turning compact part way draws the conversation
    /// on the compact page: the steer carries on from where it had got to,
    /// relative to the conversation around it, and is drawn within the
    /// timeline in the frame of the flip, rather than flying from where the
    /// wide layout drew it or vanishing behind the old layout's bounds.
    #[gpui::test]
    fn a_travelling_steer_carries_on_when_the_window_turns_compact(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        as_mobile(cx);
        resize(cx, 1024.);
        let store = store_of(&shell, cx);
        show_timeline(&store, steered_timeline(false), cx);
        settle_frames(cx);
        let _ = sent(&host);
        // From the question's bubble to the steer's.
        let steer = |cx: &mut VisualTestContext| {
            let question = cx.debug_bounds("user-bubble-user").expect("the question");
            let bubble = cx.debug_bounds("user-bubble-steer").expect("the steer");
            bubble.top() - question.top()
        };
        let painted = |cx: &mut VisualTestContext| {
            let bubble = cx.debug_bounds("user-bubble-steer").expect("the steer");
            cx.update(|window, _| {
                let bubble = bubble.scale(window.scale_factor());
                window.painted_quads().iter().any(|quad| {
                    quad.bounds.intersects(&bubble)
                        && quad.background.as_solid().is_some_and(|fill| fill.a > 0.)
                })
            })
        };
        assert!(steer(cx) > px(0.) && painted(cx));

        show_timeline(&store, steered_timeline(true), cx);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 4);
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(cx);
        cx.simulate_resize(size(px(899.), px(800.)));
        draw(cx);
        shell.read_with(cx, |shell, cx| assert!(shell.compact(cx)));
        assert!(painted(cx), "the steer is drawn in the frame of the flip");
        display_frame(cx);
        let partway = steer(cx);
        assert!(painted(cx), "the steer is drawn as it travels on");
        settle_frames(cx);
        let taken = steer(cx);
        // Where it started, on the compact page.
        show_timeline(&store, steered_timeline(false), cx);
        settle_frames(cx);
        let pending = steer(cx);
        assert!(
            taken + px(10.) < pending,
            "the steer moved up: {pending:?} -> {taken:?}"
        );
        assert_between("the travelling steer", pending, taken, partway);
    }

    /// Types `draft` into thread-1's composer on a wide tablet, and a
    /// quarter of the way into what the draft set moving turns the window
    /// compact, which draws the composer under another parent. What `at`
    /// measures on the compact page: part way on, on the first frame it is
    /// drawn on screen as the page glides in, then at rest with the draft
    /// cleared and with it typed again, as `[cleared, partway, drafted]`.
    fn draft_across_breakpoint(
        cx: &mut TestAppContext,
        draft: &str,
        at: impl Fn(&mut VisualTestContext) -> Option<f32>,
    ) -> [f32; 3] {
        let at_first = &at;
        let at = |cx: &mut VisualTestContext| at_first(cx).expect("drawn on screen");
        let (shell, host, cx) = mount_panes(cx);
        as_mobile(cx);
        resize(cx, 1024.);
        settle_frames(cx);
        let _ = sent(&host);
        let focus_composer = |cx: &mut VisualTestContext| {
            let composer = shell.read_with(cx, |shell, cx| {
                shell.attachment.as_ref().expect("attached").all_panes(cx)[0]
                    .read(cx)
                    .chat()
                    .read(cx)
                    .composer()
            });
            let focus = composer.read_with(cx, |composer, cx| composer.input_focus_handle(cx));
            cx.update(|window, cx| focus.focus(window, cx));
            draw(cx);
        };
        focus_composer(cx);
        cx.simulate_input(draft);
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 4);
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(cx);
        cx.simulate_resize(size(px(899.), px(800.)));
        draw(cx);
        shell.read_with(cx, |shell, cx| assert!(shell.compact(cx)));
        display_frame(cx);
        let mut partway = at_first(cx);
        for _ in 0..4 {
            if partway.is_some() {
                break;
            }
            display_frame(cx);
            partway = at_first(cx);
        }
        let partway = partway.expect("drawn on screen part way");
        settle_frames(cx);
        let drafted = at(cx);
        focus_composer(cx);
        cx.simulate_keystrokes("secondary-a backspace");
        settle_frames(cx);
        let cleared = at(cx);
        focus_composer(cx);
        cx.simulate_input(draft);
        settle_frames(cx);
        assert_eq!(at(cx), drafted, "typed again, it comes to rest there");
        [cleared, partway, drafted]
    }

    /// Send is dim while there is nothing to send, and brightens as text
    /// comes. The window turning compact part way draws the button on the
    /// compact page, keeping its clock in the composer (`Button::motion`):
    /// it carries on brightening from where it was, not from its end.
    #[gpui::test]
    fn the_send_button_carries_on_brightening_when_the_window_turns_compact(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // How opaque the button's fill is painted.
        let fill = |cx: &mut VisualTestContext| {
            let button = cx.debug_bounds("send-message").expect("the send button");
            cx.update(|window, _| {
                let button = button.scale(window.scale_factor());
                window
                    .painted_quads()
                    .iter()
                    .filter(|quad| {
                        quad.bounds.contains(&button.center())
                            && (quad.bounds.size.width.0 - button.size.width.0).abs() < 1.
                            && (quad.bounds.size.height.0 - button.size.height.0).abs() < 1.
                    })
                    .filter_map(|quad| quad.background.as_solid().map(|fill| fill.a))
                    .reduce(f32::max)
            })
        };
        let [dim, partway, bright] = draft_across_breakpoint(cx, "Look at the build.", fill);
        assert!(
            dim + 0.1 < bright,
            "the button brightens: {dim} -> {bright}"
        );
        // In hundredths, for the half-point margin the comparison allows.
        assert_between(
            "the send button's dim",
            px(dim * 100.),
            px(bright * 100.),
            px(partway * 100.),
        );
    }

    /// The field grows a line at a time as the draft does, its height
    /// gliding there. The window turning compact part way draws the
    /// composer on the compact page, the glide kept by the composer: the
    /// field carries on growing from where it was.
    #[gpui::test]
    fn the_draft_carries_on_growing_when_the_window_turns_compact(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let card = |cx: &mut VisualTestContext| {
            let card = cx.debug_bounds("composer-card").expect("the card");
            Some(f32::from(card.size.height))
        };
        let [one_line, partway, four_lines] =
            draft_across_breakpoint(cx, "one\ntwo\nthree\nfour", card);
        assert!(
            one_line + 30. < four_lines,
            "the field grew: {one_line} -> {four_lines}"
        );
        assert_between(
            "the draft's height",
            px(one_line),
            px(four_lines),
            px(partway),
        );
    }

    /// thread-1 as `turns` finished turns, each a short question and its
    /// answer, the last answer `last`.
    fn finished_timeline(turns: usize, last: &str) -> tcode_core::session::Timeline {
        use crate::chat::tests::{assistant, entry, user_item};
        let mut timeline = tcode_core::session::Timeline::default();
        timeline.turns = vec![tcode_core::session::TurnMeta::default(); turns];
        for turn in 0..turns {
            let answer = if turn + 1 == turns {
                last.to_string()
            } else {
                format!("Answer {turn}.")
            };
            for mut item in [
                entry(
                    &format!("ask-{turn}"),
                    user_item(&format!("Question {turn}?")),
                ),
                entry(&format!("answer-{turn}"), assistant(&answer)),
            ] {
                std::sync::Arc::make_mut(&mut item).turn = turn;
                timeline.entries.push(item);
            }
        }
        timeline
    }

    /// The end of a long conversation pulled past its edge and let go: the
    /// edge is on its way back when the pane is zoomed over the column, and
    /// carries on back from where it was rather than snapping home.
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    #[gpui::test]
    fn a_returning_timeline_edge_carries_on_when_its_pane_is_zoomed(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        show_timeline(&store, finished_timeline(30, "Done."), cx);
        cx.simulate_keystrokes("secondary-d");
        settle_frames(cx);
        cx.simulate_keystrokes("secondary-alt-left");
        settle_frames(cx);
        let _ = sent(&host);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
        // How far the last question is drawn from the composer, which the
        // bounce leaves where it is: what the edge is stretched by.
        let stretch = |cx: &mut VisualTestContext| {
            let question = cx
                .debug_bounds("user-bubble-ask-29")
                .expect("the last question");
            let composer = cx.debug_bounds("chat-composer").expect("the composer");
            question.top() - composer.top()
        };
        let rest = stretch(cx);
        let timeline = cx
            .debug_bounds("user-bubble-ask-29")
            .expect("the last question");
        let wheel = |delta: f32, phase: gpui::TouchPhase, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollEvent {
                position: timeline.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(delta))),
                touch_phase: phase,
                ..Default::default()
            });
            draw(cx);
        };
        // Past the end, held still, then let go.
        wheel(0., gpui::TouchPhase::Started, cx);
        for _ in 0..3 {
            wheel(-100., gpui::TouchPhase::Moved, cx);
        }
        cx.executor().advance_clock(Duration::from_millis(100));
        wheel(0., gpui::TouchPhase::Ended, cx);
        display_frame(cx);
        let returning = stretch(cx) - rest;
        assert!(returning < px(-4.), "the edge is stretched: {returning:?}");

        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        assert!(cx.debug_bounds("zoomed-pane").is_some());
        display_frame(cx);
        assert_between("the returning edge", px(0.), returning, stretch(cx) - rest);
        settle_frames(cx);
        assert_eq!(stretch(cx), rest, "the edge came home");
    }

    /// Settings → Plugins on a tablet, with more plugins to install than its
    /// list shows, opened as the compact window's section page and then
    /// shown wide: the list is scrolled, and its scrollbar is fading out
    /// once the hold after the scroll has ended when the window turns
    /// compact again. Drawn on the section page, it fades on from where it
    /// was, rather than starting over hidden; with motion reduced it is gone
    /// at once.
    #[gpui::test]
    fn a_fading_scrollbar_carries_on_when_the_window_turns_compact(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount(cx);
        as_mobile(cx);
        // Tall enough to show the whole list at either width.
        let window = |width: f32| size(px(width), px(1400.));
        cx.simulate_resize(window(899.));
        draw(cx);
        let mut settings = tcode_core::settings::Settings {
            language: Some(crate::LANGUAGE_ENGLISH.into()),
            ..Default::default()
        };
        settings
            .apply(tcode_protocol::SettingsPatch::PluginManagementProvider {
                provider: agent::ProviderKind::ClaudeCode,
                enabled: true,
            })
            .unwrap();
        let entries = (0..30)
            .map(|index| agent::ProviderPluginEntry {
                id: format!("plugin-{index}@mkt"),
                name: format!("plugin-{index}"),
                version: Some("1.0.0".into()),
                description: Some("A plugin to install".into()),
                source: agent::PluginSource {
                    marketplace: Some("mkt".into()),
                    kind: agent::PluginSourceKind::LocalPath,
                },
                installations: Vec::new(),
                enabled: agent::Tri::No,
                declared: None,
                errors: Vec::new(),
                actions: vec![agent::PluginAction::Install {
                    scope: agent::PluginScope::User,
                }],
                diagnostics: Vec::new(),
            })
            .collect();
        send_events(
            &host,
            vec![
                (Topic::Settings, ServerEvent::SettingsSnapshot(settings)),
                (
                    Topic::Providers,
                    ServerEvent::ProvidersReplaced(tcode_protocol::ProvidersStatus {
                        plugins: vec![tcode_protocol::ProviderPluginCatalog {
                            profile_id: "claude".into(),
                            context_cwd: None,
                            marketplaces: Vec::new(),
                            marketplace_actions: Vec::new(),
                            entries,
                            errors: Vec::new(),
                            state: tcode_protocol::PluginCatalogState::Fresh,
                            loading: false,
                            pending: Vec::new(),
                            challenges: Vec::new(),
                        }],
                        ..Default::default()
                    }),
                ),
            ],
        );
        await_restore_update(&shell, cx, |store, _| {
            store
                .provider_plugin_catalog("claude")
                .is_some_and(|catalog| catalog.entries.len() == 30)
                && store
                    .plugin_management()
                    .provider_switch(agent::ProviderKind::ClaudeCode)
        });
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| {
            state.pending_settings_section = Some("plugins".into());
            state.open_settings(cx);
        });
        crate::test_frames::draw_settled(cx);
        // Opened compact, the section is a page of the window's history, so
        // the window shows it again when it turns compact.
        shell.read_with(cx, |shell, cx| {
            assert_eq!(shell.destination(cx), Destination::SettingsSection)
        });
        cx.simulate_resize(window(1024.));
        crate::test_frames::draw_settled(cx);
        shell.read_with(cx, |shell, cx| assert!(!shell.compact(cx)));
        let tab = cx
            .debug_bounds("plugins-view-available-claude")
            .expect("the plugins to install have a tab");
        cx.simulate_click(tab.center(), gpui::Modifiers::default());
        crate::test_frames::draw_settled(cx);
        // How far in the thumb is painted in a full frame, from 0 to 1.
        let thumb = |cx: &mut VisualTestContext| {
            frame(cx, |window, cx| {
                let thumb = cx.theme().scrollbar_thumb;
                let scale = window.scale_factor();
                crate::test_frames::painted_quads(window)
                    .iter()
                    .filter(|quad| {
                        let width = quad.bounds.size.width.0 / scale;
                        (5. ..9.).contains(&width) && quad.bounds.size.height.0 / scale >= 40.
                    })
                    .filter_map(|quad| quad.background.as_solid())
                    .filter(|fill| {
                        (fill.h - thumb.h).abs() < 0.01 && (fill.l - thumb.l).abs() < 0.01
                    })
                    .map(|fill| fill.a / thumb.a)
                    .fold(0., f32::max)
            })
        };
        assert_eq!(thumb(cx), 0., "hidden until the list scrolls");
        let row = cx
            .debug_bounds("plugin-available-claude-plugin-0@mkt")
            .expect("the plugins to install are listed");
        assert!(row.bottom() < px(1400.), "the list is on screen: {row:?}");
        cx.simulate_event(gpui::ScrollEvent {
            position: row.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-120.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        });
        let scrolled = cx.executor().now();
        crate::test_frames::draw_settled(cx);
        assert_eq!(thumb(cx), 1., "shown as the list scrolls");
        // The hold after the scroll ends, and the scrollbar starts out. The
        // hold runs from the scroll, so the frames that faded it in count.
        let motion = cx.update(|_, cx| cx.global::<gpui_base::Theme>().scrollbar.motion());
        let shown_for = cx.executor().now().saturating_duration_since(scrolled);
        cx.executor()
            .advance_clock(motion.idle().saturating_sub(shown_for));
        draw(cx);
        cx.executor().advance_clock(motion.exit() / 2);
        draw(cx);
        let fading = thumb(cx);
        assert!(fading > 0.05 && fading < 0.95, "fading out: {fading}");

        cx.simulate_resize(window(899.));
        draw(cx);
        shell.read_with(cx, |shell, cx| {
            assert!(shell.compact(cx));
            assert_eq!(shell.destination(cx), Destination::SettingsSection);
        });
        cx.executor().advance_clock(motion.exit() / 4);
        draw(cx);
        let carried = thumb(cx);
        assert!(
            carried > 0.01 && carried < fading,
            "the scrollbar restarted under its new parent: {carried}, from {fading}"
        );
        crate::test_frames::draw_settled(cx);
        assert_eq!(thumb(cx), 0., "faded out");

        // Shown again, and with motion reduced gone at once; the first rows
        // are scrolled out of the list.
        let row = cx
            .debug_bounds("plugin-available-claude-plugin-4@mkt")
            .expect("the list on the compact page");
        cx.simulate_event(gpui::ScrollEvent {
            position: row.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(120.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        });
        crate::test_frames::draw_settled(cx);
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.executor().advance_clock(motion.idle());
        draw(cx);
        assert_eq!(thumb(cx), 0., "reduced motion: gone at once");
    }

    /// A tablet turning wide in Settings shows the settings route as it is:
    /// turning wide changes no route, so the route the window last drew
    /// wide is not drawn beneath it with Settings rising over it.
    #[gpui::test]
    fn turning_wide_in_settings_draws_the_route_at_rest(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, _host, cx) = mount(cx);
        as_mobile(cx);
        resize(cx, 1024.);
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("chat-route").is_some());
        resize(cx, 899.);
        shell.read_with(cx, |shell, cx| assert!(shell.compact(cx)));
        let state = shell.read_with(cx, |shell, _| shell.window_state());
        state.update(cx, |state, cx| state.open_settings(cx));
        crate::test_frames::draw_settled(cx);

        resize(cx, 1024.);
        shell.read_with(cx, |shell, cx| assert!(!shell.compact(cx)));
        assert!(cx.debug_bounds("settings-route").is_some());
        assert!(
            cx.debug_bounds("chat-route").is_none(),
            "the chat is drawn beneath Settings as the window turns wide"
        );
    }

    /// How much of `fill` was painted over exactly `bounds` in the last
    /// frame, from 0 (none) to 1 (all of it).
    fn painted_share(
        bounds: gpui::Bounds<Pixels>,
        fill: impl FnOnce(&App) -> gpui::Hsla,
        cx: &mut VisualTestContext,
    ) -> f32 {
        cx.update(|window, cx| {
            let fill = fill(cx);
            let bounds = bounds.scale(window.scale_factor());
            window
                .painted_quads()
                .iter()
                .filter(|quad| {
                    (quad.bounds.origin.x - bounds.origin.x).0.abs() < 1.
                        && (quad.bounds.origin.y - bounds.origin.y).0.abs() < 1.
                        && (quad.bounds.size.width - bounds.size.width).0.abs() < 1.
                        && (quad.bounds.size.height - bounds.size.height).0.abs() < 1.
                })
                .filter_map(|quad| quad.background.as_solid())
                .filter(|painted| {
                    (painted.h - fill.h).abs() < 0.01 && (painted.l - fill.l).abs() < 0.01
                })
                .map(|painted| painted.a / fill.a)
                .fold(0., f32::max)
        })
    }

    /// How far in the list hover fill painted over `row` is, from 0 to 1.
    fn hover_fill(row: gpui::Bounds<Pixels>, cx: &mut VisualTestContext) -> f32 {
        painted_share(row, |cx| cx.theme().list_hover, cx)
    }

    /// A formula's actions are revealed on hover over the block in a wide
    /// window and pinned beside the formula in a compact one. A tablet
    /// turning compact draws the conversation under a new parent: the pin
    /// fades in and the actions move from over the block's end to beside it,
    /// rather than appearing there at once; with motion reduced the way back
    /// is at once.
    #[gpui::test]
    fn a_formulas_actions_glide_beside_it_when_the_window_turns_compact(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        as_mobile(cx);
        resize(cx, 1024.);
        let store = store_of(&shell, cx);
        show_timeline(
            &store,
            finished_timeline(1, "The sum:\n\n```latex\n1+2=3\n```"),
            cx,
        );
        settle_frames(cx);
        let _ = sent(&host);
        // How far in the actions are, and how far their end is from the
        // block's.
        let actions = |cx: &mut VisualTestContext| {
            let block = cx
                .debug_bounds("markdown-math-root-1")
                .expect("the formula");
            let actions = cx
                .debug_bounds("markdown-math-actions-root-1")
                .expect("its actions");
            let shown = painted_share(actions, |cx| cx.theme().tokens.colors.muted, cx);
            (shown, actions.right() - block.right())
        };
        let (hidden, over) = actions(cx);
        assert_eq!(hidden, 0., "revealed on hover only");
        assert!(over < px(-1.), "over the block's end: {over:?}");

        cx.simulate_resize(size(px(899.), px(800.)));
        draw(cx);
        shell.read_with(cx, |shell, cx| assert!(shell.compact(cx)));
        cx.executor().advance_clock(crate::edge_back::SETTLE / 3);
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        draw(cx);
        let (fading, moving) = actions(cx);
        settle_frames(cx);
        let (pinned, beside) = actions(cx);
        assert_eq!(pinned, 1., "pinned on a compact window");
        assert!(beside > over + px(1.), "beside the formula: {beside:?}");
        assert!(
            fading > 0.05 && fading < 0.95,
            "the pin restarted under its new parent: {fading}"
        );
        assert_between("the actions", over, beside, moving);

        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.simulate_resize(size(px(1024.), px(800.)));
        draw(cx);
        assert_eq!(actions(cx), (0., over), "reduced motion: at once");
    }

    /// The pane beside an empty one closes: the empty pane shows the start
    /// hub as the closing pane glides away, and is drawn under a new parent
    /// once the closed pane leaves the dock's tree. The fill coming in under
    /// the project the pointer is on carries on from where it was then,
    /// rather than starting over at rest; with motion reduced it comes and
    /// goes at once.
    #[gpui::test]
    fn a_start_hub_rows_fill_carries_on_when_the_pane_beside_it_closes(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let (topic, mut index) = panes_index(&[]);
        if let ServerEvent::IndexSnapshot(snapshot) = &mut index {
            snapshot.projects = vec![tcode_core::project::Project {
                id: "project".into(),
                name: "Project".into(),
                root: PANES_CWD.into(),
                quick_chat: false,
                icon_path: None,
                permission_defaults: Default::default(),
                created_at: 0,
            }];
        }
        send_events(&host, vec![(topic, index)]);
        await_restore_update(&shell, cx, |store, _| !store.projects().is_empty());
        cx.simulate_keystrokes("secondary-d");
        settle_frames(cx);
        cx.simulate_keystrokes("secondary-alt-left");
        settle_frames(cx);
        let _ = sent(&host);
        assert_eq!(pane_threads(&shell, cx), [Some("thread-1".into()), None]);
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        let row = |cx: &mut VisualTestContext| {
            cx.debug_bounds("start-hub-project-project")
                .expect("the empty pane's project")
        };

        // The thread's pane closes; once the hub has glided over, the
        // pointer comes onto its project.
        cx.simulate_keystrokes("secondary-shift-w");
        display_frame(cx);
        let mut last = row(cx);
        loop {
            display_frame(cx);
            let now = row(cx);
            if now == last {
                break;
            }
            last = now;
        }
        assert_eq!(
            pane_threads(&shell, cx).len(),
            2,
            "the pane is still closing"
        );
        cx.simulate_mouse_move(last.center(), None, gpui::Modifiers::none());
        draw(cx);
        let mut before = 0.;
        while pane_threads(&shell, cx).len() == 2 {
            before = hover_fill(row(cx), cx);
            display_frame(cx);
        }
        assert_eq!(pane_threads(&shell, cx), [None]);
        let lifted = hover_fill(row(cx), cx);
        assert!(
            before > 0.05 && before < 0.95,
            "the fill was coming in: {before}"
        );
        assert!(
            lifted >= before,
            "the fill restarted under its new parent: {before} -> {lifted}"
        );
        settle_frames(cx);
        assert_eq!(hover_fill(row(cx), cx), 1., "the fill came in");

        cx.update(|_, cx| cx.set_reduce_motion(true));
        let at = row(cx);
        cx.simulate_mouse_move(
            at.center() + gpui::point(px(0.), px(200.)),
            None,
            gpui::Modifiers::none(),
        );
        draw(cx);
        assert_eq!(
            hover_fill(at, cx),
            0.,
            "reduced motion takes the fill away at once"
        );
        cx.simulate_mouse_move(at.center(), None, gpui::Modifiers::none());
        draw(cx);
        assert_eq!(hover_fill(at, cx), 1., "reduced motion fills at once");
    }

    /// A control the keyboard is on keeps it, and with it its fading ring,
    /// which is kept by its focus handle, when its view is drawn under a new
    /// parent: a command's header in the left of two panes, as the pane is
    /// zoomed over the column, back, and as the tablet window turns compact.
    #[gpui::test]
    fn a_focused_control_keeps_the_keyboard_through_a_zoom_and_a_compact_flip(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (shell, host, cx) = mount_panes(cx);
        let store = store_of(&shell, cx);
        show_timeline(&store, reparented_timeline(2, false), cx);
        settle_frames(cx);
        // Shut, the command's header is the timeline's last control.
        click_command(cx);
        cx.simulate_keystrokes("secondary-d");
        settle_frames(cx);
        cx.simulate_keystrokes("secondary-alt-left");
        settle_frames(cx);
        let _ = sent(&host);
        // Back from the composer to the header, as Shift-Tab takes it.
        cx.update(|window, cx| window.focus_prev(cx));
        cx.simulate_keystrokes("shift");
        settle_frames(cx);
        let focused = cx
            .update(|window, cx| window.focused(cx))
            .expect("the header holds the keyboard");
        assert_eq!(keyboard_pane(&shell, cx), Some(0));
        // Enter presses the control the keyboard is on: the header opens or
        // shuts its command.
        let still = |what: &str, cx: &mut VisualTestContext| {
            assert_eq!(
                cx.update(|window, cx| window.focused(cx)).as_ref(),
                Some(&focused),
                "{what}: the header lost the keyboard"
            );
            let before = command_to_status(cx);
            cx.simulate_keystrokes("enter");
            cx.simulate_event(gpui::KeyUpEvent {
                keystroke: gpui::Keystroke::parse("enter").expect("a keystroke"),
            });
            settle_frames(cx);
            let after = command_to_status(cx);
            assert!(
                (after - before).abs() > px(10.),
                "{what}: enter did not press the header: {before:?} -> {after:?}"
            );
        };
        still("before", cx);

        cx.simulate_keystrokes("secondary-shift-enter");
        draw(cx);
        assert!(cx.debug_bounds("zoomed-pane").is_some());
        still("zooming", cx);
        settle_frames(cx);
        still("zoomed", cx);
        cx.simulate_keystrokes("secondary-shift-enter");
        settle_frames(cx);
        assert!(cx.debug_bounds("zoomed-pane").is_none());
        still("back in its slot", cx);

        // A tablet's window, at a width that turns compact.
        as_mobile(cx);
        cx.simulate_resize(size(px(899.), px(800.)));
        draw(cx);
        shell.read_with(cx, |shell, cx| assert!(shell.compact(cx)));
        still("turning compact", cx);
        settle_frames(cx);
        still("compact", cx);
    }
}
