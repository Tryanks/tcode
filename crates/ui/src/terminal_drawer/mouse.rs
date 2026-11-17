use super::*;

impl TerminalDrawer {
    pub(super) fn grid_point_and_side(
        &self,
        terminal_id: u64,
        position: gpui::Point<Pixels>,
    ) -> Option<((usize, usize), SelectionSide)> {
        let geometry = *self.grid_bounds.borrow().get(&terminal_id)?;
        Some(grid_point_and_side(
            f32::from(position.x - geometry.bounds.left()),
            f32::from(position.y - geometry.bounds.top()),
            geometry.cols,
            geometry.rows,
            geometry.cell_width,
            geometry.cell_height,
        ))
    }

    pub(super) fn terminal_mouse_down(
        &mut self,
        terminal_id: u64,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        window.request_virtual_keyboard();
        let Some((point, side)) = self.grid_point_and_side(terminal_id, event.position) else {
            return;
        };
        self.workspace_store.update(cx, |store, _cx| {
            store.activate_terminal_for(self.session_id.clone().unwrap_or_default(), terminal_id)
        });
        let workspace_store = self.workspace_store.clone();
        let mut stop_propagation = false;
        #[cfg(target_os = "linux")]
        let primary_text = (event.button == MouseButton::Middle)
            .then(|| cx.read_from_primary().and_then(|item| item.text()))
            .flatten();
        workspace_store.read(cx).with_terminal_workspace_for(
            self.session_id.as_deref(),
            |workspace| {
                if let Some(entry) = workspace.terminal(terminal_id) {
                    let mode = entry.terminal.mode();
                    if mappings::routes_mouse(mode, event.modifiers.shift) {
                        if let Some(button) = term_mouse_button(event.button)
                            && let Some(bytes) = mappings::mouse_button_report(
                                GridPoint {
                                    row: point.0,
                                    column: point.1,
                                },
                                button,
                                term_modifiers(event.modifiers),
                                true,
                                mode,
                            )
                        {
                            entry.terminal.write_raw(bytes);
                        }
                        if event.button == MouseButton::Right {
                            stop_propagation = true;
                        }
                        return;
                    }
                    if event.button == MouseButton::Right {
                        if entry.terminal.selected_text().is_none() {
                            entry
                                .terminal
                                .start_selection(SelectionKind::Semantic, point, side);
                        }
                        return;
                    }
                    #[cfg(target_os = "linux")]
                    if event.button == MouseButton::Middle {
                        if let Some(text) = primary_text {
                            let text =
                                prepare_terminal_paste(&text, mode.contains(Mode::BRACKETED_PASTE));
                            entry.terminal.write_input(text.into_bytes());
                        }
                        stop_propagation = true;
                        return;
                    }
                    if event.button != MouseButton::Left {
                        return;
                    }
                    if terminal_link_modifier(event.modifiers, cfg!(target_os = "macos"))
                        && let Some(link) = entry.terminal.hyperlink_at(point.0, point.1)
                    {
                        self.pressed_link = Some((terminal_id, link.url));
                        return;
                    }
                    let action = self.selection_drag.on_down(
                        terminal_id,
                        ScreenPoint {
                            x: f32::from(event.position.x),
                            y: f32::from(event.position.y),
                        },
                        point,
                        side,
                        event.click_count,
                        event.modifiers.shift,
                    );
                    match action {
                        SelectionDragAction::None => {}
                        SelectionDragAction::ClearAndWait => entry.terminal.clear_selection(),
                        SelectionDragAction::Start { kind, point, side } => {
                            entry.terminal.start_selection(kind, point, side);
                        }
                        SelectionDragAction::Update { point, side } => {
                            entry.terminal.update_selection(point, side);
                        }
                        SelectionDragAction::StartSimpleAndUpdate { .. } => {
                            unreachable!("mouse down cannot start a drag")
                        }
                    }
                }
            },
            cx,
        );
        if stop_propagation {
            cx.stop_propagation();
        }
        cx.notify();
    }

    pub(super) fn terminal_mouse_move(
        &mut self,
        terminal_id: u64,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((point, _)) = self.grid_point_and_side(terminal_id, event.position) else {
            self.hovered_link = None;
            return;
        };
        let now = cx.background_executor().now();
        let workspace_store = self.workspace_store.clone();
        workspace_store.read(cx).with_terminal_workspace_for(
            self.session_id.as_deref(),
            |workspace| {
                let Some(entry) = workspace.terminal(terminal_id) else {
                    return;
                };
                let mode = entry.terminal.mode();
                if mappings::routes_mouse(mode, event.modifiers.shift) {
                    if self
                        .selection_drag
                        .should_report_mouse_move(terminal_id, point)
                        && let Some(bytes) = mappings::mouse_move_report(
                            GridPoint {
                                row: point.0,
                                column: point.1,
                            },
                            event.pressed_button.and_then(term_mouse_button),
                            term_modifiers(event.modifiers),
                            mode,
                        )
                    {
                        entry.terminal.write_raw(bytes);
                    }
                    return;
                }
                if terminal_link_modifier(event.modifiers, cfg!(target_os = "macos")) {
                    if self
                        .last_link_hover
                        .is_none_or(|last| now - last >= Duration::from_millis(16))
                    {
                        self.last_link_hover = Some(now);
                        self.hovered_link = entry
                            .terminal
                            .hyperlink_at(point.0, point.1)
                            .map(|link| (terminal_id, link));
                    }
                } else {
                    self.hovered_link = None;
                }
            },
            cx,
        );
        cx.notify();
    }

    /// Carries the selection being dragged on `terminal_id` to the pointer,
    /// wherever it is, and scrolls while it is past the grid's top or bottom.
    pub(super) fn drag_selection(
        &mut self,
        terminal_id: u64,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) {
            // The release went somewhere this drawer never heard it.
            self.end_selection_drag(terminal_id, cx);
            return;
        }
        if terminal_link_modifier(event.modifiers, cfg!(target_os = "macos")) {
            return;
        }
        let Some(geometry) = self.grid_bounds.borrow().get(&terminal_id).copied() else {
            return;
        };
        let Some((point, side)) = self.grid_point_and_side(terminal_id, event.position) else {
            return;
        };
        let action = self.selection_drag.on_move(
            terminal_id,
            ScreenPoint {
                x: f32::from(event.position.x),
                y: f32::from(event.position.y),
            },
            point,
            side,
        );
        let scrolls = self.workspace_store.read(cx).with_terminal_workspace_for(
            self.session_id.as_deref(),
            |workspace| {
                let entry = workspace.terminal(terminal_id)?;
                match action {
                    SelectionDragAction::None => return None,
                    SelectionDragAction::StartSimpleAndUpdate {
                        anchor,
                        anchor_side,
                        point,
                        side,
                    } => {
                        entry
                            .terminal
                            .start_selection(SelectionKind::Simple, anchor, anchor_side);
                        entry.terminal.update_selection(point, side);
                    }
                    SelectionDragAction::Update { point, side } => {
                        entry.terminal.update_selection(point, side);
                    }
                    SelectionDragAction::ClearAndWait | SelectionDragAction::Start { .. } => {
                        unreachable!("unexpected mouse move action")
                    }
                }
                Some(
                    !entry.terminal.mode().contains(Mode::ALT_SCREEN)
                        && entry.terminal.history_size() > 0,
                )
            },
            cx,
        );
        let Some(Some(scrolls)) = scrolls else {
            return;
        };
        let delta = scrolls
            .then(|| gpui_base::AutoScroll::compute_delta(event.position.y, geometry.bounds))
            .flatten();
        self.drag_scroll.last_drag_position = Some(event.position);
        self.drag_scroll.set(delta, cx, move |delta, drawer, cx| {
            drawer.drag_scroll_tick(terminal_id, delta, cx)
        });
        cx.notify();
    }

    /// One frame of a dragged selection's scrolling: `delta` pixels toward the
    /// live screen (negative toward the history), the selection's far end
    /// staying under the pointer as the content moves.
    pub(super) fn drag_scroll_tick(
        &mut self,
        terminal_id: u64,
        delta: Pixels,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self.drag_scroll.last_drag_position else {
            return;
        };
        let Some((point, side)) = self.grid_point_and_side(terminal_id, position) else {
            return;
        };
        let lines = self.whole_lines(terminal_id, -f32::from(delta));
        self.workspace_store.read(cx).with_terminal_workspace_for(
            self.session_id.as_deref(),
            |workspace| {
                if let Some(entry) = workspace.terminal(terminal_id) {
                    entry.terminal.scroll(lines);
                    entry.terminal.update_selection(point, side);
                }
            },
            cx,
        );
        cx.notify();
    }

    /// The left button came up, on the grid or anywhere else.
    pub(super) fn end_selection_drag(&mut self, terminal_id: u64, cx: &mut Context<Self>) {
        self.drag_scroll.stop();
        if self.selection_drag.on_up(terminal_id) {
            #[cfg(target_os = "linux")]
            if let Some(text) = self
                .workspace_store
                .read(cx)
                .with_terminal_workspace_for(
                    self.session_id.as_deref(),
                    |workspace| {
                        workspace
                            .terminal(terminal_id)
                            .and_then(|entry| entry.terminal.selected_text())
                            .map(|selection| selection.text)
                    },
                    cx,
                )
                .flatten()
            {
                cx.write_to_primary(ClipboardItem::new_string(text));
            }
        }
        cx.notify();
    }

    pub(super) fn terminal_mouse_up(
        &mut self,
        terminal_id: u64,
        event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let released_url = self
            .grid_point_and_side(terminal_id, event.position)
            .and_then(|(point, _side)| {
                self.workspace_store
                    .read(cx)
                    .with_terminal_workspace_for(
                        self.session_id.as_deref(),
                        |workspace| {
                            let entry = workspace.terminal(terminal_id)?;
                            let mode = entry.terminal.mode();
                            if mappings::routes_mouse(mode, event.modifiers.shift) {
                                if let Some(button) = term_mouse_button(event.button)
                                    && let Some(bytes) = mappings::mouse_button_report(
                                        GridPoint {
                                            row: point.0,
                                            column: point.1,
                                        },
                                        button,
                                        term_modifiers(event.modifiers),
                                        false,
                                        mode,
                                    )
                                {
                                    entry.terminal.write_raw(bytes);
                                }
                            } else if event.button == MouseButton::Left
                                && terminal_link_modifier(
                                    event.modifiers,
                                    cfg!(target_os = "macos"),
                                )
                            {
                                let released = entry
                                    .terminal
                                    .hyperlink_at(point.0, point.1)
                                    .map(|link| link.url);
                                if let (Some((pressed_id, pressed)), Some(released)) =
                                    (self.pressed_link.take(), released)
                                    && pressed_id == terminal_id
                                    && pressed == released
                                {
                                    return Some(released);
                                }
                            }
                            None
                        },
                        cx,
                    )
                    .flatten()
            });
        if let Some(released_url) = released_url {
            cx.open_url(&released_url);
        }
        self.end_selection_drag(terminal_id, cx);
        self.pressed_link = None;
    }
}
