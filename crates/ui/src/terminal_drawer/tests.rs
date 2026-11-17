use super::*;
use std::collections::HashMap;
use tcode_protocol::terminal::{TerminalCell, TerminalFrame, TerminalRow, TerminalStyle};

fn cell(ch: char, width: CellWidth) -> TerminalCell {
    TerminalCell {
        text: if ch == ' ' { String::new() } else { ch.into() },
        width,
        ..TerminalCell::default()
    }
}

fn model(cells: Vec<TerminalCell>) -> TerminalModel {
    model_with_styles(cells, vec![TerminalStyle::default()])
}

fn model_with_styles(cells: Vec<TerminalCell>, styles: Vec<TerminalStyle>) -> TerminalModel {
    let cols = cells.len();
    let mut model = TerminalModel::new(String::new());
    model.apply_frame(TerminalFrame {
        cols: cols as u16,
        rows: 1,
        styles,
        visible: vec![TerminalRow {
            cells,
            wrapped: false,
        }],
        ..TerminalFrame::default()
    });
    model
}

#[test]
fn shell_quotes_paths() {
    assert_eq!(shell_quote("/tmp/a"), "'/tmp/a'");
    assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
    assert_eq!(shell_quote("a'b"), "'a'\\''b'");
}

#[test]
fn clipboard_shortcuts_are_platform_specific() {
    let command = gpui::Modifiers {
        platform: true,
        ..Default::default()
    };
    let control_shift = gpui::Modifiers {
        control: true,
        shift: true,
        ..Default::default()
    };

    assert_eq!(
        terminal_clipboard_shortcut("c", command, true),
        Some(ClipboardShortcut::Copy)
    );
    assert_eq!(
        terminal_clipboard_shortcut("v", command, true),
        Some(ClipboardShortcut::Paste)
    );
    assert_eq!(terminal_clipboard_shortcut("c", control_shift, true), None);
    assert_eq!(terminal_clipboard_shortcut("c", command, false), None);
    assert_eq!(
        terminal_clipboard_shortcut("C", control_shift, false),
        Some(ClipboardShortcut::Copy)
    );
    assert_eq!(
        terminal_clipboard_shortcut("V", control_shift, false),
        Some(ClipboardShortcut::Paste)
    );
    assert_eq!(
        terminal_clipboard_shortcut(
            "c",
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            false,
        ),
        None
    );
}

#[test]
fn terminal_paste_preparation_is_shared_by_clipboard_paths() {
    assert_eq!(prepare_terminal_paste("a\r\nb\nc", false), "a\rb\rc");
    assert_eq!(
        prepare_terminal_paste("a\x1bb", true),
        "\x1b[200~ab\x1b[201~"
    );
}

#[test]
fn grid_paint_preserves_column_boundaries_and_style() {
    let mut undercurl = cell('x', CellWidth::Narrow);
    undercurl.style = 1;
    let palette = TerminalPalette {
        foreground: rgb(0xffffff).into(),
        background: rgb(0x000000).into(),
        selection: rgb(0x336699).into(),
    };
    for (state, expected, wavy) in [
        (
            model(vec![
                cell('a', CellWidth::Narrow),
                cell('中', CellWidth::Wide),
                cell(' ', CellWidth::Spacer),
                cell('b', CellWidth::Narrow),
                cell('文', CellWidth::Wide),
                cell(' ', CellWidth::Spacer),
                cell('c', CellWidth::Narrow),
            ]),
            vec![(0, "a中", 2), (3, "b文", 2), (6, "c", 1)],
            false,
        ),
        (
            model(vec![cell('x', CellWidth::Narrow)]),
            vec![(0, "x", 1)],
            false,
        ),
        (
            model_with_styles(
                vec![undercurl],
                vec![
                    TerminalStyle::default(),
                    TerminalStyle {
                        flags: CellFlags::UNDERCURL.bits(),
                        ..TerminalStyle::default()
                    },
                ],
            ),
            vec![(0, "x", 1)],
            true,
        ),
    ] {
        let paint = layout_grid(&state, palette, false, None, true, true);
        let boundaries = paint.rows[0]
            .text_runs
            .iter()
            .map(|run| (run.start_col, run.text.as_ref(), run.cell_count))
            .collect::<Vec<_>>();
        assert_eq!(boundaries, expected);
        assert!(paint.rows[0].selections.is_empty());
        assert!(paint.rows[0].backgrounds.is_empty());
        for run in &paint.rows[0].text_runs {
            assert_eq!(run.style.underline, wavy);
            assert_eq!(run.style.underline_wavy, wavy);
        }
    }
}

/// A screen that scrolls keeps each row's text with the row as it moves
/// up, and lays out only the row that comes in.
#[test]
fn cached_grid_paint_follows_rows_the_screen_scrolls() {
    let palette = TerminalPalette {
        foreground: rgb(0xffffff).into(),
        background: rgb(0x000000).into(),
        selection: rgb(0x336699).into(),
    };
    let row = |ch| TerminalRow {
        cells: vec![cell(ch, CellWidth::Narrow)],
        wrapped: false,
    };
    let mut state = TerminalModel::new(String::new());
    state.apply_frame(TerminalFrame {
        cols: 1,
        rows: 3,
        styles: vec![TerminalStyle::default()],
        visible: vec![row('a'), row('b'), row('c')],
        ..TerminalFrame::default()
    });
    let mut caches = HashMap::new();
    let text = |paint: &GridPaintData| {
        paint
            .rows
            .iter()
            .map(|row| {
                row.text_runs
                    .iter()
                    .map(|run| run.text.to_string())
                    .collect()
            })
            .collect::<Vec<String>>()
    };
    let first = layout_grid_cached(&mut caches, 7, &state, palette, None, None, true, true);
    assert_eq!(text(&first), ["a", "b", "c"]);

    state.clear_damage();
    let mut delta = idle_delta(1, 3);
    delta.styles = vec![TerminalStyle::default()];
    delta.scrolled = 1;
    delta.rows_replaced = vec![tcode_protocol::terminal::TerminalRowUpdate {
        index: 2,
        row: row('d'),
    }];
    delta.history = Some(tcode_protocol::terminal::TerminalHistoryUpdate::Appended(
        vec![row('a')],
    ));
    state.apply_delta(&delta);
    let scrolled = layout_grid_cached(&mut caches, 7, &state, palette, None, None, true, true);
    assert_eq!(text(&scrolled), ["b", "c", "d"]);
}

fn idle_delta(cols: u16, rows: u16) -> tcode_protocol::terminal::TerminalDelta {
    tcode_protocol::terminal::TerminalDelta {
        cols,
        rows,
        ..Default::default()
    }
}
