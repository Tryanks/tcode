use std::path::Path;

use gpui::{
    App, Div, Entity, InteractiveElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_base::Button;

use crate::{icon::Icon, sizing::Sizable as _, theme::ActiveTheme as _, widgets::Tooltip};

use super::{
    MarkdownState,
    nodes::{InlineNode, LinkMark},
};

/// Image links are recognized by their destination, without probing the client's files.
pub(super) fn for_node(node: &InlineNode) -> Option<&LinkMark> {
    node.marks.iter().find_map(|(range, mark)| {
        let link = mark.link.as_ref()?;
        if *range != (0..node.text.len()) {
            return None;
        }
        let parsed = url::Url::parse(&link.url).ok();
        let path = match &parsed {
            Some(url) if matches!(url.scheme(), "http" | "https" | "file") => url.path(),
            Some(_) => return None,
            None => link.url.as_ref(),
        };
        let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
        matches!(
            extension.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"
        )
        .then_some(link)
    })
}

pub(super) fn badge(
    ix: usize,
    view: Entity<MarkdownState>,
    url: SharedString,
    label: SharedString,
    window: &Window,
    cx: &App,
) -> Div {
    let (row_height, badge_height) = if crate::window_seam::window_is_compact(window, cx) {
        (44., 38.)
    } else {
        (28., 26.)
    };
    let hover = cx.theme().secondary_active;
    let tooltip_url = url.clone();
    let badge = Button::new(("markdown-image-link", ix))
        .rounded(px(8.))
        .h(px(badge_height))
        .px_2()
        .gap_1()
        .flex_shrink_0()
        .max_w_full()
        .border_1()
        .border_color(cx.theme().border)
        .bg(crate::material::content_surface(cx))
        .text_size(px(13.))
        .text_color(cx.theme().foreground)
        .hover(move |badge| badge.bg(hover))
        .child(
            Icon::default()
                .path("icons/image.svg")
                .small()
                .text_color(cx.theme().link),
        )
        .accessibility_label(label.clone())
        .tooltip(move |window, cx| Tooltip::new(tooltip_url.clone()).build(window, cx))
        .child(div().min_w_0().truncate().child(label.clone()))
        .on_click(move |_, window, cx| {
            gpui_base::TextSelection::end(window, cx);
            cx.stop_propagation();
            let source = view.read(cx).image_source(&url.to_string().into());
            crate::attachments::open_image_lightbox(source, label.to_string(), window, cx);
        });
    // Preserve the line's spacing while shortening only the visible badge.
    div()
        .flex()
        .items_center()
        .min_h(px(row_height))
        .max_w(px(360.))
        .child(badge)
}
