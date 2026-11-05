//! A pane's right panel. A pane is `[conversation | panel]`: the panel holds
//! its width beside the conversation, which takes what is left, and the seam
//! between them is the panel's left edge. Opening and closing glide the split
//! within a thread. A pane too narrow for both has the panel cover it, sliding
//! in from its right edge, and an expanded panel covers the pane with the
//! conversation still laid out at its width beneath. The panel never leaves
//! the pane: other panes each have their own.
//!
//! Whether it is open, its tab and whether it is expanded are the
//! conversation's ([`ConversationUiState`]), so a pane shows the panel exactly
//! when its thread has it open, and the panel stays with the thread when the
//! thread changes panes. Its width is one preference of the user's, which
//! each pane clamps to itself. Another thread in the pane
//! is a new scene: the panel lands in that thread's state at once, open or
//! shut, never as if the reader had opened or closed it.
//!
//! gpui-base's PopoverState and Sheet are modal: a popover registers a
//! deferred popup, which suspends the text fields' context menus and the
//! chat's scrolling capture window-wide, and a sheet traps focus. The panel
//! leaves the chat beside it working, so it keeps to the pane's own layout,
//! and where it covers the chat it takes and returns the keyboard itself.
//!
//! [`ConversationUiState`]: crate::conversation_ui::ConversationUiState

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Axis, Bounds, Context, Entity, FocusHandle,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, actions, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{ElementExt as _, ResizableState, h_resizable, resizable_panel};
use tcode_core::ui::RightTab;

use super::ThreadPanel;
use crate::chat::ChatView;
use crate::diff::DiffPanel;
use crate::motion::resize_glide::{Panel, ResizeGlide};
use crate::preview_panel::PreviewPanel;
use crate::store::WorkspaceStore;
use crate::window_state::WindowState;

actions!(tcode_shell, [CloseRightPanel]);

/// The key context of a panel covering its pane, where Escape closes it: a
/// docked one holds comment editors and web pages where Escape means
/// something else.
pub(crate) const RIGHT_PANEL_CONTEXT: &str = "RightPanel";

/// How long the panel takes to slide, to grow when expanded, and to go from
/// beside the conversation to over it.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long one tab takes to fade into the other's layer.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// The panel's width until the user drags its edge.
const RIGHT_PANEL_WIDTH: f32 = 560.;

/// The panel's narrowest: the icon tabs and the panel's own controls fit in
/// a panel's narrowest content, beside the window's caption buttons where the
/// panel's strip holds them.
const RIGHT_PANEL_MIN_WIDTH: f32 =
    crate::material::PANEL_CONTENT_MIN_WIDTH + crate::window_caption::RIGHT_PANEL_CAPTION_WIDTH;

/// The conversation's narrowest beside the panel: a pane's own, so the panel
/// yields rather than asking the column for more room.
const CONVERSATION_MIN_WIDTH: f32 = crate::shell::PANE_MIN_WIDTH;

/// Where the panel rests in a pane.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Rest {
    /// Beside the conversation, this wide.
    Beside(Pixels),
    /// Over the whole pane: there is no room for both.
    Cover,
}

/// Whether a pane `pane` wide has no room for a conversation and a panel
/// side by side.
pub(crate) fn panel_covers(pane: Pixels) -> bool {
    pane < px(RIGHT_PANEL_MIN_WIDTH + CONVERSATION_MIN_WIDTH)
}

/// The width the user chose, `chosen`, as far as a pane `pane` wide holds it
/// beside the conversation.
fn beside_width(chosen: Pixels, pane: Pixels) -> Pixels {
    chosen
        .min(pane - px(CONVERSATION_MIN_WIDTH))
        .max(px(RIGHT_PANEL_MIN_WIDTH))
        .min(pane)
}

/// Where a panel the user chose `chosen` for rests in a pane `pane` wide.
fn rest_width(chosen: Pixels, pane: Pixels) -> Rest {
    if panel_covers(pane) {
        Rest::Cover
    } else {
        Rest::Beside(beside_width(chosen, pane))
    }
}

/// What one pane keeps of its drawer. Its tabs are made as it first opens
/// and stay for the pane's life, what they built let go once it has shut.
///
/// An entity of its own rather than the pane's state: the panel is laid out
/// in the frame's prepaint, once the pane's width is known, and what it
/// keeps from frame to frame is written there. Written into the pane, that
/// would draw again every view that reads the pane (the threads list reads
/// which thread it shows) on the frame after each one the pane draws.
pub(crate) struct Drawer {
    /// The pane it draws, whose conversation it lays out beside its panel.
    pane: WeakEntity<ThreadPanel>,
    /// The pane's width this frame, handed by [`Split`].
    pane_width: Pixels,
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// The attachment's panel, which owns the pages this pane's shows.
    browser: Entity<PreviewPanel>,
    tabs: Option<Entity<DiffPanel>>,
    /// The Preview tab once made, shared with the pane, which syncs its
    /// page as the window renders without reading the drawer.
    preview: Rc<std::cell::RefCell<Option<Entity<PreviewPanel>>>>,
    /// The thread the pane shows: another one, or the drawer taking its
    /// content down, starts a new scene, so what is put up starts at rest.
    scene: crate::motion::Epoch,
    /// Kept here rather than in element state: the dock draws the pane under
    /// a new parent whenever the layout around it changes.
    motion: crate::motion::owned::Owned,
    /// The pane's group (the conversation · the panel's room), whose handle
    /// is the panel's left edge.
    split: Entity<ResizableState>,
    /// The panel's room growing out of nothing as it opens, and back.
    glide: ResizeGlide,
    /// The thread the panel showed last frame.
    subject: Option<String>,
    /// The pane's width as last handed to the chat it holds, kept here: a
    /// read of the chat would draw the pane again with every frame the chat
    /// draws.
    handed_width: Option<(gpui::EntityId, Pixels)>,
    /// The panel's room in the group last frame, while it is drawn.
    laid_out: Option<Pixels>,
    /// Where the panel's content holds the keyboard.
    focus: FocusHandle,
    /// What had the keyboard when the reader opened the panel, given back
    /// when they close it from inside.
    return_to: Option<FocusHandle>,
    /// Whether the panel was open, whether it was drawn (open or sliding
    /// out), and whether it rested over the pane, last frame.
    open: bool,
    drawn: bool,
    covering: bool,
}

/// How the panel is drawn this frame.
struct PanelFrame {
    tab: RightTab,
    /// Whether it rests over the pane, where Escape closes it.
    covers: bool,
    /// How far it is from docked to floating over the pane.
    lift: f32,
    /// Whether it is on its way out.
    leaving: bool,
    subject: Option<String>,
}

/// Lays a pane's content out at the pane's width this frame: the drawer
/// works out the panel's room from it before the conversation is laid out in
/// what is left.
pub(super) type Split = crate::width_frame::WidthFrame<Drawer>;

impl Render for Drawer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pane) = self.pane.upgrade() else {
            return div().into_any_element();
        };
        let host = pane.read(cx).drawer_host();
        let conversation = ThreadPanel::conversation(&pane, window, cx);
        self.render_split(&host, conversation, self.pane_width, window, cx)
    }
}

impl Drawer {
    /// The drawer of `pane`.
    pub(super) fn in_pane(mut self, pane: WeakEntity<ThreadPanel>) -> Self {
        self.pane = pane;
        self
    }

    pub(super) fn set_pane_width(&mut self, width: Pixels) {
        self.pane_width = width;
    }

    /// Where the drawer keeps its Preview tab once made.
    pub(super) fn preview(&self) -> Rc<std::cell::RefCell<Option<Entity<PreviewPanel>>>> {
        self.preview.clone()
    }

    pub(crate) fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        browser: Entity<PreviewPanel>,
        session_id: Option<&str>,
        cx: &mut App,
    ) -> Self {
        let scene = crate::motion::Epoch::default();
        scene.show(session_id);
        Self {
            pane: WeakEntity::new_invalid(),
            pane_width: px(0.),
            store,
            window_state,
            browser,
            tabs: None,
            preview: Rc::default(),
            motion: crate::motion::owned::Owned::in_scene(&scene),
            glide: ResizeGlide::new("right-panel-split", Axis::Horizontal).in_scene(&scene),
            scene,
            split: cx.new(|_| ResizableState::default()),
            subject: None,
            handed_width: None,
            laid_out: None,
            focus: cx.focus_handle(),
            return_to: None,
            open: false,
            drawn: false,
            covering: false,
        }
    }
}

/// What the drawer reads of the pane it sits in, as the pane is now.
struct Host {
    session_id: Option<String>,
    chat: Entity<ChatView>,
    /// The pane's own focus, which a panel narrowing over it takes the
    /// keyboard from.
    focus: FocusHandle,
    /// Whether another pane is drawn over this one.
    covered: bool,
    /// Where the pane's conversation was last painted, which the drawer
    /// records as it paints it.
    conversation: Rc<Cell<Bounds<Pixels>>>,
}

impl Host {
    /// The thread the drawer shows.
    fn subject(&self, cx: &App) -> Option<String> {
        self.session_id.as_ref()?;
        self.chat.read(cx).drawer_subject().map(str::to_owned)
    }
}

impl ThreadPanel {
    fn drawer_host(&self) -> Host {
        Host {
            session_id: self.session_id.clone(),
            chat: self.chat.clone(),
            focus: self.focus_handle.clone(),
            covered: self.zoom.beneath,
            conversation: self.conversation.clone(),
        }
    }

    /// The Diff, Plan, Agents and Pull Requests tabs of the pane's drawer,
    /// made now if they never showed: the compact layout's panel page shows
    /// the same entity, as a page.
    pub(crate) fn drawer_tabs(&mut self, cx: &mut Context<Self>) -> Entity<DiffPanel> {
        let host = self.drawer_host();
        self.drawer
            .update(cx, |drawer, cx| drawer.tabs_for(&host, cx))
    }

    /// The Preview tab of the pane's drawer, made now if it never showed.
    pub(crate) fn drawer_preview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<PreviewPanel> {
        let host = self.drawer_host();
        self.drawer
            .update(cx, |drawer, cx| drawer.preview_for(&host, window, cx))
    }

    /// The compact layout's panel page shows the Preview tab, or no longer.
    pub(crate) fn select_compact_preview(
        &mut self,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !selected && self.preview.borrow().is_none() {
            return;
        }
        self.drawer_preview(window, cx).update(cx, |preview, cx| {
            preview.set_compact_panel_selected(selected, cx)
        });
    }

    /// Let go of the page the pane's Preview tab shows, and of its chrome.
    pub(crate) fn release_preview(&mut self, cx: &mut Context<Self>) {
        let preview = self.preview.borrow().clone();
        if let Some(preview) = preview {
            preview.update(cx, |preview, cx| preview.release_preview(cx));
        }
    }

    /// Hide the pane's page before the layout it sits in goes: a native view
    /// outlives its element.
    pub(crate) fn sync_preview(this: &Entity<Self>, cx: &mut App) {
        let preview = this.read(cx).preview.borrow().clone();
        if let Some(preview) = preview {
            preview.update(cx, |preview, cx| preview.sync_visibility(cx));
        }
    }

    /// Another pane is drawn over this one, or no longer.
    pub(super) fn set_preview_covered(&mut self, covered: bool, cx: &mut Context<Self>) {
        let preview = self.preview.borrow().clone();
        if let Some(preview) = preview {
            PreviewPanel::set_covered(&preview, covered, cx);
        }
    }

    /// Where the pane's conversation is drawn: the pane less the panel's room.
    pub(super) fn conversation_bounds(&self) -> Bounds<Pixels> {
        self.conversation.get()
    }
}

impl Drawer {
    /// The Diff, Plan, Agents and Pull Requests tabs, made now if they never
    /// showed.
    fn tabs_for(&mut self, host: &Host, cx: &mut Context<Self>) -> Entity<DiffPanel> {
        let subject = host.subject(cx);
        let lead = host
            .session_id
            .clone()
            .filter(|_| host.session_id != subject);
        let at_corner = host.chat.read(cx).at_window_corner();
        let tabs = self.tabs.get_or_insert_with(|| {
            let (store, window_state) = (self.store.clone(), self.window_state.clone());
            let scene = self.scene.clone();
            cx.new(|cx| {
                let mut tabs = DiffPanel::new(store, window_state, subject.clone(), cx);
                tabs.share_scene(&scene, cx);
                tabs
            })
        });
        let tabs = tabs.clone();
        DiffPanel::set_session(&tabs, subject, cx);
        DiffPanel::set_lead(&tabs, lead, cx);
        DiffPanel::set_at_window_corner(&tabs, at_corner, cx);
        tabs
    }

    /// The Preview tab, made now if it never showed.
    fn preview_for(
        &mut self,
        host: &Host,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<PreviewPanel> {
        let subject = host.subject(cx);
        let at_corner = host.chat.read(cx).at_window_corner();
        let existing = self.preview.borrow().clone();
        let preview = match existing {
            Some(preview) => preview,
            None => {
                let preview = self.browser.clone().update(cx, |browser, cx| {
                    browser.for_pane(subject.clone(), window, cx)
                });
                preview.update(cx, |preview, _| preview.share_scene(&self.scene));
                *self.preview.borrow_mut() = Some(preview.clone());
                preview
            }
        };
        PreviewPanel::set_session(&preview, subject, cx);
        PreviewPanel::set_at_window_corner(&preview, at_corner, cx);
        PreviewPanel::set_covered(&preview, host.covered, cx);
        preview
    }
    /// This frame of a pane `pane_width` wide: `conversation`, and the panel
    /// while it is drawn. One group whether or not the panel is open, so the
    /// conversation keeps its place in the tree as the panel comes and goes;
    /// and the panel always in the group's last slot, whatever it is doing,
    /// so what it holds keeps its place as it goes from docked to covering.
    fn render_split(
        &mut self,
        host: &Host,
        conversation: impl IntoElement,
        pane_width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let new_scene = self.scene.show(host.session_id.as_deref());
        let subject = host.subject(cx);
        // An open panel changing thread within the pane's own shows the new
        // one at once, content at rest; a shut one slides in on it.
        if std::mem::replace(&mut self.subject, subject.clone()) != subject
            && self.open
            && !new_scene
        {
            self.scene.restart();
        }
        if let Some(tabs) = &self.tabs {
            DiffPanel::set_session(tabs, subject.clone(), cx);
        }
        if self.preview.borrow().is_some() {
            self.preview_for(host, window, cx);
        }
        let handed = (host.chat.entity_id(), pane_width);
        if self.handed_width.replace(handed) != Some(handed) {
            ChatView::set_pane_width(&host.chat, pane_width, cx);
        }
        let store = self.store.clone();
        let panel = store.read(cx).panel_state_for(subject.as_deref());
        let panel_open = panel.right_panel_open;
        let right_tab = panel.right_tab;
        let expanded = panel.right_panel_expanded;

        let chosen = store
            .read(cx)
            .right_panel_width()
            .map_or(px(RIGHT_PANEL_WIDTH), |width| px(width as f32));
        let rest = rest_width(chosen, pane_width);
        let covering = rest == Rest::Cover;
        let motion = self.motion.clone();
        let slide = motion.transition(
            "right-panel-in",
            if panel_open { 1f32 } else { 0. },
            GLIDE,
            window,
            cx,
        );
        let cover = motion.transition(
            "right-panel-cover",
            if covering { 1f32 } else { 0. },
            GLIDE,
            window,
            cx,
        );
        let expand = motion.transition(
            "right-panel-expand",
            if expanded { 1f32 } else { 0. },
            GLIDE,
            window,
            cx,
        );
        // The room the conversation gives up, and the panel's own width: both
        // start from the panel's docked width, so a pane crossing into cover
        // moves neither at once. Expanded, the panel outgrows its room and
        // the conversation stays as it is beneath.
        let docked = beside_width(chosen, pane_width);
        let room = docked * (1. - cover);
        let lift = cover.max(expand);
        let panel_width = docked + (pane_width - docked) * lift;

        let glide = self.glide.sample(
            &[Panel::flex(true), Panel::sized(room, panel_open)],
            true,
            &self.split,
            window,
            cx,
        );
        let drawn = panel_open || glide.mounted(1) || (cover > 0. && slide > 0.);
        let leaving = !panel_open && drawn;

        // The keyboard stays where it was as a docked panel opens. Over the
        // pane the panel is what the reader sees, so it takes the keyboard,
        // and either way closing it from inside gives the keyboard back.
        // A pane narrowing into cover hands the panel the keyboard held in
        // what it covers, so nothing is typed into what cannot be seen, and
        // widening hands it back, unless the reader went on inside the panel.
        // Another thread in the pane shows its own panel as that thread left
        // it, keyboard and all.
        let was_open = std::mem::replace(&mut self.open, panel_open);
        let was_covering = std::mem::replace(&mut self.covering, covering);
        if new_scene {
            self.return_to = None;
        } else if panel_open && !was_open {
            if !self.focus.contains_focused(window, cx) {
                self.return_to = window.focused(cx);
            }
            if covering {
                self.focus.focus(window, cx);
            }
        } else if !panel_open
            && was_open
            && let Some(previous) = self.return_to.take()
            && self.focus.contains_focused(window, cx)
        {
            previous.focus(window, cx);
        } else if panel_open && covering && !was_covering {
            if host.focus.contains_focused(window, cx) && !self.focus.contains_focused(window, cx) {
                self.return_to = window.focused(cx);
                self.focus.focus(window, cx);
            }
        } else if panel_open
            && !covering
            && was_covering
            && self.focus.is_focused(window)
            && let Some(previous) = self.return_to.take()
        {
            previous.focus(window, cx);
        }
        // Once the panel has gone, its content is taken down and what it
        // built to draw is let go. Another thread in the pane keeps its own
        // panel as it left it, page included.
        let was_drawn = std::mem::replace(&mut self.drawn, drawn);
        if !drawn && host.session_id != subject {
            host.chat.update(cx, |chat, _| chat.release_drawer_agent());
        }
        if was_drawn && !drawn {
            if let Some(tabs) = &self.tabs {
                tabs.update(cx, |tabs, cx| tabs.release(cx));
            }
            if !new_scene {
                self.scene.restart();
                // A frame in the new scene, so the panel's next opening
                // glides from where this one left it.
                window.request_animation_frame();
                let preview = self.preview.borrow().clone();
                if let Some(preview) = preview {
                    preview.update(cx, |preview, cx| preview.release_page(cx));
                }
            }
        }

        // The state keeps the panel's size for a drag to start from. A panel
        // new to the group has its size but not yet the conversation's; a
        // room that changes is held at that size this frame, and the state
        // takes it once the frame has laid both out.
        let moved = drawn && self.laid_out.replace(room) != Some(room);
        if !drawn {
            self.laid_out = None;
        }
        if moved && glide.size(1).is_none() {
            let split = self.split.clone();
            window.defer(cx, move |window, cx| {
                split.update(cx, |state, cx| state.resize_panel(1, room, window, cx));
            });
        }

        let panel = drawn.then(|| {
            // How far the panel is past the pane's right edge. Docked, its
            // left edge is the seam, wherever the glide has it; over the
            // pane it slides on its own.
            let open_docked = match glide.size(1) {
                Some(size) => size / docked.max(px(1.)),
                None if panel_open => 1.,
                None => 0.,
            };
            let docked_away = panel_width * (1. - open_docked);
            let covering_away = panel_width * (1. - slide);
            let away = docked_away + (covering_away - docked_away) * cover;
            let panel = self.render_panel(
                host,
                PanelFrame {
                    tab: right_tab,
                    covers: expanded || covering,
                    lift,
                    leaving,
                    subject,
                },
                window,
                cx,
            );
            div()
                .absolute()
                .top_0()
                .right(px(0.) - away)
                .h_full()
                .w(panel_width)
                .child(panel)
        });
        let slot = panel.map(|panel| {
            let at_rest = if moved || cover > 0. {
                room..room
            } else {
                let least = px(RIGHT_PANEL_MIN_WIDTH);
                least..(pane_width - px(CONVERSATION_MIN_WIDTH)).max(least)
            };
            // `fixed`: the panel keeps its width as the pane's changes, the
            // conversation taking the difference.
            resizable_panel()
                .flex_none()
                .fixed()
                .size(room)
                .size_range(glide.range(1, at_rest))
                .child(glide.hold(1, div()))
                .child(panel)
        });
        // The panel's edge is its group's handle; expanded or covering, it
        // has no room to be dragged in, and gpui-base has no API to disable
        // one handle, so it is covered.
        let edge_cover = (drawn && (expanded || expand > 0. || cover > 0.)).then(|| {
            div()
                .absolute()
                .top_0()
                .left(pane_width - glide.size(1).unwrap_or(room) - px(5.))
                .w(px(10.))
                .h_full()
                .occlude()
        });
        let conversation_bounds = host.conversation.clone();
        let conversation = crate::material::left_of_pages(
            div()
                .debug_selector(|| "pane-conversation".into())
                .size_full()
                .on_prepaint(move |painted, _, _| conversation_bounds.set(painted))
                .child(conversation),
        );
        let chat_min = (pane_width - room).min(px(CONVERSATION_MIN_WIDTH));
        let group = h_resizable("right-panel-split")
            .with_state(&self.split)
            .on_resize(move |state, _, cx| {
                if !expanded
                    && !covering
                    && let Some(width) = state.read(cx).sizes().get(1)
                {
                    let width = f32::from(*width).round() as u32;
                    store.update(cx, |store, cx| store.set_right_panel_width(width, cx));
                }
            })
            .child(
                resizable_panel()
                    .size_range(glide.range(0, chat_min..Pixels::MAX))
                    .child(glide.hold(0, conversation)),
            )
            // Not `children`: the group's replaces the panels added before.
            .when_some(slot, |group, slot| group.child(slot));
        div()
            .relative()
            .size_full()
            .child(group)
            .children(edge_cover)
            .into_any_element()
    }

    /// The panel itself.
    fn render_panel(
        &mut self,
        host: &Host,
        frame: PanelFrame,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Which entity fills the panel: the Preview tab shows the embedded
        // browser; the other tabs share the DiffPanel container. Each keeps
        // its own layer, so the one left fades out under the one chosen; a
        // page leaving the Preview fades natively as it is let go.
        let fade = self
            .motion
            .layers(
                "right-panel-body",
                usize::from(frame.tab == RightTab::Preview),
                2,
                CHANGE,
                window,
                cx,
            )
            .fill();
        let mut layers: Vec<(usize, AnyElement)> = Vec::new();
        if fade.shows(0) {
            layers.push((0, self.tabs_for(host, cx).into_any_element()));
        }
        if fade.shows(1) {
            layers.push((1, self.preview_for(host, window, cx).into_any_element()));
        }
        // Under the scene's id, so what the tabs key in element state starts
        // over with it.
        let content = div()
            .id(self.scene.id("right-panel-scene"))
            .size_full()
            .child(fade.render(layers).size_full());
        let closing = self.store.downgrade();
        let subject = frame.subject;
        crate::material::lifting_panel(
            div().id("right-panel"),
            crate::material::Side::Right,
            frame.lift,
            cx,
        )
        .debug_selector(|| "right-panel".into())
        .size_full()
        .role(gpui::Role::Complementary)
        .tab_group()
        .track_focus(&self.focus)
        .when(frame.covers, |panel| panel.key_context(RIGHT_PANEL_CONTEXT))
        .on_action(move |_: &CloseRightPanel, _, cx| {
            _ = closing.update(cx, |store, cx| {
                store.close_right_panel_for(subject.as_deref(), cx)
            });
        })
        .occlude()
        .child(content)
        .when(frame.leaving, |panel| panel.child(crate::motion::shield()))
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chosen width while the pane holds it beside the conversation,
    /// yielding down to the panel's narrowest; below that the panel covers
    /// the pane, the conversation never going under a pane's own narrowest.
    #[test]
    fn a_panel_rests_beside_the_conversation_until_there_is_no_room_for_both() {
        let least = px(RIGHT_PANEL_MIN_WIDTH + CONVERSATION_MIN_WIDTH);
        assert_eq!(rest_width(px(560.), px(1200.)), Rest::Beside(px(560.)));
        assert_eq!(
            rest_width(px(560.), least + px(100.)),
            Rest::Beside(px(RIGHT_PANEL_MIN_WIDTH) + px(100.))
        );
        assert_eq!(
            rest_width(px(10_000.), px(1200.)),
            Rest::Beside(px(1200.) - px(CONVERSATION_MIN_WIDTH))
        );
        assert_eq!(
            rest_width(px(10.), px(1200.)),
            Rest::Beside(px(RIGHT_PANEL_MIN_WIDTH))
        );
        assert_eq!(
            rest_width(px(560.), least),
            Rest::Beside(px(RIGHT_PANEL_MIN_WIDTH))
        );
        assert_eq!(rest_width(px(560.), least - px(1.)), Rest::Cover);
        assert_eq!(rest_width(px(560.), px(400.)), Rest::Cover);
    }
}
