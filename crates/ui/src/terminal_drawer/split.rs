use super::*;

impl TerminalDrawer {
    /// The drawer's one or two panes in their resizable group, and the
    /// group's state.
    pub(super) fn render_split_body(
        &mut self,
        terminals: &[(u64, Rc<crate::store::ClientTerminal>)],
        active_id: Option<u64>,
        active_split: Option<tcode_protocol::TerminalSplitStatus>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Entity<gpui_base::ResizableState>) {
        // Splitting opens the second pane out of nothing beside or under the
        // first, which gives way; closing the split shrinks it away again.
        if active_split.is_some() {
            self.last_split = active_split;
        }
        let direction = active_split
            .or(self.last_split)
            .map_or(TerminalSplitDirection::Horizontal, |split| split.direction);
        let axis = match direction {
            TerminalSplitDirection::Horizontal => gpui::Axis::Horizontal,
            TerminalSplitDirection::Vertical => gpui::Axis::Vertical,
        };
        self.split_glide.turn(axis);
        let axis_ix = usize::from(axis == gpui::Axis::Vertical);
        let reduced = cx.reduce_motion();
        let state = self.split_states[axis_ix].clone();
        let body_size = self.body_size.get();
        let extent = match axis {
            gpui::Axis::Horizontal => body_size.width,
            gpui::Axis::Vertical => body_size.height,
        };
        // Both panes are sized, so the glide lands each where it was headed
        // (the 1px handle between them).
        let half = ((extent - px(1.)) / 2.).max(px(0.));
        let rest = if active_split.is_some() {
            (extent - px(1.) - half).max(px(0.))
        } else {
            extent
        };
        let frame = self.split_glide.sample(
            &[
                crate::motion::resize_glide::Panel::sized(rest, true),
                crate::motion::resize_glide::Panel::sized(half, active_split.is_some()),
            ],
            !reduced,
            &state,
            window,
            cx,
        );
        let (first, second) = match (active_split, self.last_split) {
            (Some(split), _) => (Some(split.first), Some(split.second)),
            (None, Some(last)) => (
                active_id,
                Some(if active_id == Some(last.first) {
                    last.second
                } else {
                    last.first
                }),
            ),
            (None, None) => (active_id, None),
        };
        let paired = frame.mounted(1);
        // Each pane of a pair carries a 2px gutter toward the handle.
        let gutter = |pane: AnyElement, leading: bool| {
            div()
                .debug_selector(move || {
                    if leading {
                        "terminal-pane-first".into()
                    } else {
                        "terminal-pane-second".into()
                    }
                })
                .size_full()
                .when(paired, |pane| match (axis, leading) {
                    (gpui::Axis::Horizontal, true) => pane.pr(px(2.)),
                    (gpui::Axis::Horizontal, false) => pane.pl(px(2.)),
                    (gpui::Axis::Vertical, true) => pane.pb(px(2.)),
                    (gpui::Axis::Vertical, false) => pane.pt(px(2.)),
                })
                .child(pane)
                .into_any_element()
        };
        // "Starting…" fades out under the terminal once it has one.
        let starting = crate::motion::CrossFade::new(
            "terminal-starting",
            usize::from(first.is_some()),
            2,
            CHANGE,
            window,
            cx,
        );
        let mut layers = Vec::new();
        if starting.shows(0) {
            layers.push((
                0,
                div()
                    .p(cx.theme().tokens.spacing.md)
                    .child(crate::tr!("terminal.starting"))
                    .into_any_element(),
            ));
        }
        if let Some(id) = first {
            layers.push((
                1,
                div()
                    .size_full()
                    .child(self.render_terminal(id, terminals, active_id, cx))
                    .into_any_element(),
            ));
        }
        let first = starting
            .fill()
            .render(layers)
            .size_full()
            .into_any_element();
        let first = resizable_panel()
            .size_range(frame.range(0, gpui_base::PANEL_MIN_SIZE..Pixels::MAX))
            .child(frame.hold(0, gutter(first, true)));
        let second = second.filter(|_| paired).map(|id| {
            let pane = self.render_terminal(id, terminals, active_id, cx);
            // No size of its own: at rest the group splits the body evenly
            // until a drag, as the glide lands it.
            resizable_panel()
                .size_range(frame.range(1, gpui_base::PANEL_MIN_SIZE..Pixels::MAX))
                .child(frame.hold(1, gutter(pane, false)))
        });
        let body: AnyElement = match axis {
            gpui::Axis::Horizontal => h_resizable("terminal-split-h"),
            gpui::Axis::Vertical => v_resizable("terminal-split-v"),
        }
        .with_state(&state)
        .child(first)
        // Not `children`: the group's replaces the panels added before.
        .when_some(second, |group, second| group.child(second))
        .into_any_element();
        (body, state)
    }

    pub(super) fn resize_panes_on_prepaint(
        &self,
        active_id: Option<u64>,
        active_split: Option<tcode_protocol::TerminalSplitStatus>,
        state: &Entity<gpui_base::ResizableState>,
    ) -> impl FnOnce(Bounds<Pixels>, &mut Window, &mut App) + 'static {
        let workspace_store = self.workspace_store.clone();
        let (cell_width, cell_height) = (self.cell_width, self.cell_height);
        let split_state = state.clone();
        let body_size = self.body_size.clone();
        let session_id = self.session_id.clone();
        move |bounds, window, cx| {
            body_size.set(bounds.size);
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            // Each pane shown, with the room it has.
            let panes: [Option<(u64, f32, f32)>; 2] = match active_split {
                None => [active_id.map(|id| (id, width, height)), None],
                Some(split) => {
                    // Panel sizes from the resize handle; before any
                    // drag the group splits the axis evenly (1px
                    // handle). Each pane also carries a 2px gutter
                    // (`pr`/`pl`/`pb`/`pt`) toward the handle.
                    let sizes = split_state.read(cx).sizes();
                    let axis = match split.direction {
                        TerminalSplitDirection::Horizontal => width,
                        TerminalSplitDirection::Vertical => height,
                    };
                    let (first, second) = match sizes.as_slice() {
                        [first, second] => (f32::from(*first), f32::from(*second)),
                        _ => ((axis - 1.) / 2., (axis - 1.) / 2.),
                    };
                    match split.direction {
                        TerminalSplitDirection::Horizontal => [
                            Some((split.first, first - 2., height)),
                            Some((split.second, second - 2., height)),
                        ],
                        TerminalSplitDirection::Vertical => [
                            Some((split.first, width, first - 2.)),
                            Some((split.second, width, second - 2.)),
                        ],
                    }
                }
            };
            let scale_factor = window.scale_factor();
            let cell_width_px = (cell_width * scale_factor).round().max(1.) as u32;
            let cell_height_px = (cell_height * scale_factor).round().max(1.) as u32;
            workspace_store.read(cx).with_terminal_workspace_for(
                session_id.as_deref(),
                |workspace| {
                    for (terminal_id, pane_width, pane_height) in panes.into_iter().flatten() {
                        let Some(entry) = workspace.terminal(terminal_id) else {
                            continue;
                        };
                        let cols = ((pane_width - 2. * PANE_PADDING) / cell_width)
                            .floor()
                            .max(2.) as usize;
                        let rows = ((pane_height - 2. * PANE_PADDING) / cell_height)
                            .floor()
                            .max(2.) as usize;
                        entry.terminal.resize_with_cell_size(
                            cols,
                            rows,
                            cell_width_px,
                            cell_height_px,
                        );
                    }
                },
                cx,
            );
        }
    }
}
