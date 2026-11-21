//! Tcode-owned scroll-area compositions over gpui-base's scrollbar, scroll
//! mask and edge-bounce behavior.
//!
//! GPUI's own scroll containers apply every wheel or pan delta that reaches
//! them, so two nested viewports would both move on one gesture, and a
//! horizontal-only container maps vertical input onto its axis. The areas here
//! pair a viewport with an [`AreaMask`] sibling: the mask takes the events its
//! viewport can use in the capture phase and lets the rest bubble to the
//! ancestor scroller, and a gesture stays with the area that first moved for
//! it ([`gesture`]).

mod autoscroll;
pub(crate) mod gesture;
mod mask;
mod reveal;

pub(crate) use autoscroll::{AutoscrollTarget, MiddleAutoscroll, report_room};
pub(crate) use gesture::recorder as gesture_recorder;
pub(crate) use mask::AreaMask;

use std::{ops::Range, panic::Location, rc::Rc};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use crate::motion::{ListScroll, ScrollTarget};
use crate::widgets::scrollbar::Scrollbar;

use gpui::{
    AnyElement, App, Axis, Bounds, Div, Element, ElementId, Hsla, InteractiveElement, IntoElement,
    ListAlignment, ListOffset, ListSizingBehavior, ListState, ParentElement, Pixels,
    Refineable as _, RenderOnce, ScrollHandle, ScrollStrategy, Stateful,
    StatefulInteractiveElement, StyleRefinement, Styled, UniformListScrollHandle, Window, canvas,
    div, list, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_base::{InteractiveElementExt as _, ScrollBounce, ScrollbarHandle, StyledExt as _};

pub(crate) trait ScrollableElement:
    InteractiveElement + Styled + ParentElement + Element
{
    /// A vertical viewport with Tcode's overlay scrollbar. Nested inside
    /// another scroller it takes a vertical gesture that begins where it can
    /// move, and keeps it at its edges; one that begins at an edge it cannot
    /// move past goes to the ancestor.
    #[track_caller]
    fn overflow_y_scrollbar(self) -> Scrollable<Self> {
        Scrollable::new(self)
    }

    /// A bounded vertical viewport without a scrollbar, for menus, option
    /// lists and detail panes that can sit inside a page or the timeline.
    /// It takes gestures as [`Self::overflow_y_scrollbar`] does.
    #[track_caller]
    fn overflow_y_scroll_area(self) -> ScrollArea<Self> {
        ScrollArea::new(self, Axis::Vertical)
    }

    /// A horizontal strip (tables, key bars, segmented tracks). It owns
    /// horizontal input even at its edges, so a horizontal gesture never moves
    /// the page, and leaves vertical input to the ancestor scroller.
    #[track_caller]
    fn overflow_x_scroll_area(self) -> ScrollArea<Self> {
        ScrollArea::new(self, Axis::Horizontal)
    }
}

/// A page-level vertical viewport with gpui-base's edge bounce.
/// `handle` must be the scroll handle of `element`.
/// Scrollbars and toolbars stay outside; never nest one inside a masked area.
#[track_caller]
pub(crate) fn page_viewport<H: ScrollbarHandle + Clone + 'static>(
    id: impl Into<ElementId>,
    handle: &H,
    element: impl IntoElement,
) -> impl IntoElement {
    let id = id.into();
    let element = reveal::reveals((id.clone(), "reveals"), handle, element);
    report_room(handle, ScrollBounce::new(id, handle, element)).page()
}

/// How far a reader rising from the end of a list following its tail goes
/// before the list stops following.
///
/// gpui-base's `ScrollBounce` hands each wheel and pan packet to
/// `ListState::set_offset_from_scrollbar`, which takes an offset within 1px
/// of the end for a drag to the end and goes on following, and gpui-fast's
/// list layout resumes following within that pixel too. A following list is
/// laid out at its end again every frame, so each packet starts from the
/// end, and a slow trackpad scroll, whose packets are under a pixel, never
/// leaves it. Twice that pixel clears it with room for rounding.
const TAIL_ESCAPE: Pixels = px(2.);

/// The room to scroll a list that fits its viewport reports to its bounce.
///
/// gpui-base's `ScrollBounce` stretches only an area that has somewhere to
/// scroll, and has no setting for one that always bounces: with no room it
/// neither takes the gesture nor keeps a stretch. This is room the bounce
/// can see and a pan cannot feel: the list never moves by it, so all of
/// every packet is left over and stretches the edge.
const FITTING_ROOM: Pixels = px(0.005);

/// A list following its tail, as the handle its [`page_viewport`] scrolls:
/// the packets that rise from the end add up until they reach
/// [`TAIL_ESCAPE`], then move the list by all of them, so a scroll up leaves
/// the tail however slowly it comes. Remove it once a wheel packet that
/// rises from the end of a gpui-fast list pauses its following.
///
/// A list that fits its viewport bounces at its edges as a longer one does,
/// by [`FITTING_ROOM`]. Remove that once gpui-base's bounce can stretch an
/// area with nowhere to scroll.
#[derive(Clone)]
pub(crate) struct FollowedList {
    list: ListState,
    /// What rose from the end while the list followed, not yet applied.
    rise: Rc<std::cell::Cell<Pixels>>,
}

impl FollowedList {
    pub(crate) fn new(list: &ListState) -> Self {
        Self {
            list: list.clone(),
            rise: Rc::default(),
        }
    }

    /// Whether the list's rows fit its viewport, leaving it nowhere to
    /// scroll.
    fn fits(&self) -> bool {
        self.list.max_offset_for_scrollbar().y <= px(0.)
    }
}

impl ScrollbarHandle for FollowedList {
    fn viewport_bounds(&self) -> gpui::Bounds<Pixels> {
        ScrollbarHandle::viewport_bounds(&self.list)
    }

    fn offset(&self) -> gpui::Point<Pixels> {
        ScrollbarHandle::offset(&self.list)
    }

    fn set_offset(&self, mut offset: gpui::Point<Pixels>) {
        if self.fits() {
            return;
        }
        if !self.list.is_following_tail() {
            self.rise.set(px(0.));
            self.list.set_offset_from_scrollbar(offset);
            return;
        }
        // Anchored past its last row, the list reports more than it shows.
        let end = -self.list.max_offset_for_scrollbar().y;
        let rise = offset.y - self.list.scroll_px_offset_for_scrollbar().y.max(end);
        if rise > px(0.) {
            let rise = self.rise.get() + rise;
            if rise < TAIL_ESCAPE {
                self.rise.set(rise);
                return;
            }
            offset.y = end + rise;
        }
        self.rise.set(px(0.));
        self.list.set_offset_from_scrollbar(offset);
    }

    fn content_size(&self) -> gpui::Size<Pixels> {
        let mut size = ScrollbarHandle::content_size(&self.list);
        if self.fits() {
            size.height += FITTING_ROOM;
        }
        size
    }

    fn start_drag(&self) {
        ScrollbarHandle::start_drag(&self.list);
    }

    fn end_drag(&self) {
        ScrollbarHandle::end_drag(&self.list);
    }
}

#[derive(IntoElement)]
pub(crate) struct Scrollable<E: InteractiveElement + Styled + ParentElement + Element> {
    id: ElementId,
    element: E,
}

impl<E> Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    #[track_caller]
    fn new(element: E) -> Self {
        Self {
            id: caller_id(),
            element,
        }
    }
}

impl<E> Styled for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl<E> ParentElement for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.element.extend(elements);
    }
}

impl<E> InteractiveElement for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.element.interactivity()
    }
}

impl<E> RenderOnce for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element + 'static,
{
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let scroll_handle = window
            .use_keyed_state(self.id.clone(), cx, |_, _| ScrollHandle::default())
            .read(cx)
            .clone();
        let root_style = root_style_from(self.element.style());
        let content = self
            .element
            .id((self.id.clone(), "content"))
            .flex_none()
            .h_auto()
            .min_h_full();
        let scroll_area = div()
            .id((self.id.clone(), "area"))
            .size_full()
            .flex()
            .flex_col()
            .track_scroll(&scroll_handle)
            .overflow_y_scroll()
            .lock_scroll_axis()
            .child(content);
        let scroll_area =
            reveal::reveals((self.id.clone(), "reveals"), &scroll_handle, scroll_area);

        div()
            .id(self.id.clone())
            .size_full()
            .refine_style(&root_style)
            .relative()
            .child(scroll_area)
            .child(AreaMask::new(
                Axis::Vertical,
                &scroll_handle,
                (self.id.clone(), "mask"),
            ))
            .child(ScrollbarLayer {
                id: (self.id, "scrollbar").into(),
                scroll_handle: Rc::new(scroll_handle),
            })
    }
}

impl ScrollableElement for Div {}
impl<E> ScrollableElement for Stateful<E>
where
    E: ParentElement + Styled + Element,
    Self: InteractiveElement,
{
}

/// A masked viewport: the element keeps its id, styles and children and
/// becomes the scrolled viewport; a wrapper carries its sizing in the parent
/// layout and hosts the mask sibling.
#[derive(IntoElement)]
pub(crate) struct ScrollArea<E: InteractiveElement + Styled + ParentElement + Element> {
    id: ElementId,
    axis: Axis,
    element: E,
    handle: Option<ScrollHandle>,
    wheel_sideways: bool,
    edge_fades: Option<(Hsla, Pixels)>,
}

impl<E> ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    #[track_caller]
    fn new(element: E, axis: Axis) -> Self {
        let fallback = caller_id();
        Self {
            id: Element::id(&element).unwrap_or(fallback),
            axis,
            element,
            handle: None,
            wheel_sideways: false,
            edge_fades: None,
        }
    }

    /// A horizontal strip a mouse wheel moves too: its notches have the one axis, so
    /// they move the strip sideways while it has room that way and go on to the scroller
    /// around it at its end. For a strip that is a control of its own (a row of tabs),
    /// not content a page is read through.
    pub(crate) fn wheel_sideways(mut self) -> Self {
        self.wheel_sideways = true;
        self
    }

    /// Fade a horizontal strip's content into `surface`, the fill behind it, at each
    /// edge that has more past it; `radius` is the strip's corner radius, which the
    /// fade keeps. gpui-base has no scroll-edge cue.
    pub(crate) fn edge_fades(mut self, surface: Hsla, radius: Pixels) -> Self {
        self.edge_fades = Some((surface, radius));
        self
    }

    /// Scroll through `handle` instead of the area's own, for an owner that
    /// moves the area itself (e.g. to keep a keyboard selection visible).
    pub(crate) fn track_scroll(mut self, handle: &ScrollHandle) -> Self {
        self.handle = Some(handle.clone());
        self
    }
}

impl<E> Styled for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl<E> ParentElement for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.element.extend(elements);
    }
}

impl<E> InteractiveElement for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.element.interactivity()
    }
}

impl<E> RenderOnce for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element + 'static,
{
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let scroll_handle = self.handle.take().unwrap_or_else(|| {
            window
                .use_keyed_state((self.id.clone(), "scroll"), cx, |_, _| {
                    ScrollHandle::default()
                })
                .read(cx)
                .clone()
        });
        let root_style = root_style_from(self.element.style());
        // The wrapper takes the element's place in its parent's layout; the
        // viewport fills it unless a maximum height of its own bounds it.
        let bounded = self.element.style().max_size.height.is_some();
        let viewport = self
            .element
            .id(self.id.clone())
            .track_scroll(&scroll_handle);
        let viewport = match self.axis {
            // The mask moves the offset; the viewport only clips. GPUI's own
            // horizontal container would map vertical input onto this axis.
            Axis::Horizontal => viewport.overflow_hidden().into_any_element(),
            Axis::Vertical => reveal::reveals(
                (self.id.clone(), "reveals"),
                &scroll_handle,
                viewport
                    .when(!bounded, |viewport| viewport.max_h_full())
                    .overflow_y_scroll()
                    .lock_scroll_axis(),
            )
            .into_any_element(),
        };
        div()
            .relative()
            .refine_style(&root_style)
            .child(viewport)
            .children(
                self.edge_fades
                    .map(|(surface, radius)| edge_fades(scroll_handle.clone(), surface, radius)),
            )
            .child(
                AreaMask::new(self.axis, &scroll_handle, (self.id, "mask"))
                    .wheel_sideways(self.wheel_sideways),
            )
    }
}

/// How far a strip's content fades into its surface at an edge with more past it.
const EDGE_FADE: f32 = 24.;

/// The fades of [`ScrollArea::edge_fades`], over the viewport `handle` scrolls.
fn edge_fades(handle: ScrollHandle, surface: Hsla, radius: Pixels) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let (offset, max) = (handle.offset().x, handle.max_offset().x);
            let clear = surface.opacity(0.);
            let fade = px(EDGE_FADE).min(bounds.size.width / 2.);
            let none = px(0.);
            let edges = [
                (
                    offset < px(0.),
                    bounds.origin,
                    270.,
                    gpui::Corners {
                        top_left: radius,
                        bottom_left: radius,
                        top_right: none,
                        bottom_right: none,
                    },
                ),
                (
                    -offset < max,
                    gpui::point(bounds.right() - fade, bounds.top()),
                    90.,
                    gpui::Corners {
                        top_left: none,
                        bottom_left: none,
                        top_right: radius,
                        bottom_right: radius,
                    },
                ),
            ];
            for (shown, origin, angle, corners) in edges {
                if shown {
                    window.paint_quad(
                        gpui::fill(
                            Bounds::new(origin, gpui::size(fade, bounds.size.height)),
                            gpui::linear_gradient(
                                angle,
                                gpui::linear_color_stop(surface, 1.),
                                gpui::linear_color_stop(clear, 0.),
                            ),
                        )
                        .corner_radii(corners),
                    );
                }
            }
        },
    )
    .absolute()
    .inset_0()
}

/// Give `state`'s unmeasured rows `row_height` once the list has laid out, so
/// wheel, bounce and scrollbar extents include rows not yet in view. Place it
/// after the list: GPUI clears height hints on the first layout and on width
/// changes.
pub(crate) fn list_height_hint(state: &ListState, row_height: Pixels) -> impl IntoElement {
    let list = state.clone();
    canvas(
        move |_, _, _| {
            if list.is_scrolled_to_end().is_none() && list.max_offset_for_scrollbar().y > px(0.) {
                list.clone().with_uniform_item_height(row_height);
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

type RenderRows = Box<dyn Fn(Range<usize>, &mut Window, &mut App) -> Vec<AnyElement>>;
type RenderRow = Box<dyn FnMut(usize, &mut Window, &mut App) -> AnyElement>;

enum Rows {
    /// Every row is as tall as row `measure`, which also sets the width; rows
    /// never wrap.
    Uniform { render: RenderRows, measure: usize },
    /// Rows are measured as they come into view.
    Measured(RenderRow),
}

/// A bounded vertical viewport that lays out only the rows in view, for
/// lists that can grow to hundreds of rows. It scrolls like
/// [`ScrollableElement::overflow_y_scroll_area`], keeps its position while it
/// stays rendered, and without a definite height shrinks to its rows up to its
/// maximum height. Row gaps belong to the rows.
///
/// Gap: gpui-base's `VirtualList` takes every row's size before it lays the
/// rows out; measured rows here get theirs only as they come into view, which
/// GPUI's `list` does and gpui-base has no counterpart for.
#[derive(IntoElement)]
pub(crate) struct VirtualList {
    id: ElementId,
    count: usize,
    rows: Rows,
    style: StyleRefinement,
    scrollbar: bool,
    reveal: Option<usize>,
    keyed: Option<Keyed>,
    measure_all: bool,
    page: bool,
}

/// What a [`VirtualList::keyed`] list knows of its rows: each row's key, and
/// how to draw a row that left from its key.
struct Keyed {
    keys: Rc<[u64]>,
    duration: std::time::Duration,
    leaving: LeavingRow,
    /// Whether a change keeps the reader's place ([`VirtualList::keyed_live`]).
    live: bool,
}

type LeavingRow = Box<dyn FnMut(u64, &mut Window, &mut App) -> Option<AnyElement>>;

/// What the rows that left a [`VirtualList::keyed`] list were last drawn
/// from, by key, for drawing them again as they fold away: kept by the
/// list's owner, which hands it each frame's rows.
pub(crate) struct LeftRows<T> {
    drawn: std::collections::HashMap<u64, T>,
    left: std::collections::HashMap<u64, (T, Instant)>,
    duration: std::time::Duration,
}

impl<T: Clone> LeftRows<T> {
    /// Rows that fold away over `duration`, as the list's do.
    pub(crate) fn over(duration: std::time::Duration) -> Self {
        Self {
            drawn: Default::default(),
            left: Default::default(),
            duration,
        }
    }

    /// The list's rows this frame, by key: what one drawn last frame and no
    /// longer here showed is kept for as long as it can take to fold away.
    pub(crate) fn update(&mut self, rows: impl IntoIterator<Item = (u64, T)>, cx: &App) {
        let now = cx.background_executor().now();
        let drawn: std::collections::HashMap<u64, T> = rows.into_iter().collect();
        for (key, value) in self.drawn.drain() {
            if !drawn.contains_key(&key) {
                self.left.insert(key, (value, now));
            }
        }
        let keep = self.duration * 2;
        self.left.retain(|key, (_, at)| {
            !drawn.contains_key(key) && now.saturating_duration_since(*at) <= keep
        });
        self.drawn = drawn;
    }

    /// What row `key`, which left, was last drawn from.
    pub(crate) fn get(&self, key: u64) -> Option<&T> {
        self.left.get(&key).map(|(value, _)| value)
    }
}

/// A row key for a [`VirtualList::keyed`] list from what names the row.
pub(crate) fn row_key(name: impl std::hash::Hash) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// A keyed list's rows as last drawn, and their motion.
struct RowsMotion {
    keys: Option<Rc<[u64]>>,
    rows: crate::motion::list_rows::ListRows<(), u64>,
    flip: crate::motion::ListFlip,
}

impl VirtualList {
    pub(crate) fn uniform<R: IntoElement>(
        id: impl Into<ElementId>,
        count: usize,
        render: impl Fn(Range<usize>, &mut Window, &mut App) -> Vec<R> + 'static,
    ) -> Self {
        Self::new(
            id,
            count,
            Rows::Uniform {
                render: Box::new(move |range, window, cx| {
                    render(range, window, cx)
                        .into_iter()
                        .map(|row| div().whitespace_nowrap().child(row).into_any_element())
                        .collect()
                }),
                measure: 0,
            },
        )
    }

    pub(crate) fn measured<R: IntoElement>(
        id: impl Into<ElementId>,
        count: usize,
        mut render: impl FnMut(usize, &mut Window, &mut App) -> R + 'static,
    ) -> Self {
        Self::new(
            id,
            count,
            Rows::Measured(Box::new(move |index, window, cx| {
                render(index, window, cx).into_any_element()
            })),
        )
    }

    fn new(id: impl Into<ElementId>, count: usize, rows: Rows) -> Self {
        Self {
            id: id.into(),
            count,
            rows,
            style: StyleRefinement::default(),
            scrollbar: false,
            reveal: None,
            keyed: None,
            measure_all: false,
            page: false,
        }
    }

    /// Rows known by `keys`, one per row, which stay the same for as long as
    /// a row means the same thing: when they change (a filter, a re-sort),
    /// the rows that stayed glide to their new places, new ones fold in and
    /// the ones that went fold away, drawn by `leaving` from their keys, all
    /// over `duration`. Measured rows only.
    pub(crate) fn keyed(
        mut self,
        keys: impl Into<Rc<[u64]>>,
        duration: std::time::Duration,
        leaving: impl FnMut(u64, &mut Window, &mut App) -> Option<AnyElement> + 'static,
    ) -> Self {
        self.keyed = Some(Keyed {
            keys: keys.into(),
            duration,
            leaving: Box::new(leaving),
            live: false,
        });
        self
    }

    /// [`Self::keyed`] for rows that change under the reader rather than at
    /// the reader's request (agents that start and finish, not a filter
    /// typed): a change keeps the reader's place in the list, and the
    /// heights of the rows it did not touch, rather than starting the list
    /// again from its top.
    pub(crate) fn keyed_live(
        mut self,
        keys: impl Into<Rc<[u64]>>,
        duration: std::time::Duration,
        leaving: impl FnMut(u64, &mut Window, &mut App) -> Option<AnyElement> + 'static,
    ) -> Self {
        self = self.keyed(keys, duration, leaving);
        if let Some(keyed) = &mut self.keyed {
            keyed.live = true;
        }
        self
    }

    /// Lay every measured row out, once and again only when it changes,
    /// rather than only the rows in view: the list's height and scroll
    /// range are its rows' from the first frame, for a list laid out in a
    /// layout that shares its height out by what each part holds (the
    /// rail's cards) or that is as tall as what it holds (a sheet).
    /// Measured rows only.
    pub(crate) fn measure_all(mut self) -> Self {
        self.measure_all = true;
        self
    }

    /// A page-level list, which bounces at its edges as [`page_viewport`]
    /// does rather than handing what it cannot use to a scroller around it.
    /// Measured rows only.
    pub(crate) fn page(mut self) -> Self {
        self.page = true;
        self
    }

    /// Scroll row `index` into view whenever it changes, as a keyboard
    /// highlight moves.
    pub(crate) fn reveal(mut self, index: Option<usize>) -> Self {
        self.reveal = index;
        self
    }

    /// Size uniform rows, and a list without a definite width, by row `index`.
    pub(crate) fn width_from_row(mut self, index: usize) -> Self {
        if let Rows::Uniform { measure, .. } = &mut self.rows {
            *measure = index;
        }
        self
    }

    /// Overlay Tcode's scrollbar, as [`ScrollableElement::overflow_y_scrollbar`] does.
    pub(crate) fn scrollbar(mut self) -> Self {
        self.scrollbar = true;
        self
    }
}

impl Styled for VirtualList {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for VirtualList {
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let root_style = root_style_from(&self.style);
        let revealed = window.use_keyed_state((self.id.clone(), "revealed"), cx, |_, _| None);
        let mut last_revealed = *revealed.read(cx);
        let mut width_probe = None;
        // A viewport under a cap it shares with pinned siblings shrinks to
        // what they leave.
        let (viewport, mask, scrollbar) = match self.rows {
            Rows::Uniform { render, measure } => {
                let scroll = window
                    .use_keyed_state((self.id.clone(), "scroll"), cx, |_, _| {
                        UniformListScrollHandle::new()
                    })
                    .read(cx)
                    .clone();
                let viewport = uniform_list(self.id.clone(), self.count, render)
                    .with_sizing_behavior(ListSizingBehavior::Infer)
                    .with_width_from_item(Some(measure))
                    .track_scroll(&scroll)
                    .min_h_0()
                    .refine_style(&self.style);
                if let Some(index) = reveal_next(self.reveal, self.count, last_revealed) {
                    scroll.scroll_to_item(index, ScrollStrategy::Nearest);
                }
                let base = scroll.0.borrow().base_handle.clone();
                let (mask, scrollbar) = overlays(&self.id, &base, self.scrollbar);
                (viewport.into_any_element(), Some(mask), scrollbar)
            }
            Rows::Measured(mut render) => {
                let count = self.count;
                // A list that shrinks to its rows sizes itself from the rows
                // it measured; measuring a viewport past its last height lets
                // a longer list always reach its cap.
                let measure_all = self.measure_all;
                let state = window
                    .use_keyed_state((self.id.clone(), "list"), cx, |window, _| {
                        let state = ListState::new(
                            count,
                            ListAlignment::Top,
                            window.viewport_size().height,
                        );
                        if measure_all {
                            state.measure_all()
                        } else {
                            state
                        }
                    })
                    .read(cx)
                    .clone();
                let glide = window
                    .use_keyed_state((self.id.clone(), "glide"), cx, |_, _| ListScroll::default())
                    .read(cx)
                    .clone();
                // A keyed list notes where its rows were before it takes the
                // new ones, and lays every row out afresh, a live one every
                // row a change moved: an index may hold another row now,
                // whatever the count did.
                let motion = self.keyed.take().map(|keyed| {
                    let duration = keyed.duration;
                    let live = keyed.live;
                    let motion = window
                        .use_keyed_state((self.id.clone(), "rows"), cx, move |_, _| {
                            Rc::new(std::cell::RefCell::new(RowsMotion {
                                keys: None,
                                rows: crate::motion::list_rows::ListRows::over(duration),
                                flip: crate::motion::ListFlip::default(),
                            }))
                        })
                        .read(cx)
                        .clone();
                    let mut rows = motion.borrow_mut();
                    let new = keyed.keys.clone();
                    // A list that had no rows (still loading) is in place
                    // at once: there is nothing for its rows to glide from.
                    if let Some(old) = rows.keys.clone()
                        && old != new
                        && !old.is_empty()
                    {
                        rows.flip
                            .replace(&state, |index| old.get(index).copied(), &new, window);
                        rows.rows.replace(
                            &old,
                            &new,
                            |_, _| Some(()),
                            |index| Some(old[index]),
                            cx,
                        );
                        if live {
                            splice_rows(&state, &old, &new);
                        } else if state.item_count() == count {
                            state.reset(count);
                        }
                    }
                    rows.keys = Some(new.clone());
                    rows.rows.settle_list(&state, &new, window, cx);
                    drop(rows);
                    (motion, new, keyed.leaving)
                });
                if state.item_count() != count {
                    glide.cancel();
                    state.reset(count);
                    last_revealed = None;
                }
                if let Some(index) = reveal_next(self.reveal, self.count, last_revealed) {
                    let here = state.logical_scroll_top();
                    reveal_row(&state, index);
                    let to = state.logical_scroll_top();
                    if to != here {
                        state.scroll_to(here);
                        glide.scroll_to(&state, ScrollTarget::Row(to), cx);
                    }
                }
                glide.step(&state, window, cx);
                // GPUI sizes a list that shrinks to its rows by measuring them
                // at their widest, so a wrapping row would be measured as one
                // line. Rows keep the width the list last laid out at, and a
                // new width lays the list out again. The list places its rows
                // inside its padding, so a row is as wide as what the padding
                // leaves.
                let bounds = state.viewport_bounds();
                let mut style = gpui::Style::default();
                style.refine(&self.style);
                let padding = style
                    .padding
                    .to_pixels(bounds.size.into(), window.rem_size());
                let width = bounds.size.width;
                let row_width = (width > px(0.)).then(|| width - padding.left - padding.right);
                let mut motion = motion;
                let render = move |index, window: &mut Window, cx: &mut App| {
                    let row = render(index, window, cx);
                    let row = match &mut motion {
                        Some((motion, keys, leaving)) => match keys.get(index).copied() {
                            Some(key) => {
                                let around = motion.borrow().rows.around(key, cx);
                                let mut left: std::collections::HashMap<u64, AnyElement> = around
                                    .above
                                    .iter()
                                    .chain(&around.below)
                                    .filter_map(|(key, _, _)| {
                                        leaving(*key, window, cx).map(|row| (*key, row))
                                    })
                                    .collect();
                                let row = around.draw(
                                    "virtual-row",
                                    key,
                                    crate::motion::list_rows::Glide::Rest,
                                    row,
                                    |key, _| {
                                        left.remove(key)
                                            .unwrap_or_else(|| gpui::Empty.into_any_element())
                                    },
                                    window,
                                );
                                motion.borrow().flip.row(index, ("virtual-row", key), row)
                            }
                            None => row,
                        },
                        None => row,
                    };
                    div()
                        .when_some(row_width, |cell, width| cell.w(width))
                        .child(row)
                        .into_any_element()
                };
                // GPUI's `list` sizing itself to its rows lays out the rows
                // in view on every layout, twice a frame, measured or not; a
                // list that has measured them all takes their height
                // instead, which `ListState` does not report but places its
                // last row by.
                let rows = self.measure_all.then(|| rows_height(&state));
                width_probe = Some(width_probe_for(state.clone(), width, rows));
                let rows = rows.flatten();
                let viewport = list(state.clone(), render)
                    .with_sizing_behavior(if rows.is_some() {
                        ListSizingBehavior::Auto
                    } else {
                        ListSizingBehavior::Infer
                    })
                    .min_h_0()
                    .refine_style(&self.style)
                    .when_some(rows, |list, rows| {
                        list.h(rows + padding.top + padding.bottom)
                    });
                let (mask, scrollbar) = overlays(&self.id, &state, self.scrollbar);
                if self.page {
                    let viewport = page_viewport((self.id.clone(), "bounce"), &state, viewport)
                        .into_any_element();
                    (viewport, None, scrollbar)
                } else {
                    (viewport.into_any_element(), Some(mask), scrollbar)
                }
            }
        };
        let reveal = self.reveal.or(last_revealed);
        revealed.update(cx, |revealed, _| *revealed = reveal);
        div()
            .id((self.id, "area"))
            .relative()
            .flex()
            .flex_col()
            .refine_style(&root_style)
            .child(viewport)
            .children(width_probe)
            .children(mask)
            .children(scrollbar)
    }
}

/// How tall `list`'s rows are together, once it has measured the last of
/// them where it last laid out.
fn rows_height(list: &ListState) -> Option<Pixels> {
    let last = list.bounds_for_item(list.item_count().checked_sub(1)?)?;
    Some(last.bottom() - list.viewport_bounds().top() - list.scroll_px_offset_for_scrollbar().y)
}

/// Replaces the rows of `list` keyed `old` with the rows keyed `new`, keeping
/// the reader's place and what was measured of every row that stayed: the
/// rows that went are taken out and those that came put in where they stand,
/// so the list keeps its scroll anchor and its height as gpui keeps them
/// through a splice. Rows that stayed but changed places are measured again,
/// each from the height its place had, which a reorder leaves summing to the
/// list's height as it was.
fn splice_rows(list: &ListState, old: &[u64], new: &[u64]) {
    use std::collections::HashSet;
    let stays: HashSet<u64> = new.iter().copied().collect();
    let stood: HashSet<u64> = old.iter().copied().collect();
    for index in (0..old.len()).rev() {
        if !stays.contains(&old[index]) {
            list.splice(index..index + 1, 0);
        }
    }
    for (index, key) in new.iter().enumerate() {
        if !stood.contains(key) {
            list.splice(index..index, 1);
        }
    }
    let kept = |keys: &[u64], of: &HashSet<u64>| -> Vec<u64> {
        keys.iter()
            .copied()
            .filter(|key| of.contains(key))
            .collect()
    };
    if kept(old, &stays) != kept(new, &stood) {
        list.remeasure();
    }
}

/// The row a list should scroll to: one requested, in range and not already
/// revealed.
fn reveal_next(request: Option<usize>, count: usize, last: Option<usize>) -> Option<usize> {
    request.filter(|index| *index < count && last != Some(*index))
}

/// Scroll a measured list so row `index` is fully in view. A row not yet
/// measured below the top has no known position, so it moves to the top.
fn reveal_row(state: &ListState, index: usize) {
    let viewport = state.viewport_bounds();
    match state.bounds_for_item(index) {
        Some(row) if row.top() >= viewport.top() && row.bottom() <= viewport.bottom() => {}
        None if index > state.logical_scroll_top().item_ix => state.scroll_to(ListOffset {
            item_ix: index,
            offset_in_item: px(0.),
        }),
        _ => state.scroll_to_reveal_item(index),
    }
}

/// Renders the list's view again once the list lays out at a width other than
/// `width`, the one its rows were given, or, for a list that measures all its
/// rows, once it has measured them to a height other than `rows`, the one it
/// was given: the list measures a row that came in or settled only as it is
/// laid out, after a layout around it has read the list's height.
fn width_probe_for(state: ListState, width: Pixels, rows: Option<Option<Pixels>>) -> AnyElement {
    canvas(
        move |_, window, _| {
            if state.viewport_bounds().size.width != width
                || rows.is_some_and(|rows| rows != rows_height(&state))
            {
                window.request_animation_frame();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .into_any_element()
}

/// The mask that routes wheel input to a virtual list, and its scrollbar.
fn overlays<H: ScrollbarHandle + Clone>(
    id: &ElementId,
    handle: &H,
    scrollbar: bool,
) -> (AnyElement, Option<AnyElement>) {
    let mask = AreaMask::new(Axis::Vertical, handle, (id.clone(), "mask")).into_any_element();
    let scrollbar = scrollbar.then(|| {
        ScrollbarLayer {
            id: (id.clone(), "scrollbar").into(),
            scroll_handle: Rc::new(handle.clone()),
        }
        .into_any_element()
    });
    (mask, scrollbar)
}

#[derive(IntoElement)]
struct ScrollbarLayer<H: ScrollbarHandle + Clone> {
    id: ElementId,
    scroll_handle: Rc<H>,
}

impl<H> RenderOnce for ScrollbarLayer<H>
where
    H: ScrollbarHandle + Clone + 'static,
{
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        if window.is_inspector_picking(cx) {
            return div();
        }
        div().absolute().inset_0().child(
            Scrollbar::vertical(self.scroll_handle.as_ref())
                .id(self.id)
                .viewport_from_layout(),
        )
    }
}

#[track_caller]
fn caller_id() -> ElementId {
    ElementId::CodeLocation(*Location::caller())
}

fn root_style_from(style: &StyleRefinement) -> StyleRefinement {
    StyleRefinement {
        size: style.size.clone(),
        min_size: style.min_size.clone(),
        max_size: style.max_size.clone(),
        flex_grow: style.flex_grow,
        flex_shrink: style.flex_shrink,
        flex_basis: style.flex_basis,
        align_self: style.align_self,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, TestAppContext, TouchPhase, VisualTestContext, point, px};

    /// A page with a 100 px nested area `before` px down its content.
    struct Nested {
        page: ScrollHandle,
        area: ScrollHandle,
        before: Pixels,
    }

    impl Render for Nested {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(200.))
                .h(px(300.))
                .flex()
                .flex_col()
                .child(gesture_recorder())
                .child(page_viewport(
                    "nested-page-bounce",
                    &self.page,
                    div()
                        .id("nested-page")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .lock_scroll_axis()
                        .track_scroll(&self.page)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .debug_selector(|| "nested-page-top".into())
                                        .h(self.before)
                                        .flex_none(),
                                )
                                .child(
                                    div()
                                        .id("nested-area")
                                        .h(px(100.))
                                        .flex_none()
                                        .overflow_y_scroll_area()
                                        .track_scroll(&self.area)
                                        .child(div().h(px(400.)).flex_none()),
                                )
                                .child(div().h(px(1000.)).flex_none()),
                        ),
                ))
        }
    }

    fn nested(
        cx: &mut TestAppContext,
        before: f32,
    ) -> (gpui::Entity<Nested>, &mut VisualTestContext) {
        cx.update(crate::test_client::init);
        let (view, cx) = cx.add_window_view(|_, _| Nested {
            page: ScrollHandle::new(),
            area: ScrollHandle::new(),
            before: px(before),
        });
        crate::test_frames::draw_settled(cx);
        (view, cx)
    }

    /// One packet of a pan or of its momentum, a display frame apart: GPUI
    /// delivers every one at the point the finger first touched.
    fn packet(cx: &mut VisualTestContext, dy: f32, phase: TouchPhase) {
        cx.simulate_event(gpui::ScrollEvent {
            position: point(px(100.), px(150.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(dy))),
            touch_phase: phase,
            source: gpui::ScrollSource::Touch,
            ..Default::default()
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(16));
        crate::test_frames::draw(cx);
    }

    /// A flick: a pan, its release and the momentum after it.
    fn flick(cx: &mut VisualTestContext, dy: f32, steps: usize) {
        packet(cx, dy, TouchPhase::Started);
        for _ in 0..steps {
            packet(cx, dy, TouchPhase::Moved);
        }
        packet(cx, dy, TouchPhase::Ended);
        for _ in 0..steps {
            packet(cx, dy, TouchPhase::Moved);
        }
        packet(cx, 0., TouchPhase::Ended);
    }

    fn offsets(view: &gpui::Entity<Nested>, cx: &mut VisualTestContext) -> (Pixels, Pixels) {
        view.read_with(cx, |view, _| (view.page.offset().y, view.area.offset().y))
    }

    /// A gesture belongs to the area that first moved for it until its
    /// momentum runs out: a nested area the page scrolls under the finger
    /// does not take the rest, and a nested area at its end does not hand
    /// the rest to the page.
    #[gpui::test]
    fn a_scroll_gesture_stays_with_the_area_that_first_moved_for_it(cx: &mut TestAppContext) {
        // The flick begins on the page, above the area; the area reaches the
        // finger after the first 100 px.
        let (view, cx) = nested(cx, 250.);
        flick(cx, -30., 10);
        let (page, area) = offsets(&view, cx);
        assert_eq!(
            area,
            px(0.),
            "the area that slid under the finger stays put"
        );
        assert!(page < px(-300.), "the page takes the whole flick: {page:?}");

        // The flick begins in the area, which has 300 px to give.
        let (view, cx) = nested(cx, 100.);
        flick(cx, -40., 10);
        let (page, area) = offsets(&view, cx);
        assert_eq!(area, px(-300.), "the area runs to its end");
        assert_eq!(page, px(0.), "and keeps the rest of its flick there");
    }

    /// A page that begins with a 100 px nested area, over a button that
    /// counts its taps.
    struct Tapped {
        page: ScrollHandle,
        area: ScrollHandle,
        taps: usize,
    }

    impl Render for Tapped {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(200.))
                .h(px(300.))
                .flex()
                .flex_col()
                .child(gesture_recorder())
                .child(page_viewport(
                    "tapped-page-bounce",
                    &self.page,
                    div()
                        .id("tapped-page")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .lock_scroll_axis()
                        .track_scroll(&self.page)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .id("tapped-area")
                                        .debug_selector(|| "tapped-area".into())
                                        .h(px(100.))
                                        .flex_none()
                                        .overflow_y_scroll_area()
                                        .track_scroll(&self.area)
                                        .child(div().h(px(400.)).flex_none()),
                                )
                                .child(
                                    div()
                                        .debug_selector(|| "tapped-rest".into())
                                        .h(px(1000.))
                                        .flex_none(),
                                ),
                        ),
                ))
                .child(
                    div()
                        .id("tapped-button")
                        .debug_selector(|| "tapped-button".into())
                        .h(px(60.))
                        .flex_none()
                        .on_click(cx.listener(|this, _, _, _| this.taps += 1)),
                )
        }
    }

    /// A flick that runs into the end of the area it belongs to is over
    /// there: the next touch is a tap, not spent on catching momentum
    /// nothing moves for.
    #[gpui::test]
    fn a_fling_ends_where_it_runs_into_the_end_of_its_area(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
        // The page is at its top and the finger flicks down; the nested area
        // is flicked up past all it has to give.
        for (from, travel) in [("tapped-rest", 100.), ("tapped-area", -400.)] {
            let (view, cx) = cx.add_window_view(|_, _| Tapped {
                page: ScrollHandle::new(),
                area: ScrollHandle::new(),
                taps: 0,
            });
            crate::test_frames::draw_settled(cx);
            let touch = |cx: &mut VisualTestContext, phase, position| {
                cx.simulate_event(gpui::TouchEvent {
                    id: gpui::TouchId(1),
                    phase,
                    position,
                    predicted_position: None,
                    force: None,
                });
                crate::test_frames::draw(cx);
            };
            let start = cx.debug_bounds(from).unwrap().origin + point(px(100.), px(20.));
            let end = start + point(px(0.), px(travel));
            touch(cx, TouchPhase::Started, start);
            touch(
                cx,
                TouchPhase::Moved,
                start + point(px(0.), px(travel / 5.)),
            );
            touch(cx, TouchPhase::Moved, end);
            touch(cx, TouchPhase::Ended, end);
            // The frame after the release brings the fling's first packet.
            cx.update(|window, cx| window.simulate_next_frame(cx));

            let button = cx.debug_bounds("tapped-button").unwrap().center();
            touch(cx, TouchPhase::Started, button);
            touch(cx, TouchPhase::Ended, button);
            assert_eq!(view.read_with(cx, |view, _| view.taps), 1, "{from}");
        }
    }

    /// A page whose content begins with a 100 px wide strip of 400 px.
    struct Strip {
        page: ScrollHandle,
        strip: ScrollHandle,
        wheel_sideways: bool,
    }

    impl Render for Strip {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let strip = div()
                .id("strip")
                .flex()
                .flex_none()
                .w(px(100.))
                .h(px(40.))
                .overflow_x_scroll_area()
                .track_scroll(&self.strip)
                .child(div().w(px(400.)).h_full().flex_none());
            let strip = if self.wheel_sideways {
                strip.wheel_sideways()
            } else {
                strip
            };
            div()
                .w(px(200.))
                .h(px(300.))
                .flex()
                .flex_col()
                .child(gesture_recorder())
                .child(page_viewport(
                    "strip-page-bounce",
                    &self.page,
                    div()
                        .id("strip-page")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .lock_scroll_axis()
                        .track_scroll(&self.page)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(strip)
                                .child(div().h(px(1000.)).flex_none()),
                        ),
                ))
        }
    }

    fn strip(
        cx: &mut TestAppContext,
        wheel_sideways: bool,
    ) -> (gpui::Entity<Strip>, &mut VisualTestContext) {
        cx.update(crate::test_client::init);
        let (view, cx) = cx.add_window_view(|_, _| Strip {
            page: ScrollHandle::new(),
            strip: ScrollHandle::new(),
            wheel_sideways,
        });
        crate::test_frames::draw_settled(cx);
        (view, cx)
    }

    /// A mouse wheel has one axis. Over a strip that is a control of its own (a row of
    /// tabs) its notches move the strip sideways, and go on to the page once the strip is
    /// at its end; over any other strip they move the page, which is read through it.
    #[gpui::test]
    fn a_wheel_moves_a_strip_sideways_only_where_the_strip_asks(cx: &mut TestAppContext) {
        let notch = |cx: &mut VisualTestContext| {
            cx.simulate_event(gpui::ScrollEvent {
                position: point(px(50.), px(20.)),
                delta: gpui::ScrollDelta::Lines(point(0., -1.)),
                ..Default::default()
            });
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            crate::test_frames::draw(cx);
        };
        let offsets = |view: &gpui::Entity<Strip>, cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| (view.page.offset().y, view.strip.offset().x))
        };

        let (view, cx) = strip(cx, true);
        notch(cx);
        let (page, first) = offsets(&view, cx);
        assert!(first < px(0.), "the strip moves sideways: {first:?}");
        assert_eq!(page, px(0.), "and the page stays");
        for _ in 0..100 {
            notch(cx);
            if offsets(&view, cx).0 < px(0.) {
                break;
            }
        }
        let (page, strip_offset) = offsets(&view, cx);
        assert_eq!(strip_offset, px(-300.), "the strip ran to its end");
        assert!(page < px(0.), "where a notch went on to the page: {page:?}");

        let (view, cx) = strip(cx, false);
        notch(cx);
        let (page, strip_offset) = offsets(&view, cx);
        assert_eq!(strip_offset, px(0.), "a strip that did not ask stays");
        assert!(page < px(0.), "and the page moves under it: {page:?}");
    }

    /// A pull past the page's top over an area at its own top stretches the
    /// page; pushing back up in the same gesture is still the page's, so the
    /// area does not take its release and the stretch lets go. Only where
    /// gpui-base's bounce stretches a page at all.
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    #[gpui::test]
    fn a_stretched_page_keeps_its_gesture_to_the_release(cx: &mut TestAppContext) {
        let (view, cx) = nested(cx, 100.);
        let top = cx.debug_bounds("nested-page-top").unwrap().top();
        packet(cx, 10., TouchPhase::Started);
        packet(cx, 40., TouchPhase::Moved);
        packet(cx, 40., TouchPhase::Moved);
        assert!(cx.debug_bounds("nested-page-top").unwrap().top() > top);
        packet(cx, -20., TouchPhase::Moved);
        packet(cx, -10., TouchPhase::Ended);
        crate::test_frames::draw_settled(cx);
        assert_eq!(offsets(&view, cx).1, px(0.), "the area is left alone");
        assert_eq!(
            cx.debug_bounds("nested-page-top").unwrap().top(),
            top,
            "the page's edge is back where it rests"
        );
    }

    /// A live list of 20 px rows, keyed by what each shows, in a 100 px
    /// viewport.
    struct Live(Vec<u64>);

    impl Render for Live {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let keys = self.0.clone();
            let row = |key: u64| {
                div()
                    .debug_selector(move || format!("live-{key}"))
                    .h(px(20.))
                    .flex_none()
                    .into_any_element()
            };
            div().w(px(200.)).h(px(600.)).child(
                div().debug_selector(|| "live".into()).child(
                    VirtualList::measured("live-list", keys.len(), move |index, _, _| {
                        row(keys[index])
                    })
                    .keyed_live(self.0.clone(), crate::motion::KEYSTROKE, |_, _, _| None)
                    .measure_all()
                    .w_full()
                    .max_h(px(100.)),
                ),
            )
        }
    }

    /// Rows that come and go under a reader scrolled down a live list, above
    /// and below the rows in view, leave those rows where they are: the
    /// list does not start again from its top.
    #[gpui::test]
    fn a_live_list_keeps_the_readers_place_as_its_rows_change(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
        let (view, cx) = cx.add_window_view(|_, _| Live((0..40).collect()));
        crate::test_frames::draw_settled(cx);
        let viewport = cx.debug_bounds("live").unwrap();
        cx.simulate_event(gpui::ScrollEvent {
            position: viewport.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-300.))),
            ..Default::default()
        });
        crate::test_frames::draw_settled(cx);
        let read = cx.debug_bounds("live-16").expect("the row read is in view");
        assert!(
            viewport.contains(&read.center()),
            "{read:?} in {viewport:?}"
        );

        view.update(cx, |view, cx| {
            // Two rows go and one comes above the rows in view, one comes
            // below them.
            view.0.retain(|key| ![3, 7].contains(key));
            view.0.insert(0, 100);
            view.0.insert(30, 101);
            cx.notify();
        });
        crate::test_frames::draw_settled(cx);
        let now = cx
            .debug_bounds("live-16")
            .expect("the row read is still drawn");
        assert!(
            viewport.contains(&now.center()),
            "the row read stays in view: {now:?} in {viewport:?}"
        );
    }
}
