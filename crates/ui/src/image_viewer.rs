//! The window-level image viewer: one image on a host-filling dialog, zoomed
//! and panned by wheel, trackpad, pinch, drag, touch, keyboard and a toolbar.
//! Shared by the composer's pending strip, sent-message thumbnails, Markdown
//! images and image-link badges.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, Bounds, ClickEvent, Context, FocusHandle, ImageCacheError, ImageSource,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, PinchEvent, Pixels, Point, Render, RenderImage, ScrollDelta,
    ScrollWheelEvent, SharedString, Size, StatefulInteractiveElement as _, Styled as _, Window,
    actions, div, img, point, prelude::FluentBuilder as _, px, size,
};
use gpui_base::motion::{Interpolate, Transition, transition};

use crate::icon::IconName;
use crate::overlay::OverlayExt as _;
use crate::theme::ActiveTheme as _;

actions!(image_viewer, [ZoomIn, ZoomOut, ZoomToFit, ActualSize]);

const CONTEXT: &str = "ImageViewer";

/// Zoom gained by one wheel notch, one key press or one toolbar press.
const ZOOM_STEP: f32 = 1.25;
/// Zoom relative to the fitted size past which zooming stops. The image's
/// own size stays reachable even when that lies further out.
const MAX_ZOOM: f32 = 8.;
/// What one wheel line is worth, so pixel-precise trackpad deltas zoom at
/// the same rate as notched wheels.
const WHEEL_LINE: Pixels = px(20.);
/// Pointer travel before a press is a drag rather than the click it began as.
const DRAG_SLOP: Pixels = px(4.);
/// The toolbar is one of the shell's top strips, the height the window's
/// own controls expect.
const TOOLBAR_HEIGHT: Pixels = px(crate::window_caption::CAPTION_STRIP_HEIGHT);
const STAGE_MARGIN: Pixels = px(16.);
/// How long keys, buttons and double-clicks take to reach their placement.
const GLIDE: Duration = Duration::from_millis(200);

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("shift-=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("secondary-=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("-", ZoomOut, Some(CONTEXT)),
        KeyBinding::new("secondary--", ZoomOut, Some(CONTEXT)),
        KeyBinding::new("0", ZoomToFit, Some(CONTEXT)),
        KeyBinding::new("secondary-0", ZoomToFit, Some(CONTEXT)),
        KeyBinding::new("1", ActualSize, Some(CONTEXT)),
        KeyBinding::new("secondary-1", ActualSize, Some(CONTEXT)),
    ]);
}

/// Open `source` in the viewer on the window's dialog stack, where it
/// inherits Escape dismissal and focus restoration.
pub(crate) fn open(
    source: ImageSource,
    title: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    let viewer = cx.new(|cx| ImageViewer::new(source, title.into(), cx));
    let focus = viewer.read(cx).focus_handle.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.fill().close_button(false).child(viewer.clone())
    });
    // The dialog focused its own host; the viewer's key bindings sit below it.
    focus.focus(window, cx);
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
    /// Keys, buttons and double-clicks glide to their placement; wheel,
    /// pinch and drag track the pointer.
    animate: bool,
    /// The last frame's geometry, so pointer input resolves against what was
    /// drawn: the stage is the whole dialog host, the frame is the area a
    /// fitted image fills, and `natural` is the decoded image's own size.
    stage: Bounds<Pixels>,
    frame: Size<Pixels>,
    natural: Option<Size<Pixels>>,
    failed: bool,
}

impl ImageViewer {
    fn new(source: ImageSource, title: SharedString, cx: &mut Context<Self>) -> Self {
        Self {
            source,
            title,
            focus_handle: cx.focus_handle(),
            placement: Placement::FIT,
            drag: None,
            animate: false,
            stage: Bounds::default(),
            frame: Size::default(),
            natural: None,
            failed: false,
        }
    }

    /// The scale at which the image fits the frame. A smaller image is shown
    /// at its own size rather than enlarged.
    fn fit_scale(&self) -> Option<f32> {
        let natural = self.natural?;
        if natural.width <= Pixels::ZERO
            || natural.height <= Pixels::ZERO
            || self.frame.width <= Pixels::ZERO
            || self.frame.height <= Pixels::ZERO
        {
            return None;
        }
        Some(
            (self.frame.width / natural.width)
                .min(self.frame.height / natural.height)
                .min(1.),
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
            window.close_dialog(cx);
        }
    }

    /// A notched wheel zooms; a trackpad or touch pan moves the image, or
    /// zooms when Control or the platform modifier is held, as browsers do.
    fn scrolled(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(WHEEL_LINE);
        let zooms = event.modifiers.control
            || event.modifiers.platform
            || matches!(event.delta, ScrollDelta::Lines(_));
        if zooms {
            let notches = delta.y / WHEEL_LINE;
            if notches != 0. {
                let zoom = self.placement.zoom * ZOOM_STEP.powf(notches);
                self.zoom_to(zoom, event.position, false, cx);
            }
        } else {
            self.pan_by(delta, cx);
        }
    }

    fn pinched(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        if event.delta > -1. {
            let zoom = self.placement.zoom * (1. + event.delta);
            self.zoom_to(zoom, event.position, false, cx);
        }
    }

    fn decode(&mut self, window: &mut Window, cx: &mut App) {
        let decoded = match &self.source {
            ImageSource::Resource(resource) => {
                window.use_asset::<gpui::ImgResourceLoader>(resource, cx)
            }
            ImageSource::Custom(load) => load(window, cx),
            ImageSource::Render(image) => Some(Ok(image.clone())),
            ImageSource::Image(image) => image.clone().use_render_image(window, cx).map(Ok),
        };
        let natural = decoded.map(|result: Result<Arc<RenderImage>, ImageCacheError>| {
            result
                .ok()
                .map(|image| image.size(0))
                .filter(|size| size.width.0 > 0 && size.height.0 > 0)
        });
        self.failed = matches!(natural, Some(None));
        self.natural = natural
            .flatten()
            .map(|size| gpui::size(px(size.width.0 as f32), px(size.height.0 as f32)));
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
        let band = insets.top.max(insets.bottom) + TOOLBAR_HEIGHT + STAGE_MARGIN;
        self.frame = size(
            (viewport.width - insets.left - insets.right - STAGE_MARGIN * 2.).max(px(1.)),
            (viewport.height - band * 2.).max(px(1.)),
        );
        self.decode(window, cx);
        self.placement = self.clamped(self.placement);
        let policy = Transition::new(if self.animate { GLIDE } else { Duration::ZERO });
        let shown = transition("image-viewer-placement", self.placement, policy, window, cx);
        let image = self.image_bounds(shown);
        let zoomed = self.placement.zoom > 1.;
        let percent = self
            .fit_scale()
            .map(|fit| format!("{}%", (fit * shown.zoom * 100.).round() as i32));
        let pannable = self.pannable();
        let muted = cx.theme().muted_foreground;

        let stage = div()
            .id("image-viewer-stage")
            .absolute()
            .inset_0()
            .occlude()
            .when(pannable, |el| el.cursor_grab())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, _, _| this.pressed(event)),
            )
            .on_mouse_move(cx.listener(|this, event, _, cx| this.pointer_moved(event, cx)))
            .on_click(cx.listener(|this, event, window, cx| this.clicked(event, window, cx)))
            .on_scroll_wheel(cx.listener(|this, event, _, cx| this.scrolled(event, cx)))
            .on_pinch(cx.listener(|this, event, _, cx| this.pinched(event, cx)))
            .when_some(image, |el, bounds| {
                el.child(
                    img(self.source.clone())
                        .debug_selector(|| "image-viewer-image".into())
                        .absolute()
                        .left(bounds.origin.x)
                        .top(bounds.origin.y)
                        .w(bounds.size.width)
                        .h(bounds.size.height),
                )
            })
            .when(image.is_none(), |el| {
                el.flex().items_center().justify_center().map(|el| {
                    if self.failed {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(crate::tr!("image_viewer.load_failed").into_owned()),
                        )
                    } else {
                        el.child(crate::widgets::Spinner::new())
                    }
                })
            });

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
        let toolbar = div()
            .absolute()
            .top(insets.top)
            .left(insets.left)
            .right(insets.right)
            .h(TOOLBAR_HEIGHT)
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .pl(crate::window_caption::traffic_light_inset(window).max(px(8.)))
            .occlude()
            .bg(cx.theme().popover)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .child(self.title.clone()),
            )
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
                div()
                    .w(px(48.))
                    .text_center()
                    .text_xs()
                    .text_color(muted)
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
                .on_click(|_, window, cx| window.close_dialog(cx)),
            );

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, TouchPhase, VisualTestContext};
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

    #[gpui::test]
    fn pointer_gestures_zoom_about_the_pointer_pan_within_the_stage_and_close(
        cx: &mut TestAppContext,
    ) {
        // 400×200 fits the 800×600 window untouched, centred on (400, 300).
        let cx = open_viewer(cx, 400, 200);
        assert_eq!(
            image(cx),
            Bounds::new(point(px(200.), px(200.)), size(px(400.), px(200.)))
        );

        // One wheel notch at the centre grows the image a step, still centred.
        cx.simulate_event(ScrollWheelEvent {
            position: point(px(400.), px(300.)),
            delta: ScrollDelta::Lines(point(0., 1.)),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
        draw(cx);
        assert_eq!(
            image(cx),
            Bounds::new(point(px(150.), px(175.)), size(px(500.), px(250.)))
        );

        // A pinch at the image's corner doubles it about that corner; now
        // wider than the stage, it is held flush to the left edge instead of
        // leaving a gap, and still centred vertically.
        cx.simulate_event(PinchEvent {
            position: point(px(150.), px(175.)),
            delta: 1.,
            modifiers: Modifiers::default(),
            phase: TouchPhase::Moved,
        });
        draw(cx);
        assert_eq!(
            image(cx),
            Bounds::new(point(px(0.), px(50.)), size(px(1000.), px(500.)))
        );

        // Dragging pans, and the release of a drag is not a click.
        cx.simulate_mouse_down(
            point(px(400.), px(300.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(px(300.), px(300.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(300.), px(300.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        draw(cx);
        assert_eq!(image(cx).origin, point(px(-100.), px(50.)));

        // A trackpad or touch pan moves the image too.
        cx.simulate_event(ScrollWheelEvent {
            position: point(px(400.), px(300.)),
            delta: ScrollDelta::Pixels(point(px(-50.), px(0.))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
        draw(cx);
        assert_eq!(image(cx).origin, point(px(-150.), px(50.)));

        // A click beside the image closes the viewer.
        cx.simulate_click(point(px(400.), px(580.)), Modifiers::default());
        draw(cx);
        assert!(cx.debug_bounds("image-viewer-image").is_none());
    }

    #[gpui::test]
    fn keys_glide_between_fit_and_actual_size_and_escape_closes(cx: &mut TestAppContext) {
        // 1600×800 fits the 768×480 frame at 0.48: 768×384, centred.
        let cx = open_viewer(cx, 1600, 800);
        let fitted = Bounds::new(point(px(16.), px(108.)), size(px(768.), px(384.)));
        assert_eq!(image(cx), fitted);

        cx.simulate_keystrokes("1");
        draw(cx);
        assert_eq!(image(cx), fitted, "the glide starts from the fit");
        // The frame after the key press began the glide; the next ends it.
        cx.executor().advance_clock(GLIDE);
        draw(cx);
        assert_eq!(
            image(cx),
            Bounds::new(point(px(-400.), px(-100.)), size(px(1600.), px(800.)))
        );

        cx.simulate_keystrokes("0");
        draw(cx);
        cx.executor().advance_clock(GLIDE);
        draw(cx);
        assert_eq!(image(cx), fitted);

        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(cx.debug_bounds("image-viewer-image").is_none());
    }
}
