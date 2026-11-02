use std::path::{Path, PathBuf};

use agent::ItemContent;
use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window, img, px,
};
use gpui_base::h_flex;
use tcode_core::session::{EntryContent, TimelineEntry};

use crate::theme::ActiveTheme as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};

/// The images a run of reads showed, as tiles in a row that wraps. A tile
/// that arrives widens in, a tile it pushes onto another line glides there,
/// and the row's height glides to a line it starts.
pub(crate) fn image_tiles(
    entries: &[&TimelineEntry],
    cwd: &Path,
    session_id: &str,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let tiles: Vec<(SharedString, Tile)> = entries
        .iter()
        .flat_map(|entry| {
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
            let copy = crate::chat::tools::copy_items(entry);
            images
                .into_iter()
                .enumerate()
                .map(move |(index, (title, path))| {
                    let tile = Tile {
                        entry_id: entry.id.clone(),
                        copy: copy.clone(),
                        index,
                        title,
                        path,
                    };
                    (tile.key(), tile)
                })
        })
        .collect();
    tile_row(
        SharedString::from(format!(
            "image-tiles-{}",
            entries.first().map_or("", |entry| entry.id.as_str())
        )),
        tiles,
        session_id,
        window,
        cx,
    )
}

/// One image an item read, as its tile knows it.
#[derive(Clone)]
pub(crate) struct Tile {
    entry_id: String,
    copy: Vec<(String, String)>,
    index: usize,
    title: String,
    path: Option<PathBuf>,
}

impl Tile {
    pub(crate) fn of(entry: &TimelineEntry, index: usize, title: String) -> Self {
        Self {
            entry_id: entry.id.clone(),
            copy: crate::chat::tools::copy_items(entry),
            index,
            title,
            path: None,
        }
    }

    pub(crate) fn key(&self) -> SharedString {
        SharedString::from(format!("image-read-{}-{}", self.entry_id, self.index))
    }
}

/// `tiles` in a row that wraps, keyed by image.
pub(crate) fn tile_row(
    id: SharedString,
    tiles: Vec<(SharedString, Tile)>,
    session_id: &str,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let count = tiles.len() as u64;
    let session_id = session_id.to_owned();
    let glide =
        crate::motion::GlideState::keyed(SharedString::from(format!("{id}-glide")), window, cx);
    let row = crate::motion::keyed_across(id, tiles, window, cx, |_, tile, _, cx| {
        item_tile(tile.clone(), &session_id, cx)
    })
    .render(h_flex().w_full().gap_2().flex_wrap());
    crate::motion::SizeGlide::new(&glide, row)
        .on_change(count)
        .into_any_element()
}

/// [`tile_row`] for a caller that holds only `&App`.
pub(crate) fn sampled_tile_row(
    id: SharedString,
    tiles: Vec<(SharedString, Tile)>,
    session_id: String,
) -> AnyElement {
    crate::motion::Sampled::new(move |window, cx| tile_row(id, tiles, &session_id, window, cx))
        .into_any_element()
}

/// One image an item read, as a thumbnail that opens the image viewer.
pub(crate) fn item_tile(tile: Tile, session_id: &str, cx: &App) -> AnyElement {
    let Tile {
        entry_id,
        copy: copy_items,
        index,
        title,
        path,
    } = tile;
    let source = tcode_protocol::OriginalTarget::Item {
        session_id: session_id.into(),
        item_id: entry_id.clone(),
        image_index: index,
    };
    let preview =
        || crate::store::item_image_preview(session_id.into(), entry_id.clone(), index, 120., 80.);
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
            .child(crate::motion::Sampled::new({
                let source = preview();
                let id = SharedString::from(format!("image-read-{entry_id}-{index}"));
                // The picture fades in over the tile's fill once it decodes.
                move |window, cx| {
                    let arrived = crate::motion::image_arrival(id, &source, window, cx);
                    img(source)
                        .id("tool-image")
                        .w_full()
                        .h_full()
                        .object_fit(ObjectFit::Cover)
                        .opacity(arrived)
                        .with_fallback(crate::image_viewer::unpreviewed(true, false, cx))
                        .into_any_element()
                }
            })),
        SharedString::from(format!("image-read-{entry_id}-{index}")),
        Role::Button,
        title.clone(),
        cx,
    )
    .debug_selector({
        let selector = format!("image-read-{entry_id}-{index}");
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
