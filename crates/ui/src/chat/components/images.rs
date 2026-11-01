use std::path::{Path, PathBuf};

use agent::ItemContent;
use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, img, px,
};
use gpui_base::h_flex;
use tcode_core::session::{EntryContent, TimelineEntry};

use crate::theme::ActiveTheme as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};

pub(crate) fn image_tiles(
    entries: &[&TimelineEntry],
    cwd: &Path,
    session_id: &str,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .flex_wrap()
        .children(entries.iter().flat_map(|entry| {
            let EntryContent::Item(item) = &entry.content else {
                unreachable!()
            };
            let images: Vec<(String, Option<PathBuf>)> = match item {
                ItemContent::ImageRead { path, .. } => {
                    let path = cwd.join(path);
                    let title = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    vec![(title, Some(path))]
                }
                ItemContent::ToolCall {
                    image_reads, name, ..
                } => (0..image_reads.len())
                    .map(|index| (format!("{name} · {}", index + 1), None))
                    .collect(),
                _ => unreachable!(),
            };
            images
                .into_iter()
                .enumerate()
                .map(move |(index, (title, path))| {
                    item_tile(entry, index, title, path, session_id, cx)
                })
        }))
}

/// One image an item read, as a thumbnail that opens the image viewer.
pub(crate) fn item_tile(
    entry: &TimelineEntry,
    index: usize,
    title: String,
    path: Option<PathBuf>,
    session_id: &str,
    cx: &App,
) -> AnyElement {
    let source = tcode_protocol::OriginalTarget::Item {
        session_id: session_id.into(),
        item_id: entry.id.clone(),
        image_index: index,
    };
    let copy_items = crate::chat::tools::copy_items(entry);
    let preview =
        || crate::store::item_image_preview(session_id.into(), entry.id.clone(), index, 120., 80.);
    let viewer_preview = preview();
    // The image is the control; a `Button` would add chrome around a picture.
    crate::material::accessible_clickable(
        gpui::div()
            .w(px(120.))
            .h(px(80.))
            .flex_shrink_0()
            .cursor_pointer()
            .rounded(crate::material::radius_card(cx))
            .overflow_hidden()
            .bg(cx.theme().muted)
            .child(
                img(preview())
                    .id("tool-image")
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Cover)
                    .with_fallback(crate::image_viewer::unpreviewed(true, false, cx)),
            ),
        SharedString::from(format!("image-read-{}-{index}", entry.id)),
        Role::Button,
        title.clone(),
        cx,
    )
    .debug_selector({
        let selector = format!("image-read-{}-{index}", entry.id);
        move || selector.clone()
    })
    .on_click(move |_, window, cx| {
        crate::image_viewer::open(
            crate::image_viewer::ViewerSource::original(source.clone(), viewer_preview.clone()),
            title.clone(),
            window,
            cx,
        );
    })
    .context_menu(move |mut menu, _, _| {
        if let Some(path) = &path {
            menu = menu.path_items(&path.to_string_lossy(), None);
        }
        copy_items
            .iter()
            .cloned()
            .fold(menu, |menu, (label, text)| {
                menu.menu(label, Box::new(CopyText(text)))
            })
    })
    .into_any_element()
}
