//! The "Frosted Instrument" material system (docs/visual-redesign.md).
//!
//! The window canvas (theme `background`) is the only intentionally translucent
//! tier; reading surfaces sit on top of it at near-full opacity so body text
//! never lands on the raw blur. Semantic colors stay on `cx.theme()` — this
//! module owns the material tiers, the role-based radius scale, and the few
//! light effects (faded hairlines, the primary-button top light) the spec
//! defines.

use gpui::{
    App, BoxShadow, Div, ElementId, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Rgba, Role, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, div,
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

/// The canvas flattened to full opacity. Painted under the glass tiers only
/// while the window is fullscreen: a fullscreen Space has nothing but black
/// behind the vibrancy material, and the canvas alpha compositing against it
/// muddies every surface above. Same fallback Windows Acrylic's `FallbackColor`
/// and macOS "Reduce Transparency" apply. Windowed, the canvas stays
/// translucent and Root owns it (docs/visual-redesign.md §0).
pub fn opaque_canvas(cx: &App) -> Hsla {
    cx.theme().background.opacity(1.)
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

/// Gives a raw clickable surface the same keyboard and accessibility treatment
/// as the component-library controls. GPUI automatically maps Enter/Space to
/// `on_click` for a focused clickable div; this helper supplies the tab stop,
/// semantic role/name, and a keyboard-only outline that remains legible in
/// both themes without changing layout.
pub fn accessible_clickable(
    el: Div,
    id: impl Into<ElementId>,
    role: Role,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let ring = cx.theme().ring.opacity(if cx.theme().mode.is_dark() {
        0.72
    } else {
        0.58
    });

    el.tab_index(0)
        .focus_visible(|style| {
            style.shadow(vec![
                BoxShadow::new(px(0.), px(0.), ring).spread_radius(px(2.)),
            ])
        })
        .id(id)
        .role(role)
        .aria_label(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, KeyBinding, KeyUpEvent, Keystroke, Render, TestAppContext, VisualTestContext,
        Window,
    };
    use std::{cell::Cell, rc::Rc};

    gpui::actions!(accessible_controls_probe, [FocusNext, FocusPrevious]);

    struct AccessibleControlsProbe {
        activations: Rc<Cell<[usize; 2]>>,
    }

    impl AccessibleControlsProbe {
        fn new(activations: Rc<Cell<[usize; 2]>>, _cx: &mut Context<Self>) -> Self {
            Self { activations }
        }
    }

    impl Render for AccessibleControlsProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let first_activations = self.activations.clone();
            let second_activations = self.activations.clone();

            div()
                .key_context("AccessibleControlsProbe")
                .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
                .on_action(|_: &FocusPrevious, window, cx| window.focus_prev(cx))
                .child(
                    accessible_clickable(div().size(px(24.)), "first", Role::Button, "First", cx)
                        .on_click(move |_, _, _| {
                            let mut counts = first_activations.get();
                            counts[0] += 1;
                            first_activations.set(counts);
                        }),
                )
                .child(
                    accessible_clickable(div().size(px(24.)), "second", Role::Switch, "Second", cx)
                        .on_click(move |_, _, _| {
                            let mut counts = second_activations.get();
                            counts[1] += 1;
                            second_activations.set(counts);
                        }),
                )
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    fn activate(cx: &mut VisualTestContext, key: &str) {
        cx.simulate_keystrokes(key);
        cx.simulate_event(KeyUpEvent {
            keystroke: Keystroke::parse(key).expect("valid activation key"),
        });
    }

    #[gpui::test]
    fn raw_controls_follow_root_tab_order_and_activate_from_the_keyboard(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.bind_keys([
                KeyBinding::new("tab", FocusNext, Some("AccessibleControlsProbe")),
                KeyBinding::new("shift-tab", FocusPrevious, Some("AccessibleControlsProbe")),
            ]);
        });
        let activations = Rc::new(Cell::new([0, 0]));
        let probe_activations = activations.clone();
        let (_probe, cx) = cx.add_window_view(move |_, cx| {
            AccessibleControlsProbe::new(probe_activations.clone(), cx)
        });
        let cx: &mut VisualTestContext = cx;
        draw(cx);

        // A dependency's `Root` cannot be instantiated with GPUI's macOS mock
        // window, so bootstrap the first focus exactly as Root's Tab action
        // does, then exercise real Tab/Shift-Tab key dispatch from there.
        cx.update(|window, cx| window.focus_next(cx));
        draw(cx);
        activate(cx, "enter");
        assert_eq!(activations.get(), [1, 0]);

        cx.simulate_keystrokes("tab");
        draw(cx);
        activate(cx, "space");
        assert_eq!(activations.get(), [1, 1]);

        cx.simulate_keystrokes("shift-tab");
        draw(cx);
        activate(cx, "enter");
        assert_eq!(activations.get(), [2, 1]);
    }
}
