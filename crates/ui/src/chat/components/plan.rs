use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use crate::icon::{Icon, IconName};
use crate::markdown::{MarkdownState, MarkdownView};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::copy::copy_button;
use crate::widgets::menu::{ContextMenuExt as _, PopupMenu};
use agent::{ApprovalOption, ApprovalOptionKind};
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_base::{h_flex, v_flex};

use super::assistant::copy_message_items;

/// Sends the option a reader chose.
pub(crate) type ChooseOption = Rc<dyn Fn(&str, &mut Window, &mut App)>;

/// The decision the agent waits on for this plan.
pub(crate) struct PlanDecision<'a> {
    pub(crate) options: &'a [ApprovalOption],
    /// Whether a choice can be sent: connected, writable, none in flight.
    pub(crate) enabled: bool,
    pub(crate) on_choose: ChooseOption,
}

pub(crate) struct PlanData<'a> {
    pub(crate) image_host: crate::store::ImageHost,
    pub(crate) id: &'a str,
    pub(crate) text: &'a str,
    /// What Copy puts on the clipboard: the text, shared rather than copied
    /// on every render.
    pub(crate) copy_text: Arc<str>,
    pub(crate) cwd: &'a Path,
    pub(crate) markdown: Option<Entity<MarkdownState>>,
    pub(crate) compact: bool,
    pub(crate) copied: bool,
    pub(crate) decision: Option<PlanDecision<'a>>,
    /// The chat's motion, which keeps the plan's buttons' clocks.
    pub(crate) motion: crate::motion::owned::Owned,
}

/// A plan the harness proposed, set apart from the prose around it as a
/// document: a bordered sheet under a titled strip, holding the plan's
/// Markdown, with the decision the agent waits on along its foot. The sheet
/// keeps the page's colour so the code blocks inside it still stand out.
pub(crate) fn plan_block(
    data: PlanData<'_>,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let PlanData {
        image_host,
        id,
        text,
        copy_text,
        cwd,
        markdown,
        compact,
        copied,
        decision,
        motion,
    } = data;
    let theme = cx.theme();
    let radius = crate::material::radius_card(cx);
    let menu_items = copy_message_items(copy_text);
    let body = markdown.map_or_else(
        || div().child(text.to_string()).into_any_element(),
        |markdown| {
            MarkdownView::new(&markdown)
                .compact_headings(true)
                .selectable(true)
                .image_host(image_host)
                .base_dir(cwd)
                .menu_extension(menu_items.clone())
                .into_any_element()
        },
    );
    let header = h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .pl_3()
        .pr_1()
        .py_1()
        .rounded_t(radius)
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.tokens.colors.muted)
        .child(
            Icon::new(IconName::Map)
                .small()
                .text_color(theme.muted_foreground),
        )
        .child(
            div()
                .flex_1()
                .text_caption(cx)
                .text_color(theme.muted_foreground)
                .child(crate::widgets::fade_text(crate::tr!("chat.plan_title"))),
        )
        .child(copy_button(&format!("plan:{id}"), copied, compact, on_copy, cx).motion(&motion));
    let footer = decision.map(|decision| {
        let mut choices = h_flex().gap_2().items_center().flex_wrap();
        for option in decision.options {
            let on_choose = decision.on_choose.clone();
            let option_id = option.id.clone();
            let button = Button::new(SharedString::from(format!("plan-option-{}", option.id)))
                .motion(&motion)
                .chip()
                .touch(compact)
                .disabled(!decision.enabled)
                .label(option.label.clone())
                .map(|button| match option.kind {
                    ApprovalOptionKind::AllowOnce | ApprovalOptionKind::AllowAlways => {
                        button.primary()
                    }
                    ApprovalOptionKind::RejectOnce | ApprovalOptionKind::RejectAlways => {
                        button.outline()
                    }
                })
                .on_click(move |_, window, cx| on_choose(&option_id, window, cx));
            let selector = format!("plan-option-{}", option.id);
            choices = choices.child(div().debug_selector(move || selector).child(button));
        }
        h_flex()
            .debug_selector(|| "plan-decision".into())
            .w_full()
            .gap_2()
            .items_center()
            .flex_wrap()
            .px_3()
            .py_2()
            .rounded_b(radius)
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.tokens.colors.muted)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("chat.plan_feedback_hint").into_owned()),
            )
            .child(choices)
    });
    let margin_menu = move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
        menu_items(menu, window, cx)
    };
    v_flex()
        .id(SharedString::from(format!("plan-{id}")))
        .w_full()
        .rounded(radius)
        .border_1()
        .border_color(theme.border)
        .flex_none()
        .bg(theme.background)
        .child(header)
        .child(
            div()
                .w_full()
                .px_3()
                .py_2()
                .text_token(&theme.tokens.typography.md)
                .child(body),
        )
        .children(footer)
        .context_menu(margin_menu)
        .into_any_element()
}
