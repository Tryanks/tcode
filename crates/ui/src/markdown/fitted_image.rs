//! Block images scaled to the available width with their aspect ratio kept.
//!
//! GPUI's `img` turns an auto width and height into the image's pixel size;
//! a `max_w` then narrows the box but leaves its height, and `ObjectFit`
//! letterboxes the picture inside it. This element gives `img` a size that
//! is already proportional, until `img` derives its height from a clamped
//! width itself.
//!
//! The picture fades in over its tile once it decodes, and the block's size
//! glides from the tile's to the picture's, so the blocks after it move down
//! rather than jump: `img` reports neither.

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, Entity, GlobalElementId,
    ImageSource, InspectorElementId, IntoElement, LayoutId, ObjectFit, Pixels, Size, Style, Styled,
    StyledImage as _, WeakEntity, Window, div, img, px, relative, size,
};

use super::state::MarkdownState;
use crate::motion::GlideState;

/// How long a picture drawn here takes to fade in once it has decoded.
const ARRIVE: std::time::Duration = std::time::Duration::from_millis(200);

pub(super) const MAX_HEIGHT: Pixels = px(720.);
const MIN_SIDE: Pixels = px(15.);
/// The size of the tile drawn for an image without a preview.
const UNPREVIEWED: Size<Pixels> = Size {
    width: px(120.),
    height: px(80.),
};

pub(super) struct FittedImage<E> {
    source: ImageSource,
    image: Option<E>,
    owner: WeakEntity<MarkdownState>,
    path: String,
    revision: usize,
    previous_size: Option<Size<Pixels>>,
    glide: GlideState,
    motion: crate::motion::owned::Owned,
}

/// What a fitted image found while it laid out: the picture's own size, and
/// how far the picture is in over its tile.
pub(super) struct FittedLayout {
    intrinsic: Size<Pixels>,
    arrived: f32,
}

impl<E: IntoElement + Styled + 'static> FittedImage<E> {
    /// `image` is the styled `img` of `source` that is painted; its width
    /// and height are set here.
    pub(super) fn new(
        source: ImageSource,
        image: E,
        owner: &Entity<MarkdownState>,
        path: String,
        cx: &App,
    ) -> Self {
        let (revision, previous_size) = owner.read(cx).image_layout(&path);
        let glide = owner.read(cx).glide(&path);
        let motion = owner.read(cx).motion();
        Self {
            source,
            image: Some(image),
            owner: owner.downgrade(),
            path,
            revision,
            previous_size,
            glide,
            motion,
        }
    }
}

impl<E: IntoElement + Styled + 'static> IntoElement for FittedImage<E> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: IntoElement + Styled + 'static> Element for FittedImage<E> {
    type RequestLayoutState = FittedLayout;
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
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let intrinsic = img(self.source.clone())
            .object_fit(ObjectFit::Contain)
            .with_fallback(|| {
                div()
                    .w(UNPREVIEWED.width)
                    .h(UNPREVIEWED.height)
                    .into_any_element()
            })
            .into_any_element()
            .layout_as_root(AvailableSpace::min_size(), window, cx);
        if self.previous_size != Some(intrinsic) {
            let owner = self.owner.clone();
            let path = self.path.clone();
            let revision = self.revision;
            // ListState can be borrowed while its children lay out. Invalidate
            // its cached heights after this frame, not from inside that borrow.
            cx.defer(move |cx| {
                let _ = owner.update(cx, |state, cx| {
                    state.image_resized(path, revision, intrinsic, cx)
                });
            });
        }
        // The picture fades in over its tile once it decodes, and the block
        // glides from the tile's size to the picture's.
        let arrived = crate::motion::image_arrival(
            crate::motion::Clock::from(&self.motion),
            format!("markdown-image-{}", self.path),
            &self.source,
            ARRIVE,
            window,
            cx,
        );
        let shows = crate::motion::size_key(intrinsic);
        let glide = self.glide.clone();
        let mut style = Style::default();
        style.max_size.width = relative(1.).into();
        let layout_id = window.request_measured_layout(
            style,
            move |known_dimensions, available_space, _, cx| {
                let available_width = known_dimensions.width.or(match available_space.width {
                    AvailableSpace::Definite(width) => Some(width),
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                });
                glide.toward(shows, fit(intrinsic, available_width), cx)
            },
        );
        (layout_id, FittedLayout { intrinsic, arrived })
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.glide.painted_at(bounds.size, window);
        // At rest the block is the fitted size; while it glides the picture
        // is contained in the block's size on the way.
        let fitted = if self.glide.moving(cx.background_executor().now()) {
            bounds.size
        } else {
            fit(layout.intrinsic, Some(bounds.size.width))
        };
        let mut image = self
            .image
            .take()?
            .w(fitted.width)
            .h(fitted.height)
            .opacity(layout.arrived)
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
        _: &mut Self::RequestLayoutState,
        image: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(image) = image {
            image.paint(window, cx);
        }
    }
}

/// The largest size with `intrinsic`'s aspect ratio that fits the available
/// width and [`MAX_HEIGHT`], never larger than `intrinsic` itself. An image that has not
/// loaded holds the tile drawn for an image without a preview, the size it keeps if it
/// never loads; the host's preview does not report its size before it is decoded.
pub(super) fn fit(intrinsic: Size<Pixels>, available_width: Option<Pixels>) -> Size<Pixels> {
    if intrinsic.width <= Pixels::ZERO || intrinsic.height <= Pixels::ZERO {
        return fit(UNPREVIEWED, available_width);
    }
    let ratio = intrinsic.height / intrinsic.width;
    let width = available_width
        .map_or(intrinsic.width, |available| intrinsic.width.min(available))
        .min(MAX_HEIGHT / ratio);
    size(width.max(MIN_SIDE), (width * ratio).max(MIN_SIDE))
}
