//! A gpui-base resizable group's panels gliding to the sizes a change of
//! which panels it shows gives them.

use std::{cell::Cell, ops::Range, rc::Rc};

use gpui::{
    AnyElement, App, Axis, ElementId, Entity, IntoElement, ParentElement as _, Pixels, Size,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{
    ElementExt as _, ResizableState,
    motion::{Presence, PresencePhase},
};

/// How long a resized surface takes to glide to its new size.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// What one panel of a group is once the layout rests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Panel {
    /// `None` for a panel that takes what the others leave.
    size: Option<Pixels>,
    shown: bool,
    kept: bool,
}

impl Panel {
    /// A panel the group holds at `size`.
    pub(crate) fn sized(size: Pixels, shown: bool) -> Self {
        Self {
            size: Some(size),
            shown,
            kept: false,
        }
    }

    /// A panel that takes what the others leave.
    pub(crate) fn flex(shown: bool) -> Self {
        Self {
            size: None,
            shown,
            kept: false,
        }
    }

    /// Keep the panel in the group while it is not shown, at nothing, and
    /// its size in the group's state at nothing with it: for a panel before
    /// others, whose place the state's sizes are indexed by. A panel left
    /// out of the group would leave its last size in the state, and the
    /// state counts it against the container.
    pub(crate) fn kept(mut self) -> Self {
        self.kept = true;
        self
    }
}

/// A resizable group's panels gliding to their new sizes when the group
/// starts or stops showing one of them: a panel opening grows out of
/// nothing, a closing one stays mounted while it shrinks to nothing, and the
/// panels that take what the others leave follow. Owned by the view that
/// renders the group and sampled in its render, once per frame, before the
/// group is built.
///
/// gpui-base's `h_resizable`/`v_resizable` keep panel sizes in
/// [`ResizableState`] and have no size transition: a panel mounted into a
/// group, or taken out, snaps; this goes once they glide. The glide never writes the state while it
/// runs, so a drag or a restore the owner makes meanwhile is not fought;
/// it pins each sized panel's `size_range` to the glide's size for the
/// frame instead, the one constraint the panel lets its caller set that
/// overrides the state's size, and lets the panels that take the rest go
/// down to nothing. On the frame a change lands, the glide's last or, when
/// it does not glide, the change's own, it pins the targets and has the
/// group measure every panel afresh from that layout, so the next frame,
/// unpinned, draws the same layout, and a drag starts from it.
///
/// Only a change of which panels are shown glides; a size that changes
/// while they stay the same (a window resize, a drag) is followed at once,
/// and a glide under way turns toward it.
///
/// The chat column's dock glides its splits with [`crate::panes::Glides`]
/// instead: its slots take no `size_range` from the caller, so that one
/// writes the sizes into the dock's tree on every frame.
pub(crate) struct ResizeGlide {
    id: ElementId,
    axis: Axis,
    slots: Vec<Slot>,
    glide: Option<Glide>,
    generation: u64,
    /// The scene of the surface the group lays out: a new one's panels
    /// land where it puts them, as the group's first frame does.
    scene: super::InScene,
}

#[derive(Default)]
struct Slot {
    shown: bool,
    /// The size the panel's content was drawn at, recorded by
    /// [`ResizeFrame::hold`]; both sides, so a group that turns to the other
    /// axis knows where its panels start.
    drawn: Rc<Cell<Option<Size<Pixels>>>>,
}

struct Glide {
    generation: u64,
    from: Vec<Pixels>,
    /// `None` for a panel that takes what the others leave.
    to: Vec<Option<Pixels>>,
    /// Each panel's widest size over the glide. A panel growing out of
    /// nothing or shrinking into it has its content laid out at that and
    /// clipped, so it slides in or out whole.
    widest: Vec<Pixels>,
    /// Whether the panel is shown at both ends of the glide. Its content is
    /// laid out at the size it is drawn at on every frame: it has no edge to
    /// slide from, and held at its widest it would jump to its new layout at
    /// one end.
    follows: Vec<bool>,
    mounted: Vec<bool>,
    now: Vec<Pixels>,
}

/// One frame of a [`ResizeGlide`], for building the group.
pub(crate) struct ResizeFrame {
    axis: Axis,
    pins: Vec<Pin>,
    drawn: Vec<Rc<Cell<Option<Size<Pixels>>>>>,
}

#[derive(Clone, Copy, Debug)]
enum Pin {
    Rest {
        shown: bool,
        kept: bool,
    },
    /// `end`: the panel is before the first one that takes the rest, so its
    /// content keeps to its far edge and slides in and out with it.
    Sized {
        size: Pixels,
        hold: Pixels,
        end: bool,
    },
    Flex {
        hold: Option<Pixels>,
    },
}

impl ResizeGlide {
    /// The glide of a group along `axis`, named `id` among the window's
    /// transitions.
    pub(crate) fn new(id: impl Into<ElementId>, axis: Axis) -> Self {
        Self {
            id: id.into(),
            axis,
            slots: Vec::new(),
            glide: None,
            generation: 0,
            scene: Default::default(),
        }
    }

    /// The group of a surface in `epoch`'s scene.
    pub(crate) fn in_scene(mut self, epoch: &super::Epoch) -> Self {
        self.scene = super::InScene::new(epoch);
        self
    }

    /// Lay the group out along `axis` from now on. A glide under way lands;
    /// the sizes its panels were last drawn at are kept, so a change of
    /// which panels show on the frame it turns glides along the new axis.
    pub(crate) fn turn(&mut self, axis: Axis) {
        if self.axis != axis {
            self.axis = axis;
            self.glide = None;
        }
    }

    /// Forget the group's layout: its next frame lands where it is, as the
    /// group's first does.
    pub(crate) fn reset(&mut self) {
        self.slots.clear();
        self.glide = None;
    }

    /// This frame's layout for the group, whose state is `state`, showing
    /// `panels` in order. A change of which panels are shown glides
    /// when `animate`, and lands at once otherwise; the first frame never
    /// glides, nor does one whose shown panels have not all been drawn.
    pub(crate) fn sample(
        &mut self,
        panels: &[Panel],
        animate: bool,
        state: &Entity<ResizableState>,
        window: &mut Window,
        cx: &mut App,
    ) -> ResizeFrame {
        if self.scene.entered() {
            self.reset();
        }
        let axis = self.axis;
        if self.slots.len() != panels.len() {
            self.slots.resize_with(panels.len(), Slot::default);
            self.glide = None;
        }
        let changed = self
            .slots
            .iter()
            .zip(panels)
            .any(|(slot, panel)| slot.shown != panel.shown);
        let measured = self
            .slots
            .iter()
            .all(|slot| !slot.shown || slot.drawn.get().is_some());
        let drawn_before = self.slots.iter().any(|slot| slot.shown);
        let was_gliding = self.glide.is_some();
        if changed && animate && measured && drawn_before {
            self.begin(panels);
        } else if !animate {
            self.glide = None;
        } else if let Some(glide) = &mut self.glide {
            for (ix, panel) in panels.iter().enumerate() {
                if let Some(size) = panel.size {
                    let to = if panel.shown { size } else { px(0.) };
                    glide.to[ix] = Some(to);
                    glide.widest[ix] = glide.widest[ix].max(to);
                }
            }
        }
        for (slot, panel) in self.slots.iter_mut().zip(panels) {
            slot.shown = panel.shown;
        }
        let drawn = self.slots.iter().map(|slot| slot.drawn.clone()).collect();

        let Some(glide) = &mut self.glide else {
            if changed || was_gliding {
                lay_out_afresh(state, cx);
            }
            settle(state, panels, false, window, cx);
            return ResizeFrame {
                axis,
                pins: panels
                    .iter()
                    .map(|panel| Pin::Rest {
                        shown: panel.shown,
                        kept: panel.kept,
                    })
                    .collect(),
                drawn,
            };
        };
        let sample = Presence::new(
            ElementId::NamedChild(
                std::sync::Arc::new(self.id.clone()),
                glide.generation.to_string().into(),
            ),
            true,
        )
        .transition(gpui_base::motion::Transition::new(GLIDE))
        .sample(window, cx);
        let landed = sample.phase == PresencePhase::Present;
        let progress = if landed { 1. } else { sample.progress };
        glide.now = glide
            .from
            .iter()
            .zip(&glide.to)
            .map(|(from, to)| match to {
                Some(to) => *from + (*to - *from) * progress,
                None => px(0.),
            })
            .collect();
        let first_flex = panels.iter().position(|panel| panel.size.is_none());
        let pins = glide
            .to
            .iter()
            .enumerate()
            .map(|(ix, to)| match (glide.mounted[ix], to) {
                (false, _) => Pin::Rest {
                    shown: false,
                    kept: panels[ix].kept,
                },
                (true, Some(_)) => Pin::Sized {
                    size: glide.now[ix],
                    hold: match glide.follows[ix] {
                        true => glide.now[ix],
                        false => glide.widest[ix],
                    },
                    end: first_flex.is_some_and(|flex| ix < flex),
                },
                (true, None) => Pin::Flex {
                    hold: (!glide.follows[ix]).then_some(glide.widest[ix]),
                },
            })
            .collect();
        if landed {
            lay_out_afresh(state, cx);
            settle(state, panels, true, window, cx);
            self.glide = None;
        }
        ResizeFrame { axis, pins, drawn }
    }

    fn begin(&mut self, panels: &[Panel]) {
        let current: Vec<Option<Pixels>> = self
            .slots
            .iter()
            .enumerate()
            .map(|(ix, slot)| {
                let drawn = slot
                    .drawn
                    .get()
                    .map(|drawn| along(drawn, self.axis))
                    .unwrap_or_default();
                let gliding =
                    self.glide.as_ref().filter(|glide| glide.mounted[ix]).map(
                        |glide| match glide.to[ix] {
                            Some(_) => glide.now[ix],
                            None => drawn,
                        },
                    );
                gliding.or_else(|| slot.shown.then_some(drawn))
            })
            .collect();
        let extent: Pixels = current.iter().flatten().copied().sum();
        let to: Vec<Option<Pixels>> = panels
            .iter()
            .map(|panel| {
                panel
                    .size
                    .map(|size| if panel.shown { size } else { px(0.) })
            })
            .collect();
        let flexes = panels
            .iter()
            .filter(|panel| panel.size.is_none() && panel.shown)
            .count();
        let rest = (extent - to.iter().flatten().copied().sum::<Pixels>()).max(px(0.));
        let flex_size = rest / flexes.max(1) as f32;
        let (widest, follows) = panels
            .iter()
            .zip(&current)
            .zip(&to)
            .map(|((panel, current), to)| {
                let before = current.unwrap_or_default();
                let after = match to {
                    Some(to) => *to,
                    None if panel.shown => flex_size,
                    None => px(0.),
                };
                (before.max(after), before > px(0.) && after > px(0.))
            })
            .unzip();
        let from: Vec<Pixels> = current
            .iter()
            .map(|current| current.unwrap_or_default())
            .collect();
        self.generation += 1;
        self.glide = Some(Glide {
            generation: self.generation,
            now: from.clone(),
            from,
            to,
            widest,
            follows,
            mounted: current
                .iter()
                .zip(panels)
                .map(|(current, panel)| current.is_some() || panel.shown)
                .collect(),
        });
    }
}

impl ResizeFrame {
    /// Whether panel `ix` is in the group this frame: shown, or still
    /// gliding out.
    pub(crate) fn mounted(&self, ix: usize) -> bool {
        match self.pins.get(ix) {
            Some(Pin::Rest { shown, kept }) => *shown || *kept,
            Some(_) => true,
            None => false,
        }
    }

    /// The size panel `ix` is held at while the group glides.
    pub(crate) fn size(&self, ix: usize) -> Option<Pixels> {
        match self.pins.get(ix) {
            Some(Pin::Sized { size, .. }) => Some(*size),
            _ => None,
        }
    }

    /// Whether panel `ix`'s content is drawn this frame: shown, or gliding.
    pub(crate) fn shows(&self, ix: usize) -> bool {
        match self.pins.get(ix) {
            Some(Pin::Rest { shown, .. }) => *shown,
            Some(_) => true,
            None => false,
        }
    }

    /// The `size_range` panel `ix` is given this frame, for the range it has
    /// at rest.
    pub(crate) fn range(&self, ix: usize, rest: Range<Pixels>) -> Range<Pixels> {
        match self.pins.get(ix) {
            Some(Pin::Sized { size, .. }) => *size..*size,
            Some(Pin::Flex { .. }) => px(0.)..rest.end,
            Some(Pin::Rest {
                shown: false,
                kept: true,
            }) => px(0.)..px(0.),
            _ => rest,
        }
    }

    /// Panel `ix`'s content: while the group glides, laid out at the size
    /// it is drawn at when it is shown at both ends, and otherwise at its
    /// widest, clipped to the panel and kept to the edge it slides in from;
    /// measured, so a glide knows where it starts. Always the same elements,
    /// so the content keeps what it holds by its place in the tree.
    pub(crate) fn hold(&self, ix: usize, content: impl IntoElement) -> AnyElement {
        let (hold, end) = match self.pins.get(ix) {
            Some(Pin::Sized { hold, end, .. }) => (Some(*hold), *end),
            Some(Pin::Flex { hold }) => (*hold, false),
            _ => (None, false),
        };
        let axis = self.axis;
        let drawn = self.drawn.get(ix).cloned();
        div()
            .size_full()
            .flex()
            .when(axis == Axis::Vertical, |this| this.flex_col())
            .when(end, |this| this.justify_end())
            .when(hold.is_some(), |this| this.overflow_hidden())
            .child(
                div()
                    .map(|this| match (hold, axis) {
                        (Some(hold), Axis::Horizontal) => this.flex_none().h_full().w(hold),
                        (Some(hold), Axis::Vertical) => this.flex_none().w_full().h(hold),
                        (None, _) => this.size_full(),
                    })
                    .child(content),
            )
            .on_prepaint(move |bounds, _, _| {
                if let Some(drawn) = drawn {
                    drawn.set(Some(bounds.size));
                }
            })
            .into_any_element()
    }
}

fn along(size: Size<Pixels>, axis: Axis) -> Pixels {
    match axis {
        Axis::Horizontal => size.width,
        Axis::Vertical => size.height,
    }
}

/// Have the group measure every panel's size from this frame's layout, as
/// it does on its first frame: for a frame that lands a change of which
/// panels show, before the group is built.
///
/// gpui-base takes a panel's size into the state only from the panel's
/// first measured bounds, and a panel joining the group leaves the others'
/// sizes as they were; a drag then starts from those sizes rather than
/// from the bounds. A panel that joined at its full size would leave its
/// neighbour's size from before it was there, and the first drag would
/// move the edge by that difference.
fn lay_out_afresh(state: &Entity<ResizableState>, cx: &mut App) {
    state.update(cx, |state, _| state.clear());
}

/// Bring the group's state into line with `panels` after this frame's
/// prepaint, which records the ranges this frame gives them: every kept panel
/// not shown at nothing. A glide that `landed` draws the next frame, unpinned.
fn settle(
    state: &Entity<ResizableState>,
    panels: &[Panel],
    landed: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let sizes = state.read(cx).sizes();
    let writes: Vec<(usize, Pixels)> = panels
        .iter()
        .enumerate()
        .filter(|(ix, panel)| {
            panel.kept && !panel.shown && sizes.get(*ix).is_some_and(|held| held.abs() > px(0.5))
        })
        .map(|(ix, _)| (ix, px(0.)))
        .collect();
    if writes.is_empty() && !landed {
        return;
    }
    let state = state.clone();
    window.defer(cx, move |window, cx| {
        state.update(cx, |state, cx| {
            for (ix, size) in writes {
                state.resize_panel(ix, size, window, cx);
            }
        });
        window.refresh();
    });
}
