use super::*;

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
                let now = cx.background_executor().now();
                let moving = !cx.reduce_motion() && this.capture.is_none();
                let arrival = |row: &TimelineRow| {
                    let arrival = this.arrivals.get(&row.identity)?;
                    (moving && !arrival.settled(now)).then(|| (*arrival, arrival.progress(now)))
                };
                let entering = arrival(&row);
                // The row below took the end's padding from this one; this
                // one gives it up as that one rises.
                let below = this
                    .rows
                    .get(index + 1)
                    .and_then(arrival)
                    .filter(|(next, _)| next.arrival == Arrival::Appended)
                    .map(|(_, progress)| progress);
                if entering.is_some() || below.is_some() {
                    window.request_animation_frame();
                }
                // The followed end of a running turn glides, and so does the
                // end the row now rising below took over from: the record that
                // brings a row often reshapes the one before it, as prose ends
                // a run of tool calls.
                let glide = match entering {
                    Some((_, progress)) => Glide::Enter(progress),
                    None if moving
                        && turn.running
                        && following
                        && (index + 1 == item_count || below.is_some()) =>
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
                let padding = below.unwrap_or(1.) * opening;
                // The rhythm below a row is set by the row after it.
                let rem = gpui::rems(0.25).to_pixels(window.rem_size());
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
                // A row that opens its turn brings the turn's trailer with it;
                // otherwise the trailer was on screen, and moves down to it.
                let trailer = rendered.trailer.map(|(trailer, gap)| {
                    let (trailer_glide, gap_scale) = match entering {
                        Some((arrival, progress)) if arrival.brings_turn => {
                            (Glide::Enter(progress), 1.)
                        }
                        _ => (Glide::Rest, opening),
                    };
                    RowGlide::new(
                        ("timeline-trailer-glide", row.identity),
                        trailer_glide,
                        trailer.when(gap, |trailer| trailer.pt(px(SEGMENT_GAP) * gap_scale)),
                    )
                });
                // A turn a search result jumped to is marked, and the mark
                // fades in and out.
                let highlight = cx.theme().list_active;
                let highlight = gpui_base::motion::transition(
                    gpui::ElementId::from(("timeline-highlight", row.identity)),
                    if this.highlighted_turn == Some(row.turn) {
                        highlight
                    } else {
                        highlight.opacity(0.)
                    },
                    crate::motion::settle(),
                    window,
                    cx,
                );
                let history_activity = (index == 0)
                    .then(|| {
                        let loading = this
                            .workspace_store
                            .read(cx)
                            .history_loading_for(this.session_id.as_deref());
                        crate::motion::shown("history-activity", loading.then_some(()), window, cx)
                    })
                    .flatten();
                let painted = this.painted_rows.clone();
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
                    .px(px(if this.window_state.read(cx).compact {
                        crate::material::COMPACT_PAGE_INSET
                    } else {
                        CONTENT_MIN_PADDING
                    }))
                    .rounded(crate::material::radius_card(cx))
                    .bg(highlight)
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
                            .child(RowGlide::new(
                                ("timeline-row-glide", row.identity),
                                glide,
                                rendered.content,
                            ))
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
