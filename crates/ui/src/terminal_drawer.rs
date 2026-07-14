use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Bounds, ClipboardItem, ContentMask, Context, Entity, FocusHandle, Focusable,
    FontFeatures, FontStyle, FontWeight, Hsla, InputHandler, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Point, Render, ScrollWheelEvent, StatefulInteractiveElement as _, Styled as _, Task,
    TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, canvas, div, fill, font, point,
    prelude::FluentBuilder as _, px, rgb, size,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, ElementExt as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    resizable::{h_resizable, resizable_panel, v_resizable},
    v_flex,
};
use term::{
    Cell, Color, CursorShape, HyperlinkMatch, SelectionKind, TermEvent, TermState,
    mappings::{self, GridPoint, Modifiers as TermModifiers, MouseButton as TermMouseButton},
};

use tcode_runtime::app::{AppState, MAX_TERMINALS_PER_SESSION, TerminalSplitDirection};

const FONT_SIZE: f32 = 13.;
#[cfg(target_os = "macos")]
const TERMINAL_FONT_FAMILY: &str = "Menlo";
#[cfg(target_os = "windows")]
const TERMINAL_FONT_FAMILY: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const TERMINAL_FONT_FAMILY: &str = "DejaVu Sans Mono";
const PANE_PADDING_X: f32 = 8.;
const PANE_PADDING_Y: f32 = 5.;
const PANE_BORDER: f32 = 1.;

#[derive(Clone, Copy)]
struct GridGeometry {
    bounds: Bounds<Pixels>,
    cols: usize,
    rows: usize,
    cell_width: f32,
    cell_height: f32,
}

struct TerminalEventSubscription {
    receiver: async_channel::Receiver<TermEvent>,
    _task: Task<()>,
}

#[derive(Clone)]
struct MarkedText {
    terminal_id: u64,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridTextStyle {
    fg: Color,
    bg: Color,
    bold: bool,
    italic: bool,
    underline: bool,
    selected: bool,
    cursor: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BatchedTextRun {
    row: usize,
    start_col: usize,
    text: String,
    /// The number of non-spacer grid cells, matching Zed's batching model.
    cell_count: usize,
    style: GridTextStyle,
}

#[derive(Clone, Copy)]
struct TerminalPalette {
    foreground: Hsla,
    background: Hsla,
    selection: Hsla,
}

#[derive(Clone, Copy)]
struct BackgroundRect {
    row: usize,
    start_col: usize,
    cell_count: usize,
    color: Hsla,
}

#[derive(Clone, Copy)]
struct CursorPaint {
    row: usize,
    start_col: usize,
    cell_count: usize,
    color: Hsla,
    visible: bool,
    shape: CursorShape,
    focused: bool,
}

#[derive(Clone)]
struct GridPaintData {
    text_runs: Vec<BatchedTextRun>,
    backgrounds: Vec<BackgroundRect>,
    selections: Vec<BackgroundRect>,
    cursor: Option<CursorPaint>,
}

pub struct TerminalDrawer {
    app_state: Entity<AppState>,
    focus_handle: FocusHandle,
    grid_bounds: Rc<RefCell<HashMap<u64, GridGeometry>>>,
    cell_width: f32,
    cell_height: f32,
    scroll_remainder: HashMap<u64, f32>,
    selection_anchor: Option<(u64, (usize, usize), SelectionKind)>,
    last_mouse_point: HashMap<u64, (usize, usize)>,
    focus_subscriptions: Vec<gpui::Subscription>,
    event_subscriptions: HashMap<u64, TerminalEventSubscription>,
    marked_text: Option<MarkedText>,
    bell_tabs: HashSet<u64>,
    hovered_link: Option<(u64, HyperlinkMatch)>,
    pressed_link: Option<(u64, String)>,
    last_link_hover: Option<Instant>,
    cursor_phase: bool,
    last_input: Instant,
    terminal_focused: bool,
    blink_task: Option<Task<()>>,
    _app_state_subscription: gpui::Subscription,
}

impl TerminalDrawer {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let app_state_subscription = cx.observe(&app_state, |_, _, cx| cx.notify());
        Self {
            app_state,
            focus_handle: cx.focus_handle(),
            grid_bounds: Rc::new(RefCell::new(HashMap::new())),
            cell_width: 7.83,
            cell_height: 17.,
            scroll_remainder: HashMap::new(),
            selection_anchor: None,
            last_mouse_point: HashMap::new(),
            focus_subscriptions: Vec::new(),
            event_subscriptions: HashMap::new(),
            marked_text: None,
            bell_tabs: HashSet::new(),
            hovered_link: None,
            pressed_link: None,
            last_link_hover: None,
            cursor_phase: true,
            last_input: Instant::now(),
            terminal_focused: false,
            blink_task: None,
            _app_state_subscription: app_state_subscription,
        }
    }

    pub fn resize(&self, _width: f32, height: f32, cx: &mut Context<Self>) {
        self.app_state.update(cx, |state, _| {
            state.set_terminal_height(height);
        });
    }

    fn with_terminal(&self, cx: &mut Context<Self>, f: impl FnOnce(&term::Terminal)) {
        if let Some(terminal) = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|a| a.terminal_workspace.active())
        {
            f(&terminal.terminal);
        }
    }

    fn with_terminal_id(
        &self,
        terminal_id: u64,
        cx: &mut Context<Self>,
        f: impl FnOnce(&term::Terminal),
    ) {
        if let Some(terminal) = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|active| active.terminal_workspace.terminal(terminal_id))
        {
            f(&terminal.terminal);
        }
    }

    /// Keep one gpui-side drain task per live PTY. Terminal restarts retain the
    /// tab id, so channel identity (rather than just the id) determines whether
    /// an existing subscription is still valid.
    fn sync_event_subscriptions(&mut self, window: &Window, cx: &mut Context<Self>) {
        let streams = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .map(|active| {
                active
                    .terminal_workspace
                    .terminals
                    .iter()
                    .map(|entry| (entry.id, entry.terminal.events()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        self.event_subscriptions
            .retain(|id, _| streams.iter().any(|(stream_id, _)| stream_id == id));

        for (terminal_id, receiver) in streams {
            let already_subscribed = self
                .event_subscriptions
                .get(&terminal_id)
                .is_some_and(|subscription| subscription.receiver.same_channel(&receiver));
            if already_subscribed {
                continue;
            }

            let task_receiver = receiver.clone();
            let task = cx.spawn_in(window, async move |this, cx| {
                while let Ok(first_event) = task_receiver.recv().await {
                    // The first event is visible immediately. A short trailing
                    // window then collapses Wakeup floods from large PTY writes.
                    if this
                        .update_in(cx, |this, window, cx| {
                            if matches!(first_event, TermEvent::Bell) {
                                this.bell_tabs.insert(terminal_id);
                                window.play_system_bell();
                            }
                            window.invalidate_character_coordinates();
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }

                    let deadline = Instant::now() + Duration::from_millis(4);
                    let mut saw_batched_event = false;
                    let mut non_wakeup_events = 0;
                    loop {
                        let next = smol::future::or(
                            async {
                                smol::Timer::at(deadline).await;
                                None
                            },
                            async { Some(task_receiver.recv().await) },
                        )
                        .await;
                        let Some(next) = next else {
                            break;
                        };
                        let Ok(event) = next else {
                            return;
                        };
                        saw_batched_event = true;
                        if matches!(event, TermEvent::Bell) {
                            let _ = this.update_in(cx, |this, window, _| {
                                this.bell_tabs.insert(terminal_id);
                                window.play_system_bell();
                            });
                        }
                        if !matches!(event, TermEvent::Wakeup) {
                            non_wakeup_events += 1;
                            if non_wakeup_events >= 100 {
                                break;
                            }
                        }
                    }
                    if saw_batched_event
                        && this
                            .update_in(cx, |_, window, cx| {
                                window.invalidate_character_coordinates();
                                cx.notify();
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            });
            self.event_subscriptions.insert(
                terminal_id,
                TerminalEventSubscription {
                    receiver,
                    _task: task,
                },
            );
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.note_input(cx);
        let keystroke = &event.keystroke;
        if event.prefer_character_input {
            return;
        }
        if keystroke.modifiers.platform {
            if keystroke.key.eq_ignore_ascii_case("c") {
                if let Some(text) = self
                    .app_state
                    .read(cx)
                    .active
                    .as_ref()
                    .and_then(|a| a.terminal_workspace.active())
                    .and_then(|entry| entry.terminal.selected_text())
                    .map(|selection| selection.text)
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    cx.stop_propagation();
                }
                return;
            }
            if keystroke.key.eq_ignore_ascii_case("v")
                && let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
            {
                self.with_terminal(cx, |terminal| {
                    let mode = terminal.snapshot().mode;
                    let text = if mode.bracketed_paste {
                        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
                    } else {
                        text.replace("\r\n", "\r").replace('\n', "\r")
                    };
                    terminal.write_input(text.into_bytes());
                });
                cx.stop_propagation();
            }
            return;
        }

        let mut handled = false;
        self.with_terminal(cx, |terminal| {
            if let Some(bytes) = mappings::key_bytes(
                &keystroke.key,
                term_modifiers(keystroke.modifiers),
                terminal.snapshot().mode,
                true,
            ) {
                terminal.write_input(bytes);
                handled = true;
            }
        });
        if handled {
            cx.stop_propagation();
        }
    }

    fn note_input(&mut self, cx: &mut Context<Self>) {
        self.last_input = Instant::now();
        self.cursor_phase = true;
        if let Some(id) = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|active| active.terminal_workspace.active_id)
        {
            self.bell_tabs.remove(&id);
        }
        cx.notify();
    }

    fn on_scroll(
        &mut self,
        terminal_id: u64,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = f32::from(event.delta.pixel_delta(px(self.cell_height)).y);
        let remainder = self.scroll_remainder.entry(terminal_id).or_default();
        let total = *remainder + delta;
        let lines = (total / self.cell_height).trunc() as i32;
        *remainder = total - lines as f32 * self.cell_height;
        if lines != 0 {
            if let Some(entry) = self
                .app_state
                .read(cx)
                .active
                .as_ref()
                .and_then(|active| active.terminal_workspace.terminal(terminal_id))
            {
                let snapshot = entry.terminal.snapshot();
                let point = self
                    .grid_point(terminal_id, event.position)
                    .map(|(row, column)| GridPoint { row, column })
                    .unwrap_or(GridPoint { row: 0, column: 0 });
                if snapshot.mode.routes_mouse(event.modifiers.shift) {
                    if let Some(bytes) = mappings::scroll_report(
                        point,
                        lines,
                        term_modifiers(event.modifiers),
                        snapshot.mode,
                    ) {
                        entry.terminal.write_input(bytes);
                    }
                } else if snapshot.mode.alt_screen
                    && snapshot.mode.alternate_scroll
                    && !event.modifiers.shift
                {
                    entry.terminal.write_input(mappings::alt_scroll(lines));
                } else {
                    entry.terminal.scroll(lines);
                }
            }
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn render_grid(
        &self,
        terminal_id: u64,
        state: &TermState,
        register_input: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = TerminalPalette {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            selection: cx.theme().primary.opacity(0.28),
        };
        let marked_text = self
            .marked_text
            .as_ref()
            .filter(|marked| marked.terminal_id == terminal_id)
            .map(|marked| marked.text.clone());
        let hovered_link = self
            .hovered_link
            .as_ref()
            .filter(|(id, _)| *id == terminal_id)
            .map(|(_, link)| link);
        let paint_data = layout_grid(
            state,
            palette,
            marked_text.is_some(),
            hovered_link,
            self.terminal_focused,
            self.cursor_phase || self.last_input.elapsed() < Duration::from_millis(500),
        );
        let cell_width = self.cell_width;
        let cell_height = self.cell_height;
        let rows = state.rows;
        let focus_handle = self.focus_handle.clone();
        let drawer = cx.entity();

        canvas(
            |_bounds, _window, _cx| (),
            move |bounds, (), window, cx| {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    let scale_factor = window.scale_factor();
                    let snap_down = |value: Pixels| {
                        px((f32::from(value) * scale_factor).floor() / scale_factor)
                    };
                    let snap_up = |value: Pixels| {
                        px((f32::from(value) * scale_factor).ceil() / scale_factor)
                    };
                    let origin = point(
                        snap_down(bounds.origin.x),
                        snap_down(bounds.origin.y),
                    );

                    for background in paint_data
                        .backgrounds
                        .iter()
                        .chain(&paint_data.selections)
                    {
                        let left = origin.x + px(background.start_col as f32 * cell_width);
                        let right = origin.x
                            + px(
                                (background.start_col + background.cell_count) as f32 * cell_width,
                            );
                        let left = snap_down(left);
                        let background_bounds = Bounds::new(
                            point(
                                left,
                                origin.y + px(background.row as f32 * cell_height),
                            ),
                            size(snap_up(right) - left, px(cell_height)),
                        );
                        window.paint_quad(fill(background_bounds, background.color));
                    }

                    let cursor_bounds = paint_data.cursor.map(|cursor| {
                        Bounds::new(
                            point(
                                origin.x + px(cursor.start_col as f32 * cell_width),
                                origin.y + px(cursor.row as f32 * cell_height),
                            ),
                            size(px(cursor.cell_count as f32 * cell_width), px(cell_height)),
                        )
                    });
                    if marked_text.is_none()
                        && let Some(cursor) = paint_data.cursor.filter(|cursor| cursor.visible)
                        && let Some(cursor_bounds) = cursor_bounds
                    {
                        match (cursor.focused, cursor.shape) {
                            (false, _) | (_, CursorShape::HollowBlock) => {
                                let t = px(1.);
                                window.paint_quad(fill(Bounds::new(cursor_bounds.origin, size(cursor_bounds.size.width, t)), cursor.color));
                                window.paint_quad(fill(Bounds::new(point(cursor_bounds.left(), cursor_bounds.bottom() - t), size(cursor_bounds.size.width, t)), cursor.color));
                                window.paint_quad(fill(Bounds::new(cursor_bounds.origin, size(t, cursor_bounds.size.height)), cursor.color));
                                window.paint_quad(fill(Bounds::new(point(cursor_bounds.right() - t, cursor_bounds.top()), size(t, cursor_bounds.size.height)), cursor.color));
                            }
                            (_, CursorShape::Bar) => window.paint_quad(fill(Bounds::new(cursor_bounds.origin, size(px(2.), cursor_bounds.size.height)), cursor.color)),
                            (_, CursorShape::Underline) => window.paint_quad(fill(Bounds::new(point(cursor_bounds.left(), cursor_bounds.bottom() - px(2.)), size(cursor_bounds.size.width, px(2.))), cursor.color)),
                            (_, CursorShape::Block) => window.paint_quad(fill(cursor_bounds, cursor.color)),
                            (_, CursorShape::Hidden) => {}
                        }
                    }

                    for run in &paint_data.text_runs {
                        let mut run_font = terminal_font();
                        run_font.weight = if run.style.bold {
                            FontWeight::BOLD
                        } else {
                            FontWeight::NORMAL
                        };
                        run_font.style = if run.style.italic {
                            FontStyle::Italic
                        } else {
                            FontStyle::Normal
                        };
                        let foreground = if run.style.cursor {
                            terminal_color(run.style.bg, palette)
                        } else {
                            terminal_color(run.style.fg, palette)
                        };
                        let text_run = TextRun {
                            len: run.text.len(),
                            font: run_font,
                            color: foreground,
                            background_color: None,
                            strikethrough: None,
                            underline: run.style.underline.then_some(UnderlineStyle {
                                thickness: px(1.),
                                color: Some(foreground),
                                wavy: false,
                            }),
                        };
                        let shaped = window.text_system().shape_line(
                            run.text.clone().into(),
                            px(FONT_SIZE),
                            &[text_run],
                            Some(px(cell_width)),
                        );
                        let position = point(
                            origin.x + px(run.start_col as f32 * cell_width),
                            origin.y + px(run.row as f32 * cell_height),
                        );
                        let _ = shaped.paint(
                            position,
                            px(cell_height),
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }

                    if let Some(marked_text) = marked_text.as_ref().filter(|text| !text.is_empty())
                        && let Some(cursor_bounds) = cursor_bounds
                    {
                        let ime_run = TextRun {
                            len: marked_text.len(),
                            font: terminal_font(),
                            color: palette.foreground,
                            background_color: None,
                            strikethrough: None,
                            underline: Some(UnderlineStyle {
                                thickness: px(1.),
                                color: Some(palette.foreground),
                                wavy: false,
                            }),
                        };
                        let shaped = window.text_system().shape_line(
                            marked_text.clone().into(),
                            px(FONT_SIZE),
                            &[ime_run],
                            None,
                        );
                        let covered_cells = (f32::from(shaped.width) / cell_width).ceil().max(1.);
                        let ime_bounds = Bounds::new(
                            cursor_bounds.origin,
                            size(px(covered_cells * cell_width), px(cell_height)),
                        );
                        window.paint_quad(fill(ime_bounds, palette.background));
                        let _ = shaped.paint(
                            cursor_bounds.origin,
                            px(cell_height),
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }

                    if register_input {
                        window.handle_input(
                            &focus_handle,
                            TerminalInputHandler {
                                drawer,
                                terminal_id,
                                cursor_bounds,
                                cell_width: px(cell_width),
                            },
                            cx,
                        );
                    }
                });
            },
        )
        .w_full()
        .h(px(rows as f32 * cell_height))
        .into_any_element()
    }

    fn grid_point(
        &self,
        terminal_id: u64,
        position: gpui::Point<Pixels>,
    ) -> Option<(usize, usize)> {
        let geometry = *self.grid_bounds.borrow().get(&terminal_id)?;
        let x =
            (f32::from(position.x - geometry.bounds.left()) - PANE_BORDER - PANE_PADDING_X).max(0.);
        let y =
            (f32::from(position.y - geometry.bounds.top()) - PANE_BORDER - PANE_PADDING_Y).max(0.);
        Some((
            ((y / geometry.cell_height) as usize).min(geometry.rows.saturating_sub(1)),
            ((x / geometry.cell_width) as usize).min(geometry.cols.saturating_sub(1)),
        ))
    }

    fn terminal_mouse_down(
        &mut self,
        terminal_id: u64,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let Some(point) = self.grid_point(terminal_id, event.position) else {
            return;
        };
        self.app_state.update(cx, |state, cx| {
            state.activate_terminal(terminal_id, cx);
            if let Some(entry) = state
                .active
                .as_ref()
                .and_then(|active| active.terminal_workspace.terminal(terminal_id))
            {
                let snapshot = entry.terminal.snapshot();
                if snapshot.mode.routes_mouse(event.modifiers.shift) {
                    if let Some(button) = term_mouse_button(event.button)
                        && let Some(bytes) = mappings::mouse_button_report(
                            GridPoint {
                                row: point.0,
                                column: point.1,
                            },
                            button,
                            term_modifiers(event.modifiers),
                            true,
                            snapshot.mode,
                        )
                    {
                        entry.terminal.write_input(bytes);
                    }
                    return;
                }
                if event.button != MouseButton::Left {
                    return;
                }
                if event.modifiers.platform
                    && let Some(link) = entry.terminal.hyperlink_at(point.0, point.1)
                {
                    self.pressed_link = Some((terminal_id, link.url));
                    return;
                }
                if event.modifiers.shift {
                    entry.terminal.extend_selection(point);
                    return;
                }
                entry.terminal.clear_selection();
                let kind = match event.click_count {
                    1 => SelectionKind::Simple,
                    2 => SelectionKind::Semantic,
                    3 => SelectionKind::Lines,
                    _ => return,
                };
                entry.terminal.select_kind(point, point, kind);
                self.selection_anchor = Some((terminal_id, point, kind));
            }
        });
        cx.notify();
    }

    fn terminal_mouse_move(
        &mut self,
        terminal_id: u64,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(point) = self.grid_point(terminal_id, event.position) else {
            self.hovered_link = None;
            return;
        };
        if let Some(entry) = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|active| active.terminal_workspace.terminal(terminal_id))
        {
            let snapshot = entry.terminal.snapshot();
            if snapshot.mode.routes_mouse(event.modifiers.shift) {
                if self.last_mouse_point.get(&terminal_id) != Some(&point) {
                    self.last_mouse_point.insert(terminal_id, point);
                    if let Some(bytes) = mappings::mouse_move_report(
                        GridPoint {
                            row: point.0,
                            column: point.1,
                        },
                        event.pressed_button.and_then(term_mouse_button),
                        term_modifiers(event.modifiers),
                        snapshot.mode,
                    ) {
                        entry.terminal.write_input(bytes);
                    }
                }
                return;
            }
            if event.modifiers.platform {
                if self
                    .last_link_hover
                    .is_none_or(|last| last.elapsed() >= Duration::from_millis(16))
                {
                    self.last_link_hover = Some(Instant::now());
                    self.hovered_link = entry
                        .terminal
                        .hyperlink_at(point.0, point.1)
                        .map(|link| (terminal_id, link));
                }
            } else {
                self.hovered_link = None;
                if let Some((selection_id, start, kind)) = self.selection_anchor
                    && selection_id == terminal_id
                    && event.dragging()
                {
                    entry.terminal.select_kind(start, point, kind);
                    if !snapshot.mode.alt_screen
                        && snapshot.history_size > 0
                        && let Some(lines) = drag_scroll_lines(
                            event.position.y,
                            self.grid_bounds.borrow().get(&terminal_id).copied(),
                            self.cell_height,
                        )
                    {
                        entry.terminal.scroll(lines);
                    }
                }
            }
        }
        cx.notify();
    }

    fn terminal_mouse_up(
        &mut self,
        terminal_id: u64,
        event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(point) = self.grid_point(terminal_id, event.position)
            && let Some(entry) = self
                .app_state
                .read(cx)
                .active
                .as_ref()
                .and_then(|active| active.terminal_workspace.terminal(terminal_id))
        {
            let snapshot = entry.terminal.snapshot();
            if snapshot.mode.routes_mouse(event.modifiers.shift) {
                if let Some(button) = term_mouse_button(event.button)
                    && let Some(bytes) = mappings::mouse_button_report(
                        GridPoint {
                            row: point.0,
                            column: point.1,
                        },
                        button,
                        term_modifiers(event.modifiers),
                        false,
                        snapshot.mode,
                    )
                {
                    entry.terminal.write_input(bytes);
                }
            } else if event.button == MouseButton::Left && event.modifiers.platform {
                let released = entry
                    .terminal
                    .hyperlink_at(point.0, point.1)
                    .map(|link| link.url);
                if let (Some((pressed_id, pressed)), Some(released)) =
                    (self.pressed_link.take(), released)
                    && pressed_id == terminal_id
                    && pressed == released
                {
                    cx.open_url(&released);
                }
            } else if let Some((selection_id, start, kind)) = self.selection_anchor
                && selection_id == terminal_id
            {
                entry.terminal.select_kind(start, point, kind);
            }
        }
        self.selection_anchor = None;
        self.pressed_link = None;
        self.last_mouse_point.remove(&terminal_id);
        cx.notify();
    }

    fn render_terminal(&self, terminal_id: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some((snapshot, label, register_input)) =
            self.app_state.read(cx).active.as_ref().and_then(|active| {
                active
                    .terminal_workspace
                    .terminal(terminal_id)
                    .map(|entry| {
                        (
                            entry.terminal.snapshot(),
                            entry.terminal.label(),
                            active.terminal_workspace.active_id == Some(terminal_id),
                        )
                    })
            })
        else {
            return div().into_any_element();
        };

        let mut grid = v_flex().min_w_full().child(self.render_grid(
            terminal_id,
            &snapshot,
            register_input,
            cx,
        ));
        if snapshot.exited {
            let status = snapshot
                .exit_code
                .map(|code| tcode_i18n::tr!("terminal.exited_code", code = code).into_owned())
                .unwrap_or_else(|| tcode_i18n::tr!("terminal.exited").into_owned());
            grid = grid.child(
                div()
                    .h(px(self.cell_height))
                    .text_color(cx.theme().muted_foreground)
                    .child(status),
            );
        }

        // The add-to-context button is a pure overlay: it must never affect the
        // grid's geometry. Reserving space for it while a selection exists
        // resized the PTY mid-drag — rows jumped and blank lines appeared.
        let has_selection = snapshot.cells.iter().any(|cell| cell.selected);
        let grid_bounds = self.grid_bounds.clone();
        let app_state = self.app_state.clone();
        let cell_width = self.cell_width;
        let cell_height = self.cell_height;
        let link_hovered = self
            .hovered_link
            .as_ref()
            .is_some_and(|(id, _)| *id == terminal_id);
        div()
            .id(("terminal-grid", terminal_id))
            .relative()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .px(px(PANE_PADDING_X))
            .py(px(PANE_PADDING_Y))
            .border_1()
            .rounded(cx.theme().radius)
            .border_color(
                if self
                    .app_state
                    .read(cx)
                    .active
                    .as_ref()
                    .is_some_and(|active| active.terminal_workspace.active_id == Some(terminal_id))
                {
                    cx.theme().ring.opacity(0.72)
                } else {
                    cx.theme().border
                },
            )
            .when(link_hovered, |this| this.cursor_pointer())
            .on_prepaint(move |bounds, _window, cx| {
                let content_width =
                    f32::from(bounds.size.width) - 2. * (PANE_BORDER + PANE_PADDING_X);
                let content_height =
                    f32::from(bounds.size.height) - 2. * (PANE_BORDER + PANE_PADDING_Y);
                let cols = (content_width / cell_width).floor().max(2.) as usize;
                let rows = (content_height / cell_height).floor().max(2.) as usize;
                grid_bounds.borrow_mut().insert(
                    terminal_id,
                    GridGeometry {
                        bounds,
                        cols,
                        rows,
                        cell_width,
                        cell_height,
                    },
                );
                if let Some(entry) = app_state
                    .read(cx)
                    .active
                    .as_ref()
                    .and_then(|active| active.terminal_workspace.terminal(terminal_id))
                {
                    entry.terminal.resize(cols, rows);
                }
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_down(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_down(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_down(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_move(cx.listener(move |this, event, window, cx| {
                this.terminal_mouse_move(terminal_id, event, window, cx)
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_up(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_up(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_up(terminal_id, event, window, cx)
                }),
            )
            .on_scroll_wheel(cx.listener(move |this, event, window, cx| {
                this.on_scroll(terminal_id, event, window, cx)
            }))
            .child(grid)
            .when(has_selection, |this| {
                this.child(
                    Button::new(("terminal-add-context", terminal_id))
                        .absolute()
                        .right(px(PANE_BORDER + PANE_PADDING_X))
                        .top(px(PANE_BORDER + PANE_PADDING_Y))
                        .small()
                        .label(tcode_i18n::tr!("terminal.add_context"))
                        .tooltip(format!(
                            "{} · {}",
                            label,
                            tcode_i18n::tr!("terminal.selection")
                        ))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.app_state.update(cx, |state, cx| {
                                state.capture_terminal_selection(terminal_id, cx)
                            });
                        })),
                )
            })
            .into_any_element()
    }
}

impl Focusable for TerminalDrawer {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalDrawer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.terminal_focused = self.focus_handle.is_focused(window);
        if self.blink_task.is_none() {
            self.blink_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    smol::Timer::after(Duration::from_millis(500)).await;
                    if this
                        .update(cx, |this, cx| {
                            this.cursor_phase = !this.cursor_phase;
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }
        self.sync_event_subscriptions(window, cx);
        if self.focus_subscriptions.is_empty() {
            let app_state = self.app_state.clone();
            let focus_in = window.on_focus_in(&self.focus_handle, cx, move |_, cx| {
                if let Some(entry) = app_state
                    .read(cx)
                    .active
                    .as_ref()
                    .and_then(|active| active.terminal_workspace.active())
                    && entry.terminal.snapshot().mode.focus_in_out
                {
                    entry.terminal.write_input(b"\x1b[I".to_vec());
                }
            });
            let app_state = self.app_state.clone();
            let focus_out = window.on_focus_out(&self.focus_handle, cx, move |_, _, cx| {
                if let Some(entry) = app_state
                    .read(cx)
                    .active
                    .as_ref()
                    .and_then(|active| active.terminal_workspace.active())
                    && entry.terminal.snapshot().mode.focus_in_out
                {
                    entry.terminal.write_input(b"\x1b[O".to_vec());
                }
            });
            self.focus_subscriptions.extend([focus_in, focus_out]);
        }

        // PTY dimensions and mouse hit-testing use the exact advance and
        // vertical metrics of the same resolved face used by StyledText.
        let shaped_cell = window.text_system().shape_line(
            "MMMMMMMMMM".into(),
            px(FONT_SIZE),
            &[TextRun {
                len: 10,
                font: terminal_font(),
                color: cx.theme().foreground,
                background_color: None,
                strikethrough: None,
                underline: None,
            }],
            None,
        );
        self.cell_width = f32::from(shaped_cell.width) / 10.;
        self.cell_height = f32::from(shaped_cell.ascent + shaped_cell.descent)
            .ceil()
            .max(FONT_SIZE + 2.);
        let (tabs, active_id, active_split) = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .map(|active| {
                let workspace = &active.terminal_workspace;
                (
                    workspace
                        .terminals
                        .iter()
                        .map(|entry| {
                            (
                                entry.id,
                                entry.terminal.label(),
                                entry.terminal.snapshot().exited,
                                self.bell_tabs.contains(&entry.id),
                            )
                        })
                        .collect::<Vec<_>>(),
                    workspace.active_id,
                    workspace.active_id.and_then(|id| workspace.split_for(id)),
                )
            })
            .unwrap_or_default();

        if self
            .marked_text
            .as_ref()
            .is_some_and(|marked| Some(marked.terminal_id) != active_id)
        {
            self.marked_text = None;
        }

        let mut tab_strip = h_flex().min_w_0().gap(px(2.)).overflow_hidden();
        for (id, label, exited, bell) in &tabs {
            let id = *id;
            let selected = active_id == Some(id);
            let close_id = id;
            tab_strip = tab_strip.child(
                h_flex()
                    .id(("terminal-tab", id))
                    .h(px(25.))
                    .gap(px(2.))
                    .px_2()
                    .rounded_t(px(5.))
                    .cursor_pointer()
                    .bg(if selected {
                        cx.theme().muted.opacity(0.72)
                    } else {
                        cx.theme().background.opacity(0.)
                    })
                    .border_b_1()
                    .border_color(if selected {
                        cx.theme().primary
                    } else {
                        cx.theme().border.opacity(0.)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.activate_terminal(id, cx));
                    }))
                    .child(
                        div()
                            .max_w(px(92.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(11.))
                            .text_color(if *exited || !selected {
                                cx.theme().muted_foreground
                            } else {
                                cx.theme().foreground
                            })
                            .child(label.clone()),
                    )
                    .when(*bell, |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(cx.theme().warning)
                                .child("●"),
                        )
                    })
                    .child(
                        Button::new(("terminal-tab-close", close_id))
                            .ghost()
                            .compact()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(tcode_i18n::tr!("terminal.close_tab"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.app_state
                                    .update(cx, |state, cx| state.close_terminal(close_id, cx));
                            })),
                    ),
            );
        }

        let at_limit = tabs.len() >= MAX_TERMINALS_PER_SESSION;
        let can_split = !at_limit && active_id.is_some() && active_split.is_none();
        let active_exited = tabs
            .iter()
            .any(|(id, _, exited, _)| Some(*id) == active_id && *exited);
        let header = h_flex()
            .flex_none()
            .h(px(31.))
            .px_2()
            .gap_1()
            .items_center()
            .border_t_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(tab_strip)
            .child(div().flex_1())
            .when(active_exited, |this| {
                this.child(
                    Button::new("terminal-restart")
                        .ghost()
                        .small()
                        .label(tcode_i18n::tr!("terminal.restart"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.app_state
                                .update(cx, |state, cx| state.restart_terminal(cx))
                        })),
                )
            })
            .child(
                Button::new("terminal-split-horizontal")
                    .ghost()
                    .small()
                    .compact()
                    .label("↔")
                    .disabled(!can_split)
                    .tooltip(tcode_i18n::tr!("terminal.split_horizontal"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let cwd = this
                            .app_state
                            .read(cx)
                            .active
                            .as_ref()
                            .and_then(|active| active.terminal_workspace.active())
                            .map(|entry| entry.terminal.working_directory());
                        if let Some(cwd) = cwd {
                            term::Terminal::with_spawn_cwd(cwd, || {
                                this.app_state.update(cx, |state, cx| {
                                    state.split_terminal(TerminalSplitDirection::Horizontal, cx)
                                })
                            });
                        }
                    })),
            )
            .child(
                Button::new("terminal-split-vertical")
                    .ghost()
                    .small()
                    .compact()
                    .label("↕")
                    .disabled(!can_split)
                    .tooltip(tcode_i18n::tr!("terminal.split_vertical"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let cwd = this
                            .app_state
                            .read(cx)
                            .active
                            .as_ref()
                            .and_then(|active| active.terminal_workspace.active())
                            .map(|entry| entry.terminal.working_directory());
                        if let Some(cwd) = cwd {
                            term::Terminal::with_spawn_cwd(cwd, || {
                                this.app_state.update(cx, |state, cx| {
                                    state.split_terminal(TerminalSplitDirection::Vertical, cx)
                                })
                            });
                        }
                    })),
            )
            .child(
                Button::new("terminal-new")
                    .ghost()
                    .small()
                    .compact()
                    .label("+")
                    .disabled(at_limit)
                    .tooltip(if at_limit {
                        tcode_i18n::tr!("terminal.max_reached", count = MAX_TERMINALS_PER_SESSION)
                    } else {
                        tcode_i18n::tr!("terminal.new")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        let cwd = this
                            .app_state
                            .read(cx)
                            .active
                            .as_ref()
                            .and_then(|active| active.terminal_workspace.active())
                            .map(|entry| entry.terminal.working_directory());
                        if let Some(cwd) = cwd {
                            term::Terminal::with_spawn_cwd(cwd, || {
                                this.app_state
                                    .update(cx, |state, cx| state.new_terminal(cx))
                            });
                        }
                    })),
            )
            .child(
                Button::new("terminal-close-drawer")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Close)
                    .tooltip(tcode_i18n::tr!("terminal.close"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.close_terminal_panel(cx))
                    })),
            );

        let body: AnyElement = match (active_id, active_split) {
            (_, Some(split)) => match split.direction {
                TerminalSplitDirection::Horizontal => {
                    let first = resizable_panel()
                        .pr(px(2.))
                        .child(self.render_terminal(split.first, cx));
                    let second = resizable_panel()
                        .pl(px(2.))
                        .child(self.render_terminal(split.second, cx));
                    h_resizable(("terminal-split-h", split.first))
                        .child(first)
                        .child(second)
                        .into_any_element()
                }
                TerminalSplitDirection::Vertical => {
                    let first = resizable_panel()
                        .pb(px(2.))
                        .child(self.render_terminal(split.first, cx));
                    let second = resizable_panel()
                        .pt(px(2.))
                        .child(self.render_terminal(split.second, cx));
                    v_resizable(("terminal-split-v", split.first))
                        .child(first)
                        .child(second)
                        .into_any_element()
                }
            },
            (Some(id), None) => self.render_terminal(id, cx),
            _ => div()
                .p_3()
                .child(tcode_i18n::tr!("terminal.starting"))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .min_h_0()
            .bg(cx.theme().background)
            .font_family(TERMINAL_FONT_FAMILY)
            .text_size(px(FONT_SIZE))
            .child(header)
            .child(
                div()
                    .track_focus(&self.focus_handle)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .flex_1()
                    .min_h_0()
                    .child(body),
            )
    }
}

fn layout_grid(
    state: &TermState,
    palette: TerminalPalette,
    composing: bool,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
) -> GridPaintData {
    let cursor = layout_cursor(state, palette);
    let cursor_cell = cursor.map(|cursor| (cursor.row, cursor.start_col));
    let mut text_runs: Vec<BatchedTextRun> = Vec::new();
    let mut backgrounds: Vec<BackgroundRect> = Vec::new();
    let mut selections: Vec<BackgroundRect> = Vec::new();

    for row in 0..state.rows {
        let mut previous_cell_had_extras = false;
        for col in 0..state.cols {
            let Some(cell) = state.cell(row, col) else {
                break;
            };
            let (fg, bg) = cell_colors(cell);

            let background = if matches!(bg, Color::DefaultBackground) {
                None
            } else {
                Some(terminal_color(bg, palette))
            };
            if let Some(color) = background {
                push_background(&mut backgrounds, row, col, color);
            }
            if cell.selected {
                push_background(&mut selections, row, col, palette.selection);
            }

            // A wide spacer still participates in backgrounds and hit-testing,
            // but never contributes a glyph to the shaped text.
            if cell.wide_spacer {
                continue;
            }

            // Alacritty stores emoji variation/modifier codepoints as extras;
            // its following placeholder space is not an independently painted
            // character. This mirrors Zed's terminal layout workaround.
            if cell.ch == ' ' && previous_cell_had_extras {
                previous_cell_had_extras = false;
                continue;
            }
            previous_cell_had_extras = cell.text.chars().nth(1).is_some();

            let text = display_cell_text(cell);
            if matches!(cell.ch, '\0' | ' ') && !cell.underline {
                continue;
            }

            let cursor_visible = !composing
                && !cell.selected
                && state.display_offset == 0
                && cursor_cell == Some((row, col))
                && focused
                && state.cursor_shape == CursorShape::Block
                && (!state.cursor_blinking || blink_phase);
            let hyperlink_hovered =
                hovered_link.is_some_and(|link| (row, col) >= link.start && (row, col) <= link.end);
            let style = GridTextStyle {
                fg,
                bg,
                bold: cell.bold,
                italic: cell.italic,
                underline: cell.underline || hyperlink_hovered,
                selected: cell.selected,
                cursor: cursor_visible,
            };

            if let Some(current) = text_runs.last_mut()
                && current.row == row
                && current.start_col + current.cell_count == col
                && current.style == style
            {
                current.text.push_str(&text);
                current.cell_count += 1;
            } else {
                text_runs.push(BatchedTextRun {
                    row,
                    start_col: col,
                    text,
                    cell_count: 1,
                    style,
                });
            }
        }
    }

    GridPaintData {
        text_runs,
        backgrounds,
        selections,
        cursor: cursor.map(|mut cursor| {
            cursor.focused = focused;
            cursor.visible &= !composing && (!state.cursor_blinking || blink_phase);
            cursor
        }),
    }
}

fn push_background(backgrounds: &mut Vec<BackgroundRect>, row: usize, col: usize, color: Hsla) {
    if let Some(previous) = backgrounds.last_mut()
        && previous.row == row
        && previous.start_col + previous.cell_count == col
        && previous.color == color
    {
        previous.cell_count += 1;
    } else {
        backgrounds.push(BackgroundRect {
            row,
            start_col: col,
            cell_count: 1,
            color,
        });
    }
}

fn display_cell_text(cell: &Cell) -> String {
    let mut characters = cell.text.chars();
    let Some(first) = characters.next() else {
        return " ".to_string();
    };
    let mut text = String::new();
    text.push(if first == '\0' { ' ' } else { first });
    text.extend(characters);
    text
}

fn cell_colors(cell: &Cell) -> (Color, Color) {
    let (mut fg, mut bg) = (cell.fg, cell.bg);
    if cell.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

fn layout_cursor(state: &TermState, palette: TerminalPalette) -> Option<CursorPaint> {
    if state.display_offset != 0 {
        return None;
    }
    let (row, col) = state.cursor?;
    let cell = state.cell(row, col)?;
    let (start_col, cell_count, color_cell) = if cell.wide_spacer && col > 0 {
        let base = state.cell(row, col - 1)?;
        (col - 1, 2, base)
    } else if cell.wide {
        (col, 2, cell)
    } else {
        (col, 1, cell)
    };
    let (fg, _) = cell_colors(color_cell);
    Some(CursorPaint {
        row,
        start_col,
        cell_count,
        color: terminal_color(fg, palette).opacity(0.72),
        visible: !color_cell.selected,
        shape: state.cursor_shape,
        focused: true,
    })
}

struct TerminalInputHandler {
    drawer: Entity<TerminalDrawer>,
    terminal_id: u64,
    cursor_bounds: Option<Bounds<Pixels>>,
    cell_width: Pixels,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.drawer
            .read(cx)
            .marked_text
            .as_ref()
            .filter(|marked| marked.terminal_id == self.terminal_id)
            .map(|marked| 0..marked.text.encode_utf16().count())
    }

    fn text_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let terminal_id = self.terminal_id;
        let text = text.to_string();
        self.drawer.update(cx, |drawer, cx| {
            drawer.marked_text = None;
            drawer.last_input = Instant::now();
            drawer.cursor_phase = true;
            drawer.bell_tabs.remove(&terminal_id);
            if !text.is_empty() {
                drawer.with_terminal_id(terminal_id, cx, |terminal| {
                    terminal.write_input(text.into_bytes());
                });
            }
            cx.notify();
        });
        window.invalidate_character_coordinates();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let terminal_id = self.terminal_id;
        let marked_text = (!new_text.is_empty()).then(|| MarkedText {
            terminal_id,
            text: new_text.to_string(),
        });
        self.drawer.update(cx, |drawer, cx| {
            drawer.marked_text = marked_text;
            cx.notify();
        });
        window.invalidate_character_coordinates();
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        let terminal_id = self.terminal_id;
        self.drawer.update(cx, |drawer, cx| {
            if drawer
                .marked_text
                .as_ref()
                .is_some_and(|marked| marked.terminal_id == terminal_id)
            {
                drawer.marked_text = None;
                cx.notify();
            }
        });
        window.invalidate_character_coordinates();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let mut bounds = self.cursor_bounds?;
        bounds.origin.x += self.cell_width * range_utf16.start as f32;
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

fn term_modifiers(modifiers: gpui::Modifiers) -> TermModifiers {
    TermModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
        platform: modifiers.platform,
    }
}

fn term_mouse_button(button: MouseButton) -> Option<TermMouseButton> {
    match button {
        MouseButton::Left => Some(TermMouseButton::Left),
        MouseButton::Middle => Some(TermMouseButton::Middle),
        MouseButton::Right => Some(TermMouseButton::Right),
        _ => None,
    }
}

fn drag_scroll_lines(y: Pixels, geometry: Option<GridGeometry>, cell_height: f32) -> Option<i32> {
    let geometry = geometry?;
    let top = geometry.bounds.top() + px(PANE_BORDER + PANE_PADDING_Y);
    let bottom = top + px(geometry.rows as f32 * geometry.cell_height);
    let pixels = if y < top {
        f32::from(top - y)
    } else if y > bottom {
        -f32::from(y - bottom)
    } else {
        return None;
    };
    let lines = (pixels.abs().powf(1.1) / cell_height).ceil() as i32;
    Some(lines.clamp(1, 3) * pixels.signum() as i32)
}

fn terminal_font() -> gpui::Font {
    let mut terminal_font = font(TERMINAL_FONT_FAMILY);
    terminal_font.features = FontFeatures::disable_ligatures();
    terminal_font
}

fn terminal_color(color: Color, palette: TerminalPalette) -> Hsla {
    match color {
        Color::DefaultForeground => palette.foreground,
        Color::DefaultBackground => palette.background,
        Color::Rgb(r, g, b) => {
            rgb((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)).into()
        }
        Color::Indexed(index) => {
            const ANSI: [u32; 16] = [
                0x1f2329, 0xe45649, 0x50a14f, 0xc18401, 0x4078f2, 0xa626a4, 0x0184bc, 0xabb2bf,
                0x5c6370, 0xff616e, 0x7bc275, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
            ];
            if index < 16 {
                return rgb(ANSI[index as usize]).into();
            }
            if index < 232 {
                let n = index - 16;
                let component = |v: u8| if v == 0 { 0 } else { 55 + 40 * u32::from(v) };
                let r = component(n / 36);
                let g = component((n % 36) / 6);
                let b = component(n % 6);
                return rgb((r << 16) | (g << 8) | b).into();
            }
            let gray = 8 + 10 * u32::from(index - 232);
            rgb((gray << 16) | (gray << 8) | gray).into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(ch: char, text: &str, wide: bool, wide_spacer: bool) -> Cell {
        Cell {
            ch,
            text: text.to_string(),
            fg: Color::DefaultForeground,
            bg: Color::DefaultBackground,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            selected: false,
            wide,
            wide_spacer,
        }
    }

    #[test]
    fn batches_mixed_cjk_at_physical_column_boundaries() {
        let cells = vec![
            cell('a', "a", false, false),
            cell('中', "中", true, false),
            cell(' ', " ", false, true),
            cell('b', "b", false, false),
            cell('文', "文", true, false),
            cell(' ', " ", false, true),
            cell('c', "c", false, false),
        ];
        let state = TermState {
            cols: cells.len(),
            rows: 1,
            cells,
            cursor: None,
            cursor_shape: CursorShape::Block,
            cursor_blinking: false,
            title: String::new(),
            exited: false,
            exit_code: None,
            display_offset: 0,
            history_size: 0,
            mode: term::ModeSnapshot::default(),
        };
        let palette = TerminalPalette {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            selection: rgb(0x336699).into(),
        };

        let runs = layout_grid(&state, palette, false, None, true, true).text_runs;
        let boundaries = runs
            .iter()
            .map(|run| (run.start_col, run.text.as_str(), run.cell_count))
            .collect::<Vec<_>>();
        assert_eq!(boundaries, vec![(0, "a中", 2), (3, "b文", 2), (6, "c", 1)]);
        assert_eq!(
            runs.iter()
                .map(|run| run.start_col as f32 * 8.)
                .collect::<Vec<_>>(),
            vec![0., 24., 48.]
        );
    }

    #[test]
    fn printable_keys_defer_to_input_handler_but_control_keys_stay_raw() {
        let mode = term::ModeSnapshot::default();
        let encode = |key: &str| {
            let key = gpui::Keystroke::parse(key).unwrap();
            mappings::key_bytes(&key.key, term_modifiers(key.modifiers), mode, true)
        };
        assert_eq!(encode("a"), None);
        assert_eq!(encode("ctrl-space"), Some(vec![0]));
        assert_eq!(encode("ctrl-c"), Some(vec![3]));
    }
}
