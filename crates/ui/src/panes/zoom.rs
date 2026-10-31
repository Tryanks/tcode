//! A pane zoomed over the chat column.

use gpui::{
    AnyElement, App, Bounds, ElementId, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Styled as _, Window, div,
    point, px,
};
use gpui_base::FocusTrapElement as _;
use gpui_base::motion::Interpolate as _;

use super::ThreadPanel;

/// The pane zoomed over the whole chat column: it grows out of its slot until
/// it covers the column over `edge_back::SETTLE`, and shrinks back into its
/// slot as the zoom ends. The other panes stay where they are, drawn beneath
/// it and shielded, so they take no pointer and no drop; the dock's tree and
/// its shares never change for it.
///
/// gpui-base's zoom (`DockArea::set_zoomed_in`) draws the zoomed group alone
/// in the area's frame in place of the tree: it cannot keep the other panes
/// drawn beneath it, nor draw the group anywhere between its slot and the
/// frame. So the shell draws the pane over the dock itself, and the dock is
/// never zoomed.
///
/// The pane is drawn over the column from the frame the zoom starts until the
/// frame it is back in its slot, and laid out at the column's size all that
/// time, clipped to where it has grown, so it reflows once on the way in and
/// once on the way out. Its slot keeps a placeholder of the slot's size
/// meanwhile (`ThreadTabs::render_active_panel`). Being drawn under another
/// parent, the pane's own keyed state starts over in each of those two frames
/// (see `ThreadPanes`); the panes beneath it and its slot keep theirs.
///
/// The panes beneath stay laid out with their tab stops, so the pane is a
/// focus trap while it is drawn over them: Tab cycles inside it.
#[derive(Default)]
pub(crate) struct Zoom {
    current: Option<Zoomed>,
    generation: u64,
}

struct Zoomed {
    pane: Entity<ThreadPanel>,
    /// Zoomed in, or on its way in; otherwise on its way back to its slot.
    zoomed: bool,
    generation: u64,
    /// How far it has grown out of its slot, as last sampled.
    progress: f32,
    /// The focus trap it is drawn in.
    trap: FocusHandle,
}

/// How a zoom of the column leaves one pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PaneZoom {
    /// A pane of the column is zoomed in, so the column is that pane alone.
    pub(crate) zoomed: bool,
    /// The pane is drawn over the column, its slot holding a placeholder.
    pub(crate) overlaid: bool,
}

impl Zoom {
    /// The pane drawn over the column: zooming in, zoomed, or on its way
    /// back into its slot.
    pub(crate) fn pane(&self) -> Option<&Entity<ThreadPanel>> {
        self.current.as_ref().map(|zoomed| &zoomed.pane)
    }

    /// Whether a pane is zoomed in, or on its way in.
    pub(crate) fn is_zoomed(&self) -> bool {
        self.current.as_ref().is_some_and(|zoomed| zoomed.zoomed)
    }

    /// How a zoom leaves `pane` now.
    pub(crate) fn of(&self, pane: &Entity<ThreadPanel>) -> PaneZoom {
        PaneZoom {
            zoomed: self.is_zoomed(),
            overlaid: self.pane() == Some(pane),
        }
    }

    /// Zoom `pane` in from its slot, taking any other pane drawn over the
    /// column back into its slot at once.
    pub(crate) fn zoom_in(&mut self, pane: Entity<ThreadPanel>, cx: &App) {
        self.generation += 1;
        self.current = Some(Zoomed {
            pane,
            zoomed: true,
            generation: self.generation,
            progress: 0.,
            trap: cx.focus_handle(),
        });
    }

    /// Turn the zoom the other way from where it is.
    pub(crate) fn toggle(&mut self) {
        if let Some(zoomed) = &mut self.current {
            zoomed.zoomed = !zoomed.zoomed;
        }
    }

    /// Take the zoomed pane back into its slot.
    pub(crate) fn zoom_out(&mut self) {
        if let Some(zoomed) = &mut self.current {
            zoomed.zoomed = false;
        }
    }

    /// Put the pane back in its slot now.
    pub(crate) fn land(&mut self) {
        self.current = None;
    }

    /// This frame of the zoom. Once the pane is back in its slot nothing is
    /// drawn over the column any more.
    pub(crate) fn sample(&mut self, window: &mut Window, cx: &mut App) {
        let Some(zoomed) = &mut self.current else {
            return;
        };
        let sample = crate::motion::appear(
            ElementId::NamedInteger("pane-zoom".into(), zoomed.generation),
            zoomed.zoomed,
            window,
            cx,
        );
        if sample.should_render() {
            zoomed.progress = sample.progress;
        } else {
            // Back in its slot the pane's keyed state starts over, a focused
            // button's handle with it, so a keyboard inside goes to the pane.
            if zoomed.trap.contains_focused(window, cx) {
                window.focus(&zoomed.pane.focus_handle(cx), cx);
            }
            self.current = None;
        }
    }

    /// What is drawn over the column painted at `area`: a shield over the
    /// panes beneath, and the zoomed pane at its progress from the slot it was
    /// last laid out in to the whole column, laid out at the column's size and
    /// place, so the growing pane uncovers its content rather than moving it.
    pub(crate) fn render(&self, area: Bounds<Pixels>, cx: &App) -> Option<AnyElement> {
        let zoomed = self.current.as_ref()?;
        let slot = zoomed.pane.read(cx).slot();
        let from = Bounds::new(slot.origin - area.origin, slot.size);
        let to = Bounds::new(point(px(0.), px(0.)), area.size);
        let at = from.interpolate(&to, zoomed.progress);
        Some(
            div()
                .absolute()
                .inset_0()
                .child(crate::motion::shield())
                .child(
                    div()
                        .debug_selector(|| "zoomed-pane".into())
                        .absolute()
                        .left(at.origin.x)
                        .top(at.origin.y)
                        .w(at.size.width)
                        .h(at.size.height)
                        .overflow_hidden()
                        // The column's paper lets the canvas through; over
                        // other panes it would let them through, so it is
                        // laid on the opaque canvas here.
                        .bg(crate::material::opaque_canvas(cx)
                            .blend(crate::material::content_surface(cx)))
                        .shadow_sm()
                        // The content stays where the column puts it while
                        // the pane grows over it or shrinks off it.
                        .child(
                            div()
                                .absolute()
                                .left(-at.origin.x)
                                .top(-at.origin.y)
                                .w(area.size.width)
                                .h(area.size.height)
                                .child(zoomed.pane.clone()),
                        )
                        .focus_trap("pane-zoom", &zoomed.trap),
                )
                .into_any_element(),
        )
    }
}
