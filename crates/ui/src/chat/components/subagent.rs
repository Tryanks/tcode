use crate::theme::ActiveTheme as _;
use crate::widgets::spinner::Spinner;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use agent::{ItemContent, ItemStatus};
use tcode_core::session::{EntryContent, TimelineEntry};

use super::super::model::{displayed_error_text, one_line};

pub(crate) struct SubagentState {
    pub(crate) expanded: bool,
    pub(crate) truncated: bool,
    pub(crate) duration: Option<String>,
}

pub(crate) fn subagent_row(
    entry: &TimelineEntry,
    state: SubagentState,
    children: Vec<AnyElement>,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let EntryContent::Item(ItemContent::Subagent {
        agent_type,
        description,
        status,
        summary,
    }) = &entry.content
    else {
        unreachable!();
    };
    let SubagentState {
        expanded,
        truncated,
        duration,
    } = state;
    let muted = cx.theme().muted_foreground;
    let finished = !matches!(status, ItemStatus::InProgress);
    let row_label = crate::tr!(
        "chat.subagent_row",
        agent = agent_type.clone(),
        description = one_line(description)
    )
    .into_owned();
    let lifecycle: AnyElement = match status {
        ItemStatus::InProgress => Spinner::new()
            .xsmall()
            .color(cx.theme().primary)
            .into_any_element(),
        // The row is a surface, not a trace, so its outcome may carry color:
        // green settled, red failed — the task-row badge pair.
        ItemStatus::Completed => h_flex()
            .h(px(22.))
            .px_2()
            .gap_1()
            .items_center()
            .rounded(crate::material::radius_chip())
            .bg(cx.theme().success.opacity(0.12))
            .text_color(cx.theme().success)
            .text_size(px(11.5))
            .child(Icon::new(IconName::Check).size(px(12.)))
            .child(crate::tr!("chat.subagent_completed"))
            .into_any_element(),
        ItemStatus::Failed | ItemStatus::Declined => h_flex()
            .h(px(22.))
            .px_2()
            .gap_1()
            .items_center()
            .rounded(crate::material::radius_chip())
            .bg(cx.theme().danger.opacity(0.12))
            .text_color(cx.theme().danger)
            .text_size(px(11.5))
            .child(
                Icon::new(IconName::CircleX)
                    .size(px(12.))
                    .text_color(cx.theme().danger),
            )
            .child(crate::tr!("chat.subagent_failed"))
            .into_any_element(),
    };
    let mut row = crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("subagent-row-{}", entry.id)),
        Role::Button,
        row_label,
        cx,
    )
    .aria_expanded(expanded)
    .w_full()
    .h(px(44.))
    .min_w_0()
    .gap_2()
    .items_center()
    .px_3()
    .rounded_full()
    .border_1()
    .border_color(cx.theme().border)
    .bg(crate::material::content_surface(cx))
    .text_size(px(13.))
    .cursor_pointer()
    .hover(|row| row.bg(cx.theme().accent))
    .on_click(on_toggle)
    .child(lifecycle)
    .child(div().flex_none().font_medium().child(agent_type.clone()))
    .child(
        div()
            .min_w_0()
            .flex_1()
            .overflow_hidden()
            .text_ellipsis()
            .text_color(muted)
            .child(one_line(description)),
    )
    .when_some(duration, |row, duration| {
        row.child(
            div()
                .flex_none()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(px(11.))
                .text_color(muted)
                // Baseline compensation beside the 13px row text (see
                // work_log.rs — flex can't baseline-align text boxes).
                .relative()
                .top(px(1.))
                .child(duration),
        )
    });
    if finished && let Some(summary) = summary.as_deref().filter(|summary| !summary.is_empty()) {
        row = row.child(
            div()
                .max_w(px(180.))
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(muted)
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(px(11.5))
                .child(one_line(summary)),
        );
    }
    row = row.child(Icon::new(chevron(expanded)).xsmall().text_color(muted));

    let mut block = v_flex().w_full().gap_1().child(row);
    if expanded {
        // The same hairline rail the activity drill-downs hang on.
        let mut nested = v_flex()
            .w_full()
            .gap_1()
            .ml_2()
            .pl(px(14.))
            .py_0p5()
            .border_l_1()
            .border_color(cx.theme().border);
        if truncated {
            nested = nested.child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(crate::tr!("chat.earlier_steps_truncated")),
            );
        }
        nested = nested.children(children);
        block = block.child(nested);
    }
    block.into_any_element()
}

pub(crate) fn subagent_child(
    entry: &TimelineEntry,
    fallback: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    match &entry.content {
        EntryContent::Item(ItemContent::UserMessage { text, .. })
        | EntryContent::Steer { text, .. } => h_flex()
            .w_full()
            .justify_end()
            .child(
                div()
                    .max_w_3_4()
                    .px(px(10.))
                    .py(px(6.))
                    .rounded_lg()
                    .bg(cx.theme().muted)
                    .text_size(px(13.))
                    .line_height(px(18.2))
                    .text_color(cx.theme().foreground)
                    .child(text.clone()),
            )
            .into_any_element(),
        EntryContent::Item(ItemContent::AssistantMessage { text }) => div()
            .w_full()
            .text_size(px(13.5))
            .line_height(px(21.))
            .text_color(cx.theme().foreground)
            .child(text.clone())
            .into_any_element(),
        EntryContent::Error { .. } | EntryContent::ProviderStartError { .. } => div()
            .w_full()
            .text_size(px(13.))
            .text_color(cx.theme().danger)
            .child(displayed_error_text(&entry.content).into_owned())
            .into_any_element(),
        EntryContent::Item(ItemContent::FileChange { changes, .. }) => div()
            .text_size(px(13.))
            .text_color(muted)
            .child(crate::tr!("chat.changed_files", count = changes.len()))
            .into_any_element(),
        _ => fallback.expect("activity child requires a fallback component"),
    }
}

fn chevron(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}
