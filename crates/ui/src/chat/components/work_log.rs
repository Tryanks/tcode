use std::borrow::Cow;
use std::path::Path;

use crate::theme::ActiveTheme as _;
use crate::widgets::spinner::Spinner;
use crate::widgets::tooltip::Tooltip;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::TurnStatus;
use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use super::super::model::WorkLogCounts;
use super::indicator;
use tcode_core::session::{TimelineEntry, TurnMeta};

pub(crate) type WorkLogArgs<'a> = (
    usize,
    &'a str,
    &'a TurnMeta,
    &'a Path,
    &'a [&'a TimelineEntry],
    &'a WorkLogCounts,
    bool,
);

pub(crate) struct WorkLogData {
    pub(crate) index: usize,
    pub(crate) segment_id: String,
    pub(crate) capsule_label: String,
    pub(crate) duration: String,
    pub(crate) outcome: TurnStatus,
    pub(crate) expanded: bool,
    pub(crate) running: bool,
    pub(crate) rows: Vec<AnyElement>,
    pub(crate) rows_expanded: bool,
    pub(crate) previous_logs_label: Option<Cow<'static, str>>,
    pub(crate) started_at: Option<u64>,
    pub(crate) served_model: Option<String>,
}

/// One work log as a naked disclosure: a hugging header row, then the trace
/// itself in the flow. No capsule — execution traces carry no surface.
pub(crate) fn work_log(
    data: WorkLogData,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_toggle_rows: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let WorkLogData {
        index,
        segment_id,
        capsule_label,
        duration,
        outcome,
        expanded,
        running,
        rows,
        rows_expanded,
        previous_logs_label,
        started_at,
        served_model,
    } = data;
    let muted = cx.theme().muted_foreground;
    // One animation locus at a time: while the body is open the working
    // indicator carries the motion, so the header only spins when folded.
    let header_spinner = running && !expanded;
    let header = crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("worklog-header-{index}-{segment_id}")),
        Role::Button,
        capsule_label.clone(),
        cx,
    )
    .aria_expanded(expanded)
    .self_start()
    .h(px(28.))
    .px_1p5()
    .gap_1p5()
    .items_center()
    .rounded(crate::material::radius_button())
    .text_size(px(12.5))
    .font_medium()
    .text_color(muted)
    .cursor_pointer()
    .hover(|row| row.bg(cx.theme().accent))
    .on_click(on_toggle)
    .child(Icon::new(chevron(expanded)).size(px(12.)).text_color(muted))
    .child(capsule_label)
    .when(!running, |row| {
        // A settled run needs no badge; only a bad ending is called out.
        let failure = match outcome {
            TurnStatus::Completed => None,
            TurnStatus::Failed => Some(crate::tr!("chat.work_log_failed")),
            TurnStatus::Interrupted => Some(crate::tr!("chat.work_log_interrupted")),
        };
        row.when_some(failure, |row, label| {
            row.child(
                h_flex()
                    .flex_none()
                    .h(px(22.))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .rounded(crate::material::radius_chip())
                    .bg(cx.theme().danger.opacity(0.12))
                    .text_color(cx.theme().danger)
                    .text_size(px(11.5))
                    .child(Icon::new(IconName::CircleX).size(px(12.)))
                    .child(label),
            )
        })
        .child(
            div()
                .flex_none()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(px(11.))
                .child(duration),
        )
    })
    .when(header_spinner, |row| {
        row.child(Spinner::new().xsmall().color(cx.theme().primary))
    });

    // Rows line up under the header label: 6px header padding + 12px chevron
    // + 6px gap, minus the 4px an activity row keeps for its hover pill.
    let mut body = v_flex().w_full().gap_1().pl(px(20.));
    // The fold hides *earlier* entries, so its switch belongs above them.
    if let Some(toggle_label) = previous_logs_label {
        body = body.child(
            crate::material::accessible_clickable(
                h_flex(),
                SharedString::from(format!("worklog-more-{index}-{segment_id}")),
                Role::Button,
                toggle_label.clone(),
                cx,
            )
            .aria_expanded(rows_expanded)
            .self_start()
            .gap_1()
            .items_center()
            .py_0p5()
            .text_size(px(12.5))
            .text_color(muted)
            .cursor_pointer()
            .hover(|style| style.text_color(cx.theme().foreground))
            .on_click(on_toggle_rows)
            .child(
                Icon::new(if rows_expanded {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall(),
            )
            .child(toggle_label),
        );
    }
    body = body.children(rows);

    if running {
        body = body.child(
            h_flex()
                .gap_2()
                .items_center()
                .text_size(px(12.5))
                .text_color(muted)
                .child(indicator::working_indicator(
                    SharedString::from(format!("working-{index}-{segment_id}")),
                    started_at,
                    cx,
                ))
                .when_some(served_model, |row, served| {
                    let tooltip = crate::tr!("chat.served_model_tooltip").into_owned();
                    row.child(
                        h_flex()
                            .id(SharedString::from(format!(
                                "served-model-{index}-{segment_id}"
                            )))
                            .gap_1()
                            .items_center()
                            .text_color(cx.theme().warning)
                            .child(Icon::new(IconName::TriangleAlert).size(px(12.)))
                            .child(served)
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip.clone()).build(window, cx)
                            }),
                    )
                }),
        );
    }

    v_flex()
        .w_full()
        .gap_1()
        .child(header)
        .when(expanded, |flow| flow.child(body))
        .into_any_element()
}

fn chevron(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}
