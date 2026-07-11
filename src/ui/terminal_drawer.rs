use std::{cell::RefCell, collections::HashMap, ops::Range, rc::Rc, time::Duration};

use gpui::{
    AnyElement, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable, FontStyle,
    FontWeight, HighlightStyle, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Render,
    ScrollWheelEvent, StatefulInteractiveElement as _, Styled as _, StyledText, Task,
    UnderlineStyle, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, ElementExt as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    resizable::{h_resizable, resizable_panel, v_resizable},
    v_flex,
};
use term::{Color, TermState};

use crate::app::{AppState, MAX_TERMINALS_PER_SESSION, TerminalSplitDirection};

const FONT_SIZE: f32 = 12.;
const LINE_HEIGHT: f32 = 17.;
const CELL_WIDTH: f32 = 7.25;

pub struct TerminalDrawer {
    app_state: Entity<AppState>,
    focus_handle: FocusHandle,
    grid_bounds: Rc<RefCell<HashMap<u64, Bounds<Pixels>>>>,
    selection_anchor: Option<(u64, (usize, usize))>,
    _ticker: Task<()>,
}

impl TerminalDrawer {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                smol::Timer::after(Duration::from_millis(75)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            app_state,
            focus_handle: cx.focus_handle(),
            grid_bounds: Rc::new(RefCell::new(HashMap::new())),
            selection_anchor: None,
            _ticker: ticker,
        }
    }

    pub fn resize(&self, width: f32, height: f32, cx: &mut Context<Self>) {
        let cols = (width / CELL_WIDTH).floor().max(2.) as usize;
        let rows = ((height - 34.) / LINE_HEIGHT).floor().max(2.) as usize;
        self.app_state.update(cx, |state, _| {
            state.set_terminal_height(height);
        });
        let _ = (cols, rows);
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

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
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
            if keystroke.key.eq_ignore_ascii_case("v") {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let text = text.replace("\r\n", "\r").replace('\n', "\r");
                    self.with_terminal(cx, |terminal| terminal.write_input(text.into_bytes()));
                    cx.stop_propagation();
                }
            }
            return;
        }

        if let Some(bytes) = key_bytes(keystroke) {
            self.with_terminal(cx, |terminal| terminal.write_input(bytes));
            cx.stop_propagation();
        }
    }

    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(px(LINE_HEIGHT)).y;
        let lines = (f32::from(delta) / LINE_HEIGHT).round() as i32;
        if lines != 0 {
            self.with_terminal(cx, |terminal| terminal.scroll(lines));
            cx.notify();
        }
    }

    fn render_line(&self, state: &TermState, row: usize, cx: &mut Context<Self>) -> AnyElement {
        let mut text = String::with_capacity(state.cols);
        let mut runs: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        for col in 0..state.cols {
            let Some(cell) = state.cell(row, col) else {
                break;
            };
            let start = text.len();
            let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
            text.push(ch);
            let end = text.len();
            let (mut fg, mut bg) = (cell.fg, cell.bg);
            if cell.inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            let cursor = state.cursor == Some((row, col)) && state.display_offset == 0;
            runs.push((
                start..end,
                HighlightStyle {
                    color: Some(color(fg, cx)),
                    background_color: if cell.selected {
                        Some(cx.theme().primary.opacity(0.28))
                    } else if cursor {
                        Some(cx.theme().foreground.opacity(0.72))
                    } else if matches!(bg, Color::DefaultBackground) {
                        None
                    } else {
                        Some(color(bg, cx))
                    },
                    font_weight: cell.bold.then_some(FontWeight::BOLD),
                    font_style: cell.italic.then_some(FontStyle::Italic),
                    underline: cell.underline.then_some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(color(fg, cx)),
                        wavy: false,
                    }),
                    ..HighlightStyle::default()
                },
            ));
        }
        div()
            .h(px(LINE_HEIGHT))
            .line_height(px(LINE_HEIGHT))
            .whitespace_nowrap()
            .child(StyledText::new(text).with_highlights(runs))
            .into_any_element()
    }

    fn grid_point(
        &self,
        terminal_id: u64,
        position: gpui::Point<Pixels>,
    ) -> Option<(usize, usize)> {
        let bounds = *self.grid_bounds.borrow().get(&terminal_id)?;
        let x = (f32::from(position.x - bounds.left()) - 8.).max(0.);
        let y = (f32::from(position.y - bounds.top()) - 4.).max(0.);
        Some(((y / LINE_HEIGHT) as usize, (x / CELL_WIDTH) as usize))
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
                entry.terminal.select(point, point);
            }
        });
        self.selection_anchor = Some((terminal_id, point));
        cx.notify();
    }

    fn terminal_mouse_move(
        &mut self,
        terminal_id: u64,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((selection_id, start)) = self.selection_anchor else {
            return;
        };
        if selection_id != terminal_id || !event.dragging() {
            return;
        }
        let Some(point) = self.grid_point(terminal_id, event.position) else {
            return;
        };
        if let Some(entry) = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|active| active.terminal_workspace.terminal(terminal_id))
        {
            entry.terminal.select(start, point);
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
        let Some((selection_id, start)) = self.selection_anchor else {
            return;
        };
        if selection_id != terminal_id {
            return;
        }
        if let Some(point) = self.grid_point(terminal_id, event.position) {
            if let Some(entry) = self
                .app_state
                .read(cx)
                .active
                .as_ref()
                .and_then(|active| active.terminal_workspace.terminal(terminal_id))
            {
                entry.terminal.select(start, point);
            }
        }
        self.selection_anchor = None;
        cx.notify();
    }

    fn render_terminal(&self, terminal_id: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some((snapshot, label)) = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|active| active.terminal_workspace.terminal(terminal_id))
            .map(|entry| (entry.terminal.snapshot(), entry.terminal.label()))
        else {
            return div().into_any_element();
        };

        let mut grid = v_flex().min_w_full();
        for row in 0..snapshot.rows {
            grid = grid.child(self.render_line(&snapshot, row, cx));
        }
        if snapshot.exited {
            let status = snapshot
                .exit_code
                .map(|code| rust_i18n::t!("terminal.exited_code", code = code).into_owned())
                .unwrap_or_else(|| rust_i18n::t!("terminal.exited").into_owned());
            grid = grid.child(
                div()
                    .h(px(LINE_HEIGHT))
                    .text_color(cx.theme().muted_foreground)
                    .child(status),
            );
        }

        let selected_row = snapshot
            .cells
            .iter()
            .rposition(|cell| cell.selected)
            .map(|index| index / snapshot.cols);
        let grid_bounds = self.grid_bounds.clone();
        let app_state = self.app_state.clone();
        div()
            .id(("terminal-grid", terminal_id))
            .relative()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .px_2()
            .py_1()
            .border_1()
            .border_color(
                if self
                    .app_state
                    .read(cx)
                    .active
                    .as_ref()
                    .is_some_and(|active| active.terminal_workspace.active_id == Some(terminal_id))
                {
                    cx.theme().primary.opacity(0.45)
                } else {
                    cx.theme().border
                },
            )
            .on_prepaint(move |bounds, _window, cx| {
                grid_bounds.borrow_mut().insert(terminal_id, bounds);
                let cols = (f32::from(bounds.size.width) / CELL_WIDTH).floor().max(2.) as usize;
                let rows = ((f32::from(bounds.size.height) - 8.) / LINE_HEIGHT)
                    .floor()
                    .max(2.) as usize;
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
            .on_mouse_move(cx.listener(move |this, event, window, cx| {
                this.terminal_mouse_move(terminal_id, event, window, cx)
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_up(terminal_id, event, window, cx)
                }),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(grid)
            .when_some(selected_row, |this, row| {
                this.child(
                    Button::new(("terminal-add-context", terminal_id))
                        .absolute()
                        .right(px(12.))
                        .top(px((row as f32 * LINE_HEIGHT + 8.).min(170.)))
                        .small()
                        .label(rust_i18n::t!("terminal.add_context"))
                        .tooltip(format!(
                            "{} · {}",
                            label,
                            rust_i18n::t!("terminal.selection")
                        ))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                            )
                        })
                        .collect::<Vec<_>>(),
                    workspace.active_id,
                    workspace.active_id.and_then(|id| workspace.split_for(id)),
                )
            })
            .unwrap_or_default();

        let mut tab_strip = h_flex().min_w_0().gap_1().overflow_hidden();
        for (id, label, exited) in &tabs {
            let id = *id;
            let selected = active_id == Some(id);
            let close_id = id;
            tab_strip = tab_strip.child(
                h_flex()
                    .id(("terminal-tab", id))
                    .h(px(27.))
                    .gap_1()
                    .px_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .bg(if selected {
                        cx.theme().muted
                    } else {
                        cx.theme().background
                    })
                    .border_1()
                    .border_color(if selected {
                        cx.theme().primary.opacity(0.45)
                    } else {
                        cx.theme().border
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
                            .text_color(if *exited {
                                cx.theme().muted_foreground
                            } else {
                                cx.theme().foreground
                            })
                            .child(label.clone()),
                    )
                    .child(
                        Button::new(("terminal-tab-close", close_id))
                            .ghost()
                            .compact()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(rust_i18n::t!("terminal.close_tab"))
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
            .any(|(id, _, exited)| Some(*id) == active_id && *exited);
        let header = h_flex()
            .flex_none()
            .h(px(33.))
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
                        .label(rust_i18n::t!("terminal.restart"))
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
                    .tooltip(rust_i18n::t!("terminal.split_horizontal"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state.update(cx, |state, cx| {
                            state.split_terminal(TerminalSplitDirection::Horizontal, cx)
                        })
                    })),
            )
            .child(
                Button::new("terminal-split-vertical")
                    .ghost()
                    .small()
                    .compact()
                    .label("↕")
                    .disabled(!can_split)
                    .tooltip(rust_i18n::t!("terminal.split_vertical"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state.update(cx, |state, cx| {
                            state.split_terminal(TerminalSplitDirection::Vertical, cx)
                        })
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
                        rust_i18n::t!("terminal.max_reached", count = MAX_TERMINALS_PER_SESSION)
                    } else {
                        rust_i18n::t!("terminal.new")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.new_terminal(cx))
                    })),
            )
            .child(
                Button::new("terminal-close-drawer")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Close)
                    .tooltip(rust_i18n::t!("terminal.close"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.close_terminal_panel(cx))
                    })),
            );

        let body: AnyElement = match (active_id, active_split) {
            (_, Some(split)) => {
                let first = resizable_panel().child(self.render_terminal(split.first, cx));
                let second = resizable_panel().child(self.render_terminal(split.second, cx));
                match split.direction {
                    TerminalSplitDirection::Horizontal => {
                        h_resizable(("terminal-split-h", split.first))
                            .child(first)
                            .child(second)
                            .into_any_element()
                    }
                    TerminalSplitDirection::Vertical => {
                        v_resizable(("terminal-split-v", split.first))
                            .child(first)
                            .child(second)
                            .into_any_element()
                    }
                }
            }
            (Some(id), None) => self.render_terminal(id, cx),
            _ => div()
                .p_3()
                .child(rust_i18n::t!("terminal.starting"))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .min_h_0()
            .bg(cx.theme().background)
            .font_family("SF Mono")
            .text_size(px(FONT_SIZE))
            .child(header)
            .child(
                div()
                    .track_focus(&self.focus_handle)
                    .flex_1()
                    .min_h_0()
                    .child(body),
            )
    }
}

fn key_bytes(key: &gpui::Keystroke) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.control;
    if ctrl {
        let ch = key.key.chars().next()?;
        if ch.is_ascii_alphabetic() {
            return Some(vec![(ch.to_ascii_lowercase() as u8) - b'a' + 1]);
        }
        return match key.key.as_str() {
            "space" => Some(vec![0]),
            "[" => Some(vec![27]),
            "\\" => Some(vec![28]),
            "]" => Some(vec![29]),
            "^" => Some(vec![30]),
            "_" => Some(vec![31]),
            _ => None,
        };
    }
    let bytes = match key.key.as_str() {
        "enter" => b"\r".to_vec(),
        "backspace" => vec![0x7f],
        "tab" => b"\t".to_vec(),
        "escape" => vec![0x1b],
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        _ => key.key_char.as_ref()?.as_bytes().to_vec(),
    };
    Some(bytes)
}

fn color(color: Color, cx: &mut Context<TerminalDrawer>) -> gpui::Hsla {
    match color {
        Color::DefaultForeground => cx.theme().foreground,
        Color::DefaultBackground => cx.theme().background,
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
