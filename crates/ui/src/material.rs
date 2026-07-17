//! The "Frosted Instrument" material system (docs/visual-redesign.md).
//!
//! The window canvas (theme `background`) is the only intentionally translucent
//! tier; reading surfaces sit on top of it at near-full opacity so body text
//! never lands on the raw blur. Semantic colors stay on `cx.theme()` — this
//! module owns the material tiers, the role-based radius scale, and the few
//! light effects (faded hairlines, the primary-button top light) the spec
//! defines.

use gpui::{
    App, Div, Hsla, IntoElement, ParentElement as _, Pixels, Rgba, Styled as _, div,
    linear_color_stop, linear_gradient, px,
};
use gpui_component::ActiveTheme as _;

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Hsla {
    Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: a as f32 / 255.,
    }
    .into()
}

/// T1 paper: the near-opaque reading plane the chat workspace, right panel and
/// full-page routes paint over the vibrancy canvas. Warm paper in light mode,
/// blue-carbon in dark.
pub fn content_surface(cx: &App) -> Hsla {
    if cx.theme().mode.is_dark() {
        rgba(0x1B, 0x1E, 0x24, 0xF0)
    } else {
        rgba(0xFD, 0xFD, 0xFB, 0xF2)
    }
}

// Role-based radius scale — no magic corner numbers outside this table.
/// Popovers, menus, dialogs, toasts.
pub fn radius_overlay() -> Pixels {
    px(14.)
}
/// Cards, event cards, diff blocks.
pub fn radius_card() -> Pixels {
    px(12.)
}
/// Plain inputs and button-group containers.
pub fn radius_input() -> Pixels {
    px(10.)
}
/// Buttons.
pub fn radius_button() -> Pixels {
    px(8.)
}
/// The composer field — the hero element.
pub fn radius_composer() -> Pixels {
    px(16.)
}

/// A 1px separator that fades out toward both ends, replacing full-bleed
/// hairlines inside the paper plane.
pub fn faded_hairline(cx: &App) -> impl IntoElement {
    let color = cx.theme().border;
    let clear = color.opacity(0.);
    div()
        .w_full()
        .h(px(1.))
        .flex()
        .child(div().flex_1().h_full().bg(linear_gradient(
            90.,
            linear_color_stop(color, 1.),
            linear_color_stop(clear, 0.),
        )))
        .child(div().flex_1().h_full().bg(linear_gradient(
            90.,
            linear_color_stop(clear, 1.),
            linear_color_stop(color, 0.),
        )))
}

/// Primary buttons get a faint top light so they read as physical controls:
/// a barely-brighter wash over the top of the plain primary fill.
pub fn primary_button_fill(cx: &App) -> gpui::Background {
    let base = cx.theme().primary;
    let lit = base.blend(gpui::white().opacity(0.10));
    linear_gradient(
        180.,
        linear_color_stop(lit, 0.),
        linear_color_stop(base, 0.6),
    )
}

/// Applies the T3 overlay contour: near-opaque fill, hairline border and a
/// large soft shadow. Radius stays the caller's choice (`radius_overlay`).
pub fn overlay_contour(el: Div, cx: &App) -> Div {
    el.bg(cx.theme().popover)
        .border_1()
        .border_color(cx.theme().border)
        .shadow_xl()
}
