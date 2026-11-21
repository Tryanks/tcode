//! Standalone visualization card in the conversation stream.
//!
//! Visual calls stand apart from collapsed Work Logs like plans and images:
//! a titled bordered sheet hosting our reusable [`HtmlView`], with controls to
//! toggle between rendered HTML and raw source and to copy the HTML source.

use std::sync::Arc;

use agent::ItemStatus;
use gpui::{
    AnyElement, App, ClickEvent, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use crate::html::{HtmlState, HtmlView};
use crate::icon::{Icon, IconName};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::Spinner;
use crate::widgets::copy::{action_button, copy_button};
use crate::widgets::menu::{ContextMenuExt as _, CopyText};

pub(crate) struct VisualizeCardData<'a> {
    pub(crate) id: &'a str,
    pub(crate) title: &'a str,
    pub(crate) description: Option<&'a str>,
    pub(crate) html: Arc<str>,
    pub(crate) state: Entity<HtmlState>,
    pub(crate) status: ItemStatus,
    pub(crate) collapsed: bool,
    pub(crate) showing_source: bool,
    pub(crate) copied: bool,
    pub(crate) compact: bool,
    pub(crate) viewport_width: Option<f32>,
    pub(crate) motion: crate::motion::owned::Owned,
}

pub(crate) fn visualize_card(
    data: VisualizeCardData<'_>,
    on_toggle_collapse: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_toggle_source: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let VisualizeCardData {
        id,
        title,
        description,
        html,
        state,
        status,
        collapsed,
        showing_source,
        copied,
        compact,
        viewport_width,
        motion,
    } = data;

    let theme = cx.theme();
    let radius = crate::material::radius_card(cx);
    let in_progress = status == ItemStatus::InProgress;
    let has_html = !html.trim().is_empty();

    let (toggle_icon, toggle_label) = if showing_source {
        (IconName::Eye, crate::tr!("chat.visualize_show_rendered"))
    } else {
        (IconName::Code, crate::tr!("chat.visualize_show_source"))
    };

    let (collapse_icon, collapse_label) = if collapsed {
        (IconName::ChevronDown, crate::tr!("chat.visualize_expand"))
    } else {
        (IconName::ChevronUp, crate::tr!("chat.visualize_collapse"))
    };

    let collapse_btn = {
        let selector = format!("visualize-collapse-{id}");
        action_button(
            format!("visualize-collapse-{id}"),
            collapse_icon,
            collapse_label.into_owned(),
            cx,
        )
        .touch(compact)
        .motion(&motion)
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_toggle_collapse(event, window, cx);
        })
        .debug_selector(move || selector)
    };

    let toggle_btn = (has_html && !collapsed).then(|| {
        let selector = format!("visualize-toggle-{id}");
        action_button(
            format!("visualize-toggle-{id}"),
            toggle_icon,
            toggle_label.into_owned(),
            cx,
        )
        .touch(compact)
        .motion(&motion)
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_toggle_source(event, window, cx);
        })
        .debug_selector(move || selector)
    });

    let copy_btn = has_html.then(|| {
        copy_button(
            &format!("visualize-{id}"),
            copied,
            compact,
            move |event, window, cx| {
                cx.stop_propagation();
                on_copy(event, window, cx);
            },
            cx,
        )
        .motion(&motion)
    });

    let header = h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .pl_3()
        .pr_1p5()
        .py_1p5()
        .when(collapsed, |bar| bar.rounded(radius))
        .when(!collapsed, |bar| {
            bar.rounded_t(radius)
                .border_b_1()
                .border_color(theme.border)
        })
        .bg(theme.tokens.colors.muted)
        .child(if in_progress {
            Spinner::new().small().into_any_element()
        } else {
            Icon::new(IconName::LayoutDashboard)
                .small()
                .text_color(theme.muted_foreground)
                .into_any_element()
        })
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .text_body(false, cx)
                        .font_semibold()
                        .truncate()
                        .child(title.to_string()),
                )
                .when_some(description, |row, desc| {
                    row.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .truncate()
                            .child(desc.to_string()),
                    )
                }),
        )
        .child(
            h_flex()
                .flex_shrink_0()
                .items_center()
                .gap_0p5()
                .children(toggle_btn)
                .children(copy_btn)
                .child(collapse_btn),
        );

    let body: Option<AnyElement> = (!collapsed).then(|| {
        if !has_html && in_progress {
            div()
                .w_full()
                .p_4()
                .text_secondary(false, cx)
                .child(crate::tr!("chat.visualize_building").into_owned())
                .into_any_element()
        } else if showing_source {
            let source_id = SharedString::from(format!("visualize-source-{id}"));
            div()
                .id(source_id)
                .w_full()
                .max_h(px(460.0))
                .overflow_scroll()
                .p_3()
                .bg(theme.tokens.colors.muted.opacity(0.45))
                .font_family(theme.mono_font_family.clone())
                .text_size(theme.tokens.typography.mono_md.size)
                .text_color(theme.foreground)
                .child(html.to_string())
                .into_any_element()
        } else {
            let mut view = HtmlView::new(&state);
            if let Some(w) = viewport_width {
                view = view.viewport_width(w);
            }
            div()
                .w_full()
                .min_w_0()
                .p_3()
                .child(view)
                .into_any_element()
        }
    });

    let copy_html = html.to_string();
    let card_id = SharedString::from(format!("visualize-card-{id}"));
    let selector = format!("visualize-card-{id}");
    v_flex()
        .id(card_id)
        .w_full()
        .min_w_0()
        .rounded(radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .overflow_hidden()
        .child(header)
        .children(body)
        .debug_selector(move || selector)
        .context_menu(move |menu, _, _| {
            menu.menu_with_icon(
                crate::tr!("chat.copy_html").into_owned(),
                IconName::Copy,
                Box::new(CopyText(copy_html.clone())),
            )
        })
        .into_any_element()
}
