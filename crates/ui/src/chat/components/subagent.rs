use crate::icon::{Icon, IconName};
use crate::material::semantic_chip;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::spinner::Spinner;
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

/// `model · effort` label for the row chip; `None` when neither is known.
fn model_chip(model: &Option<String>, effort: &Option<String>) -> Option<String> {
    let parts: Vec<&str> = [model, effort]
        .into_iter()
        .filter_map(|part| part.as_deref())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// `shown` marks a row whose thread a pane already shows, where opening it
/// only moves the focus there.
pub(crate) fn subagent_row(
    entry: &TimelineEntry,
    on_open: Option<ClickHandler>,
    shown: bool,
    cx: &App,
) -> AnyElement {
    let EntryContent::Item(ItemContent::Subagent {
        agent_type,
        description,
        status,
        summary,
        model,
        effort,
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
    let theme = cx.theme();
    let lifecycle: AnyElement = match status {
        ItemStatus::InProgress => Spinner::new().color(theme.primary).into_any_element(),
        ItemStatus::Completed => semantic_chip(
            crate::tr!("chat.subagent_completed"),
            theme.success.opacity(0.12),
            theme.success,
            cx,
        )
        .into_any_element(),
        ItemStatus::Interrupted => semantic_chip(
            crate::tr!("chat.subagent_interrupted"),
            theme.warning.opacity(0.12),
            theme.warning,
            cx,
        )
        .into_any_element(),
        ItemStatus::Failed | ItemStatus::Declined => semantic_chip(
            crate::tr!("chat.subagent_failed"),
            theme.danger.opacity(0.12),
            theme.danger,
            cx,
        )
        .into_any_element(),
    };
    let row = h_flex()
        .w_full()
        .h(px(crate::material::TOUCH_TARGET))
        .min_w_0()
        .gap_2()
        .items_center()
        .px_3()
        .rounded_full()
        .border_1()
        .border_color(cx.theme().border)
        .bg(crate::material::content_surface(cx))
        .text_body(false, cx)
        .child(lifecycle)
        .child(div().flex_none().font_medium().child(agent_type.clone()))
        .when_some(model_chip(model, effort), |row, chip| {
            row.child(
                semantic_chip(chip, theme.muted.opacity(0.4), muted, cx)
                    .font_family(theme.mono_font_family.clone()),
            )
        })
        .child(
            div()
                .min_w_0()
                .flex_1()
                .overflow_hidden()
                .text_color(muted)
                .child(crate::widgets::fade_text(one_line(description))),
        )
        .when_some(
            summary.as_deref().filter(|summary| !summary.is_empty()),
            |row, summary| {
                row.child(
                    div()
                        .max_w(px(180.))
                        .min_w_0()
                        .overflow_hidden()
                        .text_meta_mono(false, cx)
                        .child(crate::widgets::fade_text(one_line(summary))),
                )
            },
        );

    let Some(on_open) = on_open else {
        return row.into_any_element();
    };
    let id = entry.id.clone();
    let rest = crate::material::content_surface(cx);
    let fill = cx.theme().accent;
    // The fill and the check come and go as a pane starts or stops showing
    // the agent.
    crate::motion::Sampled::new(move |window, cx| {
        let row = row.bg(gpui_base::motion::transition(
            SharedString::from(format!("subagent-fill-{id}")),
            if shown { fill } else { rest },
            crate::motion::settle(),
            window,
            cx,
        ));
        let check_id = SharedString::from(format!("subagent-shown-{id}"));
        let row = match crate::motion::shown(check_id.clone(), shown.then_some(()), window, cx) {
            Some(check) => {
                let label = SharedString::from(crate::tr!("panes.shown_beside").into_owned());
                row.child(
                    check.fade(
                        div()
                            .id(check_id)
                            .debug_selector(|| "subagent-shown".into())
                            .role(Role::Status)
                            .aria_label(label.clone())
                            .tooltip(move |window, cx| {
                                crate::widgets::tooltip::Tooltip::new(label.clone())
                                    .build(window, cx)
                            })
                            .child(Icon::new(IconName::Check).text_color(muted)),
                    ),
                )
            }
            None => row,
        };
        crate::material::accessible_clickable(
            row,
            SharedString::from(format!("subagent-row-{id}")),
            Role::Button,
            row_label,
            cx,
        )
        .debug_selector(move || format!("subagent-row-{id}"))
        .cursor_pointer()
        .hover(move |row| row.bg(fill))
        .on_click(on_open)
        .into_any_element()
    })
    .into_any_element()
}
