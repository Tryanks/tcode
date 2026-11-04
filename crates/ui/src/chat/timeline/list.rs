use super::*;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

impl ChatView {
    /// The timeline's virtual list: one element per [`TimelineRow`], each
    /// drawn in the centred content column with the rhythm around it.
    pub(in crate::chat) fn render_timeline_list(
        &mut self,
        cwd: &Path,
        cx: &mut Context<Self>,
    ) -> gpui::List {
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

        let highlighted_turn = self.highlighted_turn.and_then(|turn| {
            let first = self
                .workspace_store
                .read(cx)
                .absolute_turn_for(self.session_id.as_deref(), 0);
            turn.checked_sub(first)
        });
        let item_count = self.rows.len();
        let item_cwd = cwd.to_path_buf();
        // Read before the list lays out, which holds its state.
        let following = self.list_state.is_following_tail();
        list(
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
                let now = cx.background_executor().now();
                let (above, below) = this.rows_motion.leaving(row.identity, now);
                let gone = |left: Vec<crate::motion::list_rows::Left<'_, LeftRow>>| {
                    left.into_iter()
                        .map(|left| (left.key, left.value.clone(), left.progress))
                        .collect::<Vec<_>>()
                };
                let (above, below) = (gone(above), gone(below));
                if !above.is_empty() || !below.is_empty() {
                    window.request_animation_frame();
                }
                // A Work Log part keeps the Work Log's rhythm as it folds.
                let rem = gpui::rems(0.25).to_pixels(window.rem_size());
                let mut fold = |(key, left): (u64, LeftRow),
                                progress: f32,
                                above: bool,
                                window: &mut Window,
                                cx: &mut Context<Self>| {
                    // Drawn from what it was, under its own keys, so what it
                    // draws keeps clear of a row that shows it now.
                    let motion = this.motion.clone();
                    this.motion = motion.scope(format!("leaving-{key}"));
                    let content = this
                        .render_row(
                            (
                                &left.row,
                                &left.turn,
                                &item_cwd,
                                &left.own,
                                &[],
                                (last_user_id.as_deref(), last_assistant_id.as_deref()),
                            ),
                            window,
                            cx,
                        )
                        .content;
                    this.motion = motion;
                    RowGlide::leaving(
                        ("timeline-row-leaving", key),
                        progress,
                        div()
                            .w_full()
                            .map(|fold| if above { fold.pb(rem) } else { fold.pt(rem) })
                            .child(content),
                    )
                    .owned(&this.motion)
                    .into_any_element()
                };
                let above: Vec<_> = above
                    .into_iter()
                    .map(|(key, left, progress)| fold((key, left), progress, true, window, cx))
                    .collect();
                this.row_folds = below
                    .into_iter()
                    .map(|(key, left, progress)| fold((key, left), progress, false, window, cx))
                    .collect();
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
                let below = std::mem::take(&mut this.row_folds);
                let moving = !cx.reduce_motion() && this.capture.is_none();
                let arrival = |row: &TimelineRow| {
                    moving
                        .then(|| this.rows_motion.entering(row.identity, now))
                        .flatten()
                };
                let entering = arrival(&row);
                // The reader follows the reveal: the row opens with its text,
                // and nothing stands below the text yet to show.
                let revealing = (moving && following)
                    .then(|| own.last().and_then(|entry| this.md_states.get(&entry.id)))
                    .flatten()
                    .and_then(|md| md.state.read(cx).unrevealed(now));
                // The row below took the end's padding from this one; this
                // one gives it up as that one rises.
                let below_arrival = this
                    .rows
                    .get(index + 1)
                    .and_then(arrival)
                    .filter(|(next, _)| next.arrival == Arrival::Appended)
                    .map(|(_, progress)| progress);
                if entering.is_some() || below_arrival.is_some() {
                    window.request_animation_frame();
                }
                // The followed end of a running turn glides, and so does the
                // end the row now rising below took over from: the record that
                // brings a row often reshapes the one before it, as prose ends
                // a run of tool calls. So does a row still revealing, whatever
                // came after it or ended its turn.
                let glide = match entering {
                    Some((_, progress)) => Glide::Enter(progress),
                    None if revealing.is_some()
                        || moving
                            && turn.running
                            && following
                            && (index + 1 == item_count || below_arrival.is_some()) =>
                    {
                        Glide::Tail {
                            content: row.content,
                        }
                    }
                    None => Glide::Rest,
                };
                // What arrives in place of a row, or between two, opens its
                // own room; what arrives at the end takes the end's padding
                // from the row above.
                let opening = entering
                    .filter(|(arrival, _)| arrival.arrival != Arrival::Appended)
                    .map_or(1., |(_, progress)| progress);
                let padding = below_arrival.unwrap_or(1.) * opening;
                // The rhythm below a row is set by the row after it.
                let strip = this.phone_strip_height;
                let edge_below = |next: Option<&TimelineRow>| match next {
                    None => px(TIMELINE_EDGE_PADDING) + strip,
                    Some(next) if next.turn != row.turn => px(TURN_GAP),
                    // An expanded Work Log's parts keep the Work Log's own rhythm.
                    Some(next)
                        if matches!(next.part, RowPart::WorkLogActivity | RowPart::WorkLogLive) =>
                    {
                        rem
                    }
                    Some(_) => px(SEGMENT_GAP),
                };
                let inserted = |row: &TimelineRow| {
                    arrival(row)
                        .filter(|(arrival, _)| arrival.arrival == Arrival::Inserted)
                        .map(|(_, progress)| progress)
                };
                let mut edge = edge_below(this.rows.get(index + 1));
                // A row inserted below this one changes the rhythm it ends
                // with; the change goes as fast as the row opens, so what was
                // on screen does not move.
                if let Some(progress) = this.rows.get(index + 1).and_then(inserted) {
                    let before = edge_below(
                        this.rows[index + 1..]
                            .iter()
                            .find(|row| inserted(row).is_none()),
                    );
                    edge = before + (edge - before) * progress;
                }
                if let Some((_, until)) = revealing {
                    this.rows_motion.hold(row.identity, until);
                }
                // A row that opens its turn brings the turn's trailer with it;
                // otherwise the trailer was on screen, and moves down to it.
                let trailer = rendered.trailer.map(|(trailer, gap)| {
                    let (trailer_glide, gap_scale) = match entering {
                        Some((arrival, progress)) if arrival.note => (Glide::Enter(progress), 1.),
                        _ => (Glide::Rest, opening),
                    };
                    RowGlide::new(
                        ("timeline-trailer-glide", row.identity),
                        trailer_glide,
                        trailer.when(gap, |trailer| trailer.pt(px(SEGMENT_GAP) * gap_scale)),
                    )
                    .owned(&this.motion)
                });
                // The turn the reader jumped to is marked, and the mark
                // fades in and out.
                let highlighted = highlighted_turn == Some(row.turn);
                let highlight = cx.theme().list_active;
                let highlight = this.motion.transition(
                    format!("timeline-highlight-{}", row.identity),
                    if highlighted {
                        highlight
                    } else {
                        highlight.opacity(0.)
                    },
                    GLIDE,
                    window,
                    cx,
                );
                let history_activity = (index == 0)
                    .then(|| {
                        let loading = this
                            .workspace_store
                            .read(cx)
                            .history_loading_for(this.session_id.as_deref());
                        this.motion.shown(
                            "history-activity",
                            loading.then_some(()),
                            SHOW,
                            window,
                            cx,
                        )
                    })
                    .flatten();
                let painted = this.painted_rows.clone();
                let inset = px(if this.window_state.read(cx).compact {
                    crate::material::COMPACT_PAGE_INSET
                } else {
                    CONTENT_MIN_PADDING
                });
                v_flex()
                    .id(("timeline-row", row.identity))
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
                    .px(inset)
                    .pr(inset + this.body.shift())
                    .rounded(crate::material::radius_card(cx))
                    .bg(highlight)
                    // A press inside the mark lets it go, and still does what
                    // it does: it selects, follows a link, presses a button.
                    // A drag that selects starts with that press.
                    .when(highlighted, |item| {
                        item.capture_any_mouse_down(
                            cx.listener(|this, _, _, cx| this.dismiss_highlight(cx)),
                        )
                    })
                    .when(index == 0, |item| item.pt(px(TIMELINE_EDGE_PADDING)))
                    .pb(padding * edge)
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
                                    .children(history_activity.map(|shown| {
                                        shown.fade(
                                            div()
                                                .id("history-activity")
                                                .w_full()
                                                .h(px(24.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(
                                                    crate::widgets::spinner::Spinner::new()
                                                        .xsmall()
                                                        .color(cx.theme().muted_foreground),
                                                ),
                                        )
                                    })),
                            )
                        },
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .flex()
                            .flex_col()
                            .children(above)
                            .child(
                                RowGlide::new(
                                    ("timeline-row-glide", row.identity),
                                    glide,
                                    rendered.content,
                                )
                                .withheld(revealing.map_or(px(0.), |(height, _)| height))
                                .owned(&this.motion),
                            )
                            .children(below)
                            .children(trailer),
                    )
                    .into_any_element()
            }),
        )
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .flex_1()
        .min_h_0()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::tests::*;
    use crate::test_frames::draw_settled;
    use crate::window_state::WindowState;
    use gpui::{
        FocusHandle, Modifiers, MouseButton, Pixels, TestAppContext, VisualTestContext, point, size,
    };
    use tcode_core::session::TurnMeta;

    /// Four turns; the last ran a command and answered with a link.
    fn turns() -> Timeline {
        let mut timeline = synthetic_markdown_timeline(3);
        timeline.turns.push(TurnMeta::default());
        timeline.entries.extend([
            at_turn(entry("user-3", user_item("Where is it documented?")), 3),
            at_turn(command("command-0"), 3),
            at_turn(
                entry(
                    "assistant-3",
                    assistant("[Example](https://example.com) has the details."),
                ),
                3,
            ),
        ]);
        timeline
    }

    /// The chat beside another pane, as the shell's dock lays them out.
    struct Panes {
        chat: Entity<ChatView>,
        other: FocusHandle,
    }

    impl Render for Panes {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            h_flex()
                .size_full()
                .child(div().flex_1().min_w_0().h_full().child(self.chat.clone()))
                .child(
                    div()
                        .id("other-pane")
                        .track_focus(&self.other)
                        .w(px(200.))
                        .h_full(),
                )
        }
    }

    fn open(cx: &mut TestAppContext) -> (TwoThreads, Entity<Panes>, &mut VisualTestContext) {
        let threads = seed_two_threads(cx, turns());
        let store = threads.store.clone();
        let window_state = cx.new(|_| WindowState::new(false));
        let (panes, cx) = cx.add_window_view(|window, cx| Panes {
            chat: cx.new(|cx| {
                ChatView::new(store, window_state, Some(threads.first.clone()), window, cx)
            }),
            other: cx.focus_handle(),
        });
        cx.simulate_resize(size(px(1_224.), px(900.)));
        // The app's window is active: an inactive one holds no focus path.
        cx.update(|window, _| window.activate_window());
        draw_settled(cx);
        // The shell gives the keyboard to the composer of the thread it opens.
        let chat = panes.read_with(cx, |panes, _| panes.chat.clone());
        chat.update_in(cx, |chat, window, cx| chat.focus_composer(window, cx));
        draw_settled(cx);
        (threads, panes, cx)
    }

    /// Jump as the palette does, to a turn of the thread on screen.
    fn jump(threads: &TwoThreads, turn: usize, cx: &mut VisualTestContext) {
        let thread = threads.first.clone();
        threads.store.update(cx, |store, cx| {
            store.show_thread_at_turn(thread, turn, cx);
        });
        draw_settled(cx);
    }

    fn marked(panes: &Entity<Panes>, cx: &mut VisualTestContext) -> Option<usize> {
        panes.read_with(cx, |panes, cx| panes.chat.read(cx).highlighted_turn)
    }

    fn first_row_of(
        panes: &Entity<Panes>,
        turn: usize,
        cx: &mut VisualTestContext,
    ) -> gpui::Bounds<Pixels> {
        let index = panes.read_with(cx, |panes, cx| {
            rows_of_turn(&panes.chat.read(cx).rows, turn).start
        });
        cx.debug_bounds(format!("timeline-row-{index}").leak())
            .expect("the turn's first row is on screen")
    }

    fn click(position: gpui::Point<Pixels>, cx: &mut VisualTestContext) {
        cx.simulate_click(position, Modifiers::default());
        draw_settled(cx);
    }

    /// The mark stays on the turn jumped to, however long the reader reads
    /// and wherever else they click; a press inside it lets it go and still
    /// does what it does there.
    #[gpui::test]
    fn a_press_inside_the_jumped_to_turn_lets_its_mark_go(cx: &mut TestAppContext) {
        let (threads, panes, cx) = open(cx);

        jump(&threads, 1, cx);
        assert_eq!(marked(&panes, cx), Some(1));
        jump(&threads, 3, cx);
        assert_eq!(marked(&panes, cx), Some(3), "the mark did not move");
        cx.executor().advance_clock(Duration::from_secs(60));
        draw_settled(cx);
        click(first_row_of(&panes, 1, cx).center(), cx);
        assert_eq!(
            marked(&panes, cx),
            Some(3),
            "time or a click on another turn let the mark go"
        );

        // The answer is the last message painted, so its block is this one.
        let link = cx
            .debug_bounds("markdown-block-0")
            .expect("the answer's link");
        click(point(link.left() + px(8.), link.center().y), cx);
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
        assert_eq!(
            marked(&panes, cx),
            None,
            "a click on the link kept the mark"
        );
        cx.executor().advance_clock(Duration::from_secs(1));
        draw_settled(cx);
        assert_eq!(marked(&panes, cx), None, "the mark came back");

        jump(&threads, 3, cx);
        assert_eq!(marked(&panes, cx), Some(3));
        let header = cx
            .debug_bounds("worklog-header-3-command-0")
            .expect("the turn's work log");
        click(header.center(), cx);
        assert!(
            cx.debug_bounds("activity-row-command-0").is_some(),
            "the work log did not open"
        );
        assert_eq!(
            marked(&panes, cx),
            None,
            "a click on the work log kept the mark"
        );

        // A drag inside selects, and lets the mark go as it starts.
        jump(&threads, 3, cx);
        let (start, end) = (
            point(link.left() + px(80.), link.center().y),
            point(link.left() + px(160.), link.center().y),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        draw_settled(cx);
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        draw_settled(cx);
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        draw_settled(cx);
        let selected = panes.read_with(cx, |panes, cx| {
            panes.chat.read(cx).selection.read(cx).selected_text(cx)
        });
        assert!(!selected.is_empty(), "the drag selected nothing");
        assert_eq!(marked(&panes, cx), None);
        threads.shutdown(cx);
    }

    /// The keyboard rests in the composer while the reader reads the turn
    /// jumped to; putting it to use there, or taking it out of the chat,
    /// lets the mark go, and so does showing another thread.
    #[gpui::test]
    fn the_jumped_to_turn_mark_goes_when_the_keyboard_is_used(cx: &mut TestAppContext) {
        let (mut threads, panes, cx) = open(cx);
        let chat = panes.read_with(cx, |panes, _| panes.chat.clone());

        jump(&threads, 3, cx);
        assert_eq!(marked(&panes, cx), Some(3));
        cx.simulate_keystrokes("a");
        draw_settled(cx);
        assert_eq!(
            marked(&panes, cx),
            None,
            "typing in the composer kept the mark"
        );
        // A key the field binds to an action is typing too.
        jump(&threads, 3, cx);
        cx.simulate_keystrokes("backspace");
        draw_settled(cx);
        assert_eq!(
            marked(&panes, cx),
            None,
            "deleting in the composer kept the mark"
        );

        jump(&threads, 3, cx);
        let composer = cx.debug_bounds("chat-composer").expect("the composer");
        click(
            point(composer.left() + px(80.), composer.top() + px(24.)),
            cx,
        );
        assert_eq!(
            marked(&panes, cx),
            None,
            "a click on the composer kept the mark"
        );

        jump(&threads, 3, cx);
        let other = panes.read_with(cx, |panes, _| panes.other.clone());
        cx.update(|window, cx| window.focus(&other, cx));
        draw_settled(cx);
        assert_eq!(
            marked(&panes, cx),
            None,
            "focusing another pane kept the mark"
        );

        chat.update_in(cx, |chat, window, cx| chat.focus_composer(window, cx));
        jump(&threads, 3, cx);
        assert_eq!(marked(&panes, cx), Some(3));
        cx.deactivate_window();
        draw_settled(cx);
        assert_eq!(
            marked(&panes, cx),
            None,
            "moving to another window kept the mark"
        );

        cx.update(|window, _| window.activate_window());
        chat.update_in(cx, |chat, window, cx| chat.focus_composer(window, cx));
        jump(&threads, 3, cx);
        assert_eq!(marked(&panes, cx), Some(3));
        // Opening the terminal hands it the keyboard, inside the chat view.
        let first = threads.first.clone();
        threads.store.update(cx, |store, cx| {
            store.toggle_terminal_panel_for(first, cx);
            cx.notify();
        });
        draw_settled(cx);
        let drawer = chat.read_with(cx, |chat, _| chat.terminal_drawer());
        assert!(
            cx.update(|window, cx| {
                gpui::Focusable::focus_handle(drawer.read(cx), cx).contains_focused(window, cx)
            }),
            "the terminal did not take the keyboard"
        );
        assert_eq!(
            marked(&panes, cx),
            None,
            "the terminal taking the keyboard kept the mark"
        );

        chat.update_in(cx, |chat, window, cx| chat.focus_composer(window, cx));
        jump(&threads, 3, cx);
        assert_eq!(marked(&panes, cx), Some(3));
        let second = threads.second.clone();
        threads.bind(
            &chat,
            &second,
            single_assistant_timeline("other", "Other"),
            cx,
        );
        draw_settled(cx);
        assert_eq!(marked(&panes, cx), None, "another thread kept the mark");
        threads.shutdown(cx);
    }

    /// A page of history read in above the turn jumped to leaves the mark on
    /// that turn, not on the turn now at its place in the held window.
    #[gpui::test]
    fn the_mark_stays_on_its_turn_as_history_is_read_in_above(cx: &mut TestAppContext) {
        use crate::test_frames::draw;

        let events = (0..400)
            .flat_map(|turn| {
                [
                    agent::AgentEvent::TurnStarted {
                        turn_id: turn.to_string(),
                    },
                    user_event(&format!("user-{turn}"), &format!("Question {turn}")),
                    answer_event(&format!("answer-{turn}"), &format!("Answer {turn}.")),
                    agent::AgentEvent::TurnCompleted {
                        turn_id: turn.to_string(),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    },
                ]
            })
            .collect();
        let mut thread = ScriptedThread::new(cx, events, 4 * 390, false);
        let store = thread.store.clone();
        let (view, cx) = thread.open(cx);
        // A frame as the display draws it, then the page it asked for.
        let frame = |thread: &mut ScriptedThread, cx: &mut VisualTestContext| -> Option<u64> {
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
            let before = thread.answer_page(8)?;
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(250));
            Some(before)
        };
        let settle = |thread: &mut ScriptedThread, cx: &mut VisualTestContext| {
            let pages = std::iter::from_fn(|| frame(thread, cx)).take(100).count();
            assert!(pages < 100, "the prefetch settles");
        };
        settle(&mut thread, cx);
        let first_held = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, _| store.absolute_turn_for(Some(THREAD), 0))
        };
        let turn = first_held(cx) + 1;

        store.update(cx, |store, cx| {
            store.show_thread_at_turn(THREAD.into(), turn, cx);
        });
        let marked =
            |cx: &mut VisualTestContext| view.read_with(cx, |chat, _| chat.highlighted_turn);
        settle(&mut thread, cx);
        assert!(
            first_held(cx) + 1 < turn,
            "no page was read in above the turn"
        );
        assert!(marked(cx).is_some());

        draw_settled(cx);
        let bubble = cx
            .debug_bounds(format!("user-bubble-user-{turn}").leak())
            .expect("the turn jumped to stays on screen");
        cx.simulate_click(bubble.center(), Modifiers::default());
        draw_settled(cx);
        assert_eq!(
            marked(cx),
            None,
            "the turn jumped to was no longer the one marked"
        );
    }
}
