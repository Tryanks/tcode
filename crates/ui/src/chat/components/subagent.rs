use crate::icon::{Icon, IconName};
use crate::material::semantic_chip;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::TooltipTrigger as _;
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
/// only moves the focus there. A row that opens its agent fills under the
/// pointer as `hover`, the timeline's, follows it.
pub(crate) fn subagent_row(
    entry: &TimelineEntry,
    on_open: Option<ClickHandler>,
    shown: bool,
    hover: crate::motion::ListHover,
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
    let row_label = crate::tr!(
        "chat.subagent_row",
        agent = agent_type.clone(),
        description = one_line(description)
    )
    .into_owned();
    // The shared header grammar inside the agent's own pill: what it does,
    // the agent, how it stands, how long it took. The model and the summary
    // arrive after the pill does, and widen into it.
    let glyph = crate::chat::tools::window::status_glyph(
        &entry.id,
        crate::icon::IconNamed::path(IconName::Bot),
        *status,
        cx,
    );
    let verb = crate::chat::tools::window::Verb("agent").text(*status);
    let status_chip = crate::chat::tools::window::status_chip(&entry.id, *status, cx);
    let elapsed = crate::chat::tools::window::elapsed_of(entry)
        .filter(|elapsed| *elapsed >= 100)
        .map(super::super::model::format_elapsed);
    let chip = model_chip(model, effort);
    let summary = summary
        .as_deref()
        .filter(|summary| !summary.is_empty())
        .map(one_line);
    let (id, agent_type, description, failed) = (
        entry.id.clone(),
        agent_type.clone(),
        one_line(description),
        *status == ItemStatus::Failed,
    );
    // A pane showing the agent fills the pill, and its check comes in.
    let clickable = on_open.is_some();
    crate::motion::Sampled::new(move |window, cx| {
        let muted = cx.theme().muted_foreground;
        let gap = gpui::rems(0.5).to_pixels(window.rem_size());
        let fill = gpui_base::motion::transition(
            SharedString::from(format!("subagent-fill-{id}")),
            if shown && clickable {
                cx.theme().accent
            } else {
                crate::material::content_surface(cx)
            },
            crate::motion::settle(),
            window,
            cx,
        );
        let check_id = SharedString::from(format!("subagent-shown-{id}"));
        let check = crate::motion::shown(
            check_id.clone(),
            (shown && clickable).then_some(()),
            window,
            cx,
        )
        .map(|check| {
            let label = SharedString::from(crate::tr!("panes.shown_beside").into_owned());
            check.fold_across_gap(
                gap,
                div()
                    .id(check_id)
                    .debug_selector(|| "subagent-shown".into())
                    .role(Role::Status)
                    .aria_label(label.clone())
                    .with_tooltip(move |_, _| crate::widgets::tooltip::Tooltip::new(label.clone()))
                    .child(Icon::new(IconName::Check).text_color(muted)),
            )
        });
        let chip = crate::motion::shown(
            SharedString::from(format!("subagent-model-{id}")),
            chip,
            window,
            cx,
        )
        .map(|chip| {
            let theme = cx.theme();
            chip.fold_across_gap(
                gap,
                semantic_chip(chip.value.clone(), theme.muted.opacity(0.4), muted, cx)
                    .font_family(theme.mono_font_family.clone()),
            )
        });
        let summary = crate::motion::shown(
            SharedString::from(format!("subagent-summary-{id}")),
            summary,
            window,
            cx,
        )
        .map(|summary| {
            let danger = cx.theme().danger;
            summary.fold_across_gap(
                gap,
                div()
                    .max_w(px(180.))
                    .min_w_0()
                    .overflow_hidden()
                    .text_meta_mono(false, cx)
                    .when(failed, |summary| summary.text_color(danger))
                    .child(crate::widgets::fade_text(summary.value.clone())),
            )
        });
        let row = h_flex()
            .debug_selector({
                let id = id.clone();
                move || format!("subagent-{id}")
            })
            .w_full()
            .h(px(crate::material::TOUCH_TARGET))
            .min_w_0()
            .gap(gap)
            .items_center()
            .px_3()
            .rounded_full()
            .border_1()
            .border_color(cx.theme().border)
            .bg(fill)
            .text_body(false, cx)
            .when(clickable, |row| {
                row.relative().child(hover.layer(
                    format!("subagent-row-{id}"),
                    cx.theme().accent,
                    px(999.),
                ))
            })
            .child(glyph)
            .child(div().flex_none().font_medium().child(verb))
            .child(div().flex_none().font_medium().child(agent_type))
            .children(chip)
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .overflow_hidden()
                    .text_color(muted)
                    .child(crate::widgets::fade_text(description)),
            )
            .children(summary)
            .children(status_chip)
            .when_some(elapsed, |row, elapsed| {
                row.child(div().flex_none().text_meta_mono(false, cx).child(elapsed))
            })
            .children(check);
        let Some(on_open) = on_open else {
            return row.into_any_element();
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
        .on_click(on_open)
        .into_any_element()
    })
    .into_any_element()
}
