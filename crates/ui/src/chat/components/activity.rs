use crate::theme::ActiveTheme as _;
use crate::widgets::spinner::Spinner;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, ClickEvent, Div, HighlightStyle, InteractiveElement as _, IntoElement,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledText, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use agent::{ItemContent, ItemStatus};
use tcode_core::session::{EntryContent, TimelineEntry};

use super::super::model::{one_line, one_line_with_break_markers, output_tail, tool_brief};
use super::indicator;

const ACTIVITY_OUTPUT_TAIL_LINES: usize = 20;

/// One Work Log activity row: a muted status icon + a one-line summary.
pub(crate) fn activity_row(
    entry: &TimelineEntry,
    compact: bool,
    live_reasoning: bool,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let expandable = matches!(
        entry.content,
        EntryContent::Item(ItemContent::CommandExecution { .. })
            | EntryContent::Item(ItemContent::ToolCall { .. })
            | EntryContent::Item(ItemContent::Reasoning { .. })
    ) && !live_reasoning;
    let (status, icon, summary): (ItemStatus, IconName, AnyElement) = match &entry.content {
        EntryContent::Item(ItemContent::CommandExecution {
            command, status, ..
        }) => {
            let icon = activity_icon(*status);
            let (preview, breaks) = one_line_with_break_markers(command);
            let marker_style = HighlightStyle {
                color: Some(muted.opacity(0.45)),
                ..Default::default()
            };
            // Same scheme as a tool row: the primary word is the thing's own
            // identity — the binary here, the tool name there — never a verb.
            let summary = activity_summary(
                binary_name(command),
                Some(
                    StyledText::new(preview)
                        .with_highlights(breaks.into_iter().map(|range| (range, marker_style)))
                        .into_any_element(),
                ),
                true,
                cx,
            );
            (*status, icon, summary)
        }
        EntryContent::Item(ItemContent::ToolCall {
            name,
            input,
            output,
            status,
        }) => {
            let mut brief = tool_brief(input);
            if brief.is_empty()
                && let Some(output) = output
            {
                brief = one_line(output);
            }
            let summary = activity_summary(name.clone(), argument(brief), true, cx);
            (*status, activity_icon(*status), summary)
        }
        EntryContent::Item(ItemContent::WebSearch { query }) => {
            let summary = activity_summary("web_search", argument(one_line(query)), false, cx);
            (
                ItemStatus::Completed,
                activity_icon(ItemStatus::Completed),
                summary,
            )
        }
        EntryContent::Item(ItemContent::Other {
            provider_kind,
            summary: item_summary,
        }) => {
            let summary = activity_summary(
                provider_kind.clone(),
                argument(one_line(item_summary)),
                false,
                cx,
            );
            (
                ItemStatus::Completed,
                activity_icon(ItemStatus::Completed),
                summary,
            )
        }
        EntryContent::Item(ItemContent::Reasoning { .. }) => {
            if live_reasoning {
                let summary = indicator::shimmer_label(
                    SharedString::from(format!("thinking-{}", entry.id)),
                    crate::tr!("chat.thinking_live"),
                    cx,
                );
                (ItemStatus::InProgress, IconName::Loader, summary)
            } else {
                let summary = div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(crate::tr!("chat.thinking_done"))
                    .into_any_element();
                (ItemStatus::Completed, IconName::Check, summary)
            }
        }
        EntryContent::ContextCompacted => {
            let summary = div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(muted)
                .child(crate::tr!("chat.context_compacted"))
                .into_any_element();
            (ItemStatus::Completed, IconName::Minimize, summary)
        }
        _ => (
            ItemStatus::Completed,
            IconName::Check,
            div().into_any_element(),
        ),
    };

    let row = h_flex()
        .w_full()
        .min_h(px(28.))
        .px_1()
        .gap_2()
        .items_center()
        .when(!compact, |row| row.py_0p5())
        .text_size(px(12.5))
        .child(match status {
            ItemStatus::InProgress => Spinner::new()
                .xsmall()
                .color(cx.theme().primary)
                .into_any_element(),
            ItemStatus::Completed => Icon::new(icon)
                .size(px(13.))
                .text_color(muted)
                .into_any_element(),
            ItemStatus::Failed | ItemStatus::Declined => Icon::new(icon)
                .size(px(13.))
                .text_color(cx.theme().danger)
                .into_any_element(),
        })
        .child(summary)
        .when(expandable, |row| {
            row.child(Icon::new(chevron(expanded)).size(px(13.)).text_color(muted))
        });

    let row: AnyElement = if expandable {
        crate::material::accessible_clickable(
            row,
            SharedString::from(format!("activity-row-{}", entry.id)),
            Role::Button,
            crate::tr!("chat.activity_details"),
            cx,
        )
        .aria_expanded(expanded)
        .rounded(crate::material::radius_chip())
        .cursor_pointer()
        .hover(|row| row.bg(cx.theme().accent))
        .on_click(on_toggle)
        .into_any_element()
    } else {
        row.into_any_element()
    };

    let mut block = v_flex().w_full().gap_1().child(row);
    if expanded && expandable {
        block = block.child(activity_detail(entry, cx));
    }
    block.into_any_element()
}

/// A row summary in the tool-chips grammar: the bare verb, then its argument
/// as plain inline text. No surface of its own — the row packs left so the
/// chevron sits right after the words, and only the argument shrinks.
fn activity_summary(
    primary: impl IntoElement,
    argument: Option<AnyElement>,
    monospace: bool,
    cx: &App,
) -> AnyElement {
    h_flex()
        .min_w_0()
        .gap_2()
        .overflow_hidden()
        .child(
            div()
                .flex_none()
                .font_medium()
                .text_color(cx.theme().foreground)
                .child(primary),
        )
        .when_some(argument, |row, argument| {
            row.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(11.5))
                    .text_color(cx.theme().muted_foreground)
                    .when(monospace, |text| {
                        text.font_family(cx.theme().mono_font_family.clone())
                    })
                    .child(argument),
            )
        })
        .into_any_element()
}

/// The command's first token, which is what a reader scans for. Falls back to
/// the whole (whitespace-only) command so the row is never left wordless.
fn binary_name(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .unwrap_or(command)
        .to_string()
}

fn argument(text: String) -> Option<AnyElement> {
    (!text.is_empty()).then(|| text.into_any_element())
}

fn activity_detail(entry: &TimelineEntry, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let mono = cx.theme().mono_font_family.clone();
    let detail = match &entry.content {
        EntryContent::Item(ItemContent::CommandExecution {
            command, output, ..
        }) => v_flex()
            .w_full()
            .gap_2()
            .child(activity_detail_section(
                crate::tr!("chat.command").into_owned(),
                command.clone(),
                true,
                mono.clone(),
                muted,
            ))
            .when(!output.is_empty(), |detail| {
                detail.child(activity_detail_section(
                    crate::tr!("chat.output").into_owned(),
                    output_tail(output, ACTIVITY_OUTPUT_TAIL_LINES),
                    true,
                    mono.clone(),
                    muted,
                ))
            })
            .into_any_element(),
        EntryContent::Item(ItemContent::ToolCall { input, output, .. }) => {
            let mut input_brief = tool_brief(input);
            if input_brief.is_empty() {
                input_brief = one_line(&input.to_string());
            }
            let input_brief = truncate_chars(&input_brief, 240);
            v_flex()
                .w_full()
                .gap_2()
                .child(activity_detail_section(
                    crate::tr!("chat.input").into_owned(),
                    input_brief,
                    true,
                    mono,
                    muted,
                ))
                .when_some(output.clone(), |detail, output| {
                    detail.child(activity_detail_section(
                        crate::tr!("chat.output").into_owned(),
                        truncate_chars(&one_line(&output), 240),
                        false,
                        String::new().into(),
                        muted,
                    ))
                })
                .into_any_element()
        }
        EntryContent::Item(ItemContent::Reasoning { text }) => div()
            .w_full()
            .text_size(px(11.5))
            .line_height(px(18.))
            .text_color(muted)
            .whitespace_normal()
            .child(text.clone())
            .into_any_element(),
        _ => div().into_any_element(),
    };
    // Every drill-down hangs off the same hairline rail — no inset panel.
    div()
        .w_full()
        .ml_2()
        .pl(px(14.))
        .py_0p5()
        .border_l_1()
        .border_color(cx.theme().border)
        .text_size(px(11.5))
        .child(detail)
        .into_any_element()
}

fn activity_icon(status: ItemStatus) -> IconName {
    match status {
        ItemStatus::InProgress => IconName::LoaderCircle,
        ItemStatus::Completed => IconName::Check,
        ItemStatus::Failed | ItemStatus::Declined => IconName::CircleX,
    }
}

fn activity_detail_section(
    label: String,
    text: String,
    monospace: bool,
    mono: SharedString,
    muted: gpui::Hsla,
) -> Div {
    let lines = text
        .split('\n')
        .map(|line| {
            div()
                .w_full()
                .line_height(px(18.))
                .child(if line.is_empty() {
                    " ".to_string()
                } else {
                    line.to_string()
                })
        })
        .collect::<Vec<_>>();
    v_flex()
        .w_full()
        .gap_1()
        .child(
            div()
                .text_size(px(10.5))
                .font_medium()
                .text_color(muted)
                .child(crate::material::tracked_uppercase(&label)),
        )
        .child(
            v_flex()
                .w_full()
                .text_size(px(11.5))
                .text_color(muted)
                .when(monospace, |body| body.font_family(mono))
                .children(lines),
        )
}

fn truncate_chars(text: &str, max: usize) -> String {
    let text = one_line(text);
    if text.chars().count() <= max {
        return text;
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

fn chevron(open: bool) -> IconName {
    if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}
