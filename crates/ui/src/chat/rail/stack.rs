//! The rail's cards stacked in the rail's height, which they share and
//! never leave.
//!
//! Each card is its header and its body. When the bodies fit, every
//! card is as tall as its content; when they do not, each body gives
//! up height in proportion to its own, down to a few rows, and scrolls
//! inside; when not even those few rows fit, the headers keep their height
//! and the bodies share what is left. A card coming or going takes its
//! share as it moves, so the others give way to it as it goes rather than
//! at the end.
//!
//! Taffy cannot lay this out: a flex item's minimum height never gives way
//! when the minimums overflow the column, and a fold (`motion::Reveal`)
//! keeps its height while it moves rather than sharing in the shrink. So
//! the stack measures each header and body as the fold measures its
//! content, works the heights out itself, lays each part out at its height
//! and folds cards in and out itself.

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Window, point, px, size,
};

/// How tall a body stays, at least, while the cards share a rail too
/// short for them all: about three rows.
pub(super) const MIN_BODY: Pixels = px(72.);

/// One card of the stack.
pub(super) struct StackCard {
    /// The card's surface, laid out at the card's height.
    pub(super) chrome: AnyElement,
    pub(super) header: AnyElement,
    /// The body's scroll region, laid out at the height the card is given.
    pub(super) body: Option<AnyElement>,
    /// How far in the card is, from absent to at rest.
    pub(super) presence: f32,
    /// A part of the body that scales down before the body scrolls.
    pub(super) scale: Option<Scale>,
}

/// A part of a card's body that gives up height by scaling rather than
/// scrolling (a picture), from `full` down to `min`. The stack sets the
/// height it is to take in `height`, which the body draws it at from the
/// next frame on.
pub(super) struct Scale {
    pub(super) full: f32,
    pub(super) min: f32,
    pub(super) height: std::rc::Rc<std::cell::Cell<Option<f32>>>,
}

impl Scale {
    /// The height the part is drawn at now.
    pub(super) fn now(&self) -> f32 {
        self.height.get().unwrap_or(self.full)
    }
}

/// The cards, top to bottom, `gap` apart.
pub(super) struct RailStack {
    pub(super) id: ElementId,
    pub(super) cards: Vec<StackCard>,
    pub(super) gap: Pixels,
}

/// What one body asks of the rail's height: as tall as its content, and no
/// shorter than its minimum while the rail has room for that.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Demand {
    natural: f32,
    min: f32,
}

/// The height each body gets out of `room`: each its natural height when
/// they all fit; else each shrunk in proportion to its natural height but
/// no lower than its minimum; else the minimums scaled down to fit.
fn allot(demands: &[Demand], room: f32) -> Vec<f32> {
    let room = room.max(0.);
    let natural: f32 = demands.iter().map(|demand| demand.natural).sum();
    if natural <= room {
        return demands.iter().map(|demand| demand.natural).collect();
    }
    let floor: f32 = demands.iter().map(|demand| demand.min).sum();
    if floor >= room {
        let scale = if floor > 0. { room / floor } else { 0. };
        return demands.iter().map(|demand| demand.min * scale).collect();
    }
    // Shrink the bodies that stay above their minimum; one that would go
    // below it holds there, and the rest share what remains.
    let mut held = vec![false; demands.len()];
    loop {
        let (free, held_room) =
            demands
                .iter()
                .zip(&held)
                .fold((0f32, 0f32), |(free, held_room), (demand, held)| {
                    if *held {
                        (free, held_room + demand.min)
                    } else {
                        (free + demand.natural, held_room)
                    }
                });
        let scale = if free > 0. {
            (room - held_room) / free
        } else {
            0.
        };
        let mut changed = false;
        for (demand, held) in demands.iter().zip(held.iter_mut()) {
            if !*held && demand.natural * scale < demand.min {
                *held = true;
                changed = true;
            }
        }
        if !changed {
            return demands
                .iter()
                .zip(&held)
                .map(|(demand, held)| {
                    if *held {
                        demand.min
                    } else {
                        demand.natural * scale
                    }
                })
                .collect();
        }
    }
}

/// Where each card's parts were laid out, for painting.
pub(super) struct Placed {
    cards: Vec<PlacedCard>,
}

struct PlacedCard {
    /// Whether its surface is drawn.
    surface: bool,
    /// The card's place in the rail.
    bounds: Bounds<Pixels>,
    /// The part of the card its body shows in.
    body: Bounds<Pixels>,
}

impl IntoElement for RailStack {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for RailStack {
    type RequestLayoutState = ();
    type PrepaintState = Placed;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = gpui::Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Placed {
        let width = bounds.size.width;
        let natural = size(AvailableSpace::Definite(width), AvailableSpace::MinContent);
        let gap = f32::from(self.gap);
        let mut room = f32::from(bounds.size.height);
        let mut headers = Vec::with_capacity(self.cards.len());
        let mut demands = Vec::with_capacity(self.cards.len());
        for card in &mut self.cards {
            let presence = card.presence.clamp(0., 1.);
            let header = f32::from(card.header.layout_as_root(natural, window, cx).height);
            let body = card.body.as_mut().map_or(0., |body| {
                f32::from(body.layout_as_root(natural, window, cx).height)
            });
            room -= presence * (header + gap);
            headers.push(header);
            demands.push(Demand {
                natural: body,
                min: body.min(f32::from(MIN_BODY)),
            });
        }
        let presences: Vec<f32> = self
            .cards
            .iter()
            .map(|card| card.presence.clamp(0., 1.))
            .collect();
        // A part that scales gives up its height first: each body is
        // measured as it would be with the part at full height, and the
        // part takes as much less as the rail lacks, down to its minimum.
        let mut lacking = demands
            .iter()
            .zip(&self.cards)
            .zip(&presences)
            .map(|((demand, card), presence)| {
                let full = card
                    .scale
                    .as_ref()
                    .map_or(0., |scale| scale.full - scale.now());
                (demand.natural + full) * presence
            })
            .sum::<f32>()
            - room;
        for ((demand, card), presence) in demands.iter_mut().zip(&self.cards).zip(&presences) {
            let Some(scale) = &card.scale else {
                continue;
            };
            let give = if lacking > 0. && *presence > 0. {
                (lacking / presence).min(scale.full - scale.min).max(0.)
            } else {
                0.
            };
            lacking -= give * presence;
            let height = scale.full - give;
            demand.natural += height - scale.now();
            demand.min = demand.natural.min(f32::from(MIN_BODY));
            if (height - scale.now()).abs() >= 0.5 {
                scale.height.set(Some(height));
                window.request_animation_frame();
            }
        }
        // What each body takes of the rail, as far in as its card is.
        let shares = allot(
            &demands
                .iter()
                .zip(&presences)
                .map(|(demand, presence)| Demand {
                    natural: demand.natural * presence,
                    min: demand.min * presence,
                })
                .collect::<Vec<_>>(),
            room,
        );

        let mut placed = Vec::with_capacity(self.cards.len());
        let mut top = bounds.origin.y;
        for (index, card) in self.cards.iter_mut().enumerate() {
            let presence = presences[index];
            let header = headers[index];
            // A card on its way in or out is drawn at its height in, clipped
            // to the part of it that is in.
            let shown = if presence > 0. {
                shares[index] / presence
            } else {
                0.
            };
            let full = header + shown;
            let height = px(full * presence);
            let origin = point(bounds.origin.x, top);
            let card_bounds = Bounds::new(origin, size(width, height));
            // A card just in or almost out is shorter than its surface's
            // border, which would then reach below it: it shows no surface.
            // The layout puts the surface's bottom on the nearest device
            // pixel, up to half of one below a card whose height falls
            // between them, which is not the border reaching below it.
            let snap = px(0.5 / window.scale_factor());
            let surface = card
                .chrome
                .layout_as_root(
                    size(
                        AvailableSpace::Definite(width),
                        AvailableSpace::Definite(height),
                    ),
                    window,
                    cx,
                )
                .height
                <= height + snap;
            if surface {
                card.chrome.prepaint_at(origin, window, cx);
            }
            let body_origin = point(origin.x, origin.y + px(header));
            let mut body_bounds =
                Bounds::new(body_origin, size(width, px(shown))).intersect(&card_bounds);
            let shows_body = body_bounds.size.height > px(0.);
            if !shows_body {
                body_bounds.size.height = px(0.);
            }
            window.with_content_mask(
                Some(ContentMask {
                    bounds: card_bounds,
                }),
                |window| {
                    card.header.prepaint_at(origin, window, cx);
                    // A body none of which is in yet is not drawn.
                    if let Some(body) = card.body.as_mut().filter(|_| shows_body) {
                        body.layout_as_root(
                            size(
                                AvailableSpace::Definite(width),
                                AvailableSpace::Definite(px(shown)),
                            ),
                            window,
                            cx,
                        );
                        window.with_content_mask(
                            Some(ContentMask {
                                bounds: body_bounds,
                            }),
                            |window| body.prepaint_at(body_origin, window, cx),
                        );
                    }
                },
            );
            placed.push(PlacedCard {
                surface,
                bounds: card_bounds,
                body: body_bounds,
            });
            top += height + px(gap * presence);
        }
        Placed { cards: placed }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        placed: &mut Placed,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (card, place) in self.cards.iter_mut().zip(&placed.cards) {
            if place.surface {
                card.chrome.paint(window, cx);
            }
            window.with_content_mask(
                Some(ContentMask {
                    bounds: place.bounds,
                }),
                |window| {
                    card.header.paint(window, cx);
                    if let Some(body) = card
                        .body
                        .as_mut()
                        .filter(|_| place.body.size.height > px(0.))
                    {
                        window.with_content_mask(
                            Some(ContentMask { bounds: place.body }),
                            |window| body.paint(window, cx),
                        );
                    }
                },
            );
        }
    }
}
