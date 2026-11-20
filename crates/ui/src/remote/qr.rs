use gpui::{
    AnyElement, App, ElementId, IntoElement as _, ParentElement as _, Styled as _, Window, div, px,
};
use gpui_base::{h_flex, v_flex};
use qrcode::render::svg;

use crate::theme::ActiveTheme as _;

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// A link's QR next to the text about it. Compact stacks the QR over the
/// text rather than putting a fixed-size image beside text that then has
/// nowhere to wrap; a wide layout too narrow for both wraps the QR under it.
/// A new payload cross-fades from the code `id` showed before; the QR and the
/// text glide, through `reflow`, to where a change of window or a wrap puts
/// them.
pub(super) fn beside_qr(
    id: impl Into<ElementId>,
    reflow: crate::motion::reflow::Reflow,
    text: impl gpui::IntoElement,
    payload: &str,
    compact: bool,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Div {
    let id = id.into();
    let qr = qrcode::QrCode::new(payload.as_bytes()).is_ok().then(|| {
        let qr = crate::motion::cross_fade(
            id,
            payload.to_owned(),
            CHANGE,
            window,
            cx,
            |payload, _, cx| {
                qr_element(payload, compact, cx).unwrap_or_else(|| div().into_any_element())
            },
        );
        reflow.part("qr", qr)
    });
    let layout = if compact {
        v_flex()
            .items_center()
            .children(qr)
            .child(reflow.part("text", text))
    } else {
        h_flex()
            .flex_wrap()
            .items_start()
            // The narrowest the text column gets before the QR wraps under it;
            // a layout breakpoint, not a size any owner supplies.
            .child(reflow.part("text", div().flex_1().min_w(px(200.)).child(text)))
            .children(qr)
    };
    let layout = layout.w_full().gap(crate::material::section_gap(cx));
    div().w_full().child(reflow.render(layout))
}

/// A module is 4 points on a compact layout, where the code is held up to
/// another phone's camera, and 3 on a wide one, where it sits beside its text
/// without outgrowing it. Drawn as a single SVG rather than laid-out boxes so
/// fractional window scales cannot shear rows across independently rounded
/// module runs.
pub(super) fn qr_element(payload: &str, compact: bool, cx: &App) -> Option<AnyElement> {
    let module: f32 = if compact { 4. } else { 3. };
    let code = qrcode::QrCode::new(payload.as_bytes()).ok()?;
    let width = code.width() as f32;
    let svg_text = code
        .render::<svg::Color>()
        .quiet_zone(false)
        .module_dimensions(1, 1)
        .dark_color(svg::Color("#000"))
        // The background rect would otherwise fill the whole mask.
        .light_color(svg::Color("none"))
        .build();
    // A QR is scanned by a camera, not read by a human: it must stay black on
    // white in both themes, so neither colour comes from the palette.
    let graphic = gpui::svg()
        .data(svg_text.as_bytes())
        .size(px(width * module))
        .text_color(gpui::black());
    Some(
        div()
            .flex_none()
            .p(px(crate::material::CARD_INSET))
            .rounded(crate::material::radius_card(cx))
            .bg(gpui::white())
            // On a light card the white would not show where the code ends.
            .border_1()
            .border_color(cx.theme().border)
            .child(graphic)
            .into_any_element(),
    )
}
