use std::path::Path;

use crate::material::{Bar, TOUCH_TARGET, bar_height};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::spinner::Spinner;
use agent::TurnStatus;
use gpui::{
    AnyElement, App, ClickEvent, Div, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use tcode_core::session::{TimelineEntry, TurnMeta};

use super::super::timeline::rows::RowPart;

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) type WorkLogArgs<'a> = (
    usize,
    &'a RowPart,
    &'a TurnMeta,
    &'a Path,
    &'a [&'a TimelineEntry],
    bool,
);

pub(crate) struct WorkLogData {
    pub(crate) index: usize,
    pub(crate) compact: bool,
    pub(crate) segment_id: String,
    pub(crate) capsule_label: String,
    /// How long the run took, when its times say.
    pub(crate) duration: Option<String>,
    pub(crate) outcome: TurnStatus,
    pub(crate) expanded: bool,
    pub(crate) running: bool,
    pub(crate) rows: Vec<AnyElement>,
    /// The timeline's hover, which the header's fill follows.
    pub(crate) hover: crate::motion::ListHover,
    /// The chat's motion, which keeps the chevron's turn and the state's
    /// cross-fade.
    pub(crate) motion: crate::motion::owned::Owned,
}

/// One work log as a naked disclosure: a hugging header row, then the trace
/// itself in the flow. No capsule — execution traces carry no surface.
pub(crate) fn work_log(
    data: WorkLogData,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let WorkLogData {
        index,
        compact,
        segment_id,
        capsule_label,
        duration,
        outcome,
        expanded,
        running,
        rows,
        hover,
        motion,
    } = data;
    let muted = cx.theme().muted_foreground;
    let key = format!("worklog-header-{index}-{segment_id}");
    let radius = crate::material::radius_button(cx);
    let fill = hover.layer(key.clone(), cx.theme().accent, radius);
    // Not a `Button`: on a touch layout the label wraps, and `Button` lays its
    // content out on one line.
    let header = crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(key.clone()),
        Role::Button,
        capsule_label.clone(),
        cx,
    )
    .debug_selector(move || key)
    .aria_expanded(expanded)
    .self_start()
    .h(px(bar_height(Bar::Section, false)))
    .when(compact, |row| row.h_auto().min_h(px(TOUCH_TARGET)))
    .when(compact, |row| row.max_w_full().flex_wrap())
    .px_1p5()
    .gap_1p5()
    .items_center()
    .relative()
    .rounded(radius)
    .text_body(false, cx)
    .text_color(muted)
    .cursor_pointer()
    .on_click(on_toggle)
    .child(fill)
    .child(
        crate::widgets::Chevron::new(
            SharedString::from(format!("worklog-chevron-{index}-{segment_id}")),
            expanded,
        )
        .owned(&motion)
        .text_color(muted),
    )
    .child(capsule_label.clone())
    .child(settled_state(
        &motion,
        SharedString::from(format!("worklog-state-{index}-{segment_id}")),
        running,
        outcome,
        capsule_label,
        duration,
        cx,
    ));

    v_flex()
        .w_full()
        .gap_1()
        .child(header)
        .when(expanded && !rows.is_empty(), |flow| {
            flow.child(work_log_body(rows))
        })
        .into_any_element()
}

/// Folded activity rows under an expanded header.
pub(crate) fn work_log_body(rows: Vec<AnyElement>) -> Div {
    // Rows line up under the header label: 6px header padding + 14px chevron
    // (the body text size) + 6px gap, minus the 4px an activity row keeps for
    // its hover pill.
    v_flex().w_full().gap_1().pl(px(22.)).children(rows)
}

/// The spinner while the run goes on; once it settles, how long it took and,
/// only for a bad ending, a chip saying so. One cross-fades into the other.
/// The duration sits on the baseline of the header's `label`.
fn settled_state(
    motion: &crate::motion::owned::Owned,
    id: SharedString,
    running: bool,
    outcome: TurnStatus,
    label: String,
    duration: Option<String>,
    cx: &App,
) -> AnyElement {
    let primary = cx.theme().primary;
    let danger = cx.theme().danger;
    let motion = motion.clone();
    crate::motion::Sampled::new(move |window, cx| {
        motion.cross_fade_part(
            id,
            running,
            CHANGE,
            window,
            cx,
            move |running, window, cx| {
                if *running {
                    return Spinner::new().color(primary).into_any_element();
                }
                // A settled run needs no badge; only a bad ending is called out.
                let failure = match outcome {
                    TurnStatus::Completed => None,
                    TurnStatus::Failed => Some(crate::tr!("chat.work_log_failed")),
                    TurnStatus::Interrupted => Some(crate::tr!("chat.work_log_interrupted")),
                };
                let theme = cx.theme();
                let typography = &theme.tokens.typography;
                let duration = duration.clone().map(|duration| {
                    let drop = crate::typography::baseline_below_centre(
                        SharedString::from(label.clone()),
                        theme.font_family.clone(),
                        typography.sm.size,
                        window,
                    ) - crate::typography::baseline_below_centre(
                        SharedString::from(duration.clone()),
                        typography.mono.clone(),
                        typography.xs.size,
                        window,
                    );
                    div()
                        .flex_none()
                        .text_meta_mono(false, cx)
                        .relative()
                        .top(drop)
                        .child(duration)
                });
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .when_some(failure, |row, failure| {
                        row.child(crate::material::semantic_chip(
                            failure,
                            danger.opacity(0.12),
                            danger,
                            cx,
                        ))
                    })
                    .children(duration)
                    .into_any_element()
            },
        )
    })
    .into_any_element()
}
