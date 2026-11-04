//! A pane's right panel: a drawer over the pane's chat, which keeps its whole
//! column beneath. It slides in from the pane's right edge as it opens and
//! back out as it closes, drawn as it was while it goes, and never leaves the
//! pane: other panes each have their own.
//!
//! Whether it is open, its tab and whether it is expanded are the
//! conversation's ([`ConversationUiState`]), so a pane shows the drawer exactly
//! when its thread has it open, and the drawer stays with the thread when the
//! thread changes panes. Its width is one preference of the user's, which
//! each pane clamps to itself. Another thread in the pane
//! is a new scene: the drawer lands in that thread's state at once, open or
//! shut, never as if the reader had opened or closed it.
//!
//! gpui-base's PopoverState and Sheet are modal: a popover registers a
//! deferred popup, which suspends the text fields' context menus and the
//! chat's scrolling capture window-wide, and a sheet traps focus. The drawer
//! leaves the chat beside it working, so it takes and returns the keyboard
//! itself.
//!
//! [`ConversationUiState`]: crate::conversation_ui::ConversationUiState

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, StatefulInteractiveElement as _, Styled as _, Window,
    actions, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{ResizableState, h_resizable, resizable_panel};
use tcode_core::ui::RightTab;

use super::ThreadPanel;
use crate::diff::DiffPanel;
use crate::preview_panel::PreviewPanel;
use crate::store::WorkspaceStore;
use crate::window_state::WindowState;

actions!(tcode_shell, [CloseRightPanel]);

/// The key context of a drawer, where Escape closes it.
pub(crate) const RIGHT_PANEL_CONTEXT: &str = "RightPanel";

/// How long the drawer takes to slide, and to grow when expanded.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long one tab takes to fade into the other's layer.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// The drawer's width until the user drags its edge.
const RIGHT_PANEL_WIDTH: f32 = 560.;

/// The drawer's narrowest: the icon tabs and the panel's own controls fit in
/// a panel's narrowest content, beside the window's caption buttons where the
/// panel's strip holds them.
const RIGHT_PANEL_MIN_WIDTH: f32 =
    crate::material::PANEL_CONTENT_MIN_WIDTH + crate::window_caption::RIGHT_PANEL_CAPTION_WIDTH;

/// The narrowest strip of chat worth leaving beside the drawer.
const MIN_CHAT_STRIP: f32 = 240.;

/// The width a drawer the user chose `chosen` for rests at in a pane `pane`
/// wide: all of it where what would be left beside it is no use.
fn rest_width(chosen: Pixels, pane: Pixels) -> Pixels {
    let chosen = chosen.max(px(RIGHT_PANEL_MIN_WIDTH)).min(pane);
    if pane - chosen < px(MIN_CHAT_STRIP) {
        pane
    } else {
        chosen
    }
}

/// What one pane keeps of its drawer. Its tabs are made as it first opens
/// and stay for the pane's life, what they built let go once it has shut.
pub(crate) struct Drawer {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// The attachment's panel, which owns the pages this pane's shows.
    browser: Entity<PreviewPanel>,
    tabs: Option<Entity<DiffPanel>>,
    preview: Option<Entity<PreviewPanel>>,
    /// The thread the pane shows: another one, or the drawer taking its
    /// content down, starts a new scene, so what is put up starts at rest.
    scene: crate::motion::Epoch,
    /// Kept here rather than in element state: the dock draws the pane under
    /// a new parent whenever the layout around it changes.
    motion: crate::motion::owned::Owned,
    /// The drawer's group (room for the chat · the panel), whose handle is
    /// the panel's left edge.
    split: Entity<ResizableState>,
    /// Keep giving the panel its width until its group reports it, then stop
    /// so the restore never fights a drag. Armed while the panel is not
    /// drawn, while it is expanded, and whenever its width should change.
    restore_pending: bool,
    /// The thread the drawer showed last frame.
    subject: Option<String>,
    /// The pane's width and the panel's rest width last frame.
    laid_out: (Pixels, Pixels),
    /// Where the panel's content holds the keyboard.
    focus: FocusHandle,
    /// What had the keyboard when the reader opened the panel, given back
    /// when they close it from inside.
    return_to: Option<FocusHandle>,
    /// Whether the panel was open, and whether it was drawn (open or sliding
    /// out), last frame.
    open: bool,
    drawn: bool,
}

impl Drawer {
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
            store,
            window_state,
            browser,
            tabs: None,
            preview: None,
            motion: crate::motion::owned::Owned::in_scene(&scene),
            scene,
            split: cx.new(|_| ResizableState::default()),
            restore_pending: true,
            subject: None,
            laid_out: Default::default(),
            focus: cx.focus_handle(),
            return_to: None,
            open: false,
            drawn: false,
        }
    }
}

impl ThreadPanel {
    /// The thread the pane's drawer shows.
    fn drawer_subject(&self, cx: &App) -> Option<String> {
        self.session_id.as_ref()?;
        self.chat.read(cx).drawer_subject().map(str::to_owned)
    }

    /// The Diff, Plan, Agents and Pull Requests tabs of the pane's drawer,
    /// made now if they never showed: the compact layout's panel page shows
    /// the same entity, as a page.
    /// Whether the keyboard is in the pane's conversation: its composer, the
    /// card of the agent beside it, or its drawer.
    pub(crate) fn holds_input(&self, window: &Window, cx: &App) -> bool {
        let chat = self.chat.read(cx);
        chat.composer_has_keyboard(window, cx)
            || chat.side_composer_has_keyboard(window, cx)
            || self.drawer.focus.contains_focused(window, cx)
    }

    pub(crate) fn drawer_tabs(&mut self, cx: &mut Context<Self>) -> Entity<DiffPanel> {
        let subject = self.drawer_subject(cx);
        let lead = self
            .session_id
            .clone()
            .filter(|_| self.session_id != subject);
        let at_corner = self.chat.read(cx).at_window_corner();
        let tabs = self.drawer.tabs.get_or_insert_with(|| {
            let (store, window_state) =
                (self.drawer.store.clone(), self.drawer.window_state.clone());
            let scene = self.drawer.scene.clone();
            cx.new(|cx| {
                let mut tabs = DiffPanel::new(store, window_state, subject.clone(), cx);
                tabs.share_scene(&scene, cx);
                tabs
            })
        });
        let tabs = tabs.clone();
        tabs.update(cx, |tabs, cx| {
            tabs.set_session(subject, cx);
            tabs.set_lead(lead, cx);
            tabs.set_at_window_corner(at_corner, cx);
        });
        tabs
    }

    /// The Preview tab of the pane's drawer, made now if it never showed.
    pub(crate) fn drawer_preview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<PreviewPanel> {
        let subject = self.drawer_subject(cx);
        let at_corner = self.chat.read(cx).at_window_corner();
        let covered = self.zoom.beneath;
        let preview = match &self.drawer.preview {
            Some(preview) => preview.clone(),
            None => {
                let preview = self.drawer.browser.clone().update(cx, |browser, cx| {
                    browser.for_pane(subject.clone(), window, cx)
                });
                preview.update(cx, |preview, _| preview.share_scene(&self.drawer.scene));
                self.drawer.preview = Some(preview.clone());
                preview
            }
        };
        preview.update(cx, |preview, cx| {
            preview.set_session(subject, cx);
            preview.set_at_window_corner(at_corner, cx);
            preview.set_covered(covered, cx);
        });
        preview
    }

    /// The compact layout's panel page shows the Preview tab, or no longer.
    pub(crate) fn select_compact_preview(
        &mut self,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !selected && self.drawer.preview.is_none() {
            return;
        }
        self.drawer_preview(window, cx).update(cx, |preview, cx| {
            preview.set_compact_panel_selected(selected, cx)
        });
    }

    /// Let go of the page the pane's Preview tab shows, and of its chrome.
    pub(crate) fn release_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(preview) = &self.drawer.preview {
            preview.update(cx, |preview, cx| preview.release_preview(cx));
        }
    }

    /// Hide the pane's page before the layout it sits in goes: a native view
    /// outlives its element.
    pub(crate) fn sync_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(preview) = &self.drawer.preview {
            preview.update(cx, |preview, cx| preview.sync_visibility(cx));
        }
    }

    /// Another pane is drawn over this one, or no longer.
    pub(super) fn set_preview_covered(&mut self, covered: bool, cx: &mut Context<Self>) {
        if let Some(preview) = &self.drawer.preview {
            preview.update(cx, |preview, cx| preview.set_covered(covered, cx));
        }
    }

    /// This frame of the drawer: none while it is shut.
    pub(super) fn render_drawer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let new_scene = self.drawer.scene.show(self.session_id.as_deref());
        let subject = self.drawer_subject(cx);
        // An open drawer changing thread within the pane's own shows the new
        // one at once, content at rest; a shut one slides in on it.
        if std::mem::replace(&mut self.drawer.subject, subject.clone()) != subject
            && self.drawer.open
            && !new_scene
        {
            self.drawer.scene.restart();
        }
        if let Some(tabs) = &self.drawer.tabs {
            let subject = subject.clone();
            tabs.update(cx, |tabs, cx| tabs.set_session(subject, cx));
        }
        if self.drawer.preview.is_some() {
            self.drawer_preview(window, cx);
        }
        let store = self.drawer.store.clone();
        let panel = store.read(cx).panel_state_for(subject.as_deref());
        let panel_open = panel.right_panel_open;
        let right_tab = panel.right_tab;
        // "Expanded" (the whole pane) is a diff-only affordance; the preview
        // tab keeps the panel's width so the webview keeps a stable size.
        let expanded = panel.right_panel_expanded && right_tab != RightTab::Preview;
        let pane_width = self.bounds.get().size.width;
        // A pane is laid out after its first render; what it draws over
        // needs that frame's width.
        if panel_open && pane_width == px(0.) {
            window.request_animation_frame();
        }

        let motion = self.drawer.motion.clone();
        let drawer_in = motion.transition(
            "right-panel-in",
            if panel_open { 1f32 } else { 0. },
            GLIDE,
            window,
            cx,
        );
        let drawn = panel_open || drawer_in > 0.;
        let leaving = !panel_open && drawn;
        let expand = motion.transition(
            "right-panel-expand",
            if expanded { 1f32 } else { 0. },
            GLIDE,
            window,
            cx,
        );
        let chosen = store
            .read(cx)
            .right_panel_width()
            .map_or(px(RIGHT_PANEL_WIDTH), |width| px(width as f32));
        let rest = rest_width(chosen, pane_width);
        let whole_pane = rest == pane_width;
        let drawer_width = rest + (pane_width - rest) * expand;
        let moved =
            std::mem::replace(&mut self.drawer.laid_out, (pane_width, rest)) != (pane_width, rest);
        if !drawn || moved || expand > 0. {
            self.drawer.restore_pending = true;
        }
        if drawn && expand == 0. && self.drawer.restore_pending {
            let restored = self
                .drawer
                .split
                .update(cx, |state, cx| match state.sizes().get(1) {
                    Some(width) if (*width - rest).abs() > px(0.5) => {
                        state.resize_panel(1, rest, window, cx);
                        false
                    }
                    Some(_) => true,
                    None => false,
                });
            // A match on the frame the width changed is the old layout, not
            // a settled one.
            if restored && !moved {
                self.drawer.restore_pending = false;
            }
        }
        // Held at the width it is being given until the group has it, so no
        // frame draws it at the group's minimum or rescaled by the pane.
        let pinned = self.drawer.restore_pending || whole_pane;

        // The reader opening the panel hands it the keyboard, and closing it
        // from inside gives the keyboard back. Another thread in the pane
        // shows its own panel as that thread left it, keyboard and all.
        let was_open = std::mem::replace(&mut self.drawer.open, panel_open);
        if new_scene {
            self.drawer.return_to = None;
        } else if panel_open && !was_open {
            if !self.drawer.focus.contains_focused(window, cx) {
                self.drawer.return_to = window.focused(cx);
            }
            self.drawer.focus.focus(window, cx);
        } else if !panel_open
            && was_open
            && let Some(previous) = self.drawer.return_to.take()
            && self.drawer.focus.contains_focused(window, cx)
        {
            previous.focus(window, cx);
        }
        // Once the panel has slid out, its content is taken down and what it
        // built to draw is let go. Another thread in the pane keeps its own
        // panel as it left it, page included.
        let was_drawn = std::mem::replace(&mut self.drawer.drawn, drawn);
        if !drawn && self.session_id != subject {
            self.chat.update(cx, |chat, _| chat.release_drawer_agent());
        }
        if was_drawn && !drawn {
            if let Some(tabs) = &self.drawer.tabs {
                tabs.update(cx, |tabs, cx| tabs.release(cx));
            }
            if !new_scene {
                self.drawer.scene.restart();
                if let Some(preview) = &self.drawer.preview {
                    preview.update(cx, |preview, cx| preview.release_page(cx));
                }
            }
        }
        if !drawn {
            return None;
        }

        // Which entity fills the panel: the Preview tab shows the embedded
        // browser; the other tabs share the DiffPanel container. Each keeps
        // its own layer, so the one left fades out under the one chosen; a
        // page leaving the Preview fades natively as it is let go.
        let fade = motion
            .layers(
                "right-panel-body",
                usize::from(right_tab == RightTab::Preview),
                2,
                CHANGE,
                window,
                cx,
            )
            .fill();
        let mut layers: Vec<(usize, AnyElement)> = Vec::new();
        if fade.shows(0) {
            layers.push((0, self.drawer_tabs(cx).into_any_element()));
        }
        if fade.shows(1) {
            layers.push((1, self.drawer_preview(window, cx).into_any_element()));
        }
        // Under the scene's id, so what the tabs key in element state starts
        // over with it.
        let content = div()
            .id(self.drawer.scene.id("right-panel-scene"))
            .size_full()
            .child(fade.render(layers).size_full());

        let away = drawer_width * (1. - drawer_in);
        // The panel's edge is its group's handle; spanning the pane, the
        // panel has no room to be dragged in, and gpui-base has no API to
        // disable one handle, so it is covered.
        let edge_cover = (expanded || expand > 0. || whole_pane).then(|| {
            div()
                .absolute()
                .top_0()
                .left(pane_width - drawer_width - px(5.))
                .w(px(10.))
                .h_full()
                .occlude()
        });
        let remembered = store.clone();
        let closing = store.downgrade();
        let closes = subject;
        Some(
            div()
                .absolute()
                .inset_0()
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .right(px(0.) - away)
                        .h_full()
                        .w(pane_width)
                        .child(
                            h_resizable("right-panel-drawer")
                                .with_state(&self.drawer.split)
                                .on_resize(move |state, _, cx| {
                                    if !expanded
                                        && !whole_pane
                                        && let Some(width) = state.read(cx).sizes().get(1)
                                    {
                                        let width = f32::from(*width).round() as u32;
                                        remembered.update(cx, |store, cx| {
                                            store.set_right_panel_width(width, cx)
                                        });
                                    }
                                })
                                .child(resizable_panel().size_range(px(0.)..Pixels::MAX))
                                .child(
                                    resizable_panel()
                                        .flex_none()
                                        .size(rest)
                                        .size_range(if pinned {
                                            drawer_width..drawer_width
                                        } else {
                                            px(RIGHT_PANEL_MIN_WIDTH).min(pane_width)
                                                ..(pane_width - px(MIN_CHAT_STRIP))
                                        })
                                        .child(
                                            crate::material::floating_panel(
                                                div().id("right-panel"),
                                                crate::material::Side::Right,
                                                cx,
                                            )
                                            .debug_selector(|| "right-panel".into())
                                            .size_full()
                                            .role(gpui::Role::Complementary)
                                            .tab_group()
                                            .track_focus(&self.drawer.focus)
                                            .key_context(RIGHT_PANEL_CONTEXT)
                                            .on_action(move |_: &CloseRightPanel, _, cx| {
                                                _ = closing.update(cx, |store, cx| {
                                                    store.close_right_panel_for(
                                                        closes.as_deref(),
                                                        cx,
                                                    )
                                                });
                                            })
                                            .occlude()
                                            .child(content)
                                            .when(leaving, |panel| {
                                                panel.child(crate::motion::shield())
                                            }),
                                        ),
                                ),
                        )
                        .children(edge_cover),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chosen width while the pane leaves the chat room beside it, all
    /// of the pane once it would not.
    #[test]
    fn the_drawer_takes_the_pane_when_no_usable_strip_is_left() {
        assert_eq!(rest_width(px(560.), px(1200.)), px(560.));
        assert_eq!(rest_width(px(560.), px(800.)), px(560.));
        assert_eq!(rest_width(px(560.), px(799.)), px(799.));
        assert_eq!(rest_width(px(560.), px(400.)), px(400.));
        assert_eq!(rest_width(px(10_000.), px(1200.)), px(1200.));
    }
}
