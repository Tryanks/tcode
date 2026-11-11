//! The window-level image viewer: one image on a host-filling dialog, zoomed
//! and panned by wheel, trackpad, pinch, drag, touch, keyboard and a toolbar.
//! Shared by the composer's pending strip, sent-message thumbnails, Markdown
//! images and image-link badges.

use std::time::Duration;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;

use gpui::{
    App, AppContext as _, Bounds, ClickEvent, Context, FocusHandle, ImageSource,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, PinchEvent, Pixels, Point, Render, ScrollDelta, ScrollEvent, SharedString,
    Size, StatefulInteractiveElement as _, Styled as _, TouchPhase, Window, actions, div, img,
    point, prelude::FluentBuilder as _, px, size,
};
use gpui_base::motion::{Interpolate, Transition, transition};
#[cfg(target_family = "wasm")]
use web_time::Instant;

use crate::icon::IconName;
use crate::overlay::OverlayExt as _;
use crate::sizing::Sizable as _;
use crate::store::{ImageHost, images::HostImages};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

actions!(image_viewer, [ZoomIn, ZoomOut, ZoomToFit, ActualSize]);

const CONTEXT: &str = "ImageViewer";

/// Zoom gained by one wheel notch, one key press or one toolbar press.
const ZOOM_STEP: f32 = 1.25;
/// What one wheel notch reports when a wheel reports pixels, as browsers do.
const WHEEL_NOTCH: Pixels = px(100.);
/// The pixel delta that zooms by a factor of e when a pinch arrives as a
/// Control-wheel, the encoding browsers give trackpad pinches.
const PINCH_PIXELS: Pixels = px(100.);
/// A pan's momentum carries no touch phase; scrolls arriving closer together
/// than this after the pan's fingers lifted still belong to it.
const MOMENTUM_GAP: Duration = Duration::from_millis(100);
/// Zoom relative to the fitted size past which zooming stops. The image's
/// own size stays reachable even when that lies further out.
const MAX_ZOOM: f32 = 8.;
/// What one wheel line is worth when a line delta pans.
const WHEEL_LINE: Pixels = px(20.);
/// Pointer travel before a press is a drag rather than the click it began as.
const DRAG_SLOP: Pixels = px(4.);
const STAGE_MARGIN: Pixels = px(16.);

/// What the stage shows while it has no image to show.
#[derive(Clone, PartialEq)]
enum Placeholder {
    Message(SharedString),
    Loading,
    Glyph,
}

#[derive(Clone)]
pub(crate) enum ViewerSource {
    Image(ImageSource),
    Original {
        host: ImageHost,
        target: tcode_protocol::OriginalTarget,
        preview: ImageSource,
    },
}

impl From<ImageSource> for ViewerSource {
    fn from(source: ImageSource) -> Self {
        Self::Image(source)
    }
}
impl ViewerSource {
    pub(crate) fn original(
        host: ImageHost,
        target: tcode_protocol::OriginalTarget,
        preview: ImageSource,
    ) -> Self {
        Self::Original {
            host,
            target,
            preview,
        }
    }
}

#[derive(Clone, PartialEq)]
enum OriginalState {
    Loading { received: u64, total: Option<u64> },
    Cancelled,
    Failed(SharedString),
    Complete,
}

pub(crate) fn init(cx: &mut App) {
    use crate::shortcut::secondary;
    let bindings = [
        KeyBinding::new("=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("shift-=", ZoomIn, Some(CONTEXT)),
        secondary("secondary-=", ZoomIn, Some(CONTEXT), cx),
        KeyBinding::new("-", ZoomOut, Some(CONTEXT)),
        secondary("secondary--", ZoomOut, Some(CONTEXT), cx),
        KeyBinding::new("0", ZoomToFit, Some(CONTEXT)),
        secondary("secondary-0", ZoomToFit, Some(CONTEXT), cx),
        KeyBinding::new("1", ActualSize, Some(CONTEXT)),
        secondary("secondary-1", ActualSize, Some(CONTEXT), cx),
    ];
    cx.bind_keys(bindings);
}

/// Open `source` in the viewer on the window's dialog stack, where it
/// inherits Escape dismissal and focus restoration.
pub(crate) fn open(
    source: impl Into<ViewerSource>,
    title: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    let viewer = cx.new(|cx| match source.into() {
        ViewerSource::Image(source) => ImageViewer::new(source, title.into(), cx),
        ViewerSource::Original {
            host,
            target,
            preview,
        } => {
            let mut viewer = ImageViewer::new(preview, title.into(), cx);
            viewer.target = Some((host, target));
            viewer.start_original(cx);
            viewer.host_changes = Some(cx.observe_global::<HostImages>(move |this, cx| {
                if HostImages::link(host, cx).is_none() {
                    this.cancel_original(cx);
                }
            }));
            viewer
        }
    });
    let focus = viewer.read(cx).focus_handle.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let closing = viewer.clone();
        dialog
            .fill()
            .close_button(false)
            .on_close(move |_, _, cx| {
                closing.update(cx, |viewer, cx| viewer.cancel_original(cx));
            })
            .child(viewer.clone())
    });
    focus.focus(window, cx);
}

/// What an image draws when it has no preview to show; the viewer it still opens reads the
/// original. `caption` names the action on tiles large enough for text, and `framed` gives
/// the placeholder its own surface where nothing behind the image draws one.
pub(crate) fn unpreviewed(
    caption: bool,
    framed: bool,
    cx: &App,
) -> impl Fn() -> gpui::AnyElement + 'static {
    let color = cx.theme().muted_foreground;
    let surface = framed.then(|| (cx.theme().muted, crate::material::radius_card(cx)));
    let caption_text = cx.theme().tokens.typography.xs;
    let spacing_xs = cx.theme().tokens.spacing.xs;
    let caption =
        caption.then(|| SharedString::from(crate::tr!("markdown.open_image").into_owned()));
    move || {
        gpui_base::v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(spacing_xs)
            .text_color(color)
            .when_some(surface, |tile, (background, radius)| {
                tile.rounded(radius).bg(background)
            })
            .child(
                crate::icon::Icon::default()
                    .path("icons/image.svg")
                    .text_color(color),
            )
            .when_some(caption.clone(), |tile, caption| {
                tile.child(div().text_token(&caption_text).child(caption))
            })
            .into_any_element()
    }
}

/// Where the image sits: its zoom relative to the fitted size, where 1 fits
/// the stage, and the pan of its centre from the stage centre.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Placement {
    zoom: f32,
    offset: Point<Pixels>,
}

impl Placement {
    const FIT: Self = Self {
        zoom: 1.,
        offset: Point {
            x: Pixels::ZERO,
            y: Pixels::ZERO,
        },
    };
}

impl Interpolate for Placement {
    fn interpolate(&self, target: &Self, progress: f32) -> Self {
        Self {
            zoom: self.zoom + (target.zoom - self.zoom) * progress,
            offset: self.offset + (target.offset - self.offset) * progress,
        }
    }
}

/// A scroll gesture delimited by touch phases: a trackpad, or fingers on a
/// touch screen.
#[derive(Clone, Copy)]
enum Pan {
    Touching,
    /// The fingers lifted at this instant, or the momentum after it last moved.
    Coasting(Instant),
}

/// A primary press on the stage, kept until the click it completes reads it
/// or a release elsewhere ends it.
struct Drag {
    origin: Point<Pixels>,
    last: Point<Pixels>,
    moved: bool,
}

pub(crate) struct ImageViewer {
    source: ImageSource,
    title: SharedString,
    focus_handle: FocusHandle,
    placement: Placement,
    drag: Option<Drag>,
    pan: Option<Pan>,
    /// Keys, buttons and double-clicks glide to their placement; wheel,
    /// pinch and drag track the pointer.
    animate: bool,
    /// The last frame's geometry, so pointer input resolves against what was
    /// drawn: the stage is the whole dialog host, the frame is the area a
    /// fitted image fills, and `natural` is the decoded image's own size, or
    /// the original's while a preview stands in for it.
    stage: Bounds<Pixels>,
    frame: Size<Pixels>,
    natural: Option<Size<Pixels>>,
    /// The original's size as its metadata names it, before its bytes arrive.
    original_size: Option<Size<Pixels>>,
    failed: bool,
    target: Option<(ImageHost, tcode_protocol::OriginalTarget)>,
    remote_progress: bool,
    /// The preview the original replaced, and when: it stays under the
    /// original while the original fades in over it.
    replaced: Option<(ImageSource, Instant)>,
    original_state: Option<OriginalState>,
    transfer: Option<gpui::Task<()>>,
    host_changes: Option<gpui::Subscription>,
}

impl ImageViewer {
    fn new(source: ImageSource, title: SharedString, cx: &mut Context<Self>) -> Self {
        Self {
            source,
            title,
            focus_handle: cx.focus_handle(),
            placement: Placement::FIT,
            drag: None,
            pan: None,
            animate: false,
            stage: Bounds::default(),
            frame: Size::default(),
            natural: None,
            original_size: None,
            failed: false,
            target: None,
            remote_progress: false,
            replaced: None,
            original_state: None,
            transfer: None,
            host_changes: None,
        }
    }

    fn cancel_original(&mut self, cx: &mut Context<Self>) {
        if self.transfer.take().is_some()
            && matches!(self.original_state, Some(OriginalState::Loading { .. }))
        {
            self.original_state = Some(OriginalState::Cancelled);
            cx.notify();
        }
    }

    fn start_original(&mut self, cx: &mut Context<Self>) {
        self.cancel_original(cx);
        let Some((host, target)) = self.target.clone() else {
            return;
        };
        let Some(host) = HostImages::link(host, cx) else {
            self.original_state = Some(OriginalState::Failed(
                crate::tr!("image_viewer.load_failed").into_owned().into(),
            ));
            return;
        };
        self.remote_progress = host.is_remote();
        let remote = self.remote_progress;
        let renderer = cx.svg_renderer();
        self.original_state = Some(OriginalState::Loading {
            received: 0,
            total: None,
        });
        self.transfer = Some(cx.spawn(async move |this, cx| {
            let mut transfer = host.read_original(target);
            let result = async {
                let metadata = transfer.metadata().await?;
                let total = metadata.len;
                let dimensions = metadata.dimensions;
                let _ = this.update(cx, |this, cx| {
                    this.original_size =
                        dimensions.map(|(width, height)| size(px(width as f32), px(height as f32)));
                    if remote {
                        this.original_state = Some(OriginalState::Loading {
                            received: 0,
                            total: Some(total),
                        });
                    }
                    cx.notify();
                });
                let mut bytes = Vec::new();
                while let Some(block) = transfer.read().await? {
                    bytes
                        .try_reserve(block.len())
                        .map_err(tcode_client::original::error)?;
                    bytes.extend_from_slice(&block);
                    let received = transfer.received();
                    if remote
                        && this
                            .update(cx, |this, cx| {
                                this.original_state = Some(OriginalState::Loading {
                                    received,
                                    total: Some(total),
                                });
                                cx.notify();
                            })
                            .is_err()
                    {
                        return Err(tcode_client::original::error("viewer closed"));
                    }
                }
                let format = image::guess_format(&bytes)
                    .ok()
                    .and_then(|format| gpui::ImageFormat::from_mime_type(format.to_mime_type()))
                    .ok_or_else(|| tcode_client::original::error("unsupported image format"))?;
                cx.background_executor()
                    .spawn(async move {
                        gpui::Image::from_bytes(format, bytes)
                            .to_image_data(renderer)
                            .map_err(tcode_client::original::error)
                    })
                    .await
            }
            .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(original) => {
                        let original = crate::store::images::Owned::new(original, cx).source();
                        let preview = std::mem::replace(&mut this.source, original);
                        this.replaced = Some((preview, cx.background_executor().now()));
                        this.original_state = Some(OriginalState::Complete);
                    }
                    Err(error) => {
                        this.original_state = Some(OriginalState::Failed(error.message.into()))
                    }
                }
                cx.notify();
            });
        }));
    }

    /// The scale at which the image fits the frame. A smaller image is shown
    /// at its own size rather than enlarged, except a preview of an original
    /// whose size is not known yet, which stands in at the frame's size.
    fn fit_scale(&self) -> Option<f32> {
        let natural = self.natural?;
        if natural.width <= Pixels::ZERO
            || natural.height <= Pixels::ZERO
            || self.frame.width <= Pixels::ZERO
            || self.frame.height <= Pixels::ZERO
        {
            return None;
        }
        let fit = (self.frame.width / natural.width).min(self.frame.height / natural.height);
        Some(
            if self.original_size.is_none()
                && matches!(self.original_state, Some(OriginalState::Loading { .. }))
            {
                fit
            } else {
                fit.min(1.)
            },
        )
    }

    /// The zoom that shows the image at its own size.
    fn actual_zoom(&self) -> f32 {
        self.fit_scale().map_or(1., |fit| 1. / fit)
    }

    fn max_zoom(&self) -> f32 {
        MAX_ZOOM.max(self.actual_zoom())
    }

    fn image_bounds(&self, placement: Placement) -> Option<Bounds<Pixels>> {
        let natural = self.natural?;
        let scale = self.fit_scale()? * placement.zoom;
        let size = size(natural.width * scale, natural.height * scale);
        let centre = self.stage.center() + placement.offset;
        Some(Bounds {
            origin: point(centre.x - size.width / 2., centre.y - size.height / 2.),
            size,
        })
    }

    /// The zoom kept in range, and the image kept covering the stage on each
    /// axis where it is larger and centred on each where it is not.
    fn clamped(&self, placement: Placement) -> Placement {
        let zoom = placement.zoom.clamp(1., self.max_zoom());
        let centred = Placement {
            zoom,
            offset: Point::default(),
        };
        let Some(bounds) = self.image_bounds(centred) else {
            return centred;
        };
        let slack = |image: Pixels, stage: Pixels| ((image - stage) / 2.).max(Pixels::ZERO);
        let x = slack(bounds.size.width, self.stage.size.width);
        let y = slack(bounds.size.height, self.stage.size.height);
        Placement {
            zoom,
            offset: point(
                placement.offset.x.max(-x).min(x),
                placement.offset.y.max(-y).min(y),
            ),
        }
    }

    fn pannable(&self) -> bool {
        self.image_bounds(self.placement).is_some_and(|bounds| {
            bounds.size.width > self.stage.size.width || bounds.size.height > self.stage.size.height
        })
    }

    fn set_placement(&mut self, placement: Placement, animate: bool, cx: &mut Context<Self>) {
        self.placement = self.clamped(placement);
        self.animate = animate;
        cx.notify();
    }

    /// Zoom to `zoom` keeping the image point under `anchor` where it is,
    /// as far as the clamp allows.
    fn zoom_to(&mut self, zoom: f32, anchor: Point<Pixels>, animate: bool, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(1., self.max_zoom());
        let ratio = zoom / self.placement.zoom;
        let from_centre = anchor - self.stage.center();
        let offset = from_centre * (1. - ratio) + self.placement.offset * ratio;
        self.set_placement(Placement { zoom, offset }, animate, cx);
    }

    fn step(&mut self, factor: f32, cx: &mut Context<Self>) {
        let zoom = self.placement.zoom * factor;
        self.zoom_to(zoom, self.stage.center(), true, cx);
    }

    fn pan_by(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) {
        let offset = self.placement.offset + delta;
        self.set_placement(
            Placement {
                offset,
                ..self.placement
            },
            false,
            cx,
        );
    }

    /// A double-click or double-tap: back to the fit when zoomed, otherwise
    /// in to the image's own size, or twice the fit when that is nearer.
    fn toggle_zoom(&mut self, anchor: Point<Pixels>, cx: &mut Context<Self>) {
        let zoom = if self.placement.zoom > 1. {
            1.
        } else {
            self.actual_zoom().max(2.)
        };
        self.zoom_to(zoom, anchor, true, cx);
    }

    fn pressed(&mut self, event: &MouseDownEvent) {
        self.drag = Some(Drag {
            origin: event.position,
            last: event.position,
            moved: false,
        });
    }

    fn pointer_moved(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let delta = event.position - drag.last;
        drag.last = event.position;
        let travel = event.position - drag.origin;
        if !drag.moved && travel.x.abs() < DRAG_SLOP && travel.y.abs() < DRAG_SLOP {
            return;
        }
        drag.moved = true;
        self.pan_by(delta, cx);
    }

    fn clicked(&mut self, event: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_some_and(|drag| drag.moved) {
            return;
        }
        let position = event.position();
        if event.click_count() == 2 {
            self.toggle_zoom(position, cx);
            return;
        }
        let on_image = self
            .image_bounds(self.placement)
            .is_some_and(|bounds| bounds.contains(&position));
        if !on_image {
            self.cancel_original(cx);
            window.close_dialog(cx);
        }
    }

    /// A trackpad or touch pan moves the image; any other scroll is a wheel,
    /// and zooms about the pointer, as does a scroll with Control or the
    /// platform modifier held, as browsers do.
    fn scrolled(&mut self, event: &ScrollEvent, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        self.pan = match (event.touch_phase, self.pan) {
            (TouchPhase::Started, _) => Some(Pan::Touching),
            (TouchPhase::Ended | TouchPhase::Cancelled, Some(_)) => Some(Pan::Coasting(now)),
            (TouchPhase::Moved, Some(Pan::Touching)) => Some(Pan::Touching),
            (TouchPhase::Moved, Some(Pan::Coasting(last)))
                if now.saturating_duration_since(last) <= MOMENTUM_GAP =>
            {
                Some(Pan::Coasting(now))
            }
            _ => None,
        };
        let modified = event.modifiers.control || event.modifiers.platform;
        if self.pan.is_some() && !modified {
            self.pan_by(event.delta.pixel_delta(WHEEL_LINE), cx);
            return;
        }
        let factor = match event.delta {
            ScrollDelta::Lines(lines) => ZOOM_STEP.powf(lines.y),
            ScrollDelta::Pixels(pixels) if modified => (pixels.y / PINCH_PIXELS).exp(),
            ScrollDelta::Pixels(pixels) => ZOOM_STEP.powf(pixels.y / WHEEL_NOTCH),
        };
        if factor != 1. {
            self.zoom_to(self.placement.zoom * factor, event.position, false, cx);
        }
    }

    fn pinched(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        if event.delta > -1. {
            let zoom = self.placement.zoom * (1. + event.delta);
            self.zoom_to(zoom, event.position, false, cx);
        }
    }

    fn decode(&mut self, window: &mut Window, cx: &mut App) {
        let load = crate::motion::image_load(&self.source, window, cx);
        self.failed = load.settled() && load.size().is_none();
        let decoded = load.size();
        self.natural = match self.original_state {
            Some(OriginalState::Complete) => decoded,
            _ => decoded.and(self.original_size).or(decoded),
        };
    }
}

impl Render for ImageViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let insets = crate::window_seam::content_insets(window);
        let compact = crate::window_seam::window_is_compact(window, cx);
        self.stage = Bounds {
            origin: Point::default(),
            size: viewport,
        };
        // The frame is centred on the stage, so the band the toolbar takes
        // above it is mirrored below.
        let band = insets.top.max(insets.bottom)
            + px(crate::material::bar_height(
                crate::material::Bar::Strip,
                compact,
            ))
            + STAGE_MARGIN;
        self.frame = size(
            (viewport.width - insets.left - insets.right - STAGE_MARGIN * 2.).max(px(1.)),
            (viewport.height - band * 2.).max(px(1.)),
        );
        self.decode(window, cx);
        self.placement = self.clamped(self.placement);
        let policy = if self.animate {
            gpui_base::motion::Transition::new(GLIDE)
        } else {
            Transition::new(Duration::ZERO)
        };
        let shown = transition("image-viewer-placement", self.placement, policy, window, cx);
        let image = self.image_bounds(shown);
        let zoomed = self.placement.zoom > 1.;
        let percent = self
            .fit_scale()
            .map(|fit| format!("{}%", (fit * shown.zoom * 100.).round() as i32));
        let pannable = self.pannable();
        let muted = cx.theme().muted_foreground;

        // The image fades in once it has decoded, over what stood in for it.
        let arrived = transition(
            "image-viewer-arrived",
            if image.is_some() { 1. } else { 0. },
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        // The original fades in over the preview it replaces, in its place.
        let now = cx.background_executor().now();
        let swapped = match &self.replaced {
            Some((_, at)) if !cx.reduce_motion() => {
                crate::motion::Curve::new(GLIDE).progress(now.saturating_duration_since(*at))
            }
            _ => 1.,
        };
        let replaced = if swapped < 1. {
            window.request_animation_frame();
            self.replaced.clone()
        } else {
            self.replaced = None;
            None
        };
        let placeholder = image.is_none().then(|| {
            if self.failed
                || matches!(
                    self.original_state,
                    Some(OriginalState::Failed(_) | OriginalState::Cancelled)
                )
            {
                Placeholder::Message(match &self.original_state {
                    Some(OriginalState::Failed(message)) => message.clone(),
                    Some(OriginalState::Cancelled) => {
                        crate::tr!("image_viewer.cancelled").into_owned().into()
                    }
                    _ => crate::tr!("image_viewer.load_failed").into_owned().into(),
                })
            } else if self.target.is_none() || self.remote_progress {
                Placeholder::Loading
            } else {
                Placeholder::Glyph
            }
        });
        let placeholder =
            crate::motion::shown("image-viewer-placeholder", placeholder, SHOW, window, cx);
        let stage = div()
            .id("image-viewer-stage")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .when(pannable, |el| el.cursor_grab())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, _, _| this.pressed(event)),
            )
            .on_mouse_move(cx.listener(|this, event, _, cx| this.pointer_moved(event, cx)))
            .on_click(cx.listener(|this, event, window, cx| this.clicked(event, window, cx)))
            .on_scroll(cx.listener(|this, event, _, cx| this.scrolled(event, cx)))
            .on_pinch(cx.listener(|this, event, _, cx| this.pinched(event, cx)))
            .when_some(image.zip(replaced), |el, (bounds, (preview, _))| {
                el.child(
                    img(preview)
                        .id("image-viewer-replaced")
                        .absolute()
                        .left(bounds.origin.x)
                        .top(bounds.origin.y)
                        .w(bounds.size.width)
                        .h(bounds.size.height)
                        .opacity(arrived),
                )
            })
            .when_some(image, |el, bounds| {
                el.child(
                    img(self.source.clone())
                        .id("image-viewer-content")
                        .debug_selector(|| "image-viewer-image".into())
                        .absolute()
                        .left(bounds.origin.x)
                        .top(bounds.origin.y)
                        .w(bounds.size.width)
                        .h(bounds.size.height)
                        .opacity(arrived * swapped),
                )
            })
            .children(placeholder.map(|placeholder| {
                let state = placeholder.value.clone();
                placeholder.fade(crate::motion::cross_fade(
                    "image-viewer-placeholder-state",
                    state,
                    CHANGE,
                    window,
                    cx,
                    move |state, _, cx| match state {
                        Placeholder::Message(message) => div()
                            .text_token(&cx.theme().tokens.typography.sm)
                            .text_color(muted)
                            .child(message.clone())
                            .into_any_element(),
                        Placeholder::Loading => crate::widgets::Spinner::new().into_any_element(),
                        Placeholder::Glyph => crate::icon::Icon::default()
                            .path("icons/image.svg")
                            .into_any_element(),
                    },
                ))
            }));

        let fit_or_actual = if zoomed {
            crate::material::toolbar_icon_button(
                "image-viewer-fit",
                IconName::Minimize2,
                crate::tr!("image_viewer.fit").into_owned(),
                compact,
            )
            .on_click(cx.listener(|this, _, _, cx| this.set_placement(Placement::FIT, true, cx)))
        } else {
            crate::material::toolbar_icon_button(
                "image-viewer-actual-size",
                IconName::Maximize2,
                crate::tr!("image_viewer.actual_size").into_owned(),
                compact,
            )
            .disabled(self.actual_zoom() <= 1.)
            .on_click(cx.listener(|this, _, _, cx| {
                let zoom = this.actual_zoom();
                this.zoom_to(zoom, this.stage.center(), true, cx);
            }))
        };
        let show_transfer = (self.remote_progress
            || !matches!(self.original_state, Some(OriginalState::Loading { .. })))
            && matches!(
                self.original_state,
                Some(
                    OriginalState::Loading { .. }
                        | OriginalState::Cancelled
                        | OriginalState::Failed(_)
                )
            );
        let transfer = crate::motion::shown(
            "image-viewer-transfer",
            show_transfer.then(|| self.original_state.clone()).flatten(),
            SHOW,
            window,
            cx,
        );
        let transfer_controls = transfer.map(|transfer| {
            let panel = crate::material::overlay_contour(div(), cx)
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .gap(cx.theme().tokens.spacing.sm)
                .p(cx.theme().tokens.spacing.sm)
                .rounded(crate::material::radius_overlay(cx))
                .text_color(cx.theme().foreground)
                .occlude()
                .map(|panel| match &transfer.value {
                    OriginalState::Loading { received, total } => panel
                        .child(
                            div().text_secondary(false, cx).child(match total {
                                Some(total) => crate::tr!(
                                    "image_viewer.progress",
                                    received = received.to_string(),
                                    total = total.to_string()
                                )
                                .into_owned(),
                                None => crate::tr!("image_viewer.connecting").into_owned(),
                            }),
                        )
                        .child(
                            Button::new("image-viewer-cancel")
                                .secondary()
                                .small()
                                .label(crate::tr!("image_viewer.cancel"))
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_original(cx))),
                        ),
                    OriginalState::Cancelled | OriginalState::Failed(_) => panel.child(
                        Button::new("image-viewer-retry")
                            .secondary()
                            .small()
                            .label(crate::tr!("image_viewer.retry"))
                            .on_click(cx.listener(|this, _, _, cx| this.start_original(cx))),
                    ),
                    OriginalState::Complete => panel,
                });
            div()
                .absolute()
                .bottom(insets.bottom + STAGE_MARGIN)
                .left(insets.left + STAGE_MARGIN)
                .right(insets.right + STAGE_MARGIN)
                .flex()
                .flex_col()
                .child(transfer.fade(panel))
        });

        let traffic_lights = crate::window_caption::traffic_light_inset(window);
        let bar = crate::material::bar_divider_below(
            crate::material::bar(
                "image-viewer-toolbar",
                crate::material::Bar::Strip,
                compact,
                cx,
            ),
            cx,
        )
        .when(traffic_lights > Pixels::ZERO, |bar| bar.pl(traffic_lights))
        .bg(cx.theme().popover)
        .child(crate::material::bar_title(self.title.clone(), cx))
        .child(
            crate::material::toolbar_icon_button(
                "image-viewer-zoom-out",
                IconName::ZoomOut,
                crate::tr!("image_viewer.zoom_out").into_owned(),
                compact,
            )
            .disabled(!zoomed)
            .on_click(cx.listener(|this, _, _, cx| this.step(1. / ZOOM_STEP, cx))),
        )
        .child(
            // Fixed so the zoom controls hold still as the percentage changes.
            div()
                .w(px(48.))
                .text_center()
                .text_secondary(false, cx)
                .children(percent),
        )
        .child(
            crate::material::toolbar_icon_button(
                "image-viewer-zoom-in",
                IconName::ZoomIn,
                crate::tr!("image_viewer.zoom_in").into_owned(),
                compact,
            )
            .disabled(self.placement.zoom >= self.max_zoom())
            .on_click(cx.listener(|this, _, _, cx| this.step(ZOOM_STEP, cx))),
        )
        .child(fit_or_actual)
        .child(
            crate::material::toolbar_icon_button(
                "image-viewer-close",
                IconName::Close,
                crate::tr!("image_viewer.close").into_owned(),
                compact,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.cancel_original(cx);
                window.close_dialog(cx);
            })),
        );
        let toolbar = div()
            .absolute()
            .top(insets.top)
            .left(insets.left)
            .right(insets.right)
            .occlude()
            .child(bar);

        div()
            .id("image-viewer")
            .track_focus(&self.focus_handle)
            .key_context(CONTEXT)
            .size_full()
            .relative()
            .overflow_hidden()
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.step(ZOOM_STEP, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.step(1. / ZOOM_STEP, cx)))
            .on_action(cx.listener(|this, _: &ZoomToFit, _, cx| {
                this.set_placement(Placement::FIT, true, cx)
            }))
            .on_action(cx.listener(|this, _: &ActualSize, _, cx| {
                let zoom = this.actual_zoom();
                this.zoom_to(zoom, this.stage.center(), true, cx);
            }))
            .child(stage)
            .child(toolbar)
            .children(transfer_controls)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use gpui::{RenderImage, TestAppContext, VisualTestContext};
    use image::{Frame, ImageBuffer, Rgba};

    struct Body;

    impl Render for Body {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    fn open_viewer(cx: &mut TestAppContext, width: u32, height: u32) -> &mut VisualTestContext {
        cx.update(crate::theme::init);
        cx.update(init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let body = cx.new(|_| Body);
            gpui_base::Root::new(body, window, cx)
        });
        cx.simulate_resize(size(px(800.), px(600.)));
        let frame = Frame::new(ImageBuffer::from_pixel(width, height, Rgba([0, 0, 0, 255])));
        let image = Arc::new(RenderImage::new(vec![frame]));
        cx.update(|window, cx| open(ImageSource::Render(image), "shot.png", window, cx));
        draw(cx);
        cx
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            _ = window.draw(cx);
        });
    }

    fn image(cx: &mut VisualTestContext) -> Bounds<Pixels> {
        cx.debug_bounds("image-viewer-image")
            .expect("the viewer shows its image")
    }

    /// An original's bytes replace its preview: the decoded original, smaller
    /// than the frame, is shown at its own size, which neither the preview at
    /// its own size nor the preview standing in at the frame's size has.
    #[gpui::test]
    fn a_delivered_original_replaces_its_preview(cx: &mut TestAppContext) {
        use tcode_client::original::{
            OpenedOriginal, OriginalFuture, OriginalOpener, OriginalReader,
        };
        struct Bytes(Option<Vec<u8>>);
        impl OriginalReader for Bytes {
            fn read(&mut self) -> OriginalFuture<'_, Option<Vec<u8>>> {
                let block = self.0.take();
                Box::pin(async move { Ok(block) })
            }
            fn cancel(&mut self) {}
        }
        struct Delivered(Vec<u8>);
        impl OriginalOpener for Delivered {
            fn open(
                &self,
                _: tcode_protocol::OriginalTarget,
            ) -> OriginalFuture<'static, OpenedOriginal> {
                let bytes = self.0.clone();
                Box::pin(async move {
                    Ok(OpenedOriginal {
                        metadata: tcode_protocol::OriginalMetadata {
                            len: bytes.len() as u64,
                            mime: Some("image/png".into()),
                            dimensions: None,
                        },
                        reader: Box::new(Bytes(Some(bytes))),
                    })
                })
            }
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(400, 300, Rgba([0, 0, 255, 255]))
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode the original");
        let cx = open_viewer(cx, 10, 10);
        cx.simulate_keystrokes("escape");
        draw(cx);
        let (to_host, _control) = async_channel::unbounded();
        let (_replies, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new_local(
            to_host,
            from_host,
            Some(Arc::new(Delivered(png.into_inner()))),
        );
        cx.update(|window, cx| {
            let host = HostImages::open(link, cx);
            let preview = Arc::new(RenderImage::new(vec![Frame::new(ImageBuffer::from_pixel(
                120,
                90,
                Rgba([255, 0, 0, 255]),
            ))]));
            open(
                ViewerSource::original(
                    host,
                    tcode_protocol::OriginalTarget::File {
                        path: "/host/delivered.png".into(),
                    },
                    ImageSource::Render(preview),
                ),
                "delivered.png",
                window,
                cx,
            );
        });
        draw(cx);
        assert_eq!(image(cx).size, size(px(400.), px(300.)));
    }

    #[gpui::test]
    fn escape_closes_the_viewer_and_drops_its_transfer(cx: &mut TestAppContext) {
        let cx = open_viewer(cx, 1600, 800);
        assert!(cx.debug_bounds("image-viewer-image").is_some());

        cx.simulate_keystrokes("escape");
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        assert!(cx.debug_bounds("image-viewer-image").is_none());

        // Contract: Escape cancels an in-flight original even while a dialog
        // frame still holds the viewer's callbacks.
        use tcode_client::original::{
            OpenedOriginal, OriginalFuture, OriginalOpener, OriginalReader,
        };
        struct PendingReader(async_channel::Receiver<Vec<u8>>);
        impl OriginalReader for PendingReader {
            fn read(&mut self) -> OriginalFuture<'_, Option<Vec<u8>>> {
                Box::pin(async move {
                    self.0
                        .recv()
                        .await
                        .map(Some)
                        .map_err(tcode_client::original::error)
                })
            }
            fn cancel(&mut self) {}
        }
        struct PendingOriginal {
            source: std::sync::Mutex<Option<async_channel::Receiver<Vec<u8>>>>,
            opened: async_channel::Sender<()>,
        }
        impl OriginalOpener for PendingOriginal {
            fn open(
                &self,
                _: tcode_protocol::OriginalTarget,
            ) -> OriginalFuture<'static, OpenedOriginal> {
                let source = self.source.lock().unwrap().take().unwrap();
                self.opened.try_send(()).unwrap();
                Box::pin(async move {
                    Ok(OpenedOriginal {
                        metadata: tcode_protocol::OriginalMetadata {
                            len: 1024,
                            mime: Some("image/png".into()),
                            dimensions: None,
                        },
                        reader: Box::new(PendingReader(source)),
                    })
                })
            }
        }
        let (blocks, source) = async_channel::bounded(2);
        let (opened, requests) = async_channel::bounded(1);
        let (to_host, _control) = async_channel::unbounded();
        let (_replies, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new_remote(
            to_host,
            from_host,
            Some(Arc::new(PendingOriginal {
                source: std::sync::Mutex::new(Some(source)),
                opened,
            })),
        );
        cx.update(|window, cx| {
            let host = HostImages::open(link, cx);
            let preview = Arc::new(RenderImage::new(vec![Frame::new(ImageBuffer::from_pixel(
                240,
                120,
                Rgba([255, 0, 0, 255]),
            ))]));
            open(
                ViewerSource::original(
                    host,
                    tcode_protocol::OriginalTarget::File {
                        path: "/host/pending.png".into(),
                    },
                    ImageSource::Render(preview),
                ),
                "pending.png",
                window,
                cx,
            );
        });
        draw(cx);
        assert!(
            cx.debug_bounds("image-viewer-image").is_some(),
            "the loaded preview remains visible while the original is pending"
        );
        requests
            .try_recv()
            .expect("explicit open starts the viewer transfer");
        assert!(!blocks.is_closed());
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(blocks.is_closed(), "Escape must drop the transfer reader");
    }
}
