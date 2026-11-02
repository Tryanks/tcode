use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use std::path::{Path, PathBuf};
use std::rc::Rc;

pub(crate) mod components;
mod empty;
mod header;
pub(crate) mod model;
pub(crate) use model::format_elapsed;
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
use gpui_base::{ElementExt as _, Scrollbar, StyledExt as _, h_flex, v_flex};
use serde::Deserialize;

use tcode_core::git::GitAction;
use tcode_core::notice::{HostNotice, OrchestrateCallback};
use tcode_core::session::{EntryContent, SteeringStatus, Timeline, TimelineEntry};
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
    AutoActivityExpansions, CaptureSession, FullOutput, PendingMarkdownBuild,
    ReservationScrollBack, RowArrival, history_screens_covered, jump_to_latest_visible,
    settle_carried_anchor, timeline_inset, transcript_selection,
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

/// Which of the window's edges a conversation's header runs along, and so
/// which window chrome it makes room for.
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
    /// The close button at the header's right end, while the conversation's
    /// pane shares the column: the pane's focus handle, faded in and out by
    /// the shell, which samples it above the dock.
    pane_close: Option<crate::motion::Shown<gpui::FocusHandle>>,
    pull_requests: Entity<crate::pull_requests::PullRequestsPanel>,
    window_state: Entity<WindowState>,
    composer: Entity<Composer>,
    terminal_drawer: Entity<TerminalDrawer>,
    /// The Agents sheet's content on phone.
    agents: Entity<crate::agents_panel::AgentsPanel>,
    terminal_was_open: bool,
    list_state: ListState,
    /// The timeline changed since the list last mirrored it. GPUI's list
    /// resolves wheel and pan packets against the row indices it painted, so
    /// rows are only spliced during a frame, never between two frames.
    timeline_stale: bool,
    history_placeholder_height: gpui::Pixels,
    /// Height of the phone row floating over the foot of the timeline, zero
    /// while it is absent. The last row's padding holds it so the tail clears it.
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
    /// Rows that arrived live, by identity, until they have risen in.
    arrivals: HashMap<u64, RowArrival>,
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
    auto_activity_expansions: AutoActivityExpansions,
    command_panels: RefCell<CommandPanelCache>,
    inline_diffs: InlineDiffCache,
    /// Whole tool outputs fetched for the open session, by item id.
    full_outputs: HashMap<String, FullOutput>,
    session_key: Option<String>,
    /// Turn selected from a command-palette content hit.
    highlighted_turn: Option<usize>,
    /// 1s ticker kept alive while an error card shows a scheduled resume.
    _limit_tick: Option<Task<()>>,
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
    /// The composer itself declines on a mobile build, where focus would
    /// raise the software keyboard; a desktop window of any width types.
    pub fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.focus(window, cx));
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let compact = window_state.read(cx).compact;
        let session_id = workspace_store.read(cx).active_session_id();
        let composer = cx.new(|cx| {
            if compact {
                Composer::new_with_layout(workspace_store.clone(), true, window, cx)
            } else {
                Composer::new(workspace_store.clone(), window, cx)
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
        list_state.set_scroll_handler(move |event, window, cx| {
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
        let subscriptions = vec![
            cx.on_focus_out(&focus_handle, window, |this, _, _, cx| {
                this.selection
                    .update(cx, |selection, cx| selection.clear(cx));
            }),
            // A press on the view's blank space focuses the view itself; the
            // keyboard goes on to the composer, as it does from the pane.
            cx.on_focus(&focus_handle, window, |this, window, cx| {
                if this.session_id.is_some() {
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
                    .terminal_open;
                if open && !this.terminal_was_open {
                    let drawer = this.terminal_drawer.clone();
                    window.defer(cx, move |window, cx| {
                        if !crate::window_seam::is_mobile(cx) {
                            gpui::Focusable::focus_handle(drawer.read(cx), cx).focus(window, cx);
                        }
                    });
                }
                this.terminal_was_open = open;
                cx.notify();
            }),
        ];
        let terminal_drawer = cx.new(|cx| TerminalDrawer::new(workspace_store.clone(), window, cx));
        let pull_requests = cx.new(|cx| {
            crate::pull_requests::PullRequestsPanel::new(
                workspace_store.clone(),
                window_state.clone(),
                cx,
            )
        });
        let agents =
            cx.new(|cx| crate::agents_panel::AgentsPanel::for_sheet(workspace_store.clone(), cx));
        let terminal_was_open = workspace_store
            .read(cx)
            .panel_state_for(session_id.as_deref())
            .terminal_open;

        let mut this = Self {
            pull_requests,
            workspace_store,
            session_id,
            edges: WindowEdges::BOTH,
            pane_close: None,
            window_state,
            composer,
            terminal_drawer,
            agents,
            terminal_was_open,
            list_state,
            timeline_stale: false,
            history_placeholder_height: px(0.),
            phone_strip_height: px(0.),
            reservation_scroll_back: None,
            carried_anchor: false,
            rows: Vec::new(),
            arrivals: HashMap::new(),
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
            auto_activity_expansions: AutoActivityExpansions::default(),
            command_panels: RefCell::new(CommandPanelCache::new()),
            inline_diffs: InlineDiffCache::new(),
            full_outputs: HashMap::new(),
            session_key: None,
            highlighted_turn: None,
            _limit_tick: None,
            copied: CopiedMark::default(),
            commit_dialog: None,
            capture: None,
            _subscriptions: subscriptions,
        };
        this.sync_markdown_states(cx);
        this
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub(crate) fn set_edges(&mut self, edges: WindowEdges, cx: &mut Context<Self>) {
        if self.edges != edges {
            self.edges = edges;
            cx.notify();
        }
    }

    pub(crate) fn set_pane_close(
        &mut self,
        close: Option<crate::motion::Shown<gpui::FocusHandle>>,
        cx: &mut Context<Self>,
    ) {
        if self.pane_close != close {
            self.pane_close = close;
            cx.notify();
        }
    }

    /// Whether the keyboard is in the composer's field, which its card shows.
    pub(crate) fn composer_has_keyboard(&self, window: &Window, cx: &App) -> bool {
        self.composer.read(cx).has_keyboard(window, cx)
    }

    /// Show `session_id`'s thread, or none; the next sync finds the thread
    /// changed.
    pub(crate) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        if self.session_id == session_id {
            return;
        }
        self.composer.update(cx, |composer, cx| {
            composer.set_session(session_id.clone(), cx)
        });
        self.terminal_drawer
            .update(cx, |drawer, cx| drawer.set_session(session_id.clone(), cx));
        self.session_id = session_id;
        self.timeline_stale = true;
        cx.notify();
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
        if self.timeline_stale {
            self.sync_markdown_states(cx);
        }
        self.inline_diffs.sweep();
        let now = cx.background_executor().now();
        self.arrivals.retain(|_, arrival| !arrival.settled(now));
        self.painted_rows.set(None);
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
        let show_jump_to_latest = jump_to_latest_visible(&self.list_state);
        self.sync_markdown_scroll_position(cx);
        self.apply_reservation_scroll_back(window, cx);
        // Measure after this frame's list layout, including the initial tail
        // frame and frames caused by prepends. No scroll event is required.
        // The chat is only read, and the store updated only when the report
        // changes it: an update of either builds the chat again with the next
        // render of the view around it.
        let chat = cx.entity().downgrade();
        window.on_next_frame(move |_, cx| {
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
            .chat_active_session_for(self.session_id.as_deref());

        let compact = self.window_state.read(cx).compact;
        // The composer follows the layout in place. Rebuilding it here would
        // throw away the draft, its selection and its pending attachments every
        // time the window crossed the breakpoint.
        self.composer
            .update(cx, |composer, cx| composer.set_compact(compact, cx));
        // The phone reads on T1 paper; the desktop keeps the glass canvas.
        // The header's title menu dispatches these, and the header is a sibling of `main`.
        let root = v_flex()
            .size_full()
            .min_w_0()
            .track_focus(&self.focus_handle)
            .bg(if compact {
                crate::material::content_surface(cx)
            } else {
                crate::material::canvas(cx)
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
            .chat_loading_for(self.session_id.as_deref());
        let surface = match (&active, loading) {
            (_, true) => 0,
            (None, false) => 1,
            (Some(_), false) => 2,
        };
        let fade = crate::motion::CrossFade::new("chat-surface", surface, 3, window, cx).fill();
        let mut layers = Vec::new();
        if fade.shows(0) {
            layers.push((
                0,
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
        if fade.shows(1) {
            layers.push((
                1,
                v_flex()
                    .size_full()
                    .when(!compact, |el| {
                        el.child(self.render_header(None, false, None, None, cx))
                    })
                    .child(self.render_empty_state(window, cx))
                    .into_any_element(),
            ));
        }
        if let Some(active) = active.filter(|_| !loading) {
            layers.push((
                2,
                self.render_thread(active, show_jump_to_latest, window, cx),
            ));
        }
        root.child(fade.render(layers).size_full())
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
                .map(|id| crate::agents_panel::Agents::of(store, id))
                .filter(|agents| !agents.is_empty())
        };
        let header = self.render_header(title, is_draft, Some(cwd.clone()), agents.as_ref(), cx);
        let phone_strip = self.render_phone_strip(agents.as_ref(), cx);
        let phone_strip_height = if phone_strip.is_some() {
            px(crate::material::bar_height(
                crate::material::Bar::Toolbar,
                true,
            ))
        } else {
            px(0.)
        };
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
        let terminal_open = panel.terminal_open && !self.window_state.read(cx).compact;
        let terminal_height = panel.terminal_height;

        let item_count = self.rows.len();
        let timeline = self.render_timeline_list(&cwd, cx);

        // A fresh phone draft shows its project/model context above the focused composer.
        let timeline: AnyElement = if compact && is_draft && item_count == 0 {
            self.render_compact_draft_empty(&cwd, cx)
        } else {
            // GPUI's scrollbar extent excludes List padding, while wheel and
            // tail-resume calculations include it. Keep the inset outside the
            // list so every input path shares the same bottom and pixel offset.
            v_flex()
                .flex_1()
                .min_h_0()
                .relative()
                .py(timeline_inset(cx))
                .child(crate::scroll::page_viewport(
                    "timeline-bounce",
                    &self.list_state,
                    SelectionArea::new(&self.selection, timeline)
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0(),
                ))
                .when(!window.is_inspector_picking(cx), |timeline| {
                    timeline.child(Scrollbar::vertical(&self.list_state).id("timeline-scrollbar"))
                })
                .into_any_element()
        };

        let composer = self.composer.clone().into_any_element();
        let deliveries = self.workspace_store.read(cx).delivery_messages();
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
            let text_color = gpui_base::motion::transition(
                SharedString::from(format!("delivery-text-{key}")),
                if acknowledged {
                    cx.theme().foreground
                } else {
                    cx.theme().muted_foreground
                },
                crate::motion::settle(),
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
            let status = crate::motion::shown(
                SharedString::from(format!("delivery-status-{key}")),
                status,
                window,
                cx,
            )
            .map(|status| {
                let (danger, muted) = (cx.theme().danger, cx.theme().muted_foreground);
                let line = crate::motion::cross_fade(
                    SharedString::from(format!("delivery-status-line-{key}")),
                    status.value.clone(),
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
            let actions = crate::motion::shown(
                SharedString::from(format!("delivery-actions-{key}")),
                failure.is_some().then_some(()),
                window,
                cx,
            )
            .map(|actions| {
                actions.fold(
                    h_flex()
                        .pt_1()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new(SharedString::from(format!("retry-{key}")))
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
                                .ghost()
                                .small()
                                .debug_selector(|| "discard-delivery".into())
                                .label(crate::tr!("chat.discard_delivery"))
                                .on_click(move |_, _, cx| {
                                    discard_store
                                        .update(cx, |store, _| store.discard_delivery(&discard_key))
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

        let main = v_flex()
            .size_full()
            .min_h_0()
            .on_action(cx.listener(Self::on_link_url))
            .on_action(cx.listener(Self::on_rewind_turn))
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
                                let view = cx.entity().downgrade();
                                let painted = self.painted_rows.clone();
                                move |_, window, cx| {
                                    // List prepaint may change geometry after render
                                    // (resize, splice, or markdown remeasurement).
                                    // Reconcile the sibling control after that layout.
                                    if jump_to_latest_visible(&list) != show_jump_to_latest {
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
                    .px_4()
                    .gap_2()
                    .children(delivery_rows),
            )
            .child(
                div()
                    .id("chat-composer")
                    .debug_selector(|| "chat-composer".into())
                    .w_full()
                    .flex_none()
                    .child(composer),
            );

        let body: AnyElement = if terminal_open {
            let drawer = self.terminal_drawer.clone();
            let drawer_resize = self.terminal_drawer.clone();
            let width = f32::from(window.bounds().size.width);
            if !drawer.read(cx).is_size(width, terminal_height) {
                drawer.update(cx, |drawer, cx| drawer.resize(width, terminal_height, cx));
            }
            gpui_base::v_resizable("chat-terminal-panels")
                .on_resize(move |state, _, cx| {
                    let height = state.read(cx).sizes().get(1).copied();
                    if let Some(height) = height {
                        drawer_resize
                            .update(cx, |drawer, cx| drawer.resize(width, f32::from(height), cx));
                    }
                })
                .child(gpui_base::resizable_panel().child(main))
                .child(
                    gpui_base::resizable_panel()
                        .flex_none()
                        .size(px(terminal_height))
                        .size_range(px(120.)..px(600.))
                        .child(self.terminal_drawer.clone()),
                )
                .into_any_element()
        } else {
            main.into_any_element()
        };
        v_flex()
            .size_full()
            .when(!self.window_state.read(cx).compact, |el| el.child(header))
            .child(body)
            .into_any_element()
    }
}

#[cfg(test)]
pub(in crate::chat) mod tests {
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
        store.update(cx, |store, cx| store.select_session(session_id.clone(), cx));
        driver.wait_until(cx, &store, "selected conversation", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
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

    #[gpui::test]
    fn compact_running_status_clears_composer_and_keyboard(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px};
        struct OccludedChat {
            chat: Entity<ChatView>,
            bottom: gpui::Pixels,
        }
        impl Render for OccludedChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                // Reserve the system-occluded area, as the shell's window seam does.
                div().size_full().pb(self.bottom).child(self.chat.clone())
            }
        }
        let mut timeline = synthetic_markdown_timeline(30);
        Arc::make_mut(timeline.entries.last_mut().unwrap()).content =
            assistant(&"A long running reply keeps its final status visible.\n\n".repeat(40));
        timeline.turn_running = true;
        timeline.turns.last_mut().unwrap().running = true;
        let (store, window_state, _) = seed_chat(cx, timeline);
        cx.update(|cx| cx.set_reduce_motion(true));
        window_state.update(cx, |state, _| state.compact = true);
        let (root, cx) = cx.add_window_view(|window, cx| OccludedChat {
            chat: cx.new(|cx| ChatView::new(store, window_state, window, cx)),
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
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(self.0.clone())
            }
        }
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(6));
        let (around, cx) = cx.add_window_view(|window, cx| {
            Around(cx.new(|cx| ChatView::new(store, window_state, window, cx)))
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

    pub(in crate::chat) fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
        Arc::new(TimelineEntry {
            id: id.to_string(),
            content,
            ts: None,
            finished_ts: None,
            turn: 0,
        })
    }

    pub(in crate::chat) fn user_item(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::UserMessage {
            text: text.into(),
            context_len: None,
            attachments: Vec::new(),
        })
    }

    pub(in crate::chat) fn assistant(text: &str) -> EntryContent {
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

    pub(in crate::chat) fn command_with_output(id: &str) -> Arc<TimelineEntry> {
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
        store.update(cx, |store, cx| store.select_session(first_id.clone(), cx));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, _| !store.chat_loading())
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
            store.update(cx, |store, cx| store.select_session(thread.to_string(), cx));
            chat.update(cx, |chat, cx| {
                chat.set_session(Some(thread.to_string()), cx)
            });
            self.driver
                .wait_until(cx, &store, "the bound thread's history", |cx| {
                    store.read_with(cx, |store, _| store.with_active_timeline(|_| ()).is_some())
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
        workspace_store.update(cx, |store, cx| store.select_session(session_id.clone(), cx));
        driver.wait_until(cx, &workspace_store, "selected conversation", |cx| {
            workspace_store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        workspace_store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), timeline, cx);
        });
        driver.wait_until(cx, &workspace_store, "Git status", |cx| {
            workspace_store.read_with(cx, |store, _| {
                store
                    .chat_git_controls_for(store.active_session_id().as_deref())
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
}
