//! Plain-text command rendering for targets without terminal emulation.

use std::collections::HashMap;

use gpui::{
    AnyElement, App, IntoElement as _, ParentElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::v_flex;

pub(crate) type ColsChangeHandler = Box<dyn Fn(&usize, &mut Window, &mut App) + 'static>;

pub(crate) struct CommandPanelCache {
    entries: HashMap<String, (String, String)>,
}

impl CommandPanelCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn resize(&mut self, _id: &str, _cols: usize) -> bool {
        false
    }

    pub(crate) fn render(
        &mut self,
        id: &str,
        command: &str,
        output: &str,
        _on_cols_change: Option<ColsChangeHandler>,
        _cx: &App,
    ) -> AnyElement {
        self.entries
            .insert(id.to_string(), (command.to_string(), output.to_string()));
        v_flex()
            .w_full()
            .overflow_hidden()
            .font_family("monospace")
            .text_size(px(12.))
            .child(div().child(command.to_string()))
            .when(!output.is_empty(), |panel| {
                panel.child(div().child(output.to_string()))
            })
            .into_any_element()
    }
}
