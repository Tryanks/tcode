use super::*;

pub(super) fn snapped_grid_origin(bounds: Bounds<Pixels>, scale_factor: f32) -> Point<Pixels> {
    let snap_down = |value: Pixels| px((f32::from(value) * scale_factor).floor() / scale_factor);
    point(snap_down(bounds.origin.x), snap_down(bounds.origin.y))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_terminal_grid(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
    paint_data: &GridPaintData,
    palette: TerminalPalette,
    cell_width: f32,
    cell_height: f32,
    show_cursor: bool,
) -> Option<Bounds<Pixels>> {
    let scale_factor = window.scale_factor();
    let snap_down = |value: Pixels| px((f32::from(value) * scale_factor).floor() / scale_factor);
    let snap_up = |value: Pixels| px((f32::from(value) * scale_factor).ceil() / scale_factor);
    let origin = snapped_grid_origin(bounds, scale_factor);

    let rows = || paint_data.rows.iter().enumerate();
    let backgrounds =
        rows().flat_map(|(row, paint)| paint.backgrounds.iter().map(move |rect| (row, rect)));
    let selections =
        rows().flat_map(|(row, paint)| paint.selections.iter().map(move |rect| (row, rect)));
    for (row, background) in backgrounds.chain(selections) {
        let left = origin.x + px(background.start_col as f32 * cell_width);
        let right =
            origin.x + px((background.start_col + background.cell_count) as f32 * cell_width);
        let left = snap_down(left);
        let background_bounds = Bounds::new(
            point(left, origin.y + px(row as f32 * cell_height)),
            size(snap_up(right) - left, px(cell_height)),
        );
        window.paint_quad(fill(background_bounds, background.color));
    }

    let cursor_bounds = paint_data.cursor.map(|cursor| {
        Bounds::new(
            point(
                origin.x + px(cursor.start_col as f32 * cell_width),
                origin.y + px(cursor.row as f32 * cell_height),
            ),
            size(px(cursor.cell_count as f32 * cell_width), px(cell_height)),
        )
    });
    if show_cursor
        && let Some(cursor) = paint_data.cursor.filter(|cursor| cursor.visible)
        && let Some(cursor_bounds) = cursor_bounds
    {
        match (cursor.focused, cursor.shape) {
            (false, _) => {
                let t = px(1.);
                window.paint_quad(fill(
                    Bounds::new(cursor_bounds.origin, size(cursor_bounds.size.width, t)),
                    cursor.color,
                ));
                window.paint_quad(fill(
                    Bounds::new(
                        point(cursor_bounds.left(), cursor_bounds.bottom() - t),
                        size(cursor_bounds.size.width, t),
                    ),
                    cursor.color,
                ));
                window.paint_quad(fill(
                    Bounds::new(cursor_bounds.origin, size(t, cursor_bounds.size.height)),
                    cursor.color,
                ));
                window.paint_quad(fill(
                    Bounds::new(
                        point(cursor_bounds.right() - t, cursor_bounds.top()),
                        size(t, cursor_bounds.size.height),
                    ),
                    cursor.color,
                ));
            }
            (_, CursorShape::Beam) => window.paint_quad(fill(
                Bounds::new(
                    cursor_bounds.origin,
                    size(px(2.), cursor_bounds.size.height),
                ),
                cursor.color,
            )),
            (_, CursorShape::Underline) => window.paint_quad(fill(
                Bounds::new(
                    point(cursor_bounds.left(), cursor_bounds.bottom() - px(2.)),
                    size(cursor_bounds.size.width, px(2.)),
                ),
                cursor.color,
            )),
            (_, CursorShape::Block) => window.paint_quad(fill(cursor_bounds, cursor.color)),
            (_, CursorShape::Hidden) => {}
        }
    }

    let font = terminal_font();
    for (row, run) in
        rows().flat_map(|(row, paint)| paint.text_runs.iter().map(move |run| (row, run)))
    {
        let mut run_font = font.clone();
        run_font.weight = if run.style.bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };
        run_font.style = if run.style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        let foreground = if run.style.cursor {
            terminal_color(run.style.bg, palette)
        } else {
            terminal_color(run.style.fg, palette)
        };
        let text_run = TextRun {
            len: run.text.len(),
            font: run_font,
            color: foreground,
            background_color: None,
            strikethrough: None,
            underline: run.style.underline.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(foreground),
                wavy: run.style.underline_wavy,
            }),
        };
        let shaped = window.text_system().shape_line(
            run.text.clone(),
            px(TERMINAL_FONT_SIZE),
            &[text_run],
            Some(px(cell_width)),
        );
        let position = point(
            origin.x + px(run.start_col as f32 * cell_width),
            origin.y + px(row as f32 * cell_height),
        );
        let _ = shaped.paint(position, px(cell_height), TextAlign::Left, None, window, cx);
    }

    cursor_bounds
}

/// Lay out a whole grid with no row cache. The live drawer keeps a cache per
/// terminal; this is for one-shot renders of stored command output.
pub(crate) fn layout_grid(
    state: &TerminalModel,
    palette: TerminalPalette,
    composing: bool,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
) -> GridPaintData {
    layout_grid_cached(
        &mut HashMap::new(),
        0,
        state,
        palette,
        composing.then_some(""),
        hovered_link,
        focused,
        blink_phase,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn layout_grid_cached(
    caches: &mut HashMap<u64, TerminalGridCache>,
    terminal_id: u64,
    state: &TerminalModel,
    palette: TerminalPalette,
    marked_text: Option<&str>,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
) -> GridPaintData {
    let composing = marked_text.is_some();
    let cursor = layout_cursor(state, palette);
    let cursor_cell = cursor.map(|cursor| (cursor.row, cursor.start_col));
    let grid_key = GridCacheKey {
        cols: state.cols(),
        screen_lines: state.rows(),
        display_offset: state.display_offset(),
        palette,
    };
    let cache = caches
        .entry(terminal_id)
        .or_insert_with(|| TerminalGridCache {
            key: grid_key,
            rows: vec![None; state.rows()],
        });
    if cache.key != grid_key || state.fully_damaged() {
        cache.key = grid_key;
        cache.rows = vec![None; state.rows()];
    } else {
        let scrolled = state.scrolled_rows().min(cache.rows.len());
        cache.rows.drain(..scrolled);
        cache.rows.resize(state.rows(), None);
    }

    let mut rows = Vec::with_capacity(state.rows());
    for (row, cached_row) in cache.rows.iter_mut().enumerate() {
        let row_key = row_layout_key(
            state,
            row,
            cursor_cell,
            marked_text,
            hovered_link,
            focused,
            blink_phase,
        );
        let clean = !state.row_damaged(row);
        let cached = cached_row
            .as_ref()
            .filter(|cached| clean && cached.key == row_key)
            .map(|cached| cached.paint.clone());
        let row_paint = cached.unwrap_or_else(|| {
            let paint = Rc::new(layout_row(
                state,
                row,
                palette,
                composing,
                hovered_link,
                focused,
                blink_phase,
                cursor_cell,
            ));
            *cached_row = Some(CachedRowLayout {
                key: row_key,
                paint: paint.clone(),
            });
            paint
        });
        rows.push(row_paint);
    }

    GridPaintData {
        rows,
        cursor: cursor.map(|mut cursor| {
            cursor.focused = focused;
            cursor.visible &= !composing && (!state.cursor_blinking() || blink_phase);
            cursor
        }),
    }
}

pub(super) fn row_layout_key(
    state: &TerminalModel,
    row: usize,
    cursor_cell: Option<(usize, usize)>,
    marked_text: Option<&str>,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
) -> RowLayoutKey {
    let mut selected = (0..state.cols()).filter(|col| state.is_selected(row, *col));
    let selection = selected
        .next()
        .map(|first| (first, selected.next_back().unwrap_or(first)));
    let hovered_link = hovered_link
        .filter(|link| row >= link.start.0 && row <= link.end.0)
        .map(|link| (link.start, link.end));
    let cursor = cursor_cell
        .filter(|(cursor_row, _)| *cursor_row == row)
        .map(|position| CursorRowKey {
            position,
            shape: state.cursor_shape(),
            blinking: state.cursor_blinking(),
            marked_text: marked_text.map(str::to_owned),
            focused,
            blink_phase,
        });
    RowLayoutKey {
        selection,
        hovered_link,
        cursor,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn layout_row(
    state: &TerminalModel,
    row: usize,
    palette: TerminalPalette,
    composing: bool,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
    cursor_cell: Option<(usize, usize)>,
) -> RowPaintData {
    let mut paint = RowPaintData::default();
    // Each run's text grows cell by cell before it is shared.
    let mut runs: Vec<(usize, String, usize, GridTextStyle)> = Vec::new();
    let mut previous_cell_had_extras = false;
    for col in 0..state.cols() {
        let Some(cell) = state.cell(row, col) else {
            break;
        };
        let style = state.style(cell);
        let flags = style.flags();
        let selected = state.is_selected(row, col);
        let (fg, bg) = cell_colors(style.fg, style.bg, flags);

        let background = if bg == TerminalColor::Background {
            None
        } else {
            Some(terminal_color(bg, palette))
        };
        if let Some(color) = background {
            push_background(&mut paint.backgrounds, col, color);
        }
        if selected {
            push_background(&mut paint.selections, col, palette.selection);
        }

        // A wide spacer still participates in backgrounds and hit-testing,
        // but never contributes a glyph to the shaped text.
        if cell.width == CellWidth::Spacer {
            continue;
        }

        // Alacritty stores emoji variation/modifier codepoints as extras;
        // its following placeholder space is not an independently painted
        // character.
        let blank = cell.text.is_empty();
        if blank && previous_cell_had_extras {
            previous_cell_had_extras = false;
            continue;
        }
        previous_cell_had_extras = cell.text.chars().nth(1).is_some();

        let underline = flags.intersects(CellFlags::ALL_UNDERLINES);
        if blank && !underline {
            continue;
        }

        let cursor_visible = !composing
            && !selected
            && cursor_cell == Some((row, col))
            && focused
            && state.cursor_shape() == CursorShape::Block
            && (!state.cursor_blinking() || blink_phase);
        let hyperlink_hovered =
            hovered_link.is_some_and(|link| (row, col) >= link.start && (row, col) <= link.end);
        let text = if blank { " " } else { cell.text.as_str() };
        let style = GridTextStyle {
            fg,
            bg,
            bold: flags.contains(CellFlags::BOLD),
            italic: flags.contains(CellFlags::ITALIC),
            underline: underline || hyperlink_hovered,
            underline_wavy: flags.contains(CellFlags::UNDERCURL),
            selected,
            cursor: cursor_visible,
        };

        if let Some((start_col, run_text, cell_count, run_style)) = runs.last_mut()
            && *start_col + *cell_count == col
            && *run_style == style
        {
            run_text.push_str(text);
            *cell_count += 1;
        } else {
            runs.push((col, text.to_owned(), 1, style));
        }
    }
    paint.text_runs = runs
        .into_iter()
        .map(|(start_col, text, cell_count, style)| BatchedTextRun {
            start_col,
            text: text.into(),
            cell_count,
            style,
        })
        .collect();
    paint
}

pub(super) fn push_background(backgrounds: &mut Vec<BackgroundRect>, col: usize, color: Hsla) {
    if let Some(previous) = backgrounds.last_mut()
        && previous.start_col + previous.cell_count == col
        && previous.color == color
    {
        previous.cell_count += 1;
    } else {
        backgrounds.push(BackgroundRect {
            start_col: col,
            cell_count: 1,
            color,
        });
    }
}

pub(super) fn cell_colors(
    foreground: TerminalColor,
    background: TerminalColor,
    flags: CellFlags,
) -> (TerminalColor, TerminalColor) {
    let (mut fg, mut bg) = (foreground, background);
    if flags.contains(CellFlags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

pub(super) fn layout_cursor(
    state: &TerminalModel,
    palette: TerminalPalette,
) -> Option<CursorPaint> {
    let (row, col) = state.cursor()?;
    let cell = state.cell(row, col)?;
    let (start_col, cell_count, color_col) = if cell.width == CellWidth::Spacer && col > 0 {
        (col - 1, 2, col - 1)
    } else if cell.width == CellWidth::Wide {
        (col, 2, col)
    } else {
        (col, 1, col)
    };
    let style = state.style(state.cell(row, color_col)?);
    let (fg, _) = cell_colors(style.fg, style.bg, style.flags());
    Some(CursorPaint {
        row,
        start_col,
        cell_count,
        color: terminal_color(fg, palette).opacity(0.72),
        visible: !state.is_selected(row, color_col),
        shape: state.cursor_shape(),
        focused: true,
    })
}
