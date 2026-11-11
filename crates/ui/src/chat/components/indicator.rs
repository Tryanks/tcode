use crate::widgets::TooltipTrigger as _;
use std::borrow::Cow;
use std::time::Duration;

use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::tooltip::Tooltip;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AppContext as _, Context, Div, ElementId,
    Entity, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render, RenderOnce,
    SharedString, Stateful, Styled as _, StyledText, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::h_flex;

use super::super::model::{TurnTimeClause, format_elapsed, format_local_time, turn_time_clauses};
use crate::time::now_millis;
use tcode_core::session::TurnMeta;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// A running turn's working indicator, drawn by a view whose clock ticks
/// only while a frame draws it: the view is the element's state, so a turn
/// scrolled out of the timeline's rows drops it, and its clock with it.
/// What a working indicator's clock was made for: its start, and whether it stops.
type Shown = (Option<u64>, bool);

#[derive(IntoElement)]
struct WorkingIndicator {
    id: ElementId,
    started_at: Option<u64>,
    /// Stop was pressed and the turn has not ended: it says so, without a time.
    stopping: bool,
}

impl RenderOnce for WorkingIndicator {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let started_at = (!self.stopping).then_some(self.started_at).flatten();
        let stopping = self.stopping;
        // The view is kept with the start it shows, so it is placed without
        // reading it: what draws the indicator is not built again with it.
        window.with_global_id(self.id, |id, window| {
            window.with_element_state(id, |held: Option<(Shown, Entity<WorkingClock>)>, _| {
                let view = match held {
                    Some((held, view)) if held == (started_at, stopping) => view,
                    _ => cx.new(|_| WorkingClock {
                        started_at,
                        stopping,
                        tick: None,
                    }),
                };
                (view.clone(), ((started_at, stopping), view))
            })
        })
    }
}

struct WorkingClock {
    started_at: Option<u64>,
    stopping: bool,
    tick: Option<Task<()>>,
}

impl Render for WorkingClock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The indicator's animations draw it again on every frame, which moves
        // its time with them. Reduced motion stills them, and the time, shown
        // to the decisecond, is then moved by a timer.
        if !cx.reduce_motion() || self.started_at.is_none() {
            self.tick = None;
        } else if self.tick.is_none() {
            self.tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
        working_indicator("working".into(), self.started_at, self.stopping, window, cx)
    }
}

pub(crate) fn shimmer_label(id: SharedString, label: Cow<'static, str>, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let foreground = cx.theme().foreground;
    let char_starts = label
        .char_indices()
        .map(|(start, _)| start)
        .collect::<Vec<_>>();
    let char_count = char_starts.len().max(1) as f32;

    h_flex()
        .gap(px(0.))
        .text_body(false, cx)
        .children(char_starts.iter().enumerate().map(|(index, &start)| {
            let end = char_starts.get(index + 1).copied().unwrap_or(label.len());
            let glyph = SharedString::from(label[start..end].to_owned());
            let animation_id = SharedString::from(format!("{id}-glyph-{index}"));
            let position = (index as f32 + 0.5) / char_count;

            div().child(StyledText::new(glyph)).with_animation(
                animation_id,
                // On the app's clock, not the element's: a running turn's
                // label moves to each new last row of the turn, whose fresh
                // element state would restart the sweep. gpui-base's
                // `animate_keyframes` keeps its start in element state and has
                // no clock-synced repeat, so gpui's `repeat_synced` stands in.
                Animation::new(Duration::from_millis(1400)).repeat_synced(),
                move |element, delta| {
                    // Enter and leave outside the text so every glyph returns to muted.
                    let center = -0.15 + delta * 1.3;
                    let intensity = (1. - (position - center).abs() / 0.15).clamp(0., 1.);
                    element.text_color(Hsla {
                        h: muted.h + (foreground.h - muted.h) * intensity,
                        s: muted.s + (foreground.s - muted.s) * intensity,
                        l: muted.l + (foreground.l - muted.l) * intensity,
                        a: muted.a + (foreground.a - muted.a) * intensity,
                    })
                },
            )
        }))
        .into_any_element()
}

pub(crate) fn working_indicator(
    id: SharedString,
    started_at: Option<u64>,
    stopping: bool,
    window: &mut Window,
    cx: &App,
) -> AnyElement {
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
                    // On the app's clock, as the shimmer is: the indicator
                    // moves to each new last row of its turn.
                    Animation::new(Duration::from_millis(CYCLE_MS)).repeat_synced(),
                    move |element, delta| {
                        let phase = (delta + 1. - delay_ms as f32 / CYCLE_MS as f32) % 1.;
                        let triangle = 1. - (phase * 2. - 1.).abs();
                        let eased = triangle * triangle * (3. - 2. * triangle);
                        element.opacity(0.15 + eased * 0.85)
                    },
                ),
        );
    }

    let label = if stopping {
        crate::tr!("background.stopping")
    } else {
        crate::tr!("chat.working")
    };
    // A turn whose start is not known has no time to show.
    let timer = started_at.map(|start| {
        let elapsed = format_elapsed(now_millis(cx).saturating_sub(start));
        // The timer is moved onto the label's baseline.
        let theme = cx.theme();
        let typography = &theme.tokens.typography;
        let drop = crate::typography::baseline_below_centre(
            SharedString::from(label.clone().into_owned()),
            theme.font_family.clone(),
            typography.sm.size,
            window,
        ) - crate::typography::baseline_below_centre(
            SharedString::from(elapsed.clone()),
            typography.mono.clone(),
            typography.xs.size,
            window,
        );
        div()
            .text_meta_mono(false, cx)
            .relative()
            .top(drop)
            .child(elapsed)
    });
    h_flex()
        .gap(px(10.))
        .items_center()
        .text_color(cx.theme().muted_foreground)
        .child(pixels)
        .child(shimmer_label(
            SharedString::from(format!("{id}-label")),
            label,
            cx,
        ))
        .children(timer)
        .into_any_element()
}

/// A running turn's liveness, bare in the flow after every segment: the pixel
/// grid, the shimmer label, the timer, and the served-model warning. It belongs
/// to the turn, not to any disclosure, so no fold can hide it.
pub(crate) fn turn_working_indicator(
    turn: usize,
    started_at: Option<u64>,
    stopping: bool,
    served_model: Option<String>,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let motion = motion.clone();
    let warning = cx.theme().warning;
    h_flex()
        .gap_2()
        .items_center()
        .text_body(false, cx)
        .text_color(cx.theme().muted_foreground)
        .child(WorkingIndicator {
            id: SharedString::from(format!("working-{turn}")).into(),
            started_at,
            stopping,
        })
        .child(crate::motion::Sampled::new(move |window, cx| {
            let id = SharedString::from(format!("served-model-{turn}"));
            match motion.shown(id.clone(), served_model, SHOW, window, cx) {
                Some(served) => {
                    let tooltip = crate::tr!("chat.served_model_tooltip").into_owned();
                    served.fade(
                        h_flex()
                            .id(id)
                            .gap_1()
                            .items_center()
                            .text_color(warning)
                            .child(crate::icon::Icon::new(crate::icon::IconName::TriangleAlert))
                            .child(served.value.clone())
                            .with_tooltip(move |_, _| Tooltip::new(tooltip.clone())),
                    )
                }
                None => div().into_any_element(),
            }
        }))
        .into_any_element()
}

/// A finished turn's footer: on the leading edge the actions of the message
/// it ends on, if any, and on the trailing edge its quiet mono timing, whose
/// clauses wrap toward that edge as the column narrows.
fn turn_time_footer(
    id: SharedString,
    clauses: Vec<TurnTimeClause>,
    actions: Option<AnyElement>,
    window: &Window,
    cx: &App,
) -> Stateful<Div> {
    let warning = cx.theme().warning;
    let last = clauses.len().saturating_sub(1);
    // A clause the mono face lacks glyphs for, such as a duration in CJK,
    // paints its baseline lower than the others; each is moved back onto the
    // mono face's own, which the separators keep.
    let typography = &cx.theme().tokens.typography;
    let below_centre = |text: &str| {
        crate::typography::baseline_below_centre(
            SharedString::from(text.to_owned()),
            typography.mono.clone(),
            typography.xs.size,
            window,
        )
    };
    let mono_baseline = below_centre("0");
    h_flex()
        .id(id)
        .w_full()
        .gap_1p5()
        .items_center()
        .text_meta_mono(false, cx)
        .children(actions.map(|actions| div().flex_none().child(actions)))
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .flex_wrap()
                .justify_end()
                .gap_x(px(4.))
                .gap_y(px(2.))
                .children(clauses.into_iter().enumerate().map(|(index, clause)| {
                    let selector = clause.selector;
                    let drop = mono_baseline - below_centre(&clause.text);
                    // The clause alone triggers its tooltip, which stands
                    // beside what it explains.
                    let text = div()
                        .id(selector)
                        .min_w_0()
                        .relative()
                        .top(drop)
                        .debug_selector(move || selector.into())
                        .child(clause.text)
                        .when_some(clause.tooltip, |text, tooltip| {
                            text.with_tooltip(move |_, _| Tooltip::new(tooltip.clone()))
                        });
                    h_flex()
                        .min_w_0()
                        .gap_x(px(4.))
                        .when(clause.warning, |element| element.text_color(warning))
                        .child(text)
                        .when(index != last, |element| element.child("·"))
                })),
        )
}

/// [`turn_time_footer`] for `turn`, timed from `timestamp` when it has one.
pub(crate) fn finished_turn_time(
    timestamp: Option<u64>,
    turn: &TurnMeta,
    requested_model: Option<&str>,
    actions: Option<AnyElement>,
    window: &Window,
    cx: &App,
) -> Stateful<Div> {
    let clauses = turn_time_clauses(
        timestamp.map(format_local_time),
        turn.timing,
        turn.cost_usd,
        turn.served_model.as_deref(),
        requested_model,
    );
    turn_time_footer(
        SharedString::from(format!("turn-time-{}", timestamp.unwrap_or_default())),
        clauses,
        actions,
        window,
        cx,
    )
}
