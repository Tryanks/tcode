use std::borrow::Cow;
use std::path::Path;

use agent::TurnStatus;
use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex, spinner::Spinner,
    v_flex,
};

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
    pub(crate) subagent_count: usize,
    pub(crate) rows: Vec<AnyElement>,
    pub(crate) rows_expanded: bool,
    pub(crate) previous_logs_label: Option<Cow<'static, str>>,
    pub(crate) started_at: Option<u64>,
    pub(crate) served_model: Option<String>,
}

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
        subagent_count,
        rows,
        rows_expanded,
        previous_logs_label,
        started_at,
        served_model,
    } = data;
    let muted = cx.theme().muted_foreground;
    let header = crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("worklog-header-{index}-{segment_id}")),
        Role::Button,
        capsule_label.clone(),
        cx,
    )
    .aria_expanded(expanded)
    .w_full()
    .h(px(30.))
    .px_3()
    .gap_2()
    .items_center()
    .text_size(px(12.5))
    .font_medium()
    .text_color(muted)
    .cursor_pointer()
    .hover(|row| row.bg(cx.theme().accent))
    .on_click(on_toggle)
    .when(running, |row| {
        row.child(Spinner::new().xsmall().color(cx.theme().primary))
    })
    .when(!running, |row| {
        let (icon, label) = match outcome {
            TurnStatus::Completed => {
                return row.child(Icon::new(IconName::Check).size(px(12.)).text_color(muted));
            }
            TurnStatus::Failed => (IconName::Redo2, crate::tr!("chat.work_log_failed")),
            TurnStatus::Interrupted => (IconName::CircleX, crate::tr!("chat.work_log_interrupted")),
        };
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
                .child(Icon::new(icon).size(px(12.)))
                .child(label),
        )
    })
    .child(
        div()
            .flex_1()
            .min_w_0()
            .text_ellipsis()
            .child(capsule_label),
    )
    .when(!running, |row| {
        row.child(
            div()
                .flex_none()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(px(11.))
                .child(duration),
        )
    })
    .when(subagent_count > 0, |row| {
        row.child(
            div()
                .flex_none()
                .h(px(22.))
                .px_2()
                .items_center()
                .rounded(crate::material::radius_chip())
                .bg(cx.theme().muted)
                .text_size(px(11.5))
                .font_family(cx.theme().mono_font_family.clone())
                .child(crate::tr!("chat.subagent_count", count = subagent_count)),
        )
    })
    .child(Icon::new(chevron(expanded)).xsmall().text_color(muted));

    let mut body = v_flex().w_full().gap_1p5().px_3().pb_3().children(rows);
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
                    row.child(
                        div()
                            .text_color(cx.theme().warning)
                            .child(format!("⚠ {served}")),
                    )
                }),
        );
    }

    v_flex()
        .w_full()
        .rounded(crate::material::radius_card())
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().muted.opacity(0.42))
        .overflow_hidden()
        .child(header)
        .when(expanded, |capsule| capsule.child(body))
        .into_any_element()
}

fn chevron(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}
