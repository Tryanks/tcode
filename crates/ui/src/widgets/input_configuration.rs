//! Adds native keyboard intent to the upstream editor's input handler.
#[cfg(target_os = "ios")]
use gpui::Focusable as _;
use gpui::{
    App, Bounds, ClipboardItem, ElementInputHandler, Entity, InputHandler, Pixels, Point,
    TextInputAction, TextInputConfiguration, UTF16Selection, Window,
};
use gpui_base::input::{InputBaseState, InputModeKind, RopeExt as _};
use std::ops::Range;

// Supply the native selection/length hooks and keyboard intent missing upstream.
pub(super) struct ConfiguredInput<M: InputModeKind> {
    inner: ElementInputHandler<InputBaseState<M>>,
    entity: Entity<InputBaseState<M>>,
    multi_line: bool,
    text_assistance: bool,
}

impl<M: InputModeKind> ConfiguredInput<M> {
    pub fn new(
        bounds: Bounds<Pixels>,
        entity: Entity<InputBaseState<M>>,
        multi_line: bool,
        text_assistance: bool,
    ) -> Self {
        Self {
            inner: ElementInputHandler::new(bounds, entity.clone()),
            entity,
            multi_line,
            text_assistance,
        }
    }
}

impl<M: InputModeKind> InputHandler for ConfiguredInput<M> {
    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.inner
            .selected_text_range(ignore_disabled_input, window, cx)
    }
    fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.inner.marked_text_range(window, cx)
    }
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        self.inner
            .text_for_range(range_utf16, adjusted_range, window, cx)
    }
    fn replace_text_in_range(
        &mut self,
        replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner
            .replace_text_in_range(replacement_range, text, window, cx)
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.inner.replace_and_mark_text_in_range(
            range_utf16,
            new_text,
            new_selected_range,
            window,
            cx,
        )
    }
    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        self.inner.unmark_text(window, cx)
    }
    fn paste(&mut self, item: ClipboardItem, window: &mut Window, cx: &mut App) {
        #[cfg(target_os = "ios")]
        {
            // gpui-base's InputHandler uses ordinary typing replacement for
            // paste. Its Paste action owns atomic undo and selection semantics.
            let focus = self.entity.read(cx).focus_handle(cx);
            if !focus.is_focused(window) {
                return;
            }
            gpui_ios::with_native_paste(item, || {
                focus.dispatch_action(&gpui_base::input::Paste, window, cx);
            });
        }
        #[cfg(not(target_os = "ios"))]
        self.inner.paste(item, window, cx)
    }
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.inner.bounds_for_range(range_utf16, window, cx)
    }
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<usize> {
        self.inner.character_index_for_point(point, window, cx)
    }
    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.entity.update(cx, |state, cx| {
            let text = state.text();
            let range = text.offset_utf16_to_offset(range_utf16.start)
                ..text.offset_utf16_to_offset(range_utf16.end);
            if state.selected_range() != range {
                state.set_selected_range(range, cx);
            }
        });
    }
    fn element_bounds(&mut self, window: &mut Window, cx: &mut App) -> Option<Bounds<Pixels>> {
        self.inner.element_bounds(window, cx)
    }
    fn text_length_utf16(&mut self, _window: &mut Window, cx: &mut App) -> Option<usize> {
        Some(self.entity.read(cx).text().len_utf16())
    }
    fn apple_press_and_hold_enabled(&mut self) -> bool {
        self.inner.apple_press_and_hold_enabled()
    }
    fn accepts_text_input(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.inner.accepts_text_input(window, cx)
    }
    fn text_input_editable_range(
        &mut self,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Range<usize>> {
        self.inner.text_input_editable_range(window, cx)
    }
    fn prefers_ime_for_printable_keys(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.inner.prefers_ime_for_printable_keys(window, cx)
    }
    fn text_input_configuration(
        &mut self,
        window: &mut Window,
        cx: &mut App,
    ) -> TextInputConfiguration {
        let mut configuration = self.inner.text_input_configuration(window, cx);
        configuration.autocorrect = self.text_assistance;
        configuration.suggestions = self.text_assistance;
        configuration.input_action = if self.multi_line {
            TextInputAction::Enter
        } else {
            TextInputAction::Done
        };
        configuration
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_base::input::TextareaState;

    #[gpui::test]
    fn native_selection_replaces_the_word_after_an_emoji(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (input, cx) = cx.add_window_view(|window, cx| {
            TextareaState::new(window, cx).default_value("😀 exmple here")
        });
        cx.update(|window, cx| {
            let mut handler = ConfiguredInput::new(Bounds::default(), input.clone(), true, true);
            assert_eq!(handler.text_length_utf16(window, cx), Some(14));
            handler.set_selected_text_range(3..9, window, cx);
            handler.replace_text_in_range(None, "example", window, cx);
            assert_eq!(input.read(cx).value(), "😀 example here");
            assert_eq!(
                handler.selected_text_range(true, window, cx).unwrap().range,
                10..10
            );
        });
    }
}
