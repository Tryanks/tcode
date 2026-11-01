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
pub(crate) use model::format_duration;
mod timeline;

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
    ReservationScrollBack, SEGMENT_GAP, TIMELINE_EDGE_PADDING, TURN_GAP, history_screens_covered,
    jump_to_latest_visible, settle_carried_anchor, timeline_inset, transcript_selection,
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

use self::components::changed_files::InlineDiffCache;
use self::components::command_panel::CommandPanelCache;
use self::model::{
    LimitResumeAction, RowRenderArgs, Segment, activity_run_duration_ms, displayed_error_text,
    divergent_served_model, format_elapsed_deciseconds, latest_message_ids, limit_resume_action,
    live_edit_counts, live_edit_rows, notice_keys, partition_activity_run, plain_text_as_markdown,
    segment_entries, start_hub_projects, timeline_overdraw, user_content, user_notice,
    user_visible_text, work_log_capsule_label, work_log_counts, work_log_key, work_log_outcome,
};
use self::timeline::rows::{
    ListSync, RowPart, TimelineContinuity, TimelineRow, TurnIndexCache, row_of_entry, rows_of_turn,
};
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

        if self
            .workspace_store
            .read(cx)
            .chat_loading_for(self.session_id.as_deref())
        {
            return root.child(
                v_flex()
                    .size_full()
                    .when_some(
                        self.workspace_store
                            .read(cx)
                            .history_error_for(self.session_id.as_deref())
                            .map(str::to_owned),
                        |container, error| container.child(div().px_4().child(error)),
                    )
                    .child(crate::material::loading_skeleton(cx)),
            );
        }

        let Some((title, cwd, is_draft)) = active else {
            return root
                .when(!self.window_state.read(cx).compact, |el| {
                    el.child(self.render_header(None, false, None, None, cx))
                })
                .child(self.render_empty_state(window, cx));
        };

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

        // Group entries by turn and render each turn section into the centered
        // content column. The column fills the available width up to
        // `CONTENT_MAX_WIDTH`; horizontal padding lives on the centering wrapper
        // (below) so the column shrinks gracefully — never clipping — when the
        // diff panel narrows the chat region.
        // The newest user / assistant message: their action rows stay visible
        // (hover is not the only way to reach Copy / native rewind).
        let (last_user_id, last_assistant_id) = self
            .workspace_store
            .read(cx)
            .with_timeline_for(self.session_id.as_deref(), |timeline| {
                latest_message_ids(&timeline.entries)
            })
            .unwrap_or_default();

        let item_count = self.rows.len();
        let item_cwd = cwd.clone();
        let timeline = list(
            self.list_state.clone(),
            cx.processor(move |this, index: usize, window, cx| {
                let Some(row) = this.rows.get(index).cloned() else {
                    return div().into_any_element();
                };
                // The trailer's turn span comes from the timeline, not its
                // rows: a pending steer after a message belongs to no row.
                // The rows can trail the live timeline by a frame (e.g.
                // adopting a running background thread whose timeline is
                // being re-folded), so the ranges are bounds-checked.
                let own_range = match &row.part {
                    RowPart::WorkLogHeader { run } => run.clone(),
                    _ => row.entry_range.clone(),
                };
                let Some((turn, own, trailer)) = this.workspace_store.read(cx).with_timeline_for(
                    this.session_id.as_deref(),
                    |timeline| {
                        let entries = &timeline.entries;
                        let trailer = if row.last_in_turn {
                            let start = entries.partition_point(|entry| entry.turn < row.turn);
                            let len =
                                entries[start..].partition_point(|entry| entry.turn == row.turn);
                            entries[start..start + len].to_vec()
                        } else {
                            Vec::new()
                        };
                        (
                            timeline.turns.get(row.turn).cloned().unwrap_or_default(),
                            entries
                                .get(own_range)
                                .map(<[_]>::to_vec)
                                .unwrap_or_default(),
                            trailer,
                        )
                    },
                ) else {
                    return div().into_any_element();
                };
                let rendered = this.render_row(
                    (
                        &row,
                        &turn,
                        &item_cwd,
                        &own,
                        &trailer,
                        (last_user_id.as_deref(), last_assistant_id.as_deref()),
                    ),
                    window,
                    cx,
                );
                // An expanded Work Log's parts keep the Work Log's own rhythm.
                let continued = this.rows.get(index + 1).is_some_and(|next| {
                    matches!(next.part, RowPart::WorkLogActivity | RowPart::WorkLogLive)
                });
                let painted = this.painted_rows.clone();
                v_flex()
                    .debug_selector(move || format!("timeline-row-{index}"))
                    .on_prepaint(move |_, _, _| {
                        painted.set(Some(
                            painted.get().map_or((index, index + 1), |(start, end)| {
                                (start.min(index), end.max(index + 1))
                            }),
                        ));
                    })
                    .w_full()
                    .items_center()
                    .px(px(if this.window_state.read(cx).compact {
                        crate::material::COMPACT_PAGE_INSET
                    } else {
                        CONTENT_MIN_PADDING
                    }))
                    .when(this.highlighted_turn == Some(row.turn), |item| {
                        item.rounded(crate::material::radius_card(cx))
                            .bg(cx.theme().list_active)
                    })
                    .when(index == 0, |item| item.pt(px(TIMELINE_EDGE_PADDING)))
                    .map(|item| {
                        if index + 1 == item_count {
                            item.pb(px(TIMELINE_EDGE_PADDING) + this.phone_strip_height)
                        } else if row.last_in_turn {
                            item.pb(px(TURN_GAP))
                        } else if continued {
                            item.pb_1()
                        } else {
                            item.pb(px(SEGMENT_GAP))
                        }
                    })
                    // `min_w_0`: a turn holds nowrap content (diff rows, command
                    // output). Without it this flex item grows to that content
                    // and the column runs past the page inset instead of
                    // scrolling inside it.
                    .when(
                        index == 0 && this.history_placeholder_height > px(0.),
                        |item| {
                            item.child(
                                div()
                                    .w_full()
                                    .h(this.history_placeholder_height)
                                    .flex_none()
                                    .flex()
                                    .flex_col()
                                    .justify_end()
                                    .when(
                                        this.workspace_store
                                            .read(cx)
                                            .history_loading_for(this.session_id.as_deref()),
                                        |space| {
                                            space.child(
                                                div()
                                                    .id("history-activity")
                                                    .h(px(24.))
                                                    .flex_none()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(
                                                        crate::widgets::spinner::Spinner::new()
                                                            .xsmall()
                                                            .color(cx.theme().muted_foreground),
                                                    ),
                                            )
                                        },
                                    ),
                            )
                        },
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .child(rendered),
                    )
                    .into_any_element()
            }),
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .flex_1()
        .min_h_0();

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
        let waiting = !self
            .workspace_store
            .read(cx)
            .connection_state()
            .is_connected();
        let delivery_rows = deliveries
            .into_iter()
            .map(|(key, text, failure, acknowledged)| {
                let retry_key = key.clone();
                let discard_key = key.clone();
                let retry_store = self.workspace_store.clone();
                let discard_store = self.workspace_store.clone();
                v_flex()
                    .id(SharedString::from(format!("delivery-{key}")))
                    .debug_selector(|| "pending-delivery-bubble".into())
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .items_end()
                    .gap_1()
                    .child(
                        div()
                            .max_w_3_4()
                            .px(cx.theme().tokens.spacing.md)
                            .py(cx.theme().tokens.spacing.xs)
                            .text_token(&cx.theme().tokens.typography.md)
                            .rounded(crate::material::radius_composer(cx))
                            .bg(cx.theme().foreground.opacity(0.08))
                            .text_color(if acknowledged {
                                cx.theme().foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(text),
                    )
                    .when(failure.is_some() || !acknowledged, |row| {
                        row.child(
                            div()
                                .text_secondary(false, cx)
                                .text_color(if failure.is_some() {
                                    cx.theme().danger
                                } else {
                                    cx.theme().muted_foreground
                                })
                                .child(failure.clone().unwrap_or_else(|| {
                                    crate::tr!(if waiting {
                                        "chat.waiting_connection"
                                    } else {
                                        "chat.sending"
                                    })
                                    .into_owned()
                                })),
                        )
                    })
                    .when(failure.is_some(), |row| {
                        row.child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new(SharedString::from(format!("retry-{key}")))
                                        .ghost()
                                        .small()
                                        .debug_selector(|| "retry-delivery".into())
                                        .label(crate::tr!("chat.retry_delivery"))
                                        .on_click(move |_, _, cx| {
                                            retry_store.update(cx, |store, _| {
                                                store.retry_delivery(&retry_key)
                                            })
                                        }),
                                )
                                .child(
                                    Button::new(SharedString::from(format!("discard-{key}")))
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
                    })
            })
            .collect::<Vec<_>>();

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
                    .when(show_jump_to_latest, |this| {
                        this.child(self.render_scroll_pill(cx))
                    })
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
        root.when(!self.window_state.read(cx).compact, |el| el.child(header))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::timeline::residency::ASYNC_MARKDOWN_THRESHOLD_BYTES;
    use super::{ChatView, RowPart};
    use crate::store::WorkspaceStore;
    use crate::test_frames::{draw, draw_settled};
    use crate::window_state::WindowState;
    use agent::{ItemContent, ItemStatus};
    use gpui::{AppContext as _, Entity, TestAppContext};
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use std::time::{Duration, Instant};
    use tcode_core::session::{EntryContent, Timeline, TimelineEntry, TurnMeta};

    static NEXT_RESIDENCY_TEST_ID: AtomicU64 = AtomicU64::new(0);

    /// Opening a running thread opens the detail of its latest command only;
    /// the commands before it were already superseded.
    #[gpui::test]
    fn running_session_snapshot_renders_only_latest_command_expanded(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries.push(entry("user", user_item("go")));
        for index in 1..=5 {
            timeline
                .entries
                .push(command_with_output(&format!("command-{index}")));
        }

        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        cx.update(|cx| cx.set_reduce_motion(true));
        let (_view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);

        for index in 1..=4 {
            assert!(
                command_open(cx, &format!("command-{index}")).is_none(),
                "historical command {index} reopened"
            );
        }
        assert!(command_open(cx, "command-5").is_some());
    }

    /// The latest command's detail opens by itself. Once a newer command
    /// starts it stays open until it has shown for half a second, unless a
    /// second newer one has started, and a newer one that goes away leaves it
    /// the latest again.
    #[gpui::test]
    fn the_latest_command_stays_open_for_half_a_second_after_the_next_starts(
        cx: &mut TestAppContext,
    ) {
        use gpui::{VisualTestContext, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries = vec![entry("user", user_item("go")), command_with_output("first")];
        let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
        cx.update(|cx| cx.set_reduce_motion(true));
        let (_view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(900.)));
        let mut show = |commands: &[&str], cx: &mut VisualTestContext| {
            timeline.entries.truncate(1);
            timeline
                .entries
                .extend(commands.iter().map(|id| command_with_output(id)));
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                cx.notify();
            });
            draw_settled(cx);
        };
        let open = |cx: &mut VisualTestContext, ids: &[&str]| {
            ["first", "second", "third", "fourth"]
                .into_iter()
                .filter(|id| command_open(cx, id).is_some())
                .collect::<Vec<_>>()
                == ids
        };
        let wait = |duration: u64, cx: &mut VisualTestContext| {
            cx.executor().advance_clock(Duration::from_millis(duration));
            draw_settled(cx);
        };

        show(&["first"], cx);
        assert!(open(cx, &["first"]));
        show(&["first", "second"], cx);
        assert!(open(cx, &["first", "second"]), "first folded on its own");
        wait(400, cx);
        assert!(open(cx, &["first", "second"]), "first folded early");
        wait(100, cx);
        assert!(open(cx, &["second"]), "first stayed past its minimum");

        show(&["first", "second", "third", "fourth"], cx);
        assert!(
            open(cx, &["third", "fourth"]),
            "second kept its minimum behind two newer commands"
        );

        // The newest goes again: the one before it is the latest once more.
        show(&["first", "second", "third"], cx);
        wait(1_000, cx);
        assert!(open(cx, &["third"]), "the latest folded");
    }

    /// A user turn's rewind popover offers exactly the scopes its harness
    /// restores that turn in: Claude Code restores only files of the first
    /// turn and all three scopes of a later one; OpenCode restores the
    /// conversation, with or without files, of any turn, and never files alone.
    #[gpui::test]
    fn the_rewind_popover_offers_exactly_the_harness_rewind_modes(cx: &mut TestAppContext) {
        use agent::{ProviderKind, RewindMode};
        use gpui::{Modifiers, VisualTestContext, px, size};

        const ALL: [RewindMode; 3] = [
            RewindMode::FilesAndConversation,
            RewindMode::Conversation,
            RewindMode::Files,
        ];
        for (provider, turn, offered) in [
            (ProviderKind::ClaudeCode, 0, &[RewindMode::Files][..]),
            (ProviderKind::ClaudeCode, 1, &ALL[..]),
            (
                ProviderKind::OpenCode,
                0,
                &[RewindMode::FilesAndConversation, RewindMode::Conversation][..],
            ),
            (
                ProviderKind::OpenCode,
                1,
                &[RewindMode::FilesAndConversation, RewindMode::Conversation][..],
            ),
        ] {
            let mut timeline = Timeline::default();
            timeline.turns = (0..2)
                .map(|index| TurnMeta {
                    provider_checkpoint_id: Some(format!("msg_checkpoint_{index}")),
                    ..TurnMeta::default()
                })
                .collect();
            timeline.entries.extend([
                at_turn(entry("user-0", user_item("first")), 0),
                at_turn(entry("assistant-0", assistant("One.")), 0),
                at_turn(entry("user-1", user_item("second")), 1),
                at_turn(entry("assistant-1", assistant("Two.")), 1),
            ]);
            let (workspace_store, window_state, _, _driver) =
                seed_chat_with_history_on(cx, timeline, false, Some(provider));
            let (_view, window) = cx.add_window_view(|window, cx| {
                ChatView::new(workspace_store, window_state, window, cx)
            });
            let window: &mut VisualTestContext = window;
            window.simulate_resize(size(px(1_024.), px(700.)));
            window.update(|window, cx| {
                let _ = window.draw(cx);
            });
            // The bubble's actions show while the pointer is over it.
            let bubble = window
                .debug_bounds(["user-bubble-user-0", "user-bubble-user-1"][turn])
                .expect("the user bubble is drawn");
            window.simulate_mouse_move(bubble.center(), None, Modifiers::default());
            window.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let trigger = window
                .debug_bounds(["rewind-0", "rewind-1"][turn])
                .unwrap_or_else(|| panic!("{provider:?} turn {turn} has a rewind button"));
            window.simulate_click(trigger.center(), Modifiers::default());
            window.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let shown: Vec<RewindMode> = ALL
                .into_iter()
                .filter(|mode| {
                    window
                        .debug_bounds(match mode {
                            RewindMode::FilesAndConversation => {
                                "rewind-option-FilesAndConversation"
                            }
                            RewindMode::Conversation => "rewind-option-Conversation",
                            RewindMode::Files => "rewind-option-Files",
                        })
                        .is_some()
                })
                .collect();
            assert_eq!(shown, offered, "{provider:?} turn {turn}");
        }
    }

    #[gpui::test]
    fn pending_steer_after_the_last_segment_renders_in_the_trailer(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        use tcode_core::session::SteeringStatus;

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries.extend([
            entry("user", user_item("go")),
            entry("assistant", assistant("On it.")),
            entry(
                "steer",
                EntryContent::Steer {
                    text: "also do this".into(),
                    status: SteeringStatus::Pending,
                    context_len: None,
                    attachments: Vec::new(),
                },
            ),
        ]);

        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        cx.update(|cx| cx.set_reduce_motion(true));
        let (_view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);

        assert!(cx.debug_bounds("steering-steer").is_some());
    }

    /// The host steers its notices into a running turn: steered or still
    /// pending, a notice is a disclosure row per notice, never the user's bubble.
    #[gpui::test]
    fn steered_notices_render_as_disclosures_not_bubbles(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        use tcode_core::session::SteeringStatus;

        let steer = |text: String, status| EntryContent::Steer {
            text,
            status,
            context_len: None,
            attachments: Vec::new(),
        };
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries.extend([
            entry("user", user_item("go")),
            entry(
                "wake",
                steer(
                    tcode_core::pull_request_watch::merged_message(5, "https://x/pull/5"),
                    SteeringStatus::Accepted,
                ),
            ),
            entry("assistant", assistant("On it.")),
            entry(
                "results",
                steer(
                    "[orchestrate] thread a (\"First\") reported.\nResult (reported via report_result):\none\n\n[orchestrate] thread b (\"Second\") failed.\nthe provider closed".into(),
                    SteeringStatus::Pending,
                ),
            ),
        ]);

        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        cx.update(|cx| cx.set_reduce_motion(true));
        let (_view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);

        for row in [
            "disclosure-pull-request-update-wake",
            "disclosure-orchestrate-callback-results-0",
            "disclosure-orchestrate-callback-results-1",
        ] {
            assert!(cx.debug_bounds(row).is_some(), "{row} is not a disclosure");
        }
        for bubble in ["steering-wake", "steering-results"] {
            assert!(cx.debug_bounds(bubble).is_none(), "{bubble} is a bubble");
        }
    }

    #[gpui::test]
    fn a_paused_reader_on_the_live_window_stays_put_as_it_folds(cx: &mut TestAppContext) {
        use gpui::{ListOffset, Modifiers, VisualTestContext, px, size};
        use tcode_core::session::SteeringStatus;

        let tool = |id: &str| {
            entry(
                id,
                EntryContent::Item(ItemContent::ToolCall {
                    image_reads: Vec::new(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": id }),
                    output: None,
                    status: ItemStatus::Completed,
                }),
            )
        };
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries.push(entry("user", user_item("go")));
        for index in 0..20 {
            timeline.entries.push(tool(&format!("tool-{index}")));
        }
        // Steers queued behind the live window keep content below it.
        for index in 0..12 {
            timeline.entries.push(entry(
                &format!("steer-{index}"),
                EntryContent::Steer {
                    text: format!("queued steer {index}"),
                    status: SteeringStatus::Pending,
                    context_len: None,
                    attachments: Vec::new(),
                },
            ));
        }

        let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
        cx.update(|cx| cx.set_reduce_motion(true));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);
        view.update(cx, |chat, cx| {
            chat.list_state.scroll_to(ListOffset::default());
            cx.notify();
        });
        draw_settled(cx);
        let header = cx
            .debug_bounds("worklog-header-0-tool-0")
            .expect("collapsed work log");
        cx.simulate_click(header.center(), Modifiers::default());
        draw_settled(cx);
        // The reader rests two activities into the live window.
        view.update(cx, |chat, cx| {
            let live = chat
                .rows
                .iter()
                .position(|row| row.part == RowPart::WorkLogLive)
                .expect("an expanded live work log");
            chat.list_state.scroll_to(ListOffset {
                item_ix: live,
                offset_in_item: px(70.),
            });
            cx.notify();
        });
        draw_settled(cx);
        let reading = cx
            .debug_bounds("activity-row-tool-18")
            .expect("the activity under the reader")
            .top();
        let top = |cx: &mut VisualTestContext| {
            cx.debug_bounds("activity-row-tool-18")
                .map(|bounds| bounds.top())
        };

        let update = |timeline: &Timeline, cx: &mut VisualTestContext| {
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                cx.notify();
            });
            draw_settled(cx);
            draw_settled(cx);
        };
        timeline.entries.push(tool("tool-20"));
        timeline.entries.push(tool("tool-21"));
        update(&timeline, cx);
        assert_eq!(
            top(cx),
            Some(reading),
            "new activities folding the live window moved the reader"
        );

        timeline.turns[0].running = false;
        update(&timeline, cx);
        // Settled, the activity takes the folded rows' indent but keeps its
        // line.
        assert_eq!(
            top(cx),
            Some(reading),
            "the live window settling moved the reader"
        );
    }

    #[gpui::test]
    fn collapsing_work_log_preserves_following_message_bounds(cx: &mut TestAppContext) {
        use gpui::{Modifiers, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            start_ts: Some(1_000),
            end_ts: Some(2_600),
            ..TurnMeta::default()
        }];
        timeline.entries = vec![
            entry("user", user_item("Check the layout.")),
            entry(
                "command",
                EntryContent::Item(ItemContent::CommandExecution {
                    command: "printf 'layout checked\\n'".into(),
                    output: "layout checked\n".into(),
                    exit_code: Some(0),
                    status: ItemStatus::Completed,
                }),
            ),
            entry(
                "answer",
                assistant(
                    "The layout check is complete. The final assistant message stays in the centred content column.\n\nThe footer belongs below this last line.",
                ),
            ),
        ];
        let (store, window_state, _) = seed_chat(cx, timeline);
        let (_view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        for (mode, width) in [
            (crate::theme::ThemeMode::Light, 1_400.),
            (crate::theme::ThemeMode::Light, 393.),
            (crate::theme::ThemeMode::Dark, 1_400.),
            (crate::theme::ThemeMode::Dark, 393.),
        ] {
            cx.update(|window, cx| crate::theme::change_mode(mode, Some(window), cx));
            cx.simulate_resize(size(px(width), px(900.)));
            draw_settled(cx);
            let before = [
                cx.debug_bounds("markdown-block-0").expect("answer start"),
                cx.debug_bounds("markdown-block-1")
                    .expect("answer last line"),
            ];
            let footer_before = cx.debug_bounds("turn-time-row").expect("turn footer");
            for expanded in [true, false] {
                let header = cx
                    .debug_bounds("worklog-header-0-command")
                    .expect("work log toggle");
                cx.simulate_click(header.center(), Modifiers::default());
                draw_settled(cx);
                assert_eq!(
                    cx.debug_bounds("activity-row-command").is_some(),
                    expanded,
                    "the click must toggle the work log"
                );
            }
            let after = [
                cx.debug_bounds("markdown-block-0").expect("answer start"),
                cx.debug_bounds("markdown-block-1")
                    .expect("answer last line"),
            ];
            let footer_after = cx.debug_bounds("turn-time-row").expect("turn footer");
            assert_eq!(
                after, before,
                "collapsing moved or resized the answer at {width}px in {mode:?}"
            );
            assert_eq!(footer_after, footer_before, "collapsing moved the footer");
            assert!(
                footer_after.top() >= after[1].bottom(),
                "the footer overlaps the answer's last line: {after:?}, {footer_after:?}"
            );
        }
    }

    #[gpui::test]
    fn an_expanded_work_log_lays_out_only_the_activities_on_screen(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, Modifiers, VisualTestContext, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default()];
        timeline.entries.push(entry("user", user_item("go")));
        for index in 0..400 {
            timeline.entries.push(command(&format!("command-{index}")));
        }
        timeline
            .entries
            .push(entry("assistant", assistant("Done.")));

        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        let (view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);

        let header = cx
            .debug_bounds("worklog-header-0-command-0")
            .expect("collapsed work log");
        cx.simulate_click(header.center(), Modifiers::default());
        draw_settled(cx);
        view.update(cx, |chat, cx| {
            chat.list_state.scroll_to(ListOffset::default());
            cx.notify();
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("activity-row-command-0").is_some());
        assert!(
            cx.debug_bounds("activity-row-command-399").is_none(),
            "an expanded work log must not lay out all 400 activities"
        );

        view.update(cx, |chat, cx| {
            chat.list_state.set_follow_mode(FollowMode::Tail);
            cx.notify();
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("activity-row-command-399").is_some());
        assert!(cx.debug_bounds("activity-row-command-0").is_none());

        // Opening one activity keeps the measured heights of the turn's
        // other rows, however far away.
        view.update(cx, |chat, cx| {
            chat.list_state.scroll_to(ListOffset::default());
            cx.notify();
        });
        draw_settled(cx);
        let last = view.read_with(cx, |chat, _| chat.rows.len() - 1);
        let measured = |cx: &mut VisualTestContext| {
            view.read_with(cx, |chat, _| {
                chat.list_state.bounds_for_item(last).is_some()
            })
        };
        assert!(measured(cx));
        let row = cx
            .debug_bounds("activity-row-command-0")
            .expect("the first activity");
        cx.simulate_click(row.center(), Modifiers::default());
        draw_settled(cx);
        assert!(cx.debug_bounds("activity-detail").is_some());
        assert!(
            measured(cx),
            "opening one activity remeasured the whole turn"
        );
    }

    #[gpui::test]
    fn a_live_file_edit_lays_out_only_the_diff_lines_on_screen(cx: &mut TestAppContext) {
        use gpui::{Modifiers, ScrollDelta, ScrollEvent, TouchPhase, VisualTestContext};
        use gpui::{point, px, size};

        let mut diff = format!("@@ -1,400 +1,401 @@\n+// {}\n", "wide ".repeat(80));
        for line in 0..400 {
            diff.push_str(&format!("-fn old_{line}() {{}}\n+fn new_{line}() {{}}\n"));
        }
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries = vec![entry("user", user_item("go"))];
        for note in 0..20 {
            timeline.entries.push(entry(
                &format!("note-{note}"),
                assistant("A note above the edit."),
            ));
        }
        timeline.entries.push(entry(
            "edit",
            EntryContent::Item(ItemContent::FileChange {
                changes: vec![agent::FileChange {
                    path: "src/lib.rs".into(),
                    kind: agent::FileChangeKind::Modify,
                    diff: Some(diff),
                }],
                status: ItemStatus::InProgress,
            }),
        ));
        let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
        cx.update(|cx| cx.set_reduce_motion(true));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(600.), px(900.)));
        draw_settled(cx);

        assert!(cx.debug_bounds("file-edit-diff-line-0").is_some());
        assert!(
            cx.debug_bounds("file-edit-diff-line-300").is_none(),
            "an inline diff must not lay out all 801 lines"
        );

        // Both axes scroll inside the diff; the timeline stays put.
        let timeline_top = |cx: &mut VisualTestContext| {
            let top = view.read_with(cx, |chat, _| chat.list_state.logical_scroll_top());
            (top.item_ix, top.offset_in_item)
        };
        let before = timeline_top(cx);
        assert!(
            view.read_with(cx, |chat, _| chat.list_state.max_offset_for_scrollbar().y) > px(0.)
        );
        let diff_bounds = cx.debug_bounds("file-edit-diff").expect("inline diff");
        let wheel = |delta, cx: &mut VisualTestContext| {
            for (touch_phase, delta) in [
                (TouchPhase::Started, point(px(0.), px(0.))),
                (TouchPhase::Moved, delta),
                (TouchPhase::Ended, point(px(0.), px(0.))),
            ] {
                cx.simulate_event(ScrollEvent {
                    position: diff_bounds.center(),
                    delta: ScrollDelta::Pixels(delta),
                    modifiers: Modifiers::default(),
                    touch_phase,
                    ..Default::default()
                });
            }
            draw_settled(cx);
        };
        wheel(point(px(0.), px(-2_000.)), cx);
        assert!(cx.debug_bounds("file-edit-diff-line-0").is_none());
        // `debug_bounds` takes a `'static` selector.
        let shown: &'static str = (1..801)
            .map(|ix| &*format!("file-edit-diff-line-{ix}").leak())
            .find(|selector| cx.debug_bounds(selector).is_some())
            .expect("a line scrolled into the diff");
        let line = cx.debug_bounds(shown).unwrap();
        wheel(point(px(-200.), px(0.)), cx);
        let scrolled = cx.debug_bounds(shown).unwrap();
        assert!(
            scrolled.left() < line.left(),
            "{scrolled:?} did not scroll left of {line:?}"
        );
        assert_eq!(scrolled.top(), line.top());
        assert_eq!(timeline_top(cx), before);

        // The provider reports a different patch for the same edit.
        timeline.entries[21] = entry(
            "edit",
            EntryContent::Item(ItemContent::FileChange {
                changes: vec![agent::FileChange {
                    path: "src/lib.rs".into(),
                    kind: agent::FileChangeKind::Modify,
                    diff: Some("@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n".into()),
                }],
                status: ItemStatus::InProgress,
            }),
        );
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("file-edit-diff-line-1").is_some());
        assert!(cx.debug_bounds("file-edit-diff-line-2").is_none());
    }

    /// A message on screen dozens of short rows above the end shows as
    /// Markdown, not as its source, when the reader comes back to the
    /// latest from the start of the thread.
    #[gpui::test]
    fn markdown_on_screen_above_an_expanded_work_log_is_built(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, Modifiers, VisualTestContext, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default()];
        timeline.entries = vec![entry("user", user_item("go"))];
        for index in 0..100 {
            timeline.entries.push(entry(
                &format!("note-{index}"),
                assistant(&format!("Note {index}.")),
            ));
        }
        // The only code block in the thread: its lines exist only once the
        // message is rendered as Markdown.
        timeline
            .entries
            .push(entry("early", assistant("```\nLooking around first.\n```")));
        for index in 0..40 {
            timeline.entries.push(command(&format!("command-{index}")));
        }
        timeline.entries.push(entry("late", assistant("Done.")));
        let (store, window_state, _) = seed_chat(cx, timeline);
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(1_900.)));
        draw_settled(cx);
        let header = cx
            .debug_bounds("worklog-header-0-command-0")
            .expect("collapsed work log");
        cx.simulate_click(header.center(), Modifiers::default());
        draw_settled(cx);

        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        list.scroll_to(ListOffset::default());
        draw_settled(cx);
        list.set_follow_mode(FollowMode::Tail);
        draw_settled(cx);

        let line = cx
            .debug_bounds("markdown-code-line-0")
            .expect("the message on screen is rendered as Markdown");
        assert!(
            line.intersects(&list.viewport_bounds()),
            "{line:?} is not on screen"
        );
    }

    #[gpui::test]
    fn manually_collapsed_activity_stays_closed_while_new_activity_opens(cx: &mut TestAppContext) {
        use gpui::{px, size};

        let command = ItemContent::CommandExecution {
            command: "echo hello".into(),
            output: "hello\n".into(),
            exit_code: Some(0),
            status: ItemStatus::Completed,
        };
        let file_edit = ItemContent::FileChange {
            changes: vec![agent::FileChange {
                path: "src/lib.rs".into(),
                kind: agent::FileChangeKind::Modify,
                diff: Some("@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n".into()),
            }],
            status: ItemStatus::Completed,
        };
        for (item, selector, detail_selector) in [
            (command, "activity-row-first", "activity-detail"),
            (file_edit, "file-edit-row", "file-edit-diff"),
        ] {
            let mut timeline = Timeline::default();
            // Render a live turn without starting the perpetual timeline ticker.
            timeline.turns = vec![TurnMeta {
                running: true,
                ..TurnMeta::default()
            }];
            timeline.entries = vec![
                entry("user", user_item("go")),
                entry("first", EntryContent::Item(item.clone())),
            ];
            let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
            cx.update(|cx| cx.set_reduce_motion(true));
            let (_, cx) = cx.add_window_view(|window, cx| {
                ChatView::new(store.clone(), window_state, window, cx)
            });
            cx.simulate_resize(size(px(1_024.), px(900.)));
            draw_settled(cx);

            assert!(cx.debug_bounds(detail_selector).is_some());
            let header = cx.debug_bounds(selector).expect("activity toggle");
            cx.simulate_click(header.center(), gpui::Modifiers::default());
            draw_settled(cx);
            assert!(
                cx.debug_bounds(detail_selector).is_none(),
                "{selector} did not close"
            );

            let mut updated = item.clone();
            match &mut updated {
                ItemContent::CommandExecution { output, .. } => output.push_str("more output\n"),
                ItemContent::FileChange { changes, .. } => {
                    changes[0]
                        .diff
                        .as_mut()
                        .unwrap()
                        .push_str("+fn another() {}\n");
                }
                _ => unreachable!(),
            }
            timeline.entries[1] = entry("first", EntryContent::Item(updated));
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                cx.notify();
            });
            draw_settled(cx);
            assert!(
                cx.debug_bounds(detail_selector).is_none(),
                "{selector} reopened on an activity update"
            );

            let header = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(header.center(), gpui::Modifiers::default());
            draw_settled(cx);
            assert!(cx.debug_bounds(detail_selector).is_some());
            let header = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(header.center(), gpui::Modifiers::default());
            draw_settled(cx);
            assert!(cx.debug_bounds(detail_selector).is_none());

            timeline
                .entries
                .push(entry("next", EntryContent::Item(item)));
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                cx.notify();
            });
            draw_settled(cx);
            assert!(
                cx.debug_bounds(detail_selector).is_some(),
                "the next {selector} did not open automatically"
            );
        }
    }

    /// A running command's panel shows the screens the host renders at the
    /// panel's width, pushed as the output grows, over one subscription per
    /// width that collapsing the panel or leaving the thread releases.
    #[gpui::test]
    fn a_running_command_shows_the_screens_the_host_pushes_for_its_width(cx: &mut TestAppContext) {
        use crate::terminal_drawer::TERMINAL_CELL_HEIGHT;
        use gpui::{VisualTestContext, px, size};
        use tcode_protocol::{Topic, terminal::TerminalColor};

        let (host, session_id, _) = seeded_chat_host(cx, Timeline::default(), false);
        cx.update(|cx| cx.set_reduce_motion(true));
        let seeded_id = session_id.clone();
        let other_id = smol::block_on(host.update_state_for_test(move |state, cx| {
            state.residents.live.get_mut(&seeded_id).unwrap().timeline = Timeline::default();
            for (ts, event) in [
                agent::AgentEvent::TurnStarted {
                    turn_id: "turn".into(),
                },
                agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: "user".into(),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: "build it".into(),
                        context_len: None,
                        attachments: Vec::new(),
                    },
                }),
                agent::AgentEvent::ItemStarted(agent::ThreadItem {
                    id: "run".into(),
                    parent_item_id: None,
                    content: ItemContent::CommandExecution {
                        command: "make".into(),
                        output: String::new(),
                        exit_code: None,
                        status: ItemStatus::InProgress,
                    },
                }),
            ]
            .iter()
            .enumerate()
            {
                state.record_event_for_replica_test(&seeded_id, ts as u64, event, cx);
            }
            let other = state.start_draft("other-thread".into(), std::env::temp_dir(), cx);
            let active = state.residents.live.get_mut(&other).expect("second draft");
            active.draft = false;
            let meta = active.meta.clone();
            state.sessions.push(meta);
            other
        }))
        .expect("record the running command");
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
        store.update(cx, |store, cx| store.select_session(session_id.clone(), cx));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, _| !store.chat_loading())
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let (_, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(900.)));

        let link = driver.link.clone();
        let output_topics = || {
            link.subscribed_topics()
                .into_iter()
                .filter_map(|topic| match topic {
                    Topic::CommandOutput {
                        session_id,
                        item_id,
                        cols,
                    } => Some((session_id, item_id, cols)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let watched = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store
                    .command_output_for(store.active_session_id().as_deref(), "run")
                    .and_then(|output| output.watched())
            })
        };
        let frame = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store
                    .command_output_for(store.active_session_id().as_deref(), "run")
                    .and_then(|output| output.frame().cloned())
            })
        };
        let mut settle = |cx: &mut VisualTestContext,
                          description: &str,
                          ready: &dyn Fn(&mut VisualTestContext) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                driver.step(cx, &store);
                draw_settled(cx);
                if ready(cx) {
                    return;
                }
                assert!(Instant::now() < deadline, "waiting for {description}");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        // Nothing drives this turn on the host, so its Work Log reads as
        // finished and folded: open it and the command, as a reader would.
        settle(cx, "the Work Log", &|cx| {
            cx.debug_bounds("worklog-header-0-run").is_some()
        });
        let header = cx.debug_bounds("worklog-header-0-run").unwrap();
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        settle(cx, "the command row", &|cx| {
            cx.debug_bounds("activity-row-run").is_some()
        });
        if cx.debug_bounds("activity-detail").is_none() {
            let row = cx.debug_bounds("activity-row-run").unwrap();
            cx.simulate_click(row.center(), gpui::Modifiers::default());
        }
        settle(cx, "the panel to measure its width", &|cx| {
            watched(cx).is_some()
        });
        let cols = watched(cx).unwrap();
        let subscribed = vec![(session_id.clone(), "run".to_string(), cols)];
        assert_eq!(output_topics(), subscribed);

        let mut long_line = "x".repeat(usize::from(cols));
        long_line.push_str("yz");
        let growth = [
            "10%\r".to_string(),
            "50%\r".into(),
            "100%\n".into(),
            format!("{long_line}\n"),
            "\u{1b}[32mok\u{1b}[0m\n".into(),
        ];
        for (index, text) in growth.into_iter().enumerate() {
            let seeded_id = session_id.clone();
            smol::block_on(host.update_state_for_test(move |state, cx| {
                state.record_event_for_replica_test(
                    &seeded_id,
                    10 + index as u64,
                    &agent::AgentEvent::Delta {
                        item_id: "run".into(),
                        kind: agent::DeltaKind::CommandOutput,
                        text,
                    },
                    cx,
                );
            }))
            .expect("grow the command output");
            settle(cx, "one host turn", &|_| true);
            assert_eq!(output_topics(), subscribed, "growth re-subscribed");
        }
        settle(cx, "the screen after the last growth", &|cx| {
            frame(cx).is_some_and(|frame| frame.rows == 4)
        });
        let screen = frame(cx).unwrap();
        let text = |row: usize| {
            screen.visible[row]
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        };
        assert_eq!(screen.cols, cols);
        assert_eq!(text(0), "100%", "the progress bar was not redrawn in place");
        assert_eq!(text(1), "x".repeat(usize::from(cols)));
        assert_eq!(text(2), "yz");
        assert_eq!(text(3), "ok");
        assert_eq!(
            screen.style(&screen.visible[3].cells[0]).fg,
            TerminalColor::Indexed(2)
        );
        assert_eq!(output_topics(), subscribed);
        // The panel is as tall as the host's screen, not as the raw output's
        // three lines.
        let output = cx
            .debug_bounds("command-output-run")
            .expect("the output grid");
        assert_eq!(output.size.height, px(4. * TERMINAL_CELL_HEIGHT));

        cx.simulate_resize(size(px(600.), px(900.)));
        settle(cx, "the narrower screen", &|cx| {
            watched(cx).is_some_and(|watched| watched < cols)
                && frame(cx).is_some_and(|frame| Some(frame.cols) == watched(cx))
        });
        let narrower = watched(cx).unwrap();
        assert_eq!(
            output_topics(),
            vec![(session_id.clone(), "run".to_string(), narrower)]
        );

        let header = cx.debug_bounds("activity-row-run").expect("command row");
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        settle(cx, "the collapsed panel to release its screen", &|_| {
            output_topics().is_empty()
        });
        let header = cx.debug_bounds("activity-row-run").expect("command row");
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        settle(cx, "the reopened panel", &|_| output_topics().len() == 1);

        store.update(cx, |store, cx| store.select_session(other_id, cx));
        settle(cx, "leaving the thread", &|_| output_topics().is_empty());
        driver.shutdown(cx, &store, &host);
    }

    /// A `tcode_control` anchor opens its thread at the turn of the entry it
    /// names: here the user message of a turn above the window the thread
    /// opens with, in a log whose streamed replies the host merges on the
    /// wire, so no held record index stands for a log position.
    #[gpui::test]
    fn an_anchor_link_opens_its_thread_at_the_anchored_turn(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        const TURNS: u64 = 12;
        const DELTAS: u64 = 40;
        let (host, session_id, _) = seeded_chat_host(cx, Timeline::default(), false);
        let seeded_id = session_id.clone();
        // Turn `t` takes the rows from `t * (DELTAS + 5)`; its user message is
        // the second, and the position a `tcode_control` anchor names it by.
        let rows = smol::block_on(host.update_state_for_test(move |state, cx| {
            let mut events = Vec::new();
            for turn in 0..TURNS {
                let answer = format!("answer-{turn}");
                events.push(agent::AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                });
                events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: format!("user-{turn}"),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: format!("Question {turn}"),
                        context_len: None,
                        attachments: Vec::new(),
                    },
                }));
                events.push(agent::AgentEvent::ItemStarted(agent::ThreadItem {
                    id: answer.clone(),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: String::new(),
                        summarized: false,
                    },
                }));
                events.extend((0..DELTAS).map(|_| agent::AgentEvent::Delta {
                    item_id: answer.clone(),
                    kind: agent::DeltaKind::AssistantText,
                    text: "word ".into(),
                }));
                events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: answer,
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: "word ".repeat(DELTAS as usize),
                        summarized: false,
                    },
                }));
                events.push(agent::AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                });
            }
            for (ts, event) in events.iter().enumerate() {
                state.record_event_for_replica_test(&seeded_id, ts as u64, event, cx);
            }
            events.len() as u64
        }))
        .expect("record the streamed turns");
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
        let link = driver.link.clone();
        let page = driver.await_with_events(
            cx,
            &store,
            link.query(tcode_protocol::Query::SessionHistoryPage {
                session_id: session_id.clone(),
                before: u64::MAX,
                limit: 1,
            }),
        );
        assert!(
            matches!(
                page,
                Ok(tcode_protocol::QueryResponse::SessionHistoryPage { end, .. }) if end == rows
            ),
            "the recorded events are the log's only rows: {page:?}"
        );
        let window_state = cx.new(|_| WindowState::new(false));
        let (chat, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        // The shell binds its pane to the thread the link opens.
        chat.update(cx, |chat, cx| {
            chat.set_session(Some(session_id.clone()), cx)
        });

        let opened = |cx: &mut VisualTestContext| {
            let view = cx.update(|window, _| window.root::<ChatView>().flatten())?;
            let (row, highlighted) = view.read_with(cx, |chat, _| {
                let row = chat.list_state.logical_scroll_top().item_ix;
                (
                    chat.rows.get(row).map(|row| row.turn),
                    chat.highlighted_turn,
                )
            });
            let turn = row.filter(|turn| highlighted == Some(*turn))?;
            store
                .read_with(cx, |store, _| {
                    store.with_active_timeline(|timeline| {
                        timeline
                            .entries
                            .iter()
                            .find(|entry| {
                                entry.turn == turn
                                    && matches!(
                                        entry.content,
                                        EntryContent::Item(ItemContent::UserMessage { .. })
                                    )
                            })
                            .map(|entry| entry.id.clone())
                    })
                })
                .flatten()
        };
        // Turn 1 is above the opening window; turn 9 is inside it, in the
        // thread already open.
        for turn in [1, 9] {
            let expected = format!("user-{turn}");
            store.update(cx, |store, cx| {
                let position = turn * (DELTAS + 5) + 1;
                store.select_session_at_position(session_id.clone(), position, cx);
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while opened(cx).as_deref() != Some(expected.as_str()) {
                assert!(Instant::now() < deadline, "waiting for {expected}");
                driver.step(cx, &store);
                draw_settled(cx);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        driver.shutdown(cx, &store, &host);
    }

    #[gpui::test]
    fn a_plan_is_decided_on_its_block_and_answered_in_words_from_the_composer(
        cx: &mut TestAppContext,
    ) {
        use gpui::{Modifiers, VisualTestContext, px, size};
        use tcode_core::project::SessionMeta;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let root = crate::store::tests::scratch_root("chat-plan-decision-test");
        let session_store =
            tcode_services::store::SessionStore::open_at(root.clone()).expect("test session store");
        let meta = SessionMeta::new(agent::ProviderKind::ClaudeCode, root.clone(), None);
        let session_id = meta.id.clone();
        session_store.upsert_meta(&meta).expect("persist session");
        let host = tcode_runtime::pipe::spawn_host(
            session_store,
            tcode_runtime::pipe::HostServices::default(),
        )
        .expect("spawn test host");
        let scripted = tcode_runtime::app::scripted_provider(agent::ProviderKind::ClaudeCode);
        let (provider_events, provider_commands) =
            (scripted.events.clone(), scripted.commands.clone());
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.set_provider_launcher_for_test(scripted.launcher)
        }))
        .expect("script the provider");
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
        store.update(cx, |store, cx| store.select_session(session_id.clone(), cx));
        store.update(cx, |store, _| {
            store.send_turn_for(session_id.clone(), "Plan it first".into(), Vec::new())
        });
        driver.wait_until(cx, &store, "the turn reaches the provider", |_| {
            matches!(
                provider_commands.try_recv(),
                Ok(agent::SessionCommand::SendTurn { .. })
            )
        });

        let window_state = cx.new(|_| WindowState::new(false));
        let (chat, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(800.)));
        chat.update(cx, |chat, cx| {
            chat.set_session(Some(session_id.clone()), cx)
        });
        let propose = |id: &str, request: &str| {
            let markdown = format!("# Plan {id}\n\n1. Write hello.txt");
            for event in [
                agent::AgentEvent::ItemStarted(agent::ThreadItem {
                    id: id.into(),
                    parent_item_id: None,
                    content: ItemContent::Plan {
                        markdown: markdown.clone(),
                    },
                }),
                agent::AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                    id: request.into(),
                    turn_id: None,
                    parent_item_id: None,
                    kind: agent::ApprovalKind::Plan {
                        item_id: id.into(),
                        markdown,
                    },
                    options: vec![
                        agent::ApprovalOption {
                            id: "allow".into(),
                            label: "Approve".into(),
                            kind: agent::ApprovalOptionKind::AllowOnce,
                        },
                        agent::ApprovalOption {
                            id: "deny".into(),
                            label: "Keep planning".into(),
                            kind: agent::ApprovalOptionKind::RejectOnce,
                        },
                    ],
                }),
            ] {
                provider_events.try_send(event).expect("provider event");
            }
        };
        let decided = |driver: &mut crate::store::tests::LocalHostDriver,
                       cx: &mut VisualTestContext,
                       what: &str| {
            let mut decision = None;
            let deadline = Instant::now() + Duration::from_secs(5);
            while decision.is_none() {
                assert!(Instant::now() < deadline, "waiting for {what}");
                driver.step(cx, &store);
                draw_settled(cx);
                if let Ok(agent::SessionCommand::RespondApproval {
                    request_id,
                    decision: answer,
                }) = provider_commands.try_recv()
                {
                    decision = Some((request_id, answer));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            decision.unwrap()
        };
        let shown = |driver: &mut crate::store::tests::LocalHostDriver,
                     cx: &mut VisualTestContext| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while cx.debug_bounds("plan-decision").is_none() {
                assert!(Instant::now() < deadline, "waiting for the plan's decision");
                driver.step(cx, &store);
                draw_settled(cx);
                std::thread::sleep(Duration::from_millis(1));
            }
        };

        propose("plan-1", "decide-1");
        shown(&mut driver, cx);
        assert!(
            cx.debug_bounds("approval-header").is_none(),
            "a plan is decided on its block, not in the composer"
        );
        let approve = cx
            .debug_bounds("plan-option-allow")
            .expect("approve button");
        cx.simulate_click(approve.center(), Modifiers::default());
        assert_eq!(
            decided(&mut driver, cx, "the approval"),
            (
                "decide-1".to_string(),
                agent::ApprovalDecision::Option("allow".into())
            )
        );
        provider_events
            .try_send(agent::AgentEvent::ApprovalResolved {
                request_id: "decide-1".into(),
                decision: agent::ApprovalDecision::Option("allow".into()),
            })
            .expect("provider event");

        propose("plan-2", "decide-2");
        shown(&mut driver, cx);
        chat.update_in(cx, |chat, window, cx| chat.focus_composer(window, cx));
        cx.simulate_input("Split step one");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            decided(&mut driver, cx, "the feedback"),
            (
                "decide-2".to_string(),
                agent::ApprovalDecision::Feedback("Split step one".into())
            )
        );
        driver.shutdown(cx, &store, &host);
    }

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
    fn timeline_scrollbar_drag_pauses_tail_following(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Render,
            Window, point, px,
        };

        struct ChatRoot(Entity<ChatView>);
        impl Render for ChatRoot {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }

        // A phone and a tablet: the shell's layout rule, applied by hand since
        // no shell is mounted here.
        cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, true));
        for (width, height) in [(393., 852.), (1024., 768.)] {
            let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(30));
            let (view, cx) = cx.add_window_view(|window, cx| {
                ChatRoot(cx.new(|cx| ChatView::new(store, window_state.clone(), window, cx)))
            });
            cx.simulate_resize(gpui::size(px(width), px(height)));
            cx.update(|window, cx| {
                let compact = crate::window_seam::window_is_compact(window, cx);
                window_state.update(cx, |state, _| state.compact = compact);
            });
            cx.run_until_parked();
            draw_settled(cx);
            let list = view.read_with(cx, |view, cx| view.0.read(cx).list_state.clone());
            let viewport = list.viewport_bounds();
            let tail = list.scroll_px_offset_for_scrollbar().y;
            assert!(list.is_following_tail());
            assert!(list.max_offset_for_scrollbar().y > viewport.size.height);

            let thumb = point(viewport.right() - px(5.), viewport.bottom() - px(8.));
            cx.simulate_mouse_move(thumb, None, Modifiers::default());
            draw_settled(cx);
            cx.simulate_event(MouseDownEvent {
                position: thumb,
                button: MouseButton::Left,
                ..Default::default()
            });
            let target = point(thumb.x, viewport.center().y);
            cx.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
            cx.simulate_event(MouseUpEvent {
                position: target,
                button: MouseButton::Left,
                ..Default::default()
            });
            draw_settled(cx);

            assert!(
                list.scroll_px_offset_for_scrollbar().y > tail + viewport.size.height,
                "dragging the visible scrollbar must scroll the conversation at width {width}"
            );
            assert!(!list.is_following_tail());

            // A finger on the thumb drags the scrollbar too, instead of
            // panning the conversation underneath it.
            list.set_follow_mode(gpui::FollowMode::Tail);
            list.scroll_to_end();
            draw_settled(cx);
            assert!(list.is_following_tail());
            cx.simulate_mouse_move(thumb, None, Modifiers::default());
            draw_settled(cx);
            for (phase, position) in [
                (gpui::TouchPhase::Started, thumb),
                (gpui::TouchPhase::Moved, target),
                (gpui::TouchPhase::Ended, target),
            ] {
                cx.simulate_event(gpui::TouchDragEvent {
                    phase,
                    start_position: thumb,
                    position,
                });
            }
            draw_settled(cx);
            assert!(
                list.scroll_px_offset_for_scrollbar().y > tail + viewport.size.height,
                "touch-dragging the scrollbar must scroll the conversation at width {width}"
            );
            assert!(!list.is_following_tail());
        }
    }

    #[gpui::test]
    fn touch_pan_pauses_tail_following_and_shows_the_pill(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, PlatformInput, Render, TouchEvent, TouchId, TouchPhase, Window,
            point, px,
        };
        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(30));
        window_state.update(cx, |state, _| state.compact = true);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| ChatView::new(store, window_state, window, cx)))
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        cx.run_until_parked();
        draw_settled(cx);
        let list = root.read_with(cx, |root, cx| root.0.read(cx).list_state.clone());
        let viewport = list.viewport_bounds();
        let height = viewport.size.height;
        assert!(list.is_following_tail());
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        let touch = |phase, y: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                window.dispatch_event(
                    PlatformInput::Touch(TouchEvent {
                        id: TouchId(1),
                        phase,
                        position: point(viewport.center().x, y),
                        predicted_position: None,
                        force: None,
                    }),
                    cx,
                );
            });
            draw_settled(cx);
        };

        // The finger moves down: earlier content comes into view and following
        // pauses on the first step. Cancel avoids release momentum.
        let top = viewport.top() + px(10.);
        touch(TouchPhase::Started, top, cx);
        touch(TouchPhase::Moved, top + px(40.), cx);
        assert!(!list.is_following_tail(), "the first step pauses following");
        touch(TouchPhase::Moved, top + height * 3., cx);
        touch(TouchPhase::Cancelled, top + height * 3., cx);
        assert!(!list.is_following_tail());
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "the pill follows the list geometry after a touch pan"
        );

        // Panning back to the end resumes following and hides the pill.
        let bottom = viewport.bottom() - px(10.);
        touch(TouchPhase::Started, bottom, cx);
        touch(TouchPhase::Moved, bottom - height * 4., cx);
        touch(TouchPhase::Cancelled, bottom - height * 4., cx);
        assert!(list.is_following_tail());
        assert!(cx.debug_bounds("scroll-to-end").is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    #[gpui::test]
    fn timeline_bounces_at_its_edges_without_moving_the_list(cx: &mut TestAppContext) {
        use gpui::{
            Context, IntoElement, PlatformInput, Render, TouchEvent, TouchId, TouchPhase, Window,
            point, px,
        };
        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(30));
        window_state.update(cx, |state, _| state.compact = true);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| ChatView::new(store, window_state, window, cx)))
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw_settled(cx);
        let list = root.read_with(cx, |root, cx| root.0.read(cx).list_state.clone());
        assert!(list.max_offset_for_scrollbar().y > px(0.));
        for at_end in [false, true] {
            draw_settled(cx);
            list.set_follow_mode(gpui::FollowMode::Normal);
            if at_end {
                list.scroll_to_end();
            } else {
                list.scroll_to(gpui::ListOffset {
                    item_ix: 0,
                    offset_in_item: px(0.),
                });
            }
            draw_settled(cx);
            let resting = list.viewport_bounds();
            let offset = list.scroll_px_offset_for_scrollbar().y;
            let row_selector = if at_end {
                "timeline-row-89"
            } else {
                "timeline-row-0"
            };
            let row = cx.debug_bounds(row_selector).expect("edge row");
            let composer = cx.debug_bounds("chat-composer").expect("composer");
            let touch = |phase, y: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
                cx.update(|window, cx| {
                    window.dispatch_event(
                        PlatformInput::Touch(TouchEvent {
                            id: TouchId(1),
                            phase,
                            position: point(resting.center().x, y),
                            predicted_position: None,
                            force: None,
                        }),
                        cx,
                    );
                    let _ = window.draw(cx);
                });
            };
            let direction = if at_end { -1. } else { 1. };
            let start = resting.center().y;
            touch(TouchPhase::Started, start, cx);
            touch(TouchPhase::Moved, start + px(200. * direction), cx);
            let stretched = list.viewport_bounds();
            assert!(
                (stretched.top() - resting.top()) * direction > px(0.),
                "an outward pan stretches the timeline: {stretched:?} vs {resting:?}"
            );
            assert_eq!(stretched.size, resting.size);
            assert_eq!(
                list.scroll_px_offset_for_scrollbar().y,
                offset,
                "the stretch is a displacement, not a scroll"
            );
            let stretched_row = cx.debug_bounds(row_selector).expect("edge row");
            assert_eq!(
                stretched_row.top() - row.top(),
                stretched.top() - resting.top()
            );
            assert_eq!(
                cx.debug_bounds("chat-composer").expect("composer"),
                composer,
                "fixed chrome stays outside the stretch"
            );
            touch(TouchPhase::Cancelled, start + px(200. * direction), cx);
            assert_eq!(list.scroll_px_offset_for_scrollbar().y, offset);
        }
    }

    #[gpui::test]
    fn a_drag_right_after_a_flick_scrolls_the_timeline(cx: &mut TestAppContext) {
        use gpui::{point, px};
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(30));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw_settled(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        list.set_follow_mode(gpui::FollowMode::Normal);
        list.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
        draw_settled(cx);
        let resting = list.viewport_bounds();
        // One trackpad packet per displayed frame.
        let packet = |phase, dy: f32, cx: &mut gpui::VisualTestContext| {
            cx.simulate_event(gpui::ScrollEvent {
                position: resting.center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(dy))),
                touch_phase: phase,
                ..Default::default()
            });
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.run_until_parked();
            cx.update(|window, cx| window.simulate_next_frame(cx));
        };
        packet(gpui::TouchPhase::Started, 0., cx);
        for _ in 0..6 {
            packet(gpui::TouchPhase::Moved, -70., cx);
        }
        packet(gpui::TouchPhase::Ended, 0., cx);
        let flicked = list.scroll_px_offset_for_scrollbar().y;
        assert!(flicked < px(0.) && flicked > -list.max_offset_for_scrollbar().y);
        packet(gpui::TouchPhase::Started, 0., cx);
        for _ in 0..4 {
            packet(gpui::TouchPhase::Moved, -50., cx);
        }
        assert_eq!(
            list.scroll_px_offset_for_scrollbar().y,
            flicked - px(200.),
            "the drag scrolls the conversation"
        );
        assert_eq!(list.viewport_bounds(), resting, "nothing is displaced");
    }

    #[gpui::test]
    fn timeline_tail_has_one_composer_gap(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, Render, Window, px};
        struct ChatRoot(Entity<ChatView>);
        impl Render for ChatRoot {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }

        for (mobile, running) in [(false, false), (false, true), (true, false), (true, true)] {
            for (width, height) in [(393., 852.), (1024., 768.)] {
                cx.update(|cx| crate::window_seam::override_mobile_for_test(cx, mobile));
                let expected_gap = if mobile {
                    0.
                } else {
                    super::timeline::TIMELINE_INSET
                };
                let mut timeline = synthetic_markdown_timeline(30);
                timeline.turn_running = running;
                timeline.turns.last_mut().unwrap().running = running;
                let (store, window_state, _) = seed_chat(cx, timeline);
                cx.update(|cx| cx.set_reduce_motion(true));
                let (view, cx) = cx.add_window_view(|window, cx| {
                    ChatRoot(cx.new(|cx| ChatView::new(store, window_state.clone(), window, cx)))
                });
                cx.simulate_resize(gpui::size(px(width), px(height)));
                // The shell's layout rule, applied by hand since no shell is
                // mounted here: only the mobile phone is compact.
                cx.update(|window, cx| {
                    let compact = crate::window_seam::window_is_compact(window, cx);
                    window_state.update(cx, |state, _| state.compact = compact);
                });
                let list = view.read_with(cx, |view, cx| view.0.read(cx).list_state.clone());
                list.scroll_to_end();
                cx.run_until_parked();
                draw_settled(cx);
                let last = cx
                    .debug_bounds("timeline-row-89")
                    .expect("last timeline row");
                assert!(
                    f32::from(last.bottom() - list.viewport_bounds().bottom()).abs() <= 1.,
                    "the gap must stay outside the list's viewport"
                );
                let composer = cx.debug_bounds("chat-composer").expect("composer");
                let gap = f32::from(composer.top() - last.bottom());
                assert!(
                    (gap - expected_gap).abs() <= 1.,
                    "{width}×{height}, running={running}, mobile={mobile}: expected {expected_gap}px gap, got {gap}px"
                );
                let card = cx
                    .debug_bounds("composer-card")
                    .expect("visible composer card");
                assert_eq!(card.top(), composer.top(), "no extra composer top inset");
                if running {
                    let status = cx
                        .debug_bounds("working-status-29")
                        .expect("running status");
                    // The row's own edge padding sits below the status.
                    assert_eq!(
                        status.bottom(),
                        last.bottom() - px(super::TIMELINE_EDGE_PADDING)
                    );
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

    #[gpui::test]
    fn wheel_event_between_a_prepend_and_its_frame_keeps_the_reading_position(
        cx: &mut TestAppContext,
    ) {
        use gpui::{Context, IntoElement, Render, VisualTestContext, Window, point, px};

        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let full = synthetic_markdown_timeline(120);
        let mut tail = full.clone();
        tail.turns.drain(..20);
        tail.entries.retain(|entry| entry.turn >= 20);
        for entry in &mut tail.entries {
            Arc::make_mut(entry).turn -= 20;
        }
        let (store, window_state, session_id) = seed_chat(cx, tail);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| ChatView::new(store.clone(), window_state, window, cx)))
        });
        let view = root.read_with(cx, |root, _| root.0.clone());
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw_settled(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let height = list.viewport_bounds().size.height;
        let scroll = |distance, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollEvent {
                position: list.viewport_bounds().center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), distance)),
                touch_phase: gpui::TouchPhase::Moved,
                ..Default::default()
            });
        };
        for _ in 0..12 {
            scroll(height / 4., cx);
            draw_settled(cx);
        }
        assert!(!list.is_following_tail());
        let before = list.logical_scroll_top();
        let before_px = list.scroll_px_offset_for_scrollbar().y;

        // An earlier history page arrives from the host between two frames,
        // while a trackpad pan is still delivering packets. The test app
        // redraws dirty windows whenever effects flush, so the store update,
        // its observers and the packet share one flush: the deferred packet
        // runs after the observers and before the frame, as on a display link.
        let wheel = gpui::ScrollEvent {
            position: list.viewport_bounds().center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(40.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        };
        cx.update(|window, cx| {
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id, full, cx);
                cx.notify();
            });
            window.defer(cx, move |window, cx| {
                window.dispatch_event(gpui::InputEvent::to_platform_input(wheel), cx);
            });
        });
        draw_settled(cx);

        // The row the reader was on sits 60 rows lower and exactly 40px
        // further down the viewport: both the page and the pan applied.
        let row_top = list
            .bounds_for_item(before.item_ix + 60)
            .expect("the reader's row stays on screen")
            .top();
        assert_eq!(
            row_top - list.viewport_bounds().top(),
            px(40.) - before.offset_in_item,
            "a 40px pan and a 60-row page both apply (offset {:?} -> {:?})",
            before.offset_in_item,
            list.logical_scroll_top().offset_in_item
        );
        assert_eq!(list.scroll_px_offset_for_scrollbar().y, before_px + px(40.));
    }

    #[gpui::test]
    fn late_updates_preserve_user_scroll_intent(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, Render, Window, px};
        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        for phase in [gpui::TouchPhase::Started, gpui::TouchPhase::Moved] {
            let mut timeline = synthetic_markdown_timeline(30);
            timeline.mark_idle();
            let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
            let (root, cx) = cx.add_window_view(|window, cx| {
                TouchChat(cx.new(|cx| ChatView::new(store.clone(), window_state, window, cx)))
            });
            cx.simulate_resize(gpui::size(px(393.), px(852.)));
            draw_settled(cx);
            let list = root.read_with(cx, |root, cx| root.0.read(cx).list_state.clone());
            let bottom = list.scroll_px_offset_for_scrollbar().y;
            let scroll = |delta, cx: &mut gpui::VisualTestContext| {
                cx.simulate_event(gpui::ScrollEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), delta)),
                    touch_phase: phase,
                    ..Default::default()
                });
            };
            scroll(px(120.), cx);
            draw_settled(cx);
            assert_eq!(list.scroll_px_offset_for_scrollbar().y, bottom + px(120.));
            assert!(!list.is_following_tail());
            assert!(cx.debug_bounds("scroll-to-end").is_none());
            let anchor = list.logical_scroll_top();
            for update in 1..=30 {
                cx.executor().advance_clock(Duration::from_millis(100));
                Arc::make_mut(timeline.entries.last_mut().unwrap()).content =
                    assistant(&"Late output frame.\n\n".repeat(update));
                store.update(cx, |store, cx| {
                    store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx);
                    cx.notify();
                });
                draw_settled(cx);
                assert!(!list.is_following_tail(), "late update {update}, {phase:?}");
                assert_eq!(list.logical_scroll_top().item_ix, anchor.item_ix);
                assert_eq!(
                    list.logical_scroll_top().offset_in_item,
                    anchor.offset_in_item
                );
            }
            scroll(px(-100000.), cx);
            draw_settled(cx);
            assert!(
                list.is_following_tail(),
                "return to bottom {phase:?}: offset {:?}, max {:?}, anchor {:?}",
                list.scroll_px_offset_for_scrollbar(),
                list.max_offset_for_scrollbar(),
                list.logical_scroll_top()
            );
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id, synthetic_markdown_timeline(31), cx);
                cx.notify();
            });
            draw_settled(cx);
            assert!(list.is_following_tail());
            assert_eq!(list.logical_scroll_top().item_ix, list.item_count());
        }
    }

    /// A reader following the tail stays pinned to the bottom while the last
    /// message grows in place: each delta the host streams into it lands
    /// with the message's end on the timeline's bottom edge.
    #[gpui::test]
    fn a_following_reader_stays_pinned_while_the_last_message_grows(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        let (host, session_id, _) = seeded_chat_host(cx, Timeline::default(), false);
        // The streaming turn's working indicator rests.
        cx.update(|cx| cx.set_reduce_motion(true));
        let ts = std::cell::Cell::new(0);
        let record = |events: Vec<agent::AgentEvent>| {
            let session_id = session_id.clone();
            let from = ts.replace(ts.get() + events.len() as u64);
            smol::block_on(host.update_state_for_test(move |state, cx| {
                for (offset, event) in events.iter().enumerate() {
                    state.record_event_for_replica_test(
                        &session_id,
                        from + offset as u64,
                        event,
                        cx,
                    );
                }
            }))
            .expect("record on the host");
        };
        let user = |turn: usize| {
            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                id: format!("user-{turn}"),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: format!("Question {turn}"),
                    context_len: None,
                    attachments: Vec::new(),
                },
            })
        };
        let seeded_id = session_id.clone();
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.residents.live.get_mut(&seeded_id).unwrap().timeline = Timeline::default();
        }))
        .expect("clear the seeded timeline");
        // Earlier turns fill more than a screen above the streaming one.
        let mut events = Vec::new();
        for turn in 0..4 {
            events.extend([
                agent::AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                },
                user(turn),
                agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: format!("answer-{turn}"),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: "An earlier answer.\n\n".repeat(8),
                        summarized: false,
                    },
                }),
                agent::AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                },
            ]);
        }
        events.extend([
            agent::AgentEvent::TurnStarted {
                turn_id: "4".into(),
            },
            user(4),
            agent::AgentEvent::ItemStarted(agent::ThreadItem {
                id: "streaming".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: String::new(),
                    summarized: false,
                },
            }),
        ]);
        record(events);

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
        store.update(cx, |store, cx| store.select_session(session_id.clone(), cx));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, _| !store.chat_loading())
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(list.is_following_tail());
        assert!(
            list.max_offset_for_scrollbar().y > list.viewport_bounds().size.height,
            "the thread scrolls"
        );

        let streamed = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| {
                store
                    .with_active_timeline(|timeline| {
                        timeline
                            .entries
                            .last()
                            .and_then(|entry| match &entry.content {
                                EntryContent::Item(ItemContent::AssistantMessage {
                                    text, ..
                                }) => Some(text.len()),
                                _ => None,
                            })
                    })
                    .flatten()
                    .unwrap_or(0)
            })
        };
        let mut heights = Vec::new();
        for delta in 0..12 {
            let before = streamed(cx);
            record(vec![agent::AgentEvent::Delta {
                item_id: "streaming".into(),
                kind: agent::DeltaKind::AssistantText,
                text: format!("Paragraph {delta} of the streamed answer.\n\n"),
            }]);
            let deadline = Instant::now() + Duration::from_secs(5);
            while streamed(cx) == before {
                assert!(Instant::now() < deadline, "waiting for delta {delta}");
                driver.step(cx, &store);
                std::thread::sleep(Duration::from_millis(1));
            }
            draw_settled(cx);

            assert!(list.is_following_tail(), "delta {delta} paused following");
            let last = cx
                .debug_bounds(format!("timeline-row-{}", list.item_count() - 1).leak())
                .expect("the streaming message is on screen");
            let bottom = list.viewport_bounds().bottom();
            assert!(
                (last.bottom() - bottom).abs() <= px(1.),
                "delta {delta}: the message ends at {:?}, the timeline at {bottom:?}",
                last.bottom()
            );
            heights.push(last.size.height);
        }
        assert!(
            heights.windows(2).all(|pair| pair[1] > pair[0]),
            "the message grew with every delta: {heights:?}"
        );
        driver.shutdown(cx, &store, &host);
    }

    #[gpui::test]
    fn scroll_pill_over_inline_image_does_not_open_lightbox(cx: &mut TestAppContext) {
        use gpui::{Modifiers, MouseButton, ScrollDelta, ScrollEvent, TouchPhase, point, px};

        let path = std::env::temp_dir().join(format!(
            "tcode-pill-image-{}-{}.png",
            std::process::id(),
            NEXT_RESIDENCY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        image::RgbaImage::from_pixel(400, 1000, image::Rgba([40, 80, 120, 255]))
            .save(&path)
            .unwrap();
        let mut timeline = synthetic_markdown_timeline(30);
        Arc::make_mut(timeline.entries.last_mut().unwrap()).content = assistant(&format!(
            "![shot]({})\n\n{}",
            path.display(),
            "Text after the screenshot.\n\n".repeat(80)
        ));
        let (host, session_id, timeline) = seeded_chat_host(cx, timeline, false);
        let seeded_id = session_id.clone();
        smol::block_on(host.update_state_for_test(move |state, cx| {
            state.residents.live.get_mut(&seeded_id).unwrap().timeline = Timeline::default();
            let mut ts = 0;
            for turn in 0..timeline.turns.len() {
                let mut events = vec![agent::AgentEvent::TurnStarted {
                    turn_id: turn.to_string(),
                }];
                events.extend(
                    timeline
                        .entries
                        .iter()
                        .filter(|entry| entry.turn == turn)
                        .map(|entry| {
                            let EntryContent::Item(content) = &entry.content else {
                                panic!("image fixture contains provider items");
                            };
                            agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                                id: entry.id.clone(),
                                parent_item_id: None,
                                content: content.clone(),
                            })
                        }),
                );
                events.push(agent::AgentEvent::TurnCompleted {
                    turn_id: turn.to_string(),
                    status: agent::TurnStatus::Completed,
                    usage: None,
                });
                for event in events {
                    state.record_event_for_replica_test(&seeded_id, ts, &event, cx);
                    ts += 1;
                }
            }
        }))
        .expect("record inline image conversation");
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
        store.update(cx, |store, cx| store.select_session(session_id.clone(), cx));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        let window_state = cx.new(|_| WindowState::new(false));
        struct LoadedImageChat {
            chat: Entity<ChatView>,
            sources: Vec<gpui::ImageSource>,
            ready: std::rc::Rc<std::cell::Cell<bool>>,
        }
        impl gpui::Render for LoadedImageChat {
            fn render(
                &mut self,
                window: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let mut loaded = true;
                for source in &self.sources {
                    let gpui::ImageSource::Custom(load) = source else {
                        unreachable!("host images load through the query plane");
                    };
                    match load(window, cx) {
                        Some(Ok(_)) => {}
                        Some(Err(error)) => panic!("inline host image load failed: {error:?}"),
                        None => loaded = false,
                    }
                }
                self.ready.set(loaded);
                self.chat.clone()
            }
        }
        let ready = std::rc::Rc::new(std::cell::Cell::new(false));
        let image_sources = vec![crate::store::image_preview(
            path.clone(),
            crate::store::MAX_PREVIEW_PIXELS as f32,
            false,
        )];
        let (root, cx) = cx.add_window_view(|window, cx| {
            let chat = cx.new(|cx| ChatView::new(store.clone(), window_state, window, cx));
            let surface = cx.new(|_| LoadedImageChat {
                chat,
                sources: image_sources,
                ready: ready.clone(),
            });
            gpui_base::Root::new(surface, window, cx)
        });
        let view = root.read_with(cx, |root, cx| {
            root.view()
                .clone()
                .downcast::<LoadedImageChat>()
                .unwrap()
                .read(cx)
                .chat
                .clone()
        });
        cx.simulate_resize(gpui::size(px(393.), px(600.)));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            driver.step(cx, &store);
            draw_settled(cx);
            if ready.get() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "inline host image finishes loading"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let row = cx.debug_bounds("timeline-row-89").unwrap_or_else(|| {
            panic!(
                "last assistant row: selected={:?}, list_items={}",
                store.read_with(cx, |store, _| store.active_session_id()),
                list.item_count()
            )
        });
        cx.simulate_event(ScrollEvent {
            position: list.viewport_bounds().center(),
            delta: ScrollDelta::Pixels(point(px(0.), list.viewport_bounds().top() - row.top())),
            touch_phase: TouchPhase::Moved,
            ..Default::default()
        });
        draw_settled(cx);
        let pill = cx
            .debug_bounds("scroll-to-end")
            .expect("pill above the image");
        let click = |position, cx: &mut gpui::VisualTestContext| {
            cx.simulate_mouse_move(position, None, Modifiers::default());
            cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
            cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
            draw_settled(cx);
        };
        click(point(pill.center().x, pill.top() - px(40.)), cx);
        let deadline = Instant::now() + Duration::from_secs(5);
        while cx.debug_bounds("image-viewer-image").is_none() {
            driver.step(cx, &store);
            draw_settled(cx);
            assert!(
                Instant::now() < deadline,
                "exposed image finishes its explicit read"
            );
            std::thread::yield_now();
        }
        assert!(
            cx.debug_bounds("image-viewer-image").is_some(),
            "exposed image opens the lightbox"
        );
        cx.simulate_keystrokes("escape");
        draw_settled(cx);
        assert!(cx.debug_bounds("image-viewer-image").is_none());
        assert!(!list.is_following_tail());

        let pill = cx.debug_bounds("scroll-to-end").unwrap();
        click(pill.center(), cx);
        assert!(list.is_following_tail(), "pill click must reach the tail");
        assert!(
            cx.debug_bounds("image-viewer-image").is_none(),
            "pill click opened the image underneath"
        );
        driver.shutdown(cx, &store, &host);
        std::fs::remove_file(path).unwrap();
    }

    #[gpui::test]
    fn jump_to_latest_survives_unmeasured_history(cx: &mut TestAppContext) {
        use gpui::{Context, IntoElement, Render, VisualTestContext, Window, point, px};

        struct TouchChat(Entity<ChatView>);
        impl Render for TouchChat {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                self.0.clone()
            }
        }
        let full = synthetic_markdown_timeline(120);
        let mut tail = full.clone();
        tail.turns.drain(..20);
        tail.entries.retain(|entry| entry.turn >= 20);
        for entry in &mut tail.entries {
            Arc::make_mut(entry).turn -= 20;
        }
        assert_eq!(tail.entries.len(), 300);
        let (store, window_state, session_id) = seed_chat(cx, tail);
        let (root, cx) = cx.add_window_view(|window, cx| {
            TouchChat(cx.new(|cx| ChatView::new(store.clone(), window_state, window, cx)))
        });
        let view = root.read_with(cx, |root, _| root.0.clone());
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        draw_settled(cx);
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let height = list.viewport_bounds().size.height;
        assert!(height > px(0.));
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        let scroll = |distance, phase, cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollEvent {
                position: list.viewport_bounds().center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), distance)),
                touch_phase: phase,
                ..Default::default()
            });
            draw_settled(cx);
        };
        // Small wheel deltas must accumulate, rather than snapping back while
        // the viewport is still inside the one-screen follow threshold.
        for _ in 0..12 {
            scroll(height / 4., gpui::TouchPhase::Moved, cx);
        }
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "jump to latest must appear three screens above the tail (position {:?}, end {:?})",
            list.logical_scroll_top(),
            list.is_scrolled_to_end()
        );
        assert!(!list.is_following_tail());
        // Reach a nonzero intra-row offset through the same real wheel path;
        // a row-boundary anchor would miss prepend regressions that reset it.
        scroll(
            list.logical_scroll_top().offset_in_item - px(7.),
            gpui::TouchPhase::Moved,
            cx,
        );
        let before = list.logical_scroll_top();
        assert!(before.item_ix > 0);
        assert_eq!(before.offset_in_item, px(7.));
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), full, cx);
            cx.notify();
        });
        draw_settled(cx);
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "prepend hides jump"
        );
        assert_eq!(list.logical_scroll_top().item_ix, before.item_ix + 60);
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            before.offset_in_item
        );

        let button = cx.debug_bounds("scroll-to-end").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        draw_settled(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        assert!(list.is_following_tail());
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                session_id.clone(),
                synthetic_markdown_timeline(121),
                cx,
            );
            cx.notify();
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        assert!(list.is_following_tail());
        assert_eq!(
            list.logical_scroll_top().item_ix,
            list.item_count(),
            "new turn did not move the tail anchor"
        );

        // A touch pan reaches the list's own wheel handler. No store
        // notification drives this UI.
        scroll(height * 3., gpui::TouchPhase::Started, cx);
        assert!(
            cx.debug_bounds("scroll-to-end").is_some(),
            "a touch pan hides jump"
        );
        assert!(!list.is_following_tail());
        let before = list.logical_scroll_top();
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                session_id.clone(),
                synthetic_markdown_timeline(122),
                cx,
            );
            cx.notify();
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_some());
        assert_eq!(
            list.logical_scroll_top().item_ix,
            before.item_ix,
            "new turn steals reading position"
        );
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            before.offset_in_item
        );
        assert!(!list.is_following_tail());

        let mut streaming = synthetic_markdown_timeline(122);
        let tail = Arc::make_mut(streaming.entries.last_mut().unwrap());
        tail.content = assistant(&"Streaming paragraph.\n\n".repeat(20));
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), streaming, cx);
            cx.notify();
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("scroll-to-end").is_some());
        assert_eq!(list.logical_scroll_top().item_ix, before.item_ix);
        assert_eq!(
            list.logical_scroll_top().offset_in_item,
            before.offset_in_item
        );
        assert!(
            !list.is_following_tail(),
            "streaming steals reading position"
        );

        scroll(-height * 100., gpui::TouchPhase::Moved, cx);
        assert!(cx.debug_bounds("scroll-to-end").is_none());
        // Reaching the bottom resumes following, independently of pill visibility.
        assert!(list.is_following_tail());
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, synthetic_markdown_timeline(123), cx);
            cx.notify();
        });
        draw_settled(cx);
        assert!(list.is_following_tail());
        assert!(cx.debug_bounds("scroll-to-end").is_none());
    }

    /// A scrolling screenshot moves the timeline tile by tile from wherever it
    /// was, learns how far it really moved once layout has settled, and leaves
    /// it where it was.
    #[gpui::test]
    fn scrolling_capture_moves_the_timeline_within_its_content_and_restores_it(
        cx: &mut TestAppContext,
    ) {
        use gpui::{FollowMode, ListOffset, px};
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(60));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        };
        // The rows on screen are built and measured before a capture starts.
        for _ in 0..4 {
            draw(cx);
            cx.run_until_parked();
        }
        assert!(view.read_with(cx, |chat, _| chat.capture_settled()));
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        assert!(list.is_following_tail());
        assert!(
            super::timeline::capture_scrolled(&list) > px(852.),
            "sixty turns scroll well past one screen"
        );
        // Rows measured for the first time move the list; the capturer scrolls
        // again after each frame until nothing moves.
        let settle = |offset: gpui::Pixels, cx: &mut gpui::VisualTestContext| {
            for _ in 0..8 {
                let scroll = view
                    .update(cx, |chat, cx| chat.capture_scroll(offset, cx))
                    .expect("a capture is in progress");
                if !scroll.moved {
                    return scroll.reached;
                }
                draw(cx);
            }
            panic!("the timeline kept moving");
        };

        // Following the tail: the capture starts at the bottom.
        assert!(view.update(cx, |chat, cx| chat.capture_begin(cx)));
        let viewport = view.read_with(cx, |chat, _| chat.capture_viewport());
        assert!(viewport.is_some_and(|bounds| bounds.size.height > px(0.)));
        assert!(
            !list.is_following_tail(),
            "layout must not snap back to the end"
        );
        assert_eq!(
            settle(px(0.), cx),
            px(0.),
            "the first tile is the screen itself"
        );
        assert_eq!(settle(px(-400.), cx), px(-400.));
        assert!(
            list.logical_scroll_top().item_ix < 176,
            "a screen above the last rows"
        );
        assert_eq!(
            settle(px(400.), cx),
            px(0.),
            "nothing lies below the end of the content"
        );
        let top = settle(px(-1.0e6), cx);
        assert!(
            top < px(-852.) && top > px(-1.0e6),
            "the tile above the first turn stops at the top: {top:?}"
        );
        assert_eq!(list.logical_scroll_top().item_ix, 0);
        view.update(cx, |chat, cx| chat.capture_end(cx));
        assert!(list.is_following_tail());
        assert_eq!(
            view.update(cx, |chat, cx| chat.capture_scroll(px(0.), cx)),
            None,
            "tiles after the end are refused"
        );

        // Reading in the middle: the anchor comes back exactly, still paused.
        list.set_follow_mode(FollowMode::Normal);
        let anchor = ListOffset {
            item_ix: 10,
            offset_in_item: px(7.),
        };
        list.scroll_to(anchor);
        view.update(cx, |_, cx| cx.notify());
        draw(cx);
        assert!(view.update(cx, |chat, cx| chat.capture_begin(cx)));
        assert_eq!(settle(px(0.), cx), px(0.));
        assert_eq!(settle(px(300.), cx), px(300.));
        assert!(list.logical_scroll_top().item_ix >= 10);
        view.update(cx, |chat, cx| chat.capture_end(cx));
        let restored = list.logical_scroll_top();
        assert_eq!(restored.item_ix, anchor.item_ix);
        assert_eq!(restored.offset_in_item, anchor.offset_in_item);
        assert!(!list.is_following_tail());
    }

    /// A long streamed conversation opens with one partial turn: the snapshot
    /// holds only its last records. The first page completes that turn and
    /// adds an earlier one; the reader's position must survive rather than
    /// resetting to the tail.
    #[gpui::test]
    fn completing_the_only_partial_turn_keeps_the_reading_anchor(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, px};
        let mut full = synthetic_markdown_timeline(2);
        // Tall enough that the reader can scroll into the loaded content.
        let long_answer = "A paragraph of the answer.\n\n".repeat(80);
        full.entries.pop();
        full.entries
            .push(at_turn(entry("assistant-1", assistant(&long_answer)), 1));
        let mut tail = Timeline::default();
        tail.turns = vec![TurnMeta::default()];
        tail.entries = vec![entry("assistant-1", assistant(&long_answer))];
        let (store, window_state, session_id, _driver) = seed_chat_with_history(cx, tail, true);
        cx.update(|cx| cx.set_reduce_motion(true));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        cx.simulate_resize(gpui::size(px(1024.), px(700.)));
        for _ in 0..2 {
            view.update(cx, |_, cx| cx.notify());
            draw_settled(cx);
        }
        let (list, placeholder, leading) = view.read_with(cx, |chat, _| {
            (
                chat.list_state.clone(),
                chat.history_placeholder_height,
                chat.leading_space(),
            )
        });
        assert!(placeholder > px(0.), "earlier history is still unloaded");
        list.set_follow_mode(FollowMode::Normal);
        list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: leading + px(20.),
        });
        view.update(cx, |_, cx| cx.notify());
        draw_settled(cx);
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, full, cx);
            cx.notify();
        });
        draw_settled(cx);
        let anchor = list.logical_scroll_top();
        // The page adds the earlier turn's three rows and the user bubble
        // above the answer.
        assert_eq!(
            (anchor.item_ix, anchor.offset_in_item),
            (5, px(20.)),
            "the completed turn keeps its reader at the same content offset"
        );
        assert!(
            !list.is_following_tail(),
            "the page must not reset the list to its tail"
        );
    }

    /// A reader who scrolls far up a long thread and comes back sees rows at
    /// every step. Once they have stayed at the tail, the pages fetched above
    /// it are released without moving the tail: the conversation is again the
    /// one the tail needed, nothing is fetched back while they stay, and the
    /// next scroll up asks for the page above that window.
    #[gpui::test]
    fn pages_read_far_up_are_released_once_the_reader_stays_at_the_tail(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, point, px};
        use tcode_core::session::StoredEvent;
        use tcode_protocol::{
            ClientPayload, EventEnvelope, HostMessage, Query, QueryResponse, ServerEvent, Topic,
        };
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let data_root = std::env::temp_dir().join(format!(
            "tcode-chat-release-test-{}-{}",
            std::process::id(),
            NEXT_RESIDENCY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let host = spawn_host(
            SessionStore::open_at(data_root.clone()).expect("test session store"),
            HostServices::default(),
        )
        .expect("spawn test host");
        let mut status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("long".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        host.shutdown_blocking().unwrap();
        let _ = std::fs::remove_dir_all(data_root);
        status.session_id = "long".into();
        status.draft = false;
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            std::env::temp_dir(),
            None,
        );
        meta.id = "long".into();

        // Turn `t` is the four records at `4 * t` of a 400-turn log.
        let turns = |range: std::ops::Range<u64>| -> Vec<StoredEvent> {
            range
                .flat_map(|turn| {
                    [
                        agent::AgentEvent::TurnStarted {
                            turn_id: turn.to_string(),
                        },
                        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                            id: format!("user-{turn}"),
                            parent_item_id: None,
                            content: ItemContent::UserMessage {
                                text: format!("Question {turn}"),
                                context_len: None,
                                attachments: vec![],
                            },
                        }),
                        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                            id: format!("answer-{turn}"),
                            parent_item_id: None,
                            content: ItemContent::AssistantMessage {
                                text: format!("Answer {turn}.\n\n").repeat(3),
                                summarized: false,
                            },
                        }),
                        agent::AgentEvent::TurnCompleted {
                            turn_id: turn.to_string(),
                            status: agent::TurnStatus::Completed,
                            usage: None,
                        },
                    ]
                    .map(StoredEvent::from)
                })
                .collect()
        };

        let (to_host, outgoing) = async_channel::unbounded::<String>();
        let (incoming, from_host) = async_channel::unbounded::<String>();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn({
            let link = link.clone();
            async move {
                link.pump_with_timer(|| executor.timer(Duration::from_millis(25)))
                    .await;
            }
        });
        let send = |message: HostMessage| {
            incoming
                .try_send(tcode_protocol::encode_line(&message).unwrap())
                .unwrap();
        };
        let event = |topic: Topic, event: ServerEvent| {
            send(HostMessage::Event(EventEnvelope {
                request_id: None,
                topic,
                event,
            }));
        };
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
        store.update(cx, |store, cx| store.select_session("long".into(), cx));
        event(
            Topic::Settings,
            ServerEvent::SettingsSnapshot(Default::default()),
        );
        event(
            Topic::Index,
            ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                summary: Default::default(),
                sessions: vec![meta],
                projects: vec![],
            }),
        );
        event(
            Topic::SessionStatus {
                session_id: "long".into(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        event(
            Topic::SessionEvents {
                session_id: "long".into(),
            },
            ServerEvent::SessionSnapshot {
                from: 4 * 390,
                end: 4 * 400,
                records: turns(390..400),
                total: 4 * 400,
                total_turns: 400,
                truncated: false,
            },
        );
        let window_state = cx.new(|_| WindowState::new(false));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));

        // Present a frame, as the display link does, and answer the page it
        // asked for, ten turns above `before`; then let the store's single
        // request gate reopen.
        let frame = |cx: &mut VisualTestContext| -> Option<u64> {
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
            let pages: Vec<(u64, u64)> = std::iter::from_fn(|| outgoing.try_recv().ok())
                .map(|line| tcode_protocol::decode_client_line(&line).unwrap())
                .filter_map(|message| match message.payload {
                    ClientPayload::Query(Query::SessionHistoryPage { before, .. }) => {
                        Some((message.id, before))
                    }
                    _ => None,
                })
                .collect();
            assert!(pages.len() <= 1, "one page in flight: {pages:?}");
            let (id, before) = *pages.first()?;
            send(HostMessage::QueryResult {
                id,
                result: Ok(QueryResponse::SessionHistoryPage {
                    records: turns(before / 4 - 10..before / 4),
                    from: before - 40,
                    end: before,
                    truncated: false,
                }),
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(250));
            Some(before)
        };
        let settle = |cx: &mut VisualTestContext| -> Vec<u64> {
            let pages: Vec<u64> = std::iter::from_fn(|| frame(cx)).take(40).collect();
            assert!(frame(cx).is_none(), "the prefetch settles");
            pages
        };
        let replica = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| store.with_active_timeline(Timeline::clone))
        };
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        // One trackpad gesture.
        let scroll = |distance: gpui::Pixels, cx: &mut VisualTestContext| {
            let position = list.viewport_bounds().center();
            for (touch_phase, distance) in [
                (gpui::TouchPhase::Started, px(0.)),
                (gpui::TouchPhase::Moved, distance),
                (gpui::TouchPhase::Ended, px(0.)),
            ] {
                cx.simulate_event(gpui::ScrollEvent {
                    position,
                    delta: gpui::ScrollDelta::Pixels(point(px(0.), distance)),
                    touch_phase,
                    ..Default::default()
                });
            }
        };

        let tail_pages = settle(cx);
        assert!(list.is_following_tail());
        assert!(!tail_pages.is_empty(), "ten turns do not fill six screens");
        let tail = replica(cx).expect("the tail window");
        let tail_from = tail_pages.last().unwrap() - 40;

        let height = list.viewport_bounds().size.height;
        let mut pages_away = 0;
        while pages_away < 6 {
            scroll(height * 3., cx);
            while frame(cx).is_some() {
                pages_away += 1;
                view.read_with(cx, |chat, cx| {
                    assert!(!chat.workspace_store.read(cx).chat_loading());
                    assert!(!chat.rows.is_empty(), "the conversation stays on screen");
                });
            }
            assert!(!list.is_following_tail());
        }
        let read = replica(cx).unwrap();
        assert!(read.turns.len() > tail.turns.len());

        let back = |cx: &mut VisualTestContext| {
            // The pill shows once a frame has seen the reader away.
            draw(cx);
            let pill = cx.debug_bounds("scroll-to-end").expect("jump to latest");
            cx.simulate_click(pill.center(), gpui::Modifiers::none());
            assert!(frame(cx).is_none());
            assert!(list.is_following_tail());
        };
        back(cx);
        cx.executor().advance_clock(Duration::from_secs(20));
        assert!(frame(cx).is_none());
        assert_eq!(
            replica(cx).unwrap(),
            read,
            "a glance at the tail keeps them"
        );
        scroll(height * 1.5, cx);
        assert!(frame(cx).is_none());
        assert!(!list.is_following_tail());
        cx.executor().advance_clock(Duration::from_secs(20));
        assert!(frame(cx).is_none());
        assert_eq!(replica(cx).unwrap(), read, "reading above keeps them");

        back(cx);
        let last_row = |cx: &mut VisualTestContext| {
            cx.debug_bounds(format!("timeline-row-{}", list.item_count() - 1).leak())
                .expect("the last row is on screen")
        };
        let rows = list.item_count();
        let bottom = last_row(cx).bottom();
        cx.executor().advance_clock(Duration::from_secs(30));
        assert!(frame(cx).is_none(), "the tail's own pages are still held");
        assert_eq!(replica(cx).unwrap(), tail);
        assert!(list.is_following_tail());
        assert!(list.item_count() < rows);
        assert_eq!(last_row(cx).bottom(), bottom, "the tail stays where it was");
        assert!(settle(cx).is_empty());

        scroll(height * 3., cx);
        assert_eq!(frame(cx), Some(tail_from), "the page above the tail window");

        // Left with that page held, the thread reopens from its cursor with
        // the window its tail needed.
        while outgoing.try_recv().is_ok() {}
        store.update(cx, |store, cx| {
            store.select_session("other".into(), cx);
            store.select_session("long".into(), cx);
        });
        let events_topic = Topic::SessionEvents {
            session_id: "long".into(),
        };
        let mut resumed_id = None;
        let resumed: Vec<_> = std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(|line| {
                let message = tcode_protocol::decode_client_line(&line).unwrap();
                match message.payload {
                    ClientPayload::Subscribe(subscription)
                        if subscription.topic == events_topic =>
                    {
                        resumed_id = Some(message.id);
                        Some(subscription.after)
                    }
                    _ => None,
                }
            })
            .collect();
        assert_eq!(resumed, [Some(4 * 400)]);
        send(HostMessage::Event(EventEnvelope {
            request_id: resumed_id,
            topic: events_topic,
            event: ServerEvent::SessionSnapshot {
                from: 4 * 400,
                end: 4 * 400,
                records: vec![],
                total: 4 * 400,
                total_turns: 400,
                truncated: false,
            },
        }));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        assert_eq!(replica(cx).unwrap(), tail);
    }

    #[gpui::test]
    fn pan_packet_after_a_page_lands_keeps_the_walk_back_into_it(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, point, px};
        for pan_delta in [0., 40.] {
            let full = synthetic_markdown_timeline(60);
            let mut tail = synthetic_markdown_timeline(60);
            tail.turns.drain(..20);
            tail.entries.retain(|entry| entry.turn >= 20);
            for entry in &mut tail.entries {
                std::sync::Arc::make_mut(entry).turn -= 20;
            }
            let (store, window_state, session_id, _driver) = seed_chat_with_history(cx, tail, true);
            cx.update(|cx| cx.set_reduce_motion(true));
            let (view, cx) = cx.add_window_view(|window, cx| {
                ChatView::new(store.clone(), window_state, window, cx)
            });
            cx.simulate_resize(gpui::size(px(393.), px(852.)));
            draw_settled(cx);
            let (list, placeholder, leading) = view.read_with(cx, |chat, _| {
                (
                    chat.list_state.clone(),
                    chat.history_placeholder_height,
                    chat.leading_space(),
                )
            });
            assert!(placeholder > px(100.));
            list.set_follow_mode(FollowMode::Normal);
            list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: leading - px(100.),
            });
            draw_settled(cx);
            let content_top = list.bounds_for_item(0).unwrap().top() + leading;
            assert!(content_top > list.viewport_bounds().top());
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(session_id, full, cx);
                store.suppress_history_prefetch_for_test();
                cx.notify();
            });
            // The frame that lands the page measures it.
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            // The finger is still moving: a pan packet resolves against that
            // frame's anchor before the next frame walks back into the page.
            if pan_delta != 0. {
                cx.simulate_event(gpui::ScrollEvent {
                    position: list.viewport_bounds().center(),
                    delta: gpui::ScrollDelta::Pixels(point(px(0.), px(pan_delta))),
                    touch_phase: gpui::TouchPhase::Moved,
                    ..Default::default()
                });
            }
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
                let _ = window.draw(cx);
            });
            draw_settled(cx);
            let anchor = list.logical_scroll_top();
            assert!(
                anchor.item_ix < 60 && anchor.offset_in_item >= px(0.),
                "pan {pan_delta}: {anchor:?}"
            );
            let after = list
                .bounds_for_item(60)
                .expect("previous first turn remains on screen")
                .top();
            assert!(
                (after - (content_top + px(pan_delta))).abs() < px(1.),
                "the walk-back and the pan both apply: {content_top:?} + {pan_delta} -> {after:?}"
            );
            assert!(!list.is_following_tail());
        }
    }

    #[gpui::test]
    fn upward_scroll_moves_only_by_the_input_distance(cx: &mut TestAppContext) {
        use gpui::{ScrollDelta, ScrollEvent, point, px, size};

        for width in [393., 1024.] {
            let mut timeline = synthetic_markdown_timeline(3);
            for entry in &mut timeline.entries {
                if let EntryContent::Item(ItemContent::AssistantMessage { text, .. }) =
                    &mut Arc::make_mut(entry).content
                {
                    *text = (0..40)
                        .map(|n| {
                            format!(
                                "## Section {n}\n\nA paragraph with **bold** text and `code`.\n\n"
                            )
                        })
                        .collect();
                }
            }
            let (store, window_state, _) = seed_chat(cx, timeline);
            let (view, cx) =
                cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
            cx.simulate_resize(size(px(width), px(700.)));
            draw_settled(cx);
            let list = view.read_with(cx, |chat, _| chat.list_state.clone());
            list.set_offset_from_scrollbar(point(
                px(0.),
                list.scroll_px_offset_for_scrollbar().y + px(40.),
            ));
            view.update(cx, |_, cx| cx.notify());
            draw_settled(cx);
            for step in 0..160 {
                let item = list.logical_scroll_top().item_ix;
                let before = list.bounds_for_item(item).unwrap().top();
                cx.simulate_event(ScrollEvent {
                    position: list.viewport_bounds().center(),
                    delta: ScrollDelta::Pixels(point(px(0.), px(40.))),
                    touch_phase: gpui::TouchPhase::Moved,
                    ..Default::default()
                });
                draw_settled(cx);
                let after = list
                    .bounds_for_item(item)
                    .expect("reading anchor stays visible")
                    .top();
                assert!(
                    (f32::from(after - before) - 40.).abs() < 1.,
                    "step {step}: upward input of 40px moved from {before:?} to {after:?}"
                );
            }
        }
    }

    /// Scrolling to a turn far above the tail renders its messages there
    /// without moving the reader.
    #[gpui::test]
    fn a_distant_turn_renders_its_messages_under_the_reader_without_moving_them(
        cx: &mut TestAppContext,
    ) {
        use gpui::{FollowMode, ListOffset, VisualTestContext, px, size};

        // The user bubble of turn 40: three rows per synthetic turn.
        const TARGET: usize = 120;
        let mut timeline = synthetic_markdown_timeline(240);
        // The thread's only code block, in the answer below the reader: its
        // lines exist only once the message is rendered as Markdown.
        Arc::make_mut(&mut timeline.entries[TARGET + 2]).content =
            assistant("```\nAssistant 40\n```");
        let (workspace_store, window_state, _) = seed_chat(cx, timeline);
        let (view, cx) = cx
            .add_window_view(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);
        let list_state = view.read_with(cx, |chat, _| chat.list_state.clone());

        list_state.set_follow_mode(FollowMode::Normal);
        list_state.scroll_to(ListOffset {
            item_ix: TARGET,
            offset_in_item: px(7.),
        });
        draw_settled(cx);

        assert!(
            cx.debug_bounds("markdown-code-line-0")
                .is_some_and(|line| line.intersects(&list_state.viewport_bounds())),
            "the turn under the reader is not rendered as Markdown"
        );
        let scroll_top = list_state.logical_scroll_top();
        assert_eq!(scroll_top.item_ix, TARGET);
        assert_eq!(
            scroll_top.offset_in_item,
            px(7.),
            "rendering the scroll-top turn changed the list anchor"
        );
    }

    /// A message over the background-parse threshold shows its text at once
    /// and its Markdown from the frame after the parse lands, inside its row;
    /// a smaller one is Markdown from its first frame.
    #[gpui::test]
    fn large_markdown_renders_once_its_background_parse_lands(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        for (id, text, asynchronous) in [
            ("small", "small **streaming** reply".to_string(), false),
            ("large", large_markdown("async content"), true),
        ] {
            assert_eq!(text.len() >= ASYNC_MARKDOWN_THRESHOLD_BYTES, asynchronous);
            let timeline = single_assistant_timeline(id, &text);
            let (workspace_store, window_state, _) = seed_chat(cx, timeline);
            // Opening the window draws a frame before any background work runs.
            let window = cx
                .add_window(|window, cx| ChatView::new(workspace_store, window_state, window, cx));
            let cx = &mut VisualTestContext::from_window(window.into(), cx);
            cx.simulate_resize(size(px(1_024.), px(700.)));
            assert_eq!(
                painted_markdown_block(cx).is_some(),
                !asynchronous,
                "{id}: the first frame"
            );
            draw_settled(cx);
            let block = painted_markdown_block(cx).expect("the parsed message");
            let row = cx.debug_bounds("timeline-row-0").expect("the message row");
            assert!(
                block.top() >= row.top() && block.bottom() <= row.bottom(),
                "{id}: {block:?} spills out of its row {row:?}"
            );
        }
    }

    /// A rewrite of a message whose parse is still in flight shows the
    /// rewritten text once the parse lands, not the text it started from.
    #[gpui::test]
    fn in_flight_markdown_update_finishes_with_latest_text(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        let original = large_markdown("original");
        // The thread's only code block: its lines show only the rewrite.
        let latest = format!("{}\n```\nedited\n```\n", large_markdown("edited"));
        let timeline = single_assistant_timeline("large", &original);
        let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
        let window = cx.add_window(|window, cx| {
            ChatView::new(workspace_store.clone(), window_state, window, cx)
        });
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(size(px(1_024.), px(700.)));
        assert!(
            painted_markdown_block(cx).is_none(),
            "the original's parse is in flight"
        );

        // The rewrite lands and is drawn before the parse finishes.
        workspace_store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                session_id,
                single_assistant_timeline("large", &latest),
                cx,
            );
            cx.notify();
        });
        assert!(painted_markdown_block(cx).is_none());
        draw_settled(cx);
        assert!(
            cx.debug_bounds("markdown-code-line-0").is_some(),
            "the message settled on the text its parse started from"
        );
    }

    /// A thread switched away from while a message's parse is in flight
    /// does not get that message back in the thread it switched to, even
    /// under the same item id.
    #[gpui::test]
    fn session_switch_does_not_resurrect_in_flight_markdown(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        // The thread's only code block: its lines show only the stale text.
        let text = format!("{}\n```\nstale\n```\n", large_markdown("stale session"));
        let (host, session_id, timeline) =
            seeded_chat_host(cx, single_assistant_timeline("large", &text), false);
        let replacement = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("replacement".into(), std::env::temp_dir(), cx);
            let active = state.residents.live.get_mut(&id).expect("second draft");
            active.draft = false;
            let meta = active.meta.clone();
            state.sessions.push(meta);
            id
        }))
        .expect("seed the replacement thread");
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
        store.update(cx, |store, cx| store.select_session(session_id.clone(), cx));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, _| !store.chat_loading())
        });
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let window =
            cx.add_window(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        let view = window.root(cx).expect("the chat is the window's root");
        let cx = &mut VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(size(px(1_024.), px(700.)));
        assert!(
            painted_markdown_block(cx).is_none(),
            "the stale message's parse is in flight"
        );

        // The shell binds the pane to the thread it opens.
        store.update(cx, |store, cx| {
            store.select_session(replacement.clone(), cx)
        });
        view.update(cx, |chat, cx| {
            chat.set_session(Some(replacement.clone()), cx)
        });
        driver.wait_until(cx, &store, "the replacement thread's history", |cx| {
            store.read_with(cx, |store, _| store.with_active_timeline(|_| ()).is_some())
        });
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(
                replacement,
                single_assistant_timeline("large", "The replacement."),
                cx,
            );
            cx.notify();
        });
        draw_settled(cx);

        assert!(
            painted_markdown_block(cx).is_some(),
            "the replacement message"
        );
        assert!(
            cx.debug_bounds("markdown-code-line-0").is_none(),
            "the switched-away message came back"
        );
        driver.shutdown(cx, &store, &host);
    }

    #[gpui::test]
    fn long_markdown_paints_middle_blocks_when_scrolled_in_chat_outer_list(
        cx: &mut TestAppContext,
    ) {
        use gpui::{VisualTestContext, point, px, size};
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        const DEMO_MARKDOWN: &str = r#"# H1

## H2

This paragraph has **bold**, *italic*, ~~strikethrough~~, and `inline code`.

```rust
fn main() {
    let language = "Rust";
    let message = format!("Hello from {language}!");
    println!("{message}");
}
```

```typescript
const count: number = 3;
interface Demo {
  title: string;
  enabled: boolean;
}
const demo: Demo = { title: "TypeScript", enabled: true };
```

```python
def greet(name: str) -> str:
    message = f"Hello, {name}!"
    return message

print(greet("Python"))
```

```go
package main
func main() {
    message := "Hello from Go"
    println(message)
}
```

```toml
[demo]
title = "TOML sample"
enabled = true
count = 3
```

```kotlin
fun main() {
    val language: String = "Kotlin"
    println("Hello from $language")
}
```

```text
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

| Left | Center | Right |
| :--- | :----: | ----: |
| alpha | beta | 100 |
| gamma | delta | 200 |

| Name | Status | Owner | Description | Count | Notes |
| :--- | :----: | :--- | :---------- | ----: | :---- |
| Short | Ready | UI | This deliberately long table cell contains about sixty characters total. | 12 | wraps |
| Longer component name | Pending | Visual QA | Brief | 3 | compact |

- [x] Render headings
- [ ] Inspect every pixel

1. Ordered parent
   - Unordered child
     1. Nested ordered child
2. Second ordered item

- Unordered parent
  - Nested bullet
    1. Ordered grandchild

> First line of the blockquote.
> Second line of the blockquote.

Visit [Example](https://example.com) or the bare URL https://tcode.dev for more.

---

This paragraph has a soft
line break, followed by a hard break.\
This begins after the hard break."#;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let data_root = crate::store::tests::scratch_root("markdown-outer-list-test");
        let store = SessionStore::open_at(data_root).expect("test session store");
        let host = spawn_host(store, HostServices::default()).expect("spawn test host");
        let (session_id, timeline) =
            smol::block_on(host.update_state_for_test(move |state, cx| {
                let id = state.start_draft("markdown-test".into(), std::env::temp_dir(), cx);
                let active = state.residents.live.get_mut(&id).expect("selected draft");
                active.timeline = Timeline::default();
                active.timeline.turns = vec![TurnMeta::default()];
                active.timeline.entries = vec![
                    entry("user", user_item("render a long document")),
                    entry("assistant", assistant(DEMO_MARKDOWN)),
                ];
                active.draft = false;
                (active.meta.id.clone(), active.timeline.clone())
            }))
            .expect("seed markdown host");
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
        workspace_store.update(cx, |store, cx| store.select_session(session_id.clone(), cx));
        driver.wait_until(cx, &workspace_store, "selected conversation", |cx| {
            workspace_store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        workspace_store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let window_state = cx.new(|_| WindowState::new(false));

        let (view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(workspace_store.clone(), window_state, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        cx.run_until_parked();
        draw_settled(cx);
        cx.run_until_parked();
        draw_settled(cx);

        let outer_list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let max_scroll = outer_list.max_offset_for_scrollbar().y;
        assert!(
            max_scroll > px(800.),
            "long markdown did not contribute its full height to the chat list: {max_scroll:?}"
        );
        let scroll_step = px(40.);
        let steps = (f32::from(max_scroll / 2.) / f32::from(scroll_step)).ceil() as usize;
        for step in 0..steps {
            let distance = (scroll_step * (step + 1) as f32).min(max_scroll / 2.);
            outer_list.set_offset_from_scrollbar(point(px(0.), -(max_scroll - distance)));
            view.update(cx, |_, cx| cx.notify());
            draw_settled(cx);
        }

        let viewport = outer_list.viewport_bounds();
        let middle = cx.debug_bounds("markdown-block-6");
        assert!(
            middle.is_some_and(|bounds| bounds.intersects(&viewport)),
            "middle markdown block did not paint at the chat list's middle offset; bounds={middle:?}, max_scroll={max_scroll:?}, viewport={viewport:?}"
        );
        assert!(
            cx.debug_bounds("markdown-block-18").is_none(),
            "offscreen tail block was painted; block virtualization did not cull it"
        );
        driver.shutdown(cx, &workspace_store, &host);
    }

    /// Unloaded history reserves a screen above the first turn from the first
    /// frame, so the timeline's extent is settled by it.
    #[gpui::test]
    fn the_history_reservation_is_settled_by_the_first_frame(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px};
        let (store, window_state, _, _driver) =
            seed_chat_with_history(cx, synthetic_markdown_timeline(1), true);
        // Opening the window draws its first frame.
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        let list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let first = list.max_offset_for_scrollbar().y;
        view.update(cx, |_, cx| cx.notify());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert_eq!(list.max_offset_for_scrollbar().y, first);
        assert!(
            first > px(0.),
            "one short turn scrolls into the reservation above it"
        );
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

    /// Whether the header's split buttons carry their labels follows the width
    /// it is laid out at, from the frame a resize draws.
    #[gpui::test]
    fn header_labels_follow_the_width_of_the_frame(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        let (store, window_state, _) = seed_chat(cx, synthetic_markdown_timeline(1));
        let (_view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
        let cx: &mut VisualTestContext = cx;
        draw(cx);
        // A resize draws its frame at the new size.
        let git_main = |width: f32, cx: &mut VisualTestContext| {
            cx.simulate_resize(size(px(width), px(700.)));
            cx.debug_bounds("git-main")
                .expect("the git button")
                .size
                .width
        };
        let labelled = git_main(1_024., cx);
        let narrow = git_main(600., cx);
        assert!(narrow < labelled, "a 600pt header kept its labels");
        assert_eq!(
            git_main(1_024., cx),
            labelled,
            "a 1024pt header lost its labels"
        );
    }

    fn entry(id: &str, content: EntryContent) -> Arc<TimelineEntry> {
        Arc::new(TimelineEntry {
            id: id.to_string(),
            content,
            ts: None,
            turn: 0,
        })
    }

    fn user_item(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::UserMessage {
            text: text.into(),
            context_len: None,
            attachments: Vec::new(),
        })
    }

    fn assistant(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        })
    }

    fn reasoning(text: &str) -> EntryContent {
        EntryContent::Item(ItemContent::Reasoning { text: text.into() })
    }

    fn command(id: &str) -> Arc<TimelineEntry> {
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

    fn command_with_output(id: &str) -> Arc<TimelineEntry> {
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
    fn command_open(
        cx: &mut gpui::VisualTestContext,
        id: &str,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        cx.debug_bounds(format!("command-output-{id}").leak())
    }

    fn at_turn(mut entry: Arc<TimelineEntry>, turn: usize) -> Arc<TimelineEntry> {
        Arc::make_mut(&mut entry).turn = turn;
        entry
    }

    fn synthetic_markdown_timeline(turn_count: usize) -> Timeline {
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

    fn single_assistant_timeline(id: &str, text: &str) -> Timeline {
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default()];
        timeline.entries.push(entry(id, assistant(text)));
        timeline
    }

    /// A Markdown block of any message painted in the last frame.
    fn painted_markdown_block(
        cx: &mut gpui::VisualTestContext,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        static SELECTORS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
        SELECTORS
            .get_or_init(|| {
                (0..256)
                    .map(|ix| &*format!("markdown-block-{ix}").leak())
                    .collect()
            })
            .iter()
            .find_map(|selector| cx.debug_bounds(selector))
    }

    fn large_markdown(marker: &str) -> String {
        let mut text = format!("# {marker}\n\n");
        while text.len() <= ASYNC_MARKDOWN_THRESHOLD_BYTES {
            text.push_str("- a sufficiently substantial Markdown list item\n");
        }
        text
    }

    fn seed_chat(
        cx: &mut TestAppContext,
        timeline: Timeline,
    ) -> (Entity<WorkspaceStore>, Entity<WindowState>, String) {
        let (store, window_state, session_id, _) = seed_chat_with_history(cx, timeline, false);
        (store, window_state, session_id)
    }

    fn seeded_chat_host(
        cx: &mut TestAppContext,
        timeline: Timeline,
        paged: bool,
    ) -> (tcode_runtime::pipe::SpawnedHost, String, Timeline) {
        seeded_chat_host_on(cx, timeline, paged, None)
    }

    /// A host holding one thread with `timeline`, on `provider` when given.
    fn seeded_chat_host_on(
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

    fn seed_chat_with_history(
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

    fn seed_chat_with_history_on(
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
