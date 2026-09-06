use std::borrow::Cow;
use std::path::Path;

use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, ClickEvent, Div, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use tcode_core::session::OrchestrateCallback;

use super::super::model::one_line;
use crate::markdown::{MarkdownState, MarkdownView};

const CALLBACK_TITLE_MAX_CHARS: usize = 24;
const DISCLOSURE_LINE_HEIGHT: f32 = 20.;
const DISCLOSURE_CARD_MAX_HEIGHT: f32 = 320.;

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub(crate) fn callback_row(
    entry_id: &str,
    callback: &OrchestrateCallback,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let title = truncate_chars(&callback.title, CALLBACK_TITLE_MAX_CHARS);
    let label = SharedString::from(format!(
        "{title} {}",
        localized_callback_state(&callback.state)
    ));
    let body = if callback.body.trim().is_empty() {
        crate::tr!("chat.orchestrate_callback_empty").into_owned()
    } else {
        callback.body.clone()
    };
    disclosure(
        &format!("orchestrate-callback-{entry_id}"),
        label,
        &body,
        expanded,
        on_toggle,
        cx,
    )
}

/// A centered disclosure notification with a height-capped verbatim body: the
/// divider grammar's stub–label–stub row, with the label clickable to expand.
/// Orchestrate context and child-thread results read as ambient notifications
/// of the flow — the same standing as a relay or model-change divider.
pub(crate) fn disclosure(
    key: &str,
    label: SharedString,
    full_text: &str,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let toggle = crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("disclosure-{key}")),
        Role::Button,
        label.clone(),
        cx,
    )
    .aria_expanded(expanded)
    .flex_none()
    .h(px(24.))
    .px_1p5()
    .gap_1p5()
    .items_center()
    .rounded(crate::material::radius_button())
    .text_size(px(11.))
    .text_color(muted)
    .cursor_pointer()
    .hover(|row| row.bg(cx.theme().accent))
    .on_click(on_toggle)
    .child(Icon::new(chevron(expanded)).size(px(12.)).text_color(muted))
    .child(label);

    let row = h_flex()
        .w_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(super::dividers::divider_stub(cx))
        .child(toggle)
        .child(super::dividers::divider_stub(cx));

    let mut block = v_flex().w_full().gap_1().child(row);
    if expanded {
        block = block.child(disclosure_body(key, full_text, cx));
    }
    block.into_any_element()
}

fn disclosure_body(key: &str, full_text: &str, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    let lines: Vec<AnyElement> = full_text
        .split('\n')
        .map(|line| {
            div()
                .w_full()
                .line_height(px(DISCLOSURE_LINE_HEIGHT))
                .child(if line.is_empty() {
                    " ".to_string()
                } else {
                    line.to_string()
                })
                .into_any_element()
        })
        .collect();
    div()
        .w_full()
        .rounded(crate::material::radius_card())
        .bg(cx.theme().muted)
        .occlude()
        .p_3()
        .child(
            div()
                .id(SharedString::from(format!("disclosure-body-{key}")))
                .w_full()
                .max_h(px(DISCLOSURE_CARD_MAX_HEIGHT))
                .overflow_y_scroll()
                .child(
                    v_flex()
                        .w_full()
                        .text_size(px(13.))
                        .text_color(muted)
                        .children(lines),
                ),
        )
}

pub(crate) struct PlanCardData<'a> {
    pub(crate) turn: usize,
    pub(crate) markdown: &'a str,
    pub(crate) cwd: &'a Path,
    pub(crate) markdown_state: Option<Entity<MarkdownState>>,
    pub(crate) collapsed: bool,
    pub(crate) copied: bool,
}

pub(crate) struct PlanCardHandlers {
    pub(crate) toggle: ClickHandler,
    pub(crate) copy: ClickHandler,
    pub(crate) download: ClickHandler,
    pub(crate) save: ClickHandler,
}

pub(crate) fn proposed_plan_card(
    data: PlanCardData<'_>,
    handlers: PlanCardHandlers,
    cx: &App,
) -> AnyElement {
    let PlanCardData {
        turn,
        markdown,
        cwd,
        markdown_state,
        collapsed,
        copied,
    } = data;
    let PlanCardHandlers {
        toggle,
        copy,
        download,
        save,
    } = handlers;
    let title = tcode_core::session::plan_title(markdown)
        .unwrap_or_else(|| crate::tr!("plan.proposed_plan").into_owned());
    let long = markdown.chars().count() > 900 || markdown.lines().count() > 20;

    let body: AnyElement = if collapsed {
        div().into_any_element()
    } else if let Some(markdown_state) = markdown_state {
        div()
            .w_full()
            .text_size(px(15.))
            .line_height(px(22.))
            .child(
                MarkdownView::new(&markdown_state)
                    .selectable(true)
                    .base_dir(cwd),
            )
            .into_any_element()
    } else {
        div()
            .w_full()
            .child(markdown.to_string())
            .into_any_element()
    };

    let content = v_flex()
        .flex_1()
        .min_w_0()
        .gap_2()
        .p_4()
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .h(px(22.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_full()
                        .bg(cx.theme().info.opacity(0.12))
                        .text_color(cx.theme().info_foreground)
                        .text_size(px(11.5))
                        .font_medium()
                        .child(crate::tr!("plan.badge")),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(15.))
                        .font_semibold()
                        .child(title),
                )
                .when(long, |this| {
                    this.child(
                        Button::new(("plan-collapse", turn))
                            .ghost()
                            .xsmall()
                            .label(if collapsed {
                                crate::tr!("plan.expand")
                            } else {
                                crate::tr!("plan.collapse")
                            })
                            .on_click(toggle),
                    )
                }),
        )
        .child(body)
        .child(
            h_flex()
                .w_full()
                .gap_1()
                .flex_wrap()
                .child(
                    Button::new(("plan-copy", turn))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Copy)
                        .label(if copied {
                            crate::tr!("plan.copied")
                        } else {
                            crate::tr!("plan.copy")
                        })
                        .on_click(copy),
                )
                .child(
                    Button::new(("plan-download", turn))
                        .ghost()
                        .xsmall()
                        .icon(Icon::empty().path("icons/download.svg"))
                        .label(crate::tr!("plan.download"))
                        .on_click(download),
                )
                .child(
                    Button::new(("plan-save", turn))
                        .ghost()
                        .xsmall()
                        .icon(IconName::HardDrive)
                        .label(crate::tr!("plan.save_workspace"))
                        .on_click(save),
                ),
        );

    h_flex()
        .w_full()
        .items_stretch()
        .rounded(crate::material::radius_card())
        .overflow_hidden()
        .bg(cx.theme().muted.opacity(0.6))
        .child(
            div()
                .flex_none()
                .w(px(2.))
                .ml(px(8.))
                .my(px(8.))
                .rounded_full()
                .bg(cx.theme().info),
        )
        .child(content)
        .into_any_element()
}

fn localized_callback_state(state: &str) -> Cow<'static, str> {
    match state {
        "completed" => crate::tr!("chat.orchestrate_state_completed"),
        "failed" => crate::tr!("chat.orchestrate_state_failed"),
        other => Cow::Owned(other.to_string()),
    }
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
