use gpui::{AnyElement, IntoElement as _, ParentElement as _, Styled as _, div, px};
use gpui_base::{h_flex, v_flex};
/// A QR code as `(width_in_modules, dark_module_flags)`, row-major.
fn qr_modules(payload: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(payload.as_bytes()).ok()?;
    let width = code.width();
    let modules = code
        .into_colors()
        .into_iter()
        .map(|color| color == qrcode::Color::Dark)
        .collect();
    Some((width, modules))
}

/// Paint the matrix as one flex row per module row, collapsing consecutive
/// same-colour modules into a single box — a per-module element would be
/// thousands of nodes repainting every countdown tick.
pub(super) fn qr_element(payload: &str) -> Option<AnyElement> {
    const MODULE: f32 = 4.;
    const QUIET: f32 = 12.;
    let (width, modules) = qr_modules(payload)?;
    // A QR is scanned by a camera, not read by a human: it must stay black on
    // white in both themes, so neither colour comes from the palette.
    let dark = gpui::black();
    let mut grid = v_flex().flex_none();
    for row in modules.chunks(width) {
        let mut line = h_flex().flex_none().h(px(MODULE));
        let mut start = 0;
        while start < row.len() {
            let mut end = start + 1;
            while end < row.len() && row[end] == row[start] {
                end += 1;
            }
            // The row centers its children, so a run without an explicit
            // height would collapse to nothing and paint no modules at all.
            let run = div()
                .flex_none()
                .h(px(MODULE))
                .w(px((end - start) as f32 * MODULE));
            line = line.child(if row[start] { run.bg(dark) } else { run });
            start = end;
        }
        grid = grid.child(line);
    }
    Some(
        div()
            .flex_none()
            .p(px(QUIET))
            .rounded(crate::material::radius_card())
            .bg(gpui::white())
            .child(grid)
            .into_any_element(),
    )
}
