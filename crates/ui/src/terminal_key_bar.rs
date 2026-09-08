//! Touch-only terminal keys that a software keyboard cannot produce.
//!
//! This owns only the sticky modifier state and presentation. Byte encoding
//! remains in `tcode_protocol::terminal::mappings`, fed by the same replicated
//! terminal modes as hardware key events.

use crate::material;
use crate::theme::ActiveTheme as _;
use crate::touch_scroll::TouchScrollExt as _;
use gpui::{
    App, Context, EventEmitter, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex};
use tcode_protocol::terminal::{
    KeyboardModes, TerminalMode,
    mappings::{self, Modifiers},
};

const KEY_BAR_HEIGHT: f32 = 44.;

/// The scrolling symbol tail, ordered by how often a shell line needs the
/// character: what falls past the scroll edge is what is reached for least.
const SYMBOL_KEYS: [(&str, char); 7] = [
    ("terminal-key-dash", '-'),
    ("terminal-key-slash", '/'),
    ("terminal-key-pipe", '|'),
    ("terminal-key-tilde", '~'),
    ("terminal-key-colon", ':'),
    ("terminal-key-period", '.'),
    ("terminal-key-underscore", '_'),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalKey {
    Escape,
    Tab,
    Left,
    Up,
    Down,
    Right,
    Symbol(char),
    /// A one-tap Control combination: interrupt and end-of-file, the two the
    /// sticky Ctrl modifier is reached for most.
    Control(char),
}

impl TerminalKey {
    fn mapping_name(self) -> String {
        match self {
            Self::Escape => "escape".to_owned(),
            Self::Tab => "tab".to_owned(),
            Self::Left => "left".to_owned(),
            Self::Up => "up".to_owned(),
            Self::Down => "down".to_owned(),
            Self::Right => "right".to_owned(),
            Self::Symbol(symbol) | Self::Control(symbol) => symbol.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TerminalKeyBarEvent(pub(crate) TerminalKey);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StickyModifiers {
    control: bool,
    alt: bool,
}

impl StickyModifiers {
    fn take(&mut self) -> Modifiers {
        let modifiers = Modifiers {
            control: self.control,
            alt: self.alt,
            ..Modifiers::default()
        };
        *self = Self::default();
        modifiers
    }
}

pub(crate) struct TerminalKeyBar {
    terminal_focus: FocusHandle,
    encoder: TerminalKeyEncoder,
}

#[derive(Debug, Default)]
struct TerminalKeyEncoder {
    modifiers: StickyModifiers,
}

impl TerminalKeyEncoder {
    fn new() -> Self {
        Self {
            modifiers: StickyModifiers::default(),
        }
    }

    fn encode_key(
        &mut self,
        key: TerminalKey,
        mode: TerminalMode,
        keyboard_mode: KeyboardModes,
        modify_other_keys: Option<u8>,
    ) -> Vec<u8> {
        // A combo carries its own Control. It still clears whatever the user
        // had made sticky, so Ctrl · Ctrl+C never encodes a double modifier
        // and never leaves Ctrl armed for the next key.
        let control = matches!(key, TerminalKey::Control(_));
        let key = key.mapping_name();
        let mut modifiers = self.modifiers.take();
        modifiers.control |= control;
        mappings::key_bytes(
            &key,
            modifiers,
            mode,
            keyboard_mode,
            modify_other_keys,
            true,
        )
        .unwrap_or_else(|| key.into_bytes())
    }

    /// Encode committed software-keyboard text. Sticky modifiers apply to the
    /// first character only; every character still passes through the shared
    /// mapping so kitty and modifyOtherKeys modes match hardware input.
    fn encode_text(
        &mut self,
        text: &str,
        mode: TerminalMode,
        keyboard_mode: KeyboardModes,
        modify_other_keys: Option<u8>,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, character) in text.chars().enumerate() {
            let key = match character {
                '\n' | '\r' => "enter".to_owned(),
                '\t' => "tab".to_owned(),
                character => character.to_string(),
            };
            let modifiers = if index == 0 {
                self.modifiers.take()
            } else {
                Modifiers::default()
            };
            if let Some(encoded) = mappings::key_bytes(
                &key,
                modifiers,
                mode,
                keyboard_mode,
                modify_other_keys,
                true,
            ) {
                bytes.extend(encoded);
            } else {
                bytes.extend(character.to_string().into_bytes());
            }
        }
        bytes
    }
}

impl TerminalKeyBar {
    pub(crate) fn new(terminal_focus: FocusHandle) -> Self {
        Self {
            terminal_focus,
            encoder: TerminalKeyEncoder::new(),
        }
    }

    pub(crate) fn encode_key(
        &mut self,
        key: TerminalKey,
        mode: TerminalMode,
        keyboard_mode: KeyboardModes,
        modify_other_keys: Option<u8>,
    ) -> Vec<u8> {
        self.encoder
            .encode_key(key, mode, keyboard_mode, modify_other_keys)
    }

    pub(crate) fn encode_text(
        &mut self,
        text: &str,
        mode: TerminalMode,
        keyboard_mode: KeyboardModes,
        modify_other_keys: Option<u8>,
    ) -> Vec<u8> {
        self.encoder
            .encode_text(text, mode, keyboard_mode, modify_other_keys)
    }

    fn key_button(
        &self,
        id: &'static str,
        label: impl Into<gpui::SharedString>,
        accessibility_label: String,
        key: TerminalKey,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let label = label.into();
        let focus = self.terminal_focus.clone();
        material::accessible_clickable(div(), id, Role::Button, accessibility_label, cx)
            .debug_selector(move || id.into())
            .flex_none()
            .size(px(material::TOUCH_TARGET))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .rounded(material::radius_button())
            .text_size(px(13.))
            .hover(|style| style.bg(cx.theme().accent))
            .on_click(cx.listener(move |_, _, window, cx| {
                focus.focus(window, cx);
                crate::window_seam::WindowSeam::request_soft_keyboard(cx);
                cx.emit(TerminalKeyBarEvent(key));
            }))
            .child(label)
    }

    fn modifier_button(
        &self,
        id: &'static str,
        label: &'static str,
        accessibility_label: String,
        selected: bool,
        control: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let focus = self.terminal_focus.clone();
        material::accessible_clickable(div(), id, Role::Button, accessibility_label, cx)
            .debug_selector(move || id.into())
            .aria_selected(selected)
            .flex_none()
            .size(px(material::TOUCH_TARGET))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .rounded(material::radius_button())
            .text_size(px(12.))
            .font_medium()
            .when(selected, |style| {
                style
                    .bg(cx.theme().primary)
                    .text_color(cx.theme().primary_foreground)
            })
            .when(!selected, |style| {
                style.hover(|hover| hover.bg(cx.theme().accent))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if control {
                    this.encoder.modifiers.control = !this.encoder.modifiers.control;
                } else {
                    this.encoder.modifiers.alt = !this.encoder.modifiers.alt;
                }
                focus.focus(window, cx);
                crate::window_seam::WindowSeam::request_soft_keyboard(cx);
                cx.notify();
            }))
            .child(label)
    }
}

impl EventEmitter<TerminalKeyBarEvent> for TerminalKeyBar {}

impl Render for TerminalKeyBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fixed = h_flex()
            .flex_none()
            .child(self.key_button(
                "terminal-key-escape",
                "Esc",
                crate::tr!("terminal.key_escape").into_owned(),
                TerminalKey::Escape,
                cx,
            ))
            .child(self.key_button(
                "terminal-key-tab",
                "Tab",
                crate::tr!("terminal.key_tab").into_owned(),
                TerminalKey::Tab,
                cx,
            ))
            .child(self.modifier_button(
                "terminal-key-control",
                "Ctrl",
                crate::tr!("terminal.key_control").into_owned(),
                self.encoder.modifiers.control,
                true,
                cx,
            ))
            .child(self.modifier_button(
                "terminal-key-alt",
                "Alt",
                crate::tr!("terminal.key_alt").into_owned(),
                self.encoder.modifiers.alt,
                false,
                cx,
            ));
        let arrows = [
            (
                "terminal-key-left",
                "←",
                "terminal.key_left",
                TerminalKey::Left,
            ),
            ("terminal-key-up", "↑", "terminal.key_up", TerminalKey::Up),
            (
                "terminal-key-down",
                "↓",
                "terminal.key_down",
                TerminalKey::Down,
            ),
            (
                "terminal-key-right",
                "→",
                "terminal.key_right",
                TerminalKey::Right,
            ),
        ];
        let mut tail = h_flex().flex_none().pr(px(16.));
        for (id, label, translation, key) in arrows {
            tail = tail.child(self.key_button(
                id,
                label,
                crate::tr!(translation).into_owned(),
                key,
                cx,
            ));
        }

        // The two combos the sticky Ctrl modifier is otherwise used for, one
        // tap each, right after the arrows they sit next to on a keyboard.
        for (id, label, translation, key) in [
            (
                "terminal-key-ctrl-c",
                "^C",
                "terminal.key_ctrl_c",
                TerminalKey::Control('c'),
            ),
            (
                "terminal-key-ctrl-d",
                "^D",
                "terminal.key_ctrl_d",
                TerminalKey::Control('d'),
            ),
        ] {
            tail = tail.child(self.key_button(
                id,
                label,
                crate::tr!(translation).into_owned(),
                key,
                cx,
            ));
        }

        for (id, symbol) in SYMBOL_KEYS {
            tail = tail.child(self.key_button(
                id,
                symbol.to_string(),
                crate::tr!("terminal.key_symbol", symbol = symbol).into_owned(),
                TerminalKey::Symbol(symbol),
                cx,
            ));
        }

        h_flex()
            .id("terminal-key-bar")
            .debug_selector(|| "terminal-key-bar".into())
            .role(Role::Toolbar)
            .aria_label(crate::tr!("terminal.key_bar"))
            .flex_none()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .h(px(KEY_BAR_HEIGHT))
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .child(fixed)
            .child(
                h_flex()
                    .id("terminal-key-scroll")
                    .debug_selector(|| "terminal-key-scroll".into())
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_hidden()
                    .touch_overflow_x_scroll()
                    .child(tail),
            )
    }
}

pub(crate) fn should_show_terminal_key_bar(terminal_focused: bool, cx: &App) -> bool {
    crate::window_seam::soft_keyboard_for_key_bar(cx) && terminal_focused
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};

    #[test]
    fn sticky_control_encodes_one_character_then_clears() {
        let mut state = TerminalKeyEncoder {
            modifiers: StickyModifiers {
                control: true,
                alt: false,
            },
        };
        assert_eq!(
            state.encode_text("c", TerminalMode::empty(), KeyboardModes::NO_MODE, None),
            vec![0x03]
        );
        assert_eq!(state.modifiers, StickyModifiers::default());
        assert_eq!(
            state.encode_text("c", TerminalMode::empty(), KeyboardModes::NO_MODE, None),
            b"c"
        );
    }

    #[test]
    fn arrows_use_the_replicated_application_cursor_mode() {
        let mut state = TerminalKeyEncoder {
            modifiers: StickyModifiers::default(),
        };
        assert_eq!(
            state.encode_key(
                TerminalKey::Up,
                TerminalMode::empty(),
                KeyboardModes::NO_MODE,
                None
            ),
            b"\x1b[A"
        );
        assert_eq!(
            state.encode_key(
                TerminalKey::Up,
                TerminalMode::APP_CURSOR,
                KeyboardModes::NO_MODE,
                None,
            ),
            b"\x1bOA"
        );
    }

    /// A combo is one tap: it encodes through the same `key_bytes` path a
    /// hardware Ctrl+C takes, and it consumes any sticky Ctrl rather than
    /// doubling it or leaving it armed for the next key.
    #[test]
    fn control_combos_encode_once_and_clear_the_sticky_modifiers() {
        let mut state = TerminalKeyEncoder {
            modifiers: StickyModifiers::default(),
        };
        for (key, byte) in [
            (TerminalKey::Control('c'), 0x03),
            (TerminalKey::Control('d'), 0x04),
        ] {
            assert_eq!(
                state.encode_key(key, TerminalMode::empty(), KeyboardModes::NO_MODE, None),
                vec![byte]
            );
        }

        state.modifiers = StickyModifiers {
            control: true,
            alt: false,
        };
        assert_eq!(
            state.encode_key(
                TerminalKey::Control('c'),
                TerminalMode::empty(),
                KeyboardModes::NO_MODE,
                None,
            ),
            vec![0x03],
            "a sticky Ctrl on top of the combo is still one Control-C"
        );
        assert_eq!(state.modifiers, StickyModifiers::default());
        assert_eq!(
            state.encode_text("c", TerminalMode::empty(), KeyboardModes::NO_MODE, None),
            b"c",
            "the combo must not leave Ctrl armed for the next key"
        );
    }

    /// The tail is ordered by how often a shell line needs the character, so
    /// what scrolls off the edge is what is reached for least.
    #[test]
    fn the_symbol_tail_is_ordered_by_shell_frequency() {
        assert_eq!(
            SYMBOL_KEYS.map(|(_, symbol)| symbol),
            ['-', '/', '|', '~', ':', '.', '_']
        );
    }

    #[test]
    fn escape_is_the_terminal_escape_byte() {
        let mut state = TerminalKeyEncoder {
            modifiers: StickyModifiers::default(),
        };
        assert_eq!(
            state.encode_key(
                TerminalKey::Escape,
                TerminalMode::empty(),
                KeyboardModes::NO_MODE,
                None,
            ),
            vec![0x1b]
        );
    }

    struct NarrowBarProbe {
        width: f32,
        bar: Entity<TerminalKeyBar>,
    }

    impl Render for NarrowBarProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            crate::touch_scroll::root(div().w(px(self.width)).child(self.bar.clone()))
        }
    }

    #[gpui::test]
    fn phone_bar_scrolls_without_losing_pinned_keys_or_vertical_motion(cx: &mut TestAppContext) {
        use gpui::{PlatformInput, TouchEvent, TouchId, TouchPhase, point};
        cx.update(crate::theme::init);
        let requests = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = requests.clone();
        cx.update(|cx| {
            cx.set_global(
                crate::window_seam::WindowSeam::new(Default::default)
                    .with_soft_keyboard(move || observed.set(observed.get() + 1)),
            )
        });
        for width in [360., 393.] {
            let (_, cx) = cx.add_window_view(|_, cx| NarrowBarProbe {
                width,
                bar: cx.new(|cx| TerminalKeyBar::new(cx.focus_handle())),
            });
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let viewport = cx.debug_bounds("terminal-key-scroll").unwrap();
            let pinned = cx.debug_bounds("terminal-key-escape").unwrap();
            let first = cx.debug_bounds("terminal-key-left").unwrap();
            let last = cx.debug_bounds("terminal-key-underscore").unwrap();
            assert_eq!(
                cx.debug_bounds("terminal-key-bar").unwrap().size.width,
                px(width)
            );
            assert_eq!(pinned.size.width, px(44.));
            assert_eq!(pinned.size.height, px(44.));
            assert!(
                last.right() > viewport.right(),
                "content must exceed the viewport"
            );
            assert!(viewport.right() <= px(width));
            let send = |phase, x, y, cx: &mut VisualTestContext| {
                cx.update(|window, cx| {
                    window.dispatch_event(
                        PlatformInput::Touch(TouchEvent {
                            id: TouchId(1),
                            phase,
                            position: point(px(x), px(y)),
                            predicted_position: None,
                            force: None,
                        }),
                        cx,
                    );
                    let _ = window.draw(cx);
                });
            };
            send(TouchPhase::Started, width - 20., 20., cx);
            send(TouchPhase::Moved, 190., 20., cx);
            send(TouchPhase::Cancelled, 190., 20., cx);
            let moved = cx.debug_bounds("terminal-key-left").unwrap();
            assert!(moved.left() < first.left(), "horizontal swipe must scroll");
            assert_eq!(moved.top(), first.top());
            assert_eq!(cx.debug_bounds("terminal-key-escape").unwrap(), pinned);
            send(TouchPhase::Started, 190., 20., cx);
            send(TouchPhase::Moved, width + 300., 20., cx);
            send(TouchPhase::Cancelled, width + 300., 20., cx);
            assert_eq!(cx.debug_bounds("terminal-key-left").unwrap(), first);
            let before = requests.get();
            cx.simulate_click(pinned.center(), gpui::Modifiers::default());
            assert_eq!(requests.get(), before + 1, "key-bar taps reopen the IME");
        }
    }

    struct KeyBarProbe {
        terminal_focused: bool,
        bar: Entity<TerminalKeyBar>,
    }

    impl Render for KeyBarProbe {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().when(
                should_show_terminal_key_bar(self.terminal_focused, cx),
                |root| root.child(self.bar.clone()),
            )
        }
    }

    #[gpui::test]
    fn bar_renders_only_for_a_focused_soft_keyboard_terminal(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (probe, cx) = cx.add_window_view(|_, cx| {
            let focus = cx.focus_handle();
            let bar_focus = focus.clone();
            KeyBarProbe {
                terminal_focused: false,
                bar: cx.new(|_| TerminalKeyBar::new(bar_focus)),
            }
        });
        let cx: &mut VisualTestContext = cx;
        let draw = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                _ = window.draw(cx);
            });
        };

        cx.update(|_, cx| crate::window_seam::override_soft_keyboard_for_test(cx, false));
        probe.update(cx, |probe, cx| {
            probe.terminal_focused = true;
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("terminal-key-bar").is_none());
        cx.update(|_, cx| crate::window_seam::override_soft_keyboard_for_test(cx, true));
        probe.update(cx, |probe, cx| {
            probe.terminal_focused = false;
            cx.notify();
        });
        draw(cx);
        assert!(cx.debug_bounds("terminal-key-bar").is_none());

        probe.update(cx, |probe, cx| {
            probe.terminal_focused = true;
            cx.notify();
        });
        draw(cx);
        assert_eq!(
            cx.debug_bounds("terminal-key-bar")
                .expect("focused phone terminal key bar")
                .size
                .height,
            px(KEY_BAR_HEIGHT)
        );
    }
}
