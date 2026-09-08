//! Android child coordinates are physical window pixels. The shell's canvas
//! bounds already include its safe-area padding; callers convert them to
//! content-relative bounds before applying this mapping.
use gpui::{Bounds, Pixels, Point};

pub(super) fn physical_bounds(
    content_bounds: Bounds<Pixels>,
    safe_origin: Point<Pixels>,
    scale: f32,
) -> [i32; 4] {
    let origin = content_bounds.origin + safe_origin;
    let left = (f32::from(origin.x) * scale).round() as i32;
    let top = (f32::from(origin.y) * scale).round() as i32;
    let right = (f32::from(origin.x + content_bounds.size.width) * scale).round() as i32;
    let bottom = (f32::from(origin.y + content_bounds.size.height) * scale).round() as i32;
    [left, top, (right - left).max(0), (bottom - top).max(0)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};
    #[test]
    fn physical_child_tracks_safe_content_without_double_inset() {
        // Portrait: 24 logical pixels of status bar + 100 of shell chrome.
        let portrait = Bounds {
            origin: point(px(8.), px(100.)),
            size: size(px(384.), px(500.)),
        };
        assert_eq!(
            physical_bounds(portrait, point(px(0.), px(24.)), 3.),
            [24, 372, 1152, 1500]
        );
        // Landscape cutout moves to the left; the canvas already moved with it.
        let landscape = Bounds {
            origin: point(px(8.), px(100.)),
            size: size(px(700.), px(240.)),
        };
        assert_eq!(
            physical_bounds(landscape, point(px(44.), px(0.)), 2.),
            [104, 200, 1400, 480]
        );
    }
    #[test]
    fn fractional_scale_rounds_edges_and_collapsed_view_has_no_area() {
        let bounds = Bounds {
            origin: point(px(1.), px(1.)),
            size: size(px(1.), px(0.)),
        };
        assert_eq!(physical_bounds(bounds, Point::default(), 1.5), [2, 2, 1, 0]);
    }
}
