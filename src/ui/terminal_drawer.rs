use std::{ops::Range, time::Duration};

use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable, FontStyle, FontWeight, HighlightStyle,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render,
    ScrollWheelEvent, Styled as _, StyledText, Task, UnderlineStyle, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use term::{Color, TermState};

use crate::app::AppState;

const FONT_SIZE: f32 = 12.;
const LINE_HEIGHT: f32 = 17.;
const CELL_WIDTH: f32 = 7.25;

pub struct TerminalDrawer {
    app_state: Entity<AppState>,
    focus_handle: FocusHandle,
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
            _ticker: ticker,
        }
    }

    pub fn resize(&self, width: f32, height: f32, cx: &mut Context<Self>) {
        let cols = (width / CELL_WIDTH).floor().max(2.) as usize;
        let rows = ((height - 34.) / LINE_HEIGHT).floor().max(2.) as usize;
        self.app_state.update(cx, |state, _| {
            if let Some(active) = state.active.as_mut() {
                active.terminal_height = height;
                if let Some(terminal) = active.terminal.as_ref() {
                    terminal.resize(cols, rows);
                }
            }
        });
    }

    fn with_terminal(&self, cx: &mut Context<Self>, f: impl FnOnce(&term::Terminal)) {
        if let Some(terminal) = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|a| a.terminal.as_ref())
        {
            f(terminal);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform {
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
                    background_color: if cursor {
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
}

impl Focusable for TerminalDrawer {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalDrawer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (snapshot, shell_name) = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|active| {
                active
                    .terminal
                    .as_ref()
                    .map(|terminal| (terminal.snapshot(), terminal.shell_name().to_string()))
            })
            .map(|(snapshot, shell)| (Some(snapshot), shell))
            .unwrap_or((None, "shell".into()));

        let header = h_flex()
            .flex_none()
            .h(px(33.))
            .px_3()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_size(px(12.)).font_medium().child("Terminal"))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(shell_name),
            )
            .child(div().flex_1())
            .when(
                snapshot.as_ref().is_some_and(|state| state.exited),
                |this| {
                    this.child(
                        Button::new("terminal-restart")
                            .ghost()
                            .small()
                            .label("Restart")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.app_state
                                    .update(cx, |state, cx| state.restart_terminal(cx))
                            })),
                    )
                },
            )
            .child(
                Button::new("terminal-close")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Close)
                    .tooltip("Close terminal")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.close_terminal_panel(cx))
                    })),
            );

        let mut grid = v_flex().min_w_full();
        if let Some(state) = &snapshot {
            for row in 0..state.rows {
                grid = grid.child(self.render_line(state, row, cx));
            }
            if state.exited {
                let status = state
                    .exit_code
                    .map(|code| format!("process exited ({code})"))
                    .unwrap_or_else(|| "process exited".into());
                grid = grid.child(
                    div()
                        .h(px(LINE_HEIGHT))
                        .text_color(cx.theme().muted_foreground)
                        .child(status),
                );
            }
        } else {
            grid = grid.child("Starting terminal…");
        }

        v_flex()
            .size_full()
            .min_h_0()
            .bg(cx.theme().background)
            .font_family("SF Mono")
            .text_size(px(FONT_SIZE))
            .child(header)
            .child(
                div()
                    .id("terminal-grid")
                    .track_focus(&self.focus_handle)
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .px_2()
                    .py_1()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.focus_handle.focus(window, cx)),
                    )
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_scroll_wheel(cx.listener(Self::on_scroll))
                    .child(grid),
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
