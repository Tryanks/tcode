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
                // An expanded Work Log's parts keep the Work Log's own rhythm.
                let continued = this.rows.get(index + 1).is_some_and(|next| {
                    matches!(next.part, RowPart::WorkLogActivity | RowPart::WorkLogLive)
                });
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
        .min_h_0()
    }
}
