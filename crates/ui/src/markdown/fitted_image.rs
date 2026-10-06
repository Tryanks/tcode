//! Block images scaled to the available width with their aspect ratio kept.
//!
//! GPUI's `img` turns an auto width and height into the image's pixel size;
//! a `max_w` then narrows the box but leaves its height, and `ObjectFit`
//! letterboxes the picture inside it. This element gives `img` a size that
//! is already proportional, until `img` derives its height from a clamped
//! width itself.

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId, ImageSource,
    InspectorElementId, IntoElement, LayoutId, ObjectFit, Pixels, Size, Style, Styled,
    StyledImage as _, Window, img, px, relative, size,
};

const MAX_HEIGHT: Pixels = px(720.);
const MIN_SIDE: Pixels = px(15.);

pub(super) struct FittedImage<E> {
    source: ImageSource,
    image: Option<E>,
}

impl<E: IntoElement + Styled + 'static> FittedImage<E> {
    /// `image` is the styled `img` of `source` that is painted; its width
    /// and height are set here.
    pub(super) fn new(source: ImageSource, image: E) -> Self {
        Self {
            source,
            image: Some(image),
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
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let intrinsic = img(self.source.clone())
            .object_fit(ObjectFit::Contain)
            .into_any_element()
            .layout_as_root(AvailableSpace::min_size(), window, cx);
        let mut style = Style::default();
        style.max_size.width = relative(1.).into();
        let layout_id = window.request_measured_layout(
            style,
            move |known_dimensions, available_space, _, _| {
                let available_width = known_dimensions.width.or(match available_space.width {
                    AvailableSpace::Definite(width) => Some(width),
                    AvailableSpace::MinContent | AvailableSpace::MaxContent => None,
                });
                fit(intrinsic, available_width)
            },
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let mut image = self
            .image
            .take()?
            .w(bounds.size.width)
            .h(bounds.size.height)
            .into_any_element();
        image.prepaint_as_root(
            bounds.origin,
            size(
                AvailableSpace::Definite(bounds.size.width),
                AvailableSpace::Definite(bounds.size.height),
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
/// loaded keeps a small square so it can still be clicked.
fn fit(intrinsic: Size<Pixels>, available_width: Option<Pixels>) -> Size<Pixels> {
    if intrinsic.width <= Pixels::ZERO || intrinsic.height <= Pixels::ZERO {
        return size(MIN_SIDE, MIN_SIDE);
    }
    let ratio = intrinsic.height / intrinsic.width;
    let width = available_width
        .map_or(intrinsic.width, |available| intrinsic.width.min(available))
        .min(MAX_HEIGHT / ratio);
    size(width.max(MIN_SIDE), (width * ratio).max(MIN_SIDE))
}
