use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use crate::icon::IconName;
use crate::markdown::{MarkdownState, MarkdownView};
use crate::material::event_card::{self, CardTone};
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
use gpui_base::h_flex;

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

/// A plan the harness proposed, set apart from the prose around it as an
/// event card holding the plan's Markdown, with the decision the agent
/// waits on along its foot.
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
    let tone = if decision.is_some() {
        CardTone::Waiting
    } else {
        CardTone::Neutral
    };
    let header = event_card::header(true, cx)
        .child(event_card::icon_tile(IconName::Map, cx))
        .child(event_card::header_text(
            crate::tr!("chat.plan_title").into_owned(),
            None,
            cx,
        ))
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
        event_card::footer(cx)
            .debug_selector(|| "plan-decision".into())
            .flex_wrap()
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
    event_card::event_card(tone, cx)
        .id(SharedString::from(format!("plan-{id}")))
        .debug_selector({
            let id = id.to_owned();
            move || format!("plan-card-{id}")
        })
        .flex_none()
        .child(header)
        .child(
            event_card::section(cx)
                .text_token(&theme.tokens.typography.md)
                .child(body),
        )
        .children(footer)
        .context_menu(margin_menu)
        .into_any_element()
}
