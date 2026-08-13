use crate::theme::ActiveTheme as _;
use agent::ProviderKind;
use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    Styled as _, div, px,
};
use gpui_component::h_flex;

pub(crate) fn relay_divider(
    id: &str,
    from: ProviderKind,
    to: ProviderKind,
    cx: &App,
) -> AnyElement {
    let border = cx.theme().border;
    let muted = cx.theme().muted_foreground;
    h_flex()
        .id(SharedString::from(format!("relay-{id}")))
        .w_full()
        .items_center()
        .gap_3()
        .child(div().h(px(1.)).flex_1().bg(border))
        .child(
            div()
                .flex_none()
                .px_2()
                .py_0p5()
                .rounded_full()
                .border_1()
                .border_color(border)
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!(
                    "chat.relayed",
                    from = from.display_name(),
                    to = to.display_name()
                )),
        )
        .child(div().h(px(1.)).flex_1().bg(border))
        .into_any_element()
}

pub(crate) fn model_change_divider(
    id: &str,
    from: Option<&str>,
    to: &str,
    reason: Option<&str>,
    cx: &App,
) -> AnyElement {
    let warning = cx.theme().warning;
    let label = match from {
        Some(from) => crate::tr!("chat.model_changed", from = from, to = to).into_owned(),
        None => crate::tr!("chat.model_changed_to", to = to).into_owned(),
    };
    let label = match reason {
        Some(reason) if !reason.is_empty() => format!("{label} ({reason})"),
        _ => label,
    };

    h_flex()
        .id(SharedString::from(format!("model-change-{id}")))
        .w_full()
        .items_center()
        .gap_3()
        .child(div().h(px(1.)).flex_1().bg(warning.opacity(0.45)))
        .child(
            div()
                .flex_none()
                .px_2()
                .py_0p5()
                .rounded_full()
                .border_1()
                .border_color(warning.opacity(0.55))
                .text_size(px(11.))
                .text_color(warning)
                .child(label),
        )
        .child(div().h(px(1.)).flex_1().bg(warning.opacity(0.45)))
        .into_any_element()
}
