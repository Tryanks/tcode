//! Mermaid fences render as diagrams.
//!
//! A pure-Rust engine lays a fence's source out to SVG, and the picture is
//! rasterized, both on the background executor: a diagram is never laid out
//! on the main thread, and never shown before its pixels and its size exist.
//! A document opened with a whole fence shows its code until the picture is
//! drawn, at the diagram's height when that is known, then the picture at
//! once; the chat holds a thread unseen until its first pictures are drawn.
//!
//! A streaming fence's source flips between parsing and not on almost every
//! token, so each fence has a [`Slot`] that keeps what is shown moving one way.
//! Only whole statements are tried, one render at a time, the newest winning.
//! A source that does not parse while it streams changes nothing on screen.
//! A newer picture replaces the shown one at most once per [`swap_interval`],
//! fading across while the block's height glides to the new one. The whole
//! source is tried as soon as the fence closes or the message ends, and a
//! final source that does not parse ends as its code with the engine's message.
//!
//! gpui-base draws no diagrams and the engine has no streaming or incremental
//! mode. GPUI's `img` draws an `Image` only once its asynchronous decode has
//! finished, knows no size until then, and keeps every image it decoded for
//! good; so the picture is rasterized here and handed to `img` decoded, and a
//! picture no longer shown leaves the sprite atlas.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    panic::AssertUnwindSafe,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Context, Element, ElementId, Entity, GlobalElementId,
    InspectorElementId, InteractiveElement as _, IntoElement, LayoutId, MouseButton, ObjectFit,
    ParentElement as _, Pixels, RenderImage, Role, SharedString, StatefulInteractiveElement as _,
    Style, Styled, StyledImage as _, SvgRenderer, Task, Window, div, img,
    prelude::FluentBuilder as _, px, relative, rems, size,
};
use gpui_base::h_flex;
use mermaid_rs_renderer::{
    LayoutConfig, Theme, compute_layout, measure_svg_dimensions, parse_mermaid_strict, render_svg,
};

use super::{
    fitted_image::{MAX_HEIGHT, fit},
    nodes::{BlockNode, CodeBlock},
    state::{MarkdownState, PendingContextTarget},
};
use crate::{
    icon::IconName,
    motion::{CrossFade, GlideState, SizeGlide},
    recent_cache::RecentCache,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::copy::{action_button, copy_button},
};

pub(super) fn is_mermaid(lang: Option<&str>) -> bool {
    lang.is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
}

/// How long a new picture rests before the next may replace it. A growing
/// diagram that fades from one layout straight into the next reads as
/// flicker; one fade at a time, each followed by a beat at rest, reads as
/// the diagram growing.
const REST: Duration = Duration::from_millis(150);

/// How long one picture takes to fade into the next.
const SWAP: Duration = Duration::from_millis(200);

/// The least time between two pictures replacing each other while a fence
/// streams in: one fade, then [`REST`].
fn swap_interval() -> Duration {
    SWAP + REST
}

/// The most pixels a diagram is rasterized with, so a huge one does not
/// take hundreds of megabytes; it is shown far smaller than that anyway.
const MAX_PIXELS: f32 = 16_000_000.;

/// The scale a picture of `natural` size is rasterized at: twice its own
/// size, as GPUI rasterizes SVG, but no more than twice the size it can be
/// shown at, never taller than [`MAX_HEIGHT`], and within [`MAX_PIXELS`].
fn raster_scale(natural: gpui::Size<f32>) -> f32 {
    if natural.width <= 0. || natural.height <= 0. {
        return gpui::SMOOTH_SVG_SCALE_FACTOR;
    }
    let mut scale = gpui::SMOOTH_SVG_SCALE_FACTOR * (MAX_HEIGHT.as_f32() / natural.height).min(1.);
    let pixels = natural.width * natural.height * scale * scale;
    if pixels > MAX_PIXELS {
        scale *= (MAX_PIXELS / pixels).sqrt();
    }
    scale
}

/// What the engine made of a source, off the main thread.
enum Raster {
    Drawn(Arc<RenderImage>, gpui::Size<Pixels>),
    /// The source parses, but has nothing to draw yet: a header without a
    /// node or an edge.
    Empty,
    /// The source does not parse; the engine's message, when it gave one.
    Failed(Option<String>),
}

fn draw(source: &str, dark: bool, whole: bool, renderer: &SvgRenderer) -> Raster {
    let mut theme = if dark { Theme::dark() } else { Theme::modern() };
    // The fence paints the surface; the engine's own page color would sit as a
    // slab of another shade inside it.
    theme.background = "transparent".to_string();
    let config = LayoutConfig::default();
    // Every statement of a streaming fence reaches the engine as a new partial
    // source; a panic on one of them must not take the window down.
    let laid_out = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let parsed = parse_mermaid_strict(source)?;
        let layout = compute_layout(&parsed.graph, &theme, &config);
        // A pie or a gantt chart keeps its data outside nodes and edges, so
        // one shows only once its fence is whole.
        if !whole && layout.nodes.is_empty() && layout.edges.is_empty() {
            return Ok(None);
        }
        let dimensions = measure_svg_dimensions(&layout, &config, None);
        Ok(Some((
            render_svg(&layout, &theme, &config),
            gpui::size(dimensions.width, dimensions.height),
        )))
    }));
    let (svg, natural) = match laid_out {
        Ok(Ok(Some(laid_out))) => laid_out,
        Ok(Ok(None)) => return Raster::Empty,
        Ok(Err(error)) => {
            let error: mermaid_rs_renderer::ParseError = error;
            return Raster::Failed(Some(error.to_string()));
        }
        Err(_) => return Raster::Failed(None),
    };
    let scale = raster_scale(natural);
    match renderer.render_single_frame(svg.as_bytes(), scale / gpui::SMOOTH_SVG_SCALE_FACTOR) {
        Ok(image) => {
            let device = image.size(0);
            let size = gpui::size(
                px(device.width.0 as f32 / scale),
                px(device.height.0 as f32 / scale),
            );
            Raster::Drawn(image, size)
        }
        Err(_) => Raster::Failed(None),
    }
}

thread_local! {
    static FINALS: RefCell<Finals> = RefCell::new(Finals::default());
}

/// A rasterized diagram.
#[derive(Clone)]
pub(super) struct Picture(Rc<PictureData>);

struct PictureData {
    image: crate::store::images::Owned,
    size: gpui::Size<Pixels>,
}

impl Picture {
    fn new(image: Arc<RenderImage>, size: gpui::Size<Pixels>, cx: &mut App) -> Self {
        Self(Rc::new(PictureData {
            image: crate::store::images::Owned::new(image, cx),
            size,
        }))
    }

    fn size(&self) -> gpui::Size<Pixels> {
        self.0.size
    }

    fn bytes(&self) -> usize {
        let device = self.0.image.image().size(0);
        device.width.0.max(0) as usize * device.height.0.max(0) as usize * 4
    }
}

#[derive(Clone)]
enum Outcome {
    Drawn(Picture),
    Empty,
    Failed(Option<SharedString>),
}

impl Outcome {
    fn new(raster: Raster, cx: &mut App) -> Self {
        match raster {
            Raster::Drawn(image, size) => Self::Drawn(Picture::new(image, size, cx)),
            Raster::Empty => Self::Empty,
            Raster::Failed(message) => Self::Failed(message.map(Into::into)),
        }
    }
}

/// The most recent whole fences drawn, for a document rebuilt or opened
/// again and for a theme switched back; and the sizes of many more, which a
/// fence drawn again holds while its picture is on its way.
struct Finals {
    entries: VecDeque<((SharedString, bool), Outcome)>,
    bytes: usize,
    sizes: RecentCache<(SharedString, bool), gpui::Size<Pixels>>,
}

impl Default for Finals {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            sizes: RecentCache::new(FINAL_SIZES_CAPACITY),
        }
    }
}

const FINALS_CAPACITY: usize = 32;
const FINALS_BYTES: usize = 64 * 1024 * 1024;
const FINAL_SIZES_CAPACITY: usize = 512;

impl Finals {
    fn get(&mut self, key: &(SharedString, bool)) -> Option<Outcome> {
        let ix = self.entries.iter().position(|(entry, _)| entry == key)?;
        let entry = self.entries.remove(ix)?;
        let outcome = entry.1.clone();
        self.entries.push_back(entry);
        Some(outcome)
    }

    fn insert(&mut self, key: (SharedString, bool), outcome: Outcome) {
        if let Outcome::Drawn(picture) = &outcome {
            self.sizes
                .get_or_insert_with(key.clone(), || picture.size());
        }
        if let Some(ix) = self.entries.iter().position(|(entry, _)| *entry == key)
            && let Some((_, old)) = self.entries.remove(ix)
        {
            self.bytes -= weight(&old);
        }
        self.bytes += weight(&outcome);
        self.entries.push_back((key, outcome));
        while self.entries.len() > 1
            && (self.entries.len() > FINALS_CAPACITY || self.bytes > FINALS_BYTES)
        {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= weight(&old);
            }
        }
    }
}

fn weight(outcome: &Outcome) -> usize {
    match outcome {
        Outcome::Drawn(picture) => picture.bytes(),
        Outcome::Empty | Outcome::Failed(_) => 0,
    }
}

/// Which of a fence's two faces is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layer {
    Source,
    Diagram,
}

/// A picture replacing what the block showed, under way since `at`.
#[derive(Clone, Copy)]
struct Swap {
    at: Instant,
    /// The block's height where it was painted when the swap began.
    from: Option<gpui::Size<Pixels>>,
    from_layer: Layer,
}

#[derive(Clone, PartialEq)]
struct Attempt {
    source: SharedString,
    dark: bool,
    /// The fence's whole source, which is shown whatever it holds.
    whole: bool,
}

struct Shown {
    picture: Picture,
    source: SharedString,
    dark: bool,
}

/// A result waiting to be shown.
struct Ready {
    attempt: Attempt,
    /// The picture, or for a whole source, the engine's message.
    drawn: Result<Picture, Option<SharedString>>,
    /// It came of text that appears at once, or as one document.
    snap: bool,
}

/// One Mermaid fence: what it shows, and the renders on their way.
#[derive(Default)]
pub(super) struct Slot {
    shown: Option<Shown>,
    /// The picture the shown one replaced, while it fades out.
    leaving: Option<Picture>,
    /// The whole source's message, when it does not parse.
    failed: Option<Option<SharedString>>,
    generation: u64,
    swap: Option<Swap>,
    last_swap: Option<Instant>,
    ready: Option<Ready>,
    /// The newest source tried, in flight or done.
    tried: Option<Attempt>,
    in_flight: Option<Task<()>>,
    /// When the next picture may be shown, and the task that wakes the
    /// document then.
    timer: Option<(Instant, Task<()>)>,
    /// The fence has been laid out before.
    built: bool,
    /// The first render of the whole fence, which appears at once, is on
    /// its way.
    opening: bool,
    /// The size of the diagram on its way, when it was drawn before.
    reserve: Option<gpui::Size<Pixels>>,
    glide: GlideState,
}

impl Slot {
    fn layer(&self, showing_source: bool) -> Layer {
        if self.shown.is_some() && self.failed.is_none() && !showing_source {
            Layer::Diagram
        } else {
            Layer::Source
        }
    }

    /// End the swap under way: what it replaced is gone.
    fn finish_swap(&mut self) {
        self.swap = None;
        self.leaving = None;
        if self.failed.is_some() {
            self.shown = None;
        }
    }

    fn accept(&mut self, attempt: Attempt, outcome: Outcome, snap: bool) {
        let drawn = match outcome {
            Outcome::Drawn(picture) => {
                let newer = attempt.whole
                    || self.shown.as_ref().is_none_or(|shown| {
                        shown.dark != attempt.dark || shown.source.len() < attempt.source.len()
                    });
                if !newer {
                    return;
                }
                Ok(picture)
            }
            Outcome::Failed(message) if attempt.whole => Err(message),
            // While the fence streams, a statement that does not parse yet,
            // or a header with nothing under it, leaves what is shown as it is.
            Outcome::Empty | Outcome::Failed(_) => return,
        };
        self.ready = Some(Ready {
            attempt,
            drawn,
            snap,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn sync(
        &mut self,
        path: &str,
        code: &CodeBlock,
        env: &Env<'_>,
        frame: &mut Frame,
        cx: &mut Context<MarkdownState>,
    ) {
        let source = code.prepared().text.clone();
        let showing_source = env.sources.contains(path);
        // An edit that is not an append starts the fence over. A source tried
        // whole may end with the newline the parser gives a line still being
        // written.
        if self.tried.as_ref().is_some_and(|tried| {
            !source.starts_with(tried.source.strip_suffix('\n').unwrap_or(&tried.source))
        }) {
            *self = Slot {
                built: self.built,
                glide: self.glide.clone(),
                ..Slot::default()
            };
            frame.remeasure.push(path.to_string());
        }

        if let Some(swap) = self.swap {
            frame.remeasure.push(path.to_string());
            if env.now.saturating_duration_since(swap.at) >= SWAP {
                self.finish_swap();
            } else {
                frame.moving = true;
            }
        }

        let whole = code.closed || !env.streaming;
        let candidate = if whole {
            Some(source.clone())
        } else if code.open_line {
            complete_statements(source.strip_suffix('\n').unwrap_or(&source)).map(SharedString::new)
        } else {
            Some(source.clone()).filter(|source| !source.is_empty())
        };
        if let Some(candidate) = candidate {
            let attempt = Attempt {
                source: candidate,
                dark: env.dark,
                whole,
            };
            if self.tried.as_ref() != Some(&attempt) {
                let key = (attempt.source.clone(), attempt.dark);
                // A whole fence drawn before is shown at once.
                let at_once = whole
                    .then(|| FINALS.with(|finals| finals.borrow_mut().get(&key)))
                    .flatten();
                match at_once {
                    Some(outcome) => {
                        self.in_flight = None;
                        self.tried = Some(attempt.clone());
                        self.accept(attempt, outcome, env.burst);
                    }
                    None if self.in_flight.is_none() => {
                        // A fence whole when the document opens, as from
                        // history, was not watched arriving: its picture
                        // appears without motion once it is drawn.
                        let opening = whole && !self.built;
                        if opening {
                            self.opening = true;
                            self.reserve =
                                FINALS.with(|finals| finals.borrow().sizes.get(&key).copied());
                        }
                        self.tried = Some(attempt.clone());
                        self.in_flight = Some(spawn(path, attempt, env.burst || opening, cx));
                    }
                    // The render in flight hands over to the newest source
                    // when it is done.
                    None => {}
                }
            }
        }

        if let Some(ready) = &self.ready {
            let wait = if ready.attempt.whole {
                SWAP
            } else {
                swap_interval()
            };
            let due = self.last_swap.map_or(env.now, |at| at + wait);
            let theme_changed = self
                .shown
                .as_ref()
                .is_some_and(|shown| shown.dark != ready.attempt.dark);
            if env.now >= due || theme_changed || !self.glide.painted() {
                self.promote(path, showing_source, env, frame);
            } else if self.timer.as_ref().is_none_or(|(at, _)| *at < due) {
                self.timer = Some((due, wake(due - env.now, cx)));
            }
        }
        self.built = true;
    }

    fn promote(&mut self, path: &str, showing_source: bool, env: &Env<'_>, frame: &mut Frame) {
        let Some(ready) = self.ready.take() else {
            return;
        };
        self.opening = false;
        self.reserve = None;
        if self.swap.is_some() {
            self.finish_swap();
        }
        let from_layer = self.layer(showing_source);
        // A diagram drawn for the other theme cross-fades into this one's as
        // the palette around it glides (`theme::glide_mode`): a picture's
        // colours cannot be mixed as a token's are.
        let retheme = self
            .shown
            .as_ref()
            .is_some_and(|shown| shown.dark != ready.attempt.dark);
        let animate = !env.still && (!ready.snap || retheme) && self.glide.painted();
        match ready.drawn {
            Ok(picture) => {
                let replaced = self.shown.replace(Shown {
                    picture,
                    source: ready.attempt.source,
                    dark: ready.attempt.dark,
                });
                self.failed = None;
                self.generation += 1;
                self.leaving = replaced
                    .filter(|_| from_layer == Layer::Diagram)
                    .map(|replaced| replaced.picture);
            }
            Err(message) => self.failed = Some(message),
        }
        self.last_swap = Some(env.now);
        self.timer = None;
        frame.remeasure.push(path.to_string());
        if animate {
            self.swap = Some(Swap {
                at: env.now,
                from: self.glide.begin(),
                from_layer,
            });
            frame.moving = true;
        } else {
            self.finish_swap();
        }
    }
}

/// Render `attempt` on the background executor, and hand the result to the
/// fence's slot.
fn spawn(path: &str, attempt: Attempt, snap: bool, cx: &mut Context<MarkdownState>) -> Task<()> {
    let renderer = cx.svg_renderer();
    let raster = cx.background_executor().spawn({
        let (source, dark, whole) = (attempt.source.clone(), attempt.dark, attempt.whole);
        async move { draw(&source, dark, whole, &renderer) }
    });
    let path = path.to_string();
    cx.spawn(async move |this, cx| {
        let raster = raster.await;
        let _ = this.update(cx, |state, cx| {
            state.diagrams.drawn(&path, attempt, raster, snap, cx);
        });
    })
}

/// Wake the document after `delay`, when a waiting picture may be shown.
fn wake(delay: Duration, cx: &mut Context<MarkdownState>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        cx.background_executor().timer(delay).await;
        let _ = this.update(cx, |_, cx| cx.notify());
    })
}

/// The part of a streaming fence's `source`, which stops inside a line, made
/// of whole statements: up to its last line end, or in a flowchart, which
/// separates statements with `;` too, its last `;`. The line still being
/// written would draw a node from half its label, or fail to parse at all.
fn complete_statements(source: &str) -> Option<&str> {
    let header = source
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == ';')
        .next()
        .unwrap_or_default();
    let flowchart = header == "graph" || header.starts_with("flowchart");
    let end = source
        .rfind('\n')
        .into_iter()
        .chain(flowchart.then(|| source.rfind(';')).flatten())
        .max()?;
    Some(&source[..=end])
}

/// What a frame of the document needs from its fences.
pub(super) struct Env<'a> {
    pub(super) streaming: bool,
    pub(super) dark: bool,
    pub(super) now: Instant,
    /// Swaps snap: motion is reduced, or the document draws for the first
    /// time.
    pub(super) still: bool,
    /// The latest append was not watched arriving word by word: what it
    /// completes appears at once.
    pub(super) burst: bool,
    /// The fences a reader switched to their source.
    pub(super) sources: &'a HashSet<String>,
}

#[derive(Default)]
pub(super) struct Frame {
    /// The fences whose height changes this frame.
    pub(super) remeasure: Vec<String>,
    /// A swap is under way, so another frame must follow.
    pub(super) moving: bool,
}

/// Every Mermaid fence of a document, by render path.
#[derive(Default)]
pub(super) struct Diagrams {
    slots: HashMap<String, Slot>,
}

impl Diagrams {
    /// Bring every fence of `root` up to date for this frame: start the
    /// renders its source calls for, and show what is ready and due.
    pub(super) fn sync(
        &mut self,
        root: &BlockNode,
        env: &Env<'_>,
        cx: &mut Context<MarkdownState>,
    ) -> Frame {
        let mut frame = Frame::default();
        let mut fences = HashSet::new();
        if let BlockNode::Root { children } = root {
            for (ix, block) in children.iter().enumerate() {
                block.for_each_code_block_at(ix, &mut |path, code| {
                    if !is_mermaid(code.lang.as_deref()) {
                        return;
                    }
                    self.slots
                        .entry(path.clone())
                        .or_default()
                        .sync(&path, code, env, &mut frame, cx);
                    fences.insert(path);
                });
            }
        }
        self.slots.retain(|path, _| fences.contains(path));
        frame
    }

    fn drawn(
        &mut self,
        path: &str,
        attempt: Attempt,
        raster: Raster,
        snap: bool,
        cx: &mut Context<MarkdownState>,
    ) {
        let outcome = Outcome::new(raster, cx);
        if attempt.whole {
            let key = (attempt.source.clone(), attempt.dark);
            FINALS.with(|finals| finals.borrow_mut().insert(key, outcome.clone()));
        }
        if let Some(slot) = self.slots.get_mut(path)
            && slot.tried.as_ref() == Some(&attempt)
        {
            slot.in_flight = None;
            slot.accept(attempt, outcome, snap);
        }
        cx.notify();
    }

    /// Whether a fence's first picture is still being drawn.
    pub(super) fn opening(&self) -> bool {
        self.slots
            .values()
            .any(|slot| slot.opening && slot.in_flight.is_some())
    }

    /// The reader switches fence `path` between its diagram and its source,
    /// which cross-fade unless `still`.
    pub(super) fn toggle(&mut self, path: &str, showing_source: bool, now: Instant, still: bool) {
        let Some(slot) = self.slots.get_mut(path) else {
            return;
        };
        let from_layer = slot.layer(showing_source);
        slot.finish_swap();
        if !still && slot.glide.painted() {
            slot.swap = Some(Swap {
                at: now,
                from: slot.glide.begin(),
                from_layer,
            });
        }
    }
}

/// A Mermaid fence: its diagram once one is drawn, its code until then and
/// whenever the reader asks for it, both in one element tree so a swap
/// between them, or between two pictures, cross-fades while the block's
/// height glides.
pub(super) fn block(
    code: &CodeBlock,
    path: &str,
    is_last: bool,
    lines: impl FnOnce(&mut App) -> AnyElement,
    view: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let source = code.prepared().text.clone();
    let whole_code = source.strip_suffix('\n').unwrap_or(&source).to_string();
    let state = view.read(cx);
    let showing_source = state.diagram_sources.contains(path);
    let copied = state.copied.is(path);
    let streaming = state.streaming;
    let slot = state.diagrams.slots.get(path);
    let shown = slot.and_then(|slot| slot.shown.as_ref().map(|shown| shown.picture.clone()));
    let leaving = slot.and_then(|slot| slot.leaving.clone());
    let failed = slot.and_then(|slot| slot.failed.clone());
    let layer = slot.map_or(Layer::Source, |slot| slot.layer(showing_source));
    let swap = slot.and_then(|slot| slot.swap);
    let generation = slot.map_or(0, |slot| slot.generation);
    let glide = slot.map(|slot| slot.glide.clone()).unwrap_or_default();
    let reserve = slot
        .and_then(|slot| slot.reserve)
        .filter(|_| layer == Layer::Source && shown.is_none() && failed.is_none());
    let now = cx.background_executor().now();
    let progress = swap.map_or(1., |swap| {
        crate::motion::Curve::new(SWAP).progress(now.saturating_duration_since(swap.at))
    });

    let layer_fade = swap
        .filter(|swap| swap.from_layer != layer)
        .map(|_| progress);
    let shows = |of: Layer| of == layer || layer_fade.is_some();
    let mut layers = Vec::new();
    if shows(Layer::Source) {
        let failure = failed.as_ref().map(|message| {
            let text = match message {
                Some(message) => {
                    crate::tr!("markdown.mermaid_error_detail", message = message).into_owned()
                }
                None => crate::tr!("markdown.mermaid_error").into_owned(),
            };
            let line = div().pt_2().text_secondary(false, cx).child(text);
            #[cfg(test)]
            let line = {
                let path = path.to_string();
                line.debug_selector(move || format!("markdown-mermaid-error-{path}"))
            };
            line
        });
        let code = div()
            .w_full()
            .px_3()
            .py_3()
            .child(lines(cx))
            .children(failure);
        // Until its picture is drawn, the code holds the height the diagram
        // will have, so nothing below moves when it appears.
        let code = match reserve {
            Some(size) => div()
                .relative()
                .w_full()
                .overflow_hidden()
                .child(div().w_full().p_3().child(Fitted::<gpui::Img> {
                    intrinsic: size,
                    blend: None,
                    image: None,
                }))
                .child(code.absolute().top_0().left_0())
                .into_any_element(),
            None => code.into_any_element(),
        };
        layers.push((0, code));
    }
    if shows(Layer::Diagram)
        && let Some(current) = shown.clone()
    {
        let replacing = leaving
            .filter(|_| swap.is_some_and(|swap| swap.from_layer == Layer::Diagram))
            .filter(|_| layer == Layer::Diagram);
        let blend = replacing
            .as_ref()
            .map(|previous| (previous.size(), current.size(), progress));
        let place = |generation: u64| (generation % 2) as usize;
        let mut opacity = vec![0.; 2];
        opacity[place(generation)] = if replacing.is_some() { progress } else { 1. };
        let mut pictures = vec![(
            place(generation),
            picture(&current, blend, path, place(generation), cx),
        )];
        if let Some(previous) = &replacing {
            let at = place(generation + 1);
            opacity[at] = 1. - progress;
            pictures.push((at, picture(previous, blend, path, at, cx)));
        }
        let diagram = div().w_full().p_3().child(
            CrossFade::sampled(
                SharedString::from(format!("markdown-mermaid-pictures-{path}")),
                place(generation),
                opacity,
            )
            .span()
            .render(pictures),
        );
        #[cfg(test)]
        let diagram = {
            let path = path.to_string();
            diagram.debug_selector(move || format!("markdown-mermaid-{path}"))
        };
        layers.push((1, diagram.into_any_element()));
    }
    let layer_opacity = match (layer, layer_fade) {
        (Layer::Source, Some(progress)) => vec![progress, 1. - progress],
        (Layer::Diagram, Some(progress)) => vec![1. - progress, progress],
        (Layer::Source, None) => vec![1., 0.],
        (Layer::Diagram, None) => vec![0., 1.],
    };
    let faces = CrossFade::sampled(
        SharedString::from(format!("markdown-mermaid-layers-{path}")),
        match layer {
            Layer::Source => 0,
            Layer::Diagram => 1,
        },
        layer_opacity,
    )
    .span()
    .render(layers);

    let compact = crate::window_seam::window_is_compact(window, cx);
    let hover = view.read(cx).hover.clone();
    let buttons = view.read(cx).motion();
    let hover_key = SharedString::from(format!("markdown-mermaid-group-{path}"));
    let reveal = hover.reveal(&hover_key, window, cx);
    let drawing = (layer == Layer::Source && shown.is_none() && failed.is_none() && streaming)
        .then(|| {
            let label = div()
                .px_2()
                .text_secondary(false, cx)
                .child(crate::tr!("markdown.mermaid_drawing").into_owned());
            #[cfg(test)]
            let label = {
                let path = path.to_string();
                label.debug_selector(move || format!("markdown-mermaid-drawing-{path}"))
            };
            label
        });
    let toggle = (shown.is_some() && failed.is_none()).then(|| {
        let (icon, label) = if showing_source {
            (
                IconName::Network,
                crate::tr!("markdown.mermaid_show_diagram"),
            )
        } else {
            (IconName::Code, crate::tr!("markdown.mermaid_show_source"))
        };
        let (view, path) = (view.clone(), path.to_string());
        #[cfg(test)]
        let selector = format!("markdown-mermaid-toggle-{path}");
        let button = action_button(
            format!("mermaid-toggle-{path}"),
            icon,
            label.into_owned(),
            cx,
        )
        .touch(compact)
        .motion(&buttons)
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            view.update(cx, |state, cx| {
                state.set_diagram_source(&path, !showing_source, cx)
            });
        });
        #[cfg(test)]
        let button = div().debug_selector(move || selector).child(button);
        button
    });
    let copy = {
        let (view, path, code) = (view.clone(), path.to_string(), whole_code.clone());
        copy_button(
            &format!("mermaid-{path}"),
            copied,
            compact,
            move |_, _, cx| {
                cx.stop_propagation();
                view.update(cx, |state, cx| {
                    state.copy_code(path.clone(), code.clone(), cx)
                });
            },
            cx,
        )
        .motion(&buttons)
    };
    // Revealed on hover over a diagram, as a formula's actions are, and pinned
    // over code, where there is no hover, and while the diagram is building.
    // Over the card in either width class; the pin fades as the window
    // turns compact or wide.
    let pinned = super::math::actions_pinned(view, path, compact, window, cx);
    let actions = h_flex()
        .id(SharedString::from(format!(
            "markdown-mermaid-actions-{path}"
        )))
        .on_hover(hover.on_hover(hover_key.clone()))
        .absolute()
        .top_1()
        .right_1()
        .items_center()
        .block_mouse_except_scroll()
        .rounded(crate::material::radius_card(cx))
        .bg(cx.theme().tokens.colors.muted)
        .children(drawing)
        .children(toggle)
        .child(copy)
        .when(layer == Layer::Diagram, |actions| {
            actions.opacity(reveal + (1. - reveal) * pinned)
        });
    // The surface takes the glided height, so what a swap clips is clipped
    // at its edge rather than painted past it.
    let card = div()
        .w_full()
        .relative()
        .rounded(crate::material::radius_card(cx))
        .bg(cx.theme().tokens.colors.muted)
        .child(SizeGlide::new(&glide, faces).gliding(swap.and_then(|swap| swap.from), progress))
        .child(actions);
    #[cfg(test)]
    let card = {
        let path = path.to_string();
        card.debug_selector(move || format!("markdown-mermaid-card-{path}"))
    };
    let context_view = view.clone();
    let block = div()
        .id(SharedString::from(path.to_string()))
        .on_hover(hover.on_hover(hover_key))
        .w_full()
        .when(!is_last, |block| block.pb(rems(1.)))
        .on_mouse_down(MouseButton::Right, move |_, _, cx| {
            context_view.update(cx, |state, cx| {
                state.set_pending_context(
                    Some(PendingContextTarget::CodeBlock(whole_code.clone())),
                    cx,
                )
            });
        })
        .child(card);
    #[cfg(test)]
    let block = {
        let path = path.to_string();
        block.debug_selector(move || format!("markdown-mermaid-block-{path}"))
    };
    block.into_any_element()
}

/// A picture that opens in the viewer, at the block's width.
fn picture(
    picture: &Picture,
    blend: Option<(gpui::Size<Pixels>, gpui::Size<Pixels>, f32)>,
    path: &str,
    place: usize,
    cx: &mut App,
) -> AnyElement {
    let label: SharedString = crate::tr!("markdown.mermaid_diagram").into_owned().into();
    let source = picture.0.image.source();
    let viewer = source.clone();
    let viewer_label = label.clone();
    let image = crate::material::accessible_clickable(
        img(source),
        SharedString::from(format!("markdown-mermaid-picture-{path}-{place}")),
        Role::Button,
        label,
        cx,
    )
    .object_fit(ObjectFit::Contain)
    .cursor_pointer()
    .on_click(move |_, window, cx| {
        cx.stop_propagation();
        crate::image_viewer::open(viewer.clone(), viewer_label.to_string(), window, cx);
    });
    #[cfg(test)]
    let image = {
        let path = path.to_string();
        image.debug_selector(move || format!("markdown-mermaid-picture-{path}-{place}"))
    };
    Fitted {
        intrinsic: picture.size(),
        blend,
        image: Some(image),
    }
    .into_any_element()
}

/// A picture of known size, fitted to the width it is given. While one
/// picture replaces another, both are drawn at one scale gliding from the
/// old picture's fitted scale to the new one's, so the diagram's text keeps
/// its size and a picture that must shrink to fit shrinks smoothly; neither
/// is ever drawn larger than it fits, so a wider picture comes in at its own
/// fitted scale, whole, while the one it replaces shrinks to meet it.
struct Fitted<E> {
    intrinsic: gpui::Size<Pixels>,
    /// The intrinsic sizes of the picture replaced and the new one, and how
    /// far the swap has come.
    blend: Option<(gpui::Size<Pixels>, gpui::Size<Pixels>, f32)>,
    image: Option<E>,
}

impl<E> Fitted<E> {
    fn size_at(&self, width: Option<Pixels>) -> gpui::Size<Pixels> {
        let scale = |of: gpui::Size<Pixels>| {
            if of.width <= Pixels::ZERO {
                return 1.;
            }
            fit(of, width).width / of.width
        };
        let fitted = scale(self.intrinsic);
        let scale = match self.blend {
            Some((from, to, progress)) => {
                (scale(from) + (scale(to) - scale(from)) * progress).min(fitted)
            }
            None => fitted,
        };
        size(self.intrinsic.width * scale, self.intrinsic.height * scale)
    }
}

impl<E: IntoElement + Styled + 'static> IntoElement for Fitted<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: IntoElement + Styled + 'static> Element for Fitted<E> {
    type RequestLayoutState = ();
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.max_size.width = relative(1.).into();
        let sizing = Fitted::<()> {
            intrinsic: self.intrinsic,
            blend: self.blend,
            image: None,
        };
        let layout_id = window.request_measured_layout(
            style,
            move |known_dimensions, available_space, _, _| {
                let width = known_dimensions.width.or(match available_space.width {
                    AvailableSpace::Definite(width) => Some(width),
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                });
                sizing.size_at(width)
            },
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let fitted = self.size_at(Some(bounds.size.width));
        let mut image = self
            .image
            .take()?
            .w(fitted.width)
            .h(fitted.height)
            .into_any_element();
        image.prepaint_as_root(
            bounds.origin,
            size(
                AvailableSpace::Definite(fitted.width),
                AvailableSpace::Definite(fitted.height),
            ),
            window,
            cx,
        );
        Some(image)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        image: &mut Option<AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(image) = image {
            image.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{
        AppContext as _, Context, Entity, IntoElement, Render, TestAppContext, VisualTestContext,
        Window, div, px,
    };

    use super::*;
    use crate::markdown::MarkdownView;
    use crate::test_frames::{draw, draw_settled};

    const FRAME: Duration = Duration::from_millis(16);

    const FLOWCHART: &str = "flowchart TD
    A[User sends message] --> B{Provider configured?}
    B -->|Yes| C[Start session]
    B -->|No| D[Show setup dialog]
    C --> E[Stream events]
    E --> F{Turn complete?}
    F -->|No| E
    F -->|Yes| G[Persist transcript]
    D --> H[User picks provider]
    H --> C
    G --> I((Done))
";

    struct Root {
        markdown: Entity<MarkdownState>,
    }

    impl Render for Root {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(600.))
                .flex()
                .flex_col()
                .child(MarkdownView::new(&self.markdown).selectable(true))
                .child(div().h(px(20.)).debug_selector(|| "below".into()))
        }
    }

    /// A message window whose text starts as `text`, still arriving when
    /// `streaming`, as the chat shows a reply.
    fn open<'a>(
        text: &str,
        streaming: bool,
        cx: &'a mut TestAppContext,
    ) -> (Entity<MarkdownState>, &'a mut VisualTestContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let text = text.to_string();
        let (root, cx) = cx.add_window_view(|_, cx| Root {
            markdown: cx.new(|cx| {
                let mut state = MarkdownState::new(&text, cx);
                state.set_streaming(streaming, cx);
                state
            }),
        });
        cx.simulate_resize(gpui::size(px(800.), px(2400.)));
        draw(cx);
        let markdown = root.read_with(cx, |root, _| root.markdown.clone());
        (markdown, cx)
    }

    fn push(markdown: &Entity<MarkdownState>, text: &str, cx: &mut VisualTestContext) {
        markdown.update(cx, |state, cx| state.push_str(text, cx));
    }

    /// One display frame later.
    fn frame(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(FRAME);
        draw(cx);
    }

    fn frames(count: usize, cx: &mut VisualTestContext) {
        for _ in 0..count {
            frame(cx);
        }
    }

    fn block_height(cx: &mut VisualTestContext) -> Pixels {
        cx.debug_bounds("markdown-mermaid-block-root-0")
            .expect("the fence is painted")
            .size
            .height
    }

    fn block_width(cx: &mut VisualTestContext) -> Pixels {
        cx.debug_bounds("markdown-mermaid-block-root-0")
            .expect("the fence is painted")
            .size
            .width
    }

    fn diagram_shown(cx: &mut VisualTestContext) -> bool {
        cx.debug_bounds("markdown-mermaid-root-0").is_some()
    }

    fn code_shown(cx: &mut VisualTestContext) -> bool {
        cx.debug_bounds("markdown-code-line-0").is_some()
    }

    fn shown_source(
        markdown: &Entity<MarkdownState>,
        cx: &mut VisualTestContext,
    ) -> Option<String> {
        markdown.read_with(cx, |state, _| {
            state
                .diagrams
                .slots
                .get("root-0")
                .and_then(|slot| slot.shown.as_ref().map(|shown| shown.source.to_string()))
        })
    }

    #[gpui::test]
    fn a_statement_that_does_not_parse_yet_leaves_the_diagram_as_it_is(cx: &mut TestAppContext) {
        let (markdown, cx) = open("", true, cx);
        for line in [
            "```mermaid\n",
            "sequenceDiagram\n",
            "    participant U as User\n",
            "    participant C as Client\n",
            "    U->>C: Type prompt\n",
        ] {
            push(&markdown, line, cx);
            frames(30, cx);
        }
        assert!(diagram_shown(cx) && !code_shown(cx), "the diagram is drawn");
        let before = block_height(cx);
        // A block statement parses only once its `end` arrives.
        for line in [
            "    loop Streaming\n",
            "        C-->>U: Text delta\n",
            "        U-->>C: Timeline update\n",
        ] {
            push(&markdown, line, cx);
            for _ in 0..30 {
                frame(cx);
                assert!(
                    diagram_shown(cx) && !code_shown(cx),
                    "{line:?}: the diagram stays, never its code nor nothing"
                );
                assert_eq!(block_height(cx), before, "{line:?}: nothing moves");
            }
        }
        push(&markdown, "    end\n", cx);
        for _ in 0..30 {
            frame(cx);
            assert!(
                diagram_shown(cx),
                "the diagram is drawn throughout its swap"
            );
        }
        assert!(block_height(cx) > before, "the loop joins the diagram");
    }

    #[gpui::test]
    fn a_streaming_diagram_grows_by_whole_statements_one_swap_at_a_time(cx: &mut TestAppContext) {
        let (markdown, cx) = open("", true, cx);
        let source = format!("```mermaid\n{FLOWCHART}");
        let start = cx.executor().now();
        let mut swaps: Vec<(Duration, String)> = Vec::new();
        let mut settled_on_diagram = false;
        for chunk in source.as_bytes().chunks(8) {
            push(&markdown, std::str::from_utf8(chunk).unwrap(), cx);
            frame(cx);
            if let Some(shown) = shown_source(&markdown, cx)
                && swaps.last().is_none_or(|(_, last)| *last != shown)
            {
                swaps.push((cx.executor().now() - start, shown));
            }
            let (diagram, code) = (diagram_shown(cx), code_shown(cx));
            if settled_on_diagram {
                assert!(diagram && !code, "back to code after a diagram showed");
            }
            settled_on_diagram |= diagram && !code;
        }
        assert!(settled_on_diagram, "the diagram showed while streaming");
        for (_, shown) in &swaps {
            assert!(
                shown.ends_with('\n'),
                "a picture of half a statement showed: {shown:?}"
            );
        }
        // A swap is seen on the frame after the append that brought it or on
        // the one it was due, so two sightings may be one frame closer.
        for pair in swaps.windows(2) {
            assert!(
                pair[1].0 - pair[0].0 + FRAME >= swap_interval(),
                "two swaps closer than one fade and a rest: {swaps:?}"
            );
        }
        assert!(
            swaps.len() >= 2,
            "the diagram grew while streaming: {swaps:?}"
        );
    }

    #[gpui::test]
    fn a_flowchart_written_on_one_line_grows_by_its_statements(cx: &mut TestAppContext) {
        let (markdown, cx) = open("", true, cx);
        push(&markdown, "```mermaid\ngraph LR; A-->B; B-->C", cx);
        frames(30, cx);
        assert_eq!(
            shown_source(&markdown, cx).as_deref(),
            Some("graph LR; A-->B;"),
            "the statements before the one still being written"
        );
    }

    #[gpui::test]
    fn the_whole_source_is_drawn_when_the_fence_closes_or_the_reply_ends(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_reduce_motion(true));
        let statements = "flowchart TD\n    A --> B\n    B --> C\n";
        let partial = "    C --> D[Archive";
        let whole_height = |text: &str, cx: &mut TestAppContext| {
            let (_, cx) = open(text, false, cx);
            draw_settled(cx);
            block_height(cx)
        };
        let closed_height = whole_height(&format!("```mermaid\n{FLOWCHART}```\n"), cx);
        let open_height = whole_height(&format!("```mermaid\n{statements}{partial}"), cx);

        let (markdown, cx) = open("", true, cx);
        push(&markdown, &format!("```mermaid\n{FLOWCHART}"), cx);
        frames(10, cx);
        push(&markdown, "```", cx);
        draw_settled(cx);
        assert_eq!(
            block_height(cx),
            closed_height,
            "the closed fence's whole source"
        );
        assert!(diagram_shown(cx) && !code_shown(cx));
        assert!(
            cx.debug_bounds("markdown-mermaid-drawing-root-0").is_none(),
            "no longer building"
        );

        // A reply that ends inside its fence ends with its whole source too.
        markdown.update(cx, |state, cx| {
            state.set_text("", cx);
            state.push_str(&format!("```mermaid\n{statements}{partial}"), cx);
        });
        frames(30, cx);
        assert_eq!(
            shown_source(&markdown, cx).as_deref(),
            Some(statements),
            "while streaming, the statement still being written is not drawn"
        );
        let streaming_height = block_height(cx);
        markdown.update(cx, |state, cx| state.set_streaming(false, cx));
        draw_settled(cx);
        assert_eq!(block_height(cx), open_height);
        assert!(
            open_height > streaming_height,
            "the last statement joined the diagram"
        );
    }

    #[gpui::test]
    fn a_source_that_never_parses_shows_its_message_only_once_complete(cx: &mut TestAppContext) {
        let (markdown, cx) = open("", true, cx);
        push(&markdown, "```mermaid\nnot a diagram\n", cx);
        frames(30, cx);
        assert!(code_shown(cx) && !diagram_shown(cx));
        assert!(
            cx.debug_bounds("markdown-mermaid-drawing-root-0").is_some(),
            "the code says the diagram is being drawn"
        );
        assert!(
            cx.debug_bounds("markdown-mermaid-error-root-0").is_none(),
            "no error while the source is still arriving"
        );
        push(&markdown, "still not\n```\n", cx);
        draw_settled(cx);
        assert!(code_shown(cx) && !diagram_shown(cx));
        assert!(cx.debug_bounds("markdown-mermaid-error-root-0").is_some());
    }

    #[gpui::test]
    fn a_final_source_that_fails_after_a_diagram_fades_to_its_code(cx: &mut TestAppContext) {
        let (markdown, cx) = open("", true, cx);
        push(&markdown, "```mermaid\nflowchart TD\n    A --> B\n", cx);
        frames(30, cx);
        assert!(diagram_shown(cx));
        let diagram_height = block_height(cx);
        // A subgraph that never ends: the whole source does not parse.
        push(&markdown, "    subgraph Late\n    B --> C\n```\n", cx);
        let mut both = false;
        for _ in 0..30 {
            frame(cx);
            both |= diagram_shown(cx) && code_shown(cx);
        }
        assert!(
            both,
            "the diagram cross-faded to the code, not swapped at once"
        );
        assert!(code_shown(cx) && !diagram_shown(cx));
        assert!(cx.debug_bounds("markdown-mermaid-error-root-0").is_some());
        assert!(
            cx.debug_bounds("markdown-mermaid-toggle-root-0").is_none(),
            "nothing offers a picture of a source the model did not write"
        );
        assert_ne!(block_height(cx), diagram_height);
    }

    #[gpui::test]
    fn a_swap_glides_the_block_in_one_motion(cx: &mut TestAppContext) {
        let (markdown, cx) = open("", true, cx);
        push(&markdown, "```mermaid\nflowchart TD\n    A --> B\n", cx);
        frames(30, cx);
        let before = block_height(cx);
        // Wider than the row: the new picture must shrink to fit it.
        let siblings = (0..8)
            .map(|ix| format!("    A --> W{ix}[a sibling wide enough to widen the row]\n"))
            .collect::<String>();
        push(
            &markdown,
            &format!("    B --> C\n    C --> D\n{siblings}"),
            cx,
        );
        let mut heights = vec![before];
        let mut widest = Pixels::ZERO;
        for _ in 0..30 {
            frame(cx);
            let block = cx.debug_bounds("markdown-mermaid-block-root-0").unwrap();
            let below = cx.debug_bounds("below").unwrap();
            assert_eq!(
                below.top(),
                block.bottom(),
                "what is below moves with the block"
            );
            heights.push(block.size.height);
            assert_eq!(
                cx.debug_bounds("markdown-mermaid-card-root-0"),
                Some(block),
                "the block's surface covers the block as it glides"
            );
            for picture in [
                cx.debug_bounds("markdown-mermaid-picture-root-0-0"),
                cx.debug_bounds("markdown-mermaid-picture-root-0-1"),
            ]
            .into_iter()
            .flatten()
            {
                assert!(
                    picture.left() >= block.left() && picture.right() <= block.right(),
                    "a picture is cut off at the block's side: {picture:?} in {block:?}"
                );
                widest = widest.max(picture.size.width);
            }
        }
        assert!(
            widest > block_width(cx) * 0.9,
            "the new picture fills the row: {widest:?}"
        );
        let after = *heights.last().unwrap();
        assert_ne!(after, before, "{heights:?}");
        let grows = after > before;
        assert!(
            heights.windows(2).all(|pair| if grows {
                pair[1] >= pair[0]
            } else {
                pair[1] <= pair[0]
            }),
            "the block moves one way, without stepping back: {heights:?}"
        );
    }

    #[gpui::test]
    fn with_motion_reduced_a_swap_snaps(cx: &mut TestAppContext) {
        let (markdown, cx) = open("", true, cx);
        cx.update(|_, cx| cx.set_reduce_motion(true));
        push(&markdown, "```mermaid\nflowchart TD\n    A --> B\n", cx);
        frames(30, cx);
        let before = block_height(cx);
        push(&markdown, "    B --> C\n    C --> D\n", cx);
        frame(cx);
        let snapped = block_height(cx);
        assert_ne!(snapped, before);
        draw_settled(cx);
        assert_eq!(block_height(cx), snapped, "the swap took one frame");
    }

    /// A document opened with a whole fence draws it off the main thread,
    /// and the picture then appears whole at once: it never fades in from
    /// the code, and nothing moves after.
    #[gpui::test]
    fn a_diagram_from_history_appears_whole_once_drawn(cx: &mut TestAppContext) {
        let (_, cx) = open(&format!("```mermaid\n{FLOWCHART}```\n"), false, cx);
        assert!(
            diagram_shown(cx) && !code_shown(cx),
            "the picture, not a cross-fade from the code"
        );
        let first = block_height(cx);
        draw_settled(cx);
        assert_eq!(
            block_height(cx),
            first,
            "nothing moves after the first frame"
        );
    }

    /// A fence drawn before, whose picture is no longer among the recent
    /// ones, is drawn again off the main thread; meanwhile its code holds
    /// the diagram's height, so nothing below moves when the picture comes.
    #[gpui::test]
    fn a_diagram_drawn_again_holds_its_height_while_it_draws(cx: &mut TestAppContext) {
        let text = format!("```mermaid\n{FLOWCHART}```\n");
        let (_, cx) = open(&text, false, cx);
        draw_settled(cx);
        let drawn = block_height(cx);
        // Many diagrams later: the picture has left, its size is kept.
        FINALS.with(|finals| finals.borrow_mut().entries.clear());
        let root = cx
            .update(|window, _| window.root::<Root>())
            .flatten()
            .expect("the root view");
        root.update(cx, |root, cx| {
            root.markdown = cx.new(|cx| MarkdownState::new(&text, cx));
            cx.notify();
        });
        // A frame drawn before the picture is.
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        assert!(
            code_shown(cx) && !diagram_shown(cx),
            "the code, until drawn"
        );
        assert_eq!(
            block_height(cx),
            drawn,
            "the code holds the diagram's height"
        );
        draw(cx);
        assert!(diagram_shown(cx) && !code_shown(cx), "the picture, at once");
        assert_eq!(block_height(cx), drawn);
    }

    /// A theme switch draws a diagram again for the new theme; with motion
    /// reduced the old picture is never shown beside the new one.
    #[gpui::test]
    fn a_theme_switch_draws_a_diagram_again(cx: &mut TestAppContext) {
        let (_, cx) = open(&format!("```mermaid\n{FLOWCHART}```\n"), false, cx);
        cx.update(|_, cx| cx.set_reduce_motion(true));
        draw_settled(cx);
        let pictures = |cx: &mut VisualTestContext| {
            (0..4)
                .filter(|generation| {
                    cx.debug_bounds(format!("markdown-mermaid-picture-root-0-{generation}").leak())
                        .is_some()
                })
                .collect::<Vec<_>>()
        };
        let before = pictures(cx);
        assert_eq!(before.len(), 1);
        cx.update(|window, cx| crate::theme::glide_mode(crate::theme::ThemeMode::Dark, window, cx));
        let mut both = 0;
        for _ in 0..30 {
            cx.update(|window, cx| window.simulate_next_frame(cx));
            frame(cx);
            both += usize::from(pictures(cx).len() == 2);
        }
        draw_settled(cx);
        let after = pictures(cx);
        assert_eq!(after.len(), 1);
        assert_ne!(
            after, before,
            "the diagram was drawn again for the dark theme"
        );
        assert_eq!(both, 0, "with motion reduced the diagram snapped");
    }

    #[gpui::test]
    fn the_reader_switches_a_diagram_to_its_source_and_back(cx: &mut TestAppContext) {
        let (_, cx) = open(&format!("```mermaid\n{FLOWCHART}```\n"), false, cx);
        let toggle = cx
            .debug_bounds("markdown-mermaid-toggle-root-0")
            .expect("a diagram offers its source");
        cx.simulate_click(toggle.center(), Default::default());
        draw_settled(cx);
        assert!(code_shown(cx) && !diagram_shown(cx), "the source");
        let toggle = cx.debug_bounds("markdown-mermaid-toggle-root-0").unwrap();
        cx.simulate_click(toggle.center(), Default::default());
        draw_settled(cx);
        assert!(diagram_shown(cx) && !code_shown(cx), "the diagram again");
    }

    /// A long stream draws many pictures; once the diagram settles, the atlas
    /// holds only the one it shows.
    #[gpui::test]
    fn pictures_a_stream_replaced_leave_the_atlas(cx: &mut TestAppContext) {
        let (markdown, cx) = open("", true, cx);
        let mut pictures: Vec<Arc<RenderImage>> = Vec::new();
        let source = format!("```mermaid\n{FLOWCHART}```\n");
        let lines: Vec<_> = source.split_inclusive('\n').collect();
        for statements in lines.chunks(3) {
            push(&markdown, &statements.concat(), cx);
            for _ in 0..30 {
                frame(cx);
                let shown = markdown.read_with(cx, |state, _| {
                    state.diagrams.slots.get("root-0").and_then(|slot| {
                        slot.shown
                            .as_ref()
                            .map(|shown| shown.picture.0.image.image().clone())
                    })
                });
                if let Some(shown) = shown
                    && pictures.last().is_none_or(|last| last.id != shown.id)
                {
                    pictures.push(shown);
                }
            }
        }
        draw_settled(cx);
        assert!(pictures.len() > 3, "the diagram grew: {}", pictures.len());
        let (last, replaced) = pictures.split_last().unwrap();
        cx.update(|window, _| {
            assert!(
                window.has_image_atlas_entry(last),
                "the shown picture is drawn"
            );
            for picture in replaced {
                assert!(
                    !window.has_image_atlas_entry(picture),
                    "a replaced picture stays in the atlas"
                );
            }
        });
    }
}
