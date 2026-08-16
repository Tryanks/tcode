use std::path::Path;

use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use agent::TurnStatus;
use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use super::super::model::WorkLogCounts;
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
        segment_id,
        capsule_label,
        duration,
        outcome,
        expanded,
        running,
        rows,
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
                // Baseline compensation: flex can't baseline-align text boxes,
                // and the 11px digits sit visibly high beside the 12.5px label.
                .relative()
                .top(px(1.))
                .child(duration),
        )
    });

    // Rows line up under the header label: 6px header padding + 12px chevron
    // + 6px gap, minus the 4px an activity row keeps for its hover pill.
    let body = v_flex().w_full().gap_1().pl(px(20.)).children(rows);

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
