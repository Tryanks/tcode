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
use gpui_base::{StyledExt as _, h_flex};

use agent::{ItemContent, ItemStatus};
use tcode_core::session::{EntryContent, TimelineEntry};

use super::super::model::one_line;

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub(crate) fn subagent_row(
    entry: &TimelineEntry,
    on_open: Option<ClickHandler>,
    cx: &App,
) -> AnyElement {
    let EntryContent::Item(ItemContent::Subagent {
        agent_type,
        description,
        status,
        summary,
    }) = &entry.content
    else {
        log::error!(
            "subagent row received non-subagent timeline entry `{}`",
            entry.id
        );
        debug_assert!(false, "subagent row requires subagent timeline content");
        return div().into_any_element();
    };
    let muted = cx.theme().muted_foreground;
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
        ItemStatus::Interrupted => h_flex()
            .h(px(22.))
            .px_2()
            .gap_1()
            .items_center()
            .rounded(crate::material::radius_chip())
            .bg(cx.theme().warning.opacity(0.12))
            .text_color(cx.theme().warning)
            .text_size(px(11.5))
            .child(
                Icon::new(IconName::CircleX)
                    .size(px(12.))
                    .text_color(cx.theme().warning),
            )
            .child(crate::tr!("chat.subagent_interrupted"))
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
    let row = h_flex()
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
        .when_some(
            summary.as_deref().filter(|summary| !summary.is_empty()),
            |row, summary| {
                row.child(
                    div()
                        .max_w(px(180.))
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(muted)
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(px(11.5))
                        .child(one_line(summary)),
                )
            },
        );

    if let Some(on_open) = on_open {
        crate::material::accessible_clickable(
            row,
            SharedString::from(format!("subagent-row-{}", entry.id)),
            Role::Button,
            row_label,
            cx,
        )
        .cursor_pointer()
        .hover(|row| row.bg(cx.theme().accent))
        .on_click(on_open)
        .into_any_element()
    } else {
        row.into_any_element()
    }
}
