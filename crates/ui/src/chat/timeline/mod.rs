use super::*;
use crate::markdown::SelectionGesture;
use gpui::WeakEntity;

mod expansion;
mod glide;
mod list;
mod render;
mod residency;
mod rewind;
pub(super) mod rows;
mod spine;

use crate::motion::list_rows::{Arrival, Glide, RowGlide};
pub(super) use expansion::{
    AutoActivityExpansions, AutoActivityRecency, auto_activity_snapshot_keys,
};
pub(super) use render::{ActivityRow, FullOutput, WithheldLoad};
pub(super) use residency::{
    PendingMarkdownBuild, tail_row_window, transcript_selection, viewport_row_window,
};

/// A row that left while the reader looked: what it was drawn from, kept
/// until it has folded away against the row above it.
#[derive(Clone)]
pub(super) struct LeftRow {
    pub(super) row: TimelineRow,
    pub(super) turn: TurnMeta,
    pub(super) own: Vec<Arc<TimelineEntry>>,
}

/// A conversation's timeline: its rows in their virtual list, everything
/// that moves them (arrivals, folds, the followed tail, a reveal, a glide to
/// a turn or to the end), and the state they are drawn from.
///
/// A view of its own, which the chat embeds: gpui-fast builds a view again
/// when it asks for a frame or something it read changes, and draws the
/// views around it from last frame. A row's motion or a record streamed in
/// builds the timeline alone, not the chat's header, composer or rail.
pub(crate) struct TimelineView {
    pub(in crate::chat) workspace_store: Entity<WorkspaceStore>,
    pub(in crate::chat) window_state: Entity<WindowState>,
    /// The thread the timeline shows, the chat's.
    pub(in crate::chat) session_id: Option<String>,
    /// The chat that embeds the timeline, and for an agent's chat shown
    /// beside its lead, the lead's: what an agent opened from a row shows
    /// beside, and whose drawer a diff opens in.
    chat: WeakEntity<ChatView>,
    lead: Option<WeakEntity<ChatView>>,
    /// Where the chat lays the timeline out, as it last handed it.
    layout: TimelineLayout,
    /// The chat's scene, which the messages' reveals are drawn in.
    pub(in crate::chat) scene: crate::motion::Epoch,
    /// The motion of everything the timeline draws, in the chat's scene;
    /// see [`ChatView::motion`].
    pub(in crate::chat) motion: crate::motion::owned::Owned,
    pub(in crate::chat) list_state: ListState,
    /// The marks left of the content column: see [`spine::SpineView`].
    spine: Entity<spine::SpineView>,
    /// The list as the handle its bounce scrolls.
    followed: crate::scroll::FollowedList,
    /// The timeline changed since the list last mirrored it. GPUI's list
    /// resolves wheel and pan packets against the row indices it painted, so
    /// rows are only spliced during a frame, never between two frames.
    pub(in crate::chat) timeline_stale: bool,
    /// The store's revision of the thread's timeline as the list last
    /// mirrored it: a record streamed in moves it on without notifying the
    /// store.
    timeline_revision: Option<u64>,
    /// The room above the first held row that stands for the turns not held
    /// yet, so the scrollbar spans the whole thread rather than what is held.
    pub(in crate::chat) history_placeholder_height: gpui::Pixels,
    /// The mean height of a held turn, at which the reservation counts each
    /// turn not held.
    history_turn_height: Option<gpui::Pixels>,
    /// The turns not held as the last render counted them.
    history_unheld: Option<usize>,
    /// The held turns changed since the turn height was measured. It is
    /// measured again once, after the frame that lays out the change: a
    /// height that followed every frame would move the scrollbar while a
    /// reply streams in.
    history_turn_height_stale: bool,
    /// Whether the previously synced window still had unloaded history above
    /// it, so the final page reaching the start of the log still completes a
    /// partial first turn in place.
    had_unloaded_history: bool,
    /// Distance to scroll back once a page has filled the reservation the
    /// reader had scrolled into. Applied by the render after the frame that
    /// measured the page, never between frames: GPUI resolves the wheel and
    /// pan packets of a moving finger against the anchor it painted, so a
    /// scroll applied between two frames is either overwritten by the next
    /// packet or would have to give up when one arrived first.
    pub(in crate::chat) reservation_scroll_back: Option<ReservationScrollBack>,
    /// The anchor was carried into rows not yet measured.
    pub(in crate::chat) carried_anchor: bool,
    pub(in crate::chat) rows: Vec<TimelineRow>,
    pub(in crate::chat) turn_index_cache: TurnIndexCache,
    /// Rows that arrived or left while the reader looked, by identity,
    /// until they have risen in or folded away.
    pub(in crate::chat) rows_motion: crate::motion::list_rows::ListRows<bool, LeftRow>,
    /// The folds of the rows that left below the row being drawn, for a
    /// collapsed live Work Log to draw between its header and its live
    /// window, where they stood; whatever it leaves goes below the row.
    pub(in crate::chat) row_folds: Vec<AnyElement>,
    /// Where the timeline's shared elements (a steer, a turn's status) were
    /// drawn, so one that moves to another row travels there.
    pub(in crate::chat) shared: crate::motion::shared::SharedMoves,
    /// Where what opens an agent beside the thread was drawn in the rows,
    /// for the agent's card to grow out of it and shrink back into it. The
    /// chat keeps its own for what it draws, and resolves in both.
    pub(in crate::chat) side_origins: crate::motion::expand::Origins,
    /// The store's fold count for the thread at the last sync: a different
    /// one means the rows came from a fold, not from a live record.
    pub(in crate::chat) timeline_folds: u64,
    /// A Work Log was expanded or collapsed since the last sync, so the rows
    /// it gains or loses are the reader's own doing, not arrivals.
    pub(in crate::chat) rows_regrouped: bool,
    /// The delivery bubbles the chat drew as of the last frame: the timeline
    /// row that replaces one is the same message, already on screen.
    pub(in crate::chat) shown_deliveries: Vec<String>,
    /// The plan item whose block carries the decision its agent waits on.
    /// The row signature does not see approvals, so a change remeasures the
    /// rows of the plans gaining and losing it.
    pub(in crate::chat) deciding_plan: Option<String>,
    pub(in crate::chat) md_states: HashMap<String, MdState>,
    /// The pages of the visualizations near the screen, resident and evicted
    /// as the messages' Markdown is.
    pub(in crate::chat) visuals: HashMap<String, components::visualize::Visual>,
    /// The transcript's one text selection, across its messages; each
    /// message is the entry at its index in the timeline. It goes when the
    /// focus leaves the whole chat.
    pub(in crate::chat) selection: Entity<SelectionDocument>,
    /// The first and last entries, by id and index, as of the last sync:
    /// where they stand now says how far a page of history (or the pages
    /// dropped above the tail) moved every entry, and so the selection.
    entry_ends: Option<[(String, usize); 2]>,
    /// While a selection's gesture lasts, the list holds still; this is
    /// whether it was following its tail when the gesture began.
    selection_hold: Option<bool>,
    pub(in crate::chat) pending_md_builds: HashMap<String, PendingMarkdownBuild>,
    pub(in crate::chat) next_md_build_generation: u64,
    pub(in crate::chat) markdown_visible_rows: Range<usize>,
    pub(in crate::chat) markdown_scroll_top: Option<usize>,
    /// The rows the timeline paints in the frame being drawn: GPUI's list
    /// prepaints only the rows on screen.
    pub(in crate::chat) painted_rows: Rc<Cell<Option<(usize, usize)>>>,
    /// The first row painted while following the tail.
    pub(in crate::chat) painted_tail_start: Option<usize>,
    /// Open/closed keys for collapsibles other than activity details.
    pub(in crate::chat) expanded: HashSet<String>,
    /// The rewind panel a message opened, while it is open.
    pub(in crate::chat) rewind_panel: Option<rewind::RewindPanelState>,
    pub(in crate::chat) auto_activity_expansions: AutoActivityExpansions,
    pub(in crate::chat) command_panels: RefCell<CommandPanelCache>,
    pub(in crate::chat) inline_diffs: InlineDiffCache,
    pub(in crate::chat) tool_texts: tools::DerivedTexts,
    /// Whole tool outputs fetched for the open session, by item id.
    pub(in crate::chat) full_outputs: HashMap<String, FullOutput>,
    /// Records withheld for their size being read whole, by row.
    pub(in crate::chat) withheld_loads: HashMap<u64, WithheldLoad>,
    /// The output a Copy waits for, copied once it has loaded.
    pub(in crate::chat) copy_when_loaded: Option<String>,
    /// The timeline item the pointer is over: a message whose actions fade
    /// in, or a header, a row or a link whose hover fill or underline does.
    pub(in crate::chat) hover: crate::motion::ListHover,
    /// The timeline's glide to a turn or to its end; the reader's own
    /// scroll stops it.
    pub(in crate::chat) list_scroll: crate::motion::ListScroll,
    /// The thread and delivery key of a message sent while the reader was
    /// scrolled back, until it gets through or is refused.
    pub(in crate::chat) sent_message: Option<(String, String)>,
    pub(in crate::chat) session_key: Option<String>,
    /// The turn the reader jumped to (a palette hit, a thread anchor), by
    /// its turn number in the whole thread: a history page moves the held
    /// window's turn indices under it. Marked until the thread changes, the
    /// keyboard is put to use elsewhere, or a press lands inside it.
    pub(in crate::chat) highlighted_turn: Option<usize>,
    /// The call a row of the background work last pointed out, marked a
    /// while.
    pub(in crate::chat) located: Option<String>,
    located_unmark: Option<gpui::Task<()>>,
    /// Keyed `plan`, `user:<id>`, `assistant:<id>` or `error:<id>`.
    pub(in crate::chat) copied: CopiedMark,
    /// A system scrolling screenshot is driving the timeline: where it was
    /// before the capture moved it. History paging and tail following stay
    /// off until the capture ends so the origin keeps meaning the same pixel.
    pub(in crate::chat) capture: Option<CaptureSession>,
    _subscriptions: Vec<Subscription>,
}

/// Where the chat lays its timeline out this frame, handed by the chat as
/// its body is laid out, before the rows are.
#[derive(Clone, Default, PartialEq)]
pub(in crate::chat) struct TimelineLayout {
    /// The body's width, and how far past its right edge the list runs,
    /// under the panel: see [`ChatView::list_overhang`].
    pub(in crate::chat) width: gpui::Pixels,
    pub(in crate::chat) overhang: gpui::Pixels,
    /// How far the column's centre moves left, clear of the rail or the
    /// agent's card.
    pub(in crate::chat) shift: gpui::Pixels,
    /// Height of the phone row floating over the foot of the timeline, as
    /// far as it has risen in: zero while it is absent. The last row's
    /// padding holds it so the tail clears it.
    pub(in crate::chat) strip: gpui::Pixels,
    pub(in crate::chat) cwd: PathBuf,
    pub(in crate::chat) is_draft: bool,
}

impl TimelineView {
    pub(in crate::chat) fn new(
        workspace_store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        session_id: Option<String>,
        scene: &crate::motion::Epoch,
        chat: WeakEntity<ChatView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // GPUI's list, not gpui-base's `VirtualList`, which takes every row's
        // size up front: a row's height here is known only once it is laid out.
        // Every held row is measured, not only those near the screen: a row
        // the list has not laid out counts as no height, so the scrollbar
        // would span only what has been seen, and jump as the reader reaches
        // the rest.
        let height = f32::from(window.bounds().size.height);
        let overdraw = timeline_overdraw(height);
        let list_state = ListState::new(0, ListAlignment::Bottom, px(overdraw)).measure_all();
        list_state.set_follow_mode(FollowMode::Tail);
        let timeline = cx.entity().downgrade();
        let list_scroll = crate::motion::ListScroll::default();
        let glide = list_scroll.clone();
        list_state.set_scroll_handler(move |event, window, cx| {
            glide.cancel();
            let visible_rows = event.visible_range.clone();
            let timeline = timeline.clone();
            window.defer(cx, move |_, cx| {
                let _ = timeline.update(cx, |timeline, cx| {
                    timeline.set_markdown_visible_rows(visible_rows, cx);
                });
            });
        });
        let selection = transcript_selection(cx.entity().downgrade(), cx);
        let subscriptions = vec![
            cx.observe(&workspace_store, |this, _, cx| {
                this.timeline_stale = true;
                cx.notify();
            }),
            cx.subscribe(&selection, |this, _, gesture, cx| {
                this.hold_still_for_selection(*gesture, cx)
            }),
        ];
        let spine =
            cx.new(|_| spine::SpineView::new(workspace_store.clone(), session_id.clone(), scene));
        let mut this = Self {
            spine,
            workspace_store,
            window_state,
            session_id,
            chat,
            lead: None,
            layout: TimelineLayout::default(),
            motion: crate::motion::owned::Owned::in_scene(scene),
            scene: scene.clone(),
            followed: crate::scroll::FollowedList::new(&list_state),
            list_state,
            timeline_stale: false,
            timeline_revision: None,
            history_placeholder_height: px(0.),
            history_turn_height: None,
            history_unheld: None,
            history_turn_height_stale: false,
            had_unloaded_history: false,
            reservation_scroll_back: None,
            carried_anchor: false,
            rows: Vec::new(),
            turn_index_cache: TurnIndexCache::default(),
            rows_motion: crate::motion::list_rows::ListRows::over(ROWS_IN).in_scene(scene),
            row_folds: Vec::new(),
            shared: crate::motion::shared::SharedMoves::in_scene(scene),
            side_origins: crate::motion::expand::Origins::in_scene(scene),
            timeline_folds: 0,
            rows_regrouped: false,
            shown_deliveries: Vec::new(),
            deciding_plan: None,
            md_states: HashMap::new(),
            visuals: HashMap::new(),
            selection,
            entry_ends: None,
            selection_hold: None,
            pending_md_builds: HashMap::new(),
            next_md_build_generation: 0,
            markdown_visible_rows: 0..0,
            markdown_scroll_top: None,
            painted_rows: Rc::default(),
            painted_tail_start: None,
            expanded: HashSet::new(),
            rewind_panel: None,
            auto_activity_expansions: AutoActivityExpansions::default(),
            command_panels: RefCell::new(CommandPanelCache::new()),
            inline_diffs: InlineDiffCache::new(),
            tool_texts: tools::DerivedTexts::default(),
            full_outputs: HashMap::new(),
            withheld_loads: HashMap::new(),
            copy_when_loaded: None,
            located: None,
            located_unmark: None,
            hover: crate::motion::ListHover::new(crate::material::HOVER).in_scene(scene),
            list_scroll,
            sent_message: None,
            session_key: None,
            highlighted_turn: None,
            copied: CopiedMark::default(),
            capture: None,
            _subscriptions: subscriptions,
        };
        this.sync_markdown_states(cx);
        this
    }

    /// Show `session_id`'s thread, or none; the next sync finds the thread
    /// changed.
    pub(in crate::chat) fn set_session(
        &mut self,
        session_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.spine
            .update(cx, |spine, cx| spine.set_session(session_id.clone(), cx));
        self.session_id = session_id;
        self.timeline_stale = true;
        cx.notify();
    }

    /// The timeline is an agent's shown beside `lead`.
    pub(in crate::chat) fn set_lead(&mut self, lead: WeakEntity<ChatView>) {
        self.lead = Some(lead);
    }

    /// Text being selected holds still under the pointer: while a selection's
    /// gesture lasts, the list does not follow its tail, so a reply streaming
    /// in does not move the text being selected. Afterwards a click that
    /// selected nothing goes on following as before; a selection keeps the
    /// reader where it is, the following paused as a scroll up pauses it, to
    /// resume at the bottom. Pausing alone would not hold: the list resumes
    /// at the next layout that finds it at its bottom, as a press there is.
    fn hold_still_for_selection(&mut self, gesture: SelectionGesture, cx: &mut Context<Self>) {
        match gesture {
            SelectionGesture::Began => {
                self.selection_hold = Some(self.list_state.is_following_tail());
                // A bottom-aligned list at its end keeps the end in view
                // whatever its mode, so the text is held by the row and
                // offset at the top: what the list has scrolled, in pixels,
                // which it reports at its end too, measured from the start.
                let scrolled = -self.list_state.scroll_px_offset_for_scrollbar().y;
                self.list_state.set_follow_mode(FollowMode::Normal);
                self.list_state.scroll_to(ListOffset::default());
                self.list_state.scroll_by(scrolled);
            }
            SelectionGesture::Ended => {
                let Some(was_following) = self.selection_hold.take() else {
                    return;
                };
                let here = self.list_state.logical_scroll_top();
                self.list_state.set_follow_mode(FollowMode::Tail);
                if !was_following || self.selection.read(cx).has_selection() {
                    self.list_state.pause_following_tail();
                    self.list_state.scroll_to(here);
                }
                cx.notify();
            }
        }
    }

    /// Where the chat lays the timeline out this frame, handed as the chat
    /// renders, before the timeline is built in the same frame: a notify
    /// would build it again on the next frame as well.
    pub(in crate::chat) fn set_layout(&mut self, layout: TimelineLayout) {
        if layout.strip != self.layout.strip
            && let Some(last) = self.rows.len().checked_sub(1)
        {
            self.list_state.remeasure_items(last..last + 1);
        }
        self.layout = layout;
    }

    /// Whether the timeline is an agent's shown beside its lead.
    pub(in crate::chat) fn is_side(&self) -> bool {
        self.lead.is_some()
    }

    /// The lead of the agent whose timeline this is.
    pub(in crate::chat) fn side_lead(&self) -> Option<Entity<ChatView>> {
        self.lead.as_ref()?.upgrade()
    }

    /// The chat an agent opened from a row shows beside its thread: see
    /// [`super::side::agent_host`].
    pub(in crate::chat) fn agent_host(&self, cx: &App) -> Option<Entity<ChatView>> {
        super::side::agent_host(
            &self.chat,
            self.lead.as_ref(),
            self.session_id.is_some(),
            &self.window_state,
            cx,
        )
    }

    /// The jumped-to turn's mark goes; its colour transition fades it out.
    pub(in crate::chat) fn dismiss_highlight(&mut self, cx: &mut Context<Self>) {
        if self.highlighted_turn.take().is_some() {
            cx.notify();
        }
    }

    /// Land every message's reveal: the chat's scene shows, and what arrived
    /// while it readied itself unseen is there as it is on its first frame.
    pub(in crate::chat) fn settle_reveals(&self, cx: &mut App) {
        for md in self.md_states.values() {
            md.settle(cx);
        }
    }

    /// Whether the timeline can be drawn at rest for the chat's scene: its
    /// thread held whole (not loading, not catching up) and mirrored, its
    /// rows drawn (measured, at their anchor) and the Markdown of the ones
    /// on screen built, its diagrams drawn. The chat reads it only until its
    /// scene shows, drawing itself again with the timeline meanwhile.
    pub(in crate::chat) fn scene_ready(&self, cx: &App) -> bool {
        let Some(session_id) = self.session_id.as_deref() else {
            return true;
        };
        let store = self.workspace_store.read(cx);
        store.timeline_settled_for(session_id)
            && !self.timeline_stale
            && store.timeline_revision_for(Some(session_id), cx) == self.timeline_revision
            && (self.rows.is_empty() || self.painted_rows.get().is_some())
            && self.pending_md_builds.is_empty()
            && !self
                .md_states
                .values()
                .any(|md| md.state.read(cx).drawing_first_diagrams())
            && !self.carried_anchor
            && self.reservation_scroll_back.is_none()
    }
}

impl Render for TimelineView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.motion.sweep();
        self.side_origins.begin_frame();
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
        for key in self.rows_motion.settle(window, cx) {
            if let Some(row) = self.rows.iter().position(|row| row.identity == key) {
                self.list_state.remeasure_items(row..row + 1);
            }
        }
        self.painted_rows.set(None);
        self.shared
            .begin_frame(self.list_state.viewport_bounds(), cx);
        if self.carried_anchor {
            self.carried_anchor = !settle_carried_anchor(&self.list_state);
            if self.carried_anchor {
                // The next frame may be idle otherwise; make it render.
                let timeline = cx.entity().downgrade();
                window.on_next_frame(move |_, cx| {
                    let _ = timeline.update(cx, |_, cx| cx.notify());
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
        // more frame after every render. The timeline is only read, and the
        // store updated only when the report changes it: an update of either
        // builds the timeline again.
        let timeline = cx.entity().downgrade();
        window.defer(cx, move |_, cx| {
            let Some(timeline) = timeline.upgrade() else {
                return;
            };
            let measured = timeline
                .read(cx)
                .history_turn_height_due()
                .then(|| timeline.read(cx).measure_history_turn_height())
                .flatten();
            if let Some(height) = measured {
                timeline.update(cx, |timeline, cx| {
                    timeline.history_turn_height = Some(height);
                    timeline.history_turn_height_stale = false;
                    cx.notify();
                });
            }
            let timeline = timeline.read(cx);
            if timeline.capture.is_some() {
                return;
            }
            let Some(screens) =
                history_screens_covered(&timeline.list_state, timeline.leading_space())
            else {
                return;
            };
            let following_tail = timeline.list_state.is_following_tail();
            let store = timeline.workspace_store.clone();
            let session_id = timeline.session_id.clone();
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
        let placeholder = self.history_reservation_height(window, cx);
        if placeholder != self.history_placeholder_height && !self.rows.is_empty() {
            let anchor = self.list_state.logical_scroll_top();
            let following = self.list_state.is_following_tail();
            self.list_state.remeasure_items(0..1);
            if anchor.item_ix == 0 && !following {
                if self.sees_only_reservation(anchor.offset_in_item) {
                    // The reservation may now end above the reader.
                    self.carried_anchor = true;
                } else {
                    self.list_state.scroll_to(ListOffset {
                        item_ix: 0,
                        offset_in_item: (anchor.offset_in_item + placeholder
                            - self.history_placeholder_height)
                            .max(px(0.)),
                    });
                }
            }
            self.history_placeholder_height = placeholder;
        }
        let element = self.render_timeline(window, cx);
        // What a row replacing a bubble is checked against, at its next sync.
        self.shown_deliveries = self
            .workspace_store
            .read(cx)
            .delivery_messages_for(self.session_id.as_deref(), cx)
            .into_iter()
            .map(|(key, ..)| key)
            .collect();
        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .child(element)
            .child(self.render_settle_canvas(show_jump_to_latest, cx))
            .children(self.render_scroll_pill(show_jump_to_latest, window, cx))
    }
}

impl TimelineView {
    /// The padding left and right of the content column inside the list's
    /// width: the page inset on both sides, and on the right also the shift
    /// clear of the slot and the list's overhang under the panel. The rows
    /// and the scrollbar's track are both framed by it.
    pub(super) fn column_insets(&self, cx: &App) -> (gpui::Pixels, gpui::Pixels) {
        let inset = px(if self.window_state.read(cx).compact {
            crate::material::COMPACT_PAGE_INSET
        } else {
            CONTENT_MIN_PADDING
        });
        (inset, inset + self.layout.shift + self.layout.overhang)
    }

    /// The list under its scrollbar, or a fresh phone draft's empty state.
    fn render_timeline(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let TimelineLayout {
            width,
            overhang,
            ref cwd,
            is_draft,
            ..
        } = self.layout;
        let cwd = cwd.clone();
        // A fresh phone draft shows its project/model context above the focused composer.
        if compact && is_draft && self.rows.is_empty() {
            return self.render_compact_draft_empty(&cwd, cx);
        }
        let timeline = self.render_timeline_list(&cwd, cx);
        // GPUI's scrollbar extent excludes List padding, while wheel and
        // tail-resume calculations include it. Keep the inset outside the
        // list so every input path shares the same bottom and pixel offset.
        v_flex()
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
                            crate::scroll::page_viewport("timeline-bounce", &self.followed, content)
                        },
                    ),
            )
            // The spine stands at the conversation's left edge, whatever
            // the column does.
            .when(!compact && !self.is_side(), |timeline| {
                timeline.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .bottom(timeline_bottom_inset(cx))
                        .w(px(spine::SLOT))
                        .child(self.spine.clone()),
                )
            })
            .when(!window.is_inspector_picking(cx), |timeline| {
                let side = self.is_side();
                let (left, right) = self.column_insets(cx);
                // The track stands just right of the content column, framed
                // as the rows frame theirs, so it moves with the column and
                // with nothing else: not with what takes the slot beside it.
                timeline.child(
                    v_flex()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .bottom(timeline_bottom_inset(cx))
                        .items_center()
                        .pl(left)
                        .pr(right)
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .w_full()
                                .min_w_0()
                                .max_w(px(CONTENT_MAX_WIDTH))
                                .child(
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
                                        .bottom_0()
                                        .left(gpui::relative(1.))
                                        .w(TRACK)
                                        .child(
                                            Scrollbar::vertical(&self.list_state)
                                                .id("timeline-scrollbar")
                                                .motion(&self.motion)
                                                .viewport_from_layout(),
                                        ),
                                ),
                        ),
                )
            })
            .into_any_element()
    }

    /// The turn the reader is at, among those held, of the rows `painted`:
    /// the last with the thread's end on screen and the first with its
    /// start, and between them the one whose opening message is nearest the
    /// middle of the screen, or with none on screen, the one the middle of
    /// the screen is in.
    fn turn_being_read(&self, painted: Range<usize>) -> Option<usize> {
        let viewport = self.list_state.viewport_bounds();
        let middle = viewport.center().y;
        let on_screen = |index: usize| {
            self.list_state
                .bounds_for_item(index)
                .filter(|row| row.top() < viewport.bottom() && row.bottom() > viewport.top())
        };
        let last = self.rows.len().checked_sub(1)?;
        // The scrollbar's geometry, which keeps size hints for rows not
        // measured, as the jump to the latest row reads it.
        let (reach, offset) = (
            self.list_state.max_offset_for_scrollbar().y,
            self.list_state.scroll_px_offset_for_scrollbar().y,
        );
        let at_end = self.list_state.is_following_tail() || reach + offset <= px(1.);
        let at_start = offset >= px(-1.);
        let index = if at_end {
            last
        } else if at_start {
            0
        } else {
            painted
                .clone()
                .filter(|index| self.rows.get(*index).is_some_and(|row| row.first_in_turn))
                .filter_map(|index| {
                    let row = on_screen(index)?;
                    Some((index, f32::from(row.center().y - middle).abs()))
                })
                .min_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(index, _)| index)
                .or_else(|| {
                    painted.clone().find(|index| {
                        on_screen(*index)
                            .is_some_and(|row| row.top() <= middle && middle < row.bottom())
                    })
                })
                .unwrap_or(painted.start)
        };
        Some(self.rows.get(index)?.turn)
    }

    /// After the list's layout: the jump to the latest row follows the
    /// geometry the list's prepaint settled on, the command panels their
    /// screens, and the Markdown the rows the frame painted.
    fn render_settle_canvas(
        &self,
        show_jump_to_latest: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Canvas<()> {
        let list = self.list_state.clone();
        let glide = self.list_scroll.clone();
        let view = cx.entity().downgrade();
        let painted = self.painted_rows.clone();
        gpui::canvas(
            move |_, window, cx| {
                // List prepaint may change geometry after render
                // (resize, splice, or markdown remeasurement).
                // Reconcile the sibling control after that layout.
                if (jump_to_latest_visible(&list) && !glide.running()) != show_jump_to_latest {
                    let view = view.clone();
                    window.defer(cx, move |_, cx| {
                        let _ = view.update(cx, |_, cx| cx.notify());
                    });
                }
                // Only update the timeline when the frame changed something:
                // an update builds it again.
                let painted = painted.get();
                window.defer(cx, move |_, cx| {
                    let Some(view) = view.upgrade() else {
                        return;
                    };
                    let timeline = view.read(cx);
                    if let Some(turn) =
                        painted.and_then(|(start, end)| timeline.turn_being_read(start..end))
                    {
                        timeline
                            .spine
                            .clone()
                            .update(cx, |spine, cx| spine.set_reading(turn, cx));
                    }
                    let timeline = view.read(cx);
                    let panels = timeline.command_panels.borrow_mut().settle();
                    let painted =
                        painted.and_then(|(start, end)| timeline.painted_rows_change(start..end));
                    if panels.is_empty() && painted.is_none() {
                        return;
                    }
                    view.update(cx, |timeline, cx| {
                        timeline.apply_command_panel_changes(panels, cx);
                        if let Some(painted) = painted {
                            timeline.adopt_painted_rows(painted, cx);
                        }
                    });
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
    }
}

/// Vertical rhythm between turns. Turns are separated by space and typographic
/// hierarchy alone — there is deliberately no rule/divider under the user bubble.
pub(super) const TURN_GAP: f32 = 32.;

/// Vertical rhythm between the segments of one turn.
pub(super) const SEGMENT_GAP: f32 = 10.;

// ListState retains measured row heights, so variable-height turns use the same
// pixel geometry as scrolling rather than treating a long turn as one short row.
pub(super) fn history_screens_covered(list: &ListState, leading: gpui::Pixels) -> Option<f32> {
    let height = list.viewport_bounds().size.height;
    (height > px(0.)).then(|| {
        let above = -list.scroll_px_offset_for_scrollbar().y - leading;
        f32::from(above.max(px(0.))) / f32::from(height)
    })
}

pub(super) fn jump_to_latest_visible(list: &ListState) -> bool {
    let height = list.viewport_bounds().size.height;
    // The scrollbar geometry retains size hints for invalidated rows. Unlike
    // is_scrolled_to_end(), it does not require every offscreen row to have
    // been measured. Prepending shifts the extent and offset together.
    height > px(0.)
        && list.max_offset_for_scrollbar().y + list.scroll_px_offset_for_scrollbar().y > height
}

/// Move an anchor whose offset passes the end of its row onto the row that
/// offset lands in. A remeasure of the anchor row would otherwise clamp the
/// offset to that row's height. False while a row it passes is unmeasured.
pub(super) fn settle_carried_anchor(list: &ListState) -> bool {
    if list.is_following_tail() {
        return true;
    }
    let mut anchor = list.logical_scroll_top();
    let start = anchor.item_ix;
    let settled = loop {
        if anchor.item_ix + 1 >= list.item_count() {
            break true;
        }
        let Some(bounds) = list.bounds_for_item(anchor.item_ix) else {
            break false;
        };
        if anchor.offset_in_item < bounds.size.height {
            break true;
        }
        anchor.offset_in_item -= bounds.size.height;
        anchor.item_ix += 1;
    };
    if anchor.item_ix != start {
        list.scroll_to(anchor);
    }
    settled
}

/// Pixels between the top of the content and the top of the viewport. A list
/// anchored past its last row reports the whole content height; on screen it
/// is scrolled no further than the end.
pub(super) fn capture_scrolled(list: &ListState) -> gpui::Pixels {
    (-list.scroll_px_offset_for_scrollbar().y).min(list.max_offset_for_scrollbar().y)
}

pub(super) struct CaptureSession {
    anchor: ListOffset,
    following: bool,
    /// The row at the top of the viewport when the capture began; capture
    /// offsets are measured from it. A row, not a pixel count: rows measured
    /// for the first time during the capture change every pixel count above
    /// them.
    origin: ListOffset,
}

/// One tile's scroll, in pixels from the capture's starting top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureScroll {
    /// The offset reached: smaller than asked past the end of the measured
    /// content, larger above its start.
    pub reached: gpui::Pixels,
    /// Whether the scroll changed the list, so the frame on screen is stale.
    pub moved: bool,
}

/// The timeline as the surface of the platform's scrolling screenshot.
///
/// The capturer works in pixels relative to the viewport at the start of the
/// capture: it asks for the content that would sit `offset` below that top,
/// the timeline scrolls there as far as measured content allows, and the
/// platform reads back the next painted frame. The history reservation above
/// the first turn is blank and never captured.
impl TimelineView {
    /// The timeline's on-screen rectangle, when it has something to capture.
    pub(crate) fn capture_viewport(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        let bounds = self.list_state.viewport_bounds();
        (!self.rows.is_empty() && bounds.size.height > px(0.) && bounds.size.width > px(0.))
            .then_some(bounds)
    }

    pub(crate) fn capture_begin(&mut self, cx: &mut Context<Self>) -> bool {
        if self.capture_viewport().is_none() {
            return false;
        }
        if self.capture.is_none() {
            let list = &self.list_state;
            let anchor = list.logical_scroll_top();
            let following = list.is_following_tail();
            // Layout re-arms tail following whenever the bottom comes into
            // view, which the capture's last tile does; `Normal` keeps the
            // list where the capture put it.
            list.set_follow_mode(FollowMode::Normal);
            // An anchor past the last row is laid out from the end, but
            // `scroll_by` counts its pixel offset from the top of every row:
            // a scroll relative to it would land a viewport short. Re-anchor
            // at the row on screen first.
            if anchor.item_ix >= list.item_count() {
                list.scroll_by(-list.viewport_bounds().size.height);
            }
            self.capture = Some(CaptureSession {
                anchor,
                following,
                origin: list.logical_scroll_top(),
            });
            // Rows still rising settle at once: a tile shows each row whole.
            self.rows_motion.clear();
            self.reservation_scroll_back = None;
            cx.notify();
        }
        true
    }

    /// Scroll so the content `offset` pixels below the capture's starting top
    /// is at the top of the viewport. `None` outside a capture.
    pub(crate) fn capture_scroll(
        &mut self,
        offset: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> Option<CaptureScroll> {
        let session = self.capture.as_ref()?;
        let list = &self.list_state;
        let before = list.logical_scroll_top();
        // Pixel counts are only comparable within one measurement of the
        // rows, so the origin's is taken now, along with the target's.
        list.scroll_to(session.origin);
        // `scroll_by` counts from the top of every row even past the end,
        // where layout shows the list scrolled no further than the end.
        let origin = -list.scroll_px_offset_for_scrollbar().y;
        let end = list.max_offset_for_scrollbar().y;
        let target = (origin + offset)
            .min(end)
            .max(self.history_placeholder_height)
            .max(px(0.));
        list.scroll_by(target - origin);
        let after = list.logical_scroll_top();
        let moved =
            after.item_ix != before.item_ix || after.offset_in_item != before.offset_in_item;
        if moved {
            cx.notify();
        }
        Some(CaptureScroll {
            reached: capture_scrolled(list) - origin.min(end),
            moved,
        })
    }

    /// Whether a frame painted now shows the rows' content rather than a
    /// Markdown placeholder still being built.
    pub(crate) fn capture_settled(&self) -> bool {
        self.pending_md_builds.is_empty()
    }

    pub(crate) fn capture_end(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.capture.take() else {
            return;
        };
        let list = &self.list_state;
        list.set_follow_mode(FollowMode::Tail);
        if !session.following {
            list.scroll_to(session.anchor);
        }
        cx.notify();
    }
}

impl TimelineView {
    /// Mirror timeline markdown text into synchronous [`MarkdownState`] entities.
    pub(super) fn sync_markdown_states(&mut self, cx: &mut Context<Self>) {
        self.timeline_stale = false;
        // ListState owns follow intent: user scrolling pauses Tail until the
        // bottom is reached again. Content updates must not override that pause.
        let session_key = self.session_id.clone();
        let session_changed = session_key != self.session_key;
        if session_changed {
            self.expanded.clear();
            self.auto_activity_expansions
                .activate_session(session_key.as_deref());
        }
        let awaiting_activity_snapshot = self
            .auto_activity_expansions
            .awaiting_session_snapshot(session_key.as_deref());
        let history_available = self
            .workspace_store
            .read(cx)
            .history_available_for(self.session_id.as_deref());
        let continuity = if session_changed {
            TimelineContinuity::NewSession
        } else if history_available || self.had_unloaded_history {
            TimelineContinuity::PartialFirstTurn
        } else {
            TimelineContinuity::Complete
        };
        self.had_unloaded_history = history_available;
        let old_len = self.rows.len();
        let old_last_turn = self.rows.last().map(|row| row.turn);
        let folds = self
            .workspace_store
            .read(cx)
            .timeline_folds_for(self.session_id.as_deref());
        // Rows arrive live only from records streamed into the timeline the
        // reader is looking at. A fold (opening the thread, catching up on
        // what it missed, a page of history) brings rows that were already
        // there; the store holds the timeline still while it catches up, and
        // folds once it has.
        let regrouped = std::mem::take(&mut self.rows_regrouped);
        // The reader sees the rows change: a live record, or a Work Log they
        // expanded or collapsed.
        let seen = !session_changed
            && folds == self.timeline_folds
            && self.capture.is_none()
            && !cx.reduce_motion();
        self.timeline_folds = folds;
        // A collapsed Work Log's rows fold away from what they showed.
        let old_rows = (seen && regrouped).then(|| self.rows.clone());
        let (list_sync, activity_snapshot_keys, entries_moved) = self
            .workspace_store
            .read(cx)
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| {
                    let list_sync = self.turn_index_cache.sync(
                        &mut self.rows,
                        &timeline.turns,
                        &timeline.entries,
                        &self.expanded,
                        continuity,
                    );
                    let activity_snapshot_keys = awaiting_activity_snapshot
                        .then(|| auto_activity_snapshot_keys(&timeline.entries));
                    let entries_moved = entries_moved(&mut self.entry_ends, &timeline.entries);
                    (list_sync, activity_snapshot_keys, entries_moved)
                },
                cx,
            )
            .unwrap_or_else(|| {
                let list_sync = self.turn_index_cache.sync(
                    &mut self.rows,
                    &[],
                    &[],
                    &self.expanded,
                    continuity,
                );
                (list_sync, None, entries_moved(&mut self.entry_ends, &[]))
            });
        // The selection numbers entries by their place in the window, so it
        // moves with them; another thread's goes below.
        if !session_changed {
            match entries_moved {
                Some(0) => {}
                Some(by) => self
                    .selection
                    .update(cx, |selection, cx| selection.move_entries(by, cx)),
                None => self
                    .selection
                    .update(cx, |selection, cx| selection.clear(cx)),
            }
        }

        if let Some(session_key) = session_key.as_deref()
            && let Some(keys) = activity_snapshot_keys
        {
            self.auto_activity_expansions
                .hydrate_session_snapshot(session_key, keys);
        }

        let requested_turn = session_key
            .as_deref()
            .and_then(|session_id| self.workspace_store.read(cx).pending_chat_turn(session_id));
        if session_changed {
            self.painted_rows.set(None);
            self.md_states.clear();
            self.visuals.clear();
            self.selection
                .update(cx, |selection, cx| selection.clear(cx));
            self.pending_md_builds.clear();
            self.command_panels.borrow_mut().clear();
            self.inline_diffs.clear();
            self.tool_texts.clear();
            self.full_outputs.clear();
            self.withheld_loads.clear();
            self.highlighted_turn = None;
            self.located = None;
            self.located_unmark = None;
            self.session_key = session_key;
            self.painted_tail_start = None;
            self.markdown_visible_rows = tail_row_window(self.rows.len());
            self.markdown_scroll_top = Some(self.rows.len());
        }

        if !matches!(list_sync, ListSync::None) {
            self.list_scroll.settle(&self.list_state);
        }
        match list_sync {
            ListSync::None => {}
            ListSync::Reset { count } => {
                self.reservation_scroll_back = None;
                self.history_turn_height = None;
                self.history_unheld = None;
                self.list_state.reset(count);
                if session_changed {
                    // Reset also clears stale item focus handles. A newly opened
                    // session always starts actively following its tail.
                    self.list_state.set_follow_mode(FollowMode::Tail);
                }
            }
            ListSync::Incremental {
                splices,
                remeasure,
                carried,
            } => {
                let anchor = self.list_state.logical_scroll_top();
                let following = self.list_state.is_following_tail();
                let carried_offset = (!following
                    && carried.iter().any(|range| range.contains(&anchor.item_ix)))
                .then_some(anchor.offset_in_item);
                // Rows above every row: an earlier history page, or the
                // records that complete the partial first turn.
                let prepended = splices
                    .first()
                    .filter(|(range, _)| old_len > 0 && *range == (0..0))
                    .map(|(_, count)| *count);
                let arrived = if seen {
                    crate::motion::list_rows::arrived_rows(&splices, &carried, old_len)
                } else {
                    Vec::new()
                };
                let left = old_rows
                    .map(|old| self.rows_leaving(&old, cx))
                    .unwrap_or_default();
                for (range, count) in splices.iter().rev() {
                    // The row above a splice changes its padding (the former
                    // last row hands its edge padding to the new one); its
                    // cached height must not keep it.
                    if range.start > 0 {
                        self.list_state
                            .remeasure_items(range.start - 1..range.start);
                    }
                    self.list_state.splice(range.clone(), *count);
                    // A page requested before a capture began still lands
                    // during it; the capture's anchors name the same rows as
                    // before.
                    if let Some(capture) = &mut self.capture {
                        for offset in [&mut capture.anchor, &mut capture.origin] {
                            if range.end <= offset.item_ix {
                                offset.item_ix = offset.item_ix - range.len() + count;
                            } else if range.contains(&offset.item_ix) {
                                offset.item_ix = range.start;
                            }
                        }
                    }
                }
                if let Some(count) = prepended {
                    // The edge padding and the reservation move to the new
                    // first turn. Remove them from the former first turn
                    // without moving its content.
                    self.list_state.remeasure_items(count..count + 1);
                    if anchor.item_ix == 0 && !following {
                        self.keep_reading(anchor, count);
                    }
                }
                for index in remeasure {
                    self.list_state.remeasure_items(index..index + 1);
                }
                // A splice over the anchor moves it to the first new row at
                // offset zero; the carried content starts where the old row
                // did, so the old offset still names the same pixel. It may
                // pass that row's end until `settle_carried_anchor` measures it.
                if let Some(offset) = carried_offset {
                    self.list_state.scroll_to(ListOffset {
                        item_ix: self.list_state.logical_scroll_top().item_ix,
                        offset_in_item: offset,
                    });
                    self.carried_anchor = true;
                }
                let arrived = if regrouped {
                    // An expanded live window was the collapsed run's, on
                    // screen already.
                    arrived
                        .into_iter()
                        .filter(|(index, _)| {
                            self.rows
                                .get(*index)
                                .is_some_and(|row| row.part != RowPart::WorkLogLive)
                        })
                        .collect()
                } else {
                    arrived
                };
                self.stamp_arrivals(arrived, old_last_turn, cx);
                self.fold_away(left, cx);
            }
        }

        if self.list_state.is_following_tail() {
            self.markdown_visible_rows = self.markdown_tail_window();
            self.markdown_scroll_top = Some(self.rows.len());
        }

        let requested_row = requested_turn.and_then(|turn| rows_of_turn(&self.rows, turn).next());
        if let (Some(turn), Some(row)) = (requested_turn, requested_row) {
            // Within the thread on screen the jump glides there; a thread
            // opened at a turn starts there.
            self.jump_to_row(row, !session_changed, cx);
            self.highlighted_turn = Some(
                self.workspace_store
                    .read(cx)
                    .absolute_turn_for(self.session_key.as_deref(), turn),
            );
            if let Some(session_id) = self.session_key.as_deref() {
                self.workspace_store.update(cx, |store, _cx| {
                    store.take_pending_chat_turn(session_id, turn);
                });
            }
        }

        self.sync_markdown_residency(requested_row, cx);
        self.sync_deciding_plan(cx);
    }

    /// Invalidate the measured heights of every row of `turn`.
    fn remeasure_turn(&self, turn: usize) {
        let rows = rows_of_turn(&self.rows, turn);
        if !rows.is_empty() {
            self.list_state.remeasure_items(rows);
        }
    }

    /// Invalidate the measured height of the row that renders the activity
    /// `entry_id`; an expanded Work Log gives each activity a row of its own.
    pub(in crate::chat) fn remeasure_activity(&self, turn: usize, entry_id: &str, cx: &App) {
        match self.row_of_activity(turn, entry_id, cx) {
            Some(row) => self.list_state.remeasure_items(row..row + 1),
            None => self.remeasure_turn(turn),
        }
    }

    /// The row that renders the activity `entry_id` of `turn`.
    fn row_of_activity(&self, turn: usize, entry_id: &str, cx: &App) -> Option<usize> {
        self.workspace_store
            .read(cx)
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| {
                    let entries = &timeline.entries;
                    let start = entries.partition_point(|entry| entry.turn < turn);
                    let index = start
                        + entries[start..]
                            .iter()
                            .take_while(|entry| entry.turn == turn)
                            .position(|entry| entry.id == entry_id)?;
                    row_of_entry(&self.rows, index, turn)
                },
                cx,
            )
            .flatten()
    }

    /// Brings the row that renders the activity `entry_id` of `turn` to the
    /// top of the timeline, gliding there.
    pub(in crate::chat) fn scroll_to_activity(
        &mut self,
        turn: usize,
        entry_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.row_of_activity(turn, entry_id, cx) else {
            return;
        };
        self.jump_to_row(row, true, cx);
        self.sync_markdown_residency(Some(row), cx);
        cx.notify();
    }

    /// Brings the call `item_id` into view and marks it a while: the row of
    /// the background work that names it was pressed.
    pub(in crate::chat) fn locate(&mut self, item_id: &str, cx: &mut Context<Self>) {
        let turn = self
            .workspace_store
            .read(cx)
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| {
                    timeline
                        .entries
                        .iter()
                        .find(|entry| entry.id == item_id)
                        .map(|entry| entry.turn)
                },
                cx,
            )
            .flatten();
        let Some(turn) = turn else {
            return;
        };
        // A finished turn's calls fold into its work log, which opens to
        // show this one.
        let segment = self.row_of_activity(turn, item_id, cx).and_then(|row| {
            let start = self.rows.get(row)?.entry_range.start;
            self.workspace_store
                .read(cx)
                .with_timeline_for(
                    self.session_id.as_deref(),
                    |timeline| timeline.entries.get(start).map(|entry| entry.id.clone()),
                    cx,
                )
                .flatten()
        });
        if let Some(segment) = segment {
            let key = crate::chat::model::work_log_key(&segment);
            if !self.expanded.contains(&key) {
                self.toggle_expanded(turn, &key, cx);
            }
        }
        self.scroll_to_activity(turn, item_id, cx);
        let item_id = item_id.to_owned();
        self.located = Some(item_id.clone());
        self.located_unmark = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(4))
                .await;
            let _ = this.update(cx, |timeline, cx| {
                if timeline.located.as_deref() == Some(item_id.as_str()) {
                    timeline.located = None;
                    cx.notify();
                }
            });
        }));
    }

    /// Puts `row` at the top of the timeline, gliding there or at once, and
    /// stops following the tail.
    fn jump_to_row(&mut self, row: usize, glide: bool, cx: &mut Context<Self>) {
        self.reservation_scroll_back = None;
        self.list_state.pause_following_tail();
        let target = ListOffset {
            item_ix: row,
            offset_in_item: px(0.),
        };
        if glide {
            self.list_scroll.scroll_to(
                &self.list_state,
                crate::motion::ScrollTarget::Row(target),
                cx,
            );
        } else {
            self.list_scroll.cancel();
            self.list_state.scroll_to(target);
        }
        self.markdown_visible_rows = viewport_row_window(row, self.rows.len());
        self.markdown_scroll_top = Some(row);
    }

    /// The composer just put a message in the outbox. A reader scrolled
    /// back from the end is taken there once it gets through, unless they
    /// turned that off; a reader following the tail is already there.
    pub(in crate::chat) fn await_sent_message(&mut self, cx: &App) {
        let store = self.workspace_store.read(cx);
        self.sent_message = self
            .session_id
            .clone()
            .filter(|_| {
                !self.list_state.is_following_tail() && store.settings().scroll_to_latest_on_send
            })
            .and_then(|session| {
                // The newest delivery still on its way is the one just sent.
                let key = store
                    .delivery_messages_for(Some(&session), cx)
                    .into_iter()
                    .rfind(|(_, _, failure, acknowledged)| failure.is_none() && !acknowledged)?
                    .0;
                Some((session, key))
            });
    }

    /// Glide to the end once the message sent from behind it gets through:
    /// the host has it and the thread holds it, in the timeline or in the
    /// queue behind a running turn. A send the host refused stays where the
    /// reader is.
    pub(in crate::chat) fn land_sent_message(&mut self, cx: &App) {
        let Some((session, key)) = &self.sent_message else {
            return;
        };
        if self.session_id.as_ref() != Some(session) {
            self.sent_message = None;
            return;
        }
        let delivery = self
            .workspace_store
            .read(cx)
            .delivery_messages_for(Some(session), cx)
            .into_iter()
            .find(|(pending, ..)| pending == key);
        match delivery {
            Some((_, _, None, _)) => {}
            Some((_, _, Some(_), _)) => self.sent_message = None,
            None => {
                self.sent_message = None;
                if !self.list_state.is_following_tail() {
                    self.list_scroll.scroll_to(
                        &self.list_state,
                        crate::motion::ScrollTarget::End,
                        cx,
                    );
                }
            }
        }
    }
}

/// The walk back into a page that replaced the reservation the reader had
/// scrolled into. See [`TimelineView::apply_reservation_scroll_back`].
#[derive(Debug, Clone, Copy)]
pub(super) struct ReservationScrollBack {
    distance: gpui::Pixels,
    /// A frame has laid the page out since the walk back was requested, so
    /// `scroll_by` can count its rows.
    measured: bool,
}

impl TimelineView {
    /// Whether the turn height the reservation counts at is to be measured.
    fn history_turn_height_due(&self) -> bool {
        self.history_unheld.is_some()
            && (self.history_turn_height_stale || self.history_turn_height.is_none())
    }

    /// The mean height of a held turn, from the rows the last layout
    /// measured: none while a row has not been laid out yet, nor while the
    /// rows fit the viewport, which leaves the scrollbar no extent to read
    /// it from, nor while a drag holds that extent at its start.
    fn measure_history_turn_height(&self) -> Option<gpui::Pixels> {
        let list = &self.list_state;
        let turns = self.rows.last()?.turn + 1;
        if list.is_scrollbar_dragging() || list.is_scrolled_to_end().is_none() {
            return None;
        }
        let height = list.viewport_bounds().size.height;
        let held = list.max_offset_for_scrollbar().y + height - self.leading_space();
        (held > px(0.)).then(|| held / turns as f32)
    }

    /// The reservation for the turns not held: each counted at the mean
    /// height of a held turn. Until a layout has measured one, or when the
    /// unheld history is only the start of the first held turn, a screen's
    /// height: the window's, which this render already knows; the list's own
    /// height is only measured by this frame's layout.
    fn history_reservation_height(&mut self, window: &Window, cx: &App) -> gpui::Pixels {
        let store = self.workspace_store.read(cx);
        let session_id = self.session_id.as_deref();
        if !store.history_available_for(session_id) {
            self.history_unheld = None;
            return px(0.);
        }
        let unheld = store.absolute_turn_for(session_id, 0);
        if self.history_unheld.replace(unheld) != Some(unheld) {
            self.history_turn_height_stale = true;
        }
        match self.history_turn_height {
            Some(height) if unheld > 0 => height * unheld as f32,
            _ => window.viewport_size().height,
        }
    }

    /// Space above the first turn's content: the edge padding plus the history
    /// reservation. Both move to the new first row when a page lands.
    pub(super) fn leading_space(&self) -> gpui::Pixels {
        px(TIMELINE_EDGE_PADDING) + self.history_placeholder_height
    }

    /// Hold a reader who rested at `anchor` on the row now at `row`. The
    /// first row carries the leading space, so a reader there who stays at
    /// the first row keeps the offset; one whose row has moved down, and who
    /// sees it, keeps the pixel inside the padding or the reservation, which
    /// now belongs to the rows above. A list anchor cannot precede its row
    /// (rows above it stay unpainted), so the render after the next frame
    /// walks back over them once layout has measured them.
    fn keep_reading(&mut self, anchor: ListOffset, row: usize) {
        if anchor.item_ix != 0 || row == 0 {
            self.list_state.scroll_to(ListOffset {
                item_ix: row,
                offset_in_item: anchor.offset_in_item,
            });
            return;
        }
        if self.sees_only_reservation(anchor.offset_in_item) {
            // No held row is on screen to keep in place, so the reader keeps
            // their place in the thread, as the scrollbar shows it, even
            // where the page now stands there instead of the reservation.
            self.list_state.scroll_to(anchor);
            self.carried_anchor = true;
            return;
        }
        let into_reservation = self.leading_space() - anchor.offset_in_item;
        self.list_state.scroll_to(ListOffset {
            item_ix: row,
            offset_in_item: (-into_reservation).max(px(0.)),
        });
        if into_reservation > px(0.) {
            let distance = self
                .reservation_scroll_back
                .take()
                .map_or(px(0.), |back| back.distance);
            self.reservation_scroll_back = Some(ReservationScrollBack {
                distance: distance + into_reservation,
                measured: false,
            });
        }
    }

    /// Whether a reader `offset` into the first row sees only the
    /// reservation, and none of the held rows below it.
    fn sees_only_reservation(&self, offset: gpui::Pixels) -> bool {
        offset + self.list_state.viewport_bounds().size.height <= self.leading_space()
    }

    /// Walk back over a landed page inside the render that follows the frame
    /// which measured it. The walk is a relative scroll, so packets that moved
    /// the reader in between (a finger still panning, or a fling ticking on
    /// the frame) stay applied: those resolved against the painted anchor,
    /// and the anchor painted after this render carries the walk back.
    pub(super) fn apply_reservation_scroll_back(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(back) = self.reservation_scroll_back.as_mut() else {
            return;
        };
        if !back.measured {
            back.measured = true;
            // The next frame may be idle otherwise; make it render.
            let chat = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = chat.update(cx, |_, cx| cx.notify());
            });
            return;
        }
        let back = self.reservation_scroll_back.take().expect("checked above");
        let list = &self.list_state;
        let anchor = list.logical_scroll_top();
        // Following the tail or resting past the end means the reader left
        // for the bottom in between; the walk back would pull them off it.
        if self.capture.is_none() && !list.is_following_tail() && anchor.item_ix < list.item_count()
        {
            list.scroll_by(-back.distance);
        }
    }
}

/// The breathing room between the timeline and the composer. A phone has
/// none: its rows already inset themselves and the screen is short. The
/// header gets none on any screen: it has no divider, so the eye measures the
/// room under its title to the first thing below, and only
/// [`TIMELINE_EDGE_PADDING`] keeps the first row off it.
pub(super) fn timeline_bottom_inset(cx: &App) -> gpui::Pixels {
    if crate::window_seam::is_mobile(cx) {
        px(0.)
    } else {
        px(TIMELINE_BOTTOM_INSET)
    }
}

/// Desktop timeline inset below the list, in pixels.
pub(super) const TIMELINE_BOTTOM_INSET: f32 = 8.;

/// How many places every entry moved since `ends` were taken (a page of
/// history landing above them, or the pages above the tail dropped), and
/// `ends` taken anew; none when neither of them is still there.
fn entries_moved(
    ends: &mut Option<[(String, usize); 2]>,
    entries: &[Arc<TimelineEntry>],
) -> Option<i64> {
    let moved = ends.as_ref().map_or(Some(0), |ends| {
        // The last first: a record streamed in leaves it where it was.
        ends.iter().rev().find_map(|(id, at)| {
            let now = if entries.get(*at).is_some_and(|entry| entry.id == *id) {
                *at
            } else {
                entries.iter().position(|entry| entry.id == *id)?
            };
            Some(now as i64 - *at as i64)
        })
    });
    *ends = entries
        .first()
        .zip(entries.last())
        .map(|(first, last)| [(first.id.clone(), 0), (last.id.clone(), entries.len() - 1)]);
    moved
}

/// Padding inside the list on its first and last row, in pixels. Unlike
/// `TIMELINE_BOTTOM_INSET` it scrolls with the content: the header and composer still
/// clip the timeline flush, but a reader at either end sees a little air
/// between the edge turn and the chrome. It lives on the rows rather than on
/// the List element, so the scrollbar, the wheel and tail-resume all measure
/// the same extent.
pub(super) const TIMELINE_EDGE_PADDING: f32 = 8.;

#[cfg(test)]
mod tests;
