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

/// The step the timeline's list holds its width to while the column is at
/// its widest ([`ChatView::list_overhang`]): the most it runs past the pane.
const LIST_WIDTH_STEP: f32 = 256.;

mod background;
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
use crate::widgets::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu};
use crate::widgets::scrollbar::{Scrollbar, TRACK};
use crate::widgets::tooltip::Tooltip;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::ItemContent;
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
use crate::store::WorkspaceStore;
use crate::terminal_drawer::TerminalDrawer;
use crate::window_caption;
use crate::window_drag_area;
use crate::window_state::{NavigationPanel, OpenPanel, WindowState};

use self::components::assistant::MdState;
pub use self::timeline::CaptureScroll;
use self::timeline::TimelineLayout;
pub(crate) use self::timeline::TimelineView;

/// A message's context-menu rewind, which opens the panel the bubble's
/// rewind action opens.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_chat, no_json)]
pub(crate) struct OpenRewind {
    /// The message's entry id.
    pub(crate) key: String,
    pub(crate) turn: usize,
    pub(crate) steer: Option<String>,
}

/// A finished turn's fork: a new thread holding everything through it.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_chat, no_json)]
pub(crate) struct ForkThroughTurn {
    pub(crate) turn: usize,
}

gpui::actions!(tcode_chat, [OpenMoveThreadDialog, RevokeSelfSettle]);

/// What a fork is asked of, which its confirmation says.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForkOf {
    /// The whole thread.
    Thread,
    /// The thread through one of its turns.
    Turn,
    /// Through a turn, carrying the words the reader selected: the fork
    /// opens beside the thread with them in its composer.
    TurnWithQuote,
}

/// Ask before forking: a fork is a thread of its own from then on, and
/// `fork` makes it once the reader agrees.
pub(crate) fn confirm_fork(
    of: ForkOf,
    window: &mut Window,
    cx: &mut App,
    fork: impl Fn(&mut App) + 'static,
) {
    use crate::overlay::{DialogButtons, OverlayExt as _};
    let fork = Rc::new(fork);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let fork = fork.clone();
        alert
            .title(crate::tr!(match of {
                ForkOf::Thread => "fork.confirm_title_thread",
                ForkOf::Turn | ForkOf::TurnWithQuote => "fork.confirm_title",
            }))
            .description(crate::tr!(match of {
                ForkOf::Thread => "fork.confirm_thread",
                ForkOf::Turn => "fork.confirm_turn",
                ForkOf::TurnWithQuote => "fork.confirm_quote",
            }))
            .button_props(
                DialogButtons::default()
                    .ok_text(crate::tr!("fork.confirm"))
                    .cancel_text(crate::tr!("settings.cancel"))
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                fork(cx);
                true
            })
    });
}

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
    partition_activity_run, plain_text_as_markdown, segment_entries, timeline_overdraw,
    user_content, user_notice, work_log_capsule_label, work_log_counts, work_log_key,
    work_log_outcome,
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
    /// How much wider the header's git and Open buttons are with their labels
    /// than without, as the header last laid them out.
    header_label_width: Rc<Cell<Option<f32>>>,
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
    /// The host the header names beside the title: the window's panes show
    /// threads of several hosts.
    named_host: Option<gpui::SharedString>,
    /// Which of the new-thread page's slogans it shows, picked as the page
    /// comes in and kept while it stays, whatever its project; `None` off it.
    slogan_seed: Option<u64>,
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
    /// Whether the Agents sheet is open on phone.
    agents_sheet_open: bool,
    /// Whether the background list is open over the thread, where the rail
    /// does not show it.
    background_open: bool,
    /// The piece of background work the timeline last pointed out, by its
    /// call, while it stays marked.
    background_highlight: Option<String>,
    background_unmark: Option<gpui::Task<()>>,
    terminal_was_open: bool,
    /// The conversation's rows, in a view of their own: see
    /// [`TimelineView`]. The view never reads it as it renders but to tell
    /// whether its scene can show, until it has.
    timeline: Entity<TimelineView>,
    /// Where the view last laid the timeline out, as it handed it.
    timeline_layout: TimelineLayout,
    /// The timeline's list, which the agent's card edge hands the wheel on
    /// to, and where the timeline's rows record what opens an agent.
    list_state: ListState,
    timeline_origins: crate::motion::expand::Origins,
    rail_origins: crate::motion::expand::Origins,
    /// Where what opens an agent beside the thread was drawn by this view
    /// (the agent's own surface), for the agent's card to grow out of it
    /// and shrink back into it. The timeline and the rail keep their own.
    side_origins: crate::motion::expand::Origins,
    /// The side rail, in a view of its own: see [`rail::RailView`].
    rail: Entity<rail::RailView>,
    /// How the view last laid the rail out, as it handed it.
    rail_layout: rail::RailLayout,
    /// The timeline's items as the rail last listed them from: its entries'
    /// revision, or stale after any other change of the store.
    entries_revision: Option<u64>,
    rail_stale: bool,
    /// Holds the focus of everything the view draws, the composer included,
    /// so the selection knows when the focus has left it.
    focus_handle: gpui::FocusHandle,
    /// The live commit dialog entity while it is open (kept alive across frames).
    commit_dialog: Option<Entity<CommitDialog>>,
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
        let chat = cx.entity().downgrade();
        let timeline = cx.new(|cx| {
            TimelineView::new(
                workspace_store.clone(),
                window_state.clone(),
                session_id.clone(),
                &scene,
                chat,
                window,
                cx,
            )
        });
        let focus_handle = cx.focus_handle();
        let mut subscriptions = vec![
            cx.on_focus_out(&focus_handle, window, |this, _, _, cx| {
                this.timeline.update(cx, |timeline, cx| {
                    timeline
                        .selection
                        .update(cx, |selection, cx| selection.clear(cx));
                    timeline.dismiss_highlight(cx);
                });
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
                this.rail_stale = true;
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
        subscriptions.push(cx.subscribe(&composer, |this, _, event, cx| {
            match event {
                crate::composer::ComposerEvent::Sent => this
                    .timeline
                    .update(cx, |timeline, cx| timeline.await_sent_message(cx)),
            }
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

        let frame = ChatFrame::new(
            cx.entity().downgrade(),
            |chat: &mut ChatView, width| chat.width = width,
            cx,
        );
        let composer_below_card = composer.read(cx).below_card();
        let chat = cx.entity().downgrade();
        let rail = cx.new(|_| {
            rail::RailView::new(
                workspace_store.clone(),
                window_state.clone(),
                session_id.clone(),
                &scene,
                chat,
                timeline.clone(),
                composer_below_card.clone(),
            )
        });
        Self {
            pull_requests,
            workspace_store,
            session_id,
            edges: WindowEdges::BOTH,
            frame,
            width: px(0.),
            pane_close: None,
            pane_controls: None,
            header_scroll: gpui::ScrollHandle::default(),
            header_label_width: Rc::default(),
            motion: crate::motion::owned::Owned::in_scene(&scene),
            scene_shown: false,
            held: false,
            composer_drawn: false,
            title_recedes: false,
            named_host: None,
            slogan_seed: None,
            scene_at: cx.background_executor().now(),
            window_state,
            composer_layout: (compact, px(0.)),
            composer_below_card,
            composer,
            terminal_drawer,
            agents,
            agents_sheet_open: false,
            background_open: false,
            background_highlight: None,
            background_unmark: None,
            terminal_was_open,
            terminal_split: cx.new(|_| gpui_base::ResizableState::default()),
            terminal_glide: crate::motion::resize_glide::ResizeGlide::new(
                "chat-terminal-panels",
                gpui::Axis::Vertical,
            )
            .in_scene(&scene),
            terminal_glide_compact: false,
            body: Default::default(),
            side: side::Side::new(cx),
            side_of: None,
            pane_width: gpui::Pixels::ZERO,
            overhang: gpui::Pixels::ZERO,
            drawer_agent: None,
            list_state: timeline.read(cx).list_state.clone(),
            timeline_origins: timeline.read(cx).side_origins.clone(),
            rail_origins: rail.read(cx).origins.clone(),
            timeline,
            timeline_layout: TimelineLayout::default(),
            side_origins: crate::motion::expand::Origins::in_scene(&scene),
            rail,
            rail_layout: Default::default(),
            entries_revision: None,
            rail_stale: true,
            focus_handle,
            commit_dialog: None,
            scene,
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// What draws the view, wherever it is drawn.
    pub(crate) fn frame(&self) -> Entity<ChatFrame> {
        self.frame.clone()
    }

    /// How wide the pane drawing this view is, its right panel's room
    /// included. Handed as the pane's drawer renders, before the chat is
    /// built in the same frame: a notify would build it again on the next
    /// frame as well.
    pub(crate) fn set_pane_width(this: &Entity<Self>, width: gpui::Pixels, cx: &mut App) {
        this.update(cx, |chat, _| chat.pane_width = width);
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
    /// width, running on under the panel and past the pane's edge, which
    /// clips it, and only the column's centre moves. That width is held to
    /// a step: a window resized frame by frame, as macOS animates one into
    /// full screen, changes it only as the pane crosses a step, rather than
    /// laying every row out again on each frame.
    fn list_overhang(&self, width: gpui::Pixels) -> gpui::Pixels {
        if self.pane_width > px(0.) && width >= px(CONTENT_MAX_WIDTH + 2. * CONTENT_MIN_PADDING) {
            let reach = f32::from(self.pane_width.max(width));
            px((reach / LIST_WIDTH_STEP).ceil() * LIST_WIDTH_STEP) - width
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
        self.timeline
            .update(cx, |timeline, cx| timeline.dismiss_highlight(cx));
    }

    /// The conversation's timeline.
    pub(crate) fn timeline(&self) -> Entity<TimelineView> {
        self.timeline.clone()
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

    /// The host the header names, or none.
    pub(crate) fn set_named_host(
        this: &Entity<Self>,
        host: Option<gpui::SharedString>,
        cx: &mut App,
    ) {
        this.update(cx, |chat, cx| {
            chat.named_host = host;
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
        self.timeline.update(cx, |timeline, cx| {
            timeline.set_session(session_id.clone(), cx)
        });
        self.rail
            .update(cx, |rail, cx| rail.set_session(session_id.clone(), cx));
        self.session_id = session_id;
        self.background_open = false;
        self.background_highlight = None;
        self.background_unmark = None;
        self.agents_sheet_open = false;
        self.rail_stale = true;
        self.composer_drawn = false;
        // Not yet shown, for a host asking before the view next renders.
        self.scene_shown = false;
        self.scene_at = cx.background_executor().now();
        cx.notify();
    }

    /// The view was taken down and is put up anew (a phone's thread page
    /// pushed again after the reader left it): what it shows is a new
    /// scene, nothing it left moving carried into it.
    pub(crate) fn restart_scene(&mut self, cx: &mut Context<Self>) {
        self.scene.restart();
        cx.notify();
    }
}

/// What draws a conversation, at the width it is laid out at this frame.
pub(crate) type ChatFrame = crate::width_frame::WidthFrame<ChatView>;

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
        self.sync_rail(cx);
        let now = cx.background_executor().now();
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
        // Off any thread the new-thread page is left; coming back to it
        // picks its slogan anew.
        if surface == SURFACE_EMPTY {
            self.slogan_seed = None;
        }
        // What the view was bound to shows once it can be drawn at rest;
        // until then nothing does, and the skeleton only once that has
        // taken longer than a glance.
        if !self.scene_shown && self.scene_ready(loading, cx) {
            self.scene_shown = true;
            // What the thread drew while it readied itself unseen moved
            // toward what arrived meanwhile (a catch-up's rows and text, the
            // goal and rail it brought). The reader's first frame starts the
            // scene anew, so all of that is where it rests; only the
            // surface's own fade in, from what showed until now, plays.
            let unseen = if now.saturating_duration_since(self.scene_at) >= SCENE_PATIENCE {
                SURFACE_SKELETON
            } else {
                SURFACE_NOTHING
            };
            self.scene.restart();
            self.motion
                .layers("chat-surface", unseen, 4, GLIDE, window, cx);
            self.timeline.update(cx, |timeline, cx| {
                timeline.settle_reveals(cx);
                cx.notify();
            });
            self.composer.update(cx, |_, cx| cx.notify());
            self.rail.update(cx, |_, cx| cx.notify());
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
                    .child(self.render_empty_state(cx))
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
            // Motion sampled from element state as the thread is built
            // (a rail card's rows, a plan's steps) is keyed by the path
            // the view renders at, which the scene's root id is not yet
            // on: it is put there, so it is this scene's alone.
            let thread = window.with_id(self.scene.id("chat-thread"), |window| {
                self.render_thread(active, window, cx)
            });
            layers.push((SURFACE_THREAD, thread));
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
    /// once the workspace is known; a thread once its timeline can be
    /// ([`TimelineView::scene_ready`]). Reading the timeline here draws the
    /// view again with it until the scene shows.
    fn scene_ready(&self, loading: bool, cx: &App) -> bool {
        !loading && (self.session_id.is_none() || self.timeline.read(cx).scene_ready(cx))
    }

    /// Has the rail list again what its Screen and Commands cards show,
    /// once the timeline's items or anything else of the store changed.
    fn sync_rail(&mut self, cx: &mut App) {
        let revision = self
            .workspace_store
            .read(cx)
            .entries_revision_for(self.session_id.as_deref(), cx);
        if !std::mem::take(&mut self.rail_stale) && revision == self.entries_revision {
            return;
        }
        self.entries_revision = revision;
        self.rail.update(cx, |rail, cx| rail.sync(cx));
    }
}

impl ChatView {
    /// The open thread: its header over the timeline, its deliveries and its
    /// composer, with the terminal drawer below when it is open.
    fn render_thread(
        &mut self,
        (title, cwd, is_draft): (String, std::path::PathBuf, bool),
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
        let body = self.render_body(self.width, (&cwd, is_draft, agents), window, cx);
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let rail_cards = self.rail.read(cx).cards(cx);
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
                .update(cx, |composer, _| composer.set_column_shift(shift));
        }
        let rail_layout = rail::RailLayout {
            body: self.body,
            cards: rail_cards,
        };
        if rail_layout != self.rail_layout {
            self.rail_layout = rail_layout.clone();
            self.rail.update(cx, |rail, _| rail.set_layout(rail_layout));
        }
        let mut side = self.render_side(window, cx);
        let phone_strip =
            self.render_phone_strip(agents.as_ref(), &self.rail_layout.cards.clone(), window, cx);
        let phone_strip_height = phone_strip.as_ref().map_or(px(0.), |(_, progress)| {
            px(crate::material::bar_height(
                crate::material::Bar::Toolbar,
                true,
            )) * *progress
        });
        let phone_strip = phone_strip.map(|(strip, _)| strip);
        let panel = self
            .workspace_store
            .read(cx)
            .panel_state_for(self.session_id.as_deref());
        let terminal_open =
            panel.terminal_open && !self.window_state.read(cx).compact && !self.is_side();
        let terminal_height = panel.terminal_height;
        let timeline_layout = TimelineLayout {
            width,
            overhang,
            shift,
            strip: phone_strip_height,
            cwd: cwd.to_path_buf(),
            is_draft,
        };
        if timeline_layout != self.timeline_layout {
            self.timeline_layout = timeline_layout.clone();
            self.timeline
                .update(cx, |timeline, _| timeline.set_layout(timeline_layout));
        }

        let composer = self.composer.clone().into_any_element();
        let deliveries = self
            .workspace_store
            .read(cx)
            .delivery_messages_for(self.session_id.as_deref(), cx);
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
                                        retry_store.update(cx, |store, cx| {
                                            store.retry_delivery(&retry_key, cx)
                                        })
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
        // A fresh draft on the desktop is the new-thread page: where it
        // starts and the composer in the middle of the body.
        let new_page = is_draft
            && !compact
            && !self.is_side()
            && delivery_rows.is_empty()
            && self.timeline.read(cx).rows.is_empty();
        if !new_page {
            self.slogan_seed = None;
        }
        // The agent's card is laid out after the rail, over it, as the two
        // give way to each other in the slot.
        let side_slot = rail::Slots {
            below_card: self.composer_below_card.clone(),
            slots: side.card.take().into_iter().collect(),
        };
        let main = v_flex()
            .relative()
            .size_full()
            .min_h_0()
            // Dispatched along the focus path, which runs through the
            // composer here, not through the rows that offer them.
            .on_action(cx.listener(|this, action, window, cx| {
                this.timeline
                    .update(cx, |timeline, cx| timeline.on_link_url(action, window, cx))
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.timeline.update(cx, |timeline, cx| {
                    timeline.on_open_rewind(action, window, cx)
                })
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.timeline.update(cx, |timeline, cx| {
                    timeline.on_fork_through_turn(action, window, cx)
                })
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.timeline.update(cx, |timeline, cx| {
                    timeline.on_copy_whole_output(action, window, cx)
                })
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.timeline.update(cx, |timeline, cx| {
                    timeline.on_open_file_diff(action, window, cx)
                })
            }))
            .map(|main| {
                let composer = div()
                    .id("chat-composer")
                    .debug_selector(|| "chat-composer".into())
                    .w_full()
                    // The composer is as tall as it draws while the column
                    // has that much room, and no taller than the column:
                    // its panels give way inside it.
                    .flex()
                    .flex_col()
                    .min_h_0()
                    // A press on the composer puts the keyboard resting
                    // there to use. Capture, so the composer's own handling
                    // still runs and cannot hide it.
                    .capture_any_mouse_down(
                        cx.listener(|this, _, _, cx| this.dismiss_highlight(cx)),
                    )
                    .child(composer);
                if new_page {
                    // The timeline stays drawn, out of sight, so it still
                    // tells the view when the thread is ready.
                    return main
                        .child(
                            div()
                                .absolute()
                                .size_0()
                                .overflow_hidden()
                                .child(self.timeline.clone()),
                        )
                        .child(self.render_new_thread_page(
                            composer.into_any_element(),
                            shift,
                            cx,
                        ));
                }
                main.child(
                    div()
                        .id("timeline")
                        .debug_selector(|| "chat-timeline".into())
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .child(self.timeline.clone())
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
                .child(composer)
            })
            .child(self.rail.clone())
            .child(side_slot);
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
pub(crate) mod tests;
