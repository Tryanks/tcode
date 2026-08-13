use std::borrow::Cow;
use std::time::Duration;

use crate::theme::ActiveTheme as _;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Hsla, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div,
    linear_color_stop, linear_gradient, prelude::FluentBuilder as _, px,
};
use gpui_component::{h_flex, tooltip::Tooltip};

use super::super::model::{
    TurnTimeClause, format_elapsed_deciseconds, format_local_time, turn_time_breakdown,
    turn_time_clauses,
};
use crate::time::now_millis;
use tcode_core::session::TurnMeta;

pub(crate) fn shimmer_label(id: SharedString, label: Cow<'static, str>, cx: &App) -> AnyElement {
    let quiet = cx.theme().muted_foreground.opacity(0.08);
    let bright = cx.theme().foreground.opacity(0.18);
    div()
        .px_2()
        .py(px(1.))
        .rounded(crate::material::radius_chip())
        .text_size(px(13.))
        .text_color(cx.theme().foreground)
        .child(label)
        .with_animation(
            id,
            Animation::new(Duration::from_millis(1400)).repeat(),
            move |element, delta| {
                element.bg(linear_gradient(
                    90.,
                    linear_color_stop(quiet, (delta - 0.3).clamp(0., 1.)),
                    linear_color_stop(bright, delta.clamp(0., 1.)),
                ))
            },
        )
        .into_any_element()
}

pub(crate) fn working_indicator(id: SharedString, started_at: Option<u64>, cx: &App) -> AnyElement {
    const DELAYS_MS: [u64; 9] = [90, 180, 270, 0, 90, 180, 90, 180, 270];
    const CYCLE_MS: u64 = 650;

    let foreground = cx.theme().foreground;
    let mut pixels = div().grid().grid_cols(3).gap(px(1.5));
    for (cell, delay_ms) in DELAYS_MS.into_iter().enumerate() {
        let animation_id = SharedString::from(format!("{id}-pixel-{cell}"));
        pixels = pixels.child(
            div()
                .size(px(4.))
                .rounded(px(1.))
                .bg(foreground)
                .with_animation(
                    animation_id,
                    Animation::new(Duration::from_millis(CYCLE_MS)).repeat(),
                    move |element, delta| {
                        let phase = (delta + 1. - delay_ms as f32 / CYCLE_MS as f32) % 1.;
                        let triangle = 1. - (phase * 2. - 1.).abs();
                        let eased = triangle * triangle * (3. - 2. * triangle);
                        element.opacity(0.15 + eased * 0.85)
                    },
                ),
        );
    }

    let elapsed_ms = started_at.map_or(0, |start| now_millis().saturating_sub(start));
    h_flex()
        .gap(px(10.))
        .items_center()
        .text_color(cx.theme().muted_foreground)
        .child(pixels)
        .child(shimmer_label(
            SharedString::from(format!("{id}-label")),
            crate::tr!("chat.working"),
            cx,
        ))
        .child(
            div()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(px(12.))
                .child(format_elapsed_deciseconds(elapsed_ms)),
        )
        .into_any_element()
}

/// A finished turn's quiet mono timing row.
pub(crate) fn turn_time_footer(
    id: SharedString,
    clauses: Vec<TurnTimeClause>,
    breakdown: Option<String>,
    muted: Hsla,
    warning: Hsla,
    mono: SharedString,
) -> AnyElement {
    let last = clauses.len().saturating_sub(1);
    h_flex()
        .id(id)
        .w_full()
        .gap_1p5()
        .items_start()
        .text_size(px(11.))
        .font_family(mono)
        .text_color(muted)
        .debug_selector(|| "turn-time-row".into())
        .when_some(breakdown, |row, breakdown| {
            row.tooltip(move |window, cx| Tooltip::new(breakdown.clone()).build(window, cx))
        })
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .flex_wrap()
                .gap_x(px(4.))
                .gap_y(px(2.))
                .children(clauses.into_iter().enumerate().map(|(index, clause)| {
                    let selector = clause.selector;
                    div()
                        .min_w_0()
                        .debug_selector(move || selector.into())
                        .when(clause.warning, |element| element.text_color(warning))
                        .child(if index == last {
                            clause.text
                        } else {
                            format!("{} ·", clause.text)
                        })
                })),
        )
        .into_any_element()
}

pub(crate) fn finished_turn_time(
    timestamp: u64,
    turn: &TurnMeta,
    requested_model: Option<&str>,
    cx: &App,
) -> AnyElement {
    turn_time_footer(
        SharedString::from(format!("turn-time-{timestamp}")),
        turn_time_clauses(
            format_local_time(timestamp),
            turn.timing,
            turn.cost_usd,
            turn.served_model.as_deref(),
            requested_model,
        ),
        turn_time_breakdown(turn.timing),
        cx.theme().muted_foreground,
        cx.theme().warning,
        cx.theme().mono_font_family.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::turn_time_footer;
    use crate::chat::model::{TurnTimeClause, turn_time_clauses};
    use gpui::TestAppContext;
    use tcode_core::session::TurnTiming;

    struct TurnTimeFooterProbe {
        clauses: Vec<TurnTimeClause>,
    }

    impl gpui::Render for TurnTimeFooterProbe {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            // The turn column stacks the footer under the answer, so the row is
            // content-height there; reproduce that rather than letting the window
            // stretch it and mask a wrap.
            use crate::theme::ActiveTheme as _;
            use gpui::{ParentElement as _, Styled as _};
            gpui_component::v_flex().size_full().child(turn_time_footer(
                "turn-time-probe".into(),
                self.clauses.clone(),
                None,
                cx.theme().muted_foreground,
                cx.theme().warning,
                cx.theme().mono_font_family.clone(),
            ))
        }
    }

    #[gpui::test]
    fn the_time_row_keeps_its_compact_clauses_inside_narrow_widths(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        let clauses = turn_time_clauses(
            "10:18 AM".into(),
            Some(TurnTiming::new(99_000, 0)),
            None,
            None,
            None,
        );
        // The footer's clauses, in render order (mirrors `turn_time_footer`).
        let selectors = ["turn-time-clock", "turn-time-total"];
        assert_eq!(clauses.len(), selectors.len());

        cx.update(gpui_component::init);
        let (_, cx) = cx.add_window_view(|_, _| TurnTimeFooterProbe { clauses });
        let cx: &mut VisualTestContext = cx;
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| {
                _ = window.draw(cx);
            });
        };

        // A comfortable chat width: the restrained single-line baseline.
        cx.simulate_resize(size(px(900.), px(120.)));
        draw(cx);
        let first = cx.debug_bounds(selectors[0]).expect("clock bounds");
        for selector in &selectors[1..] {
            let clause = cx.debug_bounds(selector).expect("clause bounds");
            assert_eq!(
                clause.top(),
                first.top(),
                "{selector} left the single line at 900px: {clause:?}"
            );
        }

        // Opening the diff panel squeezes the chat column; sweep well below any
        // practical width. Every clause must stay inside the row and hang under
        // the clock instead of spilling past the divider.
        for width in 260..=900 {
            cx.simulate_resize(size(px(width as f32), px(120.)));
            draw(cx);

            let row = cx.debug_bounds("turn-time-row").expect("row bounds");
            for selector in &selectors {
                let clause = cx.debug_bounds(selector).expect("clause bounds");
                assert!(
                    clause.left() >= row.left() && clause.right() <= row.right(),
                    "{selector} escaped the row at {width}px: row={row:?}, clause={clause:?}"
                );
                assert!(
                    clause.left() >= first.left(),
                    "{selector} broke the hanging indent at {width}px: {clause:?}"
                );
                assert!(
                    clause.size.width > px(0.) && clause.size.height > px(0.),
                    "{selector} was squeezed away at {width}px: {clause:?}"
                );
            }
        }
    }
}
