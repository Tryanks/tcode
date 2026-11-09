use std::path::{Path, PathBuf};

use agent::ItemContent;
use gpui::{
    AnyElement, App, ImageSource, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Window, img, px,
};
use gpui_base::h_flex;
use tcode_core::session::{EntryContent, TimelineEntry};

use crate::store::ImageHost;
use crate::theme::ActiveTheme as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a picture drawn here takes to fade in once it has decoded.
const ARRIVE: std::time::Duration = std::time::Duration::from_millis(200);

/// The images a run of reads showed, as tiles in a row that wraps. A tile
/// that arrives widens in, a tile it pushes onto another line glides there,
/// and the row's height glides to a line it starts.
pub(crate) fn image_tiles(
    host: ImageHost,
    entries: &[&TimelineEntry],
    cwd: &Path,
    session_id: &str,
    motion: &crate::motion::owned::Owned,
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
        host,
        SharedString::from(format!(
            "image-tiles-{}",
            entries.first().map_or("", |entry| entry.id.as_str())
        )),
        tiles,
        session_id,
        motion,
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

/// `tiles` in a row that wraps, keyed by image, its motion kept by
/// `motion`.
pub(crate) fn tile_row(
    host: ImageHost,
    id: SharedString,
    tiles: Vec<(SharedString, Tile)>,
    session_id: &str,
    motion: &crate::motion::owned::Owned,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let count = tiles.len() as u64;
    let session_id = session_id.to_owned();
    let glide = motion.glide_state(format!("{id}-glide"));
    let row = crate::motion::keyed::keyed_owned(
        motion,
        id,
        crate::motion::keyed::Fold::Across(px(0.)),
        tiles,
        ROWS,
        window,
        cx,
        |_, tile, _, cx| item_tile(host, tile.clone(), &session_id, motion, cx),
    )
    .render(h_flex().w_full().gap_2().flex_wrap());
    crate::motion::SizeGlide::new(&glide, row)
        .on_change(count)
        .into_any_element()
}

/// [`tile_row`] for a caller that holds only `&App`.
pub(crate) fn sampled_tile_row(
    host: ImageHost,
    id: SharedString,
    tiles: Vec<(SharedString, Tile)>,
    session_id: String,
    motion: &crate::motion::owned::Owned,
) -> AnyElement {
    let motion = motion.clone();
    crate::motion::Sampled::new(move |window, cx| {
        tile_row(host, id, tiles, &session_id, &motion, window, cx)
    })
    .into_any_element()
}

/// One image an item read, as a thumbnail that opens the image viewer.
pub(crate) fn item_tile(
    host: ImageHost,
    tile: Tile,
    session_id: &str,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let Tile {
        entry_id,
        copy: copy_items,
        index,
        title,
        path,
    } = tile;
    let preview = || {
        crate::store::item_image_preview(
            host,
            session_id.into(),
            entry_id.clone(),
            index,
            120.,
            80.,
        )
    };
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
                let motion = motion.clone();
                // The picture fades in over the tile's fill once it decodes.
                move |window, cx| {
                    let arrived = crate::motion::image_arrival(
                        crate::motion::Clock::from(&motion),
                        id,
                        &source,
                        ARRIVE,
                        window,
                        cx,
                    );
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
    .on_click({
        let session_id = session_id.to_owned();
        let entry_id = entry_id.clone();
        move |_, window, cx| {
            crate::image_viewer::open(
                item_original(host, &session_id, &entry_id, index, viewer_preview.clone()),
                title.clone(),
                window,
                cx,
            );
        }
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

/// What the image viewer opens on for the `index`th image item `entry_id`
/// read: its original, read from `host`, with `preview` until it arrives.
pub(crate) fn item_original(
    host: ImageHost,
    session_id: &str,
    entry_id: &str,
    index: usize,
    preview: ImageSource,
) -> crate::image_viewer::ViewerSource {
    crate::image_viewer::ViewerSource::original(
        host,
        tcode_protocol::OriginalTarget::Item {
            session_id: session_id.into(),
            item_id: entry_id.into(),
            image_index: index,
        },
        preview,
    )
}
