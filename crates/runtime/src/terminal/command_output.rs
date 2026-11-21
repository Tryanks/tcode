//! Render a command's captured output into a [`TerminalFrame`].
//!
//! Clients keep no emulator, so the width they can show is a subscription and
//! the wrapped, styled grid is what the host pushes. This runs on the host's
//! blocking-I/O executor: it is pure CPU over one string.

use tcode_protocol::{
    MAX_STORED_OUTPUT_COLS, STORED_OUTPUT_ROWS,
    terminal::{TerminalDelta, TerminalExit, TerminalFrame, TerminalRow, TerminalRowUpdate},
};
use term::{GridEmulator, Projector};

use super::projection::same_row;

/// How much of an output's end is replayed into the emulator for a frame,
/// however its lines are long. A running command is rendered again as it
/// grows, so this is what one push may cost the host.
const TAIL_BYTES: usize = 512 * 1024;

/// Render the end of `output` as it would look in a `cols`-wide terminal: its
/// last [`STORED_OUTPUT_ROWS`] rows, and in `lines_evicted` how many came
/// before them. The rows are the screen's, a long line wrapped into as many
/// as it takes, so a frame holds at most that many rows of `cols` cells
/// whatever the output's lines are. A client shows the rows it has room for;
/// the whole output is read from the record.
///
/// `cols` is capped at [`MAX_STORED_OUTPUT_COLS`], so the frame's own `cols`
/// is the authority on what the caller actually got.
pub fn render(output: &str, cols: u16) -> TerminalFrame {
    let cols = cols.min(MAX_STORED_OUTPUT_COLS);
    let (tail, dropped) = tail(output, TAIL_BYTES);
    let emulator =
        GridEmulator::with_scrollback(usize::from(cols), usize::from(STORED_OUTPUT_ROWS), 0);
    emulator.feed(&normalized(tail.as_bytes()));

    let snapshot = emulator.snapshot_since(0, 0);
    let mut projector = Projector::default();
    let mut visible = (0..snapshot.screen_lines)
        .map(|row| projector.visible_row(&snapshot, row))
        .collect::<Vec<_>>();
    // A command block sizes to its content: trailing blank rows are not part of
    // the output, they are the unused bottom of the screen it ran on.
    // The projector drops trailing default cells, so an untouched row is empty.
    while visible.last().is_some_and(|row| row.cells.is_empty()) {
        visible.pop();
    }

    TerminalFrame {
        cols: snapshot.cols as u16,
        rows: visible.len().min(u16::MAX as usize) as u16,
        styles: projector.into_styles(),
        visible,
        lines_evicted: dropped + snapshot.lines_evicted + snapshot.history_size as u64,
        ..TerminalFrame::default()
    }
}

/// What a subscriber holding `previous` is sent to hold `next`, two renders
/// of one output at one width: the rows that differ once the screen has
/// moved up by as many rows as came in under it.
pub fn delta(previous: &TerminalFrame, next: &TerminalFrame) -> TerminalDelta {
    let same =
        |held: &TerminalRow, row: &TerminalRow| same_row(held, &previous.styles, row, &next.styles);
    // Rows that left the screen since `previous`. Where the tail's cut moved
    // too this is not the screen's own count; the rows compared below are
    // then sent whichever way they differ.
    let scrolled = next
        .lines_evicted
        .checked_sub(previous.lines_evicted)
        .map_or(0, |rows| rows.min(previous.visible.len() as u64) as usize);
    let blank = TerminalRow::default();
    let rows_replaced = next
        .visible
        .iter()
        .enumerate()
        .filter(|(index, row)| {
            !same(
                previous.visible.get(index + scrolled).unwrap_or(&blank),
                row,
            )
        })
        .map(|(index, row)| TerminalRowUpdate {
            index: index.min(u16::MAX as usize) as u16,
            row: row.clone(),
        })
        .collect();
    let exit = (next.exited, next.exit_code) != (previous.exited, previous.exit_code);
    TerminalDelta {
        cols: next.cols,
        rows: next.rows,
        modes: next.modes,
        cursor: next.cursor,
        lines_evicted: next.lines_evicted,
        styles: next.styles.clone(),
        scrolled: scrolled.min(u16::MAX as usize) as u16,
        rows_replaced,
        history: None,
        title: (next.title != previous.title).then(|| next.title.clone()),
        working_directory: next
            .working_directory
            .clone()
            .filter(|_| next.working_directory != previous.working_directory),
        exit: exit.then_some(TerminalExit {
            exited: next.exited,
            code: next.exit_code,
        }),
        bell: false,
        clipboard: None,
    }
}

/// The last `limit` bytes of `output`, from a character, and how many whole
/// lines come before them. The cut does not look for a line's start: a line
/// longer than `limit` has none in reach, and the line the cut falls in is
/// thousands of rows above the ones a frame holds.
fn tail(output: &str, limit: usize) -> (&str, u64) {
    let start = output.ceil_char_boundary(output.len().saturating_sub(limit));
    let dropped = output.as_bytes()[..start]
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count();
    (&output[start..], dropped as u64)
}

/// Captured output is a log, not a PTY stream: a bare LF means "next line at
/// column zero". Feeding it raw would leave every line indented by the last.
fn normalized(bytes: &[u8]) -> Vec<u8> {
    let mut previous_was_cr = false;
    let mut normalized = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        if byte == b'\n' && !previous_was_cr {
            normalized.push(b'\r');
        }
        normalized.push(byte);
        previous_was_cr = byte == b'\r';
    }
    normalized
}

#[cfg(test)]
mod tests {
    use tcode_protocol::{STORED_OUTPUT_ROWS, terminal::TerminalRow};

    fn text(row: &TerminalRow) -> String {
        let text: String = row
            .cells
            .iter()
            .map(|cell| {
                if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                }
            })
            .collect();
        text.trim_end().to_owned()
    }

    fn rows(frame: &tcode_protocol::terminal::TerminalFrame) -> Vec<String> {
        frame.visible.iter().map(text).collect()
    }

    fn lines(count: usize) -> String {
        (1..=count).map(|line| format!("line {line}\n")).collect()
    }

    /// A frame holds the output's last rows and counts every line before
    /// them, whether they scrolled off its screen or lie before the tail the
    /// host replays; an output that fits is all there.
    #[test]
    fn a_frame_holds_an_outputs_last_rows_and_counts_the_rest() {
        let past_the_tail = 2 * super::TAIL_BYTES / "line 1\n".len();
        for count in [3, 300, past_the_tail] {
            let frame = super::render(&lines(count), 80);
            let rows = rows(&frame);
            assert!(rows.len() <= usize::from(STORED_OUTPUT_ROWS));
            assert_eq!(
                rows.last().map(String::as_str),
                Some(format!("line {count}").as_str())
            );
            assert_eq!(
                rows.first().map(String::as_str),
                Some(format!("line {}", frame.lines_evicted + 1).as_str()),
                "what comes before its rows is counted"
            );
            assert_eq!(rows.len() as u64 + frame.lines_evicted, count as u64);
            assert!(frame.history.is_empty());
        }
    }

    /// An output that is one line longer than the tail the host replays shows
    /// that line's end, wrapped into the frame's rows, whether or not a line
    /// break ends it.
    #[test]
    fn a_line_longer_than_the_tail_shows_its_end() {
        for ending in ["", "\n"] {
            let output = format!("{}END{ending}", "x".repeat(2 * super::TAIL_BYTES));
            let frame = super::render(&output, 80);
            let rows = rows(&frame);
            assert!(rows.len() <= usize::from(STORED_OUTPUT_ROWS));
            assert!(
                rows.last().is_some_and(|row| row.ends_with("xEND")),
                "the line's end: {:?}",
                rows.last()
            );
            assert!(frame.lines_evicted > 0);
        }
    }

    /// A subscriber holding one push of a growing output is sent what makes
    /// it the next: the rows that came in, not the whole screen again, as the
    /// output grows on its screen, scrolls it, and rewrites its last line.
    #[test]
    fn a_push_of_a_growing_output_sends_only_what_came_in() {
        for (before, after, replaced) in [
            (lines(3), lines(4), 1),
            (lines(40), lines(41), 1),
            (
                format!("{}10%\r", lines(40)),
                format!("{}20%\r", lines(40)),
                1,
            ),
            (lines(2_000), lines(2_002), 2),
        ] {
            let previous = super::render(&before, 80);
            let next = super::render(&after, 80);
            let delta = super::delta(&previous, &next);
            assert_eq!(delta.rows_replaced.len(), replaced);
            let mut held = previous;
            held.apply(&delta);
            assert_eq!(rows(&held), rows(&next));
            assert_eq!(held.lines_evicted, next.lines_evicted);
        }
    }
}
