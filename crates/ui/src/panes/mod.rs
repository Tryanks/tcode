//! The desktop chat column as a gpui-base dock of thread panes.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Div, Empty, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Stateful, Styled as _,
    Subscription, Window, div,
};
use gpui_base::dock::{DockAreaRenderer, Panel, PanelEvent, TabGroupContext, TabGroupRenderer};

use crate::chat::ChatView;

/// One thread shown in one pane.
pub(crate) struct ThreadPanel {
    chat: Entity<ChatView>,
    /// The pane's own focus, tracked by its tab group. The composer's would
    /// make the group's click-to-focus bypass the composer's refusals (a
    /// software keyboard, a read-only thread) and add the composer to the tab
    /// order a second time.
    focus_handle: FocusHandle,
    _focus: Subscription,
}

impl ThreadPanel {
    pub(crate) fn new(chat: Entity<ChatView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let focus = cx.on_focus(&focus_handle, window, |panel, window, cx| {
            panel
                .chat
                .update(cx, |chat, cx| chat.focus_composer(window, cx));
        });
        Self {
            chat,
            focus_handle,
            _focus: focus,
        }
    }

    /// The thread the pane shows.
    pub(crate) fn session_id<'a>(&self, cx: &'a App) -> Option<&'a str> {
        self.chat.read(cx).session_id()
    }

    pub(crate) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        self.chat
            .update(cx, |chat, cx| chat.set_session(session_id, cx));
    }
}

impl EventEmitter<PanelEvent> for ThreadPanel {}

impl Focusable for ThreadPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ThreadPanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.chat.clone())
    }
}

impl Panel for ThreadPanel {
    fn panel_name(&self) -> &'static str {
        "thread"
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn zoomable(&self, _: &App) -> bool {
        false
    }
}

/// The panes' appearance: the area is the chat column's reading surface.
pub(crate) struct ThreadPanes;

impl DockAreaRenderer for ThreadPanes {
    fn frame(&self, _: &mut Window, cx: &mut App) -> Stateful<Div> {
        div()
            .id("thread-panes")
            .bg(crate::material::content_surface(cx))
            .shadow_sm()
    }

    fn tab_group_renderer(&self) -> Rc<dyn TabGroupRenderer> {
        Rc::new(ThreadTabs)
    }
}

/// A pane's thread wears its own header, so the group draws no tab bar.
struct ThreadTabs;

impl TabGroupRenderer for ThreadTabs {
    fn render_tab_bar(&self, _: &TabGroupContext, _: &mut Window, _: &mut App) -> AnyElement {
        Empty.into_any_element()
    }
}
