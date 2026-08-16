use crate::theme::ActiveTheme as _;
use agent::ProviderKind;
use gpui::{
    AnyElement, App, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, Styled as _, div, px,
};
use gpui_base::h_flex;

/// Width of the hairline stub flanking a divider label. Short on purpose: a
/// full-width rule would cut the flow in two, and these events only annotate it.
const STUB_WIDTH: f32 = 24.;

/// The one divider grammar: a centered 11px label between two hairline stubs.
/// `tint` colors the label alone — the rules stay border-quiet no matter what
/// the event is, so no divider can shout louder than another.
fn divider(id: SharedString, label: String, tint: Hsla, cx: &App) -> AnyElement {
    let stub = || div().h(px(1.)).w(px(STUB_WIDTH)).bg(cx.theme().border);
    h_flex()
        .id(id)
        .w_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(stub())
        .child(
            div()
                .flex_none()
                .text_size(px(11.))
                .text_color(tint)
                .child(label),
        )
        .child(stub())
        .into_any_element()
}

pub(crate) fn relay_divider(
    id: &str,
    from: ProviderKind,
    to: ProviderKind,
    cx: &App,
) -> AnyElement {
    divider(
        SharedString::from(format!("relay-{id}")),
        crate::tr!(
            "chat.relayed",
            from = from.display_name(),
            to = to.display_name()
        )
        .into_owned(),
        cx.theme().muted_foreground,
        cx,
    )
}

pub(crate) fn model_change_divider(
    id: &str,
    from: Option<&str>,
    to: &str,
    reason: Option<&str>,
    cx: &App,
) -> AnyElement {
    let label = match from {
        Some(from) => crate::tr!("chat.model_changed", from = from, to = to).into_owned(),
        None => crate::tr!("chat.model_changed_to", to = to).into_owned(),
    };
    let label = match reason {
        Some(reason) if !reason.is_empty() => format!("{label} ({reason})"),
        _ => label,
    };
    // A model swap is worth noticing, so the text carries a warning tint — but
    // nothing else about the row differs from a relay.
    divider(
        SharedString::from(format!("model-change-{id}")),
        label,
        cx.theme().warning,
        cx,
    )
}
