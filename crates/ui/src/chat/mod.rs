use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

/// How long a timeline row takes to come in, or to fold away as it leaves.
const ROWS_IN: std::time::Duration = std::time::Duration::from_millis(200);
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use std::path::{Path, PathBuf};
use std::rc::Rc;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) mod components;
mod empty;
mod header;
pub(crate) mod model;
mod rail;
mod side;
pub(crate) use model::{format_elapsed, format_elapsed_seconds};
pub(crate) use side::{CloseSide, SIDE_CONTEXT};
mod timeline;
pub(crate) mod tools;

pub(crate) use header::{
    HEADER_COMPACT_MIN_WIDTH, HEADER_OVERFLOW_MIN_WIDTH, PANE_CLOSE_WIDTH, header_extra_width_for,
};

use crate::markdown::{SelectionArea, SelectionDocument};
use crate::overlay::OverlayExt as _;
use crate::scroll::ScrollableElement as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::copy::CopiedMark;
use crate::widgets::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, open_in_zed};
use crate::widgets::scrollbar::{Scrollbar, TRACK};
use crate::widgets::tooltip::Tooltip;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::{ItemContent, RewindMode};
use gpui::{
    Action, Anchor, AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity,
    FollowMode, InteractiveElement as _, IntoElement, ListAlignment, ListOffset, ListState,
    ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, list, prelude::FluentBuilder as _, px,
};
use gpui_base::{ElementExt as _, StyledExt as _, h_flex, v_flex};
use serde::Deserialize;

use tcode_core::git::GitAction;
use tcode_core::notice::{HostNotice, OrchestrateCallback};
use tcode_core::session::{EntryContent, SteeringStatus, Timeline, TimelineEntry, TurnMeta};
use tcode_core::ui::RightTab;

use crate::commit_dialog::CommitDialog;
use crate::composer::Composer;
use crate::git::{git_action_label_key, git_hint_key};
use crate::shortcut::format_secondary_shortcut;
use crate::store::WorkspaceStore;
use crate::terminal_drawer::TerminalDrawer;
use crate::time::now_secs;
use crate::window_caption;
use crate::window_drag_area;
use crate::window_state::{NavigationPanel, OpenPanel, WindowState};

use self::components::assistant::MdState;
pub use self::timeline::CaptureScroll;
use self::timeline::{
    AutoActivityExpansions, CaptureSession, FullOutput, LeftRow, PendingMarkdownBuild,
    ReservationScrollBack, history_screens_covered, jump_to_latest_visible, settle_carried_anchor,
    timeline_bottom_inset, transcript_selection,
};

/// A message's context-menu rewind, which the chat applies as the bubble's
/// rewind popover does.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_chat, no_json)]
pub(crate) struct RewindTurn {
    pub(crate) turn: usize,
    pub(crate) mode: RewindMode,
}

gpui::actions!(tcode_chat, [OpenMoveThreadDialog, RevokeSelfSettle]);

/// A window's context-menu "Copy output" for an output the record holds only
/// a preview of: the whole of it, loaded first when it is not here yet.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_chat, no_json)]
pub(crate) struct CopyWholeOutput {
    pub(crate) item_id: String,
    pub(crate) turn: usize,
}

/// A changed-file chip's context-menu "View diff".
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_chat, no_json)]
pub(crate) struct OpenFileDiff {
    pub(crate) turn: usize,
    pub(crate) path: String,
}

use self::model::{
    LimitResumeAction, RowRenderArgs, Segment, activity_run_duration_ms, displayed_error_text,
    divergent_served_model, latest_message_ids, limit_resume_action, live_edit_counts, notice_keys,
    partition_activity_run, plain_text_as_markdown, segment_entries, start_hub_projects,
    timeline_overdraw, user_content, user_notice, user_visible_text, work_log_capsule_label,
    work_log_counts, work_log_key, work_log_outcome,
};
use self::timeline::rows::{
    ListSync, RowPart, TimelineContinuity, TimelineRow, TurnIndexCache, row_of_entry, rows_of_turn,
};
use self::tools::diff::InlineDiffCache;
use self::tools::shell::CommandPanelCache;
pub(crate) use crate::material::{
    CHAT_CONTENT_MAX_WIDTH as CONTENT_MAX_WIDTH, CHAT_CONTENT_MIN_PADDING as CONTENT_MIN_PADDING,
};

/// Which of the window's top corners a conversation's header runs into, and
/// so which window chrome it makes room for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowEdges {
    /// Where macOS draws the traffic lights over a collapsed sidebar.
    pub(crate) left: bool,
    /// Where Windows' caption buttons sit with no right panel open.
    pub(crate) right: bool,
}

impl WindowEdges {
    pub(crate) const BOTH: Self = Self {
        left: true,
        right: true,
    };
}

pub struct ChatView {
    workspace_store: Entity<WorkspaceStore>,
    /// The thread this view shows and acts on.
    session_id: Option<String>,
    edges: WindowEdges,
    /// What lays the view out ([`ChatFrame`]), and the width it gave the
    /// view this frame, which the header's tiers, the rail's room and the
    /// column's centre follow.
    frame: Entity<ChatFrame>,
    width: gpui::Pixels,
    /// The close button at the header's right end, while the conversation's
    /// pane shares the column: the pane's focus handle, faded in and out by
    /// the shell, which samples it above the dock.
    pane_close: Option<crate::motion::Shown<gpui::WeakEntity<crate::panes::ThreadPanel>>>,
    /// The pane commands the header offers, as the shell last handed them.
    /// Handed rather than read from the shell and the pane as the view
    /// renders: both lay themselves out again whenever the view does, and
    /// reading them would draw each again after the other, every frame.
    pane_controls: Option<crate::shell::PaneControls>,
    /// The header's scroll, kept by the view rather than by the header's
    /// element: the dock draws a pane's chat under a new parent whenever the
    /// panes' layout changes (see `panes::ThreadPanes`), which would scroll
    /// an element-keyed header back to its start.
    header_scroll: gpui::ScrollHandle,
    /// The scene of the thread the view shows: everything the view, its
    /// composer and its terminal drawer draw keeps its motion under it, so
    /// another thread shown starts all of it at rest, while the same thread
    /// drawn under a new parent carries on.
    scene: crate::motion::Epoch,
    /// The motion of everything the view draws, kept by the view, in its
    /// scene: the window draws the conversation under a new parent as it
    /// turns compact or wide, a zoom as it starts and lands, and the dock
    /// when it builds the panes' tree anew, which would start element-keyed
    /// motion over at its end. Swept as the view renders, so what is drawn
    /// again after a frame without it starts at rest.
    pub(super) motion: crate::motion::owned::Owned,
    /// Whether what the view shows has been shown since it was bound
    /// ([`Self::start_scene`]). Until it can be drawn at rest it is laid
    /// out unseen, so its fade in starts once there is something to fade
    /// in rather than playing out while it loads.
    scene_shown: bool,
    /// When the view was bound to what it shows.
    scene_at: Instant,
    /// A host holds the view unseen and fades it in itself once it
    /// [appears](Self::appears) (a pane from its waiting hint, the compact
    /// page over the thread it replaces): the surface takes each state at
    /// once rather than fading too.
    held: bool,
    /// Whether the last frame drew the thread, and with it the composer.
    composer_drawn: bool,
    title_recedes: bool,
    pull_requests: Entity<crate::pull_requests::PullRequestsPanel>,
    window_state: Entity<WindowState>,
    composer: Entity<Composer>,
    /// The composer's layout as this view last handed it, and where the
    /// composer reports its card's bottom. The view never reads the composer
    /// as it renders: a read would build the whole chat again every time the
    /// composer changes, on every keystroke.
    composer_layout: (bool, gpui::Pixels),
    composer_below_card: Rc<Cell<gpui::Pixels>>,
    terminal_drawer: Entity<TerminalDrawer>,
    /// The conversation and terminal drawer's split, and the drawer gliding
    /// in and out of it.
    terminal_split: Entity<gpui_base::ResizableState>,
    terminal_glide: crate::motion::resize_glide::ResizeGlide,
    /// Whether the last frame was compact, which has no drawer.
    terminal_glide_compact: bool,
    /// How the body under the header sits at the width it was laid out at
    /// this frame: set as the body is laid out, before the rows and the
    /// composer that it moves are.
    body: rail::BodyLayout,
    /// The agent shown beside the thread, opened from it.
    side: side::Side,
    /// Set while the view is itself an agent's shown beside its lead.
    side_of: Option<side::SideOf>,
    /// How wide the pane drawing this view is, its right panel's room
    /// included: nothing for a view no pane draws.
    pane_width: gpui::Pixels,
    /// How far past the body's right edge the timeline's list runs, under
    /// the panel: see [`Self::list_overhang`].
    overhang: gpui::Pixels,
    /// The agent beside this thread whose diff the pane's drawer shows, in
    /// place of this thread's panel. Here because it stays with the thread
    /// when the thread changes panes.
    drawer_agent: Option<String>,
    /// The Agents sheet's content on phone.
    agents: Entity<crate::agents_panel::AgentsPanel>,
    terminal_was_open: bool,
    list_state: ListState,
    /// The timeline changed since the list last mirrored it. GPUI's list
    /// resolves wheel and pan packets against the row indices it painted, so
    /// rows are only spliced during a frame, never between two frames.
    timeline_stale: bool,
    /// The store's revision of the thread's timeline as the list last
    /// mirrored it: a record streamed in moves it on without notifying the
    /// store.
    timeline_revision: Option<u64>,
    history_placeholder_height: gpui::Pixels,
    /// Height of the phone row floating over the foot of the timeline, as
    /// far as it has risen in: zero while it is absent. The last row's
    /// padding holds it so the tail clears it.
    phone_strip_height: gpui::Pixels,
    /// Distance to scroll back once a page has filled the reservation the
    /// reader had scrolled into. Applied by the render after the frame that
    /// measured the page, never between frames: GPUI resolves the wheel and
    /// pan packets of a moving finger against the anchor it painted, so a
    /// scroll applied between two frames is either overwritten by the next
    /// packet or would have to give up when one arrived first.
    reservation_scroll_back: Option<ReservationScrollBack>,
    /// The anchor was carried into rows not yet measured.
    carried_anchor: bool,
    rows: Vec<TimelineRow>,
    turn_index_cache: TurnIndexCache,
    /// Rows that arrived or left while the reader looked, by identity,
    /// until they have risen in or folded away.
    rows_motion: crate::motion::list_rows::ListRows<bool, LeftRow>,
    /// The folds of the rows that left below the row being drawn, for a
    /// collapsed live Work Log to draw between its header and its live
    /// window, where they stood; whatever it leaves goes below the row.
    row_folds: Vec<AnyElement>,
    /// Where the timeline's shared elements (a steer, a turn's status) were
    /// drawn, so one that moves to another row travels there.
    shared: crate::motion::shared::SharedMoves,
    /// Where what opens an agent beside the thread was drawn, for the
    /// agent's card to grow out of it and shrink back into it.
    side_origins: crate::motion::expand::Origins,
    /// The store's fold count for the thread at the last sync: a different
    /// one means the rows came from a fold, not from a live record.
    timeline_folds: u64,
    /// A Work Log was expanded or collapsed since the last sync, so the rows
    /// it gains or loses are the reader's own doing, not arrivals.
    rows_regrouped: bool,
    /// The delivery bubbles drawn last frame: the timeline row that replaces
    /// one is the same message, already on screen.
    shown_deliveries: Vec<String>,
    /// The plan item whose block carries the decision its agent waits on.
    /// The row signature does not see approvals, so a change remeasures the
    /// rows of the plans gaining and losing it.
    deciding_plan: Option<String>,
    md_states: HashMap<String, MdState>,
    /// The transcript's one text selection, across its messages; each
    /// message is the entry at its index in the timeline. It goes when the
    /// focus leaves the whole view.
    selection: Entity<SelectionDocument>,
    /// Holds the focus of everything the view draws, the composer included,
    /// so the selection knows when the focus has left it.
    focus_handle: gpui::FocusHandle,
    pending_md_builds: HashMap<String, PendingMarkdownBuild>,
    next_md_build_generation: u64,
    markdown_visible_rows: Range<usize>,
    markdown_scroll_top: Option<usize>,
    /// The rows the timeline paints in the frame being drawn: GPUI's list
    /// prepaints only the rows on screen.
    painted_rows: Rc<Cell<Option<(usize, usize)>>>,
    /// The first row painted while following the tail.
    painted_tail_start: Option<usize>,
    /// Open/closed keys for collapsibles other than activity details.
    expanded: HashSet<String>,
    /// The screenshots the rail's Screen card steps through.
    screen: rail::screen::Screenshots,
    /// The commands the rail's Commands card lists.
    commands: rail::commands::Commands,
    auto_activity_expansions: AutoActivityExpansions,
    command_panels: RefCell<CommandPanelCache>,
    inline_diffs: InlineDiffCache,
    tool_texts: tools::DerivedTexts,
    /// Whole tool outputs fetched for the open session, by item id.
    full_outputs: HashMap<String, FullOutput>,
    /// The output a Copy waits for, copied once it has loaded.
    copy_when_loaded: Option<String>,
    /// The timeline item the pointer is over: a message whose actions fade
    /// in, or a header, a row or a link whose hover fill or underline does.
    pub(super) hover: crate::motion::ListHover,
    /// The timeline's glide to a turn or to its end; the reader's own
    /// scroll stops it.
    pub(super) list_scroll: crate::motion::ListScroll,
    /// The thread and delivery key of a message sent while the reader was
    /// scrolled back, until it gets through or is refused.
    sent_message: Option<(String, String)>,
    session_key: Option<String>,
    /// The turn the reader jumped to (a palette hit, a thread anchor), by
    /// its turn number in the whole thread: a history page moves the held
    /// window's turn indices under it. Marked until the thread changes, the
    /// keyboard is put to use elsewhere, or a press lands inside it.
    highlighted_turn: Option<usize>,
    /// Keyed `plan`, `user:<id>`, `assistant:<id>` or `error:<id>`.
    copied: CopiedMark,
    /// The live commit dialog entity while it is open (kept alive across frames).
    commit_dialog: Option<Entity<CommitDialog>>,
    /// A system scrolling screenshot is driving the timeline: where it was
    /// before the capture moved it. History paging and tail following stay
    /// off until the capture ends so the origin keeps meaning the same pixel.
    capture: Option<CaptureSession>,
    _subscriptions: Vec<Subscription>,
}

impl ChatView {
    /// The composer takes the keyboard, or takes it as it is drawn: until a
    /// thread has loaded the composer is in no pane, so the view keeps the
    /// keyboard in the pane meanwhile. The composer itself declines on a
    /// mobile build, where focus would raise the software keyboard, and so
    /// does the view, whose focus it would inherit; a desktop window of any
    /// width types.
    pub fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer_drawn {
            self.composer
                .update(cx, |composer, cx| composer.focus(window, cx));
        } else if self.session_id.is_some() && !crate::window_seam::is_mobile(cx) {
            self.focus_handle.focus(window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn composer(&self) -> Entity<crate::composer::Composer> {
        self.composer.clone()
    }

    /// The thread's terminal workspace. Compact windows have no room to split
    /// it under the timeline, so the shell shows this same entity as its own
    /// destination rather than compiling the portable terminal away.
    pub(crate) fn terminal_drawer(&self) -> Entity<TerminalDrawer> {
        self.terminal_drawer.clone()
    }
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        session_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let compact = window_state.read(cx).compact;
        let scene = crate::motion::Epoch::default();
        scene.show(session_id.as_deref());
        let composer = cx.new(|cx| {
            if compact {
                Composer::new_with_layout(
                    workspace_store.clone(),
                    session_id.clone(),
                    true,
                    window,
                    cx,
                )
            } else {
                Composer::new(workspace_store.clone(), session_id.clone(), window, cx)
            }
        });
        // Measure enough rows to establish the six-screen window even when
        // distant rows still have unknown heights in ListState's summary.
        let height = f32::from(window.bounds().size.height);
        let overdraw =
            timeline_overdraw(height).max(height * (crate::store::HISTORY_WINDOW_SCREENS + 1.));
        let list_state = ListState::new(0, ListAlignment::Bottom, px(overdraw));
        list_state.set_follow_mode(FollowMode::Tail);
        let chat = cx.entity().downgrade();
        let list_scroll = crate::motion::ListScroll::default();
        let glide = list_scroll.clone();
        list_state.set_scroll_handler(move |event, window, cx| {
            glide.cancel();
            let visible_rows = event.visible_range.clone();
            let chat = chat.clone();
            window.defer(cx, move |_, cx| {
                let _ = chat.update(cx, |chat, cx| {
                    chat.set_markdown_visible_rows(visible_rows, cx);
                });
            });
        });

        let selection = transcript_selection(cx.entity().downgrade(), cx);
        let focus_handle = cx.focus_handle();
        let mut subscriptions = vec![
            cx.on_focus_out(&focus_handle, window, |this, _, _, cx| {
                this.selection
                    .update(cx, |selection, cx| selection.clear(cx));
                this.dismiss_highlight(cx);
            }),
            // A press on the view's blank space focuses the view itself; the
            // keyboard goes on to the composer, as it does from the pane. A
            // composer not drawn yet, while the thread loads, is in no pane:
            // the keyboard would be nowhere on screen and the pane pressed
            // would not be entered, so the view keeps it until the composer
            // is drawn.
            cx.on_focus(&focus_handle, window, |this, window, cx| {
                if this.session_id.is_some() && this.composer_drawn {
                    this.focus_composer(window, cx);
                }
            }),
            cx.observe_in(&workspace_store, window, |this, store, window, cx| {
                this.timeline_stale = true;
                // Desktop can type immediately after opening the terminal.
                // On software-keyboard devices this also runs during restore,
                // so wait for a tap on the grid before raising the keyboard.
                let open = store
                    .read(cx)
                    .panel_state_for(this.session_id.as_deref())
                    .terminal_open
                    && this.side_of.is_none();
                if open && !this.terminal_was_open {
                    let drawer = this.terminal_drawer.clone();
                    window.defer(cx, move |window, cx| {
                        if !crate::window_seam::is_mobile(cx) {
                            gpui::Focusable::focus_handle(drawer.read(cx), cx).focus(window, cx);
                        }
                    });
                }
                this.terminal_was_open = open;
                this.request_side_sync(window, cx);
                cx.notify();
            }),
        ];
        let terminal_drawer = cx.new(|cx| {
            let mut drawer =
                TerminalDrawer::new(workspace_store.clone(), session_id.clone(), window, cx);
            drawer.share_scene(&scene);
            drawer
        });
        composer.update(cx, |composer, _| composer.share_scene(&scene));
        subscriptions.push(cx.subscribe(&composer, |this, _, event, cx| match event {
            crate::composer::ComposerEvent::Sent => this.await_sent_message(cx),
        }));
        // The drawer is inside the view: the keyboard entering it does not
        // leave the view, but it does leave the transcript.
        subscriptions.push(cx.on_focus_in(
            &gpui::Focusable::focus_handle(terminal_drawer.read(cx), cx),
            window,
            |this, _, cx| this.dismiss_highlight(cx),
        ));
        // The keyboard rests in the composer while the reader reads; a key
        // pressed anywhere in the view puts it to use. Observed rather than
        // listened for: a key the field binds to an action (delete, enter,
        // paste) never reaches a key listener.
        subscriptions.push(cx.observe_keystrokes(|this, _, window, cx| {
            if this.focus_handle.contains_focused(window, cx) {
                this.dismiss_highlight(cx);
            }
        }));
        // The compact layout shows one thread at a time; turning compact
        // lets the agent beside this one go, and wide again brings it back.
        subscriptions.push(cx.observe_in(&window_state, window, |this, _, window, cx| {
            this.request_side_sync(window, cx)
        }));
        subscriptions.push(cx.on_release(|this, cx| this.release_side(cx)));
        let pull_requests = cx.new(|cx| {
            let mut pull_requests = crate::pull_requests::PullRequestsPanel::new(
                workspace_store.clone(),
                window_state.clone(),
                session_id.clone(),
                cx,
            );
            pull_requests.share_scene(&scene, cx);
            pull_requests
        });
        let agents = cx.new(|cx| {
            crate::agents_panel::AgentsPanel::new(workspace_store.clone(), session_id.clone(), cx)
        });
        let terminal_was_open = workspace_store
            .read(cx)
            .panel_state_for(session_id.as_deref())
            .terminal_open;

        let frame = {
            let chat = cx.entity().downgrade();
            cx.new(|_| ChatFrame { chat, width: None })
        };
        let mut this = Self {
            pull_requests,
            workspace_store,
            session_id,
            edges: WindowEdges::BOTH,
            frame,
            width: px(0.),
            pane_close: None,
            pane_controls: None,
            header_scroll: gpui::ScrollHandle::default(),
            motion: crate::motion::owned::Owned::in_scene(&scene),
            scene_shown: false,
            held: false,
            composer_drawn: false,
            title_recedes: false,
            scene_at: cx.background_executor().now(),
            window_state,
            composer_layout: (compact, px(0.)),
            composer_below_card: composer.read(cx).below_card(),
            composer,
            terminal_drawer,
            agents,
            terminal_was_open,
            terminal_split: cx.new(|_| gpui_base::ResizableState::default()),
            terminal_glide: crate::motion::resize_glide::ResizeGlide::new(
                "chat-terminal-panels",
                gpui::Axis::Vertical,
            )
            .in_scene(&scene),
            terminal_glide_compact: false,
            body: Default::default(),
            side: side::Side::new(),
            side_of: None,
            pane_width: gpui::Pixels::ZERO,
            overhang: gpui::Pixels::ZERO,
            drawer_agent: None,
            list_state,
            timeline_stale: false,
            timeline_revision: None,
            history_placeholder_height: px(0.),
            phone_strip_height: px(0.),
            reservation_scroll_back: None,
            carried_anchor: false,
            rows: Vec::new(),
            rows_motion: crate::motion::list_rows::ListRows::over(ROWS_IN).in_scene(&scene),
            row_folds: Vec::new(),
            shared: crate::motion::shared::SharedMoves::in_scene(&scene),
            side_origins: crate::motion::expand::Origins::in_scene(&scene),
            timeline_folds: 0,
            rows_regrouped: false,
            shown_deliveries: Vec::new(),
            turn_index_cache: TurnIndexCache::default(),
            deciding_plan: None,
            md_states: HashMap::new(),
            selection,
            focus_handle,
            pending_md_builds: HashMap::new(),
            next_md_build_generation: 0,
            markdown_visible_rows: 0..0,
            markdown_scroll_top: None,
            painted_rows: Rc::default(),
            painted_tail_start: None,
            expanded: HashSet::new(),
            screen: Default::default(),
            commands: Default::default(),
            auto_activity_expansions: AutoActivityExpansions::default(),
            command_panels: RefCell::new(CommandPanelCache::new()),
            inline_diffs: InlineDiffCache::new(),
            tool_texts: tools::DerivedTexts::default(),
            full_outputs: HashMap::new(),
            copy_when_loaded: None,
            hover: crate::motion::ListHover::new(crate::material::HOVER).in_scene(&scene),
            list_scroll,
            sent_message: None,
            session_key: None,
            highlighted_turn: None,
            copied: CopiedMark::default(),
            commit_dialog: None,
            capture: None,
            scene,
            _subscriptions: subscriptions,
        };
        this.sync_markdown_states(cx);
        this
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// What draws the view, wherever it is drawn.
    pub(crate) fn frame(&self) -> Entity<ChatFrame> {
        self.frame.clone()
    }

    /// How wide the pane drawing this view is, its right panel's room
    /// included.
    pub(crate) fn set_pane_width(this: &Entity<Self>, width: gpui::Pixels, cx: &mut App) {
        this.update(cx, |chat, cx| {
            chat.pane_width = width;
            cx.notify();
        });
    }

    /// Whether the pane's right panel lies over this view rather than beside
    /// it: expanded, or in a pane too narrow for both.
    pub(crate) fn panel_lies_over(&self, cx: &App) -> bool {
        let panel = self
            .workspace_store
            .read(cx)
            .panel_state_for(self.session_id.as_deref());
        panel.right_panel_open
            && (panel.right_panel_expanded || crate::panes::panel_covers(self.pane_width))
    }

    /// How far past the right edge of a body `width` wide the timeline's list
    /// runs. A list drops every row's height when its width changes, so
    /// while the body keeps the widest column the list keeps the pane's
    /// width, running on under the panel, and only the column's centre moves.
    fn list_overhang(&self, width: gpui::Pixels) -> gpui::Pixels {
        if width >= px(CONTENT_MAX_WIDTH + 2. * CONTENT_MIN_PADDING) {
            (self.pane_width - width).max(px(0.))
        } else {
            px(0.)
        }
    }

    /// The thread the drawer of the view's pane shows.
    pub(crate) fn drawer_subject(&self) -> Option<&str> {
        self.drawer_agent.as_deref().or(self.session_id.as_deref())
    }

    /// The drawer of the view's pane has slid shut: whichever agent it
    /// showed, it shows this thread again.
    pub(crate) fn release_drawer_agent(&mut self) {
        self.drawer_agent = None;
    }

    /// Whether the view's pane is at the window's top right corner.
    pub(crate) fn at_window_corner(&self) -> bool {
        self.edges.right
    }

    pub(crate) fn workspace_store(&self) -> &Entity<WorkspaceStore> {
        &self.workspace_store
    }

    /// The jumped-to turn's mark goes; its colour transition fades it out.
    fn dismiss_highlight(&mut self, cx: &mut Context<Self>) {
        if self.highlighted_turn.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn set_edges(this: &Entity<Self>, edges: WindowEdges, cx: &mut App) {
        this.update(cx, |chat, cx| {
            chat.edges = edges;
            cx.notify();
        });
    }

    /// Whether the header's title recedes, its pane not being the current one
    /// among several.
    pub(crate) fn set_title_recedes(this: &Entity<Self>, recedes: bool, cx: &mut App) {
        this.update(cx, |chat, cx| {
            chat.title_recedes = recedes;
            cx.notify();
        });
    }

    /// What the pane drawing the view hands its header: the close button,
    /// while the pane shares the column, and the pane commands.
    pub(crate) fn set_pane(
        this: &Entity<Self>,
        close: Option<crate::motion::Shown<gpui::WeakEntity<crate::panes::ThreadPanel>>>,
        controls: Option<crate::shell::PaneControls>,
        cx: &mut App,
    ) {
        this.update(cx, |chat, cx| {
            chat.pane_close = close;
            chat.pane_controls = controls;
            cx.notify();
        });
    }

    /// Show `session_id`'s thread, or none; the next sync finds the thread
    /// changed.
    pub(crate) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        if self.session_id == session_id {
            return;
        }
        self.close_drawer_agent(cx);
        self.composer.update(cx, |composer, cx| {
            composer.set_session(session_id.clone(), cx)
        });
        // An agent beside its lead has no terminal drawer of its own.
        if self.side_of.is_none() {
            self.terminal_drawer
                .update(cx, |drawer, cx| drawer.set_session(session_id.clone(), cx));
        }
        self.pull_requests
            .update(cx, |panel, cx| panel.set_session(session_id.clone(), cx));
        self.agents
            .update(cx, |panel, cx| panel.set_session(session_id.clone(), cx));
        self.scene.show(session_id.as_deref());
        self.session_id = session_id;
        self.timeline_stale = true;
        self.composer_drawn = false;
        // Not yet shown, for a host asking before the view next renders.
        self.scene_shown = false;
        self.scene_at = cx.background_executor().now();
        cx.notify();
    }

    /// The view shows another thread from this frame. Its motion is the
    /// new scene's ([`Self::scene`]); what is left is that it shows once it
    /// can be drawn at rest, from rows the new thread painted.
    pub(super) fn start_scene(&mut self, cx: &App) {
        self.scene_shown = false;
        self.scene_at = cx.background_executor().now();
        self.painted_rows.set(None);
    }
}

/// Lays a conversation out at the width it is given this frame, and hands
/// the conversation that width before it is laid out. A view of its own
/// whose render is only the query: gpui-fast lays a container query's content
/// out as a layout root of its own and draws no view around one again from
/// last frame, so a query inside the conversation would build all of it
/// again for any change in it, a keystroke or a terminal's output.
pub(crate) struct ChatFrame {
    chat: gpui::WeakEntity<ChatView>,
    /// The width last handed to the conversation.
    width: Option<gpui::Pixels>,
}

impl Render for ChatFrame {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (frame, chat) = (cx.weak_entity(), self.chat.clone());
        gpui::container_query(move |size, _, cx| {
            let Some(chat) = chat.upgrade() else {
                return div().into_any_element();
            };
            let handed = frame
                .update(cx, |frame, _| frame.width.replace(size.width))
                .ok()
                .flatten();
            if handed != Some(size.width) {
                chat.update(cx, |chat, _| chat.width = size.width);
            }
            chat.into_any_element()
        })
    }
}

/// An opaque backing for a control floating over the timeline: an outline or
/// ghost button's fill is near transparent, and the chat text scrolling
/// beneath would show through it. Scrolling still reaches the timeline.
fn floating_backing(radius: gpui::Pixels, cx: &App) -> gpui::Div {
    div()
        .flex_none()
        .block_mouse_except_scroll()
        .rounded(radius)
        .bg(cx.theme().popover)
        .shadow_md()
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.motion.sweep();
        self.side_origins.begin_frame();
        self.request_side_sync(window, cx);
        let revision = self
            .workspace_store
            .read(cx)
            .timeline_revision_for(self.session_id.as_deref(), cx);
        if revision != self.timeline_revision {
            self.timeline_revision = revision;
            self.timeline_stale = true;
        }
        if self.timeline_stale {
            self.sync_markdown_states(cx);
        }
        self.land_sent_message(cx);
        self.inline_diffs.sweep();
        self.tool_texts.sweep(&cx.theme().highlight_theme);
        let now = cx.background_executor().now();
        for key in self.rows_motion.settle(window, cx) {
            if let Some(row) = self.rows.iter().position(|row| row.identity == key) {
                self.list_state.remeasure_items(row..row + 1);
            }
        }
        let rows_drawn = self.painted_rows.get().is_some();
        self.painted_rows.set(None);
        self.shared
            .begin_frame(self.list_state.viewport_bounds(), cx);
        if self.carried_anchor {
            self.carried_anchor = !settle_carried_anchor(&self.list_state);
            if self.carried_anchor {
                // The next frame may be idle otherwise; make it render.
                let chat = cx.entity().downgrade();
                window.on_next_frame(move |_, cx| {
                    let _ = chat.update(cx, |_, cx| cx.notify());
                });
            }
        }
        self.list_scroll.step(&self.list_state, window, cx);
        // The jump goes as soon as the glide it starts is under way.
        let show_jump_to_latest =
            jump_to_latest_visible(&self.list_state) && !self.list_scroll.running();
        self.sync_markdown_scroll_position(cx);
        self.apply_reservation_scroll_back(window, cx);
        // Measure after this frame's list layout, including the initial tail
        // frame and frames caused by prepends. No scroll event is required.
        // Deferred, it runs once the frame being drawn is done, with no frame
        // of its own: a next-frame callback would wake the display for one
        // more frame after every render. The chat is only read, and the store
        // updated only when the report changes it: an update of either builds
        // the chat again with the next render of the view around it.
        let chat = cx.entity().downgrade();
        window.defer(cx, move |_, cx| {
            let Some(chat) = chat.upgrade() else {
                return;
            };
            let chat = chat.read(cx);
            if chat.capture.is_some() {
                return;
            }
            let Some(screens) = history_screens_covered(&chat.list_state, chat.leading_space())
            else {
                return;
            };
            let following_tail = chat.list_state.is_following_tail();
            let store = chat.workspace_store.clone();
            let session_id = chat.session_id.clone();
            if store.read(cx).history_window_changes_for(
                session_id.as_deref(),
                screens,
                following_tail,
            ) {
                store.update(cx, |store, cx| {
                    store.update_history_window_for(
                        session_id.as_deref(),
                        screens,
                        following_tail,
                        cx,
                    );
                });
            }
        });
        // A screen's height: the window's, which this render already knows.
        // The list's own height is only measured by this frame's layout.
        let placeholder = if self
            .workspace_store
            .read(cx)
            .history_available_for(self.session_id.as_deref())
        {
            window.viewport_size().height
        } else {
            px(0.)
        };
        if placeholder != self.history_placeholder_height && !self.rows.is_empty() {
            let anchor = self.list_state.logical_scroll_top();
            let following = self.list_state.is_following_tail();
            self.list_state.remeasure_items(0..1);
            if anchor.item_ix == 0 && !following {
                self.list_state.scroll_to(ListOffset {
                    item_ix: 0,
                    offset_in_item: (anchor.offset_in_item + placeholder
                        - self.history_placeholder_height)
                        .max(px(0.)),
                });
            }
            self.history_placeholder_height = placeholder;
        }
        let active = self
            .workspace_store
            .read(cx)
            .chat_active_session_for(self.session_id.as_deref(), cx);

        let compact = self.window_state.read(cx).compact;
        // The composer follows the layout in place. Rebuilding it here would
        // throw away the draft, its selection and its pending attachments every
        // time the window crossed the breakpoint.
        if self.composer_layout.0 != compact {
            self.composer_layout.0 = compact;
            self.composer
                .update(cx, |composer, cx| composer.set_compact(compact, cx));
        }
        // The phone reads on T1 paper; the desktop keeps the glass canvas,
        // but for an agent beside its lead, which reads on its card or sheet.
        // The header's title menu dispatches these, and the header is a sibling of `main`.
        let root = v_flex()
            .id(self.scene.id("chat"))
            .size_full()
            .min_w_0()
            .track_focus(&self.focus_handle)
            .when(!self.is_side(), |root| {
                root.bg(if compact {
                    crate::material::content_surface(cx)
                } else {
                    crate::material::canvas(cx)
                })
            })
            .on_action(cx.listener(Self::on_link_dialog))
            .on_action(cx.listener(Self::on_move_thread_dialog))
            .on_action(cx.listener(Self::on_revoke_self_settle))
            .on_action(cx.listener(Self::on_show_agent_tools))
            .on_action(cx.listener(Self::on_toggle_panel));

        // The skeleton, the empty workspace and the thread cross-fade. A
        // thread that leaves cannot be drawn again (the view is bound to what
        // replaced it), so what comes after it fades in over nothing.
        let loading = self
            .workspace_store
            .read(cx)
            .chat_loading_for(self.session_id.as_deref(), cx);
        let surface = match (&active, loading) {
            (_, true) => SURFACE_SKELETON,
            (None, false) => SURFACE_EMPTY,
            (Some(_), false) => SURFACE_THREAD,
        };
        // What the view was bound to shows once it can be drawn at rest;
        // until then nothing does, and the skeleton only once that has
        // taken longer than a glance.
        if !self.scene_shown {
            self.scene_shown = self.scene_ready(loading, rows_drawn, cx);
        }
        if self.held {
            self.motion.forget("chat-surface");
        }
        let surface = if self.scene_shown {
            surface
        } else if now.saturating_duration_since(self.scene_at) >= SCENE_PATIENCE {
            SURFACE_SKELETON
        } else {
            window.request_animation_frame();
            SURFACE_NOTHING
        };
        let fade = self
            .motion
            .layers("chat-surface", surface, 4, GLIDE, window, cx)
            .fill()
            .stage(SURFACE_THREAD)
            .keyboard(self.focus_handle.clone());
        let mut layers = Vec::new();
        if fade.shows(SURFACE_SKELETON) {
            layers.push((
                SURFACE_SKELETON,
                v_flex()
                    .size_full()
                    .when_some(
                        self.workspace_store
                            .read(cx)
                            .history_error_for(self.session_id.as_deref())
                            .map(str::to_owned),
                        |container, error| container.child(div().px_4().child(error)),
                    )
                    .child(crate::material::loading_skeleton(cx))
                    .into_any_element(),
            ));
        }
        if fade.shows(SURFACE_EMPTY) {
            layers.push((
                SURFACE_EMPTY,
                v_flex()
                    .size_full()
                    .when(!compact && !self.is_side(), |el| {
                        el.child(self.render_header(None, false, None, None, window, cx))
                    })
                    .child(self.render_empty_state(window, cx))
                    .into_any_element(),
            ));
        }
        let composer_drawn = active.is_some() && !loading;
        if composer_drawn && !self.composer_drawn && self.focus_handle.is_focused(window) {
            // The keyboard the view kept while the thread loaded.
            cx.defer_in(window, |this, window, cx| this.focus_composer(window, cx));
        }
        self.composer_drawn = composer_drawn;
        if let Some(active) = active.filter(|_| !loading) {
            layers.push((
                SURFACE_THREAD,
                self.render_thread(active, show_jump_to_latest, window, cx),
            ));
        }
        // An agent beside its lead keeps its header, and its way back, while
        // its thread loads.
        let side_header = self.render_side_header(window, cx);
        root.children(side_header)
            .child(fade.render(layers).w_full().flex_1().min_h_0())
    }
}

/// The layers of a conversation's surface: its skeleton while it loads, the
/// empty workspace with no thread, the thread, and nothing, which is what a
/// thread just bound shows while it readies itself unseen.
const SURFACE_SKELETON: usize = 0;
const SURFACE_EMPTY: usize = 1;
const SURFACE_THREAD: usize = 2;
const SURFACE_NOTHING: usize = 3;

/// How long a thread just bound may take to be ready before its skeleton
/// shows: under it, it fades in from nothing.
const SCENE_PATIENCE: Duration = Duration::from_millis(300);

impl ChatView {
    /// Whether a host fading the view in should start now: what it was
    /// bound to can be drawn at rest, or has kept the reader waiting long
    /// enough that its skeleton shows, which the thread then fades in over.
    pub(crate) fn appears(&self, cx: &App) -> bool {
        self.scene_shown
            || cx
                .background_executor()
                .now()
                .saturating_duration_since(self.scene_at)
                >= SCENE_PATIENCE
    }

    /// Whether a host holds the view unseen, to fade it in itself.
    pub(crate) fn set_held(this: &Entity<Self>, held: bool, cx: &mut App) {
        if this.read(cx).held != held {
            this.update(cx, |chat, cx| {
                chat.held = held;
                cx.notify();
            });
        }
    }

    /// Whether what the view was bound to can be drawn at rest: no thread
    /// once the workspace is known; a thread once its timeline is held whole
    /// (not loading, not catching up), its rows have been drawn (measured, at
    /// their anchor) and the Markdown of the ones on screen is built, its
    /// diagrams drawn. Reading the Markdown here draws the view again when a
    /// diagram is.
    fn scene_ready(&self, loading: bool, rows_drawn: bool, cx: &App) -> bool {
        if loading {
            return false;
        }
        let Some(session_id) = self.session_id.as_deref() else {
            return true;
        };
        self.workspace_store
            .read(cx)
            .timeline_settled_for(session_id)
            && !self.timeline_stale
            && (self.rows.is_empty() || rows_drawn)
            && self.pending_md_builds.is_empty()
            && !self
                .md_states
                .values()
                .any(|md| md.state.read(cx).drawing_first_diagrams())
            && !self.carried_anchor
            && self.reservation_scroll_back.is_none()
    }
}

impl ChatView {
    /// The open thread: its header over the timeline, its deliveries and its
    /// composer, with the terminal drawer below when it is open.
    fn render_thread(
        &mut self,
        (title, cwd, is_draft): (String, std::path::PathBuf, bool),
        show_jump_to_latest: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let title = if is_draft { None } else { Some(title) };
        let agents = {
            let store = self.workspace_store.read(cx);
            self.session_id
                .as_deref()
                .map(|id| crate::agents_panel::Agents::of(store, id, cx))
                .filter(|agents| !agents.is_empty())
        };
        let header = self.render_header(
            title,
            is_draft,
            Some(cwd.clone()),
            agents.as_ref(),
            window,
            cx,
        );
        // The body is as wide as the view: the rail's room moves the rows and
        // the composer.
        let body = self.render_body(
            self.width,
            (&cwd, is_draft, agents),
            show_jump_to_latest,
            window,
            cx,
        );
        v_flex()
            .size_full()
            .when(!compact && !self.is_side(), |el| el.child(header))
            // The body is as tall as what holds it; under the header, that
            // is what the header leaves.
            .child(div().flex_1().min_h_0().w_full().child(body))
            .into_any_element()
    }

    /// The thread under its header, `width` wide: its timeline, its
    /// deliveries and its composer, with the side rail (or the agent's card)
    /// right of their column, and the terminal drawer below when it is open.
    fn render_body(
        &mut self,
        width: gpui::Pixels,
        (cwd, is_draft, agents): (&Path, bool, Option<crate::agents_panel::Agents>),
        show_jump_to_latest: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let rail_cards = self.rail_cards(cx);
        let layout =
            rail::BodyLayout::of(width, !rail_cards.is_empty(), self.side_share(cx), compact);
        // gpui-base has no transition that moves a laid-out column to a new
        // centre, so the view glides the column's offset itself: its shift
        // follows the presence of the rail and of the agent's card, which
        // glide, and the widths, which do not, so a resize or a drag of the
        // card's edge is tracked at once.
        let presence = self.motion.transition(
            "chat-rail",
            if layout.rail() { 1f32 } else { 0. },
            SHOW,
            window,
            cx,
        );
        let side_presence = self.motion.transition(
            "chat-side",
            if layout.side() { 1f32 } else { 0. },
            SHOW,
            window,
            cx,
        );
        self.body = layout.at(presence, side_presence);
        self.overhang = self.list_overhang(width);
        let overhang = self.overhang;
        let shift = self.body.shift();
        if self.composer_layout.1 != shift {
            self.composer_layout.1 = shift;
            self.composer
                .update(cx, |composer, cx| composer.set_column_shift(shift, cx));
        }
        let rail = self.render_rail(rail_cards, window, cx);
        let mut side = self.render_side(window, cx);
        let track_inset = self.track_inset();
        let phone_strip = self.render_phone_strip(agents.as_ref(), window, cx);
        let phone_strip_height = phone_strip.as_ref().map_or(px(0.), |(_, progress)| {
            px(crate::material::bar_height(
                crate::material::Bar::Toolbar,
                true,
            )) * *progress
        });
        let phone_strip = phone_strip.map(|(strip, _)| strip);
        if phone_strip_height != self.phone_strip_height {
            if let Some(last) = self.rows.len().checked_sub(1) {
                self.list_state.remeasure_items(last..last + 1);
            }
            self.phone_strip_height = phone_strip_height;
        }
        let panel = self
            .workspace_store
            .read(cx)
            .panel_state_for(self.session_id.as_deref());
        let terminal_open =
            panel.terminal_open && !self.window_state.read(cx).compact && !self.is_side();
        let terminal_height = panel.terminal_height;

        let item_count = self.rows.len();
        let timeline = self.render_timeline_list(cwd, cx);

        // A fresh phone draft shows its project/model context above the focused composer.
        let timeline: AnyElement = if compact && is_draft && item_count == 0 {
            self.render_compact_draft_empty(cwd, cx)
        } else {
            // GPUI's scrollbar extent excludes List padding, while wheel and
            // tail-resume calculations include it. Keep the inset outside the
            // list so every input path shares the same bottom and pixel offset.
            v_flex()
                .when(self.is_side(), |timeline| {
                    timeline.debug_selector(|| "chat-side-timeline".into())
                })
                .flex_1()
                .min_h_0()
                .relative()
                .when(overhang > px(0.), |timeline| timeline.w(width + overhang))
                .pb(timeline_bottom_inset(cx))
                .child(
                    self.motion
                        .keep::<crate::motion::bounce::BounceState>("timeline-bounce")
                        .render(
                            "timeline-bounce",
                            SelectionArea::new(&self.selection, timeline)
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_h_0(),
                            |content| {
                                crate::scroll::page_viewport(
                                    "timeline-bounce",
                                    &self.list_state,
                                    content,
                                )
                            },
                        ),
                )
                .when(!window.is_inspector_picking(cx), |timeline| {
                    let side = self.is_side();
                    timeline.child(
                        div()
                            .debug_selector(move || {
                                if side {
                                    "chat-side-track"
                                } else {
                                    "chat-track"
                                }
                                .into()
                            })
                            .absolute()
                            .top_0()
                            .bottom(timeline_bottom_inset(cx))
                            .right(track_inset + overhang)
                            .w(TRACK)
                            .child(
                                Scrollbar::vertical(&self.list_state)
                                    .id("timeline-scrollbar")
                                    .motion(&self.motion)
                                    .viewport_from_layout(),
                            ),
                    )
                })
                .into_any_element()
        };

        let composer = self.composer.clone().into_any_element();
        let deliveries = self
            .workspace_store
            .read(cx)
            .delivery_messages_for(self.session_id.as_deref(), cx);
        self.shown_deliveries = deliveries.iter().map(|(key, ..)| key.clone()).collect();
        let waiting = !self
            .workspace_store
            .read(cx)
            .connection_state()
            .is_connected();
        let mut delivery_rows = Vec::new();
        for (key, text, failure, acknowledged) in deliveries {
            let retry_key = key.clone();
            let discard_key = key.clone();
            let retry_store = self.workspace_store.clone();
            let discard_store = self.workspace_store.clone();
            // The text brightens once the host has it; the line under it
            // says how the send stands, and folds away once it is through.
            let motion = self.motion.scope(format!("delivery-{key}"));
            let text_color = motion.transition(
                "text",
                if acknowledged {
                    cx.theme().foreground
                } else {
                    cx.theme().muted_foreground
                },
                GLIDE,
                window,
                cx,
            );
            let status = (failure.is_some() || !acknowledged).then(|| {
                let line = failure.clone().unwrap_or_else(|| {
                    crate::tr!(if waiting {
                        "chat.waiting_connection"
                    } else {
                        "chat.sending"
                    })
                    .into_owned()
                });
                (line, failure.is_some())
            });
            let status = motion
                .shown("status", status, SHOW, window, cx)
                .map(|status| {
                    let (danger, muted) = (cx.theme().danger, cx.theme().muted_foreground);
                    let line = motion.cross_fade(
                        "status-line",
                        status.value.clone(),
                        CHANGE,
                        window,
                        cx,
                        move |(line, failed), _, cx| {
                            div()
                                .text_secondary(false, cx)
                                .text_color(if *failed { danger } else { muted })
                                .child(line.clone())
                                .into_any_element()
                        },
                    );
                    status.fold(h_flex().pt_1().justify_end().child(line))
                });
            let actions = motion
                .shown("actions", failure.is_some().then_some(()), SHOW, window, cx)
                .map(|actions| {
                    actions.fold(
                        h_flex()
                            .pt_1()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new(SharedString::from(format!("retry-{key}")))
                                    .motion(&motion)
                                    .ghost()
                                    .small()
                                    .debug_selector(|| "retry-delivery".into())
                                    .label(crate::tr!("chat.retry_delivery"))
                                    .on_click(move |_, _, cx| {
                                        retry_store
                                            .update(cx, |store, _| store.retry_delivery(&retry_key))
                                    }),
                            )
                            .child(
                                Button::new(SharedString::from(format!("discard-{key}")))
                                    .motion(&motion)
                                    .ghost()
                                    .small()
                                    .debug_selector(|| "discard-delivery".into())
                                    .label(crate::tr!("chat.discard_delivery"))
                                    .on_click(move |_, _, cx| {
                                        discard_store.update(cx, |store, _| {
                                            store.discard_delivery(&discard_key)
                                        })
                                    }),
                            ),
                    )
                });
            delivery_rows.push(
                v_flex()
                    .id(SharedString::from(format!("delivery-{key}")))
                    .debug_selector(|| "pending-delivery-bubble".into())
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .items_end()
                    .child(
                        div()
                            .max_w_3_4()
                            .px(cx.theme().tokens.spacing.md)
                            .py(cx.theme().tokens.spacing.xs)
                            .text_token(&cx.theme().tokens.typography.md)
                            .rounded(crate::material::radius_composer(cx))
                            .bg(cx.theme().foreground.opacity(0.08))
                            .text_color(text_color)
                            .child(text),
                    )
                    .children(status)
                    .children(actions),
            );
        }

        let delivery_inset = gpui::rems(1.).to_pixels(window.rem_size());
        let slots = rail::Slots {
            below_card: self.composer_below_card.clone(),
            slots: rail.into_iter().chain(side.card.take()).collect(),
        };
        let main = v_flex()
            .relative()
            .size_full()
            .min_h_0()
            .on_action(cx.listener(Self::on_link_url))
            .on_action(cx.listener(Self::on_rewind_turn))
            .on_action(cx.listener(Self::on_copy_whole_output))
            .on_action(cx.listener(Self::on_open_file_diff))
            .child(
                div()
                    .id("timeline")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(timeline)
                    .child(
                        gpui::canvas(
                            {
                                let list = self.list_state.clone();
                                let glide = self.list_scroll.clone();
                                let view = cx.entity().downgrade();
                                let painted = self.painted_rows.clone();
                                move |_, window, cx| {
                                    // List prepaint may change geometry after render
                                    // (resize, splice, or markdown remeasurement).
                                    // Reconcile the sibling control after that layout.
                                    if (jump_to_latest_visible(&list) && !glide.running())
                                        != show_jump_to_latest
                                    {
                                        let view = view.clone();
                                        window.defer(cx, move |_, cx| {
                                            let _ = view.update(cx, |_, cx| cx.notify());
                                        });
                                    }
                                    // Only update the chat when the frame
                                    // changed something: an update builds it
                                    // again with the next render of the view
                                    // around it.
                                    let painted = painted.get();
                                    window.defer(cx, move |_, cx| {
                                        let Some(view) = view.upgrade() else {
                                            return;
                                        };
                                        let chat = view.read(cx);
                                        let panels = chat.command_panels.borrow_mut().settle();
                                        let painted = painted.and_then(|(start, end)| {
                                            chat.painted_rows_change(start..end)
                                        });
                                        if panels.is_empty() && painted.is_none() {
                                            return;
                                        }
                                        view.update(cx, |chat, cx| {
                                            chat.apply_command_panel_changes(panels, cx);
                                            if let Some(painted) = painted {
                                                chat.adopt_painted_rows(painted, cx);
                                            }
                                        });
                                    });
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(self.render_scroll_pill(show_jump_to_latest, window, cx))
                    .children(phone_strip),
            )
            .child(
                v_flex()
                    .w_full()
                    .max_h(px(200.))
                    .id("pending-deliveries")
                    .items_center()
                    .overflow_y_scroll()
                    .px(delivery_inset)
                    .pr(delivery_inset + shift)
                    .gap_2()
                    .children(delivery_rows),
            )
            .child(
                div()
                    .id("chat-composer")
                    .debug_selector(|| "chat-composer".into())
                    .w_full()
                    .flex_none()
                    // A press on the composer puts the keyboard resting
                    // there to use. Capture, so the composer's own handling
                    // still runs and cannot hide it.
                    .capture_any_mouse_down(
                        cx.listener(|this, _, _, cx| this.dismiss_highlight(cx)),
                    )
                    .child(composer),
            )
            .child(slots);
        let main = self.cover_with_side(main.into_any_element(), side);

        // Opening and closing the drawer glide; entering or leaving the
        // compact layout, which has no drawer, does not.
        let compact = self.window_state.read(cx).compact;
        let animate = compact == self.terminal_glide_compact;
        self.terminal_glide_compact = compact;
        let glide = self.terminal_glide.sample(
            &[
                crate::motion::resize_glide::Panel::flex(true),
                crate::motion::resize_glide::Panel::sized(px(terminal_height), terminal_open),
            ],
            animate,
            &self.terminal_split,
            window,
            cx,
        );
        let drawer = glide.mounted(1).then(|| {
            let drawer = self.terminal_drawer.clone();
            let width = f32::from(window.bounds().size.width);
            if !drawer.read(cx).is_size(width, terminal_height) {
                drawer.update(cx, |drawer, cx| drawer.resize(width, terminal_height, cx));
            }
            // `fixed`: the drawer keeps its height as the window's changes,
            // as it did before the glide gave it one in the group's state.
            gpui_base::resizable_panel()
                .flex_none()
                .fixed()
                .size(px(terminal_height))
                .size_range(glide.range(1, px(120.)..px(600.)))
                .child(glide.hold(1, drawer))
        });
        let drawer_resize = self.terminal_drawer.clone();
        let width = f32::from(window.bounds().size.width);
        // One group whether or not the drawer is open, so the conversation
        // keeps its place in the tree as the drawer comes and goes.
        gpui_base::v_resizable("chat-terminal-panels")
            .with_state(&self.terminal_split)
            .on_resize(move |state, _, cx| {
                let height = state.read(cx).sizes().get(1).copied();
                if let Some(height) = height {
                    drawer_resize
                        .update(cx, |drawer, cx| drawer.resize(width, f32::from(height), cx));
                }
            })
            .child(
                gpui_base::resizable_panel()
                    .size_range(glide.range(0, gpui_base::PANEL_MIN_SIZE..gpui::Pixels::MAX))
                    .child(glide.hold(0, main)),
            )
            // Not `children`: the group's replaces the panels added before.
            .when_some(drawer, |group, drawer| group.child(drawer))
            .into_any_element()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::ChatView;
    use crate::store::WorkspaceStore;
    use crate::test_frames::draw_settled;
    use crate::window_state::WindowState;
    use agent::{ItemContent, ItemStatus};
    use gpui::{AppContext as _, Entity, TestAppContext};
    use std::sync::{Arc, atomic::AtomicU64};
    use tcode_core::session::{EntryContent, Timeline, TimelineEntry, TurnMeta};

    pub(in crate::chat) static NEXT_RESIDENCY_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[gpui::test]
    fn compact_composer_keeps_effort_and_settings_visible(cx: &mut TestAppContext) {
        use gpui::px;
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let mut timeline = synthetic_markdown_timeline(1);
        timeline.usage = Some(agent::TokenUsage {
            used_tokens: Some(100_000),
            context_window: Some(200_000),
            ..Default::default()
        });
        // Seed a catalog-backed model: the generic chat fixture has no model
        // options and therefore cannot detect a missing effort control.
        cx.update(crate::theme::init);
        let data_root = crate::store::tests::scratch_root("effort-layout");
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(data_root).unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let (session_id, timeline) = smol::block_on(host.update_state_for_test(|state, cx| {
            state.providers.model_catalogs.insert(
                agent::ProviderKind::Codex,
                vec![agent::ModelSpec {
                    id: "effort-layout".into(),
                    display_name: "A model with a long display name".into(),
                    is_default: true,
                    options: vec![agent::OptionDescriptor::Select {
                        id: "reasoningEffort".into(),
                        label: "Reasoning effort".into(),
                        options: vec![agent::SelectOption {
                            value: "xhigh".into(),
                            label: "Extra High".into(),
                            description: None,
                            unavailable: None,
                        }],
                        default_value: Some("xhigh".into()),
                        role: Default::default(),
                        apply: Default::default(),
                        recommended: None,
                        permissive: None,
                    }],
                }],
            );
            let id = state.start_draft("effort-layout".into(), std::env::temp_dir(), cx);
            let active = state.residents.live.get_mut(&id).unwrap();
            active.meta.provider = agent::ProviderKind::Codex;
            active.meta.model = Some("effort-layout".into());
            active.timeline = timeline;
            (id, active.timeline.clone())
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready()
                    && store
                        .provider_model_catalog(agent::ProviderKind::Codex)
                        .iter()
                        .any(|model| model.id == "effort-layout")
            })
        });
        store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, &store, "selected conversation", |cx| {
            store.read_with(cx, |store, cx| {
                store.baseline_ready()
                    && !store.chat_loading_for(store.sole_thread().as_deref(), cx)
            })
        });
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), timeline, cx);
        });
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        let (view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        for locale in ["en", "zh-CN"] {
            crate::set_locale(locale);
            view.update(cx, |_, cx| cx.notify());
            for (width, height) in [(360., 780.), (393., 852.)] {
                cx.simulate_resize(gpui::size(px(width), px(height)));
                draw_settled(cx);
                let permission = cx.debug_bounds("permission-chip").expect("approval option");
                let effort = cx
                    .debug_bounds("traits-chip")
                    .expect("standalone effort option");
                let meter = cx
                    .debug_bounds("context-meter")
                    .expect("compact context meter");
                let model = cx.debug_bounds("model-picker").expect("model picker");
                let send = cx.debug_bounds("send-message").expect("send button");
                let card = cx.debug_bounds("composer-card").expect("input card");
                let drawer = cx
                    .debug_bounds("composer-settings-drawer")
                    .expect("attached settings drawer");
                assert_eq!(
                    send.top(),
                    model.top(),
                    "Send shares the model row at {width}"
                );
                assert_eq!(permission.top(), meter.top());
                assert!(permission.right() <= meter.left());
                assert_eq!(effort.top(), model.top(), "Effort sits on the model row");
                assert!(permission.left() >= px(0.) && meter.right() <= px(width));
                assert!(
                    model.right() <= effort.left() && effort.right() <= send.left(),
                    "controls overlap at {width}: model={model:?}, effort={effort:?}, send={send:?}"
                );
                assert!(send.right() <= card.right());
                assert!(effort.size.width >= px(44.) && effort.size.height >= px(44.));
                assert!(
                    drawer.top() >= card.bottom(),
                    "settings sit below the input card"
                );
                assert!(permission.left() >= drawer.left() && meter.right() <= drawer.right());
                assert!(meter.top() >= drawer.top() && meter.bottom() <= drawer.bottom());
                assert!(meter.size.width >= px(44.) && meter.size.height >= px(44.));
            }
        }
        driver.shutdown(cx, &store, &host);
    }

    /// On a phone the strip over the foot of the timeline rises in with the
    /// thread's first agent and sinks out with its last, over several
    /// frames, and the last row's room below grows to clear it and shrinks
    /// back in step; with motion reduced both happen at once.
    #[gpui::test]
    fn the_phone_strip_rises_in_and_out_with_the_tail_clearing_it(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px};
        use std::time::Duration;

        let plain = synthetic_markdown_timeline(3);
        let mut with_agent = plain.clone();
        // In the first turn, away from the tail it is measured at.
        with_agent.entries.insert(
            1,
            at_turn(
                entry(
                    "agent-1",
                    EntryContent::Item(ItemContent::Subagent {
                        agent_type: "Explore".into(),
                        description: "Survey the code".into(),
                        status: ItemStatus::InProgress,
                        summary: None,
                        model: None,
                        effort: None,
                    }),
                ),
                0,
            ),
        );
        let (store, window_state, session_id) = seed_chat(cx, plain.clone());
        window_state.update(cx, |state, _| state.compact = true);
        let (view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        let replace = |timeline: &Timeline, cx: &mut VisualTestContext| {
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx)
            })
        };
        replace(&plain, cx);
        draw_settled(cx);
        assert!(cx.debug_bounds("phone-strip").is_none());
        // Where the strip stands on a frame, and how tall the last row is.
        let stand = |cx: &mut VisualTestContext| {
            let last = view.read_with(cx, |chat, _| chat.rows.len() - 1);
            (
                cx.debug_bounds("phone-strip").map(|strip| strip.top()),
                cx.debug_bounds(format!("timeline-row-{last}").leak())
                    .expect("the last row")
                    .size
                    .height,
            )
        };
        let one_way = |seen: &[gpui::Pixels], up: bool| {
            seen.windows(2).all(|pair| {
                if up {
                    pair[1] <= pair[0]
                } else {
                    pair[1] >= pair[0]
                }
            })
        };
        for reduce in [false, true] {
            cx.update(|_, cx| cx.set_reduce_motion(reduce));
            for (timeline, arrives) in [(&with_agent, true), (&plain, false)] {
                let (_, rest) = stand(cx);
                replace(timeline, cx);
                crate::test_frames::draw(cx);
                let mut frames = vec![stand(cx)];
                for _ in 0..20 {
                    cx.executor().advance_clock(Duration::from_millis(16));
                    cx.update(|window, cx| window.simulate_next_frame(cx));
                    crate::test_frames::draw(cx);
                    frames.push(stand(cx));
                }
                draw_settled(cx);
                let (strip, tail) = stand(cx);
                assert_eq!(strip.is_some(), arrives);
                let tails: Vec<_> = std::iter::once(rest)
                    .chain(frames.iter().map(|(_, tail)| *tail))
                    .collect();
                assert!(
                    one_way(&tails, !arrives),
                    "the tail stepped back: {tails:?}"
                );
                // The room it gains or gives back is the strip's height.
                let strip_height = px(crate::material::bar_height(
                    crate::material::Bar::Toolbar,
                    true,
                ));
                assert_eq!((tail - rest).abs(), strip_height, "{tails:?}");
                let moving = tails
                    .iter()
                    .filter(|at| **at != rest && **at != tail)
                    .count();
                let strips: Vec<_> = frames.iter().filter_map(|(strip, _)| *strip).collect();
                if reduce {
                    assert_eq!(tails[1], tail, "the tail glided: {tails:?}");
                    assert_eq!(strips.is_empty(), !arrives, "{strips:?}");
                } else {
                    assert!(moving >= 3, "the tail jumped: {tails:?}");
                    // Rising in it moves up to its place; sinking out, down.
                    assert!(one_way(&strips, arrives), "{strips:?}");
                    assert!(strips.len() >= 3, "the strip popped: {strips:?}");
                }
            }
        }
    }

    #[gpui::test]
    fn compact_running_status_clears_composer_and_keyboard(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px};
        struct OccludedChat {
            chat: Entity<ChatView>,
            bottom: gpui::Pixels,
        }
        impl Render for OccludedChat {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                // Reserve the system-occluded area, as the shell's window seam does.
                div()
                    .size_full()
                    .pb(self.bottom)
                    .child(self.chat.read(cx).frame())
            }
        }
        let mut timeline = synthetic_markdown_timeline(30);
        Arc::make_mut(timeline.entries.last_mut().unwrap()).content =
            assistant(&"A long running reply keeps its final status visible.\n\n".repeat(40));
        timeline.turn_running = true;
        timeline.turns.last_mut().unwrap().running = true;
        let (store, window_state, session_id) = seed_chat(cx, timeline);
        cx.update(|cx| cx.set_reduce_motion(true));
        window_state.update(cx, |state, _| state.compact = true);
        let (root, cx) = cx.add_window_view(|window, cx| OccludedChat {
            chat: cx.new(|cx| ChatView::new(store, window_state, Some(session_id), window, cx)),
            bottom: px(34.),
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        let list = root.read_with(cx, |root, cx| root.chat.read(cx).list_state.clone());
        for bottom in [34., 320., 34.] {
            root.update(cx, |root, cx| {
                root.bottom = px(bottom);
                cx.notify();
            });
            for following in [true, false] {
                list.set_follow_mode(if following {
                    gpui::FollowMode::Tail
                } else {
                    gpui::FollowMode::Normal
                });
                list.scroll_to_end();
                cx.run_until_parked();
                draw_settled(cx);
                let status = cx
                    .debug_bounds("working-status-29")
                    .expect("running status after scroll_to_end");
                let composer = cx.debug_bounds("chat-composer").expect("composer");
                assert!(status.bottom() <= composer.top());
                assert!(status.bottom() <= list.viewport_bounds().bottom());
                cx.simulate_event(gpui::ScrollEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(1200.))),
                    touch_phase: gpui::TouchPhase::Started,
                    ..Default::default()
                });
                draw_settled(cx);
                cx.simulate_event(gpui::ScrollEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-100000.))),
                    touch_phase: gpui::TouchPhase::Moved,
                    ..Default::default()
                });
                draw_settled(cx);
                let status = cx
                    .debug_bounds("working-status-29")
                    .expect("running status row");
                let composer = cx.debug_bounds("chat-composer").expect("composer");
                let meter = cx
                    .debug_bounds("context-meter")
                    .expect("second composer row");
                let model = cx.debug_bounds("model-picker").expect("first composer row");
                assert!(meter.top() >= model.bottom());
                assert!(
                    status.bottom() <= composer.top(),
                    "status {status:?} overlaps composer {composer:?}, inset {bottom}, following {following}"
                );
                assert!(
                    status.bottom() <= list.viewport_bounds().bottom(),
                    "status is clipped by timeline"
                );
                assert!(status.top() >= list.viewport_bounds().top());
                assert!(composer.bottom() <= px(852. - bottom));
            }
        }
    }

    /// A thread with nothing new to show is drawn again from what it drew
    /// when the view around it renders again, as the shell does for its own
    /// reasons, and asks for no frame of its own.
    #[gpui::test]
    fn an_idle_chat_is_reused_when_the_view_around_it_renders(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, ParentElement as _, Render, Styled as _, VisualTestContext,
            Window, div, px, size,
        };
        struct Around(Entity<ChatView>);
        impl Render for Around {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(self.0.read(cx).frame())
            }
        }
        let (store, window_state, session_id) = seed_chat(cx, synthetic_markdown_timeline(6));
        let (around, cx) =
            cx.add_window_view(|window, cx| {
                Around(cx.new(|cx| {
                    ChatView::new(store, window_state, Some(session_id.clone()), window, cx)
                }))
            });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(900.)));
        draw_settled(cx);

        for _ in 0..2 {
            cx.update(|window, _| window.reset_layout_stats());
            around.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            assert_eq!(
                cx.update(|window, _| window.layout_stats().views_built),
                1,
                "the chat was built again with the view around it"
            );
            assert_eq!(
                cx.update(|window, cx| window.simulate_next_frame(cx)),
                0,
                "the idle chat asked for another frame"
            );
        }
    }

    /// A window drawing the conversation `build` makes as the panes draw
    /// one: through its frame.
    pub(crate) fn add_chat_window(
        cx: &mut TestAppContext,
        build: impl FnOnce(&mut gpui::Window, &mut gpui::Context<ChatView>) -> ChatView,
    ) -> (Entity<ChatView>, &mut gpui::VisualTestContext) {
        struct Framed {
            chat: Entity<ChatView>,
            frame: Entity<super::ChatFrame>,
        }
        impl gpui::Render for Framed {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                self.frame.clone()
            }
        }
        let (root, cx) = cx.add_window_view(|window, cx| {
            let chat = cx.new(|cx| build(window, cx));
            let frame = chat.read(cx).frame();
            Framed { chat, frame }
        });
        let chat = root.read_with(cx, |root, _| root.chat.clone());
        (chat, cx)
    }

    pub(crate) fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
        Arc::new(TimelineEntry {
            id: id.to_string(),
            content,
            ts: None,
            finished_ts: None,
            background: false,
            turn: 0,
        })
    }

    pub(crate) fn user_item(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::UserMessage {
            text: text.into(),
            context_len: None,
            attachments: Vec::new(),
        })
    }

    pub(crate) fn assistant(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        })
    }

    pub(in crate::chat) fn reasoning(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::Reasoning { text: text.into() })
    }

    pub(in crate::chat) fn command(id: &str) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::Item(ItemContent::CommandExecution {
                command: id.to_string(),
                output: String::new(),
                exit_code: Some(0),
                status: agent::ItemStatus::Completed,
            }),
        )
    }

    pub(crate) fn command_with_output(id: &str) -> Arc<TimelineEntry> {
        entry(
            id,
            EntryContent::Item(ItemContent::CommandExecution {
                command: format!("run {id}"),
                output: format!("output of {id}\n"),
                exit_code: Some(0),
                status: ItemStatus::Completed,
            }),
        )
    }

    /// The output of the command `id`'s open detail, painted in the last
    /// frame.
    pub(in crate::chat) fn command_open(
        cx: &mut gpui::VisualTestContext,
        id: &str,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        cx.debug_bounds(format!("command-output-{id}").leak())
    }

    pub(in crate::chat) fn at_turn(
        mut entry: Arc<TimelineEntry>,
        turn: usize,
    ) -> Arc<TimelineEntry> {
        Arc::make_mut(&mut entry).turn = turn;
        entry
    }

    pub(in crate::chat) fn synthetic_markdown_timeline(turn_count: usize) -> Timeline {
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default(); turn_count];
        for turn in 0..turn_count {
            timeline.entries.extend([
                at_turn(
                    entry(&format!("user-{turn}"), user_item(&format!("User {turn}"))),
                    turn,
                ),
                at_turn(
                    entry(
                        &format!("reasoning-{turn}"),
                        reasoning(&format!("Reasoning {turn}")),
                    ),
                    turn,
                ),
                at_turn(
                    entry(
                        &format!("assistant-{turn}"),
                        assistant(&format!("Assistant {turn}")),
                    ),
                    turn,
                ),
            ]);
        }
        timeline
    }

    pub(in crate::chat) fn single_assistant_timeline(id: &str, text: &str) -> Timeline {
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default()];
        timeline.entries.push(entry(id, assistant(text)));
        timeline
    }

    pub(in crate::chat) fn seed_chat(
        cx: &mut TestAppContext,
        timeline: Timeline,
    ) -> (Entity<WorkspaceStore>, Entity<WindowState>, String) {
        let (store, window_state, session_id, _) = seed_chat_with_history(cx, timeline, false);
        (store, window_state, session_id)
    }

    pub(in crate::chat) fn seeded_chat_host(
        cx: &mut TestAppContext,
        timeline: Timeline,
        paged: bool,
    ) -> (tcode_runtime::pipe::SpawnedHost, String, Timeline) {
        seeded_chat_host_on(cx, timeline, paged, None)
    }

    /// A host holding one thread with `timeline`, on `provider` when given.
    pub(in crate::chat) fn seeded_chat_host_on(
        cx: &mut TestAppContext,
        timeline: Timeline,
        paged: bool,
        provider: Option<agent::ProviderKind>,
    ) -> (tcode_runtime::pipe::SpawnedHost, String, Timeline) {
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let data_root = crate::store::tests::scratch_root("chat-residency-test");
        let store = SessionStore::open_at(data_root).expect("test session store");
        let host = spawn_host(store, HostServices::default()).expect("spawn test host");
        let (session_id, timeline) =
            smol::block_on(host.update_state_for_test(move |state, cx| {
                let id =
                    state.start_draft("markdown-residency-test".into(), std::env::temp_dir(), cx);
                if paged {
                    for ts in 0..2000 {
                        state.record_event_for_replica_test(
                            &id,
                            ts,
                            &agent::AgentEvent::Warning {
                                message: "short".into(),
                            },
                            cx,
                        );
                    }
                }
                let active = state.residents.live.get_mut(&id).expect("selected draft");
                active.timeline = timeline;
                active.draft = false;
                if let Some(provider) = provider {
                    active.meta.provider = provider;
                }
                let result = (active.meta.id.clone(), active.timeline.clone());
                let meta = active.meta.clone();
                state.sessions.push(meta);
                result
            }))
            .expect("seed markdown host");
        (host, session_id, timeline)
    }

    /// Two threads on one host, the first selected and holding `first`.
    pub(in crate::chat) struct TwoThreads {
        pub(in crate::chat) store: Entity<WorkspaceStore>,
        pub(in crate::chat) driver: crate::store::tests::LocalHostDriver,
        pub(in crate::chat) host: tcode_runtime::pipe::SpawnedHost,
        pub(in crate::chat) first: String,
        pub(in crate::chat) second: String,
    }

    pub(in crate::chat) fn seed_two_threads(
        cx: &mut TestAppContext,
        first: Timeline,
    ) -> TwoThreads {
        let (host, first_id, timeline) = seeded_chat_host(cx, first, false);
        let second = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("second".into(), std::env::temp_dir(), cx);
            let active = state.residents.live.get_mut(&id).expect("second draft");
            active.draft = false;
            let meta = active.meta.clone();
            state.sessions.push(meta);
            id
        }))
        .expect("seed the second thread");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, cx| store.show_alone(first_id.clone(), cx));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, cx| {
                !store.chat_loading_for(store.sole_thread().as_deref(), cx)
            })
        });
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(first_id.clone(), timeline, cx);
        });
        TwoThreads {
            store,
            driver,
            host,
            first: first_id,
            second,
        }
    }

    impl TwoThreads {
        /// Binds `chat`'s pane to `thread`, as the shell does, and gives the
        /// thread `timeline` once its history has arrived.
        pub(in crate::chat) fn bind(
            &mut self,
            chat: &Entity<ChatView>,
            thread: &str,
            timeline: Timeline,
            cx: &mut gpui::VisualTestContext,
        ) {
            let store = self.store.clone();
            store.update(cx, |store, cx| store.show_alone(thread.to_string(), cx));
            chat.update(cx, |chat, cx| {
                chat.set_session(Some(thread.to_string()), cx)
            });
            self.driver
                .wait_until(cx, &store, "the bound thread's history", |cx| {
                    store.read_with(cx, |store, cx| {
                        store
                            .with_timeline_for(store.sole_thread().as_deref(), |_| (), cx)
                            .is_some()
                    })
                });
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(thread.to_string(), timeline, cx);
                cx.notify();
            });
        }

        pub(in crate::chat) fn shutdown(mut self, cx: &mut gpui::VisualTestContext) {
            self.driver.shutdown(cx, &self.store, &self.host);
        }
    }

    pub(in crate::chat) fn seed_chat_with_history(
        cx: &mut TestAppContext,
        timeline: Timeline,
        paged: bool,
    ) -> (
        Entity<WorkspaceStore>,
        Entity<WindowState>,
        String,
        crate::store::tests::LocalHostDriver,
    ) {
        seed_chat_with_history_on(cx, timeline, paged, None)
    }

    pub(in crate::chat) fn seed_chat_with_history_on(
        cx: &mut TestAppContext,
        timeline: Timeline,
        paged: bool,
        provider: Option<agent::ProviderKind>,
    ) -> (
        Entity<WorkspaceStore>,
        Entity<WindowState>,
        String,
        crate::store::tests::LocalHostDriver,
    ) {
        let (host, session_id, timeline) = seeded_chat_host_on(cx, timeline, paged, provider);
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let workspace_store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &workspace_store, "workspace baseline", |cx| {
            workspace_store.read_with(cx, |store, _| store.baseline_ready())
        });
        if paged {
            let link = driver.link.clone();
            driver
                .await_with_events(
                    cx,
                    &workspace_store,
                    link.query(tcode_protocol::Query::SessionHistoryPage {
                        session_id: session_id.clone(),
                        before: u64::MAX,
                        limit: 1,
                    }),
                )
                .expect("the seeded history is read");
        }
        workspace_store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, &workspace_store, "selected conversation", |cx| {
            workspace_store.read_with(cx, |store, cx| {
                store.baseline_ready()
                    && !store.chat_loading_for(store.sole_thread().as_deref(), cx)
            })
        });
        workspace_store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), timeline, cx);
        });
        driver.wait_until(cx, &workspace_store, "Git status", |cx| {
            workspace_store.read_with(cx, |store, cx| {
                store
                    .chat_git_controls_for(store.sole_thread().as_deref(), cx)
                    .is_some()
            })
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let link = driver.link.clone();
        cx.on_quit(move || {
            let events = link.events();
            smol::block_on(smol::future::race(
                link.pump(),
                smol::future::race(
                    async { link.shutdown().await.expect("stop chat test host") },
                    async {
                        // The UI is gone at test teardown; drain its retired subscriptions.
                        while events.recv().await.is_ok() {}
                        std::future::pending::<()>().await;
                    },
                ),
            ));
            host.stopped
                .recv_blocking()
                .expect("chat test host stopped");
        });
        (workspace_store, window_state, session_id, driver)
    }

    use gpui::VisualTestContext;
    use tcode_core::session::StoredEvent;
    use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, Topic};

    /// A thread whose host is scripted: its window arrives as a snapshot,
    /// then records stream in one at a time, as a host sends them.
    pub(in crate::chat) struct ScriptedThread {
        pub(in crate::chat) store: Entity<WorkspaceStore>,
        pub(in crate::chat) incoming: async_channel::Sender<String>,
        pub(in crate::chat) records: Vec<StoredEvent>,
        /// The first record the window holds; the ones before it are history
        /// pages.
        pub(in crate::chat) held_from: usize,
        pub(in crate::chat) status: tcode_protocol::SessionStatus,
        pub(in crate::chat) outgoing: async_channel::Receiver<String>,
        _pump: gpui::Task<()>,
    }

    pub(in crate::chat) const THREAD: &str = "scripted";

    pub(in crate::chat) fn events_topic() -> Topic {
        Topic::SessionEvents {
            session_id: THREAD.into(),
        }
    }

    pub(in crate::chat) fn user_event(id: &str, text: &str) -> agent::AgentEvent {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: agent::ItemContent::UserMessage {
                text: text.into(),
                context_len: None,
                attachments: Vec::new(),
            },
        })
    }

    pub(in crate::chat) fn answer_event(id: &str, text: &str) -> agent::AgentEvent {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: agent::ItemContent::AssistantMessage {
                text: text.into(),
                summarized: false,
            },
        })
    }

    impl ScriptedThread {
        /// The thread holding `events`, its window starting at record
        /// `held_from`.
        pub(in crate::chat) fn new(
            cx: &mut TestAppContext,
            events: Vec<agent::AgentEvent>,
            held_from: usize,
            running: bool,
        ) -> Self {
            use tcode_runtime::pipe::{HostServices, spawn_host};
            use tcode_services::store::SessionStore;

            cx.update(crate::theme::init);
            cx.update(crate::markdown::init);
            let data_root = crate::store::tests::scratch_root("chat-glide-test");
            let host = spawn_host(
                SessionStore::open_at(data_root.clone()).expect("test session store"),
                HostServices::default(),
            )
            .expect("spawn test host");
            let mut status = smol::block_on(host.update_state_for_test(|state, cx| {
                let id = state.start_draft("glide".into(), std::env::temp_dir(), cx);
                state.session_status_snapshot(&id).unwrap()
            }))
            .unwrap();
            host.shutdown_blocking().unwrap();
            let _ = std::fs::remove_dir_all(data_root);
            status.session_id = THREAD.into();
            status.draft = false;
            let mut meta = tcode_core::project::SessionMeta::new(
                agent::ProviderKind::Codex,
                std::env::temp_dir(),
                None,
            );
            meta.id = THREAD.into();

            let (to_host, outgoing) = async_channel::unbounded::<String>();
            let (incoming, from_host) = async_channel::unbounded::<String>();
            let link = tcode_client::HostLink::new(to_host, from_host);
            let executor = cx.background_executor.clone();
            let pump = cx.background_executor.spawn({
                let link = link.clone();
                async move {
                    link.pump_with_timer(|delay| executor.timer(delay)).await;
                }
            });
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    link,
                    crate::store::WorkspaceAttachment::Local,
                    None,
                    None,
                    false,
                    cx,
                )
            });
            crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
            store.update(cx, |store, cx| store.show_alone(THREAD.into(), cx));
            let mut thread = Self {
                store,
                incoming,
                records: Vec::new(),
                held_from,
                status,
                outgoing,
                _pump: pump,
            };
            thread.event(
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            );
            thread.event(
                Topic::Index,
                ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    summary: Default::default(),
                    sessions: vec![meta],
                    projects: vec![],
                }),
            );
            thread.send_window(events);
            thread.set_running(running);
            thread
        }

        pub(in crate::chat) fn event(&self, topic: Topic, event: ServerEvent) {
            self.incoming
                .try_send(
                    tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }

        pub(in crate::chat) fn turns(&self) -> u64 {
            self.records
                .iter()
                .filter(|record| matches!(record.event, agent::AgentEvent::TurnStarted { .. }))
                .count() as u64
        }

        /// The host's status for the thread: whether its last turn runs.
        pub(in crate::chat) fn set_running(&mut self, running: bool) {
            self.status.activity.turn_running = running;
            self.status.running_turn = running.then(|| tcode_core::session::RunningTurn {
                turn: self.turns() - 1,
                started_at: None,
            });
            self.event(
                Topic::SessionStatus {
                    session_id: THREAD.into(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(self.status.clone())),
            );
        }

        /// The host sends its whole window, holding `events` after what it
        /// sent before: what a client folds when it opens the thread, or
        /// when it catches up on the records it missed.
        pub(in crate::chat) fn send_window(&mut self, events: Vec<agent::AgentEvent>) {
            self.records
                .extend(events.into_iter().map(StoredEvent::from));
            let end = self.records.len() as u64;
            let total_turns = self.turns();
            self.event(
                events_topic(),
                ServerEvent::SessionSnapshot {
                    from: self.held_from as u64,
                    end,
                    records: self.records[self.held_from..].to_vec(),
                    total: end,
                    total_turns,
                    truncated: false,
                },
            );
        }

        /// Answer the history page the client asked for with at most `records`
        /// of the records before the window, and say where it ended. `None`
        /// when it asked for none. A client keeps one page in flight.
        pub(in crate::chat) fn answer_page(&mut self, records: usize) -> Option<u64> {
            use tcode_protocol::{ClientPayload, Query, QueryResponse};
            let asked: Vec<_> = std::iter::from_fn(|| self.outgoing.try_recv().ok())
                .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
                .filter_map(|message| match message.payload {
                    ClientPayload::Query(Query::SessionHistoryPage { before, .. }) => {
                        Some((message.id, before))
                    }
                    _ => None,
                })
                .collect();
            assert!(asked.len() <= 1, "one page in flight: {asked:?}");
            let (id, before) = *asked.first()?;
            let from = (before as usize).saturating_sub(records);
            self.held_from = from;
            self.incoming
                .try_send(
                    tcode_protocol::encode_line(&HostMessage::QueryResult {
                        id,
                        result: Ok(QueryResponse::SessionHistoryPage {
                            records: self.records[from..before as usize].to_vec(),
                            from: from as u64,
                            end: before,
                            truncated: false,
                        }),
                    })
                    .unwrap(),
                )
                .unwrap();
            Some(before)
        }

        /// The host records `event` and streams it, as it does while the
        /// agent works.
        pub(in crate::chat) fn live(&mut self, event: agent::AgentEvent) {
            let record = StoredEvent::from(event);
            self.records.push(record.clone());
            self.event(events_topic(), ServerEvent::SessionEvent(record));
        }

        /// Deliver what the host sent.
        pub(in crate::chat) fn deliver(&self, cx: &mut VisualTestContext) {
            cx.run_until_parked();
            self.store
                .update(cx, |store, cx| store.drain_host_events_for_test(cx));
        }

        /// A chat view on the thread, at desktop size.
        pub(in crate::chat) fn open<'a>(
            &self,
            cx: &'a mut TestAppContext,
        ) -> (Entity<ChatView>, &'a mut VisualTestContext) {
            self.open_at(gpui::size(gpui::px(1_024.), gpui::px(700.)), cx)
        }

        pub(in crate::chat) fn open_at<'a>(
            &self,
            size: gpui::Size<gpui::Pixels>,
            cx: &'a mut TestAppContext,
        ) -> (Entity<ChatView>, &'a mut VisualTestContext) {
            let store = self.store.clone();
            let window_state = cx.new(|_| WindowState::new(false));
            let (view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
                ChatView::new(store, window_state, Some(THREAD.to_owned()), window, cx)
            });
            cx.simulate_resize(size);
            (view, cx)
        }
    }
}
