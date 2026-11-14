//! The rail's cards stacked in the rail's height, which they share and
//! never leave.
//!
//! Each card is its header and its body. When the bodies fit, every
//! card is as tall as its content; when they do not, the bodies share the
//! rail alike rather than by how much each holds: one that holds less than
//! the others are given keeps all of it, and those that hold more are each
//! given the same height, down to a few rows, and scroll inside it, while a
//! body that cannot ([`StackCard::rigid`]) keeps its whole content, and a
//! body of rows that never scrolls ([`StackCard::fit`]) gives up rows down
//! to its first, shows those that fit and counts the rest; when not even
//! that fits, a part that scales gives up its height ([`Scale`]), and then
//! the headers keep their height and the bodies share what is left in
//! proportion to those heights. A card coming or going takes its share as
//! it moves, so the others give way to it as it goes rather than at the end.
//!
//! Taffy cannot lay this out: a flex item's minimum height never gives way
//! when the minimums overflow the column, and a fold (`motion::Reveal`)
//! keeps its height while it moves rather than sharing in the shrink. So
//! the stack measures each header and body as the fold measures its
//! content, works the heights out itself, lays each part out at its height
//! and folds cards in and out itself.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, SharedString, Window, point, px, size,
};

/// How tall a body that scrolls stays, at least, while the cards share a
/// rail too short for them all: about three rows.
pub(super) const MIN_BODY: Pixels = px(72.);

/// One card of the stack.
pub(super) struct StackCard {
    /// The card's surface, laid out at the card's height.
    pub(super) chrome: AnyElement,
    pub(super) header: AnyElement,
    /// The body's scroll region, laid out at the height the card is given.
    pub(super) body: Option<AnyElement>,
    /// Whether the body holds content of its own height rather than rows
    /// to scroll through: it gives up none of it to the other cards.
    pub(super) rigid: bool,
    /// Rows the body shows as many of as fit, counting the rest, rather
    /// than scrolling through them.
    pub(super) fit: Option<Fit>,
    /// How far in the card is, from absent to at rest.
    pub(super) presence: f32,
    /// A part of the body that scales down before the bodies are cut below
    /// their least.
    pub(super) scale: Option<Scale>,
}

/// A part of a card's body that gives up height by scaling rather than
/// scrolling (a picture), from `full` down to `min`, once the rail cannot
/// hold every body at its least with the part at `full`. The stack sets
/// the height it is to take in `height`, which the body draws it at from
/// the next frame on.
pub(super) struct Scale {
    pub(super) full: f32,
    pub(super) min: f32,
    pub(super) height: Rc<Cell<Option<f32>>>,
}

impl Scale {
    /// The height the part is drawn at now.
    pub(super) fn now(&self) -> f32 {
        self.height.get().unwrap_or(self.full)
    }
}

/// A body of rows that never scrolls: given less height than all its rows
/// take, it shows as many as fit above a count of the rest. The body notes
/// in `fitted` the height it lays each row and the count out at, and shows
/// as many rows as the stack sets there from the next frame on.
pub(super) struct Fit {
    /// The body's rows in order, each by the key its height is noted under.
    pub(super) rows: Vec<SharedString>,
    /// How much of the body's height is not rows or the count (its inset).
    pub(super) edge: f32,
    pub(super) fitted: Rc<RefCell<Fitted>>,
}

/// What a [`Fit`] body and the stack tell each other.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Fitted {
    /// How tall the body lays each row out, by its key, once it has: rows
    /// differ (one with a button stands taller than one of text).
    pub(super) rows: HashMap<SharedString, f32>,
    /// How tall it lays the count out, once it has.
    pub(super) more: Option<f32>,
    /// How many rows it shows: all of them until the stack says.
    pub(super) shown: Option<usize>,
}

impl Fit {
    /// How far a row may run past the height it is given and still be
    /// in it: under half a pixel, which laying it out on whole pixels takes.
    const SLACK: f32 = 0.5;

    /// How tall each row is: as it was laid out, and one not laid out yet
    /// as the shortest that was, so that it is shown, and so measured,
    /// rather than left out for a height it may not have. `None` until a
    /// row has been laid out.
    fn heights(&self, fitted: &Fitted) -> Option<Vec<f32>> {
        let laid_out = |key| fitted.rows.get(key).copied().filter(|row| *row > 0.);
        let shortest = self.rows.iter().filter_map(laid_out).reduce(f32::min)?;
        Some(
            self.rows
                .iter()
                .map(|key| laid_out(key).unwrap_or(shortest))
                .collect(),
        )
    }

    /// What the body asks of the rail, laid out `laid_out` tall: every row,
    /// and at least its first and the count of the rest. Until it has laid
    /// a row out it asks for what it laid out, which is every row.
    fn demand(&self, fitted: &Fitted, laid_out: f32) -> Demand {
        let Some(heights) = self.heights(fitted) else {
            return Demand {
                natural: laid_out,
                min: least(laid_out, false),
            };
        };
        let natural = self.edge + heights.iter().sum::<f32>();
        let first = heights[0];
        Demand {
            natural,
            min: natural.min(self.edge + first + fitted.more.unwrap_or(first)),
        }
    }

    /// How many rows fit `height`: every one when they all do, else as
    /// many as fit whole above the count.
    fn fitting(&self, fitted: &Fitted, height: f32) -> usize {
        let Some(heights) = self.heights(fitted) else {
            return self.rows.len();
        };
        let room = height + Self::SLACK - self.edge;
        if heights.iter().sum::<f32>() <= room {
            return self.rows.len();
        }
        let room = room - fitted.more.unwrap_or(heights[0]);
        let mut taken = 0.;
        heights[..heights.len() - 1]
            .iter()
            .take_while(|row| {
                taken += **row;
                taken <= room
            })
            .count()
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

impl Demand {
    /// How tall the body is with every body at most `level` tall.
    fn at(&self, level: f32) -> f32 {
        level.min(self.natural).max(self.min)
    }
}

/// The least a body `natural` tall is given while the rail has room for
/// every body's least: all of it when it is rigid, else a few rows of it.
fn least(natural: f32, rigid: bool) -> f32 {
    if rigid {
        natural
    } else {
        natural.min(f32::from(MIN_BODY))
    }
}

/// The height each body gets out of `room`, each `presence` of the way
/// in: each its natural height when they all fit; else one height for
/// every body, which a body of less content than that keeps all of and a
/// body whose minimum is more than that is held at, as tall as the room
/// lets it be; else the minimums scaled down to fit. A body on its way in
/// or out takes as much of its height as it is in.
fn allot(demands: &[Demand], presences: &[f32], room: f32) -> Vec<f32> {
    let room = room.max(0.);
    let taken = |height: &dyn Fn(&Demand) -> f32| -> f32 {
        demands
            .iter()
            .zip(presences)
            .map(|(demand, presence)| height(demand) * presence)
            .sum()
    };
    let natural = taken(&|demand| demand.natural);
    if natural <= room {
        return demands.iter().map(|demand| demand.natural).collect();
    }
    let floor = taken(&|demand| demand.min);
    if floor >= room {
        let scale = if floor > 0. { room / floor } else { 0. };
        return demands.iter().map(|demand| demand.min * scale).collect();
    }
    // What the bodies take with every one at most `level` tall grows with
    // `level` in a line between the heights a body stops at, so the level
    // that fills the room lies on the line between two of them.
    let at = |level: f32| taken(&|demand| demand.at(level));
    let mut stops: Vec<f32> = demands
        .iter()
        .flat_map(|demand| [demand.min, demand.natural])
        .collect();
    stops.sort_by(f32::total_cmp);
    let mut below = (0., at(0.));
    let mut level = stops.last().copied().unwrap_or(0.);
    for stop in stops {
        let taken = at(stop);
        if taken >= room {
            level = if taken > below.1 {
                below.0 + (room - below.1) * (stop - below.0) / (taken - below.1)
            } else {
                stop
            };
            break;
        }
        below = (stop, taken);
    }
    demands.iter().map(|demand| demand.at(level)).collect()
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
        // What each fitting body had noted as this frame began.
        let fitted: Vec<Option<Fitted>> = self
            .cards
            .iter()
            .map(|card| card.fit.as_ref().map(|fit| fit.fitted.borrow().clone()))
            .collect();
        for (card, fitted) in self.cards.iter_mut().zip(&fitted) {
            let presence = card.presence.clamp(0., 1.);
            let header = f32::from(card.header.layout_as_root(natural, window, cx).height);
            let body = card.body.as_mut().map_or(0., |body| {
                f32::from(body.layout_as_root(natural, window, cx).height)
            });
            room -= presence * (header + gap);
            headers.push(header);
            demands.push(match (&card.fit, fitted) {
                (Some(fit), Some(fitted)) => fit.demand(fitted, body),
                _ => Demand {
                    natural: body,
                    min: least(body, card.rigid),
                },
            });
        }
        let presences: Vec<f32> = self
            .cards
            .iter()
            .map(|card| card.presence.clamp(0., 1.))
            .collect();
        // A part that scales gives up its height once the bodies at their
        // least do not fit: each body is measured as it would be with the
        // part at full height, and the part takes as much less as the rail
        // lacks, down to its minimum.
        let mut lacking = demands
            .iter()
            .zip(&self.cards)
            .zip(&presences)
            .map(|((demand, card), presence)| {
                let min = match &card.scale {
                    Some(scale) => least(demand.natural + scale.full - scale.now(), card.rigid),
                    None => demand.min,
                };
                min * presence
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
            demand.min = least(demand.natural, card.rigid);
            if (height - scale.now()).abs() >= 0.5 {
                scale.height.set(Some(height));
                window.request_animation_frame();
            }
        }
        // How tall each body is drawn; its card takes of the rail as much
        // of that as it is in.
        let shares = allot(&demands, &presences, room);

        let mut placed = Vec::with_capacity(self.cards.len());
        let mut top = bounds.origin.y;
        for (index, card) in self.cards.iter_mut().enumerate() {
            let presence = presences[index];
            let header = headers[index];
            // A card on its way in or out is drawn at its height in, clipped
            // to the part of it that is in.
            let shown = if presence > 0. { shares[index] } else { 0. };
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
            // A body that fits its rows shows from the next frame as many as
            // fit what it is given, and is measured again once it has noted
            // new heights.
            if let (Some(fit), Some(began)) = (&card.fit, &fitted[index])
                && presence > 0.
            {
                let mut now = fit.fitted.borrow_mut();
                let rows = fit.fitting(&now, shown);
                if *now != *began || now.shown != Some(rows) {
                    now.shown = Some(rows);
                    window.request_animation_frame();
                }
            }
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
