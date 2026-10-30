use std::rc::Rc;

use crate::widgets::kbd::Kbd;
use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate, Keystroke, Modifiers, NoAction};
use serde::Deserialize;

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) enum NavigateThread {
    Index(usize),
    Next,
    Previous,
}

pub(crate) fn init(cx: &mut App) {
    cx.on_action(crate::shell::navigate_thread);
    crate::shell::init_pane_actions(cx);
    // With nothing focused the shell's own Undo listener is off the dispatch path.
    cx.on_action(crate::shell::undo_thread_action);
    let bindings = [
        secondary("secondary-z", crate::sidebar::ThreadUndo, None, cx),
        secondary("secondary-c", gpui_base::input::Copy, Some("TcodeDiff"), cx),
        #[cfg(target_os = "ios")]
        KeyBinding::new("cmd-c", gpui_base::input::Copy, Some("TcodeDiff")),
        // GPUI gives later bindings precedence, including a global binding over
        // the editor's earlier contextual one. Keep the editor's native action.
        secondary("secondary-z", gpui_base::input::Undo, Some("Input"), cx),
    ];
    cx.bind_keys(bindings);
    // gpui-base gates Apple's editor bindings to macOS. iOS hardware
    // keyboards use Command too, while GPUI's secondary modifier is Control.
    #[cfg(target_os = "ios")]
    cx.bind_keys([
        KeyBinding::new("cmd-a", gpui_base::input::SelectAll, Some("Input")),
        KeyBinding::new("cmd-c", gpui_base::input::Copy, Some("Input")),
        KeyBinding::new("cmd-x", gpui_base::input::Cut, Some("Input")),
        KeyBinding::new("cmd-v", gpui_base::input::Paste, Some("Input")),
        KeyBinding::new("cmd-z", gpui_base::input::Undo, Some("Input")),
        KeyBinding::new("cmd-shift-z", gpui_base::input::Redo, Some("Input")),
    ]);
    // A browser hands the clipboard only to its own paste event, which the
    // key fires once no binding claims it; gpui-base's Paste would read the
    // clipboard itself, which a page without a secure context cannot.
    #[cfg(target_family = "wasm")]
    cx.bind_keys([KeyBinding::new("ctrl-v", NoAction, Some("Input"))]);
    for number in 1..=9 {
        let key = format!("secondary-{number}");
        let bindings = [
            secondary(&key, NavigateThread::Index(number - 1), None, cx),
            secondary(
                &key,
                NoAction,
                Some("ModelPicker || ModelPicker > Input"),
                cx,
            ),
        ];
        cx.bind_keys(bindings);
    }
    cx.bind_keys([
        KeyBinding::new("ctrl-tab", NavigateThread::Next, None),
        KeyBinding::new("ctrl-shift-tab", NavigateThread::Previous, None),
    ]);
    // Later than gpui-base's editor bindings, so these win inside a text
    // field too, where they shadow only its add-cursor keys on macOS and
    // Windows (`secondary-alt-up`/`-down`).
    {
        use crate::panes::{ClosePane, FocusPane, SplitDown, SplitRight, ZoomPane};
        let [split_right, split_down, close, zoom, left, right, up, down] = PANE_KEYS;
        cx.bind_keys([
            KeyBinding::new(split_right, SplitRight, None),
            KeyBinding::new(split_down, SplitDown, None),
            KeyBinding::new(close, ClosePane, None),
            KeyBinding::new("escape", ClosePane, Some(crate::panes::WAITING_CONTEXT)),
            KeyBinding::new(zoom, ZoomPane, None),
            KeyBinding::new(left, FocusPane::Left, None),
            KeyBinding::new(right, FocusPane::Right, None),
            KeyBinding::new(up, FocusPane::Up, None),
            KeyBinding::new(down, FocusPane::Down, None),
        ]);
    }
    // Bindings dispatch before key-down listeners, and the terminal forwards
    // raw keystrokes (Tab, Ctrl-C as an interrupt) from its listener, so the
    // window root's focus traversal and selection copy must not claim them.
    let terminal = Some(crate::terminal_drawer::CONTEXT);
    let undo = secondary("secondary-z", NoAction, terminal, cx);
    // A terminal reads every pane key itself (Ctrl-D is EOF), so none is a
    // binding while it has the keyboard.
    cx.bind_keys(PANE_KEYS.map(|key| KeyBinding::new(key, NoAction, terminal)));
    cx.bind_keys([
        undo,
        KeyBinding::new("tab", NoAction, terminal),
        KeyBinding::new("shift-tab", NoAction, terminal),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", NoAction, terminal),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", NoAction, terminal),
    ]);
}

/// A binding whose `secondary-` is GPUI's semantic secondary modifier for the
/// host OS: Command on a Mac, a browser on one included, and Control elsewhere.
pub(crate) fn secondary<A: Action>(
    keystrokes: &str,
    action: A,
    context: Option<&str>,
    cx: &App,
) -> KeyBinding {
    let context = context.map(|context| {
        Rc::new(KeyBindingContextPredicate::parse(context).expect("valid key context"))
    });
    KeyBinding::load(keystrokes, Box::new(action), context, false, None, cx)
        .expect("valid keystroke")
}

/// The chat column's pane commands' keys, in [`init`]'s order.
const PANE_KEYS: [&str; 8] = [
    "secondary-d",
    "secondary-shift-d",
    "secondary-shift-w",
    "secondary-shift-enter",
    "secondary-alt-left",
    "secondary-alt-right",
    "secondary-alt-up",
    "secondary-alt-down",
];

/// Format a shortcut using GPUI's semantic secondary modifier.
pub(crate) fn format_secondary_shortcut(key: &str, cx: &App) -> String {
    let os = cx.platform_info().os;
    Kbd::format(
        &Keystroke {
            modifiers: Modifiers::secondary_key(cx),
            key: key.to_owned(),
            key_char: None,
        },
        os,
    )
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{
        AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
        KeyDownEvent, ParentElement as _, Render, TestAppContext, VisualTestContext, Window, div,
    };

    struct Harness {
        terminal: FocusHandle,
        composer: FocusHandle,
        keys: Vec<String>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .child(
                    div()
                        .key_context(crate::terminal_drawer::CONTEXT)
                        .track_focus(&self.terminal)
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                            this.keys
                                .push(event.keystroke.unparse(cx.platform_info().os));
                        })),
                )
                .child(
                    div()
                        .key_context(crate::composer::CONTEXT)
                        .child(div().track_focus(&self.composer)),
                )
        }
    }

    fn mount(cx: &mut TestAppContext) -> (Entity<Harness>, &mut VisualTestContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            super::init(cx);
        });
        let built = Rc::new(RefCell::new(None));
        let capture = built.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let harness = cx.new(|cx| Harness {
                terminal: cx.focus_handle().tab_stop(true),
                composer: cx.focus_handle().tab_stop(true),
                keys: Vec::new(),
            });
            *capture.borrow_mut() = Some(harness.clone());
            gpui_base::Root::new(harness, window, cx)
        });
        let harness = built.borrow_mut().take().unwrap();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (harness, cx)
    }

    #[gpui::test]
    fn root_focus_traversal_leaves_terminal_its_keys(cx: &mut TestAppContext) {
        let (harness, cx) = mount(cx);
        let terminal = harness.read_with(cx, |harness, _| harness.terminal.clone());
        cx.update(|window, cx| terminal.focus(window, cx));
        cx.simulate_keystrokes("tab shift-tab");
        harness.read_with(cx, |harness, _| {
            assert_eq!(harness.keys, ["tab", "shift-tab"]);
        });
        assert!(cx.update(|window, _| terminal.is_focused(window)));
    }
}
