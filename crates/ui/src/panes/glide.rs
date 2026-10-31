//! The chat column's layout motion: a split's slots glide to the shares an
//! edit gave them.

use std::collections::{HashMap, HashSet};

use gpui::{Along as _, App, Axis, ElementId, Pixels, Size, Window};
use gpui_base::dock::{NodeId, PaneNode, PaneRef, PaneTree, PanelId};

/// The chat column's splits gliding to the shares their last edit gave them,
/// over `edge_back::SETTLE`: a new pane grows out of nothing beside the one
/// it halves, and a closing one shrinks to nothing into its neighbours.
///
/// gpui-base's dock draws a split at the slot sizes its tree holds and has no
/// layout transition, so the shell samples this every frame and writes the
/// sizes back through `DockArea::set_split_sizes`; the element tree never
/// changes for it. A glide is expressed in shares of its split, so a window
/// resize meanwhile only rescales it.
///
/// Every dock slot is floored at gpui-base's `PANEL_MIN_SIZE` (100px):
/// `DockArea` builds each slot with `resizable_panel`'s default size range
/// and offers no way to give a slot another. Below that floor a glide shows
/// only where the slot is its split's last, which then runs past the split's
/// end and is clipped there by the split's frame; anywhere else the slot
/// stays 100px wide while its share is smaller, so its content fades over
/// that stretch with the glide's own progress instead ([`Held::opacity`]):
/// out to nothing as it is taken out, in from nothing as it passes the floor.
#[derive(Default)]
pub(crate) struct Glides {
    splits: HashMap<NodeId, Glide>,
    generation: u64,
}

struct Glide {
    generation: u64,
    from: Vec<f32>,
    to: Vec<f32>,
    /// Each slot's widest share over the glide. The pane's content is laid
    /// out at that and clipped, so it reflows once when the glide lands
    /// rather than on every frame.
    widest: Vec<f32>,
    now: Vec<f32>,
}

/// The layout an edit starts from, for [`Glides::begin`].
pub(crate) struct Before {
    panels: HashSet<PanelId>,
    /// Each split's slots: the first panel each holds, the share it is drawn
    /// at, and its widest share while it glides.
    splits: HashMap<NodeId, Vec<(Option<PanelId>, f32, f32)>>,
}

/// One frame of the glides.
#[derive(Default)]
pub(crate) struct GlideFrame {
    /// The slot sizes of each gliding split.
    pub(crate) sizes: Vec<(NodeId, Vec<Pixels>)>,
    /// How each gliding pane's content is drawn.
    pub(crate) holds: HashMap<PanelId, Held>,
}

/// How a pane's content is drawn while its split glides.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Held {
    /// The size it is laid out at, along the axes that glide.
    pub(crate) size: Size<Option<Pixels>>,
    /// Below one while its slot's share is under the dock's floor and the
    /// slot is not its split's last.
    pub(crate) opacity: f32,
}

impl Default for Held {
    fn default() -> Self {
        Self {
            size: Size::default(),
            opacity: 1.,
        }
    }
}

enum Visit<'a> {
    Split {
        node: NodeId,
        children: &'a [PaneNode],
        extent: Pixels,
        /// The shares the tree's sizes give the slots.
        tree: Vec<f32>,
        /// The shares the slots are drawn at.
        shares: Vec<f32>,
        gliding: bool,
    },
    Panel {
        panel: PanelId,
        held: Held,
    },
}

impl Glides {
    /// Whether nothing glides.
    pub(crate) fn is_idle(&self) -> bool {
        self.splits.is_empty()
    }

    /// Whether the split `node` is gliding.
    pub(crate) fn gliding(&self, node: NodeId) -> bool {
        self.splits.contains_key(&node)
    }

    /// The layout `tree`, drawn at `size`, as an edit finds it.
    pub(crate) fn before(&self, tree: &PaneTree, size: Size<Pixels>) -> Before {
        let mut splits = HashMap::new();
        self.visit(tree.root(), size, Held::default(), false, &mut |visit| {
            if let Visit::Split {
                node,
                children,
                shares,
                ..
            } = visit
            {
                let widest = self.splits.get(&node).map(|glide| glide.widest.clone());
                let slots = children
                    .iter()
                    .zip(&shares)
                    .enumerate()
                    .map(|(ix, (child, share))| {
                        let widest = widest.as_ref().map_or(*share, |widest| widest[ix]);
                        (first_panel(child, &|_| true), *share, widest)
                    })
                    .collect();
                splits.insert(node, slots);
            }
        });
        Before {
            panels: tree.panels().collect(),
            splits,
        }
    }

    /// What each gliding split's slots will be once it lands, for an edit
    /// to divide: the dock divides the sizes it last measured, which mid-glide
    /// are the glide's.
    pub(crate) fn targets(
        &self,
        tree: &PaneTree,
        size: Size<Pixels>,
    ) -> Vec<(NodeId, Vec<Pixels>)> {
        self.frame(tree, size, true).sizes
    }

    /// Glide the panes an edit added from nothing, out of the slots that were
    /// there before (`before`), into the shares the dock gave them in `tree`.
    /// A split already gliding turns toward its new shares from where it is.
    pub(crate) fn begin(&mut self, before: &Before, tree: &PaneTree, size: Size<Pixels>) {
        let mut started = Vec::new();
        self.visit(tree.root(), size, Held::default(), true, &mut |visit| {
            let Visit::Split {
                node,
                children,
                tree: to,
                ..
            } = visit
            else {
                return;
            };
            let keys: Vec<_> = children
                .iter()
                .map(|child| first_panel(child, &|panel| before.panels.contains(&panel)))
                .collect();
            if keys.iter().all(Option::is_some) || keys.iter().all(Option::is_none) {
                return;
            }
            let (from, widest): (Vec<f32>, Vec<f32>) = match before.splits.get(&node) {
                Some(slots) => keys
                    .iter()
                    .map(|key| {
                        key.and_then(|key| slots.iter().find(|slot| slot.0 == Some(key)))
                            .map_or((0., 0.), |slot| (slot.1, slot.2))
                    })
                    .unzip(),
                // A split the edit made: its panes from before had all of it.
                None => {
                    let kept: f32 = keys
                        .iter()
                        .zip(&to)
                        .filter(|(key, _)| key.is_some())
                        .map(|(_, share)| share)
                        .sum();
                    keys.iter()
                        .zip(&to)
                        .map(|(key, share)| match key {
                            Some(_) if kept > 0. => share / kept,
                            _ => 0.,
                        })
                        .map(|share| (share, share))
                        .unzip()
                }
            };
            started.push((node, normalized(from), to, widest));
        });
        for (node, from, to, widest) in started {
            self.start(node, from, to, widest);
        }
    }

    /// Glide the pane holding `panel` out of its split: its slot shrinks to
    /// nothing and its neighbours share what it leaves in proportion, as the
    /// dock divides a removed slot's space. A split whose other slots are all
    /// closing already glides out of its own split instead.
    pub(crate) fn close(&mut self, tree: &PaneTree, size: Size<Pixels>, panel: PanelId) {
        if let Some(group) = tree.find_panel_node(panel) {
            self.close_node(tree, size, group);
        }
    }

    fn close_node(&mut self, tree: &PaneTree, size: Size<Pixels>, closing: NodeId) {
        let mut found = None;
        self.visit(tree.root(), size, Held::default(), false, &mut |visit| {
            if let Visit::Split {
                node,
                children,
                shares,
                ..
            } = visit
                && let Some(ix) = children.iter().position(|child| child.id() == closing)
            {
                found = Some((node, ix, shares));
            }
        });
        let Some((node, ix, now)) = found else {
            return;
        };
        let (to, widest) = match self.splits.get(&node) {
            Some(glide) if glide.to.len() == now.len() => (glide.to.clone(), glide.widest.clone()),
            _ => (now.clone(), now.clone()),
        };
        let left = 1. - to[ix];
        if left < 1e-4 {
            self.close_node(tree, size, node);
            return;
        }
        let to = to
            .iter()
            .enumerate()
            .map(|(child, share)| if child == ix { 0. } else { share / left })
            .collect();
        self.start(node, now, to, widest);
    }

    fn start(&mut self, node: NodeId, from: Vec<f32>, to: Vec<f32>, widest: Vec<f32>) {
        if from
            .iter()
            .zip(&to)
            .all(|(from, to)| (from - to).abs() < 1e-4)
        {
            return;
        }
        self.generation += 1;
        let widest = widest
            .iter()
            .zip(from.iter().zip(&to))
            .map(|(widest, (from, to))| widest.max(*from).max(*to))
            .collect();
        self.splits.insert(
            node,
            Glide {
                generation: self.generation,
                now: from.clone(),
                from,
                to,
                widest,
            },
        );
    }

    /// This frame's sizes for `tree`, drawn at `size`. A glide that lands
    /// here is gone after it; so is one whose split the layout no longer has.
    pub(crate) fn sample(
        &mut self,
        tree: &PaneTree,
        size: Size<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> GlideFrame {
        let mut landed = Vec::new();
        for (node, glide) in &mut self.splits {
            let sample = crate::motion::appear(
                ElementId::NamedInteger("pane-glide".into(), glide.generation),
                true,
                window,
                cx,
            );
            let progress = match sample.phase {
                crate::motion::PresencePhase::Present => {
                    landed.push(*node);
                    1.
                }
                _ => sample.progress,
            };
            glide.now = glide
                .from
                .iter()
                .zip(&glide.to)
                .map(|(from, to)| from + (to - from) * progress)
                .collect();
        }
        let sizes = self.frame(tree, size, false).sizes;
        self.splits.retain(|node, _| {
            !landed.contains(node) && sizes.iter().any(|(sized, _)| sized == node)
        });
        // A landed glide holds nothing in its last frame: nothing asks for
        // another, so content held now would stay held.
        let holds = self.frame(tree, size, false).holds;
        GlideFrame { sizes, holds }
    }

    /// Land every glide at once: the sizes they were heading for.
    pub(crate) fn land(&mut self, tree: &PaneTree, size: Size<Pixels>) -> GlideFrame {
        let sizes = self.targets(tree, size);
        self.splits.clear();
        GlideFrame {
            sizes,
            ..GlideFrame::default()
        }
    }

    /// Stop the split `node`'s glide where it is: a drag on its handle has
    /// taken its sizes over. Other splits glide on.
    pub(crate) fn stop(&mut self, node: NodeId) {
        self.splits.remove(&node);
    }

    fn frame(&self, tree: &PaneTree, size: Size<Pixels>, target: bool) -> GlideFrame {
        let mut frame = GlideFrame::default();
        self.visit(
            tree.root(),
            size,
            Held::default(),
            target,
            &mut |visit| match visit {
                Visit::Split {
                    node,
                    extent,
                    shares,
                    gliding: true,
                    ..
                } => frame
                    .sizes
                    .push((node, shares.iter().map(|share| extent * *share).collect())),
                Visit::Split { .. } => {}
                Visit::Panel { panel, held } => {
                    if held != Held::default() {
                        frame.holds.insert(panel, held);
                    }
                }
            },
        );
        frame
    }

    /// Walk `node`, drawn at `size`, its panes' content held at `held`: each
    /// split with its slots' shares as drawn now, or as they land when
    /// `target`, after its children.
    fn visit<'a>(
        &self,
        node: &'a PaneNode,
        size: Size<Pixels>,
        held: Held,
        target: bool,
        f: &mut dyn FnMut(Visit<'a>),
    ) {
        match node.kind() {
            PaneRef::Tabs { panels, .. } => {
                for panel in panels {
                    f(Visit::Panel {
                        panel: *panel,
                        held,
                    });
                }
            }
            PaneRef::Split {
                axis,
                children,
                sizes,
            } => {
                let extent = size.along(axis);
                let tree = tree_shares(sizes, extent);
                let glide = self
                    .splits
                    .get(&node.id())
                    .filter(|glide| glide.to.len() == children.len());
                let shares = match glide {
                    Some(glide) if target => glide.to.clone(),
                    Some(glide) => glide.now.clone(),
                    None => tree.clone(),
                };
                for (ix, child) in children.iter().enumerate() {
                    let drawn = extent * shares[ix];
                    let held_along = match glide {
                        Some(glide) => Some(extent * glide.widest[ix]),
                        None => along(held.size, axis).map(|held| held * tree[ix]),
                    };
                    let floored = glide.is_some() && ix + 1 < children.len();
                    let opacity = match floored {
                        true => held.opacity * (drawn / gpui_base::PANEL_MIN_SIZE).clamp(0., 1.),
                        false => held.opacity,
                    };
                    self.visit(
                        child,
                        size.apply_along(axis, |_| drawn),
                        Held {
                            size: with_along(held.size, axis, held_along),
                            opacity,
                        },
                        target,
                        f,
                    );
                }
                f(Visit::Split {
                    node: node.id(),
                    children,
                    extent,
                    tree,
                    shares,
                    gliding: glide.is_some(),
                });
            }
        }
    }
}

/// The first panel under `node` that `counts`.
fn first_panel(node: &PaneNode, counts: &dyn Fn(PanelId) -> bool) -> Option<PanelId> {
    let mut found = None;
    node.walk(&mut |node| {
        if found.is_none()
            && let PaneRef::Tabs { panels, .. } = node.kind()
        {
            found = panels.iter().copied().find(|panel| counts(*panel));
        }
    });
    found
}

/// The shares a split's tree `sizes` give its slots across `extent`. Sizes
/// all named divide in proportion, as the dock rescales them to its
/// container; unnamed ones share what the named ones leave.
fn tree_shares(sizes: &[Option<Pixels>], extent: Pixels) -> Vec<f32> {
    let count = sizes.len().max(1) as f32;
    if sizes.iter().all(Option::is_some) {
        let total: f32 = sizes.iter().flatten().map(|size| f32::from(*size)).sum();
        return sizes
            .iter()
            .flatten()
            .map(|size| match total > 0. {
                true => f32::from(*size) / total,
                false => 1. / count,
            })
            .collect();
    }
    let extent = f32::from(extent).max(1.);
    let named: f32 = sizes
        .iter()
        .flatten()
        .map(|size| f32::from(*size) / extent)
        .sum::<f32>()
        .min(1.);
    let unnamed = sizes.iter().filter(|size| size.is_none()).count() as f32;
    sizes
        .iter()
        .map(|size| match size {
            Some(size) => f32::from(*size) / extent,
            None => (1. - named) / unnamed,
        })
        .collect()
}

fn normalized(shares: Vec<f32>) -> Vec<f32> {
    let total: f32 = shares.iter().sum();
    if total <= 0. {
        return shares;
    }
    shares.into_iter().map(|share| share / total).collect()
}

fn along(size: Size<Option<Pixels>>, axis: Axis) -> Option<Pixels> {
    match axis {
        Axis::Horizontal => size.width,
        Axis::Vertical => size.height,
    }
}

fn with_along(
    size: Size<Option<Pixels>>,
    axis: Axis,
    value: Option<Pixels>,
) -> Size<Option<Pixels>> {
    match axis {
        Axis::Horizontal => Size {
            width: value,
            ..size
        },
        Axis::Vertical => Size {
            height: value,
            ..size
        },
    }
}
