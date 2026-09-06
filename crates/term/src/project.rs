//! Project a host [`TermSnapshot`] into the replicated wire types.
//!
//! This is the only place rio's grid representation is translated, so the
//! runtime's live projection and the chat command panel's local rendering
//! stay one implementation.

use std::collections::HashMap;

use rio_vt::{
    ansi::CursorShape as RioCursorShape,
    config::colors::{AnsiColor, ColorRgb, NamedColor},
    crosswords::{
        grid::row::Row,
        square::{ContentTag, Square, Wide},
        style::Style,
    },
};
use tcode_protocol::terminal::{
    CellWidth, HISTORY_LIMIT, TerminalCell, TerminalColor, TerminalCursor, TerminalLink,
    TerminalModes, TerminalOverlay, TerminalRow, TerminalStyle,
};

use crate::{
    TermSnapshot,
    graphics::{
        AtlasPlacement, IncompletePlacement, KittyPlacement, OverlayViewport, PLACEHOLDER,
        PlaceholderRun, VirtualPlacement, atlas_overlay_geometry, compute_run_geometry,
        kitty_image_key, kitty_overlay_geometry,
    },
};

/// Interns cell styles into one table per wire message.
#[derive(Debug)]
pub struct Projector {
    styles: Vec<TerminalStyle>,
    index: HashMap<TerminalStyle, u16>,
}

impl Default for Projector {
    fn default() -> Self {
        let default = TerminalStyle::default();
        Self {
            styles: vec![default],
            index: HashMap::from([(default, 0)]),
        }
    }
}

impl Projector {
    /// Project one visible row.
    pub fn visible_row(&mut self, snapshot: &TermSnapshot, row: usize) -> TerminalRow {
        match snapshot.visible_rows.get(row) {
            Some(row) => self.row(snapshot, row),
            None => TerminalRow::default(),
        }
    }

    /// Project one retained scrollback row (oldest first).
    pub fn history_row(&mut self, snapshot: &TermSnapshot, row: usize) -> TerminalRow {
        match snapshot.history_rows.get(row) {
            Some(row) => self.row(snapshot, row),
            None => TerminalRow::default(),
        }
    }

    /// Project every retained scrollback row carried by `snapshot`.
    pub fn history(&mut self, snapshot: &TermSnapshot) -> Vec<TerminalRow> {
        (0..snapshot.history_rows.len())
            .map(|row| self.history_row(snapshot, row))
            .collect()
    }

    pub fn styles(&self) -> &[TerminalStyle] {
        &self.styles
    }

    pub fn into_styles(self) -> Vec<TerminalStyle> {
        self.styles
    }

    fn row(&mut self, snapshot: &TermSnapshot, row: &Row<Square>) -> TerminalRow {
        let wrapped = row
            .inner
            .get(snapshot.cols.saturating_sub(1))
            .is_some_and(|square| square.wrapline());
        let mut cells = Vec::with_capacity(snapshot.cols);
        for square in row.inner.iter().take(snapshot.cols) {
            cells.push(self.cell(snapshot, square));
        }
        // Trailing defaults are implied; a reader pads back to `cols`.
        while cells
            .last()
            .is_some_and(|cell| *cell == TerminalCell::default())
        {
            cells.pop();
        }
        TerminalRow { cells, wrapped }
    }

    fn cell(&mut self, snapshot: &TermSnapshot, square: &Square) -> TerminalCell {
        let style = self.intern(wire_style(style_of(square, &snapshot.styles)));
        TerminalCell {
            text: cell_text(square, &snapshot.zero_width),
            width: match square.wide() {
                Wide::Wide => CellWidth::Wide,
                Wide::Spacer => CellWidth::Spacer,
                // A leading spacer is end-of-line padding, not half of a wide
                // character: it paints and hit-tests as an ordinary blank.
                Wide::Narrow | Wide::LeadingSpacer => CellWidth::Narrow,
            },
            style,
            link: link_of(square, &snapshot.links),
        }
    }

    fn intern(&mut self, style: TerminalStyle) -> u16 {
        if let Some(index) = self.index.get(&style) {
            return *index;
        }
        // rio's own style table is u16-indexed, so this cannot realistically
        // overflow; a saturating fallback would silently mis-style instead.
        let index = u16::try_from(self.styles.len()).unwrap_or(u16::MAX);
        self.styles.push(style);
        self.index.insert(style, index);
        index
    }
}

/// The cursor position within the host's visible screen.
pub fn cursor(snapshot: &TermSnapshot) -> Option<TerminalCursor> {
    let (row, col) = snapshot.cursor?;
    Some(TerminalCursor {
        row: row.min(u16::MAX as usize) as u16,
        col: col.min(u16::MAX as usize) as u16,
        shape: match snapshot.cursor_state.content {
            RioCursorShape::Block => tcode_protocol::terminal::CursorShape::Block,
            RioCursorShape::Underline => tcode_protocol::terminal::CursorShape::Underline,
            RioCursorShape::Beam => tcode_protocol::terminal::CursorShape::Beam,
            RioCursorShape::Hidden => tcode_protocol::terminal::CursorShape::Hidden,
        },
        blinking: snapshot.cursor_blinking,
    })
}

pub fn modes(snapshot: &TermSnapshot, modify_other_keys: Option<u8>) -> TerminalModes {
    TerminalModes {
        mode: snapshot.mode.bits(),
        keyboard: snapshot.keyboard_mode.bits(),
        modify_other_keys,
    }
}

/// Lay out every image placement in the snapshot.
///
/// Geometry is computed once, on the host, against a viewport whose origin is
/// the top-left of the **oldest retained scrollback row** and whose cell size
/// is the client's own physical cell size. A client only has to translate by
/// its grid origin and its local scroll offset, then clip.
pub fn overlays(
    snapshot: &TermSnapshot,
    cell_width: f32,
    cell_height: f32,
    image_size: impl Fn(u64) -> Option<(usize, usize)>,
) -> Vec<TerminalOverlay> {
    let retained = snapshot.history_size.min(HISTORY_LIMIT) as i64;
    let viewport = OverlayViewport {
        cell_width,
        cell_height,
        origin_x: 0.,
        origin_y: 0.,
        history_size: (snapshot.lines_evicted.min(i64::MAX as u64) as i64)
            .saturating_add(snapshot.history_size.min(i64::MAX as usize) as i64),
        display_offset: retained,
        screen_lines: snapshot.screen_lines as i64 + retained,
    };

    let mut overlays: Vec<(TerminalOverlay, u8, u32)> = Vec::new();
    let mut push = |overlay: TerminalOverlay, protocol_order: u8, placement_order: u32| {
        if overlay.width > 0. && overlay.height > 0. {
            overlays.push((overlay, protocol_order, placement_order));
        }
    };

    for placement in &snapshot.atlas_placements {
        push_atlas(&mut push, placement, &viewport, &image_size);
    }
    for placement in &snapshot.kitty_placements {
        push_kitty(&mut push, placement, &viewport, &image_size);
    }
    for paint in placeholder_runs(snapshot) {
        push_virtual(
            &mut push,
            &paint,
            &snapshot.kitty_virtual_placements,
            &viewport,
            retained,
            &image_size,
        );
    }

    overlays.sort_by_key(|(overlay, protocol_order, placement_order)| {
        (
            overlay.z_index,
            *protocol_order,
            overlay.image_key,
            *placement_order,
        )
    });
    overlays
        .into_iter()
        .map(|(overlay, _, _)| overlay)
        .collect()
}

fn push_atlas(
    push: &mut impl FnMut(TerminalOverlay, u8, u32),
    placement: &AtlasPlacement,
    viewport: &OverlayViewport,
    image_size: &impl Fn(u64) -> Option<(usize, usize)>,
) {
    if image_size(placement.image_key).is_none() {
        return;
    }
    let Some(geometry) = atlas_overlay_geometry(placement, viewport) else {
        return;
    };
    push(
        TerminalOverlay {
            image_key: placement.image_key,
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            z_index: -1,
            source_rect: geometry.source_rect,
        },
        0,
        0,
    );
}

fn push_kitty(
    push: &mut impl FnMut(TerminalOverlay, u8, u32),
    placement: &KittyPlacement,
    viewport: &OverlayViewport,
    image_size: &impl Fn(u64) -> Option<(usize, usize)>,
) {
    let image_key = kitty_image_key(placement.image_id);
    let Some((width, height)) = image_size(image_key) else {
        return;
    };
    let Some(geometry) = kitty_overlay_geometry(placement, width, height, viewport) else {
        return;
    };
    push(
        TerminalOverlay {
            image_key,
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            z_index: placement.z_index,
            source_rect: geometry.source_rect,
        },
        1,
        placement.placement_id,
    );
}

fn push_virtual(
    push: &mut impl FnMut(TerminalOverlay, u8, u32),
    paint: &PlaceholderPaint,
    placements: &HashMap<(u32, u32), VirtualPlacement>,
    viewport: &OverlayViewport,
    retained: i64,
    image_size: &impl Fn(u64) -> Option<(usize, usize)>,
) {
    let placement = placements
        .get(&(paint.run.image_id, paint.run.placement_id))
        .or_else(|| placements.get(&(paint.run.image_id, 0)));
    let Some(placement) = placement else {
        return;
    };
    let image_key = kitty_image_key(paint.run.image_id);
    let Some((width, height)) = image_size(image_key) else {
        return;
    };
    let (Ok(image_width), Ok(image_height)) = (u32::try_from(width), u32::try_from(height)) else {
        return;
    };
    // The viewport origin is the oldest retained row, so a screen line sits
    // `retained` rows below it.
    let Ok(screen_line) = usize::try_from(paint.screen_line as i64 + retained) else {
        return;
    };
    let Some(geometry) = compute_run_geometry(
        &paint.run,
        placement.columns,
        placement.rows,
        image_width,
        image_height,
        (placement.x, placement.y, placement.width, placement.height),
        viewport.cell_width,
        viewport.cell_height,
        viewport.origin_x,
        viewport.origin_y,
        screen_line,
        paint.start_screen_col,
    ) else {
        return;
    };
    push(
        TerminalOverlay {
            image_key,
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            // rio's own renderer puts virtual placements below glyphs.
            z_index: -1,
            source_rect: geometry.source_rect,
        },
        2,
        placement.placement_id,
    );
}

struct PlaceholderPaint {
    run: PlaceholderRun,
    screen_line: usize,
    start_screen_col: usize,
}

fn placeholder_runs(snapshot: &TermSnapshot) -> Vec<PlaceholderPaint> {
    let mut paints = Vec::new();
    for (screen_line, row) in snapshot.visible_rows.iter().enumerate() {
        if !row.kitty_virtual_placeholder {
            continue;
        }
        let mut current: Option<(IncompletePlacement, usize)> = None;
        for (col, square) in row.inner.iter().take(snapshot.cols).enumerate() {
            if square.c() != PLACEHOLDER {
                flush(&mut paints, &mut current, screen_line);
                continue;
            }
            let style = style_of(square, &snapshot.styles);
            let combining = square
                .extras_id()
                .and_then(|id| snapshot.zero_width.get(&id))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut cell =
                IncompletePlacement::from_cell(style.fg, style.underline_color, combining);
            if let Some((placement, _)) = current.as_mut()
                && placement.can_append(&cell)
            {
                placement.append();
                continue;
            }
            flush(&mut paints, &mut current, screen_line);
            // Missing coordinates on the first cell default to zero before
            // continuation matching, as required by kitty's placeholder rules.
            cell.row.get_or_insert(0);
            cell.col.get_or_insert(0);
            current = Some((cell, col));
        }
        flush(&mut paints, &mut current, screen_line);
    }
    paints
}

fn flush(
    paints: &mut Vec<PlaceholderPaint>,
    current: &mut Option<(IncompletePlacement, usize)>,
    screen_line: usize,
) {
    if let Some((placement, start_screen_col)) = current.take() {
        paints.push(PlaceholderPaint {
            run: placement.complete(),
            screen_line,
            start_screen_col,
        });
    }
}

fn style_of(square: &Square, styles: &[Style]) -> Style {
    match square.content_tag() {
        ContentTag::Codepoint => styles
            .get(square.style_id() as usize)
            .copied()
            .unwrap_or_default(),
        ContentTag::BgPalette => Style {
            bg: AnsiColor::Indexed(square.bg_palette_index()),
            ..Style::default()
        },
        ContentTag::BgRgb => {
            let (r, g, b) = square.bg_rgb();
            Style {
                bg: AnsiColor::Spec(ColorRgb { r, g, b }),
                ..Style::default()
            }
        }
    }
}

fn wire_style(style: Style) -> TerminalStyle {
    TerminalStyle {
        fg: color(style.fg),
        bg: color(style.bg),
        underline_color: style.underline_color.map(color),
        flags: style.flags.bits(),
    }
}

fn color(color: AnsiColor) -> TerminalColor {
    match color {
        AnsiColor::Named(
            NamedColor::Foreground | NamedColor::LightForeground | NamedColor::DimForeground,
        ) => TerminalColor::Foreground,
        AnsiColor::Named(NamedColor::Background) => TerminalColor::Background,
        AnsiColor::Named(NamedColor::Cursor) => TerminalColor::Cursor,
        AnsiColor::Named(NamedColor::DimBlack) => TerminalColor::Indexed(0),
        AnsiColor::Named(NamedColor::DimRed) => TerminalColor::Indexed(1),
        AnsiColor::Named(NamedColor::DimGreen) => TerminalColor::Indexed(2),
        AnsiColor::Named(NamedColor::DimYellow) => TerminalColor::Indexed(3),
        AnsiColor::Named(NamedColor::DimBlue) => TerminalColor::Indexed(4),
        AnsiColor::Named(NamedColor::DimMagenta) => TerminalColor::Indexed(5),
        AnsiColor::Named(NamedColor::DimCyan) => TerminalColor::Indexed(6),
        AnsiColor::Named(NamedColor::DimWhite) => TerminalColor::Indexed(7),
        AnsiColor::Named(named) => TerminalColor::Indexed(named as u8),
        AnsiColor::Spec(color) => TerminalColor::Rgb {
            r: color.r,
            g: color.g,
            b: color.b,
        },
        AnsiColor::Indexed(index) => TerminalColor::Indexed(index),
    }
}

fn cell_text(square: &Square, zero_width: &HashMap<u16, Vec<char>>) -> String {
    if square.content_tag() != ContentTag::Codepoint {
        return String::new();
    }
    let base = square.c();
    let extras = square
        .extras_id()
        .and_then(|id| zero_width.get(&id))
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    // A blank cell carries no text; the renderer paints its background only.
    // Kitty's Unicode placeholder is placement metadata, never a glyph.
    if extras.is_empty() && matches!(base, '\0' | ' ' | PLACEHOLDER) {
        return String::new();
    }
    let mut text = String::new();
    text.push(if base == '\0' { ' ' } else { base });
    text.extend(
        extras
            .iter()
            .copied()
            .map(|ch| if ch == '\0' { ' ' } else { ch }),
    );
    text
}

fn link_of(square: &Square, links: &HashMap<u16, (String, String)>) -> Option<TerminalLink> {
    if !square.has_hyperlink() {
        return None;
    }
    let (uri, id) = links.get(&square.extras_id()?)?;
    Some(TerminalLink {
        uri: uri.clone(),
        id: (!id.is_empty()).then(|| id.clone()),
    })
}

#[cfg(test)]
mod tests {
    use rio_vt::{ansi::KeyboardModes, crosswords::Mode, crosswords::style::StyleFlags};
    use tcode_protocol::terminal::{CellFlags, KeyboardModes as WireKeyboard, TerminalMode};

    /// The wire carries rio's raw bits, so the two definitions must not drift.
    /// Every flag a client acts on is listed by name here, not derived.
    #[test]
    fn wire_mode_bits_match_rio() {
        for (rio, wire) in [
            (Mode::SHOW_CURSOR, TerminalMode::SHOW_CURSOR),
            (Mode::APP_CURSOR, TerminalMode::APP_CURSOR),
            (Mode::APP_KEYPAD, TerminalMode::APP_KEYPAD),
            (Mode::MOUSE_REPORT_CLICK, TerminalMode::MOUSE_REPORT_CLICK),
            (Mode::BRACKETED_PASTE, TerminalMode::BRACKETED_PASTE),
            (Mode::SGR_MOUSE, TerminalMode::SGR_MOUSE),
            (Mode::MOUSE_MOTION, TerminalMode::MOUSE_MOTION),
            (Mode::LINE_WRAP, TerminalMode::LINE_WRAP),
            (Mode::LINE_FEED_NEW_LINE, TerminalMode::LINE_FEED_NEW_LINE),
            (Mode::ORIGIN, TerminalMode::ORIGIN),
            (Mode::INSERT, TerminalMode::INSERT),
            (Mode::FOCUS_IN_OUT, TerminalMode::FOCUS_IN_OUT),
            (Mode::ALT_SCREEN, TerminalMode::ALT_SCREEN),
            (Mode::MOUSE_DRAG, TerminalMode::MOUSE_DRAG),
            (Mode::UTF8_MOUSE, TerminalMode::UTF8_MOUSE),
            (Mode::ALTERNATE_SCROLL, TerminalMode::ALTERNATE_SCROLL),
            (Mode::VI, TerminalMode::VI),
            (Mode::URGENCY_HINTS, TerminalMode::URGENCY_HINTS),
            (
                Mode::DISAMBIGUATE_ESC_CODES,
                TerminalMode::DISAMBIGUATE_ESC_CODES,
            ),
            (Mode::REPORT_EVENT_TYPES, TerminalMode::REPORT_EVENT_TYPES),
            (
                Mode::REPORT_ALTERNATE_KEYS,
                TerminalMode::REPORT_ALTERNATE_KEYS,
            ),
            (
                Mode::REPORT_ALL_KEYS_AS_ESC,
                TerminalMode::REPORT_ALL_KEYS_AS_ESC,
            ),
            (
                Mode::REPORT_ASSOCIATED_TEXT,
                TerminalMode::REPORT_ASSOCIATED_TEXT,
            ),
            (Mode::MOUSE_REPORT_X10, TerminalMode::MOUSE_REPORT_X10),
            (Mode::GRAPHEME_CLUSTER, TerminalMode::GRAPHEME_CLUSTER),
            (Mode::MOUSE_MODE, TerminalMode::MOUSE_MODE),
            (Mode::SIXEL_DISPLAY, TerminalMode::SIXEL_DISPLAY),
            (Mode::SIXEL_PRIV_PALETTE, TerminalMode::SIXEL_PRIV_PALETTE),
            (
                Mode::SIXEL_CURSOR_TO_THE_RIGHT,
                TerminalMode::SIXEL_CURSOR_TO_THE_RIGHT,
            ),
        ] {
            assert_eq!(rio.bits(), wire.bits(), "{rio:?} moved");
        }
    }

    #[test]
    fn wire_keyboard_and_style_bits_match_rio() {
        for (rio, wire) in [
            (
                KeyboardModes::DISAMBIGUATE_ESC_CODES,
                WireKeyboard::DISAMBIGUATE_ESC_CODES,
            ),
            (
                KeyboardModes::REPORT_EVENT_TYPES,
                WireKeyboard::REPORT_EVENT_TYPES,
            ),
            (
                KeyboardModes::REPORT_ALTERNATE_KEYS,
                WireKeyboard::REPORT_ALTERNATE_KEYS,
            ),
            (
                KeyboardModes::REPORT_ALL_KEYS_AS_ESC,
                WireKeyboard::REPORT_ALL_KEYS_AS_ESC,
            ),
            (
                KeyboardModes::REPORT_ASSOCIATED_TEXT,
                WireKeyboard::REPORT_ASSOCIATED_TEXT,
            ),
        ] {
            assert_eq!(rio.bits(), wire.bits(), "{rio:?} moved");
        }
        assert_eq!(KeyboardModes::NO_MODE.bits(), WireKeyboard::NO_MODE.bits());

        for (rio, wire) in [
            (StyleFlags::INVERSE, CellFlags::INVERSE),
            (StyleFlags::BOLD, CellFlags::BOLD),
            (StyleFlags::ITALIC, CellFlags::ITALIC),
            (StyleFlags::DIM, CellFlags::DIM),
            (StyleFlags::HIDDEN, CellFlags::HIDDEN),
            (StyleFlags::STRIKEOUT, CellFlags::STRIKEOUT),
            (StyleFlags::UNDERLINE, CellFlags::UNDERLINE),
            (StyleFlags::DOUBLE_UNDERLINE, CellFlags::DOUBLE_UNDERLINE),
            (StyleFlags::UNDERCURL, CellFlags::UNDERCURL),
            (StyleFlags::DOTTED_UNDERLINE, CellFlags::DOTTED_UNDERLINE),
            (StyleFlags::DASHED_UNDERLINE, CellFlags::DASHED_UNDERLINE),
            (StyleFlags::ALL_UNDERLINES, CellFlags::ALL_UNDERLINES),
        ] {
            assert_eq!(rio.bits(), wire.bits(), "{rio:?} moved");
        }
    }

    #[test]
    fn projects_wide_cells_spacers_combining_marks_and_osc8_links() {
        let emulator = crate::GridEmulator::with_size(20, 3);
        emulator.feed(
            "中e\u{301}\x1b]8;;https://example.com/target\x1b\\link\x1b]8;;\x1b\\".as_bytes(),
        );
        let snapshot = emulator.snapshot();
        let mut projector = super::Projector::default();
        let row = projector.visible_row(&snapshot, 0);

        assert_eq!(row.cells[0].text, "中");
        assert_eq!(
            row.cells[0].width,
            tcode_protocol::terminal::CellWidth::Wide
        );
        assert_eq!(row.cells[1].text, "");
        assert_eq!(
            row.cells[1].width,
            tcode_protocol::terminal::CellWidth::Spacer
        );
        assert_eq!(row.cells[2].text, "e\u{301}");
        assert_eq!(
            row.cells[3].link.as_ref().map(|link| link.uri.as_str()),
            Some("https://example.com/target")
        );
        // Trailing blanks are implied rather than transmitted, so the row ends
        // at the last written cell.
        assert_eq!(row.cells.len(), 7);
        assert_eq!(row.cells[6].text, "k");
    }
}
