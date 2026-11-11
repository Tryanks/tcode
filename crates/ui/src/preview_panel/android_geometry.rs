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
